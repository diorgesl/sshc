//! Teste ponta a ponta do ator SFTP contra um servidor russh local.
// O lock serializa variáveis de ambiente globais; pode atravessar await aqui.
#![allow(clippy::await_holding_lock)]

use russh::server::{self, Auth, ChannelOpenHandle, Msg, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, FileMode, Handle, Name, OpenFlags, Status, StatusCode,
    Version,
};
use sshc_core::Protocol;
use sshc_net::sftp::{
    normalize_remote, ConflictPolicy, EntryKind, JobOutcome, SftpCmd, SftpConnection, SftpEvent,
};
use sshc_net::Credentials;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

const TEST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACC9mauQz3+ny7HSOfdhHx4vW+khzVHAx27y6eeyjLwhUgAAAJAV7SyBFe0s
gQAAAAtzc2gtZWQyNTUxOQAAACC9mauQz3+ny7HSOfdhHx4vW+khzVHAx27y6eeyjLwhUg
AAAEDM51WOxUe9g61Q9jDqvO3DVhUaC3D/nyCTeo7H9SpQ4r2Zq5DPf6fLsdI592EfHi9b
6SHNUcDHbvLp57KMvCFSAAAACXNzaGMtdGVzdAECAwQ=
-----END OPENSSH PRIVATE KEY-----
";

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[tokio::test]
async fn connect_lists_files() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), b"hi").unwrap();
    std::fs::create_dir(dir.path().join("pasta")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert_eq!(conn.home.as_deref(), Some("/"));
    assert!(conn.send(SftpCmd::List {
        id: 1,
        path: "/".into()
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::Listed {
            id: 1,
            path,
            result,
        } => {
            assert_eq!(path, "/");
            let entries = result.unwrap();
            assert!(entries
                .iter()
                .any(|e| e.name == "hello.txt" && e.kind == EntryKind::File));
            assert!(entries
                .iter()
                .any(|e| e.name == "pasta" && e.kind == EntryKind::Dir));
        }
        other => panic!("esperava Listed, veio {other:?}"),
    }

    // "~" resolve para o home que o servidor informou (realpath "." = "/").
    assert!(conn.send(SftpCmd::List {
        id: 2,
        path: "~".into()
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::Listed {
            id: 2,
            path,
            result,
        } => {
            assert_eq!(path, "/");
            assert!(result
                .unwrap()
                .iter()
                .any(|e| e.name == "hello.txt" && e.kind == EntryKind::File));
        }
        other => panic!("esperava Listed, veio {other:?}"),
    }
}

#[tokio::test]
async fn mkdir_rename_delete_roundtrip() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("pasta")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    op(
        &conn,
        SftpCmd::Mkdir {
            id: 1,
            path: "/nova".into(),
        },
    )
    .await
    .unwrap();
    assert!(dir.path().join("nova").is_dir());

    op(
        &conn,
        SftpCmd::Rename {
            id: 2,
            from: "/nova".into(),
            to: "/velha".into(),
        },
    )
    .await
    .unwrap();
    assert!(dir.path().join("velha").is_dir());

    op(
        &conn,
        SftpCmd::Delete {
            id: 3,
            paths: vec!["/velha".into()],
            recursive: true,
        },
    )
    .await
    .unwrap();
    assert!(!dir.path().join("velha").exists());

    // Erro do servidor vira OpDone::Err com o motivo.
    let error = op(
        &conn,
        SftpCmd::Mkdir {
            id: 4,
            path: "/pasta".into(),
        },
    )
    .await
    .unwrap_err();
    assert!(error.contains("criando /pasta"), "erro inesperado: {error}");
    assert!(dir.path().join("pasta").is_dir());

    // Delete não recursivo de arquivo.
    std::fs::write(dir.path().join("solto.txt"), b"x").unwrap();
    op(
        &conn,
        SftpCmd::Delete {
            id: 5,
            paths: vec!["/solto.txt".into()],
            recursive: false,
        },
    )
    .await
    .unwrap();
    assert!(!dir.path().join("solto.txt").exists());
}

#[tokio::test]
async fn delete_recursive_removes_tree() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("arvore/sub")).unwrap();
    std::fs::write(dir.path().join("arvore/a.txt"), b"a").unwrap();
    std::fs::write(dir.path().join("arvore/sub/b.txt"), b"b").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    // Sem recursão, pasta não vazia falha e nada é removido.
    let error = op(
        &conn,
        SftpCmd::Delete {
            id: 1,
            paths: vec!["/arvore".into()],
            recursive: false,
        },
    )
    .await
    .unwrap_err();
    assert!(
        error.contains("apagando /arvore"),
        "erro inesperado: {error}"
    );
    assert!(dir.path().join("arvore/sub/b.txt").exists());

    op(
        &conn,
        SftpCmd::Delete {
            id: 2,
            paths: vec!["/arvore".into()],
            recursive: true,
        },
    )
    .await
    .unwrap();
    assert!(!dir.path().join("arvore").exists());
}

#[tokio::test]
async fn delete_root_is_rejected() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("sentinela.txt"), b"x").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    // "/.." normaliza para "/": nenhum dos dois pode esvaziar a raiz remota.
    for (id, path) in [(1u64, "/"), (2u64, "/..")] {
        let error = op(
            &conn,
            SftpCmd::Delete {
                id,
                paths: vec![path.into()],
                recursive: true,
            },
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("não é seguro apagar a raiz /"),
            "erro inesperado: {error}"
        );
        assert!(
            dir.path().join("sentinela.txt").exists(),
            "a raiz remota foi alterada por Delete {path}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn chmod_sets_mode() {
    use std::os::unix::fs::PermissionsExt;

    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("arquivo.txt");
    std::fs::write(&file, b"x").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    op(
        &conn,
        SftpCmd::Chmod {
            id: 1,
            paths: vec!["/arquivo.txt".into()],
            mode: 0o600,
        },
    )
    .await
    .unwrap();

    let metadata = std::fs::metadata(&file).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);

    assert!(conn.send(SftpCmd::List {
        id: 2,
        path: "/".into()
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::Listed { id: 2, result, .. } => {
            let entries = result.unwrap();
            let entry = entries.iter().find(|e| e.name == "arquivo.txt").unwrap();
            assert_eq!(entry.mode, Some(0o600));
        }
        other => panic!("esperava Listed, veio {other:?}"),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_listing_shows_target() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alvo.txt"), b"x").unwrap();
    std::os::unix::fs::symlink("alvo.txt", dir.path().join("link")).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::List {
        id: 1,
        path: "/".into()
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::Listed { id: 1, result, .. } => {
            let entries = result.unwrap();
            let link = entries.iter().find(|e| e.name == "link").unwrap();
            assert_eq!(link.kind, EntryKind::Symlink);
            assert_eq!(link.link_target.as_deref(), Some("alvo.txt"));
            let target = entries.iter().find(|e| e.name == "alvo.txt").unwrap();
            assert_eq!(target.kind, EntryKind::File);
            assert_eq!(target.link_target, None);
        }
        other => panic!("esperava Listed, veio {other:?}"),
    }
}

#[tokio::test]
async fn upload_download_file_roundtrip() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let local_in = tempfile::tempdir().unwrap();
    let local_out = tempfile::tempdir().unwrap();
    let bytes = b"conteudo".repeat(1000);
    std::fs::write(local_in.path().join("a.bin"), &bytes).unwrap();
    let (_server, conn) = connect(dir.path()).await;

    start_upload(&conn, 10, vec![local_in.path().join("a.bin")], "/").await;
    assert_eq!(std::fs::read(dir.path().join("a.bin")).unwrap(), bytes);

    start_download(&conn, 11, vec!["/a.bin".into()], local_out.path()).await;
    assert_eq!(
        std::fs::read(local_out.path().join("a.bin")).unwrap(),
        bytes
    );
    assert!(
        !local_out.path().join("a.bin.part").exists(),
        "o download deixou o .part para trás"
    );
}

#[tokio::test]
async fn cancel_running_download_removes_part() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let size = 4 * 1024 * 1024;
    std::fs::write(dir.path().join("slow.bin"), vec![7u8; size]).unwrap();
    let (server, conn) = connect(dir.path()).await;

    // A fixture segura as leituras: o job fica parado no primeiro chunk.
    server.control().pause_reads();
    assert!(conn.send(SftpCmd::Download {
        job_id: 42,
        sources: vec!["/slow.bin".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    match recv_event(&conn, Duration::from_secs(10)).await {
        SftpEvent::JobStarted {
            id,
            items_total,
            bytes_total,
        } => {
            assert_eq!(id, 42);
            assert_eq!(items_total, 1);
            assert_eq!(bytes_total, Some(size as u64));
        }
        other => panic!("esperava JobStarted, veio {other:?}"),
    }

    let part = out.path().join("slow.bin.part");
    wait_until(Duration::from_secs(10), || part.exists()).await;
    assert!(conn.send(SftpCmd::Cancel { job_id: 42 }));
    assert_eq!(wait_done(&conn, 42).await, JobOutcome::Cancelled);
    assert!(!part.exists(), "o .part ficou no destino após cancelar");
    assert!(!out.path().join("slow.bin").exists());
}

#[tokio::test]
async fn paused_download_resumes_after_reads_are_released() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..200_000).map(|index| (index % 251) as u8).collect();
    std::fs::write(dir.path().join("lento.bin"), &bytes).unwrap();
    let (server, conn) = connect(dir.path()).await;

    server.control().pause_reads();
    assert!(conn.send(SftpCmd::Download {
        job_id: 7,
        sources: vec!["/lento.bin".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    let part = out.path().join("lento.bin.part");
    wait_until(Duration::from_secs(10), || part.exists()).await;
    server.control().resume_reads();

    let outcome = wait_job(&conn, 7).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
    assert_eq!(std::fs::read(out.path().join("lento.bin")).unwrap(), bytes);
    assert!(!part.exists());
}

#[tokio::test]
async fn download_directory_source_fails_until_recursive_arrives() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("pasta")).unwrap();
    std::fs::write(dir.path().join("pasta/a.txt"), b"a").unwrap();
    let (_server, conn) = connect(dir.path()).await;

    assert!(conn.send(SftpCmd::Download {
        job_id: 9,
        sources: vec!["/pasta".into()],
        dest: out.path().to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    assert_eq!(
        wait_done(&conn, 9).await,
        JobOutcome::Failed("pasta ainda não suportada".into())
    );
    assert!(!out.path().join("pasta.part").exists());
}

#[test]
fn normalize_remote_collapses_and_clamps() {
    assert_eq!(normalize_remote("/a//b/../c/").as_deref(), Some("/a/c"));
    assert_eq!(normalize_remote("/..").as_deref(), Some("/"));
    assert_eq!(normalize_remote("relativo"), None);
    assert_eq!(normalize_remote(""), None);
}

async fn connect(dir: &Path) -> (TestSftpServer, SftpConnection) {
    let server = TestSftpServer::start(dir).await;
    std::env::set_var("SSHC_KNOWN_HOSTS", dir.join("known_hosts"));
    let session = test_session(server.port());
    let conn = tokio::task::spawn_blocking(move || {
        sshc_net::sftp::connect(&session, Credentials::Password("x".into()))
    })
    .await
    .unwrap()
    .unwrap();
    (server, conn)
}

async fn op(conn: &SftpConnection, command: SftpCmd) -> Result<(), String> {
    let id = match &command {
        SftpCmd::Mkdir { id, .. }
        | SftpCmd::Rename { id, .. }
        | SftpCmd::Delete { id, .. }
        | SftpCmd::Chmod { id, .. } => *id,
        other => panic!("comando sem OpDone: {other:?}"),
    };
    assert!(conn.send(command), "conexão SFTP encerrada");
    match recv_event(conn, Duration::from_secs(10)).await {
        SftpEvent::OpDone { id: done, result } => {
            assert_eq!(done, id, "OpDone com id diferente do comando");
            result
        }
        other => panic!("esperava OpDone, veio {other:?}"),
    }
}

async fn start_upload(conn: &SftpConnection, job_id: u64, sources: Vec<PathBuf>, dest: &str) {
    assert!(conn.send(SftpCmd::Upload {
        job_id,
        sources,
        dest: dest.to_string(),
        policy: ConflictPolicy::Overwrite,
    }));
    let outcome = wait_job(conn, job_id).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
}

async fn start_download(conn: &SftpConnection, job_id: u64, sources: Vec<String>, dest: &Path) {
    assert!(conn.send(SftpCmd::Download {
        job_id,
        sources,
        dest: dest.to_path_buf(),
        policy: ConflictPolicy::Overwrite,
    }));
    let outcome = wait_job(conn, job_id).await;
    assert_eq!(outcome, JobOutcome::Completed { ignored_links: 0 });
}

/// Consome eventos até JobDone e exige que JobStarted tenha vindo antes.
async fn wait_job(conn: &SftpConnection, job_id: u64) -> JobOutcome {
    let mut started = false;
    loop {
        match recv_event(conn, Duration::from_secs(10)).await {
            SftpEvent::JobStarted {
                id, items_total, ..
            } => {
                assert_eq!(id, job_id, "JobStarted com id diferente");
                assert_eq!(items_total, 1);
                started = true;
            }
            SftpEvent::JobProgress { id, .. } => assert_eq!(id, job_id),
            SftpEvent::JobDone { id, outcome } => {
                assert_eq!(id, job_id, "JobDone com id diferente");
                assert!(started, "JobDone sem JobStarted");
                return outcome;
            }
            other => panic!("esperava eventos do job {job_id}, veio {other:?}"),
        }
    }
}

/// Consome eventos até JobDone, sem exigir JobStarted (jobs que falham no
/// planejamento não iniciam).
async fn wait_done(conn: &SftpConnection, job_id: u64) -> JobOutcome {
    loop {
        match recv_event(conn, Duration::from_secs(10)).await {
            SftpEvent::JobStarted { id, .. } | SftpEvent::JobProgress { id, .. } => {
                assert_eq!(id, job_id);
            }
            SftpEvent::JobDone { id, outcome } => {
                assert_eq!(id, job_id, "JobDone com id diferente");
                return outcome;
            }
            other => panic!("esperava eventos do job {job_id}, veio {other:?}"),
        }
    }
}

async fn wait_until(timeout: Duration, condition: impl Fn() -> bool) {
    let start = std::time::Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < timeout,
            "tempo esgotado esperando condição"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn test_session(port: u16) -> sshc_core::Session {
    sshc_core::Session {
        name: "sftp".into(),
        protocol: Protocol::Ssh,
        host: "127.0.0.1".into(),
        port,
        user: Some("teste".into()),
        emulation: "xterm-256color".into(),
        auth: None,
        description: None,
        baud: None,
    }
}

async fn recv_event(conn: &SftpConnection, timeout: Duration) -> SftpEvent {
    let start = std::time::Instant::now();
    loop {
        match conn.events.try_recv() {
            Ok(event) => return event,
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                assert!(
                    start.elapsed() < timeout,
                    "tempo esgotado esperando evento SFTP"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                panic!("conexão SFTP encerrou sem evento")
            }
        }
    }
}

struct TestSftpServer {
    port: u16,
    task: tokio::task::JoinHandle<()>,
    control: TestControl,
}

impl TestSftpServer {
    async fn start(root: &Path) -> Self {
        let host_key = russh::keys::PrivateKey::from_openssh(TEST_KEY).unwrap();
        let config = Arc::new(server::Config {
            keys: vec![host_key],
            inactivity_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        });

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let control = TestControl::default();
        let mut server = FixtureServer {
            root: root.to_path_buf(),
            channels: Arc::new(Mutex::new(HashMap::new())),
            control: control.clone(),
        };
        let task = tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });
        tokio::time::sleep(Duration::from_millis(150)).await;

        Self {
            port,
            task,
            control,
        }
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn control(&self) -> &TestControl {
        &self.control
    }
}

/// Controle de pausa compartilhado entre teste e fixture. Sem ele o
/// cancelamento de um download em andamento não seria determinístico.
#[derive(Clone, Default)]
struct TestControl {
    paused: Arc<AtomicBool>,
    reads_resumed: Arc<Notify>,
}

impl TestControl {
    fn pause_reads(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    fn resume_reads(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.reads_resumed.notify_waiters();
    }

    /// Bloqueia enquanto a pausa estiver ativa. O enable registra o waiter
    /// antes da última checagem, então um resume concorrente não é perdido.
    async fn wait_reads(&self) {
        loop {
            if !self.paused.load(Ordering::SeqCst) {
                return;
            }
            let notified = self.reads_resumed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !self.paused.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for TestSftpServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Channels = Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>;

#[derive(Clone)]
struct FixtureServer {
    root: PathBuf,
    channels: Channels,
    control: TestControl,
}

impl server::Server for FixtureServer {
    type Handler = FixtureSession;

    fn new_client(&mut self, _addr: Option<std::net::SocketAddr>) -> FixtureSession {
        FixtureSession {
            root: self.root.clone(),
            channels: self.channels.clone(),
            control: self.control.clone(),
        }
    }
}

struct FixtureSession {
    root: PathBuf,
    channels: Channels,
    control: TestControl,
}

impl server::Handler for FixtureSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        self.channels.lock().unwrap().insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        let channel = self.channels.lock().unwrap().remove(&channel_id);
        let Some(channel) = channel else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        session.channel_success(channel_id)?;
        russh_sftp::server::run(
            channel.into_stream(),
            SftpFs::new(self.root.clone(), self.control.clone()),
        )
        .await;
        Ok(())
    }
}

struct SftpFs {
    root: PathBuf,
    control: TestControl,
    dirs: HashMap<String, VecDeque<File>>,
    files: HashMap<String, std::fs::File>,
}

impl SftpFs {
    fn new(root: PathBuf, control: TestControl) -> Self {
        Self {
            root,
            control,
            dirs: HashMap::new(),
            files: HashMap::new(),
        }
    }

    /// Mapeia um caminho remoto absoluto para o tempdir do servidor. Recusa
    /// travessia (..), NUL, caminhos relativos e componentes que escapem da raiz.
    fn local(&self, path: &str) -> Result<PathBuf, StatusCode> {
        if path.is_empty() || path.contains('\0') {
            return Err(StatusCode::NoSuchFile);
        }
        if path.split('/').any(|part| part == "..") {
            return Err(StatusCode::NoSuchFile);
        }
        let normalized = normalize_remote(path).ok_or(StatusCode::NoSuchFile)?;
        let mut local = self.root.clone();
        for part in normalized.trim_start_matches('/').split('/') {
            if part.is_empty() {
                continue;
            }
            let component = Path::new(part);
            if component.is_absolute() || component.components().count() != 1 {
                return Err(StatusCode::NoSuchFile);
            }
            local.push(part);
            if !local.starts_with(&self.root) {
                return Err(StatusCode::NoSuchFile);
            }
        }
        Ok(local)
    }
}

fn status_code(error: &std::io::Error) -> StatusCode {
    match error.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en-US".to_string(),
    }
}

fn attributes(metadata: &std::fs::Metadata) -> FileAttributes {
    let mut attrs = FileAttributes::from(metadata);
    if metadata.file_type().is_symlink() {
        let permissions = attrs.permissions.unwrap_or(0) & 0o7777;
        attrs.permissions = Some(FileMode::LNK.bits() | permissions);
    }
    attrs
}

impl russh_sftp::server::Handler for SftpFs {
    type Error = StatusCode;

    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        attrs: FileAttributes,
    ) -> Result<Handle, StatusCode> {
        let local = self.local(&filename)?;
        let file = std::fs::OpenOptions::from(pflags)
            .open(&local)
            .map_err(|error| status_code(&error))?;
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|error| status_code(&error))?;
        }
        #[cfg(not(unix))]
        let _ = attrs;
        let handle = format!("file-{id}");
        self.files.insert(handle.clone(), file);
        Ok(Handle { id, handle })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, StatusCode> {
        // Pausa antes de qualquer byte: é o que torna o cancelamento
        // determinístico no teste.
        let control = self.control.clone();
        control.wait_reads().await;

        let Some(file) = self.files.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| status_code(&error))?;
        // Limita o pacote de resposta (o cliente pode pedir 256 KiB).
        let mut data = vec![0u8; (len as usize).min(64 * 1024)];
        let read = file.read(&mut data).map_err(|error| status_code(&error))?;
        if read == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(read);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        let Some(file) = self.files.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| status_code(&error))?;
        file.write_all(&data).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        let Some(file) = self.files.get(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        let metadata = file.metadata().map_err(|error| status_code(&error))?;
        Ok(Attrs {
            id,
            attrs: attributes(&metadata),
        })
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, StatusCode> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, _path: String) -> Result<Name, StatusCode> {
        Ok(Name {
            id,
            files: vec![File::dummy("/")],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, StatusCode> {
        let local = self.local(&path)?;
        let metadata = std::fs::metadata(&local).map_err(|error| status_code(&error))?;
        if !metadata.is_dir() {
            return Err(StatusCode::Failure);
        }

        let mut files = Vec::new();
        let entries = std::fs::read_dir(&local).map_err(|error| status_code(&error))?;
        for entry in entries {
            let entry = entry.map_err(|error| status_code(&error))?;
            let metadata =
                std::fs::symlink_metadata(entry.path()).map_err(|error| status_code(&error))?;
            files.push(File::new(
                entry.file_name().to_string_lossy().into_owned(),
                attributes(&metadata),
            ));
        }
        files.sort_by(|left, right| left.filename.cmp(&right.filename));

        let handle = format!("dir-{id}");
        self.dirs.insert(handle.clone(), files.into());
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusCode> {
        let Some(files) = self.dirs.get_mut(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        if files.is_empty() {
            return Err(StatusCode::Eof);
        }
        let files = files.drain(..).collect();
        Ok(Name { id, files })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let local = self.local(&path)?;
        let metadata = std::fs::symlink_metadata(&local).map_err(|error| status_code(&error))?;
        Ok(Attrs {
            id,
            attrs: attributes(&metadata),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        let local = self.local(&path)?;
        let metadata = std::fs::metadata(&local).map_err(|error| status_code(&error))?;
        Ok(Attrs {
            id,
            attrs: attributes(&metadata),
        })
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, StatusCode> {
        let local = self.local(&path)?;
        std::fs::create_dir(&local).map_err(|error| status_code(&error))?;
        #[cfg(unix)]
        if let Some(mode) = _attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|error| status_code(&error))?;
        }
        Ok(ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
        let local = self.local(&path)?;
        std::fs::remove_dir(&local).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, StatusCode> {
        let local = self.local(&filename)?;
        std::fs::remove_file(&local).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, StatusCode> {
        let from = self.local(&oldpath)?;
        let to = self.local(&newpath)?;
        std::fs::rename(&from, &to).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, StatusCode> {
        let local = self.local(&path)?;
        #[cfg(unix)]
        if let Some(mode) = attrs.permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&local, std::fs::Permissions::from_mode(mode & 0o7777))
                .map_err(|error| status_code(&error))?;
        }
        #[cfg(not(unix))]
        if attrs.permissions.is_some() {
            return Err(StatusCode::OpUnsupported);
        }
        Ok(ok_status(id))
    }

    #[cfg(unix)]
    async fn symlink(
        &mut self,
        id: u32,
        linkpath: String,
        targetpath: String,
    ) -> Result<Status, StatusCode> {
        let link = self.local(&linkpath)?;
        std::os::unix::fs::symlink(&targetpath, &link).map_err(|error| status_code(&error))?;
        Ok(ok_status(id))
    }

    #[cfg(not(unix))]
    async fn symlink(
        &mut self,
        _id: u32,
        _linkpath: String,
        _targetpath: String,
    ) -> Result<Status, StatusCode> {
        Err(StatusCode::OpUnsupported)
    }

    #[cfg(unix)]
    async fn readlink(&mut self, id: u32, path: String) -> Result<Name, StatusCode> {
        let local = self.local(&path)?;
        let target = std::fs::read_link(&local).map_err(|error| status_code(&error))?;
        Ok(Name {
            id,
            files: vec![File::new(
                target.to_string_lossy().into_owned(),
                FileAttributes::dummy(),
            )],
        })
    }

    #[cfg(not(unix))]
    async fn readlink(&mut self, _id: u32, _path: String) -> Result<Name, StatusCode> {
        Err(StatusCode::OpUnsupported)
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
        self.dirs.remove(&handle);
        self.files.remove(&handle);
        Ok(ok_status(id))
    }
}
