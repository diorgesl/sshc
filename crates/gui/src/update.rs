//! Atualização a partir dos releases do GitHub.
//!
//! O app consulta `releases/latest`, compara a versão e, no macOS rodando pelo
//! .app, baixa o zip do release, confere o SHA-256 publicado pela API, extrai
//! com `ditto` e troca o bundle inteiro por um helper que espera o processo
//! atual terminar. Substituir arquivos dentro do bundle quebraria a assinatura
//! ad-hoc (o `Contents` é selado pelo `CodeResources`).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

const REPO: &str = "diorgesl/sshc";
const API: &str = "https://api.github.com";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SWAP_SCRIPT: &str = r#"#!/bin/bash
app="$1"
new="$2"
pid="$3"
work="$4"
while kill -0 "$pid" 2>/dev/null; do
  sleep 0.3
done
old="${app}.old-$$"
mv "$app" "$old" || exit 1
if mv "$new" "$app"; then
  /usr/bin/xattr -dr com.apple.quarantine "$app" 2>/dev/null || true
  if /usr/bin/open "$app"; then
    rm -rf "$old"
  fi
  rm -rf "$work"
else
  mv "$old" "$app"
  exit 1
fi
"#;

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Versão sem o prefixo "v".
    pub version: String,
    pub html_url: String,
    pub asset: Option<Asset>,
}

#[derive(Debug, Clone)]
pub struct Asset {
    pub name: String,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub url: String,
    pub size: u64,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub sha256: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ApiRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Debug, Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    digest: Option<String>,
}

/// Versão mostrada na UI; `SSHC_UPDATE_FROM` simula uma versão antiga em teste.
pub fn current_version() -> String {
    std::env::var("SSHC_UPDATE_FROM")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| CURRENT_VERSION.to_string())
}

pub fn releases_url() -> String {
    format!("https://github.com/{REPO}/releases")
}

pub fn check() -> Result<Option<UpdateInfo>> {
    let url = format!("{API}/repos/{REPO}/releases/latest");
    let response = client()?
        .get(&url)
        .send()
        .context("consultando atualizações no GitHub")?;
    if !response.status().is_success() {
        bail!(
            "GitHub respondeu {} ao procurar releases",
            response.status()
        );
    }
    let release: ApiRelease = response.json().context("resposta do GitHub inválida")?;
    let version = normalize_tag(&release.tag_name);
    if !is_newer(&version, &current_version()) {
        return Ok(None);
    }
    let asset = asset_name().and_then(|wanted| {
        release
            .assets
            .into_iter()
            .find(|asset| asset.name == wanted)
            .map(|asset| Asset {
                name: asset.name,
                url: asset.browser_download_url,
                size: asset.size,
                sha256: asset
                    .digest
                    .as_deref()
                    .and_then(|digest| digest.strip_prefix("sha256:"))
                    .map(str::to_lowercase),
            })
    });
    Ok(Some(UpdateInfo {
        version,
        html_url: release.html_url,
        asset,
    }))
}

/// Só o macOS com bundle .app sabe se substituir com segurança.
pub fn self_install_supported() -> bool {
    cfg!(target_os = "macos") && asset_name().is_some() && current_bundle().is_some()
}

pub fn open_url(url: &str) -> Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg(url);
        command
    } else if cfg!(target_os = "windows") {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", "", url]);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(url);
        command
    };
    command.spawn().context("abrindo o navegador")?;
    Ok(())
}

fn client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(format!("sshc-gui/{CURRENT_VERSION}"))
        .build()
        .context("criando cliente HTTP")
}

fn normalize_tag(tag: &str) -> String {
    tag.trim().trim_start_matches('v').to_string()
}

fn is_newer(latest: &str, current: &str) -> bool {
    let latest = version_parts(latest);
    let current = version_parts(current);
    for index in 0..latest.len().max(current.len()) {
        let new = latest.get(index).copied().unwrap_or(0);
        let old = current.get(index).copied().unwrap_or(0);
        if new != old {
            return new > old;
        }
    }
    false
}

fn version_parts(value: &str) -> Vec<u64> {
    value
        .trim()
        .trim_start_matches('v')
        .split('.')
        .take(3)
        .map(|part| {
            part.split(|character: char| !character.is_ascii_digit())
                .next()
                .unwrap_or("")
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

fn asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("sshc-macos-arm64.zip"),
        ("linux", "x86_64") => Some("sshc-linux-x86_64.tar.gz"),
        ("windows", "x86_64") => Some("sshc-windows-x86_64.zip"),
        _ => None,
    }
}

fn asset_name() -> Option<&'static str> {
    asset_for(std::env::consts::OS, std::env::consts::ARCH)
}

fn current_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|path| path.extension().and_then(|extension| extension.to_str()) == Some("app"))
        .map(Path::to_path_buf)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn download(asset: &Asset, destination: &Path) -> Result<()> {
    let response = client()?
        .get(&asset.url)
        .send()
        .context("baixando a atualização")?;
    if !response.status().is_success() {
        bail!("download da atualização falhou ({})", response.status());
    }
    let bytes = response.bytes().context("lendo a atualização")?;
    if asset.size > 0 && bytes.len() as u64 != asset.size {
        bail!(
            "download incompleto: {} de {} bytes",
            bytes.len(),
            asset.size
        );
    }
    fs::write(destination, bytes).with_context(|| format!("gravando {}", destination.display()))?;
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("lendo {}", path.display()))?;
    let hash = format!("{:x}", Sha256::digest(&bytes));
    if !hash.eq_ignore_ascii_case(expected) {
        bail!("checksum não confere (esperado {expected}, obtido {hash})");
    }
    Ok(())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn writable_dir(dir: &Path) -> bool {
    let probe = dir.join(format!(".sshc-update-probe-{}", std::process::id()));
    match fs::File::create(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
pub fn install(info: &UpdateInfo) -> Result<()> {
    if std::env::var_os("SSHC_UPDATE_FROM").is_some() {
        bail!("SSHC_UPDATE_FROM está definido (modo de teste); desligue para instalar de verdade");
    }
    let asset = info
        .asset
        .as_ref()
        .context("esta versão não tem build para a sua plataforma")?;
    let sha256 = asset
        .sha256
        .as_deref()
        .context("release sem checksum SHA-256; abra o release e atualize à mão")?;
    let bundle = current_bundle().context("o app não está dentro de um bundle .app")?;
    if bundle.to_string_lossy().contains("/AppTranslocation/") {
        bail!("o app está rodando de um local temporário; mova o sshc.app para /Applications");
    }
    let parent = bundle.parent().context("bundle sem pasta pai")?;
    if !writable_dir(parent) {
        bail!(
            "sem permissão para gravar em {}; abra o release e atualize à mão",
            parent.display()
        );
    }

    let work = std::env::temp_dir().join(format!(
        "sshc-update-{}-{}",
        info.version
            .chars()
            .map(|character| if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            })
            .collect::<String>(),
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;

    let zip = work.join("update.zip");
    download(asset, &zip)?;
    verify_sha256(&zip, sha256)?;

    let extracted = work.join("extract");
    fs::create_dir_all(&extracted)?;
    let status = std::process::Command::new("ditto")
        .args(["-x", "-k"])
        .arg(&zip)
        .arg(&extracted)
        .status()
        .context("executando ditto")?;
    if !status.success() {
        bail!("não consegui extrair a atualização");
    }
    let new_app = extracted.join("sshc.app");
    if !new_app.join("Contents/MacOS/sshc").is_file() {
        bail!("a atualização não contém um sshc.app válido");
    }

    let script = work.join("apply.sh");
    fs::write(&script, SWAP_SCRIPT).context("gravando o instalador")?;
    let mut command = std::process::Command::new("/bin/bash");
    command
        .arg(&script)
        .arg(&bundle)
        .arg(&new_app)
        .arg(std::process::id().to_string())
        .arg(&work)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.spawn().context("iniciando o instalador")?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
pub fn install(_info: &UpdateInfo) -> Result<()> {
    bail!("instalação automática só no macOS; abra o release e baixe o arquivo")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.3.0", "0.2.0"));
        assert!(is_newer("0.2.1", "0.2.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.9", "0.2.0"));
        assert!(!is_newer("0.3.0-beta", "0.3.0"));
    }

    #[test]
    fn tags_are_normalized() {
        assert_eq!(normalize_tag("v0.3.0"), "0.3.0");
        assert_eq!(normalize_tag(" 0.4.1 "), "0.4.1");
    }

    #[test]
    fn assets_match_release_names() {
        assert_eq!(asset_for("macos", "aarch64"), Some("sshc-macos-arm64.zip"));
        assert_eq!(
            asset_for("linux", "x86_64"),
            Some("sshc-linux-x86_64.tar.gz")
        );
        assert_eq!(
            asset_for("windows", "x86_64"),
            Some("sshc-windows-x86_64.zip")
        );
        assert_eq!(asset_for("macos", "x86_64"), None);
        assert_eq!(asset_for("linux", "aarch64"), None);
    }

    #[test]
    fn checksum_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("asset.bin");
        fs::write(&file, b"conteudo").unwrap();
        assert!(verify_sha256(&file, "0000").is_err());
        let good = format!("{:x}", Sha256::digest(b"conteudo"));
        assert!(verify_sha256(&file, &good).is_ok());
    }
}
