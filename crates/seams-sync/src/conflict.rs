//! The three-way conflict decision. The rules implemented here are normative in
//! `docs/CONFLICTS.md` — keep the two in sync.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use seams_identity::DeviceId;
use seams_proto::{relation, IndexEntry, Relation};

/// How a folder handles conflicts. Recorded per folder in the node configuration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictPolicy {
    #[default]
    Manual,
    KeepBoth,
    NewestWins,
}

/// Outcome of comparing the local state against a peer's announcement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// Both sides agree with the last synced state; nothing to transfer.
    UpToDate,
    /// The peer's version should be applied here (a deletion is applied via trash).
    ApplyRemote,
    /// Our version is the authority; it must be announced back to the peer.
    KeepLocal,
    /// Concurrent changes: both versions are kept, resolution belongs to the UI/policy.
    Conflict,
}

/// Current on-disk state of one local file, as observed by the sync engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalFile {
    pub content_hash: [u8; 32],
    /// The file's version vector including this device's local changes.
    pub version: seams_proto::VersionVector,
}

/// Decides what to do with `remote` given the last-synced `base` and the current local
/// state. Missing `base` means the file was not present at the last sync point.
pub fn decide(
    base: Option<&IndexEntry>,
    local: Option<&LocalFile>,
    remote: &IndexEntry,
) -> Resolution {
    let base_hash = base.map(|b| b.file_hash);
    let local_changed = local.map(|l| l.content_hash) != base_hash;
    let remote_changed = Some(remote.file_hash) != base_hash;

    // Both sides deleted the same file: nothing left to decide.
    if remote.deleted && local.is_none() {
        return Resolution::UpToDate;
    }

    match (local_changed, remote_changed) {
        (false, false) => Resolution::UpToDate,
        (false, true) => Resolution::ApplyRemote,
        (true, false) => Resolution::KeepLocal,
        (true, true) => match local {
            Some(l) if l.content_hash == remote.file_hash => Resolution::UpToDate,
            Some(l) => match relation(&l.version, &remote.version) {
                Relation::Ahead => Resolution::KeepLocal,
                Relation::Behind => Resolution::ApplyRemote,
                // Equal vectors with different content would mean history was tampered
                // with; treat it like a conflict rather than guessing.
                Relation::Equal | Relation::Concurrent => Resolution::Conflict,
            },
            // Local deleted while the peer edited: edit-vs-delete conflict (§2.2).
            None => Resolution::Conflict,
        },
    }
}

/// Which side keeps the original filename in a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictRole {
    Primary,
    ConflictCopy,
}

/// Deterministic tie-break (CONFLICTS.md §3): the device with the smaller ID keeps the
/// original filename. Both devices compute the same outcome without extra round-trips.
pub fn conflict_role(self_id: &DeviceId, peer_id: &DeviceId) -> ConflictRole {
    if self_id < peer_id {
        ConflictRole::Primary
    } else {
        ConflictRole::ConflictCopy
    }
}

/// The converged layout of one conflict: one copy keeps the original name, the other
/// carries the conflict suffix referencing the device whose version the copy holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictLayout {
    pub primary_name: String,
    pub conflict_copy_name: String,
}

pub fn conflict_layout(
    self_id: &DeviceId,
    peer_id: &DeviceId,
    original: &str,
    now: DateTime<Utc>,
) -> ConflictLayout {
    let copy_owner = if self_id < peer_id { peer_id } else { self_id };
    ConflictLayout {
        primary_name: original.to_string(),
        conflict_copy_name: super::trash::conflict_name(original, copy_owner, now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seams_identity::DeviceIdentity;
    use std::collections::BTreeMap;

    #[test]
    fn deleted_remote_with_no_local_file_is_up_to_date() {
        let remote = IndexEntry {
            rel_path: "gone.md".into(),
            size: 0,
            mtime_ns: 0,
            file_hash: [0u8; 32],
            version: BTreeMap::new(),
            deleted: true,
        };
        assert_eq!(decide(None, None, &remote), Resolution::UpToDate);
    }

    #[test]
    fn local_strictly_ahead_wins_without_conflict() {
        let self_id = DeviceIdentity::from_seed([31u8; 32]).device_id();
        let peer_id = DeviceIdentity::from_seed([32u8; 32]).device_id();

        let mut base_version = seams_proto::VersionVector::new();
        seams_proto::increment(&mut base_version, self_id);
        let base = IndexEntry {
            rel_path: "doc.md".into(),
            size: 1,
            mtime_ns: 0,
            file_hash: [1u8; 32],
            version: base_version,
            deleted: false,
        };

        // Local integrated the peer's change, then edited again -> strictly ahead.
        let mut local_version = base.version.clone();
        seams_proto::merge(&mut local_version, &BTreeMap::from([(peer_id, 2u64)]));
        seams_proto::increment(&mut local_version, self_id);
        let local = LocalFile {
            content_hash: [9u8; 32],
            version: local_version,
        };

        let mut remote = base.clone();
        remote.file_hash = [2u8; 32];
        remote.version = BTreeMap::from([(peer_id, 2u64)]);

        assert_eq!(decide(Some(&base), Some(&local), &remote), Resolution::KeepLocal);
    }
}
