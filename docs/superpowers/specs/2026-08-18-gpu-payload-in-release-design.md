# GPU payload in the release design

## Goal

Task #103 makes a local archive the primary source of a GPU payload, and
`archive_url` the way back when there is no local one.

#93 built the whole boundary -- catalog, download, verification, cache,
staging -- and gave it one door: `cache::prepare`, which knows how to fetch an
archive over HTTPS and nothing else. Nothing on the host walks through that
door; `vmlord-gpu-payload` is linked by `xtask` alone. Meanwhile the payload
for Ubuntu 26.04 is built by a recipe in the repository and reaches no VM: the
per-VM `<vm>/gpu-payload` directory that `gpu_exports` already offers a guest
as `vmlord.gpu.payload` is never filled by anyone.

The end state of this task is a release that carries the archive next to
`vmlord.exe`, a `prepare` that will read it from there without touching the
network, and a host-side service that turns a prepared generation into that
per-VM directory.

## Decisions

* `archive_url` stays a required catalog field and keeps its meaning: where
  these bytes are published. It is also the fallback, and the channel for
  replacing a payload between releases.
* A local archive is not a URL. It is found by its own layout rule beside the
  executable and reaches `prepare` as its own field, so no catalog field ever
  names a path on the machine that built it.
* Trust is not relaxed for a local archive. It goes through the same
  preparation as a downloaded one: copied into a quarantined temporary
  directory, hashed against `archive_sha256`, expanded under the entry's file
  count and size limits, with `payload.json` and `sources.json` checked
  against the entry's provenance.
* The archive is not compiled into the binary. `include_bytes!` costs ~93 KB
  today and hundreds of megabytes the day `mesa_policy` becomes `bundled`.
* A host that has a local archive does not reach the network at all.
* The embedded catalog stays empty. Filling it belongs to the task that
  publishes the archive and puts a real URL in the spec.

## What the empty catalog means for this task

`PayloadCatalog::embedded()` with no entries selects nothing, so in a built
release the chain from local archive through the cache to a VM's staging
directory is unreachable. That is deliberate and it is stated here so that it
is not discovered later: this task builds the mechanism and proves it with
tests over fixtures, and the first real bytes to travel it will be the ones
the publishing task puts in the catalog.

The consequence is a scope line, not a gap. Everything below is written and
tested; only the last switch stays off.

## The archive source in `PrepareRequest`

`PrepareRequest` gains one field:

```rust
pub local_archive: Option<&'a Path>,
```

`prepare` reads it before it reaches for the network:

* `Some(path)` and the path is a regular file -- prepare from it. No HTTP
  agent is constructed, no `LockedArchive` is acquired, and `Connecting` and
  `Downloading` are never reported.
* `Some(path)` and there is no such file -- download, exactly as today. A
  build that shipped without its payload is not a broken build; it is a build
  that needs the network.
* `None` -- download.

A local file that is present but does not match the entry is an **error**, not
a reason to fall back. `ArchiveSizeMismatch` or `DigestMismatch` comes back
from the preparation that was already going to check it, and the host stops
there. The alternative -- quietly downloading what the local file failed to
be -- would put a machine on the network precisely when someone had arranged
for it not to be, and would hide a corrupt or substituted release artifact
behind a successful start.

The order inside `prepare` is unchanged otherwise: the digest lock is taken
first, an existing cache entry is verified and returned before any source is
consulted, and a cache entry that fails verification is quarantined.

`prepare_verified_archive` stays `pub(crate)`, against the wording of the
task. It runs under the `DigestLock` that `prepare` holds, and a public entry
beside that lock would let two processes prepare one generation at once --
which is the single thing the cross-process lock exists to prevent. The local
source reaches the same code through `prepare`, so a second public door buys
nothing and costs the guarantee.

## Where the archive lives

One function in the crate owns the layout, so that the release build and the
running application cannot disagree about it:

```rust
pub fn local_archive_path(directory: &Path, payload_id: &str) -> PathBuf
```

-- `directory.join("gpu-payload").join(format!("{payload_id}.zip"))`. The
caller supplies the directory holding the executable; the crate does not call
`current_exe` itself, because a function that reads process state cannot be
tested and this one has to be.

`payload_id` names the file because a release will carry more than one target,
and `payload_id` is already unique across the catalog.

## `cargo dist`

`dist` takes an optional repeatable argument:

```
cargo dist [--gpu-payload <directory>]
```

The directory is the output of the recipe in `payloads/<target>/README.md`:
`payload.zip` and `catalog-entry.json` as `xtask gpu-payload pack` wrote them,
side by side.

For each such directory `dist`:

1. reads `catalog-entry.json` and validates it through the catalog's own
   boundary, so the entry passes exactly the checks the embedded catalog's
   entries pass rather than a second, looser check written here. `pack` writes
   a bare entry object rather than a catalog document, and already wraps it in
   `{"schema_version": 1, "entries": [...]}` to validate what it just wrote;
   that wrapping moves into the crate as `PayloadCatalog::from_entry_json`, so
   `pack` and `dist` share one reading of the file instead of two;
2. verifies `payload.zip` against that entry's `archive_size` and
   `archive_sha256`;
3. copies it to `dist/gpu-payload/<payload_id>.zip`.

Any failure fails the build. A missing file, a truncated archive, a digest
that does not match the entry beside it -- each of them means the pair is not
what `pack` produced, and a release is the last place to find that out
politely.

Deeper validation is not repeated here. `payload.json`, `sources.json` and the
expansion limits are checked by `prepare` on the machine that will use the
payload, and a copy of that logic in the build tool would be a second opinion
that can drift from the first.

`catalog-entry.json` itself does not travel into the release. The catalog is
embedded and therefore trusted; a second catalog sitting on disk beside the
executable would be a catalog an attacker can edit, and the whole point of
`local_archive` being separate from `archive_url` is that no untrusted file
gets to say what bytes are expected.

Without the argument `dist` prints that no GPU payload is included and builds
as before. `dist` runs on Windows and the recipe is a bash script that fetches
from the network, so the build tool cannot produce the archive itself; it can
only refuse to ship a wrong one.

## Filling a VM's staging directory

A new `crates/platform/src/gpu_staging.rs`, beside `gpu_exports.rs`, holds one
service. Given the executable's directory, a cache root, a VM directory and
the `GuestTarget` an agent reported, it:

1. selects the catalog entry for that target from `PayloadCatalog::embedded()`;
2. calls `prepare` with `local_archive_path(executable_directory, payload_id)`
   as the local source and the cache root it was given;
3. calls `stage_payload` into `gpu_payload_staging_directory(vm_directory)` --
   the exact `<vm>/gpu-payload` child that `gpu_exports` will canonicalize and
   offer as `vmlord.gpu.payload`;
4. returns the `StagedGpuPayload`, whose generation directory and ready marker
   are what a guest is later told to read.

The cache root is a parameter. No `AppSettings` field is added: a setting
nobody writes and nobody reads is surface without a user, and the task that
starts a VM with a GPU is the one that will know where the cache belongs.

Like `gpu_exports`, this module is called by nothing yet -- a start cannot
know a VM's GPU mode until the task that applies assignment records one, and
that task is this service's caller. It carries the same `allow(dead_code)`
note, and the note goes away with the same task.

Failure is a failure of this service and not of the VM. GPU support is best
effort by #93's decision and by the epic's; the caller decides what a
`PayloadError` means for a start, and this module decides nothing about
lifecycle.

`vmlord-platform` gains a dependency on `vmlord-gpu-payload` -- the first
non-`xtask` consumer the crate has had.

## Tests

In the crate:

* `prepare` with a local archive that matches produces the same ready payload
  as the download path does, over the existing fixtures, and does so with no
  HTTP agent in reach;
* a local archive whose bytes are wrong fails with a digest mismatch and does
  not download -- driven by a request whose `archive_url` points at a server
  that would fail the test if it were contacted;
* a local archive that is short fails with `ArchiveSizeMismatch`;
* `local_archive: Some(path)` where the path does not exist downloads, and
  `None` downloads;
* a warm cache is returned without consulting either source;
* `local_archive_path` composes the documented layout.

In `xtask`:

* the argument parses, repeats and rejects a missing value;
* `PayloadCatalog::from_entry_json` accepts what `pack` writes and rejects an
  entry that fails catalog validation;
* a valid pair is copied under the payload's ID;
* a truncated archive, an archive whose digest does not match, an entry that
  fails catalog validation, and a missing file each fail the build.

In `platform`:

* the staging service fills the exact `<vm>/gpu-payload` child, and the
  directory it fills is the one `gpu_exports` accepts;
* an unsupported target comes back as `UnsupportedTarget` and leaves the VM
  directory untouched.

`cargo test-windows` and `cargo check-windows` are the final checks.

## Documentation

`payloads/ubuntu-26.04-amd64/README.md` gains what a built pair is for: not
only "paste it into the catalog when there is somewhere to publish", but
"pass its directory to `cargo dist`". **ARCHITECTURE.md** records the local
archive as the primary source, the release layout beside the executable, and
the staging service's place between the cache and the Plan9 export.

## Out of scope

Hosting and publishing the archive; filling the embedded catalog; building a
bundled Mesa; changing `mesa_policy`; the caller that knows a starting VM's
GPU mode.
