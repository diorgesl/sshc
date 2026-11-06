//! Onde ficam as sessões **nesta máquina**. A nuvem é o Google Drive via
//! OAuth (crates/sync); esta pasta é a cópia de trabalho local, e o
//! `sshc sync` reconcilia as duas.

use std::env;
use std::path::PathBuf;

pub fn home() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Diretório de dados do app no SO (nunca a pasta do Drive/iCloud).
pub fn app_data_dir() -> PathBuf {
    let home = home().unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/sshc")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".sshc")
    }
}

/// Raiz da store: `SSHC_SESSIONS` se definido, senão o diretório do app.
pub fn store_root() -> PathBuf {
    if let Some(dir) = env::var_os("SSHC_SESSIONS") {
        return PathBuf::from(dir);
    }
    app_data_dir().join("sessions")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_override_wins() {
        env::set_var("SSHC_SESSIONS", "/tmp/sshc-test-root");
        assert_eq!(store_root(), PathBuf::from("/tmp/sshc-test-root"));
        env::remove_var("SSHC_SESSIONS");
    }

    #[test]
    fn default_is_inside_app_data() {
        env::remove_var("SSHC_SESSIONS");
        assert!(store_root().starts_with(app_data_dir()));
    }
}
