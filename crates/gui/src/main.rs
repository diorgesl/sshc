mod keychain;
mod sftp;
mod update;

use anyhow::Result;
use eframe::egui;
use sftp::{
    apply_saved_paths, load_sftp_state, save_sftp_state, session_path_key, sftp_modals, sftp_view,
    ConnState, SftpAction, SftpState, SftpTab,
};
use sshc_core::{cloud, securecrt, Protocol, Session, SessionEntry, Store};
use sshc_net::sftp::SftpConnection;
use sshc_net::{Connection, Credentials};
use sshc_sync::telegram::{self, BackupReport, RestoreReport, TelegramConfig};
use sshc_term::{Color, Terminal};
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(96, 165, 250);
const TERM_BG: egui::Color32 = egui::Color32::from_rgb(12, 14, 18);
const ERROR: egui::Color32 = egui::Color32::from_rgb(248, 113, 113);

fn main() -> Result<()> {
    let (width, height) = if std::env::var_os("SSHC_GUI_SMOKE").is_some() {
        (1120.0, 720.0)
    } else {
        load_window_size()
            .map(|(width, height)| (width as f32, height as f32))
            .unwrap_or((1120.0, 720.0))
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([width, height])
            .with_min_inner_size([760.0, 480.0])
            .with_title("sshc"),
        ..Default::default()
    };
    eframe::run_native("sshc", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
        .map_err(|e| anyhow::anyhow!("falha ao abrir a janela: {e}"))?;
    Ok(())
}

enum Action {
    None,
    OpenSftp,
    Disconnect,
    Reconnect,
    Close,
}

/// Tema do app; o terminal mantém a paleta escura própria.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeChoice {
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    fn label(self) -> &'static str {
        match self {
            Self::System => "Sistema",
            Self::Light => "Claro",
            Self::Dark => "Escuro",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    fn from_key(value: &str) -> Option<Self> {
        match value.trim() {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    fn preference(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Light => egui::ThemePreference::Light,
            Self::Dark => egui::ThemePreference::Dark,
        }
    }

    /// `SSHC_THEME` vence o arquivo (usado nas capturas de tela).
    fn load() -> Self {
        std::env::var("SSHC_THEME")
            .ok()
            .and_then(|value| Self::from_key(&value))
            .or_else(|| {
                std::fs::read_to_string(theme_path())
                    .ok()
                    .and_then(|text| Self::from_key(&text))
            })
            .unwrap_or(Self::System)
    }

    fn save(self) {
        let path = theme_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, self.key());
    }
}

/// Aplica o tema e ajusta o contraste do modo claro: o egui usa cinza 80
/// para texto normal, que fica lavado em telas claras. Deixamos texto e
/// ícones bem mais escuros, mantendo o modo escuro intacto.
fn apply_theme(ctx: &egui::Context, theme: ThemeChoice) {
    ctx.set_theme(theme.preference());
    ctx.style_mut_of(egui::Theme::Light, |style| {
        let text = egui::Color32::from_gray(28);
        style.visuals.override_text_color = Some(text);
        style.visuals.weak_text_color = Some(egui::Color32::from_gray(64));
        let widgets = &mut style.visuals.widgets;
        widgets.noninteractive.fg_stroke.color = text;
        widgets.inactive.fg_stroke.color = text;
        widgets.hovered.fg_stroke.color = text;
        widgets.active.fg_stroke.color = text;
    });
}

fn theme_path() -> PathBuf {
    cloud::app_data_dir().join("theme")
}

/// Grupos recolhidos, um por linha (chave com barra na frente; raiz = "/").
fn groups_state_path() -> PathBuf {
    cloud::app_data_dir().join("groups")
}

fn group_state_key(group: &str) -> String {
    format!("/{group}")
}

fn parse_collapsed_groups(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

fn collapsed_groups_text(groups: &BTreeSet<String>) -> String {
    let mut text = String::new();
    for group in groups {
        text.push_str(group);
        text.push('\n');
    }
    text
}

fn load_collapsed_groups() -> BTreeSet<String> {
    std::fs::read_to_string(groups_state_path())
        .map(|text| parse_collapsed_groups(&text))
        .unwrap_or_default()
}

fn save_collapsed_groups(groups: &BTreeSet<String>) {
    let path = groups_state_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, collapsed_groups_text(groups));
}

fn window_size_path() -> PathBuf {
    cloud::app_data_dir().join("window")
}

fn parse_window_size(text: &str) -> Option<(u32, u32)> {
    let (width, height) = text.trim().split_once('x')?;
    let width: u32 = width.trim().parse().ok()?;
    let height: u32 = height.trim().parse().ok()?;
    Some((width.max(760), height.max(480)))
}

fn load_window_size() -> Option<(u32, u32)> {
    parse_window_size(&std::fs::read_to_string(window_size_path()).ok()?)
}

fn save_window_size(width: u32, height: u32) {
    let path = window_size_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, format!("{width}x{height}"));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FormMode {
    New,
    Edit,
    Quick,
}

/// Formulário de cadastro/edição/conexão rápida.
struct FormState {
    mode: FormMode,
    editing: Option<PathBuf>,
    group: String,
    /// Trocou o select para "Nova pasta…" e está digitando o nome.
    new_group: bool,
    name: String,
    protocol: Protocol,
    host: String,
    port: String,
    user: String,
    baud: String,
    emulation: String,
    error: Option<String>,
}

impl FormState {
    fn new() -> Self {
        Self {
            mode: FormMode::New,
            editing: None,
            group: String::new(),
            new_group: false,
            name: String::new(),
            protocol: Protocol::Ssh,
            host: String::new(),
            port: "22".into(),
            user: String::new(),
            baud: "9600".into(),
            emulation: "xterm-256color".into(),
            error: None,
        }
    }

    fn from_entry(entry: &SessionEntry, mode: FormMode) -> Self {
        let s = &entry.session;
        let name = if mode == FormMode::Edit {
            s.name.clone()
        } else {
            format!("{} (cópia)", s.name)
        };
        Self {
            mode,
            editing: (mode == FormMode::Edit).then(|| entry.path.clone()),
            group: entry.group.clone(),
            new_group: false,
            name,
            protocol: s.protocol.clone(),
            host: s.host.clone(),
            port: if s.port == 0 {
                String::new()
            } else {
                s.port.to_string()
            },
            user: s.user.clone().unwrap_or_default(),
            baud: s.baud.unwrap_or(9600).to_string(),
            emulation: s.emulation.clone(),
            error: None,
        }
    }

    fn quick(selected: Option<(Session, String, PathBuf)>) -> Self {
        let mut form = Self::new();
        form.mode = FormMode::Quick;
        if let Some((s, _, _)) = selected {
            form.protocol = s.protocol;
            form.host = s.host;
            form.port = if s.port == 0 {
                String::new()
            } else {
                s.port.to_string()
            };
            form.user = s.user.unwrap_or_default();
            form.emulation = s.emulation;
        }
        form
    }

    fn default_port(&mut self) {
        self.port = match self.protocol {
            Protocol::Ssh => "22".into(),
            Protocol::Telnet => "23".into(),
            _ => String::new(),
        };
    }

    fn build(&self) -> Result<Session, String> {
        if self.mode != FormMode::Quick && self.name.trim().is_empty() {
            return Err("informe um nome".into());
        }
        if self.new_group {
            validate_new_group(&self.group)?;
        }
        let host = self.host.trim();
        if host.is_empty() {
            return Err("informe o host (ou o dispositivo serial)".into());
        }
        let serial = matches!(self.protocol, Protocol::Serial);
        let port = if serial || matches!(self.protocol, Protocol::LocalShell) {
            0
        } else {
            let port: u16 = self
                .port
                .trim()
                .parse()
                .map_err(|_| "porta inválida".to_string())?;
            if port == 0 {
                return Err("porta inválida".into());
            }
            port
        };
        let baud = if serial {
            Some(
                self.baud
                    .trim()
                    .parse::<u32>()
                    .map_err(|_| "baud inválido".to_string())?,
            )
        } else {
            None
        };
        let user = self.user.trim();
        Ok(Session {
            name: if self.mode == FormMode::Quick {
                host.to_string()
            } else {
                self.name.trim().to_string()
            },
            protocol: self.protocol.clone(),
            host: host.to_string(),
            port,
            user: (!user.is_empty()).then(|| user.to_string()),
            emulation: if self.emulation.trim().is_empty() {
                "xterm-256color".to_string()
            } else {
                self.emulation.trim().to_string()
            },
            auth: None,
            description: None,
            baud,
        })
    }
}

/// Uma aba: sessão conectada (ou com a conexão caída) e seu conteúdo.
struct Tab {
    id: u64,
    session: Session,
    path: Option<PathBuf>,
    body: TabBody,
}

/// Conteúdo da aba.
// O plano fixa as variantes como valores diretos; o terminal é grande por
// natureza e a aba SFTP é bem menor que ele.
#[allow(clippy::large_enum_variant)]
enum TabBody {
    Terminal(TerminalTab),
    Sftp(SftpTab),
}

/// Estado do terminal de uma aba.
struct TerminalTab {
    term: Terminal,
    conn: Option<Connection>,
    /// Credenciais que abriram a conexão (em memória, para abrir o SFTP).
    creds: Option<Credentials>,
    sel: Option<Selection>,
    search: Option<Search>,
    log: Option<TabLog>,
    notice: Option<String>,
}

/// Conexão em andamento: o socket abre numa thread, a UI não trava.
struct PendingConnect {
    session: Session,
    path: Option<PathBuf>,
    rx: std::sync::mpsc::Receiver<std::result::Result<Connection, String>>,
    auth: Option<SshAuth>,
    /// Credenciais usadas; ficam na aba para abrir o SFTP depois.
    creds: Option<Credentials>,
    /// (chave do Keychain, senha) a gravar quando a conexão der certo.
    save_password: Option<(String, String)>,
}

/// Conexão SFTP em andamento: como o terminal, o handshake roda em thread.
struct PendingSftp {
    tab_id: u64,
    rx: std::sync::mpsc::Receiver<std::result::Result<SftpConnection, String>>,
}

/// Janela de backup/restore no Telegram; o tráfego de rede roda em thread.
struct TelegramUi {
    open: bool,
    config: Option<TelegramConfig>,
    token: String,
    chat: String,
    busy: bool,
    rx: Option<std::sync::mpsc::Receiver<std::result::Result<TelegramDone, String>>>,
    message: Option<String>,
    error: Option<String>,
}

enum TelegramDone {
    Connected(TelegramConfig),
    Backup(BackupReport),
    Restore(RestoreReport),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TelegramAction {
    None,
    Login,
    Backup,
    Restore,
    Forget,
}

impl TelegramUi {
    fn new() -> Self {
        Self {
            open: false,
            config: telegram::load_config().ok().flatten(),
            token: String::new(),
            chat: String::new(),
            busy: false,
            rx: None,
            message: None,
            error: None,
        }
    }
}

fn start_telegram_job(
    telegram: &mut TelegramUi,
    job: impl FnOnce() -> std::result::Result<TelegramDone, String> + Send + 'static,
) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(job());
    });
    telegram.rx = Some(rx);
    telegram.busy = true;
    telegram.message = None;
    telegram.error = None;
}

/// Checagem/instalação de atualização; rede em thread, como o Telegram.
struct UpdatesUi {
    open: bool,
    info: Option<update::UpdateInfo>,
    rx: Option<std::sync::mpsc::Receiver<UpdateDone>>,
    installing: bool,
    manual: bool,
    checked: bool,
    error: Option<String>,
}

enum UpdateDone {
    Checked(std::result::Result<Option<update::UpdateInfo>, String>),
    Installed(std::result::Result<(), String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UpdateAction {
    None,
    Check,
    Install,
    OpenRelease,
}

impl UpdatesUi {
    fn new() -> Self {
        let mut state = Self {
            open: false,
            info: None,
            rx: None,
            installing: false,
            manual: false,
            checked: false,
            error: None,
        };
        state.start_check(false);
        state
    }

    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn start_check(&mut self, manual: bool) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = update::check().map_err(|error| format!("{error:#}"));
            let _ = tx.send(UpdateDone::Checked(result));
        });
        self.rx = Some(rx);
        self.installing = false;
        self.manual = manual;
        self.checked = false;
        self.error = None;
    }

    fn start_install(&mut self, info: update::UpdateInfo) {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = update::install(&info).map_err(|error| format!("{error:#}"));
            let _ = tx.send(UpdateDone::Installed(result));
        });
        self.rx = Some(rx);
        self.installing = true;
        self.manual = false;
        self.error = None;
    }
}

/// Lê uma linha de "host;senha" ou "host;usuário;senha".
fn parse_password_line(line: &str) -> Option<(String, Option<String>, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let parts: Vec<&str> = line
        .split([';', ',', '\t'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    match parts.as_slice() {
        [host, password] => Some((host.to_string(), None, password.to_string())),
        [host, user, password] => Some((
            host.to_string(),
            Some(user.to_string()),
            password.to_string(),
        )),
        _ => None,
    }
}

/// Chave do item no Keychain: host:porta:usuário.
fn session_key(session: &Session) -> String {
    format!(
        "{}:{}:{}",
        session.host,
        session.port,
        session.user.as_deref().unwrap_or("")
    )
}

/// Transcript da sessão em arquivo (bytes crus, como chegaram).
// ponytail: log bruto, com sequências de escape; se virar problema de leitura,
// limpar pelo parser VT (o mesmo que alimenta a tela) na hora de gravar.
struct TabLog {
    path: PathBuf,
    file: std::fs::File,
}

impl TabLog {
    fn write_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)?;
        self.file.flush()
    }
}

fn timestamp() -> String {
    #[cfg(unix)]
    if let Ok(out) = std::process::Command::new("date")
        .arg("+%Y%m%d-%H%M%S")
        .output()
    {
        if out.status.success() {
            let stamp = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !stamp.is_empty() {
                return stamp;
            }
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

fn log_dir() -> PathBuf {
    sshc_core::cloud::app_data_dir().join("logs")
}

/// Nome sugerido para o arquivo (sem diretório).
fn log_name(session: &Session, stamp: &str) -> String {
    let safe: String = session
        .name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = if safe.is_empty() {
        "sessao".to_string()
    } else {
        safe
    };
    format!("{safe}-{stamp}.log")
}

fn open_log(path: &Path, session: &Session, stamp: &str) -> anyhow::Result<TabLog> {
    let mut file = std::fs::File::create(path)?;
    writeln!(
        file,
        "# sshc — {} ({}) — início {}\n",
        session.name,
        session.endpoint(),
        stamp
    )?;
    Ok(TabLog {
        path: path.to_path_buf(),
        file,
    })
}

/// Log no diretório padrão do app (usado pelo modo smoke).
fn start_log(session: &Session) -> anyhow::Result<TabLog> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;
    let stamp = timestamp();
    let path = dir.join(log_name(session, &stamp));
    open_log(&path, session, &stamp)
}

/// Pergunta pasta e nome do arquivo antes de gravar.
fn start_log_dialog(
    session: &Session,
    last_dir: &mut Option<PathBuf>,
) -> anyhow::Result<Option<TabLog>> {
    let stamp = timestamp();
    let mut dialog = rfd::FileDialog::new()
        .set_title("Salvar log da sessão")
        .set_file_name(log_name(session, &stamp));
    if let Some(dir) = last_dir.as_ref() {
        dialog = dialog.set_directory(dir);
    } else {
        dialog = dialog.set_directory(home_dir());
    }
    let Some(path) = dialog.save_file() else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        *last_dir = Some(parent.to_path_buf());
    }
    Ok(Some(open_log(&path, session, &stamp)?))
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Busca no histórico + tela.
#[derive(Default)]
struct Search {
    query: String,
    /// (linha absoluta, coluna inicial, coluna final)
    matches: Vec<(usize, usize, usize)>,
    current: usize,
    compiled_for: String,
    focused: bool,
}

impl Search {
    /// Recalcula se a busca mudou; devolve true quando mudou.
    fn refresh(&mut self, screen: &sshc_term::Screen) -> bool {
        if self.query == self.compiled_for {
            return false;
        }
        self.matches = find_matches(screen, &self.query);
        self.compiled_for = self.query.clone();
        self.current = 0;
        true
    }

    fn step(&mut self, forward: bool, screen: &mut sshc_term::Screen) {
        if self.matches.is_empty() {
            return;
        }
        let len = self.matches.len();
        self.current = if forward {
            (self.current + 1) % len
        } else {
            (self.current + len - 1) % len
        };
        screen.scroll_to_row(self.matches[self.current].0);
    }
}

/// Acha todas as ocorrências (case-insensitive) no histórico + tela.
fn find_matches(screen: &sshc_term::Screen, query: &str) -> Vec<(usize, usize, usize)> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let width = needle.chars().count();
    let mut out = Vec::new();
    for row in 0..screen.total_rows() {
        let text = screen.absolute_row_text(row).to_lowercase();
        let mut from = 0;
        while let Some(pos) = text[from..].find(&needle) {
            let col = from + pos;
            out.push((row, col, col + width));
            from = col + 1;
        }
    }
    out
}

/// Seleção de texto no grid, em (linha, coluna).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Selection {
    anchor: (usize, usize),
    cursor: (usize, usize),
}

impl Selection {
    /// Início e fim normalizados, sempre início <= fim.
    fn range(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }
}

/// Texto da seleção, com espaços à direita removidos por linha.
/// Texto da seleção. As linhas são **absolutas**: 0 = primeira do histórico.
fn selection_text(screen: &sshc_term::Screen, sel: &Selection) -> String {
    let cols = screen.cols();
    let last = screen.total_rows().saturating_sub(1);
    let ((sr, sc), (er, ec)) = sel.range();
    let er = er.min(last);
    let mut out = String::new();
    for row in sr.min(last)..=er {
        let start = if row == sr { sc } else { 0 };
        let end = if row == er {
            ec.min(cols.saturating_sub(1))
        } else {
            cols - 1
        };
        let mut line = String::new();
        let cells = screen.absolute_row(row);
        for cell in &cells[start..=end.min(cells.len().saturating_sub(1))] {
            line.push(cell.ch);
        }
        out.push_str(line.trim_end());
        if row != er {
            out.push('\n');
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthMode {
    Auto,
    Key,
    Password,
}

/// Credenciais para sessões SSH (nunca salvas em disco).
struct SshAuth {
    session: Session,
    path: Option<PathBuf>,
    /// Aba SFTP que pediu a autenticação (None = terminal).
    sftp_tab: Option<u64>,
    user: String,
    mode: AuthMode,
    password: String,
    key_path: String,
    passphrase: String,
    save_to_keychain: bool,
    error: Option<String>,
}

impl SshAuth {
    fn new(session: Session) -> Self {
        let key_path = std::env::var_os("HOME")
            .map(|home| {
                std::path::PathBuf::from(home)
                    .join(".ssh/id_ed25519")
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        Self {
            user: session.user.clone().unwrap_or_default(),
            session,
            path: None,
            sftp_tab: None,
            mode: AuthMode::Auto,
            password: String::new(),
            key_path,
            passphrase: String::new(),
            save_to_keychain: true,
            error: None,
        }
    }

    fn credentials(&self) -> std::result::Result<Credentials, String> {
        match self.mode {
            AuthMode::Auto => Ok(Credentials::Auto),
            AuthMode::Key => {
                if self.key_path.trim().is_empty() {
                    return Err("informe o caminho da chave".into());
                }
                let passphrase = self.passphrase.trim();
                Ok(Credentials::Key {
                    path: PathBuf::from(self.key_path.trim()),
                    passphrase: (!passphrase.is_empty()).then(|| passphrase.to_string()),
                })
            }
            AuthMode::Password => {
                if self.password.is_empty() {
                    return Err("informe a senha".into());
                }
                Ok(Credentials::Password(self.password.clone()))
            }
        }
    }
}

/// Para onde o diálogo de autenticação deve mandar as credenciais.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthTarget {
    Terminal,
    Sftp(u64),
}

/// Roteia a autenticação: uma aba SFTP pediu, ou é o fluxo normal do terminal.
fn auth_target(auth: &SshAuth) -> AuthTarget {
    match auth.sftp_tab {
        Some(tab_id) => AuthTarget::Sftp(tab_id),
        None => AuthTarget::Terminal,
    }
}

struct App {
    store: Option<Store>,
    entries: Vec<SessionEntry>,
    /// Caminhos selecionados na árvore; o último é o principal (conectar/quick connect).
    selected: Vec<PathBuf>,
    filter: String,
    import_open: bool,
    import_path: String,
    form: Option<FormState>,
    auth: Option<SshAuth>,
    pw_import_open: bool,
    pw_list: String,
    pw_message: Option<String>,
    /// Sessões aguardando confirmação de exclusão (uma ou várias).
    confirm_delete: Option<Vec<(PathBuf, String)>>,
    message: Option<String>,
    error: Option<String>,
    tabs: Vec<Tab>,
    active: Option<u64>,
    next_tab_id: u64,
    pending: Vec<PendingConnect>,
    pending_sftp: Vec<PendingSftp>,
    /// (aba, chave do Keychain, senha) a gravar quando o connect SFTP der certo.
    sftp_saves: Vec<(u64, String, String)>,
    /// Estado persistido do explorador SFTP (ocultos, divisória e caminhos).
    sftp_state: SftpState,
    focus_terminal: bool,
    last_log_dir: Option<PathBuf>,
    reload_requested: bool,
    telegram: TelegramUi,
    updates: UpdatesUi,
    theme: ThemeChoice,
    collapsed_groups: BTreeSet<String>,
    window_size: Option<(u32, u32)>,
    focus_filter: bool,
    smoke_frames: Option<u32>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // Cópias locais de edição remota (Task 13) mais velhas que 24 h não
        // devem acumular no temp do SO.
        sftp::cleanup_edit_temp(&sftp::edit_temp_root(), sftp::EDIT_MAX_AGE);
        let theme = ThemeChoice::load();
        apply_theme(&cc.egui_ctx, theme);
        let mut app = Self {
            store: None,
            entries: Vec::new(),
            selected: Vec::new(),
            filter: String::new(),
            import_open: false,
            import_path: String::new(),
            form: None,
            auth: None,
            pw_import_open: false,
            pw_list: String::new(),
            pw_message: None,
            confirm_delete: None,
            message: None,
            error: None,
            tabs: Vec::new(),
            active: None,
            next_tab_id: 1,
            pending: Vec::new(),
            pending_sftp: Vec::new(),
            sftp_saves: Vec::new(),
            sftp_state: load_sftp_state(&sftp::sftp_state_path()),
            focus_terminal: false,
            last_log_dir: None,
            reload_requested: false,
            telegram: TelegramUi::new(),
            updates: UpdatesUi::new(),
            theme,
            collapsed_groups: if std::env::var_os("SSHC_GUI_SMOKE").is_some() {
                BTreeSet::new()
            } else {
                load_collapsed_groups()
            },
            window_size: load_window_size(),
            focus_filter: false,
            smoke_frames: std::env::var("SSHC_GUI_SMOKE").ok().map(|_| 120),
        };
        app.reload();
        if app.smoke_frames.is_some() {
            app.selected = app
                .entries
                .first()
                .map(|e| vec![e.path.clone()])
                .unwrap_or_default();
            if std::env::var_os("SSHC_GUI_SMOKE_FORM").is_some() {
                app.form = Some(FormState::new());
            } else if let Ok(kind) = std::env::var("SSHC_GUI_SMOKE_AUTH") {
                if let Some((session, _, _)) = app.selected_session() {
                    let mut auth = SshAuth::new(session);
                    if kind == "key" {
                        auth.mode = AuthMode::Key;
                    }
                    app.auth = Some(auth);
                }
            } else if std::env::var_os("SSHC_GUI_SMOKE_TELEGRAM").is_some() {
                // captura da janela do Telegram, sem abrir conexão
                app.telegram.open = true;
            } else if std::env::var_os("SSHC_GUI_SMOKE_UPDATE").is_some() {
                app.updates.open = true;
            } else if std::env::var_os("SSHC_GUI_SMOKE_OFFLINE").is_some() {
                // captura só da UI (Session Manager/tema), sem tocar na rede
            } else {
                // quantas abas abrir na captura (default: 1)
                let count = std::env::var("SSHC_GUI_SMOKE_TABS")
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(1);
                let targets: Vec<(Session, PathBuf)> = app
                    .entries
                    .iter()
                    .take(count)
                    .map(|entry| (entry.session.clone(), entry.path.clone()))
                    .collect();
                for (session, path) in targets {
                    app.request_connect(session, Some(path));
                }
                if std::env::var_os("SSHC_GUI_SMOKE_LOG").is_some() {
                    if let Some(tab) = app.tabs.last_mut() {
                        if let Ok(log) = start_log(&tab.session) {
                            if let TabBody::Terminal(terminal) = &mut tab.body {
                                terminal.log = Some(log);
                            }
                        }
                    }
                }
                // seleção/busca de exemplo, só para a captura
                if let Some(tab) = app.tabs.last_mut() {
                    if let TabBody::Terminal(terminal) = &mut tab.body {
                        match std::env::var("SSHC_GUI_SMOKE_SEARCH") {
                            Ok(query) => {
                                terminal.search = Some(Search {
                                    query,
                                    ..Default::default()
                                })
                            }
                            Err(_) => {
                                let screen = terminal.term.screen();
                                let top = screen.total_rows().saturating_sub(screen.rows());
                                let cols = screen.cols();
                                terminal.sel = Some(Selection {
                                    anchor: (top, 0),
                                    cursor: (top, 28.min(cols.saturating_sub(1))),
                                });
                            }
                        }
                    }
                }
            }
        }
        app
    }

    fn reload(&mut self) {
        match Store::open(cloud::store_root()).and_then(|store| {
            let entries = store.load_all()?;
            Ok((store, entries))
        }) {
            Ok((store, entries)) => {
                self.store = Some(store);
                self.entries = entries;
                // descarta seleção de sessões que sumiram (sync, outro processo…)
                self.selected
                    .retain(|path| self.entries.iter().any(|e| &e.path == path));
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn selected_session(&self) -> Option<(Session, String, PathBuf)> {
        let selected = self.selected.last()?;
        self.entries
            .iter()
            .find(|e| &e.path == selected)
            .map(|e| (e.session.clone(), e.group.clone(), e.path.clone()))
    }

    fn selected_entry(&self) -> Option<&SessionEntry> {
        let selected = self.selected.last()?;
        self.entries.iter().find(|e| &e.path == selected)
    }

    /// Formulário de nova sessão já com a pasta da sessão selecionada.
    fn new_session_form(&self) -> FormState {
        let mut form = FormState::new();
        if let Some((_, group, _)) = self.selected_session() {
            form.group = group;
        }
        form
    }

    /// Acha a aba de uma sessão (por caminho, ou nome+endpoint para quick connect).
    fn tab_index(&self, path: &Option<PathBuf>, session: &Session) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            matches!(&tab.body, TabBody::Terminal(_))
                && match path {
                    Some(path) => tab.path.as_deref() == Some(path.as_path()),
                    None => {
                        tab.path.is_none()
                            && tab.session.name == session.name
                            && tab.session.endpoint() == session.endpoint()
                    }
                }
        })
    }

    /// Acha a aba SFTP de uma sessão (mesma regra de tab_index).
    fn sftp_tab_index(&self, path: &Option<PathBuf>, session: &Session) -> Option<usize> {
        self.tabs.iter().position(|tab| {
            matches!(&tab.body, TabBody::Sftp(_))
                && match path {
                    Some(path) => tab.path.as_deref() == Some(path.as_path()),
                    None => {
                        tab.path.is_none()
                            && tab.session.name == session.name
                            && tab.session.endpoint() == session.endpoint()
                    }
                }
        })
    }

    /// Fechar uma aba SFTP com transferência ativa pede confirmação.
    fn close_sftp_tab(&mut self, id: u64) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
            if let TabBody::Sftp(sftp) = &mut tab.body {
                // Descartar a conexão cancela os jobs do worker.
                sftp.disconnect();
            }
        }
        self.close_tab(id);
    }

    fn request_close_tab(&mut self, id: u64) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
            return;
        };
        let needs_confirm = match &mut self.tabs[index].body {
            TabBody::Sftp(sftp) => sftp.request_close(id),
            TabBody::Terminal(_) => false,
        };
        if needs_confirm {
            // O modal só é desenhado na aba ativa.
            self.active = Some(id);
            return;
        }
        self.close_tab(id);
    }

    fn close_tab(&mut self, id: u64) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        self.tabs.remove(index);
        // Fechar durante o connect descarta o que chegar depois.
        self.pending_sftp.retain(|pending| pending.tab_id != id);
        self.sftp_saves.retain(|(tab_id, _, _)| *tab_id != id);
        if self.tabs.is_empty() {
            self.active = None;
        } else if self.active == Some(id) {
            let next = index.min(self.tabs.len() - 1);
            self.active = Some(self.tabs[next].id);
        }
    }

    fn connect_selected(&mut self) {
        match self.selected_session() {
            Some((session, _group, path)) => self.request_connect(session, Some(path)),
            None => self.error = Some("selecione uma sessão".into()),
        }
    }

    /// SSH tenta agent + ~/.ssh sozinho (como o `ssh`); senha só se falhar.
    fn request_connect(&mut self, session: Session, path: Option<PathBuf>) {
        // Aba já existe: ativa e, se estiver caída, reconecta nela.
        if let Some(index) = self.tab_index(&path, &session) {
            self.active = Some(self.tabs[index].id);
            self.focus_terminal = true;
            if let TabBody::Terminal(terminal) = &self.tabs[index].body {
                if terminal.conn.is_some() {
                    return;
                }
            }
        }

        // ENTER repetido (ou duplo clique) enquanto conecta não abre outra conexão.
        if self
            .pending
            .iter()
            .any(|p| p.path == path && p.session.endpoint() == session.endpoint())
        {
            return;
        }

        if !matches!(session.protocol, Protocol::Ssh) {
            self.start_connect(session, path, None, None, None);
            return;
        }

        // senha guardada no Keychain vale mais que agent/chave
        if let Ok(Some(password)) = keychain::load(&session_key(&session)) {
            self.start_connect(
                session,
                path,
                Some(Credentials::Password(password)),
                None,
                None,
            );
            return;
        }

        if let Some(password) = std::env::var("SSHC_SSH_PASSWORD")
            .ok()
            .filter(|p| !p.is_empty())
        {
            self.start_connect(
                session,
                path,
                Some(Credentials::Password(password)),
                None,
                None,
            );
            return;
        }

        self.start_connect(session, path, Some(Credentials::Auto), None, None);
    }

    /// Abre (ou ativa) a aba SFTP de uma sessão SSH.
    fn open_sftp(&mut self, session: Session, path: Option<PathBuf>, creds: Option<Credentials>) {
        if let Some(index) = self.sftp_tab_index(&path, &session) {
            self.active = Some(self.tabs[index].id);
            self.focus_terminal = false;
            return;
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let creds = creds.unwrap_or(Credentials::Auto);
        self.tabs.push(Tab {
            id,
            session,
            path,
            body: TabBody::Sftp(SftpTab::connecting(Some(creds.clone()))),
        });
        self.active = Some(id);
        self.focus_terminal = false;
        self.start_sftp_connect(id, creds, None);
    }

    /// Abre a conexão numa thread; o resultado chega no `pump`.
    fn start_connect(
        &mut self,
        session: Session,
        path: Option<PathBuf>,
        creds: Option<Credentials>,
        auth: Option<SshAuth>,
        save_password: Option<(String, String)>,
    ) {
        let pending_creds = creds.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let target = session.clone();
        std::thread::spawn(move || {
            let result = sshc_net::connect(&target, creds).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.message = Some(format!("conectando em {}…", session.endpoint()));
        self.error = None;
        self.pending.push(PendingConnect {
            session,
            path,
            rx,
            auth,
            creds: pending_creds,
            save_password,
        });
    }

    /// Coloca uma conexão já aberta na aba existente (reconexão) ou numa nova.
    fn finish_connect(
        &mut self,
        session: &Session,
        path: Option<PathBuf>,
        creds: Option<Credentials>,
        conn: Connection,
    ) {
        let note = conn.note.clone();
        if let Some(index) = self.tab_index(&path, session) {
            if let TabBody::Terminal(terminal) = &mut self.tabs[index].body {
                terminal.conn = Some(conn);
                terminal.creds = creds;
                self.active = Some(self.tabs[index].id);
            }
        } else {
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            self.tabs.push(Tab {
                id,
                session: session.clone(),
                path,
                body: TabBody::Terminal(TerminalTab {
                    term: Terminal::new(100, 30),
                    conn: Some(conn),
                    creds,
                    sel: None,
                    search: None,
                    log: None,
                    notice: None,
                }),
            });
            self.active = Some(id);
        }
        self.focus_terminal = true;
        self.error = None;
        self.message = Some(if note.is_empty() {
            format!("conectado em {}", session.endpoint())
        } else {
            format!("conectado em {} — {note}", session.endpoint())
        });
    }

    /// Sobe o connect SFTP da aba numa thread; o resultado chega no `pump`.
    fn start_sftp_connect(
        &mut self,
        tab_id: u64,
        creds: Credentials,
        save_password: Option<(String, String)>,
    ) {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) else {
            return;
        };
        let TabBody::Sftp(sftp) = &mut self.tabs[index].body else {
            return;
        };
        sftp.creds = Some(creds.clone());
        sftp.state = ConnState::Connecting;
        sftp.conn = None;
        sftp.note = None;
        sftp.home = None;
        let session = self.tabs[index].session.clone();
        // uma tentativa anterior da mesma aba não pode sobrescrever esta
        self.pending_sftp.retain(|pending| pending.tab_id != tab_id);
        self.sftp_saves.retain(|(id, _, _)| *id != tab_id);
        if let Some((key, password)) = save_password {
            self.sftp_saves.push((tab_id, key, password));
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result =
                sshc_net::sftp::connect(&session, creds).map_err(|error| format!("{error:#}"));
            let _ = tx.send(result);
        });
        self.pending_sftp.push(PendingSftp { tab_id, rx });
        self.focus_terminal = false;
    }

    /// Reconecta a aba SFTP com as credenciais guardadas (Auto se não houver).
    fn reconnect_sftp(&mut self, tab_id: u64) {
        let creds = match self
            .tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| &tab.body)
        {
            Some(TabBody::Sftp(sftp)) => sftp.creds.clone().unwrap_or(Credentials::Auto),
            _ => return,
        };
        self.start_sftp_connect(tab_id, creds, None);
    }

    /// Aplica o connect SFTP que terminou; ignora se a aba foi fechada.
    fn finish_sftp_connect(
        &mut self,
        tab_id: u64,
        result: std::result::Result<SftpConnection, String>,
    ) {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id && matches!(&tab.body, TabBody::Sftp(_)))
        else {
            self.sftp_saves.retain(|(id, _, _)| *id != tab_id);
            return;
        };
        let endpoint = self.tabs[index].session.endpoint();
        match result {
            Ok(conn) => {
                if let Some((_, key, password)) =
                    self.sftp_saves.iter().find(|(id, _, _)| *id == tab_id)
                {
                    if let Err(error) = keychain::save(key, password) {
                        self.error = Some(format!("não gravei a senha: {error:#}"));
                    }
                }
                let note = conn.note.clone();
                let home = conn.home.clone();
                let session = self.tabs[index].session.clone();
                let TabBody::Sftp(sftp) = &mut self.tabs[index].body else {
                    return;
                };
                sftp.note = (!note.trim().is_empty()).then_some(note);
                sftp.home = home;
                sftp.conn = Some(conn);
                sftp.state = ConnState::Connected;
                sftp.on_connected();
                apply_saved_paths(sftp, &self.sftp_state, &session);
                self.active = Some(tab_id);
                self.error = None;
                self.message = Some(format!("SFTP conectado em {endpoint}"));
            }
            Err(message) => {
                let auth_error = sshc_net::is_auth_error(&message);
                let message = if auth_error {
                    message
                } else {
                    friendly_sftp_error(&message)
                };
                {
                    let TabBody::Sftp(sftp) = &mut self.tabs[index].body else {
                        return;
                    };
                    sftp.conn = None;
                    sftp.state = ConnState::Failed(message.clone());
                }
                if auth_error {
                    // como no terminal, abre o diálogo de senha já com o alvo.
                    let mut auth = SshAuth::new(self.tabs[index].session.clone());
                    auth.path = self.tabs[index].path.clone();
                    auth.sftp_tab = Some(tab_id);
                    if let Some(key) = sshc_net::encrypted_key_path(&message) {
                        auth.mode = AuthMode::Key;
                        auth.key_path = key.to_string();
                    }
                    auth.error = Some(message);
                    self.auth = Some(auth);
                }
            }
        }
        self.sftp_saves.retain(|(id, _, _)| *id != tab_id);
    }

    /// Persiste caminhos e layout quando alguma aba SFTP sinaliza mudança.
    /// Erros de escrita viram status da aba, nunca derrubam o app.
    fn save_sftp_state_if_needed(&mut self) {
        // O smoke não pode sobrescrever o estado real do usuário.
        if self.smoke_frames.is_some() {
            return;
        }
        let mut last_changed = None;
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let TabBody::Sftp(sftp) = &mut tab.body else {
                continue;
            };
            let changes = sftp.take_state_changes();
            if !changes.any() {
                continue;
            }
            if changes.paths {
                // Só grava o remoto confirmado por um Listed Ok: o "/"
                // provisório do connect ou um caminho que falhou não entram.
                self.sftp_state
                    .paths
                    .insert(session_path_key(&tab.session), sftp.saved_path());
            }
            if changes.layout {
                self.sftp_state.hidden = sftp.hidden;
                self.sftp_state.split = sftp.split;
            }
            last_changed = Some(index);
        }
        let Some(index) = last_changed else {
            return;
        };
        if let Err(error) = save_sftp_state(&sftp::sftp_state_path(), &self.sftp_state) {
            if let TabBody::Sftp(sftp) = &mut self.tabs[index].body {
                sftp.status = Some(format!("não gravei o estado do SFTP: {error:#}"));
            }
        }
    }

    /// Guarda o tamanho da janela quando ela para de mudar (sem gravar a cada
    /// frame durante o arraste). O smoke não sobrescreve o tamanho real.
    fn remember_window_size(&mut self, ctx: &egui::Context) {
        if self.smoke_frames.is_some() {
            return;
        }
        let size = ctx.input(|input| input.viewport_rect().size());
        let width = size.x.round() as u32;
        let height = size.y.round() as u32;
        if width < 320 || height < 240 {
            return;
        }
        if ctx.input(|input| input.pointer.any_down()) {
            return;
        }
        if self.window_size == Some((width, height)) {
            return;
        }
        self.window_size = Some((width, height));
        save_window_size(width, height);
    }

    /// Drena todos os sockets e atualiza as telas.
    fn pump(&mut self, ctx: &egui::Context) {
        // conexões abertas em thread
        let outcome = self
            .pending
            .iter()
            .enumerate()
            .find_map(|(index, pending)| match pending.rx.try_recv() {
                Ok(result) => Some((index, result)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some((index, Err("conexão abortada".to_string())))
                }
            });
        if let Some((index, result)) = outcome {
            let pending = self.pending.remove(index);
            match result {
                Ok(conn) => {
                    if let Some((key, password)) = &pending.save_password {
                        if let Err(e) = keychain::save(key, password) {
                            self.error = Some(format!("não gravei a senha: {e:#}"));
                        }
                    }
                    self.finish_connect(&pending.session, pending.path, pending.creds, conn)
                }
                Err(message) if sshc_net::is_auth_error(&message) => {
                    let mut auth = pending
                        .auth
                        .unwrap_or_else(|| SshAuth::new(pending.session.clone()));
                    auth.path = pending.path.clone();
                    if let Some(key) = sshc_net::encrypted_key_path(&message) {
                        auth.mode = AuthMode::Key;
                        auth.key_path = key.to_string();
                    }
                    auth.error = Some(message);
                    self.auth = Some(auth);
                }
                Err(message) => self.error = Some(message),
            }
        }
        if !self.pending.is_empty() || !self.pending_sftp.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        // connects SFTP abertos em thread
        let mut sftp_outcomes = Vec::new();
        self.pending_sftp
            .retain(|pending| match pending.rx.try_recv() {
                Ok(result) => {
                    sftp_outcomes.push((pending.tab_id, result));
                    false
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    sftp_outcomes.push((pending.tab_id, Err("conexão abortada".to_string())));
                    false
                }
            });
        for (tab_id, result) in sftp_outcomes {
            self.finish_sftp_connect(tab_id, result);
        }

        let mut any_live = false;
        let mut jobs_active = false;
        for tab in &mut self.tabs {
            match &mut tab.body {
                TabBody::Terminal(terminal) => {
                    let mut log_error = None;
                    if let Some(conn) = &terminal.conn {
                        while let Ok(chunk) = conn.output.try_recv() {
                            terminal.term.feed(&chunk);
                            if let Some(log) = &mut terminal.log {
                                if let Err(e) = log.write_chunk(&chunk) {
                                    log_error = Some(format!("{e}"));
                                }
                            }
                            ctx.request_repaint();
                        }
                    }
                    if let Some(error) = log_error {
                        terminal.log = None;
                        terminal.notice = Some(format!("log interrompido: {error}"));
                    }
                    if terminal.conn.as_ref().is_some_and(|conn| conn.is_closed()) {
                        terminal.conn = None;
                        self.message = Some(format!("conexão encerrada: {}", tab.session.name));
                    }
                    if terminal.conn.is_some() {
                        any_live = true;
                    }
                }
                TabBody::Sftp(sftp) => {
                    let mut events = Vec::new();
                    if let Some(conn) = &sftp.conn {
                        while let Ok(event) = conn.events.try_recv() {
                            events.push(event);
                        }
                    }
                    let implicitly_closed = sftp.conn.as_ref().is_some_and(|conn| conn.is_closed());
                    let mut closed = false;
                    for event in events {
                        // Closed é processado no meio da drenagem: os Failed
                        // que o worker manda depois não podem reescrever os
                        // jobs já marcados Interrupted.
                        if sftp.handle_event(event) {
                            closed = true;
                        }
                    }
                    if !closed && implicitly_closed {
                        sftp.on_closed("conexão encerrada".to_string());
                        closed = true;
                    }
                    if closed {
                        self.message = Some(format!("SFTP encerrado: {}", tab.session.name));
                    }
                    if !sftp.edits.is_empty() {
                        // Metadados das cópias em edição: no máximo 1x/s.
                        if sftp.poll_edits(std::time::Instant::now()) {
                            ctx.request_repaint();
                        }
                        ctx.request_repaint_after(std::time::Duration::from_secs(1));
                    }
                    if sftp.has_active_jobs() {
                        // Progresso e fila andam mesmo com a aba em segundo plano.
                        ctx.request_repaint();
                        jobs_active = true;
                    }
                    if sftp.conn.is_some() {
                        any_live = true;
                    }
                }
            }
        }
        if any_live || jobs_active {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }
    }

    fn run_import(&mut self) {
        let from = PathBuf::from(self.import_path.trim());
        let Some(store) = &self.store else {
            self.error = Some("store não abriu".into());
            return;
        };
        match import_into(store, &from) {
            Ok(count) => {
                self.message = Some(format!("{count} sessões importadas de {}", from.display()));
                self.reload();
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn save_form(&mut self, form: &mut FormState) -> bool {
        let session = match form.build() {
            Ok(session) => session,
            Err(msg) => {
                form.error = Some(msg);
                return false;
            }
        };
        let Some(store) = &self.store else {
            form.error = Some("store não abriu".into());
            return false;
        };
        match store.save(form.group.trim(), &session) {
            Ok(path) => {
                if let Some(old) = &form.editing {
                    if old != &path {
                        let _ = store.delete(old);
                    }
                }
                self.selected = vec![path];
                self.message = Some(format!("sessão '{}' salva", session.name));
                true
            }
            Err(e) => {
                form.error = Some(format!("{e:#}"));
                false
            }
        }
    }
}

fn import_into(store: &Store, from: &Path) -> Result<usize> {
    let report = securecrt::import_dir(from)?;
    for (path, session) in &report.entries {
        let group = path
            .parent()
            .and_then(|dir| dir.strip_prefix(from).ok())
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        store.save(&group, session)?;
    }
    Ok(report.entries.len())
}

/// Agrupa por pasta preservando a ordem e aplicando o filtro de busca.
fn grouped<'a>(entries: &'a [SessionEntry], filter: &str) -> Vec<(&'a str, Vec<&'a SessionEntry>)> {
    let needle = filter.trim().to_lowercase();
    let matches = |e: &SessionEntry| {
        needle.is_empty()
            || e.session.name.to_lowercase().contains(&needle)
            || e.session.host.to_lowercase().contains(&needle)
            || e.group.to_lowercase().contains(&needle)
    };

    let mut groups: Vec<(&str, Vec<&SessionEntry>)> = Vec::new();
    for entry in entries.iter().filter(|e| matches(e)) {
        match groups.last_mut() {
            Some((name, list)) if *name == entry.group.as_str() => list.push(entry),
            _ => groups.push((entry.group.as_str(), vec![entry])),
        }
    }
    groups.sort_by(|a, b| a.0.cmp(b.0));
    groups
}

/// Pastas (grupos) existentes, ordenadas, para o select do formulário.
fn group_names(entries: &[SessionEntry]) -> Vec<String> {
    let mut groups: Vec<String> = entries
        .iter()
        .map(|entry| entry.group.clone())
        .filter(|group| !group.is_empty())
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

/// Nome digitado em "Nova pasta…": relativo, sem .., ocultos ou separadores
/// esquisitos. Subpasta é separada por / (as pastas são criadas ao salvar).
fn validate_new_group(group: &str) -> Result<(), String> {
    let group = group.trim();
    if group.is_empty() {
        return Err("informe o nome da nova pasta".into());
    }
    if group.starts_with('/') || group.ends_with('/') {
        return Err("nome de pasta inválido".into());
    }
    for part in group.split('/') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if part == "." || part == ".." || part.starts_with('.') {
            return Err("nome de pasta inválido".into());
        }
        if part
            .chars()
            .any(|character| matches!(character, '\\' | ':' | '\0'))
        {
            return Err("nome de pasta inválido".into());
        }
    }
    Ok(())
}

/// Sessões (caminho + nome) na ordem da árvore para os caminhos selecionados.
fn selected_entries(entries: &[SessionEntry], selected: &[PathBuf]) -> Vec<(PathBuf, String)> {
    entries
        .iter()
        .filter(|e| selected.contains(&e.path))
        .map(|e| (e.path.clone(), e.session.name.clone()))
        .collect()
}

/// Clique simples substitui a seleção; ⌘/Ctrl alterna; Shift seleciona o intervalo.
fn update_selection(
    selected: &mut Vec<PathBuf>,
    flat: &[PathBuf],
    path: &Path,
    command: bool,
    shift: bool,
) {
    if command {
        match selected.iter().position(|p| p == path) {
            Some(pos) => {
                selected.remove(pos);
            }
            None => selected.push(path.to_path_buf()),
        }
        return;
    }
    if shift {
        let anchor = selected
            .last()
            .and_then(|anchor| flat.iter().position(|p| p.as_path() == anchor));
        let end = flat.iter().position(|p| p.as_path() == path);
        if let (Some(anchor), Some(end)) = (anchor, end) {
            let (lo, hi) = if anchor <= end {
                (anchor, end)
            } else {
                (end, anchor)
            };
            *selected = flat[lo..=hi].to_vec();
            return;
        }
    }
    *selected = vec![path.to_path_buf()];
}

/// Select de pasta no formulário, com a opção de digitar uma nova.
fn group_choice(ui: &mut egui::Ui, form: &mut FormState, groups: &[String]) {
    #[derive(PartialEq, Eq)]
    enum Choice {
        Root,
        Existing(String),
        New,
    }

    let mut choice = if form.new_group {
        Choice::New
    } else if form.group.trim().is_empty() {
        Choice::Root
    } else {
        Choice::Existing(form.group.trim().to_string())
    };
    let selected = match &choice {
        Choice::Root => "(raiz)".to_string(),
        Choice::Existing(group) => group.clone(),
        Choice::New => "Nova pasta…".to_string(),
    };
    egui::ComboBox::from_id_salt("form-group")
        .selected_text(selected)
        .width(220.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut choice, Choice::Root, "(raiz)");
            for group in groups {
                ui.selectable_value(&mut choice, Choice::Existing(group.clone()), group);
            }
            ui.selectable_value(&mut choice, Choice::New, "Nova pasta…");
        });
    match choice {
        Choice::Root => {
            form.group.clear();
            form.new_group = false;
        }
        Choice::Existing(group) => {
            form.group = group;
            form.new_group = false;
        }
        Choice::New => {
            if !form.new_group {
                form.group.clear();
            }
            form.new_group = true;
        }
    }
    if form.new_group {
        ui.add(
            egui::TextEdit::singleline(&mut form.group)
                .hint_text("nome da pasta (use / para subpasta)")
                .desired_width(260.0),
        );
    }
}

/// Ícone do accent + texto na cor normal (WidgetText com duas cores).
fn item_text(ui: &egui::Ui, icon: &str, text: &str) -> egui::WidgetText {
    let mut job = egui::text::LayoutJob::default();
    let font = egui::TextStyle::Body.resolve(ui.style());
    job.append(
        icon,
        0.0,
        egui::TextFormat {
            font_id: font.clone(),
            color: ACCENT,
            ..Default::default()
        },
    );
    job.append(
        text,
        0.0,
        egui::TextFormat {
            font_id: font,
            color: ui.visuals().text_color(),
            ..Default::default()
        },
    );
    job.into()
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.remember_window_size(ui.ctx());
        self.pump(ui.ctx());

        let app_modal_open = self.form.is_some()
            || self.auth.is_some()
            || self.import_open
            || self.pw_import_open
            || self.confirm_delete.is_some()
            || self.telegram.open
            || self.updates.open;
        // Conflito, confirmações e diálogos da aba SFTP também bloqueiam
        // atalhos globais.
        let sftp_modal_open = self
            .active
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .is_some_and(|tab| match &tab.body {
                TabBody::Sftp(sftp) => sftp.has_modal(),
                TabBody::Terminal(_) => false,
            });
        let modal_open = app_modal_open || sftp_modal_open;
        // Cmd+F/Cmd+I não podem roubar o foco de uma pane SFTP nem agir por
        // baixo de um diálogo.
        let sftp_pane_focused = self
            .active
            .and_then(|id| self.tabs.iter().find(|tab| tab.id == id))
            .is_some_and(|tab| matches!(&tab.body, TabBody::Sftp(_)));

        // Cmd+F / Ctrl+F: busca no terminal da aba ativa
        if !modal_open
            && !sftp_pane_focused
            && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F))
        {
            if let Some(index) = self
                .active
                .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
            {
                if let TabBody::Terminal(terminal) = &mut self.tabs[index].body {
                    if terminal.search.is_none() {
                        terminal.search = Some(Search::default());
                    }
                }
            }
        }

        // ⌘/Ctrl+I: foca o filtro do Session Manager (como no SecureCRT).
        if !modal_open
            && !sftp_pane_focused
            && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::I))
        {
            self.focus_filter = true;
        }

        if self.reload_requested {
            self.reload_requested = false;
            self.reload();
        }

        if let Some(left) = self.smoke_frames.as_mut() {
            *left = left.saturating_sub(1);
            ui.ctx().request_repaint();
            if *left == 60 {
                ui.ctx()
                    .send_viewport_cmd(
                        egui::ViewportCommand::Screenshot(egui::UserData::default()),
                    );
            }
            if let Some(image) = ui.input(|i| {
                i.events.iter().find_map(|e| match e {
                    egui::Event::Screenshot { image, .. } => Some(image.clone()),
                    _ => None,
                })
            }) {
                let _ = save_ppm("/tmp/sshc-gui.ppm", &image);
                *left = 0;
            }
            if *left == 0 {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        egui::Panel::top("top").show(ui, |ui| {
            egui::Frame::NONE
                .inner_margin(egui::Margin::symmetric(12, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading(egui::RichText::new("sshc").color(ACCENT).strong());
                        ui.label(
                            egui::RichText::new(format!("·  {} sessões", self.entries.len()))
                                .weak()
                                .small(),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(12.0);
                            if ui.button("Importar SecureCRT…").clicked() {
                                self.import_open = true;
                            }
                            if ui.button("Importar senhas…").clicked() {
                                self.pw_import_open = true;
                            }
                            if ui.button("Atualizar").clicked() {
                                self.reload_requested = true;
                            }
                            if ui
                                .button(egui::RichText::new("+ Nova sessão").strong())
                                .clicked()
                            {
                                let form = self.new_session_form();
                                self.form = Some(form);
                            }
                            if ui.button("Quick connect").clicked() {
                                self.form = Some(FormState::quick(self.selected_session()));
                            }
                            if ui.button("Telegram…").clicked() {
                                self.telegram.open = true;
                            }
                        });
                    });
                });
        });

        // Painel de abas: só aparece quando existe alguma.
        if !self.tabs.is_empty() {
            let mut activate = None;
            let mut close = None;
            egui::Panel::top("tabbar").show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(8, 4))
                    .show(ui, |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("tabbar-scroll")
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 4.0;
                                    for tab in &self.tabs {
                                        let is_active = self.active == Some(tab.id);
                                        let (title, terminal) = match &tab.body {
                                            TabBody::Terminal(terminal) => {
                                                let title = if terminal.conn.is_some() {
                                                    tab.session.name.clone()
                                                } else {
                                                    format!("{} ·off", tab.session.name)
                                                };
                                                (title, true)
                                            }
                                            TabBody::Sftp(sftp) => (
                                                sftp_tab_title(
                                                    &tab.session.name,
                                                    sftp.active_jobs(),
                                                ),
                                                false,
                                            ),
                                        };
                                        if ui
                                            .selectable_label(is_active, egui::RichText::new(title))
                                            .clicked()
                                        {
                                            activate = Some((tab.id, terminal));
                                        }
                                        if ui.small_button("×").clicked() {
                                            close = Some(tab.id);
                                        }
                                    }
                                });
                            });
                    });
            });
            if let Some((id, terminal)) = activate {
                self.active = Some(id);
                self.focus_terminal = terminal;
            }
            if let Some(id) = close {
                self.request_close_tab(id);
            }
        }

        // Duplo clique conecta depois do painel (evita conflito de empréstimo).
        let mut pending_connect: Option<(Session, PathBuf)> = None;
        let mut connect_clicked = false;
        let mut new_session_clicked = false;
        let mut quick_clicked = false;
        let mut edit_clicked = false;
        let mut duplicate_clicked = false;
        let mut delete_clicked = false;
        egui::Panel::left("sessions")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new("Session Manager").strong());
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "{} sessões",
                                            self.entries.len()
                                        ))
                                        .weak()
                                        .small(),
                                    );
                                },
                            );
                        });
                        ui.add_space(6.0);

                        let has_selection = !self.selected.is_empty();
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if ui.small_button("+").on_hover_text("Nova sessão").clicked() {
                                new_session_clicked = true;
                            }
                            if ui
                                .small_button("⚡")
                                .on_hover_text("Quick connect (sem salvar)")
                                .clicked()
                            {
                                quick_clicked = true;
                            }
                            if ui
                                .add_enabled(has_selection, egui::Button::new("▶").small())
                                .on_hover_text("Conectar sessão selecionada")
                                .clicked()
                            {
                                connect_clicked = true;
                            }
                            if ui
                                .add_enabled(has_selection, egui::Button::new("✏").small())
                                .on_hover_text("Editar sessão")
                                .clicked()
                            {
                                edit_clicked = true;
                            }
                            if ui
                                .add_enabled(has_selection, egui::Button::new("🗐").small())
                                .on_hover_text("Duplicar sessão")
                                .clicked()
                            {
                                duplicate_clicked = true;
                            }
                            if ui
                                .add_enabled(
                                    has_selection,
                                    egui::Button::new(
                                        egui::RichText::new("×").color(ERROR),
                                    )
                                    .small(),
                                )
                                .on_hover_text("Excluir selecionada(s)")
                                .clicked()
                            {
                                delete_clicked = true;
                            }
                            if ui
                                .small_button("↻")
                                .on_hover_text("Atualizar sessões")
                                .clicked()
                            {
                                self.reload_requested = true;
                            }
                        });
                        ui.add_space(6.0);

                        let shortcut = if cfg!(target_os = "macos") {
                            "⌘I"
                        } else {
                            "Ctrl+I"
                        };
                        ui.horizontal(|ui| {
                            let width = (ui.available_width() - 36.0).max(90.0);
                            let filter = ui.add(
                                egui::TextEdit::singleline(&mut self.filter)
                                    .hint_text("filtrar por nome, host ou pasta")
                                    .desired_width(width),
                            );
                            ui.label(egui::RichText::new(shortcut).weak().small())
                                .on_hover_text("foca o filtro");
                            if self.focus_filter {
                                filter.request_focus();
                                self.focus_filter = false;
                            }
                        });
                    });

                egui::Panel::bottom("sessions-footer")
                    .frame(egui::Frame::NONE.inner_margin(egui::Margin::symmetric(10, 4)))
                    .show(ui, |ui| {
                        let status = match self.selected.len() {
                            0 => "Pronto".to_string(),
                            1 => "Pronto · 1 selecionada".to_string(),
                            n => format!("Pronto · {n} selecionadas"),
                        };
                        ui.label(egui::RichText::new(status).weak().small());
                    });

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                        ui.add_space(2.0);
                        let groups = grouped(&self.entries, &self.filter);
                        let flat: Vec<PathBuf> = groups
                            .iter()
                            .flat_map(|(_, list)| list.iter().map(|e| e.path.clone()))
                            .collect();
                        if groups.is_empty() {
                            ui.add_space(12.0);
                            ui.label(egui::RichText::new("nenhuma sessão").weak());
                            ui.label(
                                egui::RichText::new(
                                    "crie com “+ Nova sessão”, importe do SecureCRT… ou use Quick connect",
                                )
                                .weak()
                                .small(),
                            );
                        }
                        for (group, list) in groups {
                            let title = if group.is_empty() {
                                "(raiz)".to_string()
                            } else {
                                group.to_string()
                            };
                            let key = group_state_key(group);
                            let collapsed = self.collapsed_groups.contains(&key);
                            let header = item_text(
                                ui,
                                "📁  ",
                                &format!("{title}  ·  {}", list.len()),
                            );
                            let response = egui::CollapsingHeader::new(header)
                                .id_salt(("group", key.clone()))
                                .default_open(!collapsed)
                                .show(ui, |ui| {
                                    for entry in list {
                                        let s = &entry.session;
                                        let selected = self.selected.contains(&entry.path);
                                        let label = match &s.user {
                                            Some(user) => format!("{}  @{}", s.name, user),
                                            None => s.name.clone(),
                                        };
                                        let text = item_text(ui, "📄  ", &label);
                                        let resp = ui
                                            .selectable_label(selected, text)
                                            .on_hover_text(format!(
                                                "{}\n{} · duplo clique conecta · ⌘/Ctrl+clique marca vários · botão direito edita",
                                                s.endpoint(),
                                                protocol_label(&s.protocol)
                                            ));
                                        if resp.clicked() {
                                            let mods = ui.input(|i| i.modifiers);
                                            update_selection(
                                                &mut self.selected,
                                                &flat,
                                                &entry.path,
                                                mods.command,
                                                mods.shift,
                                            );
                                            self.focus_terminal = true;
                                        }
                                        if resp.double_clicked() {
                                            self.selected = vec![entry.path.clone()];
                                            pending_connect =
                                                Some((entry.session.clone(), entry.path.clone()));
                                        }
                                        resp.context_menu(|ui| {
                                            if ui.button("Editar").clicked() {
                                                self.form = Some(FormState::from_entry(
                                                    entry,
                                                    FormMode::Edit,
                                                ));
                                                ui.close();
                                            }
                                            if ui.button("Duplicar").clicked() {
                                                self.form = Some(FormState::from_entry(
                                                    entry,
                                                    FormMode::New,
                                                ));
                                                ui.close();
                                            }
                                            let in_selection =
                                                self.selected.contains(&entry.path);
                                            let label =
                                                if in_selection && self.selected.len() > 1 {
                                                    format!(
                                                        "Excluir {} selecionadas",
                                                        self.selected.len()
                                                    )
                                                } else {
                                                    "Excluir".to_string()
                                                };
                                            if ui.button(label).clicked() {
                                                self.confirm_delete = Some(if in_selection {
                                                    selected_entries(
                                                        &self.entries,
                                                        &self.selected,
                                                    )
                                                } else {
                                                    vec![(entry.path.clone(), s.name.clone())]
                                                });
                                                ui.close();
                                            }
                                        });
                                    }
                                });
                            if response.header_response.clicked() {
                                if collapsed {
                                    self.collapsed_groups.remove(&key);
                                } else {
                                    self.collapsed_groups.insert(key);
                                }
                                save_collapsed_groups(&self.collapsed_groups);
                            }
                        }
                    });
            });

        if new_session_clicked {
            let form = self.new_session_form();
            self.form = Some(form);
        }
        if quick_clicked {
            self.form = Some(FormState::quick(self.selected_session()));
        }
        if connect_clicked {
            self.connect_selected();
        }
        if edit_clicked || duplicate_clicked {
            if let Some(entry) = self.selected_entry().cloned() {
                let mode = if edit_clicked {
                    FormMode::Edit
                } else {
                    FormMode::New
                };
                self.form = Some(FormState::from_entry(&entry, mode));
            }
        }
        if delete_clicked {
            self.confirm_delete = Some(selected_entries(&self.entries, &self.selected));
        }
        if let Some((session, path)) = pending_connect {
            self.request_connect(session, Some(path));
        }

        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("store: {}", cloud::store_root().display()))
                        .weak()
                        .small(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(8.0);
                    let mut theme = self.theme;
                    egui::ComboBox::from_id_salt("theme")
                        .selected_text(egui::RichText::new(theme.label()).small())
                        .width(82.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut theme, ThemeChoice::System, "Sistema");
                            ui.selectable_value(&mut theme, ThemeChoice::Light, "Claro");
                            ui.selectable_value(&mut theme, ThemeChoice::Dark, "Escuro");
                        });
                    if theme != self.theme {
                        self.theme = theme;
                        theme.save();
                        apply_theme(ui.ctx(), theme);
                    }
                    ui.label(egui::RichText::new("Tema").weak().small());
                    ui.separator();
                    let update_label = match &self.updates.info {
                        Some(info) => format!("↑ v{} disponível", info.version),
                        None => format!("v{}", update::current_version()),
                    };
                    let update_text = if self.updates.info.is_some() {
                        egui::RichText::new(update_label)
                            .color(ACCENT)
                            .small()
                            .strong()
                    } else {
                        egui::RichText::new(update_label).weak().small()
                    };
                    if ui
                        .add(egui::Button::new(update_text).frame(false))
                        .on_hover_text("atualizações")
                        .clicked()
                    {
                        self.updates.open = true;
                    }
                    ui.separator();
                    if let Some(err) = &self.error {
                        ui.colored_label(ERROR, err);
                    } else if let Some(msg) = &self.message {
                        ui.label(egui::RichText::new(msg).weak().small());
                    }
                });
            });
            ui.add_space(3.0);
        });

        let active_index = self
            .active
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id));
        let mut action = Action::None;
        let mut sftp_action = SftpAction::None;
        let focus = &mut self.focus_terminal;
        let last_log_dir = &mut self.last_log_dir;
        let tabs = &mut self.tabs;
        egui::CentralPanel::default_margins().show(ui, |ui| {
            match active_index.and_then(|index| tabs.get_mut(index)) {
                Some(tab) => {
                    if matches!(&tab.body, TabBody::Terminal(_)) {
                        action = terminal_view(ui, tab, focus, last_log_dir);
                    } else if let TabBody::Sftp(sftp) = &mut tab.body {
                        let session = &tab.session;
                        sftp_action = sftp_view(ui, sftp, session, modal_open);
                    }
                }
                None => {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new("selecione uma sessão — duplo clique conecta")
                                .weak()
                                .size(16.0),
                        );
                    });
                }
            }
        });

        // Antes de qualquer ação fechar/trocar a aba: grava o que mudou nela.
        self.save_sftp_state_if_needed();

        match action {
            Action::OpenSftp => {
                let target = active_index
                    .and_then(|index| self.tabs.get(index))
                    .filter(|tab| matches!(&tab.session.protocol, Protocol::Ssh))
                    .map(|tab| {
                        let creds = match &tab.body {
                            TabBody::Terminal(terminal) => terminal.creds.clone(),
                            TabBody::Sftp(_) => None,
                        };
                        (tab.session.clone(), tab.path.clone(), creds)
                    });
                if let Some((session, path, creds)) = target {
                    self.open_sftp(session, path, creds);
                }
            }
            Action::Disconnect => {
                if let Some(tab) = active_index.and_then(|index| self.tabs.get_mut(index)) {
                    if let TabBody::Terminal(terminal) = &mut tab.body {
                        terminal.conn = None;
                        self.message = Some(format!("desconectado: {}", tab.session.name));
                    }
                }
            }
            Action::Reconnect => {
                // ENTER no terminal não pode disparar conexão com diálogo aberto
                // por cima (o foco do terminal continua ativo).
                let modal_open = self.form.is_some()
                    || self.auth.is_some()
                    || self.import_open
                    || self.pw_import_open
                    || self.confirm_delete.is_some()
                    || self.telegram.open
                    || self.updates.open;
                if !modal_open {
                    if let Some(tab) = active_index.and_then(|index| self.tabs.get(index)) {
                        if matches!(&tab.body, TabBody::Terminal(_)) {
                            let session = tab.session.clone();
                            let path = tab.path.clone();
                            self.request_connect(session, path);
                        }
                    }
                }
            }
            Action::Close => {
                if let Some(tab) = active_index.and_then(|index| self.tabs.get(index)) {
                    self.close_tab(tab.id);
                }
            }
            Action::None => {}
        }

        match sftp_action {
            SftpAction::None => {}
            SftpAction::Reconnect => {
                let id = active_index
                    .and_then(|index| self.tabs.get(index))
                    .map(|tab| tab.id);
                if let Some(id) = id {
                    self.reconnect_sftp(id);
                }
            }
            SftpAction::Close => {
                let id = active_index
                    .and_then(|index| self.tabs.get(index))
                    .map(|tab| tab.id);
                if let Some(id) = id {
                    self.request_close_tab(id);
                }
            }
            SftpAction::CloseConfirmed(id) => self.close_sftp_tab(id),
            SftpAction::Disconnect => {
                let id = active_index
                    .and_then(|index| self.tabs.get(index))
                    .map(|tab| tab.id);
                if let Some(id) = id {
                    if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == id) {
                        if let TabBody::Sftp(sftp) = &mut tab.body {
                            sftp.disconnect();
                        }
                        self.message = Some(format!("SFTP desconectado: {}", tab.session.name));
                    }
                }
            }
            SftpAction::NeedAuth => {
                let request = active_index
                    .and_then(|index| self.tabs.get(index))
                    .and_then(|tab| match &tab.body {
                        TabBody::Sftp(sftp) => Some((
                            tab.id,
                            tab.session.clone(),
                            tab.path.clone(),
                            match &sftp.state {
                                ConnState::Failed(error) => Some(error.clone()),
                                _ => None,
                            },
                        )),
                        TabBody::Terminal(_) => None,
                    });
                if let Some((id, session, path, error)) = request {
                    let mut auth = SshAuth::new(session);
                    auth.path = path;
                    auth.sftp_tab = Some(id);
                    auth.error = error;
                    self.auth = Some(auth);
                }
            }
        }

        // Modais próprios da aba SFTP ativa (conflito/fechamento). Não
        // empilham sobre os diálogos do App.
        if !app_modal_open && self.auth.is_none() {
            let modal_close = self.active.and_then(|id| {
                self.tabs
                    .iter_mut()
                    .find(|tab| tab.id == id)
                    .and_then(|tab| match &mut tab.body {
                        TabBody::Sftp(sftp) => match sftp_modals(ui.ctx(), sftp) {
                            SftpAction::CloseConfirmed(id) => Some(id),
                            _ => None,
                        },
                        TabBody::Terminal(_) => None,
                    })
            });
            if let Some(id) = modal_close {
                self.close_sftp_tab(id);
            }
        }

        self.show_password_import(ui.ctx());
        self.show_form(ui.ctx());
        self.show_auth(ui.ctx());
        self.show_import(ui.ctx());
        self.show_delete_confirm(ui.ctx());
        self.show_telegram(ui.ctx());
        self.show_updates(ui.ctx());
    }
}

impl App {
    fn show_form(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.form.take() else {
            return;
        };
        let groups = group_names(&self.entries);
        let title = match form.mode {
            FormMode::New => "Nova sessão",
            FormMode::Edit => "Editar sessão",
            FormMode::Quick => "Quick connect",
        };
        let mut open = true;
        let mut save = false;
        let mut connect = false;
        let mut cancel = false;

        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                egui::Grid::new("form-grid")
                    .num_columns(2)
                    .spacing([10.0, 7.0])
                    .show(ui, |ui| {
                        if form.mode != FormMode::Quick {
                            ui.label("Nome");
                            ui.add(egui::TextEdit::singleline(&mut form.name).desired_width(300.0));
                            ui.end_row();

                            ui.label("Grupo");
                            ui.vertical(|ui| group_choice(ui, &mut form, &groups));
                            ui.end_row();
                        }

                        ui.label("Protocolo");
                        let before = form.protocol.clone();
                        egui::ComboBox::from_id_salt("protocol")
                            .selected_text(protocol_name(&form.protocol))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut form.protocol, Protocol::Ssh, "SSH");
                                ui.selectable_value(&mut form.protocol, Protocol::Telnet, "Telnet");
                                ui.selectable_value(&mut form.protocol, Protocol::Serial, "Serial");
                            });
                        if before != form.protocol {
                            form.default_port();
                        }
                        ui.end_row();

                        let serial = matches!(form.protocol, Protocol::Serial);
                        ui.label(if serial { "Dispositivo" } else { "Host" });
                        ui.add(
                            egui::TextEdit::singleline(&mut form.host)
                                .hint_text(if serial {
                                    "/dev/tty.usbserial"
                                } else {
                                    "10.0.0.1"
                                })
                                .desired_width(300.0),
                        );
                        ui.end_row();

                        if serial {
                            ui.label("Baud");
                            ui.add(egui::TextEdit::singleline(&mut form.baud).desired_width(120.0));
                            ui.end_row();
                        } else {
                            ui.label("Porta");
                            ui.add(egui::TextEdit::singleline(&mut form.port).desired_width(120.0));
                            ui.end_row();
                        }

                        ui.label("Usuário");
                        ui.add(
                            egui::TextEdit::singleline(&mut form.user)
                                .hint_text("opcional")
                                .desired_width(300.0),
                        );
                        ui.end_row();

                        ui.label("Emulação");
                        ui.add(
                            egui::TextEdit::singleline(&mut form.emulation).desired_width(300.0),
                        );
                        ui.end_row();
                    });

                if let Some(err) = &form.error {
                    ui.add_space(4.0);
                    ui.colored_label(ERROR, err);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if form.mode == FormMode::Quick {
                        if ui
                            .button(egui::RichText::new("Conectar").strong())
                            .clicked()
                        {
                            connect = true;
                        }
                    } else if ui.button(egui::RichText::new("Salvar").strong()).clicked() {
                        save = true;
                    }
                    if ui.button("Cancelar").clicked() {
                        cancel = true;
                    }
                });
            });

        if save {
            if self.save_form(&mut form) {
                self.reload();
                return;
            }
            self.form = Some(form);
            return;
        }
        if connect {
            match form.build() {
                Ok(session) => {
                    self.request_connect(session, None);
                    return;
                }
                Err(msg) => form.error = Some(msg),
            }
        }
        if open && !cancel {
            self.form = Some(form);
        }
    }

    fn show_auth(&mut self, ctx: &egui::Context) {
        let Some(mut auth) = self.auth.take() else {
            return;
        };
        let mut open = true;
        let mut connect = false;
        let mut cancel = false;

        egui::Window::new(format!("Conectar em {}", auth.session.endpoint()))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                egui::Grid::new("auth-grid")
                    .num_columns(2)
                    .spacing([10.0, 7.0])
                    .show(ui, |ui| {
                        ui.label("Usuário");
                        ui.add(egui::TextEdit::singleline(&mut auth.user).desired_width(260.0));
                        ui.end_row();

                        ui.label("Autenticação");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut auth.mode, AuthMode::Auto, "Automático");
                            ui.selectable_value(&mut auth.mode, AuthMode::Key, "Chave");
                            ui.selectable_value(&mut auth.mode, AuthMode::Password, "Senha");
                        });
                        ui.end_row();

                        match auth.mode {
                            AuthMode::Auto => {
                                ui.label("");
                                ui.label(
                                    egui::RichText::new(
                                        "usa o ssh-agent e as chaves de ~/.ssh (config incluso)",
                                    )
                                    .weak()
                                    .small(),
                                );
                                ui.end_row();
                            }
                            AuthMode::Key => {
                                ui.label("Chave privada");
                                ui.add(
                                    egui::TextEdit::singleline(&mut auth.key_path)
                                        .desired_width(320.0),
                                );
                                ui.end_row();
                                ui.label("Passphrase");
                                ui.add(
                                    egui::TextEdit::singleline(&mut auth.passphrase)
                                        .hint_text("se a chave for protegida")
                                        .password(true)
                                        .desired_width(260.0),
                                );
                                ui.end_row();
                            }
                            AuthMode::Password => {
                                ui.label("Senha");
                                ui.add(
                                    egui::TextEdit::singleline(&mut auth.password)
                                        .password(true)
                                        .desired_width(260.0),
                                );
                                ui.end_row();
                                ui.label("");
                                ui.checkbox(&mut auth.save_to_keychain, "Salvar no Keychain");
                                ui.end_row();
                            }
                        }
                    });

                if let Some(err) = &auth.error {
                    ui.add_space(4.0);
                    ui.colored_label(ERROR, err);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("Conectar").strong())
                        .clicked()
                    {
                        connect = true;
                    }
                    if ui.button("Cancelar").clicked() {
                        cancel = true;
                    }
                });
            });

        if connect {
            match auth.credentials() {
                Ok(creds) => {
                    let user = auth.user.trim().to_string();
                    let mut session = auth.session.clone();
                    session.user = (!user.is_empty()).then_some(user);
                    let path = auth.path.clone();
                    let save = match (&creds, auth.save_to_keychain) {
                        (Credentials::Password(password), true) => {
                            Some((session_key(&session), password.clone()))
                        }
                        _ => None,
                    };
                    match auth_target(&auth) {
                        AuthTarget::Terminal => {
                            self.start_connect(session, path, Some(creds), Some(auth), save);
                        }
                        AuthTarget::Sftp(tab_id) => {
                            // o usuário editado no diálogo vale para a aba SFTP
                            if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == tab_id) {
                                if matches!(&tab.body, TabBody::Sftp(_)) {
                                    tab.session.user = session.user.clone();
                                }
                            }
                            self.start_sftp_connect(tab_id, creds, save);
                        }
                    }
                    return;
                }
                Err(msg) => auth.error = Some(msg),
            }
        }
        if open && !cancel {
            self.auth = Some(auth);
        }
    }

    fn show_password_import(&mut self, ctx: &egui::Context) {
        if !self.pw_import_open {
            return;
        }
        let mut open = true;
        let mut run = false;
        egui::Window::new("Importar senhas")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .show(ctx, |ui| {
                ui.label("Uma por linha:  host;senha   ou   host;usuário;senha");
                ui.add(
                    egui::TextEdit::multiline(&mut self.pw_list)
                        .desired_rows(10)
                        .desired_width(520.0)
                        .hint_text("10.0.0.1;admin;senha\n10.0.0.2;senha"),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "As senhas vão para o Keychain desta máquina — nunca para a nuvem.",
                    )
                    .weak()
                    .small(),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new("Importar").strong())
                        .clicked()
                    {
                        run = true;
                    }
                    if let Some(msg) = &self.pw_message {
                        ui.label(egui::RichText::new(msg).weak().small());
                    }
                });
            });
        self.pw_import_open = open;
        if run {
            self.run_password_import();
        }
    }

    fn run_password_import(&mut self) {
        let parsed: Vec<(String, Option<String>, String)> = self
            .pw_list
            .lines()
            .filter_map(parse_password_line)
            .collect();
        let bad = self
            .pw_list
            .lines()
            .filter(|line| {
                let line = line.trim();
                !line.is_empty() && !line.starts_with('#') && parse_password_line(line).is_none()
            })
            .count();
        let sessions: Vec<Session> = self.entries.iter().map(|e| e.session.clone()).collect();
        let (mut saved, mut hosts, mut unknown, mut errors) = (0, 0, 0, 0);

        for (host, user, password) in &parsed {
            let mut matched = 0;
            for session in &sessions {
                if session.host != *host {
                    continue;
                }
                if let Some(user) = user {
                    if session.user.as_deref() != Some(user.as_str()) {
                        continue;
                    }
                }
                match keychain::save(&session_key(session), password) {
                    Ok(()) => {
                        saved += 1;
                        matched += 1;
                    }
                    Err(_) => errors += 1,
                }
            }
            if matched == 0 {
                unknown += 1;
            } else {
                hosts += 1;
            }
        }

        self.pw_message = Some(format!(
            "{hosts} hosts → {saved} sessões no Keychain · {unknown} hosts sem sessão · {bad} linhas inválidas · {errors} erros"
        ));
    }

    fn show_import(&mut self, ctx: &egui::Context) {
        if !self.import_open {
            return;
        }
        let mut open = true;
        let mut requested_import = false;
        egui::Window::new("Importar sessões do SecureCRT")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label("Pasta Sessions do SecureCRT:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.import_path)
                            .hint_text("/caminho/para/Sessions")
                            .desired_width(380.0),
                    );
                    if ui.button("Procurar…").clicked() {
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_title("Pasta Sessions do SecureCRT")
                            .pick_folder()
                        {
                            self.import_path = dir.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.add_space(6.0);
                if ui.button("Importar").clicked() {
                    requested_import = true;
                }
            });
        self.import_open = open && !requested_import;
        if requested_import {
            self.run_import();
        }
    }

    fn show_delete_confirm(&mut self, ctx: &egui::Context) {
        let Some(targets) = self.confirm_delete.clone() else {
            return;
        };
        let mut open = true;
        let mut confirmed = false;
        let mut cancel = false;
        let title = if targets.len() == 1 {
            "Excluir sessão"
        } else {
            "Excluir sessões"
        };
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                if let [(_, name)] = targets.as_slice() {
                    ui.label(format!("Excluir “{name}”?"));
                } else {
                    ui.label(format!("Excluir {} sessões?", targets.len()));
                    ui.add_space(4.0);
                    for (_, name) in targets.iter().take(8) {
                        ui.label(egui::RichText::new(format!("· {name}")).weak());
                    }
                    if targets.len() > 8 {
                        ui.label(
                            egui::RichText::new(format!("… e mais {}", targets.len() - 8)).weak(),
                        );
                    }
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Excluir").clicked() {
                        confirmed = true;
                    }
                    if ui.button("Cancelar").clicked() {
                        cancel = true;
                    }
                });
            });
        if confirmed {
            self.confirm_delete = None;
            let Some(store) = &self.store else {
                self.error = Some("store não abriu".into());
                return;
            };
            let mut removed = Vec::new();
            let mut error = None;
            for (path, _) in &targets {
                match store.delete(path) {
                    Ok(()) => removed.push(path.clone()),
                    Err(e) => error = Some(format!("{e:#}")),
                }
            }
            self.selected.retain(|path| !removed.contains(path));
            self.reload_requested = true;
            match (targets.len(), error) {
                (1, None) => self.message = Some(format!("sessão '{}' excluída", targets[0].1)),
                (_, None) => self.message = Some(format!("{} sessões excluídas", removed.len())),
                (_, Some(e)) => self.error = Some(e),
            }
            return;
        }
        if open && !cancel {
            self.confirm_delete = Some(targets);
        } else {
            self.confirm_delete = None;
        }
    }
}

impl App {
    fn show_telegram(&mut self, ctx: &egui::Context) {
        // Drena o job mesmo com a janela fechada (rede nunca trava a UI).
        let mut done = None;
        if let Some(rx) = &self.telegram.rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.telegram.rx = None;
                    self.telegram.busy = false;
                    done = Some(result);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.telegram.rx = None;
                    self.telegram.busy = false;
                    self.telegram.error = Some("operação abortada".into());
                }
            }
        }
        if let Some(result) = done {
            match result {
                Ok(TelegramDone::Connected(config)) => {
                    self.telegram.config = Some(config);
                    self.telegram.token.clear();
                    self.telegram.chat.clear();
                    self.telegram.message = Some("bot conectado".into());
                }
                Ok(TelegramDone::Backup(report)) => {
                    self.telegram.message = Some(format!(
                        "backup enviado: {} sessões ({} bytes)",
                        report.files, report.bytes
                    ));
                }
                Ok(TelegramDone::Restore(report)) => {
                    self.reload_requested = true;
                    let mut message = format!(
                        "{} restauradas · {} já iguais · {} conflitos",
                        report.restored, report.unchanged, report.conflicts
                    );
                    if let Some(dir) = &report.conflicts_dir {
                        message.push_str(&format!(
                            " — versões remotas dos conflitos em {}",
                            dir.display()
                        ));
                    }
                    self.telegram.message = Some(message);
                }
                Err(error) => self.telegram.error = Some(error),
            }
        }

        if !self.telegram.open {
            return;
        }

        let root = cloud::store_root();
        let mut action = TelegramAction::None;
        let mut open = self.telegram.open;
        let tg = &mut self.telegram;
        egui::Window::new("Nuvem no Telegram")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(480.0);
                match tg.config.clone() {
                    Some(config) => {
                        let bot = config
                            .bot
                            .as_deref()
                            .map(|name| format!("@{name}"))
                            .unwrap_or_else(|| "bot".to_string());
                        ui.label(format!("{bot} · chat {}", config.chat_id));
                        ui.label(
                            egui::RichText::new(
                                "As sessões vão num documento fixado no chat (sem senhas). O backup novo substitui o anterior; restaurar não apaga nada local.",
                            )
                            .weak()
                            .small(),
                        );
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(!tg.busy, egui::Button::new("Enviar sessões"))
                                .on_hover_text("empacota a store e fixa no chat")
                                .clicked()
                            {
                                action = TelegramAction::Backup;
                            }
                            if ui
                                .add_enabled(!tg.busy, egui::Button::new("Baixar sessões"))
                                .on_hover_text("baixa o documento fixado; conflitos vão para disco")
                                .clicked()
                            {
                                action = TelegramAction::Restore;
                            }
                            if ui
                                .add_enabled(
                                    !tg.busy,
                                    egui::Button::new(
                                        egui::RichText::new("Esquecer bot").color(ERROR),
                                    ),
                                )
                                .clicked()
                            {
                                action = TelegramAction::Forget;
                            }
                        });
                    }
                    None => {
                        ui.label(
                            egui::RichText::new(
                                "No Telegram: fale com o @BotFather, crie um bot (/newbot) e mande /start para ele. O token fica só no Keychain.",
                            )
                            .weak()
                            .small(),
                        );
                        ui.add_space(6.0);
                        egui::Grid::new("telegram-grid")
                            .num_columns(2)
                            .spacing([10.0, 7.0])
                            .show(ui, |ui| {
                                ui.label("Token do bot");
                                ui.add(
                                    egui::TextEdit::singleline(&mut tg.token)
                                        .password(true)
                                        .desired_width(340.0)
                                        .hint_text("123456:ABC…"),
                                );
                                ui.end_row();

                                ui.label("Chat (opcional)");
                                ui.add(
                                    egui::TextEdit::singleline(&mut tg.chat)
                                        .desired_width(340.0)
                                        .hint_text("vazio = descobre pelo /start"),
                                );
                                ui.end_row();
                            });
                        ui.add_space(6.0);
                        if ui
                            .add_enabled(
                                !tg.busy && !tg.token.trim().is_empty(),
                                egui::Button::new(egui::RichText::new("Conectar").strong()),
                            )
                            .clicked()
                        {
                            action = TelegramAction::Login;
                        }
                    }
                }

                if tg.busy {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new("falando com o Telegram…").weak().small());
                    });
                }
                if let Some(message) = &tg.message {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(message).weak().small());
                }
                if let Some(error) = &tg.error {
                    ui.add_space(4.0);
                    ui.colored_label(ERROR, error);
                }
            });
        tg.open = open;

        match action {
            TelegramAction::None => {}
            TelegramAction::Login => {
                let token = tg.token.trim().to_string();
                let chat = match tg.chat.trim() {
                    "" => Ok(None),
                    value => value
                        .parse::<i64>()
                        .map(Some)
                        .map_err(|_| "chat deve ser um ID numérico".to_string()),
                };
                match chat {
                    Ok(chat) if !token.is_empty() && !tg.busy => {
                        start_telegram_job(tg, move || {
                            telegram::login(&token, chat)
                                .map(TelegramDone::Connected)
                                .map_err(|error| format!("{error:#}"))
                        });
                    }
                    Ok(_) => {}
                    Err(error) => tg.error = Some(error),
                }
            }
            TelegramAction::Backup => {
                if let Some(config) = tg.config.clone() {
                    let root = root.clone();
                    start_telegram_job(tg, move || {
                        telegram::backup(&root, &config)
                            .map(TelegramDone::Backup)
                            .map_err(|error| format!("{error:#}"))
                    });
                }
            }
            TelegramAction::Restore => {
                if let Some(config) = tg.config.clone() {
                    let root = root.clone();
                    start_telegram_job(tg, move || {
                        telegram::restore(&root, &config)
                            .map(TelegramDone::Restore)
                            .map_err(|error| format!("{error:#}"))
                    });
                }
            }
            TelegramAction::Forget => match telegram::forget() {
                Ok(()) => {
                    tg.config = None;
                    tg.token.clear();
                    tg.chat.clear();
                    tg.message = Some("bot esquecido nesta máquina".into());
                    tg.error = None;
                }
                Err(error) => tg.error = Some(format!("{error:#}")),
            },
        }
    }
}

impl App {
    fn show_updates(&mut self, ctx: &egui::Context) {
        let mut done = None;
        if let Some(rx) = &self.updates.rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.updates.rx = None;
                    done = Some(result);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.updates.rx = None;
                    self.updates.error = Some("operação abortada".into());
                }
            }
        }
        if let Some(done) = done {
            match done {
                UpdateDone::Checked(Ok(info)) => {
                    self.updates.checked = true;
                    self.updates.info = info;
                }
                UpdateDone::Checked(Err(error)) => {
                    // falha no check automático é silenciosa; manual mostra
                    if self.updates.manual {
                        self.updates.error = Some(error);
                    }
                }
                UpdateDone::Installed(Ok(())) => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                UpdateDone::Installed(Err(error)) => {
                    self.updates.error = Some(error);
                }
            }
            self.updates.installing = false;
            self.updates.manual = false;
        }

        if !self.updates.open {
            return;
        }

        let mut open = self.updates.open;
        let mut close = false;
        let mut action = UpdateAction::None;
        let current = update::current_version();
        let install_supported = update::self_install_supported();
        let state = &mut self.updates;
        egui::Window::new("Atualização")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.set_min_width(460.0);
                ui.horizontal(|ui| {
                    ui.label("Versão atual:");
                    ui.label(egui::RichText::new(format!("v{current}")).strong());
                });
                ui.add_space(4.0);

                if let Some(info) = &state.info {
                    ui.label(
                        egui::RichText::new(format!("v{} disponível", info.version))
                            .color(ACCENT)
                            .strong(),
                    );
                    if let Some(asset) = &info.asset {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} · {:.1} MB",
                                asset.name,
                                asset.size as f64 / 1_048_576.0
                            ))
                            .weak()
                            .small(),
                        );
                    } else {
                        ui.label(
                            egui::RichText::new(
                                "não há build desta versão para a sua plataforma",
                            )
                            .weak()
                            .small(),
                        );
                    }
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if install_supported
                            && info.asset.is_some()
                            && ui
                                .add_enabled(
                                    !state.busy(),
                                    egui::Button::new(
                                        egui::RichText::new("Baixar e reiniciar").strong(),
                                    ),
                                )
                                .clicked()
                        {
                            action = UpdateAction::Install;
                        }
                        if ui
                            .add_enabled(!state.busy(), egui::Button::new("Abrir release"))
                            .clicked()
                        {
                            action = UpdateAction::OpenRelease;
                        }
                        if ui.button("Depois").clicked() {
                            close = true;
                        }
                    });
                    if !install_supported {
                        ui.add_space(2.0);
                        ui.label(
                            egui::RichText::new(
                                "Instalação automática só no macOS rodando pelo .app; use Abrir release.",
                            )
                            .weak()
                            .small(),
                        );
                    }
                } else if state.busy() {
                    ui.label("Procurando atualizações…");
                } else if state.checked {
                    ui.label("Você já está na versão mais recente.");
                } else {
                    ui.label("Ainda não verifiquei (sem conexão?).");
                }

                if state.busy() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            egui::RichText::new(if state.installing {
                                "baixando e preparando…"
                            } else {
                                "consultando o GitHub…"
                            })
                            .weak()
                            .small(),
                        );
                    });
                }
                if let Some(error) = &state.error {
                    ui.add_space(4.0);
                    ui.colored_label(ERROR, error);
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(!state.busy(), egui::Button::new("Verificar de novo"))
                        .clicked()
                    {
                        action = UpdateAction::Check;
                    }
                    if ui.button("Fechar").clicked() {
                        close = true;
                    }
                });
            });

        if close {
            open = false;
        }
        self.updates.open = open;
        match action {
            UpdateAction::None => {}
            UpdateAction::Check => {
                self.updates.start_check(true);
                self.updates.open = true;
            }
            UpdateAction::Install => {
                if let Some(info) = self.updates.info.clone() {
                    self.updates.start_install(info);
                }
            }
            UpdateAction::OpenRelease => {
                let url = self
                    .updates
                    .info
                    .as_ref()
                    .map(|info| info.html_url.clone())
                    .unwrap_or_else(update::releases_url);
                if let Err(error) = update::open_url(&url) {
                    self.updates.error = Some(format!("{error:#}"));
                }
            }
        }
    }
}

enum SearchAction {
    None,
    Close,
    Step(bool),
}

fn search_bar(ui: &mut egui::Ui, search: &mut Search, term: &mut Terminal) -> SearchAction {
    let mut action = SearchAction::None;
    if search.refresh(term.screen()) {
        // salta para a primeira ocorrência quando a busca muda
        if let Some((row, _, _)) = search.matches.first() {
            term.screen_mut().scroll_to_row(*row);
        }
    }
    ui.horizontal(|ui| {
        let field = ui.add(
            egui::TextEdit::singleline(&mut search.query)
                .hint_text("buscar no terminal")
                .desired_width(220.0),
        );
        if !search.focused {
            field.request_focus();
            search.focused = true;
        }
        if field.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            let backwards = ui.input(|i| i.modifiers.shift);
            action = SearchAction::Step(!backwards);
        }
        if ui.button("◀").clicked() {
            action = SearchAction::Step(false);
        }
        if ui.button("▶").clicked() {
            action = SearchAction::Step(true);
        }
        let count = if search.matches.is_empty() {
            "0".to_string()
        } else {
            format!("{}/{}", search.current + 1, search.matches.len())
        };
        ui.label(egui::RichText::new(count).weak().small());
        if ui.button("×").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            action = SearchAction::Close;
        }
    });
    action
}

fn terminal_view(
    ui: &mut egui::Ui,
    tab: &mut Tab,
    focus: &mut bool,
    last_log_dir: &mut Option<PathBuf>,
) -> Action {
    let mut action = Action::None;
    let TabBody::Terminal(terminal) = &mut tab.body else {
        return Action::None;
    };
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.heading(&tab.session.name);
        ui.label(
            egui::RichText::new(format!(
                "{} · {}",
                tab.session.endpoint(),
                protocol_label(&tab.session.protocol)
            ))
            .weak(),
        );
        if terminal.conn.is_none() {
            ui.label(
                egui::RichText::new("desconectado · ENTER reconecta")
                    .color(ERROR)
                    .small(),
            );
        }
        if terminal.term.screen().scroll_offset() > 0 {
            let offset = terminal.term.screen().scroll_offset();
            if ui
                .button(egui::RichText::new(format!("↑ {offset} linhas")).small())
                .on_hover_text("voltar ao fim")
                .clicked()
            {
                terminal.term.screen_mut().scroll_reset();
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if matches!(&tab.session.protocol, Protocol::Ssh) && ui.button("Abrir SFTP").clicked() {
                action = Action::OpenSftp;
            }
            if terminal.conn.is_some() {
                if ui.button("Desconectar").clicked() {
                    action = Action::Disconnect;
                }
                let label = if terminal.log.is_some() {
                    "● gravando log"
                } else {
                    "Gravar log"
                };
                let response = ui.button(label);
                if response.clicked() {
                    if terminal.log.is_some() {
                        terminal.log = None;
                        terminal.notice = Some("log encerrado".into());
                    } else {
                        match start_log_dialog(&tab.session, last_log_dir) {
                            Ok(Some(log)) => {
                                terminal.notice = Some(format!("log: {}", log.path.display()));
                                terminal.log = Some(log);
                            }
                            Ok(None) => {}
                            Err(e) => terminal.notice = Some(format!("log falhou: {e:#}")),
                        }
                    }
                }
                if let Some(notice) = &terminal.notice {
                    response.on_hover_text(notice);
                }
            } else {
                if ui.button("Reconectar").clicked() {
                    action = Action::Reconnect;
                }
                if ui.button("Fechar").clicked() {
                    action = Action::Close;
                }
            }
        });
    });
    ui.add_space(8.0);

    if terminal.search.is_some() {
        let mut search_action = SearchAction::None;
        if let Some(search) = terminal.search.as_mut() {
            search_action = search_bar(ui, search, &mut terminal.term);
        }
        match search_action {
            SearchAction::None => {}
            SearchAction::Close => terminal.search = None,
            SearchAction::Step(forward) => {
                if let Some(search) = terminal.search.as_mut() {
                    search.step(forward, terminal.term.screen_mut());
                }
            }
        }
    }

    {
        let live = terminal;
        let rect = ui.available_rect_before_wrap();
        let (cell_w, cell_h) = cell_metrics(ui);
        let cols = ((rect.width() / cell_w).floor() as usize).max(20);
        let rows = ((rect.height() / cell_h).floor() as usize).max(5);
        let (have_cols, have_rows) = live.term.screen().size();
        if have_cols != cols || have_rows != rows {
            live.term.resize(cols, rows);
            if let Some(conn) = &live.conn {
                conn.resize(cols as u16, rows as u16);
            }
        }

        let terminal_id = ui.id().with("terminal");
        let response = ui.interact(rect, terminal_id, egui::Sense::click_and_drag());

        // Linha visível -> linha absoluta (0 = começo do histórico).
        let top_row = |live: &TerminalTab| {
            live.term
                .screen()
                .total_rows()
                .saturating_sub(rows)
                .saturating_sub(live.term.screen().scroll_offset())
        };
        let cell_at = |live: &TerminalTab, pos: egui::Pos2| -> (usize, usize) {
            let col = (pos.x - rect.min.x) / cell_w;
            let row = (pos.y - rect.min.y) / cell_h;
            (
                top_row(live) + row.clamp(0.0, (rows - 1) as f32) as usize,
                col.clamp(0.0, (cols - 1) as f32) as usize,
            )
        };

        if response.drag_started() {
            if let Some(pos) = response.interact_pointer_pos() {
                let at = cell_at(&*live, pos);
                live.sel = Some(Selection {
                    anchor: at,
                    cursor: at,
                });
            }
        }
        if response.dragged() {
            if let Some(pos) = response.interact_pointer_pos() {
                // arrastar além da borda rola o histórico (seleção atravessa a tela)
                if pos.y < rect.min.y {
                    live.term.screen_mut().scroll_back(1);
                } else if pos.y > rect.max.y {
                    live.term.screen_mut().scroll_forward(1);
                }
                let at = cell_at(&*live, pos);
                if let Some(sel) = live.sel.as_mut() {
                    sel.cursor = at;
                }
            }
        }
        if response.clicked() {
            live.sel = None;
        }

        // roda do mouse rola o histórico
        if response.hovered() {
            let delta = ui.input(|i| i.smooth_scroll_delta.y);
            if delta.abs() > 0.5 {
                let lines = ((delta.abs() / cell_h).ceil() as usize).max(1);
                if delta > 0.0 {
                    live.term.screen_mut().scroll_back(lines);
                } else {
                    live.term.screen_mut().scroll_forward(lines);
                }
            }
        }

        paint_terminal(
            ui,
            live.term.screen(),
            rect,
            cell_w,
            cell_h,
            live.sel.as_ref(),
            live.search.as_ref(),
        );

        if response.clicked() || *focus {
            response.request_focus();
            *focus = false;
        }
        if response.has_focus() {
            // TAB e setas pertencem ao terminal: sem isso o egui usa TAB
            // para navegar entre botões em vez de autocompletar no shell.
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    terminal_id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
            if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::A)) {
                let cols = live.term.screen().cols();
                let total = live.term.screen().total_rows();
                live.sel = Some(Selection {
                    anchor: (0, 0),
                    cursor: (total.saturating_sub(1), cols.saturating_sub(1)),
                });
                live.term.screen_mut().scroll_to_row(0);
            }
            if let Some(sel) = live.sel.as_ref() {
                if wants_copy(ui) {
                    let text = selection_text(live.term.screen(), sel);
                    if !text.is_empty() {
                        ui.ctx().copy_text(text);
                    }
                }
            }
            let typing = ui.input(|i| {
                i.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Text(_)
                            | egui::Event::Paste(_)
                            | egui::Event::Key { pressed: true, .. }
                    )
                })
            });
            if typing {
                live.term.screen_mut().scroll_reset();
            }
            if let Some(conn) = &live.conn {
                send_input(ui, conn, live.term.screen().bracketed_paste());
            } else if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                // Como no SecureCRT: ENTER num terminal caído reconecta.
                action = Action::Reconnect;
            }
        }
    }
    action
}

/// Colar no terminal: quebras de linha viram CR (o que o shell espera).
fn paste_bytes(text: &str) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    normalized.into_bytes()
}

fn send_input(ui: &egui::Ui, conn: &Connection, bracketed_paste: bool) {
    let events = ui.input(|i| i.events.clone());
    if std::env::var_os("SSHC_DEBUG_KEYS").is_some() && !events.is_empty() {
        eprintln!("[keys] {events:?}");
    }
    for event in events {
        match event {
            egui::Event::Text(text) => {
                let _ = conn.send(text.as_bytes());
            }
            // Cmd+V / Ctrl+V: o egui entrega o texto do clipboard aqui.
            egui::Event::Paste(text) => {
                let mut bytes = paste_bytes(&text);
                if bracketed_paste && !bytes.is_empty() {
                    let mut wrapped = b"\x1b[200~".to_vec();
                    wrapped.append(&mut bytes);
                    wrapped.extend_from_slice(b"\x1b[201~");
                    bytes = wrapped;
                }
                if !bytes.is_empty() {
                    let _ = conn.send(&bytes);
                }
            }
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                // Ctrl+C/^D/^Z etc: o terminal precisa do código de controle.
                if modifiers.ctrl && !modifiers.command {
                    let name = key.name();
                    if name.len() == 1 {
                        let letter = name.as_bytes()[0].to_ascii_uppercase();
                        if letter.is_ascii_uppercase() {
                            let _ = conn.send(&[letter & 0x1f]);
                            continue;
                        }
                    }
                }
                let bytes: &[u8] = match key {
                    egui::Key::Enter => b"\r",
                    egui::Key::Backspace => b"\x7f",
                    egui::Key::Tab => b"\t",
                    egui::Key::Escape => b"\x1b",
                    egui::Key::ArrowUp => b"\x1b[A",
                    egui::Key::ArrowDown => b"\x1b[B",
                    egui::Key::ArrowRight => b"\x1b[C",
                    egui::Key::ArrowLeft => b"\x1b[D",
                    egui::Key::Home => b"\x1b[H",
                    egui::Key::End => b"\x1b[F",
                    egui::Key::Delete => b"\x1b[3~",
                    _ => b"",
                };
                if !bytes.is_empty() {
                    let _ = conn.send(bytes);
                }
            }
            _ => {}
        }
    }
}

fn cell_metrics(ui: &egui::Ui) -> (f32, f32) {
    let font = egui::FontId::monospace(13.0);
    ui.ctx().fonts_mut(|f| {
        (
            f.glyph_width(&font, 'M').max(1.0),
            f.row_height(&font).max(1.0),
        )
    })
}

/// Cmd+C (macOS) ou Ctrl+C (demais SOs).
fn wants_copy(ui: &egui::Ui) -> bool {
    ui.input(|i| {
        i.events.iter().any(|event| match event {
            egui::Event::Copy => true,
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => matches!(key, egui::Key::C) && modifiers.command,
            _ => false,
        })
    })
}

fn paint_terminal(
    ui: &mut egui::Ui,
    screen: &sshc_term::Screen,
    rect: egui::Rect,
    cell_w: f32,
    cell_h: f32,
    selection: Option<&Selection>,
    search: Option<&Search>,
) {
    let font = egui::FontId::monospace(13.0);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, TERM_BG);
    let (cols, rows) = screen.size();
    let top = screen
        .total_rows()
        .saturating_sub(rows)
        .saturating_sub(screen.scroll_offset());

    if let Some(search) = search {
        for (index, (row, start, end)) in search.matches.iter().enumerate() {
            if *row < top || *row >= top + rows {
                continue;
            }
            let y = *row - top;
            let color = if index == search.current {
                egui::Color32::from_rgba_unmultiplied(250, 180, 60, 200)
            } else {
                egui::Color32::from_rgba_unmultiplied(250, 200, 120, 90)
            };
            let min = rect.min + egui::vec2(*start as f32 * cell_w, y as f32 * cell_h);
            let size = egui::vec2((end - start) as f32 * cell_w, cell_h);
            painter.rect_filled(egui::Rect::from_min_size(min, size), 0.0, color);
        }
    }

    if let Some(sel) = selection {
        let ((sr, sc), (er, ec)) = sel.range();
        for abs_row in sr..=er {
            if abs_row < top || abs_row >= top + rows {
                continue;
            }
            let row = abs_row - top;
            let start = if abs_row == sr { sc } else { 0 };
            let end = if abs_row == er {
                ec.min(cols - 1)
            } else {
                cols - 1
            };
            let min = rect.min + egui::vec2(start as f32 * cell_w, row as f32 * cell_h);
            let size = egui::vec2((end + 1 - start) as f32 * cell_w, cell_h);
            painter.rect_filled(
                egui::Rect::from_min_size(min, size),
                0.0,
                egui::Color32::from_rgba_unmultiplied(96, 165, 250, 90),
            );
        }
    }

    for y in 0..rows {
        let cells = screen.visible_row(y);
        let mut x = 0;
        while x < cols {
            let cell = cells[x];
            let mut run = String::new();
            let mut run_len = 0;
            while x + run_len < cols {
                let next = cells[x + run_len];
                if next.fg != cell.fg || next.bg != cell.bg || next.bold != cell.bold {
                    break;
                }
                run.push(next.ch);
                run_len += 1;
            }
            let pos = rect.min + egui::vec2(x as f32 * cell_w, y as f32 * cell_h);
            if cell.bg != Color::Default {
                painter.rect_filled(
                    egui::Rect::from_min_size(pos, egui::vec2(run_len as f32 * cell_w, cell_h)),
                    0.0,
                    color32(cell.bg),
                );
            }
            if !run.trim().is_empty() {
                painter.text(
                    pos,
                    egui::Align2::LEFT_TOP,
                    &run,
                    font.clone(),
                    color32(cell.fg),
                );
            }
            x += run_len;
        }
    }

    let (cx, cy) = screen.cursor();
    if screen.scroll_offset() == 0 && cx < cols && cy < rows {
        let pos = rect.min + egui::vec2(cx as f32 * cell_w, cy as f32 * cell_h);
        painter.rect_filled(
            egui::Rect::from_min_size(pos, egui::vec2(cell_w, cell_h)),
            0.0,
            egui::Color32::from_rgba_unmultiplied(200, 210, 220, 110),
        );
    }
}

fn color32(color: Color) -> egui::Color32 {
    match color {
        Color::Default => egui::Color32::from_rgb(200, 210, 220),
        Color::Rgb(r, g, b) => egui::Color32::from_rgb(r, g, b),
        Color::Idx(i) => {
            const BASE: [(u8, u8, u8); 16] = [
                (24, 26, 31),
                (232, 92, 92),
                (126, 199, 110),
                (214, 191, 102),
                (98, 152, 230),
                (191, 122, 209),
                (90, 191, 191),
                (200, 210, 220),
                (90, 98, 112),
                (255, 122, 122),
                (150, 230, 130),
                (245, 220, 130),
                (130, 180, 255),
                (220, 150, 240),
                (120, 220, 220),
                (240, 244, 250),
            ];
            if (i as usize) < 16 {
                let (r, g, b) = BASE[i as usize];
                egui::Color32::from_rgb(r, g, b)
            } else if i < 232 {
                let i = i - 16;
                let r = (i / 36) % 6;
                let g = (i / 6) % 6;
                let b = i % 6;
                let scale = |v: u8| -> u8 {
                    if v == 0 {
                        0
                    } else {
                        55 + v * 40
                    }
                };
                egui::Color32::from_rgb(scale(r), scale(g), scale(b))
            } else {
                let v = 8 + (i - 232) * 10;
                egui::Color32::from_rgb(v, v, v)
            }
        }
    }
}

/// Dump da tela em PPM (sem dependência de encoder); converte-se com sips.
fn save_ppm(path: &str, image: &egui::ColorImage) -> Result<()> {
    let [w, h] = image.size;
    let mut out = Vec::with_capacity(w * h * 3 + 32);
    out.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
    for px in &image.pixels {
        out.extend_from_slice(&[px.r(), px.g(), px.b()]);
    }
    std::fs::write(path, out)?;
    Ok(())
}

fn protocol_label(p: &Protocol) -> &'static str {
    match p {
        Protocol::Ssh => "ssh",
        Protocol::Telnet => "telnet",
        Protocol::Serial => "serial",
        Protocol::LocalShell => "local",
        Protocol::Other(_) => "outro",
    }
}

fn protocol_name(p: &Protocol) -> &'static str {
    match p {
        Protocol::Ssh => "SSH",
        Protocol::Telnet => "Telnet",
        Protocol::Serial => "Serial",
        Protocol::LocalShell => "Shell local",
        Protocol::Other(_) => "Outro",
    }
}

/// Título da aba SFTP com o badge de transferências ativas.
fn sftp_tab_title(name: &str, active: usize) -> String {
    if active > 0 {
        format!("SFTP · {name} ({active})")
    } else {
        format!("SFTP · {name}")
    }
}

/// Erros de conexão SFTP com uma dica para os casos conhecidos.
fn friendly_sftp_error(message: &str) -> String {
    if message.contains("AdministrativelyProhibited") || message.contains("MaxSessions") {
        format!(
            "{message}\nMaxSessions: o servidor limita sessões simultâneas; feche uma conexão extra e tente de novo"
        )
    } else {
        message.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(group: &str, name: &str, host: &str) -> SessionEntry {
        SessionEntry {
            group: group.to_string(),
            path: PathBuf::from(format!("/tmp/{group}/{name}.toml")),
            session: Session {
                name: name.to_string(),
                protocol: Protocol::Ssh,
                host: host.to_string(),
                port: 22,
                user: None,
                emulation: "xterm-256color".to_string(),
                auth: None,
                description: None,
                baud: None,
            },
        }
    }

    #[test]
    fn groups_preserve_folder_and_order() {
        let entries = vec![
            entry("A", "z", "10.0.0.9"),
            entry("A", "b", "10.0.0.1"),
            entry("B", "c", "10.0.0.2"),
        ];
        let groups = grouped(&entries, "");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "A");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "B");
    }

    #[test]
    fn filter_matches_name_host_and_group() {
        let entries = vec![
            entry("Clientes", "web-01", "10.1.1.1"),
            entry("Lab", "db", "10.2.2.2"),
        ];
        assert_eq!(grouped(&entries, "web").len(), 1);
        assert_eq!(grouped(&entries, "10.2").len(), 1);
        assert_eq!(grouped(&entries, "lab").len(), 1);
        assert!(grouped(&entries, "nao-existe").is_empty());
    }

    #[test]
    fn selection_supports_toggle_and_shift_range() {
        let flat: Vec<PathBuf> = ["a", "b", "c", "d"].iter().map(PathBuf::from).collect();
        let mut sel = Vec::new();
        update_selection(&mut sel, &flat, Path::new("b"), false, false);
        assert_eq!(sel, vec![PathBuf::from("b")]);
        update_selection(&mut sel, &flat, Path::new("d"), true, false);
        assert_eq!(sel, vec![PathBuf::from("b"), PathBuf::from("d")]);
        update_selection(&mut sel, &flat, Path::new("b"), true, false);
        assert_eq!(sel, vec![PathBuf::from("d")]);
        update_selection(&mut sel, &flat, Path::new("a"), false, true);
        assert_eq!(
            sel,
            ["a", "b", "c", "d"]
                .iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn selected_entries_follows_tree_order() {
        let entries = vec![entry("A", "z", "10.0.0.9"), entry("B", "c", "10.0.0.2")];
        let picked = selected_entries(
            &entries,
            &[entries[1].path.clone(), entries[0].path.clone()],
        );
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].1, "z");
        assert_eq!(picked[1].1, "c");
    }

    #[test]
    fn find_matches_looks_at_history_and_screen() {
        let mut term = sshc_term::Terminal::new(20, 2);
        term.feed(b"erro: disco\r\nok\r\nERRO de rede\r\nfim");
        let hits = find_matches(term.screen(), "erro");
        // uma ocorrência na linha 0 (já no histórico) e outra na tela
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert!(hits.iter().all(|(_, start, end)| end - start == 4));
    }

    #[test]
    fn find_matches_is_case_insensitive_and_empty_query_is_noop() {
        let mut term = sshc_term::Terminal::new(20, 3);
        term.feed(b"Ping OK\r\nping ok");
        assert_eq!(find_matches(term.screen(), "PING").len(), 2);
        assert!(find_matches(term.screen(), "   ").is_empty());
    }

    #[test]
    fn paste_converts_newlines_to_cr() {
        assert_eq!(paste_bytes("ls -la\n"), b"ls -la\r".to_vec());
        assert_eq!(paste_bytes("a\r\nb"), b"a\rb".to_vec());
        assert_eq!(paste_bytes("sem quebra"), b"sem quebra".to_vec());
        assert!(paste_bytes("").is_empty());
    }

    #[test]
    fn enter_on_disconnected_terminal_requests_reconnect() {
        let mut tab = Tab {
            id: 1,
            session: session_named("caida"),
            path: None,
            body: TabBody::Terminal(TerminalTab {
                term: Terminal::new(100, 30),
                conn: None,
                creds: None,
                sel: None,
                search: None,
                log: None,
                notice: None,
            }),
        };
        let mut focus = true;
        let mut last_log_dir = None;
        let mut action = Action::None;
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let _ = egui::Context::default().run_ui(input, |ui| {
            action = terminal_view(ui, &mut tab, &mut focus, &mut last_log_dir);
        });
        assert!(matches!(action, Action::Reconnect));
    }

    #[test]
    fn selection_range_normalizes_reversed() {
        let sel = Selection {
            anchor: (2, 5),
            cursor: (0, 1),
        };
        assert_eq!(sel.range(), ((0, 1), (2, 5)));
        let same = Selection {
            anchor: (1, 1),
            cursor: (1, 1),
        };
        assert_eq!(same.range(), ((1, 1), (1, 1)));
    }

    #[test]
    fn selection_text_extracts_and_trims() {
        let mut term = sshc_term::Terminal::new(10, 3);
        term.feed(b"abc\r\ndef");
        let sel = Selection {
            anchor: (0, 0),
            cursor: (1, 1),
        };
        assert_eq!(selection_text(term.screen(), &sel), "abc\nde");
    }

    #[test]
    fn group_names_are_sorted_and_unique() {
        let entries = vec![
            entry("B", "b", "10.0.0.3"),
            entry("A", "a1", "10.0.0.1"),
            entry("A", "a2", "10.0.0.2"),
            entry("", "raiz", "10.0.0.9"),
        ];
        assert_eq!(
            group_names(&entries),
            vec!["A".to_string(), "B".to_string()]
        );
    }

    #[test]
    fn new_group_requires_a_valid_name() {
        let mut form = FormState::new();
        form.name = "x".into();
        form.host = "h".into();
        form.new_group = true;
        assert!(form.build().unwrap_err().contains("pasta"));

        form.group = "../fora".into();
        assert!(form.build().is_err());
        form.group = "/fora".into();
        assert!(form.build().is_err());
        form.group = "Clientes/OLTs".into();
        assert!(form.build().is_ok());
    }

    #[test]
    fn window_size_parses_and_clamps() {
        assert_eq!(parse_window_size("1120x720"), Some((1120, 720)));
        assert_eq!(parse_window_size(" 800 x 600 "), Some((800, 600)));
        assert_eq!(parse_window_size("100x100"), Some((760, 480)));
        assert_eq!(parse_window_size("0x0"), Some((760, 480)));
        assert_eq!(parse_window_size("1120"), None);
        assert_eq!(parse_window_size("abcx720"), None);
    }

    #[test]
    fn collapsed_groups_roundtrip() {
        let mut groups = BTreeSet::new();
        groups.insert("/".to_string());
        groups.insert("/A/B".to_string());
        let text = collapsed_groups_text(&groups);
        assert_eq!(parse_collapsed_groups(&text), groups);
        assert_eq!(parse_collapsed_groups("  /A \n\n/B\n").len(), 2);
        assert!(parse_collapsed_groups("").is_empty());
    }

    #[test]
    fn group_state_keys_are_unique() {
        assert_eq!(group_state_key(""), "/");
        assert_eq!(group_state_key("A"), "/A");
        assert_eq!(group_state_key("A/B"), "/A/B");
        assert_ne!(group_state_key(""), group_state_key("A"));
    }

    #[test]
    fn light_theme_text_is_dark_enough() {
        let ctx = egui::Context::default();
        apply_theme(&ctx, ThemeChoice::Light);
        let style = ctx.style_of(egui::Theme::Light);
        let text = style.visuals.text_color();
        assert!(
            text.r() < 64 && text.g() < 64 && text.b() < 64,
            "texto do tema claro ainda claro: {text:?}"
        );
        assert!(style.visuals.weak_text_color().r() < 96);
        assert!(style.visuals.widgets.noninteractive.fg_stroke.color.r() < 64);
    }

    #[test]
    fn theme_choice_roundtrips_file_keys() {
        for theme in [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark] {
            assert_eq!(ThemeChoice::from_key(theme.key()), Some(theme));
        }
        assert_eq!(ThemeChoice::from_key("nada"), None);
    }

    fn session_named(name: &str) -> Session {
        Session {
            name: name.to_string(),
            protocol: Protocol::Ssh,
            host: "10.0.0.1".into(),
            port: 22,
            user: None,
            emulation: "xterm-256color".into(),
            auth: None,
            description: None,
            baud: None,
        }
    }

    #[test]
    fn password_lines_accept_two_or_three_fields() {
        assert_eq!(
            parse_password_line("10.0.0.1;segredo"),
            Some(("10.0.0.1".into(), None, "segredo".into()))
        );
        assert_eq!(
            parse_password_line("10.0.0.1; admin ; senha"),
            Some(("10.0.0.1".into(), Some("admin".into()), "senha".into()))
        );
        assert_eq!(
            parse_password_line("10.0.0.2,senha"),
            Some(("10.0.0.2".into(), None, "senha".into()))
        );
        assert_eq!(parse_password_line("# comentário"), None);
        assert_eq!(parse_password_line(""), None);
        assert_eq!(parse_password_line("só-um-campo"), None);
    }

    #[test]
    fn log_name_is_sanitized() {
        assert_eq!(
            log_name(&session_named("Meu Servidor / prod"), "20260101-101010"),
            "Meu_Servidor___prod-20260101-101010.log"
        );
        assert_eq!(log_name(&session_named("///"), "x"), "___-x.log");
    }

    #[test]
    fn log_receives_raw_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.log");
        let mut log = TabLog {
            path: path.clone(),
            file: std::fs::File::create(&path).unwrap(),
        };
        log.write_chunk(b"linha 1\r\n").unwrap();
        log.write_chunk(b"linha 2\r\n").unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "linha 1\r\nlinha 2\r\n");
    }

    #[test]
    fn selection_spans_scrollback() {
        let mut term = sshc_term::Terminal::new(10, 2);
        term.feed(b"a\r\nb\r\nc");
        assert_eq!(term.screen().history_len(), 1);
        let sel = Selection {
            anchor: (0, 0),
            cursor: (2, 0),
        };
        assert_eq!(selection_text(term.screen(), &sel), "a\nb\nc");
    }

    #[test]
    fn select_all_covers_history_and_screen() {
        let mut term = sshc_term::Terminal::new(10, 2);
        term.feed(b"a\r\nb\r\nc");
        let screen = term.screen();
        let sel = Selection {
            anchor: (0, 0),
            cursor: (screen.total_rows() - 1, screen.cols() - 1),
        };
        assert_eq!(selection_text(screen, &sel), "a\nb\nc");
    }

    #[test]
    fn form_requires_name_and_host() {
        let mut form = FormState::new();
        assert!(form.build().is_err());
        form.name = "web".into();
        assert!(form.build().is_err());
        form.host = "10.0.0.1".into();
        let session = form.build().unwrap();
        assert_eq!(session.port, 22);
        assert_eq!(session.name, "web");
    }

    #[test]
    fn form_rejects_bad_port() {
        let mut form = FormState::new();
        form.name = "x".into();
        form.host = "h".into();
        form.port = "abc".into();
        assert!(form.build().unwrap_err().contains("porta"));
        form.port = "0".into();
        assert!(form.build().is_err());
    }

    #[test]
    fn form_serial_keeps_baud_and_no_port() {
        let mut form = FormState::new();
        form.name = "console".into();
        form.protocol = Protocol::Serial;
        form.host = "/dev/tty.usbserial".into();
        form.baud = "115200".into();
        let session = form.build().unwrap();
        assert_eq!(session.port, 0);
        assert_eq!(session.baud, Some(115200));
    }

    #[test]
    fn quick_connect_uses_host_as_name() {
        let mut form = FormState::quick(None);
        form.host = "10.9.9.9".into();
        form.protocol = Protocol::Telnet;
        form.port = "23".into();
        let session = form.build().unwrap();
        assert_eq!(session.name, "10.9.9.9");
        assert_eq!(session.protocol, Protocol::Telnet);
    }

    #[test]
    fn edit_prefills_and_duplicate_renames() {
        let e = entry("Lab", "r1", "192.168.0.1");
        let edit = FormState::from_entry(&e, FormMode::Edit);
        assert_eq!(edit.name, "r1");
        assert_eq!(edit.editing.as_ref(), Some(&e.path));

        let dup = FormState::from_entry(&e, FormMode::New);
        assert_eq!(dup.name, "r1 (cópia)");
        assert!(dup.editing.is_none());
    }

    /// Sessão de teste de auth da GUI (o Keychain só é tocado ao conectar).
    fn session_sem_keychain() -> Session {
        session_named("auth-gui")
    }

    #[test]
    fn auth_target_routes_to_sftp_when_tab_id_is_set() {
        let mut auth = SshAuth::new(session_sem_keychain());
        auth.sftp_tab = Some(7);
        assert_eq!(auth_target(&auth), AuthTarget::Sftp(7));
        auth.sftp_tab = None;
        assert_eq!(auth_target(&auth), AuthTarget::Terminal);
    }

    #[test]
    fn sftp_tab_title_shows_active_badge() {
        assert_eq!(sftp_tab_title("web", 0), "SFTP · web");
        assert_eq!(sftp_tab_title("web", 3), "SFTP · web (3)");
    }

    #[test]
    fn max_sessions_error_gets_a_friendly_hint() {
        let message = friendly_sftp_error(
            "abrindo canal SFTP: Failed to open channel (AdministrativelyProhibited)",
        );
        assert!(message.contains("MaxSessions"), "{message}");
        assert!(message.contains("sessões simultâneas"), "{message}");
        assert_eq!(friendly_sftp_error("tempo esgotado"), "tempo esgotado");
    }
}
