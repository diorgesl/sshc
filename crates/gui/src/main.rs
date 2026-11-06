use anyhow::Result;
use eframe::egui;
use sshc_core::{cloud, securecrt, Session, SessionEntry, Store};
use std::path::{Path, PathBuf};

const ACCENT: egui::Color32 = egui::Color32::from_rgb(96, 165, 250);

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

struct App {
    store: Option<Store>,
    entries: Vec<SessionEntry>,
    selected: Option<PathBuf>,
    filter: String,
    import_open: bool,
    import_path: String,
    message: Option<String>,
    error: Option<String>,
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
            smoke_frames: std::env::var("SSHC_GUI_SMOKE").ok().map(|_| 180),
        };
        app.reload();
        if app.smoke_frames.is_some() {
            app.selected = app.entries.first().map(|e| e.path.clone());
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
        if let Some(left) = self.smoke_frames.as_mut() {
            *left = left.saturating_sub(1);
            if std::env::var_os("SSHC_GUI_DEBUG").is_some() && *left % 10 == 0 {
                eprintln!("[smoke] frames_left={left}");
            }
            ui.ctx().request_repaint();
            if *left == 45 {
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
                                egui::RichText::new("use Importar SecureCRT… ou o comando sshc import")
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
        egui::CentralPanel::default_margins().show(ui, |ui| match selected {
            Some((session, group)) => terminal_view(ui, &session, &group),
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

        if self.import_open {
            let mut open = true;
            let mut requested_import = false;
            egui::Window::new("Importar sessões do SecureCRT")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label("Pasta Sessions do SecureCRT:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.import_path)
                            .hint_text("/caminho/para/Sessions do SecureCRT")
                            .desired_width(460.0),
                    );
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

fn terminal_view(ui: &mut egui::Ui, session: &Session, group: &str) {
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
            ui.add_enabled(false, egui::Button::new("Conectar"))
                .on_disabled_hover_text("transporte SSH ainda não implementado");
        });
    });
    ui.add_space(8.0);

    egui::Frame::NONE
        .fill(egui::Color32::from_rgb(12, 14, 18))
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.set_min_height((ui.available_height() - 8.0).max(120.0));
            for line in [
                format!("sshc 0.1 — {}, {}", session.endpoint(), session.emulation),
                String::new(),
                "Terminal ainda não ligado: o transporte (russh) e o emulador".into(),
                "(vte/alacritty_terminal) são a próxima etapa.".into(),
                String::new(),
                "Sessão carregada da store local; senha nunca sai do Keychain.".into(),
            ] {
                ui.label(
                    egui::RichText::new(line)
                        .monospace()
                        .color(egui::Color32::from_rgb(200, 210, 220)),
                );
            }
        });
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
