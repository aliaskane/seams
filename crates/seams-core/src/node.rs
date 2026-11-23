//! Node runtime: binds the transport, runs discovery, accepts peers, and drives sync
//! sessions. `SeamsNode` is the object the Android UI talks to through UniFFI.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use seams_identity::DeviceId;
use tokio::sync::broadcast;

use crate::config::{CoreError, FolderConfig, NodeStore};
use crate::events::NodeEvent;
use crate::session::{run_session, FolderReport, FolderWork};

pub(crate) struct Engine {
    pub data_dir: PathBuf,
    pub identity: Arc<seams_identity::DeviceIdentity>,
    pub store: RwLock<NodeStore>,
    pub events: broadcast::Sender<NodeEvent>,
    pub transport: Mutex<Option<seams_transport::Transport>>,
}

/// UI-facing folder summary.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FolderInfo {
    pub id: String,
    pub label: String,
    pub path: String,
    /// "manual" | "keep_both" | "newest_wins"
    pub conflict_policy: String,
}

/// UI-facing peer summary.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PeerInfo {
    pub device_id: String,
    pub name: String,
    pub last_addr: Option<String>,
}

/// Events pushed from the engine to the UI thread.
#[uniffi::export(callback_interface)]
pub trait NodeEvents: Send + Sync {
    fn on_event(&self, event: NodeEvent);
}

/// The sync node. All methods are safe to call from any thread; async methods run on
/// the UniFFI tokio runtime.
#[derive(uniffi::Object)]
pub struct SeamsNode {
    engine: Arc<Engine>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    discovery_stop: Mutex<Option<Arc<std::sync::atomic::AtomicBool>>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl SeamsNode {
    /// Loads (or creates) the node identity and configuration under `data_dir`.
    #[uniffi::constructor]
    pub fn create(data_dir: String, device_name: String) -> Result<Self, CoreError> {
        let store = NodeStore::init(Path::new(&data_dir), &device_name)?;
        let identity = store.identity().clone();
        let (events, _) = broadcast::channel(256);
        Ok(Self {
            engine: Arc::new(Engine {
                data_dir: PathBuf::from(&data_dir),
                identity,
                store: RwLock::new(store),
                events,
                transport: Mutex::new(None),
            }),
            tasks: Mutex::new(Vec::new()),
            discovery_stop: Mutex::new(None),
        })
    }

    pub fn device_id(&self) -> String {
        self.engine.identity.device_id().to_string()
    }

    pub fn short_device_id(&self) -> String {
        self.engine.identity.device_id().short()
    }

    pub fn device_name(&self) -> String {
        self.lock_store().device_name().to_string()
    }

    pub fn set_device_name(&self, name: String) -> Result<(), CoreError> {
        self.lock_store_mut()?.set_device_name(name)
    }

    pub fn list_folders(&self) -> Vec<FolderInfo> {
        self.lock_store()
            .config()
            .folders
            .iter()
            .map(|f| FolderInfo {
                id: f.id.clone(),
                label: f.label.clone(),
                path: f.path.display().to_string(),
                conflict_policy: policy_name(f),
            })
            .collect()
    }

    pub fn list_peers(&self) -> Vec<PeerInfo> {
        self.lock_store()
            .config()
            .peers
            .iter()
            .map(|p| PeerInfo {
                device_id: p.device_id.to_string(),
                name: p.name.clone(),
                last_addr: p.last_addr.clone(),
            })
            .collect()
    }

    /// Adds a folder to sync. The folder id is derived from the label, so devices that
    /// both add the same label match automatically.
    pub fn add_folder(&self, label: String, path: String) -> Result<String, CoreError> {
        self.lock_store_mut()?.add_folder(label, PathBuf::from(path))
    }

    pub fn remove_folder(&self, folder_id: String) -> Result<bool, CoreError> {
        self.lock_store_mut()?.remove_folder(&folder_id)
    }

    /// Sets the conflict policy: "manual", "keep_both" or "newest_wins".
    pub fn set_conflict_policy(&self, folder_id: String, policy: String) -> Result<(), CoreError> {
        use crate::config::ConflictPolicy;
        let parsed = match policy.as_str() {
            "manual" => ConflictPolicy::Manual,
            "keep_both" => ConflictPolicy::KeepBoth,
            "newest_wins" => ConflictPolicy::NewestWins,
            other => {
                return Err(CoreError::Invalid(format!(
                    "unknown conflict policy `{other}`"
                )))
            }
        };
        let mut store = self.lock_store_mut()?;
        let folder = store
            .config_mut_folder(&folder_id)
            .ok_or_else(|| CoreError::Invalid(format!("unknown folder `{folder_id}`")))?;
        folder.conflict_policy = parsed;
        store.save_config()
    }

    /// Starts listening for peers and advertising on the LAN. `events` receives every
    /// [NodeEvent] from a background thread.
    pub async fn start(&self, port: u16, events: Box<dyn NodeEvents>) -> Result<(), CoreError> {
        let transport = self.ensure_transport(port)?;
        let actual_port = transport.local_addr()?.port();

        // Forward engine events to the UI callback.
        let mut rx = self.engine.events.subscribe();
        let forward = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => events.on_event(event),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.push_task(forward);

        // Accept loop: every inbound connection becomes a session.
        let accept_transport = transport.clone();
        let accept_engine = self.engine.clone();
        let accept = tokio::spawn(async move {
            loop {
                let conn = match accept_transport.accept().await {
                    Ok(conn) => conn,
                    Err(_) => break,
                };
                let engine = accept_engine.clone();
                tokio::spawn(async move {
                    let addr_hint = Some(conn.remote_address().to_string());
                    let _ = run_sync_session(&engine, conn, addr_hint).await;
                });
            }
        });
        self.push_task(accept);

        // Discovery runs on its own thread: mdns-sd uses blocking channels.
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (name, device_id, events_tx) = {
            let store = self.lock_store();
            (
                store.device_name().to_string(),
                self.engine.identity.device_id(),
                self.engine.events.clone(),
            )
        };
        match seams_discovery::MdnsDiscovery::start(&device_id, &name, actual_port) {
            Ok(discovery) => {
                *self.discovery_stop.lock().expect("poisoned") = Some(stop.clone());
                std::thread::spawn(move || loop {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        discovery.stop();
                        break;
                    }
                    if let Some(peer) = discovery.next_peer(std::time::Duration::from_secs(2)) {
                        let _ = events_tx.send(NodeEvent::DiscoveryObserved {
                            device_id: peer.device_id.to_string(),
                            name: peer.name,
                            addr: peer.addr.to_string(),
                            port: peer.port,
                            protocol_version: peer.protocol_version,
                        });
                    }
                });
            }
            Err(e) => {
                let _ = self.engine.events.send(NodeEvent::Notice {
                    message: format!("discovery unavailable: {e}"),
                });
            }
        }

        Ok(())
    }

    /// Connects to a discovered device, syncs all matching folders, and records the
    /// pairing. Returns per-folder reports.
    pub async fn connect_and_sync(
        &self,
        device_id: String,
        addr: String,
        port: u16,
        name: String,
    ) -> Result<Vec<FolderReport>, CoreError> {
        let expected = DeviceId::from_base32(&device_id)
            .ok_or_else(|| CoreError::Invalid("device id is not valid base32".into()))?;
        let socket: SocketAddr = format!("{addr}:{port}")
            .parse()
            .map_err(|e| CoreError::Invalid(format!("bad address `{addr}:{port}`: {e}")))?;

        let transport = self.ensure_transport(0)?;
        let conn = transport
            .connect(&self.engine.identity, socket, expected)
            .await?;

        let outcome = run_sync_session(&self.engine, conn, Some(socket.to_string())).await?;
        // Prefer the human-readable name from discovery/Hello.
        let display_name = if name.is_empty() {
            outcome.peer_name.clone()
        } else {
            name
        };
        self.lock_store_mut()?
            .upsert_peer(outcome.peer_id, display_name, Some(socket.to_string()))?;
        Ok(outcome.reports)
    }

    /// Syncs with every paired peer whose last known address we have. Failures are
    /// reported as events, not errors, so one offline peer does not stop the rest.
    pub async fn sync_known_peers(&self) -> Result<Vec<FolderReport>, CoreError> {
        let peers: Vec<PeerInfo> = self.list_peers();
        let mut reports = Vec::new();
        for peer in peers {
            let Some(addr) = peer.last_addr.clone() else {
                continue;
            };
            let socket: SocketAddr = match addr.parse() {
                Ok(socket) => socket,
                Err(_) => {
                    let _ = self.engine.events.send(NodeEvent::SyncFailed {
                        device_id: peer.device_id.clone(),
                        message: format!("stored address `{addr}` is unusable"),
                    });
                    continue;
                }
            };
            match self
                .connect_and_sync(
                    peer.device_id.clone(),
                    socket.ip().to_string(),
                    socket.port(),
                    peer.name.clone(),
                )
                .await
            {
                Ok(mut r) => reports.append(&mut r),
                Err(e) => {
                    let _ = self.engine.events.send(NodeEvent::SyncFailed {
                        device_id: peer.device_id.clone(),
                        message: e.to_string(),
                    });
                }
            }
        }
        Ok(reports)
    }

    /// Stops listening, discovery, and all background tasks.
    pub fn stop(&self) {
        if let Some(stop) = self.discovery_stop.lock().expect("poisoned").take() {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        for task in self.tasks.lock().expect("poisoned").drain(..) {
            task.abort();
        }
        if let Some(transport) = self.engine.transport.lock().expect("poisoned").take() {
            drop(transport);
        }
    }

    /// Emits a test event through the normal event pipeline (used by integration tests).
    pub fn emit_notice(&self, message: String) {
        let _ = self.engine.events.send(NodeEvent::Notice { message });
    }
}

impl SeamsNode {
    fn lock_store(&self) -> std::sync::RwLockReadGuard<'_, NodeStore> {
        self.engine.store.read().expect("node store poisoned")
    }

    fn lock_store_mut(&self) -> Result<std::sync::RwLockWriteGuard<'_, NodeStore>, CoreError> {
        self.engine
            .store
            .write()
            .map_err(|_| CoreError::Invalid("node store poisoned".into()))
    }

    fn push_task(&self, task: tokio::task::JoinHandle<()>) {
        self.tasks.lock().expect("poisoned").push(task);
    }

    fn ensure_transport(&self, port: u16) -> Result<seams_transport::Transport, CoreError> {
        let mut guard = self
            .engine
            .transport
            .lock()
            .map_err(|_| CoreError::Invalid("transport lock poisoned".into()))?;
        if let Some(existing) = guard.as_ref() {
            return Ok(existing.clone());
        }
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let transport = seams_transport::Transport::bind(self.engine.identity.clone(), addr)?;
        *guard = Some(transport.clone());
        Ok(transport)
    }
}

fn policy_name(folder: &FolderConfig) -> String {
    use crate::config::ConflictPolicy;
    match folder.conflict_policy {
        ConflictPolicy::Manual => "manual".to_string(),
        ConflictPolicy::KeepBoth => "keep_both".to_string(),
        ConflictPolicy::NewestWins => "newest_wins".to_string(),
    }
}

/// Shared session driver for inbound (accept) and outbound (connect) connections.
pub(crate) async fn run_sync_session(
    engine: &Arc<Engine>,
    conn: quinn::Connection,
    addr_hint: Option<String>,
) -> Result<crate::session::SessionOutcome, CoreError> {
    let (folders, device_name) = {
        let store = engine
            .store
            .read()
            .map_err(|_| CoreError::Invalid("node store poisoned".into()))?;
        let mut works = Vec::new();
        for f in &store.config().folders {
            let base = crate::index_store::load(&engine.data_dir, &f.id)?;
            works.push(FolderWork {
                config: f.clone(),
                base_index: base,
            });
        }
        (works, store.device_name().to_string())
    };

    let outcome = run_session(
        conn,
        engine.identity.clone(),
        device_name,
        &engine.data_dir,
        folders,
        engine.events.clone(),
    )
    .await?;

    {
        let mut store = engine
            .store
            .write()
            .map_err(|_| CoreError::Invalid("node store poisoned".into()))?;
        store.upsert_peer(
            outcome.peer_id,
            outcome.peer_name.clone(),
            addr_hint.clone(),
        )?;
        for id in &outcome.matched_folder_ids {
            let _ = store.share_folder_with(id, outcome.peer_id);
        }
    }

    Ok(outcome)
}
