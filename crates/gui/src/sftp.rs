//! Aba do explorador SFTP.
//!
//! A aba guarda as duas panes (local e remota), a navegação e a listagem.
//! Também vivem aqui a fila de transferências, os conflitos, o drag & drop,
//! o clipboard interno, as operações de arquivo (menus e diálogos) e a
//! edição remota (cópia temporária, snapshot, banner e envio confirmado).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, Utc};
use eframe::egui;
use serde::{Deserialize, Serialize};
use sshc_core::Session;
use sshc_net::sftp::{
    local_dest, normalize_remote, ConflictDecision, ConflictPolicy, Direction, EntryKind,
    JobOutcome, SftpCmd, SftpConnection, SftpEntry, SftpEvent,
};
use sshc_net::Credentials;

const ERROR: egui::Color32 = egui::Color32::from_rgb(248, 113, 113);
const OK: egui::Color32 = egui::Color32::from_rgb(74, 222, 128);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(96, 165, 250);
const WARN: egui::Color32 = egui::Color32::from_rgb(251, 191, 36);

const ROW_HEIGHT: f32 = 20.0;
const HEADER_HEIGHT: f32 = 18.0;
const SPLIT_MIN: f32 = 0.25;
const SPLIT_MAX: f32 = 0.75;
const SPLIT_HANDLE: f32 = 8.0;

/// Raiz das cópias locais usadas pela edição remota.
const EDIT_ROOT: &str = "sshc-sftp-edit";
/// Limite de arquivos simultaneamente abertos para edição nesta aba.
const EDIT_LIMIT: usize = 20;
/// Polling de metadados: no máximo uma vez por segundo.
const EDIT_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Uma mudança precisa permanecer por 1 s antes de virar "dirty".
const EDIT_STABLE: Duration = Duration::from_secs(1);
/// Cópias de edição mais antigas que isso são removidas na inicialização.
pub const EDIT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Estado de uma transferência na fila da aba.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    Interrupted,
}

impl JobState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    fn label(self) -> &'static str {
        match self {
            Self::Queued => "na fila",
            Self::Running => "ativa",
            Self::Done => "concluída",
            Self::Failed => "falhou",
            Self::Cancelled => "cancelada",
            Self::Interrupted => "interrompida",
        }
    }
}

/// Transferência que pode ser reenfileirada por "Tentar de novo".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetrySpec {
    Download { sources: Vec<String>, dest: PathBuf },
    Upload { sources: Vec<PathBuf>, dest: String },
}

/// Linha da fila de transferências.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobView {
    pub id: u64,
    pub direction: Direction,
    pub label: String,
    pub state: JobState,
    pub bytes_done: u64,
    pub bytes_total: Option<u64>,
    pub items_done: u64,
    pub items_total: u64,
    pub ignored_links: u64,
    pub error: Option<String>,
    pub retry: RetrySpec,
}

/// Caminhos guardados pelo clipboard interno.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipPaths {
    Local(Vec<PathBuf>),
    Remote(Vec<String>),
}

impl ClipPaths {
    fn len(&self) -> usize {
        match self {
            Self::Local(paths) => paths.len(),
            Self::Remote(paths) => paths.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Conteúdo copiado de uma pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub side: Side,
    pub paths: ClipPaths,
}

/// Confirmações da aba.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirm {
    /// Exclusão local ou remota, com os caminhos já resolvidos.
    Delete {
        remote: bool,
        paths: Vec<String>,
    },
    CloseTab {
        id: u64,
    },
    /// Arquivo remoto mudou antes do envio de volta: só sobe após confirmar.
    OverwriteRemote {
        edit: usize,
    },
}

/// Diálogo de criação de pasta.
#[derive(Debug, Clone)]
struct NewFolderDialog {
    remote: bool,
    name: String,
    request_focus: bool,
    error: Option<String>,
}

/// Caminhos que compartilham o mesmo conjunto de bits especiais.
#[derive(Debug, Clone)]
struct PermissionGroup {
    paths: Vec<String>,
    special: u32,
}

impl PermissionGroup {
    /// Bits de permissão a enviar no Chmod: 3 dígitos editados + especiais
    /// preservados.
    fn effective_mode(&self, mode: u32) -> u32 {
        (mode & 0o777) | (self.special & 0o7000)
    }
}

/// Estado do diálogo de permissões (só remoto).
#[derive(Debug, Clone)]
struct PermissionsDialog {
    /// Um grupo por conjunto de bits especiais (setuid/sticky).
    groups: Vec<PermissionGroup>,
    /// Vazio quando a seleção tem modos diferentes (campo mostra "—").
    mode_text: String,
    mixed: bool,
    /// Tipo do primeiro selecionado, só para a prévia.
    is_dir: bool,
}

impl PermissionsDialog {
    fn item_count(&self) -> usize {
        self.groups.iter().map(|group| group.paths.len()).sum()
    }
}

/// Diálogo de operação aberto (um por vez).
#[derive(Debug, Clone)]
enum FileDialog {
    NewFolder(NewFolderDialog),
    Permissions(PermissionsDialog),
}

/// Operação remota aguardando o OpDone correspondente.
#[derive(Debug, Clone)]
struct PendingOp {
    id: u64,
    success: String,
}

/// Cópia local de um arquivo remoto aberto para edição.
///
/// O snapshot (remote_*, local_*) é o estado de quando o arquivo foi
/// aberto. Nada é enviado de volta sem confirmação do usuário.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditedFile {
    pub remote: String,
    pub local: PathBuf,
    pub remote_size: u64,
    pub remote_mtime: Option<u32>,
    pub local_size: u64,
    pub local_mtime: SystemTime,
    pub dirty: bool,
    /// Instante da primeira observação da mudança ainda não confirmada.
    pub stable_since: Option<Instant>,
}

/// Download feito para abrir um arquivo remoto, aguardando JobDone.
#[derive(Debug, Clone)]
struct PendingEditDownload {
    job_id: u64,
    remote: String,
    local: PathBuf,
    remote_size: u64,
    remote_mtime: Option<u32>,
}

/// Motivo do List usado para ler os metadados do arquivo remoto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditStatPurpose {
    /// Antes de enviar de volta: detectar mudança remota.
    BeforeUpload,
    /// Depois de um upload: atualizar o snapshot sem avisar mudança.
    Refresh,
}

/// List remoto aguardando resposta para checar/atualizar o snapshot.
#[derive(Debug, Clone)]
struct PendingEditStat {
    list_id: u64,
    edit: usize,
    remote: String,
    purpose: EditStatPurpose,
}

/// Upload de envio de volta aguardando JobDone.
#[derive(Debug, Clone)]
struct PendingEditUpload {
    job_id: u64,
    edit: usize,
    remote: String,
    /// Snapshot da cópia local no instante em que o upload começou. Se o
    /// arquivo mudar enquanto sobe, o dirty não pode ser limpo.
    local_size: u64,
    local_mtime: Option<SystemTime>,
}

/// Arquivo já existente no destino aguardando decisão.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictPrompt {
    pub job_id: u64,
    pub direction: Direction,
    pub source: String,
    pub dest: String,
    pub source_size: u64,
    pub dest_size: u64,
}

/// Estado da conexão SFTP de uma aba.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Closed(String),
    Failed(String),
}

/// Estado persistido do explorador SFTP (`app_data_dir()/sftp.json`).
///
/// `hidden` significa "mostrar ocultos" (dotfiles), igual ao checkbox da
/// view: true exibe os arquivos que começam com ponto.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SftpState {
    /// Mostrar arquivos ocultos (dotfiles).
    pub hidden: bool,
    /// Fração da largura ocupada pela pane local (0.25–0.75).
    pub split: f32,
    /// Caminhos por sessão, com chave `host:porta:usuário`.
    pub paths: BTreeMap<String, SavedPath>,
}

impl Default for SftpState {
    fn default() -> Self {
        Self {
            hidden: true,
            split: 0.5,
            paths: BTreeMap::new(),
        }
    }
}

/// Caminho local e remoto salvos de uma sessão.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPath {
    pub remote: String,
    pub local: String,
}

/// Mudanças ainda não gravadas do estado do SFTP (Task 14).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StateChanges {
    pub paths: bool,
    pub layout: bool,
}

impl StateChanges {
    pub fn any(self) -> bool {
        self.paths || self.layout
    }
}

/// Caminho do arquivo de estado no diretório de dados do app.
pub fn sftp_state_path() -> PathBuf {
    sshc_core::cloud::app_data_dir().join("sftp.json")
}

/// Lê o estado do SFTP. Arquivo ausente ou corrompido devolve os padrões
/// (mostrar ocultos, divisória no meio).
pub fn load_sftp_state(path: &Path) -> SftpState {
    let state: SftpState = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    SftpState {
        split: if state.split.is_finite() {
            state.split.clamp(SPLIT_MIN, SPLIT_MAX)
        } else {
            0.5
        },
        ..state
    }
}

/// Grava o estado de forma atômica (tmp + rename).
pub fn save_sftp_state(path: &Path, state: &SftpState) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(state)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Chave por sessão no estado e no Keychain: `host:porta:usuário`.
pub fn session_path_key(session: &Session) -> String {
    format!(
        "{}:{}:{}",
        session.host,
        session.port,
        session.user.as_deref().unwrap_or("")
    )
}

/// Aplica o estado salvo numa aba que acabou de conectar. O layout (ocultos
/// e divisória) é global; os caminhos são por sessão. A pane local só aceita
/// o caminho salvo se ele ainda for um diretório. O remoto é listado; se o
/// List falhar, `apply_listed` volta para o home com um aviso.
pub fn apply_saved_paths(tab: &mut SftpTab, state: &SftpState, session: &Session) {
    tab.hidden = state.hidden;
    tab.split = if state.split.is_finite() {
        state.split.clamp(SPLIT_MIN, SPLIT_MAX)
    } else {
        0.5
    };
    let Some(saved) = state.paths.get(&session_path_key(session)) else {
        return;
    };
    let saved_local = PathBuf::from(&saved.local);
    tab.local.cwd = if saved_local.is_dir() {
        saved_local
    } else {
        home_dir()
    };
    tab.reload_local();
    if tab.conn.is_none() {
        return;
    }
    let remote_home = tab.home.clone().unwrap_or_else(|| "/".to_string());
    let remote = saved.remote.trim();
    if remote.is_empty() || remote == remote_home {
        // on_connected já pediu o home; não repete a listagem.
        tab.remote.cwd = remote_home;
        if !matches!(tab.remote.listing, Listing::Loading) {
            tab.reload_remote();
        }
        return;
    }
    tab.remote.cwd = remote.to_string();
    tab.pending_saved_remote = Some(remote.to_string());
    tab.reload_remote();
}

/// O que a view pede ao App; o App aplica depois do painel central.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SftpAction {
    None,
    Reconnect,
    Disconnect,
    Close,
    /// O usuário confirmou o fechamento no modal de jobs ativos.
    CloseConfirmed(u64),
    NeedAuth,
}

/// Lado de uma pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Local,
    Remote,
}

/// Um item listado em uma pane.
#[derive(Debug, Clone)]
pub struct Entry<P> {
    pub name: String,
    pub path: P,
    pub kind: EntryKind,
    pub size: u64,
    /// Segundos desde a época Unix.
    pub modified: Option<u32>,
    /// Bits de permissão (sem o tipo do arquivo).
    pub mode: Option<u32>,
    pub link_target: Option<String>,
}

/// Resultado da listagem de uma pane.
#[derive(Debug, Clone)]
pub enum Listing<P> {
    Loading,
    Ready(Vec<Entry<P>>),
    Error(String),
}

/// Edição efêmera do caminho na barra da pane.
#[derive(Debug, Clone, Default)]
struct PathEdit {
    input: String,
    error: Option<String>,
    request_focus: bool,
}

/// Renomeação inline de uma linha da lista.
#[derive(Debug, Clone)]
struct RowRename {
    from: String,
    input: String,
    request_focus: bool,
    error: Option<String>,
}

/// Estado de uma pane (local ou remota).
#[derive(Debug, Clone)]
pub struct PaneState<P> {
    pub cwd: P,
    pub listing: Listing<P>,
    pub selected: BTreeSet<String>,
    /// Posição (na lista visível) do clique que abriu a seleção atual.
    pub anchor: Option<usize>,
    pub filter: String,
    edit: Option<PathEdit>,
    rename: Option<RowRename>,
}

impl<P> PaneState<P> {
    fn new(cwd: P) -> Self {
        Self {
            cwd,
            listing: Listing::Loading,
            selected: BTreeSet::new(),
            anchor: None,
            filter: String::new(),
            edit: None,
            rename: None,
        }
    }

    fn clear_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.rename = None;
    }

    fn clear_navigation(&mut self) {
        self.clear_selection();
        self.filter.clear();
        self.edit = None;
    }
}

/// Estado de uma aba SFTP. A sessão em si fica no Tab do main.rs.
pub struct SftpTab {
    pub conn: Option<SftpConnection>,
    /// Credenciais usadas na conexão (ficam só na memória, para reconectar).
    pub creds: Option<Credentials>,
    pub state: ConnState,
    /// Aviso do handshake (host key aprendida / compatibilidade legada).
    pub note: Option<String>,
    /// Caminho inicial remoto resolvido no connect.
    pub home: Option<String>,
    pub local: PaneState<PathBuf>,
    pub remote: PaneState<String>,
    /// Ocultos visíveis por padrão (dotfiles).
    pub hidden: bool,
    /// Fração da largura ocupada pela pane local (0.25–0.75).
    pub split: f32,
    pub focus: Side,
    /// Fila de transferências desta conexão/aba.
    pub jobs: Vec<JobView>,
    /// Conflitos aguardando decisão, um modal por vez.
    pub conflicts: Vec<ConflictPrompt>,
    /// Clipboard interno (lado + caminhos).
    pub clip: Option<Clip>,
    /// Confirmação modal pendente (fechar aba com jobs).
    pub confirm: Option<Confirm>,
    /// Próximo id de List/op/job. Monotônico entre reconexões.
    pub next_id: u64,
    /// Mensagem efêmera exibida no rodapé da aba.
    pub status: Option<String>,
    /// Painel de transferências visível.
    pub queue_open: bool,
    /// "Aplicar a todos" marcado no conflito atual (persiste entre frames).
    conflict_apply_all: bool,
    /// Id da listagem remota mais recente.
    list_id: Option<u64>,
    /// Diálogo de operação aberto (nova pasta/permissões).
    file_dialog: Option<FileDialog>,
    /// Operações remotas aguardando o OpDone correspondente.
    pending_ops: Vec<PendingOp>,
    /// Arquivos remotos abertos para edição (cópia local + snapshot).
    pub edits: Vec<EditedFile>,
    /// Downloads de edição ainda em andamento.
    pending_edit_downloads: Vec<PendingEditDownload>,
    /// Listas remoto usados para checar/atualizar snapshots de edição.
    pending_edit_stats: Vec<PendingEditStat>,
    /// Uploads de "Enviar de volta" em andamento.
    pending_edit_uploads: Vec<PendingEditUpload>,
    /// Último polling de metadados das cópias em edição.
    last_edit_poll: Option<Instant>,
    /// Caminhos das panes mudaram e ainda não foram gravados (Task 14).
    paths_dirty: bool,
    /// Ocultos/divisória mudaram e ainda não foram gravados (Task 14).
    layout_dirty: bool,
    /// Caminho remoto restaurado do disco aguardando o List confirmar.
    pending_saved_remote: Option<String>,
    /// Navegação remota pedida; o próximo Listed ok confirma a persistência.
    nav_pending: bool,
    /// Último caminho remoto confirmado por um Listed Ok. Fica None enquanto
    /// a primeira listagem não der certo, para o "/" provisório não ser salvo.
    last_confirmed_remote: Option<String>,
}

impl SftpTab {
    pub fn connecting(creds: Option<Credentials>) -> Self {
        let home = home_dir();
        let mut tab = Self {
            conn: None,
            creds,
            state: ConnState::Connecting,
            note: None,
            home: None,
            local: PaneState::new(home),
            remote: PaneState::new("/".to_string()),
            hidden: true,
            split: 0.5,
            focus: Side::Local,
            jobs: Vec::new(),
            conflicts: Vec::new(),
            clip: None,
            confirm: None,
            next_id: 1,
            status: None,
            queue_open: true,
            conflict_apply_all: false,
            list_id: None,
            file_dialog: None,
            pending_ops: Vec::new(),
            edits: Vec::new(),
            pending_edit_downloads: Vec::new(),
            pending_edit_stats: Vec::new(),
            pending_edit_uploads: Vec::new(),
            last_edit_poll: None,
            paths_dirty: false,
            layout_dirty: false,
            pending_saved_remote: None,
            nav_pending: false,
            last_confirmed_remote: None,
        };
        tab.reload_local();
        tab
    }

    /// Algum modal da aba está aberto (usado no guard de atalhos).
    pub fn has_modal(&self) -> bool {
        self.confirm.is_some() || self.file_dialog.is_some() || !self.conflicts.is_empty()
    }

    /// Marca que os caminhos das panes precisam ser gravados.
    fn mark_paths_changed(&mut self) {
        self.paths_dirty = true;
    }

    /// Marca que ocultos/divisória precisam ser gravados.
    fn mark_layout_changed(&mut self) {
        self.layout_dirty = true;
    }

    /// Consome as mudanças pendentes que o App deve persistir.
    pub fn take_state_changes(&mut self) -> StateChanges {
        let changes = StateChanges {
            paths: self.paths_dirty,
            layout: self.layout_dirty,
        };
        self.paths_dirty = false;
        self.layout_dirty = false;
        changes
    }

    /// Entrada de caminho a persistir. O remoto só entra depois de um Listed
    /// Ok; antes disso fica vazio e apply_saved_paths resolve para o home.
    pub fn saved_path(&self) -> SavedPath {
        SavedPath {
            remote: self.last_confirmed_remote.clone().unwrap_or_default(),
            local: self.local.cwd.display().to_string(),
        }
    }

    /// Chamado quando o connect termina: na primeira conexão começa no home;
    /// numa reconexão tenta de novo o cwd que já estava na pane.
    pub fn on_connected(&mut self) {
        if matches!(&self.remote.listing, Listing::Loading) {
            self.remote.cwd = self.home.clone().unwrap_or_else(|| "/".to_string());
        }
        self.remote.filter.clear();
        self.remote.edit = None;
        self.remote.rename = None;
        self.reload_remote();
    }

    pub fn disconnect(&mut self) {
        self.conn = None;
        self.list_id = None;
        self.state = ConnState::Closed("desconectado".to_string());
        self.mark_interrupted();
    }

    /// Aloca um id monotônico para List, operações e jobs. Ids não são
    /// reciclados na reconexão, então eventos atrasados nunca casam com um
    /// pedido novo.
    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// O ator caiu ou foi desconectado: jobs não terminados viram
    /// Interrupted e conflitos pendentes deixam de existir.
    pub fn on_closed(&mut self, reason: String) {
        self.conn = None;
        self.list_id = None;
        self.state = ConnState::Closed(reason);
        self.mark_interrupted();
    }

    fn mark_interrupted(&mut self) {
        for job in &mut self.jobs {
            if job.state.is_active() {
                job.state = JobState::Interrupted;
            }
        }
        self.conflicts.clear();
        self.conflict_apply_all = false;
        // Sem conexão não há OpDone: as operações pendentes não vão chegar.
        self.pending_ops.clear();
        // Downloads/stat/upload de edição também não terminam sem conexão.
        // As cópias locais e o estado dirty continuam para uma reconexão.
        self.pending_edit_downloads.clear();
        self.pending_edit_stats.clear();
        self.pending_edit_uploads.clear();
        self.file_dialog = None;
        if matches!(
            self.confirm,
            Some(Confirm::Delete { remote: true, .. }) | Some(Confirm::OverwriteRemote { .. })
        ) {
            self.confirm = None;
        }
    }

    pub fn reload_local(&mut self) {
        self.local.clear_selection();
        self.local.listing = read_local(&self.local.cwd);
    }

    pub fn navigate_local(&mut self, path: PathBuf) {
        self.local.cwd = path;
        self.local.clear_navigation();
        self.reload_local();
        self.mark_paths_changed();
    }

    pub fn reload_remote(&mut self) {
        self.remote.clear_selection();
        self.remote.edit = None;
        let path = self.remote.cwd.clone();
        if self.conn.is_none() {
            self.remote.listing = Listing::Error("sem conexão".to_string());
            return;
        }
        let id = self.alloc_id();
        let conn = self.conn.as_ref().expect("conexão checada");
        self.list_id = Some(id);
        self.remote.listing = Listing::Loading;
        if !conn.send(SftpCmd::List { id, path }) {
            self.list_id = None;
            self.remote.listing = Listing::Error("conexão encerrada".to_string());
        }
    }

    pub fn navigate_remote(&mut self, path: String) {
        self.remote.cwd = path;
        self.remote.clear_navigation();
        // Uma navegação do usuário sobrepõe o caminho restaurado do disco.
        self.pending_saved_remote = None;
        self.nav_pending = true;
        self.reload_remote();
    }

    /// Aplica o Listed do pedido mais recente. Um id antigo (pedido
    /// substituído por uma navegação ou por uma reconexão) é descartado.
    pub fn apply_listed(
        &mut self,
        id: u64,
        path: String,
        result: std::result::Result<Vec<SftpEntry>, String>,
    ) {
        if self.list_id != Some(id) {
            return;
        }
        self.list_id = None;
        match result {
            Ok(entries) => {
                let mut entries: Vec<Entry<String>> =
                    entries.into_iter().map(entry_from_sftp).collect();
                sort_entries(&mut entries);
                self.last_confirmed_remote = Some(path.clone());
                self.remote.cwd = path;
                self.remote.listing = Listing::Ready(entries);
                self.pending_saved_remote = None;
                if std::mem::take(&mut self.nav_pending) {
                    self.mark_paths_changed();
                }
            }
            Err(error) => {
                if self.pending_saved_remote.take().is_some() {
                    let fallback = self.home.clone().unwrap_or_else(|| "/".to_string());
                    self.status = Some(format!(
                        "caminho remoto salvo {path} indisponível ({error}); usando {fallback}"
                    ));
                    self.remote.cwd = fallback;
                    self.reload_remote();
                    return;
                }
                self.remote.listing = Listing::Error(error);
            }
        }
        self.remote.clear_selection();
    }

    /// Processa um evento do ator. Devolve true quando a conexão encerrou.
    pub fn handle_event(&mut self, event: SftpEvent) -> bool {
        match event {
            SftpEvent::Listed { id, path, result } => {
                // Um List de edição (stat) não alimenta a listagem da pane.
                if self.handle_edit_stat_listed(id, &result) {
                    return false;
                }
                self.apply_listed(id, path, result);
                false
            }
            SftpEvent::JobStarted {
                id,
                items_total,
                bytes_total,
            } => {
                if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
                    if job.state.is_active() {
                        job.state = JobState::Running;
                        job.items_total = items_total;
                        job.bytes_total = bytes_total;
                        if items_total > 1 && !job.label.contains('(') {
                            job.label = format!("{} ({items_total} arquivos)", job.label);
                        }
                    }
                }
                false
            }
            SftpEvent::JobProgress {
                id,
                items_done,
                bytes_done,
                bytes_total,
                ..
            } => {
                if let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) {
                    if job.state.is_active() {
                        job.items_done = items_done;
                        job.bytes_done = bytes_done;
                        job.bytes_total = bytes_total;
                    }
                }
                false
            }
            SftpEvent::JobDone { id, outcome } => {
                self.finish_job(id, outcome);
                false
            }
            SftpEvent::Conflict {
                id,
                direction,
                source,
                dest,
                source_size,
                dest_size,
            } => {
                // O job espera uma resposta por conflito; um evento repetido
                // não pode virar dois modais (e duas respostas atrasadas).
                if !self.conflicts.iter().any(|prompt| prompt.job_id == id) {
                    self.conflicts.push(ConflictPrompt {
                        job_id: id,
                        direction,
                        source,
                        dest,
                        source_size,
                        dest_size,
                    });
                }
                false
            }
            SftpEvent::OpDone { id, result } => {
                self.finish_op(id, result);
                false
            }
            SftpEvent::Closed { reason } => {
                self.on_closed(reason);
                true
            }
        }
    }

    fn finish_job(&mut self, id: u64, outcome: JobOutcome) {
        let Some(job) = self.jobs.iter_mut().find(|job| job.id == id) else {
            return;
        };
        // Um Failed atrasado depois de Closed não cria uma segunda linha:
        // o job já está Interrupted e permanece assim.
        if !job.state.is_active() {
            return;
        }
        let direction = job.direction;
        match outcome {
            JobOutcome::Completed { ignored_links } => {
                if let Some(total) = job.bytes_total {
                    job.bytes_done = total;
                }
                job.ignored_links = ignored_links;
                job.state = JobState::Done;
            }
            JobOutcome::Cancelled => job.state = JobState::Cancelled,
            JobOutcome::Failed(error) => {
                job.state = JobState::Failed;
                job.error = Some(error);
            }
        }
        let completed = job.state == JobState::Done;
        // Downloads/uploads de edição têm snapshot e confirmação próprios.
        self.finish_edit_job(id, completed);
        let conflicts_before = self.conflicts.len();
        self.conflicts.retain(|prompt| prompt.job_id != id);
        if self.conflicts.len() != conflicts_before {
            self.conflict_apply_all = false;
        }
        if completed {
            match direction {
                Direction::Download => self.reload_local(),
                Direction::Upload => self.reload_remote(),
            }
        }
    }

    /// Começa a edição de um arquivo remoto: baixa uma cópia para o temp e
    /// abre quando o download terminar.
    pub fn begin_edit(
        &mut self,
        session: &Session,
        remote: String,
        snapshot: (u64, Option<u32>),
    ) -> Option<u64> {
        self.begin_edit_impl(session, remote, snapshot, true)
    }

    /// Implementação com um seam para testes: abrir o app do SO numa cópia
    /// recuperada lançaria um editor real durante a suíte; o teste usa false.
    fn begin_edit_impl(
        &mut self,
        session: &Session,
        remote: String,
        snapshot: (u64, Option<u32>),
        open_existing: bool,
    ) -> Option<u64> {
        if let Some(index) = self.edits.iter().position(|edit| edit.remote == remote) {
            let busy = self.edits[index].dirty
                || self
                    .pending_edit_uploads
                    .iter()
                    .any(|pending| pending.edit == index)
                || self.pending_edit_stats.iter().any(|pending| {
                    pending.edit == index && pending.purpose == EditStatPurpose::BeforeUpload
                });
            if busy {
                self.status = Some(
                    "arquivo já está em edição — use Enviar de volta, Reabrir ou Descartar"
                        .to_string(),
                );
            } else {
                // Cópia limpa e ociosa: um novo Editar só reabre o arquivo.
                self.reopen_edit(index);
            }
            return None;
        }
        if self
            .pending_edit_downloads
            .iter()
            .any(|pending| pending.remote == remote)
        {
            self.status = Some("download para edição já está em andamento".to_string());
            return None;
        }
        if self.edits.len() + self.pending_edit_downloads.len() >= EDIT_LIMIT {
            self.status = Some(format!(
                "máximo de {EDIT_LIMIT} arquivos em edição — descarte um ou cancele um download para abrir outro"
            ));
            return None;
        }
        let key = edit_key(session);
        let root = edit_temp_root();
        if let Err(error) = ensure_edit_root(&root) {
            self.status = Some(format!("edição: {error:#}"));
            return None;
        }
        if let Err(error) = ensure_edit_session_dir(&root, &key) {
            self.status = Some(format!("edição: {error:#}"));
            return None;
        }
        let local = match edit_temp_path(&root, &key, &remote) {
            Ok(path) => path,
            Err(error) => {
                self.status = Some(format!("edição: {error:#}"));
                return None;
            }
        };
        if self.edits.iter().any(|edit| edit.local == local) {
            self.status = Some(format!(
                "cópia local já pertence a outro arquivo em edição: {}",
                local.display()
            ));
            return None;
        }
        // Uma cópia que ninguém rastreia não pode ser sobrescrita por um
        // download Overwrite: é uma edição não enviada de outro momento.
        if self.recover_untracked_edit(&remote, &local, snapshot, open_existing) {
            return None;
        }
        if self.conn.is_none() {
            self.status = Some("sem conexão".to_string());
            return None;
        }
        let Some(parent) = local.parent().map(Path::to_path_buf) else {
            self.status = Some(format!("destino inválido: {}", local.display()));
            return None;
        };
        let job_id = self.alloc_id();
        let sent = self.conn.as_ref().is_some_and(|conn| {
            conn.send(SftpCmd::Download {
                job_id,
                sources: vec![remote.clone()],
                dest: parent.clone(),
                policy: ConflictPolicy::Overwrite,
            })
        });
        if !sent {
            self.status = Some("conexão encerrada".to_string());
            return None;
        }
        let label = format!("editar {}", remote_base_name(&remote));
        self.jobs.push(JobView {
            id: job_id,
            direction: Direction::Download,
            label,
            state: JobState::Queued,
            bytes_done: 0,
            bytes_total: None,
            items_done: 0,
            items_total: 0,
            ignored_links: 0,
            error: None,
            retry: RetrySpec::Download {
                sources: vec![remote.clone()],
                dest: parent,
            },
        });
        self.pending_edit_downloads.push(PendingEditDownload {
            job_id,
            remote,
            local,
            remote_size: snapshot.0,
            remote_mtime: snapshot.1,
        });
        self.queue_open = true;
        self.status = Some("baixando cópia para edição…".to_string());
        Some(job_id)
    }

    /// Se o caminho temp já existe e nenhuma edição viva o rastreia, registra
    /// essa cópia como dirty em vez de baixar por cima. Devolve true quando
    /// tratou o caminho (inclusive quando recusou por não ser arquivo).
    fn recover_untracked_edit(
        &mut self,
        remote: &str,
        local: &Path,
        snapshot: (u64, Option<u32>),
        open_existing: bool,
    ) -> bool {
        let metadata = match std::fs::symlink_metadata(local) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(error) => {
                self.status = Some(format!("lendo cópia local {}: {error}", local.display()));
                return true;
            }
        };
        let name = remote_base_name(remote);
        if metadata.file_type().is_symlink() {
            self.status = Some(format!(
                "cópia local é um link simbólico — não vou editar {}",
                local.display()
            ));
            return true;
        }
        if !metadata.is_file() {
            self.status = Some(format!(
                "cópia local existente não é arquivo: {}",
                local.display()
            ));
            return true;
        }
        let local_mtime = metadata.modified().unwrap_or_else(|_| SystemTime::now());
        self.edits.push(EditedFile {
            remote: remote.to_string(),
            local: local.to_path_buf(),
            remote_size: snapshot.0,
            remote_mtime: snapshot.1,
            local_size: metadata.len(),
            local_mtime,
            // Sem snapshot confiável do conteúdo: o envio exige confirmação.
            dirty: true,
            stable_since: None,
        });
        if !open_existing {
            self.status = Some(format!(
                "{name}: cópia local não enviada preservada — use Reabrir e confirme o envio de volta"
            ));
            return true;
        }
        match open_with_os(local) {
            Ok(()) => {
                self.status = Some(format!(
                    "abrindo {name}: cópia local não enviada preservada — confirme o envio de volta"
                ));
            }
            Err(error) => {
                self.status = Some(format!(
                    "cópia local não enviada preservada, mas não abri {}: {error}",
                    local.display()
                ));
            }
        }
        true
    }

    /// Poll de metadados das cópias locais (no máximo 1x/s). Devolve true
    /// quando algum arquivo acabou de ficar dirty.
    pub fn poll_edits(&mut self, now: Instant) -> bool {
        if self.edits.is_empty() {
            return false;
        }
        if self
            .last_edit_poll
            .is_some_and(|last| now.saturating_duration_since(last) < EDIT_POLL_INTERVAL)
        {
            return false;
        }
        self.last_edit_poll = Some(now);
        let mut newly_dirty = false;
        for edit in &mut self.edits {
            if edit.dirty {
                continue;
            }
            let Ok(metadata) = std::fs::metadata(&edit.local) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            if metadata.len() == edit.local_size && modified == edit.local_mtime {
                edit.stable_since = None;
                continue;
            }
            match edit.stable_since {
                Some(since) if now.saturating_duration_since(since) >= EDIT_STABLE => {
                    edit.dirty = true;
                    edit.stable_since = None;
                    newly_dirty = true;
                }
                Some(_) => {}
                None => edit.stable_since = Some(now),
            }
        }
        newly_dirty
    }

    /// "Enviar de volta": antes de qualquer upload confere o remoto.
    fn request_send_back(&mut self, edit: usize) {
        let Some(local) = self.edits.get(edit).map(|current| current.local.clone()) else {
            return;
        };
        if self
            .pending_edit_uploads
            .iter()
            .any(|pending| pending.edit == edit)
        {
            self.status = Some("envio já está em andamento".to_string());
            return;
        }
        if self
            .pending_edit_stats
            .iter()
            .any(|pending| pending.edit == edit)
        {
            self.status = Some("já verificando o arquivo remoto".to_string());
            return;
        }
        if self.conn.is_none() {
            self.status = Some("sem conexão".to_string());
            return;
        }
        if !local.is_file() {
            self.status = Some(format!("cópia local não existe: {}", local.display()));
            return;
        }
        if self.request_edit_stat(edit, EditStatPurpose::BeforeUpload) {
            self.status = Some("verificando o arquivo remoto…".to_string());
        }
    }

    /// Job de upload em andamento de uma edição, se houver.
    fn upload_job_for_edit(&self, edit: usize) -> Option<u64> {
        self.pending_edit_uploads
            .iter()
            .find(|pending| pending.edit == edit)
            .map(|pending| pending.job_id)
    }

    /// Cancela um upload de edição ou, quando não dá para falar com a
    /// conexão, marca o job como cancelado para ignorar um JobDone atrasado.
    fn cancel_edit_upload(&mut self, job_id: u64) {
        if let Some(conn) = &self.conn {
            let _ = conn.send(SftpCmd::Cancel { job_id });
        }
        if let Some(job) = self.jobs.iter_mut().find(|job| job.id == job_id) {
            if job.state.is_active() {
                job.state = JobState::Cancelled;
            }
        }
    }

    /// Abre de novo a cópia local de uma edição (útil após falha do app do SO).
    fn reopen_edit(&mut self, index: usize) {
        let Some((local, remote)) = self
            .edits
            .get(index)
            .map(|edit| (edit.local.clone(), edit.remote.clone()))
        else {
            return;
        };
        let name = remote_base_name(&remote);
        if !local.is_file() {
            self.status = Some(format!(
                "cópia local não existe: {} — use Descartar",
                local.display()
            ));
            return;
        }
        match open_with_os(&local) {
            Ok(()) => self.status = Some(format!("abrindo {name}")),
            Err(error) => self.status = Some(format!("não abri {}: {error}", local.display())),
        }
    }

    /// Remove a cópia local e a entrada de edição. Se houver um upload em
    /// andamento, ele é cancelado primeiro para o remoto não ser sobrescrito
    /// por um arquivo que o usuário descartou.
    fn discard_edit(&mut self, index: usize) {
        if index >= self.edits.len() {
            return;
        }
        if let Some(job_id) = self.upload_job_for_edit(index) {
            self.cancel_edit_upload(job_id);
        }
        // Remove o stat de antes do envio: um Listed atrasado não pode iniciar
        // um upload depois do descarte.
        self.pending_edit_stats
            .retain(|pending| pending.edit != index);
        self.pending_edit_uploads
            .retain(|pending| pending.edit != index);
        let edit = self.edits.remove(index);
        self.pending_edit_downloads
            .retain(|pending| pending.local != edit.local);
        let mut failure = None;
        if let Err(error) = std::fs::remove_file(&edit.local) {
            if error.kind() != std::io::ErrorKind::NotFound {
                failure = Some(format!("excluindo {}: {error}", edit.local.display()));
            }
        }
        for pending in &mut self.pending_edit_stats {
            if pending.edit > index {
                pending.edit -= 1;
            }
        }
        for pending in &mut self.pending_edit_uploads {
            if pending.edit > index {
                pending.edit -= 1;
            }
        }
        let clear_confirm = matches!(
            &self.confirm,
            Some(Confirm::OverwriteRemote { edit }) if *edit == index
        );
        if clear_confirm {
            self.confirm = None;
        } else if let Some(Confirm::OverwriteRemote { edit }) = &mut self.confirm {
            if *edit > index {
                *edit -= 1;
            }
        }
        self.status = Some(match failure {
            Some(error) => error,
            None => format!("edição descartada: {}", remote_base_name(&edit.remote)),
        });
    }

    /// Envia um List do diretório remoto para checar/atualizar o snapshot.
    /// sshc_net não expõe um Stat próprio (e crates/net não pode ser tocado
    /// nesta task); o List do diretório-pai traz lstat de cada entrada, que é
    /// o stat disponível para comparar tamanho/mtime sem abrir o arquivo.
    fn request_edit_stat(&mut self, edit: usize, purpose: EditStatPurpose) -> bool {
        let Some(remote) = self.edits.get(edit).map(|current| current.remote.clone()) else {
            return false;
        };
        if self
            .pending_edit_stats
            .iter()
            .any(|pending| pending.edit == edit)
        {
            return false;
        }
        let Some(parent) = remote_parent(&remote) else {
            if purpose == EditStatPurpose::BeforeUpload {
                self.status = Some("caminho remoto inválido".to_string());
            }
            return false;
        };
        if self.conn.is_none() {
            if purpose == EditStatPurpose::BeforeUpload {
                self.status = Some("sem conexão".to_string());
            }
            return false;
        }
        let list_id = self.alloc_id();
        let sent = self.conn.as_ref().is_some_and(|conn| {
            conn.send(SftpCmd::List {
                id: list_id,
                path: parent,
            })
        });
        if !sent {
            if purpose == EditStatPurpose::BeforeUpload {
                self.status = Some("conexão encerrada".to_string());
            }
            return false;
        }
        self.pending_edit_stats.push(PendingEditStat {
            list_id,
            edit,
            remote,
            purpose,
        });
        true
    }

    /// Processa o Listed de um stat de edição. Devolve true quando o evento
    /// era desse fluxo (e não deve alimentar a listagem da pane).
    fn handle_edit_stat_listed(
        &mut self,
        id: u64,
        result: &std::result::Result<Vec<SftpEntry>, String>,
    ) -> bool {
        let Some(position) = self
            .pending_edit_stats
            .iter()
            .position(|pending| pending.list_id == id)
        else {
            return false;
        };
        let pending = self.pending_edit_stats.remove(position);
        let still_current = self
            .edits
            .get(pending.edit)
            .is_some_and(|edit| edit.remote == pending.remote);
        if !still_current {
            return true;
        }
        match pending.purpose {
            EditStatPurpose::Refresh => {
                if let Ok(entries) = result {
                    if let Some(entry) = entries.iter().find(|entry| entry.path == pending.remote) {
                        if let Some(edit) = self.edits.get_mut(pending.edit) {
                            edit.remote_size = entry.size;
                            edit.remote_mtime = entry.modified;
                        }
                    }
                }
            }
            EditStatPurpose::BeforeUpload => match result {
                Err(error) => {
                    self.status = Some(format!("não verifiquei o remoto: {error}"));
                }
                Ok(entries) => {
                    let changed = self
                        .edits
                        .get(pending.edit)
                        .is_some_and(|edit| remote_changed(entries, edit));
                    if changed {
                        self.status =
                            Some("o arquivo remoto mudou — confirme o envio de volta".to_string());
                        self.confirm = Some(Confirm::OverwriteRemote { edit: pending.edit });
                    } else {
                        self.start_edit_upload(pending.edit);
                    }
                }
            },
        }
        true
    }

    /// Envia a cópia local por cima do arquivo remoto.
    fn start_edit_upload(&mut self, edit: usize) {
        let Some((local, remote)) = self
            .edits
            .get(edit)
            .map(|current| (current.local.clone(), current.remote.clone()))
        else {
            return;
        };
        if self
            .pending_edit_uploads
            .iter()
            .any(|pending| pending.edit == edit)
        {
            return;
        }
        let metadata = match std::fs::metadata(&local) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => {
                self.status = Some(format!("cópia local não existe: {}", local.display()));
                return;
            }
        };
        let upload_size = metadata.len();
        let upload_mtime = metadata.modified().ok();
        let Some(parent) = remote_parent(&remote) else {
            self.status = Some("caminho remoto inválido".to_string());
            return;
        };
        let name = remote_base_name(&remote);
        if self.conn.is_none() {
            self.status = Some("sem conexão".to_string());
            return;
        }
        let job_id = self.alloc_id();
        let sent = self.conn.as_ref().is_some_and(|conn| {
            conn.send(SftpCmd::Upload {
                job_id,
                sources: vec![local.clone()],
                dest: parent.clone(),
                policy: ConflictPolicy::Overwrite,
            })
        });
        if !sent {
            self.status = Some("conexão encerrada".to_string());
            return;
        }
        self.jobs.push(JobView {
            id: job_id,
            direction: Direction::Upload,
            label: format!("enviar de volta {name}"),
            state: JobState::Queued,
            bytes_done: 0,
            bytes_total: None,
            items_done: 0,
            items_total: 0,
            ignored_links: 0,
            error: None,
            retry: RetrySpec::Upload {
                sources: vec![local],
                dest: parent,
            },
        });
        self.pending_edit_uploads.push(PendingEditUpload {
            job_id,
            edit,
            remote,
            local_size: upload_size,
            local_mtime: upload_mtime,
        });
        self.queue_open = true;
        self.status = Some(format!("enviando {name} de volta…"));
    }

    /// Trata o JobDone de um download/upload ligado à edição.
    fn finish_edit_job(&mut self, id: u64, completed: bool) {
        let error = self
            .jobs
            .iter()
            .find(|job| job.id == id)
            .and_then(|job| job.error.clone());
        if let Some(position) = self
            .pending_edit_downloads
            .iter()
            .position(|pending| pending.job_id == id)
        {
            let pending = self.pending_edit_downloads.remove(position);
            if completed {
                self.open_downloaded_edit(pending);
            } else {
                let name = remote_base_name(&pending.remote);
                self.status = Some(match error.as_deref() {
                    Some(error) => format!("download de {name} para edição falhou: {error}"),
                    None => format!("download de {name} para edição não concluído"),
                });
            }
        }
        if let Some(position) = self
            .pending_edit_uploads
            .iter()
            .position(|pending| pending.job_id == id)
        {
            let pending = self.pending_edit_uploads.remove(position);
            if completed {
                self.complete_edit_upload(&pending);
            } else {
                let name = remote_base_name(&pending.remote);
                self.status = Some(match error.as_deref() {
                    Some(error) => {
                        format!("envio de volta de {name} falhou: {error} — edição mantida")
                    }
                    None => format!("envio de volta de {name} não concluído — edição mantida"),
                });
            }
        }
    }

    /// Abre a cópia baixada e guarda o snapshot; só chamado quando o
    /// download terminou e o arquivo existe.
    fn open_downloaded_edit(&mut self, pending: PendingEditDownload) {
        let local = pending.local.clone();
        let name = remote_base_name(&pending.remote);
        if self.record_edit(pending).is_none() {
            return;
        }
        match open_with_os(&local) {
            Ok(()) => {
                self.status = Some(format!(
                    "editando {name} — mudanças só sobem com confirmação"
                ));
            }
            Err(error) => {
                self.status = Some(format!("não abri {}: {error}", local.display()));
            }
        }
    }

    /// Registra a cópia que acabou de ser baixada como arquivo em edição.
    /// Um download que não gerou um arquivo regular não entra na lista.
    fn record_edit(&mut self, pending: PendingEditDownload) -> Option<usize> {
        let metadata = match std::fs::metadata(&pending.local) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => {
                self.status = Some(format!(
                    "download de edição não gerou {}",
                    pending.local.display()
                ));
                return None;
            }
        };
        let edit = EditedFile {
            remote: pending.remote,
            local: pending.local,
            remote_size: pending.remote_size,
            remote_mtime: pending.remote_mtime,
            local_size: metadata.len(),
            local_mtime: metadata.modified().unwrap_or_else(|_| SystemTime::now()),
            dirty: false,
            stable_since: None,
        };
        self.edits.retain(|current| current.remote != edit.remote);
        self.edits.push(edit);
        Some(self.edits.len() - 1)
    }

    /// Upload concluído. Só limpa dirty e atualiza o snapshot se a cópia
    /// local continuar idêntica ao instante em que o upload começou; se o
    /// editor salvou durante o envio, o remoto pode estar velho e o dirty fica.
    fn complete_edit_upload(&mut self, pending: &PendingEditUpload) {
        let Some((local, remote)) = self
            .edits
            .get(pending.edit)
            .map(|current| (current.local.clone(), current.remote.clone()))
        else {
            return;
        };
        if remote != pending.remote {
            return;
        }
        let current = std::fs::metadata(&local)
            .ok()
            .filter(|metadata| metadata.is_file());
        let unchanged = current.as_ref().is_some_and(|metadata| {
            metadata.len() == pending.local_size && metadata.modified().ok() == pending.local_mtime
        });
        if !unchanged {
            if let Some(edit) = self.edits.get_mut(pending.edit) {
                edit.dirty = true;
                edit.stable_since = None;
            }
            let name = remote_base_name(&pending.remote);
            self.status = Some(format!(
                "{name}: arquivo mudou durante o envio — envie de novo"
            ));
            return;
        }
        if let (Some(metadata), Some(edit)) = (current, self.edits.get_mut(pending.edit)) {
            edit.local_size = metadata.len();
            if let Ok(modified) = metadata.modified() {
                edit.local_mtime = modified;
            }
            edit.remote_size = metadata.len();
            edit.dirty = false;
            edit.stable_since = None;
        }
        let name = remote_base_name(&pending.remote);
        self.status = Some(format!("{name} enviado de volta"));
        self.request_edit_stat(pending.edit, EditStatPurpose::Refresh);
    }

    /// Enfileira um download de caminhos remotos para uma pasta local.
    pub fn enqueue_download(&mut self, sources: Vec<String>, dest: PathBuf) -> Option<u64> {
        if sources.is_empty() {
            self.status = Some("selecione itens para baixar".to_string());
            return None;
        }
        let label = label_remote(&sources);
        self.enqueue(
            Direction::Download,
            label,
            RetrySpec::Download { sources, dest },
        )
    }

    /// Enfileira um upload de caminhos locais para uma pasta remota.
    pub fn enqueue_upload(&mut self, sources: Vec<PathBuf>, dest: String) -> Option<u64> {
        if sources.is_empty() {
            self.status = Some("selecione itens para enviar".to_string());
            return None;
        }
        let label = label_local(&sources);
        self.enqueue(
            Direction::Upload,
            label,
            RetrySpec::Upload { sources, dest },
        )
    }

    fn enqueue(&mut self, direction: Direction, label: String, retry: RetrySpec) -> Option<u64> {
        if self.conn.is_none() {
            self.status = Some("sem conexão".to_string());
            return None;
        }
        let id = self.alloc_id();
        let conn = self.conn.as_ref().expect("conexão checada");
        let sent = match &retry {
            RetrySpec::Download { sources, dest } => conn.send(SftpCmd::Download {
                job_id: id,
                sources: sources.clone(),
                dest: dest.clone(),
                policy: ConflictPolicy::Ask,
            }),
            RetrySpec::Upload { sources, dest } => conn.send(SftpCmd::Upload {
                job_id: id,
                sources: sources.clone(),
                dest: dest.clone(),
                policy: ConflictPolicy::Ask,
            }),
        };
        if !sent {
            self.status = Some("conexão encerrada".to_string());
            return None;
        }
        self.jobs.push(JobView {
            id,
            direction,
            label,
            state: JobState::Queued,
            bytes_done: 0,
            bytes_total: None,
            items_done: 0,
            items_total: 0,
            ignored_links: 0,
            error: None,
            retry,
        });
        self.queue_open = true;
        self.status = Some("transferência enfileirada".to_string());
        Some(id)
    }

    /// Reenfileira um job que falhou com um id novo; o registro antigo fica
    /// no histórico até "Limpar concluídas".
    pub fn retry_job(&mut self, id: u64) {
        let Some(job) = self.jobs.iter().find(|job| job.id == id) else {
            return;
        };
        if job.state != JobState::Failed {
            return;
        }
        let direction = job.direction;
        let label = job.label.clone();
        let retry = job.retry.clone();
        // "Tentar de novo" de um envio de volta precisa passar pelo stat e
        // pela confirmação remota de novo; um upload avulso não atualizaria
        // o snapshot nem limparia o dirty.
        if let Some(edit) = self.edit_for_failed_upload(&retry) {
            self.request_send_back(edit);
            return;
        }
        self.enqueue(direction, label, retry);
    }

    /// Reconhece o RetrySpec de um upload gerado pelo envio de volta.
    fn edit_for_failed_upload(&self, retry: &RetrySpec) -> Option<usize> {
        let RetrySpec::Upload { sources, dest } = retry else {
            return None;
        };
        self.edits.iter().position(|edit| {
            sources.len() == 1
                && sources[0] == edit.local
                && remote_parent(&edit.remote).as_deref() == Some(dest.as_str())
        })
    }

    pub fn cancel_job(&mut self, id: u64) {
        let Some(job) = self.jobs.iter().find(|job| job.id == id) else {
            return;
        };
        if !job.state.is_active() {
            return;
        }
        let Some(conn) = &self.conn else {
            self.status = Some("sem conexão".to_string());
            return;
        };
        let _ = conn.send(SftpCmd::Cancel { job_id: id });
    }

    pub fn queued_jobs(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| job.state == JobState::Queued)
            .count()
    }

    pub fn running_jobs(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| job.state == JobState::Running)
            .count()
    }

    pub fn active_jobs(&self) -> usize {
        self.jobs.iter().filter(|job| job.state.is_active()).count()
    }

    pub fn has_active_jobs(&self) -> bool {
        self.jobs.iter().any(|job| job.state.is_active())
    }

    /// Edições com mudanças locais ainda não enviadas ao servidor.
    pub fn dirty_edits(&self) -> usize {
        self.edits.iter().filter(|edit| edit.dirty).count()
    }

    pub fn finished_jobs(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| !job.state.is_active())
            .count()
    }

    pub fn clear_finished(&mut self) {
        self.jobs.retain(|job| job.state.is_active());
    }

    /// Fechar com job ativo ou edição dirty exige confirmação; devolve true
    /// quando o modal foi aberto e o App não deve fechar agora.
    pub fn request_close(&mut self, id: u64) -> bool {
        if self.has_active_jobs() || self.dirty_edits() > 0 {
            self.confirm = Some(Confirm::CloseTab { id });
            true
        } else {
            false
        }
    }

    /// Abre o diálogo de criação de pasta da pane.
    pub fn request_new_folder(&mut self, remote: bool) {
        if remote && self.conn.is_none() {
            self.status = Some("sem conexão".to_string());
            return;
        }
        self.file_dialog = Some(FileDialog::NewFolder(NewFolderDialog {
            remote,
            name: String::new(),
            request_focus: true,
            error: None,
        }));
    }

    /// Abre o diálogo de permissões para a seleção remota.
    pub fn request_permissions(&mut self) {
        let Some((groups, mode, is_dir)) = permissions_for_selection(&self.remote) else {
            self.status = Some("selecione itens remotos".to_string());
            return;
        };
        self.file_dialog = Some(FileDialog::Permissions(PermissionsDialog {
            groups,
            mode_text: mode.map(format_octal).unwrap_or_default(),
            mixed: mode.is_none(),
            is_dir,
        }));
    }

    /// Pede a confirmação de exclusão da seleção da pane.
    pub fn request_delete_selection(&mut self, side: Side) {
        let paths = match side {
            Side::Local => selected_local_paths(&self.local)
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
            Side::Remote => selected_remote_paths(&self.remote),
        };
        self.request_delete(side == Side::Remote, paths);
    }

    /// Abre a confirmação de exclusão; recusa a raiz e seleção vazia.
    pub fn request_delete(&mut self, remote: bool, paths: Vec<String>) {
        let paths: Vec<String> = paths.into_iter().filter(|path| !path.is_empty()).collect();
        if paths.is_empty() {
            self.status = Some("selecione itens para excluir".to_string());
            return;
        }
        if paths.iter().any(|path| is_root_target(remote, path)) {
            self.status = Some("não é possível excluir a raiz".to_string());
            return;
        }
        self.confirm = Some(Confirm::Delete { remote, paths });
    }

    /// Cria uma pasta local ou manda Mkdir no remoto.
    pub fn create_folder(&mut self, remote: bool, name: &str) {
        let name = name.trim();
        let valid = if remote {
            valid_entry_name(name)
        } else {
            valid_local_entry_name(name)
        };
        if !valid {
            self.status = Some(if remote {
                "informe um nome não vazio, sem /, . ou ..".to_string()
            } else {
                "informe um nome sem /, \\, . ou ..".to_string()
            });
            return;
        }
        if remote {
            let path = remote_join(&self.remote.cwd, name);
            let id = self.alloc_id();
            let sent = self
                .conn
                .as_ref()
                .is_some_and(|conn| conn.send(SftpCmd::Mkdir { id, path }));
            if !sent {
                self.status = Some("sem conexão".to_string());
                return;
            }
            self.pending_ops.push(PendingOp {
                id,
                success: format!("pasta {name} criada"),
            });
            self.status = Some(format!("criando {name}…"));
            return;
        }
        let path = self.local.cwd.join(name);
        match std::fs::create_dir(&path) {
            Ok(()) => self.status = Some(format!("pasta {name} criada")),
            Err(error) => self.status = Some(format!("criando {}: {error}", path.display())),
        }
        self.reload_local();
    }

    /// Renomeia uma entrada local e re-lista a pane.
    pub fn rename_local(&mut self, from: PathBuf, to: PathBuf) {
        let label = to
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| to.display().to_string());
        match std::fs::rename(&from, &to) {
            Ok(()) => self.status = Some(format!("renomeado para {label}")),
            Err(error) => {
                self.status = Some(format!("renomeando {}: {error}", from.display()));
            }
        }
        self.reload_local();
    }

    /// Renomeia uma entrada remota e aguarda o OpDone.
    pub fn rename_remote(&mut self, from: String, to: String) {
        let label = remote_base_name(&to);
        let id = self.alloc_id();
        let sent = self
            .conn
            .as_ref()
            .is_some_and(|conn| conn.send(SftpCmd::Rename { id, from, to }));
        if !sent {
            self.status = Some("sem conexão".to_string());
            return;
        }
        self.pending_ops.push(PendingOp {
            id,
            success: format!("renomeado para {label}"),
        });
        self.status = Some(format!("renomeando para {label}…"));
    }

    /// Exclui localmente (recursivo, sem lixeira) ou manda Delete no remoto.
    pub fn delete_paths(&mut self, remote: bool, paths: &[String]) {
        if paths.is_empty() {
            self.status = Some("selecione itens para excluir".to_string());
            return;
        }
        if paths.iter().any(|path| is_root_target(remote, path)) {
            self.status = Some("não é possível excluir a raiz".to_string());
            return;
        }
        if remote {
            let id = self.alloc_id();
            let sent = self.conn.as_ref().is_some_and(|conn| {
                conn.send(SftpCmd::Delete {
                    id,
                    paths: paths.to_vec(),
                    recursive: true,
                })
            });
            if !sent {
                self.status = Some("sem conexão".to_string());
                return;
            }
            self.pending_ops.push(PendingOp {
                id,
                success: format!("{} item(ns) excluído(s)", paths.len()),
            });
            self.status = Some("excluindo…".to_string());
            return;
        }
        let mut errors = Vec::new();
        for path in paths {
            let path = PathBuf::from(path);
            let result = match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.file_type().is_dir() => std::fs::remove_dir_all(&path),
                Ok(_) => std::fs::remove_file(&path),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                errors.push(format!("excluindo {}: {error}", path.display()));
            }
        }
        self.reload_local();
        self.status = Some(match errors.first() {
            Some(error) => error.clone(),
            None => format!("{} item(ns) excluído(s)", paths.len()),
        });
    }

    /// Manda Chmod para os caminhos da seleção remota com o modo efetivo
    /// (3 dígitos editados + bits especiais preservados).
    pub fn apply_permissions(&mut self, paths: Vec<String>, mode: u32) {
        let mode = mode & 0o7777;
        let id = self.alloc_id();
        let sent = self
            .conn
            .as_ref()
            .is_some_and(|conn| conn.send(SftpCmd::Chmod { id, paths, mode }));
        if !sent {
            self.status = Some("sem conexão".to_string());
            return;
        }
        self.pending_ops.push(PendingOp {
            id,
            success: format!("permissões alteradas para {}", format_octal(mode)),
        });
        self.status = Some(format!("aplicando permissões {}…", format_octal(mode)));
    }

    fn take_pending_op(&mut self, id: u64) -> Option<PendingOp> {
        let index = self
            .pending_ops
            .iter()
            .position(|operation| operation.id == id)?;
        Some(self.pending_ops.remove(index))
    }

    fn finish_op(&mut self, id: u64, result: std::result::Result<(), String>) {
        let Some(operation) = self.take_pending_op(id) else {
            return;
        };
        match result {
            Ok(()) => {
                self.status = Some(operation.success);
                if self.conn.is_some() {
                    self.reload_remote();
                }
            }
            Err(error) => self.status = Some(error),
        }
    }

    pub fn copy_selection(&mut self, side: Side) {
        let paths = match side {
            Side::Local => ClipPaths::Local(paths_for_names(
                &self.local,
                &selected_names(&self.local.selected),
            )),
            Side::Remote => ClipPaths::Remote(paths_for_names(
                &self.remote,
                &selected_names(&self.remote.selected),
            )),
        };
        if paths.is_empty() {
            self.status = Some("selecione itens para copiar".to_string());
            return;
        }
        let count = paths.len();
        self.clip = Some(Clip { side, paths });
        self.status = Some(format!("{count} item(ns) copiado(s)"));
    }

    pub fn paste(&mut self, side: Side) {
        let Some(clip) = self.clip.clone() else {
            self.status = Some("clipboard vazio".to_string());
            return;
        };
        let Some(source) = paste_target(side, &clip) else {
            self.status = Some("copie para a outra pane".to_string());
            return;
        };
        match (source, clip.paths) {
            (Side::Remote, ClipPaths::Remote(sources)) => {
                let dest = self.local.cwd.clone();
                self.enqueue_download(sources, dest);
            }
            (Side::Local, ClipPaths::Local(sources)) => {
                let dest = self.remote.cwd.clone();
                self.enqueue_upload(sources, dest);
            }
            _ => self.status = Some("clipboard inválido".to_string()),
        }
    }

    /// Responde somente o primeiro conflito da fila: exatamente um
    /// ResolveConflict por prompt exibido.
    pub fn resolve_conflict(&mut self, decision: ConflictDecision, apply_all: bool) {
        let Some((job_id, decision, apply_all)) =
            pop_conflict_reply(&mut self.conflicts, decision, apply_all)
        else {
            return;
        };
        self.conflict_apply_all = false;
        if let Some(conn) = &self.conn {
            let _ = conn.send(SftpCmd::ResolveConflict {
                job_id,
                decision,
                apply_all,
            });
        } else {
            self.status = Some("sem conexão".to_string());
        }
    }
}

/// Retira o primeiro prompt e devolve os dados de um único ResolveConflict.
fn pop_conflict_reply(
    conflicts: &mut Vec<ConflictPrompt>,
    decision: ConflictDecision,
    apply_all: bool,
) -> Option<(u64, ConflictDecision, bool)> {
    let prompt = conflicts.first()?;
    let job_id = prompt.job_id;
    conflicts.remove(0);
    Some((job_id, decision, apply_all))
}

/// Copiar/colar só faz sentido para a outra pane.
pub fn paste_target(side: Side, clip: &Clip) -> Option<Side> {
    (clip.side != side).then_some(clip.side)
}

/// Soltar numa pasta usa a pasta; soltar no vazio usa o cwd da pane.
pub fn drop_destination(cwd: &str, hovered_folder: Option<&str>) -> String {
    hovered_folder.unwrap_or(cwd).to_string()
}

fn selected_names(selected: &BTreeSet<String>) -> Vec<String> {
    selected.iter().cloned().collect()
}

fn paths_for_names<P: Clone>(state: &PaneState<P>, names: &[String]) -> Vec<P> {
    let Listing::Ready(entries) = &state.listing else {
        return Vec::new();
    };
    names
        .iter()
        .filter_map(|name| {
            entries
                .iter()
                .find(|entry| &entry.name == name)
                .map(|entry| entry.path.clone())
        })
        .collect()
}

fn selected_local_paths(state: &PaneState<PathBuf>) -> Vec<PathBuf> {
    paths_for_names(state, &selected_names(&state.selected))
}

fn selected_remote_paths(state: &PaneState<String>) -> Vec<String> {
    paths_for_names(state, &selected_names(&state.selected))
}

/// A raiz local/remota não pode ser excluída.
fn is_root_target(remote: bool, path: &str) -> bool {
    if remote {
        path == "/"
    } else {
        Path::new(path).parent().is_none()
    }
}

/// Junta um nome validado ao cwd remoto (que é absoluto e normalizado).
fn remote_join(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn entry_by_name<'a, P>(state: &'a PaneState<P>, name: &str) -> Option<&'a Entry<P>> {
    let Listing::Ready(entries) = &state.listing else {
        return None;
    };
    entries.iter().find(|entry| entry.name == name)
}

fn listing_has<P>(listing: &Listing<P>, name: &str) -> bool {
    matches!(listing, Listing::Ready(entries) if entries.iter().any(|entry| entry.name == name))
}

fn validate_new_folder_name<P>(listing: &Listing<P>, name: &str, local: bool) -> Option<String> {
    let name = name.trim();
    let valid = if local {
        valid_local_entry_name(name)
    } else {
        valid_entry_name(name)
    };
    if !valid {
        return Some(if local {
            "informe um nome sem /, \\, . ou ..".to_string()
        } else {
            "informe um nome não vazio, sem /, . ou ..".to_string()
        });
    }
    if listing_has(listing, name) {
        return Some("já existe um item com esse nome".to_string());
    }
    None
}

fn rename_problem<P>(entries: &[Entry<P>], from: &str, to: &str, local: bool) -> Option<String> {
    let to = to.trim();
    let valid = if local {
        valid_local_entry_name(to)
    } else {
        valid_entry_name(to)
    };
    if !valid {
        return Some(if local {
            "nome inválido: vazio, com /, \\, . ou ..".to_string()
        } else {
            "nome inválido: vazio, com /, . ou ..".to_string()
        });
    }
    if to != from && entries.iter().any(|entry| entry.name == to) {
        return Some("já existe um item com esse nome".to_string());
    }
    None
}

fn begin_rename<P>(state: &mut PaneState<P>, name: String) {
    state.rename = Some(RowRename {
        input: name.clone(),
        from: name,
        request_focus: true,
        error: None,
    });
}

/// Seleção remota para o Chmod: grupos por bits especiais, modo comum (ou
/// None) e tipo. Cada grupo recebe um Chmod com seus bits preservados.
fn permissions_for_selection(
    state: &PaneState<String>,
) -> Option<(Vec<PermissionGroup>, Option<u32>, bool)> {
    let Listing::Ready(entries) = &state.listing else {
        return None;
    };
    let selected: Vec<&Entry<String>> = entries
        .iter()
        .filter(|entry| state.selected.contains(&entry.name))
        .collect();
    if selected.is_empty() {
        return None;
    }
    let first_mode = selected[0].mode.map(|mode| mode & 0o777);
    let mixed = selected
        .iter()
        .any(|entry| entry.mode.map(|mode| mode & 0o777) != first_mode);
    let mut by_special: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for entry in &selected {
        let special = entry.mode.unwrap_or(0) & 0o7000;
        by_special
            .entry(special)
            .or_default()
            .push(entry.path.clone());
    }
    let groups = by_special
        .into_iter()
        .map(|(special, paths)| PermissionGroup { paths, special })
        .collect();
    Some((
        groups,
        if mixed { None } else { first_mode },
        selected[0].kind == EntryKind::Dir,
    ))
}

fn permission_preview(dialog: &PermissionsDialog) -> String {
    parse_octal(&dialog.mode_text)
        .map(|mode| mode_string(mode, dialog.is_dir))
        .unwrap_or_else(|| "—".to_string())
}

fn remote_base_name(path: &str) -> String {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(path)
        .to_string()
}

fn label_remote(sources: &[String]) -> String {
    match sources {
        [one] => remote_base_name(one),
        many => format!("{} itens", many.len()),
    }
}

fn label_local(sources: &[PathBuf]) -> String {
    match sources {
        [one] => one
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| one.display().to_string()),
        many => format!("{} itens", many.len()),
    }
}

fn dropped_local_files(ui: &egui::Ui) -> Vec<PathBuf> {
    ui.input(|input| {
        input
            .raw
            .dropped_files
            .iter()
            .filter_map(|file| file.path.clone())
            .collect()
    })
}

/// Pastas primeiro, depois nome case-insensitive; empate preserva a caixa.
// O brief fixa a assinatura com &mut Vec; uma slice mudaria a interface.
#[allow(clippy::ptr_arg)]
pub fn sort_entries<P>(entries: &mut Vec<Entry<P>>) {
    entries.sort_by(|left, right| {
        let left_dir = left.kind == EntryKind::Dir;
        let right_dir = right.kind == EntryKind::Dir;
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
}

/// Segmentos clicáveis de um caminho POSIX: "/" vira ["/"].
pub fn breadcrumb(path: &str) -> Vec<String> {
    let mut parts = Vec::new();
    if path.starts_with('/') {
        parts.push("/".to_string());
    }
    for segment in path.split('/') {
        if !segment.is_empty() {
            parts.push(segment.to_string());
        }
    }
    if parts.is_empty() {
        parts.push("/".to_string());
    }
    parts
}

/// Tamanho legível (1024-based).
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Aceita "0" ou exatamente três dígitos octais (000–777).
pub fn parse_octal(input: &str) -> Option<u32> {
    let trimmed = input.trim();
    if trimmed == "0" {
        return Some(0);
    }
    if trimmed.len() != 3 || !trimmed.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return None;
    }
    u32::from_str_radix(trimmed, 8).ok()
}

/// Três dígitos octais; bits especiais (setuid/sticky) não são editados.
pub fn format_octal(mode: u32) -> String {
    format!("{:03o}", mode & 0o777)
}

/// Nome de entrada remoto: não vazio, sem "/" e sem "."/"..".
/// A barra invertida é permitida (é um caractere POSIX válido).
pub fn valid_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

/// Nome de entrada local. Além das regras remotas, recusa "\" e, no
/// Windows, prefixos de unidade (ex.: "C:x") para não escapar do cwd.
pub fn valid_local_entry_name(name: &str) -> bool {
    if !valid_entry_name(name) || name.contains('\\') {
        return false;
    }
    #[cfg(windows)]
    {
        let bytes = name.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            return false;
        }
    }
    true
}

/// Permissões no formato ls (rwx). Bits especiais não são exibidos.
pub fn mode_string(mode: u32, is_dir: bool) -> String {
    let mut text = String::with_capacity(10);
    text.push(if is_dir { 'd' } else { '-' });
    for shift in [6, 3, 0] {
        text.push(if mode & (0o4 << shift) != 0 { 'r' } else { '-' });
        text.push(if mode & (0o2 << shift) != 0 { 'w' } else { '-' });
        text.push(if mode & (0o1 << shift) != 0 { 'x' } else { '-' });
    }
    text
}

/// Valida um caminho remoto digitado. Aceita /, ~ e ~/; devolve o caminho
/// normalizado quando absoluto.
pub fn validate_remote_input(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }
    if trimmed == "~" || trimmed.starts_with("~/") {
        return Some(trimmed.to_string());
    }
    normalize_remote(trimmed)
}

/// Valida um caminho local digitado. Aceita ~, ~/… e caminhos absolutos.
pub fn validate_local_input(input: &str, home: &Path) -> Option<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return None;
    }
    let path = if trimmed == "~" {
        home.to_path_buf()
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        home.join(rest)
    } else if trimmed.starts_with('~') {
        return None;
    } else {
        PathBuf::from(trimmed)
    };
    path.is_absolute().then_some(path)
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn read_local(path: &Path) -> Listing<PathBuf> {
    let read_dir = match std::fs::read_dir(path) {
        Ok(read_dir) => read_dir,
        Err(error) => return Listing::Error(format!("lendo {}: {error}", path.display())),
    };
    let mut entries = Vec::new();
    for item in read_dir.flatten() {
        let name = item.file_name().to_string_lossy().into_owned();
        if name.is_empty() {
            continue;
        }
        let path = item.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let file_type = metadata.file_type();
        let kind = if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            EntryKind::File
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::Other
        };
        let link_target = if kind == EntryKind::Symlink {
            std::fs::read_link(&path)
                .ok()
                .map(|target| target.to_string_lossy().into_owned())
        } else {
            None
        };
        entries.push(Entry {
            name,
            path,
            kind,
            size: metadata.len(),
            modified: metadata.modified().ok().and_then(epoch_seconds),
            mode: mode_of(&metadata),
            link_target,
        });
    }
    sort_entries(&mut entries);
    Listing::Ready(entries)
}

fn entry_from_sftp(entry: SftpEntry) -> Entry<String> {
    Entry {
        name: entry.name,
        path: entry.path,
        kind: entry.kind,
        size: entry.size,
        modified: entry.modified,
        mode: entry.mode,
        link_target: entry.link_target,
    }
}

#[cfg(unix)]
fn mode_of(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn mode_of(_: &std::fs::Metadata) -> Option<u32> {
    None
}

fn epoch_seconds(time: SystemTime) -> Option<u32> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u32::try_from(duration.as_secs()).ok())
}

fn format_modified(modified: Option<u32>) -> String {
    let Some(seconds) = modified else {
        return "—".to_string();
    };
    let Some(utc) = DateTime::<Utc>::from_timestamp(i64::from(seconds), 0) else {
        return "—".to_string();
    };
    utc.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

fn visible_indices<P>(entries: &[Entry<P>], filter: &str, hidden: bool) -> Vec<usize> {
    let needle = filter.trim().to_lowercase();
    entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| {
            (hidden || !entry.name.starts_with('.'))
                && (needle.is_empty() || entry.name.to_lowercase().contains(&needle))
        })
        .map(|(index, _)| index)
        .collect()
}

fn entry_by_visible<P>(
    state: &PaneState<P>,
    hidden: bool,
    visible_position: usize,
) -> Option<&Entry<P>> {
    let Listing::Ready(entries) = &state.listing else {
        return None;
    };
    let visible = visible_indices(entries, &state.filter, hidden);
    let index = *visible.get(visible_position)?;
    entries.get(index)
}

fn open_position<P>(
    selected: &BTreeSet<String>,
    anchor: Option<usize>,
    entries: &[Entry<P>],
    visible: &[usize],
) -> Option<usize> {
    if let Some(anchor) = anchor {
        if let Some(&index) = visible.get(anchor) {
            if selected.contains(&entries[index].name) {
                return Some(anchor);
            }
        }
    }
    selected.iter().find_map(|name| {
        visible
            .iter()
            .position(|&index| &entries[index].name == name)
    })
}

fn remote_parent(path: &str) -> Option<String> {
    if path.is_empty() || path == "/" {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(index) => Some(trimmed[..index].to_string()),
        None => Some("/".to_string()),
    }
}

fn resolve_remote_input(input: &str, home: Option<&str>) -> Option<String> {
    let valid = validate_remote_input(input)?;
    let expanded = if valid == "~" {
        home?.to_string()
    } else if let Some(rest) = valid.strip_prefix("~/") {
        let home = home?;
        if home == "/" {
            format!("/{rest}")
        } else {
            format!("{}/{}", home.trim_end_matches('/'), rest)
        }
    } else {
        valid
    };
    normalize_remote(&expanded)
}

fn local_crumbs(path: &Path) -> Vec<(String, PathBuf)> {
    let display = path.to_string_lossy();
    let display = if cfg!(windows) {
        display.replace('\\', "/")
    } else {
        display.into_owned()
    };
    let mut current = PathBuf::new();
    let mut crumbs = Vec::new();
    for label in breadcrumb(&display) {
        if label == "/" {
            current = PathBuf::from("/");
        } else if cfg!(windows) && label.len() == 2 && label.ends_with(':') {
            current = PathBuf::from(format!("{label}/"));
        } else {
            current.push(&label);
        }
        crumbs.push((label, current.clone()));
    }
    crumbs
}

fn remote_crumbs(path: &str) -> Vec<(String, String)> {
    let mut current = String::new();
    let mut crumbs = Vec::new();
    for label in breadcrumb(path) {
        if label == "/" {
            current = "/".to_string();
        } else if current == "/" {
            current = format!("/{label}");
        } else {
            current = format!("{current}/{label}");
        }
        crumbs.push((label, current.clone()));
    }
    crumbs
}

/// Raiz das cópias locais da edição remota: <temp do SO>/sshc-sftp-edit.
pub fn edit_temp_root() -> PathBuf {
    std::env::temp_dir().join(EDIT_ROOT)
}

/// Garante que a raiz da edição é uma pasta real (nunca um link simbólico)
/// e, no Unix, privada (0700): o temp do SO é compartilhado e um link plantado
/// ali não pode virar escrita nem remoção recursiva no alvo.
fn ensure_edit_root(root: &Path) -> anyhow::Result<()> {
    ensure_edit_dir(root, "raiz de edição")
}

/// Garante a pasta da sessão sob a raiz com a mesma verificação de link e
/// modo. Um link simbólico no lugar dela é removido sem seguir o alvo.
fn ensure_edit_session_dir(root: &Path, key: &str) -> anyhow::Result<PathBuf> {
    if key.is_empty() || !valid_local_entry_name(key) {
        anyhow::bail!("chave de edição inválida: {key:?}");
    }
    let dir = root.join(key);
    ensure_edit_dir(&dir, "pasta da sessão")?;
    Ok(dir)
}

/// Verifica/cria uma pasta de edição. Links são removidos sem seguir o alvo;
/// se o modo não for privado, tenta corrigir e recusa quando não é possível.
fn ensure_edit_dir(dir: &Path, what: &str) -> anyhow::Result<()> {
    for _ in 0..3 {
        match std::fs::symlink_metadata(dir) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                remove_symlink(dir).map_err(|error| {
                    anyhow::anyhow!("removendo link {what} {}: {error}", dir.display())
                })?;
            }
            Ok(metadata) if metadata.is_dir() => {
                make_edit_dir_private(dir, &metadata)?;
                return verify_edit_dir(dir, what);
            }
            Ok(_) => anyhow::bail!("{what} não é uma pasta: {}", dir.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match create_private_dir(dir) {
                    Ok(()) => return verify_edit_dir(dir, what),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => {
                        anyhow::bail!("criando {what} {}: {error}", dir.display());
                    }
                }
            }
            Err(error) => anyhow::bail!("lendo {what} {}: {error}", dir.display()),
        }
    }
    anyhow::bail!("{what} mudou durante a preparação: {}", dir.display())
}

fn verify_edit_dir(dir: &Path, what: &str) -> anyhow::Result<()> {
    let metadata = std::fs::symlink_metadata(dir)
        .map_err(|error| anyhow::anyhow!("lendo {what} {}: {error}", dir.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        anyhow::bail!("{what} não é uma pasta real: {}", dir.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!("{what} não é privada (modo {mode:o}): {}", dir.display());
        }
    }
    Ok(())
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700).create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(dir)
    }
}

fn make_edit_dir_private(dir: &Path, metadata: &std::fs::Metadata) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(
                |error| {
                    anyhow::anyhow!(
                        "tornando privada {} (modo {mode:o}): {error}",
                        dir.display()
                    )
                },
            )?;
        }
    }
    #[cfg(not(unix))]
    let _ = metadata;
    Ok(())
}

/// Remove o link em si; em alguns sistemas um link para pasta exige rmdir.
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(first) => match std::fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(_) => Err(first),
        },
    }
}

/// Identificador da sessão na árvore temporária: host_port_user, com tudo
/// que não for seguro em nome de pasta trocado por "_".
fn edit_key(session: &Session) -> String {
    let raw = format!(
        "{}_{}_{}",
        session.host,
        session.port,
        session.user.as_deref().unwrap_or("")
    );
    raw.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Caminho local da cópia de remote em root/key/caminho relativo.
/// Reusa a proteção contra travessia de sshc_net::sftp::local_dest.
pub fn edit_temp_path(root: &Path, key: &str, remote: &str) -> anyhow::Result<PathBuf> {
    if key.is_empty() || !valid_local_entry_name(key) {
        anyhow::bail!("chave de edição inválida: {key:?}");
    }
    let Some(relative) = remote.strip_prefix('/') else {
        anyhow::bail!("caminho remoto inválido: {remote:?}");
    };
    local_dest(&root.join(key), relative)
}

/// O arquivo remoto mudou desde o snapshot? Tamanho ou mtime diferentes.
pub fn changed_since(
    size: u64,
    mtime: Option<u32>,
    snapshot_size: u64,
    snapshot_mtime: Option<u32>,
) -> bool {
    size != snapshot_size || mtime != snapshot_mtime
}

/// Algum item da listagem remota conflita com o snapshot da edição? Um
/// arquivo que sumiu do servidor conta como mudança.
fn remote_changed(entries: &[SftpEntry], edit: &EditedFile) -> bool {
    match entries.iter().find(|entry| entry.path == edit.remote) {
        Some(entry) => changed_since(
            entry.size,
            entry.modified,
            edit.remote_size,
            edit.remote_mtime,
        ),
        None => true,
    }
}

/// Remove da raiz temporária as pastas de sessão sem atividade há mais de
/// max_age. A idade é o mtime mais recente dentro da pasta.
pub fn cleanup_edit_temp(root: &Path, max_age: Duration) {
    cleanup_edit_temp_at(root, SystemTime::now(), max_age);
}

fn cleanup_edit_temp_at(root: &Path, now: SystemTime, max_age: Duration) {
    // A raiz precisa ser uma pasta real antes de qualquer leitura: read_dir
    // de um link seguiria o alvo e a limpeza apagaria arquivos da vítima.
    if ensure_edit_root(root).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // file_type() usa o d_type do readdir e nunca segue link simbólico.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let newest = newest_mtime(&path).unwrap_or(UNIX_EPOCH);
        let too_old = now
            .duration_since(newest)
            .map(|age| age > max_age)
            .unwrap_or(false);
        if !too_old {
            continue;
        }
        if file_type.is_symlink() {
            // Remove só o link; o alvo nunca é visitado.
            let _ = remove_symlink(&path);
        } else if file_type.is_dir() {
            // Confere de novo: se o caminho virou link depois do readdir,
            // remove_dir_all não pode atravessá-lo.
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    let _ = std::fs::remove_dir_all(&path);
                }
                Ok(_) => {}
                Err(_) => {}
            }
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Mtime mais recente de path e de tudo abaixo dele (sem seguir links).
fn newest_mtime(path: &Path) -> Option<SystemTime> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let mut newest = metadata.modified().ok();
    if !metadata.is_dir() {
        return newest;
    }
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let child = entry.path();
            let Ok(metadata) = std::fs::symlink_metadata(&child) else {
                continue;
            };
            if let Ok(modified) = metadata.modified() {
                newest = Some(match newest {
                    Some(previous) => previous.max(modified),
                    None => modified,
                });
            }
            if metadata.is_dir() {
                pending.push(child);
            }
        }
    }
    newest
}

/// Programa e argumentos para abrir um caminho no app padrão do SO.
/// Windows usa explorer.exe com o caminho como um único argumento: nada de
/// cmd /C start, que reparsearia & | ^ < > % e permitiria injeção.
fn opener_command(path: &Path) -> (&'static str, Vec<OsString>) {
    #[cfg(target_os = "windows")]
    let program = "explorer.exe";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    #[cfg(not(any(unix, windows)))]
    let program = "open";
    (program, vec![path.as_os_str().to_os_string()])
}

/// Abre um caminho no app padrão do SO (sem shell, sem bloquear a UI).
fn open_with_os(path: &Path) -> std::io::Result<()> {
    let (program, args) = opener_command(path);
    let mut command = std::process::Command::new(program);
    command.args(args);
    let mut child = command.spawn()?;
    // Não bloqueia a UI esperando o app do SO fechar.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Ação do menu de uma pane (botão ☰ ou clique direito).
#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuAction {
    Open(String),
    Edit(String),
    NewFolder,
    Rename(String),
    Delete,
    Permissions,
    Copy,
    CopyPath,
    Refresh,
    DownloadTo,
    UploadSelection,
    UploadFiles,
    UploadFolder,
}

/// Ação que uma pane pede à view.
enum PaneAction {
    None,
    NavigateLocal(PathBuf),
    NavigateRemote(String),
    OpenLocal(PathBuf),
    EditRemote(String),
    ReloadLocal,
    ReloadRemote,
    CopySelection,
    Paste,
    /// Envia a seleção local para o cwd remoto.
    UploadSelection(Vec<PathBuf>),
    /// Baixa a seleção remota para o cwd local.
    DownloadSelection(Vec<String>),
    /// Upload de uma queda (arrastar/OS) para um destino remoto explícito.
    Upload {
        sources: Vec<PathBuf>,
        dest: String,
    },
    /// Download de uma queda para um destino local explícito.
    Download {
        sources: Vec<String>,
        dest: PathBuf,
    },
    NewFolder {
        remote: bool,
    },
    DeleteSelection {
        remote: bool,
    },
    RenameLocal {
        from: PathBuf,
        to: PathBuf,
    },
    RenameRemote {
        from: String,
        to: String,
    },
    Permissions,
    CopyPath(Side),
    /// Abre o seletor nativo de arquivos (folder=false) ou de pasta.
    UploadPick {
        folder: bool,
    },
    /// Abre o seletor de pasta de destino para baixar a seleção remota.
    DownloadTo,
    Status(String),
}

/// Ação pedida pela lista de uma pane. O destino de uma queda é genérico
/// porque a pane local usa PathBuf e a remota usa String.
enum ListAction<D> {
    None,
    Open(usize),
    Up,
    EditPath,
    Reload,
    Copy,
    Paste,
    Delete,
    Rename {
        from: String,
        to: String,
    },
    Menu(MenuAction),
    Status(String),
    StartDrag(Vec<String>),
    Drop {
        folder: Option<D>,
        clip: Clip,
    },
    OsDrop {
        folder: Option<D>,
        sources: Vec<PathBuf>,
    },
}

/// Larguras fixas das colunas; o nome ocupa o resto.
struct Columns {
    name: f32,
    size: f32,
    modified: f32,
    mode: f32,
}

impl Columns {
    fn new(width: f32, remote: bool) -> Self {
        let size = 78.0;
        let modified = 128.0;
        let mode = if remote { 84.0 } else { 0.0 };
        Self {
            name: (width - size - modified - mode - 12.0).max(40.0),
            size,
            modified,
            mode,
        }
    }
}

/// Retângulo da linha alvo em coordenadas de tela. O show_rows posiciona o
/// viewport na primeira linha visível (range.start), então linhas fora da
/// viewport também podem ser calculadas para o scroll_to_rect.
fn row_rect_in_viewport(
    viewport_top: f32,
    left: f32,
    width: f32,
    first_row: usize,
    target_row: usize,
) -> egui::Rect {
    let top = viewport_top + (target_row as f32 - first_row as f32) * ROW_HEIGHT;
    egui::Rect::from_min_size(
        egui::pos2(left, top),
        egui::vec2(width.max(1.0), ROW_HEIGHT),
    )
}

/// Cabeçalho da aba: nome, endpoint, estado e ações.
pub fn sftp_view(
    ui: &mut egui::Ui,
    tab: &mut SftpTab,
    session: &Session,
    app_modal_open: bool,
) -> SftpAction {
    let mut action = SftpAction::None;
    // Com qualquer diálogo aberto as teclas da pane não podem agir por baixo.
    let keyboard_blocked = app_modal_open || tab.has_modal();

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.heading(&session.name);
        ui.label(egui::RichText::new(session.endpoint()).weak());
        match &tab.state {
            ConnState::Connecting => {
                ui.spinner();
                ui.label("conectando…");
            }
            ConnState::Connected => {
                ui.label(egui::RichText::new("conectado").color(OK).strong());
            }
            ConnState::Closed(reason) => {
                ui.colored_label(ERROR, format!("conexão encerrada: {reason}"));
            }
            ConnState::Failed(error) => {
                ui.colored_label(ERROR, error);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if ui.button("Fechar").clicked() {
                action = SftpAction::Close;
            }
            match &tab.state {
                ConnState::Connected => {
                    if ui.button("Desconectar").clicked() {
                        action = SftpAction::Disconnect;
                    }
                }
                ConnState::Closed(_) | ConnState::Failed(_) => {
                    if ui.button("Reconectar").clicked() {
                        action = SftpAction::Reconnect;
                    }
                    if matches!(tab.state, ConnState::Failed(_))
                        && ui.button("Informar senha…").clicked()
                    {
                        action = SftpAction::NeedAuth;
                    }
                }
                ConnState::Connecting => {}
            }
            if ui.button("Atualizar").clicked() {
                tab.reload_local();
                tab.reload_remote();
            }
        });
    });

    if let Some(note) = &tab.note {
        if !note.trim().is_empty() {
            ui.label(egui::RichText::new(note).weak().small());
        }
    }

    ui.separator();
    ui.horizontal(|ui| {
        let response = ui
            .checkbox(&mut tab.hidden, "Mostrar ocultos")
            .on_hover_text("arquivos que começam com ponto");
        if response.changed() {
            tab.mark_layout_changed();
        }
    });
    edits_panel(ui, tab);
    ui.separator();

    // A fila e o rodapé são um painel inferior: as panes ficam com o que
    // sobrar, então abrir a fila nunca empurra a barra de status para fora.
    egui::Panel::bottom("sftp-bottom")
        .frame(egui::Frame::NONE)
        .show(ui, |ui| {
            queue_panel(ui, tab);
            ui.separator();
            footer(ui, tab);
        });

    let home = home_dir();
    let remote_home = tab.home.clone();
    let connected = tab.conn.is_some() && matches!(tab.state, ConnState::Connected);
    let remote_note = if connected {
        None
    } else if matches!(tab.state, ConnState::Connecting) {
        Some("conectando…")
    } else {
        Some("sem conexão — use Reconectar")
    };

    let width = ui.available_width();
    let height = ui.available_height().max(120.0);
    let usable = (width - SPLIT_HANDLE).max(1.0);
    let split = tab.split.clamp(SPLIT_MIN, SPLIT_MAX);
    let left_width = (usable * split).max(40.0);
    let right_width = (usable - left_width).max(40.0);

    let mut local_action = PaneAction::None;
    let mut remote_action = PaneAction::None;

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;

        let local_focused = tab.focus == Side::Local;
        let local = ui
            .allocate_ui_with_layout(
                egui::vec2(left_width, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::same(4))
                        .corner_radius(4.0)
                        .stroke(pane_stroke(ui, local_focused))
                        .show(ui, |ui| {
                            local_pane(
                                ui,
                                &mut tab.local,
                                tab.hidden,
                                &home,
                                &mut tab.focus,
                                keyboard_blocked,
                            )
                        })
                        .inner
                },
            )
            .inner;
        local_action = local;

        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(SPLIT_HANDLE, height),
            egui::Sense::click_and_drag(),
        );
        let response = response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if ui.is_rect_visible(rect) {
            let color = if response.hovered() || response.dragged() {
                ACCENT
            } else {
                ui.visuals().widgets.noninteractive.bg_stroke.color
            };
            ui.painter().rect_filled(
                egui::Rect::from_center_size(rect.center(), egui::vec2(2.0, rect.height())),
                1.0,
                color,
            );
        }
        if response.dragged() {
            tab.split = (tab.split + response.drag_delta().x / usable).clamp(SPLIT_MIN, SPLIT_MAX);
        }
        // Grava só quando solta a divisória, não a cada frame do arraste.
        if response.drag_stopped() {
            tab.mark_layout_changed();
        }

        let remote_focused = tab.focus == Side::Remote;
        let remote = ui
            .allocate_ui_with_layout(
                egui::vec2(right_width, height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    egui::Frame::NONE
                        .inner_margin(egui::Margin::same(4))
                        .corner_radius(4.0)
                        .stroke(pane_stroke(ui, remote_focused))
                        .show(ui, |ui| {
                            remote_pane(
                                ui,
                                &mut tab.remote,
                                tab.hidden,
                                remote_home.as_deref(),
                                remote_note,
                                &mut tab.focus,
                                keyboard_blocked,
                            )
                        })
                        .inner
                },
            )
            .inner;
        remote_action = remote;
    });

    match local_action {
        PaneAction::NavigateLocal(path) => tab.navigate_local(path),
        PaneAction::ReloadLocal => tab.reload_local(),
        PaneAction::OpenLocal(path) => match open_with_os(&path) {
            Ok(()) => tab.status = Some(format!("abrindo {}", path.display())),
            Err(error) => tab.status = Some(format!("não abri {}: {error}", path.display())),
        },
        PaneAction::CopySelection => tab.copy_selection(Side::Local),
        PaneAction::Paste => tab.paste(Side::Local),
        PaneAction::UploadSelection(sources) => {
            let dest = tab.remote.cwd.clone();
            tab.enqueue_upload(sources, dest);
        }
        PaneAction::Download { sources, dest } => {
            tab.enqueue_download(sources, dest);
        }
        PaneAction::NewFolder { remote } => tab.request_new_folder(remote),
        PaneAction::DeleteSelection { remote } => {
            tab.request_delete_selection(if remote { Side::Remote } else { Side::Local });
        }
        PaneAction::RenameLocal { from, to } => tab.rename_local(from, to),
        PaneAction::CopyPath(side) => copy_paths_text(ui, tab, side),
        PaneAction::UploadPick { folder } => pick_upload(tab, folder),
        PaneAction::DownloadTo => pick_download_to(tab),
        PaneAction::Status(message) => tab.status = Some(message),
        _ => {}
    }
    match remote_action {
        PaneAction::NavigateRemote(path) => tab.navigate_remote(path),
        PaneAction::ReloadRemote => tab.reload_remote(),
        PaneAction::EditRemote(path) => {
            let snapshot = remote_snapshot(&tab.remote, &path);
            tab.begin_edit(session, path, snapshot);
        }
        PaneAction::CopySelection => tab.copy_selection(Side::Remote),
        PaneAction::Paste => tab.paste(Side::Remote),
        PaneAction::DownloadSelection(sources) => {
            let dest = tab.local.cwd.clone();
            tab.enqueue_download(sources, dest);
        }
        PaneAction::Upload { sources, dest } => {
            tab.enqueue_upload(sources, dest);
        }
        PaneAction::NewFolder { remote } => tab.request_new_folder(remote),
        PaneAction::DeleteSelection { remote } => {
            tab.request_delete_selection(if remote { Side::Remote } else { Side::Local });
        }
        PaneAction::RenameRemote { from, to } => tab.rename_remote(from, to),
        PaneAction::Permissions => tab.request_permissions(),
        PaneAction::CopyPath(side) => copy_paths_text(ui, tab, side),
        PaneAction::DownloadTo => pick_download_to(tab),
        PaneAction::Status(message) => tab.status = Some(message),
        _ => {}
    }

    action
}

/// Copia os caminhos da seleção como texto (um por linha).
fn copy_paths_text(ui: &egui::Ui, tab: &mut SftpTab, side: Side) {
    let text = match side {
        Side::Local => selected_local_paths(&tab.local)
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        Side::Remote => selected_remote_paths(&tab.remote).join("\n"),
    };
    if text.is_empty() {
        tab.status = Some("selecione itens para copiar caminho".to_string());
        return;
    }
    ui.ctx().copy_text(text);
    tab.status = Some("caminhos copiados".to_string());
}

/// Enviar…: seletor nativo de arquivos ou de pasta, com o cwd local.
fn pick_upload(tab: &mut SftpTab, folder: bool) {
    if tab.conn.is_none() {
        tab.status = Some("sem conexão".to_string());
        return;
    }
    let dialog = rfd::FileDialog::new()
        .set_title("Enviar para o servidor")
        .set_directory(&tab.local.cwd);
    let sources: Vec<PathBuf> = if folder {
        dialog.pick_folder().into_iter().collect()
    } else {
        dialog.pick_files().unwrap_or_default()
    };
    if sources.is_empty() {
        return;
    }
    let dest = tab.remote.cwd.clone();
    tab.enqueue_upload(sources, dest);
}

/// Baixar para…: seletor nativo de pasta, com o cwd local como padrão.
fn pick_download_to(tab: &mut SftpTab) {
    let sources = selected_remote_paths(&tab.remote);
    if sources.is_empty() {
        tab.status = Some("selecione itens para baixar".to_string());
        return;
    }
    let Some(dest) = rfd::FileDialog::new()
        .set_title("Baixar para…")
        .set_directory(&tab.local.cwd)
        .pick_folder()
    else {
        return;
    };
    tab.enqueue_download(sources, dest);
}

/// Lista sempre visível dos arquivos em edição. Arquivos dirty têm o envio
/// de volta; todos podem ser reabertos ou descartados (o descarte é a saída
/// para uma cópia limpa que não é mais necessária). Nunca envia sozinho.
fn edits_panel(ui: &mut egui::Ui, tab: &mut SftpTab) {
    if tab.edits.is_empty() {
        return;
    }
    let dirty = tab.edits.iter().filter(|edit| edit.dirty).count();
    let mut send = None;
    let mut reopen = None;
    let mut discard = None;
    let fill = if dirty > 0 {
        WARN.gamma_multiply(0.16)
    } else {
        ui.visuals().faint_bg_color
    };
    egui::Frame::NONE
        .fill(fill)
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("Em edição ({})", tab.edits.len()))
                        .strong()
                        .small(),
                );
                if dirty > 0 {
                    ui.label(
                        egui::RichText::new(format!("{dirty} com mudanças"))
                            .color(WARN)
                            .small(),
                    );
                }
            });
            egui::ScrollArea::vertical()
                .id_salt("sftp-edits")
                .max_height(96.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for (index, edit) in tab.edits.iter().enumerate() {
                        let uploading = tab
                            .pending_edit_uploads
                            .iter()
                            .any(|pending| pending.edit == index);
                        let checking = tab
                            .pending_edit_stats
                            .iter()
                            .any(|pending| pending.edit == index);
                        let (state, color) = if edit.dirty {
                            ("arquivo mudou".to_string(), WARN)
                        } else if uploading {
                            ("enviando…".to_string(), ACCENT)
                        } else if checking {
                            ("verificando…".to_string(), ACCENT)
                        } else {
                            ("em edição".to_string(), ui.visuals().weak_text_color())
                        };
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(remote_base_name(&edit.remote)).small());
                            ui.label(egui::RichText::new(state).small().color(color));
                            if edit.dirty && ui.small_button("Enviar de volta").clicked() {
                                send = Some(index);
                            }
                            if ui.small_button("Reabrir").clicked() {
                                reopen = Some(index);
                            }
                            if ui.small_button("Descartar").clicked() {
                                discard = Some(index);
                            }
                            ui.add_sized(
                                [200.0, ROW_HEIGHT],
                                egui::Label::new(
                                    egui::RichText::new(edit.local.display().to_string())
                                        .weak()
                                        .small(),
                                )
                                .truncate(),
                            )
                            .on_hover_text("cópia local em edição");
                        });
                    }
                });
        });
    if let Some(index) = send {
        tab.request_send_back(index);
    }
    if let Some(index) = reopen {
        tab.reopen_edit(index);
    }
    if let Some(index) = discard {
        tab.discard_edit(index);
    }
}

/// Snapshot remoto (tamanho/mtime) da listagem atual, para o download de edição.
fn remote_snapshot(state: &PaneState<String>, remote: &str) -> (u64, Option<u32>) {
    let Listing::Ready(entries) = &state.listing else {
        return (0, None);
    };
    entries
        .iter()
        .find(|entry| entry.path == remote)
        .map(|entry| (entry.size, entry.modified))
        .unwrap_or((0, None))
}

/// Painel colapsável de transferências no rodapé.
fn queue_panel(ui: &mut egui::Ui, tab: &mut SftpTab) {
    let mut cancel = None;
    let mut retry = None;
    let mut toggle = false;
    let mut clear = false;

    ui.horizontal(|ui| {
        let arrow = if tab.queue_open { "⏷" } else { "⏵" };
        if ui
            .small_button(arrow)
            .on_hover_text("mostrar/ocultar transferências")
            .clicked()
        {
            toggle = true;
        }
        ui.label(egui::RichText::new("Transferências").strong());
        ui.label(
            egui::RichText::new(format!(
                "{} ativas · {} na fila",
                tab.running_jobs(),
                tab.queued_jobs()
            ))
            .weak()
            .small(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add_enabled(
                    tab.finished_jobs() > 0,
                    egui::Button::new("Limpar concluídas").small(),
                )
                .clicked()
            {
                clear = true;
            }
        });
    });

    if tab.queue_open && !tab.jobs.is_empty() {
        egui::ScrollArea::vertical()
            .id_salt("sftp-jobs")
            .max_height(150.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for job in &tab.jobs {
                    ui.horizontal(|ui| {
                        let arrow = match job.direction {
                            Direction::Download => "⬇",
                            Direction::Upload => "⬆",
                        };
                        ui.label(arrow);
                        let progress = match (job.state, job.bytes_total) {
                            (JobState::Done, _) => 1.0,
                            (_, Some(total)) if total > 0 => {
                                (job.bytes_done as f32 / total as f32).clamp(0.0, 1.0)
                            }
                            _ => 0.0,
                        };
                        let mut bar = egui::ProgressBar::new(progress)
                            .desired_width(110.0)
                            .desired_height(10.0);
                        if job.bytes_total.is_none() && job.state.is_active() {
                            bar = bar.animate(true).text("…");
                        }
                        ui.add(bar);
                        let items = if job.items_total == 0 {
                            if job.state.is_active() {
                                "0/…".to_string()
                            } else {
                                "—".to_string()
                            }
                        } else {
                            format!("{}/{}", job.items_done, job.items_total)
                        };
                        ui.label(egui::RichText::new(items).small());
                        ui.add_sized([160.0, ROW_HEIGHT], egui::Label::new(&job.label).truncate())
                            .on_hover_text(&job.label);
                        let color = match job.state {
                            JobState::Done => OK,
                            JobState::Failed => ERROR,
                            JobState::Running => ACCENT,
                            _ => ui.visuals().weak_text_color(),
                        };
                        ui.label(egui::RichText::new(job.state.label()).small().color(color));
                        if job.state.is_active()
                            && ui
                                .small_button("×")
                                .on_hover_text("cancelar transferência")
                                .clicked()
                        {
                            cancel = Some(job.id);
                        }
                        if job.state == JobState::Failed
                            && ui.small_button("Tentar de novo").clicked()
                        {
                            retry = Some(job.id);
                        }
                    });
                    if job.ignored_links > 0 {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} links simbólicos ignorados",
                                job.ignored_links
                            ))
                            .small()
                            .weak(),
                        );
                    }
                    if let Some(error) = &job.error {
                        ui.label(egui::RichText::new(error).small().color(ERROR))
                            .on_hover_text(error);
                    }
                    ui.separator();
                }
            });
    }

    if toggle {
        tab.queue_open = !tab.queue_open;
    }
    if clear {
        tab.clear_finished();
    }
    if let Some(id) = cancel {
        tab.cancel_job(id);
    }
    if let Some(id) = retry {
        tab.retry_job(id);
    }
}

fn footer(ui: &mut egui::Ui, tab: &mut SftpTab) {
    ui.horizontal(|ui| {
        let local_count = match &tab.local.listing {
            Listing::Ready(entries) => {
                visible_indices(entries, &tab.local.filter, tab.hidden).len()
            }
            _ => 0,
        };
        let remote_count = match &tab.remote.listing {
            Listing::Ready(entries) => {
                visible_indices(entries, &tab.remote.filter, tab.hidden).len()
            }
            _ => 0,
        };
        ui.label(
            egui::RichText::new(format!(
                "local: {local_count} itens · {} selecionados",
                tab.local.selected.len()
            ))
            .weak()
            .small(),
        );
        ui.separator();
        ui.label(
            egui::RichText::new(format!(
                "remoto: {remote_count} itens · {} selecionados",
                tab.remote.selected.len()
            ))
            .weak()
            .small(),
        );
        if let Some(status) = &tab.status {
            ui.separator();
            ui.label(egui::RichText::new(status).small());
        }
    });
}

/// Modais da aba SFTP: confirmações, diálogos de operação e conflito.
/// Um modal por vez: confirmação > diálogo > conflito.
pub fn sftp_modals(ctx: &egui::Context, tab: &mut SftpTab) -> SftpAction {
    if let Some(confirm) = tab.confirm.clone() {
        return confirm_modal(ctx, tab, confirm);
    }
    if tab.file_dialog.is_some() {
        file_dialog_modal(ctx, tab);
        return SftpAction::None;
    }
    conflict_modal(ctx, tab);
    SftpAction::None
}

fn confirm_modal(ctx: &egui::Context, tab: &mut SftpTab, confirm: Confirm) -> SftpAction {
    match confirm {
        Confirm::CloseTab { id } => close_confirm_modal(ctx, tab, id),
        Confirm::Delete { remote, paths } => {
            delete_confirm_modal(ctx, tab, remote, &paths);
            SftpAction::None
        }
        Confirm::OverwriteRemote { edit } => {
            overwrite_remote_modal(ctx, tab, edit);
            SftpAction::None
        }
    }
}

/// Texto da confirmação de fechamento; cobre jobs ativos e edições que
/// serão perdidas sem envio.
fn close_confirm_message(active_jobs: usize, dirty_edits: usize) -> String {
    let mut message = String::new();
    if active_jobs > 0 {
        message.push_str(&format!(
            "{active_jobs} transferência(s) ativa(s) serão canceladas."
        ));
    }
    if dirty_edits > 0 {
        if !message.is_empty() {
            message.push(' ');
        }
        message.push_str(&format!(
            "{dirty_edits} edição(ões) com mudanças não enviadas serão perdidas."
        ));
    }
    message
}

fn close_confirm_modal(ctx: &egui::Context, tab: &mut SftpTab, id: u64) -> SftpAction {
    let mut confirmed = false;
    let mut cancelled = false;
    let message = close_confirm_message(tab.active_jobs(), tab.dirty_edits());
    egui::Modal::new(egui::Id::new("sftp-close-tab")).show(ctx, |ui| {
        ui.set_min_width(340.0);
        ui.heading("Fechar aba SFTP?");
        ui.add_space(4.0);
        ui.label(message);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Fechar aba").clicked() {
                confirmed = true;
            }
            if ui.button("Cancelar").clicked() {
                cancelled = true;
            }
        });
    });
    if confirmed {
        tab.confirm = None;
        SftpAction::CloseConfirmed(id)
    } else {
        if cancelled {
            tab.confirm = None;
        }
        SftpAction::None
    }
}

/// Nome curto exibido na confirmação de exclusão.
fn delete_display_name(remote: bool, path: &str) -> String {
    if remote {
        remote_base_name(path)
    } else {
        Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    }
}

fn delete_confirm_modal(ctx: &egui::Context, tab: &mut SftpTab, remote: bool, paths: &[String]) {
    let mut confirmed = false;
    let mut cancelled = false;
    egui::Modal::new(egui::Id::new("sftp-delete")).show(ctx, |ui| {
        ui.set_min_width(380.0);
        ui.heading(format!("Excluir {} item(ns)?", paths.len()));
        ui.add_space(4.0);
        for path in paths.iter().take(6) {
            ui.label(format!("• {}", delete_display_name(remote, path)));
        }
        if paths.len() > 6 {
            ui.label(egui::RichText::new(format!("… e mais {}", paths.len() - 6)).weak());
        }
        ui.add_space(6.0);
        let warning = if remote {
            "Pastas remotas são excluídas recursivamente. A exclusão é permanente."
        } else {
            "A exclusão é permanente (sem lixeira); pastas são removidas com todo o conteúdo."
        };
        ui.label(egui::RichText::new(warning).color(ERROR).small());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.add(egui::Button::new("Excluir").fill(ERROR)).clicked() {
                confirmed = true;
            }
            if ui.button("Cancelar").clicked() {
                cancelled = true;
            }
        });
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            cancelled = true;
        }
    });
    if confirmed {
        tab.confirm = None;
        tab.delete_paths(remote, paths);
    } else if cancelled {
        tab.confirm = None;
    }
}

/// Confirmação da Task 13: o envio de volta sobrescreve o remoto.
fn overwrite_remote_modal(ctx: &egui::Context, tab: &mut SftpTab, edit: usize) {
    let mut confirmed = false;
    let mut cancelled = false;
    egui::Modal::new(egui::Id::new("sftp-overwrite-remote")).show(ctx, |ui| {
        ui.set_min_width(420.0);
        ui.heading("Sobrescrever arquivo remoto?");
        ui.add_space(4.0);
        ui.label("O arquivo remoto mudou desde o download. Enviar sua cópia local por cima?");
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button("Sobrescrever").clicked() {
                confirmed = true;
            }
            if ui.button("Cancelar").clicked() {
                cancelled = true;
            }
        });
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            cancelled = true;
        }
    });
    if confirmed {
        tab.confirm = None;
        tab.start_edit_upload(edit);
    } else if cancelled {
        tab.confirm = None;
    }
}

fn file_dialog_modal(ctx: &egui::Context, tab: &mut SftpTab) {
    let Some(dialog) = tab.file_dialog.take() else {
        return;
    };
    match dialog {
        FileDialog::NewFolder(dialog) => {
            if let Some(dialog) = new_folder_modal(ctx, tab, dialog) {
                tab.file_dialog = Some(FileDialog::NewFolder(dialog));
            }
        }
        FileDialog::Permissions(dialog) => {
            if let Some(dialog) = permissions_modal(ctx, tab, dialog) {
                tab.file_dialog = Some(FileDialog::Permissions(dialog));
            }
        }
    }
}

fn new_folder_modal(
    ctx: &egui::Context,
    tab: &mut SftpTab,
    mut dialog: NewFolderDialog,
) -> Option<NewFolderDialog> {
    let mut submit = false;
    let mut cancel = false;
    egui::Modal::new(egui::Id::new("sftp-new-folder")).show(ctx, |ui| {
        ui.set_min_width(380.0);
        ui.heading("Criar pasta");
        let target = if dialog.remote {
            tab.remote.cwd.clone()
        } else {
            tab.local.cwd.display().to_string()
        };
        ui.label(egui::RichText::new(target).weak().small());
        ui.add_space(4.0);
        let response = ui.add(
            egui::TextEdit::singleline(&mut dialog.name)
                .id(egui::Id::new("sftp-new-folder-name"))
                .hint_text("nome da pasta")
                .desired_width(320.0),
        );
        if dialog.request_focus {
            response.request_focus();
            dialog.request_focus = false;
        }
        if response.has_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
            submit = true;
        }
        if let Some(error) = &dialog.error {
            ui.colored_label(ERROR, error);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Criar").clicked() {
                submit = true;
            }
            if ui.button("Cancelar").clicked() {
                cancel = true;
            }
        });
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
    });
    if cancel {
        return None;
    }
    if submit {
        let problem = if dialog.remote {
            validate_new_folder_name(&tab.remote.listing, &dialog.name, false)
        } else {
            validate_new_folder_name(&tab.local.listing, &dialog.name, true)
        };
        if let Some(problem) = problem {
            dialog.error = Some(problem);
            return Some(dialog);
        }
        tab.create_folder(dialog.remote, &dialog.name);
        return None;
    }
    Some(dialog)
}

/// Um bit rwx do diálogo de permissões.
fn permission_bit(ui: &mut egui::Ui, mode: &mut u32, mask: u32, label: &str) -> bool {
    let mut checked = *mode & mask != 0;
    let changed = ui.checkbox(&mut checked, label).changed();
    if changed {
        if checked {
            *mode |= mask;
        } else {
            *mode &= !mask;
        }
    }
    changed
}

fn permissions_modal(
    ctx: &egui::Context,
    tab: &mut SftpTab,
    mut dialog: PermissionsDialog,
) -> Option<PermissionsDialog> {
    let mut apply = None;
    let mut cancel = false;
    egui::Modal::new(egui::Id::new("sftp-permissions")).show(ctx, |ui| {
        ui.set_min_width(430.0);
        ui.heading("Permissões");
        ui.label(
            egui::RichText::new(format!(
                "{} item(ns) · pastas não são recursivas",
                dialog.item_count()
            ))
            .weak()
            .small(),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Octal");
            ui.add(
                egui::TextEdit::singleline(&mut dialog.mode_text)
                    .id(egui::Id::new("sftp-permissions-octal"))
                    .hint_text(if dialog.mixed { "—" } else { "755" })
                    .desired_width(64.0),
            );
            let valid = parse_octal(&dialog.mode_text);
            if ui
                .add_enabled(valid.is_some(), egui::Button::new("Aplicar"))
                .clicked()
            {
                apply = valid;
            }
            if ui.button("Cancelar").clicked() {
                cancel = true;
            }
        });
        let mut mode = parse_octal(&dialog.mode_text).unwrap_or(0);
        let mut changed = false;
        egui::Grid::new("sftp-permissions-bits")
            .num_columns(4)
            .spacing([8.0, 2.0])
            .show(ui, |ui| {
                ui.label("Dono");
                changed |= permission_bit(ui, &mut mode, 0o400, "r");
                changed |= permission_bit(ui, &mut mode, 0o200, "w");
                changed |= permission_bit(ui, &mut mode, 0o100, "x");
                ui.end_row();
                ui.label("Grupo");
                changed |= permission_bit(ui, &mut mode, 0o040, "r");
                changed |= permission_bit(ui, &mut mode, 0o020, "w");
                changed |= permission_bit(ui, &mut mode, 0o010, "x");
                ui.end_row();
                ui.label("Outros");
                changed |= permission_bit(ui, &mut mode, 0o004, "r");
                changed |= permission_bit(ui, &mut mode, 0o002, "w");
                changed |= permission_bit(ui, &mut mode, 0o001, "x");
                ui.end_row();
            });
        if changed {
            dialog.mode_text = format_octal(mode);
        }
        ui.add_space(4.0);
        ui.label(egui::RichText::new(permission_preview(&dialog)).monospace());
        ui.label(
            egui::RichText::new("bits especiais (setuid/sticky) são preservados")
                .weak()
                .small(),
        );
        if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
    });
    if cancel {
        return None;
    }
    if let Some(mode) = apply {
        // Um Chmod por grupo: cada grupo leva os seus bits especiais.
        for group in &dialog.groups {
            tab.apply_permissions(group.paths.clone(), group.effective_mode(mode));
        }
        return None;
    }
    Some(dialog)
}

fn conflict_modal(ctx: &egui::Context, tab: &mut SftpTab) {
    let Some(prompt) = tab.conflicts.first().cloned() else {
        return;
    };
    let mut decision = None;
    let mut apply_all = tab.conflict_apply_all;
    egui::Modal::new(egui::Id::new("sftp-conflict")).show(ctx, |ui| {
        ui.set_max_width(520.0);
        ui.heading("Conflito de arquivo");
        ui.add_space(4.0);
        let verb = match prompt.direction {
            Direction::Download => "Baixando",
            Direction::Upload => "Enviando",
        };
        ui.label(format!("{verb}: {}", prompt.source));
        ui.label(format!("já existe no destino: {}", prompt.dest));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(format!(
                "origem: {} · destino: {}",
                human_size(prompt.source_size),
                human_size(prompt.dest_size)
            ));
        });
        ui.add_space(6.0);
        ui.checkbox(&mut apply_all, "aplicar a todos os conflitos deste job");
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("Sobrescrever").clicked() {
                decision = Some(ConflictDecision::Overwrite);
            }
            if ui.button("Renomear").clicked() {
                decision = Some(ConflictDecision::Rename);
            }
            if ui.button("Pular").clicked() {
                decision = Some(ConflictDecision::Skip);
            }
        });
    });
    tab.conflict_apply_all = apply_all;
    if let Some(decision) = decision {
        tab.resolve_conflict(decision, apply_all);
    }
}

fn pane_stroke(ui: &egui::Ui, focused: bool) -> egui::Stroke {
    if focused {
        egui::Stroke::new(1.0, ACCENT)
    } else {
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
    }
}

/// Menu comum da pane (usado no botão ☰ e no clique direito).
fn pane_menu<P>(
    ui: &mut egui::Ui,
    side: Side,
    selected: &BTreeSet<String>,
    entries: &[Entry<P>],
) -> Option<MenuAction> {
    let mut action = None;
    let has_selection = !selected.is_empty();
    let single = if selected.len() == 1 {
        selected.iter().next().cloned()
    } else {
        None
    };

    match side {
        Side::Remote => {
            let editable = single.as_deref().is_some_and(|name| {
                entries
                    .iter()
                    .any(|entry| entry.name == name && entry.kind != EntryKind::Dir)
            });
            if ui
                .add_enabled(editable, egui::Button::new("Editar"))
                .clicked()
            {
                action = single.clone().map(MenuAction::Edit);
                ui.close();
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Baixar para…"))
                .clicked()
            {
                action = Some(MenuAction::DownloadTo);
                ui.close();
            }
            ui.separator();
            if ui.button("Criar pasta").clicked() {
                action = Some(MenuAction::NewFolder);
                ui.close();
            }
            if ui
                .add_enabled(single.is_some(), egui::Button::new("Renomear"))
                .clicked()
            {
                action = single.clone().map(MenuAction::Rename);
                ui.close();
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Permissões…"))
                .clicked()
            {
                action = Some(MenuAction::Permissions);
                ui.close();
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Excluir"))
                .clicked()
            {
                action = Some(MenuAction::Delete);
                ui.close();
            }
            ui.separator();
            if ui
                .add_enabled(has_selection, egui::Button::new("Copiar"))
                .clicked()
            {
                action = Some(MenuAction::Copy);
                ui.close();
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Copiar caminho"))
                .clicked()
            {
                action = Some(MenuAction::CopyPath);
                ui.close();
            }
            ui.separator();
            if ui.button("Atualizar").clicked() {
                action = Some(MenuAction::Refresh);
                ui.close();
            }
        }
        Side::Local => {
            if ui
                .add_enabled(single.is_some(), egui::Button::new("Abrir"))
                .clicked()
            {
                action = single.clone().map(MenuAction::Open);
                ui.close();
            }
            ui.menu_button("Enviar…", |ui| {
                if ui
                    .add_enabled(has_selection, egui::Button::new("Enviar seleção"))
                    .clicked()
                {
                    action = Some(MenuAction::UploadSelection);
                    ui.close();
                }
                if ui.button("Arquivos…").clicked() {
                    action = Some(MenuAction::UploadFiles);
                    ui.close();
                }
                if ui.button("Pasta…").clicked() {
                    action = Some(MenuAction::UploadFolder);
                    ui.close();
                }
            });
            ui.separator();
            if ui.button("Criar pasta").clicked() {
                action = Some(MenuAction::NewFolder);
                ui.close();
            }
            if ui
                .add_enabled(single.is_some(), egui::Button::new("Renomear"))
                .clicked()
            {
                action = single.clone().map(MenuAction::Rename);
                ui.close();
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Excluir"))
                .clicked()
            {
                action = Some(MenuAction::Delete);
                ui.close();
            }
            ui.separator();
            if ui
                .add_enabled(has_selection, egui::Button::new("Copiar"))
                .clicked()
            {
                action = Some(MenuAction::Copy);
                ui.close();
            }
            if ui
                .add_enabled(has_selection, egui::Button::new("Copiar caminho"))
                .clicked()
            {
                action = Some(MenuAction::CopyPath);
                ui.close();
            }
            ui.separator();
            if ui.button("Atualizar").clicked() {
                action = Some(MenuAction::Refresh);
                ui.close();
            }
        }
    }
    action
}

/// Abre o menu da pane com a listagem atual (vazia se ainda não pronta).
fn menu_for_state<P>(ui: &mut egui::Ui, side: Side, state: &PaneState<P>) -> Option<MenuAction> {
    let entries: &[Entry<P>] = match &state.listing {
        Listing::Ready(entries) => entries,
        _ => &[],
    };
    pane_menu(ui, side, &state.selected, entries)
}

fn apply_local_menu(state: &mut PaneState<PathBuf>, menu: MenuAction) -> PaneAction {
    match menu {
        MenuAction::Open(name) => match entry_by_name(state, &name) {
            Some(entry)
                if entry.kind == EntryKind::Dir
                    || (entry.kind == EntryKind::Symlink && entry.path.is_dir()) =>
            {
                PaneAction::NavigateLocal(entry.path.clone())
            }
            Some(entry) => PaneAction::OpenLocal(entry.path.clone()),
            None => PaneAction::None,
        },
        MenuAction::NewFolder => PaneAction::NewFolder { remote: false },
        MenuAction::Rename(name) => {
            begin_rename(state, name);
            PaneAction::None
        }
        MenuAction::Delete => PaneAction::DeleteSelection { remote: false },
        MenuAction::Copy => PaneAction::CopySelection,
        MenuAction::CopyPath => PaneAction::CopyPath(Side::Local),
        MenuAction::Refresh => PaneAction::ReloadLocal,
        MenuAction::UploadSelection => PaneAction::UploadSelection(selected_local_paths(state)),
        MenuAction::UploadFiles => PaneAction::UploadPick { folder: false },
        MenuAction::UploadFolder => PaneAction::UploadPick { folder: true },
        MenuAction::Edit(_) | MenuAction::Permissions | MenuAction::DownloadTo => PaneAction::None,
    }
}

fn apply_remote_menu(state: &mut PaneState<String>, menu: MenuAction) -> PaneAction {
    match menu {
        MenuAction::Open(name) => match entry_by_name(state, &name) {
            Some(entry) if entry.kind == EntryKind::Dir => {
                PaneAction::NavigateRemote(entry.path.clone())
            }
            Some(entry) => PaneAction::EditRemote(entry.path.clone()),
            None => PaneAction::None,
        },
        MenuAction::Edit(name) => entry_by_name(state, &name)
            .map(|entry| PaneAction::EditRemote(entry.path.clone()))
            .unwrap_or(PaneAction::None),
        MenuAction::NewFolder => PaneAction::NewFolder { remote: true },
        MenuAction::Rename(name) => {
            begin_rename(state, name);
            PaneAction::None
        }
        MenuAction::Delete => PaneAction::DeleteSelection { remote: true },
        MenuAction::Permissions => PaneAction::Permissions,
        MenuAction::Copy => PaneAction::CopySelection,
        MenuAction::CopyPath => PaneAction::CopyPath(Side::Remote),
        MenuAction::Refresh => PaneAction::ReloadRemote,
        MenuAction::DownloadTo => PaneAction::DownloadTo,
        MenuAction::UploadSelection | MenuAction::UploadFiles | MenuAction::UploadFolder => {
            PaneAction::None
        }
    }
}

fn local_pane(
    ui: &mut egui::Ui,
    state: &mut PaneState<PathBuf>,
    hidden: bool,
    home: &Path,
    focus: &mut Side,
    keyboard_blocked: bool,
) -> PaneAction {
    let mut action = PaneAction::None;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Local").strong());
    });

    if state.edit.is_some() {
        let mut submit = None;
        let mut cancel = false;
        ui.horizontal(|ui| {
            let Some(edit) = state.edit.as_mut() else {
                return;
            };
            let response = ui.add(
                egui::TextEdit::singleline(&mut edit.input)
                    .id(ui.id().with("local-path-edit"))
                    .desired_width((ui.available_width() - 96.0).max(80.0))
                    .hint_text("/caminho"),
            );
            if edit.request_focus {
                response.request_focus();
                edit.request_focus = false;
            }
            if response.has_focus() {
                *focus = Side::Local;
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = Some(edit.input.clone());
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            }
            if ui.button("Ir").clicked() {
                submit = Some(edit.input.clone());
            }
            if ui.button("×").clicked() {
                cancel = true;
            }
        });
        if cancel {
            state.edit = None;
        }
        if let Some(input) = submit {
            match validate_local_input(&input, home) {
                Some(path) if path.is_dir() => {
                    state.edit = None;
                    action = PaneAction::NavigateLocal(path);
                }
                Some(_) => {
                    if let Some(edit) = state.edit.as_mut() {
                        edit.error = Some("não é uma pasta".to_string());
                    }
                }
                None => {
                    if let Some(edit) = state.edit.as_mut() {
                        edit.error = Some("informe um caminho absoluto ou ~".to_string());
                    }
                }
            }
        }
        if let Some(error) = state.edit.as_ref().and_then(|edit| edit.error.clone()) {
            ui.colored_label(ERROR, error);
        }
    } else {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (index, (label, target)) in local_crumbs(&state.cwd).into_iter().enumerate() {
                if index > 0 {
                    ui.label(egui::RichText::new("›").weak());
                }
                if ui.small_button(label).clicked() {
                    *focus = Side::Local;
                    action = PaneAction::NavigateLocal(target);
                }
            }
            if ui
                .small_button("✏")
                .on_hover_text("editar caminho")
                .clicked()
            {
                *focus = Side::Local;
                let input = state.cwd.display().to_string();
                start_path_edit(state, input);
            }
        });
    }

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let parent = state.cwd.parent().map(Path::to_path_buf);
        if ui
            .add_enabled(parent.is_some(), egui::Button::new("↑").small())
            .on_hover_text("subir (Backspace)")
            .clicked()
        {
            if let Some(parent) = parent {
                *focus = Side::Local;
                action = PaneAction::NavigateLocal(parent);
            }
        }
        if ui.small_button("⌂").on_hover_text("home").clicked() {
            *focus = Side::Local;
            action = PaneAction::NavigateLocal(home.to_path_buf());
        }
        if ui.small_button("↻").on_hover_text("atualizar").clicked() {
            *focus = Side::Local;
            action = PaneAction::ReloadLocal;
        }
        if ui
            .small_button("Nova pasta")
            .on_hover_text("criar pasta")
            .clicked()
        {
            *focus = Side::Local;
            action = PaneAction::NewFolder { remote: false };
        }
        let has_selection = !state.selected.is_empty();
        if ui
            .add_enabled(has_selection, egui::Button::new("Enviar…").small())
            .on_hover_text("enviar seleção para a pane remota")
            .on_disabled_hover_text("selecione itens locais")
            .clicked()
        {
            *focus = Side::Local;
            action = PaneAction::UploadSelection(selected_local_paths(state));
        }
        if ui
            .add_enabled(has_selection, egui::Button::new("Copiar caminho").small())
            .on_hover_text("copiar caminhos como texto")
            .clicked()
        {
            *focus = Side::Local;
            action = PaneAction::CopyPath(Side::Local);
        }
        let menu_response = ui.menu_button("☰", |ui| menu_for_state(ui, Side::Local, state));
        let _ = menu_response.response.on_hover_text("menu da pane");
        if let Some(menu) = menu_response.inner.flatten() {
            *focus = Side::Local;
            action = apply_local_menu(state, menu);
        }
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Filtrar").weak().small());
        let response = ui.add(
            egui::TextEdit::singleline(&mut state.filter)
                .id(ui.id().with("local-filter"))
                .desired_width(ui.available_width().max(80.0))
                .hint_text("filtrar"),
        );
        if response.changed() {
            state.clear_selection();
        }
        if response.clicked() {
            state.rename = None;
        }
        if response.has_focus() {
            *focus = Side::Local;
        }
    });

    ui.separator();
    match list_view(
        ui,
        state,
        hidden,
        Side::Local,
        focus,
        keyboard_blocked,
        |entry| entry.path.clone(),
    ) {
        ListAction::None => {}
        ListAction::Reload => action = PaneAction::ReloadLocal,
        ListAction::Up => {
            if let Some(parent) = state.cwd.parent() {
                action = PaneAction::NavigateLocal(parent.to_path_buf());
            }
        }
        ListAction::EditPath => {
            start_path_edit(state, "/".to_string());
        }
        ListAction::Open(position) => {
            if let Some(entry) = entry_by_visible(state, hidden, position) {
                if entry.kind == EntryKind::Dir
                    || (entry.kind == EntryKind::Symlink && entry.path.is_dir())
                {
                    action = PaneAction::NavigateLocal(entry.path.clone());
                } else {
                    action = PaneAction::OpenLocal(entry.path.clone());
                }
            }
        }
        ListAction::Copy => action = PaneAction::CopySelection,
        ListAction::Paste => action = PaneAction::Paste,
        ListAction::Delete => action = PaneAction::DeleteSelection { remote: false },
        ListAction::Rename { from, to } => {
            action = PaneAction::RenameLocal {
                from: state.cwd.join(&from),
                to: state.cwd.join(&to),
            };
        }
        ListAction::Menu(menu) => action = apply_local_menu(state, menu),
        ListAction::Status(message) => action = PaneAction::Status(message),
        ListAction::StartDrag(names) => {
            let paths = paths_for_names(state, &names);
            if !paths.is_empty() {
                egui::DragAndDrop::set_payload(
                    ui.ctx(),
                    Clip {
                        side: Side::Local,
                        paths: ClipPaths::Local(paths),
                    },
                );
            }
        }
        ListAction::Drop { folder, clip } => {
            if clip.side == Side::Local {
                action = PaneAction::Status("solte na outra pane".to_string());
            } else if let ClipPaths::Remote(sources) = clip.paths {
                let dest = folder.unwrap_or_else(|| state.cwd.clone());
                action = PaneAction::Download { sources, dest };
            }
        }
        ListAction::OsDrop { .. } => {}
    }

    // Arrastar da pane remota para qualquer área da pane local (inclusive
    // barras e cabeçalho) baixa para o cwd local.
    if matches!(action, PaneAction::None) && egui::DragAndDrop::has_any_payload(ui.ctx()) {
        let response = ui.interact(
            ui.max_rect(),
            ui.id().with("local-pane-drop"),
            egui::Sense::hover(),
        );
        if let Some(clip) = response.dnd_release_payload::<Clip>() {
            if clip.side == Side::Local {
                action = PaneAction::Status("solte na outra pane".to_string());
            } else if let ClipPaths::Remote(sources) = clip.paths.clone() {
                action = PaneAction::Download {
                    sources,
                    dest: state.cwd.clone(),
                };
            }
        }
    }

    action
}

fn remote_pane(
    ui: &mut egui::Ui,
    state: &mut PaneState<String>,
    hidden: bool,
    home: Option<&str>,
    connection_note: Option<&str>,
    focus: &mut Side,
    keyboard_blocked: bool,
) -> PaneAction {
    let mut action = PaneAction::None;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Remoto").strong());
    });

    let Some(connection_note) = connection_note else {
        return remote_connected_pane(ui, state, hidden, home, focus, keyboard_blocked, action);
    };

    ui.label(egui::RichText::new(connection_note).weak());
    ui.separator();
    ui.add_space(10.0);
    ui.label(egui::RichText::new(&state.cwd).weak().small());

    // Sem conexão a queda não enfileira nada, mas o usuário recebe aviso.
    let os_files = dropped_local_files(ui);
    if !os_files.is_empty() || egui::DragAndDrop::has_any_payload(ui.ctx()) {
        let response = ui.interact(
            ui.max_rect(),
            ui.id().with("remote-offline-drop"),
            egui::Sense::hover(),
        );
        if response.dnd_release_payload::<Clip>().is_some()
            || (!os_files.is_empty() && response.contains_pointer())
        {
            action = PaneAction::Status("sem conexão — queda ignorada".to_string());
        }
    }
    action
}

fn remote_connected_pane(
    ui: &mut egui::Ui,
    state: &mut PaneState<String>,
    hidden: bool,
    home: Option<&str>,
    focus: &mut Side,
    keyboard_blocked: bool,
    mut action: PaneAction,
) -> PaneAction {
    if state.edit.is_some() {
        let mut submit = None;
        let mut cancel = false;
        ui.horizontal(|ui| {
            let Some(edit) = state.edit.as_mut() else {
                return;
            };
            let response = ui.add(
                egui::TextEdit::singleline(&mut edit.input)
                    .id(ui.id().with("remote-path-edit"))
                    .desired_width((ui.available_width() - 96.0).max(80.0))
                    .hint_text("/caminho"),
            );
            if edit.request_focus {
                response.request_focus();
                edit.request_focus = false;
            }
            if response.has_focus() {
                *focus = Side::Remote;
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = Some(edit.input.clone());
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    cancel = true;
                }
            }
            if ui.button("Ir").clicked() {
                submit = Some(edit.input.clone());
            }
            if ui.button("×").clicked() {
                cancel = true;
            }
        });
        if cancel {
            state.edit = None;
        }
        if let Some(input) = submit {
            match resolve_remote_input(&input, home) {
                Some(path) => {
                    state.edit = None;
                    action = PaneAction::NavigateRemote(path);
                }
                None => {
                    if let Some(edit) = state.edit.as_mut() {
                        edit.error = Some("informe /, ~ ou ~/…".to_string());
                    }
                }
            }
        }
        if let Some(error) = state.edit.as_ref().and_then(|edit| edit.error.clone()) {
            ui.colored_label(ERROR, error);
        }
    } else {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (index, (label, target)) in remote_crumbs(&state.cwd).into_iter().enumerate() {
                if index > 0 {
                    ui.label(egui::RichText::new("›").weak());
                }
                if ui.small_button(label).clicked() {
                    *focus = Side::Remote;
                    action = PaneAction::NavigateRemote(target);
                }
            }
            if ui
                .small_button("✏")
                .on_hover_text("editar caminho")
                .clicked()
            {
                *focus = Side::Remote;
                start_path_edit(state, state.cwd.clone());
            }
        });
    }

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let parent = remote_parent(&state.cwd);
        if ui
            .add_enabled(parent.is_some(), egui::Button::new("↑").small())
            .on_hover_text("subir (Backspace)")
            .clicked()
        {
            if let Some(parent) = parent {
                *focus = Side::Remote;
                action = PaneAction::NavigateRemote(parent);
            }
        }
        if ui.small_button("⌂").on_hover_text("home").clicked() {
            *focus = Side::Remote;
            action = PaneAction::NavigateRemote(home.unwrap_or("/").to_string());
        }
        if ui.small_button("↻").on_hover_text("atualizar").clicked() {
            *focus = Side::Remote;
            action = PaneAction::ReloadRemote;
        }
        if ui
            .small_button("Nova pasta")
            .on_hover_text("criar pasta")
            .clicked()
        {
            *focus = Side::Remote;
            action = PaneAction::NewFolder { remote: true };
        }
        let has_selection = !state.selected.is_empty();
        if ui
            .add_enabled(has_selection, egui::Button::new("Baixar…").small())
            .on_hover_text("baixar seleção para a pane local")
            .on_disabled_hover_text("selecione itens remotos")
            .clicked()
        {
            *focus = Side::Remote;
            action = PaneAction::DownloadSelection(selected_remote_paths(state));
        }
        if ui
            .add_enabled(has_selection, egui::Button::new("Copiar caminho").small())
            .on_hover_text("copiar caminhos como texto")
            .clicked()
        {
            *focus = Side::Remote;
            action = PaneAction::CopyPath(Side::Remote);
        }
        let menu_response = ui.menu_button("☰", |ui| menu_for_state(ui, Side::Remote, state));
        let _ = menu_response.response.on_hover_text("menu da pane");
        if let Some(menu) = menu_response.inner.flatten() {
            *focus = Side::Remote;
            action = apply_remote_menu(state, menu);
        }
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Filtrar").weak().small());
        let response = ui.add(
            egui::TextEdit::singleline(&mut state.filter)
                .id(ui.id().with("remote-filter"))
                .desired_width(ui.available_width().max(80.0))
                .hint_text("filtrar"),
        );
        if response.changed() {
            state.clear_selection();
        }
        if response.clicked() {
            state.rename = None;
        }
        if response.has_focus() {
            *focus = Side::Remote;
        }
    });

    ui.separator();
    match list_view(
        ui,
        state,
        hidden,
        Side::Remote,
        focus,
        keyboard_blocked,
        |entry| entry.path.clone(),
    ) {
        ListAction::None => {}
        ListAction::Reload => action = PaneAction::ReloadRemote,
        ListAction::Up => {
            if let Some(parent) = remote_parent(&state.cwd) {
                action = PaneAction::NavigateRemote(parent);
            }
        }
        ListAction::EditPath => {
            start_path_edit(state, "/".to_string());
        }
        ListAction::Open(position) => {
            if let Some(entry) = entry_by_visible(state, hidden, position) {
                if entry.kind == EntryKind::Dir {
                    action = PaneAction::NavigateRemote(entry.path.clone());
                } else {
                    action = PaneAction::EditRemote(entry.path.clone());
                }
            }
        }
        ListAction::Copy => action = PaneAction::CopySelection,
        ListAction::Paste => action = PaneAction::Paste,
        ListAction::Delete => action = PaneAction::DeleteSelection { remote: true },
        ListAction::Rename { from, to } => {
            action = PaneAction::RenameRemote {
                from: remote_join(&state.cwd, &from),
                to: remote_join(&state.cwd, &to),
            };
        }
        ListAction::Menu(menu) => action = apply_remote_menu(state, menu),
        ListAction::Status(message) => action = PaneAction::Status(message),
        ListAction::StartDrag(names) => {
            let paths = paths_for_names(state, &names);
            if !paths.is_empty() {
                egui::DragAndDrop::set_payload(
                    ui.ctx(),
                    Clip {
                        side: Side::Remote,
                        paths: ClipPaths::Remote(paths),
                    },
                );
            }
        }
        ListAction::Drop { folder, clip } => {
            if clip.side == Side::Remote {
                action = PaneAction::Status("solte na outra pane".to_string());
            } else if let ClipPaths::Local(sources) = clip.paths.clone() {
                let dest = drop_destination(&state.cwd, folder.as_deref());
                action = PaneAction::Upload { sources, dest };
            }
        }
        ListAction::OsDrop { folder, sources } => {
            let dest = drop_destination(&state.cwd, folder.as_deref());
            action = PaneAction::Upload { sources, dest };
        }
    }

    // Quedas fora da lista (barra de caminho, filtro, estado vazio) usam o
    // cwd da pane. Só registra a área quando há algo sendo arrastado para não
    // roubar o hover das linhas.
    if matches!(action, PaneAction::None) {
        let os_files = dropped_local_files(ui);
        if egui::DragAndDrop::has_any_payload(ui.ctx()) || !os_files.is_empty() {
            let response = ui.interact(
                ui.max_rect(),
                ui.id().with("remote-pane-drop"),
                egui::Sense::hover(),
            );
            if let Some(clip) = response.dnd_release_payload::<Clip>() {
                if clip.side == Side::Remote {
                    action = PaneAction::Status("solte na outra pane".to_string());
                } else if let ClipPaths::Local(sources) = clip.paths.clone() {
                    let dest = drop_destination(&state.cwd, None);
                    action = PaneAction::Upload { sources, dest };
                }
            } else if !os_files.is_empty() && response.contains_pointer() {
                let dest = drop_destination(&state.cwd, None);
                action = PaneAction::Upload {
                    sources: os_files,
                    dest,
                };
            }
        }
    }

    action
}

fn start_path_edit<P>(state: &mut PaneState<P>, input: String) {
    state.rename = None;
    state.edit = Some(PathEdit {
        input,
        error: None,
        request_focus: true,
    });
}

fn list_view<P: Clone, D: Clone>(
    ui: &mut egui::Ui,
    state: &mut PaneState<P>,
    hidden: bool,
    side: Side,
    focus: &mut Side,
    keyboard_blocked: bool,
    folder_dest: impl Fn(&Entry<P>) -> D,
) -> ListAction<D> {
    let is_remote = side == Side::Remote;
    let os_files = if is_remote {
        dropped_local_files(ui)
    } else {
        Vec::new()
    };

    if !matches!(state.listing, Listing::Ready(_)) {
        ui.add_space(10.0);
        match &state.listing {
            Listing::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("carregando…");
                });
            }
            Listing::Error(error) => {
                ui.colored_label(ERROR, error);
                ui.add_space(4.0);
                if ui.button("Tentar de novo").clicked() {
                    *focus = side;
                    return ListAction::Reload;
                }
            }
            Listing::Ready(_) => {}
        }
        return ListAction::None;
    }

    let PaneState {
        listing,
        selected,
        anchor,
        filter,
        edit,
        rename,
        ..
    } = state;
    let Listing::Ready(entries) = listing else {
        return ListAction::None;
    };
    let visible = visible_indices(entries, filter, hidden);
    let width = ui.available_width();
    let columns = Columns::new(width, is_remote);
    paint_header(ui, width, &columns);

    if visible.is_empty() {
        ui.add_space(12.0);
        let message = if entries.is_empty() {
            "pasta vazia"
        } else {
            "nada corresponde ao filtro"
        };
        ui.label(egui::RichText::new(message).weak());
        let response = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with(("sftp-empty-menu", side)),
            egui::Sense::click(),
        );
        let mut menu = None;
        response.context_menu(|ui| {
            menu = pane_menu(ui, side, selected, entries);
        });
        return menu.map(ListAction::Menu).unwrap_or(ListAction::None);
    }

    let mut action: ListAction<D> = ListAction::None;
    let mut status = None;
    let mut scroll_to = None;
    let rename_started_this_frame = rename.as_ref().is_some_and(|rename| rename.request_focus);
    let mut clicked_editor = false;
    let modal_open = ui.ctx().memory(|memory| memory.top_modal_layer().is_some());
    if *focus == side
        && edit.is_none()
        && rename.is_none()
        && !modal_open
        && !keyboard_blocked
        && !ui.ctx().egui_wants_keyboard_input()
    {
        let keys = ui.input(|input| {
            (
                input.modifiers.command && input.key_pressed(egui::Key::A),
                input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::Backspace) && !input.modifiers.command,
                input.modifiers.command && input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::Slash),
                input.modifiers.command && input.key_pressed(egui::Key::C),
                input.modifiers.command && input.key_pressed(egui::Key::V),
                input.key_pressed(egui::Key::Delete)
                    || (input.modifiers.command && input.key_pressed(egui::Key::Backspace)),
            )
        });
        if keys.9 {
            return ListAction::Delete;
        }
        if keys.4 || keys.5 {
            return ListAction::Up;
        }
        if keys.6 {
            return ListAction::EditPath;
        }
        if keys.7 {
            return ListAction::Copy;
        }
        if keys.8 {
            return ListAction::Paste;
        }
        if keys.0 {
            selected.clear();
            for index in &visible {
                selected.insert(entries[*index].name.clone());
            }
            *anchor = Some(visible.len() - 1);
            *focus = side;
        }
        if keys.1 || keys.2 {
            let current = anchor
                .filter(|position| *position < visible.len())
                .or_else(|| {
                    selected.iter().find_map(|name| {
                        visible
                            .iter()
                            .position(|&index| &entries[index].name == name)
                    })
                });
            let target = if keys.1 {
                current.map_or(0, |position| (position + 1).min(visible.len() - 1))
            } else {
                current.map_or(visible.len() - 1, |position| position.saturating_sub(1))
            };
            selected.clear();
            selected.insert(entries[visible[target]].name.clone());
            *anchor = Some(target);
            *focus = side;
            scroll_to = Some(target);
        }
        if keys.3 {
            if let Some(position) = open_position(selected, *anchor, entries, &visible) {
                action = ListAction::Open(position);
            }
        }
    }

    ui.spacing_mut().item_spacing.y = 0.0;
    let scroll = egui::ScrollArea::vertical()
        .id_salt(("sftp-list", side))
        .auto_shrink([false, false])
        // Rolagem por teclado precisa ser imediata: a linha selecionada tem
        // que estar visível no próximo frame.
        .animated(false)
        .show_rows(ui, ROW_HEIGHT, visible.len(), |ui, range| {
            let first_row = range.start;
            for position in range {
                let index = visible[position];
                let entry = &entries[index];
                let (rect, response) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), ROW_HEIGHT),
                    egui::Sense::click_and_drag(),
                );
                let is_selected = selected.contains(&entry.name);
                let drop_hover = response.dnd_hover_payload::<Clip>();
                if is_selected {
                    ui.painter().rect_filled(
                        rect,
                        0.0,
                        ui.visuals().selection.bg_fill.gamma_multiply(0.35),
                    );
                } else if response.hovered() {
                    ui.painter()
                        .rect_filled(rect, 0.0, ui.visuals().widgets.hovered.bg_fill);
                }
                if drop_hover.is_some() {
                    ui.painter()
                        .rect_filled(rect, 0.0, ACCENT.gamma_multiply(0.25));
                }

                let mut x = rect.left() + 6.0;
                let name_rect = egui::Rect::from_min_size(
                    egui::pos2(x, rect.top()),
                    egui::vec2(columns.name, rect.height()),
                );
                let editing = rename
                    .as_ref()
                    .is_some_and(|rename| rename.from == entry.name);
                let mut commit_rename = None;
                let mut cancel_rename = false;
                if editing {
                    if let Some(rename) = rename.as_mut() {
                        let mut field = egui::TextEdit::singleline(&mut rename.input)
                            .id(ui.id().with(("sftp-rename", side)))
                            .hint_text("novo nome");
                        if rename.error.is_some() {
                            field = field.text_color(ERROR);
                        }
                        let response = ui.place(name_rect, field);
                        if rename.request_focus {
                            response.request_focus();
                            rename.request_focus = false;
                        }
                        if ui.input(|input| input.pointer.primary_clicked()) {
                            if let Some(position) = ui.input(|input| input.pointer.interact_pos()) {
                                clicked_editor |= name_rect.contains(position);
                            }
                        }
                        if response.has_focus() {
                            if ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                                let proposed = rename.input.trim().to_string();
                                match rename_problem(
                                    entries,
                                    &rename.from,
                                    &proposed,
                                    side == Side::Local,
                                ) {
                                    Some(problem) => {
                                        rename.error = Some(problem.clone());
                                        status = Some(problem);
                                    }
                                    None => {
                                        commit_rename = Some((rename.from.clone(), proposed));
                                    }
                                }
                            } else if ui.input(|input| input.key_pressed(egui::Key::Escape)) {
                                cancel_rename = true;
                            }
                        }
                        // Perder o foco (clique fora, Tab etc.) encerra a
                        // renomeação; Enter/Esc acima têm prioridade.
                        if response.lost_focus() && commit_rename.is_none() && !cancel_rename {
                            cancel_rename = true;
                        }
                    }
                }
                if let Some((from, to)) = commit_rename {
                    if to == from {
                        *rename = None;
                    } else {
                        *rename = None;
                        action = ListAction::Rename { from, to };
                    }
                } else if cancel_rename {
                    *rename = None;
                }

                let mut row_menu = None;
                if response.secondary_clicked() {
                    *focus = side;
                    if !selected.contains(&entry.name) {
                        selected.clear();
                        selected.insert(entry.name.clone());
                        *anchor = Some(position);
                    }
                    if rename
                        .as_ref()
                        .is_some_and(|rename| rename.from != entry.name)
                    {
                        *rename = None;
                    }
                }
                response.context_menu(|ui| {
                    row_menu = pane_menu(ui, side, selected, entries);
                });
                if let Some(menu) = row_menu {
                    action = ListAction::Menu(menu);
                }

                let painter = ui.painter();
                let font = egui::TextStyle::Body.resolve(ui.style());
                let weak_font = egui::TextStyle::Small.resolve(ui.style());
                let text_color = ui.visuals().text_color();
                let weak_color = ui.visuals().weak_text_color();
                if !editing {
                    let icon = match entry.kind {
                        EntryKind::Dir => "📁",
                        EntryKind::File => "📄",
                        EntryKind::Symlink => "🔗",
                        EntryKind::Other => "•",
                    };
                    let name = match (&entry.link_target, entry.kind) {
                        (Some(target), EntryKind::Symlink) => {
                            format!("{icon} {} → {target}", entry.name)
                        }
                        _ => format!("{icon} {}", entry.name),
                    };
                    let name_clip = name_rect;
                    painter.with_clip_rect(name_clip).text(
                        egui::pos2(x, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        name,
                        font.clone(),
                        text_color,
                    );
                }
                x += columns.name;

                let size = if entry.kind == EntryKind::Dir {
                    "—".to_string()
                } else {
                    human_size(entry.size)
                };
                painter.text(
                    egui::pos2(x + columns.size - 4.0, rect.center().y),
                    egui::Align2::RIGHT_CENTER,
                    size,
                    weak_font.clone(),
                    weak_color,
                );
                x += columns.size;

                painter.text(
                    egui::pos2(x, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    format_modified(entry.modified),
                    weak_font.clone(),
                    weak_color,
                );
                x += columns.modified;

                if is_remote {
                    let mode = entry
                        .mode
                        .map(|mode| mode_string(mode, entry.kind == EntryKind::Dir))
                        .unwrap_or_else(|| "—".to_string());
                    painter.text(
                        egui::pos2(x, rect.center().y),
                        egui::Align2::LEFT_CENTER,
                        mode,
                        weak_font,
                        weak_color,
                    );
                }

                if let Some(clip) = response.dnd_release_payload::<Clip>() {
                    let folder = (entry.kind == EntryKind::Dir).then(|| folder_dest(entry));
                    action = ListAction::Drop {
                        folder,
                        clip: (*clip).clone(),
                    };
                } else if is_remote && !os_files.is_empty() && response.contains_pointer() {
                    let folder = (entry.kind == EntryKind::Dir).then(|| folder_dest(entry));
                    action = ListAction::OsDrop {
                        folder,
                        sources: os_files.clone(),
                    };
                }

                if response.clicked() {
                    *focus = side;
                    let modifiers = ui.input(|input| input.modifiers);
                    if modifiers.shift {
                        if let Some(anchor_position) = *anchor {
                            selected.clear();
                            let start = anchor_position.min(position);
                            let end = anchor_position.max(position);
                            for current in start..=end {
                                if let Some(index) = visible.get(current) {
                                    selected.insert(entries[*index].name.clone());
                                }
                            }
                        } else {
                            selected.clear();
                            selected.insert(entry.name.clone());
                            *anchor = Some(position);
                        }
                    } else if modifiers.command {
                        if !selected.remove(&entry.name) {
                            selected.insert(entry.name.clone());
                        }
                        *anchor = Some(position);
                    } else {
                        selected.clear();
                        selected.insert(entry.name.clone());
                        *anchor = Some(position);
                    }
                }
                if response.double_clicked() {
                    *focus = side;
                    selected.clear();
                    selected.insert(entry.name.clone());
                    *anchor = Some(position);
                    action = ListAction::Open(position);
                }
                if response.drag_started() {
                    *focus = side;
                    if !selected.contains(&entry.name) {
                        selected.clear();
                        selected.insert(entry.name.clone());
                        *anchor = Some(position);
                    }
                    action = ListAction::StartDrag(selected.iter().cloned().collect());
                }

                if let Some(clip) = drop_hover {
                    let count = clip.paths.len();
                    let verb = match clip.side {
                        Side::Local => "Enviar",
                        Side::Remote => "Baixar",
                    };
                    let destination = if entry.kind == EntryKind::Dir {
                        "a pasta".to_string()
                    } else {
                        "a pane".to_string()
                    };
                    response.on_hover_text(format!("{verb} {count} item(ns) para {destination}"));
                }
            }

            // Depois de o viewport estar posicionado, rola até a linha que o
            // teclado selecionou (inclusive se ela ainda não está visível).
            if let Some(target) = scroll_to {
                let rect = row_rect_in_viewport(
                    ui.max_rect().top(),
                    ui.max_rect().left(),
                    ui.max_rect().width(),
                    first_row,
                    target,
                );
                if !ui.clip_rect().contains_rect(rect) {
                    ui.scroll_to_rect(rect, None);
                }
            }
        });

    // Se a linha renomeada não está na viewport, o TextEdit nem foi criado e
    // não consumiu o pedido de foco; libera o pedido para os guardas abaixo.
    if let Some(rename) = rename.as_mut() {
        if rename_started_this_frame {
            rename.request_focus = false;
        }
    }

    // Clique primário fora do editor (outra linha, filtro, barra ou rodapé)
    // encerra a renomeação mesmo que a linha tenha saído da viewport.
    if rename.is_some()
        && !rename_started_this_frame
        && !clicked_editor
        && ui.input(|input| input.pointer.primary_clicked())
    {
        *rename = None;
    }
    // Esc enquanto renomeia também cancela quando o TextEdit já perdeu o foco.
    if rename.is_some()
        && !modal_open
        && !keyboard_blocked
        && ui.input(|input| input.key_pressed(egui::Key::Escape))
    {
        *rename = None;
    }

    // Soltar no vazio (ou sobre uma linha não-pasta) usa o cwd da pane.
    if matches!(action, ListAction::None)
        && (egui::DragAndDrop::has_any_payload(ui.ctx()) || !os_files.is_empty())
    {
        let response = ui.interact(
            scroll.inner_rect,
            ui.id().with(("sftp-list-drop", side)),
            egui::Sense::hover(),
        );
        if let Some(clip) = response.dnd_release_payload::<Clip>() {
            action = ListAction::Drop {
                folder: None,
                clip: (*clip).clone(),
            };
        } else if is_remote && !os_files.is_empty() && response.contains_pointer() {
            action = ListAction::OsDrop {
                folder: None,
                sources: os_files,
            };
        }
    }
    if matches!(action, ListAction::None) {
        if let Some(message) = status {
            return ListAction::Status(message);
        }
    }
    action
}

fn paint_header(ui: &mut egui::Ui, width: f32, columns: &Columns) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, HEADER_HEIGHT), egui::Sense::hover());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let painter = ui.painter().with_clip_rect(rect);
    let font = egui::TextStyle::Small.resolve(ui.style());
    let color = ui.visuals().weak_text_color();
    let mut x = rect.left() + 6.0;
    painter.text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "Nome",
        font.clone(),
        color,
    );
    x += columns.name;
    painter.text(
        egui::pos2(x + columns.size - 4.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        "Tamanho",
        font.clone(),
        color,
    );
    x += columns.size;
    painter.text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        "Modificado",
        font.clone(),
        color,
    );
    x += columns.modified;
    if columns.mode > 0.0 {
        painter.text(
            egui::pos2(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            "Modo",
            font,
            color,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, kind: EntryKind) -> Entry<String> {
        Entry {
            name: name.to_string(),
            path: format!("/{name}"),
            kind,
            size: 0,
            modified: None,
            mode: None,
            link_target: None,
        }
    }

    fn names(entries: &[Entry<String>]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    #[test]
    fn sort_puts_dirs_first_case_insensitive() {
        let mut v = vec![
            entry("zeta.txt", EntryKind::File),
            entry("Alfa", EntryKind::Dir),
            entry("beta", EntryKind::File),
        ];
        sort_entries(&mut v);
        assert_eq!(names(&v), vec!["Alfa", "beta", "zeta.txt"]);
    }

    #[test]
    fn breadcrumb_keeps_root_and_segments() {
        assert_eq!(breadcrumb("/a/b"), vec!["/", "a", "b"]);
        assert_eq!(breadcrumb("/"), vec!["/"]);
    }

    #[test]
    fn validate_remote_rejects_relative() {
        assert!(validate_remote_input("relativo").is_none());
        assert_eq!(
            validate_remote_input("/var/www").as_deref(),
            Some("/var/www")
        );
    }

    #[test]
    fn mode_string_renders_rwx() {
        assert_eq!(mode_string(0o644, false), "-rw-r--r--");
        assert_eq!(mode_string(0o755, true), "drwxr-xr-x");
    }

    #[test]
    fn human_size_uses_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(1024 * 1024), "1.0 MB");
    }

    #[test]
    fn validate_local_accepts_home_and_absolute() {
        let home = std::env::temp_dir();
        assert_eq!(
            validate_local_input("~/proj", &home),
            Some(home.join("proj"))
        );
        assert_eq!(
            validate_local_input(&home.to_string_lossy(), &home),
            Some(home.clone())
        );
        assert!(validate_local_input("proj", &home).is_none());
    }

    #[test]
    fn hidden_and_filter_affect_only_visibility() {
        let entries = vec![
            entry(".oculto", EntryKind::File),
            entry("Alfa.txt", EntryKind::File),
            entry("Beta", EntryKind::Dir),
        ];
        let without_hidden = visible_indices(&entries, "", false);
        assert_eq!(without_hidden, vec![1, 2]);
        let filtered = visible_indices(&entries, "ALFA", true);
        assert_eq!(filtered, vec![1]);
    }

    #[test]
    fn remote_parent_walks_to_root() {
        assert_eq!(remote_parent("/a/b"), Some("/a".to_string()));
        assert_eq!(remote_parent("/a"), Some("/".to_string()));
        assert_eq!(remote_parent("/"), None);
    }

    #[test]
    fn local_listing_sorts_dirs_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("pastas")).unwrap();
        std::fs::write(dir.path().join("A.txt"), b"x").unwrap();
        let Listing::Ready(entries) = read_local(dir.path()) else {
            panic!("listagem local deveria estar pronta");
        };
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["pastas", "A.txt", "b.txt"]);
    }

    #[test]
    fn navigate_local_clears_filter_and_selection() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("arquivo.txt"), b"x").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.local.filter = "zzz".to_string();
        tab.local.selected.insert("arquivo.txt".to_string());
        tab.local.anchor = Some(0);
        tab.navigate_local(dir.path().to_path_buf());
        assert!(tab.local.filter.is_empty());
        assert!(tab.local.selected.is_empty());
        assert!(tab.local.anchor.is_none());
        assert!(matches!(tab.local.listing, Listing::Ready(_)));
    }

    #[test]
    fn listed_updates_remote_cwd_and_clears_selection() {
        let mut tab = SftpTab::connecting(None);
        tab.remote.cwd = "/tmp".to_string();
        tab.remote.listing = Listing::Loading;
        tab.remote.selected.insert("velho".to_string());
        tab.remote.anchor = Some(0);
        tab.list_id = Some(7);
        tab.apply_listed(
            7,
            "/tmp".to_string(),
            Ok(vec![SftpEntry {
                name: "novo.txt".to_string(),
                path: "/tmp/novo.txt".to_string(),
                kind: EntryKind::File,
                size: 4,
                modified: None,
                mode: Some(0o644),
                link_target: None,
            }]),
        );
        assert_eq!(tab.remote.cwd, "/tmp");
        let Listing::Ready(entries) = &tab.remote.listing else {
            panic!("listagem remota deveria estar pronta");
        };
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "novo.txt");
        assert!(tab.remote.selected.is_empty());
        assert!(tab.remote.anchor.is_none());
    }

    fn test_job(tab: &mut SftpTab, retry: RetrySpec) -> u64 {
        let id = tab.alloc_id();
        tab.jobs.push(JobView {
            id,
            direction: match retry {
                RetrySpec::Download { .. } => Direction::Download,
                RetrySpec::Upload { .. } => Direction::Upload,
            },
            label: "job".to_string(),
            state: JobState::Queued,
            bytes_done: 0,
            bytes_total: None,
            items_done: 0,
            items_total: 0,
            ignored_links: 0,
            error: None,
            retry,
        });
        id
    }

    fn prompt(job_id: u64) -> ConflictPrompt {
        ConflictPrompt {
            job_id,
            direction: Direction::Download,
            source: "/remoto/a".to_string(),
            dest: "/local/a".to_string(),
            source_size: 1,
            dest_size: 2,
        }
    }

    #[test]
    fn paste_only_crosses_sides() {
        let clip = Clip {
            side: Side::Remote,
            paths: ClipPaths::Remote(vec!["/a".into()]),
        };
        assert_eq!(paste_target(Side::Local, &clip), Some(Side::Remote));
        assert_eq!(paste_target(Side::Remote, &clip), None);
    }

    #[test]
    fn drop_on_folder_uses_folder() {
        assert_eq!(drop_destination("/home", Some("/home/sub")), "/home/sub");
        assert_eq!(drop_destination("/home", None), "/home");
    }

    #[test]
    fn ids_stay_monotonic_across_reconnect() {
        let mut tab = SftpTab::connecting(None);
        let first = tab.alloc_id();
        tab.on_connected();
        assert_eq!(tab.alloc_id(), first + 1);
    }

    fn test_session() -> Session {
        Session {
            name: "web".to_string(),
            protocol: sshc_core::Protocol::Ssh,
            host: "127.0.0.1".to_string(),
            port: 22,
            user: None,
            emulation: "xterm-256color".to_string(),
            auth: None,
            description: None,
            baud: None,
        }
    }

    #[test]
    fn sftp_view_renders_queue_and_conflict_modal() {
        let ctx = egui::Context::default();
        let mut tab = SftpTab::connecting(None);
        let id = test_job(
            &mut tab,
            RetrySpec::Download {
                sources: vec!["/r/a".to_string()],
                dest: std::env::temp_dir(),
            },
        );
        tab.handle_event(SftpEvent::JobStarted {
            id,
            items_total: 2,
            bytes_total: None,
        });
        tab.handle_event(SftpEvent::Conflict {
            id,
            direction: Direction::Download,
            source: "/r/a".to_string(),
            dest: "/l/a".to_string(),
            source_size: 3,
            dest_size: 4,
        });
        tab.conflict_apply_all = true;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = sftp_view(ui, &mut tab, &test_session(), false);
            });
            let _ = sftp_modals(ui.ctx(), &mut tab);
        });
        assert_eq!(tab.jobs.len(), 1);
        assert_eq!(tab.jobs[0].state, JobState::Running);
        assert_eq!(tab.conflicts.len(), 1);
        // O checkbox "aplicar a todos" não pode resetar entre frames.
        assert!(tab.conflict_apply_all);
    }

    #[test]
    fn stale_listed_is_ignored() {
        let mut tab = SftpTab::connecting(None);
        tab.remote.cwd = "/atual".to_string();
        tab.remote.listing = Listing::Loading;
        tab.list_id = Some(2);
        tab.apply_listed(1, "/atual".to_string(), Ok(Vec::new()));
        assert!(matches!(tab.remote.listing, Listing::Loading));
        tab.apply_listed(2, "/atual".to_string(), Ok(Vec::new()));
        assert!(matches!(tab.remote.listing, Listing::Ready(_)));
    }

    #[test]
    fn job_events_update_progress_and_done() {
        let mut tab = SftpTab::connecting(None);
        let id = test_job(
            &mut tab,
            RetrySpec::Download {
                sources: vec!["/r/a".to_string()],
                dest: std::env::temp_dir(),
            },
        );
        tab.handle_event(SftpEvent::JobStarted {
            id,
            items_total: 2,
            bytes_total: Some(10),
        });
        assert_eq!(tab.jobs[0].state, JobState::Running);
        assert_eq!(tab.jobs[0].items_total, 2);
        assert_eq!(tab.jobs[0].label, "job (2 arquivos)");
        tab.handle_event(SftpEvent::JobProgress {
            id,
            items_done: 1,
            bytes_done: 4,
            bytes_total: Some(10),
            current: "/r/a".to_string(),
        });
        assert_eq!(tab.jobs[0].items_done, 1);
        assert_eq!(tab.jobs[0].bytes_done, 4);
        tab.handle_event(SftpEvent::JobDone {
            id,
            outcome: JobOutcome::Completed { ignored_links: 3 },
        });
        assert_eq!(tab.jobs[0].state, JobState::Done);
        assert_eq!(tab.jobs[0].ignored_links, 3);
    }

    #[test]
    fn closed_interrupts_active_jobs_and_ignores_late_failure() {
        let mut tab = SftpTab::connecting(None);
        let id = test_job(
            &mut tab,
            RetrySpec::Upload {
                sources: vec![PathBuf::from("/l/a")],
                dest: "/r".to_string(),
            },
        );
        tab.handle_event(SftpEvent::JobStarted {
            id,
            items_total: 1,
            bytes_total: None,
        });
        assert!(tab.handle_event(SftpEvent::Closed {
            reason: "caiu".to_string(),
        }));
        assert_eq!(tab.jobs[0].state, JobState::Interrupted);
        assert!(tab.conn.is_none());
        tab.handle_event(SftpEvent::JobDone {
            id,
            outcome: JobOutcome::Failed("tarde".to_string()),
        });
        assert_eq!(tab.jobs[0].state, JobState::Interrupted);
        assert!(tab.jobs[0].error.is_none());
        assert_eq!(tab.jobs.len(), 1);
    }

    #[test]
    fn conflicts_are_answered_one_at_a_time() {
        let mut prompts = vec![prompt(1), prompt(2)];
        assert_eq!(
            pop_conflict_reply(&mut prompts, ConflictDecision::Skip, true),
            Some((1, ConflictDecision::Skip, true))
        );
        assert_eq!(prompts.len(), 1);
        assert_eq!(
            pop_conflict_reply(&mut prompts, ConflictDecision::Rename, false),
            Some((2, ConflictDecision::Rename, false))
        );
        assert_eq!(
            pop_conflict_reply(&mut prompts, ConflictDecision::Overwrite, false),
            None
        );
        assert!(prompts.is_empty());
    }

    #[test]
    fn repeated_conflict_for_the_same_job_is_not_queued_twice() {
        let mut tab = SftpTab::connecting(None);
        tab.handle_event(SftpEvent::Conflict {
            id: 9,
            direction: Direction::Download,
            source: "/r/a".to_string(),
            dest: "/l/a".to_string(),
            source_size: 1,
            dest_size: 1,
        });
        tab.handle_event(SftpEvent::Conflict {
            id: 9,
            direction: Direction::Download,
            source: "/r/a".to_string(),
            dest: "/l/a".to_string(),
            source_size: 1,
            dest_size: 1,
        });
        assert_eq!(tab.conflicts.len(), 1);
    }

    #[test]
    fn close_confirmation_only_when_a_job_is_active() {
        let mut tab = SftpTab::connecting(None);
        assert!(!tab.request_close(7));
        let id = test_job(
            &mut tab,
            RetrySpec::Download {
                sources: vec!["/r/a".to_string()],
                dest: std::env::temp_dir(),
            },
        );
        assert!(tab.request_close(7));
        assert_eq!(tab.confirm, Some(Confirm::CloseTab { id: 7 }));
        tab.handle_event(SftpEvent::JobDone {
            id,
            outcome: JobOutcome::Cancelled,
        });
        tab.confirm = None;
        assert!(!tab.request_close(7));
    }

    #[test]
    fn close_confirmation_counts_dirty_edits() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"a").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(edited_file("/r/a.txt", local));

        // Cópia limpa não bloqueia o fechamento.
        assert!(!tab.request_close(7));
        tab.edits[0].dirty = true;
        assert!(tab.request_close(7));
        assert_eq!(tab.confirm, Some(Confirm::CloseTab { id: 7 }));

        let message = close_confirm_message(0, 1);
        assert!(message.contains("não enviadas"), "mensagem: {message}");
        let both = close_confirm_message(2, 1);
        assert!(both.contains("transferência"), "mensagem: {both}");
        assert!(both.contains("não enviadas"), "mensagem: {both}");
    }

    #[test]
    fn clear_finished_keeps_queued_and_running() {
        let mut tab = SftpTab::connecting(None);
        let queued = test_job(
            &mut tab,
            RetrySpec::Download {
                sources: vec!["/r/a".to_string()],
                dest: std::env::temp_dir(),
            },
        );
        let running = test_job(
            &mut tab,
            RetrySpec::Upload {
                sources: vec![PathBuf::from("/l/a")],
                dest: "/r".to_string(),
            },
        );
        let done = test_job(
            &mut tab,
            RetrySpec::Upload {
                sources: vec![PathBuf::from("/l/b")],
                dest: "/r".to_string(),
            },
        );
        tab.handle_event(SftpEvent::JobStarted {
            id: running,
            items_total: 1,
            bytes_total: Some(1),
        });
        tab.handle_event(SftpEvent::JobDone {
            id: done,
            outcome: JobOutcome::Completed { ignored_links: 0 },
        });
        assert_eq!(tab.active_jobs(), 2);
        tab.clear_finished();
        assert_eq!(tab.jobs.len(), 2);
        assert!(tab.jobs.iter().any(|job| job.id == queued));
        assert!(tab.jobs.iter().any(|job| job.id == running));
    }

    #[test]
    fn paste_on_the_same_side_is_a_no_op_with_status() {
        let mut tab = SftpTab::connecting(None);
        tab.clip = Some(Clip {
            side: Side::Local,
            paths: ClipPaths::Local(vec![PathBuf::from("/l/a")]),
        });
        tab.paste(Side::Local);
        assert!(tab.jobs.is_empty());
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("outra pane"));
    }

    #[test]
    fn enqueue_without_connection_reports_status() {
        let mut tab = SftpTab::connecting(None);
        assert_eq!(
            tab.enqueue_upload(vec![PathBuf::from("/l/a")], "/r".to_string()),
            None
        );
        assert!(tab.jobs.is_empty());
        assert_eq!(tab.status.as_deref(), Some("sem conexão"));
    }

    #[test]
    fn row_rect_in_viewport_maps_offscreen_rows() {
        let inside = row_rect_in_viewport(200.0, 10.0, 300.0, 5, 7);
        assert_eq!(inside.top(), 240.0);
        assert_eq!(inside.height(), ROW_HEIGHT);
        let below = row_rect_in_viewport(200.0, 10.0, 300.0, 5, 100);
        assert_eq!(below.top(), 200.0 + 95.0 * ROW_HEIGHT);
        assert!(below.top() > 200.0);
        let above = row_rect_in_viewport(200.0, 10.0, 300.0, 50, 40);
        assert_eq!(above.top(), 0.0);
        assert!(above.bottom() <= 200.0);
    }

    #[test]
    fn opener_is_shell_free_and_passes_path_unchanged() {
        let path = Path::new("pasta com espaço/x&calc.exe");
        let (program, args) = opener_command(path);
        assert!(!matches!(program, "cmd" | "cmd.exe" | "sh" | "bash"));
        assert_eq!(args, vec![path.as_os_str().to_os_string()]);
        #[cfg(target_os = "windows")]
        assert_eq!(program, "explorer.exe");
        #[cfg(target_os = "macos")]
        assert_eq!(program, "open");
        #[cfg(all(unix, not(target_os = "macos")))]
        assert_eq!(program, "xdg-open");
    }

    #[test]
    fn scroll_to_rect_moves_the_scroll_area() {
        let ctx = egui::Context::default();
        let mut offset = 0.0;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(400.0, 200.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let output = egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .animated(false)
                    .show_rows(ui, ROW_HEIGHT, 100, |ui, range| {
                        let first_row = range.start;
                        for _ in range {
                            let _ = ui.allocate_exact_size(
                                egui::vec2(ui.available_width(), ROW_HEIGHT),
                                egui::Sense::hover(),
                            );
                        }
                        let rect = row_rect_in_viewport(
                            ui.max_rect().top(),
                            ui.max_rect().left(),
                            ui.max_rect().width(),
                            first_row,
                            80,
                        );
                        if !ui.clip_rect().contains_rect(rect) {
                            ui.scroll_to_rect(rect, None);
                        }
                    });
                offset = output.state.offset.y;
            });
        });
        assert!(offset > 0.0, "offset={offset}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_opener_avoids_cmd_shell() {
        let path = Path::new(r"C:\temp\x&calc.exe");
        let (program, args) = opener_command(path);
        assert_eq!(program, "explorer.exe");
        assert_ne!(program, "cmd");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], path.as_os_str());
    }

    #[test]
    fn octal_roundtrips() {
        assert_eq!(parse_octal("644"), Some(0o644));
        assert_eq!(parse_octal("0"), Some(0));
        assert_eq!(parse_octal("888"), None);
        assert_eq!(parse_octal("64"), None);
        assert_eq!(format_octal(0o755), "755");
    }

    #[test]
    fn name_rejects_slash_and_empty() {
        assert!(!valid_entry_name(""));
        assert!(!valid_entry_name("a/b"));
        assert!(valid_entry_name("a b.txt"));
    }

    #[test]
    fn octal_ignores_special_bits_and_rejects_bad_input() {
        assert_eq!(format_octal(0o4755), "755");
        assert_eq!(parse_octal(" 640 "), Some(0o640));
        assert_eq!(parse_octal("0755"), None);
        assert_eq!(parse_octal("64a"), None);
        assert_eq!(parse_octal(""), None);
    }

    #[test]
    fn delete_request_uses_selected_paths() {
        let mut tab = SftpTab::connecting(None);
        tab.remote.cwd = "/".to_string();
        tab.remote.listing = Listing::Ready(vec![
            entry("a.txt", EntryKind::File),
            entry("dir", EntryKind::Dir),
        ]);
        tab.remote.selected.insert("a.txt".to_string());
        tab.remote.selected.insert("dir".to_string());
        tab.request_delete_selection(Side::Remote);
        assert_eq!(
            tab.confirm,
            Some(Confirm::Delete {
                remote: true,
                paths: vec!["/a.txt".into(), "/dir".into()],
            })
        );
    }

    #[test]
    fn delete_refuses_root_and_empty_selection() {
        let mut tab = SftpTab::connecting(None);
        tab.request_delete(false, vec!["/".to_string()]);
        assert!(tab.confirm.is_none());
        assert!(tab.status.as_deref().unwrap_or_default().contains("raiz"));
        tab.status = None;
        tab.request_delete_selection(Side::Remote);
        assert!(tab.confirm.is_none());
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("selecione"));
    }

    #[test]
    fn delete_confirm_removes_local_file_and_tree() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let tree = dir.path().join("pasta");
        std::fs::create_dir(&tree).unwrap();
        std::fs::write(tree.join("b.txt"), b"y").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.navigate_local(dir.path().to_path_buf());
        tab.delete_paths(
            false,
            &[file.display().to_string(), tree.display().to_string()],
        );
        assert!(!file.exists());
        assert!(!tree.exists());
        assert!(tab.status.as_deref().unwrap_or_default().contains("2"));
    }

    #[test]
    fn local_folder_and_rename_are_applied() {
        let dir = tempfile::tempdir().unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.navigate_local(dir.path().to_path_buf());
        tab.create_folder(false, "nova pasta");
        assert!(dir.path().join("nova pasta").is_dir());
        let from = dir.path().join("nova pasta");
        let to = dir.path().join("final");
        tab.rename_local(from, to.clone());
        assert!(to.is_dir());
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("renomeado"));
    }

    #[test]
    fn op_done_success_and_error_land_in_status() {
        let mut tab = SftpTab::connecting(None);
        tab.pending_ops.push(PendingOp {
            id: 4,
            success: "pasta criada".to_string(),
        });
        tab.handle_event(SftpEvent::OpDone {
            id: 4,
            result: Ok(()),
        });
        assert!(tab.pending_ops.is_empty());
        assert_eq!(tab.status.as_deref(), Some("pasta criada"));
        tab.pending_ops.push(PendingOp {
            id: 5,
            success: "renomeado".to_string(),
        });
        tab.handle_event(SftpEvent::OpDone {
            id: 5,
            result: Err("sem permissão".to_string()),
        });
        assert!(tab.pending_ops.is_empty());
        assert_eq!(tab.status.as_deref(), Some("sem permissão"));
    }

    #[test]
    fn unknown_op_done_is_ignored() {
        let mut tab = SftpTab::connecting(None);
        tab.status = Some("antes".to_string());
        tab.handle_event(SftpEvent::OpDone {
            id: 99,
            result: Err("tarde".to_string()),
        });
        assert_eq!(tab.status.as_deref(), Some("antes"));
    }

    #[test]
    fn permissions_dialog_uses_single_mode_or_mixed_dash() {
        let mut tab = SftpTab::connecting(None);
        let mut a = entry("a", EntryKind::File);
        a.mode = Some(0o644);
        let mut b = entry("b", EntryKind::File);
        b.mode = Some(0o600);
        tab.remote.listing = Listing::Ready(vec![a, b]);
        tab.remote.selected.insert("a".into());
        tab.remote.selected.insert("b".into());
        tab.request_permissions();
        let Some(FileDialog::Permissions(dialog)) = &tab.file_dialog else {
            panic!("diálogo de permissões deveria abrir");
        };
        assert!(dialog.mixed);
        assert!(dialog.mode_text.is_empty());
        assert_eq!(permission_preview(dialog), "—");

        tab.file_dialog = None;
        let mut dir = entry("d", EntryKind::Dir);
        dir.mode = Some(0o755);
        tab.remote.listing = Listing::Ready(vec![dir]);
        tab.remote.selected.clear();
        tab.remote.selected.insert("d".into());
        tab.request_permissions();
        let Some(FileDialog::Permissions(dialog)) = &tab.file_dialog else {
            panic!("diálogo de permissões deveria abrir");
        };
        assert!(!dialog.mixed);
        assert_eq!(dialog.mode_text, "755");
        assert_eq!(permission_preview(dialog), "drwxr-xr-x");

        tab.file_dialog = None;
        let mut setuid = entry("s", EntryKind::File);
        setuid.mode = Some(0o4755);
        tab.remote.listing = Listing::Ready(vec![setuid]);
        tab.remote.selected.clear();
        tab.remote.selected.insert("s".into());
        tab.request_permissions();
        let Some(FileDialog::Permissions(dialog)) = &tab.file_dialog else {
            panic!("diálogo de permissões deveria abrir");
        };
        assert_eq!(dialog.mode_text, "755");
        assert_eq!(dialog.groups.len(), 1);
        assert_eq!(dialog.groups[0].special, 0o4000);
        assert_eq!(dialog.groups[0].effective_mode(0o755), 0o4755);
    }

    #[test]
    fn permissions_group_mixed_special_bits() {
        let mut tab = SftpTab::connecting(None);
        let mut sticky = entry("sticky", EntryKind::Dir);
        sticky.mode = Some(0o1777);
        let mut normal = entry("normal", EntryKind::File);
        normal.mode = Some(0o644);
        tab.remote.listing = Listing::Ready(vec![sticky, normal]);
        tab.remote.selected.insert("sticky".into());
        tab.remote.selected.insert("normal".into());
        tab.request_permissions();
        let Some(FileDialog::Permissions(dialog)) = &tab.file_dialog else {
            panic!("diálogo de permissões deveria abrir");
        };
        assert_eq!(dialog.groups.len(), 2);
        let sticky_group = dialog
            .groups
            .iter()
            .find(|group| group.paths.iter().any(|path| path == "/sticky"))
            .expect("grupo sticky");
        assert_eq!(sticky_group.special, 0o1000);
        assert_eq!(sticky_group.effective_mode(0o755), 0o1755);
        let normal_group = dialog
            .groups
            .iter()
            .find(|group| group.paths.iter().any(|path| path == "/normal"))
            .expect("grupo normal");
        assert_eq!(normal_group.special, 0);
        assert_eq!(normal_group.effective_mode(0o755), 0o755);
    }

    #[test]
    fn new_folder_name_validation_rejects_existing_and_slash() {
        let listing = Listing::Ready(vec![entry("a.txt", EntryKind::File)]);
        assert!(validate_new_folder_name(&listing, "", false).is_some());
        assert!(validate_new_folder_name(&listing, "a/b", false).is_some());
        assert!(validate_new_folder_name(&listing, "a.txt", false).is_some());
        assert!(validate_new_folder_name(&listing, "a b.txt", false).is_none());
        let empty: Listing<String> = Listing::Ready(Vec::new());
        assert!(validate_new_folder_name(&empty, "  ", true).is_some());
        assert!(validate_new_folder_name(&empty, "..\\x", true).is_some());
        assert!(validate_new_folder_name(&empty, "C:\\x", true).is_some());
        assert!(validate_new_folder_name(&empty, "nova pasta", true).is_none());
    }

    #[test]
    fn local_names_reject_traversal_and_windows_prefixes() {
        assert!(!valid_local_entry_name(".."));
        assert!(!valid_local_entry_name("..\\x"));
        assert!(!valid_local_entry_name("C:\\x"));
        assert!(!valid_local_entry_name("a/b"));
        assert!(valid_local_entry_name("a b.txt"));
        // A barra invertida continua válida para nomes remotos POSIX.
        assert!(valid_entry_name("a\\b"));
        assert!(!valid_entry_name("."));
        assert!(!valid_entry_name(".."));
        #[cfg(windows)]
        assert!(!valid_local_entry_name("C:x"));
    }

    #[test]
    fn rename_problem_rejects_empty_slash_and_existing() {
        let entries = vec![
            entry("a.txt", EntryKind::File),
            entry("b.txt", EntryKind::File),
        ];
        assert!(rename_problem(&entries, "a.txt", "", false).is_some());
        assert!(rename_problem(&entries, "a.txt", "a/b", false).is_some());
        assert!(rename_problem(&entries, "a.txt", "..", false).is_some());
        assert!(rename_problem(&entries, "a.txt", "b.txt", false).is_some());
        assert!(rename_problem(&entries, "a.txt", "a.txt", false).is_none());
        assert!(rename_problem(&entries, "a.txt", "c.txt", false).is_none());
        assert!(rename_problem(&entries, "a.txt", "..\\x", true).is_some());
        assert!(rename_problem(&entries, "a.txt", "C:\\x", true).is_some());
        assert!(rename_problem(&entries, "a.txt", "c.txt", true).is_none());
    }

    #[test]
    fn pane_menu_renders_without_triggering_actions() {
        let ctx = egui::Context::default();
        let entries = vec![entry("a.txt", EntryKind::File)];
        let selected = BTreeSet::from(["a.txt".to_string()]);
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(500.0, 400.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert_eq!(pane_menu(ui, Side::Remote, &selected, &entries), None);
            });
        });
        let _ = ctx.run_ui(input(), |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                assert_eq!(pane_menu(ui, Side::Local, &selected, &entries), None);
            });
        });
    }

    #[test]
    fn pane_menu_actions_map_to_pane_actions() {
        let mut local = PaneState::<PathBuf>::new(std::env::temp_dir());
        assert!(matches!(
            apply_local_menu(&mut local, MenuAction::NewFolder),
            PaneAction::NewFolder { remote: false }
        ));
        assert!(matches!(
            apply_local_menu(&mut local, MenuAction::UploadFiles),
            PaneAction::UploadPick { folder: false }
        ));
        apply_local_menu(&mut local, MenuAction::Rename("a".to_string()));
        assert_eq!(
            local.rename.as_ref().map(|rename| rename.from.as_str()),
            Some("a")
        );

        let mut remote = PaneState::<String>::new("/".to_string());
        assert!(matches!(
            apply_remote_menu(&mut remote, MenuAction::Permissions),
            PaneAction::Permissions
        ));
        assert!(matches!(
            apply_remote_menu(&mut remote, MenuAction::DownloadTo),
            PaneAction::DownloadTo
        ));
        assert!(matches!(
            apply_remote_menu(&mut remote, MenuAction::NewFolder),
            PaneAction::NewFolder { remote: true }
        ));
    }

    #[test]
    fn delete_paths_refuses_root_for_the_remote() {
        let mut tab = SftpTab::connecting(None);
        tab.delete_paths(true, &["/".to_string()]);
        assert!(tab.status.as_deref().unwrap_or_default().contains("raiz"));
        assert!(tab.pending_ops.is_empty());
    }

    #[test]
    fn delete_key_opens_delete_confirmation_for_focused_pane() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.navigate_local(dir.path().to_path_buf());
        tab.local.selected.insert("a.txt".to_string());
        tab.focus = Side::Local;
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            events: vec![egui::Event::Key {
                key: egui::Key::Delete,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = sftp_view(ui, &mut tab, &test_session(), false);
            });
        });
        assert_eq!(
            tab.confirm,
            Some(Confirm::Delete {
                remote: false,
                paths: vec![file.display().to_string()],
            })
        );
    }

    #[test]
    fn cmd_backspace_opens_delete_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.navigate_local(dir.path().to_path_buf());
        tab.local.selected.insert("a.txt".to_string());
        tab.focus = Side::Local;
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            modifiers: egui::Modifiers {
                command: true,
                ..Default::default()
            },
            events: vec![egui::Event::Key {
                key: egui::Key::Backspace,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers {
                    command: true,
                    ..Default::default()
                },
            }],
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = sftp_view(ui, &mut tab, &test_session(), false);
            });
        });
        assert_eq!(
            tab.confirm,
            Some(Confirm::Delete {
                remote: false,
                paths: vec![file.display().to_string()],
            })
        );
    }

    #[test]
    fn modal_open_blocks_delete_shortcut() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.navigate_local(dir.path().to_path_buf());
        tab.local.selected.insert("a.txt".to_string());
        tab.focus = Side::Local;
        // Um diálogo aberto não pode deixar o atalho agir por baixo.
        tab.file_dialog = Some(FileDialog::NewFolder(NewFolderDialog {
            remote: false,
            name: String::new(),
            request_focus: false,
            error: None,
        }));
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            events: vec![egui::Event::Key {
                key: egui::Key::Delete,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = sftp_view(ui, &mut tab, &test_session(), false);
            });
        });
        assert!(tab.confirm.is_none());
    }

    #[test]
    fn rename_ends_on_click_outside_and_shortcuts_resume() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.navigate_local(dir.path().to_path_buf());
        tab.local.selected.insert("a.txt".to_string());
        tab.focus = Side::Local;
        begin_rename(&mut tab.local, "a.txt".to_string());

        let ctx = egui::Context::default();
        let run = |tab: &mut SftpTab, events: Vec<egui::Event>, modifiers: egui::Modifiers| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 600.0),
                )),
                modifiers,
                events,
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let _ = sftp_view(ui, tab, &test_session(), false);
                });
            });
        };
        let key = |key: egui::Key, command: bool| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                command,
                ..Default::default()
            },
        };

        // Frame 1: o TextEdit da renomeação ganha foco.
        run(&mut tab, Vec::new(), egui::Modifiers::default());
        assert!(tab.local.rename.is_some());

        // Frame 2: clique primário longe do editor (painel remoto/rodapé).
        let click = egui::Pos2::new(880.0, 590.0);
        run(
            &mut tab,
            vec![
                egui::Event::PointerMoved(click),
                egui::Event::PointerButton {
                    pos: click,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
                egui::Event::PointerButton {
                    pos: click,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            egui::Modifiers::default(),
        );
        assert!(
            tab.local.rename.is_none(),
            "clique fora deve encerrar a renomeação"
        );

        // Frame 3: setas voltam a navegar.
        run(
            &mut tab,
            vec![key(egui::Key::ArrowDown, false)],
            egui::Modifiers::default(),
        );
        assert!(tab.local.selected.contains("b.txt"));

        // Frame 4: Cmd+C volta a copiar a seleção para o clipboard interno.
        run(
            &mut tab,
            vec![key(egui::Key::C, true)],
            egui::Modifiers {
                command: true,
                ..Default::default()
            },
        );
        assert!(tab.clip.is_some());

        // Frame 5: Delete volta a abrir a confirmação.
        run(
            &mut tab,
            vec![key(egui::Key::Delete, false)],
            egui::Modifiers::default(),
        );
        assert!(matches!(tab.confirm, Some(Confirm::Delete { .. })));
    }

    #[test]
    fn sftp_modals_render_delete_and_new_folder_dialogs() {
        let ctx = egui::Context::default();
        let mut tab = SftpTab::connecting(None);
        tab.confirm = Some(Confirm::Delete {
            remote: true,
            paths: vec!["/a".into(), "/b".into()],
        });
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            let _ = sftp_modals(ui.ctx(), &mut tab);
        });
        assert!(matches!(tab.confirm, Some(Confirm::Delete { .. })));
        tab.confirm = None;
        tab.file_dialog = Some(FileDialog::NewFolder(NewFolderDialog {
            remote: true,
            name: "x".to_string(),
            request_focus: true,
            error: None,
        }));
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            let _ = sftp_modals(ui.ctx(), &mut tab);
        });
        assert!(tab.file_dialog.is_some());

        tab.file_dialog = Some(FileDialog::Permissions(PermissionsDialog {
            groups: vec![PermissionGroup {
                paths: vec!["/a".into()],
                special: 0,
            }],
            mode_text: "644".to_string(),
            mixed: false,
            is_dir: false,
        }));
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            let _ = sftp_modals(ui.ctx(), &mut tab);
        });
        assert!(matches!(tab.file_dialog, Some(FileDialog::Permissions(_))));
    }

    #[test]
    fn closing_clears_remote_ops_and_dialogs() {
        let mut tab = SftpTab::connecting(None);
        tab.pending_ops.push(PendingOp {
            id: 1,
            success: "x".to_string(),
        });
        tab.file_dialog = Some(FileDialog::NewFolder(NewFolderDialog {
            remote: true,
            name: "x".to_string(),
            request_focus: false,
            error: None,
        }));
        tab.on_closed("caiu".to_string());
        assert!(tab.pending_ops.is_empty());
        assert!(tab.file_dialog.is_none());
    }

    #[test]
    fn has_modal_covers_dialogs_and_conflicts() {
        let mut tab = SftpTab::connecting(None);
        assert!(!tab.has_modal());
        tab.file_dialog = Some(FileDialog::Permissions(PermissionsDialog {
            groups: vec![PermissionGroup {
                paths: vec!["/a".into()],
                special: 0,
            }],
            mode_text: "644".to_string(),
            mixed: false,
            is_dir: false,
        }));
        assert!(tab.has_modal());
        tab.file_dialog = None;
        tab.confirm = Some(Confirm::CloseTab { id: 1 });
        assert!(tab.has_modal());
    }

    fn edited_file(remote: &str, local: PathBuf) -> EditedFile {
        let metadata = std::fs::metadata(&local).unwrap();
        EditedFile {
            remote: remote.to_string(),
            local,
            remote_size: metadata.len(),
            remote_mtime: Some(1),
            local_size: metadata.len(),
            local_mtime: metadata.modified().unwrap(),
            dirty: false,
            stable_since: None,
        }
    }

    fn sftp_entry(path: &str, size: u64, modified: Option<u32>) -> SftpEntry {
        SftpEntry {
            name: remote_base_name(path),
            path: path.to_string(),
            kind: EntryKind::File,
            size,
            modified,
            mode: Some(0o644),
            link_target: None,
        }
    }

    #[test]
    fn edit_path_stays_under_root() {
        let root = Path::new("/tmp/edit");
        let p = edit_temp_path(root, "host_22_user", "/etc/nginx/nginx.conf").unwrap();
        assert_eq!(p, root.join("host_22_user/etc/nginx/nginx.conf"));
        assert!(edit_temp_path(root, "k", "/../../fora").is_err());
    }

    #[test]
    fn changed_since_compares_size_or_mtime() {
        assert!(!changed_since(10, Some(5), 10, Some(5)));
        assert!(changed_since(11, Some(5), 10, Some(5)));
        assert!(changed_since(10, Some(6), 10, Some(5)));
    }

    #[test]
    fn edit_path_rejects_relative_remote_and_unsafe_key() {
        let root = Path::new("/tmp/edit");
        assert!(edit_temp_path(root, "k", "relativo").is_err());
        assert!(edit_temp_path(root, "k", "/").is_err());
        assert!(edit_temp_path(root, "..", "/a").is_err());
        assert!(edit_temp_path(root, "a/b", "/a").is_err());
    }

    #[test]
    fn cleanup_removes_sessions_older_than_24h() {
        let root = tempfile::tempdir().unwrap();
        let session = root.path().join("host_22_user");
        std::fs::create_dir(&session).unwrap();
        std::fs::write(session.join("a.txt"), b"x").unwrap();
        let now = SystemTime::now();
        let day = Duration::from_secs(24 * 60 * 60);
        cleanup_edit_temp_at(root.path(), now + Duration::from_secs(3600), day);
        assert!(session.exists());
        cleanup_edit_temp_at(root.path(), now + Duration::from_secs(25 * 60 * 60), day);
        assert!(!session.exists());
    }

    #[test]
    fn ensure_edit_root_creates_real_idempotent_directories() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("sshc-sftp-edit");
        ensure_edit_root(&root).unwrap();
        assert!(std::fs::symlink_metadata(&root).unwrap().is_dir());
        ensure_edit_root(&root).unwrap();
        let session = ensure_edit_session_dir(&root, "host_22_user").unwrap();
        assert_eq!(session, root.join("host_22_user"));
        assert!(std::fs::symlink_metadata(&session).unwrap().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn ensure_edit_root_replaces_symlink_and_cleanup_never_follows() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let victim = tempfile::tempdir().unwrap();
        let sentinel = victim.path().join("sentinela.txt");
        std::fs::write(&sentinel, b"keep").unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("sshc-sftp-edit");
        symlink(victim.path(), &root).unwrap();

        // A raiz linkada vira pasta real 0700 e o alvo fica intacto.
        ensure_edit_root(&root).unwrap();
        let metadata = std::fs::symlink_metadata(&root).unwrap();
        assert!(!metadata.file_type().is_symlink());
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert!(sentinel.exists());

        // O mesmo vale para a pasta da sessão.
        let session = root.join("host_22_user");
        symlink(victim.path(), &session).unwrap();
        ensure_edit_session_dir(&root, "host_22_user").unwrap();
        let metadata = std::fs::symlink_metadata(&session).unwrap();
        assert!(!metadata.file_type().is_symlink());
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert!(sentinel.exists());

        // Um filho linkado e antigo é removido como link; o alvo não.
        let old_child = root.join("sessao_antiga");
        symlink(victim.path(), &old_child).unwrap();
        cleanup_edit_temp_at(
            &root,
            SystemTime::now() + Duration::from_secs(2 * 24 * 60 * 60),
            Duration::from_secs(24 * 60 * 60),
        );
        assert!(sentinel.exists(), "a limpeza seguiu o link do filho");
        assert!(
            std::fs::symlink_metadata(&old_child).is_err(),
            "o link antigo deveria sumir sem tocar no alvo"
        );
    }

    #[test]
    fn poll_edits_marks_dirty_after_a_stable_change() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"one").unwrap();
        let metadata = std::fs::metadata(&local).unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(EditedFile {
            remote: "/r/a.txt".to_string(),
            local: local.clone(),
            remote_size: metadata.len(),
            remote_mtime: Some(1),
            local_size: metadata.len(),
            local_mtime: metadata.modified().unwrap(),
            dirty: false,
            stable_since: None,
        });
        let start = Instant::now();
        assert!(!tab.poll_edits(start));
        std::fs::write(&local, b"three").unwrap();
        // Menos de 1 s desde o último polling: nem olha os metadados.
        assert!(!tab.poll_edits(start + Duration::from_millis(250)));
        assert!(tab.edits[0].stable_since.is_none());
        // Primeira observação da mudança: ainda não é estável.
        assert!(!tab.poll_edits(start + Duration::from_secs(1)));
        assert!(tab.edits[0].stable_since.is_some());
        assert!(!tab.edits[0].dirty);
        // Passou 1 s estável: vira dirty.
        assert!(tab.poll_edits(start + Duration::from_secs(2)));
        assert!(tab.edits[0].dirty);
        assert!(tab.edits[0].stable_since.is_none());
        // Polling só marca; nunca enfileira upload sozinho.
        assert!(tab.jobs.is_empty());
    }

    #[test]
    fn discard_removes_temp_copy_and_reindexes_pending() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("a.txt");
        let second = dir.path().join("b.txt");
        std::fs::write(&first, b"a").unwrap();
        std::fs::write(&second, b"b").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(edited_file("/r/a.txt", first.clone()));
        tab.edits.push(edited_file("/r/b.txt", second.clone()));
        tab.pending_edit_uploads.push(PendingEditUpload {
            job_id: 1,
            edit: 1,
            remote: "/r/b.txt".to_string(),
            local_size: 1,
            local_mtime: None,
        });
        tab.discard_edit(0);
        assert!(!first.exists());
        assert!(second.exists());
        assert_eq!(tab.edits.len(), 1);
        assert_eq!(tab.pending_edit_uploads[0].edit, 0);
    }

    #[test]
    fn discard_works_for_a_clean_edit() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"a").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(edited_file("/r/a.txt", local.clone()));
        assert!(!tab.edits[0].dirty);
        tab.discard_edit(0);
        assert!(tab.edits.is_empty());
        assert!(!local.exists());
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("descartada"));
    }

    #[test]
    fn reopen_missing_copy_reports_status_without_opening() {
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(EditedFile {
            remote: "/r/a.txt".to_string(),
            local: PathBuf::from("/nao/existe/a.txt"),
            remote_size: 1,
            remote_mtime: Some(1),
            local_size: 1,
            local_mtime: UNIX_EPOCH,
            dirty: false,
            stable_since: None,
        });
        tab.reopen_edit(0);
        assert_eq!(tab.edits.len(), 1);
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("não existe"));
    }

    #[test]
    fn discard_cancels_an_in_flight_edit_upload() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"novo").unwrap();
        let metadata = std::fs::metadata(&local).unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(EditedFile {
            remote: "/r/a.txt".to_string(),
            local: local.clone(),
            remote_size: 2,
            remote_mtime: Some(2),
            local_size: 2,
            local_mtime: UNIX_EPOCH,
            dirty: true,
            stable_since: None,
        });
        let id = tab.alloc_id();
        tab.jobs.push(JobView {
            id,
            direction: Direction::Upload,
            label: "enviar de volta a.txt".to_string(),
            state: JobState::Running,
            bytes_done: 1,
            bytes_total: Some(4),
            items_done: 0,
            items_total: 1,
            ignored_links: 0,
            error: None,
            retry: RetrySpec::Upload {
                sources: vec![local.clone()],
                dest: "/r".to_string(),
            },
        });
        tab.pending_edit_uploads.push(PendingEditUpload {
            job_id: id,
            edit: 0,
            remote: "/r/a.txt".to_string(),
            local_size: metadata.len(),
            local_mtime: metadata.modified().ok(),
        });
        tab.pending_edit_stats.push(PendingEditStat {
            list_id: 77,
            edit: 0,
            remote: "/r/a.txt".to_string(),
            purpose: EditStatPurpose::BeforeUpload,
        });
        assert_eq!(tab.upload_job_for_edit(0), Some(id));

        tab.discard_edit(0);
        assert!(tab.edits.is_empty());
        assert!(!local.exists());
        assert!(tab.pending_edit_uploads.is_empty());
        assert!(tab.pending_edit_stats.is_empty());
        assert_eq!(tab.jobs[0].state, JobState::Cancelled);

        // Um JobDone/Listed atrasado não pode ressuscitar a edição nem
        // disparar o upload descartado.
        tab.handle_event(SftpEvent::JobDone {
            id,
            outcome: JobOutcome::Completed { ignored_links: 0 },
        });
        tab.handle_event(SftpEvent::Listed {
            id: 77,
            path: "/r".to_string(),
            result: Ok(vec![sftp_entry("/r/a.txt", 4, Some(9))]),
        });
        assert_eq!(tab.jobs[0].state, JobState::Cancelled);
        assert_eq!(tab.jobs.len(), 1);
        assert!(tab.edits.is_empty());
    }

    #[test]
    fn remote_changed_detects_size_mtime_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"x").unwrap();
        let mut edit = edited_file("/r/a.txt", local);
        edit.remote_size = 10;
        edit.remote_mtime = Some(5);
        assert!(!remote_changed(
            &[sftp_entry("/r/a.txt", 10, Some(5))],
            &edit
        ));
        assert!(remote_changed(
            &[sftp_entry("/r/a.txt", 11, Some(5))],
            &edit
        ));
        assert!(remote_changed(
            &[sftp_entry("/r/a.txt", 10, Some(6))],
            &edit
        ));
        assert!(remote_changed(&[], &edit));
    }

    #[test]
    fn changed_remote_asks_for_overwrite_confirmation() {
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(EditedFile {
            remote: "/r/a.txt".to_string(),
            local: PathBuf::from("/nao/existe"),
            remote_size: 10,
            remote_mtime: Some(5),
            local_size: 1,
            local_mtime: UNIX_EPOCH,
            dirty: true,
            stable_since: None,
        });
        tab.pending_edit_stats.push(PendingEditStat {
            list_id: 9,
            edit: 0,
            remote: "/r/a.txt".to_string(),
            purpose: EditStatPurpose::BeforeUpload,
        });
        let result = Ok(vec![sftp_entry("/r/a.txt", 12, Some(5))]);
        assert!(tab.handle_edit_stat_listed(9, &result));
        assert_eq!(tab.confirm, Some(Confirm::OverwriteRemote { edit: 0 }));
        assert!(tab.pending_edit_stats.is_empty());
    }

    #[test]
    fn completed_edit_upload_updates_snapshot_and_clears_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"novo").unwrap();
        let metadata = std::fs::metadata(&local).unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(EditedFile {
            remote: "/r/a.txt".to_string(),
            local: local.clone(),
            remote_size: 2,
            remote_mtime: Some(2),
            local_size: 2,
            local_mtime: UNIX_EPOCH,
            dirty: true,
            stable_since: Some(Instant::now()),
        });
        tab.pending_edit_uploads.push(PendingEditUpload {
            job_id: 5,
            edit: 0,
            remote: "/r/a.txt".to_string(),
            local_size: metadata.len(),
            local_mtime: metadata.modified().ok(),
        });
        tab.finish_edit_job(5, true);
        assert!(!tab.edits[0].dirty);
        assert!(tab.edits[0].stable_since.is_none());
        assert_eq!(tab.edits[0].local_size, metadata.len());
        assert_eq!(tab.edits[0].remote_size, metadata.len());
        assert!(tab.pending_edit_uploads.is_empty());
    }

    #[test]
    fn local_change_during_upload_keeps_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"antigo").unwrap();
        let metadata = std::fs::metadata(&local).unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(EditedFile {
            remote: "/r/a.txt".to_string(),
            local: local.clone(),
            remote_size: 6,
            remote_mtime: Some(3),
            local_size: metadata.len(),
            local_mtime: UNIX_EPOCH,
            dirty: true,
            stable_since: None,
        });
        tab.pending_edit_uploads.push(PendingEditUpload {
            job_id: 7,
            edit: 0,
            remote: "/r/a.txt".to_string(),
            local_size: metadata.len(),
            local_mtime: metadata.modified().ok(),
        });
        // O editor salvou de novo enquanto o upload estava em andamento.
        std::fs::write(&local, b"conteudo novo").unwrap();
        tab.finish_edit_job(7, true);
        assert!(tab.edits[0].dirty);
        assert_eq!(tab.edits[0].remote_size, 6);
        assert!(tab.pending_edit_uploads.is_empty());
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("mudou durante o envio"));
    }

    #[test]
    fn failed_edit_upload_retry_is_routed_back_to_the_edit_flow() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"a").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(edited_file("/r/a.txt", local.clone()));
        let retry = RetrySpec::Upload {
            sources: vec![local],
            dest: "/r".to_string(),
        };
        assert_eq!(tab.edit_for_failed_upload(&retry), Some(0));
        let other = RetrySpec::Upload {
            sources: vec![PathBuf::from("/fora/a.txt")],
            dest: "/r".to_string(),
        };
        assert_eq!(tab.edit_for_failed_upload(&other), None);
        assert_eq!(
            tab.edit_for_failed_upload(&RetrySpec::Download {
                sources: vec!["/r/a.txt".to_string()],
                dest: dir.path().to_path_buf(),
            }),
            None
        );
    }

    #[test]
    fn downloaded_file_becomes_an_edit_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"abc").unwrap();
        let mut tab = SftpTab::connecting(None);
        let index = tab
            .record_edit(PendingEditDownload {
                job_id: 1,
                remote: "/r/a.txt".to_string(),
                local,
                remote_size: 7,
                remote_mtime: Some(9),
            })
            .unwrap();
        let edit = &tab.edits[index];
        assert_eq!(edit.remote, "/r/a.txt");
        assert_eq!(edit.remote_size, 7);
        assert_eq!(edit.remote_mtime, Some(9));
        assert_eq!(edit.local_size, 3);
        assert!(!edit.dirty);
    }

    #[test]
    fn failed_download_is_not_recorded_as_an_edit() {
        let mut tab = SftpTab::connecting(None);
        assert!(tab
            .record_edit(PendingEditDownload {
                job_id: 2,
                remote: "/r/a.txt".to_string(),
                local: PathBuf::from("/nao/existe/a.txt"),
                remote_size: 1,
                remote_mtime: None,
            })
            .is_none());
        assert!(tab.edits.is_empty());
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("não gerou"));
    }

    #[test]
    fn failed_edit_download_job_is_not_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        let mut tab = SftpTab::connecting(None);
        let id = tab.alloc_id();
        tab.jobs.push(JobView {
            id,
            direction: Direction::Download,
            label: "editar a.txt".to_string(),
            state: JobState::Queued,
            bytes_done: 0,
            bytes_total: None,
            items_done: 0,
            items_total: 0,
            ignored_links: 0,
            error: None,
            retry: RetrySpec::Download {
                sources: vec!["/r/a.txt".to_string()],
                dest: dir.path().to_path_buf(),
            },
        });
        tab.pending_edit_downloads.push(PendingEditDownload {
            job_id: id,
            remote: "/r/a.txt".to_string(),
            local: local.clone(),
            remote_size: 1,
            remote_mtime: None,
        });
        tab.handle_event(SftpEvent::JobDone {
            id,
            outcome: JobOutcome::Failed("caiu".to_string()),
        });
        assert!(tab.edits.is_empty());
        assert!(tab.pending_edit_downloads.is_empty());
        assert!(!local.exists());
        assert!(tab.status.as_deref().unwrap_or_default().contains("caiu"));
    }

    #[test]
    fn send_back_without_connection_keeps_dirty() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"a").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(edited_file("/r/a.txt", local));
        tab.edits[0].dirty = true;
        tab.request_send_back(0);
        assert!(tab.edits[0].dirty);
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("sem conexão"));
    }

    #[test]
    fn edit_stat_listed_does_not_touch_the_remote_pane() {
        let mut tab = SftpTab::connecting(None);
        tab.remote.cwd = "/atual".to_string();
        tab.remote.listing = Listing::Ready(vec![entry("a.txt", EntryKind::File)]);
        tab.edits.push(EditedFile {
            remote: "/r/a.txt".to_string(),
            local: PathBuf::from("/nao/existe"),
            remote_size: 10,
            remote_mtime: Some(5),
            local_size: 1,
            local_mtime: UNIX_EPOCH,
            dirty: true,
            stable_since: None,
        });
        tab.pending_edit_stats.push(PendingEditStat {
            list_id: 42,
            edit: 0,
            remote: "/r/a.txt".to_string(),
            purpose: EditStatPurpose::BeforeUpload,
        });
        let event = SftpEvent::Listed {
            id: 42,
            path: "/r".to_string(),
            result: Ok(vec![sftp_entry("/r/a.txt", 99, Some(7))]),
        };
        assert!(!tab.handle_event(event));
        assert_eq!(tab.remote.cwd, "/atual");
        assert_eq!(tab.edits[0].remote_size, 10);
        assert!(tab.pending_edit_stats.is_empty());
    }

    #[test]
    fn overwrite_remote_modal_renders_without_confirming() {
        let ctx = egui::Context::default();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(EditedFile {
            remote: "/r/a.txt".to_string(),
            local: PathBuf::from("/nao/existe"),
            remote_size: 1,
            remote_mtime: Some(1),
            local_size: 1,
            local_mtime: UNIX_EPOCH,
            dirty: true,
            stable_since: None,
        });
        tab.confirm = Some(Confirm::OverwriteRemote { edit: 0 });
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            let _ = sftp_modals(ui.ctx(), &mut tab);
        });
        assert_eq!(tab.confirm, Some(Confirm::OverwriteRemote { edit: 0 }));
        assert!(tab.jobs.is_empty());
    }

    #[test]
    fn sftp_view_renders_the_edits_panel() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        let clean = dir.path().join("b.txt");
        std::fs::write(&local, b"x").unwrap();
        std::fs::write(&clean, b"y").unwrap();
        let mut tab = SftpTab::connecting(None);
        let mut edit = edited_file("/r/a.txt", local);
        edit.dirty = true;
        tab.edits.push(edit);
        tab.edits.push(edited_file("/r/b.txt", clean));
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                let _ = sftp_view(ui, &mut tab, &test_session(), false);
            });
        });
        // A lista mostra também a cópia limpa, que continua descartável.
        assert_eq!(tab.edits.len(), 2);
        assert!(tab.edits[0].dirty);
        assert!(!tab.edits[1].dirty);
    }

    #[test]
    fn failed_edit_upload_keeps_dirty_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("a.txt");
        std::fs::write(&local, b"a").unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(edited_file("/r/a.txt", local));
        tab.edits[0].dirty = true;
        tab.pending_edit_uploads.push(PendingEditUpload {
            job_id: 6,
            edit: 0,
            remote: "/r/a.txt".to_string(),
            local_size: 1,
            local_mtime: None,
        });
        tab.finish_edit_job(6, false);
        assert!(tab.edits[0].dirty);
        assert!(tab.pending_edit_uploads.is_empty());
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("edição mantida"));
    }

    #[test]
    fn begin_edit_on_a_clean_entry_reopens_without_downloading() {
        let mut tab = SftpTab::connecting(None);
        tab.edits.push(EditedFile {
            remote: "/r/a.txt".to_string(),
            local: PathBuf::from("/nao/existe/a.txt"),
            remote_size: 1,
            remote_mtime: None,
            local_size: 1,
            local_mtime: UNIX_EPOCH,
            dirty: false,
            stable_since: None,
        });
        assert!(tab
            .begin_edit(&test_session(), "/r/a.txt".to_string(), (1, None))
            .is_none());
        // Nem baixa de novo nem abre uma segunda cópia: reusa a lista.
        assert!(tab.jobs.is_empty());
        assert!(tab.pending_edit_downloads.is_empty());
        assert_eq!(tab.edits.len(), 1);
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("não existe"));
    }

    #[test]
    fn begin_edit_preserves_untracked_temp_copy() {
        let mut session = test_session();
        // Chave exclusiva para não disputar a raiz real com outros testes.
        session.host = "untracked-edit.test".to_string();
        let key = edit_key(&session);
        let root = edit_temp_root();
        let session_dir = root.join(&key);
        let _ = std::fs::remove_dir_all(&session_dir);
        let local = edit_temp_path(&root, &key, "/r/a.txt").unwrap();
        std::fs::create_dir_all(local.parent().unwrap()).unwrap();
        std::fs::write(&local, b"nao enviado").unwrap();

        let mut tab = SftpTab::connecting(None);
        // open_existing=false: o teste não pode abrir um editor real.
        assert!(tab
            .begin_edit_impl(&session, "/r/a.txt".to_string(), (10, Some(3)), false)
            .is_none());
        assert!(tab.jobs.is_empty(), "não pode enfileirar Download");
        assert!(tab.pending_edit_downloads.is_empty());
        assert_eq!(tab.edits.len(), 1);
        assert_eq!(tab.edits[0].remote, "/r/a.txt");
        assert_eq!(tab.edits[0].local, local);
        assert!(tab.edits[0].dirty, "cópia desconhecida deve exigir envio");
        assert_eq!(std::fs::read(&local).unwrap(), b"nao enviado");
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("preservada"));
        let _ = std::fs::remove_dir_all(&session_dir);
    }

    #[test]
    fn begin_edit_refuses_duplicate_and_the_twenty_first() {
        let mut tab = SftpTab::connecting(None);
        let session = test_session();
        for index in 0..EDIT_LIMIT {
            tab.edits.push(EditedFile {
                remote: format!("/r/{index}.txt"),
                local: PathBuf::from(format!("/tmp/{index}.txt")),
                remote_size: 0,
                remote_mtime: None,
                local_size: 0,
                local_mtime: UNIX_EPOCH,
                dirty: false,
                stable_since: None,
            });
        }
        assert!(tab
            .begin_edit(&session, "/r/novo.txt".to_string(), (0, None))
            .is_none());
        assert!(tab.status.as_deref().unwrap_or_default().contains("20"));
        tab.edits.truncate(1);
        tab.edits[0].dirty = true;
        assert!(tab
            .begin_edit(&session, "/r/0.txt".to_string(), (0, None))
            .is_none());
        assert!(tab
            .status
            .as_deref()
            .unwrap_or_default()
            .contains("já está em edição"));
    }

    #[test]
    fn sftp_state_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sftp.json");
        let mut state = SftpState::default();
        state.paths.insert(
            "h:22:u".into(),
            SavedPath {
                remote: "/var".into(),
                local: "/tmp".into(),
            },
        );
        save_sftp_state(&path, &state).unwrap();
        let back = load_sftp_state(&path);
        assert_eq!(back.paths["h:22:u"].remote, "/var");
        assert_eq!(back.split, 0.5);
        assert!(back.hidden);
    }

    #[test]
    fn corrupt_state_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sftp.json");
        std::fs::write(&path, "{").unwrap();
        let state = load_sftp_state(&path);
        assert_eq!(state.split, 0.5);
        assert!(state.hidden);
    }

    #[test]
    fn session_path_key_matches_keychain_format() {
        let mut session = test_session();
        session.user = Some("root".to_string());
        assert_eq!(session_path_key(&session), "127.0.0.1:22:root");
    }

    #[test]
    fn apply_saved_paths_restores_local_dir_and_layout() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("proj");
        std::fs::create_dir(&sub).unwrap();
        let mut state = SftpState {
            hidden: false,
            split: 0.9,
            ..SftpState::default()
        };
        state.paths.insert(
            "127.0.0.1:22:".to_string(),
            SavedPath {
                remote: "/srv".to_string(),
                local: sub.display().to_string(),
            },
        );
        let mut tab = SftpTab::connecting(None);
        apply_saved_paths(&mut tab, &state, &test_session());
        assert_eq!(tab.local.cwd, sub);
        assert!(!tab.hidden);
        // O split persistido fora da faixa é grampeado.
        assert_eq!(tab.split, 0.75);
    }

    #[test]
    fn successful_navigation_is_marked_for_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.navigate_local(dir.path().to_path_buf());
        assert!(tab.take_state_changes().paths);

        tab.navigate_remote("/srv".to_string());
        // Sem conexão o List não sai; simula a resposta do pedido mais recente.
        tab.list_id = Some(9);
        tab.apply_listed(9, "/srv".to_string(), Ok(Vec::new()));
        let changes = tab.take_state_changes();
        assert!(changes.paths);
        assert!(!changes.layout);
    }

    #[test]
    fn apply_saved_paths_ignores_local_dir_that_does_not_exist() {
        let mut state = SftpState::default();
        state.paths.insert(
            "127.0.0.1:22:".to_string(),
            SavedPath {
                remote: "/srv".to_string(),
                local: "/nao/existe/sshc-local".to_string(),
            },
        );
        let mut tab = SftpTab::connecting(None);
        apply_saved_paths(&mut tab, &state, &test_session());
        assert_eq!(tab.local.cwd, home_dir());
    }

    #[test]
    fn local_navigation_before_remote_listing_does_not_save_slash() {
        let dir = tempfile::tempdir().unwrap();
        let mut tab = SftpTab::connecting(None);
        tab.navigate_local(dir.path().to_path_buf());
        assert!(tab.take_state_changes().paths);
        let saved = tab.saved_path();
        assert_eq!(saved.local, dir.path().display().to_string());
        // O "/" provisório de PaneState::new não pode ir para o disco.
        assert!(saved.remote.is_empty());

        let path = dir.path().join("sftp.json");
        let mut state = SftpState::default();
        state.paths.insert("127.0.0.1:22:".to_string(), saved);
        save_sftp_state(&path, &state).unwrap();
        let back = load_sftp_state(&path);
        assert!(back.paths["127.0.0.1:22:"].remote.is_empty());
    }

    #[test]
    fn failed_list_keeps_last_confirmed_remote() {
        let mut tab = SftpTab::connecting(None);
        tab.list_id = Some(1);
        tab.apply_listed(1, "/srv".to_string(), Ok(Vec::new()));
        assert_eq!(tab.saved_path().remote, "/srv");

        // A navegação falha: o caminho confirmado anterior continua valendo.
        tab.navigate_remote("/quebrado".to_string());
        tab.list_id = Some(2);
        tab.apply_listed(2, "/quebrado".to_string(), Err("sem permissão".to_string()));
        assert_eq!(tab.saved_path().remote, "/srv");
        assert!(!tab.take_state_changes().paths);
    }

    #[test]
    fn successful_listed_updates_saved_remote() {
        let mut tab = SftpTab::connecting(None);
        tab.list_id = Some(1);
        tab.apply_listed(1, "/srv".to_string(), Ok(Vec::new()));
        assert_eq!(tab.saved_path().remote, "/srv");

        tab.navigate_remote("/srv/www".to_string());
        tab.list_id = Some(2);
        tab.apply_listed(2, "/srv/www".to_string(), Ok(Vec::new()));
        assert_eq!(tab.saved_path().remote, "/srv/www");
        assert!(tab.take_state_changes().paths);
    }
}
