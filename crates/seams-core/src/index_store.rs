//! Per-folder index persistence. The index is the last-synced state; it must survive
//! restarts for version vectors to stay honest.

use std::fs;
use std::path::{Path, PathBuf};

use seams_proto::IndexEntry;
use seams_sync::{FolderIndex, MemoryIndex};

use crate::config::CoreError;

/// Loads `data_dir/index/<folder_id>.json`, or an empty index when absent.
pub fn load(data_dir: &Path, folder_id: &str) -> Result<MemoryIndex, CoreError> {
    let path = index_path(data_dir, folder_id);
    match fs::read(&path) {
        Ok(bytes) => {
            let entries: Vec<IndexEntry> = serde_json::from_slice(&bytes)?;
            let mut index = MemoryIndex::default();
            for entry in entries {
                index.upsert(entry);
            }
            Ok(index)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MemoryIndex::default()),
        Err(e) => Err(e.into()),
    }
}

/// Persists a converged index state directly from the session's final map.
pub fn save_from_map(
    data_dir: &Path,
    folder_id: &str,
    entries: &std::collections::BTreeMap<String, IndexEntry>,
) -> Result<(), CoreError> {
    let path = index_path(data_dir, folder_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let owned: Vec<IndexEntry> = entries.values().cloned().collect();
    write_atomic(&path, &owned)
}

fn write_atomic(path: &Path, entries: &[IndexEntry]) -> Result<(), CoreError> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec(entries)?)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn index_path(data_dir: &Path, folder_id: &str) -> PathBuf {
    data_dir.join("index").join(format!("{folder_id}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use seams_proto::VersionVector;
    use seams_identity::DeviceIdentity;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seams-index-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_preserves_entries() {
        let dir = temp_dir("roundtrip");
        let id = DeviceIdentity::from_seed([1u8; 32]).device_id();
        let mut index = MemoryIndex::default();
        let mut version = VersionVector::new();
        version.insert(id, 3);
        index.upsert(IndexEntry {
            rel_path: "a.md".into(),
            size: 12,
            mtime_ns: 42,
            file_hash: [7u8; 32],
            version,
            deleted: false,
        });

        let map: std::collections::BTreeMap<String, IndexEntry> =
            [("a.md".to_string(), index.get("a.md").expect("entry exists").clone())]
                .into_iter()
                .collect();
        save_from_map(&dir, "f1", &map).unwrap();
        let loaded = load(&dir, "f1").unwrap();
        let entry = loaded.get("a.md").expect("entry persisted");
        assert_eq!(entry.size, 12);
        assert_eq!(entry.version[&id], 3);
    }
}
