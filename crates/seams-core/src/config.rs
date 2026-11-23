//! Folder and peer configuration plus the stable folder-id scheme.

use std::path::{Path, PathBuf};

use blake3::Hasher;
use serde::{Deserialize, Serialize};
use seams_identity::DeviceId;
pub use seams_sync::ConflictPolicy;
use thiserror::Error;

const CONFIG_FILE: &str = "seams.json";

/// FFI-friendly error: every variant carries a message string so it can cross the
/// UniFFI boundary to Kotlin.
#[derive(Debug, Error, uniffi::Error)]
pub enum CoreError {
    #[error("io: {0}")]
    Io(String),
    #[error("config file: {0}")]
    Json(String),
    #[error("identity: {0}")]
    Identity(String),
    #[error("protocol: {0}")]
    Proto(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("{0}")]
    Invalid(String),
}

impl From<std::io::Error> for CoreError {
    fn from(e: std::io::Error) -> Self {
        CoreError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for CoreError {
    fn from(e: serde_json::Error) -> Self {
        CoreError::Json(e.to_string())
    }
}

impl From<seams_identity::IdentityError> for CoreError {
    fn from(e: seams_identity::IdentityError) -> Self {
        CoreError::Identity(e.to_string())
    }
}

impl From<seams_proto::ProtoError> for CoreError {
    fn from(e: seams_proto::ProtoError) -> Self {
        CoreError::Proto(e.to_string())
    }
}

impl From<crate::session::SessionError> for CoreError {
    fn from(e: crate::session::SessionError) -> Self {
        CoreError::Transport(e.to_string())
    }
}

impl From<seams_transport::TransportError> for CoreError {
    fn from(e: seams_transport::TransportError) -> Self {
        CoreError::Transport(e.to_string())
    }
}

/// Folder ids are derived from the label, so two devices that both add a folder
/// called "Photos" match without any manual id exchange.
pub fn folder_id_from_label(label: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(label.trim().to_lowercase().as_bytes());
    let hash = hasher.finalize();
    format!("{:016x}", u64::from_be_bytes(hash.as_bytes()[..8].try_into().expect("fixed slice")))
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

/// Loads and saves the node configuration under one data directory.
#[derive(Debug)]
pub struct NodeStore {
    data_dir: PathBuf,
    identity: std::sync::Arc<seams_identity::DeviceIdentity>,
    config: AppConfig,
}

impl NodeStore {
    /// Loads the node from `data_dir`, creating identity and config on first run.
    pub fn init(data_dir: &Path, device_name: &str) -> Result<Self, CoreError> {
        std::fs::create_dir_all(data_dir)?;
        let identity =
            std::sync::Arc::new(seams_identity::DeviceIdentity::load_or_create(&data_dir.join("identity"))?);

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

    pub fn identity(&self) -> &std::sync::Arc<seams_identity::DeviceIdentity> {
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

    pub fn add_folder(&mut self, label: String, path: PathBuf) -> Result<String, CoreError> {
        if !path.is_dir() {
            return Err(CoreError::Invalid(format!(
                "folder path `{}` does not exist",
                path.display()
            )));
        }
        let id = folder_id_from_label(&label);
        if self.config.folders.iter().any(|f| f.id == id) {
            return Err(CoreError::Invalid(format!(
                "a folder labelled `{label}` already exists"
            )));
        }
        self.config.folders.push(FolderConfig {
            id: id.clone(),
            label,
            path,
            conflict_policy: ConflictPolicy::default(),
            peers: Vec::new(),
        });
        self.save_config()?;
        Ok(id)
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

    /// Records a paired device (or refreshes its name and last address).
    pub fn upsert_peer(
        &mut self,
        device_id: DeviceId,
        name: String,
        last_addr: Option<String>,
    ) -> Result<(), CoreError> {
        if let Some(peer) = self
            .config
            .peers
            .iter_mut()
            .find(|p| p.device_id == device_id)
        {
            peer.name = name;
            if last_addr.is_some() {
                peer.last_addr = last_addr;
            }
        } else {
            self.config.peers.push(PeerConfig {
                device_id,
                name,
                last_addr,
            });
        }
        self.save_config()
    }

    pub(crate) fn config_mut_folder(&mut self, id: &str) -> Option<&mut FolderConfig> {
        self.config.folders.iter_mut().find(|f| f.id == id)
    }

    pub fn save_config(&self) -> Result<(), CoreError> {
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
    fn folder_ids_match_across_devices_for_same_label() {
        assert_eq!(folder_id_from_label("Photos"), folder_id_from_label("photos "));
        assert_ne!(folder_id_from_label("Photos"), folder_id_from_label("Music"));
    }

    #[test]
    fn add_folder_uses_stable_id() {
        let dir = temp_dir("ids");
        let real = temp_dir("ids-real");
        std::fs::create_dir_all(&real).unwrap();
        let mut store = NodeStore::init(&dir, "Pixel").unwrap();
        let id = store.add_folder("Vault".into(), real).unwrap();
        assert_eq!(id, folder_id_from_label("vault"));
        assert_eq!(store.config().folders[0].id, id);
    }
}
