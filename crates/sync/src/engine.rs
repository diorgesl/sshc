//! Reconciliação entre a pasta local e o app-data folder.
//!
//! Regras (deliberadamente simples):
//! - só local mudou  -> upload
//! - só remoto mudou -> download
//! - os dois mudaram -> local vence, versão remota vai para `.conflict/`
//! - remoto sumiu    -> ignora (não propagamos delete)

use crate::google::{Drive, RemoteFile};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub const MANIFEST: &str = ".sshc-sync.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub remote_modified: String,
    pub local_mtime: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub files: BTreeMap<String, Item>,
}

impl Manifest {
    pub fn load(root: &Path) -> Result<Self> {
        let path = root.join(MANIFEST);
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path).with_context(|| format!("lendo {}", path.display()))?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        let path = root.join(MANIFEST);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Upload { path: String, existing_id: Option<String> },
    Download { path: String, id: String, modified: String },
    Conflict { path: String, id: String, modified: String },
}

/// Pastas ocultas (`.conflict`, `.git`...) não são sessões.
fn is_hidden(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root)
            .ok()
            .map(|rel| rel.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')))
            .unwrap_or(false)
}

fn local_sessions(root: &Path) -> Result<Vec<(String, u64)>> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_hidden(e.path(), root))
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let rel = path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/");
        let mtime = path
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push((rel, mtime));
    }
    out.sort();
    Ok(out)
}

pub fn plan(root: &Path, remote: &[RemoteFile], manifest: &Manifest) -> Result<Vec<Action>> {
    let far: BTreeMap<&str, &RemoteFile> = remote.iter().map(|r| (r.name.as_str(), r)).collect();
    let mut actions = Vec::new();

    for (path, mtime) in local_sessions(root)? {
        match (manifest.files.get(&path), far.get(path.as_str())) {
            (None, None) => actions.push(Action::Upload { path, existing_id: None }),
            (Some(item), Some(remote_file)) => {
                let local_changed = mtime != item.local_mtime;
                let remote_changed = remote_file.modified != item.remote_modified;
                match (local_changed, remote_changed) {
                    (false, false) => {}
                    (true, false) => actions.push(Action::Upload {
                        path,
                        existing_id: Some(item.id.clone()),
                    }),
                    (false, true) => actions.push(Action::Download {
                        path,
                        id: remote_file.id.clone(),
                        modified: remote_file.modified.clone(),
                    }),
                    (true, true) => actions.push(Action::Conflict {
                        path,
                        id: remote_file.id.clone(),
                        modified: remote_file.modified.clone(),
                    }),
                }
            }
            // Manifesto conhece o arquivo mas o remoto sumiu: não apagamos local.
            _ => actions.push(Action::Upload { path, existing_id: None }),
        }
    }

    // Remoto que não existe localmente e não está no manifesto = sessão nova
    // vinda de outro PC.
    let local_paths: std::collections::BTreeSet<String> =
        local_sessions(root)?.into_iter().map(|(p, _)| p).collect();
    for (name, file) in &far {
        if !local_paths.contains(*name) && !manifest.files.contains_key(*name) {
            actions.push(Action::Download {
                path: (*name).to_string(),
                id: file.id.clone(),
                modified: file.modified.clone(),
            });
        }
    }

    Ok(actions)
}

#[derive(Debug, Default)]
pub struct SyncReport {
    pub uploaded: usize,
    pub downloaded: usize,
    pub conflicts: usize,
}

pub fn apply(
    root: &Path,
    drive: &Drive,
    actions: &[Action],
    manifest: &mut Manifest,
    dry_run: bool,
) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    for action in actions {
        match action {
            Action::Upload { path, existing_id } => {
                report.uploaded += 1;
                if dry_run {
                    continue;
                }
                let full = safe_join(root, path)?;
                let bytes = fs::read(&full).with_context(|| format!("lendo {}", full.display()))?;
                let file = match existing_id {
                    Some(id) => drive.update(id, &bytes)?,
                    None => drive.upload_new(path, &bytes)?,
                };
                let mtime = full
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                manifest.files.insert(
                    path.clone(),
                    Item { id: file.id, remote_modified: file.modified, local_mtime: mtime },
                );
            }
            Action::Download { path, id, modified } => {
                report.downloaded += 1;
                if dry_run {
                    continue;
                }
                let bytes = drive.download(id)?;
                let full = safe_join(root, path)?;
                if let Some(parent) = full.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&full, bytes)?;
                manifest.files.insert(
                    path.clone(),
                    Item {
                        id: id.clone(),
                        remote_modified: modified.clone(),
                        local_mtime: mtime_of(&full),
                    },
                );
            }
            Action::Conflict { path, id, modified: _ } => {
                report.conflicts += 1;
                if dry_run {
                    continue;
                }
                let bytes = drive.download(id)?;
                let conflict = root.join(".conflict").join(path);
                if let Some(parent) = conflict.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&conflict, bytes)?;
                let full = safe_join(root, path)?;
                let local = fs::read(&full)?;
                let file = drive.upload_new(path, &local)?;
                manifest.files.insert(
                    path.clone(),
                    Item { id: file.id, remote_modified: file.modified, local_mtime: mtime_of(&full) },
                );
            }
        }
    }
    if !dry_run {
        manifest.save(root)?;
    }
    Ok(report)
}

fn mtime_of(path: &Path) -> u64 {
    path.metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Impede que um nome vindo do remoto escape da pasta local.
fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let candidate = root.join(rel);
    let normalized = candidate
        .components()
        .fold(PathBuf::new(), |mut acc, c| {
            match c {
                std::path::Component::ParentDir => {
                    acc.pop();
                }
                std::path::Component::CurDir => {}
                other => acc.push(other.as_os_str()),
            }
            acc
        });
    if !normalized.starts_with(root) {
        anyhow::bail!("caminho remoto suspeito: {rel}");
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, b"name = 'x'").unwrap();
    }

    fn remote(name: &str, modified: &str) -> RemoteFile {
        RemoteFile { id: format!("id-{name}"), name: name.to_string(), modified: modified.to_string() }
    }

    #[test]
    fn new_local_file_uploads() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a/b.toml");
        let actions = plan(tmp.path(), &[], &Manifest::default()).unwrap();
        assert_eq!(actions, vec![Action::Upload { path: "a/b.toml".into(), existing_id: None }]);
    }

    #[test]
    fn unchanged_files_do_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.toml");
        let mtime = mtime_of(&tmp.path().join("a.toml"));
        let mut manifest = Manifest::default();
        manifest.files.insert(
            "a.toml".into(),
            Item { id: "id-a".into(), remote_modified: "2024".into(), local_mtime: mtime },
        );
        let actions = plan(tmp.path(), &[remote("a.toml", "2024")], &manifest).unwrap();
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn new_remote_file_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let actions = plan(tmp.path(), &[remote("novo.toml", "2024")], &Manifest::default()).unwrap();
        assert_eq!(
            actions,
            vec![Action::Download { path: "novo.toml".into(), id: "id-novo.toml".into(), modified: "2024".into() }]
        );
    }

    #[test]
    fn conflict_folder_is_not_a_session() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), ".conflict/a.toml");
        touch(tmp.path(), "real.toml");
        let actions = plan(tmp.path(), &[], &Manifest::default()).unwrap();
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(actions[0], Action::Upload { path: "real.toml".into(), existing_id: None });
    }

    #[test]
    fn path_traversal_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(safe_join(tmp.path(), "../../etc/passwd").is_err());
    }
}
