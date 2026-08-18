# GPU payload runtime catalog design

## Goal

A GPU payload is not part of the application. Today it is: `PayloadCatalog::embedded()`
(catalog.rs:272) is `include_bytes!` of a checked-in `catalog/catalog.json`, and
`gpu_staging.rs:45` knows no other catalog. Shipping a new payload therefore means
rebuilding `vmlord.exe`, which is a release of the application to distribute bytes the
application never links against.

The half that justified the arrangement does not exist. Every `archive_url` points at
`payloads.vmlord.invalid`, there is nowhere to publish, and `download.rs` is 500 lines of
`ureq` — timeouts, resumption, a `.part` lock — serving a URL that has never resolved.

The end state is a catalog assembled at runtime from files beside the executable, a
`prepare` with no network in it at all, and a schema that claims only what someone can
check.

## Where the catalog lives

`<executable directory>/gpu-payload/`, holding a pair per payload:

```
gpu-payload/
  ubuntu-26.04-amd64-7.0.0-28-v1.json
  ubuntu-26.04-amd64-7.0.0-28-v1.zip
```

`release.rs` already owns this rule for the archive and keeps owning it, gaining
`local_entry_path` beside `local_archive_path`. The directory is still a parameter rather
than read from `current_exe` inside the crate: `start.rs:121` computes it, the crate is
testable, and `cargo dist` — which is filling a distribution rather than running from
one — uses the same rule to write what the application will read.

The file name is not decoration. `from_release_directory` requires `<name>.json` to
contain `payload_id: "<name>"`, so the archive a given entry describes is found by the
entry's own ID and one directory listing tells a person which payloads a release carries.

## Reading it

```rust
impl PayloadCatalog {
    pub fn from_release_directory(directory: &Path) -> Result<Self, PayloadError>;
}
```

`directory` is the executable's, exactly as `local_archive_path` takes it; the
`gpu-payload` child is `release.rs`'s to know, so no caller spells it out. Each `*.json`
in that child is one entry document carrying its own
`schema_version: 2` — the entry is a release artifact now, not a build artifact waiting
to be pasted into a larger document, and it deserves its own version rather than
borrowing the catalog's.

`CatalogDocument`, `PayloadCatalog::from_json` and the wrapping inside `from_entry_json`
all go. The file `pack` writes *is* the format the application reads, so the entry has
one reading rather than three that can drift.

Uniqueness of `payload_id` and of `target` is still checked across the assembled
catalog — two entries for one guest would make selection depend on directory order.

### A missing catalog is an empty catalog

No `gpu-payload` directory, an empty one, or a directory that cannot be listed: the
catalog has no entries. `select_for_guest` then answers `NoPayloadForGuest`, and
`gpu_prepare.rs:220` logs a warning and starts the VM without GPU support. A build
without a payload is a build without GPU, and GPU assignment is best effort by the epic's
decision; refusing to start a VM over it would be a worse rule than the one it enforces.

A file that is *there* and wrong is an error, and the whole catalog fails with it: JSON
that does not parse, an entry that fails validation, a name that is not its `payload_id`,
or an entry whose `.zip` is missing. That is a broken release and saying nothing about it
would leave someone debugging a silent absence.

A `.zip` with no `.json` beside it is ignored. Nothing in the release claims it, and
failing over a leftover file would be a rule worse than the problem.

## What the entry schema loses

Four fields go, and two of them were holding something.

**`archive_url`** described where bytes are published. Nothing publishes them. It leaves
with `validate_url` and the `url` dependency.

**`archive_size`** was the declared length of the archive. It backed two real mechanisms,
and both move from *claimed* to *measured*:

* `copy_and_flush` (cache.rs:154) bounded its copy by it — it now takes the length
  `require_regular_file` already returns for the source file;
* `archive::extract` (archive.rs:203) capped the sum of the members' compressed sizes by
  it, guarding against a doctored central directory — it now caps by the archive's actual
  length.

This is strictly tighter than what it replaces. The bytes are hashed against
`archive_sha256` regardless, and a digest pins length as surely as it pins content, so
the declared size was a second opinion about something already proven. `ArchiveSizeMismatch`
survives with one meaning: the file changed under us mid-copy.

**`vmlord_revision`** and **`builder_version`** were cross-checked against `sources.json`
inside the archive (manifest.rs:179-180). They leave the entry *and* `sources.json`: with
no counterpart to agree with, a self-reported revision inside a digest-pinned archive
proves nothing that the digest does not already prove. This reaches the recipe
(`PackRecipe`), `payload.spec.json`, `prepare.py`'s `--revision`, and `prepare.sh`'s
`git rev-parse HEAD` with its dirty-tree warning — all of which existed to fill those two
fields.

`payload_manifest_sha256`, `expanded_size_limit`, `file_count_limit`, `required_renderers`,
`mesa_policy`, `sources` and `licenses` stay. Each is either a limit checked during
expansion or provenance a person reads.

## The network goes

`download.rs` is deleted and `ureq` leaves `Cargo.toml`. In `cache.rs` the fallback
branch (`None => LockedArchive::acquire(...)`, cache.rs:94-113) goes with it, and
`PrepareRequest` states the archive rather than offering it:

```rust
pub struct PrepareRequest<'a> {
    pub entry: &'a CatalogEntry,
    pub cache_root: &'a Path,
    pub archive: &'a Path,
    pub progress: &'a dyn Fn(PayloadProgress),
    pub cancel: &'a AtomicBool,
}
```

An archive that is not there is an error from `prepare`. There is no longer a second
source to fall back to, and the catalog guarantees the pair: an entry whose archive is
missing never reaches `prepare`, because `from_release_directory` refused it.

`DigestLock` stays. It exists so that two processes cannot prepare one generation at
once, which has nothing to do with downloading. `LockedArchive` and the `.part` lock go —
they were about a transfer that can be interrupted.

`PayloadProgress` moves out of `download.rs` into its own `progress.rs`, losing
`Connecting` and `Downloading` and keeping `Verifying`, `Extracting`, `Staging` and
`Ready`. Nothing outside the crate inspects the variants; `gpu_prepare.rs:213` passes an
empty closure.

## The staging service

`gpu_staging::stage_for_vm` changes in two lines: `PayloadCatalog::embedded()` becomes
`PayloadCatalog::from_release_directory(request.executable_directory)`, and
`local_archive: Some(&archive)` becomes `archive: &archive`. Its contract to its caller is unchanged, including that a
failure is a failure of GPU support and not of the VM.

## The build side

`pack` writes an entry document at `schema_version: 2` without the four fields, so what
it produces can be dropped into a release directory unedited.

`cargo dist --gpu-payload <directory>` now copies **both** files:
`catalog-entry.json` to `gpu-payload/<payload_id>.json` and `payload.zip` to
`gpu-payload/<payload_id>.zip`. Before copying it validates the entry through the
catalog's own boundary and hashes the archive against `archive_sha256`; the size check
goes with the field. Any failure fails the build — a release is the last place to find
out politely that a pair is not what `pack` produced.

The entry travelling into the release is the change of trust this task makes. It is
stated rather than hidden: the entry is no longer trusted for being compiled in, and
whoever can write into the installation directory can equally replace `vmlord.exe`
itself. The boundary does not move; it becomes visible. What stays refused is reading a
payload from a user directory or from configuration — the directory is derived from
`current_exe` by `release.rs`'s rule and from nothing else.

`crates/gpu-payload/catalog/catalog.json` and `PayloadCatalog::embedded()` are deleted.

## Tests

In the crate:

* a release directory with one valid pair yields a catalog that selects it;
* a directory that does not exist, one that is empty, and one holding only a stray `.zip`
  each yield an empty catalog, and selection answers `NoPayloadForGuest`;
* a `.json` that does not parse, one that fails entry validation, one whose name is not
  its `payload_id`, and one whose `.zip` is missing each fail;
* two entries claiming one target fail;
* `prepare` over the existing fixtures produces the same ready payload it does today, and
  an archive that is not there is an error;
* a corrupt archive fails with a digest mismatch, and a truncated one is caught by the
  same digest;
* a warm cache is returned without touching the archive;
* the compressed-size guard still fires when a member claims more than the archive holds;
* `local_entry_path` and `local_archive_path` compose the documented layout.

The catalog JSON literals in cache.rs, staging.rs, archive.rs and manifest.rs move to one
shared helper that writes an entry document, so the schema has one spelling in the tests
as it has one in the code.

In `xtask`: a valid pair is copied under the payload's ID as two files; a missing file,
an archive whose digest does not match its entry, and an entry that fails validation each
fail the build.

In `platform`: the staging service reads the catalog from the executable's directory, and
an executable directory with no payload leaves the VM directory untouched.

`cargo check-windows` and `cargo test-windows` are the final checks.

## Documentation

**ARCHITECTURE.md** replaces the local-archive-with-URL-fallback section with the runtime
catalog: the release layout, the pair, the missing-is-empty rule, and the trust model
above. `payloads/ubuntu-26.04-amd64/README.md` loses the "before this can be published"
section — there is nothing to publish — and says that `dist` places both files. Its
"commit before building" paragraph goes with `vmlord_revision`.

## Out of scope

Building a bundled Mesa (TASK-108), changing `mesa_policy`, and hosting a payload
anywhere. This task removes the mechanism that assumed hosting; it does not decide what
would replace it if hosting ever appeared.
