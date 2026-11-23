//! Turns the local scan, the last-synced index, and the peer's announced index into
//! the per-file work list plus the index this side announces back. The decision rules
//! are specified in docs/CONFLICTS.md §2.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use blake3::Hasher;
use seams_identity::DeviceId;
use seams_proto::{increment, IndexEntry, VersionVector};
use thiserror::Error;

use crate::conflict::{decide, Resolution};
use crate::hash::{hash_file, BLOCK_SIZE};
use crate::index::FolderIndex;
use crate::scanner::ScannedFile;

/// Provides the content hash of a file by its folder-relative path. Production uses
/// disk hashing; tests use in-memory content.
pub trait ContentHasher {
    fn hash(&self, rel_path: &str) -> io::Result<[u8; 32]>;
}

pub struct DiskHasher<'a> {
    root: &'a Path,
}

impl<'a> DiskHasher<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self { root }
    }
}

impl ContentHasher for DiskHasher<'_> {
    fn hash(&self, rel_path: &str) -> io::Result<[u8; 32]> {
        let root = self.root;
        Ok(hash_file(&root.join(rel_path))?.whole)
    }
}

/// Hashes in-memory content, used by tests and by the block-serving path.
pub struct MemoryHasher<'a> {
    contents: &'a BTreeMap<String, Vec<u8>>,
}

impl<'a> MemoryHasher<'a> {
    pub fn new(contents: &'a BTreeMap<String, Vec<u8>>) -> Self {
        Self { contents }
    }
}

impl ContentHasher for MemoryHasher<'_> {
    fn hash(&self, rel_path: &str) -> io::Result<[u8; 32]> {
        let Some(data) = self.contents.get(rel_path) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, rel_path.to_string()));
        };
        let mut hasher = Hasher::new();
        hasher.update(data);
        Ok(*hasher.finalize().as_bytes())
    }
}

/// Work the engine performs for one file after the index exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanAction {
    /// Download the peer's version and apply it (restoring the source mtime).
    FetchRemote,
    /// The peer deleted the file; park our copy in the trash.
    TrashLocal,
    /// Our state is the authority; the peer fetches from us. Nothing to do locally.
    AnnounceLocal,
    /// Concurrent changes: record both copies per the deterministic conflict layout.
    Conflict,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedFile {
    pub rel_path: String,
    pub action: PlanAction,
    /// The peer's announcement for this path (context for the action).
    pub remote: IndexEntry,
    /// Current local content hash when the file exists locally, `None` otherwise.
    pub local_hash: Option<[u8; 32]>,
    /// Local version vector including this device's pending change counters.
    pub local_version: VersionVector,
}

#[derive(Debug, Default, PartialEq)]
pub struct SyncPlan {
    /// Work list, sorted by path for deterministic progress reporting.
    pub actions: Vec<PlannedFile>,
    /// The index this side sends to the peer: current local state with bumped counters
    /// for changed files and tombstones for locally deleted ones.
    pub outgoing_index: Vec<IndexEntry>,
}

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("io while hashing: {0}")]
    Io(#[from] io::Error),
}

/// Hashed snapshot of one folder's local state. Built once per sync round; the
/// outgoing index is derived from it and the plan is computed against the peer's
/// announcement without touching the disk again.
pub struct FolderSnapshot {
    /// Current disk state, sorted by path.
    pub scan: Vec<ScannedFile>,
    /// The index this side announces to the peer (state + tombstones, bumped counters).
    pub outgoing: Vec<IndexEntry>,
    scan_by_path: BTreeMap<String, ScannedFile>,
    local: BTreeMap<String, LocalState>,
    tombstones: BTreeMap<String, IndexEntry>,
    /// The last-synced entries this snapshot was built from.
    base_map: BTreeMap<String, IndexEntry>,
}

impl FolderSnapshot {
    pub fn mtime_of(&self, rel_path: &str) -> Option<u64> {
        self.scan_by_path.get(rel_path).map(|f| f.mtime_ns)
    }

    pub fn size_of(&self, rel_path: &str) -> Option<u64> {
        self.scan_by_path.get(rel_path).map(|f| f.size)
    }

    pub fn tombstones(&self) -> impl Iterator<Item = &IndexEntry> {
        self.tombstones.values()
    }
}

/// Builds the local snapshot: current state with base-hash reuse for unchanged
/// (size, mtime) pairs, tombstones for vanished files.
pub fn snapshot_folder(
    self_id: &DeviceId,
    scan: Vec<ScannedFile>,
    base: &dyn FolderIndex,
    hasher: &dyn ContentHasher,
) -> Result<FolderSnapshot, PlanError> {
    let scan_by_path: BTreeMap<String, ScannedFile> =
        scan.iter().map(|f| (f.rel_path.clone(), f.clone())).collect();

    let mut local: BTreeMap<String, LocalState> = BTreeMap::new();
    for file in &scan {
        let base_entry = base.get(&file.rel_path);
        let unchanged = base_entry
            .map(|b| b.size == file.size && b.mtime_ns == file.mtime_ns)
            .unwrap_or(false);
        let content_hash = if unchanged {
            base_entry.map(|b| b.file_hash).expect("checked above")
        } else {
            hasher.hash(&file.rel_path)?
        };
        let changed = match base_entry {
            Some(b) => b.file_hash != content_hash,
            None => true,
        };
        let mut version = base_entry.map(|b| b.version.clone()).unwrap_or_default();
        if changed {
            increment(&mut version, *self_id);
        }
        local.insert(
            file.rel_path.clone(),
            LocalState {
                content_hash,
                version,
            },
        );
    }

    let base_map: BTreeMap<String, IndexEntry> = base
        .all()
        .into_iter()
        .map(|e| (e.rel_path.clone(), e.clone()))
        .collect();

    let mut tombstones: BTreeMap<String, IndexEntry> = BTreeMap::new();
    for base_entry in base.all() {
        if !local.contains_key(&base_entry.rel_path) {
            let mut version = base_entry.version.clone();
            increment(&mut version, *self_id);
            tombstones.insert(
                base_entry.rel_path.clone(),
                IndexEntry {
                    rel_path: base_entry.rel_path.clone(),
                    size: 0,
                    mtime_ns: 0,
                    file_hash: [0u8; 32],
                    version,
                    deleted: true,
                },
            );
        }
    }

    let mut outgoing: Vec<IndexEntry> = local
        .iter()
        .map(|(path, state)| {
            let file = &scan_by_path[path];
            IndexEntry {
                rel_path: path.clone(),
                size: file.size,
                mtime_ns: file.mtime_ns,
                file_hash: state.content_hash,
                version: state.version.clone(),
                deleted: false,
            }
        })
        .chain(tombstones.values().cloned())
        .collect();
    outgoing.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    Ok(FolderSnapshot {
        scan,
        outgoing,
        scan_by_path,
        local,
        tombstones,
        base_map,
    })
}

/// Compares the snapshot against the peer's announcement and produces the work list.
pub fn plan_against(snapshot: &FolderSnapshot, remote_entries: &[IndexEntry]) -> SyncPlan {
    let mut remote_by_path: BTreeMap<&str, &IndexEntry> = BTreeMap::new();
    for entry in remote_entries {
        remote_by_path.insert(entry.rel_path.as_str(), entry);
    }

    let mut actions = Vec::new();
    let mut paths: Vec<String> = snapshot
        .local
        .keys()
        .cloned()
        .chain(snapshot.tombstones.keys().cloned())
        .chain(remote_by_path.keys().map(|p| (*p).to_string()))
        .collect();
    paths.sort();
    paths.dedup();
    for path in paths {
        let remote = match remote_by_path.get(path.as_str()) {
            Some(entry) => (*entry).clone(),
            // The peer does not know this path: they will pull it from our outgoing
            // index, so there is nothing to do locally.
            None => continue,
        };

        let (local_hash, local_version) = match snapshot.local.get(&path) {
            Some(state) => (Some(state.content_hash), state.version.clone()),
            None => (
                None,
                snapshot
                    .tombstones
                    .get(&path)
                    .map(|t| t.version.clone())
                    .unwrap_or_default(),
            ),
        };
        let base_entry = snapshot.base_map.get(&path).cloned();
        let local_file = local_hash.map(|content_hash| crate::conflict::LocalFile {
            content_hash,
            version: local_version.clone(),
        });

        let resolution = decide(base_entry.as_ref(), local_file.as_ref(), &remote);
        let action = match resolution {
            Resolution::UpToDate | Resolution::KeepLocal => continue,
            Resolution::ApplyRemote => {
                if remote.deleted {
                    PlanAction::TrashLocal
                } else {
                    PlanAction::FetchRemote
                }
            }
            Resolution::Conflict => PlanAction::Conflict,
        };
        actions.push(PlannedFile {
            rel_path: path.clone(),
            action,
            remote,
            local_hash,
            local_version,
        });
    }
    actions.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    SyncPlan {
        actions,
        outgoing_index: snapshot.outgoing.clone(),
    }
}

#[derive(Debug, Clone)]
struct LocalState {
    content_hash: [u8; 32],
    version: VersionVector,
}

/// Number of 128 KiB blocks a file of `size` bytes is split into (for progress and
/// block requests).
pub fn block_count(size: u64) -> u32 {
    size.div_ceil(BLOCK_SIZE) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::MemoryIndex;
    use seams_identity::DeviceIdentity;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seams-plan-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(rel_path: &str, size: u64, mtime_ns: u64, hash_byte: u8, id: &DeviceId, counter: u64) -> IndexEntry {
        let mut version = VersionVector::new();
        version.insert(*id, counter);
        IndexEntry {
            rel_path: rel_path.to_string(),
            size,
            mtime_ns,
            file_hash: [hash_byte; 32],
            version,
            deleted: false,
        }
    }

    #[test]
    fn unchanged_files_reuse_base_hashes_without_rehash() {
        let id = DeviceIdentity::from_seed([1u8; 32]).device_id();
        let root = temp_dir("unchanged");
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let mut base = MemoryIndex::default();
        base.upsert(entry("a.txt", 5, 100, 7, &id, 1));

        let scan = vec![ScannedFile {
            rel_path: "a.txt".into(),
            size: 5,
            mtime_ns: 100,
        }];

        // The hasher would panic if consulted (empty contents) — proves the fast path.
        let empty = BTreeMap::new();
        let snapshot = snapshot_folder(&id, scan, &base, &MemoryHasher::new(&empty)).unwrap();
        let plan = plan_against(&snapshot, &[]);
        assert!(plan.actions.is_empty());
        assert_eq!(plan.outgoing_index.len(), 1);
        assert_eq!(plan.outgoing_index[0].file_hash, [7u8; 32]);
        assert_eq!(plan.outgoing_index[0].version[&id], 1);
    }

    #[test]
    fn local_edit_is_fetched_by_peer_and_bumps_counter() {
        let me = DeviceIdentity::from_seed([2u8; 32]).device_id();
        let mut contents = BTreeMap::new();
        contents.insert("doc.md".to_string(), b"new text".to_vec());

        let mut base = MemoryIndex::default();
        base.upsert(entry("doc.md", 4, 1, 1, &me, 1));

        let scan = vec![ScannedFile {
            rel_path: "doc.md".into(),
            size: 8,
            mtime_ns: 2,
        }];

        let snapshot = snapshot_folder(&me, scan, &base, &MemoryHasher::new(&contents)).unwrap();
        let plan = plan_against(&snapshot, &[]);
        assert!(plan.actions.is_empty(), "no remote yet: nothing to fetch");
        let out = &plan.outgoing_index[0];
        assert_ne!(out.file_hash, [1u8; 32]);
        assert_eq!(out.version[&me], 2, "counter bumped once for the edit");
    }

    #[test]
    fn peer_create_is_fetched_peer_delete_is_trashed() {
        let me = DeviceIdentity::from_seed([3u8; 32]).device_id();
        let peer = DeviceIdentity::from_seed([4u8; 32]).device_id();

        let mut base = MemoryIndex::default();
        base.upsert(entry("kept.md", 10, 1, 3, &me, 1));
        base.upsert(entry("deleted.md", 10, 1, 3, &me, 1));

        // deleted.md still exists locally; only the peer removed it.
        let scan = vec![
            ScannedFile {
                rel_path: "kept.md".into(),
                size: 10,
                mtime_ns: 1,
            },
            ScannedFile {
                rel_path: "deleted.md".into(),
                size: 10,
                mtime_ns: 1,
            },
        ];

        let remote = vec![
            entry("created.md", 20, 2, 5, &peer, 1),
            {
                let mut tomb = entry("deleted.md", 0, 0, 0, &peer, 1);
                tomb.deleted = true;
                tomb.file_hash = [0u8; 32];
                tomb
            },
        ];

        let contents = BTreeMap::new();
        let snapshot = snapshot_folder(&me, scan, &base, &MemoryHasher::new(&contents)).unwrap();
        let plan = plan_against(&snapshot, &remote);
        let by_path: BTreeMap<&str, &PlannedFile> = plan
            .actions
            .iter()
            .map(|p| (p.rel_path.as_str(), p))
            .collect();

        assert_eq!(by_path["created.md"].action, PlanAction::FetchRemote);
        assert_eq!(by_path["deleted.md"].action, PlanAction::TrashLocal);
        assert_eq!(plan.actions.len(), 2);
    }

    #[test]
    fn concurrent_edits_conflict_sequential_do_not() {
        let me = DeviceIdentity::from_seed([5u8; 32]).device_id();
        let peer = DeviceIdentity::from_seed([6u8; 32]).device_id();

        let mut base = MemoryIndex::default();
        base.upsert(entry("doc.md", 10, 1, 1, &me, 1));

        // Peer edited after seeing our v1: their vector covers ours plus their counter.
        let mut peer_version = VersionVector::new();
        peer_version.insert(me, 1);
        peer_version.insert(peer, 1);
        let peer_entry = IndexEntry {
            version: peer_version,
            file_hash: [9u8; 32],
            ..entry("doc.md", 12, 3, 9, &peer, 1)
        };

        // Case 1: we did not touch the file since v1 -> clean fetch.
        let scan = vec![ScannedFile {
            rel_path: "doc.md".into(),
            size: 10,
            mtime_ns: 1,
        }];
        let contents = BTreeMap::new();
        let snapshot =
            snapshot_folder(&me, scan, &base, &MemoryHasher::new(&contents)).unwrap();
        let plan = plan_against(&snapshot, &[peer_entry.clone()]);
        assert_eq!(plan.actions[0].action, PlanAction::FetchRemote);

        // Case 2: we edited too -> concurrent -> conflict.
        let mut contents = BTreeMap::new();
        contents.insert("doc.md".to_string(), b"local edit".to_vec());
        let scan_local = vec![ScannedFile {
            rel_path: "doc.md".into(),
            size: 10,
            mtime_ns: 2,
        }];
        let snapshot =
            snapshot_folder(&me, scan_local, &base, &MemoryHasher::new(&contents)).unwrap();
        let plan = plan_against(&snapshot, &[peer_entry]);
        assert_eq!(plan.actions[0].action, PlanAction::Conflict);
    }

    #[test]
    fn delete_on_both_sides_is_up_to_date() {
        let me = DeviceIdentity::from_seed([7u8; 32]).device_id();
        let peer = DeviceIdentity::from_seed([8u8; 32]).device_id();

        let mut base = MemoryIndex::default();
        base.upsert(entry("gone.md", 10, 1, 3, &me, 1));

        let mut peer_tomb = entry("gone.md", 0, 0, 0, &peer, 1);
        peer_tomb.deleted = true;
        peer_tomb.file_hash = [0u8; 32];

        let snapshot =
            snapshot_folder(&me, Vec::new(), &base, &MemoryHasher::new(&BTreeMap::new())).unwrap();
        let plan = plan_against(&snapshot, &[peer_tomb]);
        assert!(plan.actions.is_empty());
        // Our own tombstone is still announced so the peer's index converges.
        assert_eq!(plan.outgoing_index.len(), 1);
        assert!(plan.outgoing_index[0].deleted);
    }

    #[test]
    fn block_count_scales() {
        assert_eq!(block_count(0), 0);
        assert_eq!(block_count(1), 1);
        assert_eq!(block_count(BLOCK_SIZE), 1);
        assert_eq!(block_count(BLOCK_SIZE + 1), 2);
    }
}
