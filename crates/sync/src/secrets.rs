//! Onde vivem os segredos: **no Keychain do SO**, nunca num arquivo do projeto
//! nem na pasta sincronizada. Hoje: refresh token do Google e token do bot do
//! Telegram (JSON com token + chat).

use anyhow::Result;

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use anyhow::{bail, Context};
    use std::process::Command;

    pub fn save(service: &str, account: &str, secret: &str) -> Result<()> {
        let status = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-a",
                account,
                "-s",
                service,
                "-w",
                secret,
            ])
            .status()
            .context("executando 'security'")?;
        if !status.success() {
            bail!("'security' falhou ao gravar no Keychain");
        }
        Ok(())
    }

    pub fn load(service: &str, account: &str) -> Result<Option<String>> {
        let out = Command::new("security")
            .args(["find-generic-password", "-a", account, "-s", service, "-w"])
            .output()
            .context("executando 'security'")?;
        if !out.status.success() {
            return Ok(None);
        }
        Ok(Some(
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
        ))
    }

    pub fn delete(service: &str, account: &str) -> Result<()> {
        let _ = Command::new("security")
            .args(["delete-generic-password", "-a", account, "-s", service])
            .output();
        Ok(())
    }
}

// ponytail: só macOS por enquanto; Windows/Linux usam credential store nativo
// quando alguém pedir. Antes disso, falha explícita em vez de token em claro.
#[cfg(not(target_os = "macos"))]
mod platform {
    use super::*;
    use anyhow::bail;

    pub fn save(_service: &str, _account: &str, _secret: &str) -> Result<()> {
        bail!("armazenamento seguro de token ainda só no macOS")
    }
    pub fn load(_service: &str, _account: &str) -> Result<Option<String>> {
        Ok(None)
    }
    pub fn delete(_service: &str, _account: &str) -> Result<()> {
        Ok(())
    }
}

const GOOGLE_SERVICE: &str = "sshc-google-oauth";
const GOOGLE_ACCOUNT: &str = "refresh-token";
const TELEGRAM_SERVICE: &str = "sshc-telegram";
const TELEGRAM_ACCOUNT: &str = "bot";

pub fn save(token: &str) -> Result<()> {
    platform::save(GOOGLE_SERVICE, GOOGLE_ACCOUNT, token)
}

pub fn load() -> Result<Option<String>> {
    platform::load(GOOGLE_SERVICE, GOOGLE_ACCOUNT)
}

pub fn delete() -> Result<()> {
    platform::delete(GOOGLE_SERVICE, GOOGLE_ACCOUNT)
}

/// Config do Telegram (JSON com token e chat), criada por `telegram::login`.
pub fn save_telegram(config: &str) -> Result<()> {
    platform::save(TELEGRAM_SERVICE, TELEGRAM_ACCOUNT, config)
}

pub fn load_telegram() -> Result<Option<String>> {
    platform::load(TELEGRAM_SERVICE, TELEGRAM_ACCOUNT)
}

pub fn delete_telegram() -> Result<()> {
    platform::delete(TELEGRAM_SERVICE, TELEGRAM_ACCOUNT)
}
