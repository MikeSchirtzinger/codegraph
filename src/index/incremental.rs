//! Incremental indexing via SHA-256 file content hashing.
//!
//! On re-index, only files whose content hash differs from the stored hash
//! in `file_metadata` are re-parsed. Deleted files are detected by comparing
//! the set of known files against what's on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use sha2::{Digest, Sha256};
use surrealdb::engine::any::Any;
use surrealdb::Surreal;

/// What happened to a file since last index.
#[allow(dead_code)]
#[derive(Debug)]
pub enum FileChange {
    Added(PathBuf),
    Modified(PathBuf),
    Unchanged(PathBuf),
    Deleted(PathBuf),
}

/// Compute SHA-256 hash of a file's contents.
pub fn hash_file(path: &Path) -> Result<String> {
    let content = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&content);
    Ok(hex::encode(hasher.finalize()))
}

/// Compare on-disk files against stored file_metadata to determine changes.
pub async fn detect_changes(
    db: &Arc<Surreal<Any>>,
    project_id: &str,
    on_disk: &[PathBuf],
) -> Result<Vec<FileChange>> {
    // Load all stored file hashes for this project
    let mut response = db
        .query("SELECT file_path, content_hash FROM file_metadata WHERE project_id = $pid")
        .bind(("pid", project_id.to_string()))
        .await?;

    let rows: Vec<surrealdb_types::Value> = response.take(0)?;

    // Build a map of file_path -> content_hash from the raw values
    let mut stored_map: HashMap<String, String> = HashMap::new();
    for row in &rows {
        if let surrealdb_types::Value::Object(obj) = row {
            let fp = obj.get("file_path").and_then(|v| match v {
                surrealdb_types::Value::String(s) => Some(s.to_string()),
                _ => None,
            });
            let hash = obj.get("content_hash").and_then(|v| match v {
                surrealdb_types::Value::String(s) => Some(s.to_string()),
                _ => None,
            });
            if let (Some(fp), Some(hash)) = (fp, hash) {
                stored_map.insert(fp, hash);
            }
        }
    }

    let mut changes = Vec::with_capacity(on_disk.len());
    let mut seen_paths = std::collections::HashSet::new();

    for path in on_disk {
        // We need the relative path that was stored — derive it the same way
        // the caller does (strip root prefix). The caller passes absolute paths,
        // but stored paths are relative. We need to match by the tail.
        // For now, we hash the file and compare.
        let current_hash = hash_file(path)?;

        // Find the stored entry by scanning for a matching suffix
        // (stored paths are relative to project root)
        let rel = path.to_string_lossy().to_string();

        // Look for any stored path that matches the end of this path
        let stored_hash = stored_map
            .iter()
            .find(|(k, _)| rel.ends_with(k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()));

        match stored_hash {
            Some((key, hash)) => {
                seen_paths.insert(key.clone());
                if hash == current_hash {
                    changes.push(FileChange::Unchanged(path.clone()));
                } else {
                    changes.push(FileChange::Modified(path.clone()));
                }
            }
            None => {
                changes.push(FileChange::Added(path.clone()));
            }
        }
    }

    // Detect deleted files (in DB but not on disk)
    for stored_path in stored_map.keys() {
        if !seen_paths.contains(stored_path) {
            changes.push(FileChange::Deleted(PathBuf::from(stored_path)));
        }
    }

    Ok(changes)
}
