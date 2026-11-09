//! Aba do explorador SFTP.
//!
//! Nesta etapa a aba só abre a conexão, mostra o estado e roteia a
//! autenticação. Os painéis de arquivos entram na Task 10.

use eframe::egui;
use sshc_net::sftp::SftpConnection;
use sshc_net::Credentials;

const ERROR: egui::Color32 = egui::Color32::from_rgb(248, 113, 113);
const OK: egui::Color32 = egui::Color32::from_rgb(74, 222, 128);

/// Estado da conexão SFTP de uma aba.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connecting,
    Connected,
    Closed(String),
    Failed(String),
}

/// O que a view pede ao App; o App aplica depois do painel central.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SftpAction {
    None,
    Reconnect,
    Close,
    NeedAuth,
}

/// Estado de uma aba SFTP. A sessão em si fica no `Tab` do main.rs.
pub struct SftpTab {
    pub conn: Option<SftpConnection>,
    /// Credenciais usadas na conexão (ficam só na memória, para reconectar).
    pub creds: Option<Credentials>,
    pub state: ConnState,
    /// Aviso do handshake (host key aprendida / compatibilidade legada).
    pub note: Option<String>,
    /// Caminho inicial remoto resolvido no connect.
    pub home: Option<String>,
}

impl SftpTab {
    pub fn connecting(creds: Option<Credentials>) -> Self {
        Self {
            conn: None,
            creds,
            state: ConnState::Connecting,
            note: None,
            home: None,
        }
    }
}

/// Cabeçalho mínimo da aba: estado, nota do handshake e ações.
pub fn sftp_view(ui: &mut egui::Ui, tab: &mut SftpTab) -> SftpAction {
    let mut action = SftpAction::None;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        match &tab.state {
            ConnState::Connecting => {
                ui.spinner();
                ui.label("conectando…");
            }
            ConnState::Connected => {
                ui.label(egui::RichText::new("conectado").color(OK).strong());
                if let Some(home) = &tab.home {
                    ui.label(egui::RichText::new(home).weak().small());
                }
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
        });
    });

    if matches!(tab.state, ConnState::Closed(_) | ConnState::Failed(_)) {
        ui.horizontal(|ui| {
            if ui.button("Reconectar").clicked() {
                action = SftpAction::Reconnect;
            }
            if matches!(tab.state, ConnState::Failed(_)) && ui.button("Informar senha…").clicked()
            {
                action = SftpAction::NeedAuth;
            }
        });
    }

    if let Some(note) = &tab.note {
        if !note.trim().is_empty() {
            ui.label(egui::RichText::new(note).weak().small());
        }
    }
    ui.separator();
    action
}
