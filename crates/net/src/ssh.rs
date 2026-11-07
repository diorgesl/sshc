//! Cliente SSH (russh). Roda num runtime tokio próprio, numa thread; o resto
//! do app continua síncrono e conversa por canais.

use super::{Connection, Credentials, Input};

/// Prefixo de erro de autenticação: a UI usa isso para oferecer a senha.
pub const AUTH_ERROR: &str = "auth: ";
/// Marca a chave que precisa de passphrase, com o caminho logo depois.
pub const PASSPHRASE_PREFIX: &str = "passphrase: ";

pub fn is_auth_error(message: &str) -> bool {
    message.contains(AUTH_ERROR)
}

/// Caminho da chave que exigiu passphrase, se o erro for esse.
pub fn encrypted_key_path(message: &str) -> Option<&str> {
    message
        .split(PASSPHRASE_PREFIX)
        .nth(1)
        .and_then(|rest| rest.split('\n').next())
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn key_load_error(path: &std::path::Path, error: &russh::keys::Error) -> String {
    if matches!(error, russh::keys::Error::KeyIsEncrypted) {
        format!("{AUTH_ERROR}{PASSPHRASE_PREFIX}{}", path.display())
    } else {
        format!("{AUTH_ERROR}lendo chave {}: {error}", path.display())
    }
}
use anyhow::{bail, Context, Result};
use russh::client::{self, Handler};
use russh::keys::known_hosts::learn_known_hosts_path;
use russh::keys::{
    check_known_hosts_path, load_secret_key, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate,
};
use russh::{ChannelMsg, Disconnect};
use sshc_core::Session;
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct HostKeyCheck {
    host: String,
    port: u16,
    known_hosts: PathBuf,
    learned: Arc<Mutex<Option<String>>>,
}

impl Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Ok(true);
        };
        let known = check_known_hosts_path(&self.host, self.port, key, &self.known_hosts);
        if let Ok(true) = known {
            return Ok(true);
        }
        // TOFU: aprende e avisa a impressão digital.
        let _ = learn_known_hosts_path(&self.host, self.port, key, &self.known_hosts);
        if let Ok(mut slot) = self.learned.lock() {
            *slot = Some(key.fingerprint(HashAlg::Sha256).to_string());
        }
        Ok(true)
    }
}

fn known_hosts_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SSHC_KNOWN_HOSTS") {
        return PathBuf::from(path);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".ssh/known_hosts")
}

pub fn connect(session: &Session, creds: Credentials, cols: u32, rows: u32) -> Result<Connection> {
    let user = resolve_user(session.user.clone(), &session.host)
        .context("sessão SSH sem usuário — edite a sessão e informe o usuário")?;
    let host = session.host.clone();
    let port = session.port;
    let term = session.emulation.clone();
    let display_host = host.clone();

    let (out_tx, out_rx) = channel::<Vec<u8>>();
    let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel::<Input>();
    let (ready_tx, ready_rx) = channel::<std::result::Result<String, String>>();
    let closed = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let closed_thread = closed.clone();
    let stop_thread = stop.clone();
    let handle = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("runtime tokio: {e}")));
                closed_thread.store(true, Ordering::Relaxed);
                return;
            }
        };
        let result = runtime.block_on(run(
            host,
            port,
            user,
            term,
            creds,
            cols,
            rows,
            in_rx,
            out_tx,
            ready_tx,
            stop_thread,
        ));
        if let Err(e) = result {
            eprintln!("ssh: {e:#}");
        }
        closed_thread.store(true, Ordering::Relaxed);
    });

    match ready_rx.recv_timeout(Duration::from_secs(25)) {
        Ok(Ok(info)) => Ok(Connection::new(
            out_rx,
            in_tx,
            stop,
            closed,
            vec![handle],
            info,
        )),
        Ok(Err(message)) => {
            closed.store(true, Ordering::Relaxed);
            bail!("{message}")
        }
        Err(_) => {
            closed.store(true, Ordering::Relaxed);
            bail!("tempo esgotado conectando em {display_host}:{port}")
        }
    }
}

/// Tenta as credenciais na ordem em que o `ssh` faria.
async fn authenticate<H: Handler>(
    handle: &mut client::Handle<H>,
    user: &str,
    creds: Credentials,
    host: &str,
) -> std::result::Result<bool, String> {
    match creds {
        Credentials::Password(password) => match handle.authenticate_password(user, password).await
        {
            Ok(result) => Ok(result.success()),
            Err(e) => Err(format!("{AUTH_ERROR}senha: {e}")),
        },
        Credentials::Key { path, passphrase } => {
            authenticate_key(handle, user, &path, passphrase.as_deref()).await
        }
        Credentials::Auto => {
            // 1) agent (chaves carregadas com ssh-add)
            if let Some(accepted) = agent_auth(handle, user).await {
                if accepted {
                    return Ok(true);
                }
            }
            // 2) IdentityFile do ~/.ssh/config e chaves padrão
            let mut last_error = None;
            let mut encrypted = None;
            for path in key_candidates(host) {
                match authenticate_key(handle, user, &path, None).await {
                    Ok(true) => return Ok(true),
                    Ok(false) => {}
                    Err(e) => {
                        if encrypted_key_path(&e).is_some() && encrypted.is_none() {
                            encrypted = Some(e.clone());
                        }
                        last_error = Some(e);
                    }
                }
            }
            match (last_error, encrypted) {
                // chave protegida tem prioridade no aviso: dá pra resolver na UI
                (_, Some(e)) => Err(e),
                (Some(e), None) => Err(e),
                (None, None) => Ok(false),
            }
        }
    }
}

async fn authenticate_key<H: Handler>(
    handle: &mut client::Handle<H>,
    user: &str,
    path: &std::path::Path,
    passphrase: Option<&str>,
) -> std::result::Result<bool, String> {
    let key = match load_secret_key(path, passphrase) {
        Ok(key) => key,
        Err(e) => return Err(key_load_error(path, &e)),
    };
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();
    match handle
        .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
        .await
    {
        Ok(result) => Ok(result.success()),
        Err(e) => Err(format!("{AUTH_ERROR}chave {}: {e}", path.display())),
    }
}

/// Usa o ssh-agent quando existir (SSH_AUTH_SOCK).
#[cfg(unix)]
async fn agent_auth<H: Handler>(handle: &mut client::Handle<H>, user: &str) -> Option<bool> {
    use russh::keys::agent::client::AgentClient;

    std::env::var_os("SSH_AUTH_SOCK")?;
    let mut agent = AgentClient::connect_env().await.ok()?;
    let identities = agent.request_identities().await.ok()?;
    for identity in identities {
        let key = identity.public_key().into_owned();
        let hash = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();
        if let Ok(result) = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
        {
            if result.success() {
                return Some(true);
            }
        }
    }
    Some(false)
}

#[cfg(not(unix))]
async fn agent_auth<H: Handler>(_handle: &mut client::Handle<H>, _user: &str) -> Option<bool> {
    None
}

/// Chaves que o `ssh` tentaria: IdentityFile do config + nomes padrão.
fn key_candidates(host: &str) -> Vec<PathBuf> {
    key_candidates_in(home_dir(), host)
}

fn key_candidates_in(home: Option<PathBuf>, host: &str) -> Vec<PathBuf> {
    let mut out = ssh_config_for(host).identities;
    if let Some(home) = home {
        for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
            let path = home.join(".ssh").join(name);
            if path.exists() && !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// Usuário na mesma ordem do `ssh`: sessão, ~/.ssh/config, usuário local.
fn resolve_user(session_user: Option<String>, host: &str) -> Option<String> {
    session_user
        .or_else(|| ssh_config_for(host).user)
        .or_else(|| std::env::var("USER").ok())
        .filter(|user| !user.trim().is_empty())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SshConfig {
    pub user: Option<String>,
    pub identities: Vec<PathBuf>,
}

/// Lê o ~/.ssh/config para o host: primeiro valor de User ganha; todos os
/// IdentityFile de blocos que casam entram na lista.
pub fn ssh_config_for(host: &str) -> SshConfig {
    let Some(home) = home_dir() else {
        return SshConfig::default();
    };
    let path = home.join(".ssh/config");
    parse_ssh_config(&path, host).unwrap_or_default()
}

fn parse_ssh_config(path: &std::path::Path, host: &str) -> Option<SshConfig> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut config = SshConfig::default();
    let mut matches = false;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(keyword) = parts.next() else {
            continue;
        };
        match keyword.to_lowercase().as_str() {
            "host" => {
                matches = parts.any(|pattern| glob_match(pattern, host));
            }
            "match" => matches = false,
            "user" if matches && config.user.is_none() => {
                config.user = parts.next().map(|u| u.to_string());
            }
            "identityfile" if matches => {
                if let Some(value) = parts.next() {
                    let expanded = expand_tilde(value);
                    if !config.identities.contains(&expanded) {
                        config.identities.push(expanded);
                    }
                }
            }
            _ => {}
        }
    }
    Some(config)
}

fn expand_tilde(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/") {
        if let Some(home) = home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(value)
}

/// Globbing simples de `Host` (`*` e `?`).
fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let value: Vec<char> = value.to_lowercase().chars().collect();
    let (mut p, mut v) = (0usize, 0usize);
    let (mut star, mut resume) = (None::<usize>, 0usize);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            resume = v;
            p += 1;
        } else if let Some(s) = star {
            p = s + 1;
            resume += 1;
            v = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

fn is_negotiation_error(error: &russh::Error) -> bool {
    matches!(error, russh::Error::NoCommonAlgo { .. })
}

/// Configuração do cliente. `legacy` é o segundo round para equipamentos que
/// só oferecem SHA-1: os algoritmos modernos continuam na frente e os legados
/// só entram no fim da lista (servidor moderno nunca negocia um deles).
fn client_config(legacy: bool) -> Arc<client::Config> {
    let mut preferred = russh::Preferred::DEFAULT;
    if legacy {
        let mut kexes = preferred.kex.to_vec();
        for name in [
            russh::kex::DH_G14_SHA1,
            russh::kex::DH_GEX_SHA1,
            russh::kex::DH_G1_SHA1,
        ] {
            if !kexes.contains(&name) {
                kexes.push(name);
            }
        }
        preferred.kex = Cow::Owned(kexes);

        let mut ciphers = preferred.cipher.to_vec();
        for name in [
            russh::cipher::AES_128_CBC,
            russh::cipher::AES_256_CBC,
            russh::cipher::AES_192_CBC,
            russh::cipher::TRIPLE_DES_CBC,
        ] {
            if !ciphers.contains(&name) {
                ciphers.push(name);
            }
        }
        preferred.cipher = Cow::Owned(ciphers);

        let mut macs = preferred.mac.to_vec();
        for name in [russh::mac::HMAC_SHA1, russh::mac::HMAC_SHA1_ETM] {
            if !macs.contains(&name) {
                macs.push(name);
            }
        }
        preferred.mac = Cow::Owned(macs);
    }
    Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(3600)),
        preferred,
        ..Default::default()
    })
}

fn host_key_handler(host: &str, port: u16, learned: Arc<Mutex<Option<String>>>) -> HostKeyCheck {
    HostKeyCheck {
        host: host.to_string(),
        port,
        known_hosts: known_hosts_path(),
        learned,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    host: String,
    port: u16,
    user: String,
    term: String,
    creds: Credentials,
    cols: u32,
    rows: u32,
    mut input: tokio::sync::mpsc::UnboundedReceiver<Input>,
    output: Sender<Vec<u8>>,
    ready: Sender<std::result::Result<String, String>>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let fail = |message: String| {
        let _ = ready.send(Err(message));
    };

    let learned = Arc::new(Mutex::new(None));
    let mut legacy = false;
    let mut handle = match client::connect(
        client_config(false),
        (host.as_str(), port),
        host_key_handler(&host, port, learned.clone()),
    )
    .await
    {
        Ok(handle) => handle,
        Err(first_error) if is_negotiation_error(&first_error) => {
            // Equipamento antigo (OLT/switch): segundo round só com o que falta.
            match client::connect(
                client_config(true),
                (host.as_str(), port),
                host_key_handler(&host, port, learned.clone()),
            )
            .await
            {
                Ok(handle) => {
                    legacy = true;
                    handle
                }
                Err(error) => {
                    fail(format!(
                        "conectando em {host}:{port}: {error}\n(compatibilidade legada também não resolveu: servidor sem algoritmo em comum)"
                    ));
                    return Ok(());
                }
            }
        }
        Err(error) => {
            fail(format!("conectando em {host}:{port}: {error}"));
            return Ok(());
        }
    };

    let authenticated = match authenticate(&mut handle, &user, creds, &host).await {
        Ok(true) => true,
        Ok(false) => {
            fail(format!(
                "{AUTH_ERROR}o servidor recusou as credenciais disponíveis"
            ));
            return Ok(());
        }
        Err(message) => {
            fail(message);
            return Ok(());
        }
    };
    if !authenticated {
        return Ok(());
    }

    let channel = match handle.channel_open_session().await {
        Ok(channel) => channel,
        Err(e) => {
            fail(format!("abrindo canal: {e}"));
            return Ok(());
        }
    };
    channel
        .request_pty(false, &term, cols, rows, 0, 0, &[])
        .await
        .ok();
    if let Err(e) = channel.request_shell(true).await {
        fail(format!("abrindo shell: {e}"));
        return Ok(());
    }

    let note = learned
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .map(|fingerprint| format!("host key aprendida ({fingerprint})"))
        .unwrap_or_default();
    let note = if legacy {
        if note.is_empty() {
            "compatibilidade legada (SHA-1/CBC)".to_string()
        } else {
            format!("compatibilidade legada (SHA-1/CBC); {note}")
        }
    } else {
        note
    };
    let _ = ready.send(Ok(note));

    let (mut reader, writer) = channel.split();
    loop {
        tokio::select! {
            message = reader.wait() => match message {
                Some(ChannelMsg::Data { data }) => {
                    if output.send(data.to_vec()).is_err() {
                        break;
                    }
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    let _ = output.send(data.to_vec());
                }
                Some(ChannelMsg::Eof) | Some(ChannelMsg::Close) | None => break,
                _ => {}
            },
            item = input.recv() => match item {
                Some(Input::Data(bytes)) => {
                    if writer.data(bytes.as_slice()).await.is_err() {
                        break;
                    }
                }
                Some(Input::Resize(cols, rows)) => {
                    let _ = writer.window_change(cols as u32, rows as u32, 0, 0).await;
                }
                None => break,
            },
        }
        if stop.load(Ordering::Relaxed) {
            break;
        }
    }

    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypted_key_is_reported_with_its_path() {
        let path = std::path::Path::new("/home/u/.ssh/id_ed25519");
        let message = key_load_error(path, &russh::keys::Error::KeyIsEncrypted);
        assert!(is_auth_error(&message));
        assert_eq!(
            encrypted_key_path(&message),
            Some("/home/u/.ssh/id_ed25519")
        );

        let other = key_load_error(path, &russh::keys::Error::KeyIsCorrupt);
        assert!(is_auth_error(&other));
        assert_eq!(encrypted_key_path(&other), None);
    }

    #[test]
    fn glob_matches_like_ssh_host_patterns() {
        assert!(glob_match("*", "qualquer"));
        assert!(glob_match("prod-*", "prod-web"));
        assert!(!glob_match("prod-*", "homolog-web"));
        assert!(glob_match("10.0.0.?", "10.0.0.9"));
        assert!(glob_match("*web*", "meu-web-01"));
        assert!(!glob_match("web", "web01"));
    }

    #[test]
    fn ssh_config_reads_user_and_identity_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "Host prod-*\n\
    User deploy\n\
    IdentityFile /tmp/chave_prod\n\
\n\
Host outro\n\
    User alguem\n\
\n\
Host prod-web\n\
    User outro_deploy\n",
        )
        .unwrap();

        let config = parse_ssh_config(&path, "prod-web").unwrap();
        assert_eq!(config.user.as_deref(), Some("deploy"));
        assert_eq!(config.identities, vec![PathBuf::from("/tmp/chave_prod")]);

        let other = parse_ssh_config(&path, "10.0.0.9").unwrap();
        assert_eq!(other, SshConfig::default());
    }

    #[test]
    fn key_candidates_prefer_existing_default_keys() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("id_ed25519"), "x").unwrap();
        std::fs::write(ssh.join("id_rsa"), "x").unwrap();
        // id_ecdsa não existe: não deve aparecer
        let keys = key_candidates_in(Some(dir.path().to_path_buf()), "alvo");
        assert_eq!(
            keys,
            vec![ssh.join("id_ed25519"), ssh.join("id_rsa")],
            "ordem e existência das chaves"
        );
    }

    #[test]
    fn tilde_is_expanded() {
        if let Some(home) = home_dir() {
            assert_eq!(expand_tilde("~/x"), home.join("x"));
        }
        assert_eq!(expand_tilde("/abs/x"), PathBuf::from("/abs/x"));
    }

    #[test]
    fn legacy_config_appends_sha1_after_modern_algorithms() {
        let modern = client_config(false);
        assert!(!modern.preferred.kex.contains(&russh::kex::DH_G14_SHA1));
        assert!(!modern
            .preferred
            .cipher
            .contains(&russh::cipher::AES_128_CBC));
        assert!(!modern.preferred.mac.contains(&russh::mac::HMAC_SHA1));

        let legacy = client_config(true);
        // modernos continuam na frente: servidor novo nunca escolhe legado
        assert_eq!(legacy.preferred.kex.first(), modern.preferred.kex.first());
        assert!(legacy.preferred.kex.contains(&russh::kex::DH_G14_SHA1));
        assert!(legacy.preferred.kex.contains(&russh::kex::DH_GEX_SHA1));
        assert!(legacy.preferred.kex.contains(&russh::kex::DH_G1_SHA1));
        assert!(legacy
            .preferred
            .cipher
            .contains(&russh::cipher::AES_128_CBC));
        assert!(legacy
            .preferred
            .cipher
            .contains(&russh::cipher::AES_256_CBC));
        assert!(legacy
            .preferred
            .cipher
            .contains(&russh::cipher::AES_192_CBC));
        assert!(legacy
            .preferred
            .cipher
            .contains(&russh::cipher::TRIPLE_DES_CBC));
        assert!(legacy.preferred.mac.contains(&russh::mac::HMAC_SHA1));
        assert!(legacy.preferred.mac.contains(&russh::mac::HMAC_SHA1_ETM));
    }
}
