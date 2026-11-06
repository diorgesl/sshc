//! Teste ponta a ponta do cliente SSH contra um servidor russh local.

use russh::server::{self, Auth, ChannelOpenHandle, Msg, Session as ServerSession, Server as _};
use russh::{Channel, ChannelId};
use sshc_core::Protocol;
use sshc_net::{connect, Credentials};
use std::sync::Arc;
use std::time::Duration;

const TEST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\n\
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n\
QyNTUxOQAAACC9mauQz3+ny7HSOfdhHx4vW+khzVHAx27y6eeyjLwhUgAAAJAV7SyBFe0s\n\
gQAAAAtzc2gtZWQyNTUxOQAAACC9mauQz3+ny7HSOfdhHx4vW+khzVHAx27y6eeyjLwhUg\n\
AAAEDM51WOxUe9g61Q9jDqvO3DVhUaC3D/nyCTeo7H9SpQ4r2Zq5DPf6fLsdI592EfHi9b\n\
6SHNUcDHbvLp57KMvCFSAAAACXNzaGMtdGVzdAECAwQ=\n\
-----END OPENSSH PRIVATE KEY-----\n";

#[derive(Clone, Default)]
struct Echo {
    channels: Arc<std::sync::Mutex<std::collections::HashMap<ChannelId, Channel<Msg>>>>,
}

impl server::Server for Echo {
    type Handler = Echo;

    fn new_client(&mut self, addr: Option<std::net::SocketAddr>) -> Echo {
        self.clone()
    }
}

impl server::Handler for Echo {
    type Error = russh::Error;

    async fn auth_password(
        &mut self,
        _user: &str,
        _password: &str,
    ) -> Result<Auth, Self::Error> {
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        // O canal precisa continuar vivo: guardamos como nos exemplos do russh.
        {
            let mut channels = self.channels.lock().unwrap();
            channels.insert(channel.id(), channel);
        }
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel);
        session.data(channel, b"exec-ok\r\n".to_vec())?;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        _col_width: u32,
        _row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel);
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        session.request_success();
        session.data(channel, b"echo-banner\r\n".to_vec())?;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut ServerSession,
    ) -> Result<(), Self::Error> {
        session.data(channel, data.to_vec())?;
        Ok(())
    }
}

/// Os testes mexem em variável de ambiente global; serializa para não correr.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Espera dados sem bloquear a thread: o servidor roda neste mesmo runtime.
async fn recv(conn: &sshc_net::Connection, timeout: Duration) -> Vec<u8> {
    let start = std::time::Instant::now();
    loop {
        if let Ok(chunk) = conn.output.try_recv() {
            return chunk;
        }
        assert!(start.elapsed() < timeout, "tempo esgotado esperando dados");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn ssh_password_roundtrip() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("host_key");
    std::fs::write(&key_path, TEST_KEY).unwrap();
    let host_key = russh::keys::load_secret_key(&key_path, None).unwrap();

    let config = Arc::new(server::Config {
        keys: vec![host_key],
        inactivity_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut server = Echo::default();
    tokio::spawn(async move {
        let _ = server.run_on_socket(config, &listener).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;

    let known_hosts = dir.path().join("known_hosts");
    std::env::set_var("SSHC_KNOWN_HOSTS", &known_hosts);

    let session = sshc_core::Session {
        name: "echo".into(),
        protocol: Protocol::Ssh,
        host: "127.0.0.1".into(),
        port,
        user: Some("teste".into()),
        emulation: "xterm-256color".into(),
        auth: None,
        description: None,
        baud: None,
    };

    let conn = tokio::task::spawn_blocking(move || {
        connect(&session, Some(Credentials::Password("senha".into())))
    })
    .await
    .unwrap()
    .unwrap();

    let banner = recv(&conn, Duration::from_secs(10)).await;
    assert_eq!(banner, b"echo-banner\r\n");

    conn.send(b"ping\r\n");
    let mut echoed = Vec::new();
    while echoed.len() < 6 {
        let chunk = recv(&conn, Duration::from_secs(10)).await;
        echoed.extend_from_slice(&chunk);
    }
    assert_eq!(echoed, b"ping\r\n");

    let known = std::fs::read_to_string(&known_hosts).unwrap();
    assert!(known.contains("127.0.0.1"), "host key deveria ser aprendida: {known}");
}

#[tokio::test]
async fn ssh_wrong_key_fails() {
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("host_key");
    std::fs::write(&key_path, TEST_KEY).unwrap();
    let host_key = russh::keys::load_secret_key(&key_path, None).unwrap();
    let config = Arc::new(server::Config {
        keys: vec![host_key],
        inactivity_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut server = Echo::default();
    tokio::spawn(async move {
        let _ = server.run_on_socket(config, &listener).await;
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    std::env::set_var("SSHC_KNOWN_HOSTS", dir.path().join("known_hosts"));

    let session = sshc_core::Session {
        name: "echo".into(),
        protocol: Protocol::Ssh,
        host: "127.0.0.1".into(),
        port,
        user: Some("teste".into()),
        emulation: "xterm-256color".into(),
        auth: None,
        description: None,
        baud: None,
    };
    let missing = dir.path().join("nao-existe");
    let err = tokio::task::spawn_blocking(move || {
        connect(&session, Some(Credentials::Key { path: missing, passphrase: None }))
    })
    .await
    .unwrap()
    .err()
    .expect("conectar com chave inexistente deveria falhar");
    assert!(format!("{err:#}").contains("chave"), "{err:#}");
}
