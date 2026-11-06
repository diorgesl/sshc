//! Cliente SSH (russh). Roda num runtime tokio próprio, numa thread; o resto
//! do app continua síncrono e conversa por canais.

use super::{Connection, Credentials, Input};
use anyhow::{bail, Context, Result};
use russh::client::{self, Handler};
use russh::keys::known_hosts::learn_known_hosts_path;
use russh::keys::{
    check_known_hosts_path, load_secret_key, HashAlg, PrivateKeyWithHashAlg,
    PublicKeyOrCertificate,
};
use russh::{ChannelMsg, Disconnect};
use sshc_core::Session;
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
    let user = session
        .user
        .clone()
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
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(e) => {
                let _ = ready_tx.send(Err(format!("runtime tokio: {e}")));
                closed_thread.store(true, Ordering::Relaxed);
                return;
            }
        };
        let result = runtime.block_on(run(
            host, port, user, term, creds, cols, rows, in_rx, out_tx, ready_tx, stop_thread,
        ));
        if let Err(e) = result {
            eprintln!("ssh: {e:#}");
        }
        closed_thread.store(true, Ordering::Relaxed);
    });

    match ready_rx.recv_timeout(Duration::from_secs(25)) {
        Ok(Ok(info)) => Ok(Connection::new(out_rx, in_tx, stop, closed, vec![handle], info)),
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

    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(3600)),
        ..Default::default()
    });
    let learned = Arc::new(Mutex::new(None));
    let handler = HostKeyCheck {
        host: host.clone(),
        port,
        known_hosts: known_hosts_path(),
        learned: learned.clone(),
    };

    let mut handle = match client::connect(config, (host.as_str(), port), handler).await {
        Ok(handle) => handle,
        Err(e) => {
            fail(format!("conectando em {host}:{port}: {e}"));
            return Ok(());
        }
    };

    let authenticated = match creds {
        Credentials::Password(password) => match handle.authenticate_password(user.clone(), password).await {
            Ok(result) => result.success(),
            Err(e) => {
                fail(format!("autenticação: {e}"));
                return Ok(());
            }
        },
        Credentials::Key { path, passphrase } => {
            let key = match load_secret_key(&path, passphrase.as_deref()) {
                Ok(key) => key,
                Err(e) => {
                    fail(format!("lendo chave {}: {e}", path.display()));
                    return Ok(());
                }
            };
            let hash = handle
                .best_supported_rsa_hash()
                .await
                .ok()
                .flatten()
                .flatten();
            match handle
                .authenticate_publickey(
                    user.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await
            {
                Ok(result) => result.success(),
                Err(e) => {
                    fail(format!("autenticação por chave: {e}"));
                    return Ok(());
                }
            }
        }
    };
    if !authenticated {
        fail("autenticação recusada pelo servidor".into());
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

    let _ = handle
        .disconnect(Disconnect::ByApplication, "", "en")
        .await;
    Ok(())
}
