# File Clipboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add safe, bounded, bidirectional file copy over the display clipboard, enabled while the viewer is focused.

**Architecture:** A negotiated `FILE_CLIPBOARD` extension adds an independent streaming state machine to the existing clipboard channel. The host parses human-readable policy from `settings.toml`, passes byte/second values to the viewer, and announces them to the guest; platform adapters enumerate source trees and materialise validated relative entries under private staging roots before publishing `CF_HDROP` or local file URIs.

**Tech Stack:** Rust; prost/protobuf; Win32 through `windows`; Mutter RemoteDesktop through `zbus` 5; pure-Rust portable state machines and path validation.

**Spec:** `docs/superpowers/specs/2026-08-26-file-clipboard-design.md`

## Global Constraints

- All application code is Rust; Windows integration uses native APIs, never PowerShell, WMI or external processes.
- Guest binaries remain statically linked for `x86_64-unknown-linux-musl`; add no dependency on a system C library.
- `unsafe` stays inside platform-specific modules.
- Never reuse or renumber protobuf fields, enum values or record types; every enum retains a zero unspecified value.
- File clipboard requires both `CAPABILITY_CLIPBOARD` and `CAPABILITY_FILE_CLIPBOARD`; old peers exchange existing formats unchanged.
- Defaults are exactly `1GB` per file, `4GB` per transfer and `24h` retention. `KB/MB/GB` are binary multiples; accepted duration units are `s/m/h`.
- Protocol constants are 4096 entries, depth 64, path 1024 UTF-8 bytes, chunk 60 KiB, one transfer per direction and five seconds without protocol progress.
- Only regular files and directories cross. Any link/reparse point or special entry cancels the complete file transfer.
- Never log content, file names, full paths or URIs. Allowed metadata is direction, transfer ID, counts, aggregate bytes, limit, reason and outcome.
- User-visible settings UI is out of scope.
- Use `tracing`, not `log`; user-visible events use `vmlord_core::diagnostic!`.
- Commit subjects use `TASK-139: <comment>`. Do not push or open a merge request without explicit approval.
- Use project aliases: `cargo check-windows`, `cargo test-windows`, `cargo display-services`; use `cargo test -p ...` for portable crates.

---

## File Structure

| file | responsibility |
| --- | --- |
| `crates/core/src/settings.rs` | human-readable sizes/duration and `FileClipboardSettings` defaults/validation |
| `crates/core/src/lib.rs` | re-export file clipboard settings and value types |
| `crates/display-viewer/proto/vmlord/display/viewer/viewer.proto` | parsed file policy in viewer launch parameters |
| `crates/display-viewer/src/launch.rs` | typed launch policy and launch-contract revision |
| `crates/platform/src/display_launches.rs` | pass application policy into each viewer |
| `crates/platform/src/repository.rs` | obtain current settings for `LaunchRequest` |
| `crates/display-protocol/proto/vmlord/display/v1/display.proto` | capability, policy and file-transfer records |
| `crates/display-protocol/src/clipboard/files.rs` | portable file state machine, limits, messages and operations |
| `crates/display-protocol/src/clipboard/path.rs` | portable relative-path validation and Windows collision key |
| `crates/display-protocol/src/clipboard.rs` | expose `files` and retain the existing in-memory state machine unchanged |
| `crates/display-viewer/src/clipboard/files.rs` | Windows staging, cleanup, `CF_HDROP` enumeration/building |
| `crates/display-viewer/src/windows/clipboard.rs` | integrate file operations into the focused clipboard pump |
| `crates/display-services/src/clipboard_files.rs` | Linux source walking, staging and URI parsing/generation |
| `crates/display-services/src/clipboard_main.rs` | integrate file operations with Mutter and the clipboard channel |
| `crates/display-services/src/mutter.rs` | expose local URI-list reads and writes without buffering file bodies |
| `crates/display-protocol/tests/*` | descriptor, golden, malformed and compatibility coverage |
| `crates/display-payload/src/protocol.rs` | declare payload coverage for the new protocol minor |
| `payloads/display/*/payload.spec.json` | release a new display payload version for all supported Ubuntu releases |
| `ARCHITECTURE.md` | final protocol, policy, safety and lifecycle decisions |

---

### Task 1: Human-readable file clipboard settings

**Files:**
- Modify: `crates/core/src/settings.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: every in-repository `AppSettings` literal reported by `rg -n 'AppSettings \\{' crates`

**Interfaces:**
- Produces: `DataSize::bytes() -> u64`, `Retention::seconds() -> u64`, `FileClipboardSettings { max_file_size, max_transfer_size, retention }`, `FileClipboardSettings::validate() -> Result<(), FileClipboardSettingsError>`.
- Defaults: `DataSize(1 << 30)`, `DataSize(4 << 30)`, `Retention(86_400)`.

- [ ] **Step 1: Add failing serde and validation tests**

```rust
#[test]
fn file_clipboard_settings_use_human_units_and_binary_multipliers() {
    let settings: FileClipboardSettings = toml::from_str(
        "max_file_size = \"1GB\"\nmax_transfer_size = \"4096MB\"\nretention = \"24h\"",
    ).unwrap();
    assert_eq!(settings.max_file_size.bytes(), 1 << 30);
    assert_eq!(settings.max_transfer_size.bytes(), 4 << 30);
    assert_eq!(settings.retention.seconds(), 86_400);
    assert!(settings.validate().is_ok());
    assert!(toml::to_string(&settings).unwrap().contains("max_file_size = \"1GB\""));
}

#[test]
fn invalid_file_clipboard_values_are_refused() {
    for value in ["0GB", "1GiB", "1.5GB", "1 GB", "1d", "18446744073709551615GB"] {
        let document = format!(
            "max_file_size = \"{value}\"\nmax_transfer_size = \"4GB\"\nretention = \"24h\""
        );
        assert!(toml::from_str::<FileClipboardSettings>(&document).is_err());
    }
    let invalid = FileClipboardSettings {
        max_file_size: DataSize::from_bytes(5 << 30),
        max_transfer_size: DataSize::from_bytes(4 << 30),
        ..Default::default()
    };
    assert!(invalid.validate().is_err());
}
```

- [ ] **Step 2: Run `rtk cargo test -p vmlord-core settings` and verify compilation fails because the types do not exist.**

- [ ] **Step 3: Implement strict string serde** using one shared `parse_scaled(text, units)` helper with `checked_mul`, case-insensitive suffix matching ordered longest-first, positive integer digits only, and shortest-exact-unit serialisation. Add `#[serde(default)] pub clipboard_files: FileClipboardSettings` as the final table field in `AppSettings`; call `validate()` after TOML deserialisation and before save, mapping failure to a new `SettingsError::Validation(FileClipboardSettingsError)` whose display text names the invalid field.

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataSize(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileClipboardSettings {
    pub max_file_size: DataSize,
    pub max_transfer_size: DataSize,
    pub retention: Retention,
}
```

- [ ] **Step 4: Update all `AppSettings` literals with `clipboard_files: Default::default()` and run `rtk cargo test -p vmlord-core` plus `rtk cargo test -p vmlord-app`.** Expected: PASS.

- [ ] **Step 5: Commit:** `TASK-139: Add file clipboard settings`.

---

### Task 2: Carry the parsed policy into the viewer

**Files:**
- Modify: `crates/display-viewer/proto/vmlord/display/viewer/viewer.proto`
- Modify: `crates/display-viewer/src/launch.rs`
- Modify: `crates/platform/src/display_launches.rs`
- Modify: `crates/platform/src/repository.rs`

**Interfaces:**
- Consumes: `FileClipboardSettings` from Task 1.
- Produces: `launch::FilePolicy { max_file_bytes: u64, max_transfer_bytes: u64, retention_seconds: u64 }`; `LaunchParameters.file_policy`; `LaunchRequest.file_policy`.

- [ ] **Step 1: Add a failing launch round-trip test** with `FilePolicy { 1 << 30, 4 << 30, 86_400 }` and assert all three `u64` fields survive encode/decode.
- [ ] **Step 2: Run `rtk cargo test -p vmlord-display-viewer launch` and verify the missing field failure.**
- [ ] **Step 3: Append protobuf fields 12–14 to `LaunchParameters`, increment `launch::REVISION` from 1 to 2, and map them to the typed `FilePolicy`.** Reject zero values and `max_file_bytes > max_transfer_bytes` in `decode` with a named `LaunchError::Policy`.
- [ ] **Step 4: Add `file_policy` to `LaunchRequest`; populate it from the repository's current `AppSettings`, not from globals or a second settings read.** Convert the validated wrappers to byte/second values before spawning.
- [ ] **Step 5: Run `rtk cargo test -p vmlord-display-viewer` and the relevant platform unit tests via `rtk cargo test-windows`.** Expected: PASS.
- [ ] **Step 6: Commit:** `TASK-139: Pass file policy to the viewer`.

---

### Task 3: Extend and negotiate the wire contract

**Files:**
- Modify: `crates/display-protocol/proto/vmlord/display/v1/display.proto`
- Modify: `crates/display-protocol/src/handshake.rs`
- Modify: `crates/platform/src/display_session.rs`
- Modify: `crates/display-services/src/control.rs`
- Modify: `crates/display-protocol/tests/descriptor.rs`
- Modify: `crates/display-protocol/tests/compatibility.rs`
- Modify: `crates/display-protocol/tests/golden.rs`
- Modify: `crates/display-protocol/tests/malformed.rs`
- Modify: `crates/display-protocol/tests/fuzz.rs`

**Interfaces:**
- Produces: protocol v1.3; `Capability::FileClipboard = 4`; `ClipboardRecord::{FilePolicy=9, FileOffer=10, FileRequest=11, FileEntry=12, FileChunk=13, FileComplete=14, FileCancel=15}`; `FileEntryKind`; `FileCancelReason`; protobuf messages matching `clipboard::files::Message` in Task 4.

- [ ] **Step 1: Add failing compatibility tests** asserting v1.2 negotiation excludes `FileClipboard`, v1.3 peers may settle it only alongside `Clipboard`, and an old peer never receives record types 9–15.
- [ ] **Step 2: Run `rtk cargo test -p vmlord-display-protocol compatibility` and verify the missing capability/version failure.**
- [ ] **Step 3: Append the capability, enums and messages.** Use these schemas:

```protobuf
message ClipboardFilePolicy { uint64 max_file_bytes = 1; uint64 max_transfer_bytes = 2; uint64 retention_seconds = 3; }
message ClipboardFileOffer { uint32 serial = 1; }
message ClipboardFileRequest { uint32 serial = 1; uint32 transfer = 2; }
message ClipboardFileEntry { uint32 transfer = 1; string path = 2; FileEntryKind kind = 3; uint64 size = 4; }
message ClipboardFileChunk { uint32 transfer = 1; bytes chunk = 2; }
message ClipboardFileComplete { uint32 transfer = 1; }
message ClipboardFileCancel { uint32 transfer = 1; FileCancelReason reason = 2; }
```

`FileCancelReason` includes unspecified, superseded, too-large, focus-lost, unavailable, timed-out, unsafe-entry, invalid-path, I/O-failed and policy-rejected.

- [ ] **Step 4: Advertise `FileClipboard` from both current host and guest support lists and enforce the dependency during capability settlement.** A peer offering only `FileClipboard` has it removed, not promoted to ordinary clipboard.
- [ ] **Step 5: Regenerate `proto/display.descriptor.bin` and both golden fixture files using the commands emitted by their failing tests; add malformed sizes, invalid enums and unknown records to fuzz/malformed coverage.**
- [ ] **Step 6: Run `rtk cargo test -p vmlord-display-protocol`.** Expected: PASS.
- [ ] **Step 7: Commit:** `TASK-139: Add file clipboard wire records`.

---

### Task 4: Portable path policy and file-transfer state machine

**Files:**
- Create: `crates/display-protocol/src/clipboard/path.rs`
- Create: `crates/display-protocol/src/clipboard/files.rs`
- Modify: `crates/display-protocol/src/clipboard.rs`

**Interfaces:**
- Produces: `ValidatedPath::parse(&str) -> Result<Self, PathError>`, `ValidatedPath::components()`, `ValidatedPath::windows_key()`; `Policy::new(max_file_bytes, max_transfer_bytes, retention_seconds)`; `Exchange::{local_offer, peer_offer, peer_request, produced_entry, produced_chunk, produced_complete, peer_entry, peer_chunk, peer_complete, peer_cancel, focus_lost, tick}`.
- Produces operations: `Op::{Send(Message), Enumerate{transfer}, CreateEntry{transfer,path,kind,size}, WriteChunk{transfer,bytes}, Commit{transfer}, Abort{transfer}}`.

- [ ] **Step 1: Write table-driven failing path tests** for `../x`, `/x`, `C:/x`, empty components, `.`, NUL, colon, trailing dot/space, reserved names with extensions, more than 1024 bytes, and case-insensitive collisions; include accepted Unicode and nested paths.
- [ ] **Step 2: Run the path tests and verify the module is missing.**
- [ ] **Step 3: Implement lexical wire-path validation** without filesystem access. Preserve original UTF-8 for creation, compute a lowercase Windows comparison key per component, and expose no unchecked constructor.
- [ ] **Step 4: Write failing state-machine tests** for pull semantics, policy-before-offer, entry/chunk order, per-file and aggregate limits on both sender and receiver, 4097 entries, depth 65, independent incoming/outgoing IDs, supersession, focus loss, timeout, late chunks and commit only after complete.

```rust
let mut receiver = Exchange::new(policy(), now);
assert_eq!(receiver.peer_offer(7, now), vec![Op::Send(Message::Request { serial: 7, transfer: 1 })]);
assert!(matches!(receiver.peer_entry(1, "safe/a.txt", EntryKind::File, 3, now)[0], Op::CreateEntry { .. }));
assert!(matches!(receiver.peer_chunk(1, b"abc", now)[0], Op::WriteChunk { .. }));
assert_eq!(receiver.peer_complete(1, now), vec![Op::Commit { transfer: 1 }]);
```

- [ ] **Step 5: Implement the minimal state machine.** It tracks declared/current/aggregate bytes and collision keys, emits at most one `FileChunk` message per `produced_chunk` call, and never owns filesystem handles or source paths.
- [ ] **Step 6: Run `rtk cargo test -p vmlord-display-protocol clipboard::files` and then the complete crate suite.** Expected: PASS.
- [ ] **Step 7: Commit:** `TASK-139: Add the file transfer state machine`.

---

### Task 5: Linux URI, source-tree and staging adapters

**Files:**
- Create: `crates/display-services/src/clipboard_files.rs`
- Modify: `crates/display-services/src/lib.rs`
- Modify: `crates/display-services/src/mutter.rs`

**Interfaces:**
- Consumes: `ValidatedPath`, `Policy`, `EntryKind`.
- Produces: `parse_uri_list(&[u8]) -> Result<Vec<PathBuf>, UriError>`; `uri_lists(&[PathBuf]) -> UriPayloads`; `SourceTree::open(paths, policy)` and incremental `next()`; `Staging::{create, create_entry, write_chunk, commit, abort}`.

- [ ] **Step 1: Add failing URI tests** accepting percent-encoded local `file:///home/u/a%20b`, ignoring `#` comments and GNOME's `copy` header, and rejecting remote authorities, malformed escapes, NUL and non-file schemes.
- [ ] **Step 2: Add filesystem tests under a temporary private directory** for regular files/directories, symlink and FIFO rejection, depth/count/size enforcement, duplicate destinations, traversal, partial abort and successful top-level URI generation.
- [ ] **Step 3: Run `rtk cargo test -p vmlord-display-services clipboard_files` and verify the module is missing.**
- [ ] **Step 4: Implement descriptor-relative Linux traversal and staging.** Require `XDG_RUNTIME_DIR`; create `vmlord/clipboard/<session>/<transfer>` mode 0700; use no `/tmp` fallback; use `symlink_metadata`/no-follow descriptor operations and create-new semantics; recursively delete without following links.
- [ ] **Step 5: Extend Mutter with raw `read_mime(mime, cap)` and `write_mime(serial, bytes)` edges, retaining the current typed wrappers for in-memory formats.** File bodies never pass through D-Bus—only URI payloads do.
- [ ] **Step 6: Run `rtk cargo test -p vmlord-display-services` and `rtk cargo display-services`.** Expected: PASS and a static musl build.
- [ ] **Step 7: Commit:** `TASK-139: Add safe guest file staging`.

---

### Task 6: Windows `CF_HDROP`, source-tree and staging adapters

**Files:**
- Create: `crates/display-viewer/src/clipboard/files.rs`
- Modify: `crates/display-viewer/src/clipboard/mod.rs`
- Modify: `crates/display-viewer/Cargo.toml` only if an already-enabled Windows feature is insufficient

**Interfaces:**
- Produces: `hdrop_paths(HDROP) -> Result<Vec<PathBuf>, FileError>`; `SourceTree::{open,next}`; `Staging::{create,create_entry,write_chunk,commit,abort}`; `hdrop_for(&[PathBuf]) -> Result<OwnedGlobal, FileError>`; `cleanup(root, now, retention)`.

- [ ] **Step 1: Add portable unit tests** for Windows name policy and `DROPFILES` wide-string layout, including two top-level paths and the double NUL terminator.
- [ ] **Step 2: Add Windows-only tests** that create a directory tree, reject a junction/reparse point, enforce sizes, abort a partial tree, preserve a completed tree and remove completed/incomplete trees according to retention.
- [ ] **Step 3: Run the focused tests through `rtk cargo test-windows` and verify the missing adapter failure.**
- [ ] **Step 4: Implement the Win32 edge.** Enumerate `CF_HDROP` with `DragQueryFileW`; open filesystem objects with `CreateFileW` and `FILE_FLAG_OPEN_REPARSE_POINT`; inspect opened handles with `GetFileInformationByHandleEx`; reject reparse attributes; create destinations new; build a wide `DROPFILES` block. Wrap each raw handle/global allocation in an owning Rust type and keep every unsafe call in this module.
- [ ] **Step 5: Implement startup cleanup** under `%LOCALAPPDATA%\\VMLord\\Clipboard`, always deleting incomplete markers and deleting committed trees whose recorded completion time is older than `retention_seconds`; never traverse reparse points during cleanup.
- [ ] **Step 6: Run `rtk cargo test-windows`.** Expected: PASS.
- [ ] **Step 7: Commit:** `TASK-139: Add safe Windows file staging`.

---

### Task 7: Integrate file streaming into both clipboard pumps

**Files:**
- Modify: `crates/display-viewer/src/windows/clipboard.rs`
- Modify: `crates/display-services/src/clipboard_main.rs`
- Modify: `crates/display-services/src/mutter.rs`

**Interfaces:**
- Consumes: state machine and adapters from Tasks 4–6; launch `FilePolicy` from Task 2; wire records from Task 3.
- Produces: symmetric `record_of_file`/`handle_file` mappings and bounded pump scheduling.

- [ ] **Step 1: Add failing record-mapping tests** on both sides for every file message and assert protobuf payloads round-trip to the same portable message.
- [ ] **Step 2: Add pump tests with scripted adapters** proving: no file request while unfocused; focus loss emits file cancel and aborts staging; a file chunk is interleaved with an ordinary clipboard record; disconnect aborts partial staging; a completed transfer publishes only top-level paths; a peer lacking capability sees no file records.
- [ ] **Step 3: Run `rtk cargo test -p vmlord-display-services clipboard_main` and `rtk cargo test-windows`; verify failures identify missing file operations.**
- [ ] **Step 4: Integrate the guest pump.** Detect URI MIME types as a file offer without adding them to `Kind`; fetch URI bytes only after `Enumerate`; stream one source chunk per loop; on `Commit`, own both URI MIME types; on any exit, call `Abort` for partial staging.
- [ ] **Step 5: Integrate the Windows pump.** Give `CF_HDROP` priority when available; announce file and ordinary offers under the same clipboard serial; send `FilePolicy` after a file-capable channel binds; materialise guest files before calling `SetClipboardData(CF_HDROP)`; retain the committed staging owner separately from `Vec<Piece>`.
- [ ] **Step 6: Preserve scheduling:** cap file work per pump iteration to one 60-KiB chunk, drain focus/control/ordinary clipboard events before the next file chunk, and apply the five-second progress timeout in the portable state machine.
- [ ] **Step 7: Add captured-log assertions** using sentinel file name/content and verify neither appears at any level.
- [ ] **Step 8: Run `rtk cargo test -p vmlord-display-protocol`, `rtk cargo test -p vmlord-display-services`, and `rtk cargo test-windows`.** Expected: PASS.
- [ ] **Step 9: Commit:** `TASK-139: Stream files through the clipboard channel`.

---

### Task 8: Payload compatibility and release metadata

**Files:**
- Modify: `crates/display-payload/src/protocol.rs`
- Modify: `payloads/display/ubuntu-22.04-amd64/payload.spec.json`
- Modify: `payloads/display/ubuntu-24.04-amd64/payload.spec.json`
- Modify: `payloads/display/ubuntu-26.04-amd64/payload.spec.json`
- Modify: payload/catalog fixtures selected by failures from `rtk cargo test -p vmlord-display-payload`

**Interfaces:**
- Consumes: protocol v1.3 support from Task 3 and rebuilt guest binary from Tasks 5/7.
- Produces: display payload version `0.1.6` whose protocol interval includes v1.3.

- [ ] **Step 1: Add a failing protocol-coverage test** asserting the current display payload covers `(1, 3)` while the preceding `0.1.5` fixture remains bounded to its historical interval.
- [ ] **Step 2: Run `rtk cargo test -p vmlord-display-payload` and verify the coverage failure.**
- [ ] **Step 3: Extend the current protocol interval to minor 3 and set all three release manifests to `0.1.6` with matching payload IDs/artifact metadata.** Update only fixtures whose expected current release changed.
- [ ] **Step 4: Run `rtk cargo test -p vmlord-display-payload`, `rtk cargo display-services`, and `rtk cargo check-windows`.** Expected: PASS.
- [ ] **Step 5: Commit:** `TASK-139: Move the display payload to 0.1.6`.

---

### Task 9: Architecture and operator documentation

**Files:**
- Modify: `ARCHITECTURE.md`
- Modify: an existing user/troubleshooting document only where `rg -n 'clipboard' docs README.md` identifies current clipboard guidance

**Interfaces:**
- Consumes: implemented behaviour from Tasks 1–8.
- Produces: authoritative documentation matching the shipped protocol and settings.

- [ ] **Step 1: Replace the task-139 refusal paragraph** with the negotiated capability, pull stream, staging roots, limits, path rules, cancellation/focus behaviour, cleanup and no-logging policy.
- [ ] **Step 2: Document the exact TOML grammar and defaults** (`1GB`, `4GB`, `24h`; binary size units; positive integer syntax; no UI), plus unsupported links/special files and retention implications.
- [ ] **Step 3: Run `rtk rg -n 'task #139|files are refused|text/uri-list.*ignored' ARCHITECTURE.md docs crates` and remove only stale claims contradicted by the implementation.**
- [ ] **Step 4: Run `rtk git diff --check`.** Expected: no whitespace errors.
- [ ] **Step 5: Commit:** `TASK-139: Document file clipboard transfers`.

---

### Task 10: Full verification and task completion

**Files:**
- Modify only files required by failures that directly exercise Task 139.
- Update Vikunja task 139 only after every automated verification succeeds.

- [ ] **Step 1: Run formatting:** `rtk cargo fmt --all -- --check`. Expected: PASS; if not, run `rtk cargo fmt --all`, inspect the diff, and rerun the check.
- [ ] **Step 2: Run portable suites:** `rtk cargo test -p vmlord-core`, `rtk cargo test -p vmlord-display-protocol`, `rtk cargo test -p vmlord-display-services`, `rtk cargo test -p vmlord-display-payload`. Expected: PASS.
- [ ] **Step 3: Run guest build:** `rtk cargo display-services`. Expected: all three musl binaries build without a system C toolchain.
- [ ] **Step 4: Run Windows suites:** `rtk cargo check-windows` and `rtk cargo test-windows`. Expected: PASS.
- [ ] **Step 5: Inspect repository state:** `rtk git status --short`, `rtk git diff main...HEAD --check`, and `rtk git log --oneline main..HEAD`. Confirm only Task 139 files/commits are present and every commit has the required prefix.
- [ ] **Step 6: Perform the manual Windows/Ubuntu matrix when the environment is available:** file, multiple files and nested directory in both directions; cancellation mid-file; focus loss; reconnect; symlink/reparse rejection; configured limit violation; expired staging cleanup; concurrent frames/input remain responsive. Record results without paths or content.
- [ ] **Step 7: If manual integration cannot run in this environment, leave Vikunja open and report the exact unverified matrix. If it passes, mark task 139 done and add a concise comment containing commit range and verification commands.**
