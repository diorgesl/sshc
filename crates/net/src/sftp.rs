//! Ator SFTP: thread própria com runtime tokio, no mesmo padrão do terminal.
//! A UI envia comandos por canal e recebe eventos; nenhum I/O remoto bloqueia
//! a interface.

use super::ssh::open_authenticated;
use super::Credentials;
use anyhow::Result;
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileType;
use sshc_core::Session;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Tempo máximo para o handshake SSH, o subsistema e a sessão SFTP ficarem
/// prontos. É o mesmo timeout do terminal.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
/// Timeout de cada operação SFTP, em segundos (a API do russh-sftp usa segundos).
const OPERATION_TIMEOUT_SECS: u64 = 30;
/// O servidor precisa confirmar o subsistema antes de iniciar o protocolo SFTP.
const SUBSYSTEM_TIMEOUT: Duration = Duration::from_secs(10);
/// Intervalo de checagem da sessão SSH enquanto o ator está ocioso.
const CLOSED_POLL: Duration = Duration::from_millis(500);

/// Entrada de uma listagem remota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpEntry {
    pub name: String,
    /// Caminho absoluto POSIX.
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
    /// Segundos desde a época Unix.
    pub modified: Option<u32>,
    /// Bits de permissão (sem o tipo do arquivo).
    pub mode: Option<u32>,
    pub link_target: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug)]
pub enum SftpCmd {
    List { id: u64, path: String },
    Disconnect,
}

#[derive(Debug)]
pub enum SftpEvent {
    Listed {
        id: u64,
        path: String,
        result: std::result::Result<Vec<SftpEntry>, String>,
    },
    Closed {
        reason: String,
    },
}

/// Conexão SFTP em segundo plano. 'send' e 'is_closed' são seguros para a UI.
pub struct SftpConnection {
    commands: UnboundedSender<SftpCmd>,
    pub events: Receiver<SftpEvent>,
    closed: Arc<AtomicBool>,
    /// Aviso do handshake (host key aprendida / compatibilidade legada).
    pub note: String,
    /// Caminho inicial remoto, quando o servidor permitir resolver.
    pub home: Option<String>,
}

impl SftpConnection {
    pub fn send(&self, command: SftpCmd) -> bool {
        self.commands.send(command).is_ok()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl Drop for SftpConnection {
    fn drop(&mut self) {
        // A thread é destacada: não esperamos por ela para não travar a UI.
        let _ = self.commands.send(SftpCmd::Disconnect);
    }
}

type Ready = std::result::Result<(String, Option<String>), String>;

/// Abre a conexão SFTP e espera até o protocolo estar pronto.
pub fn connect(session: &Session, creds: Credentials) -> Result<SftpConnection> {
    let session = session.clone();
    let endpoint = format!("{}:{}", session.host, session.port);

    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel::<SftpCmd>();
    let (event_tx, event_rx) = channel::<SftpEvent>();
    let (ready_tx, ready_rx) = channel::<Ready>();
    let closed = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));

    let closed_thread = closed.clone();
    let stop_thread = stop.clone();
    thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = ready_tx.send(Err(format!("runtime tokio: {error}")));
                closed_thread.store(true, Ordering::Relaxed);
                return;
            }
        };
        if let Err(error) = runtime.block_on(run(
            session,
            creds,
            command_rx,
            event_tx,
            ready_tx,
            stop_thread,
        )) {
            eprintln!("sftp: {error:#}");
        }
        closed_thread.store(true, Ordering::Relaxed);
    });

    match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
        Ok(Ok((note, home))) => Ok(SftpConnection {
            commands: command_tx,
            events: event_rx,
            closed,
            note,
            home,
        }),
        Ok(Err(message)) => {
            closed.store(true, Ordering::Relaxed);
            anyhow::bail!("{message}")
        }
        Err(_) => {
            // Sinaliza para a thread encerrar assim que terminar o handshake.
            stop.store(true, Ordering::Relaxed);
            closed.store(true, Ordering::Relaxed);
            anyhow::bail!("tempo esgotado conectando em {endpoint}")
        }
    }
}

async fn run(
    session: Session,
    creds: Credentials,
    mut commands: UnboundedReceiver<SftpCmd>,
    events: Sender<SftpEvent>,
    ready: Sender<Ready>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    macro_rules! fail_ready {
        ($($arg:tt)*) => {{
            let _ = ready.send(Err(format!($($arg)*)));
            return Ok(());
        }};
    }

    let (handle, note) = match open_authenticated(&session, creds).await {
        Ok(opened) => opened,
        Err(error) => fail_ready!("{error:#}"),
    };
    if stop.load(Ordering::Relaxed) {
        fail_ready!("conexão cancelada");
    }

    let mut channel = match handle.channel_open_session().await {
        Ok(channel) => channel,
        Err(error) => fail_ready!("abrindo canal SFTP: {error}"),
    };
    if let Err(error) = channel.request_subsystem(true, "sftp").await {
        fail_ready!("pedindo subsistema SFTP: {error}");
    }
    // request_subsystem só envia o pedido. Esperar a confirmação permite
    // distinguir "servidor sem SFTP" de uma sessão que caiu.
    loop {
        match tokio::time::timeout(SUBSYSTEM_TIMEOUT, channel.wait()).await {
            Ok(Some(ChannelMsg::Success)) => break,
            Ok(Some(ChannelMsg::Failure)) | Ok(None) => fail_ready!("servidor não oferece SFTP"),
            Ok(Some(_)) => {}
            Err(_) => fail_ready!("servidor não respondeu ao pedido de SFTP"),
        }
    }

    let sftp = match SftpSession::new(channel.into_stream()).await {
        Ok(sftp) => sftp,
        Err(error) => fail_ready!("iniciando SFTP: {error}"),
    };
    sftp.set_timeout(OPERATION_TIMEOUT_SECS);

    let home = match sftp.canonicalize(".").await {
        Ok(home) => Some(home),
        Err(_) => sftp.expand_path("~").await.ok().flatten(),
    };

    if ready.send(Ok((note, home.clone()))).is_err() {
        // O chamador desistiu (timeout do connect).
        return Ok(());
    }

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            command = commands.recv() => match command {
                Some(SftpCmd::List { id, path }) => {
                    let (resolved, result) = list(&sftp, &path, home.as_deref()).await;
                    if handle.is_closed() {
                        let _ = events.send(SftpEvent::Closed {
                            reason: "conexão SSH caiu".to_string(),
                        });
                        break;
                    }
                    if events
                        .send(SftpEvent::Listed {
                            id,
                            path: resolved,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Some(SftpCmd::Disconnect) | None => break,
            },
            _ = tokio::time::sleep(CLOSED_POLL) => {
                if handle.is_closed() {
                    let _ = events.send(SftpEvent::Closed {
                        reason: "conexão SSH caiu".to_string(),
                    });
                    break;
                }
            }
        }
    }

    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "", "en")
        .await;
    Ok(())
}

async fn list(
    session: &SftpSession,
    requested: &str,
    home: Option<&str>,
) -> (String, std::result::Result<Vec<SftpEntry>, String>) {
    let Some(path) = resolve_remote(requested, home) else {
        return (
            requested.to_string(),
            Err(format!("caminho remoto inválido: {requested}")),
        );
    };
    let result = read_entries(session, &path).await;
    (path, result)
}

async fn read_entries(
    session: &SftpSession,
    path: &str,
) -> std::result::Result<Vec<SftpEntry>, String> {
    let dir = session
        .read_dir(path.to_string())
        .await
        .map_err(|error| format!("listando {path}: {error}"))?;

    let mut entries = Vec::new();
    for entry in dir {
        let name = entry.file_name();
        if name.is_empty() || name == "." || name == ".." {
            continue;
        }
        let entry_path = join_remote(path, &name);
        // O readdir já traz atributos, mas alguns servidores os omitem. O lstat
        // também identifica symlink sem seguir o alvo.
        let metadata = match session.symlink_metadata(entry_path.clone()).await {
            Ok(metadata) => metadata,
            Err(_) => entry.metadata(),
        };
        let kind = match metadata.file_type() {
            FileType::File => EntryKind::File,
            FileType::Dir => EntryKind::Dir,
            FileType::Symlink => EntryKind::Symlink,
            FileType::Other => EntryKind::Other,
        };
        entries.push(SftpEntry {
            name,
            path: entry_path,
            kind,
            size: metadata.len(),
            modified: metadata.mtime,
            mode: metadata.permissions.map(|mode| mode & 0o7777),
            link_target: None,
        });
    }
    Ok(entries)
}

/// Expande ~ e normaliza um caminho remoto. Devolve None para caminho
/// relativo, vazio ou com NUL.
fn resolve_remote(path: &str, home: Option<&str>) -> Option<String> {
    let expanded = if path == "~" {
        home?.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{}/{}", home?.trim_end_matches('/'), rest)
    } else {
        path.to_string()
    };
    normalize_remote(&expanded)
}

fn join_remote(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// Normaliza um caminho remoto POSIX absoluto: remove barras duplicadas e
/// finais, resolve . e .. sem subir acima de /. Caminhos relativos, vazios ou
/// com NUL são inválidos.
pub fn normalize_remote(path: &str) -> Option<String> {
    if path.is_empty() || path.contains('\0') || !path.starts_with('/') {
        return None;
    }

    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }

    let mut normalized = String::from("/");
    normalized.push_str(&parts.join("/"));
    Some(normalized)
}
