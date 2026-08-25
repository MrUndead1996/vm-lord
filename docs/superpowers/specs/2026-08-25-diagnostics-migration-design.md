# Diagnostics migration (TASK-8)

## Why

VMLord's diagnostics are the last piece of the AppSandbox migration, and they
are Rust-native work rather than a port: the C module they would have stood
beside is gone.

Two things are wrong with what stands today.

Records are strings. `core/logging.rs` writes `[stamp] [LEVEL] target: message`
through a hand-written `log::Log`, and everything a reader might want to select
on -- which VM, which subsystem, which Windows error code -- is prose inside
`message`.

Diagnostics are a second, parallel channel. `Diagnostic { level, message }` is
pushed by hand at some thirty sites in `platform/repository.rs` and twenty in
`app/lib.rs`, travels through `VmRepository::take_diagnostics`, and is drained
by `app::collect_diagnostics`. Worker threads carry a clone of the shared
`Arc<Mutex<Vec<Diagnostic>>>` so they have somewhere to push. The two channels
overlap in content and diverge on every edit.

## What this changes

`tracing` replaces `log` as the facade in the eleven host crates that use it.
The guest crates -- `agent` and `display-services` -- use neither; they write to
`eprintln!` and journald, and this work does not touch them.

Diagnostics stop being a channel and become a view of the one stream: code
emits a marked event, and a layer collects it.

### Non-goals

* No filter by subsystem in the UI. The field exists; whether the interface
  needs a control for it is a separate question.
* No log export, no file rotation, no telemetry.
* No field-by-field rewrite of every call site. See "Scope of the rewrite".

## Architecture

The subscriber is a `Registry` with two layers.

**The record layer** is today's `core/logging.rs` with `impl Log` replaced by
`impl Layer`. `tracing_subscriber::fmt` is deliberately not used: its UTC
timestamp needs the `time` crate, and this repository already has `timestamp`
-- Howard Hinnant's civil-date algorithm, with tests for the leap day and for
millisecond resolution. The line format, `compose`, `emit`, and the
`Console::Echo` / `Console::Silent` distinction (the `vmlord-display` pipe
carries framed launch messages and must never receive a log line) all stay.
What changes is the source: an `Event` instead of a `Record`, with its fields
and its ancestor spans' fields appended to the line through a `Visit`.

**The diagnostics layer** is a new `core/diagnostics.rs`. It owns
`DiagnosticsSink` -- an `Arc<Mutex<VecDeque<Diagnostic>>>` capped at 100
records, the cap moving here from `app::collect_diagnostics`. On `on_event` it
tests for the marker field, and when present composes a `Diagnostic` and queues
it. The UI drains the queue with `take`.

`vmlord/src/main.rs` builds the sink, installs the subscriber, and hands a clone
to `Application`. `vmlord-com1` and `vmlord-display` install the same subscriber
without the diagnostics layer: neither has a panel to show them in.

### What this removes

* `VmRepository::take_diagnostics` leaves the trait. Diagnostics were never a
  property of the repository.
* `Repository::diagnostics`, `push_diagnostic`, `push_shared_diagnostic`, and
  the `Diagnostics` alias in `display_launches.rs` go entirely. A worker thread
  that needed a clone of the shared buffer now writes `warn!` and is done; the
  plumbing that carried the handle disappears with it.
* `app::collect_diagnostics`. `app::diagnostics` remains, reading the sink.

### Test subscribers

`set_global_default` installs once per process, and the tests in `platform` and
`display-viewer` install a capturing logger today. They move to
`tracing::subscriber::with_default`, which is scoped -- an improvement on the
global logger, because tests stop interfering with each other.

## The diagnostic record

```rust
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub subsystem: Subsystem,
    pub vm: Option<String>,
    pub code: Option<u32>,
    pub at: SystemTime,
    pub message: String,
}
```

`Subsystem` is the list TASK-8 names -- `Hcs`, `Hns`, `Network`, `Gpu`,
`Display`, `Provisioning` -- plus `App` for what originates in the application
layer (settings saved, a user action recorded) and `Image` for image download,
which otherwise has nowhere to go. This is what "one stream" means in practice:
the subsystem stops being a guess from the wording of a message and becomes a
field.

`code` is `Option<u32>`: an HRESULT unsigned, shown as `0x{:08X}`. Win32 status
codes are already widened to HRESULTs in `subnet.rs`, so one field suffices.

`at` is stamped by the layer rather than the call site. When an event was
observed is a property of the observation, and taking it in the layer means it
cannot be forgotten.

### The marker

An event reaches the panel when it carries the `diagnostic` field. Not by
level: a `warn!` that an image cache did not warm belongs in the file and not in
the user's face. Not by target: targets move when code moves between modules,
which happens constantly.

`core` exports a macro so the marker cannot be misspelled:

```rust
diagnostic!(Warning, Subsystem::Gpu, vm = %name, "GPU-PV assignment was refused");
```

expanding to `tracing::warn!(diagnostic = true, subsystem = ?…, vm = %name, …)`.
AGENTS.md discourages unnecessary abstraction, and this one earns its place
narrowly: a typo in the field name would silently fail to show in the UI and
would break no test. Ordinary `info!` / `warn!` stay plain `tracing`. The macro
is only for what is addressed to a person.

### Spans

The repository's entry points -- `create_vm`, `start_vm`, `stop_vm`,
`delete_vm`, `update_display_payload`, and the display session -- open a span
carrying `vm`. Every event inside the operation then inherits the VM without
repeating it: the record layer writes ancestor fields, and the diagnostics layer
falls back to the span's `vm` when the event has none.

This is how TASK-8's "VM context on errors" is met -- not by adding a name at
two hundred sites, but by making it impossible to lose.

## Errors

`RepositoryError` stops wrapping a bare `String`:

```rust
pub struct RepositoryError {
    message: String,
    vm: Option<String>,
    operation: Option<&'static str>,
    code: Option<u32>,
}
```

`Display` produces exactly the string it produces today --
`Windows API operation "open compute system" for VM "dev-linux" failed (HRESULT 0x80070005): <description>`
-- composed from the fields instead of glued together in advance. The text is
load-bearing: tests in `platform/src/error.rs` assert it, and a person reads it.

Two constructors, and the difference between them is the point:

* `RepositoryError::new(message)` for what has no Windows code and never could:
  request validation, "this backend does not support that", a picker that is
  absent. The fields are empty honestly.
* `RepositoryError::windows(operation, vm, code, message)` takes the code
  positionally, so it cannot be omitted. `hresult_to_repository_error` and
  `windows_error` move onto it and stop formatting strings; they remain as the
  convenient way in from a `windows::core::Error`.

The site that turns an error into an event unpacks it into fields, so `code` and
`vm` reach the `Diagnostic` from deep inside `platform` instead of arriving as
prose. `operation` is not copied into `Diagnostic`: it is already inside
`message`, and a second field duplicating the text is noise.

`RepositoryError` lives in `core`, which does not depend on `windows`. `code` is
therefore a plain `u32`; the conversion stays in `platform`, where the `windows`
crate already is. No dependency boundary moves.

## Redaction

**A secret has neither a `Display` nor a `Debug` that shows its value.**
`tracing` requires an explicit `%` or `?` to record a field, which is to say one
of those two traits -- so a secret does not compile into a field at all. The
guarantee comes from the compiler rather than from vigilance.

What it covers:

* `Password`, which is already built this way in `core/provisioning.rs`.
* The private key from `keys`, and the agent secret: both gain the same
  redacting `Debug`.
* The cloud-init user-data document as a whole -- it carries the password hash.
  Nothing logs it today, and the rule is what keeps it that way.

The rule goes into ARCHITECTURE.md as a paragraph, alongside the pixel rule
already stated at the head of `display-viewer/src/log.rs`, and is held up by
tests: a capturing subscriber, a real operation, and an assertion that the
secret is not in the records.

No pattern-matching filter. A leak here can only come from our own code, and our
own code is what the compiler disciplines; a scrubber would cost something on
every record and would promise more than it can keep.

## Scope of the rewrite

`tracing` accepts the same format-string style as `log`, so
`log::info!("…{x}")` becomes `tracing::info!("…{x}")` mechanically across some
648 call sites.

Fields are added deliberately, in two places only: where an event is marked as a
diagnostic, and where an error carrying a code surfaces. The remaining five
hundred-odd lines stay as text. Turning each into a set of fields would rewrite
half the repository for structure nobody would select on.

`tracing-log`'s `LogTracer` is installed permanently, not as a migration
crutch: `eframe`/`egui` and other dependencies emit `log` records, and without
it the file would silently lose what it captures today. The `log` facade leaves
our code; the dependency stays in `Cargo.toml` for theirs.

## Verification

The existing tests are the safety line, and must pass without substantive edits
-- edits are permitted only where the subscriber is installed:

* error texts in `platform/src/error.rs`
* the line format, the leap day, and millisecond resolution in
  `core/logging.rs`
* the roughly ten diagnostic-level assertions in `platform/src/repository.rs`

New tests:

* the diagnostics layer keeps a marked event and drops an unmarked one
* `vm` is picked up from the enclosing span when the event does not carry it
* a secret does not appear in the records of a real operation
* `RepositoryError::windows` displays exactly as before

Run with `cargo check-windows` and `cargo test-windows`.
