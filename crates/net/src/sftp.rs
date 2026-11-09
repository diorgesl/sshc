//! Ator SFTP: thread própria com runtime tokio, no mesmo padrão do terminal.
//! A UI envia comandos por canal e recebe eventos; nenhum I/O remoto bloqueia
//! a interface.

use super::ssh::open_authenticated;
use super::Credentials;
use anyhow::Result;
use russh::ChannelMsg;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, FileType, OpenFlags, StatusCode};
use sshc_core::Session;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
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
/// Quantos jobs de transferência podem rodar ao mesmo tempo.
const MAX_CONCURRENT_JOBS: usize = 3;
/// Intervalo mínimo entre dois JobProgress do mesmo job.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Sentido de uma transferência.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Download,
    Upload,
}

/// Política inicial de um job quando o destino já existe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy {
    /// Pergunta ao usuário por arquivo, com um SftpEvent::Conflict por vez.
    Ask,
    /// Sobrescreve sem perguntar.
    Overwrite,
    /// Pula o arquivo conflitante sem perguntar, deixando o destino intacto.
    Skip,
}

/// Resposta do usuário para um conflito de arquivo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictDecision {
    /// Sobrescreve o destino com o conteúdo transferido.
    Overwrite,
    /// Grava ao lado do destino como "stem (n).ext".
    Rename,
    /// Mantém o destino e não transfere o arquivo.
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
    /// Resposta a um SftpEvent::Conflict de um job em andamento.
    ResolveConflict {
        job_id: u64,
        decision: ConflictDecision,
        /// Fixa a decisão para os conflitos restantes do job.
        apply_all: bool,
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
    /// No máximo um a cada 100 ms por job, mais um progresso final com o
    /// total completo antes de JobDone::Completed.
    JobProgress {
        id: u64,
        items_done: u64,
        bytes_done: u64,
        bytes_total: Option<u64>,
        current: String,
    },
    /// O destino de um arquivo já existe e a política do job pede decisão.
    /// O job fica pausado até SftpCmd::ResolveConflict.
    Conflict {
        id: u64,
        direction: Direction,
        /// Origem: caminho remoto no download, local no upload.
        source: String,
        /// Destino: caminho local no download, remoto no upload.
        dest: String,
        source_size: u64,
        dest_size: u64,
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

    let mut queue = JobQueue::new(MAX_CONCURRENT_JOBS);
    let mut jobs: HashMap<u64, RunningJob> = HashMap::new();
    let mut tasks: JoinSet<u64> = JoinSet::new();
    // Job id por task tokio: um JoinSet::join_next Err não carrega o id do
    // job, mas o JoinError carrega o id da task.
    let mut task_ids: HashMap<tokio::task::Id, u64> = HashMap::new();
    let mut session_lost: Option<String> = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        tokio::select! {
            command = commands.recv() => match command {
                Some(SftpCmd::List { id, path }) => {
                    let (resolved, result) = list(&sftp, &path, home.as_deref()).await;
                    if handle.is_closed() {
                        session_lost = Some("conexão SSH caiu".to_string());
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
                        session_lost = Some("conexão SSH caiu".to_string());
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
                    queue.push(Job::download(job_id, sources, dest, policy));
                    start_next(
                        &mut queue,
                        &mut tasks,
                        &mut task_ids,
                        &mut jobs,
                        &sftp,
                        &events,
                        &home,
                    );
                }
                Some(SftpCmd::Upload {
                    job_id,
                    sources,
                    dest,
                    policy,
                }) => {
                    queue.push(Job::upload(job_id, sources, dest, policy));
                    start_next(
                        &mut queue,
                        &mut tasks,
                        &mut task_ids,
                        &mut jobs,
                        &sftp,
                        &events,
                        &home,
                    );
                }
                Some(SftpCmd::ResolveConflict {
                    job_id,
                    decision,
                    apply_all,
                }) => {
                    // Job já terminou ou ainda está na fila: a resposta não
                    // tem para quem ir e é descartada.
                    if let Some(job) = jobs.get(&job_id) {
                        let _ = job.decisions.send(DecisionReply {
                            decision,
                            apply_all,
                        });
                    }
                }
                Some(SftpCmd::Cancel { job_id }) => {
                    // Cancelar um job que ainda não começou o remove da fila:
                    // ele nunca recebe JobStarted, mas recebe JobDone.
                    if queue.cancel(job_id) {
                        let _ = events.send(SftpEvent::JobDone {
                            id: job_id,
                            outcome: JobOutcome::Cancelled,
                        });
                    } else if let Some(job) = jobs.get(&job_id) {
                        // Job em andamento: a task confere a flag e para. O
                        // wake também encerra uma espera de conflito.
                        job.cancel.store(true, Ordering::Relaxed);
                        job.wake.notify_one();
                    }
                }
                Some(SftpCmd::Disconnect) | None => break,
            },
            Some(finished) = tasks.join_next(), if !tasks.is_empty() => {
                handle_finished(finished, &mut jobs, &mut task_ids, &mut queue, &events);
                // A primeira falha de um job pode ser o primeiro sinal de que a
                // sessão caiu: não começa um job novo numa conexão morta.
                if handle.is_closed() {
                    session_lost = Some("conexão SSH caiu".to_string());
                    break;
                }
                start_next(
                    &mut queue,
                    &mut tasks,
                    &mut task_ids,
                    &mut jobs,
                    &sftp,
                    &events,
                    &home,
                );
            }
            _ = tokio::time::sleep(CLOSED_POLL) => {
                if handle.is_closed() {
                    session_lost = Some("conexão SSH caiu".to_string());
                    break;
                }
            }
        }
    }

    if let Some(reason) = &session_lost {
        let _ = events.send(SftpEvent::Closed {
            reason: reason.clone(),
        });
    }
    let reason = session_lost.as_deref().unwrap_or("conexão encerrada");
    shutdown_jobs(
        &mut jobs,
        &mut tasks,
        &mut task_ids,
        &mut queue,
        &events,
        reason,
    )
    .await;

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
        | SftpCmd::ResolveConflict { .. }
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

/// Monta o destino local de um item baixado: junta root e o caminho
/// relativo rel (POSIX, com '/') rejeitando qualquer componente que possa
/// escapar da pasta de destino. No Windows também recusa separador '\',
/// prefixo de unidade e prefixo de raiz — em Unix '\' é um nome válido.
pub fn local_dest(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    if rel.is_empty() || rel.contains('\0') {
        anyhow::bail!("nome de destino inválido: {rel:?}");
    }
    #[cfg(windows)]
    if rel.contains('\\') {
        anyhow::bail!("nome de destino inválido: {rel:?}");
    }

    let mut joined = root.to_path_buf();
    for component in rel.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            anyhow::bail!("nome de destino inválido: {rel:?}");
        }
        let piece = Path::new(component);
        if piece.components().count() != 1
            || matches!(
                piece.components().next(),
                Some(Component::Prefix(_)) | Some(Component::RootDir)
            )
        {
            anyhow::bail!("nome de destino inválido: {rel:?}");
        }
        joined.push(component);
    }

    // Defesa em profundidade: nenhum caminho pode sair da pasta escolhida.
    if !joined.starts_with(root) {
        anyhow::bail!("destino fora da pasta: {rel:?}");
    }
    Ok(joined)
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

/// Resposta do usuário encaminhada do ator para a task do job.
#[derive(Debug, Clone, Copy)]
struct DecisionReply {
    decision: ConflictDecision,
    apply_all: bool,
}

/// Handle de um job em andamento: cancelamento e canal de decisões.
struct RunningJob {
    cancel: Arc<AtomicBool>,
    /// Acorda a leitura/escrita (ou a espera de conflito) quando o job é
    /// cancelado.
    wake: Arc<Notify>,
    /// Recebe os ResolveConflict endereçados a este job.
    decisions: UnboundedSender<DecisionReply>,
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

/// Uma transferência. Arquivos e pastas são expandidos em pré-walk antes do
/// JobStarted; a transferência em si percorre só itens já planejados.
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

/// Fila pura dos jobs de transferência: guarda o que ainda não começou e
/// controla quantos rodam ao mesmo tempo. Não faz I/O nem conhece o ator.
struct JobQueue {
    pending: VecDeque<Job>,
    running: usize,
    capacity: usize,
}

impl JobQueue {
    fn new(capacity: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            running: 0,
            capacity,
        }
    }

    fn push(&mut self, job: Job) {
        self.pending.push_back(job);
    }

    /// Retira o próximo job se houver slot livre. Cada retirada bem-sucedida
    /// ocupa um slot até o finish correspondente.
    fn next(&mut self) -> Option<Job> {
        if self.running >= self.capacity {
            return None;
        }
        let job = self.pending.pop_front()?;
        self.running += 1;
        Some(job)
    }

    /// Libera um slot quando a task de um job termina.
    fn finish(&mut self) {
        self.running = self.running.saturating_sub(1);
    }

    /// Remove um job que ainda não começou. Devolve true se ele estava na fila;
    /// jobs em andamento são cancelados pelo mapa de jobs do ator.
    fn cancel(&mut self, id: u64) -> bool {
        let before = self.pending.len();
        self.pending.retain(|job| job.id != id);
        self.pending.len() != before
    }

    /// Esvazia a fila (queda de conexão) devolvendo os jobs que não começaram.
    fn drain(&mut self) -> Vec<Job> {
        self.pending.drain(..).collect()
    }
}

/// Tipo de um item planejado pelo pré-walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlannedKind {
    Directory,
    File,
}

/// Item já planejado: diretório ou arquivo, com os dois lados do job.
struct PlannedItem {
    /// Caminho mostrado nos eventos de progresso.
    source: String,
    /// Caminho remoto (origem no download, destino no upload).
    remote: String,
    /// Caminho local (destino no download, origem no upload).
    local: PathBuf,
    kind: PlannedKind,
    /// Tamanho do arquivo de origem, usado no evento de conflito.
    size: u64,
}

/// Resultado do pré-walk. A ordem dos itens garante que cada diretório aparece
/// antes dos filhos, para poder ser criado antes deles.
#[derive(Default)]
struct Plan {
    items: Vec<PlannedItem>,
    bytes_total: u64,
    ignored_links: u64,
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

/// Puxa da fila enquanto houver slot livre, registrando cada job em
/// andamento e spawnando sua task no JoinSet do ator.
fn start_next(
    queue: &mut JobQueue,
    tasks: &mut JoinSet<u64>,
    task_ids: &mut HashMap<tokio::task::Id, u64>,
    jobs: &mut HashMap<u64, RunningJob>,
    session: &Arc<SftpSession>,
    events: &Sender<SftpEvent>,
    home: &Option<String>,
) {
    while let Some(job) = queue.next() {
        let id = job.id;
        let (decision_tx, decision_rx) = tokio::sync::mpsc::unbounded_channel();
        jobs.insert(
            id,
            RunningJob {
                cancel: job.cancel.clone(),
                wake: job.wake.clone(),
                decisions: decision_tx,
            },
        );
        let session = Arc::clone(session);
        let events = events.clone();
        let home = home.clone();
        let handle = tasks.spawn(async move {
            run_job(session, events, home, job, decision_rx).await;
            id
        });
        task_ids.insert(handle.id(), id);
    }
}

/// Trata uma task concluída. Um JoinError (pânico ou abort inesperado) não pode
/// deixar o job no mapa para sempre: remove a entrada e emite JobDone::Failed.
fn handle_finished(
    finished: std::result::Result<u64, tokio::task::JoinError>,
    jobs: &mut HashMap<u64, RunningJob>,
    task_ids: &mut HashMap<tokio::task::Id, u64>,
    queue: &mut JobQueue,
    events: &Sender<SftpEvent>,
) {
    match finished {
        Ok(id) => {
            jobs.remove(&id);
            task_ids.retain(|_, mapped| *mapped != id);
            queue.finish();
        }
        Err(error) => {
            let Some(id) = task_ids.remove(&error.id()) else {
                return;
            };
            jobs.remove(&id);
            queue.finish();
            let _ = events.send(SftpEvent::JobDone {
                id,
                outcome: JobOutcome::Failed(format!("tarefa do job {id} falhou: {error}")),
            });
        }
    }
}

/// Encerra o ator sem deixar job sem JobDone: tasks em andamento são abortadas
/// (o Drop remove os .part), a fila vira Failed e o que já tinha terminado
/// conserva o desfecho que já emitiu.
async fn shutdown_jobs(
    jobs: &mut HashMap<u64, RunningJob>,
    tasks: &mut JoinSet<u64>,
    task_ids: &mut HashMap<tokio::task::Id, u64>,
    queue: &mut JobQueue,
    events: &Sender<SftpEvent>,
    reason: &str,
) {
    // Tasks que já terminaram emitiram o próprio JobDone; só remove o estado.
    while let Some(finished) = tasks.try_join_next() {
        match finished {
            Ok(id) => {
                jobs.remove(&id);
                task_ids.retain(|_, mapped| *mapped != id);
                queue.finish();
            }
            Err(error) => {
                task_ids.remove(&error.id());
            }
        }
    }
    // Aborta o que ainda está rodando antes de emitir Failed: assim ninguém
    // emite um JobDone concorrente e os PartGuard removem os .part no Drop.
    tasks.abort_all();
    while let Some(finished) = tasks.join_next().await {
        match finished {
            Ok(id) => {
                jobs.remove(&id);
                task_ids.retain(|_, mapped| *mapped != id);
            }
            Err(error) => {
                task_ids.remove(&error.id());
            }
        }
    }
    let mut running: Vec<u64> = jobs.drain().map(|(id, _)| id).collect();
    running.sort_unstable();
    for id in running {
        let _ = events.send(SftpEvent::JobDone {
            id,
            outcome: JobOutcome::Failed(reason.to_string()),
        });
    }
    // Jobs que nunca começaram também precisam de um desfecho.
    let mut queued: Vec<u64> = queue.drain().into_iter().map(|job| job.id).collect();
    queued.sort_unstable();
    for id in queued {
        let _ = events.send(SftpEvent::JobDone {
            id,
            outcome: JobOutcome::Failed(reason.to_string()),
        });
    }
}

async fn run_job(
    session: Arc<SftpSession>,
    events: Sender<SftpEvent>,
    home: Option<String>,
    job: Job,
    mut decisions: UnboundedReceiver<DecisionReply>,
) {
    let id = job.id;
    let outcome = if job.cancel.load(Ordering::Relaxed) {
        // Cancelado antes de começar: não emite JobStarted.
        JobOutcome::Cancelled
    } else {
        execute_job(&session, &events, home.as_deref(), &job, &mut decisions).await
    };
    let _ = events.send(SftpEvent::JobDone { id, outcome });
}

async fn execute_job(
    session: &SftpSession,
    events: &Sender<SftpEvent>,
    home: Option<&str>,
    job: &Job,
    decisions: &mut UnboundedReceiver<DecisionReply>,
) -> JobOutcome {
    let mut conflicts = Conflicts::default();
    let result = match job.direction {
        Direction::Download => {
            download_job(session, events, home, job, decisions, &mut conflicts).await
        }
        Direction::Upload => {
            upload_job(session, events, home, job, decisions, &mut conflicts).await
        }
    };
    match result {
        Ok(ignored_links) => JobOutcome::Completed { ignored_links },
        Err(JobError::Cancelled) => JobOutcome::Cancelled,
        Err(JobError::Failed(message)) => JobOutcome::Failed(message),
    }
}

/// Decisão fixada por apply_all, válida para o resto do job. Sem ela, cada
/// arquivo em conflito pergunta de novo (política Ask).
#[derive(Default)]
struct Conflicts {
    fixed: Option<ConflictDecision>,
}

/// Resolve a decisão de um arquivo cujo destino já existe. Com apply_all já
/// fixado ou política Overwrite/Skip a resposta é imediata; com política Ask
/// emite SftpEvent::Conflict e espera a resposta. Devolve Cancelled se o job
/// for cancelado ou o canal de decisões fechar durante a espera.
#[allow(clippy::too_many_arguments)]
async fn conflict_decision(
    job: &Job,
    events: &Sender<SftpEvent>,
    decisions: &mut UnboundedReceiver<DecisionReply>,
    conflicts: &mut Conflicts,
    direction: Direction,
    source: &str,
    dest: &str,
    source_size: u64,
    dest_size: u64,
) -> JobResult<ConflictDecision> {
    if let Some(decision) = conflicts.fixed {
        return Ok(decision);
    }
    match job.policy {
        ConflictPolicy::Overwrite => return Ok(ConflictDecision::Overwrite),
        ConflictPolicy::Skip => return Ok(ConflictDecision::Skip),
        ConflictPolicy::Ask => {}
    }
    if events
        .send(SftpEvent::Conflict {
            id: job.id,
            direction,
            source: source.to_string(),
            dest: dest.to_string(),
            source_size,
            dest_size,
        })
        .is_err()
    {
        // Sem receptor de eventos a UI sumiu: não há como perguntar.
        return Err(JobError::Cancelled);
    }
    let reply = tokio::select! {
        reply = decisions.recv() => reply,
        _ = job.wake.notified() => return Err(JobError::Cancelled),
    };
    // O canal de decisões só fecha quando o job deixa de existir; sem
    // resposta não há como continuar com segurança.
    let Some(reply) = reply else {
        return Err(JobError::Cancelled);
    };
    if reply.apply_all {
        conflicts.fixed = Some(reply.decision);
    }
    Ok(reply.decision)
}

/// Separa nome e extensão como o Path do Rust: "a.tar.gz" vira
/// ("a.tar", "gz") e ".bashrc" não tem extensão.
fn split_extension(name: &str) -> (&str, Option<&str>) {
    match name.rfind('.') {
        Some(index) if index > 0 && index + 1 < name.len() => {
            (&name[..index], Some(&name[index + 1..]))
        }
        _ => (name, None),
    }
}

/// Monta "stem (n).ext" preservando a extensão do nome original.
fn numbered_name(name: &str, number: u32) -> String {
    let (stem, extension) = split_extension(name);
    match extension {
        Some(extension) => format!("{stem} ({number}).{extension}"),
        None => format!("{stem} ({number})"),
    }
}

/// Primeiro nome livre no destino local para um arquivo renomeado.
fn numbered_local_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    for number in 2u32.. {
        let candidate = parent.join(numbered_name(&name, number));
        if std::fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    unreachable!("a busca por nome livre é infinita")
}

/// Primeiro nome livre no servidor para um arquivo renomeado. Usa lstat
/// para não considerar "livre" um nome ocupado por link (inclusive link
/// quebrado), que try_exists veria como ausente.
async fn numbered_remote_path(session: &SftpSession, path: &str) -> JobResult<String> {
    let (parent, name) = match path.rsplit_once('/') {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    };
    for number in 2u32.. {
        let candidate = join_remote(parent, &numbered_name(name, number));
        match session.symlink_metadata(candidate.clone()).await {
            // Nome ocupado por arquivo, pasta ou link: tenta o próximo.
            Ok(_) => continue,
            Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
                return Ok(candidate)
            }
            // Falha de I/O real: não assume que o nome está livre.
            Err(error) => return Err(JobError::failed(format!("lendo {candidate}: {error}"))),
        }
    }
    unreachable!("a busca por nome livre é infinita")
}

/// Aborta o planejamento se o job foi cancelado antes ou durante o pré-walk.
fn check_cancel(job: &Job) -> JobResult<()> {
    if job.cancel.load(Ordering::Relaxed) {
        Err(JobError::Cancelled)
    } else {
        Ok(())
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
            bytes_total: Some(self.bytes_total),
            current: current.to_string(),
        });
    }

    fn item_done(&mut self) {
        self.items_done += 1;
    }

    /// Progresso final: o design exige um JobProgress com os totais completos
    /// antes do JobDone::Completed, mesmo quando o transfer termina antes do
    /// primeiro intervalo de 100 ms.
    fn finish(&self, items_total: u64, bytes_total: u64, current: &str) {
        let _ = self.events.send(SftpEvent::JobProgress {
            id: self.id,
            items_done: items_total,
            bytes_done: bytes_total,
            bytes_total: Some(bytes_total),
            current: current.to_string(),
        });
    }
}

async fn download_job(
    session: &SftpSession,
    events: &Sender<SftpEvent>,
    home: Option<&str>,
    job: &Job,
    decisions: &mut UnboundedReceiver<DecisionReply>,
    conflicts: &mut Conflicts,
) -> JobResult<u64> {
    let JobDest::Local(dest_dir) = &job.dest else {
        return Err(JobError::failed("job de download sem destino local"));
    };

    let mut plan = plan_download(session, home, job, dest_dir).await?;
    std::fs::create_dir_all(dest_dir)
        .map_err(|error| JobError::failed(format!("criando {}: {error}", dest_dir.display())))?;
    let items_total = plan.items.len() as u64;
    let _ = events.send(SftpEvent::JobStarted {
        id: job.id,
        items_total,
        bytes_total: Some(plan.bytes_total),
    });

    let mut progress = Progress::new(events, job.id, plan.bytes_total);
    for item in &mut plan.items {
        check_cancel(job)?;
        match item.kind {
            // Pastas mesclam sem perguntar: só arquivos geram conflito.
            PlannedKind::Directory => {
                ensure_local_dir(&item.local)?;
                progress.item_done();
            }
            PlannedKind::File => {
                if let Some(dest_size) = ensure_local_file(&item.local)? {
                    let dest = item.local.display().to_string();
                    let decision = conflict_decision(
                        job,
                        events,
                        decisions,
                        conflicts,
                        Direction::Download,
                        &item.source,
                        &dest,
                        item.size,
                        dest_size,
                    )
                    .await?;
                    match decision {
                        ConflictDecision::Overwrite => {}
                        ConflictDecision::Rename => {
                            item.local = numbered_local_path(&item.local);
                        }
                        ConflictDecision::Skip => {
                            progress.item_done();
                            continue;
                        }
                    }
                }
                download_file(session, job, item, &mut progress).await?;
            }
        }
    }
    let current = plan
        .items
        .last()
        .and_then(|item| remote_file_name(&item.remote))
        .unwrap_or("");
    progress.finish(items_total, plan.bytes_total, current);
    Ok(plan.ignored_links)
}

/// Pré-walk do download: resolve cada origem e expande pastas remotas com
/// read_dir, sem seguir symlink.
async fn plan_download(
    session: &SftpSession,
    home: Option<&str>,
    job: &Job,
    dest_dir: &Path,
) -> JobResult<Plan> {
    let mut plan = Plan::default();
    for source in &job.sources {
        check_cancel(job)?;
        let JobSource::Remote(requested) = source else {
            return Err(JobError::failed("job de download com origem local"));
        };
        let remote = resolve_remote(requested, home)
            .ok_or_else(|| JobError::failed(format!("caminho remoto inválido: {requested}")))?;
        // A raiz remota contém tudo: transferi-la não é uma seleção válida.
        if remote == "/" {
            return Err(JobError::failed("selecione uma pasta"));
        }
        let name = remote_file_name(&remote)
            .ok_or_else(|| JobError::failed(format!("nome de arquivo inválido: {remote}")))?
            .to_string();
        plan_remote_selection(session, job, &remote, &name, dest_dir, &mut plan).await?;
    }
    Ok(plan)
}

/// Planeja uma origem remota escolhida sozinha. Link simbólico só é seguido
/// quando o alvo é arquivo regular; link para pasta é recusado.
async fn plan_remote_selection(
    session: &SftpSession,
    job: &Job,
    remote: &str,
    rel: &str,
    dest_root: &Path,
    plan: &mut Plan,
) -> JobResult<()> {
    let metadata = session
        .symlink_metadata(remote.to_string())
        .await
        .map_err(|error| JobError::failed(format!("lendo {remote}: {error}")))?;
    if metadata.file_type().is_symlink() {
        let target = session
            .metadata(remote.to_string())
            .await
            .map_err(|error| JobError::failed(format!("lendo link {remote}: {error}")))?;
        if target.file_type().is_file() {
            return plan_download_file(remote, rel, dest_root, target.len(), plan);
        }
        if target.file_type().is_dir() {
            return Err(JobError::failed(format!(
                "link simbólico para pasta não é suportado: {remote}"
            )));
        }
        return Err(JobError::failed(format!(
            "link simbólico para tipo não suportado: {remote}"
        )));
    }
    if metadata.file_type().is_dir() {
        plan_download_dir(remote, rel, dest_root, plan)?;
        walk_remote(session, job, remote, rel, dest_root, plan).await
    } else if metadata.file_type().is_file() {
        plan_download_file(remote, rel, dest_root, metadata.len(), plan)
    } else {
        Err(JobError::failed(format!("tipo não suportado: {remote}")))
    }
}

fn plan_download_dir(remote: &str, rel: &str, dest_root: &Path, plan: &mut Plan) -> JobResult<()> {
    let local = local_dest(dest_root, rel).map_err(local_dest_error)?;
    plan.items.push(PlannedItem {
        source: remote.to_string(),
        remote: remote.to_string(),
        local,
        kind: PlannedKind::Directory,
        size: 0,
    });
    Ok(())
}

fn plan_download_file(
    remote: &str,
    rel: &str,
    dest_root: &Path,
    size: u64,
    plan: &mut Plan,
) -> JobResult<()> {
    let local = local_dest(dest_root, rel).map_err(local_dest_error)?;
    plan.items.push(PlannedItem {
        source: remote.to_string(),
        remote: remote.to_string(),
        local,
        kind: PlannedKind::File,
        size,
    });
    plan.bytes_total += size;
    Ok(())
}

fn local_dest_error(error: anyhow::Error) -> JobError {
    JobError::failed(format!("{error:#}"))
}

/// Caminha uma pasta remota sem seguir symlink. Usa pilha explícita para não
/// criar recursão assíncrona; a ordem dos itens mantém cada diretório antes
/// dos filhos e a visita em ordem alfabética.
async fn walk_remote(
    session: &SftpSession,
    job: &Job,
    root: &str,
    root_rel: &str,
    dest_root: &Path,
    plan: &mut Plan,
) -> JobResult<()> {
    let mut pending = vec![(root.to_string(), root_rel.to_string())];
    while let Some((remote, rel)) = pending.pop() {
        check_cancel(job)?;
        let mut entries: Vec<_> = session
            .read_dir(remote.clone())
            .await
            .map_err(|error| JobError::failed(format!("listando {remote}: {error}")))?
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        let mut dirs = Vec::new();
        for entry in entries {
            check_cancel(job)?;
            let name = entry.file_name();
            if name.is_empty() || name == "." || name == ".." {
                continue;
            }
            let child = join_remote(&remote, &name);
            let child_rel = format!("{rel}/{name}");
            // O readdir já traz atributos, mas o lstat identifica symlink
            // mesmo quando o servidor omite o tipo.
            let metadata = match session.symlink_metadata(child.clone()).await {
                Ok(metadata) => metadata,
                Err(_) => entry.metadata(),
            };
            if metadata.file_type().is_symlink() {
                plan.ignored_links += 1;
            } else if metadata.file_type().is_dir() {
                plan_download_dir(&child, &child_rel, dest_root, plan)?;
                dirs.push((child, child_rel));
            } else if metadata.file_type().is_file() {
                plan_download_file(&child, &child_rel, dest_root, metadata.len(), plan)?;
            } else {
                return Err(JobError::failed(format!("tipo não suportado: {child}")));
            }
        }
        for dir in dirs.into_iter().rev() {
            pending.push(dir);
        }
    }
    Ok(())
}

/// Garante que o destino local de uma pasta existe, mesclando uma pasta já
/// existente e recusando arquivo (ou link) no lugar dela.
fn ensure_local_dir(path: &Path) -> JobResult<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(JobError::failed(format!(
            "destino é um link simbólico: {}",
            path.display()
        ))),
        Ok(_) => Err(JobError::failed(format!(
            "destino é um arquivo, não pasta: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir_all(path)
            .map_err(|error| JobError::failed(format!("criando {}: {error}", path.display()))),
        Err(error) => Err(JobError::failed(format!(
            "lendo {}: {error}",
            path.display()
        ))),
    }
}

/// Garante que o destino local de um arquivo não é pasta (nem link) e
/// devolve o tamanho do arquivo existente, quando houver conflito.
fn ensure_local_file(path: &Path) -> JobResult<Option<u64>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Err(JobError::failed(format!(
            "destino é uma pasta, não arquivo: {}",
            path.display()
        ))),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(JobError::failed(format!(
            "destino é um link simbólico: {}",
            path.display()
        ))),
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(JobError::failed(format!(
            "lendo {}: {error}",
            path.display()
        ))),
    }
}

async fn download_file(
    session: &SftpSession,
    job: &Job,
    file: &PlannedItem,
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
    decisions: &mut UnboundedReceiver<DecisionReply>,
    conflicts: &mut Conflicts,
) -> JobResult<u64> {
    let mut plan = plan_upload(home, job)?;
    let items_total = plan.items.len() as u64;
    let _ = events.send(SftpEvent::JobStarted {
        id: job.id,
        items_total,
        bytes_total: Some(plan.bytes_total),
    });

    let mut progress = Progress::new(events, job.id, plan.bytes_total);
    for item in &mut plan.items {
        check_cancel(job)?;
        match item.kind {
            // Pastas mesclam sem perguntar: só arquivos geram conflito.
            PlannedKind::Directory => {
                ensure_remote_dir(session, &item.remote).await?;
                progress.item_done();
            }
            PlannedKind::File => {
                if let Some(dest_size) = ensure_remote_file(session, &item.remote).await? {
                    let decision = conflict_decision(
                        job,
                        events,
                        decisions,
                        conflicts,
                        Direction::Upload,
                        &item.source,
                        &item.remote,
                        item.size,
                        dest_size,
                    )
                    .await?;
                    match decision {
                        ConflictDecision::Overwrite => {}
                        ConflictDecision::Rename => {
                            item.remote = numbered_remote_path(session, &item.remote).await?;
                        }
                        ConflictDecision::Skip => {
                            progress.item_done();
                            continue;
                        }
                    }
                }
                upload_file(session, job, item, &mut progress).await?;
            }
        }
    }
    let current = plan
        .items
        .last()
        .and_then(|item| remote_file_name(&item.remote))
        .unwrap_or("");
    progress.finish(items_total, plan.bytes_total, current);
    Ok(plan.ignored_links)
}

/// Pré-walk do upload: expande cada origem local com symlink_metadata, sem
/// seguir link, e resolve o destino remoto uma única vez.
fn plan_upload(home: Option<&str>, job: &Job) -> JobResult<Plan> {
    let JobDest::Remote(requested) = &job.dest else {
        return Err(JobError::failed("job de upload sem destino remoto"));
    };
    let dest_dir = resolve_remote(requested, home)
        .ok_or_else(|| JobError::failed(format!("caminho remoto inválido: {requested}")))?;

    let mut plan = Plan::default();
    for source in &job.sources {
        check_cancel(job)?;
        let JobSource::Local(path) = source else {
            return Err(JobError::failed("job de upload com origem remota"));
        };
        // A raiz local subiria o sistema inteiro; não é uma seleção válida.
        if path.parent().is_none() {
            return Err(JobError::failed("selecione uma pasta"));
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| JobError::failed(format!("nome inválido: {}", path.display())))?;
        let remote = join_remote(&dest_dir, name);
        plan_local_selection(path, &remote, job, &mut plan)?;
    }
    Ok(plan)
}

/// Planeja uma origem local escolhida sozinha. Link simbólico só é seguido
/// quando o alvo é arquivo regular; link para pasta é recusado.
fn plan_local_selection(path: &Path, remote: &str, job: &Job, plan: &mut Plan) -> JobResult<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| JobError::failed(format!("lendo {}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::metadata(path)
            .map_err(|error| JobError::failed(format!("lendo link {}: {error}", path.display())))?;
        if target.is_file() {
            plan_upload_file(path, remote, target.len(), plan);
            return Ok(());
        }
        if target.is_dir() {
            return Err(JobError::failed(format!(
                "link simbólico para pasta não é suportado: {}",
                path.display()
            )));
        }
        return Err(JobError::failed(format!(
            "link simbólico para tipo não suportado: {}",
            path.display()
        )));
    }
    if metadata.is_dir() {
        plan_upload_dir(path, remote, plan);
        walk_local(path, remote, job, plan)
    } else if metadata.is_file() {
        plan_upload_file(path, remote, metadata.len(), plan);
        Ok(())
    } else {
        Err(JobError::failed(format!(
            "tipo não suportado: {}",
            path.display()
        )))
    }
}

fn plan_upload_dir(path: &Path, remote: &str, plan: &mut Plan) {
    plan.items.push(PlannedItem {
        source: path.display().to_string(),
        remote: remote.to_string(),
        local: path.to_path_buf(),
        kind: PlannedKind::Directory,
        size: 0,
    });
}

fn plan_upload_file(path: &Path, remote: &str, size: u64, plan: &mut Plan) {
    plan.items.push(PlannedItem {
        source: path.display().to_string(),
        remote: remote.to_string(),
        local: path.to_path_buf(),
        kind: PlannedKind::File,
        size,
    });
    plan.bytes_total += size;
}

/// Caminha uma pasta local sem seguir symlink. O destino remoto precisa de
/// nomes UTF-8 porque o protocolo SFTP transporta caminhos como String.
fn walk_local(path: &Path, remote: &str, job: &Job, plan: &mut Plan) -> JobResult<()> {
    check_cancel(job)?;
    let mut entries = Vec::new();
    let dir = std::fs::read_dir(path)
        .map_err(|error| JobError::failed(format!("listando {}: {error}", path.display())))?;
    for entry in dir {
        let entry = entry
            .map_err(|error| JobError::failed(format!("listando {}: {error}", path.display())))?;
        entries.push(entry);
    }
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        check_cancel(job)?;
        let child = entry.path();
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| JobError::failed(format!("nome inválido: {}", child.display())))?;
        let metadata = std::fs::symlink_metadata(&child)
            .map_err(|error| JobError::failed(format!("lendo {}: {error}", child.display())))?;
        let child_remote = join_remote(remote, &name);
        if metadata.file_type().is_symlink() {
            plan.ignored_links += 1;
        } else if metadata.is_dir() {
            plan_upload_dir(&child, &child_remote, plan);
            walk_local(&child, &child_remote, job, plan)?;
        } else if metadata.is_file() {
            plan_upload_file(&child, &child_remote, metadata.len(), plan);
        } else {
            return Err(JobError::failed(format!(
                "tipo não suportado: {}",
                child.display()
            )));
        }
    }
    Ok(())
}

/// Garante que o destino remoto de uma pasta existe, mesclando uma pasta já
/// existente e recusando arquivo (ou link) no lugar dela.
async fn ensure_remote_dir(session: &SftpSession, path: &str) -> JobResult<()> {
    match session.symlink_metadata(path.to_string()).await {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(JobError::failed(format!(
            "destino remoto é um link simbólico: {path}"
        ))),
        Ok(_) => Err(JobError::failed(format!(
            "destino remoto é um arquivo, não pasta: {path}"
        ))),
        Err(_) => session
            .create_dir(path.to_string())
            .await
            .map_err(|error| JobError::failed(format!("criando {path}: {error}"))),
    }
}

/// Garante que o destino remoto de um arquivo não é pasta (nem link) e
/// devolve o tamanho do arquivo existente, quando houver conflito. O lstat
/// vem antes de qualquer try_exists: try_exists segue links, então um link
/// quebrado pareceria ausente e o open com CREATE|TRUNCATE escreveria no alvo.
async fn ensure_remote_file(session: &SftpSession, path: &str) -> JobResult<Option<u64>> {
    match session.symlink_metadata(path.to_string()).await {
        Ok(metadata) if metadata.file_type().is_dir() => Err(JobError::failed(format!(
            "destino remoto é uma pasta, não arquivo: {path}"
        ))),
        Ok(metadata) if metadata.file_type().is_symlink() => Err(JobError::failed(format!(
            "destino remoto é um link simbólico: {path}"
        ))),
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => Ok(None),
        Err(error) => Err(JobError::failed(format!("lendo {path}: {error}"))),
    }
}

async fn upload_file(
    session: &SftpSession,
    job: &Job,
    file: &PlannedItem,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u64) -> Job {
        Job::download(id, Vec::new(), PathBuf::new(), ConflictPolicy::Overwrite)
    }

    #[test]
    fn job_queue_limits_three() {
        let mut q = JobQueue::new(3);
        for id in 1..=4 {
            q.push(job(id));
        }
        assert_eq!(q.next().unwrap().id, 1);
        assert_eq!(q.next().unwrap().id, 2);
        assert_eq!(q.next().unwrap().id, 3);
        assert!(q.next().is_none());
        q.finish();
        assert_eq!(q.next().unwrap().id, 4);
    }

    /// Cancelar pela fila só remove o que ainda não começou; um job em
    /// andamento é responsabilidade do mapa de jobs do ator.
    #[test]
    fn job_queue_cancel_only_removes_pending() {
        let mut q = JobQueue::new(3);
        q.push(job(1));
        q.push(job(2));
        assert_eq!(q.next().unwrap().id, 1);
        assert!(q.cancel(2));
        assert!(!q.cancel(2), "job já removido da fila");
        assert!(!q.cancel(1), "job em andamento não está na fila");
        assert!(q.next().is_none());
        q.finish();
        assert!(q.next().is_none());
    }

    /// Cobre a falha da Task 4: um JoinError (pânico) não pode deixar o job
    /// preso no mapa nem ficar sem JobDone.
    #[tokio::test]
    async fn join_error_fails_job_and_releases_slot() {
        let mut queue = JobQueue::new(1);
        queue.push(job(1));
        assert_eq!(queue.next().unwrap().id, 1);
        queue.push(job(2));

        let mut tasks: JoinSet<u64> = JoinSet::new();
        let mut task_ids: HashMap<tokio::task::Id, u64> = HashMap::new();
        let mut jobs: HashMap<u64, RunningJob> = HashMap::new();
        let handle = tasks.spawn(async move {
            panic!("falha proposital");
        });
        task_ids.insert(handle.id(), 1);
        let (decision_tx, _decision_rx) = tokio::sync::mpsc::unbounded_channel();
        jobs.insert(
            1,
            RunningJob {
                cancel: Arc::new(AtomicBool::new(false)),
                wake: Arc::new(Notify::new()),
                decisions: decision_tx,
            },
        );

        let finished = tasks
            .join_next()
            .await
            .expect("task deveria estar no JoinSet");
        let (tx, rx) = channel();
        handle_finished(finished, &mut jobs, &mut task_ids, &mut queue, &tx);
        match rx.try_recv().expect("JobDone da task que falhou") {
            SftpEvent::JobDone {
                id: 1,
                outcome: JobOutcome::Failed(_),
            } => {}
            other => panic!("esperava JobDone::Failed, veio {other:?}"),
        }
        assert!(jobs.is_empty(), "o job falho ficou no mapa");
        assert!(task_ids.is_empty(), "o id da task falha ficou no mapa");
        assert_eq!(queue.next().unwrap().id, 2, "o slot não foi liberado");
    }

    #[test]
    fn job_queue_drain_empties_pending() {
        let mut q = JobQueue::new(1);
        q.push(job(1));
        q.push(job(2));
        q.push(job(3));
        assert_eq!(q.next().unwrap().id, 1);
        let drained: Vec<u64> = q.drain().into_iter().map(|job| job.id).collect();
        assert_eq!(drained, vec![2, 3]);
        assert!(q.next().is_none());
    }
}
