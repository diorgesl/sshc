//! Sincronização das sessões com a nuvem **via login OAuth**, sem depender do
//! cliente de sync instalado.
//!
//! Fluxo: `sshc login google` abre o navegador (PKCE + loopback), guarda só o
//! refresh token no Keychain do SO, e `sshc sync` reconcilia a pasta local com
//! um app-data folder privado no Drive (`drive.appdata`): o app enxerga apenas
//! os arquivos que ele mesmo criou.
//!
//! Alternativa sem Google Cloud: backup/restore num documento fixado no
//! Telegram com o bot do próprio usuário — ver [`telegram`].

pub mod engine;
pub mod google;
pub mod oauth;
pub mod pkce;
pub mod secrets;
pub mod telegram;

pub use engine::{Action, Manifest, SyncReport};
pub use google::{Drive, RemoteFile};
pub use oauth::TokenResponse;
