use anyhow::Result;
use eframe::egui;
use sshc_core::{cloud, securecrt, Protocol, Session, SessionEntry, Store};
use sshc_net::{Connection, Credentials};
use sshc_term::{Color, Terminal};
use std::io::Write as _;
use std::path::{Path, PathBuf};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(96, 165, 250);
const TERM_BG: egui::Color32 = egui::Color32::from_rgb(12, 14, 18);
const ERROR: egui::Color32 = egui::Color32::from_rgb(248, 113, 113);

fn main() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 720.0])
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
    Disconnect,
    Reconnect,
    Close,
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
            name,
            protocol: s.protocol.clone(),
            host: s.host.clone(),
            port: if s.port == 0 { String::new() } else { s.port.to_string() },
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
            form.port = if s.port == 0 { String::new() } else { s.port.to_string() };
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

/// Uma aba: sessão conectada (ou com a conexão caída) e seu terminal.
struct Tab {
    id: u64,
    session: Session,
    path: Option<PathBuf>,
    term: Terminal,
    conn: Option<Connection>,
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
    if let Ok(out) = std::process::Command::new("date").arg("+%Y%m%d-%H%M%S").output() {
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
    let safe = if safe.is_empty() { "sessao".to_string() } else { safe };
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
    Ok(TabLog { path: path.to_path_buf(), file })
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
        let end = if row == er { ec.min(cols.saturating_sub(1)) } else { cols - 1 };
        let mut line = String::new();
        let cells = screen.absolute_row(row);
        for col in start..=end.min(cells.len().saturating_sub(1)) {
            line.push(cells[col].ch);
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
    user: String,
    mode: AuthMode,
    password: String,
    key_path: String,
    passphrase: String,
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
            mode: AuthMode::Auto,
            password: String::new(),
            key_path,
            passphrase: String::new(),
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

struct App {
    store: Option<Store>,
    entries: Vec<SessionEntry>,
    selected: Option<PathBuf>,
    filter: String,
    import_open: bool,
    import_path: String,
    form: Option<FormState>,
    auth: Option<SshAuth>,
    confirm_delete: Option<(PathBuf, String)>,
    message: Option<String>,
    error: Option<String>,
    tabs: Vec<Tab>,
    active: Option<u64>,
    next_tab_id: u64,
    pending: Vec<PendingConnect>,
    focus_terminal: bool,
    last_log_dir: Option<PathBuf>,
    reload_requested: bool,
    smoke_frames: Option<u32>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            store: None,
            entries: Vec::new(),
            selected: None,
            filter: String::new(),
            import_open: false,
            import_path: String::new(),
            form: None,
            auth: None,
            confirm_delete: None,
            message: None,
            error: None,
            tabs: Vec::new(),
            active: None,
            next_tab_id: 1,
            pending: Vec::new(),
            focus_terminal: false,
            last_log_dir: None,
            reload_requested: false,
            smoke_frames: std::env::var("SSHC_GUI_SMOKE").ok().map(|_| 120),
        };
        app.reload();
        if app.smoke_frames.is_some() {
            app.selected = app.entries.first().map(|e| e.path.clone());
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
                            tab.log = Some(log);
                        }
                    }
                }
                // seleção/busca de exemplo, só para a captura
                if let Some(tab) = app.tabs.last_mut() {
                    match std::env::var("SSHC_GUI_SMOKE_SEARCH") {
                        Ok(query) => tab.search = Some(Search { query, ..Default::default() }),
                        Err(_) => {
                            let screen = tab.term.screen();
                            let top = screen.total_rows().saturating_sub(screen.rows());
                            let cols = screen.cols();
                            tab.sel = Some(Selection {
                                anchor: (top, 0),
                                cursor: (top, 28.min(cols.saturating_sub(1))),
                            });
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
                self.error = None;
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    fn selected_session(&self) -> Option<(Session, String, PathBuf)> {
        let selected = self.selected.as_ref()?;
        self.entries
            .iter()
            .find(|e| &e.path == selected)
            .map(|e| (e.session.clone(), e.group.clone(), e.path.clone()))
    }

    /// Acha a aba de uma sessão (por caminho, ou nome+endpoint para quick connect).
    fn tab_index(&self, path: &Option<PathBuf>, session: &Session) -> Option<usize> {
        match path {
            Some(path) => self.tabs.iter().position(|t| t.path.as_deref() == Some(path.as_path())),
            None => self.tabs.iter().position(|t| {
                t.path.is_none()
                    && t.session.name == session.name
                    && t.session.endpoint() == session.endpoint()
            }),
        }
    }

    fn close_tab(&mut self, id: u64) {
        let Some(index) = self.tabs.iter().position(|t| t.id == id) else { return };
        self.tabs.remove(index);
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
            if self.tabs[index].conn.is_some() {
                return;
            }
        }

        if !matches!(session.protocol, Protocol::Ssh) {
            self.start_connect(session, path, None, None);
            return;
        }

        if let Some(password) = std::env::var("SSHC_SSH_PASSWORD")
            .ok()
            .filter(|p| !p.is_empty())
        {
            self.start_connect(session, path, Some(Credentials::Password(password)), None);
            return;
        }

        self.start_connect(session, path, Some(Credentials::Auto), None);
    }

    /// Abre a conexão numa thread; o resultado chega no `pump`.
    fn start_connect(
        &mut self,
        session: Session,
        path: Option<PathBuf>,
        creds: Option<Credentials>,
        auth: Option<SshAuth>,
    ) {
        let (tx, rx) = std::sync::mpsc::channel();
        let target = session.clone();
        std::thread::spawn(move || {
            let result = sshc_net::connect(&target, creds).map_err(|e| format!("{e:#}"));
            let _ = tx.send(result);
        });
        self.message = Some(format!("conectando em {}…", session.endpoint()));
        self.error = None;
        self.pending.push(PendingConnect { session, path, rx, auth });
    }

    /// Coloca uma conexão já aberta na aba existente (reconexão) ou numa nova.
    fn finish_connect(&mut self, session: &Session, path: Option<PathBuf>, conn: Connection) {
        let note = conn.note.clone();
        if let Some(index) = self.tab_index(&path, session) {
            self.tabs[index].conn = Some(conn);
            self.active = Some(self.tabs[index].id);
        } else {
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            self.tabs.push(Tab {
                id,
                session: session.clone(),
                path,
                term: Terminal::new(100, 30),
                conn: Some(conn),
                sel: None,
                search: None,
                log: None,
                notice: None,
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

    /// Drena todos os sockets e atualiza as telas.
    fn pump(&mut self, ctx: &egui::Context) {
        // conexões abertas em thread
        let outcome = self.pending.iter().enumerate().find_map(|(index, pending)| {
            match pending.rx.try_recv() {
                Ok(result) => Some((index, result)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    Some((index, Err("conexão abortada".to_string())))
                }
            }
        });
        if let Some((index, result)) = outcome {
            let pending = self.pending.remove(index);
            match result {
                Ok(conn) => self.finish_connect(&pending.session, pending.path, conn),
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
        } else if !self.pending.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        let mut any_live = false;
        for tab in &mut self.tabs {
            let mut log_error = None;
            if let Some(conn) = &tab.conn {
                while let Ok(chunk) = conn.output.try_recv() {
                    tab.term.feed(&chunk);
                    if let Some(log) = &mut tab.log {
                        if let Err(e) = log.write_chunk(&chunk) {
                            log_error = Some(format!("{e}"));
                        }
                    }
                    ctx.request_repaint();
                }
            }
            if let Some(error) = log_error {
                tab.log = None;
                tab.notice = Some(format!("log interrompido: {error}"));
            }
            if tab.conn.as_ref().is_some_and(|conn| conn.is_closed()) {
                tab.conn = None;
                self.message = Some(format!("conexão encerrada: {}", tab.session.name));
            }
            if tab.conn.is_some() {
                any_live = true;
            }
        }
        if any_live {
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
                self.selected = Some(path);
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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.pump(ui.ctx());

        // Cmd+F / Ctrl+F: busca no terminal da aba ativa
        if ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            if let Some(index) = self
                .active
                .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
            {
                if self.tabs[index].search.is_none() {
                    self.tabs[index].search = Some(Search::default());
                }
            }
        }

        if self.reload_requested {
            self.reload_requested = false;
            self.reload();
        }

        if let Some(left) = self.smoke_frames.as_mut() {
            *left = left.saturating_sub(1);
            ui.ctx().request_repaint();
            if *left == 60 {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(
                    egui::UserData::default(),
                ));
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
                            if ui.button("Atualizar").clicked() {
                                self.reload_requested = true;
                            }
                            if ui
                                .button(egui::RichText::new("+ Nova sessão").strong())
                                .clicked()
                            {
                                self.form = Some(FormState::new());
                            }
                            if ui.button("Quick connect").clicked() {
                                self.form = Some(FormState::quick(self.selected_session()));
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
                                        let title = if tab.conn.is_some() {
                                            tab.session.name.clone()
                                        } else {
                                            format!("{} ·off", tab.session.name)
                                        };
                                        if ui
                                            .selectable_label(
                                                is_active,
                                                egui::RichText::new(title),
                                            )
                                            .clicked()
                                        {
                                            activate = Some(tab.id);
                                        }
                                        if ui.small_button("×").clicked() {
                                            close = Some(tab.id);
                                        }
                                    }
                                });
                            });
                    });
            });
            if let Some(id) = activate {
                self.active = Some(id);
                self.focus_terminal = true;
            }
            if let Some(id) = close {
                self.close_tab(id);
            }
        }

        // Duplo clique conecta depois do painel (evita conflito de empréstimo).
        let mut pending_connect: Option<(Session, PathBuf)> = None;
        let mut connect_clicked = false;
        egui::Panel::left("sessions")
            .resizable(true)
            .default_size(300.0)
            .show(ui, |ui| {
                egui::Frame::NONE
                    .inner_margin(egui::Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.filter)
                                .hint_text("filtrar por nome, host ou pasta")
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(4.0);
                        if ui.button("Conectar sessão selecionada").clicked() {
                            connect_clicked = true;
                        }
                    });
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 3.0;
                        ui.add_space(2.0);
                        let groups = grouped(&self.entries, &self.filter);
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
                            egui::CollapsingHeader::new(format!("{title}  ·  {}", list.len()))
                                .default_open(true)
                                .show(ui, |ui| {
                                    for entry in list {
                                        let s = &entry.session;
                                        let selected = self.selected.as_ref() == Some(&entry.path);
                                        let label = match &s.user {
                                            Some(user) => format!("{}  @{}", s.name, user),
                                            None => s.name.clone(),
                                        };
                                        let resp = ui
                                            .selectable_label(selected, egui::RichText::new(label))
                                            .on_hover_text(format!(
                                                "{}\n{} · duplo clique conecta · botão direito edita",
                                                s.endpoint(),
                                                protocol_label(&s.protocol)
                                            ));
                                        if resp.clicked() {
                                            self.selected = Some(entry.path.clone());
                                            self.focus_terminal = true;
                                        }
                                        if resp.double_clicked() {
                                            self.selected = Some(entry.path.clone());
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
                                            if ui.button("Excluir").clicked() {
                                                self.confirm_delete = Some((
                                                    entry.path.clone(),
                                                    s.name.clone(),
                                                ));
                                                ui.close();
                                            }
                                        });
                                    }
                                });
                        }
                    });
            });

        if connect_clicked {
            self.connect_selected();
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
        let focus = &mut self.focus_terminal;
        let last_log_dir = &mut self.last_log_dir;
        let tabs = &mut self.tabs;
        egui::CentralPanel::default_margins().show(ui, |ui| {
            match active_index.and_then(|index| tabs.get_mut(index)) {
                Some(tab) => action = terminal_view(ui, tab, focus, last_log_dir),
                None => {
                    ui.centered_and_justified(|ui| {
                        ui.label(
                            egui::RichText::new(
                                "selecione uma sessão — duplo clique conecta",
                            )
                            .weak()
                            .size(16.0),
                        );
                    });
                }
            }
        });

        match action {
            Action::Disconnect => {
                if let Some(tab) = active_index.and_then(|index| self.tabs.get_mut(index)) {
                    tab.conn = None;
                    self.message = Some(format!("desconectado: {}", tab.session.name));
                }
            }
            Action::Reconnect => {
                if let Some(tab) = active_index.and_then(|index| self.tabs.get(index)) {
                    let session = tab.session.clone();
                    let path = tab.path.clone();
                    self.request_connect(session, path);
                }
            }
            Action::Close => {
                if let Some(tab) = active_index.and_then(|index| self.tabs.get(index)) {
                    self.close_tab(tab.id);
                }
            }
            Action::None => {}
        }

        self.show_form(ui.ctx());
        self.show_auth(ui.ctx());
        self.show_import(ui.ctx());
        self.show_delete_confirm(ui.ctx());
    }
}

impl App {
    fn show_form(&mut self, ctx: &egui::Context) {
        let Some(mut form) = self.form.take() else { return };
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
                            ui.add(
                                egui::TextEdit::singleline(&mut form.group)
                                    .hint_text("(raiz)")
                                    .desired_width(300.0),
                            );
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
                                .hint_text(if serial { "/dev/tty.usbserial" } else { "10.0.0.1" })
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
                        if ui.button(egui::RichText::new("Conectar").strong()).clicked() {
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
        let Some(mut auth) = self.auth.take() else { return };
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
                            }
                        }
                    });

                if let Some(err) = &auth.error {
                    ui.add_space(4.0);
                    ui.colored_label(ERROR, err);
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button(egui::RichText::new("Conectar").strong()).clicked() {
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
                    self.start_connect(session, path, Some(creds), Some(auth));
                    return;
                }
                Err(msg) => auth.error = Some(msg),
            }
        }
        if open && !cancel {
            self.auth = Some(auth);
        }
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
        let Some((path, name)) = self.confirm_delete.clone() else { return };
        let mut open = true;
        let mut confirmed = false;
        let mut cancel = false;
        egui::Window::new("Excluir sessão")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .show(ctx, |ui| {
                ui.label(format!("Excluir “{name}”?"));
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
            if let Some(store) = &self.store {
                match store.delete(&path) {
                    Ok(()) => {
                        self.message = Some(format!("sessão '{name}' excluída"));
                        if self.selected.as_ref() == Some(&path) {
                            self.selected = None;
                        }
                        self.reload_requested = true;
                    }
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
            self.confirm_delete = None;
            return;
        }
        if open && !cancel {
            self.confirm_delete = Some((path, name));
        } else {
            self.confirm_delete = None;
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
        if tab.conn.is_none() {
            ui.label(egui::RichText::new("desconectado").color(ERROR).small());
        }
        if tab.term.screen().scroll_offset() > 0 {
            let offset = tab.term.screen().scroll_offset();
            if ui
                .button(egui::RichText::new(format!("↑ {offset} linhas")).small())
                .on_hover_text("voltar ao fim")
                .clicked()
            {
                tab.term.screen_mut().scroll_reset();
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if tab.conn.is_some() {
                if ui.button("Desconectar").clicked() {
                    action = Action::Disconnect;
                }
                let label = if tab.log.is_some() {
                    "● gravando log"
                } else {
                    "Gravar log"
                };
                let response = ui.button(label);
                if response.clicked() {
                    if tab.log.is_some() {
                        tab.log = None;
                        tab.notice = Some("log encerrado".into());
                    } else {
                        match start_log_dialog(&tab.session, last_log_dir) {
                            Ok(Some(log)) => {
                                tab.notice = Some(format!("log: {}", log.path.display()));
                                tab.log = Some(log);
                            }
                            Ok(None) => {}
                            Err(e) => tab.notice = Some(format!("log falhou: {e:#}")),
                        }
                    }
                }
                if let Some(notice) = &tab.notice {
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

    if tab.search.is_some() {
        let mut search_action = SearchAction::None;
        if let Some(search) = tab.search.as_mut() {
            search_action = search_bar(ui, search, &mut tab.term);
        }
        match search_action {
            SearchAction::None => {}
            SearchAction::Close => tab.search = None,
            SearchAction::Step(forward) => {
                if let Some(search) = tab.search.as_mut() {
                    search.step(forward, tab.term.screen_mut());
                }
            }
        }
    }

    {
        let live = tab;
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
            let top_row = |live: &Tab| {
                live.term
                    .screen()
                    .total_rows()
                    .saturating_sub(rows)
                    .saturating_sub(live.term.screen().scroll_offset())
            };
            let cell_at = |live: &Tab, pos: egui::Pos2| -> (usize, usize) {
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
                    live.sel = Some(Selection { anchor: at, cursor: at });
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
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
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
            egui::Event::Key { key, pressed: true, modifiers, .. } => {
                matches!(key, egui::Key::C) && modifiers.command
            }
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
            let end = if abs_row == er { ec.min(cols - 1) } else { cols - 1 };
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
                let scale = |v: u8| -> u8 { if v == 0 { 0 } else { 55 + v * 40 } };
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
    fn selection_range_normalizes_reversed() {
        let sel = Selection { anchor: (2, 5), cursor: (0, 1) };
        assert_eq!(sel.range(), ((0, 1), (2, 5)));
        let same = Selection { anchor: (1, 1), cursor: (1, 1) };
        assert_eq!(same.range(), ((1, 1), (1, 1)));
    }

    #[test]
    fn selection_text_extracts_and_trims() {
        let mut term = sshc_term::Terminal::new(10, 3);
        term.feed(b"abc\r\ndef");
        let sel = Selection { anchor: (0, 0), cursor: (1, 1) };
        assert_eq!(selection_text(term.screen(), &sel), "abc\nde");
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
        let mut log = TabLog { path: path.clone(), file: std::fs::File::create(&path).unwrap() };
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
        let sel = Selection { anchor: (0, 0), cursor: (2, 0) };
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
}
