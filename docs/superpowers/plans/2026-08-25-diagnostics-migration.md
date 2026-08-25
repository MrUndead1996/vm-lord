# Diagnostics Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `log` with `tracing` across VMLord's host crates and make the UI's diagnostics panel a view of that one stream rather than a second channel maintained by hand.

**Architecture:** A `tracing_subscriber::Registry` carries two layers. The record layer is today's hand-written logger with `impl Log` swapped for `impl Layer`, keeping the line format, the UTC millisecond timestamp and the console/pipe distinction. The diagnostics layer watches for events marked with a `diagnostic` field and queues them in a shared sink the UI drains. VM context arrives through spans opened at the repository's entry points, so no event has to repeat it.

**Tech Stack:** Rust 2024, `tracing`, `tracing-subscriber` (registry), `tracing-log`, `windows` 0.61, `egui`/`eframe`.

**Spec:** `docs/superpowers/specs/2026-08-25-diagnostics-migration-design.md`

## Global Constraints

- Rust edition 2024; workspace lints `unsafe_code = "deny"` and `clippy::all = "warn"` apply to every crate.
- Host-only work. `crates/agent` and `crates/display-services` are Linux guest programs, use neither `log` nor `tracing`, and must not be touched.
- Never add a dependency that makes the guest agent link against system C libraries. `tracing`, `tracing-subscriber` and `tracing-log` are pure Rust, so they qualify; nothing else may be added without raising it first.
- All application code in Rust; no C, no FFI. `unsafe` only inside `crates/platform` and `vmlord-agent::vsock`.
- The UI holds no business logic and calls no Windows API.
- Commit subjects are `TASK-8: <comment>`.
- Build and test with the aliases: `cargo check-windows`, `cargo test-windows`. Never prefix them with `timeout`.
- The log line format is fixed: `[1970-01-01T00:00:00.000Z] [INFO ] target: message`. Existing tests assert it.
- `RepositoryError`'s `Display` output is fixed: `Windows API operation "open compute system" for VM "dev-linux" failed (HRESULT 0x80070005)`, with `: <description>` appended when Windows supplied one.

---

## File Structure

**Created:**
- `crates/core/src/diagnostics.rs` — `Diagnostic`, `DiagnosticLevel`, `Subsystem`, `DiagnosticsSink`, `DiagnosticsLayer`, the `diagnostic!` macro.

**Modified:**
- `crates/core/src/logging.rs` — `impl Log` becomes `impl Layer`; subscriber assembly and `LogTracer` install.
- `crates/core/src/lib.rs` — `Diagnostic`/`DiagnosticLevel` move out to `diagnostics`; `VmRepository::take_diagnostics` becomes `refresh`.
- `crates/core/src/error.rs` (new home of `RepositoryError`, moved from `lib.rs`) — structured fields, two constructors.
- `crates/platform/src/error.rs` — `hresult_to_repository_error` and `windows_error` build the structured error.
- `crates/platform/src/repository.rs` — diagnostics buffer and push helpers deleted; `take_diagnostics` becomes `refresh`; spans on entry points.
- `crates/platform/src/display_launches.rs` — `Diagnostics` alias deleted.
- `crates/app/src/lib.rs` — reads the sink; `collect_diagnostics` deleted.
- `crates/ui/src/lib.rs` — `render_diagnostics` shows time, VM and code.
- `crates/vmlord/src/main.rs`, `crates/vmlord/src/bin/vmlord-com1.rs`, `crates/display-viewer/src/log.rs` — subscriber installation.
- `crates/keys/src/lib.rs`, `crates/seed/src/user_data.rs`, `crates/platform/src/create.rs` — redacting `Debug` on secrets.
- Eleven `Cargo.toml` files — `log` out of the host crates' dependencies, `tracing` in.

---

### Task 1: The record layer

Replace the `log::Log` implementation with a `tracing` layer, keeping every formatting decision and its tests. Call sites still say `log::info!` after this task and still work, because `LogTracer` forwards them.

**Files:**
- Modify: `Cargo.toml` (workspace dependencies)
- Modify: `crates/core/Cargo.toml`
- Modify: `crates/core/src/logging.rs`
- Modify: `crates/core/src/lib.rs` (re-exports)

**Interfaces:**
- Consumes: `AppSettings { log_file_path, log_level }` and `LogLevel` from `crates/core/src/settings.rs`.
- Produces: `pub fn initialize(&AppSettings) -> Result<(), LoggingError>` and `pub fn initialize_without_console(&AppSettings) -> Result<(), LoggingError>`, unchanged signatures, re-exported from `lib.rs` as `initialize_logging` and `initialize_logging_without_console`. Also `pub(crate) fn record_layer(settings: &AppSettings, console: Console) -> Result<RecordLayer, LoggingError>` for Task 3 to compose with.

- [ ] **Step 1: Add the dependencies**

In the workspace `Cargo.toml`, under `[workspace.dependencies]`, beside the existing `log = "0.4"`:

```toml
tracing = "0.1"
# `registry` is what stores span data, which is how an event inherits the VM
# name from the operation it happened inside. No `fmt`: its UTC timestamp
# needs the `time` crate, and `logging.rs` already spells the stamp itself.
tracing-subscriber = { version = "0.3", default-features = false, features = ["registry", "std"] }
# Dependencies -- `eframe` among them -- write through `log`. Without this
# their records stop reaching the file.
tracing-log = "0.2"
```

Keep `log = "0.4"`: `core` still needs it for `LogTracer`'s level mapping, and dependencies still emit through it.

In `crates/core/Cargo.toml`, add under `[dependencies]`:

```toml
tracing.workspace = true
tracing-subscriber.workspace = true
tracing-log.workspace = true
```

- [ ] **Step 2: Write the failing test for field rendering**

Add to the `tests` module at the bottom of `crates/core/src/logging.rs`:

```rust
#[test]
fn a_records_fields_follow_its_message() {
    // Fields are what makes a record selectable. They go after the message so
    // that a line still reads as a sentence first.
    let line = compose(
        "2024-02-29T01:01:01.123Z",
        Level::WARN,
        "vmlord_platform::repository",
        "the endpoint could not be attached",
        " vm=dev-linux code=0x803B0014",
    );

    assert_eq!(
        line,
        "[2024-02-29T01:01:01.123Z] [WARN ] \
         vmlord_platform::repository: the endpoint could not be attached \
         vm=dev-linux code=0x803B0014"
    );
}
```

- [ ] **Step 3: Run it and watch it fail**

Run: `cargo test-windows -p vmlord-core logging`
Expected: FAIL — `compose` takes four arguments, and `Level` now resolves to `tracing::Level`.

- [ ] **Step 4: Rewrite `compose` and the level mapping**

In `crates/core/src/logging.rs`, replace the `use log::{...}` line with:

```rust
use tracing::{Event, Level, Metadata, Subscriber, field::{Field, Visit}};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};
```

Replace `compose` with:

```rust
/// One record's line, stamp first, fields last.
///
/// Its own function because what a line looks like is worth a test, and
/// installing a subscriber to read one back is not. `fields` arrives already
/// rendered and already carrying its leading space, or empty.
fn compose(stamp: &str, level: Level, target: &str, message: &str, fields: &str) -> String {
    format!("[{stamp}] [{:<5}] {target}: {message}{fields}", level.as_str())
}
```

Replace `level_filter` with a `tracing` equivalent, keeping the existing test's shape:

```rust
fn level_filter(level: LogLevel) -> LevelFilter {
    match level {
        LogLevel::Error => LevelFilter::ERROR,
        LogLevel::Warn => LevelFilter::WARN,
        LogLevel::Info => LevelFilter::INFO,
        LogLevel::Debug => LevelFilter::DEBUG,
        LogLevel::Trace => LevelFilter::TRACE,
    }
}
```

with `use tracing::level_filters::LevelFilter;` added to the imports, and update the existing `maps_configured_levels_to_log_filters` test to compare against `LevelFilter::ERROR` and friends.

Update `a_record_carries_its_stamp_before_anything_else` to pass the new arguments:

```rust
let line = compose(
    "2024-02-29T01:01:01.123Z",
    Level::INFO,
    "vmlord_display_viewer::status",
    "the display session is Running",
    "",
);
```

- [ ] **Step 5: Write the visitor that renders fields**

Add to `crates/core/src/logging.rs`:

```rust
/// Pulls a record's message and its remaining fields apart.
///
/// `tracing` carries the message as a field named `message`, so it has to be
/// separated here rather than in the format string. Everything else is
/// rendered `name=value`, each with a leading space, so that `compose` can
/// append the lot without knowing whether there were any.
#[derive(Default)]
struct RecordVisitor {
    message: String,
    fields: String,
}

impl Visit for RecordVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        // Codes read as hex or they read as nothing: `0x803B0014` is
        // searchable and `2151546900` is not.
        if field.name() == "code" {
            let _ = write!(self.fields, " code=0x{value:08X}");
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        let _ = write!(self.fields, " {}={value}", field.name());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // A field written `%value` arrives here too, wrapped so that its
        // `Debug` prints the `Display` text unquoted.
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}
```

Add `Write as _` to the `std::fmt` import at the top of the file so `write!` works on a `String`:

```rust
use std::fmt::{self, Write as _};
```

- [ ] **Step 6: Turn the logger into a layer**

Replace the `impl Log for ApplicationLogger` block with:

```rust
pub(crate) struct RecordLayer {
    level: LevelFilter,
    console: Console,
    file: Mutex<File>,
}

impl<S> Layer<S> for RecordLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn enabled(&self, metadata: &Metadata<'_>, _: Context<'_, S>) -> bool {
        *metadata.level() <= self.level
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        if *event.metadata().level() > self.level {
            return;
        }

        let mut visitor = RecordVisitor::default();
        event.record(&mut visitor);
        // The operation's own fields come before the event's: a reader who
        // scans down a column wants the VM in the same place every line.
        let mut fields = String::new();
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                if let Some(rendered) = span.extensions().get::<SpanFields>() {
                    fields.push_str(&rendered.0);
                }
            }
        }
        fields.push_str(&visitor.fields);

        let line = compose(
            &timestamp(SystemTime::now()),
            *event.metadata().level(),
            event.metadata().target(),
            &visitor.message,
            &fields,
        );
        if let Ok(mut file) = self.file.lock() {
            emit(&line, self.console, &mut io::stdout().lock(), &mut *file);
        } else if self.console == Console::Echo {
            let _ = writeln!(io::stdout().lock(), "{line}");
        }
    }

    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: Context<'_, S>,
    ) {
        let mut visitor = RecordVisitor::default();
        attributes.record(&mut visitor);
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(SpanFields(visitor.fields));
        }
    }
}

/// A span's fields, rendered once when it opens rather than on every event
/// inside it.
struct SpanFields(String);
```

Delete the `struct ApplicationLogger` definition; `RecordLayer` replaces it field for field.

- [ ] **Step 7: Rewrite installation**

Replace `install` with:

```rust
fn install(settings: &AppSettings, console: Console) -> Result<(), LoggingError> {
    let layer = record_layer(settings, console)?;
    let level = level_filter(settings.log_level);
    // Dependencies write through `log`; without this their records stop
    // reaching the file.
    tracing_log::LogTracer::init().map_err(LoggingError::LogBridge)?;
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer))
        .map_err(LoggingError::AlreadyInitialized)?;
    log::set_max_level(level_to_log_filter(level));
    Ok(())
}

/// The file half of the subscriber, so that a process which also wants
/// diagnostics can compose the two.
pub(crate) fn record_layer(
    settings: &AppSettings,
    console: Console,
) -> Result<RecordLayer, LoggingError> {
    let log_directory =
        settings
            .log_file_path
            .parent()
            .ok_or_else(|| LoggingError::MissingParent {
                path: settings.log_file_path.clone(),
            })?;
    fs::create_dir_all(log_directory).map_err(|source| LoggingError::Io {
        operation: "create log directory",
        path: log_directory.to_path_buf(),
        source,
    })?;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&settings.log_file_path)
        .map_err(|source| LoggingError::Io {
            operation: "open log file",
            path: settings.log_file_path.clone(),
            source,
        })?;

    Ok(RecordLayer {
        level: level_filter(settings.log_level),
        console,
        file: Mutex::new(file),
    })
}

/// `LogTracer` decides what to forward from `log`'s own max level, so the two
/// have to be told the same thing.
fn level_to_log_filter(level: LevelFilter) -> log::LevelFilter {
    match level {
        LevelFilter::OFF => log::LevelFilter::Off,
        LevelFilter::ERROR => log::LevelFilter::Error,
        LevelFilter::WARN => log::LevelFilter::Warn,
        LevelFilter::INFO => log::LevelFilter::Info,
        LevelFilter::DEBUG => log::LevelFilter::Debug,
        LevelFilter::TRACE => log::LevelFilter::Trace,
        _ => log::LevelFilter::Info,
    }
}
```

Add `use tracing_subscriber::layer::SubscriberExt as _;` to the imports, and make `Console` `pub(crate)` so Task 3 can name it.

Change `LoggingError`'s `AlreadyInitialized` variant to hold `tracing::subscriber::SetGlobalDefaultError` instead of `log::SetLoggerError`, add a `LogBridge(tracing_log::log_tracer::SetLoggerError)` variant, and extend the `Display` and `source` matches:

```rust
Self::LogBridge(source) => write!(
    formatter,
    "the log-to-tracing bridge is already installed: {source}"
),
```

- [ ] **Step 8: Run the whole logging suite**

Run: `cargo test-windows -p vmlord-core logging`
Expected: PASS — the epoch, the leap day, the millisecond, both `emit` tests, the level mapping, the stamp-first line, and the new field line.

- [ ] **Step 9: Compile the workspace**

Run: `cargo check-windows`
Expected: PASS. Call sites still say `log::info!` and are forwarded by `LogTracer`.

- [ ] **Step 10: Commit**

```bash
git add Cargo.toml Cargo.lock crates/core/Cargo.toml crates/core/src/logging.rs crates/core/src/lib.rs
git commit -m "TASK-8: Write records through a tracing layer"
```

---

### Task 2: Move the call sites onto `tracing`

Mechanical, and worth its own commit so that the interesting tasks are readable in the history.

**Files:**
- Modify: every `.rs` under `crates/{core,platform,app,ui,vmlord,display-viewer,image,keys,seed,payload,display-payload,gpu-payload}/src` that calls a `log` macro
- Modify: the same crates' `Cargo.toml`

**Interfaces:**
- Consumes: the subscriber from Task 1.
- Produces: nothing new. `log` remains a dependency of `crates/core` alone, for `LogTracer`.

- [ ] **Step 1: Rewrite the macro calls**

`tracing` accepts the same format-string style, so this is a textual substitution:

```bash
grep -rl 'log::\(error\|warn\|info\|debug\|trace\)!' --include='*.rs' crates \
  | xargs sed -i 's/\blog::\(error\|warn\|info\|debug\|trace\)!/tracing::\1!/g'
```

- [ ] **Step 2: Move the dependency**

In each of `crates/{platform,app,ui,vmlord,display-viewer,image,keys,seed,payload,display-payload,gpu-payload}/Cargo.toml`, replace `log.workspace = true` with `tracing.workspace = true`. In `crates/core/Cargo.toml` keep both: `log` is what `LogTracer` bridges from.

- [ ] **Step 3: Compile**

Run: `cargo check-windows`
Expected: PASS. If a crate reports an unused `log` dependency or a missing `tracing` one, fix that crate's manifest and rerun.

- [ ] **Step 4: Run the suite**

Run: `cargo test-windows`
Expected: PASS, except tests that install `log`'s capturing logger in `crates/display-viewer/src/log.rs` — those are Task 3's business. If they fail here, note which and continue; do not fix them in this task.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "TASK-8: Write log macros through tracing"
```

---

### Task 3: The diagnostic record, the sink and the layer

**Files:**
- Create: `crates/core/src/diagnostics.rs`
- Modify: `crates/core/src/lib.rs`

**Interfaces:**
- Consumes: `record_layer` and `Console` from Task 1.
- Produces:
  - `pub enum Subsystem { Hcs, Hns, Network, Gpu, Display, Provisioning, Image, App }` with `pub fn as_str(&self) -> &'static str` and `impl FromStr`.
  - `pub struct Diagnostic { pub level: DiagnosticLevel, pub subsystem: Subsystem, pub vm: Option<String>, pub code: Option<u32>, pub at: SystemTime, pub message: String }`
  - `pub struct DiagnosticsSink(Arc<Mutex<VecDeque<Diagnostic>>>)` with `pub fn new() -> Self`, `pub fn take(&self) -> Vec<Diagnostic>` and `Clone`.
  - `pub fn initialize_with_diagnostics(settings: &AppSettings) -> Result<DiagnosticsSink, LoggingError>`
  - `macro_rules! diagnostic`, exported at the crate root.

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/diagnostics.rs` with only a `tests` module for now:

```rust
#[cfg(test)]
mod tests {
    use tracing_subscriber::layer::SubscriberExt as _;

    use super::{DiagnosticLevel, DiagnosticsLayer, DiagnosticsSink, Subsystem};

    fn collect(body: impl FnOnce()) -> Vec<super::Diagnostic> {
        let sink = DiagnosticsSink::new();
        let subscriber =
            tracing_subscriber::registry().with(DiagnosticsLayer::new(sink.clone()));
        tracing::subscriber::with_default(subscriber, body);
        sink.take()
    }

    #[test]
    fn a_marked_event_reaches_the_panel() {
        let records = collect(|| {
            crate::diagnostic!(
                Warning,
                Subsystem::Gpu,
                vm = "dev-linux",
                "GPU-PV assignment was refused"
            );
        });

        assert_eq!(records.len(), 1);
        assert_eq!(records[0].level, DiagnosticLevel::Warning);
        assert_eq!(records[0].subsystem, Subsystem::Gpu);
        assert_eq!(records[0].vm.as_deref(), Some("dev-linux"));
        assert_eq!(records[0].message, "GPU-PV assignment was refused");
    }

    #[test]
    fn an_unmarked_event_does_not() {
        // A warning that an image cache did not warm belongs in the file and
        // not in the user's face. The marker, not the level, is what decides.
        let records = collect(|| {
            tracing::warn!("the image cache could not be warmed");
        });

        assert!(records.is_empty(), "{records:?}");
    }

    #[test]
    fn a_windows_code_survives_as_a_number() {
        let records = collect(|| {
            crate::diagnostic!(
                Error,
                Subsystem::Hcs,
                vm = "dev-linux",
                code = 0x803B_0014_u32,
                "the endpoint was already attached"
            );
        });

        assert_eq!(records[0].code, Some(0x803B_0014));
    }

    #[test]
    fn the_sink_keeps_the_hundred_most_recent_records() {
        let sink = DiagnosticsSink::new();
        let subscriber =
            tracing_subscriber::registry().with(DiagnosticsLayer::new(sink.clone()));
        tracing::subscriber::with_default(subscriber, || {
            for index in 0..120 {
                crate::diagnostic!(Info, Subsystem::App, "record {index}");
            }
        });

        let records = sink.take();
        assert_eq!(records.len(), 100);
        assert_eq!(records[0].message, "record 20");
        assert_eq!(records[99].message, "record 119");
    }

    #[test]
    fn a_subsystem_survives_the_round_trip_through_a_field() {
        // The layer reads the subsystem back by parsing the string the macro
        // wrote. If the two ever disagree the record lands in the wrong place
        // silently, so they are tested against each other.
        for subsystem in [
            Subsystem::Hcs,
            Subsystem::Hns,
            Subsystem::Network,
            Subsystem::Gpu,
            Subsystem::Display,
            Subsystem::Provisioning,
            Subsystem::Image,
            Subsystem::App,
        ] {
            assert_eq!(subsystem.as_str().parse(), Ok(subsystem));
        }
    }

    #[test]
    fn taking_the_records_empties_the_sink() {
        let sink = DiagnosticsSink::new();
        let subscriber =
            tracing_subscriber::registry().with(DiagnosticsLayer::new(sink.clone()));
        tracing::subscriber::with_default(subscriber, || {
            crate::diagnostic!(Info, Subsystem::App, "settings saved");
        });

        assert_eq!(sink.take().len(), 1);
        assert!(sink.take().is_empty(), "a record is shown once");
    }
}
```

- [ ] **Step 2: Run and watch it fail**

Add `mod diagnostics;` to `crates/core/src/lib.rs` first, then run: `cargo test-windows -p vmlord-core diagnostics`
Expected: FAIL — nothing in the module exists yet.

- [ ] **Step 3: Write the record and the subsystem**

At the top of `crates/core/src/diagnostics.rs`:

```rust
//! The stream a person reads, taken from the stream the file records.
//!
//! An event marked with a `diagnostic` field is meant for the user; the layer
//! below collects those and nothing else. Everything about a record other than
//! its text is a field, so the panel can select on it rather than parse prose.

use std::{
    collections::VecDeque,
    str::FromStr,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{layer::Context, Layer};

/// How many records the panel keeps. Older ones fall off the front: a session
/// that ran all day should not carry its morning in memory.
const CAPACITY: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub subsystem: Subsystem,
    pub vm: Option<String>,
    /// A Windows error code, unsigned, shown as `0x{:08X}`. Win32 statuses are
    /// widened to HRESULTs before they get here, so one field is enough.
    pub code: Option<u32>,
    /// Stamped by the layer rather than the call site: when a thing was
    /// observed is a property of the observation, and taking it here means it
    /// cannot be forgotten.
    pub at: SystemTime,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Info,
    Warning,
    Error,
}

/// Which part of VMLord an event came from.
///
/// A field rather than a guess from the wording of a message: this is what
/// makes HCS, HNS, networking, GPU, display and provisioning one stream instead
/// of six conventions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subsystem {
    Hcs,
    Hns,
    Network,
    Gpu,
    Display,
    Provisioning,
    Image,
    App,
}

impl Subsystem {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hcs => "Hcs",
            Self::Hns => "Hns",
            Self::Network => "Network",
            Self::Gpu => "Gpu",
            Self::Display => "Display",
            Self::Provisioning => "Provisioning",
            Self::Image => "Image",
            Self::App => "App",
        }
    }
}

impl FromStr for Subsystem {
    type Err = ();

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "Hcs" => Ok(Self::Hcs),
            "Hns" => Ok(Self::Hns),
            "Network" => Ok(Self::Network),
            "Gpu" => Ok(Self::Gpu),
            "Display" => Ok(Self::Display),
            "Provisioning" => Ok(Self::Provisioning),
            "Image" => Ok(Self::Image),
            "App" => Ok(Self::App),
            _ => Err(()),
        }
    }
}
```

- [ ] **Step 4: Write the sink**

```rust
/// Where marked records wait for the UI to read them.
///
/// Shared and interior-mutable because the threads that produce them -- a
/// build, a start, a display session -- are not the thread that shows them.
#[derive(Clone, Default)]
pub struct DiagnosticsSink(Arc<Mutex<VecDeque<Diagnostic>>>);

impl DiagnosticsSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn push(&self, record: Diagnostic) {
        let mut records = self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if records.len() == CAPACITY {
            records.pop_front();
        }
        records.push_back(record);
    }

    /// Everything recorded since the last read, oldest first.
    #[must_use]
    pub fn take(&self) -> Vec<Diagnostic> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .drain(..)
            .collect()
    }
}
```

- [ ] **Step 5: Write the layer and its visitor**

```rust
pub struct DiagnosticsLayer {
    sink: DiagnosticsSink,
}

impl DiagnosticsLayer {
    #[must_use]
    pub fn new(sink: DiagnosticsSink) -> Self {
        Self { sink }
    }
}

impl<S: Subscriber> Layer<S> for DiagnosticsLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut visitor = DiagnosticVisitor::default();
        event.record(&mut visitor);
        if !visitor.marked {
            return;
        }
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => DiagnosticLevel::Error,
            tracing::Level::WARN => DiagnosticLevel::Warning,
            _ => DiagnosticLevel::Info,
        };
        self.sink.push(Diagnostic {
            level,
            subsystem: visitor.subsystem.unwrap_or(Subsystem::App),
            vm: visitor.vm,
            code: visitor.code,
            at: SystemTime::now(),
            message: visitor.message,
        });
    }
}

/// Reads the fields the `diagnostic!` macro writes, by name and by type.
#[derive(Default)]
struct DiagnosticVisitor {
    marked: bool,
    subsystem: Option<Subsystem>,
    vm: Option<String>,
    code: Option<u32>,
    message: String,
}

impl Visit for DiagnosticVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "diagnostic" {
            self.marked = value;
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "subsystem" => self.subsystem = value.parse().ok(),
            "vm" => self.vm = Some(value.to_string()),
            "message" => self.message = value.to_string(),
            _ => {}
        }
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        if field.name() == "code" {
            self.code = u32::try_from(value).ok();
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // The message arrives here whenever it interpolates anything.
        if field.name() == "message" {
            self.message = format!("{value:?}");
        }
    }
}
```

- [ ] **Step 6: Write the macro**

At the end of `crates/core/src/diagnostics.rs`:

```rust
/// Records an event that is addressed to a person.
///
/// A macro rather than a function so the marker field cannot be misspelled: a
/// `diagnostc = true` would silently fail to show in the UI and would break no
/// test. Ordinary `info!` and `warn!` stay plain `tracing`; this is only for
/// what the user is meant to read.
///
/// The level is a literal so the `tracing` macro is chosen at compile time,
/// which keeps the record's level and the event's level one decision.
#[macro_export]
macro_rules! diagnostic {
    ($level:ident, $subsystem:expr, vm = $vm:expr, code = $code:expr, $($rest:tt)*) => {
        $crate::__diagnostic_emit!(
            $level,
            diagnostic = true,
            subsystem = $subsystem.as_str(),
            vm = $vm,
            code = $code as u64,
            $($rest)*
        )
    };
    ($level:ident, $subsystem:expr, vm = $vm:expr, $($rest:tt)*) => {
        $crate::__diagnostic_emit!(
            $level,
            diagnostic = true,
            subsystem = $subsystem.as_str(),
            vm = $vm,
            $($rest)*
        )
    };
    ($level:ident, $subsystem:expr, code = $code:expr, $($rest:tt)*) => {
        $crate::__diagnostic_emit!(
            $level,
            diagnostic = true,
            subsystem = $subsystem.as_str(),
            code = $code as u64,
            $($rest)*
        )
    };
    ($level:ident, $subsystem:expr, $($rest:tt)*) => {
        $crate::__diagnostic_emit!(
            $level,
            diagnostic = true,
            subsystem = $subsystem.as_str(),
            $($rest)*
        )
    };
}

/// Turns a `DiagnosticLevel` spelled as a literal into the matching `tracing`
/// macro. Separate only so the four forms above do not each repeat it.
#[doc(hidden)]
#[macro_export]
macro_rules! __diagnostic_emit {
    (Info, $($rest:tt)*) => { ::tracing::info!($($rest)*) };
    (Warning, $($rest:tt)*) => { ::tracing::warn!($($rest)*) };
    (Error, $($rest:tt)*) => { ::tracing::error!($($rest)*) };
}
```

- [ ] **Step 7: Compose the subscriber**

In `crates/core/src/logging.rs`, add:

```rust
/// Brings up logging and the diagnostics panel together.
///
/// The panel is the reason a sink comes back: the caller hands it to the
/// application, which reads it on every refresh.
pub fn initialize_with_diagnostics(
    settings: &AppSettings,
) -> Result<crate::diagnostics::DiagnosticsSink, LoggingError> {
    let sink = crate::diagnostics::DiagnosticsSink::new();
    let layer = record_layer(settings, Console::Echo)?;
    tracing_log::LogTracer::init().map_err(LoggingError::LogBridge)?;
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry()
            .with(layer)
            .with(crate::diagnostics::DiagnosticsLayer::new(sink.clone())),
    )
    .map_err(LoggingError::AlreadyInitialized)?;
    log::set_max_level(level_to_log_filter(level_filter(settings.log_level)));
    Ok(sink)
}
```

- [ ] **Step 8: Export from the crate root**

In `crates/core/src/lib.rs`, remove the `Diagnostic` and `DiagnosticLevel` definitions (lines 221-231) and re-export instead:

```rust
mod diagnostics;

pub use diagnostics::{
    Diagnostic, DiagnosticLevel, DiagnosticsLayer, DiagnosticsSink, Subsystem,
};
pub use logging::initialize_with_diagnostics;
```

- [ ] **Step 9: Port the viewer's capturing logger**

`crates/display-viewer/src/log.rs`'s `capture` module installs a `log::Log`
globally through a `OnceLock`, which Task 2 left stranded. Replace it with a
layer the tests install per test, which is what makes them stop interfering
with each other:

```rust
/// A subscriber that keeps every record, for the tests that assert what is
/// not in them.
#[cfg(test)]
pub mod capture {
    use std::sync::{Arc, Mutex};

    use tracing::{Event, Subscriber, field::{Field, Visit}};
    use tracing_subscriber::{layer::Context, Layer};

    #[derive(Clone, Default)]
    pub struct Records(Arc<Mutex<Vec<String>>>);

    impl Records {
        /// Everything recorded so far, joined.
        pub fn text(&self) -> String {
            self.0
                .lock()
                .expect("no test panics while holding the records")
                .join("\n")
        }
    }

    pub struct Capture(pub Records);

    impl<S: Subscriber> Layer<S> for Capture {
        fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
            let mut message = String::new();
            event.record(&mut MessageVisitor(&mut message));
            self.0
                 .0
                .lock()
                .expect("no test panics while holding the records")
                .push(message);
        }
    }

    struct MessageVisitor<'a>(&'a mut String);

    impl Visit for MessageVisitor<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.0 = format!("{value:?}");
            }
        }
    }
}
```

Each test that called `capture::install()` and `capture::text()` now builds a
`Records`, installs `Capture` through `tracing::subscriber::with_default`, and
reads `records.text()` afterwards. Add `tracing-subscriber` to
`crates/display-viewer`'s `[dev-dependencies]`.

- [ ] **Step 10: Run the tests**

Run: `cargo test-windows -p vmlord-core diagnostics` then
`cargo test-windows -p vmlord-display-viewer`
Expected: PASS — the six new tests, and the viewer's tests back on their feet.

- [ ] **Step 11: Commit**

```bash
git add crates/core/src/diagnostics.rs crates/core/src/lib.rs crates/core/src/logging.rs crates/display-viewer
git commit -m "TASK-8: Collect marked events into a diagnostics sink"
```

---

### Task 4: VM context from the enclosing span

**Files:**
- Modify: `crates/core/src/diagnostics.rs`

**Interfaces:**
- Consumes: `DiagnosticsLayer` from Task 3.
- Produces: `DiagnosticsLayer`'s `Layer<S>` bound tightens to `S: Subscriber + for<'a> LookupSpan<'a>`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module of `crates/core/src/diagnostics.rs`:

```rust
#[test]
fn a_record_takes_the_vm_from_the_operation_it_happened_inside() {
    // The point of the span: an event deep inside `start_vm` should not have
    // to name the VM, and should not be able to lose it.
    let records = collect(|| {
        let span = tracing::info_span!("start_vm", vm = "dev-linux");
        let _entered = span.enter();
        crate::diagnostic!(Error, Subsystem::Hcs, "the compute system refused to start");
    });

    assert_eq!(records[0].vm.as_deref(), Some("dev-linux"));
}

#[test]
fn a_record_that_names_its_own_vm_keeps_it() {
    // A refresh that reports on one VM from inside another's span must not be
    // relabelled by its surroundings.
    let records = collect(|| {
        let span = tracing::info_span!("refresh", vm = "outer");
        let _entered = span.enter();
        crate::diagnostic!(Warning, Subsystem::Display, vm = "inner", "the window closed");
    });

    assert_eq!(records[0].vm.as_deref(), Some("inner"));
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test-windows -p vmlord-core diagnostics`
Expected: FAIL — `vm` is `None` in the first test; the second passes already.

- [ ] **Step 3: Store the span's VM when the span opens**

In `crates/core/src/diagnostics.rs`, add:

```rust
/// The VM an operation is about, kept on the span so that events inside it do
/// not have to repeat it.
struct SpanVm(String);
```

and add to `impl Layer for DiagnosticsLayer`:

```rust
fn on_new_span(
    &self,
    attributes: &tracing::span::Attributes<'_>,
    id: &tracing::span::Id,
    context: Context<'_, S>,
) {
    let mut visitor = DiagnosticVisitor::default();
    attributes.record(&mut visitor);
    if let (Some(vm), Some(span)) = (visitor.vm, context.span(id)) {
        span.extensions_mut().insert(SpanVm(vm));
    }
}
```

- [ ] **Step 4: Fall back to it in `on_event`**

Change the bound, and rename `on_event`'s context parameter from `_` to
`context` so the fallback can look the span up:

```rust
impl<S> Layer<S> for DiagnosticsLayer
where
    S: Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
```

and, after the `marked` check:

```rust
// The event's own VM wins: a refresh may report on one VM from inside
// another's span, and being relabelled by its surroundings would be a lie.
let vm = visitor.vm.or_else(|| {
    context
        .event_scope(event)?
        .from_root()
        .filter_map(|span| span.extensions().get::<SpanVm>().map(|found| found.0.clone()))
        .last()
});
```

using `vm` in place of `visitor.vm` when building the `Diagnostic`.

- [ ] **Step 5: Run the tests**

Run: `cargo test-windows -p vmlord-core diagnostics`
Expected: PASS — eight tests.

- [ ] **Step 6: Commit**

```bash
git add crates/core/src/diagnostics.rs
git commit -m "TASK-8: Take a record's VM from the operation it happened inside"
```

---

### Task 5: A `RepositoryError` that carries its context

**Files:**
- Create: `crates/core/src/error.rs`
- Modify: `crates/core/src/lib.rs` (remove `RepositoryError`, add `mod error;` and the re-export)
- Modify: `crates/platform/src/error.rs`

**Interfaces:**
- Produces:
  - `RepositoryError::new(message: impl Into<String>) -> Self`
  - `RepositoryError::windows(operation: &'static str, vm: Option<&str>, code: u32, message: impl Into<String>) -> Self`
  - `RepositoryError::vm(&self) -> Option<&str>`, `RepositoryError::code(&self) -> Option<u32>`
  - `platform::error::hresult_to_repository_error(operation: &'static str, vm_name: Option<&str>, hresult: i32) -> RepositoryError` — the `operation` parameter tightens from `&str` to `&'static str`.

- [ ] **Step 1: Write the failing tests**

Create `crates/core/src/error.rs` with a `tests` module:

```rust
#[cfg(test)]
mod tests {
    use super::RepositoryError;

    #[test]
    fn a_windows_failure_reads_exactly_as_it_always_has() {
        // The text is load-bearing: a person reads it, and the platform layer
        // asserts it. Structuring the error must not restyle it.
        let error = RepositoryError::windows(
            "open compute system",
            Some("dev-linux"),
            0x8007_0005,
            "",
        );

        assert_eq!(
            error.to_string(),
            "Windows API operation \"open compute system\" for VM \"dev-linux\" \
             failed (HRESULT 0x80070005)"
        );
    }

    #[test]
    fn windows_own_description_is_appended_when_there_is_one() {
        let error = RepositoryError::windows(
            "open compute system",
            Some("dev-linux"),
            0x8007_0005,
            "Access is denied.",
        );

        assert_eq!(
            error.to_string(),
            "Windows API operation \"open compute system\" for VM \"dev-linux\" \
             failed (HRESULT 0x80070005): Access is denied."
        );
    }

    #[test]
    fn a_failure_with_no_vm_names_none() {
        let error = RepositoryError::windows("open the host network service", None, 0x8007_0005, "");

        assert_eq!(
            error.to_string(),
            "Windows API operation \"open the host network service\" failed (HRESULT 0x80070005)"
        );
    }

    #[test]
    fn the_context_is_readable_as_fields_and_not_only_as_prose() {
        // The whole point: an error that surfaced three layers up can still be
        // recorded with its VM and its code as fields.
        let error = RepositoryError::windows(
            "attach the endpoint",
            Some("dev-linux"),
            0x803B_0014,
            "",
        );

        assert_eq!(error.vm(), Some("dev-linux"));
        assert_eq!(error.code(), Some(0x803B_0014));
    }

    #[test]
    fn an_error_with_no_windows_behind_it_says_so_by_having_no_code() {
        let error = RepositoryError::new("a password must not be empty");

        assert_eq!(error.to_string(), "a password must not be empty");
        assert_eq!(error.code(), None);
        assert_eq!(error.vm(), None);
    }
}
```

- [ ] **Step 2: Run and watch it fail**

Add `mod error;` to `crates/core/src/lib.rs`, then run: `cargo test-windows -p vmlord-core error`
Expected: FAIL — `RepositoryError::windows` does not exist.

- [ ] **Step 3: Write the error**

At the top of `crates/core/src/error.rs`:

```rust
//! The error every repository operation fails with, and the context it keeps.
//!
//! The context is fields rather than prose because an error is usually
//! recorded far from where it was raised: by then the VM name and the Windows
//! code are the only things that let a reader find the operation in the log,
//! and a formatted sentence has already thrown them away.

use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryError {
    message: String,
    vm: Option<String>,
    /// The Windows call that failed. `&'static str` because these are literals
    /// at every site, and an owned string would be allocation for nothing.
    operation: Option<&'static str>,
    /// An HRESULT, unsigned. Win32 statuses are widened before they get here.
    ///
    /// A plain `u32` rather than a `windows` type: this crate does not depend
    /// on `windows`, and the conversion belongs in the layer that does.
    code: Option<u32>,
}

impl RepositoryError {
    /// An error with no Windows call behind it: a rejected request, a backend
    /// that does not support an operation, a picker that is not there.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            vm: None,
            operation: None,
            code: None,
        }
    }

    /// A failed Windows call.
    ///
    /// The code is positional and not optional, which is the point: an HRESULT
    /// that a caller could forget is an HRESULT that gets forgotten.
    #[must_use]
    pub fn windows(
        operation: &'static str,
        vm: Option<&str>,
        code: u32,
        message: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            vm: vm.map(ToString::to_string),
            operation: Some(operation),
            code: Some(code),
        }
    }

    #[must_use]
    pub fn vm(&self) -> Option<&str> {
        self.vm.as_deref()
    }

    #[must_use]
    pub fn code(&self) -> Option<u32> {
        self.code
    }
}

impl fmt::Display for RepositoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(operation) = self.operation else {
            return formatter.write_str(&self.message);
        };
        write!(formatter, "Windows API operation \"{operation}\"")?;
        if let Some(vm) = &self.vm {
            write!(formatter, " for VM \"{vm}\"")?;
        }
        write!(
            formatter,
            " failed (HRESULT 0x{:08X})",
            self.code.unwrap_or_default()
        )?;
        if !self.message.is_empty() {
            write!(formatter, ": {}", self.message)?;
        }
        Ok(())
    }
}

impl std::error::Error for RepositoryError {}
```

- [ ] **Step 4: Move it out of `lib.rs`**

Delete the `RepositoryError` struct, its `impl` blocks and its `Display`/`Error` impls from `crates/core/src/lib.rs` (lines 233-254), and add beside the other re-exports:

```rust
pub use error::RepositoryError;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test-windows -p vmlord-core error`
Expected: PASS — five tests.

- [ ] **Step 6: Rewrite the platform side**

Replace the body of `crates/platform/src/error.rs`'s two functions:

```rust
#[must_use]
pub fn hresult_to_repository_error(
    operation: &'static str,
    vm_name: Option<&str>,
    hresult: i32,
) -> RepositoryError {
    RepositoryError::windows(operation, vm_name, hresult as u32, "")
}

/// Converts a `windows-rs` error while retaining the failed operation and VM.
///
/// Windows' own description of the HRESULT is included: an HCS error code
/// alone ("0x8037010D") names nothing a reader can act on, and looking one up
/// takes a table that is not in this repository.
#[must_use]
pub(crate) fn windows_error(
    operation: &'static str,
    vm_name: Option<&str>,
    error: Error,
) -> RepositoryError {
    RepositoryError::windows(
        operation,
        vm_name,
        error.code().0 as u32,
        error.message(),
    )
}
```

The existing tests in that file assert the rendered text and stay as they are.

- [ ] **Step 7: Compile and fix the `&'static str` fallout**

Run: `cargo check-windows`
Expected: a handful of errors where an operation name is built with `format!` rather than written as a literal. Fix each by writing the literal and moving the varying part into the message, which is where it belonged: an operation is a Windows call, not a sentence.

- [ ] **Step 8: Run the suite**

Run: `cargo test-windows`
Expected: PASS.

- [ ] **Step 9: Commit**

```bash
git add crates/core/src/error.rs crates/core/src/lib.rs crates/platform/src/error.rs
git commit -m "TASK-8: Keep a repository error's VM and code as fields"
```

---

### Task 6: Spans on the repository's entry points

**Files:**
- Modify: `crates/platform/src/repository.rs`

**Interfaces:**
- Consumes: the span fallback from Task 4.
- Produces: nothing callable; every `VmRepository` method on `HcsVmRepository` that names a VM opens a span carrying it.

- [ ] **Step 1: Open the spans**

In `crates/platform/src/repository.rs`, at the head of each of `create_vm`, `update_vm`, `start_vm`, `stop_vm`, `force_stop_vm`, `delete_vm`, `cancel_create`, `open_display`, `update_display_payload`, `open_ssh` and `open_console`:

```rust
let _span = tracing::info_span!("start_vm", vm = name).entered();
```

with the span's name matching the method and `vm` bound to whatever that method calls the VM (`name`, `request.name.as_str()`, and so on). `create_vm` and `delete_vm` take a request:

```rust
let _span = tracing::info_span!("create_vm", vm = request.name.as_str()).entered();
```

- [ ] **Step 2: Compile**

Run: `cargo check-windows`
Expected: PASS.

- [ ] **Step 3: Run the platform suite**

Run: `cargo test-windows -p vmlord-platform`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/platform/src/repository.rs
git commit -m "TASK-8: Name the VM once per operation, on its span"
```

---

### Task 7: Move the platform's diagnostics onto the macro

**Files:**
- Modify: `crates/platform/src/repository.rs`
- Modify: `crates/platform/src/display_launches.rs`

**Interfaces:**
- Consumes: `diagnostic!` and `Subsystem` from Task 3.
- Produces: `HcsVmRepository` no longer has a `diagnostics` field, a `push_diagnostic` method, or a `push_shared_diagnostic` free function. `display_launches::Diagnostics` is gone.

- [ ] **Step 1: Rewrite the push sites**

Each `self.push_diagnostic(DiagnosticLevel::X, format!(...))` and each `push_shared_diagnostic(&diagnostics, DiagnosticLevel::X, message)` becomes a `diagnostic!` call with the subsystem the site belongs to. For example, `repository.rs:361`:

```rust
Err(error) => {
    tracing::warn!(
        "VM \"{}\" is being shut down without its console: {error}",
        mapping.vm_name
    );
    vmlord_core::diagnostic!(
        Warning,
        Subsystem::Display,
        vm = mapping.vm_name.as_str(),
        "VM \"{}\" is being shut down, but its COM1 console could not be \
         opened to show it: {error}",
        mapping.vm_name
    );
}
```

and `repository.rs:459`, the SSH invocation:

```rust
Ok(invocation) => vmlord_core::diagnostic!(
    Info,
    Subsystem::Network,
    vm = mapping.vm_name.as_str(),
    "SSH session for VM \"{}\": {}",
    mapping.vm_name,
    invocation.command_line()
),
```

Subsystem per site: compute-system lifecycle is `Hcs`; endpoints, subnets, DHCP and SSH are `Network`; the host network service itself is `Hns`; GPU assignment and exports are `Gpu`; display sessions, payload updates and the COM1 console are `Display`; seed, cloud-init and guest readiness are `Provisioning`; image download is `Image`.

Where the site has a `RepositoryError` in hand, pass its code through:

```rust
vmlord_core::diagnostic!(
    Error,
    Subsystem::Hcs,
    vm = name,
    code = error.code().unwrap_or_default(),
    "{error}"
);
```

- [ ] **Step 2: Delete the buffer**

Remove the `diagnostics: Arc<Mutex<Vec<Diagnostic>>>` field from `HcsVmRepository` (`repository.rs:113`) and every clone of it handed to a worker, the `push_diagnostic` method (`repository.rs:478`), the `push_shared_diagnostic` function (`repository.rs:1873`), the `console_failure_diagnostics` helper's `Vec<Diagnostic>` return (it emits instead), and the `type Diagnostics` alias in `display_launches.rs:46` with the parameters that carried it.

- [ ] **Step 3: Compile**

Run: `cargo check-windows`
Expected: PASS once every worker that took a `Diagnostics` clone has had that parameter removed.

- [ ] **Step 4: Update the tests that assert diagnostics**

The roughly ten assertions in `repository.rs`'s own `tests` module read `take_diagnostics`. Wrap each in a scoped subscriber and read a sink:

```rust
fn with_sink<T>(body: impl FnOnce() -> T) -> (T, Vec<vmlord_core::Diagnostic>) {
    let sink = vmlord_core::DiagnosticsSink::new();
    let subscriber = tracing_subscriber::registry()
        .with(vmlord_core::DiagnosticsLayer::new(sink.clone()));
    let value = tracing::subscriber::with_default(subscriber, body);
    (value, sink.take())
}
```

with `tracing-subscriber` added to `crates/platform`'s `[dev-dependencies]`.

- [ ] **Step 5: Run the platform suite**

Run: `cargo test-windows -p vmlord-platform`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/platform
git commit -m "TASK-8: Report the platform's findings as marked events"
```

---

### Task 8: `take_diagnostics` becomes `refresh`

**Files:**
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/platform/src/repository.rs`
- Modify: `crates/app/src/lib.rs`
- Modify: `crates/vmlord/src/main.rs`
- Modify: `crates/app/tests/update_vm.rs`, `crates/platform/tests/hyperv.rs`, `crates/platform/tests/gpu_e2e.rs`

**Interfaces:**
- Produces: `VmRepository::refresh(&mut self)`, replacing `take_diagnostics`. `WorkspaceApp::diagnostics(&self) -> &[Diagnostic]` keeps its signature. `WorkspaceApp::with_diagnostics(self, sink: DiagnosticsSink) -> Self` is new.

- [ ] **Step 1: Rename in the trait**

In `crates/core/src/lib.rs:341`:

```rust
/// Reaps what background work has finished, on the one `&mut self` call the
/// application makes every refresh.
///
/// Named for what it does rather than for what it used to return: finished
/// builds and starts are adopted here, answered shutdowns give up their
/// handles, desktops that appeared are written down, and HCS events are
/// drained. Diagnostics no longer come back from it -- they are recorded as
/// events on the way through.
fn refresh(&mut self);
```

- [ ] **Step 2: Rename in the platform**

In `crates/platform/src/repository.rs:1640`, rename the method and drop its tail: the `let mut diagnostics: Vec<Diagnostic> = ...` collection, the two `extend` calls and the trailing `diagnostics` expression go. `watch::drain_events`' diagnostics and `console_failure_diagnostics` emit through `diagnostic!` instead, as Task 7 left them.

- [ ] **Step 3: Read the sink in the application**

In `crates/app/src/lib.rs`, replace the `diagnostics: Vec<Diagnostic>` field's maintenance:

```rust
/// Where the diagnostics layer leaves records, and the records already read
/// out of it.
///
/// Kept rather than drained straight to the UI because the panel shows a
/// history: `take` empties the sink, so what it returns has to be held here.
diagnostics: Vec<Diagnostic>,
sink: Option<DiagnosticsSink>,
```

and replace `collect_diagnostics` (`app/src/lib.rs:737`) with:

```rust
fn collect_diagnostics(&mut self) {
    let Some(sink) = &self.sink else {
        return;
    };
    self.diagnostics.extend(sink.take());
    const MAX_DIAGNOSTICS: usize = 100;
    if self.diagnostics.len() > MAX_DIAGNOSTICS {
        self.diagnostics
            .drain(..self.diagnostics.len() - MAX_DIAGNOSTICS);
    }
}
```

and add:

```rust
#[must_use]
pub fn with_diagnostics(mut self, sink: DiagnosticsSink) -> Self {
    self.sink = Some(sink);
    self
}
```

Every `self.repository.take_diagnostics()` call becomes `self.repository.refresh()`, and every direct `self.diagnostics.push(Diagnostic { ... })` in `app` is Task 9's business — leave them for now.

- [ ] **Step 4: Wire the composition root**

In `crates/vmlord/src/main.rs`, replace `vmlord_core::initialize_logging(settings)` with `vmlord_core::initialize_with_diagnostics(settings)`, keep the returned sink, and hand it to the application:

```rust
application = application.with_diagnostics(sink);
```

`crates/vmlord/src/bin/vmlord-com1.rs` and `crates/display-viewer/src/log.rs` keep calling `initialize_logging` and `initialize_logging_without_console`: neither has a panel to show diagnostics in.

- [ ] **Step 5: Update the test doubles**

`crates/app/src/lib.rs:790`, `crates/app/src/lib.rs:977` and `crates/app/tests/update_vm.rs:73` each implement `take_diagnostics` on a fake repository. Rename each to `fn refresh(&mut self) {}`, dropping the returned vector.

`crates/platform/tests/hyperv.rs` (lines 169, 224, 322, 455) and `crates/platform/tests/gpu_e2e.rs:187` call it for the list. Each becomes `repository.refresh()` inside a `with_default` subscriber, reading a `DiagnosticsSink` as in Task 7 Step 4.

- [ ] **Step 6: Compile and run**

Run: `cargo check-windows` then `cargo test-windows`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add -A
git commit -m "TASK-8: Rename the repository's reaping call to what it does"
```

---

### Task 9: Move the application's diagnostics onto the macro

**Files:**
- Modify: `crates/app/src/lib.rs`

**Interfaces:**
- Consumes: `diagnostic!` from Task 3, the sink from Task 8.
- Produces: `WorkspaceApp` no longer constructs a `Diagnostic` anywhere.

- [ ] **Step 1: Rewrite the push sites**

Each of the roughly twenty `self.diagnostics.push(Diagnostic { level, message })` calls becomes a `diagnostic!`. For `update_settings` (`app/src/lib.rs:218`):

```rust
vmlord_core::diagnostic!(Info, Subsystem::App, "Application settings saved");
```

For a site that reports a failed operation on a named VM:

```rust
vmlord_core::diagnostic!(
    Error,
    Subsystem::App,
    vm = name.as_str(),
    code = error.code().unwrap_or_default(),
    "{error}"
);
```

Because these run on the UI thread and the sink is read on the next refresh, follow each with `self.collect_diagnostics();` where the code already called it — the existing calls are in the right places.

- [ ] **Step 2: Compile**

Run: `cargo check-windows`
Expected: PASS. `Diagnostic` should no longer be constructed in `app`; it remains imported for the `diagnostics()` return type.

- [ ] **Step 3: Run the app suite**

Run: `cargo test-windows -p vmlord-app`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/app
git commit -m "TASK-8: Report the application's findings as marked events"
```

---

### Task 10: Show the new fields

**Files:**
- Modify: `crates/ui/src/lib.rs:2173`

**Interfaces:**
- Consumes: `Diagnostic`'s fields from Task 3.

- [ ] **Step 1: Render the record**

Replace `render_diagnostics`' body:

```rust
fn render_diagnostics(ui: &mut egui::Ui, diagnostics: &[vmlord_core::Diagnostic]) {
    ui.collapsing("Log", |ui| {
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for diagnostic in diagnostics {
                    let color = match diagnostic.level {
                        DiagnosticLevel::Info => egui::Color32::LIGHT_GRAY,
                        DiagnosticLevel::Warning => egui::Color32::YELLOW,
                        DiagnosticLevel::Error => egui::Color32::LIGHT_RED,
                    };
                    ui.colored_label(color, diagnostic_line(diagnostic));
                }
            });
    });
}

/// One record as the panel shows it.
///
/// The stamp comes first because the panel's whole use is lining a moment up
/// with the same moment in `vmlord.log`, and the code comes last because it is
/// what a reader copies into a search.
fn diagnostic_line(diagnostic: &vmlord_core::Diagnostic) -> String {
    let mut line = format!(
        "[{}] {}",
        vmlord_core::format_timestamp(diagnostic.at),
        diagnostic.message
    );
    if let Some(vm) = &diagnostic.vm {
        line.push_str(&format!(" ({vm})"));
    }
    if let Some(code) = diagnostic.code {
        line.push_str(&format!(" [0x{code:08X}]"));
    }
    line
}
```

- [ ] **Step 2: Export the timestamp**

`timestamp` in `crates/core/src/logging.rs` is private. Rename nothing; add a public wrapper in `crates/core/src/lib.rs`:

```rust
/// A moment as VMLord spells it everywhere: `1970-01-01T00:00:00.000Z`.
///
/// Public so the UI stamps a record the same way the log file does -- two
/// spellings of one moment would defeat the point of showing it.
#[must_use]
pub fn format_timestamp(at: std::time::SystemTime) -> String {
    logging::timestamp(at)
}
```

making `timestamp` `pub(crate)`.

- [ ] **Step 3: Write the test**

Add to `crates/ui/src/lib.rs`'s `tests` module:

```rust
#[test]
fn a_record_is_shown_with_its_moment_its_vm_and_its_code() {
    // The panel exists to be lined up against `vmlord.log`; without the stamp
    // there is nothing to line up.
    let line = super::diagnostic_line(&vmlord_core::Diagnostic {
        level: vmlord_core::DiagnosticLevel::Error,
        subsystem: vmlord_core::Subsystem::Hcs,
        vm: Some("dev-linux".into()),
        code: Some(0x803B_0014),
        at: std::time::UNIX_EPOCH,
        message: "the endpoint was already attached".into(),
    });

    assert_eq!(
        line,
        "[1970-01-01T00:00:00.000Z] the endpoint was already attached \
         (dev-linux) [0x803B0014]"
    );
}
```

- [ ] **Step 4: Run it**

Run: `cargo test-windows -p vmlord-ui`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/ui crates/core
git commit -m "TASK-8: Show a diagnostic's moment, VM and code"
```

---

### Task 11: Redaction

Reading the code first changes this task. The rule the spec states is already
kept, and kept the way the spec asks for: `VmKeyPair` (`crates/keys/src/lib.rs:19`)
has no `Debug` "by design", `Seed` (`crates/seed/src/lib.rs:59`) has none
because `user_data` holds the password hash, `SeedRequest` has none, and
`auth::Secret` (`crates/agent-protocol/src/auth.rs:53`) has none. Each says so
in a comment.

So there are no redacting `Debug` implementations to write. What is missing is
what makes the rule survive: a test that fails when somebody gives one of those
types a `Debug`, and the rule written down where it can be read before the
mistake rather than after. The written half is Task 12.

**Files:**
- Modify: `crates/seed/src/user_data.rs`
- Modify: `crates/seed/Cargo.toml` (`[dev-dependencies]`)

**Interfaces:**
- Consumes: the capture layer from Task 3 Step 9.

- [ ] **Step 1: Write the failing test**

The end-to-end assertion is the one worth having: run the real document build
with a real hash and a real agent secret, and read back everything it recorded.
`request()` at `crates/seed/src/user_data.rs:344` already returns a
`SeedRequest<'static>` with a `$6$` hash in it.

Add to `crates/seed/src/user_data.rs`'s `tests` module:

```rust
#[test]
fn building_the_documents_records_no_secret() {
    // `build` logs what it built. This test is what stops that line growing a
    // value: the hash and the agent secret must not be in any record, and a
    // `Debug` added to `SeedRequest` or `Seed` would put them there.
    let records = capture::Records::default();
    let subscriber = tracing_subscriber::registry().with(capture::Capture(records.clone()));

    let mut request = request();
    request.agent_secret = Some("c2VjcmV0LWFnZW50LXRva2Vu");
    tracing::subscriber::with_default(subscriber, || {
        let _seed = crate::build(&request);
    });

    let text = records.text();
    assert!(!text.contains("$6$"), "no crypt entry may be recorded: {text}");
    assert!(
        !text.contains("c2VjcmV0LWFnZW50LXRva2Vu"),
        "no agent secret may be recorded: {text}"
    );
}
```

with the capture module from Task 3 Step 9 lifted into
`crates/seed/src/user_data.rs`'s `tests` module verbatim -- it is a dozen lines
and a shared test-only crate for it would be a dependency nobody else wants.

- [ ] **Step 2: Add the dev-dependency**

In `crates/seed/Cargo.toml`, under `[dev-dependencies]`:

```toml
tracing-subscriber = { workspace = true }
```

- [ ] **Step 3: Run and watch it pass**

Run: `cargo test-windows -p vmlord-seed building_the_documents_records_no_secret`
Expected: PASS. This test guards rather than drives: it passes on the first run
because the discipline is already kept, and it is here so that it stops passing
the moment somebody breaks it.

- [ ] **Step 4: Prove the test can fail**

Temporarily add `#[derive(Debug)]` to `SeedRequest` and change `build`'s
`tracing::debug!` line to record `?request`. Rerun the test.
Expected: FAIL, naming the crypt entry. Revert both edits and rerun.
Expected: PASS.

A guard test nobody has seen fail is a guard test nobody knows works.

- [ ] **Step 5: Commit**

```bash
git add crates/seed
git commit -m "TASK-8: Hold the line that keeps secrets out of records"
```

---

### Task 12: Documentation

**Files:**
- Modify: `ARCHITECTURE.md`
- Modify: `AGENTS.md`

- [ ] **Step 1: Rewrite the migration section**

`ARCHITECTURE.md:3612` says diagnostics are the remaining migration work. Replace it: the migration is finished, and say what replaced what — `tracing` as the facade, two layers, the diagnostics sink, and `refresh` in place of `take_diagnostics`.

- [ ] **Step 2: Update the diagnostics passages**

The passages at `ARCHITECTURE.md:423`, `:626`, `:1631`, `:2099` and `:2142` describe the diagnostics buffer and `take_diagnostics` by name. Rewrite each to describe the sink and `refresh`.

- [ ] **Step 3: Write down the redaction rule**

Add a section to `ARCHITECTURE.md`, beside the existing account of the display's pixel rule:

```markdown
### What never reaches a record

A secret has neither a `Display` nor a `Debug` that shows its value. `tracing`
records a field only through one of those two traits, so a secret does not
compile into a field at all: the guarantee comes from the compiler rather than
from anybody's vigilance.

This covers the account password, the private half of a VM's key pair, the
agent secret, and the cloud-init user-data document as a whole -- it carries
the password's crypt entry. There is no pattern-matching scrubber and there
should not be one: a leak can only come from our own code, and a filter would
cost something on every record while promising more than it can keep.
```

- [ ] **Step 4: Note the facade in the development rules**

Add to `AGENTS.md` under Code Style:

```markdown
* Log through `tracing`, not `log`. The `log` crate remains a dependency only
  so that records from `eframe` and other dependencies still reach the file.
* An event meant for the user goes through `vmlord_core::diagnostic!`, which
  marks it for the diagnostics panel. Ordinary `info!` and `warn!` reach the
  log file alone.
```

- [ ] **Step 5: Commit**

```bash
git add ARCHITECTURE.md AGENTS.md
git commit -m "TASK-8: Record how diagnostics work now"
```

---

## Final verification

- [ ] Run `cargo check-windows` — expect PASS.
- [ ] Run `cargo test-windows` — expect PASS.
- [ ] Confirm `grep -rn 'log::\(error\|warn\|info\|debug\|trace\)!' --include='*.rs' crates` returns nothing.
- [ ] Confirm `grep -rn 'take_diagnostics' --include='*.rs' crates` returns nothing.
- [ ] Confirm `grep -rn 'push_shared_diagnostic\|push_diagnostic' --include='*.rs' crates` returns nothing.
- [ ] Confirm `VmKeyPair`, `Seed`, `SeedRequest` and `auth::Secret` still have no `Debug`.
