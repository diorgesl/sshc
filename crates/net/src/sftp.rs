//! Ator SFTP: thread própria com runtime tokio, no mesmo padrão do terminal.
//! A UI envia comandos por canal e recebe eventos; nenhum I/O remoto bloqueia
//! a interface.

use super::ssh::open_authenticated;
use super::Credentials;
use anyhow::Result;
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags};
use sshc_core::Session;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tokio::task::JoinSet;

/// Tempo máximo para o handshake SSH, o subsistema e a sessão SFTP ficarem
/// prontos. É o mesmo timeout do terminal.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
/// Timeout de cada operação SFTP, em segundos (a API do russh-sftp usa segundos).
const OPERATION_TIMEOUT_SECS: u64 = 30;
/// O servidor precisa confirmar o subsistema antes de iniciar o protocolo SFTP.
const SUBSYSTEM_TIMEOUT: Duration = Duration::from_secs(10);
/// Intervalo de checagem da sessão SSH enquanto o ator está ocioso.
const CLOSED_POLL: Duration = Duration::from_millis(500);
/// Tamanho dos blocos lidos/gravados nas transferências.
pub const CHUNK_SIZE: usize = 64 * 1024;
/// Intervalo mínimo entre dois JobProgress do mesmo job.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Sentido de uma transferência.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

/// O que fazer quando o destino já existe. Até a Task 7 não há detecção de
/// conflito: toda política se comporta como Overwrite.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    Ask,
    Overwrite,
    Skip,
}

/// Desfecho de um job de transferência.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Completed { ignored_links: u64 },
    Cancelled,
    Failed(String),
}

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
    List {
        id: u64,
        path: String,
    },
    Mkdir {
        id: u64,
        path: String,
    },
    Rename {
        id: u64,
        from: String,
        to: String,
    },
    Delete {
        id: u64,
        paths: Vec<String>,
        recursive: bool,
    },
    Chmod {
        id: u64,
        paths: Vec<String>,
        mode: u32,
    },
    Download {
        job_id: u64,
        /// Caminhos remotos dos arquivos.
        sources: Vec<String>,
        /// Pasta local de destino.
        dest: PathBuf,
        policy: ConflictPolicy,
    },
    Upload {
        job_id: u64,
        /// Arquivos locais.
        sources: Vec<PathBuf>,
        /// Pasta remota de destino.
        dest: String,
        policy: ConflictPolicy,
    },
    Cancel {
        job_id: u64,
    },
    Disconnect,
}

#[derive(Debug)]
pub enum SftpEvent {
    Listed {
        id: u64,
        path: String,
        result: std::result::Result<Vec<SftpEntry>, String>,
    },
    OpDone {
        id: u64,
        result: std::result::Result<(), String>,
    },
    /// Emitido antes do primeiro chunk, depois que o planejamento terminou.
    JobStarted {
        id: u64,
        items_total: u64,
        bytes_total: Option<u64>,
    },
    /// No máximo um a cada 100 ms por job.
    JobProgress {
        id: u64,
        items_done: u64,
        bytes_done: u64,
        bytes_total: u64,
        current: String,
    },
    JobDone {
        id: u64,
        outcome: JobOutcome,
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
    // Jobs rodam em tasks próprias, mas compartilham a mesma sessão SFTP
    // multiplexada com o ator.
    let sftp = Arc::new(sftp);

    if ready.send(Ok((note, home.clone()))).is_err() {
        // O chamador desistiu (timeout do connect).
        return Ok(());
    }

    let mut jobs: HashMap<u64, RunningJob> = HashMap::new();
    let mut tasks: JoinSet<u64> = JoinSet::new();

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
                Some(
                    command @ (SftpCmd::Mkdir { .. }
                    | SftpCmd::Rename { .. }
                    | SftpCmd::Delete { .. }
                    | SftpCmd::Chmod { .. }),
                ) => {
                    let (id, result) = run_op(&sftp, command, home.as_deref()).await;
                    if handle.is_closed() {
                        let _ = events.send(SftpEvent::Closed {
                            reason: "conexão SSH caiu".to_string(),
                        });
                        break;
                    }
                    if events.send(SftpEvent::OpDone { id, result }).is_err() {
                        break;
                    }
                }
                Some(SftpCmd::Download {
                    job_id,
                    sources,
                    dest,
                    policy,
                }) => {
                    spawn_job(
                        &mut tasks,
                        &mut jobs,
                        sftp.clone(),
                        events.clone(),
                        home.clone(),
                        Job::download(job_id, sources, dest, policy),
                    );
                }
                Some(SftpCmd::Upload {
                    job_id,
                    sources,
                    dest,
                    policy,
                }) => {
                    spawn_job(
                        &mut tasks,
                        &mut jobs,
                        sftp.clone(),
                        events.clone(),
                        home.clone(),
                        Job::upload(job_id, sources, dest, policy),
                    );
                }
                Some(SftpCmd::Cancel { job_id }) => {
                    // Só jobs em andamento estão no mapa; cancelar um job
                    // desconhecido (ou já concluído) é inofensivo.
                    if let Some(job) = jobs.get(&job_id) {
                        job.cancel.store(true, Ordering::Relaxed);
                        job.wake.notify_one();
                    }
                }
                Some(SftpCmd::Disconnect) | None => break,
            },
            Some(finished) = tasks.join_next(), if !tasks.is_empty() => {
                if let Ok(id) = finished {
                    jobs.remove(&id);
                }
            }
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

async fn run_op(
    session: &SftpSession,
    command: SftpCmd,
    home: Option<&str>,
) -> (u64, std::result::Result<(), String>) {
    match command {
        SftpCmd::Mkdir { id, path } => (id, make_dir(session, &path, home).await),
        SftpCmd::Rename { id, from, to } => (id, rename_entry(session, &from, &to, home).await),
        SftpCmd::Delete {
            id,
            paths,
            recursive,
        } => (id, delete_entries(session, &paths, recursive, home).await),
        SftpCmd::Chmod { id, paths, mode } => {
            (id, chmod_entries(session, &paths, mode, home).await)
        }
        SftpCmd::List { .. }
        | SftpCmd::Download { .. }
        | SftpCmd::Upload { .. }
        | SftpCmd::Cancel { .. }
        | SftpCmd::Disconnect => {
            unreachable!("run_op recebeu comando que não gera OpDone")
        }
    }
}

fn resolve_op_path(requested: &str, home: Option<&str>) -> std::result::Result<String, String> {
    resolve_remote(requested, home).ok_or_else(|| format!("caminho remoto inválido: {requested}"))
}

async fn make_dir(
    session: &SftpSession,
    requested: &str,
    home: Option<&str>,
) -> std::result::Result<(), String> {
    let path = resolve_op_path(requested, home)?;
    session
        .create_dir(path.as_str())
        .await
        .map_err(|error| format!("criando {path}: {error}"))
}

async fn rename_entry(
    session: &SftpSession,
    from: &str,
    to: &str,
    home: Option<&str>,
) -> std::result::Result<(), String> {
    let from = resolve_op_path(from, home)?;
    let to = resolve_op_path(to, home)?;
    session
        .rename(from.as_str(), to.as_str())
        .await
        .map_err(|error| format!("renomeando {from} para {to}: {error}"))
}

async fn delete_entries(
    session: &SftpSession,
    paths: &[String],
    recursive: bool,
    home: Option<&str>,
) -> std::result::Result<(), String> {
    for requested in paths {
        let path = resolve_op_path(requested, home)?;
        // Apagar a raiz remota esvaziaria tudo abaixo dela e ainda falharia no
        // rmdir final; recusa antes de qualquer remoção.
        if path == "/" {
            return Err("não é seguro apagar a raiz /".to_string());
        }
        delete_entry(session, &path, recursive).await?;
    }
    Ok(())
}

async fn delete_entry(
    session: &SftpSession,
    path: &str,
    recursive: bool,
) -> std::result::Result<(), String> {
    let metadata = session
        .symlink_metadata(path)
        .await
        .map_err(|error| format!("apagando {path}: {error}"))?;
    if metadata.file_type().is_dir() {
        if recursive {
            remove_tree(session, path).await
        } else {
            session
                .remove_dir(path)
                .await
                .map_err(|error| format!("apagando {path}: {error}"))
        }
    } else {
        session
            .remove_file(path)
            .await
            .map_err(|error| format!("apagando {path}: {error}"))
    }
}

/// Remove root e tudo abaixo, sem seguir symlinks. As pastas são removidas
/// depois dos filhos (ordem inversa da coleta).
async fn remove_tree(session: &SftpSession, root: &str) -> std::result::Result<(), String> {
    let mut pending = vec![root.to_string()];
    let mut dirs = Vec::new();
    while let Some(dir) = pending.pop() {
        dirs.push(dir.clone());
        let entries = session
            .read_dir(dir.clone())
            .await
            .map_err(|error| format!("listando {dir}: {error}"))?;
        for entry in entries {
            let child = entry.path();
            let metadata = match session.symlink_metadata(child.clone()).await {
                Ok(metadata) => metadata,
                Err(_) => entry.metadata(),
            };
            if metadata.file_type().is_dir() {
                pending.push(child);
            } else {
                session
                    .remove_file(child.as_str())
                    .await
                    .map_err(|error| format!("apagando {child}: {error}"))?;
            }
        }
    }
    for dir in dirs.into_iter().rev() {
        session
            .remove_dir(dir.as_str())
            .await
            .map_err(|error| format!("apagando {dir}: {error}"))?;
    }
    Ok(())
}

async fn chmod_entries(
    session: &SftpSession,
    paths: &[String],
    mode: u32,
    home: Option<&str>,
) -> std::result::Result<(), String> {
    let attrs = FileAttributes {
        permissions: Some(mode & 0o7777),
        ..FileAttributes::empty()
    };
    for requested in paths {
        let path = resolve_op_path(requested, home)?;
        session
            .set_metadata(path.as_str(), attrs.clone())
            .await
            .map_err(|error| format!("alterando permissões de {path}: {error}"))?;
    }
    Ok(())
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
        // readlink não é fatal: servidor sem suporte deixa o alvo desconhecido.
        let link_target = if kind == EntryKind::Symlink {
            session.read_link(entry_path.clone()).await.ok()
        } else {
            None
        };
        entries.push(SftpEntry {
            name,
            path: entry_path,
            kind,
            size: metadata.len(),
            modified: metadata.mtime,
            mode: metadata.permissions.map(|mode| mode & 0o7777),
            link_target,
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

/// Handle de cancelamento de um job em andamento.
struct RunningJob {
    cancel: Arc<AtomicBool>,
    /// Acorda a leitura/escrita em voo quando o job é cancelado.
    wake: Arc<Notify>,
}

/// Origem de um job: remota (download) ou local (upload).
enum JobSource {
    Remote(String),
    Local(PathBuf),
}

/// Destino de um job: pasta local (download) ou remota (upload).
enum JobDest {
    Local(PathBuf),
    Remote(String),
}

/// Uma transferência de arquivos. Nesta tarefa só arquivos são aceitos;
/// pastas serão expandidas em pré-walk na Task 5.
struct Job {
    id: u64,
    direction: Direction,
    sources: Vec<JobSource>,
    dest: JobDest,
    policy: ConflictPolicy,
    cancel: Arc<AtomicBool>,
    wake: Arc<Notify>,
}

impl Job {
    fn download(id: u64, sources: Vec<String>, dest: PathBuf, policy: ConflictPolicy) -> Self {
        Self {
            id,
            direction: Direction::Download,
            sources: sources.into_iter().map(JobSource::Remote).collect(),
            dest: JobDest::Local(dest),
            policy,
            cancel: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
        }
    }

    fn upload(id: u64, sources: Vec<PathBuf>, dest: String, policy: ConflictPolicy) -> Self {
        Self {
            id,
            direction: Direction::Upload,
            sources: sources.into_iter().map(JobSource::Local).collect(),
            dest: JobDest::Remote(dest),
            policy,
            cancel: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
        }
    }
}

/// Arquivo já planejado: origem, destino remoto (upload) e local (download).
struct PlannedFile {
    /// Caminho mostrado nos eventos de progresso.
    source: String,
    remote: String,
    local: PathBuf,
}

/// Erro interno de um job; vira JobOutcome em run_job.
enum JobError {
    Cancelled,
    Failed(String),
}

impl JobError {
    fn failed(message: impl Into<String>) -> Self {
        Self::Failed(message.into())
    }
}

type JobResult<T> = std::result::Result<T, JobError>;

fn spawn_job(
    tasks: &mut JoinSet<u64>,
    jobs: &mut HashMap<u64, RunningJob>,
    session: Arc<SftpSession>,
    events: Sender<SftpEvent>,
    home: Option<String>,
    job: Job,
) {
    jobs.insert(
        job.id,
        RunningJob {
            cancel: job.cancel.clone(),
            wake: job.wake.clone(),
        },
    );
    let _ = tasks.spawn(async move {
        let id = job.id;
        run_job(session, events, home, job).await;
        id
    });
}

async fn run_job(
    session: Arc<SftpSession>,
    events: Sender<SftpEvent>,
    home: Option<String>,
    job: Job,
) {
    let id = job.id;
    let outcome = if job.cancel.load(Ordering::Relaxed) {
        // Cancelado antes de começar: não emite JobStarted.
        JobOutcome::Cancelled
    } else {
        execute_job(&session, &events, home.as_deref(), &job).await
    };
    let _ = events.send(SftpEvent::JobDone { id, outcome });
}

async fn execute_job(
    session: &SftpSession,
    events: &Sender<SftpEvent>,
    home: Option<&str>,
    job: &Job,
) -> JobOutcome {
    // Task 7 implementa conflitos; por ora qualquer política sobrescreve e
    // nenhum SftpEvent::Conflict é emitido.
    let _ = job.policy;
    let result = match job.direction {
        Direction::Download => download_job(session, events, home, job).await,
        Direction::Upload => upload_job(session, events, home, job).await,
    };
    match result {
        Ok(()) => JobOutcome::Completed { ignored_links: 0 },
        Err(JobError::Cancelled) => JobOutcome::Cancelled,
        Err(JobError::Failed(message)) => JobOutcome::Failed(message),
    }
}

/// Contabiliza bytes e emite JobProgress no máximo a cada 100 ms.
struct Progress<'a> {
    events: &'a Sender<SftpEvent>,
    id: u64,
    items_done: u64,
    bytes_done: u64,
    bytes_total: u64,
    last: Instant,
}

impl<'a> Progress<'a> {
    fn new(events: &'a Sender<SftpEvent>, id: u64, bytes_total: u64) -> Self {
        Self {
            events,
            id,
            items_done: 0,
            bytes_done: 0,
            bytes_total,
            last: Instant::now(),
        }
    }

    fn add(&mut self, bytes: u64, current: &str) {
        self.bytes_done += bytes;
        if self.last.elapsed() < PROGRESS_INTERVAL {
            return;
        }
        self.last = Instant::now();
        let _ = self.events.send(SftpEvent::JobProgress {
            id: self.id,
            items_done: self.items_done,
            bytes_done: self.bytes_done,
            bytes_total: self.bytes_total,
            current: current.to_string(),
        });
    }

    fn item_done(&mut self) {
        self.items_done += 1;
    }
}

async fn download_job(
    session: &SftpSession,
    events: &Sender<SftpEvent>,
    home: Option<&str>,
    job: &Job,
) -> JobResult<()> {
    let JobDest::Local(dest_dir) = &job.dest else {
        return Err(JobError::failed("job de download sem destino local"));
    };

    let mut planned = Vec::with_capacity(job.sources.len());
    let mut bytes_total = 0u64;
    for source in &job.sources {
        let JobSource::Remote(requested) = source else {
            return Err(JobError::failed("job de download com origem local"));
        };
        let remote = resolve_remote(requested, home)
            .ok_or_else(|| JobError::failed(format!("caminho remoto inválido: {requested}")))?;
        let metadata = session
            .metadata(remote.as_str())
            .await
            .map_err(|error| JobError::failed(format!("lendo {remote}: {error}")))?;
        if metadata.file_type().is_dir() {
            return Err(JobError::failed("pasta ainda não suportada"));
        }
        let name = remote_file_name(&remote)
            .ok_or_else(|| JobError::failed(format!("nome de arquivo inválido: {remote}")))?
            .to_string();
        bytes_total += metadata.len();
        planned.push(PlannedFile {
            source: remote.clone(),
            remote,
            local: dest_dir.join(name),
        });
    }

    std::fs::create_dir_all(dest_dir)
        .map_err(|error| JobError::failed(format!("criando {}: {error}", dest_dir.display())))?;
    let _ = events.send(SftpEvent::JobStarted {
        id: job.id,
        items_total: planned.len() as u64,
        bytes_total: Some(bytes_total),
    });
    let mut progress = Progress::new(events, job.id, bytes_total);
    for file in &planned {
        download_file(session, job, file, &mut progress).await?;
    }
    Ok(())
}

async fn download_file(
    session: &SftpSession,
    job: &Job,
    file: &PlannedFile,
    progress: &mut Progress<'_>,
) -> JobResult<()> {
    let part = part_path(&file.local);
    let mut guard = PartGuard::new(part.clone());
    let mut local = std::fs::File::create(&part)
        .map_err(|error| JobError::failed(format!("criando {}: {error}", part.display())))?;
    let mut remote = session
        .open(file.remote.as_str())
        .await
        .map_err(|error| JobError::failed(format!("abrindo {}: {error}", file.remote)))?;

    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        if job.cancel.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        let read = tokio::select! {
            result = remote.read(&mut buffer) => result
                .map_err(|error| JobError::failed(format!("lendo {}: {error}", file.remote)))?,
            _ = job.wake.notified() => return Err(JobError::Cancelled),
        };
        if read == 0 {
            break;
        }
        local
            .write_all(&buffer[..read])
            .map_err(|error| JobError::failed(format!("gravando {}: {error}", part.display())))?;
        progress.add(read as u64, &file.source);
    }

    remote
        .close()
        .await
        .map_err(|error| JobError::failed(format!("fechando {}: {error}", file.remote)))?;
    // Cancelado no fim: não publica o arquivo, descarta o .part.
    if job.cancel.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    drop(local);
    std::fs::rename(&part, &file.local)
        .map_err(|error| JobError::failed(format!("renomeando {}: {error}", part.display())))?;
    guard.disarm();
    progress.item_done();
    Ok(())
}

async fn upload_job(
    session: &SftpSession,
    events: &Sender<SftpEvent>,
    home: Option<&str>,
    job: &Job,
) -> JobResult<()> {
    let JobDest::Remote(requested) = &job.dest else {
        return Err(JobError::failed("job de upload sem destino remoto"));
    };
    let dest_dir = resolve_remote(requested, home)
        .ok_or_else(|| JobError::failed(format!("caminho remoto inválido: {requested}")))?;

    let mut planned = Vec::with_capacity(job.sources.len());
    let mut bytes_total = 0u64;
    for source in &job.sources {
        let JobSource::Local(path) = source else {
            return Err(JobError::failed("job de upload com origem remota"));
        };
        let metadata = std::fs::metadata(path)
            .map_err(|error| JobError::failed(format!("lendo {}: {error}", path.display())))?;
        if metadata.is_dir() {
            return Err(JobError::failed("pasta ainda não suportada"));
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| JobError::failed(format!("nome inválido: {}", path.display())))?;
        bytes_total += metadata.len();
        planned.push(PlannedFile {
            source: path.display().to_string(),
            remote: join_remote(&dest_dir, name),
            local: path.clone(),
        });
    }

    let _ = events.send(SftpEvent::JobStarted {
        id: job.id,
        items_total: planned.len() as u64,
        bytes_total: Some(bytes_total),
    });
    let mut progress = Progress::new(events, job.id, bytes_total);
    for file in &planned {
        upload_file(session, job, file, &mut progress).await?;
    }
    Ok(())
}

async fn upload_file(
    session: &SftpSession,
    job: &Job,
    file: &PlannedFile,
    progress: &mut Progress<'_>,
) -> JobResult<()> {
    let mut local = std::fs::File::open(&file.local)
        .map_err(|error| JobError::failed(format!("abrindo {}: {error}", file.source)))?;
    let mut remote = session
        .open_with_flags(
            file.remote.as_str(),
            OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
        )
        .await
        .map_err(|error| JobError::failed(format!("criando {}: {error}", file.remote)))?;

    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        if job.cancel.load(Ordering::Relaxed) {
            return Err(JobError::Cancelled);
        }
        let read = local
            .read(&mut buffer)
            .map_err(|error| JobError::failed(format!("lendo {}: {error}", file.source)))?;
        if read == 0 {
            break;
        }
        tokio::select! {
            result = remote.write_all(&buffer[..read]) => result
                .map_err(|error| JobError::failed(format!("gravando {}: {error}", file.remote)))?,
            _ = job.wake.notified() => return Err(JobError::Cancelled),
        }
        progress.add(read as u64, &file.source);
    }

    if job.cancel.load(Ordering::Relaxed) {
        return Err(JobError::Cancelled);
    }
    remote
        .close()
        .await
        .map_err(|error| JobError::failed(format!("fechando {}: {error}", file.remote)))?;
    progress.item_done();
    Ok(())
}

/// Remove o .part quando o download não chega ao rename final, inclusive em
/// cancelamento, erro de I/O e abort da task.
struct PartGuard {
    path: PathBuf,
    keep: bool,
}

impl PartGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    fn disarm(&mut self) {
        self.keep = true;
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest.as_os_str().to_os_string();
    name.push(".part");
    PathBuf::from(name)
}

fn remote_file_name(path: &str) -> Option<&str> {
    path.rsplit('/').next().filter(|name| !name.is_empty())
}
