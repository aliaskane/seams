//! Node configuration and the top-level facade over the seams engine. This crate hosts
//! the UniFFI interface that the Android (and later iOS) shells call.

mod config;
mod events;
mod index_store;
pub mod node;
mod session;

pub use config::{
    folder_id_from_label, AppConfig, CoreError, FolderConfig, NodeStore, PeerConfig,
};
pub use events::NodeEvent;
pub use node::{FolderInfo, NodeEvents, PeerInfo, SeamsNode};
pub use seams_sync::ConflictPolicy;

uniffi::setup_scaffolding!();

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seams-core-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn restart_persists_configuration() {
        let dir = temp_dir("restart");
        let folder_root = temp_dir("restart-folder");
        std::fs::create_dir_all(&folder_root).unwrap();

        let first_id;
        {
            let mut store = NodeStore::init(&dir, "Pixel").unwrap();
            let peer = store.identity().device_id();
            store.add_folder("Vault".into(), folder_root).unwrap();
            store.upsert_peer(peer, "Laptop".into(), None).unwrap();
            first_id = store.identity().device_id();
        }

        let store = NodeStore::init(&dir, "Pixel").unwrap();
        assert_eq!(store.config().folders.len(), 1);
        assert_eq!(store.config().folders[0].label, "Vault");
        assert_eq!(store.config().folders[0].conflict_policy, ConflictPolicy::Manual);
        assert_eq!(store.config().peers.len(), 1);
        assert_eq!(store.config().peers[0].name, "Laptop");
        assert_eq!(store.identity().device_id(), first_id);
    }
}
