//! Cliente mínimo da Drive v3 REST, restrito ao `appDataFolder`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const API: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3";

#[derive(Debug, Clone, Deserialize)]
pub struct RemoteFile {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "modifiedTime")]
    pub modified: String,
}

#[derive(Debug, Deserialize, Default)]
struct FileList {
    #[serde(default)]
    files: Vec<RemoteFile>,
}

#[derive(Debug, Serialize)]
struct NewFile<'a> {
    name: &'a str,
    parents: [&'a str; 1],
}

pub struct Drive {
    client: reqwest::blocking::Client,
    access_token: String,
}

impl Drive {
    pub fn new(access_token: String) -> Self {
        Self {
            client: reqwest::blocking::Client::new(),
            access_token,
        }
    }

    fn get(&self, url: &str) -> reqwest::blocking::RequestBuilder {
        self.client.get(url).bearer_auth(&self.access_token)
    }

    fn check(resp: reqwest::blocking::Response, what: &str) -> Result<reqwest::blocking::Response> {
        if resp.status().is_success() {
            Ok(resp)
        } else {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            bail!("{what} falhou ({status}): {body}")
        }
    }

    /// Lista os arquivos do app-data folder.
    pub fn list(&self) -> Result<Vec<RemoteFile>> {
        let url = format!(
            "{API}/files?spaces=appDataFolder&pageSize=1000\
&fields=files(id,name,modifiedTime),nextPageToken"
        );
        let resp = Self::check(self.get(&url).send().context("listando arquivos")?, "list")?;
        let list: FileList = resp.json().context("lista inválida")?;
        Ok(list.files)
    }

    pub fn download(&self, id: &str) -> Result<Vec<u8>> {
        let url = format!("{API}/files/{id}?alt=media");
        let resp = Self::check(
            self.get(&url).send().context("baixando arquivo")?,
            "download",
        )?;
        Ok(resp.bytes().context("bytes")?.to_vec())
    }

    pub fn upload_new(&self, name: &str, bytes: &[u8]) -> Result<RemoteFile> {
        let boundary = format!("sshc{}", std::process::id());
        let meta = serde_json::to_string(&NewFile {
            name,
            parents: ["appDataFolder"],
        })?;
        let mut body = Vec::new();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let url = format!("{UPLOAD}/files?uploadType=multipart&fields=id,name,modifiedTime");
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.access_token)
            .header(
                "Content-Type",
                format!("multipart/related; boundary={boundary}"),
            )
            .body(body)
            .send()
            .context("enviando arquivo")?;
        let resp = Self::check(resp, "upload")?;
        resp.json::<RemoteFile>()
            .context("resposta de upload inválida")
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let url = format!("{API}/files/{id}");
        let resp = self
            .client
            .delete(&url)
            .bearer_auth(&self.access_token)
            .send()
            .context("apagando arquivo")?;
        Self::check(resp, "delete")?;
        Ok(())
    }

    pub fn update(&self, id: &str, bytes: &[u8]) -> Result<RemoteFile> {
        let url = format!("{UPLOAD}/files/{id}?uploadType=media&fields=id,name,modifiedTime");
        let resp = self
            .client
            .patch(&url)
            .bearer_auth(&self.access_token)
            .header("Content-Type", "application/octet-stream")
            .body(bytes.to_vec())
            .send()
            .context("atualizando arquivo")?;
        let resp = Self::check(resp, "update")?;
        resp.json::<RemoteFile>()
            .context("resposta de update inválida")
    }
}
