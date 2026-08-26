# Host Display Modes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish the current Windows monitor's supported resolution/refresh modes in the guest DRM connector, allow explicit viewer selection, and warn when delivered FPS remains below the configured share of DRM refresh.

**Architecture:** A shared protocol `DisplayTiming` value crosses viewer control, guest broker, and DRM module boundaries. The Windows layer produces a normalized monitor snapshot, the viewer owns selection and persistence, and the broker validates and atomically applies bounded mode lists. A viewer-side state machine measures presented frames against confirmed DRM refresh and reports one-shot warnings through the existing launch pipe.

**Tech Stack:** Rust 2024, prost/protobuf, windows-rs Win32 APIs, Linux DRM helper APIs, eframe/egui settings UI, Cargo aliases, Kbuild/DKMS payload builds.

**Spec:** `docs/superpowers/specs/2026-08-26-host-display-modes-design.md`

## Global Constraints

- Modes are `width`, `height`, and integer `refresh_hz`; do not pass or synthesize EDID.
- Accept geometry only in 640x480 through 2560x1440 and refresh only in 1 through 144 Hz.
- Prefer 1920x1080@60; otherwise use greatest resolution then greatest refresh; synthesize 1920x1080@60 only when no valid host mode exists.
- Preserve `SetResolution` compatibility and make host mode lists an append-only negotiated capability.
- Keep framebuffer geometry as truth; a request is not a committed mode.
- `fps_gap_threshold_percent` defaults to 50 and validates in 1 through 100.
- New UI text uses `t!` and exists in both `crates/ui/locales/en-US.toml` and `crates/ui/locales/ru-RU.toml`.
- User diagnostics go through `vmlord_core::diagnostic!`; ordinary samples use `tracing`.
- Keep all Win32 and kernel `unsafe` code inside existing platform-specific modules.
- Add no dependency that links the Linux guest binaries against a system C library.

---

### Task 1: Display settings and launch contract

**Files:**
- Modify: `crates/core/src/settings.rs`
- Modify: `crates/core/src/lib.rs`
- Modify: `crates/app/src/lib.rs`
- Modify: `crates/ui/src/lib.rs`
- Modify: `crates/ui/locales/en-US.toml`
- Modify: `crates/ui/locales/ru-RU.toml`
- Modify: `crates/display-viewer/proto/vmlord/display/viewer/viewer.proto`
- Modify: `crates/display-viewer/src/launch.rs`
- Modify: `crates/platform/src/display_session.rs`
- Modify: `crates/platform/src/display_launches.rs`
- Test: inline unit tests in the files above

**Interfaces:**
- Produces: `DisplaySettings { fps_gap_threshold_percent: u8 }`, `DisplaySettings::validate()`, and `LaunchParameters::fps_gap_threshold_percent: u8`.
- Consumes: existing `AppSettings`, `SettingsForm`, launch protobuf envelope, and `LaunchRequest` flow.

- [x] **Step 1: Write failing settings tests**

Add tests proving an old TOML document defaults to 50, values 1 and 100 validate, 0 and 101 fail, and the settings form round-trips the display section. Add a launch round-trip assertion for `fps_gap_threshold_percent: 50`.

```rust
#[test]
fn display_fps_gap_threshold_defaults_to_half() {
    assert_eq!(DisplaySettings::default().fps_gap_threshold_percent, 50);
}

#[test]
fn display_fps_gap_threshold_is_a_percentage() {
    assert!(DisplaySettings { fps_gap_threshold_percent: 1 }.validate().is_ok());
    assert!(DisplaySettings { fps_gap_threshold_percent: 100 }.validate().is_ok());
    assert!(DisplaySettings { fps_gap_threshold_percent: 0 }.validate().is_err());
    assert!(DisplaySettings { fps_gap_threshold_percent: 101 }.validate().is_err());
}
```

- [x] **Step 2: Run focused tests and verify RED**

Run: `cargo test -p vmlord-core settings` and `cargo test -p vmlord-display-viewer launch`

Expected: FAIL because `DisplaySettings` and the launch field do not exist.

- [x] **Step 3: Implement settings, validation, UI, and launch propagation**

Add a serde-defaulted `display: DisplaySettings` table to `AppSettings`, validate it during save/load, expose a percentage widget in the settings dialog, translate its label/help/error, bump the private launch `REVISION`, and carry the value through repository → driver → `LaunchRequest` → `LaunchParameters`.

- [x] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test -p vmlord-core settings && cargo test -p vmlord-app && cargo test -p vmlord-display-viewer launch`

- [x] **Step 5: Commit**

Run: `git add crates/core crates/app crates/ui crates/display-viewer/proto crates/display-viewer/src/launch.rs crates/platform && git commit -m "TASK-136: Configure display FPS diagnostics"`

---

### Task 2: Versioned display-mode protocol

**Files:**
- Modify: `crates/display-protocol/proto/vmlord/display/v1/display.proto`
- Modify: `crates/display-protocol/src/record.rs`
- Modify: `crates/display-protocol/src/session.rs`
- Modify: `crates/display-protocol/tests/compatibility.rs`
- Modify: `crates/display-protocol/tests/fuzz.rs`
- Modify: `crates/display-protocol/tests/golden.rs`
- Modify: `crates/display-protocol/tests/malformed.rs`
- Regenerate: `crates/display-protocol/proto/display.descriptor.bin`
- Regenerate: `crates/display-protocol/tests/golden/handshake.bin`
- Regenerate: `crates/display-protocol/tests/golden/records.bin`

**Interfaces:**
- Produces: protobuf `DisplayTiming`, `SetAvailableModes`, `SetDisplayMode`; `CAPABILITY_HOST_DISPLAY_MODES`; control records 13 and 14; active `refresh_hz` in `DisplayState`.
- Consumes: existing protocol negotiation, `SetResolution`, record size limits, checked-in descriptor and golden fixtures.

- [x] **Step 1: Write failing compatibility and malformed-input tests**

Assert exact round trips for `(2560, 1440, 144)`, repeated modes with preferred, explicit selection, rejection of missing/zero fields by the semantic decoder, and successful negotiation with an old minor peer that lacks the capability.

```rust
let timing = DisplayTiming { width: 2560, height: 1440, refresh_hz: 144 };
let update = SetAvailableModes {
    modes: vec![timing.clone()],
    preferred: Some(timing),
};
assert_eq!(SetAvailableModes::decode(update.encode_to_vec().as_slice()).unwrap(), update);
```

- [x] **Step 2: Run protocol tests and verify RED**

Run: `cargo test -p vmlord-display-protocol`

Expected: compile failure because new schema types and enum values are absent.

- [x] **Step 3: Add append-only schema and negotiation support**

Bump protocol minor, append capability/records/messages without renumbering existing values, extend `DisplayState` with `refresh_hz`, and ensure negotiated capabilities gate both new outbound records.

- [x] **Step 4: Refresh checked-in protocol artifacts and verify GREEN**

Run: `VMLORD_REFRESH_DESCRIPTOR=1 VMLORD_REFRESH_GOLDEN=1 cargo test -p vmlord-display-protocol`

Then run again without refresh variables and require a clean pass.

- [x] **Step 5: Commit**

Run: `git add crates/display-protocol && git commit -m "TASK-136: Add host modes to display protocol"`

---

### Task 3: Pure host-mode policy and persisted selection

**Files:**
- Create: `crates/display-viewer/src/display_modes.rs`
- Modify: `crates/display-viewer/src/lib.rs`
- Modify: `crates/display-viewer/src/state.rs`
- Test: inline tests in `display_modes.rs` and `state.rs`

**Interfaces:**
- Produces: `DisplayMode { width: u32, height: u32, refresh_hz: u32 }`, `normalize_modes`, `fallback_mode`, `select_mode`, and `WindowState::display_mode: Option<DisplayMode>`.
- Consumes: global bounds and preferred fallback defined by the spec.

- [x] **Step 1: Write failing policy tests**

Cover invalid bounds, refresh above 144, CVT geometry alignment, triple-field deduplication, deterministic ordering, retained selection, 1920x1080@60 preference, maximum resolution/refresh fallback, and synthetic fallback on an empty list.

```rust
#[test]
fn fallback_prefers_full_hd_at_sixty() {
    let modes = [mode(2560, 1440, 144), mode(1920, 1080, 60)];
    assert_eq!(fallback_mode(&modes), mode(1920, 1080, 60));
}

#[test]
fn fallback_uses_the_largest_available_when_full_hd_is_absent() {
    let modes = [mode(1280, 720, 60), mode(1600, 900, 75), mode(1600, 900, 120)];
    assert_eq!(fallback_mode(&modes), mode(1600, 900, 120));
}
```

- [x] **Step 2: Run viewer policy tests and verify RED**

Run: `cargo test -p vmlord-display-viewer display_modes`

- [x] **Step 3: Implement the minimal pure policy and state compatibility**

Keep normalization platform-independent. Extend the per-VM state parser with optional `display_width`, `display_height`, and `display_refresh_hz` keys; malformed or partial triples become `None`, preserving old files.

- [x] **Step 4: Run tests and verify GREEN**

Run: `cargo test -p vmlord-display-viewer display_modes && cargo test -p vmlord-display-viewer state`

- [x] **Step 5: Commit**

Run: `git add crates/display-viewer && git commit -m "TASK-136: Define host display mode policy"`

---

### Task 4: Windows monitor enumeration and change detection

**Files:**
- Create: `crates/display-viewer/src/windows/display_modes.rs`
- Modify: `crates/display-viewer/src/windows/mod.rs`
- Modify: `crates/display-viewer/src/windows/window.rs`
- Modify: `crates/display-viewer/Cargo.toml` only if additional existing windows-rs feature gates are required
- Test: pure adapter tests in `windows/display_modes.rs` and window message tests

**Interfaces:**
- Produces: `MonitorSnapshot { identity, current, preferred, modes }`, `snapshot_for_window(HWND)`, and `UiEvent::MonitorChanged`.
- Consumes: `MonitorFromWindow`, `GetMonitorInfoW`, `EnumDisplaySettingsW`, `GetDisplayConfigBufferSizes`, `QueryDisplayConfig`, and `DisplayConfigGetDeviceInfo(GET_TARGET_PREFERRED_MODE)`.

- [x] **Step 1: Write failing conversion and debounce tests**

Test conversion from a plain `RawMode` adapter into normalized modes, preservation of refresh variants, optional preferred lookup failure, and coalescing repeated move/display/DPI messages into one monitor-change event.

- [x] **Step 2: Run Windows compile/tests and verify RED**

Run: `cargo test-windows -p vmlord-display-viewer`

Expected: compile failure for the absent snapshot API/event.

- [x] **Step 3: Implement Win32 enumeration behind the pure adapter**

Enumerate only the `MONITORINFOEXW.szDevice` belonging to the nearest monitor. Retry `QueryDisplayConfig` on `ERROR_INSUFFICIENT_BUFFER`, map its active target to the GDI device name, and treat preferred-mode lookup as optional. Emit only a stale signal from `window_proc`; perform enumeration in the main loop after 250 ms.

- [x] **Step 4: Run Windows tests and verify GREEN**

Run: `cargo test-windows -p vmlord-display-viewer`

- [x] **Step 5: Commit**

Run: `git add crates/display-viewer && git commit -m "TASK-136: Enumerate Windows monitor modes"`

---

### Task 5: Multi-mode DRM module contract

**Files:**
- Modify: `payloads/display/module/vmlord_drm.c`
- Modify: `payloads/display/module/README.md`
- Modify: `payloads/display/README.md`
- Modify: `crates/display-services/src/output.rs`
- Test: inline Rust parser/writer tests and payload module build checks

**Interfaces:**
- Produces: sysfs `modes` parameter containing bounded comma-separated `WIDTHxHEIGHT@HZ`; existing `mode` becomes `WIDTHxHEIGHT@HZ`; Rust `Output::replace_modes(&[DisplayMode])` and `Output::request(DisplayMode)`.
- Consumes: kernel `drm_cvt_mode`, connector hotplug helper, existing geometry bounds and active-device mutex.

- [x] **Step 1: Write failing Rust contract tests**

Cover serialization/parsing, maximum item/byte limits, duplicate rejection, invalid refresh, atomic preservation of the old file on invalid input, and preferred mode inclusion.

```rust
#[test]
fn modes_are_written_with_integer_refresh() {
    let modes = [DisplayMode::new(1920, 1080, 60).unwrap(), DisplayMode::new(2560, 1440, 144).unwrap()];
    output.replace_modes(&modes).unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "1920x1080@60,2560x1440@144\n");
}
```

- [x] **Step 2: Run output tests and verify RED**

Run: `cargo test -p vmlord-display-services output`

- [x] **Step 3: Implement bounded Rust contract and kernel parser/publication**

Use fixed maximum counts and buffers in C, parse into a temporary array before taking the active lock, swap only after full validation, create each probed mode at its refresh, mark the active one preferred, update 96-DPI physical dimensions from it, and hotplug after a successful swap/selection.

- [x] **Step 4: Build supported payload module targets and run Rust tests**

Run: `cargo test -p vmlord-display-services output`

Run the repository's display payload/module build command documented in `payloads/display/README.md` for Ubuntu 22.04, 24.04, and 26.04 headers. If the documented command uses containers or downloads and sandboxing blocks it, request approval rather than replacing it with an unrepresentative host build.

- [x] **Step 5: Commit**

Run: `git add payloads/display crates/display-services/src/output.rs && git commit -m "TASK-136: Publish multiple DRM modes"`

---

### Task 6: Broker validation, application, and committed refresh

**Files:**
- Modify: `crates/display-services/src/control.rs`
- Modify: `crates/display-services/src/broker_main.rs`
- Modify: `crates/display-services/src/drm/mod.rs`
- Modify: `crates/display-services/src/drm/uapi.rs`
- Modify: `crates/display-services/proto/vmlord/display/broker/broker.proto`
- Modify: `crates/display-services/src/channel.rs`
- Test: inline control/broker/DRM tests

**Interfaces:**
- Produces: `Outcome::AvailableModes`, `Outcome::DisplayMode`; broker `Geometry { width, height, refresh_hz }`; control `DisplayState.refresh_hz`.
- Consumes: Task 2 wire messages and Task 5 `Output` methods.

- [x] **Step 1: Write failing broker tests**

Prove capability gating, whole-update rejection on one invalid entry, empty-list synthetic fallback, list-before-selection ordering, write failure as a nonfatal control error, reconnect replay, and committed refresh calculated from DRM mode clock/totals rather than the request.

- [x] **Step 2: Run service tests and verify RED**

Run: `cargo test -p vmlord-display-services`

- [x] **Step 3: Implement control outcomes and broker application**

Decode into the shared validated Rust mode type, cap list count before allocation, apply the mode list before selection, and extend DRM resource reading just enough to return the committed refresh. Preserve `SetResolution` by adding a temporary mode at the selected refresh before requesting it.

- [x] **Step 4: Run service tests and verify GREEN**

Run: `cargo test -p vmlord-display-services`

- [x] **Step 5: Commit**

Run: `git add crates/display-services && git commit -m "TASK-136: Apply host modes in display broker"`

---

### Task 7: Viewer mode menu, monitor transitions, and reconnect

**Files:**
- Modify: `crates/display-viewer/src/windows/window.rs`
- Modify: `crates/display-viewer/src/main.rs`
- Modify: `crates/display-viewer/src/live.rs`
- Modify: `crates/display-viewer/src/state.rs`
- Modify: `crates/display-viewer/src/fullscreen.rs`
- Test: inline viewer unit tests

**Interfaces:**
- Produces: `UiEvent::DisplayMode(DisplayMode)`, dynamic system-menu mode commands, `Live::set_available_modes`, `Live::set_display_mode`, reconnect replay state.
- Consumes: Tasks 2–4 protocol types, policy, snapshots, and persisted selection.

- [x] **Step 1: Write failing viewer orchestration tests**

Cover menu ID ↔ mode mapping, radio check, unchanged snapshot suppression, selection persistence, monitor-change fallback, fullscreen preferred selection, resize retaining selected refresh, and resending list then selection after handover.

- [x] **Step 2: Run viewer tests and verify RED**

Run: `cargo test-windows -p vmlord-display-viewer`

- [x] **Step 3: Implement the system-menu picker and orchestration**

Rebuild a bounded resolution submenu from the current snapshot, label entries `WIDTH x HEIGHT @ HZ Hz`, route command IDs through `UiEvent`, and keep Win32 calls in `windows/window.rs`. In the main loop debounce snapshots, choose by the fallback policy, send list before selection, persist explicit choices, and preserve legacy resize behavior when the capability is absent.

- [x] **Step 4: Run viewer tests and verify GREEN**

Run: `cargo test-windows -p vmlord-display-viewer`

- [x] **Step 5: Commit**

Run: `git add crates/display-viewer && git commit -m "TASK-136: Select host modes in display viewer"`

---

### Task 8: Sustained FPS-gap diagnostic

**Files:**
- Create: `crates/display-viewer/src/fps_gap.rs`
- Modify: `crates/display-viewer/src/lib.rs`
- Modify: `crates/display-viewer/src/main.rs`
- Modify: `crates/display-viewer/src/video.rs`
- Modify: `crates/display-viewer/src/launch.rs`
- Modify: `crates/display-viewer/proto/vmlord/display/viewer/viewer.proto`
- Modify: `crates/platform/src/display_launches.rs`
- Test: inline state-machine and launch-worker tests

**Interfaces:**
- Produces: `FpsGap::sample(now, presented_frames, active_mode) -> Option<GapWarning>` and `Message::Diagnostic { level, detail }` from viewer to application.
- Consumes: configured threshold, confirmed active refresh, presented complete-frame counter, existing launch-pipe `report()` mapping to `diagnostic!`.

- [x] **Step 1: Write failing state-machine tests with a fake clock**

Cover 144 Hz/71 FPS warning at 50%, 144 Hz/72 FPS no warning, ten-second sustain requirement, one-shot suppression, full-interval recovery before rearm, session/reconnect/minimize reset, and message round trip.

```rust
assert_eq!(gap.observe(t0 + Duration::from_secs(9), 71.0, mode_144), None);
assert!(gap.observe(t0 + Duration::from_secs(10), 71.0, mode_144).is_some());
assert_eq!(gap.observe(t0 + Duration::from_secs(20), 70.0, mode_144), None);
```

- [x] **Step 2: Run focused tests and verify RED**

Run: `cargo test -p vmlord-display-viewer fps_gap && cargo test -p vmlord-platform display_launches`

- [x] **Step 3: Implement measurement and diagnostic transport**

Count only frames that decode and present successfully. Pause/reset measurement outside a running visible keyed stream. Format the warning with mode, refresh, measured FPS, and percentage; send it over stdout launch framing, and let `DisplayLaunches::serve` call the existing `report(vm_name, Warning, detail)`.

- [x] **Step 4: Run focused and regression tests**

Run: `cargo test -p vmlord-display-viewer && cargo test -p vmlord-platform display_launches`

- [x] **Step 5: Commit**

Run: `git add crates/display-viewer crates/platform && git commit -m "TASK-136: Diagnose display FPS gaps"`

---

### Task 9: Architecture, full verification, and task handoff

**Files:**
- Modify: `ARCHITECTURE.md`
- Modify: relevant display documentation under `docs/` if commands or troubleshooting change

**Interfaces:**
- Consumes: completed Tasks 1–8 and the approved design.
- Produces: documented multi-mode, refresh, fallback, compatibility, and diagnostic contracts.

- [x] **Step 1: Update architecture and operator documentation**

Replace the single-mode explanation under “Resizing the desktop” with list/selection semantics, retain framebuffer truth and keyframe ordering, document no-EDID policy, 144 Hz ceiling, fallback order, settings key, and warning hysteresis.

- [x] **Step 2: Run formatting and static checks**

Run: `cargo fmt --all -- --check`

Run: `cargo check-windows`

Run: `cargo agent`

Run: `cargo display-services`

- [x] **Step 3: Run complete automated tests**

Run: `cargo test-windows`

Run portable/guest workspace tests supported on WSL, including `cargo test -p vmlord-display-protocol -p vmlord-display-services -p vmlord-display-viewer -p vmlord-core`.

Run: `git diff --check`

- [x] **Step 4: Inspect the final diff and working tree**

Run: `git diff --stat main...HEAD`, `git diff main...HEAD`, and `git status --short`. Confirm no generated build output, secrets, unrelated edits, untranslated text, or stale single-mode claims remain.

- [x] **Step 5: Commit documentation**

Run: `git add ARCHITECTURE.md docs && git commit -m "TASK-136: Document host display modes"`

- [x] **Step 6: Request code review and address findings**

Use `superpowers:requesting-code-review`, apply only verified in-scope findings, rerun the affected tests, and keep the branch local until the user explicitly asks to push or open a merge request.

## How it was executed

Tasks 1 and 2 were committed before this run; Tasks 3 through 9 followed. Four
things went differently from the steps above and are recorded here rather than
left for somebody to discover:

- **Task 5, step 4** ran the container build's own test stage
  (`docker build --target build --build-arg BASE=ubuntu:<release>` from
  `payloads/display/`) against 22.04, 24.04 and 26.04 headers -- kernels 5.15,
  6.8 and 7.0 -- rather than the whole `./rebuild_payload.sh`, which also packs
  and distributes and refuses a tree with uncommitted changes. The module build
  is the same one that script performs, and it is what proves the C compiles.
- **Task 6** was written implementation-first rather than test-first; the tests
  the step names were added afterwards and all pass.
- **Task 9, step 3** cannot run `cargo test -p vmlord-display-viewer` on this
  WSL host: `windows-future` does not compile for the Linux target, which is
  what `cargo test-windows` exists for and is what was run. The protocol,
  services and core crates were tested natively as well.
- **Task 9, step 6** was not run: this session was told not to dispatch
  subagents, and `superpowers:requesting-code-review` is one.

One defect found while reviewing the finished branch is fixed on it: the viewer
published every admissible mode its monitor drives, which for an ordinary panel
is well past the 32 the module holds, and the guest would have refused the
whole list. `display_modes::offered` now cuts it to the largest 32, keeping the
mode in use whatever its size.

Two things this branch deliberately leaves alone:

- The viewer's system-menu strings are English literals, like every other item
  on that menu: the viewer is a separate process and does not carry the
  application's `rust-i18n` catalogue. The settings text this feature added to
  the application *is* in both locales.
- `payloads/display/README.md` was listed for Task 5 and needed no change: it
  documents the build, and nothing about the build moved.
