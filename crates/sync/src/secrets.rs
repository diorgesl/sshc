//! Onde vive o refresh token: **no Keychain do SO**, nunca num arquivo do
//! projeto nem na pasta sincronizada.

use anyhow::{bail, Context, Result};

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use std::process::Command;

    const SERVICE: &str = "sshc-google-oauth";
    const ACCOUNT: &str = "refresh-token";

    pub fn save(token: &str) -> Result<()> {
        let status = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-a",
                ACCOUNT,
                "-s",
                SERVICE,
                "-w",
                token,
            ])
            .status()
            .context("executando 'security'")?;
        if !status.success() {
            bail!("'security' falhou ao gravar no Keychain");
        }
        Ok(())
    }

    pub fn load() -> Result<Option<String>> {
        let out = Command::new("security")
            .args(["find-generic-password", "-a", ACCOUNT, "-s", SERVICE, "-w"])
            .output()
            .context("executando 'security'")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    }

    pub fn delete() -> Result<()> {
        let _ = Command::new("security")
            .args(["delete-generic-password", "-a", ACCOUNT, "-s", SERVICE])
            .output();
        Ok(())
    }
}

// ponytail: só macOS por enquanto; Windows/Linux usam credential store nativo
// quando alguém pedir. Antes disso, falha explícita em vez de token em claro.
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;

    pub fn save(_: &str) -> Result<()> {
        bail!("armazenamento seguro de token ainda só no macOS")
    }
    pub fn load() -> Result<Option<String>> {
        Ok(None)
    }
    pub fn delete() -> Result<()> {
        Ok(())
    }
}

pub fn save(token: &str) -> Result<()> {
    platform::save(token)
}

pub fn load() -> Result<Option<String>> {
    platform::load()
}

pub fn delete() -> Result<()> {
    platform::delete()
}
