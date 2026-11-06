use anyhow::Result;
use clap::{Parser, Subcommand};
use sshc_core::{cloud, securecrt, Store};
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
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Where => {
            for (name, path) in cloud::detected() {
                println!("{name:<13} {}", path.display());
            }
            println!("\nstore: {}", cloud::store_root().display());
        }
        Cmd::List => {
            let store = Store::open(cloud::store_root())?;
            for entry in store.load_all()? {
                let group = if entry.group.is_empty() { "-".to_string() } else { entry.group };
                let s = &entry.session;
                println!(
                    "{group:<20} {:<22} {:<10} {}",
                    s.name,
                    format!("{:?}", s.protocol).to_lowercase(),
                    s.endpoint()
                );
            }
        }
        Cmd::Import { from, to, dry_run } => {
            let root = to.unwrap_or_else(cloud::store_root);
            let store = if dry_run { None } else { Some(Store::open(&root)?) };
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
    }
    Ok(())
}
