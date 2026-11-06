use anyhow::Result;
use eframe::egui;
use sshc_core::{cloud, securecrt, Protocol, Session, SessionEntry, Store};
use sshc_net::{Connection, Credentials};
use sshc_term::{Color, Terminal};
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
    Connect,
    Disconnect,
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

    fn quick(selected: Option<(Session, String)>) -> Self {
        let mut form = Self::new();
        form.mode = FormMode::Quick;
        if let Some((s, _)) = selected {
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

struct Live {
    term: Terminal,
    conn: Connection,
}

/// Credenciais para sessões SSH (nunca salvas em disco).
struct SshAuth {
    session: Session,
    user: String,
    password: String,
    key_path: String,
    use_key: bool,
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
            password: String::new(),
            key_path,
            use_key: true,
            error: None,
        }
    }

    fn credentials(&self) -> std::result::Result<Credentials, String> {
        if self.use_key {
            if self.key_path.trim().is_empty() {
                return Err("informe o caminho da chave".into());
            }
            Ok(Credentials::Key {
                path: PathBuf::from(self.key_path.trim()),
                passphrase: None,
            })
        } else {
            if self.password.is_empty() {
                return Err("informe a senha".into());
            }
            Ok(Credentials::Password(self.password.clone()))
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
    live: Option<Live>,
    focus_terminal: bool,
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
            live: None,
            focus_terminal: false,
            reload_requested: false,
            smoke_frames: std::env::var("SSHC_GUI_SMOKE").ok().map(|_| 120),
        };
        app.reload();
        if app.smoke_frames.is_some() {
            app.selected = app.entries.first().map(|e| e.path.clone());
            if std::env::var_os("SSHC_GUI_SMOKE_FORM").is_some() {
                app.form = Some(FormState::new());
            } else {
                app.connect_selected();
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

    fn selected_session(&self) -> Option<(Session, String)> {
        let selected = self.selected.as_ref()?;
        self.entries
            .iter()
            .find(|e| &e.path == selected)
            .map(|e| (e.session.clone(), e.group.clone()))
    }

    fn connect_selected(&mut self) {
        match self.selected_session() {
            Some((session, _)) => self.request_connect(session),
            None => self.error = Some("selecione uma sessão".into()),
        }
    }

    /// SSH pede credenciais antes; telnet/serial conectam direto.
    fn request_connect(&mut self, session: Session) {
        if matches!(session.protocol, Protocol::Ssh) {
            if let Some(password) = std::env::var("SSHC_SSH_PASSWORD").ok().filter(|p| !p.is_empty()) {
                self.connect_session(session, Some(Credentials::Password(password)));
                return;
            }
            self.auth = Some(SshAuth::new(session));
        } else {
            self.connect_session(session, None);
        }
    }

    fn connect_session(&mut self, session: Session, creds: Option<Credentials>) {
        match sshc_net::connect(&session, creds) {
            Ok(conn) => {
                let note = conn.note.clone();
                self.live = Some(Live { term: Terminal::new(100, 30), conn });
                self.focus_terminal = true;
                self.error = None;
                self.message = Some(if note.is_empty() {
                    format!("conectado em {}", session.endpoint())
                } else {
                    format!("conectado em {} — {note}", session.endpoint())
                });
            }
            Err(e) => self.error = Some(format!("{e:#}")),
        }
    }

    /// Drena o socket e atualiza a tela.
    fn pump(&mut self, ctx: &egui::Context) {
        let Some(live) = &mut self.live else { return };
        while let Ok(chunk) = live.conn.output.try_recv() {
            live.term.feed(&chunk);
            ctx.request_repaint();
        }
        if live.conn.is_closed() {
            self.live = None;
            self.message = Some("conexão encerrada".into());
            return;
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
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
                                                "{}\n{} — clique direito para editar",
                                                s.endpoint(),
                                                protocol_label(&s.protocol)
                                            ));
                                        if resp.clicked() {
                                            self.selected = Some(entry.path.clone());
                                            self.focus_terminal = true;
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

        let selected = self.selected_session();
        let mut action = Action::None;
        let focus = &mut self.focus_terminal;
        egui::CentralPanel::default_margins().show(ui, |ui| match selected {
            Some((session, group)) => {
                action = terminal_view(ui, &session, &group, self.live.as_mut(), focus);
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new("selecione uma sessão à esquerda")
                            .weak()
                            .size(16.0),
                    );
                });
            }
        });
        match action {
            Action::Connect => self.connect_selected(),
            Action::Disconnect => {
                self.live = None;
                self.message = Some("desconectado".into());
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
                    self.request_connect(session);
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
                            ui.selectable_value(&mut auth.use_key, true, "Chave");
                            ui.selectable_value(&mut auth.use_key, false, "Senha");
                        });
                        ui.end_row();

                        if auth.use_key {
                            ui.label("Chave privada");
                            ui.add(
                                egui::TextEdit::singleline(&mut auth.key_path)
                                    .desired_width(320.0),
                            );
                            ui.end_row();
                        } else {
                            ui.label("Senha");
                            ui.add(
                                egui::TextEdit::singleline(&mut auth.password)
                                    .password(true)
                                    .desired_width(260.0),
                            );
                            ui.end_row();
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
                    self.connect_session(session, Some(creds));
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

fn terminal_view(
    ui: &mut egui::Ui,
    session: &Session,
    group: &str,
    live: Option<&mut Live>,
    focus: &mut bool,
) -> Action {
    let mut action = Action::None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.heading(&session.name);
        ui.label(
            egui::RichText::new(format!(
                "{} · {}",
                session.endpoint(),
                protocol_label(&session.protocol)
            ))
            .weak(),
        );
        if !group.is_empty() {
            ui.label(egui::RichText::new(format!("[{group}]")).weak().small());
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if live.is_some() {
                if ui.button("Desconectar").clicked() {
                    action = Action::Disconnect;
                }
            } else if ui.button("Conectar").clicked() {
                action = Action::Connect;
            }
        });
    });
    ui.add_space(8.0);

    match live {
        Some(live) => {
            let rect = ui.available_rect_before_wrap();
            paint_terminal(ui, live, rect);

            let terminal_id = ui.id().with("terminal");
            let response = ui.interact(rect, terminal_id, egui::Sense::click());
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
                send_input(ui, &live.conn);
            }
        }
        None => {
            egui::Frame::NONE
                .fill(TERM_BG)
                .inner_margin(14.0)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    ui.set_min_height((ui.available_height() - 8.0).max(120.0));
                    for line in [
                        format!("sshc 0.1 — {}, {}", session.endpoint(), session.emulation),
                        String::new(),
                        "Clique em Conectar para abrir a sessão.".into(),
                        "SSH, Telnet e serial já conectam. Clique em Conectar.".into(),
                    ] {
                        ui.label(
                            egui::RichText::new(line)
                                .monospace()
                                .color(egui::Color32::from_rgb(200, 210, 220)),
                        );
                    }
                });
        }
    }
    action
}

fn send_input(ui: &egui::Ui, conn: &Connection) {
    let events = ui.input(|i| i.events.clone());
    for event in events {
        match event {
            egui::Event::Text(text) => {
                let _ = conn.send(text.as_bytes());
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

fn paint_terminal(ui: &mut egui::Ui, live: &mut Live, rect: egui::Rect) {
    let font = egui::FontId::monospace(13.0);
    let (cell_w, cell_h) = ui.ctx().fonts_mut(|f| {
        (
            f.glyph_width(&font, 'M').max(1.0),
            f.row_height(&font).max(1.0),
        )
    });

    let cols = ((rect.width() / cell_w).floor() as usize).max(20);
    let rows = ((rect.height() / cell_h).floor() as usize).max(5);
    let (have_cols, have_rows) = live.term.screen().size();
    if have_cols != cols || have_rows != rows {
        live.term.resize(cols, rows);
        live.conn.resize(cols as u16, rows as u16);
    }
    let term = &mut live.term;

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, TERM_BG);
    let cells = term.screen().cells();
    let (cols, rows) = term.screen().size();

    for y in 0..rows {
        let mut x = 0;
        while x < cols {
            let cell = cells[y * cols + x];
            let mut run = String::new();
            let mut run_len = 0;
            while x + run_len < cols {
                let next = cells[y * cols + x + run_len];
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

    let (cx, cy) = term.screen().cursor();
    if cx < cols && cy < rows {
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
