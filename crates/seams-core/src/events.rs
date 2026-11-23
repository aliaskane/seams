//! Events the engine pushes to the UI layer.

/// Everything the Android UI needs to react to, delivered through a UniFFI callback
/// interface.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum NodeEvent {
    /// A device announced itself on the LAN (mDNS). Shown in the device list.
    DiscoveryObserved {
        device_id: String,
        name: String,
        addr: String,
        port: u16,
        protocol_version: u32,
    },
    /// A session started with a peer.
    PeerConnected { device_id: String, name: String },
    /// Index exchange finished for one folder; transfer work begins.
    FolderSyncStarted {
        folder_id: String,
        peer_id: String,
        files_total: u32,
    },
    /// One file finished applying (in either direction).
    FileProgress {
        folder_id: String,
        peer_id: String,
        rel_path: String,
        files_done: u32,
        files_total: u32,
    },
    /// The folder reached a converged state with the peer.
    FolderSynced {
        folder_id: String,
        peer_id: String,
        fetched: u32,
        trashed: u32,
        announced: u32,
        conflicts: u32,
    },
    /// A folder exists on the peer but is not added on this device yet.
    FolderOffered { folder_id: String, label: String, peer_id: String },
    SyncFailed { device_id: String, message: String },
    Notice { message: String },
}
