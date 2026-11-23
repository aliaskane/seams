# Seams

Seams is a minimal, local-network file sync app — a friendlier replacement for Syncthing.
Devices find each other on the LAN, connect with one tap, and keep chosen folders in sync:
photos, music, Obsidian vaults — anything.

Core values:

- **Zero friction.** Install, tap a device in the list, done. After a phone reset: install, tap, files stream back in.
- **Real conflict handling.** Concurrent edits never silently lose data; conflicts surface in the UI with previews and clear actions.
- **Faithful files.** Modification times are preserved; deletions go to a 14-day trash instead of vanishing.
- **Privacy by design.** Everything stays on the LAN, encrypted end-to-end between paired devices. No cloud, no accounts.

## Status

Phase 0 — skeleton. The Rust workspace compiles and is unit-tested; the Android app shows the
Material 3 UI shell. Sync between devices is not wired yet (see roadmap below).

## Repository layout

```
android/            Android app (Kotlin, Jetpack Compose, Material 3)
crates/
  seams-proto       Wire protocol: messages, version vectors, encoding
  seams-identity    Device identity: keypair, device ID, TLS certificates
  seams-discovery   mDNS-based device discovery on the LAN
  seams-sync        Folder scanner, file index, hashing, conflict decisions, trash
  seams-transport   QUIC transport with mutual certificate pinning
  seams-core        Facade over the engine; future UniFFI boundary for mobile UIs
docs/
  ARCHITECTURE.md   System design, protocol draft, roadmap
  CONFLICTS.md      Conflict detection and resolution specification
```

## Development

Rust engine (stable toolchain):

```sh
cargo build
cargo test
```

Android app: open `android/` in Android Studio (minSdk 26, AGP 9.x, Kotlin 2.2, Compose BOM 2025.12).

## License

MIT OR Apache-2.0
