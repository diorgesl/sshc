use crate::session::Session;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Sessão carregada da store, com a pasta (grupo) onde ela vive.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub group: String,
    pub session: Session,
    pub path: PathBuf,
}

/// Diretório de sessões: `<root>/<grupo>/<nome>.toml`.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).with_context(|| format!("criando {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn load_all(&self) -> Result<Vec<SessionEntry>> {
        let mut out = Vec::new();
        for entry in WalkDir::new(&self.root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let text = fs::read_to_string(path)
                .with_context(|| format!("lendo {}", path.display()))?;
            let session: Session = toml::from_str(&text)
                .with_context(|| format!("parseando {}", path.display()))?;
            let group = path
                .parent()
                .and_then(|dir| dir.strip_prefix(&self.root).ok())
                .map(|dir| dir.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(SessionEntry { group, session, path: path.to_path_buf() });
        }
        out.sort_by(|a, b| {
            (a.group.as_str(), a.session.name.as_str())
                .cmp(&(b.group.as_str(), b.session.name.as_str()))
        });
        Ok(out)
    }

    pub fn save(&self, group: &str, session: &Session) -> Result<PathBuf> {
        let dir = self.group_dir(group);
        fs::create_dir_all(&dir).with_context(|| format!("criando {}", dir.display()))?;
        let path = dir.join(format!("{}.toml", file_stem(&session.name)));
        let body = toml::to_string_pretty(session)?;
        atomic_write(&path, body.as_bytes())?;
        Ok(path)
    }

    pub fn group_dir(&self, group: &str) -> PathBuf {
        if group.is_empty() {
            self.root.clone()
        } else {
            self.root.join(group)
        }
    }
}

/// Nome de arquivo seguro: mantém letras/acentos, troca separadores por `_`.
fn file_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.');
    if cleaned.is_empty() {
        "sessao".to_string()
    } else {
        cleaned.to_string()
    }
}

/// Escreve em arquivo temporário e renomeia: evita que o sync da nuvem veja
/// um .toml pela metade.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, bytes).with_context(|| format!("escrevendo {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renomeando {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Protocol;

    fn sample() -> Session {
        Session {
            name: "db-01".into(),
            protocol: Protocol::Ssh,
            host: "10.1.1.1".into(),
            port: 22,
            user: Some("admin".into()),
            emulation: "xterm-256color".into(),
            auth: None,
            description: None,
        }
    }

    #[test]
    fn save_then_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let path = store.save("Cliente A", &sample()).unwrap();
        assert!(path.ends_with("Cliente A/db-01.toml"));

        let all = store.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].group, "Cliente A");
        assert_eq!(all[0].session, sample());
    }

    #[test]
    fn unsafe_name_is_sanitized() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let mut s = sample();
        s.name = "a/b:c".into();
        let path = store.save("", &s).unwrap();
        assert_eq!(path.file_name().unwrap(), "a_b_c.toml");
    }
}
