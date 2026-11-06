//! Senhas por sessão no Keychain do macOS (item genérico via `security`).
//!
//! Chave do item: `host:porta:usuário`. Nunca sai da máquina.

use anyhow::{bail, Context, Result};

const SERVICE: &str = "sshc-session";

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::process::Command;

    pub fn save(key: &str, password: &str) -> Result<()> {
        let status = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-a",
                key,
                "-s",
                SERVICE,
                "-w",
                password,
            ])
            .status()
            .context("executando 'security'")?;
        if !status.success() {
            bail!("Keychain recusou gravar a senha");
        }
        Ok(())
    }

    pub fn load(key: &str) -> Result<Option<String>> {
        let out = Command::new("security")
            .args(["find-generic-password", "-a", key, "-s", SERVICE, "-w"])
            .output()
            .context("executando 'security'")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim_end().to_string(),
        ))
    }

    pub fn delete(key: &str) -> Result<()> {
        let _ = Command::new("security")
            .args(["delete-generic-password", "-a", key, "-s", SERVICE])
            .output();
        Ok(())
    }
}

// ponytail: só macOS; Windows usaria Credential Manager e Linux o Secret Service.
// Até lá, falha explícito em vez de gravar senha em claro.
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub fn save(_key: &str, _password: &str) -> Result<()> {
        bail!("Keychain ainda só no macOS")
    }
    pub fn load(_key: &str) -> Result<Option<String>> {
        Ok(None)
    }
    pub fn delete(_key: &str) -> Result<()> {
        Ok(())
    }
}

pub fn save(key: &str, password: &str) -> Result<()> {
    platform::save(key, password)
}

pub fn load(key: &str) -> Result<Option<String>> {
    platform::load(key)
}

#[allow(dead_code)]
pub fn delete(key: &str) -> Result<()> {
    platform::delete(key)
}
