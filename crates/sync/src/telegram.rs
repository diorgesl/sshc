//! Backup/restauração das sessões no Telegram, via um **documento fixado** no
//! chat (Bot API).
//!
//! A Bot API não deixa listar o histórico do chat (`getChatHistory` não existe),
//! então a outra máquina descobre o backup pelo `pinned_message` que o `getChat`
//! devolve. O documento é editado no lugar a cada backup; se o pin não for
//! nosso, um documento novo é enviado e fixado.
//!
//! Regra: é backup/restore (o último backup vence), não sync por arquivo.
//! Restaurar nunca apaga sessão local: divergência vira conflito em disco.

use crate::engine;
use crate::secrets;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API: &str = "https://api.telegram.org";
const BUNDLE_FILE: &str = "sshc-sessions.json";
const BUNDLE_VERSION: u32 = 1;
const CAPTION: &str = "sshc — sessões (sem senhas)";

/// Bot + chat escolhidos no login; fica no Keychain (JSON).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    pub token: String,
    pub chat_id: i64,
    #[serde(default)]
    pub bot: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BackupReport {
    pub files: usize,
    pub bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreReport {
    pub restored: usize,
    pub unchanged: usize,
    pub conflicts: usize,
    pub conflicts_dir: Option<PathBuf>,
}

/// Formato do documento fixado: caminho relativo -> conteúdo TOML.
#[derive(Debug, Serialize, Deserialize)]
struct Bundle {
    version: u32,
    files: BTreeMap<String, String>,
}

pub fn load_config() -> Result<Option<TelegramConfig>> {
    let Some(text) = secrets::load_telegram()? else {
        return Ok(None);
    };
    Ok(Some(
        serde_json::from_str(&text).context("configuração do Telegram inválida")?,
    ))
}

fn save_config(config: &TelegramConfig) -> Result<()> {
    secrets::save_telegram(&serde_json::to_string(config)?)
}

pub fn forget() -> Result<()> {
    secrets::delete_telegram()
}

/// Valida o token, descobre o chat (se não informado) e grava a config.
pub fn login(token: &str, chat: Option<i64>) -> Result<TelegramConfig> {
    let token = token.trim();
    if token.is_empty() {
        bail!("token do bot vazio");
    }
    let telegram = Telegram::new(token);
    let me = telegram.get_me()?;
    let chat_id = match chat {
        Some(id) => id,
        None => telegram.latest_private_chat()?,
    };
    telegram
        .get_chat(chat_id)
        .with_context(|| format!("o bot não tem acesso ao chat {chat_id}"))?;
    let config = TelegramConfig {
        token: token.to_string(),
        chat_id,
        bot: me.username,
    };
    save_config(&config)?;
    Ok(config)
}

/// Empacota a store e manda como documento fixado no chat.
pub fn backup(root: &Path, config: &TelegramConfig) -> Result<BackupReport> {
    let files = collect_files(root)?;
    let count = files.len();
    let bundle = Bundle {
        version: BUNDLE_VERSION,
        files,
    };
    let bytes = serde_json::to_vec_pretty(&bundle).context("serializando o backup")?;
    let size = bytes.len();
    Telegram::new(&config.token).put_bundle(config.chat_id, &bytes)?;
    Ok(BackupReport {
        files: count,
        bytes: size,
    })
}

/// Baixa o documento fixado e grava o que faltar; conflitos vão para disco.
pub fn restore(root: &Path, config: &TelegramConfig) -> Result<RestoreReport> {
    let telegram = Telegram::new(&config.token);
    let chat = telegram.get_chat(config.chat_id)?;
    let message = chat
        .pinned_message
        .context("nenhum backup fixado no chat; envie as sessões antes")?;
    let document = message
        .document
        .context("a mensagem fixada no chat não é um backup do sshc; envie as sessões antes")?;
    if let Some(name) = &document.file_name {
        if name != BUNDLE_FILE {
            bail!("a mensagem fixada no chat não é o backup do sshc ({name})");
        }
    }
    let bytes = telegram.download(&document.file_id)?;
    let bundle = parse_bundle(&bytes)?;
    apply_bundle(root, &engine::conflicts_dir(root), &bundle)
}

fn parse_bundle(bytes: &[u8]) -> Result<Bundle> {
    let bundle: Bundle = serde_json::from_slice(bytes).context("backup não é um bundle do sshc")?;
    if bundle.version != BUNDLE_VERSION {
        bail!("versão de backup não suportada: {}", bundle.version);
    }
    Ok(bundle)
}

/// Todos os .toml da store (ignora pastas ocultas), como caminho relativo.
fn collect_files(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for (rel, _) in engine::local_sessions(root)? {
        let full = root.join(&rel);
        let text =
            fs::read_to_string(&full).with_context(|| format!("lendo {}", full.display()))?;
        files.insert(rel, text);
    }
    Ok(files)
}

fn apply_bundle(root: &Path, conflict_root: &Path, bundle: &Bundle) -> Result<RestoreReport> {
    let mut report = RestoreReport::default();
    for (rel, content) in &bundle.files {
        let target = engine::safe_join(root, rel)?;
        if target.exists() {
            if fs::read_to_string(&target).ok().as_deref() == Some(content.as_str()) {
                report.unchanged += 1;
                continue;
            }
            let conflict = engine::safe_join(conflict_root, rel)?;
            if let Some(parent) = conflict.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&conflict, content)
                .with_context(|| format!("gravando conflito em {}", conflict.display()))?;
            report.conflicts += 1;
            report.conflicts_dir = Some(conflict_root.to_path_buf());
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&target, content)
                .with_context(|| format!("gravando {}", target.display()))?;
            report.restored += 1;
        }
    }
    Ok(report)
}

struct Telegram {
    client: reqwest::blocking::Client,
    token: String,
}

#[derive(Debug, Deserialize)]
struct BotUser {
    #[serde(default)]
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatFullInfo {
    #[serde(default)]
    pinned_message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    #[serde(default)]
    message_id: i64,
    #[serde(default)]
    document: Option<Document>,
    #[serde(default)]
    caption: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Document {
    file_id: String,
    #[serde(default)]
    file_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Update {
    #[serde(default)]
    message: Option<IncomingMessage>,
}

#[derive(Debug, Deserialize)]
struct IncomingMessage {
    #[serde(default)]
    chat: Option<IncomingChat>,
}

#[derive(Debug, Deserialize)]
struct IncomingChat {
    #[serde(default)]
    id: i64,
    #[serde(default)]
    r#type: String,
}

fn last_private_chat(updates: Vec<Update>) -> Option<i64> {
    updates
        .into_iter()
        .filter_map(|update| update.message)
        .filter_map(|message| message.chat)
        .rfind(|chat| chat.r#type == "private")
        .map(|chat| chat.id)
}

impl Telegram {
    fn new(token: &str) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self {
            client,
            token: token.to_string(),
        }
    }

    fn url(&self, method: &str) -> String {
        format!("{API}/bot{}/{method}", self.token)
    }

    /// Chama um método e devolve o campo `result`; erros do Telegram viram erro.
    fn call(
        &self,
        method: &str,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<serde_json::Value> {
        let response = request
            .send()
            // O token vai na URL do Bot API: erro do reqwest não pode vazar.
            .map_err(|error| error.without_url())
            .with_context(|| format!("chamando {method} no Telegram"))?;
        let status = response.status();
        let body = response.text().unwrap_or_default();
        let value: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| format!("resposta inválida do Telegram ({method})"))?;
        if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            let description = value
                .get("description")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("erro desconhecido");
            bail!("Telegram recusou {method} ({status}): {description}");
        }
        Ok(value.get("result").cloned().unwrap_or_default())
    }

    fn get_me(&self) -> Result<BotUser> {
        let result = self.call("getMe", self.client.get(self.url("getMe")))?;
        serde_json::from_value(result).context("resposta do getMe inválida")
    }

    fn get_chat(&self, chat_id: i64) -> Result<ChatFullInfo> {
        let params = [("chat_id", chat_id.to_string())];
        let result = self.call(
            "getChat",
            self.client.get(self.url("getChat")).query(&params),
        )?;
        serde_json::from_value(result).context("resposta do getChat inválida")
    }

    fn latest_private_chat(&self) -> Result<i64> {
        let result = self.call(
            "getUpdates",
            self.client
                .get(self.url("getUpdates"))
                .query(&[("limit", "100")]),
        )?;
        let updates: Vec<Update> =
            serde_json::from_value(result).context("resposta do getUpdates inválida")?;
        last_private_chat(updates).context(
            "não achei conversa privada com o bot; mande /start para ele no Telegram e tente de novo",
        )
    }

    /// Envia o bundle; se o pin atual já for nosso, edita o documento no lugar.
    fn put_bundle(&self, chat_id: i64, bytes: &[u8]) -> Result<()> {
        let chat = self.get_chat(chat_id)?;
        let ours = chat
            .pinned_message
            .as_ref()
            .filter(|message| is_ours(message));
        match ours {
            Some(message) if message.message_id > 0 => {
                match self.edit_document(chat_id, message.message_id, bytes) {
                    Ok(_) => Ok(()),
                    // Backup idêntico: o Telegram recusa a edição, mas o pin já está certo.
                    Err(error) if error.to_string().contains("not modified") => Ok(()),
                    Err(error) => Err(error),
                }
            }
            _ => {
                let sent = self.send_document(chat_id, bytes)?;
                self.pin(chat_id, sent.message_id)
            }
        }
    }

    fn send_document(&self, chat_id: i64, bytes: &[u8]) -> Result<Message> {
        let fields = [
            ("chat_id", chat_id.to_string()),
            ("caption", CAPTION.to_string()),
        ];
        let (boundary, body) = multipart_body(&fields, "document", BUNDLE_FILE, bytes);
        let result = self.call(
            "sendDocument",
            self.client
                .post(self.url("sendDocument"))
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(body),
        )?;
        serde_json::from_value(result).context("resposta do sendDocument inválida")
    }

    fn edit_document(&self, chat_id: i64, message_id: i64, bytes: &[u8]) -> Result<Message> {
        let media = serde_json::json!({
            "type": "document",
            "media": "attach://bundle",
            "caption": CAPTION,
        })
        .to_string();
        let fields = [
            ("chat_id", chat_id.to_string()),
            ("message_id", message_id.to_string()),
            ("media", media),
        ];
        let (boundary, body) = multipart_body(&fields, "bundle", BUNDLE_FILE, bytes);
        let result = self.call(
            "editMessageMedia",
            self.client
                .post(self.url("editMessageMedia"))
                .header(
                    "Content-Type",
                    format!("multipart/form-data; boundary={boundary}"),
                )
                .body(body),
        )?;
        serde_json::from_value(result).context("resposta do editMessageMedia inválida")
    }

    fn pin(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let params = [
            ("chat_id", chat_id.to_string()),
            ("message_id", message_id.to_string()),
            ("disable_notification", "true".to_string()),
        ];
        self.call(
            "pinChatMessage",
            self.client.post(self.url("pinChatMessage")).form(&params),
        )?;
        Ok(())
    }

    fn download(&self, file_id: &str) -> Result<Vec<u8>> {
        let result = self.call(
            "getFile",
            self.client
                .get(self.url("getFile"))
                .query(&[("file_id", file_id)]),
        )?;
        let file_path = result
            .get("file_path")
            .and_then(serde_json::Value::as_str)
            .context("resposta do getFile sem file_path")?;
        let response = self
            .client
            .get(format!("{API}/file/bot{}/{file_path}", self.token))
            .send()
            .map_err(|error| error.without_url())
            .context("baixando o backup do Telegram")?;
        if !response.status().is_success() {
            bail!("download do backup falhou ({})", response.status());
        }
        Ok(response.bytes().context("bytes do backup")?.to_vec())
    }
}

/// Pina só o que o sshc mandou: nome do arquivo ou legenda conhecidos.
fn is_ours(message: &Message) -> bool {
    if message.document.is_none() {
        return false;
    }
    let by_name = message
        .document
        .as_ref()
        .and_then(|document| document.file_name.as_deref())
        == Some(BUNDLE_FILE);
    let by_caption = message
        .caption
        .as_deref()
        .map(|caption| caption.starts_with("sshc"))
        .unwrap_or(false);
    by_name || by_caption
}

fn multipart_body(
    fields: &[(&str, String)],
    file_field: &str,
    filename: &str,
    bytes: &[u8],
) -> (String, Vec<u8>) {
    let boundary = format!("sshc-{}-{}", std::process::id(), nanos());
    let mut body = Vec::with_capacity(bytes.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n")
                .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (boundary, body)
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn backup_collects_toml_and_skips_hidden_and_non_sessions() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Grupo/web.toml", "name = 'web'");
        write(dir.path(), "raiz.toml", "name = 'raiz'");
        write(dir.path(), "nota.txt", "não é sessão");
        write(dir.path(), ".cache/velho.toml", "name = 'velho'");

        let files = collect_files(dir.path()).unwrap();
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            vec!["Grupo/web.toml", "raiz.toml"]
        );
        assert_eq!(files["Grupo/web.toml"], "name = 'web'");
    }

    #[test]
    fn restore_writes_missing_and_saves_conflicts() {
        let store = tempfile::tempdir().unwrap();
        let conflicts = tempfile::tempdir().unwrap();
        write(store.path(), "A/x.toml", "local");

        let mut files = BTreeMap::new();
        files.insert("A/x.toml".to_string(), "remoto".to_string());
        files.insert("B/y.toml".to_string(), "novo".to_string());
        let bundle = Bundle {
            version: BUNDLE_VERSION,
            files,
        };
        let report = apply_bundle(store.path(), conflicts.path(), &bundle).unwrap();

        assert_eq!(report.restored, 1);
        assert_eq!(report.unchanged, 0);
        assert_eq!(report.conflicts, 1);
        // local não é sobrescrito; a versão remota vai para os conflitos
        assert_eq!(
            fs::read_to_string(store.path().join("A/x.toml")).unwrap(),
            "local"
        );
        assert_eq!(
            fs::read_to_string(store.path().join("B/y.toml")).unwrap(),
            "novo"
        );
        assert_eq!(
            fs::read_to_string(conflicts.path().join("A/x.toml")).unwrap(),
            "remoto"
        );
    }

    #[test]
    fn restore_keeps_equal_files_untouched() {
        let store = tempfile::tempdir().unwrap();
        let conflicts = tempfile::tempdir().unwrap();
        write(store.path(), "A/x.toml", "igual");

        let mut files = BTreeMap::new();
        files.insert("A/x.toml".to_string(), "igual".to_string());
        let report = apply_bundle(
            store.path(),
            conflicts.path(),
            &Bundle {
                version: BUNDLE_VERSION,
                files,
            },
        )
        .unwrap();
        assert_eq!(report.unchanged, 1);
        assert_eq!(report.restored + report.conflicts, 0);
    }

    #[test]
    fn restore_rejects_paths_outside_the_store() {
        let store = tempfile::tempdir().unwrap();
        let conflicts = tempfile::tempdir().unwrap();
        let mut files = BTreeMap::new();
        files.insert("../fora.toml".to_string(), "x".to_string());
        let result = apply_bundle(
            store.path(),
            conflicts.path(),
            &Bundle {
                version: BUNDLE_VERSION,
                files,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn parse_bundle_checks_version() {
        let bytes = br#"{"version": 99, "files": {}}"#;
        assert!(parse_bundle(bytes)
            .unwrap_err()
            .to_string()
            .contains("versão"));
        let ok = br#"{"version": 1, "files": {"a.toml": "x"}}"#;
        assert_eq!(parse_bundle(ok).unwrap().files["a.toml"], "x");
    }

    #[test]
    fn last_private_chat_ignores_groups() {
        let updates: Vec<Update> = serde_json::from_str(
            r#"[
                {"update_id": 1, "message": {"chat": {"id": 10, "type": "group"}}},
                {"update_id": 2, "message": {"chat": {"id": 42, "type": "private"}}},
                {"update_id": 3, "message": {"chat": {"id": 43, "type": "private"}}}
            ]"#,
        )
        .unwrap();
        assert_eq!(last_private_chat(updates), Some(43));
        assert_eq!(last_private_chat(Vec::new()), None);
    }

    #[test]
    fn multipart_contains_fields_and_file() {
        let fields = [("chat_id", "7".to_string())];
        let (boundary, body) = multipart_body(&fields, "document", BUNDLE_FILE, b"{}");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("name=\"chat_id\""));
        assert!(text.contains("filename=\"sshc-sessions.json\""));
        assert!(text.ends_with(&format!("\r\n--{boundary}--\r\n")));
        assert!(text.contains("{}"));
    }
}
