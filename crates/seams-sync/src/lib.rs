//! Sync engine building blocks: folder scanning, hashing, the folder index,
//! conflict decisions, and the trash. The decision rules are specified in
//! `docs/CONFLICTS.md`.

pub mod conflict;
pub mod hash;
pub mod index;
pub mod scanner;
pub mod trash;

pub use conflict::{decide, ConflictLayout, ConflictPolicy, ConflictRole, LocalFile, Resolution};
pub use index::{FolderIndex, MemoryIndex};
pub use scanner::{scan_folder, ScannedFile, ScanError};

pub use seams_proto::{increment, merge, relation, Relation, VersionVector};

#[cfg(test)]
mod tests {
    use super::*;
    use seams_identity::DeviceIdentity;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seams-sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scanner_walks_recursively_and_skips_trash() {
        let root = temp_dir("scan");
        fs::create_dir_all(root.join("photos/2026")).unwrap();
        fs::create_dir_all(root.join(scanner::TRASH_DIR_NAME)).unwrap();
        fs::write(root.join("notes.md"), "hello").unwrap();
        fs::write(root.join("photos/2026/a.jpg"), "abc").unwrap();
        fs::write(root.join(scanner::TRASH_DIR_NAME).join("gone.md"), "old").unwrap();

        let files = scan_folder(&root).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(paths, ["notes.md", "photos/2026/a.jpg"]);
        assert_eq!(files[0].size, 5);
    }

    #[test]
    fn scanner_rejects_missing_folder() {
        let missing = temp_dir("scan-missing").join("nope");
        assert!(matches!(
            scan_folder(&missing),
            Err(ScanError::MissingFolder(_))
        ));
    }

    #[test]
    fn block_hashing_splits_and_matches_manual_hash() {
        use blake3::Hasher;

        let root = temp_dir("hash");
        let path = root.join("blob.bin");
        let data: Vec<u8> = (0..hash::BLOCK_SIZE as usize + 17)
            .map(|i| (i % 251) as u8)
            .collect();
        fs::write(&path, &data).unwrap();

        let hashed = hash::hash_file(&path).unwrap();
        assert_eq!(hashed.blocks.len(), 2);

        let mut whole = Hasher::new();
        whole.update(&data);
        assert_eq!(hashed.whole, *whole.finalize().as_bytes());

        let mut first = Hasher::new();
        first.update(&data[..hash::BLOCK_SIZE as usize]);
        assert_eq!(hashed.blocks[0], *first.finalize().as_bytes());
    }

    #[test]
    fn memory_index_upsert_and_remove() {
        use seams_proto::IndexEntry;
        use seams_proto::VersionVector;

        let mut index = MemoryIndex::default();
        let entry = IndexEntry {
            rel_path: "a.txt".into(),
            size: 3,
            mtime_ns: 1,
            file_hash: [1u8; 32],
            version: VersionVector::new(),
            deleted: false,
        };
        index.upsert(entry.clone());
        assert_eq!(index.get("a.txt"), Some(&entry));
        assert_eq!(index.all().len(), 1);
        assert_eq!(index.remove("a.txt"), Some(entry));
        assert!(index.get("a.txt").is_none());
    }

    #[test]
    fn decision_matrix_matches_spec() {
        use seams_identity::DeviceIdentity;
        use seams_proto::{increment, IndexEntry};
        use std::collections::BTreeMap;

        let device = DeviceIdentity::from_seed([11u8; 32]);
        let peer = DeviceIdentity::from_seed([12u8; 32]);
        let self_id = device.device_id();
        let peer_id = peer.device_id();
        let mut base_vv = VersionVector::new();
        increment(&mut base_vv, self_id);

        let base = IndexEntry {
            rel_path: "doc.md".into(),
            size: 10,
            mtime_ns: 0,
            file_hash: [1u8; 32],
            version: base_vv.clone(),
            deleted: false,
        };
        let local = LocalFile {
            content_hash: [1u8; 32],
            version: base_vv.clone(),
        };
        let remote_unchanged = base.clone();
        let mut remote_edited = base.clone();
        remote_edited.file_hash = [2u8; 32];
        let mut remote_edited_vv = base_vv.clone();
        increment(&mut remote_edited_vv, peer_id);
        remote_edited.version = remote_edited_vv;

        // Nothing changed anywhere.
        assert_eq!(
            conflict::decide(Some(&base), Some(&local), &remote_unchanged),
            Resolution::UpToDate
        );
        // Remote edited, local untouched -> apply.
        assert_eq!(
            conflict::decide(Some(&base), Some(&local), &remote_edited),
            Resolution::ApplyRemote
        );
        // Local edited, remote untouched -> keep ours.
        let mut local_edited = local.clone();
        local_edited.content_hash = [3u8; 32];
        increment(&mut local_edited.version, self_id);
        assert_eq!(
            conflict::decide(Some(&base), Some(&local_edited), &remote_unchanged),
            Resolution::KeepLocal
        );
        // Identical edits on both sides converge.
        let local_same_as_remote = LocalFile {
            content_hash: [2u8; 32],
            version: remote_edited.version.clone(),
        };
        assert_eq!(
            conflict::decide(Some(&base), Some(&local_same_as_remote), &remote_edited),
            Resolution::UpToDate
        );
        // Concurrent edits conflict.
        let mut local_concurrent = local.clone();
        local_concurrent.content_hash = [4u8; 32];
        increment(&mut local_concurrent.version, self_id);
        assert_eq!(
            conflict::decide(Some(&base), Some(&local_concurrent), &remote_edited),
            Resolution::Conflict
        );
        // Local deleted, remote untouched -> our deletion propagates.
        assert_eq!(
            conflict::decide(Some(&base), None, &remote_unchanged),
            Resolution::KeepLocal
        );
        // File was never known on either side.
        let fresh_remote = IndexEntry {
            rel_path: "doc.md".into(),
            size: 1,
            mtime_ns: 0,
            file_hash: [5u8; 32],
            version: BTreeMap::new(),
            deleted: false,
        };
        assert_eq!(
            conflict::decide(None, None, &fresh_remote),
            Resolution::ApplyRemote
        );
    }

    #[test]
    fn conflict_roles_are_deterministic_and_symmetric() {
        let a = DeviceIdentity::from_seed([21u8; 32]).device_id();
        let b = DeviceIdentity::from_seed([22u8; 32]).device_id();

        let role_a = conflict::conflict_role(&a, &b);
        let role_b = conflict::conflict_role(&b, &a);
        assert_ne!(role_a, role_b);
        // Same inputs must always produce the same winner.
        assert_eq!(role_a, conflict::conflict_role(&a, &b));
    }

    #[test]
    fn conflict_name_preserves_extension() {
        let peer = DeviceIdentity::from_seed([23u8; 32]).device_id();
        let now = chrono::Utc::now();
        let name = trash::conflict_name("thesis.docx", &peer, now);
        assert!(name.starts_with("thesis (conflict from "));
        assert!(name.ends_with(".docx"));
        assert_eq!(trash::conflict_name("Makefile", &peer, now).rsplit_once('.').is_none(), true);
    }

    #[test]
    fn trash_purge_removes_only_expired_entries() {
        use chrono::{Duration, Utc};

        let root = temp_dir("trash");
        let trash_dir = root.join(scanner::TRASH_DIR_NAME);
        let now = Utc::now();
        let fresh = (now - Duration::days(1)).format("%Y%m%dT%H%M%S").to_string();
        let stale = (now - Duration::days(trash::TRASH_RETENTION_DAYS + 1))
            .format("%Y%m%dT%H%M%S")
            .to_string();
        fs::create_dir_all(trash_dir.join(&fresh)).unwrap();
        fs::create_dir_all(trash_dir.join(&stale)).unwrap();
        fs::write(trash_dir.join(&fresh).join("keep.md"), "new").unwrap();
        fs::write(trash_dir.join(&stale).join("gone.md"), "old").unwrap();

        let removed = trash::purge_trash(&root, now).unwrap();
        assert_eq!(removed, 1);
        assert!(trash_dir.join(&fresh).join("keep.md").exists());
        assert!(!trash_dir.join(&stale).exists());
        assert!(Path::new(&trash_dir).exists());
    }
}
