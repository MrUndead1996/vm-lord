# File clipboard design

## Purpose

Task #125 carries text, HTML and images between the focused VMLord viewer and
the active GNOME session. It deliberately refuses `CF_HDROP` and
`text/uri-list`: files need bounded streaming, safe filesystem access and a
lifetime beyond one in-memory clipboard value. Task #139 adds that model in
both directions, enabled by default and governed by the existing focus gate.

The feature must not buffer a file in memory, log file contents or delay the
frame and input channels. It must interoperate with peers that implement the
original clipboard capability but not file transfer.

## Protocol model

File transfer extends the existing clipboard socket but not the existing
`clipboard::Exchange` state machine. A new `clipboard::files` state machine
owns one incoming and one outgoing file transfer. Text/image transfers and
file transfers have independent identifiers, cancellation and limits.

Protocol v1 receives a minor-version bump and a separate
`CAPABILITY_FILE_CLIPBOARD`. Both peers must negotiate it before either emits
file records. `CAPABILITY_CLIPBOARD` retains its current meaning, so an old
viewer and a new guest, or the reverse, continue to exchange supported
in-memory formats.

The clipboard record enum gains these messages:

| record | purpose |
| --- | --- |
| `FilePolicy` | host-selected size and retention policy for this session |
| `FileOffer` | a selection contains files, named by its clipboard serial |
| `FileRequest` | request that offer as a new file transfer |
| `FileEntry` | relative path, entry kind and regular-file byte length |
| `FileChunk` | the next bytes for the current regular-file entry |
| `FileComplete` | the complete tree passed sender-side validation |
| `FileCancel` | cancel one file transfer with a structured reason |

`FilePolicy` carries byte and second counts, not the strings used in the local
configuration. The receiver applies the policy before accepting an offer.
Unknown file records are never sent without the capability.

Files retain the clipboard's pull model. Copying a directory only announces a
file offer. Enumeration and file reads begin when the destination requests the
offer, which happens when its clipboard integration needs materialised paths.
The sender walks entries depth-first and sends each regular file in ordered
chunks. `FileComplete` commits the staging tree atomically from the transfer's
point of view; before it, no partial tree is exposed through a clipboard.

The existing clipboard record payload remains capped at 64 KiB. File chunks
use 60 KiB, leaving room for protobuf and record metadata. The pump writes a
bounded number of file chunks per iteration and then returns to socket,
clipboard and focus events. Files therefore cannot monopolise the clipboard
thread, and the clipboard process remains isolated from frames and input.

## Configuration

The host application settings gain an optional table:

```toml
[clipboard.files]
max_file_size = "1GB"
max_transfer_size = "4GB"
retention = "24h"
```

There is no settings UI in this task. An absent table or field receives the
shown default, so existing settings files need no migration.

Sizes accept a positive integer followed, case-insensitively, by `B`, `KB`,
`MB` or `GB`. These names use binary multipliers: 1024, 1024² and 1024³, so the
default `1GB` is exactly one GiB. Durations accept a positive integer followed
by `s`, `m` or `h`. Whitespace, fractions, compound values, unknown units,
zero and arithmetic overflow are rejected. `max_file_size` must not exceed
`max_transfer_size`. Serialisation uses the shortest exact supported unit.

The app validates the settings and passes the parsed values to the viewer in
its one-shot launch handover. The viewer sends them to the guest as
`FilePolicy`. Both file state machines enforce the same policy. The limits on
entry count, depth and wire-path length are protocol safety constants rather
than user settings:

| resource | limit |
| --- | ---: |
| entries, including directories | 4096 |
| directory depth | 64 |
| relative wire path | 1024 UTF-8 bytes |
| concurrent transfers | one per direction |

## Source-side filesystem rules

Only regular files and directories cross. Linux uses metadata operations that
do not follow symlinks. Windows rejects every reparse point, including
junctions. A symlink, reparse point, socket, FIFO, device, or other special
entry cancels the whole transfer rather than silently producing an incomplete
tree.

Top-level clipboard entries retain their base names. Every wire path is
relative UTF-8 with `/` separators. An entry is refused when it contains an
empty, `.` or `..` component, NUL, an absolute or drive-qualified prefix, a
colon, a component ending in a dot or space, or a Windows reserved device name
such as `CON`, `NUL`, `AUX`, `PRN`, `COM1` or `LPT1` (also with an extension).
Names must be representable on both platforms. Two paths that collide after
Windows case-insensitive comparison are refused.

The sender opens entries without following links and streams from the opened
handle. It checks the configured per-file limit before the first chunk and
tracks the cumulative declared and actual bytes. Both ends independently
enforce file size, total size, entry count, depth and path length. A file that
changes size while being read cancels the transfer; bytes are never padded and
the following entry is never parsed against a desynchronised stream.

## Destination and path containment

Each incoming transfer gets a fresh, unpredictable, user-private directory:

* Linux: `$XDG_RUNTIME_DIR/vmlord/clipboard/<session>/<transfer>`;
* Windows: `%LOCALAPPDATA%\VMLord\Clipboard\<session>\<transfer>`.

The Linux daemon does not fall back to shared `/tmp`: without a private runtime
directory it refuses file transfer while ordinary clipboard formats continue.
Windows creates directories with access limited to the current user.

The receiver validates and normalises a wire path before touching disk. It
then creates or opens every component relative to the staging-directory
handle, refuses existing destinations and refuses links/reparse points at
every step. It never obtains containment by concatenating an unchecked path or
by a textual prefix comparison. Files are created new and incomplete staging
is not advertised.

After `FileComplete`, the receiver builds the local selection from only the
top-level staged entries: `CF_HDROP` on Windows, and both `text/uri-list` and
`x-special/gnome-copied-files` in GNOME. URI generation percent-encodes paths;
URI parsing accepts only local `file://` entries and rejects remote
authorities.

## Lifetime and cleanup

Cancellation, validation failure, timeout, superseding offer, channel loss or
focus loss immediately closes the active handles and recursively removes
incomplete staging. Late records for a cancelled transfer ID are ignored.
Focus loss affects the file state machine independently and does not cancel or
stall an ordinary text/image operation.

A successfully staged tree remains while the selection created from it is
current. Replacing that selection removes its tree when no active transfer can
refer to it. Linux runtime files are additionally removed by the user session
lifecycle.

Windows clipboard data may outlive the viewer process while `CF_HDROP` still
contains its paths. Therefore a completed Windows tree is not deleted merely
because the viewer exits. Viewer startup removes completed trees older than
the configured retention, whose default is 24 hours, and always removes stale
incomplete trees. Cleanup is best-effort and never follows reparse points.

## Focus, supersession and cancellation

File clipboard is enabled by default whenever ordinary clipboard sync is
enabled and the viewer has keyboard focus. No separate user toggle is added.
A background VM cannot request host files or replace the host clipboard.

Losing focus cancels transfers in both directions with `FOCUS_LOST` and removes
partial destinations. A new local offer cancels an outgoing file transfer; a
new peer offer cancels an incoming one. Returning focus re-announces the
current local selection and processes the latest held peer offer, matching the
existing clipboard behaviour.

File cancellation has its own transfer ID and does not reuse an in-memory
clipboard transfer ID. A five-second lack of protocol progress cancels the
active transfer. Slow storage is handled outside frame/input processing, but
must still yield often enough for focus and cancellation to be observed.

## Platform integration

On Windows, the clipboard thread detects `CF_HDROP`, enumerates top-level paths
with Win32 APIs and uses delayed materialisation for an incoming guest offer.
It never calls PowerShell, WMI or an external process. All Win32 handles and
the small amount of required `unsafe` remain inside the Windows clipboard
module.

On Linux, `vmlord-display-clipboard` recognises `text/uri-list` and
`x-special/gnome-copied-files`. Mutter's `SelectionRead` supplies the URI list
for guest-to-host transfer; `SelectionWrite` receives the generated list for a
host-to-guest paste. Filesystem walking and staging are safe Rust plus the
minimal platform-specific descriptor operations already isolated in the
display services crate. No dependency may introduce a system C library into
the musl guest build.

## Logging and diagnostics

No log or diagnostic contains file contents, a full source path, a file name or
a URI. Records may name the direction, transfer ID, entry count, aggregate
byte count, configured limit, cancellation reason and outcome. Errors visible
to a user go through the existing diagnostics boundary where applicable;
ordinary transfer details remain `tracing` records.

## Compatibility and documentation

The protobuf descriptor and golden protocol fixtures change with the minor
version. Compatibility tests cover a peer without `FILE_CLIPBOARD`: it must
continue to exchange text, HTML and images and must see no file records.
`ARCHITECTURE.md` replaces task #139's refusal note with this model and records
the configuration grammar, staging locations, cleanup and safety policy.

The display payload version changes because the guest clipboard binary and
wire contract change together. User-facing documentation states that file
clipboard is enabled while the viewer is focused, documents the TOML-only
settings, and lists unsupported filesystem entries and name restrictions.

## Testing

Portable tests cover configuration parsing/normalisation and the file state
machine: negotiation, pull semantics, ordered entries and chunks, independent
IDs, both size limits, entry/depth/path caps, timeout, supersession, focus loss,
late chunks and old-peer compatibility.

Filesystem tests on both targets cover traversal, absolute paths, reserved
names, trailing dots/spaces, case-folded collisions, existing destinations,
symlinks/reparse points, special files, a file changing during transfer,
partial cleanup, successful retention and expiry cleanup. Tests verify that
no transferred content, name, URI or full path reaches captured logs.

Protocol descriptor, golden, malformed-record and fuzz tests gain every new
record. `cargo test-windows` exercises the Windows application and viewer;
workspace tests cover portable and Linux code; `cargo display-services`
preserves the static musl guest build. A manual end-to-end check copies a file,
multiple files and a directory in both directions, then exercises cancel,
focus loss, reconnect and a policy violation without disturbing frames or
input.

## Out of scope

This task does not add settings UI, cut/move semantics, remote URI download,
filesystem metadata preservation, ACLs, alternate data streams, sparse-file
preservation, resumable transfers or multiple concurrent file transfers in one
direction.
