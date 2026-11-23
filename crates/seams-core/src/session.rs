//! One sync session with a peer: handshake, folder matching, index exchange, transfer,
//! and conflict handling. Both sides run the same symmetric code, so outcomes converge
//! without extra round-trips (docs/CONFLICTS.md).

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use quinn::Connection;
use seams_identity::{DeviceId, DeviceIdentity};
use seams_proto::{check_compatibility, IndexEntry, Message, PROTOCOL_VERSION};
use seams_sync::hash::{hash_file, BLOCK_SIZE};
use seams_sync::{
    block_count, conflict_layout, conflict_role, finalize_incoming, plan_against,
    rename_to_conflict_copy, snapshot_folder, stage_file, trash_file, ConflictRole, DiskHasher,
    FolderSnapshot, MemoryIndex, PlanAction,
};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};

use crate::config::FolderConfig;
use crate::events::NodeEvent;
use seams_transport::{recv_message, send_message};

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("transport: {0}")]
    Transport(#[from] seams_transport::TransportError),
    #[error("protocol: {0}")]
    Proto(#[from] seams_proto::ProtoError),
    #[error("scan: {0}")]
    Scan(#[from] seams_sync::ScanError),
    #[error("plan: {0}")]
    Plan(#[from] seams_sync::PlanError),
    #[error("index store: {0}")]
    IndexStore(#[from] crate::config::CoreError),
    #[error("hash: {0}")]
    Hash(#[from] seams_sync::hash::HashError),
    #[error("session: {0}")]
    Other(String),
}

/// What changed in one folder during the session. Plain data so it crosses the UniFFI
/// boundary; the converged index is persisted by the engine itself.
#[derive(Debug, Clone, uniffi::Record)]
pub struct FolderReport {
    pub folder_id: String,
    pub fetched: u32,
    pub trashed: u32,
    pub announced: u32,
    pub conflicts: u32,
    pub bytes_transferred: u64,
}

/// One folder's session input: its config plus the last-synced index.
#[derive(Debug)]
pub struct FolderWork {
    pub config: FolderConfig,
    pub base_index: MemoryIndex,
}

#[derive(Debug)]
pub struct SessionOutcome {
    pub peer_id: DeviceId,
    pub peer_name: String,
    pub matched_folder_ids: Vec<String>,
    pub reports: Vec<FolderReport>,
}

/// Drives a full session over an established, pinned connection.
pub(crate) async fn run_session(
    conn: Connection,
    identity: Arc<DeviceIdentity>,
    device_name: String,
    data_dir: &Path,
    folders: Vec<FolderWork>,
    events: broadcast::Sender<NodeEvent>,
) -> Result<SessionOutcome, SessionError> {
    let self_id = identity.device_id();

    let (control_tx, mut control_rx) = mpsc::channel::<Message>(32);
    let (blocks_tx, mut blocks_rx) = mpsc::channel::<Message>(64);

    let our_announce: Vec<seams_proto::FolderAnnounce> = folders
        .iter()
        .map(|f| seams_proto::FolderAnnounce {
            folder_id: f.config.id.clone(),
            label: f.config.label.clone(),
            read_only: false,
        })
        .collect();
    let folder_roots: HashMap<String, PathBuf> = folders
        .iter()
        .map(|f| (f.config.id.clone(), f.config.path.clone()))
        .collect();

    send_message(
        &conn,
        &Message::Hello(seams_proto::Hello {
            protocol_version: PROTOCOL_VERSION,
            device_id: self_id,
            device_name,
        }),
    )
    .await?;

    // The dispatcher routes incoming streams: protocol messages go to `control`, block
    // data to `blocks`, block requests are served concurrently.
    let dispatcher_conn = conn.clone();
    let dispatch = tokio::spawn(async move {
        while let Ok(msg) = recv_message(&dispatcher_conn).await {
            match msg {
                Message::RequestBlocks { .. } => {
                    let serving_conn = dispatcher_conn.clone();
                    let serving_roots = folder_roots.clone();
                    tokio::spawn(async move {
                        let _ = serve_blocks(serving_conn, msg, &serving_roots).await;
                    });
                }
                Message::BlockData { .. } => {
                    let _ = blocks_tx.send(msg).await;
                }
                other => {
                    if control_tx.send(other).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    tokio::spawn(async move {
        let _ = dispatch.await;
    });

    send_message(&conn, &Message::AnnounceFolders(our_announce)).await?;

    let peer_hello = expect_hello(&mut control_rx).await?;
    check_compatibility(peer_hello.protocol_version)?;
    let peer_id = peer_hello.device_id;
    let _ = events.send(NodeEvent::PeerConnected {
        device_id: peer_id.to_string(),
        name: peer_hello.device_name.clone(),
    });

    let peer_announce = expect_announce(&mut control_rx).await?;
    let peer_labels: BTreeMap<String, String> = peer_announce
        .iter()
        .map(|f| (f.folder_id.clone(), f.label.clone()))
        .collect();

    let our_ids: Vec<String> = folders.iter().map(|f| f.config.id.clone()).collect();
    let mut matched: Vec<String> = our_ids
        .iter()
        .filter(|id| peer_labels.contains_key(*id))
        .cloned()
        .collect();
    matched.sort();

    // Folders the peer offers that we do not have yet — the UI surfaces these.
    for (id, label) in &peer_labels {
        if !our_ids.contains(id) {
            let _ = events.send(NodeEvent::FolderOffered {
                folder_id: id.clone(),
                label: label.clone(),
                peer_id: peer_id.to_string(),
            });
        }
    }

    // Phase A: snapshot every matched folder (hash changed files once), announce indexes.
    let mut snapshots: BTreeMap<String, FolderSnapshot> = BTreeMap::new();
    let mut work_by_id: BTreeMap<String, FolderWork> = folders
        .into_iter()
        .map(|f| (f.config.id.clone(), f))
        .collect();

    for folder_id in &matched {
        let work = work_by_id.get_mut(folder_id).expect("matched folder");
        let scan = seams_sync::scan_folder(&work.config.path)?;
        let hasher = DiskHasher::new(&work.config.path);
        let snapshot = snapshot_folder(&self_id, scan, &work.base_index, &hasher)?;
        send_message(
            &conn,
            &Message::Index {
                folder_id: folder_id.clone(),
                entries: snapshot.outgoing.clone(),
            },
        )
        .await?;
        snapshots.insert(folder_id.clone(), snapshot);
    }

    // Phase B: collect the peer's indexes for the same folders, in the same order.
    let mut remote_indexes: BTreeMap<String, Vec<IndexEntry>> = BTreeMap::new();
    for _ in &matched {
        match control_rx.recv().await {
            Some(Message::Index { folder_id, entries }) => {
                remote_indexes.insert(folder_id, entries);
            }
            Some(_) => continue,
            None => {
                return Err(SessionError::Other(
                    "connection closed during index exchange".into(),
                ))
            }
        }
    }

    // Phase C: plan and execute per folder.
    let mut reports = Vec::new();
    for folder_id in &matched {
        let work = work_by_id.get_mut(folder_id).expect("matched folder");
        let snapshot = snapshots.remove(folder_id).expect("snapshot built");
        let remote_entries = remote_indexes.remove(folder_id).unwrap_or_default();
        let plan = plan_against(&snapshot, &remote_entries);

        let files_total = plan.actions.len() as u32;
        let _ = events.send(NodeEvent::FolderSyncStarted {
            folder_id: folder_id.clone(),
            peer_id: peer_id.to_string(),
            files_total,
        });

        // The converged index starts from OUR current state (outgoing entries keep our
        // version vectors alive across sessions), then action results override.
        let mut final_index: BTreeMap<String, IndexEntry> = snapshot
            .outgoing
            .iter()
            .map(|e| (e.rel_path.clone(), e.clone()))
            .collect();
        let mut counters = FolderReport {
            folder_id: folder_id.clone(),
            fetched: 0,
            trashed: 0,
            announced: 0,
            conflicts: 0,
            bytes_transferred: 0,
        };

        for (i, planned) in plan.actions.iter().enumerate() {
            match planned.action {
                PlanAction::FetchRemote => {
                    let staged =
                        fetch_file(&conn, &work.config.path, folder_id, &planned.remote, &mut blocks_rx)
                            .await?;
                    finalize_incoming(
                        &work.config.path,
                        &planned.rel_path,
                        &staged,
                        planned.remote.mtime_ns,
                    )?;
                    counters.fetched += 1;
                    counters.bytes_transferred += planned.remote.size;
                    let version = merged_version(&planned.local_version, &planned.remote);
                    final_index.insert(
                        planned.rel_path.clone(),
                        IndexEntry {
                            rel_path: planned.rel_path.clone(),
                            size: planned.remote.size,
                            mtime_ns: planned.remote.mtime_ns,
                            file_hash: planned.remote.file_hash,
                            version,
                            deleted: false,
                        },
                    );
                }
                PlanAction::TrashLocal => {
                    trash_file(&work.config.path, &planned.rel_path, chrono::Utc::now())?;
                    counters.trashed += 1;
                    let version = merged_version(&planned.local_version, &planned.remote);
                    final_index.insert(
                        planned.rel_path.clone(),
                        IndexEntry {
                            rel_path: planned.rel_path.clone(),
                            size: 0,
                            mtime_ns: 0,
                            file_hash: [0u8; 32],
                            version,
                            deleted: true,
                        },
                    );
                }
                PlanAction::AnnounceLocal => {
                    counters.announced += 1;
                    if let Some(entry) = snapshot_outgoing_entry(&snapshot, &planned.rel_path) {
                        final_index.insert(planned.rel_path.clone(), entry);
                    }
                }
                PlanAction::Conflict => {
                    apply_conflict(
                        &conn,
                        &self_id,
                        &peer_id,
                        &work.config,
                        &snapshot,
                        planned,
                        &mut blocks_rx,
                        &mut final_index,
                        &mut counters,
                    )
                    .await?;
                }
            }
            let _ = events.send(NodeEvent::FileProgress {
                folder_id: folder_id.clone(),
                peer_id: peer_id.to_string(),
                rel_path: planned.rel_path.clone(),
                files_done: i as u32 + 1,
                files_total,
            });
        }

        crate::index_store::save_from_map(data_dir, folder_id, &final_index)?;
        let _ = events.send(NodeEvent::FolderSynced {
            folder_id: folder_id.clone(),
            peer_id: peer_id.to_string(),
            fetched: counters.fetched,
            trashed: counters.trashed,
            announced: counters.announced,
            conflicts: counters.conflicts,
        });
        reports.push(counters);
    }

    // Both sides wait for the peer's completion signal so the connection outlives both
    // transfer phases; dropping early would kill the peer's session mid-flight.
    send_message(&conn, &Message::SyncComplete).await?;
    wait_peer_complete(&mut control_rx).await;

    Ok(SessionOutcome {
        peer_id,
        peer_name: peer_hello.device_name,
        matched_folder_ids: matched,
        reports,
    })
}

async fn wait_peer_complete(control: &mut mpsc::Receiver<Message>) {
    loop {
        match control.recv().await {
            Some(Message::SyncComplete) => return,
            Some(_) => continue,
            // The peer went away; our own work is done, so treat this as completion.
            None => return,
        }
    }
}

fn snapshot_outgoing_entry(snapshot: &FolderSnapshot, rel_path: &str) -> Option<IndexEntry> {
    snapshot
        .outgoing
        .iter()
        .find(|e| e.rel_path == rel_path)
        .cloned()
}

fn merged_version(
    local: &seams_proto::VersionVector,
    remote: &IndexEntry,
) -> seams_proto::VersionVector {
    let mut version = local.clone();
    seams_sync::merge(&mut version, &remote.version);
    version
}

async fn expect_hello(
    control: &mut mpsc::Receiver<Message>,
) -> Result<seams_proto::Hello, SessionError> {
    loop {
        match control.recv().await {
            Some(Message::Hello(hello)) => return Ok(hello),
            Some(_) => continue,
            None => return Err(SessionError::Other("connection closed before hello".into())),
        }
    }
}

async fn expect_announce(
    control: &mut mpsc::Receiver<Message>,
) -> Result<Vec<seams_proto::FolderAnnounce>, SessionError> {
    loop {
        match control.recv().await {
            Some(Message::AnnounceFolders(folders)) => return Ok(folders),
            Some(_) => continue,
            None => return Err(SessionError::Other("connection closed before folders".into())),
        }
    }
}

/// Downloads one announced file block-by-block into a staged file and verifies the
/// whole-file hash. Returns the staged path for `finalize_incoming`.
async fn fetch_file(
    conn: &Connection,
    root: &Path,
    folder_id: &str,
    entry: &IndexEntry,
    blocks_rx: &mut mpsc::Receiver<Message>,
) -> Result<PathBuf, SessionError> {
    let total_blocks = block_count(entry.size);
    if total_blocks > 0 {
        send_message(
            conn,
            &Message::RequestBlocks {
                folder_id: folder_id.to_string(),
                rel_path: entry.rel_path.clone(),
                blocks: (0..total_blocks).collect(),
            },
        )
        .await?;
    }

    let (staged_path, mut staged) = stage_file(root, &entry.rel_path)?;

    let mut received = 0u32;
    while received < total_blocks {
        let msg = blocks_rx
            .recv()
            .await
            .ok_or_else(|| SessionError::Other("peer closed during transfer".into()))?;
        let Message::BlockData {
            folder_id: f,
            rel_path: p,
            block_index,
            data,
        } = msg
        else {
            continue;
        };
        if f != folder_id || p != entry.rel_path {
            continue;
        }
        if block_index == u32::MAX {
            let _ = std::fs::remove_file(&staged_path);
            return Err(SessionError::Other(format!(
                "file `{}` is missing on the peer",
                entry.rel_path
            )));
        }
        let offset = block_index as u64 * BLOCK_SIZE;
        staged.seek(SeekFrom::Start(offset))?;
        staged.write_all(&data)?;
        received += 1;
    }
    drop(staged);

    let actual = hash_file(&staged_path)?.whole;
    if actual != entry.file_hash {
        let _ = std::fs::remove_file(&staged_path);
        return Err(SessionError::Other(format!(
            "hash mismatch for `{}` after transfer",
            entry.rel_path
        )));
    }
    Ok(staged_path)
}

/// Serves the peer's block requests by streaming file slices back.
async fn serve_blocks(
    conn: Connection,
    msg: Message,
    folder_roots: &HashMap<String, PathBuf>,
) -> Result<(), SessionError> {
    let Message::RequestBlocks {
        folder_id,
        rel_path,
        blocks,
    } = msg
    else {
        return Ok(());
    };

    let send_missing = |conn: Connection| {
        let folder_id = folder_id.clone();
        let rel_path = rel_path.clone();
        async move {
            let _ = send_message(
                &conn,
                &Message::BlockData {
                    folder_id,
                    rel_path,
                    block_index: u32::MAX,
                    data: Vec::new(),
                },
            )
            .await;
        }
    };

    let Some(root) = folder_roots.get(&folder_id) else {
        send_missing(conn).await;
        return Ok(());
    };

    let mut file = match std::fs::File::open(root.join(&rel_path)) {
        Ok(file) => file,
        Err(_) => {
            send_missing(conn).await;
            return Ok(());
        }
    };

    for block in blocks {
        let offset = block as u64 * BLOCK_SIZE;
        file.seek(SeekFrom::Start(offset))?;
        let mut buf = vec![0u8; BLOCK_SIZE as usize];
        let mut filled = 0usize;
        while filled < buf.len() {
            let n = file.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            return Err(SessionError::Other(format!(
                "requested block {block} of `{rel_path}` is beyond end of file"
            )));
        }
        buf.truncate(filled);
        send_message(
            &conn,
            &Message::BlockData {
                folder_id: folder_id.clone(),
                rel_path: rel_path.clone(),
                block_index: block,
                data: buf,
            },
        )
        .await?;
    }
    Ok(())
}

/// Applies the per-policy conflict resolution and records the converged index entries.
/// Never silently loses a version (docs/CONFLICTS.md §2-§5).
#[allow(clippy::too_many_arguments)]
async fn apply_conflict(
    conn: &Connection,
    self_id: &DeviceId,
    peer_id: &DeviceId,
    config: &FolderConfig,
    snapshot: &FolderSnapshot,
    planned: &seams_sync::PlannedFile,
    blocks_rx: &mut mpsc::Receiver<Message>,
    final_index: &mut BTreeMap<String, IndexEntry>,
    counters: &mut FolderReport,
) -> Result<(), SessionError> {
    let local_mtime = snapshot.mtime_of(&planned.rel_path).unwrap_or(0);
    let local_size = snapshot.size_of(&planned.rel_path).unwrap_or(0);
    let merged = merged_version(&planned.local_version, &planned.remote);

    // Edit-vs-delete (§2.2): the surviving content keeps the original name on both
    // devices; there is no second content to store as a copy.
    if planned.remote.deleted || planned.local_hash.is_none() {
        if planned.local_hash.is_none() {
            if let Ok(staged) = fetch_file(conn, &config.path, &config.id, &planned.remote, blocks_rx).await {
                finalize_incoming(&config.path, &planned.rel_path, &staged, planned.remote.mtime_ns)?;
                counters.bytes_transferred += planned.remote.size;
            }
        }
        counters.conflicts += 1;
        let hash = planned
            .local_hash
            .unwrap_or(planned.remote.file_hash);
        let (size, mtime) = if planned.local_hash.is_some() {
            (local_size, local_mtime)
        } else {
            (planned.remote.size, planned.remote.mtime_ns)
        };
        final_index.insert(
            planned.rel_path.clone(),
            IndexEntry {
                rel_path: planned.rel_path.clone(),
                size,
                mtime_ns: mtime,
                file_hash: hash,
                version: merged,
                deleted: false,
            },
        );
        return Ok(());
    }

    // NewestWins: the newer mtime wins, the loser goes to the trash on its own device.
    // Filesystem mtimes have coarse granularity, so equal mtimes are common; the
    // smaller device ID breaks the tie, keeping the outcome deterministic everywhere.
    if config.conflict_policy == crate::config::ConflictPolicy::NewestWins {
        let remote_newer = planned.remote.mtime_ns > local_mtime
            || (planned.remote.mtime_ns == local_mtime && peer_id < self_id);
        if remote_newer {
            trash_file(&config.path, &planned.rel_path, chrono::Utc::now())?;
            let staged = fetch_file(conn, &config.path, &config.id, &planned.remote, blocks_rx).await?;
            finalize_incoming(&config.path, &planned.rel_path, &staged, planned.remote.mtime_ns)?;
            counters.trashed += 1;
            counters.fetched += 1;
            counters.bytes_transferred += planned.remote.size;
            final_index.insert(
                planned.rel_path.clone(),
                IndexEntry {
                    rel_path: planned.rel_path.clone(),
                    size: planned.remote.size,
                    mtime_ns: planned.remote.mtime_ns,
                    file_hash: planned.remote.file_hash,
                    version: merged,
                    deleted: false,
                },
            );
        } else {
            counters.announced += 1;
            let mut entry = snapshot_outgoing_entry(snapshot, &planned.rel_path)
                .expect("conflict on an existing local file");
            entry.version = merged;
            final_index.insert(planned.rel_path.clone(), entry);
        }
        return Ok(());
    }

    // Manual / KeepBoth: deterministic layout (§3). Both devices compute the same names
    // from the tie-break, so the folders converge.
    let layout = conflict_layout(
        self_id,
        peer_id,
        &planned.rel_path,
        local_mtime,
        planned.remote.mtime_ns,
    );
    let peer_staged =
        fetch_file(conn, &config.path, &config.id, &planned.remote, blocks_rx).await?;
    counters.conflicts += 1;
    counters.bytes_transferred += planned.remote.size;

    match conflict_role(self_id, peer_id) {
        ConflictRole::Primary => {
            // Our version keeps the original name; the peer's becomes the copy.
            finalize_incoming(
                &config.path,
                &layout.conflict_copy_name,
                &peer_staged,
                planned.remote.mtime_ns,
            )?;
            final_index.insert(
                layout.primary_name.clone(),
                IndexEntry {
                    rel_path: layout.primary_name.clone(),
                    size: local_size,
                    mtime_ns: local_mtime,
                    file_hash: planned.local_hash.expect("primary keeps its file"),
                    version: merged.clone(),
                    deleted: false,
                },
            );
            final_index.insert(
                layout.conflict_copy_name.clone(),
                IndexEntry {
                    rel_path: layout.conflict_copy_name.clone(),
                    size: planned.remote.size,
                    mtime_ns: planned.remote.mtime_ns,
                    file_hash: planned.remote.file_hash,
                    version: merged,
                    deleted: false,
                },
            );
        }
        ConflictRole::ConflictCopy => {
            // Our version becomes the copy; the peer's takes the original name.
            rename_to_conflict_copy(&config.path, &planned.rel_path, &layout.conflict_copy_name)?;
            finalize_incoming(
                &config.path,
                &planned.rel_path,
                &peer_staged,
                planned.remote.mtime_ns,
            )?;
            final_index.insert(
                layout.primary_name.clone(),
                IndexEntry {
                    rel_path: layout.primary_name.clone(),
                    size: planned.remote.size,
                    mtime_ns: planned.remote.mtime_ns,
                    file_hash: planned.remote.file_hash,
                    version: merged.clone(),
                    deleted: false,
                },
            );
            final_index.insert(
                layout.conflict_copy_name.clone(),
                IndexEntry {
                    rel_path: layout.conflict_copy_name.clone(),
                    size: local_size,
                    mtime_ns: local_mtime,
                    file_hash: planned.local_hash.expect("copy keeps our file"),
                    version: merged,
                    deleted: false,
                },
            );
        }
    }
    Ok(())
}
