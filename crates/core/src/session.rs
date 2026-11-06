use serde::{Deserialize, Serialize};

/// Protocolo de transporte. `Other` preserva o rótulo original de importadores.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Ssh,
    Telnet,
    Serial,
    LocalShell,
    Other(String),
}

impl Protocol {
    pub fn default_port(&self) -> Option<u16> {
        match self {
            Protocol::Ssh => Some(22),
            Protocol::Telnet => Some(23),
            _ => None,
        }
    }
}

fn default_emulation() -> String {
    "xterm-256color".to_string()
}

/// Uma sessão de conexão. Sem segredos: senha/chave vivem no keychain do SO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub name: String,
    pub protocol: Protocol,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default = "default_emulation")]
    pub emulation: String,
    /// Métodos de autenticação declarados (ex.: "Public Key,Password").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Só para Serial: velocidade da porta.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baud: Option<u32>,
}

impl Session {
    pub fn endpoint(&self) -> String {
        match (&self.host, self.port) {
            (h, 0) => h.clone(),
            (h, p) => format!("{h}:{p}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_hides_zero_port() {
        let s = Session {
            name: "x".into(),
            protocol: Protocol::Serial,
            host: "/dev/tty.usbserial".into(),
            port: 0,
            user: None,
            emulation: default_emulation(),
            auth: None,
            description: None,
            baud: Some(9600),
        };
        assert_eq!(s.endpoint(), "/dev/tty.usbserial");
    }

    #[test]
    fn roundtrip_toml() {
        let s = Session {
            name: "web-01".into(),
            protocol: Protocol::Ssh,
            host: "10.0.0.1".into(),
            port: 22,
            user: Some("root".into()),
            emulation: default_emulation(),
            auth: Some("Public Key".into()),
            description: None,
            baud: None,
        };
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        assert_eq!(s, back);
    }
}
