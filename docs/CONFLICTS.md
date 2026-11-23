# Seams Conflict Specification

Conflict handling is Seams' flagship feature and the main reason people leave Syncthing.
This document specifies how conflicts are detected, represented, resolved, and surfaced in
the UI. The engine parts live in `seams-sync` (`conflict.rs`, `vv.rs`, `trash.rs`); the
rules here are normative for any implementation.

## 1. Principles

1. **Never lose data silently.** A conflicting change is never overwritten; it is kept as
   a separate file until a human (or a per-folder policy) decides.
2. **Deterministic.** All devices that observe the same history must compute exactly the
   same outcome — same winner, same names, same trash layout. No per-device randomness.
3. **Visible, not buried.** Conflicts appear in a dedicated UI surface with context (who
   changed what, when) and previews, not as cryptic filenames in the folder.
4. **Cheap in the common case.** Sequential edits (the 99% case) never produce conflicts;
   the common case must cost nothing beyond normal sync.

## 2. Detection

### 2.1 Version vectors

Every file carries a **version vector (VV)**: a map `DeviceId → counter`. A device
increments its own counter whenever it changes a file (content or metadata). VVs are
compared with the standard partial-order relation:

| Relation | Meaning |
| --- | --- |
| `A == B` | same counters everywhere — states are identical |
| `A > B` | A saw everything B did, plus own changes — A strictly newer |
| `A < B` | mirror image — B strictly newer |
| `A ⋈ B` | concurrent — neither dominates; both did work the other never saw |

Only **concurrency** creates a conflict. Sequential edits (A edited after receiving B's
change, so `A > B`) merge without ceremony.

### 2.2 The three-way decision

The decision is made per file from three inputs:

- **base** — the last-synced `IndexEntry` for this path in the local folder index
  (hash + VV), or *absent* if the file did not exist when the folder was last in sync;
- **local** — the current on-disk state of the file (recomputed hash when mtime/size hint
  at a change), or *absent* if deleted/moved away;
- **remote** — the incoming `IndexEntry` announced by the peer.

Define `changed(state, base)` as "state's content hash differs from base's hash"
(absence counts as a change when the base exists, and vice versa). The decision matrix:

| local changed | remote changed | relation(local VV, remote VV) | Outcome |
| --- | --- | --- | --- |
| no | no | any | `UpToDate` — nothing to do |
| no | yes | any | `ApplyRemote` (peer is strictly newer or we deleted) |
| yes | no | any | `KeepLocal` (announce our version back) |
| yes | yes | same hash | `UpToDate` — both made the identical change; merge VVs |
| yes | yes | concurrent, different hash | `Conflict` |
| yes | yes | local strictly ahead | `KeepLocal` |
| yes | yes | remote strictly ahead | `ApplyRemote` |

Edge cases that fall out of the matrix rather than being special-cased:

- **Edit vs delete** where the editor never saw the deletion: the deletion is rejected as
  a conflict — the surviving content keeps the original name on every device. Phase 1
  stores no second copy (the deleted content is gone from the deleting device); restoring
  the loser from the local trash into a conflict copy is planned work.
- **Delete vs delete**: both sides deleted — `UpToDate`, file stays deleted.
- **Create vs create** with different content: concurrent → conflict, exactly like two
  edits.
- **Metadata-only changes** (mtime moved, content identical): never a conflict; newest
  mtime wins, VVs merge.

## 3. Representation on disk

When a conflict is recorded, both copies are written **on both devices** with the same
deterministic layout:

- One side keeps the **original name** — this is the "primary" copy.
- The other side becomes a conflict copy:
  `name (conflict from <short-id>, YYYY-MM-DD).ext`
  e.g. `thesis (conflict from ABCDEFG, 2026-11-23).docx`
- **Tie-break rule for who keeps the original name:** the device whose edit is retained
  under the original name is chosen by comparing device IDs (the smaller base32 device ID
  wins the original name). Every device can compute this locally from the same inputs, so
  both sides converge without another protocol round-trip.
- The loser's version is a full copy of the file, with its own mtime preserved.
- Both version vectors are merged in the index after the conflict is recorded, so the
  conflict resolution itself does not re-trigger.

Conflict copies are **never scanned back as new files**: the scanner and the index treat
the ` (conflict from ..., date)` suffix as conflict residue, and resolution simply
deletes or renames the copies.

## 4. Trash

Deleted-and-confirmed files go to `<folder>/.seams-trash/<timestamp>/<name>` before the
deletion is applied; the trash retention is **14 days**, after which a sweep removes
expired entries (the timestamp directory name is the record; a scan of the trash is cheap
because it is one level deep).

The trash serves two masters:

- accidental deletions by the user on the *other* device — restorable from the UI;
- the edit-vs-delete conflict in §2.2 — the "deleted" content is restored from the trash
  to form the conflict copy.

Trash directories are excluded from scanning and from the index.

## 5. Per-folder policies

Not every folder deserves manual attention. Each folder has a policy:

| Policy | Behavior | Intended for |
| --- | --- | --- |
| `Manual` (default) | Conflicts wait in the UI until the user resolves them | Documents, Obsidian vaults, anything the user authors |
| `KeepBoth` | Like Manual, but resolution is pre-applied: primary + conflict copy stay side by side indefinitely | Photo-like folders where even conflicts are just… more photos |
| `NewestWins` | The strictly newer content replaces the other; the replaced version goes to the trash (not deleted outright). Equal mtimes (common — the kernel stamps writes with ~4 ms granularity) are broken by the smaller device ID, so every device computes the same winner | Camera roll, app exports — append-mostly, immutable files |

`NewestWins` is only *advisable* for append-mostly folders; the UI steers users there
without forbidding anything. Policies are recorded in `seams.json` per folder and are
changeable at any time; changing a policy never rewrites history, it only affects future
conflicts. A policy is a property of the folder and must be set to the same value on
every device sharing the folder — with different policies two devices would resolve the
same conflict differently.

## 6. Resolution UI

The Conflicts screen lists every unresolved conflict grouped by folder. Each entry shows:

- both versions with type-appropriate previews: image thumbnails, text diff (later:
  merge editor), audio/video players, generic icon otherwise;
- provenance for each version: device name, edit time, file size;
- actions: **Keep mine**, **Keep theirs**, **Keep both**, and (phase 3, text only)
  **Merge**.

Resolution actions are themselves sync operations: the chosen outcome propagates to the
other devices as normal index updates (a rename/delete sequence under the hood), so
everyone converges to the same state.

## 7. Text merge (phase 3)

For text files the index already stores the base version's hash — and phase 3 adds
retrieving the base content itself from the peer that still has it. With (base, mine,
theirs) a standard three-way merge runs:

- non-overlapping edits apply automatically (the pleasant 90% of real markdown conflicts);
- overlapping hunks produce conflict markers in an in-app merge editor, never in the
  synced file itself; the file only updates when the user finishes the merge;
- binary files never merge — they use the copies flow of §3.

## 8. Guarantees and tests

The decision function is pure, so the matrix in §2.2 is covered exhaustively by unit
tests (`crates/seams-sync/tests` / inline `#[cfg(test)]`):

- every row of the matrix, including both orderings of the relation;
- name generation and the deterministic tie-break (same inputs → same winner on both sides);
- trash naming and 14-day purge behavior;
- scanner exclusion of `.seams-trash/` and conflict-residue names;
- VV relation/increment/merge properties (antisymmetry, monotonicity).

Any change to conflict behavior must extend these tests in the same commit.
