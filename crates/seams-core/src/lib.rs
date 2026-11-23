//! Node configuration and the top-level facade over the seams engine. This crate will
//! host the UniFFI interface that the Android (and later iOS) shells call.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use seams_identity::{DeviceId, DeviceIdentity};
use seams_sync::ConflictPolicy;
use thiserror::Error;

const CONFIG_FILE: &str = "seams.json";

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("config file: {0}")]
    Json(#[from] serde_json::Error),
    #[error("identity: {0}")]
    Identity(#[from] seams_identity::IdentityError),
    #[error("{0}")]
    Invalid(String),
}

/// One synced folder on this device.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderConfig {
    pub id: String,
    pub label: String,
    /// Absolute path on this device; never synced across devices.
    pub path: PathBuf,
    pub conflict_policy: ConflictPolicy,
    /// Devices this folder is shared with.
    pub peers: Vec<DeviceId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerConfig {
    pub device_id: DeviceId,
    pub name: String,
    /// Last address announced on the LAN; a discovery hint for the next connect.
    pub last_addr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    pub device_name: String,
    pub folders: Vec<FolderConfig>,
    pub peers: Vec<PeerConfig>,
}

/// A seams node: persisted identity plus configuration over one data directory.
#[derive(Debug)]
pub struct Node {
    data_dir: PathBuf,
    identity: DeviceIdentity,
    config: AppConfig,
}

impl Node {
    /// Loads the node from `data_dir`, creating identity and config on first run.
    pub fn init(data_dir: &Path, device_name: &str) -> Result<Self, CoreError> {
        std::fs::create_dir_all(data_dir)?;
        let identity = DeviceIdentity::load_or_create(&data_dir.join("identity"))?;

        let config_path = data_dir.join(CONFIG_FILE);
        let config = match std::fs::read(&config_path) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let config = AppConfig {
                    device_name: device_name.to_string(),
                    folders: Vec::new(),
                    peers: Vec::new(),
                };
                std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;
                config
            }
            Err(e) => return Err(e.into()),
        };

        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            identity,
            config,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn device_name(&self) -> &str {
        &self.config.device_name
    }

    pub fn set_device_name(&mut self, name: String) -> Result<(), CoreError> {
        self.config.device_name = name;
        self.save_config()
    }

    pub fn add_folder(&mut self, id: String, label: String, path: PathBuf) -> Result<(), CoreError> {
        if !path.is_dir() {
            return Err(CoreError::Invalid(format!(
                "folder path `{}` does not exist",
                path.display()
            )));
        }
        if self.config.folders.iter().any(|f| f.id == id) {
            return Err(CoreError::Invalid(format!("folder id `{id}` already exists")));
        }
        self.config.folders.push(FolderConfig {
            id,
            label,
            path,
            conflict_policy: ConflictPolicy::default(),
            peers: Vec::new(),
        });
        self.save_config()
    }

    /// Returns whether a folder with this id existed and was removed.
    pub fn remove_folder(&mut self, id: &str) -> Result<bool, CoreError> {
        let before = self.config.folders.len();
        self.config.folders.retain(|f| f.id != id);
        let removed = self.config.folders.len() != before;
        if removed {
            self.save_config()?;
        }
        Ok(removed)
    }

    pub fn share_folder_with(&mut self, folder_id: &str, peer: DeviceId) -> Result<(), CoreError> {
        let folder = self
            .config
            .folders
            .iter_mut()
            .find(|f| f.id == folder_id)
            .ok_or_else(|| CoreError::Invalid(format!("unknown folder `{folder_id}`")))?;
        if !folder.peers.contains(&peer) {
            folder.peers.push(peer);
            self.save_config()?;
        }
        Ok(())
    }

    /// Records a paired device (or refreshes its name) in the peer list.
    pub fn upsert_peer(&mut self, device_id: DeviceId, name: String) -> Result<(), CoreError> {
        if let Some(peer) = self.config.peers.iter_mut().find(|p| p.device_id == device_id) {
            peer.name = name;
        } else {
            self.config.peers.push(PeerConfig {
                device_id,
                name,
                last_addr: None,
            });
        }
        self.save_config()
    }

    fn save_config(&self) -> Result<(), CoreError> {
        let path = self.data_dir.join(CONFIG_FILE);
        std::fs::write(path, serde_json::to_vec_pretty(&self.config)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("seams-core-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_run_creates_identity_and_config() {
        let dir = temp_dir("first-run");
        let node = Node::init(&dir, "Pixel").unwrap();

        assert_eq!(node.device_name(), "Pixel");
        assert!(dir.join("identity/identity.key").exists());
        assert!(dir.join(CONFIG_FILE).exists());
        assert!(node.config().folders.is_empty());
    }

    #[test]
    fn restart_persists_configuration() {
        let dir = temp_dir("restart");
        let folder_root = temp_dir("restart-folder");
        std::fs::create_dir_all(&folder_root).unwrap();

        let first_id;
        {
            let mut node = Node::init(&dir, "Pixel").unwrap();
            let peer = node.identity().device_id();
            node.add_folder("f1".into(), "Vault".into(), folder_root.clone())
                .unwrap();
            node.upsert_peer(peer, "Laptop".into()).unwrap();
            first_id = node.identity().device_id();
        }

        let node = Node::init(&dir, "Pixel").unwrap();
        assert_eq!(node.config().folders.len(), 1);
        assert_eq!(node.config().folders[0].label, "Vault");
        assert_eq!(node.config().folders[0].conflict_policy, ConflictPolicy::Manual);
        assert_eq!(node.config().peers.len(), 1);
        assert_eq!(node.config().peers[0].name, "Laptop");
        assert_eq!(node.identity().device_id(), first_id);
    }

    #[test]
    fn folder_validation() {
        let dir = temp_dir("validation");
        let mut node = Node::init(&dir, "Pixel").unwrap();

        let missing = dir.join("does-not-exist");
        assert!(matches!(
            node.add_folder("f1".into(), "X".into(), missing),
            Err(CoreError::Invalid(_))
        ));

        let real = temp_dir("validation-real");
        std::fs::create_dir_all(&real).unwrap();
        node.add_folder("f1".into(), "X".into(), real.clone()).unwrap();
        assert!(matches!(
            node.add_folder("f1".into(), "Y".into(), real),
            Err(CoreError::Invalid(_))
        ));

        assert!(node.remove_folder("f1").unwrap());
        assert!(!node.remove_folder("f1").unwrap());
    }

    #[test]
    fn share_folder_requires_existing_folder() {
        let dir = temp_dir("share");
        let mut node = Node::init(&dir, "Pixel").unwrap();
        let peer = DeviceIdentity::generate().device_id();

        assert!(matches!(
            node.share_folder_with("nope", peer),
            Err(CoreError::Invalid(_))
        ));

        let real = temp_dir("share-folder");
        std::fs::create_dir_all(&real).unwrap();
        node.add_folder("f1".into(), "X".into(), real).unwrap();
        node.share_folder_with("f1", peer).unwrap();
        node.share_folder_with("f1", peer).unwrap();
        assert_eq!(node.config().folders[0].peers.len(), 1);
    }
}
