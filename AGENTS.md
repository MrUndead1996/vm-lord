# AGENTS.md

# VMLord

This document defines the development rules for contributors and AI coding agents.

For architecture details, see **ARCHITECTURE.md**.

## Project Status

VMLord is Rust-native. The AppSandbox C backend it started from, its FFI layer
and the `vmlord-legacy-backend` crate have been removed from the distribution.

## Development Principles

* Implement all application code in Rust.
* Do not reintroduce C code or an FFI layer for backend work.
* Prefer native Windows APIs over PowerShell, WMI or external processes.
* Isolate all `unsafe` code inside platform-specific modules.

## UI Rules

* The UI must not contain business logic.
* The UI must never call Windows APIs directly.
* The UI communicates only with the application layer.
* New user-facing text in the UI goes through `t!` and is added to both
  catalogues under `crates/ui/locales/`.

## Code Style

Prefer:

* simple solutions
* explicit code
* small modules
* descriptive names
* idiomatic Rust

Log through `tracing`, not `log`. The `log` crate remains a dependency only so
that records from `eframe` and other dependencies still reach the file.

An event meant for the user goes through `vmlord_core::diagnostic!`, which marks
it for the diagnostics panel and names its subsystem. Ordinary `info!` and
`warn!` reach the log file alone. A secret must have neither a `Display` nor a
`Debug` that shows its value -- see **ARCHITECTURE.md**, "What never reaches a
record".

Avoid:

* unnecessary abstractions
* traits with a single implementation
* large architectural rewrites unless explicitly requested

## AppSandbox references

AppSandbox's sources are no longer part of this repository, but the code and
**ARCHITECTURE.md** still cite them where they recorded Windows behaviour worth
keeping. Treat those citations as history: they explain a decision, they do not
describe anything VMLord ships.

## Documentation

Keep documentation up to date.

Update **ARCHITECTURE.md** whenever architectural decisions change.

## Workflow

* Complete each task in a dedicated branch.
* Commit the completed work before pushing the branch to the remote repository.
* Open a merge request only after receiving explicit user approval.
* Assign every merge request to `mrundead`.

## Commits

Prefix every commit subject with the task number when it is known.

Format commit subjects as `TASK-<No>: comment`.

Examples:

* `TASK-2: Add VM update workflow`
* `TASK-15: Refine SSH connection errors`

## Decision Priority

When multiple solutions are possible, prefer:

1. Correctness
2. Simplicity
3. Maintainability
4. Native Windows integration
5. Performance

## Commands

Use the Cargo aliases from `.cargo/config.toml` rather than spelling targets out:

* `cargo check-windows` — compile-check the Windows application from WSL.
* `cargo test-windows` — build and run the Windows tests from WSL; they execute
  through WSL interop, so no Wine is involved.
* `cargo agent` / `cargo agent-release` — build the Linux guest agent
  (`x86_64-unknown-linux-musl`, statically linked, no C toolchain needed).
* `cargo display-services` — build the guest display broker and capture process
  (the same target, for the same reason).
* `cargo display-bench` — run the desktop codec's benchmark scenes. With
  `--raw <file> --width <w> --height <h>` it measures a recording of a real
  desktop through the same table instead — see `--record` below, and
  **ARCHITECTURE.md**, "What a real desktop costs", for why the synthetic
  scenes are not enough on their own.
* `cargo display-pipeline-bench` — measure what a captured frame costs
  between the mapping and the socket: the cursor composite, the payload's
  copy into a record, and a mapped buffer against memory this process owns.
  Linux only, because the mapped rows want a real descriptor.
* `vmlord-display-guest-probe` — built by `cargo display-services` and run
  inside a guest under `sudo`. Reports what a real dma-buf's coherency call
  costs and runs the desktop that is on screen through the pipeline, which
  is the part `cargo display-pipeline-bench` cannot reach from a host. Its
  `--damage` pass encodes the live desktop twice at once, trusting the
  compositor's `FB_DAMAGE_CLIPS` and comparing every tile, and checks that the
  two viewers end up with the same picture — the only way to find out whether
  a compositor's damage can be believed, since damage that misses a change is
  an error nothing reports. `--damage 0` skips it, which a guest with a still
  screen wants. `--record <file>` instead writes committed frames of the live
  desktop, packed, for `cargo display-bench --raw` to measure on the host; it
  takes the first frame whatever the compositor committed, so a still desktop
  yields the one frame it has rather than nothing. It is not installed by the
  display payload; copy it over by hand.
* `cargo dist` — Windows-only release build into `dist/`.

Never add a dependency that forces the agent to link against system C
libraries without raising it first: it would cost the toolchain-free
cross-compilation described in **ARCHITECTURE.md**.
