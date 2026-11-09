//! Aba do explorador SFTP.
//!
//! A aba guarda as duas panes (local e remota), a navegação e a listagem.
//! Também vivem aqui a fila de transferências, os conflitos, o drag & drop,
//! o clipboard interno e as operações de arquivo (menus e diálogos); a
//! edição remota (Task 13) entra depois.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, Utc};
use eframe::egui;
use sshc_core::Session;
use sshc_net::sftp::{
    normalize_remote, ConflictDecision, ConflictPolicy, Direction, EntryKind, JobOutcome, SftpCmd,
    SftpConnection, SftpEntry, SftpEvent,
};
use sshc_net::Credentials;

const ERROR: egui::Color32 = egui::Color32::from_rgb(248, 113, 113);
const OK: egui::Color32 = egui::Color32::from_rgb(74, 222, 128);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(96, 165, 250);

const ROW_HEIGHT: f32 = 20.0;
const HEADER_HEIGHT: f32 = 18.0;
const SPLIT_MIN: f32 = 0.25;
const SPLIT_MAX: f32 = 0.75;
const SPLIT_HANDLE: f32 = 8.0;

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
    /// Arquivo remoto mudou antes do envio de volta. A Task 13 liga o fluxo
    /// que constrói esta confirmação.
    #[allow(dead_code)]
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
        };
        tab.reload_local();
        tab
    }

    /// Algum modal da aba está aberto (usado no guard de atalhos).
    pub fn has_modal(&self) -> bool {
        self.confirm.is_some() || self.file_dialog.is_some() || !self.conflicts.is_empty()
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
                self.remote.cwd = path;
                self.remote.listing = Listing::Ready(entries);
            }
            Err(error) => {
                self.remote.listing = Listing::Error(error);
            }
        }
        self.remote.clear_selection();
    }

    /// Processa um evento do ator. Devolve true quando a conexão encerrou.
    pub fn handle_event(&mut self, event: SftpEvent) -> bool {
        match event {
            SftpEvent::Listed { id, path, result } => {
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
        self.enqueue(direction, label, retry);
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

    pub fn finished_jobs(&self) -> usize {
        self.jobs
            .iter()
            .filter(|job| !job.state.is_active())
            .count()
    }

    pub fn clear_finished(&mut self) {
        self.jobs.retain(|job| job.state.is_active());
    }

    /// Fechar com job ativo exige confirmação; devolve true quando o modal
    /// foi aberto e o App não deve fechar agora.
    pub fn request_close(&mut self, id: u64) -> bool {
        if self.has_active_jobs() {
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

fn open_local(path: &Path) -> std::io::Result<()> {
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
        ui.checkbox(&mut tab.hidden, "Mostrar ocultos")
            .on_hover_text("arquivos que começam com ponto");
    });
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
        PaneAction::OpenLocal(path) => match open_local(&path) {
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
            tab.status = Some(format!("editar arquivo remoto (Task 13): {path}"))
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

fn close_confirm_modal(ctx: &egui::Context, tab: &mut SftpTab, id: u64) -> SftpAction {
    let mut confirmed = false;
    let mut cancelled = false;
    egui::Modal::new(egui::Id::new("sftp-close-tab")).show(ctx, |ui| {
        ui.set_min_width(340.0);
        ui.heading("Fechar aba SFTP?");
        ui.add_space(4.0);
        ui.label(format!(
            "{} transferência(s) ativa(s) serão canceladas.",
            tab.active_jobs()
        ));
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
        // O envio em si é ligado na Task 13; o índice fica reservado.
        tab.status = Some(format!(
            "envio remoto será concluído na Task 13 (arquivo {edit})"
        ));
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
}
