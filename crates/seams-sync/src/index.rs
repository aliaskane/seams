//! Per-folder index: the last known state of every file in a folder.

use std::collections::BTreeMap;

use seams_proto::IndexEntry;

pub trait FolderIndex {
    fn upsert(&mut self, entry: IndexEntry);
    fn get(&self, rel_path: &str) -> Option<&IndexEntry>;
    fn all(&self) -> Vec<&IndexEntry>;
    fn remove(&mut self, rel_path: &str) -> Option<IndexEntry>;
}

/// In-memory index for tests and the pre-persistence phase. The Android shell replaces
/// this with a SQLite-backed implementation that survives restarts.
#[derive(Debug, Default)]
pub struct MemoryIndex {
    entries: BTreeMap<String, IndexEntry>,
}

impl FolderIndex for MemoryIndex {
    fn upsert(&mut self, entry: IndexEntry) {
        self.entries.insert(entry.rel_path.clone(), entry);
    }

    fn get(&self, rel_path: &str) -> Option<&IndexEntry> {
        self.entries.get(rel_path)
    }

    fn all(&self) -> Vec<&IndexEntry> {
        self.entries.values().collect()
    }

    fn remove(&mut self, rel_path: &str) -> Option<IndexEntry> {
        self.entries.remove(rel_path)
    }
}
