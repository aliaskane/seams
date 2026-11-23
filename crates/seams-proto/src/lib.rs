//! Seams wire protocol: message types, encoding, and version vector semantics.

use std::collections::BTreeMap;

use seams_identity::DeviceId;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Version of the wire protocol this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Per-file version vector: `DeviceId -> change counter`, missing entries mean zero.
pub type VersionVector = BTreeMap<DeviceId, u64>;

/// Partial-order relation between two version vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Equal,
    Ahead,
    Behind,
    Concurrent,
}

/// Compares `a` against `b`: `Ahead` means `a` saw every change `b` has, plus some.
pub fn relation(a: &VersionVector, b: &VersionVector) -> Relation {
    match (dominates(a, b), dominates(b, a)) {
        (true, true) => Relation::Equal,
        (true, false) => Relation::Ahead,
        (false, true) => Relation::Behind,
        (false, false) => Relation::Concurrent,
    }
}

/// True when every counter in `b` is covered by `a` (missing counts as zero).
fn dominates(a: &VersionVector, b: &VersionVector) -> bool {
    b.iter()
        .all(|(id, &count)| a.get(id).copied().unwrap_or(0) >= count)
}

pub fn increment(vv: &mut VersionVector, id: DeviceId) {
    *vv.entry(id).or_insert(0) += 1;
}

/// Element-wise maximum, used after a conflict or transfer to merge both sides' history.
pub fn merge(vv: &mut VersionVector, other: &VersionVector) {
    for (id, &count) in other {
        let slot = vv.entry(*id).or_insert(0);
        if *slot < count {
            *slot = count;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub device_id: DeviceId,
    pub device_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderAnnounce {
    pub folder_id: String,
    pub label: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexEntry {
    /// `/`-separated path relative to the folder root, identical on all platforms.
    pub rel_path: String,
    pub size: u64,
    pub mtime_ns: u64,
    pub file_hash: [u8; 32],
    pub version: VersionVector,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    Hello(Hello),
    AnnounceFolders(Vec<FolderAnnounce>),
    Index {
        folder_id: String,
        entries: Vec<IndexEntry>,
    },
    RequestBlocks {
        folder_id: String,
        rel_path: String,
        blocks: Vec<u32>,
    },
    BlockData {
        folder_id: String,
        rel_path: String,
        block_index: u32,
        data: Vec<u8>,
    },
}

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("message encoding failed")]
    Encoding(#[from] postcard::Error),
    #[error("peer speaks protocol {peer}, this build speaks {ours}")]
    IncompatibleProtocol { peer: u32, ours: u32 },
}

/// Encodes one message; each message travels in its own QUIC stream, so no framing is
/// needed beyond the postcard payload itself.
pub fn encode(msg: &Message) -> Result<Vec<u8>, ProtoError> {
    Ok(postcard::to_allocvec(msg)?)
}

pub fn decode(bytes: &[u8]) -> Result<Message, ProtoError> {
    Ok(postcard::from_bytes(bytes)?)
}

pub fn check_compatibility(peer_version: u32) -> Result<(), ProtoError> {
    if peer_version == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(ProtoError::IncompatibleProtocol {
            peer: peer_version,
            ours: PROTOCOL_VERSION,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seams_identity::DeviceIdentity;

    fn test_device(seed: u8) -> DeviceIdentity {
        let mut seed_bytes = [0u8; 32];
        seed_bytes[0] = seed;
        DeviceIdentity::from_seed(seed_bytes)
    }

    #[test]
    fn version_vector_relations() {
        let a_id = test_device(1).device_id();
        let b_id = test_device(2).device_id();

        let mut a: VersionVector = BTreeMap::new();
        let mut b: VersionVector = BTreeMap::new();

        assert_eq!(relation(&a, &b), Relation::Equal);

        increment(&mut a, a_id);
        assert_eq!(relation(&a, &b), Relation::Ahead);
        assert_eq!(relation(&b, &a), Relation::Behind);

        increment(&mut b, b_id);
        assert_eq!(relation(&a, &b), Relation::Concurrent);

        merge(&mut a, &b);
        increment(&mut a, b_id);
        assert_eq!(relation(&a, &b), Relation::Ahead);
        assert_eq!(a[&a_id], 1);
        assert_eq!(a[&b_id], 2);
    }

    #[test]
    fn missing_counters_count_as_zero() {
        let id = test_device(3).device_id();
        let empty: VersionVector = BTreeMap::new();
        let mut one = VersionVector::new();
        increment(&mut one, id);
        assert_eq!(relation(&one, &empty), Relation::Ahead);
        assert_eq!(relation(&empty, &one), Relation::Behind);
    }

    #[test]
    fn message_roundtrip_covers_every_variant() {
        let id = test_device(4).device_id();
        let mut version = VersionVector::new();
        increment(&mut version, id);

        let messages = vec![
            Message::Hello(Hello {
                protocol_version: PROTOCOL_VERSION,
                device_id: id,
                device_name: "Pixel".into(),
            }),
            Message::AnnounceFolders(vec![FolderAnnounce {
                folder_id: "f-1".into(),
                label: "Obsidian".into(),
                read_only: false,
            }]),
            Message::Index {
                folder_id: "f-1".into(),
                entries: vec![IndexEntry {
                    rel_path: "notes/todo.md".into(),
                    size: 42,
                    mtime_ns: 1_700_000_000_000_000_000,
                    file_hash: [9u8; 32],
                    version,
                    deleted: false,
                }],
            },
            Message::RequestBlocks {
                folder_id: "f-1".into(),
                rel_path: "big.zip".into(),
                blocks: vec![0, 1, 2],
            },
            Message::BlockData {
                folder_id: "f-1".into(),
                rel_path: "big.zip".into(),
                block_index: 1,
                data: vec![7u8; 128 * 1024],
            },
        ];

        for msg in &messages {
            let encoded = encode(msg).expect("encode");
            assert_eq!(&decode(&encoded).expect("decode"), msg);
        }
    }

    #[test]
    fn protocol_version_check() {
        assert!(check_compatibility(PROTOCOL_VERSION).is_ok());
        assert!(matches!(
            check_compatibility(PROTOCOL_VERSION + 1),
            Err(ProtoError::IncompatibleProtocol { .. })
        ));
    }
}
