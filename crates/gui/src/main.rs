use anyhow::Result;
use eframe::egui;
use sshc_core::{cloud, securecrt, Session, SessionEntry, Store};
use sshc_net::Connection;
use sshc_term::{Color, Terminal};
use std::path::{Path, PathBuf};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(96, 165, 250);
const TERM_BG: egui::Color32 = egui::Color32::from_rgb(12, 14, 18);

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

struct Live {
    term: Terminal,
    conn: Connection,
}

struct App {
    store: Option<Store>,
    entries: Vec<SessionEntry>,
    selected: Option<PathBuf>,
    filter: String,
    import_open: bool,
    import_path: String,
    message: Option<String>,
    error: Option<String>,
    live: Option<Live>,
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
            message: None,
            error: None,
            live: None,
            smoke_frames: std::env::var("SSHC_GUI_SMOKE").ok().map(|_| 120),
        };
        app.reload();
        if app.smoke_frames.is_some() {
            app.selected = app.entries.first().map(|e| e.path.clone());
            app.connect_selected();
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
        let Some((session, _)) = self.selected_session() else {
            self.error = Some("selecione uma sessão".into());
            return;
        };
        match sshc_net::connect(&session) {
            Ok(conn) => {
                self.live = Some(Live { term: Terminal::new(100, 30), conn });
                self.error = None;
                self.message = Some(format!("conectado em {}", session.endpoint()));
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
                        ui.label(egui::RichText::new("cliente SSH multi-sessão").weak());
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
                                self.reload();
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
                                    "use Importar SecureCRT… ou o comando sshc import",
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
                                                "{}\n{}",
                                                s.endpoint(),
                                                protocol_label(&s.protocol)
                                            ));
                                        if resp.clicked() {
                                            self.selected = Some(entry.path.clone());
                                        }
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
                        ui.colored_label(egui::Color32::from_rgb(248, 113, 113), err);
                    } else if let Some(msg) = &self.message {
                        ui.label(egui::RichText::new(msg).weak().small());
                    }
                });
            });
            ui.add_space(3.0);
        });

        let selected = self.selected_session();
        let mut action = Action::None;
        egui::CentralPanel::default_margins().show(ui, |ui| match selected {
            Some((session, group)) => {
                action = terminal_view(ui, &session, &group, self.live.as_mut());
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

        if self.import_open {
            let mut open = true;
            let mut requested_import = false;
            egui::Window::new("Importar sessões do SecureCRT")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
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
    }
}

fn terminal_view(
    ui: &mut egui::Ui,
    session: &Session,
    group: &str,
    live: Option<&mut Live>,
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
            paint_terminal(ui, &mut live.term, rect);

            let response = ui.interact(rect, ui.id().with("terminal"), egui::Sense::click());
            if response.clicked() {
                response.request_focus();
            }
            if response.has_focus() {
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
                        "Telnet e serial já funcionam; SSH é a próxima etapa.".into(),
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
            egui::Event::Key { key, pressed: true, .. } => {
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

fn paint_terminal(ui: &mut egui::Ui, term: &mut Terminal, rect: egui::Rect) {
    let font = egui::FontId::monospace(13.0);
    let (cell_w, cell_h) = ui.ctx().fonts_mut(|f| {
        (
            f.glyph_width(&font, 'M').max(1.0),
            f.row_height(&font).max(1.0),
        )
    });

    let cols = ((rect.width() / cell_w).floor() as usize).max(20);
    let rows = ((rect.height() / cell_h).floor() as usize).max(5);
    let (have_cols, have_rows) = term.screen().size();
    if have_cols != cols || have_rows != rows {
        term.resize(cols, rows);
    }

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

fn protocol_label(p: &sshc_core::Protocol) -> &'static str {
    match p {
        sshc_core::Protocol::Ssh => "ssh",
        sshc_core::Protocol::Telnet => "telnet",
        sshc_core::Protocol::Serial => "serial",
        sshc_core::Protocol::LocalShell => "local",
        sshc_core::Protocol::Other(_) => "outro",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sshc_core::Protocol;

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
}
