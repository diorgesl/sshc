//! Núcleo do cliente: modelo de sessão, store em pastas e importadores.
//!
//! Decisão de arquitetura: o "banco" é um diretório de arquivos .toml, um por
//! sessão, espelhando a árvore de pastas. Assim qualquer provider de nuvem
//! (iCloud/OneDrive/Google Drive/Dropbox) sincroniza sem API e conflitos ficam
//! por arquivo. Credenciais nunca entram aqui — ficam no keychain do SO.

pub mod cloud;
pub mod securecrt;
pub mod session;
pub mod store;

pub use session::{Protocol, Session};
pub use store::{SessionEntry, Store};
