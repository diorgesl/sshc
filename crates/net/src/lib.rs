//! Transportes. Cada conexão roda em threads próprias e conversa por canais:
//! o app nunca bloqueia em I/O.

pub mod ssh;

pub use ssh::{encrypted_key_path, is_auth_error};

use anyhow::{bail, Context, Result};
use sshc_core::{Protocol, Session};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;

/// O que o app manda para dentro da conexão.
#[derive(Debug, Clone)]
pub enum Input {
    Data(Vec<u8>),
    Resize(u16, u16),
}

#[derive(Debug, Clone)]
pub enum Credentials {
    /// O que o `ssh` faz: agent, IdentityFile do ~/.ssh/config e chaves padrão.
    Auto,
    Password(String),
    Key {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

pub struct Connection {
    pub output: Receiver<Vec<u8>>,
    input: Option<UnboundedSender<Input>>,
    stop: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    /// Aviso sobre o handshake (ex.: host key aprendida).
    pub note: String,
}

impl Connection {
    pub fn new(
        output: Receiver<Vec<u8>>,
        input: UnboundedSender<Input>,
        stop: Arc<AtomicBool>,
        closed: Arc<AtomicBool>,
        threads: Vec<JoinHandle<()>>,
        note: String,
    ) -> Self {
        Self {
            output,
            input: Some(input),
            stop,
            closed,
            threads,
            note,
        }
    }

    pub fn send(&self, bytes: &[u8]) -> bool {
        self.input
            .as_ref()
            .map(|tx| tx.send(Input::Data(bytes.to_vec())).is_ok())
            .unwrap_or(false)
    }

    pub fn resize(&self, cols: u16, rows: u16) -> bool {
        self.input
            .as_ref()
            .map(|tx| tx.send(Input::Resize(cols, rows)).is_ok())
            .unwrap_or(false)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Fecha o canal antes de esperar: as threads saem quando o recv falha.
        self.input.take();
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// Abre a sessão conforme o protocolo. SSH exige credenciais.
pub fn connect(session: &Session, creds: Option<Credentials>) -> Result<Connection> {
    match &session.protocol {
        Protocol::Ssh => {
            let creds = creds.context("sessão SSH precisa de senha ou chave")?;
            ssh::connect(session, creds, 120, 40)
        }
        Protocol::Telnet => telnet(&session.host, session.port),
        Protocol::Serial => serial(&session.host, session.baud.unwrap_or(9600)),
        other => bail!("sem transporte para {other:?}"),
    }
}

fn telnet(host: &str, port: u16) -> Result<Connection> {
    let addrs = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolvendo {host}:{port}"))?;
    let mut last_error = None;
    let mut connected = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(10)) {
            Ok(stream) => {
                connected = Some(stream);
                break;
            }
            Err(e) => last_error = Some(e),
        }
    }
    let stream = match connected {
        Some(stream) => stream,
        None => bail!(
            "conectando em {host}:{port}: {}",
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "nenhum endereço".to_string())
        ),
    };
    stream.set_nodelay(true).ok();
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .context("configurando timeout de leitura")?;

    let mut reader = stream.try_clone().context("clonando socket")?;
    let mut writer = stream;

    let (out_tx, out_rx) = channel();
    let (in_tx, mut in_rx) = unbounded_channel::<Input>();
    let stop = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));

    let stop_r = stop.clone();
    let closed_r = closed.clone();
    let reader_thread = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut filter = TelnetFilter::default();
        loop {
            if stop_r.load(Ordering::Relaxed) {
                break;
            }
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let (data, replies) = filter.filter(&buf[..n]);
                    if !replies.is_empty() {
                        let _ = reader.write_all(&replies);
                    }
                    if !data.is_empty() && out_tx.send(data).is_err() {
                        break;
                    }
                }
                Err(ref e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => break,
            }
        }
        closed_r.store(true, Ordering::Relaxed);
    });

    let stop_w = stop.clone();
    let closed_w = closed.clone();
    let writer_thread = thread::spawn(move || {
        while let Some(item) = in_rx.blocking_recv() {
            if stop_w.load(Ordering::Relaxed) {
                break;
            }
            if let Input::Data(data) = item {
                if writer.write_all(&data).is_err() {
                    closed_w.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });

    Ok(Connection::new(
        out_rx,
        in_tx,
        stop,
        closed,
        vec![reader_thread, writer_thread],
        String::new(),
    ))
}

fn serial(device: &str, baud: u32) -> Result<Connection> {
    let mut port = serialport::new(device, baud)
        .timeout(Duration::from_millis(20))
        .open()
        .with_context(|| {
            format!("abrindo {device} a {baud} baud (precisa de permissão na porta serial)")
        })?;

    let (out_tx, out_rx) = channel();
    let (in_tx, mut in_rx) = unbounded_channel::<Input>();
    let stop = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));

    let stop_t = stop.clone();
    let closed_t = closed.clone();
    let handle = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            if stop_t.load(Ordering::Relaxed) {
                break;
            }
            while let Ok(item) = in_rx.try_recv() {
                if let Input::Data(data) = item {
                    if port.write_all(&data).is_err() {
                        closed_t.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            }
            match port.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    if out_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => {
                    closed_t.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
    });

    Ok(Connection::new(
        out_rx,
        in_tx,
        stop,
        closed,
        vec![handle],
        String::new(),
    ))
}

/// Remove a negociação do Telnet (IAC) e responde recusando todas as opções:
/// rede de equipamentos não precisa de ECHO/TTYPE para funcionar.
#[derive(Default)]
struct TelnetFilter {
    state: State,
    pending: u8,
}

#[derive(Default, PartialEq, Eq)]
enum State {
    #[default]
    Data,
    Iac,
    Negotiating,
    Sub,
    SubIac,
}

impl TelnetFilter {
    fn filter(&mut self, input: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut data = Vec::with_capacity(input.len());
        let mut replies = Vec::new();
        for &byte in input {
            match self.state {
                State::Data => {
                    if byte == IAC {
                        self.state = State::Iac;
                    } else {
                        data.push(byte);
                    }
                }
                State::Iac => match byte {
                    IAC => {
                        data.push(IAC);
                        self.state = State::Data;
                    }
                    WILL | WONT | DO | DONT => {
                        self.pending = byte;
                        self.state = State::Negotiating;
                    }
                    SB => self.state = State::Sub,
                    _ => self.state = State::Data,
                },
                State::Negotiating => {
                    match self.pending {
                        DO => replies.extend_from_slice(&[IAC, WONT, byte]),
                        WILL => replies.extend_from_slice(&[IAC, DONT, byte]),
                        _ => {}
                    }
                    self.state = State::Data;
                }
                State::Sub => {
                    if byte == IAC {
                        self.state = State::SubIac;
                    }
                }
                State::SubIac => {
                    self.state = if byte == SE { State::Data } else { State::Sub };
                }
            }
        }
        (data, replies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn telnet_strips_negotiation_and_replies() {
        let mut filter = TelnetFilter::default();
        let input = [IAC, DO, 1, b'h', b'i', IAC, WILL, 3, b'!'];
        let (data, replies) = filter.filter(&input);
        assert_eq!(data, b"hi!");
        assert_eq!(replies, vec![IAC, WONT, 1, IAC, DONT, 3]);
    }

    #[test]
    fn telnet_handles_escaped_iac_and_subnegotiation() {
        let mut filter = TelnetFilter::default();
        let mut input = vec![b'a', IAC, IAC, b'b'];
        input.extend_from_slice(&[IAC, SB, 24, 1, IAC, SE, b'c']);
        let (data, replies) = filter.filter(&input);
        assert_eq!(data, vec![b'a', IAC, b'b', b'c']);
        assert!(replies.is_empty());
    }

    #[test]
    fn telnet_connect_roundtrip() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.write_all(b"banner\r\n").unwrap();
            let mut buf = [0u8; 32];
            let n = s.read(&mut buf).unwrap();
            buf[..n].to_vec()
        });

        let session = Session {
            name: "t".into(),
            protocol: Protocol::Telnet,
            host: "127.0.0.1".into(),
            port: addr.port(),
            user: None,
            emulation: "xterm-256color".into(),
            auth: None,
            description: None,
            baud: None,
        };
        let conn = connect(&session, None).unwrap();
        let chunk = conn.output.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(chunk, b"banner\r\n");
        assert!(conn.send(b"oi\r\n"));
        assert_eq!(server.join().unwrap(), b"oi\r\n");
    }

    #[test]
    fn serial_bad_device_errors() {
        let err = match serial("/dev/nao-existe-sshc", 9600) {
            Ok(_) => panic!("abrir dispositivo inexistente deveria falhar"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("abrindo"));
    }

    #[test]
    fn ssh_without_credentials_is_rejected() {
        let session = Session {
            name: "s".into(),
            protocol: Protocol::Ssh,
            host: "127.0.0.1".into(),
            port: 2022,
            user: Some("u".into()),
            emulation: "xterm-256color".into(),
            auth: None,
            description: None,
            baud: None,
        };
        let err = match connect(&session, None) {
            Ok(_) => panic!("deveria exigir credenciais"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("senha ou chave"));
    }
}
