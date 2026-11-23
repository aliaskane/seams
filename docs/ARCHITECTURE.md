# Seams Architecture

This document describes what Seams is, the decisions already made, how the system is
structured, and the draft of the wire protocol. The conflict-resolution specification lives
in [CONFLICTS.md](CONFLICTS.md).

## 1. Vision and non-goals

Seams is a local-network file sync app built to replace Syncthing for everyday people.
Three product goals drive every technical decision:

1. **Zero friction.** Open the app, see nearby devices *with their names*, tap to connect,
   sync starts. After a full phone reset the recovery flow is: install the APK → tap the
   device → folders are offered → files stream in. Target: under a minute, nothing to
   reconfigure on the other side.
2. **Conflicts are a feature, not an error.** Concurrent edits must be detected honestly
   and resolved in the UI, with no data ever lost silently.
3. **Sync anything.** Arbitrary folders via real filesystem paths: camera roll, music,
   Obsidian vaults. Modification times are preserved on the receiving side.

Non-goals (for the foreseeable future):

- Sync over the internet (NAT traversal, relays, hole punching). LAN only.
- Full version history / time-machine snapshots. A 14-day trash covers accidental deletions.
- Unix ownership/ACL sync on Android (emulated storage has no real permissions). POSIX
  permissions will be synced where they exist, desktop-to-desktop.

## 2. Decision log

Decisions that were discussed and settled, recorded so they do not get re-litigated:

| Topic | Decision |
| --- | --- |
| Name | seams.dev — "the seam stitches devices together" |
| Platform order | Android first; iOS/desktop later; the engine is cross-platform from day one |
| Engine language | Rust, thin platform UIs (Kotlin/Compose now, Swift/Tauri later), UniFFI bindings |
| UI design | Material 3, dynamic color, adaptive navigation |
| Storage access | "All files access" (`MANAGE_EXTERNAL_STORAGE`) + the system folder picker, whose tree URI is resolved back to a real path; SAF (`content://`) only as a fallback |
| Modification time | Always restored to the source value after writing on the receiving side |
| Sync triggers | File-watcher (inotify) on open folders + periodic rescan + network-change events — not a permanently busy service |
| Deletions | Receiving side moves the file into a 14-day trash before applying the deletion |
| Conflicts | Flagship feature; version vectors, dedicated UI, per-folder policies (see CONFLICTS.md) |
| Pairing | Primary flow: device list on the LAN with names; QR is an optional extra, not a requirement |
| Distribution | Direct APK first; F-Droid/Play decided later |
| Code language | English everywhere; comments only where the code cannot speak for itself |
| minSdk | 26 — drops legacy launcher bitmaps, covers ~98.5% of active devices |

## 3. Repository layout

```
android/                  Android app (the only UI for now)
  app/src/main/java/com/aliaskane/seams/
    MainActivity.kt       Single activity, adaptive navigation (NavigationSuiteScaffold)
    ui/devices/           Device list screen
    ui/folders/           Folder list screen
    ui/conflicts/         Conflict resolution screen
    ui/theme/             Material 3 theme (dynamic color)
crates/
  seams-proto/            Wire protocol: message types, encoding, version vectors
  seams-identity/         Device keypair, device ID derivation, TLS certificate
  seams-discovery/        mDNS advertisement and browsing
  seams-sync/             Scanner, hashing, folder index, conflict decisions, trash
  seams-transport/        QUIC endpoints with mutual certificate pinning
  seams-core/             Configuration, node facade, future UniFFI boundary
docs/                     This documentation
```

Dependency graph (arrows point at the dependency):

```
seams-core ──> seams-sync ──> seams-proto ──> seams-identity
     │             │                            ▲
     ├──> seams-transport ──────────────────────┤
     └──> seams-discovery ──────────────────────┘
```

## 4. Crates

### seams-identity

Every installation generates an Ed25519 keypair on first run. The **device ID** is the
blake3 fingerprint of the self-signed certificate (the same scheme Syncthing uses), a
32-byte value rendered as lowercase base32 (52 characters), with a 7-character short form
for the UI. The certificate is stored alongside the key and never regenerated silently, so
the ID is stable for the life of the identity.

For TLS the keypair is wrapped in a self-signed certificate (via `rcgen`) whose common
name and SAN carry the device ID string. Because the ID is the certificate fingerprint,
pinning a peer requires no X.509 parsing: hash the presented certificate and compare.

The identity persists in the app data directory (`identity.key` PKCS#8 DER,
`identity.cert` DER) and is loaded or created on startup.

### seams-proto

The wire protocol is a small set of serde types encoded with `postcard` (compact,
non-self-describing binary). Version vectors are plain `BTreeMap<DeviceId, u64>` values;
the crate provides the relation function (`Equal / Ahead / Behind / Concurrent`), counter
increment, and element-wise merge.

Draft message set:

| Message | Direction | Purpose |
| --- | --- | --- |
| `Hello` | both | Protocol version, device ID, display name |
| `AnnounceFolders` | both | Folders available for sharing |
| `Index` | both | Full or delta index for one folder (path, size, mtime, hashes, version vector, deleted flag) |
| `RequestBlocks` | both | Missing block indices for one file |
| `BlockData` | both | Raw block bytes |

Transport framing: **one message per QUIC unidirectional stream**. QUIC streams give us
framing for free — the receiver calls `read_to_end(limit)` and gets exactly one encoded
message. No length prefixes, no partial-write hazards, and independent messages are
already multiplexed without head-of-line blocking.

Protocol version: `Hello.protocol_version` must match exactly during phase 0-1; from
phase 2 onward it becomes `major.must-match, minor.compatible`.

### seams-discovery

mDNS/DNS-SD (UDP 5353, service type `_seams._udp.local.`). The instance name is the
device ID so multiple devices of the same name cannot collide. TXT records carry:

| Key | Value |
| --- | --- |
| `v` | discovery record format version |
| `id` | full device ID (base32) |
| `name` | human-readable device name |
| `pv` | protocol version |

Discovery quirks that must be handled (this is where Syncthing's UX pain lives):

- Android requires `WifiManager.MulticastLock` before multicast packets arrive.
- Guest Wi-Fi and "AP isolation" router settings block multicast entirely; the fallback
  path is UDP broadcast, then manual IP entry as the last resort.
- Stale entries (devices that left the network) are filtered by probing reachability
  (TCP connect to the advertised address) before the device is shown in the UI. A device
  is displayed only while its announcement is fresh.

### seams-sync

- **Scanner** walks a folder and produces `ScannedFile { rel_path, size, mtime_ns }`.
  Relative paths always use `/` separators regardless of platform, so the wire format and
  the index are identical everywhere. `.seams-trash/` is excluded from scanning.
- **Hashing** uses blake3: one whole-file hash plus fixed 128 KiB block hashes. Fixed
  blocks keep the MVP simple; content-defined chunking is a later optimization if
  deduplication across folders proves worth it.
- **Index** is a per-folder map of `rel_path → IndexEntry` (size, mtime, whole-file hash,
  block hashes, version vector, deleted flag). The trait `FolderIndex` has an in-memory
  implementation now; a SQLite (WAL) implementation replaces it when the Android shell
  lands, because the index must survive restarts on device.
- **Conflict decisions** are a pure 3-way function of (local file state, last-synced base
  entry, remote entry) — see CONFLICTS.md. Pure functions make the matrix exhaustively
  testable without any network.
- **Trash** lives at `<folder>/.seams-trash/<timestamp>/<original name>`; a sweep removes
  entries older than 14 days.

### seams-transport

QUIC via `quinn` + `rustls` (TLS 1.3, ring provider). Both sides present self-signed
Ed25519 certificates:

- The **initiator** pins the exact expected device ID (the blake3 fingerprint of the
  peer's certificate) and refuses anything else — a custom `ServerCertVerifier` compares
  the hash of the presented certificate.
- The **responder** currently accepts any valid Ed25519 certificate; the pairing record
  decides who is allowed (UI-level confirmation in phase 0-1, enforced transport allow-list
  in phase 2).

The transport exposes connect/accept with the peer's device ID attached, so upper layers
never touch TLS details.

### seams-core

Configuration and the node facade. `seams.json` holds the device name, folder list
(id, label, path, per-folder conflict policy, member device IDs) and peers (device ID,
name, last seen address). The crate is deliberately thin: it wires identity + config +
engine together and will host the UniFFI interface that Kotlin/Swift call.

## 5. Android integration plan

- **Bindings:** `seams-core` is exposed to Kotlin via UniFFI (proc-macro style). The Kotlin
  side gets `DeviceIdentity`, folder operations, and a `SyncEvent` stream; it never parses
  protocol types itself.
- **Service:** an event-driven foreground service (`dataSync` type). Triggers:
  `FileObserver` (inotify) on synced folders, `ConnectivityManager` network callbacks
  (Wi-Fi available → scan + sync), a periodic sweep for missed events. When idle for a
  while the service stops itself and is restarted by the next trigger — this keeps battery
  use low and stays inside Android 15's foreground-service limits.
- **Storage:** request `MANAGE_EXTERNAL_STORAGE`, open the system folder picker
  (`ACTION_OPEN_DOCUMENT_TREE`), and resolve the returned tree URI back to a real
  filesystem path on primary storage. Real paths make the engine fast and simple; SAF
  streams remain the fallback for exotic volumes.
- **Discovery:** acquire `MulticastLock` while scanning; keep the scan window short and
  event-driven.

## 6. Security model

- Identity: Ed25519 keypair generated locally, never leaves the device.
- Device ID: blake3 fingerprint of the device certificate, shown in short form (7 base32
  chars) in the UI.
- Transport: TLS 1.3 always, including on the LAN; certificates pinned after pairing.
- Pairing: trust is established in the app — the user taps a device and confirms the
  short ID on both sides (TOFU with in-app confirmation). QR verification is optional.
- No servers. The only network traffic is between the user's own devices.

## 7. Roadmap

- **Phase 0 — skeleton (done):** workspace layout, protocol types, identity, scanner, VV
  conflict logic, QUIC transport with pinning, mDNS discovery — all with unit tests. MD3
  UI shell with Devices / Folders / Conflicts screens.
- **Phase 1 — LAN loop (done):** one or more folders sync end-to-end between two devices:
  discovery → connect → index exchange → transfer → apply, with trash on deletions and
  deterministic conflict layouts. Integration tests drive two real nodes over loopback
  QUIC (`crates/seams-core/tests/sync_loop.rs`). The Android app is wired to the engine
  through UniFFI (`SeamsRuntime`), prebuilt `.so` binaries are committed for
  `arm64-v8a`/`x86_64` (rebuild: `android/BUILDING_CORE.md`).
- **Phase 2 — trust and polish:** pairing confirmation UI, SQLite index (the JSON index
  store is the current stopgap), conflict resolution UI with previews, background service
  (foreground service + watchers), auto-sync triggers.
- **Phase 3 — breadth:** selective sync, ignore files, text 3-way merge, rename/move
  detection, conflict-copy restore-from-trash.
- **Phase 4 — platforms:** desktop shell (Tauri), iOS shell.

## 8. Phase 1 implementation notes

Lessons and decisions from wiring the engine for real:

- **The converged index starts from the outgoing snapshot.** After a session, the local
  index must equal "our announced state, then overridden by action results". Building it
  from the pre-session base instead loses this device's version-vector counters, and the
  next session silently treats concurrent edits as sequential ones (a data-loss bug we
  hit in tests and fixed).
- **Session completion handshake.** Both sides run symmetric session code over one QUIC
  connection; whoever finishes first must wait for the peer's `SyncComplete` before
  dropping the connection, or the slower side dies mid-transfer.
- **Folder identity.** `folder_id = blake3(lowercased label)[..8]`, so two devices that
  add a folder with the same label match automatically — no folder-id exchange UI needed
  for the MVP.
- **Coarse mtime.** Linux stamps writes with a coarse clock (~4 ms granularity); equal
  mtimes are normal, so every "newest wins" comparison needs a deterministic tie-break.
- **UniFFI boundary.** `seams-core` exposes `SeamsNode` (create/start/stop, folder CRUD,
  `connectAndSync`, `syncKnownPeers`) and a `NodeEvents` callback; Kotlin sees suspend
  functions and a sealed `NodeEvent` class. `DeviceId` serializes as base32 so version
  vectors work as JSON map keys in the persisted index.

## 9. Open questions

- Rename detection: hash-based move detection could avoid re-transferring large renamed
  files; cost is index bookkeeping. Revisit in phase 3.
- Whether photo folders should default to `NewestWins` policy out of the box or always
  start as `Manual` (see CONFLICTS.md §5).
