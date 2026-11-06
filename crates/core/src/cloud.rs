//! Descoberta de pastas sincronizadas. Regra: não falamos com API nenhuma.
//! O cliente da nuvem (iCloud/OneDrive/Google Drive/Dropbox) já sincroniza a
//! pasta; nós só escrevemos arquivos nela.

use std::env;
use std::path::PathBuf;

/// Nome da pasta dentro do provider. Marca provisória.
pub const APP_DIR: &str = "ssh-client";

fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Providers conhecidos, com a pasta raiz que o cliente de sync mantém.
pub fn provider_roots() -> Vec<(&'static str, PathBuf)> {
    let Some(home) = home() else { return Vec::new() };
    let mut out = Vec::new();

    // macOS
    out.push((
        "icloud",
        home.join("Library/Mobile Documents/com~apple~CloudDocs"),
    ));
    for (prefix, name) in [
        ("GoogleDrive-", "google-drive"),
        ("OneDrive-", "onedrive"),
    ] {
        let base = home.join("Library/CloudStorage");
        for dir in subdirs_with_prefix(&base, prefix) {
            let root = if name == "google-drive" {
                dir.join("My Drive")
            } else {
                dir
            };
            out.push((name, root));
        }
    }
    out.push(("dropbox", home.join("Dropbox")));

    // Windows / Linux
    if let Some(dir) = env::var_os("OneDrive") {
        out.push(("onedrive", PathBuf::from(dir)));
    }
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        let _ = dir; // placeholder: sem provider adicional hoje
    }
    out.push(("google-drive", home.join("Google Drive")));

    out
}

fn subdirs_with_prefix(base: &std::path::Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(base) else { return Vec::new() };
    read.filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .map(|e| e.path())
        .collect()
}

/// Raiz da store: `SSHC_SESSIONS` se definido, senão o primeiro provider
/// existente, senão `~/.sshc/sessions`.
pub fn store_root() -> PathBuf {
    if let Some(dir) = env::var_os("SSHC_SESSIONS") {
        return PathBuf::from(dir);
    }
    if let Some((_, root)) = provider_roots().into_iter().find(|(_, p)| p.is_dir()) {
        return root.join(APP_DIR);
    }
    home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sshc/sessions")
}

/// Providers detectados neste computador (para a UI mostrar/escolher).
pub fn detected() -> Vec<(&'static str, PathBuf)> {
    provider_roots()
        .into_iter()
        .filter(|(_, p)| p.is_dir())
        .map(|(name, p)| (name, p.join(APP_DIR)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        // 2021 edition: set_var é seguro.
        env::set_var("SSHC_SESSIONS", "/tmp/sshc-test-root");
        assert_eq!(store_root(), PathBuf::from("/tmp/sshc-test-root"));
        env::remove_var("SSHC_SESSIONS");
    }
}
