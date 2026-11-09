//! Aba do explorador SFTP.
//!
//! A aba guarda as duas panes (local e remota), a navegação e a listagem.
//! Transferências, clipboard, operações de arquivo e edição entram nas
//! próximas tasks; aqui o foco é listar e navegar.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Local, Utc};
use eframe::egui;
use sshc_core::Session;
use sshc_net::sftp::{normalize_remote, EntryKind, SftpCmd, SftpConnection, SftpEntry};
use sshc_net::Credentials;

const ERROR: egui::Color32 = egui::Color32::from_rgb(248, 113, 113);
const OK: egui::Color32 = egui::Color32::from_rgb(74, 222, 128);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(96, 165, 250);

const ROW_HEIGHT: f32 = 20.0;
const HEADER_HEIGHT: f32 = 18.0;
const SPLIT_MIN: f32 = 0.25;
const SPLIT_MAX: f32 = 0.75;
const SPLIT_HANDLE: f32 = 8.0;
const FOOTER_HEIGHT: f32 = 24.0;

/// Ids monotônicos para SftpCmd::List. A correlação por id entra na Task 11;
/// por ora só a pane remota usa e eventos fora do cwd atual são ignorados.
static NEXT_LIST_ID: AtomicU64 = AtomicU64::new(1);

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
    NeedAuth,
    /// Mensagem efêmera para a barra de status do app.
    Status(String),
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
        }
    }

    fn clear_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
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
        };
        tab.reload_local();
        tab
    }

    /// Chamado quando o connect termina: na primeira conexão começa no home;
    /// numa reconexão tenta de novo o cwd que já estava na pane.
    pub fn on_connected(&mut self) {
        if matches!(&self.remote.listing, Listing::Loading) {
            self.remote.cwd = self.home.clone().unwrap_or_else(|| "/".to_string());
        }
        self.remote.filter.clear();
        self.remote.edit = None;
        self.reload_remote();
    }

    pub fn disconnect(&mut self) {
        self.conn = None;
        self.state = ConnState::Closed("desconectado".to_string());
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
        let Some(conn) = &self.conn else {
            self.remote.listing = Listing::Error("sem conexão".to_string());
            return;
        };
        let id = NEXT_LIST_ID.fetch_add(1, Ordering::Relaxed);
        self.remote.listing = Listing::Loading;
        if !conn.send(SftpCmd::List { id, path }) {
            self.remote.listing = Listing::Error("conexão encerrada".to_string());
        }
    }

    pub fn navigate_remote(&mut self, path: String) {
        self.remote.cwd = path;
        self.remote.clear_navigation();
        self.reload_remote();
    }

    /// Aplica um Listed que chegou do ator. Eventos de um cwd antigo são
    /// descartados; a Task 11 troca isso por correlação de id.
    pub fn apply_listed(
        &mut self,
        path: String,
        result: std::result::Result<Vec<SftpEntry>, String>,
    ) {
        if path != self.remote.cwd {
            return;
        }
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

/// Ação que uma pane pede à view.
enum PaneAction {
    None,
    NavigateLocal(PathBuf),
    NavigateRemote(String),
    OpenLocal(PathBuf),
    EditRemote(String),
    ReloadLocal,
    ReloadRemote,
}

/// Ação pedida pela lista de uma pane.
enum ListAction {
    None,
    Open(usize),
    Up,
    EditPath,
    Reload,
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
pub fn sftp_view(ui: &mut egui::Ui, tab: &mut SftpTab, session: &Session) -> SftpAction {
    let mut action = SftpAction::None;
    let mut status = None;

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
    let height = (ui.available_height() - FOOTER_HEIGHT).max(120.0);
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
                            local_pane(ui, &mut tab.local, tab.hidden, &home, &mut tab.focus)
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
            Ok(()) => status = Some(format!("abrindo {}", path.display())),
            Err(error) => status = Some(format!("não abri {}: {error}", path.display())),
        },
        _ => {}
    }
    match remote_action {
        PaneAction::NavigateRemote(path) => tab.navigate_remote(path),
        PaneAction::ReloadRemote => tab.reload_remote(),
        PaneAction::EditRemote(path) => {
            status = Some(format!("editar arquivo remoto (Task 13): {path}"))
        }
        _ => {}
    }

    ui.separator();
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
    });

    match action {
        SftpAction::None => status.map(SftpAction::Status).unwrap_or(SftpAction::None),
        other => other,
    }
}

fn pane_stroke(ui: &egui::Ui, focused: bool) -> egui::Stroke {
    if focused {
        egui::Stroke::new(1.0, ACCENT)
    } else {
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
    }
}

fn local_pane(
    ui: &mut egui::Ui,
    state: &mut PaneState<PathBuf>,
    hidden: bool,
    home: &Path,
    focus: &mut Side,
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
        ui.add_enabled(false, egui::Button::new("Nova pasta").small())
            .on_disabled_hover_text("chega na Task 12");
        ui.add_enabled(false, egui::Button::new("Enviar…").small())
            .on_disabled_hover_text("transferências chegam na Task 11");
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
        if response.has_focus() {
            *focus = Side::Local;
        }
    });

    ui.separator();
    match list_view(ui, state, hidden, false, Side::Local, focus) {
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
) -> PaneAction {
    let action = PaneAction::None;

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Remoto").strong());
    });

    let Some(connection_note) = connection_note else {
        return remote_connected_pane(ui, state, hidden, home, focus, action);
    };

    ui.label(egui::RichText::new(connection_note).weak());
    ui.separator();
    ui.add_space(10.0);
    ui.label(egui::RichText::new(&state.cwd).weak().small());
    action
}

fn remote_connected_pane(
    ui: &mut egui::Ui,
    state: &mut PaneState<String>,
    hidden: bool,
    home: Option<&str>,
    focus: &mut Side,
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
        ui.add_enabled(false, egui::Button::new("Nova pasta").small())
            .on_disabled_hover_text("chega na Task 12");
        ui.add_enabled(false, egui::Button::new("Baixar…").small())
            .on_disabled_hover_text("transferências chegam na Task 11");
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
        if response.has_focus() {
            *focus = Side::Remote;
        }
    });

    ui.separator();
    match list_view(ui, state, hidden, true, Side::Remote, focus) {
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
    }

    action
}

fn start_path_edit<P>(state: &mut PaneState<P>, input: String) {
    state.edit = Some(PathEdit {
        input,
        error: None,
        request_focus: true,
    });
}

fn list_view<P: Clone>(
    ui: &mut egui::Ui,
    state: &mut PaneState<P>,
    hidden: bool,
    is_remote: bool,
    side: Side,
    focus: &mut Side,
) -> ListAction {
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
        return ListAction::None;
    }

    let mut action = ListAction::None;
    let mut scroll_to = None;
    if *focus == side && edit.is_none() && !ui.ctx().egui_wants_keyboard_input() {
        let keys = ui.input(|input| {
            (
                input.modifiers.command && input.key_pressed(egui::Key::A),
                input.key_pressed(egui::Key::ArrowDown),
                input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::Enter),
                input.key_pressed(egui::Key::Backspace) && !input.modifiers.command,
                input.modifiers.command && input.key_pressed(egui::Key::ArrowUp),
                input.key_pressed(egui::Key::Slash),
            )
        });
        if keys.4 || keys.5 {
            return ListAction::Up;
        }
        if keys.6 {
            return ListAction::EditPath;
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
    egui::ScrollArea::vertical()
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
                    egui::Sense::click(),
                );
                let is_selected = selected.contains(&entry.name);
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

                let painter = ui.painter();
                let font = egui::TextStyle::Body.resolve(ui.style());
                let weak_font = egui::TextStyle::Small.resolve(ui.style());
                let text_color = ui.visuals().text_color();
                let weak_color = ui.visuals().weak_text_color();
                let mut x = rect.left() + 6.0;
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
                let name_clip = egui::Rect::from_min_size(
                    egui::pos2(x, rect.top()),
                    egui::vec2(columns.name, rect.height()),
                );
                painter.with_clip_rect(name_clip).text(
                    egui::pos2(x, rect.center().y),
                    egui::Align2::LEFT_CENTER,
                    name,
                    font.clone(),
                    text_color,
                );
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
        tab.apply_listed(
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
}
