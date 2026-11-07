use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use sshc_core::{cloud, securecrt, Store};
use sshc_sync::{engine, google::Drive, oauth, pkce, secrets, telegram, Manifest};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "sshc", version, about = "Cliente SSH multi-sessão")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Mostra providers de nuvem detectados e a store em uso
    Where,
    /// Lista as sessões da store
    List,
    /// Importa sessões do SecureCRT
    Import {
        /// Pasta Sessions do SecureCRT
        from: PathBuf,
        /// Destino (default: store detectada)
        #[arg(long)]
        to: Option<PathBuf>,
        /// Só mostra o que seria importado
        #[arg(long)]
        dry_run: bool,
    },
    /// Entra na conta Google (OAuth PKCE, escopo drive.appdata)
    Login {
        /// Client ID do OAuth (fica salvo em ~/.sshc/client-id)
        #[arg(long)]
        client_id: Option<String>,
    },
    /// Esquece o refresh token guardado no Keychain
    Logout,
    /// Sincroniza a store local com o app-data folder do Google Drive
    Sync {
        #[arg(long)]
        dry_run: bool,
    },
    /// Salva/restaura as sessões num documento fixado no Telegram
    Telegram {
        #[command(subcommand)]
        cmd: TelegramCmd,
    },
}

#[derive(Subcommand)]
enum TelegramCmd {
    /// Conecta o bot criado no @BotFather
    Login {
        /// Token do bot (ou defina SSHC_TELEGRAM_TOKEN)
        #[arg(long)]
        token: Option<String>,
        /// ID numérico do chat (default: descobre pelo /start)
        #[arg(long)]
        chat: Option<i64>,
    },
    /// Esquece o token do bot guardado no Keychain
    Logout,
    /// Envia a store como documento fixado no chat
    Backup,
    /// Restaura as sessões do documento fixado
    Restore,
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Where => {
            println!("store: {}", cloud::store_root().display());
            println!("state: {}", sshc_sync::engine::state_dir().display());
        }
        Cmd::List => {
            let store = Store::open(cloud::store_root())?;
            for entry in store.load_all()? {
                let group = if entry.group.is_empty() {
                    "-".to_string()
                } else {
                    entry.group
                };
                let s = &entry.session;
                println!(
                    "{group:<20} {:<22} {:<10} {}",
                    s.name,
                    protocol_label(&s.protocol),
                    s.endpoint()
                );
            }
        }
        Cmd::Import { from, to, dry_run } => {
            let root = to.unwrap_or_else(cloud::store_root);
            let store = if dry_run {
                None
            } else {
                Some(Store::open(&root)?)
            };
            let report = securecrt::import_dir(&from)?;

            for (path, session) in &report.entries {
                let group = path
                    .parent()
                    .and_then(|dir| dir.strip_prefix(&from).ok())
                    .map(|d| d.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Some(store) = &store {
                    store.save(&group, session)?;
                }
                println!(
                    "{}[{group}] {} -> {}",
                    if dry_run { "dry " } else { "" },
                    session.name,
                    session.endpoint()
                );
            }
            println!(
                "\n{} sessões, {} ignoradas, destino: {}",
                report.entries.len(),
                report.skipped,
                root.display()
            );
        }
        Cmd::Login { client_id } => login(client_id)?,
        Cmd::Logout => {
            secrets::delete()?;
            println!("refresh token removido do Keychain");
        }
        Cmd::Sync { dry_run } => sync(dry_run)?,
        Cmd::Telegram { cmd } => telegram_cmd(cmd)?,
    }
    Ok(())
}

fn login(client_id_arg: Option<String>) -> Result<()> {
    let client_id = match client_id_arg {
        Some(id) => {
            save_client_id(&id)?;
            id
        }
        None => read_client_id()?,
    };

    let verifier = pkce::verifier()?;
    let state = pkce::state()?;
    let listener = TcpListener::bind("127.0.0.1:0").context("abrindo porta local")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    let url = oauth::auth_url(
        &client_id,
        &redirect_uri,
        &pkce::challenge(&verifier),
        &state,
    );

    println!("Abrindo o navegador para autorizar o sshc...");
    println!("Se não abrir, cole esta URL:\n\n{url}\n");
    open_browser(&url);

    let (mut stream, _) = listener.accept().context("esperando o redirecionamento")?;
    let mut buf = [0u8; 8192];
    let read = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..read]).into_owned();
    let body = "<html><body style='font-family:sans-serif'><h3>sshc autorizado.</h3>\
<p>Pode fechar esta aba e voltar ao terminal.</p></body></html>";
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );

    let (code, returned_state) = oauth::parse_redirect(&request)?;
    if returned_state != state {
        bail!("state do redirecionamento não confere");
    }

    let client = reqwest::blocking::Client::new();
    let token = oauth::exchange_code(&client, &client_id, &code, &verifier, &redirect_uri)?;
    let refresh = token
        .refresh_token
        .context("o Google não devolveu refresh_token; tente remover o acesso em myaccount.google.com/permissions e logar de novo")?;
    secrets::save(&refresh)?;
    println!("logado. refresh token guardado no Keychain.");
    Ok(())
}

fn sync(dry_run: bool) -> Result<()> {
    let refresh = secrets::load()?.context("não logado: rode 'sshc login google' antes")?;
    let client_id = read_client_id()?;
    let client = reqwest::blocking::Client::new();
    let token = oauth::refresh(&client, &client_id, &refresh)?;
    if let Some(rotated) = &token.refresh_token {
        secrets::save(rotated)?;
    }

    let root = cloud::store_root();
    fs::create_dir_all(&root)?;
    let drive = Drive::new(token.access_token);
    let remote = drive.list().context("listando o app-data folder")?;
    let mut manifest = Manifest::load(&root)?;
    let actions = engine::plan(&root, &remote, &manifest)?;

    for action in &actions {
        println!("{action:?}");
    }
    let report = engine::apply(&root, &drive, &actions, &mut manifest, dry_run)?;
    println!(
        "\n{} enviados, {} baixados, {} conflitos, {} apagados no remoto, {} apagados local (store: {})",
        report.uploaded,
        report.downloaded,
        report.conflicts,
        report.deleted_remote,
        report.deleted_local,
        root.display()
    );
    Ok(())
}

fn telegram_cmd(cmd: TelegramCmd) -> Result<()> {
    match cmd {
        TelegramCmd::Login { token, chat } => {
            let token = token
                .or_else(|| std::env::var("SSHC_TELEGRAM_TOKEN").ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .context("passe --token (do @BotFather) ou defina SSHC_TELEGRAM_TOKEN")?;
            let config = telegram::login(&token, chat)?;
            let bot = config
                .bot
                .as_deref()
                .map(|name| format!("@{name}"))
                .unwrap_or_else(|| "bot".to_string());
            println!("conectado: {bot} · chat {}", config.chat_id);
        }
        TelegramCmd::Logout => {
            telegram::forget()?;
            println!("token do Telegram removido do Keychain");
        }
        TelegramCmd::Backup => {
            let config = telegram_config()?;
            let report = telegram::backup(&cloud::store_root(), &config)?;
            println!(
                "backup enviado: {} sessões ({} bytes) — documento fixado no chat {}",
                report.files, report.bytes, config.chat_id
            );
        }
        TelegramCmd::Restore => {
            let config = telegram_config()?;
            let report = telegram::restore(&cloud::store_root(), &config)?;
            println!(
                "{} restauradas, {} já iguais, {} conflitos",
                report.restored, report.unchanged, report.conflicts
            );
            if let Some(dir) = &report.conflicts_dir {
                println!("versões remotas dos conflitos em {}", dir.display());
            }
        }
    }
    Ok(())
}

fn telegram_config() -> Result<telegram::TelegramConfig> {
    telegram::load_config()?
        .context("Telegram não configurado: rode 'sshc telegram login --token <TOKEN>' antes")
}

fn protocol_label(p: &sshc_core::Protocol) -> String {
    match p {
        sshc_core::Protocol::Ssh => "ssh".into(),
        sshc_core::Protocol::Telnet => "telnet".into(),
        sshc_core::Protocol::Serial => "serial".into(),
        sshc_core::Protocol::LocalShell => "local".into(),
        sshc_core::Protocol::Other(o) => o.to_lowercase(),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn client_id_path() -> PathBuf {
    home().join(".sshc").join("client-id")
}

fn read_client_id() -> Result<String> {
    if let Ok(id) = std::env::var("SSHC_GOOGLE_CLIENT_ID") {
        if !id.trim().is_empty() {
            return Ok(id.trim().to_string());
        }
    }
    let path = client_id_path();
    let id = fs::read_to_string(&path).with_context(|| {
        format!(
            "client ID não configurado. Rode: sshc login --client-id <ID>\n\
(ou defina SSHC_GOOGLE_CLIENT_ID). Arquivo esperado: {}",
            path.display()
        )
    })?;
    Ok(id.trim().to_string())
}

fn save_client_id(id: &str) -> Result<()> {
    let path = client_id_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, id.trim())?;
    Ok(())
}

fn open_browser(url: &str) {
    let candidates: [&[&str]; 3] = [
        &["open", url],
        &["xdg-open", url],
        &["cmd", "/C", "start", url],
    ];
    for cmd in candidates {
        if std::process::Command::new(cmd[0])
            .args(&cmd[1..])
            .spawn()
            .is_ok()
        {
            return;
        }
    }
    eprintln!("não consegui abrir o navegador; abra a URL manualmente");
}
