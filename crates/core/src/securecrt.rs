//! Importador do formato de sessão do SecureCRT (linhas `S:"Chave"=valor`,
//! `D:"Chave"=numero`, `Z:"Chave"=texto`, `B:"Chave"=hex`).
//!
//! Cobre o que interessa para reconectar: host, porta, protocolo, usuário,
//! emulação e métodos de autenticação. O resto do arquivo é ignorado.

use crate::session::{Protocol, Session};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Default)]
pub struct Ini {
    values: HashMap<(char, String), String>,
}

impl Ini {
    pub fn parse(text: &str) -> Self {
        let mut ini = Self::default();
        for line in text.lines() {
            let line = line.trim_end_matches('\r');
            let Some((kind, rest)) = line.split_once(':') else { continue };
            let Some(kind) = kind.chars().next() else { continue };
            let Some(rest) = rest.trim_start().strip_prefix('"') else { continue };
            let Some(end) = rest.find('"') else { continue };
            let key = &rest[..end];
            let Some(value) = rest[end + 1..].strip_prefix('=') else { continue };
            ini.values.insert((kind, key.to_string()), value.to_string());
        }
        ini
    }

    pub fn s(&self, key: &str) -> Option<&str> {
        self.values.get(&('S', key.to_string())).map(String::as_str)
    }

    pub fn z(&self, key: &str) -> Option<&str> {
        self.values.get(&('Z', key.to_string())).map(String::as_str)
    }

    pub fn d(&self, key: &str) -> Option<u32> {
        self.values
            .get(&('D', key.to_string()))
            .and_then(|v| v.trim().parse().ok())
    }
}

/// Resultado de importar uma árvore de sessões.
#[derive(Debug, Default)]
pub struct ImportReport {
    pub entries: Vec<(PathBuf, Session)>,
    pub skipped: usize,
}

/// Importa todos os `.ini` sob `root` (a pasta `Sessions` do SecureCRT).
/// O grupo de cada sessão é a pasta relativa dentro de `root`.
pub fn import_dir(root: &Path) -> Result<ImportReport> {
    let mut report = ImportReport::default();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ini") {
            continue;
        }
        let text = fs::read_to_string(path).with_context(|| format!("lendo {}", path.display()))?;
        match session_from_text(&text, path) {
            Some(session) => report.entries.push((path.to_path_buf(), session)),
            None => report.skipped += 1,
        }
    }
    report.entries.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    Ok(report)
}

fn session_from_text(text: &str, path: &Path) -> Option<Session> {
    let ini = Ini::parse(text);
    let protocol_label = ini.s("Protocol Name")?.trim().to_string();
    let protocol = match protocol_label.as_str() {
        "SSH2" | "SSH1" => Protocol::Ssh,
        "Telnet" => Protocol::Telnet,
        "Serial" => Protocol::Serial,
        "Local Shell" => Protocol::LocalShell,
        other => Protocol::Other(other.to_string()),
    };
    let port = ini
        .d(&format!("[{protocol_label}] Port"))
        .or_else(|| ini.d("Port"))
        .unwrap_or_else(|| protocol.default_port().map(u32::from).unwrap_or(0));

    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sessao".to_string());

    Some(Session {
        name,
        protocol,
        host: ini.s("Hostname").unwrap_or_default().trim().to_string(),
        port: port as u16,
        user: ini.s("Username").map(|u| u.trim().to_string()).filter(|u| !u.is_empty()),
        emulation: ini
            .s("Emulation")
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| "xterm-256color".to_string()),
        auth: ini
            .s("SSH2 Authentications V2")
            .map(|a| a.trim().to_string())
            .filter(|a| !a.is_empty()),
        description: ini.z("Description").map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
        baud: ini.d("[Serial] Baud").or_else(|| ini.d("Baud")).map(|b| b as u32),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "S:\"Hostname\"=10.0.0.5\r\n\
S:\"Protocol Name\"=SSH2\r\n\
D:\"[SSH2] Port\"=2222\r\n\
S:\"Emulation\"=xterm\r\n\
S:\"Username\"=diorges\r\n\
S:\"SSH2 Authentications V2\"=Public Key,Password\r\n\
Z:\"Description\"=Servidor de teste\r\n\
D:\"Some Boolean\"=1\r\n";

    #[test]
    fn parses_ssh_session() {
        let s = session_from_text(SAMPLE, Path::new("/tmp/Sessions/Cliente A/web-01.ini")).unwrap();
        assert_eq!(s.name, "web-01");
        assert_eq!(s.protocol, Protocol::Ssh);
        assert_eq!(s.host, "10.0.0.5");
        assert_eq!(s.port, 2222);
        assert_eq!(s.user.as_deref(), Some("diorges"));
        assert_eq!(s.emulation, "xterm");
        assert_eq!(s.auth.as_deref(), Some("Public Key,Password"));
        assert_eq!(s.description.as_deref(), Some("Servidor de teste"));
    }

    #[test]
    fn telnet_falls_back_to_well_known_port() {
        let text = "S:\"Hostname\"=10.0.0.9\nS:\"Protocol Name\"=Telnet\n";
        let s = session_from_text(text, Path::new("/tmp/telnet.ini")).unwrap();
        assert_eq!(s.protocol, Protocol::Telnet);
        assert_eq!(s.port, 23);
    }

    #[test]
    fn files_without_protocol_are_skipped() {
        assert!(session_from_text("D:\"Foo\"=1\n", Path::new("/tmp/x.ini")).is_none());
    }
}
