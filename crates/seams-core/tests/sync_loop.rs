//! End-to-end integration: two nodes connect over loopback QUIC and sync a real
//! folder in both directions, including deletion-to-trash and a conflict.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};

use seams_core::{NodeEvent, NodeEvents, SeamsNode};

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("seams-e2e-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Collects engine events; also implements the UniFFI callback interface. Clones share
/// the same underlying log.
#[derive(Default, Clone)]
struct EventLog {
    events: std::sync::Arc<Mutex<Vec<NodeEvent>>>,
}

impl EventLog {
    fn count_conflicts(&self) -> usize {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| matches!(e, NodeEvent::FolderSynced { conflicts: c, .. } if *c > 0))
            .count()
    }
}

impl NodeEvents for EventLog {
    fn on_event(&self, event: NodeEvent) {
        self.events.lock().unwrap().push(event);
    }
}

fn mtime_ns(path: &Path) -> u64 {
    path.metadata()
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn make_pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn two_nodes_sync_bidirectionally_and_converge() {
    let data_a = temp_dir("node-a");
    let data_b = temp_dir("node-b");
    let folder_a = temp_dir("folder-a");
    let folder_b = temp_dir("folder-b");

    let big = make_pattern(300 * 1024);
    fs::write(folder_a.join("notes.md"), "from alice").unwrap();
    fs::write(folder_a.join("big.bin"), &big).unwrap();
    fs::write(folder_b.join("photo.txt"), "from bob").unwrap();

    let node_a = SeamsNode::create(data_a.display().to_string(), "Alice".into()).unwrap();
    let node_b = SeamsNode::create(data_b.display().to_string(), "Bob".into()).unwrap();

    let folder_id = node_a.add_folder("Vault".into(), folder_a.display().to_string()).unwrap();
    assert_eq!(folder_id, node_b.add_folder("Vault".into(), folder_b.display().to_string()).unwrap());

    let log_a = EventLog::default();
    let log_b = EventLog::default();
    node_a.start(45101, Box::new(log_a.clone())).await.unwrap();
    node_b.start(45102, Box::new(log_b.clone())).await.unwrap();

    // Alice dials Bob; both sides must end up with identical file sets.
    let reports = node_a
        .connect_and_sync(node_b.device_id(), "127.0.0.1".into(), 45102, "Bob".into())
        .await
        .expect("sync session");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].folder_id, folder_id);
    assert_eq!(reports[0].fetched, 1, "alice fetched photo.txt");

    assert_eq!(fs::read(folder_a.join("photo.txt")).unwrap(), b"from bob");
    assert_eq!(fs::read(folder_b.join("notes.md")).unwrap(), b"from alice");
    assert_eq!(fs::read(folder_b.join("big.bin")).unwrap(), big);

    // Bob's own session pulled both of alice's files (visible in his event log).
    let bob_synced = log_b.events.lock().unwrap().iter().any(|e| matches!(
        e,
        NodeEvent::FolderSynced { fetched: 2, trashed: 0, conflicts: 0, .. }
    ));
    assert!(bob_synced, "bob should have fetched notes.md and big.bin");

    // The source modification time is restored on the receiving side.
    assert_eq!(
        mtime_ns(&folder_a.join("notes.md")),
        mtime_ns(&folder_b.join("notes.md")),
        "mtime must survive the transfer"
    );

    // A second round is a no-op: the folders are already converged.
    let reports = node_a
        .connect_and_sync(node_b.device_id(), "127.0.0.1".into(), 45102, "Bob".into())
        .await
        .unwrap();
    assert_eq!(reports[0].fetched, 0);
    assert_eq!(reports[0].announced, 0);
    eprintln!("R2 ALICE idx: {:?}", fs::read_dir(data_a.join("index")).map(|d| d.collect::<Vec<_>>()));
    eprintln!("R2 ALICE index content: {}", fs::read_to_string(data_a.join("index").join(format!("{folder_id}.json"))).unwrap_or_default());
    eprintln!("R2 BOB index content: {}", fs::read_to_string(data_b.join("index").join(format!("{folder_id}.json"))).unwrap_or_default());

    // Deletion on one side lands in the peer's trash, not in the void.
    fs::remove_file(folder_a.join("notes.md")).unwrap();
    match node_a
        .connect_and_sync(node_b.device_id(), "127.0.0.1".into(), 45102, "Bob".into())
        .await
    {
        Ok(reports) => eprintln!("R3 ALICE REPORTS: {reports:?}"),
        Err(e) => eprintln!("R3 ALICE ERR: {e}"),
    }
    for e in log_a.events.lock().unwrap().iter() {
        eprintln!("R3 ALICE EVT: {e:?}");
    }
    let bob_trashed = log_b.events.lock().unwrap().iter().any(|e| matches!(
        e,
        NodeEvent::FolderSynced { trashed: 1, .. }
    ));
    assert!(bob_trashed, "bob trashed the deleted file");
    assert!(!folder_a.join("notes.md").exists());
    assert!(!folder_b.join("notes.md").exists());
    let trash_dir = folder_b.join(".seams-trash");
    assert!(trash_dir.exists(), "trash directory created");
    let trashed: Vec<_> = fs::read_dir(&trash_dir).unwrap().collect();
    assert_eq!(trashed.len(), 1);
    let trashed_file = trash_dir
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("notes.md");
    assert_eq!(fs::read(trashed_file).unwrap(), b"from alice");

    node_a.stop();
    node_b.stop();
}

#[tokio::test]
async fn concurrent_edits_produce_the_same_conflict_layout_on_both_sides() {
    let data_a = temp_dir("conflict-a");
    let data_b = temp_dir("conflict-b");
    let folder_a = temp_dir("conflict-folder-a");
    let folder_b = temp_dir("conflict-folder-b");

    fs::write(folder_a.join("doc.md"), "version 1").unwrap();

    let node_a = SeamsNode::create(data_a.display().to_string(), "Alice".into()).unwrap();
    let node_b = SeamsNode::create(data_b.display().to_string(), "Bob".into()).unwrap();
    node_a.add_folder("Docs".into(), folder_a.display().to_string()).unwrap();
    node_b.add_folder("Docs".into(), folder_b.display().to_string()).unwrap();

    let log_a = EventLog::default();
    let log_b = EventLog::default();
    node_a.start(45201, Box::new(log_a.clone())).await.unwrap();
    node_b.start(45202, Box::new(log_b.clone())).await.unwrap();

    // Round 1: doc.md reaches Bob.
    node_a
        .connect_and_sync(node_b.device_id(), "127.0.0.1".into(), 45202, "Bob".into())
        .await
        .unwrap();
    assert_eq!(fs::read(folder_b.join("doc.md")).unwrap(), b"version 1");

    // Round 2: both sides edit concurrently, then sync.
    std::thread::sleep(Duration::from_millis(20));
    fs::write(folder_a.join("doc.md"), "alice edit").unwrap();
    fs::write(folder_b.join("doc.md"), "bob edit").unwrap();

    node_a
        .connect_and_sync(node_b.device_id(), "127.0.0.1".into(), 45202, "Bob".into())
        .await
        .unwrap();

    // Both sides must hold the same two files with the same contents: one keeps the
    // original name (deterministic tie-break), the other is a conflict copy.
    let mut files_a = file_set(&folder_a);
    let mut files_b = file_set(&folder_b);
    files_a.sort();
    files_b.sort();
    assert_eq!(files_a.len(), 2, "alice: original + conflict copy: {files_a:?}");
    assert_eq!(files_a, files_b, "layouts must converge");

    let original = files_a.iter().find(|p| !p.contains("conflict")).unwrap();
    let copy = files_a.iter().find(|p| p.contains("conflict")).unwrap();
    let contents_a: Vec<Vec<u8>> = [original, copy]
        .map(|p| fs::read(folder_a.join(p)).unwrap())
        .to_vec();
    let contents_b: Vec<Vec<u8>> = [original, copy]
        .map(|p| fs::read(folder_b.join(p)).unwrap())
        .to_vec();
    assert!(contents_a.contains(&b"alice edit".to_vec()));
    assert!(contents_a.contains(&b"bob edit".to_vec()));
    assert_eq!(contents_a, contents_b);

    assert!(log_a.count_conflicts() >= 1);
    assert!(log_b.count_conflicts() >= 1);

    node_a.stop();
    node_b.stop();
}

#[tokio::test]
async fn newest_wins_policy_replaces_older_version_without_data_loss() {
    let data_a = temp_dir("newest-a");
    let data_b = temp_dir("newest-b");
    let folder_a = temp_dir("newest-folder-a");
    let folder_b = temp_dir("newest-folder-b");

    fs::write(folder_a.join("state.txt"), "v1").unwrap();

    let node_a = SeamsNode::create(data_a.display().to_string(), "Alice".into()).unwrap();
    let node_b = SeamsNode::create(data_b.display().to_string(), "Bob".into()).unwrap();
    let folder_id = node_a.add_folder("State".into(), folder_a.display().to_string()).unwrap();
    node_b.add_folder("State".into(), folder_b.display().to_string()).unwrap();
    // The policy is a folder property and must match on both devices.
    node_a.set_conflict_policy(folder_id.clone(), "newest_wins".into()).unwrap();
    node_b.set_conflict_policy(folder_id.clone(), "newest_wins".into()).unwrap();

    let log_a = EventLog::default();
    node_a.start(45301, Box::new(log_a.clone())).await.unwrap();
    node_b.start(45302, Box::new(EventLog::default())).await.unwrap();

    node_a
        .connect_and_sync(node_b.device_id(), "127.0.0.1".into(), 45302, "Bob".into())
        .await
        .unwrap();

    // Both edit. A pause between the writes beats the kernel's coarse mtime
    // granularity, so Bob's copy is strictly newer.
    std::thread::sleep(Duration::from_millis(30));
    fs::write(folder_a.join("state.txt"), "alice older").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    fs::write(folder_b.join("state.txt"), "bob newer").unwrap();

    let reports = node_a
        .connect_and_sync(node_b.device_id(), "127.0.0.1".into(), 45302, "Bob".into())
        .await
        .unwrap();
    eprintln!("NW R2 REPORTS: {reports:?}");
    for e in log_a.events.lock().unwrap().iter() {
        eprintln!("NW A EVT: {e:?}");
    }

    assert_eq!(
        fs::read(folder_a.join("state.txt")).unwrap(),
        b"bob newer",
        "newest wins everywhere"
    );
    assert_eq!(
        fs::read(folder_b.join("state.txt")).unwrap(),
        b"bob newer"
    );
    // Alice's overwritten version is preserved in her trash.
    assert!(folder_a.join(".seams-trash").exists());
    let trashed_content = trash_contents(&folder_a.join(".seams-trash"));
    assert!(trashed_content.contains(&b"alice older".to_vec()), "loser goes to trash");

    node_a.stop();
    node_b.stop();
}

fn file_set(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().unwrap().to_string_lossy().starts_with(".seams-") {
                    continue;
                }
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out
}

fn trash_contents(trash: &Path) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut stack = vec![trash.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(fs::read(&path).unwrap());
            }
        }
    }
    out
}
