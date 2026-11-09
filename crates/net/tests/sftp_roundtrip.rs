//! Teste ponta a ponta do ator SFTP contra um servidor russh local.
// O lock serializa variáveis de ambiente globais; pode atravessar await aqui.
#![allow(clippy::await_holding_lock)]

use russh::server::{self, Auth, ChannelOpenHandle, Msg, Server as _, Session as ServerSession};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, File, FileAttributes, FileMode, Handle, Name, Status, StatusCode, Version,
};
use sshc_core::Protocol;
use sshc_net::sftp::{normalize_remote, EntryKind, SftpCmd, SftpConnection, SftpEvent};
use sshc_net::Credentials;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    let server = TestSftpServer::start(dir.path()).await;
    std::env::set_var("SSHC_KNOWN_HOSTS", dir.path().join("known_hosts"));
    let session = test_session(server.port());
    let conn = tokio::task::spawn_blocking(move || {
        sshc_net::sftp::connect(&session, Credentials::Password("x".into()))
    })
    .await
    .unwrap()
    .unwrap();

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
}

#[test]
fn normalize_remote_collapses_and_clamps() {
    assert_eq!(normalize_remote("/a//b/../c/").as_deref(), Some("/a/c"));
    assert_eq!(normalize_remote("/..").as_deref(), Some("/"));
    assert_eq!(normalize_remote("relativo"), None);
    assert_eq!(normalize_remote(""), None);
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
        let mut server = FixtureServer {
            root: root.to_path_buf(),
            channels: Arc::new(Mutex::new(HashMap::new())),
        };
        let task = tokio::spawn(async move {
            let _ = server.run_on_socket(config, &listener).await;
        });
        tokio::time::sleep(Duration::from_millis(150)).await;

        Self { port, task }
    }

    fn port(&self) -> u16 {
        self.port
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
}

impl server::Server for FixtureServer {
    type Handler = FixtureSession;

    fn new_client(&mut self, _addr: Option<std::net::SocketAddr>) -> FixtureSession {
        FixtureSession {
            root: self.root.clone(),
            channels: self.channels.clone(),
        }
    }
}

struct FixtureSession {
    root: PathBuf,
    channels: Channels,
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
        russh_sftp::server::run(channel.into_stream(), SftpFs::new(self.root.clone())).await;
        Ok(())
    }
}

struct SftpFs {
    root: PathBuf,
    dirs: HashMap<String, VecDeque<File>>,
}

impl SftpFs {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            dirs: HashMap::new(),
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

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
        self.dirs.remove(&handle);
        Ok(Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".to_string(),
            language_tag: "en-US".to_string(),
        })
    }
}
