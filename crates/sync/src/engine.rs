//! Reconciliação entre a pasta local e o app-data folder.
//!
//! Regras:
//! - só local mudou  -> upload
//! - só remoto mudou -> download
//! - os dois mudaram -> local vence, versão remota vai para `conflicts-<hash>/`
//! - apagou local    -> apaga remoto (tombstone implícito)
//! - apagou remoto   -> apaga local (se não houve edição local depois)
//!
//! Estado por máquina (manifesto e conflitos) fica **fora** da pasta
//! sincronizada: se morasse dentro, o PC B herdaria o manifesto do PC A.

use crate::google::{Drive, RemoteFile};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

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

/// Diretório de estado desta máquina (nunca sincronizado).
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("SSHC_STATE_DIR") {
        return PathBuf::from(dir);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Application Support/sshc")
    }
    #[cfg(not(target_os = "macos"))]
    {
        home.join(".sshc")
    }
}

/// Identidade estável da store (hash do caminho), para separar máquinas/stores.
fn store_key(root: &Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn manifest_path(root: &Path) -> PathBuf {
    state_dir().join(format!("manifest-{}.json", store_key(root)))
}

pub fn conflicts_dir(root: &Path) -> PathBuf {
    state_dir().join(format!("conflicts-{}", store_key(root)))
}

impl Manifest {
    pub fn load(root: &Path) -> Result<Self> {
        Self::load_from(&manifest_path(root))
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        self.save_to(&manifest_path(root))
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).with_context(|| format!("lendo {}", path.display()))?;
        Ok(serde_json::from_str(&text).unwrap_or_default())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Upload {
        path: String,
        existing_id: Option<String>,
    },
    Download {
        path: String,
        id: String,
        modified: String,
    },
    Conflict {
        path: String,
        id: String,
        modified: String,
    },
    DeleteRemote {
        path: String,
        id: String,
    },
    DeleteLocal {
        path: String,
    },
}

/// Pastas ocultas não são sessões.
fn is_hidden(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root)
        .ok()
        .map(|rel| {
            rel.components()
                .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
        })
        .unwrap_or(false)
}

pub(crate) fn local_sessions(root: &Path) -> Result<Vec<(String, u64)>> {
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
        out.push((rel, mtime_of(path)));
    }
    out.sort();
    Ok(out)
}

pub fn plan(root: &Path, remote: &[RemoteFile], manifest: &Manifest) -> Result<Vec<Action>> {
    let far: BTreeMap<&str, &RemoteFile> = remote.iter().map(|r| (r.name.as_str(), r)).collect();
    let local = local_sessions(root)?;
    let local_paths: BTreeSet<&str> = local.iter().map(|(p, _)| p.as_str()).collect();
    let mut actions = Vec::new();

    for (path, mtime) in &local {
        match (manifest.files.get(path.as_str()), far.get(path.as_str())) {
            (None, None) => actions.push(Action::Upload {
                path: path.clone(),
                existing_id: None,
            }),
            // Nome igual em dois PCs: local vence (Drive não deduplica nome).
            (None, Some(remote_file)) => actions.push(Action::Upload {
                path: path.clone(),
                existing_id: Some(remote_file.id.clone()),
            }),
            // Remoto sumiu: só apaga local se não houve edição depois do último sync.
            (Some(item), None) => {
                if *mtime != item.local_mtime {
                    actions.push(Action::Upload {
                        path: path.clone(),
                        existing_id: None,
                    });
                } else {
                    actions.push(Action::DeleteLocal { path: path.clone() });
                }
            }
            (Some(item), Some(remote_file)) => {
                let local_changed = *mtime != item.local_mtime;
                let remote_changed = remote_file.modified != item.remote_modified;
                match (local_changed, remote_changed) {
                    (false, false) => {}
                    (true, false) => actions.push(Action::Upload {
                        path: path.clone(),
                        existing_id: Some(item.id.clone()),
                    }),
                    (false, true) => actions.push(Action::Download {
                        path: path.clone(),
                        id: remote_file.id.clone(),
                        modified: remote_file.modified.clone(),
                    }),
                    (true, true) => actions.push(Action::Conflict {
                        path: path.clone(),
                        id: remote_file.id.clone(),
                        modified: remote_file.modified.clone(),
                    }),
                }
            }
        }
    }

    // Manifesto conhece o arquivo, mas ele não existe mais localmente: o
    // usuário apagou nesta máquina.
    for (path, item) in &manifest.files {
        if local_paths.contains(path.as_str()) {
            continue;
        }
        if far.contains_key(path.as_str()) {
            actions.push(Action::DeleteRemote {
                path: path.clone(),
                id: item.id.clone(),
            });
        }
    }

    // Sessão nova vinda de outra máquina.
    for (name, remote_file) in &far {
        if !local_paths.contains(*name) && !manifest.files.contains_key(*name) {
            actions.push(Action::Download {
                path: (*name).to_string(),
                id: remote_file.id.clone(),
                modified: remote_file.modified.clone(),
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
    pub deleted_remote: usize,
    pub deleted_local: usize,
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
                manifest.files.insert(
                    path.clone(),
                    Item {
                        id: file.id,
                        remote_modified: file.modified,
                        local_mtime: mtime_of(&full),
                    },
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
            Action::Conflict {
                path,
                id,
                modified: _,
            } => {
                report.conflicts += 1;
                if dry_run {
                    continue;
                }
                let bytes = drive.download(id)?;
                let conflict = conflicts_dir(root).join(path);
                if let Some(parent) = conflict.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&conflict, bytes)?;
                let full = safe_join(root, path)?;
                let local = fs::read(&full)?;
                let file = drive.upload_new(path, &local)?;
                manifest.files.insert(
                    path.clone(),
                    Item {
                        id: file.id,
                        remote_modified: file.modified,
                        local_mtime: mtime_of(&full),
                    },
                );
            }
            Action::DeleteRemote { path, id } => {
                report.deleted_remote += 1;
                if dry_run {
                    continue;
                }
                drive.delete(id)?;
                manifest.files.remove(path);
            }
            Action::DeleteLocal { path } => {
                report.deleted_local += 1;
                if dry_run {
                    continue;
                }
                let full = safe_join(root, path)?;
                let _ = fs::remove_file(&full);
                manifest.files.remove(path);
            }
        }
    }
    if !dry_run {
        // Some do manifesto o que não existe mais em lugar nenhum.
        manifest.files.retain(|path, _| root.join(path).exists());
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
pub(crate) fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let candidate = root.join(rel);
    let normalized = candidate.components().fold(PathBuf::new(), |mut acc, c| {
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
        RemoteFile {
            id: format!("id-{name}"),
            name: name.to_string(),
            modified: modified.to_string(),
        }
    }

    fn manifest_with(root: &Path, rel: &str, modified: &str) -> Manifest {
        let mut m = Manifest::default();
        m.files.insert(
            rel.to_string(),
            Item {
                id: format!("id-{rel}"),
                remote_modified: modified.to_string(),
                local_mtime: mtime_of(&root.join(rel)),
            },
        );
        m
    }

    #[test]
    fn new_local_file_uploads() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a/b.toml");
        let actions = plan(tmp.path(), &[], &Manifest::default()).unwrap();
        assert_eq!(
            actions,
            vec![Action::Upload {
                path: "a/b.toml".into(),
                existing_id: None
            }]
        );
    }

    #[test]
    fn unchanged_files_do_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.toml");
        let manifest = manifest_with(tmp.path(), "a.toml", "2024");
        let actions = plan(tmp.path(), &[remote("a.toml", "2024")], &manifest).unwrap();
        assert!(actions.is_empty(), "{actions:?}");
    }

    #[test]
    fn new_remote_file_downloads() {
        let tmp = tempfile::tempdir().unwrap();
        let actions = plan(
            tmp.path(),
            &[remote("novo.toml", "2024")],
            &Manifest::default(),
        )
        .unwrap();
        assert_eq!(
            actions,
            vec![Action::Download {
                path: "novo.toml".into(),
                id: "id-novo.toml".into(),
                modified: "2024".into()
            }]
        );
    }

    #[test]
    fn local_delete_propagates_to_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = manifest_with(tmp.path(), "sumiu.toml", "2024");
        let actions = plan(tmp.path(), &[remote("sumiu.toml", "2024")], &manifest).unwrap();
        assert_eq!(
            actions,
            vec![Action::DeleteRemote {
                path: "sumiu.toml".into(),
                id: "id-sumiu.toml".into()
            }]
        );
    }

    #[test]
    fn remote_delete_propagates_to_local() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.toml");
        let manifest = manifest_with(tmp.path(), "a.toml", "2024");
        let actions = plan(tmp.path(), &[], &manifest).unwrap();
        assert_eq!(
            actions,
            vec![Action::DeleteLocal {
                path: "a.toml".into()
            }]
        );
    }

    #[test]
    fn remote_delete_does_not_win_over_later_local_edit() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "a.toml");
        let mut manifest = manifest_with(tmp.path(), "a.toml", "2024");
        // edição local depois do sync: mtime registrado fica diferente do atual
        manifest.files.get_mut("a.toml").unwrap().local_mtime -= 10;
        let actions = plan(tmp.path(), &[], &manifest).unwrap();
        assert_eq!(
            actions,
            vec![Action::Upload {
                path: "a.toml".into(),
                existing_id: None
            }]
        );
    }

    #[test]
    fn conflict_folder_is_not_a_session() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), ".conflict/a.toml");
        touch(tmp.path(), "real.toml");
        let actions = plan(tmp.path(), &[], &Manifest::default()).unwrap();
        assert_eq!(actions.len(), 1, "{actions:?}");
        assert_eq!(
            actions[0],
            Action::Upload {
                path: "real.toml".into(),
                existing_id: None
            }
        );
    }

    #[test]
    fn state_lives_outside_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("SSHC_STATE_DIR", tmp.path().join("state"));
        let store = tmp.path().join("store");
        let manifest = manifest_path(&store);
        assert!(
            !manifest.starts_with(&store),
            "manifesto não pode ficar na store"
        );
        assert!(conflicts_dir(&store).starts_with(tmp.path().join("state")));
        // chave estável por caminho
        assert_eq!(manifest, manifest_path(&store));
        std::env::remove_var("SSHC_STATE_DIR");
    }

    #[test]
    fn path_traversal_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(safe_join(tmp.path(), "../../etc/passwd").is_err());
    }
}
