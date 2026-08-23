# Keyboard and mouse input implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the native display interactive — the viewer turns Windows keyboard and mouse messages into `#118` input records, and the guest puts them on two `/dev/uinput` devices — so that a user can log in through GDM and work in GNOME.

**Architecture:** The wire is already built: `#118` defined the input channel and its five records, `#115` binds and reads the guest end, `#117` binds the host end and sends `ReleaseAll` after every bind. This plan fills both ends. In the viewer, two portable modules do the thinking — `placement.rs` maps a client point to a guest pixel, `input/` translates scan codes and holds the focus/hover policy — and two Win32 modules do the catching: the window's mouse and focus messages, and a `WH_KEYBOARD_LL` hook installed only while the window has focus. In the guest, the privileged broker creates a keyboard and an absolute pointer device and hands their descriptors to the unprivileged session over the existing `SCM_RIGHTS` socket; the session decodes records and writes `input_event` groups into them.

**Tech Stack:** Rust 2024. Viewer: `x86_64-pc-windows-gnu` for check and test from WSL, the `windows` crate for Win32. Guest: `x86_64-unknown-linux-musl`, hand-rolled `libc` ioctls, no C toolchain. `prost` for both schemas.

**Spec:** `docs/superpowers/specs/2026-08-23-display-input-design.md`

## Global Constraints

* Task number for every commit subject: `TASK-119: <comment>`.
* **Evdev keycodes come from scan codes**, never from virtual keys — except `Pause`, `NumLock` and `PrtScn`, resolved by `VK_PAUSE` (`0x13`), `VK_NUMLOCK` (`0x90`) and `VK_SNAPSHOT` (`0x2c`).
* **The absolute range is `0..32767`, fixed forever.** Never derive it from the current resolution: #120 changes resolution at runtime and recreating the device would disconnect the guest's pointer.
* **`PointerScroll` is in hundred-and-twentieths of a detent** on the wire, on Windows (`WHEEL_DELTA` = 120) and on `REL_WHEEL_HI_RES`. The three units are the same; no conversion anywhere. Only the whole-detent `REL_WHEEL` is derived, with the remainder carried.
* **Positive vertical scroll is away from the user; positive horizontal is to the right.** Windows and evdev agree, so signs pass through untouched.
* **Every path that stops sending input owes a release.** Focus lost, keyboard released, pointer channel gone: emit `ReleaseAll` if anything is held. The guest releases what it holds when the input channel drops, and `#117` already sends `ReleaseAll` as the first record of every bind.
* **Never intercept the Secure Attention Sequence.** `Ctrl+Alt+Del` is a menu action that synthesises three presses and three releases. No undocumented hook, no `SendSAS`, no service.
* Reserved and never forwarded to the guest: `Ctrl+Alt+Left Shift`, which releases the keyboard.
* Input records are capped at 4 KiB by `#118`; every message here is a few bytes, so nothing needs chunking.
* Viewer `unsafe` lives only in modules declared with `#[allow(unsafe_code)]` in `src/windows/mod.rs`; `placement.rs` and `input/` must contain none. Every `unsafe` block carries a `// SAFETY:` comment. `crates/display-services` allows `unsafe` crate-wide, but `uinput.rs`'s emit half still needs none: `input_event` is serialised byte by byte.
* Never log key codes, button codes or pointer coordinates above `trace` level, and never log what a key means. A log that reconstructs a password is worse than no log.
* Commands: `cargo check-windows` and `cargo test-windows -p vmlord-display-viewer` for the viewer, never plain `cargo test` on it; `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl` for the guest; `cargo display-services` to build the guest binaries.
* Out of scope, and must not be implemented here: letterboxing, fullscreen, dynamic resolution and saved window state (#120); Connect wiring (#121); clipboard, audio, multi-monitor. Relative pointer mode and pointer confinement are not planned at all.

---

### Task 1: Where the picture sits

**Files:**
- Create: `crates/display-viewer/src/placement.rs`
- Modify: `crates/display-viewer/src/lib.rs`
- Modify: `crates/display-viewer/src/windows/d3d.rs` (`blit`)

**Interfaces:**
- Produces: `placement::Placement { x: i32, y: i32, width: u32, height: u32, guest_width: u32, guest_height: u32 }`; `placement::place(guest_width: u32, guest_height: u32, client_width: i32, client_height: i32) -> Option<Placement>`; `Placement::to_guest(&self, x: i32, y: i32) -> Option<(u32, u32)>`; `Placement::to_guest_clamped(&self, x: i32, y: i32) -> (u32, u32)`.

- [ ] **Step 1: Write the failing tests**

Create `crates/display-viewer/src/placement.rs` with only the tests and the module documentation:

```rust
//! Where the guest's picture sits on the client area.
//!
//! One value, and both consumers read it: the renderer copies into it and the
//! input policy maps points through it. Today it is the crop the renderer
//! already performed -- the picture at the top left, cut off at the window's
//! edges. #120 replaces [`place`] with letterboxing, and because there is one
//! of it rather than one per consumer, the pointer follows the picture without
//! a second change.

#[cfg(test)]
mod tests {
    use super::{Placement, place};

    #[test]
    fn a_picture_smaller_than_the_window_is_placed_whole() {
        let placement = place(800, 600, 1280, 720).expect("a placement");

        assert_eq!((placement.x, placement.y), (0, 0));
        assert_eq!((placement.width, placement.height), (800, 600));
    }

    #[test]
    fn a_picture_larger_than_the_window_is_cut_at_its_edges() {
        let placement = place(1920, 1080, 1280, 720).expect("a placement");

        assert_eq!((placement.width, placement.height), (1280, 720));
        assert_eq!(
            (placement.guest_width, placement.guest_height),
            (1920, 1080)
        );
    }

    #[test]
    fn a_window_with_no_area_has_no_placement() {
        assert!(place(800, 600, 0, 720).is_none());
        assert!(place(800, 600, 1280, -1).is_none());
    }

    #[test]
    fn a_point_on_the_picture_maps_to_the_pixel_under_it() {
        let placement = place(800, 600, 1280, 720).expect("a placement");

        assert_eq!(placement.to_guest(0, 0), Some((0, 0)));
        assert_eq!(placement.to_guest(799, 599), Some((799, 599)));
    }

    #[test]
    fn a_point_off_the_picture_maps_to_nothing() {
        let placement = place(800, 600, 1280, 720).expect("a placement");

        assert_eq!(placement.to_guest(800, 300), None);
        assert_eq!(placement.to_guest(300, 600), None);
        assert_eq!(placement.to_guest(-1, 300), None);
    }

    #[test]
    fn a_placement_smaller_than_its_frame_scales_rather_than_crops() {
        // Not what `place` builds today, and exactly what #120 will: the
        // mapping is written for it so that #120 changes one function.
        let placement = Placement {
            x: 40,
            y: 10,
            width: 400,
            height: 300,
            guest_width: 800,
            guest_height: 600,
        };

        assert_eq!(placement.to_guest(40, 10), Some((0, 0)));
        assert_eq!(placement.to_guest(240, 160), Some((400, 300)));
        assert_eq!(placement.to_guest(439, 309), Some((798, 598)));
        assert_eq!(placement.to_guest(39, 10), None);
    }

    #[test]
    fn a_point_off_the_picture_still_clamps_onto_it() {
        // What a drag that leaves the window sends: motion continues, and
        // every coordinate on the wire is a pixel the guest has.
        let placement = place(800, 600, 1280, 720).expect("a placement");

        assert_eq!(placement.to_guest_clamped(-30, -30), (0, 0));
        assert_eq!(placement.to_guest_clamped(5000, 5000), (799, 599));
    }
}
```

Add `pub mod placement;` to `crates/display-viewer/src/lib.rs`, in alphabetical order among the existing modules.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer placement::`
Expected: FAIL — `cannot find function place in this scope`.

- [ ] **Step 3: Write the implementation**

Above the test module in `placement.rs`:

```rust
/// The rectangle the guest's picture occupies, and the frame it shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The picture's left edge in client pixels.
    pub x: i32,
    /// Its top edge in client pixels.
    pub y: i32,
    /// How wide it is on screen, which #120 makes differ from the frame.
    pub width: u32,
    /// How tall it is on screen.
    pub height: u32,
    /// The frame's width in guest pixels.
    pub guest_width: u32,
    /// The frame's height in guest pixels.
    pub guest_height: u32,
}

/// Where a frame of this size sits on a client area of that size.
///
/// Today: the top left corner, cut off at the window's edges, which is what
/// the renderer already does. Returns `None` for a client area or a frame with
/// no pixels in it, which is a window mid-resize rather than a fault.
#[must_use]
pub fn place(
    guest_width: u32,
    guest_height: u32,
    client_width: i32,
    client_height: i32,
) -> Option<Placement> {
    let client_width = u32::try_from(client_width).ok()?;
    let client_height = u32::try_from(client_height).ok()?;
    let width = guest_width.min(client_width);
    let height = guest_height.min(client_height);
    if width == 0 || height == 0 {
        return None;
    }

    Some(Placement {
        x: 0,
        y: 0,
        width,
        height,
        guest_width,
        guest_height,
    })
}

impl Placement {
    /// The guest pixel under a client point, or `None` off the picture.
    #[must_use]
    pub fn to_guest(&self, x: i32, y: i32) -> Option<(u32, u32)> {
        let inside_x = u32::try_from(x.checked_sub(self.x)?).ok()?;
        let inside_y = u32::try_from(y.checked_sub(self.y)?).ok()?;
        if inside_x >= self.width || inside_y >= self.height {
            return None;
        }

        Some((self.scale_x(inside_x), self.scale_y(inside_y)))
    }

    /// The same, with points off the picture pulled onto its nearest edge.
    ///
    /// What a drag that left the window sends: the guest keeps receiving
    /// motion, and every coordinate on the wire is one it has a pixel for.
    #[must_use]
    pub fn to_guest_clamped(&self, x: i32, y: i32) -> (u32, u32) {
        let inside = |value: i32, origin: i32, size: u32| -> u32 {
            let offset = value.saturating_sub(origin).max(0);
            u32::try_from(offset)
                .unwrap_or(u32::MAX)
                .min(size.saturating_sub(1))
        };

        (
            self.scale_x(inside(x, self.x, self.width)),
            self.scale_y(inside(y, self.y, self.height)),
        )
    }

    fn scale_x(&self, inside: u32) -> u32 {
        scale(inside, self.width, self.guest_width)
    }

    fn scale_y(&self, inside: u32) -> u32 {
        scale(inside, self.height, self.guest_height)
    }
}

/// One axis, in wide arithmetic so that a 4K frame cannot overflow it.
fn scale(inside: u32, on_screen: u32, guest: u32) -> u32 {
    if on_screen == 0 || guest == 0 {
        return 0;
    }

    let scaled = u64::from(inside) * u64::from(guest) / u64::from(on_screen);

    u32::try_from(scaled)
        .unwrap_or(u32::MAX)
        .min(guest.saturating_sub(1))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer placement::`
Expected: PASS, seven tests.

- [ ] **Step 5: Make the renderer read the placement**

In `crates/display-viewer/src/windows/d3d.rs`, add `use crate::placement::place;` to the crate imports and replace the body of `blit` between the descriptor read and the `D3D11_BOX`:

```rust
        let client_width = i32::try_from(descriptor.Width).unwrap_or(i32::MAX);
        let client_height = i32::try_from(descriptor.Height).unwrap_or(i32::MAX);
        let Some(placement) = place(
            geometry.width(),
            geometry.height(),
            client_width,
            client_height,
        ) else {
            return Ok(());
        };

        let region = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: placement.width,
            bottom: placement.height,
            back: 1,
        };
```

and pass the destination corner from the placement rather than the two zeroes the call currently uses:

```rust
        let destination_x = u32::try_from(placement.x).unwrap_or(0);
        let destination_y = u32::try_from(placement.y).unwrap_or(0);
```

Update `blit`'s doc comment: the arithmetic now lives in `placement.rs`, and #120 changes it there.

- [ ] **Step 6: Check the Windows build**

Run: `cargo check-windows`
Expected: no errors, no new warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/display-viewer/src/placement.rs crates/display-viewer/src/lib.rs crates/display-viewer/src/windows/d3d.rs
git commit -m "TASK-119: Put the picture's placement in one function"
```

---

### Task 2: Scan codes to evdev keycodes

**Files:**
- Create: `crates/display-viewer/src/input/keymap.rs`
- Create: `crates/display-viewer/src/input/mod.rs`
- Modify: `crates/display-viewer/src/lib.rs`

**Interfaces:**
- Produces: `input::keymap::keycode(make: u16, extended: bool, virtual_key: u16) -> Option<u16>`, and the named constants `KEY_LEFTCTRL`, `KEY_LEFTSHIFT`, `KEY_LEFTALT`, `KEY_DELETE`, `KEY_PAUSE`, `KEY_NUMLOCK`, `KEY_SYSRQ`.

- [ ] **Step 1: Write the failing tests**

Create `crates/display-viewer/src/input/mod.rs` with just:

```rust
//! What the user does, turned into what the guest is told.

pub mod keymap;
```

Add `pub mod input;` to `crates/display-viewer/src/lib.rs`.

Create `crates/display-viewer/src/input/keymap.rs` with the documentation and tests only:

```rust
//! Set-1 scan codes to evdev keycodes.
//!
//! Not virtual keys: a virtual key has already had the *host's* layout applied
//! to it, and the guest then applies its own, so a host on a non-US layout
//! sends the wrong keys and breaks `Ctrl`+letter. A scan code is a position on
//! the keyboard, and evdev's low keycodes were numbered after this very set,
//! which is why most of this file is a range rather than a table.

#[cfg(test)]
mod tests {
    use super::{
        KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_NUMLOCK, KEY_PAUSE, KEY_SYSRQ, keycode,
    };

    /// A key with no virtual key worth consulting.
    fn plain(make: u16, extended: bool) -> Option<u16> {
        keycode(make, extended, 0)
    }

    #[test]
    fn the_base_page_is_the_scan_code_itself() {
        assert_eq!(plain(0x01, false), Some(1)); // Esc
        assert_eq!(plain(0x1e, false), Some(30)); // A
        assert_eq!(plain(0x1d, false), Some(KEY_LEFTCTRL));
        assert_eq!(plain(0x2a, false), Some(KEY_LEFTSHIFT));
        assert_eq!(plain(0x38, false), Some(KEY_LEFTALT));
        assert_eq!(plain(0x39, false), Some(57)); // Space
        assert_eq!(plain(0x53, false), Some(83)); // Keypad .
    }

    #[test]
    fn the_three_keys_evdev_numbered_out_of_order_are_named() {
        assert_eq!(plain(0x56, false), Some(86)); // the ISO key
        assert_eq!(plain(0x57, false), Some(87)); // F11
        assert_eq!(plain(0x58, false), Some(88)); // F12
    }

    #[test]
    fn the_extended_page_is_a_different_key_at_the_same_code() {
        assert_eq!(plain(0x1c, false), Some(28)); // Enter
        assert_eq!(plain(0x1c, true), Some(96)); // Keypad Enter
        assert_eq!(plain(0x1d, true), Some(97)); // Right Ctrl
        assert_eq!(plain(0x38, true), Some(100)); // Right Alt
        assert_eq!(plain(0x5b, true), Some(125)); // Left Super
    }

    #[test]
    fn the_navigation_block_is_extended() {
        assert_eq!(plain(0x47, true), Some(102)); // Home
        assert_eq!(plain(0x48, true), Some(103)); // Up
        assert_eq!(plain(0x53, true), Some(111)); // Delete
    }

    #[test]
    fn the_three_ambiguous_keys_are_settled_by_their_virtual_key() {
        // Pause and NumLock both report 0x45, and PrtScn arrives from a
        // sequence Windows has already collapsed. None of the three carries a
        // layout, so reading the virtual key costs the scan-code rule nothing.
        assert_eq!(keycode(0x45, false, 0x13), Some(KEY_PAUSE));
        assert_eq!(keycode(0x45, false, 0x90), Some(KEY_NUMLOCK));
        assert_eq!(keycode(0x37, true, 0x2c), Some(KEY_SYSRQ));
    }

    #[test]
    fn a_code_this_build_has_no_key_for_is_dropped() {
        // 0xE0 0x2A is the filler half of PrtScn's sequence: Windows sends it
        // as its own message, and forwarding it would press a key nobody hit.
        assert_eq!(plain(0x2a, true), None);
        assert_eq!(plain(0x00, false), None);
        assert_eq!(plain(0x77, false), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer keymap::`
Expected: FAIL — `cannot find function keycode in this scope`.

- [ ] **Step 3: Write the implementation**

Above the tests in `keymap.rs`:

```rust
/// `VK_PAUSE`, which shares a scan code with `NumLock`.
const VK_PAUSE: u16 = 0x13;
/// `VK_NUMLOCK`, the other half of that collision.
const VK_NUMLOCK: u16 = 0x90;
/// `VK_SNAPSHOT`, which Windows collapses a four-byte sequence into.
const VK_SNAPSHOT: u16 = 0x2c;

/// `KEY_LEFTCTRL`, one third of the reserved release combination.
pub const KEY_LEFTCTRL: u16 = 29;
/// `KEY_LEFTSHIFT`, another third.
pub const KEY_LEFTSHIFT: u16 = 42;
/// `KEY_LEFTALT`, the last third, and part of the Secure Attention Sequence.
pub const KEY_LEFTALT: u16 = 56;
/// `KEY_DELETE`, the rest of that sequence.
pub const KEY_DELETE: u16 = 111;
/// `KEY_PAUSE`.
pub const KEY_PAUSE: u16 = 119;
/// `KEY_NUMLOCK`.
pub const KEY_NUMLOCK: u16 = 69;
/// `KEY_SYSRQ`, which is what PrtScn is called in evdev.
pub const KEY_SYSRQ: u16 = 99;

/// The evdev keycode for one Windows key event, or `None` to drop it.
///
/// `make` is the scan code, `extended` the `0xE0` flag, `virtual_key` the
/// virtual key -- consulted only for the three keys whose scan codes are
/// ambiguous.
#[must_use]
pub fn keycode(make: u16, extended: bool, virtual_key: u16) -> Option<u16> {
    match virtual_key {
        VK_PAUSE => return Some(KEY_PAUSE),
        VK_NUMLOCK => return Some(KEY_NUMLOCK),
        VK_SNAPSHOT => return Some(KEY_SYSRQ),
        _ => {}
    }

    if extended {
        extended_keycode(make)
    } else {
        base_keycode(make)
    }
}

/// The unprefixed page, which evdev's own numbering was built from.
fn base_keycode(make: u16) -> Option<u16> {
    match make {
        0x01..=0x53 => Some(make),
        // The three evdev gave numbers above the range rather than inside it.
        0x56 => Some(86),
        0x57 => Some(87),
        0x58 => Some(88),
        _ => None,
    }
}

/// The `0xE0` page: the duplicated keys, the navigation block and the metas.
fn extended_keycode(make: u16) -> Option<u16> {
    Some(match make {
        0x1c => 96,  // Keypad Enter
        0x1d => 97,  // Right Ctrl
        0x20 => 113, // Mute
        0x2e => 114, // Volume down
        0x30 => 115, // Volume up
        0x35 => 98,  // Keypad /
        0x38 => 100, // Right Alt
        0x46 => KEY_PAUSE, // Ctrl+Break
        0x47 => 102, // Home
        0x48 => 103, // Up
        0x49 => 104, // Page Up
        0x4b => 105, // Left
        0x4d => 106, // Right
        0x4f => 107, // End
        0x50 => 108, // Down
        0x51 => 109, // Page Down
        0x52 => 110, // Insert
        0x53 => 111, // Delete
        0x5b => 125, // Left Super
        0x5c => 126, // Right Super
        0x5d => 127, // Menu
        _ => return None,
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer keymap::`
Expected: PASS, six tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-viewer/src/input crates/display-viewer/src/lib.rs
git commit -m "TASK-119: Translate scan codes into evdev keycodes"
```

---

### Task 3: The focus and hover policy

**Files:**
- Modify: `crates/display-viewer/src/input/mod.rs`

**Interfaces:**
- Consumes: `placement::Placement` and `Placement::to_guest`/`to_guest_clamped` (Task 1); `keymap::keycode` and its constants (Task 2).
- Produces: `input::Event` (`Key`, `Motion`, `Button`, `Scroll`, `ReleaseAll`); `input::Report` (what the window and the hook feed in); the button constants `BTN_LEFT`, `BTN_RIGHT`, `BTN_MIDDLE`, `BTN_SIDE`, `BTN_EXTRA`; `input::Policy` with `new()`, `set_placement(Option<Placement>)`, `report(Report)`, `drain() -> Vec<Event>`, `keyboard_release_requested() -> bool`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/display-viewer/src/input/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::{BTN_LEFT, BTN_RIGHT, Event, Policy, Report};
    use crate::{input::keymap, placement::place};

    /// A policy over an 800x600 guest in a 1280x720 window, focused.
    fn focused() -> Policy {
        let mut policy = Policy::new();
        policy.set_placement(place(800, 600, 1280, 720));
        policy.report(Report::FocusGained);
        let _ = policy.drain();

        policy
    }

    /// The scan code of `A`, which no rule here treats specially.
    const A: Report = Report::Key {
        make: 0x1e,
        extended: false,
        virtual_key: 0x41,
        pressed: true,
    };

    #[test]
    fn a_key_is_forwarded_only_while_the_window_has_focus() {
        let mut policy = Policy::new();
        policy.report(A);
        assert!(policy.drain().is_empty(), "an unfocused window is not typing");

        policy.report(Report::FocusGained);
        policy.report(A);
        assert_eq!(
            policy.drain(),
            vec![Event::Key {
                keycode: 30,
                pressed: true
            }]
        );
    }

    #[test]
    fn losing_focus_releases_what_is_held() {
        let mut policy = focused();
        policy.report(A);
        let _ = policy.drain();

        policy.report(Report::FocusLost);
        assert_eq!(policy.drain(), vec![Event::ReleaseAll]);

        // And nothing is owed the second time.
        policy.report(Report::FocusLost);
        assert!(policy.drain().is_empty());
    }

    #[test]
    fn the_reserved_combination_releases_the_keyboard_and_is_not_forwarded() {
        let mut policy = focused();
        for make in [0x1d, 0x38] {
            policy.report(Report::Key {
                make,
                extended: false,
                virtual_key: 0,
                pressed: true,
            });
        }
        let _ = policy.drain();

        policy.report(Report::Key {
            make: 0x2a,
            extended: false,
            virtual_key: 0,
            pressed: true,
        });

        assert_eq!(policy.drain(), vec![Event::ReleaseAll]);
        assert!(policy.keyboard_release_requested());
        assert!(!policy.keyboard_release_requested(), "the request is taken once");
    }

    #[test]
    fn the_secure_attention_sequence_is_three_presses_and_three_releases() {
        let mut policy = focused();
        policy.report(Report::SecureAttention);

        assert_eq!(
            policy.drain(),
            vec![
                Event::Key { keycode: keymap::KEY_LEFTCTRL, pressed: true },
                Event::Key { keycode: keymap::KEY_LEFTALT, pressed: true },
                Event::Key { keycode: keymap::KEY_DELETE, pressed: true },
                Event::Key { keycode: keymap::KEY_DELETE, pressed: false },
                Event::Key { keycode: keymap::KEY_LEFTALT, pressed: false },
                Event::Key { keycode: keymap::KEY_LEFTCTRL, pressed: false },
            ]
        );
    }

    #[test]
    fn motion_over_the_picture_is_forwarded_and_motion_off_it_is_not() {
        let mut policy = focused();
        policy.report(Report::Pointer { x: 10, y: 20 });
        assert_eq!(policy.drain(), vec![Event::Motion { x: 10, y: 20 }]);

        policy.report(Report::Pointer { x: 900, y: 20 });
        assert!(policy.drain().is_empty());
    }

    #[test]
    fn a_burst_of_motion_arrives_as_the_position_it_ended_at() {
        let mut policy = focused();
        for x in 10..40 {
            policy.report(Report::Pointer { x, y: 5 });
        }

        assert_eq!(policy.drain(), vec![Event::Motion { x: 39, y: 5 }]);
    }

    #[test]
    fn a_press_is_forwarded_only_over_the_picture() {
        let mut policy = focused();
        policy.report(Report::Pointer { x: 900, y: 20 });
        policy.report(Report::Button { button: BTN_LEFT, pressed: true });
        assert!(policy.drain().is_empty());

        policy.report(Report::Pointer { x: 100, y: 20 });
        policy.report(Report::Button { button: BTN_LEFT, pressed: true });
        assert_eq!(
            policy.drain(),
            vec![
                Event::Motion { x: 100, y: 20 },
                Event::Button { button: BTN_LEFT, pressed: true },
            ]
        );
    }

    #[test]
    fn a_drag_that_leaves_the_picture_keeps_moving_along_its_edge() {
        let mut policy = focused();
        policy.report(Report::Pointer { x: 100, y: 20 });
        policy.report(Report::Button { button: BTN_LEFT, pressed: true });
        let _ = policy.drain();

        policy.report(Report::PointerLeft);
        policy.report(Report::Pointer { x: 2000, y: 900 });
        assert_eq!(policy.drain(), vec![Event::Motion { x: 799, y: 599 }]);

        policy.report(Report::Button { button: BTN_LEFT, pressed: false });
        assert_eq!(
            policy.drain(),
            vec![Event::Button { button: BTN_LEFT, pressed: false }]
        );

        // With nothing held, motion off the picture stops again.
        policy.report(Report::Pointer { x: 2000, y: 900 });
        assert!(policy.drain().is_empty());
    }

    #[test]
    fn a_release_of_a_button_that_was_never_pressed_is_dropped() {
        let mut policy = focused();
        policy.report(Report::Button { button: BTN_RIGHT, pressed: false });

        assert!(policy.drain().is_empty());
    }

    #[test]
    fn the_wheel_is_forwarded_over_the_picture_in_the_units_it_arrived_in() {
        let mut policy = focused();
        policy.report(Report::Pointer { x: 100, y: 20 });
        let _ = policy.drain();

        policy.report(Report::Wheel { horizontal: 0, vertical: -240 });
        assert_eq!(
            policy.drain(),
            vec![Event::Scroll { horizontal: 0, vertical: -240 }]
        );
    }

    #[test]
    fn a_lost_channel_forgets_what_was_held_without_sending_anything() {
        // The guest released everything when the socket dropped, and the bind
        // that replaces it opens with `ReleaseAll` of its own.
        let mut policy = focused();
        policy.report(A);
        let _ = policy.drain();

        policy.report(Report::ChannelLost);
        assert!(policy.drain().is_empty());

        policy.report(Report::FocusLost);
        assert!(policy.drain().is_empty(), "nothing is held any more");
    }

    #[test]
    fn a_window_with_no_placement_forwards_no_pointer_events() {
        let mut policy = Policy::new();
        policy.report(Report::FocusGained);
        policy.report(Report::Pointer { x: 10, y: 10 });
        policy.report(Report::Button { button: BTN_LEFT, pressed: true });

        assert!(policy.drain().is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer input::tests`
Expected: FAIL — `cannot find type Policy in this scope`.

- [ ] **Step 3: Write the implementation**

In `crates/display-viewer/src/input/mod.rs`, between the module declaration and the tests:

```rust
use std::collections::BTreeSet;

use crate::placement::Placement;

pub mod keymap;

/// `BTN_LEFT`.
pub const BTN_LEFT: u16 = 0x110;
/// `BTN_RIGHT`.
pub const BTN_RIGHT: u16 = 0x111;
/// `BTN_MIDDLE`.
pub const BTN_MIDDLE: u16 = 0x112;
/// `BTN_SIDE`, the first thumb button.
pub const BTN_SIDE: u16 = 0x113;
/// `BTN_EXTRA`, the second.
pub const BTN_EXTRA: u16 = 0x114;

/// One thing the guest is told, matching one record of the input channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A key went down or came up.
    Key {
        /// The evdev keycode.
        keycode: u16,
        /// Whether it went down.
        pressed: bool,
    },
    /// The pointer is at this guest pixel.
    Motion {
        /// Its column.
        x: u32,
        /// Its row.
        y: u32,
    },
    /// A pointer button went down or came up.
    Button {
        /// The evdev button code.
        button: u16,
        /// Whether it went down.
        pressed: bool,
    },
    /// The wheel turned, in hundred-and-twentieths of a detent.
    Scroll {
        /// Positive to the right.
        horizontal: i32,
        /// Positive away from the user.
        vertical: i32,
    },
    /// Release everything the guest believes is held.
    ReleaseAll,
}

/// One thing the user did, as the window and the hook see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Report {
    /// The window took the keyboard focus.
    FocusGained,
    /// It lost it.
    FocusLost,
    /// The pointer is at this client point.
    Pointer {
        /// Its column in the client area.
        x: i32,
        /// Its row.
        y: i32,
    },
    /// The pointer left the window.
    PointerLeft,
    /// A pointer button changed, already named in evdev's codes.
    Button {
        /// The evdev button code.
        button: u16,
        /// Whether it went down.
        pressed: bool,
    },
    /// The wheel turned, in hundred-and-twentieths of a detent.
    Wheel {
        /// Positive to the right.
        horizontal: i32,
        /// Positive away from the user.
        vertical: i32,
    },
    /// A key changed, as Windows describes it.
    Key {
        /// The scan code.
        make: u16,
        /// Whether the `0xE0` prefix was set.
        extended: bool,
        /// The virtual key, read only for the three ambiguous keys.
        virtual_key: u16,
        /// Whether it went down.
        pressed: bool,
    },
    /// The input channel is gone; the guest has already released everything.
    ChannelLost,
    /// The user asked for the keyboard back, from the menu.
    ReleaseKeyboard,
    /// The user asked for `Ctrl+Alt+Del`, from the menu.
    SecureAttention,
}

/// What is sent, when, and what is owed when it stops.
///
/// The whole of this task's judgement, and none of its Win32: the window and
/// the hook report facts, this decides what the guest hears. Portable, so the
/// rules are tested on any host.
pub struct Policy {
    placement: Option<Placement>,
    focused: bool,
    /// The keycodes the guest believes are down.
    keys: BTreeSet<u16>,
    /// The button codes it believes are down.
    buttons: BTreeSet<u16>,
    /// Where the pointer was last seen, in client pixels.
    pointer: Option<(i32, i32)>,
    queue: Vec<Event>,
    release_requested: bool,
}

impl Policy {
    /// A policy with no focus, no placement and nothing held.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            placement: None,
            focused: false,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            pointer: None,
            queue: Vec::new(),
            release_requested: false,
        }
    }

    /// Where the picture is now, from the stream's geometry and the window.
    pub fn set_placement(&mut self, placement: Option<Placement>) {
        self.placement = placement;
    }

    /// Takes what has accumulated since the last drain.
    #[must_use]
    pub fn drain(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.queue)
    }

    /// Whether the keyboard should be handed back to Windows. Taken once.
    pub fn keyboard_release_requested(&mut self) -> bool {
        std::mem::replace(&mut self.release_requested, false)
    }

    /// Feeds in one thing the user did.
    pub fn report(&mut self, report: Report) {
        match report {
            Report::FocusGained => self.focused = true,
            Report::FocusLost => {
                self.focused = false;
                self.release_all();
            }
            Report::ReleaseKeyboard => {
                self.focused = false;
                self.release_all();
                self.release_requested = true;
            }
            Report::ChannelLost => {
                // The guest released everything when the socket dropped, and
                // the bind that replaces it opens with a `ReleaseAll` of its
                // own. Sending one down a socket that is gone is not recovery.
                self.keys.clear();
                self.buttons.clear();
            }
            Report::SecureAttention => self.secure_attention(),
            Report::Key {
                make,
                extended,
                virtual_key,
                pressed,
            } => self.key(make, extended, virtual_key, pressed),
            Report::Pointer { x, y } => self.pointer(x, y),
            Report::PointerLeft => self.pointer = None,
            Report::Button { button, pressed } => self.button(button, pressed),
            Report::Wheel {
                horizontal,
                vertical,
            } => self.wheel(horizontal, vertical),
        }
    }

    fn key(&mut self, make: u16, extended: bool, virtual_key: u16, pressed: bool) {
        if !self.focused {
            return;
        }
        let Some(keycode) = keymap::keycode(make, extended, virtual_key) else {
            return;
        };

        // The one combination the guest never sees: with the hook installed
        // the keyboard is the guest's, and this is how the user takes it back.
        if pressed
            && keycode == keymap::KEY_LEFTSHIFT
            && self.keys.contains(&keymap::KEY_LEFTCTRL)
            && self.keys.contains(&keymap::KEY_LEFTALT)
        {
            self.report(Report::ReleaseKeyboard);
            return;
        }

        if pressed {
            self.keys.insert(keycode);
        } else {
            self.keys.remove(&keycode);
        }
        self.queue.push(Event::Key { keycode, pressed });
    }

    fn pointer(&mut self, x: i32, y: i32) {
        self.pointer = Some((x, y));
        let Some(placement) = self.placement else {
            return;
        };

        let position = match placement.to_guest(x, y) {
            Some(position) => position,
            // Off the picture with a button down: the guest keeps hearing the
            // drag, along the edge, rather than losing it at the window frame.
            None if !self.buttons.is_empty() => placement.to_guest_clamped(x, y),
            None => return,
        };

        let event = Event::Motion {
            x: position.0,
            y: position.1,
        };
        // A burst of motion is one position: what the guest needs is where the
        // pointer ended up, and the queue is drained many messages later.
        if matches!(self.queue.last(), Some(Event::Motion { .. })) {
            self.queue.pop();
        }
        self.queue.push(event);
    }

    fn button(&mut self, button: u16, pressed: bool) {
        if pressed {
            if !self.over_picture() {
                return;
            }
            self.buttons.insert(button);
        } else if !self.buttons.remove(&button) {
            // A release of something the guest was never told about would
            // release whatever it does hold at that code.
            return;
        }

        self.queue.push(Event::Button { button, pressed });
    }

    fn wheel(&mut self, horizontal: i32, vertical: i32) {
        if !self.over_picture() || (horizontal == 0 && vertical == 0) {
            return;
        }

        self.queue.push(Event::Scroll {
            horizontal,
            vertical,
        });
    }

    fn secure_attention(&mut self) {
        for keycode in [
            keymap::KEY_LEFTCTRL,
            keymap::KEY_LEFTALT,
            keymap::KEY_DELETE,
        ] {
            self.queue.push(Event::Key {
                keycode,
                pressed: true,
            });
        }
        for keycode in [
            keymap::KEY_DELETE,
            keymap::KEY_LEFTALT,
            keymap::KEY_LEFTCTRL,
        ] {
            self.queue.push(Event::Key {
                keycode,
                pressed: false,
            });
        }
    }

    /// Whether the pointer is over the picture right now.
    fn over_picture(&self) -> bool {
        let (Some(placement), Some((x, y))) = (self.placement, self.pointer) else {
            return false;
        };

        placement.to_guest(x, y).is_some()
    }

    /// Queues a release if anything is held, and forgets it.
    fn release_all(&mut self) {
        if self.keys.is_empty() && self.buttons.is_empty() {
            return;
        }

        self.keys.clear();
        self.buttons.clear();
        self.queue.push(Event::ReleaseAll);
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::new()
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer input::`
Expected: PASS, the twelve policy tests plus the six keymap tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-viewer/src/input/mod.rs
git commit -m "TASK-119: Decide what the guest hears and when"
```

---

### Task 4: Putting an event on the wire

**Files:**
- Modify: `crates/display-viewer/src/live.rs`
- Modify: `crates/display-viewer/src/main.rs` (`Order`, `drive`)

**Interfaces:**
- Consumes: `input::Event` (Task 3).
- Produces: `Live::send_input(&mut self, event: input::Event)`; `Order::Input(input::Event)`.

- [ ] **Step 1: Write the failing test**

In `crates/display-viewer/src/live.rs`'s test module, beside the existing bind tests:

```rust
    #[test]
    fn an_input_event_reaches_the_guest_as_its_record() {
        let (mut live, mut harness, guest) = established();
        let (_frame, _input, mut harness) = (harness.frame, harness.input, harness);

        live.send_input(crate::input::Event::Key {
            keycode: 30,
            pressed: true,
        });
        live.send_input(crate::input::Event::Motion { x: 100, y: 50 });

        let limits = Limits::new(WIDTH, HEIGHT);
        let mut payload = Vec::new();

        let header = wait_for_record(&mut harness.input, &limits, &mut payload);
        assert_eq!(header.message_type, InputRecord::KeyEvent as u16);
        let key = KeyEvent::decode(payload.as_slice()).expect("a key event");
        assert_eq!((key.keycode, key.pressed), (30, true));

        let header = wait_for_record(&mut harness.input, &limits, &mut payload);
        assert_eq!(header.message_type, InputRecord::PointerMotion as u16);
        let motion = PointerMotion::decode(payload.as_slice()).expect("a motion");
        assert_eq!((motion.x, motion.y), (100, 50));

        // Sequence numbers advance, which is what the guest's replay check
        // rests on. The bind's own `ReleaseAll` was sequence zero.
        assert!(header.sequence > 0);
        drop(guest);
    }

    #[test]
    fn an_event_that_cannot_be_written_drops_the_socket_for_a_rebind() {
        let (mut live, harness, guest) = established();
        drop(harness.input);

        // Enough events to fill and fail the pipe rather than to be buffered.
        for _ in 0..1024 {
            live.send_input(crate::input::Event::Motion { x: 1, y: 1 });
        }

        assert!(
            live.socket(Channel::Input).is_none(),
            "a channel that failed to write is one that has to bind again"
        );
        drop(guest);
    }
```

Adapt the fixture names to whatever the existing tests in `live.rs` use for an established session (`established`, `harness`, `wait_for_record`, `WIDTH`, `HEIGHT`) — read the module before writing, and reuse rather than duplicate. Add `KeyEvent`, `PointerMotion` and `prost::Message` to the test imports.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer live::`
Expected: FAIL — `no method named send_input`.

- [ ] **Step 3: Write the implementation**

In `live.rs`, beside `request_keyframe`:

```rust
    /// Sends one input event to the guest.
    ///
    /// A write that fails closes the socket rather than retrying: the bind
    /// path reconnects it at the next generation, and the `ReleaseAll` that
    /// opens every bind covers whatever was held when it broke. The frame
    /// channel is untouched -- the picture does not stop because a key did.
    pub fn send_input(&mut self, event: input::Event) {
        if self.input.is_none() {
            return;
        }

        let (message_type, payload) = encode_input(event);
        let sequence = match self.session.take_channel_sequence(Channel::Input) {
            Ok(sequence) => sequence,
            Err(error) => {
                log::debug!("an input record could not be numbered: {error}");
                self.input = None;
                return;
            }
        };
        let record = Record::new(
            Channel::Input,
            message_type as u16,
            sequence,
            0,
            self.session.generation(Channel::Input),
            payload,
        );

        let Some(socket) = self.input.as_mut() else {
            return;
        };
        if let Err(error) = record::write(socket, &record, &self.control_limits) {
            log::debug!("the input channel could not be written to: {error}");
            self.input = None;
        }
    }
```

and, as a free function at the bottom of the module:

```rust
/// One event as the record type and payload the input channel carries.
fn encode_input(event: input::Event) -> (InputRecord, Vec<u8>) {
    match event {
        input::Event::Key { keycode, pressed } => (
            InputRecord::KeyEvent,
            KeyEvent {
                keycode: u32::from(keycode),
                pressed,
            }
            .encode_to_vec(),
        ),
        input::Event::Motion { x, y } => (
            InputRecord::PointerMotion,
            PointerMotion { x, y }.encode_to_vec(),
        ),
        input::Event::Button { button, pressed } => (
            InputRecord::PointerButton,
            PointerButton {
                button: u32::from(button),
                pressed,
            }
            .encode_to_vec(),
        ),
        input::Event::Scroll {
            horizontal,
            vertical,
        } => (
            InputRecord::PointerScroll,
            PointerScroll {
                horizontal,
                vertical,
            }
            .encode_to_vec(),
        ),
        input::Event::ReleaseAll => (InputRecord::ReleaseAll, Vec::new()),
    }
}
```

Add the imports the two need: `crate::input`, `prost::Message as _`, and `KeyEvent`, `PointerButton`, `PointerMotion`, `PointerScroll` from `vmlord_display_protocol::v1`.

- [ ] **Step 4: Carry the event from the pump to the session thread**

In `main.rs`, add the arm to `Order`:

```rust
    /// One input event for the guest.
    Input(input::Event),
```

and drain the whole queue each pass in `drive`, replacing the single `try_recv`:

```rust
        // The whole queue, not one order a pass: with a 2 ms sleep in this
        // loop, one at a time would cap pointer motion at 500 events a second
        // and add latency under exactly the load that matters.
        loop {
            match session.orders.try_recv() {
                Ok(Order::End) => {
                    live.end();
                    return Attempt::Stop;
                }
                Ok(Order::Keyframe) => live.request_keyframe(),
                Ok(Order::Retry) => return Attempt::Restart,
                Ok(Order::Input(event)) => live.send_input(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Attempt::Stop,
            }
        }
```

Add `use vmlord_display_viewer::input;` to `main.rs`'s imports.

- [ ] **Step 5: Run the tests and the build**

Run: `cargo test-windows -p vmlord-display-viewer live::`
Expected: PASS, the existing bind tests plus the two new ones.

Run: `cargo check-windows`
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
git add crates/display-viewer/src/live.rs crates/display-viewer/src/main.rs
git commit -m "TASK-119: Send input events on the input channel"
```

---

### Task 5: The window's mouse and focus messages

**Files:**
- Modify: `crates/display-viewer/src/windows/window.rs`
- Modify: `crates/display-viewer/Cargo.toml`

**Interfaces:**
- Consumes: `input::Report` and the `BTN_*` constants (Task 3).
- Produces: `UiEvent::Input(input::Report)`; `window::SC_SEND_SAS: usize` and `window::SC_RELEASE_KEYBOARD: usize`, the two system-menu commands, reported as `UiEvent::Input(Report::SecureAttention)` and `UiEvent::Input(Report::ReleaseKeyboard)`.

- [ ] **Step 1: Add the Win32 features**

In `crates/display-viewer/Cargo.toml`, add to the `windows` feature list, keeping it sorted:

```toml
    "Win32_UI_Input_KeyboardAndMouse",
```

`SetCapture`, `ReleaseCapture`, `TrackMouseEvent` and the virtual-key constants live there; `SetWindowsHookExW` and `KBDLLHOOKSTRUCT` are already reachable through `Win32_UI_WindowsAndMessaging`.

- [ ] **Step 2: Write the failing tests**

In `window.rs`'s test module:

```rust
    #[test]
    fn a_move_over_the_picture_is_reported_as_a_pointer_position() {
        let (shared, events) = shared();
        let window = Window::open("test", 320, 240, Arc::clone(&shared)).expect("a window");

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_MOUSEMOVE,
                None,
                Some(LPARAM(((30 << 16) | 20) as isize)),
            );
        }

        assert_eq!(
            drain(&events),
            vec![UiEvent::Input(Report::Pointer { x: 20, y: 30 })]
        );
    }

    #[test]
    fn a_press_and_release_are_reported_with_their_evdev_codes() {
        let (shared, events) = shared();
        let window = Window::open("test", 320, 240, Arc::clone(&shared)).expect("a window");

        // SAFETY: messages sent to this process's own window.
        unsafe {
            SendMessageW(window.handle(), WM_RBUTTONDOWN, None, Some(LPARAM(0)));
            SendMessageW(window.handle(), WM_RBUTTONUP, None, Some(LPARAM(0)));
        }

        assert_eq!(
            drain(&events),
            vec![
                UiEvent::Input(Report::Button { button: BTN_RIGHT, pressed: true }),
                UiEvent::Input(Report::Button { button: BTN_RIGHT, pressed: false }),
            ]
        );
    }

    #[test]
    fn a_click_on_the_failed_screen_is_a_button_and_never_a_guest_press() {
        let (shared, events) = shared();
        let window = Window::open("test", 320, 240, Arc::clone(&shared)).expect("a window");
        shared.failed.store(true, Ordering::Relaxed);

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(window.handle(), WM_LBUTTONDOWN, None, Some(LPARAM(0)));
        }

        assert!(
            drain(&events).is_empty(),
            "with the overlay up there is no guest to click on"
        );
    }

    #[test]
    fn the_wheel_is_reported_in_the_units_it_arrived_in() {
        let (shared, events) = shared();
        let window = Window::open("test", 320, 240, Arc::clone(&shared)).expect("a window");

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_MOUSEWHEEL,
                Some(WPARAM((-240i32 as u32 as usize) << 16)),
                Some(LPARAM(0)),
            );
        }

        assert_eq!(
            drain(&events),
            vec![UiEvent::Input(Report::Wheel { horizontal: 0, vertical: -240 })]
        );
    }

    #[test]
    fn focus_is_reported_both_ways() {
        let (shared, events) = shared();
        let window = Window::open("test", 320, 240, Arc::clone(&shared)).expect("a window");

        // SAFETY: messages sent to this process's own window.
        unsafe {
            SendMessageW(window.handle(), WM_SETFOCUS, None, None);
            SendMessageW(window.handle(), WM_KILLFOCUS, None, None);
        }

        assert_eq!(
            drain(&events),
            vec![
                UiEvent::Input(Report::FocusGained),
                UiEvent::Input(Report::FocusLost),
            ]
        );
    }

    #[test]
    fn the_menu_commands_are_reported_as_their_actions() {
        let (shared, events) = shared();
        let window = Window::open("test", 320, 240, Arc::clone(&shared)).expect("a window");

        // SAFETY: messages sent to this process's own window.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_SYSCOMMAND,
                Some(WPARAM(SC_SEND_SAS)),
                Some(LPARAM(0)),
            );
            SendMessageW(
                window.handle(),
                WM_SYSCOMMAND,
                Some(WPARAM(SC_RELEASE_KEYBOARD)),
                Some(LPARAM(0)),
            );
        }

        assert_eq!(
            drain(&events),
            vec![
                UiEvent::Input(Report::SecureAttention),
                UiEvent::Input(Report::ReleaseKeyboard),
            ]
        );
    }
```

Extend the test imports with the messages used, `Report`, `BTN_RIGHT`, `SC_SEND_SAS` and `SC_RELEASE_KEYBOARD`.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test-windows -p vmlord-display-viewer window::`
Expected: FAIL — `no variant named Input found for enum UiEvent`.

- [ ] **Step 4: Write the implementation**

Add the arm to `UiEvent`:

```rust
    /// Something the user did with the keyboard or the mouse.
    Input(crate::input::Report),
```

Add the two command identifiers above `CLASS_NAME` — below `0xF000`, which Windows reserves, and a multiple of sixteen, because it masks the low four bits:

```rust
/// The system-menu item that sends `Ctrl+Alt+Del` to the guest.
pub const SC_SEND_SAS: usize = 0x9010;

/// The one that hands the keyboard back to Windows.
pub const SC_RELEASE_KEYBOARD: usize = 0x9020;
```

Give `Shared` the two fields the window needs for itself:

```rust
    /// Whether `TrackMouseEvent` is armed, so that it is armed once per entry.
    tracking: AtomicBool,
    /// Which buttons are down, one bit each, so that the capture is released
    /// when the last of them lifts rather than when the first does.
    buttons: AtomicU32,
```

In `Window::open`, after the window exists, append the two menu items:

```rust
        // SAFETY: the window's own menu, which belongs to it until it is
        // destroyed; the two strings live across the calls.
        unsafe {
            if let Ok(menu) = GetSystemMenu(hwnd, false).ok() {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                let send = HSTRING::from("Send Ctrl+Alt+Del");
                let _ = AppendMenuW(menu, MF_STRING, SC_SEND_SAS, PCWSTR(send.as_ptr()));
                let release = HSTRING::from("Release keyboard\tCtrl+Alt+Shift");
                let _ = AppendMenuW(menu, MF_STRING, SC_RELEASE_KEYBOARD, PCWSTR(release.as_ptr()));
            }
        }
```

In `wnd_proc`, add the arms. The existing `WM_LBUTTONUP` arm keeps its failed-screen hit test and gains the guest path:

```rust
        WM_MOUSEMOVE => {
            if !shared.failed.load(Ordering::Relaxed) {
                if !shared.tracking.swap(true, Ordering::Relaxed) {
                    track_leave(hwnd);
                }
                shared.report(UiEvent::Input(Report::Pointer {
                    x: point(lparam.0, 0),
                    y: point(lparam.0, 16),
                }));
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            shared.tracking.store(false, Ordering::Relaxed);
            shared.report(UiEvent::Input(Report::PointerLeft));
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
            if !shared.failed.load(Ordering::Relaxed)
                && let Some(button) = button_of(message, wparam)
            {
                press(&shared, hwnd, button, true);
            }
            LRESULT(if message == WM_XBUTTONDOWN { 1 } else { 0 })
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => {
            if shared.failed.load(Ordering::Relaxed) {
                // A click is a button only while the failed screen is up: with
                // the guest's picture on screen there is nothing to press.
                if message == WM_LBUTTONUP {
                    let (width, height) = client_size(hwnd);
                    if let Some(button) =
                        status::hit_test(width, height, point(lparam.0, 0), point(lparam.0, 16))
                    {
                        shared.report(UiEvent::Pressed(button));
                    }
                }
            } else if let Some(button) = button_of(message, wparam) {
                press(&shared, hwnd, button, false);
            }
            LRESULT(if message == WM_XBUTTONUP { 1 } else { 0 })
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            if !shared.failed.load(Ordering::Relaxed) {
                let delta = i32::from(((wparam.0 >> 16) & 0xffff) as i16);
                let (horizontal, vertical) = if message == WM_MOUSEHWHEEL {
                    (delta, 0)
                } else {
                    (0, delta)
                };
                shared.report(UiEvent::Input(Report::Wheel {
                    horizontal,
                    vertical,
                }));
            }
            LRESULT(0)
        }
        WM_SETFOCUS => {
            shared.report(UiEvent::Input(Report::FocusGained));
            LRESULT(0)
        }
        WM_KILLFOCUS => {
            shared.report(UiEvent::Input(Report::FocusLost));
            LRESULT(0)
        }
        WM_SYSCOMMAND => {
            match wparam.0 & 0xfff0 {
                SC_SEND_SAS => {
                    shared.report(UiEvent::Input(Report::SecureAttention));
                    return LRESULT(0);
                }
                SC_RELEASE_KEYBOARD => {
                    shared.report(UiEvent::Input(Report::ReleaseKeyboard));
                    return LRESULT(0);
                }
                _ => {}
            }
            // SAFETY: the default handler, which owns Move, Size and Close.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
```

with three helpers at the bottom of the module:

```rust
/// One signed sixteen-bit half of an `lParam` point.
fn point(lparam: isize, shift: u32) -> i32 {
    i32::from(((lparam >> shift) & 0xffff) as i16)
}

/// The evdev button a mouse message names, if this build sends it.
fn button_of(message: u32, wparam: WPARAM) -> Option<u16> {
    Some(match message {
        WM_LBUTTONDOWN | WM_LBUTTONUP => input::BTN_LEFT,
        WM_RBUTTONDOWN | WM_RBUTTONUP => input::BTN_RIGHT,
        WM_MBUTTONDOWN | WM_MBUTTONUP => input::BTN_MIDDLE,
        WM_XBUTTONDOWN | WM_XBUTTONUP => match (wparam.0 >> 16) & 0xffff {
            1 => input::BTN_SIDE,
            2 => input::BTN_EXTRA,
            _ => return None,
        },
        _ => return None,
    })
}

/// Reports one button and keeps the capture for as long as any is held.
///
/// The capture is what makes a release outside the window still arrive, which
/// is what keeps a drag from ending with a button the guest thinks is down.
fn press(shared: &Shared, hwnd: HWND, button: u16, pressed: bool) {
    let bit = 1u32 << (button - input::BTN_LEFT).min(31);
    let held = if pressed {
        shared.buttons.fetch_or(bit, Ordering::Relaxed) | bit
    } else {
        shared.buttons.fetch_and(!bit, Ordering::Relaxed) & !bit
    };

    // SAFETY: a capture on this process's own window.
    unsafe {
        if pressed {
            let _ = SetCapture(hwnd);
        } else if held == 0 {
            let _ = ReleaseCapture();
        }
    }

    shared.report(UiEvent::Input(Report::Button { button, pressed }));
}

/// Asks for one `WM_MOUSELEAVE` the next time the pointer goes.
fn track_leave(hwnd: HWND) {
    let mut track = TRACKMOUSEEVENT {
        cbSize: u32::try_from(std::mem::size_of::<TRACKMOUSEEVENT>()).unwrap_or(0),
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    // SAFETY: `track` lives across the call and names this process's window.
    unsafe {
        let _ = TrackMouseEvent(&raw mut track);
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer window::`
Expected: PASS, the existing window tests plus the six new ones.

- [ ] **Step 6: Commit**

```bash
git add crates/display-viewer/Cargo.toml crates/display-viewer/src/windows/window.rs
git commit -m "TASK-119: Report the window's mouse, focus and menu actions"
```

---

### Task 6: The low-level keyboard hook

**Files:**
- Create: `crates/display-viewer/src/windows/hook.rs`
- Modify: `crates/display-viewer/src/windows/mod.rs`

**Interfaces:**
- Consumes: `window::Shared` and `UiEvent::Input` (Task 5).
- Produces: `hook::Hook::install(shared: &Arc<Shared>) -> Result<Hook, String>`, released by dropping it.

- [ ] **Step 1: Write the failing test**

Create `crates/display-viewer/src/windows/hook.rs` with the documentation and this test:

```rust
//! The keyboard, taken from Windows while the window has focus.
//!
//! Without this the guest never sees `Super`, `Alt+Tab`, `Ctrl+Esc` or
//! `Alt+Esc`: the shell takes them before any window is asked. A
//! `WH_KEYBOARD_LL` hook is the documented way to see them first, and it needs
//! no elevation.
//!
//! It is installed on focus and removed on its loss, so a user who is not in
//! the viewer has an ordinary keyboard. While it is installed every key goes
//! to the guest and none to Windows -- `Alt+F4` included, which is why
//! `Ctrl+Alt+Left Shift` exists to hand the keyboard back.
//!
//! `Ctrl+Alt+Del` is not here and cannot be: the Secure Attention Sequence is
//! routed by the kernel, no hook sees it, and reaching for undocumented means
//! is out of the question. It is a menu action instead.
//!
//! The callback runs on the thread that installed the hook -- the message
//! pump, where by construction nothing blocks. That matters: a low-level hook
//! slower than `LowLevelHooksTimeout` is removed by the system without asking.
//! So it does one thing, reports it, and returns.

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};

    use crate::{
        input::Report,
        windows::{
            hook::Hook,
            window::{Shared, UiEvent},
        },
    };

    #[test]
    fn a_hook_installs_and_comes_off_when_it_is_dropped() {
        let (events, _receiver) = mpsc::channel();
        let shared = Arc::new(Shared::new(events));

        let hook = Hook::install(&shared).expect("a hook");
        drop(hook);

        // A second install proves the first was released: the hook is
        // per-thread, and a leaked one would still be holding the keyboard.
        let hook = Hook::install(&shared).expect("a second hook");
        drop(hook);
    }

    #[test]
    fn a_key_the_hook_sees_is_reported_and_kept_from_windows() {
        let (events, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::new(events));
        let hook = Hook::install(&shared).expect("a hook");

        let swallowed = super::deliver(0x1e, 0x41, false, true);

        assert!(swallowed, "a key the guest is being sent is not Windows's");
        assert_eq!(
            receiver.try_recv().expect("a report"),
            UiEvent::Input(Report::Key {
                make: 0x1e,
                extended: false,
                virtual_key: 0x41,
                pressed: true,
            })
        );
        drop(hook);
    }

    #[test]
    fn a_key_that_arrives_with_no_hook_installed_is_left_to_windows() {
        assert!(!super::deliver(0x1e, 0x41, false, true));
    }
}
```

`deliver` is the seam the test needs: the body of the callback, taking the fields rather than a `KBDLLHOOKSTRUCT` pointer, so that the decision is testable without a real hook chain.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test-windows -p vmlord-display-viewer hook::`
Expected: FAIL — `cannot find type Hook in this scope`.

- [ ] **Step 3: Write the implementation**

Above the tests:

```rust
use std::{
    cell::RefCell,
    sync::Arc,
};

use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    UI::WindowsAndMessaging::{
        CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED,
        SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    },
};

use crate::{input::Report, windows::window::{Shared, UiEvent}};

thread_local! {
    /// Who the callback reports to. Thread-local because the hook is: it is
    /// installed on the pump's thread and called on it, and there is no
    /// context pointer in the callback's signature to carry this instead.
    static LISTENER: RefCell<Option<Arc<Shared>>> = const { RefCell::new(None) };
}

/// The keyboard, held for as long as this value lives.
pub struct Hook(HHOOK);

impl Hook {
    /// Takes the keyboard, reporting every key to `shared`.
    ///
    /// # Errors
    ///
    /// A message naming what Windows refused. The viewer carries on without
    /// it: a keyboard that misses `Super` is worth more than no session.
    pub fn install(shared: &Arc<Shared>) -> Result<Self, String> {
        LISTENER.with(|listener| *listener.borrow_mut() = Some(Arc::clone(shared)));

        // SAFETY: a hook on this thread, with a callback of this module's.
        // `None` for the module handle is what a thread-local hook takes.
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), None, 0) }
            .map_err(|error| {
                LISTENER.with(|listener| *listener.borrow_mut() = None);

                format!("the keyboard hook was refused: {error}")
            })?;

        Ok(Self(hook))
    }
}

impl Drop for Hook {
    fn drop(&mut self) {
        // SAFETY: a hook this value installed and nothing else has removed.
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
        }
        LISTENER.with(|listener| *listener.borrow_mut() = None);
    }
}

/// Reports one key and says whether Windows should be kept from it.
///
/// Everything the hook sees is the guest's while it is installed, which is
/// what makes `Alt+Tab` and `Super` reach GNOME. The one exception is an
/// injected event, which belongs to whatever injected it.
fn deliver(make: u16, virtual_key: u16, extended: bool, pressed: bool) -> bool {
    LISTENER.with(|listener| {
        let Some(shared) = listener.borrow().clone() else {
            return false;
        };

        shared.report(UiEvent::Input(Report::Key {
            make,
            extended,
            virtual_key,
            pressed,
        }));

        true
    })
}

/// What Windows calls for every key while the hook is installed.
extern "system" fn callback(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for `HC_ACTION`, `lparam` is a `KBDLLHOOKSTRUCT` that lives
        // for the length of this call, which is the only time it is read.
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let injected = event.flags & LLKHF_INJECTED != KBDLLHOOKSTRUCT_FLAGS(0);
        if !injected {
            let make = u16::try_from(event.scanCode).unwrap_or(0);
            let virtual_key = u16::try_from(event.vkCode).unwrap_or(0);
            let extended = event.flags & LLKHF_EXTENDED != KBDLLHOOKSTRUCT_FLAGS(0);
            let pressed = matches!(
                u32::try_from(wparam.0).unwrap_or(0),
                WM_KEYDOWN | WM_SYSKEYDOWN
            );

            if deliver(make, virtual_key, extended, pressed) {
                return LRESULT(1);
            }
        }
    }

    // SAFETY: the rest of the chain, which is what a hook owes it.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
```

Import `KBDLLHOOKSTRUCT_FLAGS` alongside the rest; the `windows` crate types those flag comparisons rather than leaving them integers.

Declare the module in `crates/display-viewer/src/windows/mod.rs` beside the others, with the same `#[allow(unsafe_code)]` and a one-line comment saying what its `unsafe` is for.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test-windows -p vmlord-display-viewer hook::`
Expected: PASS, three tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-viewer/src/windows/hook.rs crates/display-viewer/src/windows/mod.rs
git commit -m "TASK-119: Take the keyboard while the window has focus"
```

---

### Task 7: Wiring the viewer together

**Files:**
- Modify: `crates/display-viewer/src/main.rs`

**Interfaces:**
- Consumes: everything from Tasks 1 and 3–6.
- Produces: nothing new; this is the wiring that makes the binary interactive.

- [ ] **Step 1: Hold the policy, the hook and the geometry in the loop**

In `main.rs`, add to `Loop<'a>` nothing — the state belongs to `pump`, which owns the frame of the loop. Inside `pump`, before the `while`:

```rust
    let mut policy = input::Policy::new();
    let mut stream: Option<Geometry> = None;
    let mut hook: Option<Hook> = None;
```

and a small closure-free helper beside `pump`:

```rust
/// Tells the policy where the picture is, from the stream and the window.
fn reposition(policy: &mut input::Policy, stream: Option<Geometry>, window: &Window) {
    let Some(geometry) = stream else {
        policy.set_placement(None);
        return;
    };

    let (width, height) = window.client_size();
    policy.set_placement(place(geometry.width(), geometry.height(), width, height));
}
```

- [ ] **Step 2: Feed the policy, and act on what it asks for**

In `pump`'s `ui` drain, add the arm and keep the others:

```rust
                UiEvent::Input(report) => {
                    match report {
                        Report::FocusGained => match Hook::install(context.shared) {
                            Ok(installed) => hook = Some(installed),
                            // A viewer without the hook still types; it only
                            // loses the keys the shell takes first.
                            Err(error) => log::warn!("{error}"),
                        },
                        Report::FocusLost | Report::ReleaseKeyboard => hook = None,
                        _ => {}
                    }
                    policy.report(report);
                }
```

and extend the `Resized` arm with `reposition(&mut policy, stream, context.window);` after the swapchain resize.

In `apply`, where `Signal::Configured(geometry)` is handled, the caller needs the geometry too; hand it back by having `pump` match on the signal before calling `apply`:

```rust
        while let Ok(signal) = context.signals.try_recv() {
            worked = true;
            match &signal {
                Signal::Configured(geometry) => {
                    stream = Some(*geometry);
                    reposition(&mut policy, stream, context.window);
                }
                Signal::Ended(_) => policy.report(Report::ChannelLost),
                _ => {}
            }
            apply(&mut context, &mut progress, signal);
        }
```

- [ ] **Step 3: Send what the policy produced, once a pass**

After the three drains and before `progress.tick`:

```rust
        for event in policy.drain() {
            worked = true;
            let _ = context.orders.send(Order::Input(event));
        }
        if policy.keyboard_release_requested() {
            hook = None;
        }
```

- [ ] **Step 4: Give the keyboard back when the window goes**

Before `pump` returns, on both paths, `hook = None;` — dropping it removes the hook. A hook left installed after the window closes would swallow the user's keyboard with nothing to send it to.

- [ ] **Step 5: Check and run the whole suite**

Run: `cargo check-windows`
Expected: no errors, no new warnings.

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS, the crate's whole suite.

- [ ] **Step 6: Commit**

```bash
git add crates/display-viewer/src/main.rs
git commit -m "TASK-119: Make the viewer's window send what the user does"
```

---

### Task 8: Writing events to a uinput device

**Files:**
- Create: `crates/display-services/src/uinput.rs`
- Modify: `crates/display-services/src/lib.rs`

**Interfaces:**
- Produces: `uinput::Keyboard<W: Write>` with `new(W)`, `key(u16, bool) -> io::Result<()>`, `release_all() -> io::Result<()>`; `uinput::Pointer<W: Write>` with `new(W)`, `motion(x: u32, y: u32, width: u32, height: u32) -> io::Result<()>`, `button(u16, bool) -> io::Result<()>`, `scroll(horizontal: i32, vertical: i32) -> io::Result<()>`, `release_all() -> io::Result<()>`; the constants `ABS_RANGE: i32 = 32767`, `DETENT: i32 = 120`, `KEY_MAX_SENT: u16 = 127`, `BTN_LEFT`…`BTN_EXTRA`.

- [ ] **Step 1: Write the failing tests**

Create `crates/display-services/src/uinput.rs` with the documentation and tests only:

```rust
//! The guest's keyboard and pointer, as `/dev/uinput` devices.
//!
//! Two halves, split by who may call them. [`create`] needs root and belongs
//! to the broker; [`Keyboard`] and [`Pointer`] write into a descriptor that
//! has already been created, and belong to the session process the broker
//! hands it to.
//!
//! Two devices rather than one: libinput classifies a device by its capability
//! bits, and a node carrying keys, absolute axes and buttons at once is
//! resolved by heuristics that have changed between releases. Ubuntu 22.04,
//! 24.04 and 26.04 must behave the same, so each node is unambiguous.
//!
//! Nothing here is `unsafe` below [`create`]: an `input_event` is written out
//! byte by byte, which also makes the event stream something a test can read.

#[cfg(test)]
mod tests {
    use super::{ABS_RANGE, BTN_LEFT, EV_ABS, EV_KEY, EV_REL, EV_SYN, Keyboard, Pointer};

    /// The `(type, code, value)` triples one write put on the wire.
    fn events(bytes: &[u8]) -> Vec<(u16, u16, i32)> {
        bytes
            .chunks_exact(24)
            .map(|event| {
                (
                    u16::from_ne_bytes([event[16], event[17]]),
                    u16::from_ne_bytes([event[18], event[19]]),
                    i32::from_ne_bytes([event[20], event[21], event[22], event[23]]),
                )
            })
            .collect()
    }

    #[test]
    fn a_key_is_one_event_and_the_report_that_closes_it() {
        let mut keyboard = Keyboard::new(Vec::new());
        keyboard.key(30, true).expect("a key goes down");

        assert_eq!(
            events(keyboard.device()),
            vec![(EV_KEY, 30, 1), (EV_SYN, 0, 0)]
        );
    }

    #[test]
    fn a_key_this_build_never_sends_is_dropped_rather_than_written() {
        let mut keyboard = Keyboard::new(Vec::new());
        keyboard.key(400, true).expect("a keycode with no key");

        assert!(events(keyboard.device()).is_empty());
    }

    #[test]
    fn a_release_all_releases_what_is_held_and_nothing_else() {
        let mut keyboard = Keyboard::new(Vec::new());
        keyboard.key(30, true).expect("a");
        keyboard.key(42, true).expect("shift");
        keyboard.key(30, false).expect("a again");
        let held = keyboard.device().len();

        keyboard.release_all().expect("a release");

        assert_eq!(
            events(&keyboard.device()[held..]),
            vec![(EV_KEY, 42, 0), (EV_SYN, 0, 0)]
        );

        // And a second release owes nothing.
        let released = keyboard.device().len();
        keyboard.release_all().expect("a second release");
        assert_eq!(keyboard.device().len(), released);
    }

    #[test]
    fn motion_is_scaled_onto_the_fixed_absolute_range() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.motion(0, 0, 1920, 1080).expect("the top left");
        pointer.motion(1919, 1079, 1920, 1080).expect("the bottom right");
        pointer.motion(960, 540, 1920, 1080).expect("the middle");

        let events = events(pointer.device());
        assert_eq!(events[0], (EV_ABS, 0, 0));
        assert_eq!(events[1], (EV_ABS, 1, 0));
        assert_eq!(events[3], (EV_ABS, 0, ABS_RANGE));
        assert_eq!(events[4], (EV_ABS, 1, ABS_RANGE));
        assert_eq!(events[6], (EV_ABS, 0, ABS_RANGE / 2));
    }

    #[test]
    fn motion_past_the_edge_is_pulled_back_onto_the_screen() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.motion(4000, 4000, 1920, 1080).expect("motion");

        let events = events(pointer.device());
        assert_eq!(events[0], (EV_ABS, 0, ABS_RANGE));
        assert_eq!(events[1], (EV_ABS, 1, ABS_RANGE));
    }

    #[test]
    fn a_whole_detent_travels_on_both_axes_of_the_wheel() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.scroll(0, 120).expect("one detent up");

        assert_eq!(
            events(pointer.device()),
            vec![(EV_REL, 0x0b, 120), (EV_REL, 8, 1), (EV_SYN, 0, 0)]
        );
    }

    #[test]
    fn a_slow_wheel_carries_its_remainder_until_it_makes_a_detent() {
        let mut pointer = Pointer::new(Vec::new());
        for _ in 0..3 {
            pointer.scroll(0, 40).expect("a third of a detent");
        }

        let events = events(pointer.device());
        let detents: Vec<_> = events.iter().filter(|event| event.1 == 8).collect();
        assert_eq!(detents, vec![&(EV_REL, 8, 1)], "three thirds are one detent");
        assert_eq!(
            events.iter().filter(|event| event.1 == 0x0b).count(),
            3,
            "and every one of them travelled at full resolution"
        );
    }

    #[test]
    fn a_wheel_turned_the_other_way_carries_its_remainder_the_same() {
        let mut pointer = Pointer::new(Vec::new());
        for _ in 0..3 {
            pointer.scroll(0, -40).expect("a third of a detent");
        }

        let events = events(pointer.device());
        let detents: Vec<_> = events.iter().filter(|event| event.1 == 8).collect();
        assert_eq!(detents, vec![&(EV_REL, 8, -1)]);
    }

    #[test]
    fn a_button_this_build_never_sends_is_dropped() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.button(0x999, true).expect("a button with no name");

        assert!(events(pointer.device()).is_empty());
    }

    #[test]
    fn a_pointer_release_all_lifts_the_buttons_that_are_down() {
        let mut pointer = Pointer::new(Vec::new());
        pointer.button(BTN_LEFT, true).expect("a press");
        let held = pointer.device().len();

        pointer.release_all().expect("a release");

        assert_eq!(
            events(&pointer.device()[held..]),
            vec![(EV_KEY, BTN_LEFT, 0), (EV_SYN, 0, 0)]
        );
    }
}
```

Add `pub mod uinput;` to `crates/display-services/src/lib.rs`, in alphabetical order.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl uinput::`
Expected: FAIL — `cannot find type Keyboard in this scope`.

- [ ] **Step 3: Write the implementation**

Above the tests:

```rust
use std::{
    collections::BTreeSet,
    io::{self, Write},
};

/// `EV_SYN`.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY`.
pub const EV_KEY: u16 = 0x01;
/// `EV_REL`.
pub const EV_REL: u16 = 0x02;
/// `EV_ABS`.
pub const EV_ABS: u16 = 0x03;

/// `ABS_X`.
const ABS_X: u16 = 0x00;
/// `ABS_Y`.
const ABS_Y: u16 = 0x01;
/// `REL_HWHEEL`.
const REL_HWHEEL: u16 = 0x06;
/// `REL_WHEEL`.
const REL_WHEEL: u16 = 0x08;
/// `REL_HWHEEL_HI_RES`.
const REL_HWHEEL_HI_RES: u16 = 0x0c;
/// `REL_WHEEL_HI_RES`.
const REL_WHEEL_HI_RES: u16 = 0x0b;
/// `SYN_REPORT`.
const SYN_REPORT: u16 = 0x00;

/// `BTN_LEFT`.
pub const BTN_LEFT: u16 = 0x110;
/// `BTN_EXTRA`, the highest button this build sends.
pub const BTN_EXTRA: u16 = 0x114;

/// The absolute axes' maximum, fixed for the life of the device.
///
/// Never derived from the resolution: #120 changes that at runtime, and a
/// device recreated to follow it would disconnect the guest's pointer.
pub const ABS_RANGE: i32 = 32767;

/// One wheel detent, in the hundred-and-twentieths both the wire and the
/// kernel's high-resolution axes count in.
pub const DETENT: i32 = 120;

/// The highest keycode this build sends, and the keyboard's declared ceiling.
pub const KEY_MAX_SENT: u16 = 127;

/// How many bytes one `input_event` is on this target.
const EVENT_SIZE: usize = 24;

/// One event, written out rather than transmuted.
///
/// The timestamp stays zero: the kernel fills it in, and reading a clock per
/// mouse movement to have it overwritten is work for nothing.
fn encode(kind: u16, code: u16, value: i32) -> [u8; EVENT_SIZE] {
    let mut bytes = [0u8; EVENT_SIZE];
    bytes[16..18].copy_from_slice(&kind.to_ne_bytes());
    bytes[18..20].copy_from_slice(&code.to_ne_bytes());
    bytes[20..24].copy_from_slice(&value.to_ne_bytes());

    bytes
}

/// Writes one group and the report that closes it.
///
/// One write, not one per event: a group the kernel sees whole is a group
/// libinput sees as one motion rather than as an axis at a time.
fn emit<W: Write>(device: &mut W, group: &[[u8; EVENT_SIZE]]) -> io::Result<()> {
    if group.is_empty() {
        return Ok(());
    }

    let mut bytes = Vec::with_capacity((group.len() + 1) * EVENT_SIZE);
    for event in group {
        bytes.extend_from_slice(event);
    }
    bytes.extend_from_slice(&encode(EV_SYN, SYN_REPORT, 0));

    device.write_all(&bytes)
}

/// The guest's keyboard.
pub struct Keyboard<W: Write> {
    device: W,
    held: BTreeSet<u16>,
}

impl<W: Write> Keyboard<W> {
    /// A keyboard over a descriptor [`create`] has already made a device of.
    pub const fn new(device: W) -> Self {
        Self {
            device,
            held: BTreeSet::new(),
        }
    }

    /// Presses or releases one key.
    ///
    /// A keycode this build never sends is dropped: the device declared a
    /// ceiling, and writing past it would be a key nobody can have pressed.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn key(&mut self, keycode: u16, pressed: bool) -> io::Result<()> {
        if keycode == 0 || keycode > KEY_MAX_SENT {
            return Ok(());
        }

        if pressed {
            self.held.insert(keycode);
        } else {
            self.held.remove(&keycode);
        }

        emit(&mut self.device, &[encode(EV_KEY, keycode, i32::from(pressed))])
    }

    /// Releases every key this device believes is down.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn release_all(&mut self) -> io::Result<()> {
        if self.held.is_empty() {
            return Ok(());
        }

        let group: Vec<_> = std::mem::take(&mut self.held)
            .into_iter()
            .map(|keycode| encode(EV_KEY, keycode, 0))
            .collect();

        emit(&mut self.device, &group)
    }

    /// What has been written, for the tests that read it back.
    #[cfg(test)]
    fn device(&self) -> &W {
        &self.device
    }
}

/// The guest's absolute pointer.
pub struct Pointer<W: Write> {
    device: W,
    held: BTreeSet<u16>,
    /// The fraction of a detent not yet worth a whole one, per axis.
    horizontal: i32,
    vertical: i32,
}

impl<W: Write> Pointer<W> {
    /// A pointer over a descriptor [`create`] has already made a device of.
    pub const fn new(device: W) -> Self {
        Self {
            device,
            held: BTreeSet::new(),
            horizontal: 0,
            vertical: 0,
        }
    }

    /// Moves the pointer to one guest pixel of a screen this size.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn motion(&mut self, x: u32, y: u32, width: u32, height: u32) -> io::Result<()> {
        let group = [
            encode(EV_ABS, ABS_X, scale(x, width)),
            encode(EV_ABS, ABS_Y, scale(y, height)),
        ];

        emit(&mut self.device, &group)
    }

    /// Presses or releases one button.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn button(&mut self, button: u16, pressed: bool) -> io::Result<()> {
        if !(BTN_LEFT..=BTN_EXTRA).contains(&button) {
            return Ok(());
        }

        if pressed {
            self.held.insert(button);
        } else {
            self.held.remove(&button);
        }

        emit(&mut self.device, &[encode(EV_KEY, button, i32::from(pressed))])
    }

    /// Turns the wheel by hundred-and-twentieths of a detent.
    ///
    /// Both resolutions travel: the high-resolution axis in the unit it
    /// arrived in, and the whole detents it adds up to, with the remainder
    /// carried so that slow scrolling is not lost on an application that reads
    /// only the discrete axis.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn scroll(&mut self, horizontal: i32, vertical: i32) -> io::Result<()> {
        let mut group = Vec::new();
        if horizontal != 0 {
            group.push(encode(EV_REL, REL_HWHEEL_HI_RES, horizontal));
            self.horizontal = self.horizontal.saturating_add(horizontal);
            let detents = self.horizontal / DETENT;
            if detents != 0 {
                self.horizontal -= detents * DETENT;
                group.push(encode(EV_REL, REL_HWHEEL, detents));
            }
        }
        if vertical != 0 {
            group.push(encode(EV_REL, REL_WHEEL_HI_RES, vertical));
            self.vertical = self.vertical.saturating_add(vertical);
            let detents = self.vertical / DETENT;
            if detents != 0 {
                self.vertical -= detents * DETENT;
                group.push(encode(EV_REL, REL_WHEEL, detents));
            }
        }

        emit(&mut self.device, &group)
    }

    /// Releases every button this device believes is down.
    ///
    /// # Errors
    ///
    /// Whatever the device refused the write with.
    pub fn release_all(&mut self) -> io::Result<()> {
        if self.held.is_empty() {
            return Ok(());
        }

        let group: Vec<_> = std::mem::take(&mut self.held)
            .into_iter()
            .map(|button| encode(EV_KEY, button, 0))
            .collect();

        emit(&mut self.device, &group)
    }

    /// What has been written, for the tests that read it back.
    #[cfg(test)]
    fn device(&self) -> &W {
        &self.device
    }
}

/// One guest pixel onto the fixed absolute range.
fn scale(value: u32, size: u32) -> i32 {
    let last = u64::from(size.saturating_sub(1)).max(1);
    let value = u64::from(value).min(last);
    let scaled = value * u64::try_from(ABS_RANGE).unwrap_or(0) / last;

    i32::try_from(scaled).unwrap_or(ABS_RANGE).min(ABS_RANGE)
}
```

Note the tests read `device()` as a slice: give the tests `keyboard.device().as_slice()` semantics by having them call `.device()` on a `Vec<u8>` and indexing it directly, which works because `Vec<u8>` derefs to `[u8]`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl uinput::`
Expected: PASS, ten tests.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/uinput.rs crates/display-services/src/lib.rs
git commit -m "TASK-119: Write keyboard and pointer events to a device"
```

---

### Task 9: Creating the devices

**Files:**
- Modify: `crates/display-services/src/uinput.rs`

**Interfaces:**
- Produces: `uinput::create(path: &Path) -> io::Result<(OwnedFd, OwnedFd)>`, returning the keyboard's descriptor and the pointer's, in that order; `uinput::DEVICE_PATH: &str = "/dev/uinput"`.

- [ ] **Step 1: Write the failing test**

The ioctls need a real `/dev/uinput` and root, so what is tested here is the part that can be: the request encodings, which are what a wrong `size_of` silently corrupts.

```rust
    #[test]
    fn the_request_numbers_match_the_kernels_own_encoding() {
        // Written out from `linux/uinput.h`. A structure that grew or shrank
        // here would send a request the kernel does not answer, and the only
        // symptom would be a device that never appears.
        assert_eq!(super::UI_DEV_CREATE, 0x5501);
        assert_eq!(super::UI_DEV_SETUP, 0x405c5503);
        assert_eq!(super::UI_ABS_SETUP, 0x401c5504);
        assert_eq!(super::UI_SET_EVBIT, 0x40045564);
        assert_eq!(super::UI_SET_KEYBIT, 0x40045565);
        assert_eq!(super::UI_SET_RELBIT, 0x40045566);
        assert_eq!(super::UI_SET_ABSBIT, 0x40045567);
    }

    #[test]
    fn a_device_that_cannot_be_opened_is_an_error_rather_than_a_panic() {
        let missing = std::path::Path::new("/nonexistent/uinput");

        assert!(super::create(missing).is_err());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl uinput::`
Expected: FAIL — `cannot find value UI_DEV_CREATE in this scope`.

- [ ] **Step 3: Write the implementation**

In `uinput.rs`, adding to the imports `std::{ffi::CString, fs::OpenOptions, os::fd::{AsRawFd, OwnedFd}, path::Path}` and reusing `drm::uapi`'s encoders:

```rust
use crate::drm::uapi::{io_write, io_write_read};

/// Where the kernel's uinput device is.
pub const DEVICE_PATH: &str = "/dev/uinput";

/// The `'U'` every uinput request is built on.
const UINPUT: u32 = 0x55;

/// `_IO(UINPUT_IOCTL_BASE, 1)`.
const UI_DEV_CREATE: libc::c_ulong = (UINPUT << 8 | 1) as libc::c_ulong;
/// `_IOW(UINPUT_IOCTL_BASE, 3, struct uinput_setup)`.
const UI_DEV_SETUP: libc::c_ulong = io_write(UINPUT, 3, size_of::<UinputSetup>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 4, struct uinput_abs_setup)`.
const UI_ABS_SETUP: libc::c_ulong = io_write(UINPUT, 4, size_of::<UinputAbsSetup>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 100, int)`.
const UI_SET_EVBIT: libc::c_ulong = io_write(UINPUT, 100, size_of::<libc::c_int>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 101, int)`.
const UI_SET_KEYBIT: libc::c_ulong = io_write(UINPUT, 101, size_of::<libc::c_int>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 102, int)`.
const UI_SET_RELBIT: libc::c_ulong = io_write(UINPUT, 102, size_of::<libc::c_int>() as u32);
/// `_IOW(UINPUT_IOCTL_BASE, 103, int)`.
const UI_SET_ABSBIT: libc::c_ulong = io_write(UINPUT, 103, size_of::<libc::c_int>() as u32);

/// `BUS_VIRTUAL`, which is what a device with no bus behind it reports.
const BUS_VIRTUAL: u16 = 0x06;

/// The vendor and product these two devices are known by, so that a guest-side
/// rule can name them without matching on a string.
const VENDOR: u16 = 0x564d;
const KEYBOARD_PRODUCT: u16 = 0x0001;
const POINTER_PRODUCT: u16 = 0x0002;

/// `struct input_id`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

/// `struct uinput_setup`.
#[repr(C)]
#[derive(Clone, Copy)]
struct UinputSetup {
    id: InputId,
    name: [libc::c_char; 80],
    ff_effects_max: u32,
}

/// `struct input_absinfo`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputAbsinfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// `struct uinput_abs_setup`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsinfo,
}

/// Creates the guest's keyboard and pointer, in that order.
///
/// The broker's, because `/dev/uinput` is root's. The descriptors are what it
/// hands the session process: while one is held the device exists, and when
/// the last copy closes the kernel unregisters it and releases every key it
/// believed was down. That is what makes a killed session leave no key stuck.
///
/// # Errors
///
/// Whatever the device refused: an absent `/dev/uinput`, a denied open, or a
/// rejected `UI_DEV_CREATE`. The broker logs it and carries on without input.
pub fn create(path: &Path) -> io::Result<(OwnedFd, OwnedFd)> {
    let keyboard = create_keyboard(path)?;
    let pointer = create_pointer(path)?;

    Ok((keyboard, pointer))
}

fn create_keyboard(path: &Path) -> io::Result<OwnedFd> {
    let device = OpenOptions::new().write(true).open(path)?;
    let fd = device.as_raw_fd();

    set_bit(fd, UI_SET_EVBIT, u32::from(EV_KEY))?;
    // Every code the viewer's table can produce, and no more: a device that
    // declares a key it never sends is a device libinput reasons wrongly about.
    for keycode in 1..=u32::from(KEY_MAX_SENT) {
        set_bit(fd, UI_SET_KEYBIT, keycode)?;
    }

    setup(fd, "VMLord Keyboard", KEYBOARD_PRODUCT)?;
    create_device(fd)?;

    Ok(device.into())
}

fn create_pointer(path: &Path) -> io::Result<OwnedFd> {
    let device = OpenOptions::new().write(true).open(path)?;
    let fd = device.as_raw_fd();

    set_bit(fd, UI_SET_EVBIT, u32::from(EV_KEY))?;
    for button in u32::from(BTN_LEFT)..=u32::from(BTN_EXTRA) {
        set_bit(fd, UI_SET_KEYBIT, button)?;
    }
    set_bit(fd, UI_SET_EVBIT, u32::from(EV_ABS))?;
    set_bit(fd, UI_SET_ABSBIT, u32::from(ABS_X))?;
    set_bit(fd, UI_SET_ABSBIT, u32::from(ABS_Y))?;
    set_bit(fd, UI_SET_EVBIT, u32::from(EV_REL))?;
    for axis in [REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
        set_bit(fd, UI_SET_RELBIT, u32::from(axis))?;
    }

    for code in [ABS_X, ABS_Y] {
        let absolute = UinputAbsSetup {
            code,
            absinfo: InputAbsinfo {
                maximum: ABS_RANGE,
                ..InputAbsinfo::default()
            },
        };
        // SAFETY: `absolute` lives across the call, and the request names its
        // own size, which is what the kernel copies.
        let result = unsafe { libc::ioctl(fd, UI_ABS_SETUP, &raw const absolute) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    setup(fd, "VMLord Pointer", POINTER_PRODUCT)?;
    create_device(fd)?;

    Ok(device.into())
}

/// One `UI_SET_*BIT`.
fn set_bit(fd: libc::c_int, request: libc::c_ulong, bit: u32) -> io::Result<()> {
    // SAFETY: an ioctl on a descriptor this function's caller owns, with the
    // integer argument every `UI_SET_*BIT` request takes.
    let result = unsafe { libc::ioctl(fd, request, libc::c_int::try_from(bit).unwrap_or(0)) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// Names the device, which is what a user sees in `libinput list-devices`.
fn setup(fd: libc::c_int, name: &str, product: u16) -> io::Result<()> {
    let mut setup = UinputSetup {
        id: InputId {
            bustype: BUS_VIRTUAL,
            vendor: VENDOR,
            product,
            version: 1,
        },
        name: [0; 80],
        ff_effects_max: 0,
    };
    let text = CString::new(name).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    for (slot, byte) in setup.name.iter_mut().zip(text.as_bytes()) {
        *slot = *byte as libc::c_char;
    }

    // SAFETY: `setup` lives across the call and is the structure the request
    // names the size of.
    let result = unsafe { libc::ioctl(fd, UI_DEV_SETUP, &raw const setup) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

/// `UI_DEV_CREATE`, after which the node exists.
fn create_device(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: an ioctl with no argument on a descriptor the caller owns.
    let result = unsafe { libc::ioctl(fd, UI_DEV_CREATE) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl uinput::`
Expected: PASS, twelve tests. If the encoding assertions fail, the structure sizes are wrong, not the constants: check `UinputSetup` is 92 bytes and `UinputAbsSetup` 28.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/uinput.rs
git commit -m "TASK-119: Create the guest's keyboard and pointer devices"
```

---

### Task 10: Handing the descriptors across

**Files:**
- Modify: `crates/display-services/proto/vmlord/display/broker/broker.proto`
- Modify: `crates/display-services/src/ipc.rs`

**Interfaces:**
- Produces: `ipc::Message::InputDevices`, carrying two descriptors — the keyboard's then the pointer's — as `SCM_RIGHTS`.

- [ ] **Step 1: Write the failing test**

In `ipc.rs`'s test module, beside the existing round-trip tests:

```rust
    #[test]
    fn the_input_devices_message_survives_a_round_trip() {
        let message = Message::InputDevices;

        assert_eq!(decode(&encode(&message)).expect("a message"), message);
    }
```

and in `unix.rs`'s test module, beside `a_message_and_its_descriptors_cross_together`, whose fixtures this reuses:

```rust
    #[test]
    fn two_device_descriptors_cross_in_the_order_they_were_sent() {
        let path = socket_path("devices");
        let listener = Listener::bind(&path, own_gid()).unwrap();

        let client = std::thread::spawn({
            let path = path.clone();
            move || {
                let connection = Connection::connect(&path).unwrap();
                connection.receive().unwrap()
            }
        });

        let server = listener.accept(own_uid()).unwrap();
        let keyboard = memfd("keyboard", b"keys").unwrap();
        let pointer = memfd("pointer", b"buttons").unwrap();
        server
            .send(
                &Message::InputDevices,
                &[keyboard.as_fd(), pointer.as_fd()],
            )
            .unwrap();

        let (message, descriptors) = client.join().unwrap();
        assert_eq!(message, Message::InputDevices);
        assert_eq!(descriptors.len(), 2);

        // Order is the whole contract: the keyboard first, the pointer second.
        let mut received = descriptors.into_iter();
        let mut contents = Vec::new();
        fs::File::from(received.next().unwrap())
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, b"keys");

        contents.clear();
        fs::File::from(received.next().unwrap())
            .read_to_end(&mut contents)
            .unwrap();
        assert_eq!(contents, b"buttons");

        fs::remove_file(&path).unwrap();
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl ipc:: unix::`
Expected: FAIL — `no variant named InputDevices`.

- [ ] **Step 3: Write the implementation**

In the proto, add the arm and the message:

```proto
    InputDevices input_devices = 8;
```

```proto
// The keyboard and the pointer, whose descriptors are attached to this
// datagram in that order. Sent when the broker adopts a peer, and again in
// answer to its Attach. A broker with no input devices never sends it.
message InputDevices {}
```

In `ipc.rs`, add the arm to `Message` with the documentation the others have, and both directions of the mapping in `into_wire` and `from_wire`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl ipc:: unix::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/proto crates/display-services/src/ipc.rs crates/display-services/src/unix.rs
git commit -m "TASK-119: Carry the input devices across the broker socket"
```

---

### Task 11: The broker creates them and hands them over

**Files:**
- Modify: `crates/display-services/src/broker_main.rs`

**Interfaces:**
- Consumes: `uinput::create` (Task 9) and `ipc::Message::InputDevices` (Task 10).
- Produces: nothing new; the broker now owns the two devices for the guest's lifetime.

- [ ] **Step 1: Write the failing test**

In `broker_main.rs`'s test module:

```rust
    #[test]
    fn a_broker_with_no_uinput_carries_on_without_input() {
        // A guest whose kernel has no uinput still shows a desktop. What it
        // must not do is fail to start one.
        let devices = super::open_devices(std::path::Path::new("/nonexistent/uinput"));

        assert!(devices.is_none());
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl broker_main::`
Expected: FAIL — `cannot find function open_devices`.

- [ ] **Step 3: Write the implementation**

Add the option and its default, beside the others in `Options`:

```rust
    /// Where the kernel's uinput device is.
    pub uinput: PathBuf,
```

with `uinput: text("VMLORD_DISPLAY_UINPUT", crate::uinput::DEVICE_PATH).into()` in `from_env`.

Add the opener:

```rust
/// The two input devices, or nothing at all.
///
/// A failure here degrades the display rather than breaking the VM, which is
/// the rule #114 set for the DRM side: a desktop with no keyboard is worth
/// more than a VM that refused to show one.
fn open_devices(path: &std::path::Path) -> Option<(OwnedFd, OwnedFd)> {
    match crate::uinput::create(path) {
        Ok(devices) => Some(devices),
        Err(error) => {
            eprintln!(
                "vmlord-display-broker: this guest has no input devices ({error}); the display will be read-only"
            );

            None
        }
    }
}
```

In `serve`, after the card is found and before the threads start:

```rust
    let devices = open_devices(&options.uinput);
```

Pass `devices.as_ref()` into `serve_peers`, and from there into `adopt_peer` and `read_peer`. In both places that already send `SessionOpened`, send the descriptors first — a session that learns of its devices after its parameters would drop the first records it read:

```rust
    if let Some((keyboard, pointer)) = devices {
        let _ = connection.send(
            &Message::InputDevices,
            &[keyboard.as_fd(), pointer.as_fd()],
        );
    }
```

Report the absence to the host once, where the broker already reports a fault, so that #121's diagnostics can show it:

```rust
    if devices.is_none() {
        state.fault = Some("this guest has no input devices".to_owned());
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl broker_main::`
Expected: PASS, the existing broker tests plus the new one.

- [ ] **Step 5: Build the guest binaries**

Run: `cargo display-services`
Expected: both binaries build.

- [ ] **Step 6: Commit**

```bash
git add crates/display-services/src/broker_main.rs
git commit -m "TASK-119: Create the input devices in the broker and hand them over"
```

---

### Task 12: The session puts the records on the devices

**Files:**
- Modify: `crates/display-services/src/session_main.rs`

**Interfaces:**
- Consumes: `uinput::Keyboard`, `uinput::Pointer` (Task 8), `ipc::Message::InputDevices` (Task 10).
- Produces: nothing new; `read_input` stops discarding.

- [ ] **Step 1: Write the failing tests**

The harness already speaks to the session over a real socket, so the devices can be real pipes and the test can read what came out. In `session_main.rs`'s test module, add to the world:

```rust
        /// The read ends of the two device pipes, once they have been sent.
        keyboard: Option<std::io::PipeReader>,
        pointer: Option<std::io::PipeReader>,

        /// Hands the session a keyboard and a pointer, as the broker does.
        fn broker_sends_input_devices(&mut self) {
            let (keyboard_reader, keyboard_writer) = std::io::pipe().expect("a pipe");
            let (pointer_reader, pointer_writer) = std::io::pipe().expect("a pipe");
            self.broker
                .send(
                    &Message::InputDevices,
                    &[keyboard_writer.as_fd(), pointer_writer.as_fd()],
                )
                .expect("the devices go out");
            self.keyboard = Some(keyboard_reader);
            self.pointer = Some(pointer_reader);
        }

        /// The `(type, code, value)` triples the keyboard has been sent.
        fn keyboard_events(&mut self) -> Vec<(u16, u16, i32)> {
            Self::device_events(self.keyboard.as_mut())
        }

        /// The same for the pointer.
        fn pointer_events(&mut self) -> Vec<(u16, u16, i32)> {
            Self::device_events(self.pointer.as_mut())
        }

        /// Whatever is on a device pipe right now, and no waiting.
        ///
        /// Non-blocking, because a device with nothing on it is what half of
        /// these tests assert.
        fn device_events(reader: Option<&mut std::io::PipeReader>) -> Vec<(u16, u16, i32)> {
            let Some(reader) = reader else {
                return Vec::new();
            };
            set_nonblocking(reader.as_raw_fd()).expect("a non-blocking pipe");

            let mut bytes = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => bytes.extend_from_slice(&chunk[..read]),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("a device pipe failed: {error}"),
                }
            }

            bytes
                .chunks_exact(24)
                .map(|event| {
                    (
                        u16::from_ne_bytes([event[16], event[17]]),
                        u16::from_ne_bytes([event[18], event[19]]),
                        i32::from_ne_bytes([event[20], event[21], event[22], event[23]]),
                    )
                })
                .collect()
        }
```

`set_nonblocking` is the helper this module already uses on the frame socket; `keyboard` and `pointer` are the two fields added above.

Then the tests:

```rust
    #[test]
    fn a_key_event_reaches_the_keyboard_device() {
        let mut world = World::with_session();
        world.broker_sends_input_devices();
        world.open_input_socket();
        world.settle();

        world.host_sends_key(30, true);
        world.settle();

        assert_eq!(
            world.keyboard_events(),
            vec![(1, 30, 1), (0, 0, 0)],
            "a key press and the report that closes it"
        );
    }

    #[test]
    fn a_pointer_motion_is_scaled_by_the_session_geometry() {
        let mut world = World::with_session();
        world.broker_sends_input_devices();
        world.open_input_socket();
        world.settle();

        world.host_sends_motion(WIDTH - 1, HEIGHT - 1);
        world.settle();

        let events = world.pointer_events();
        assert_eq!(events[0], (3, 0, 32767));
        assert_eq!(events[1], (3, 1, 32767));
    }

    #[test]
    fn a_record_from_a_stale_generation_never_reaches_a_device() {
        let mut world = World::with_session();
        world.broker_sends_input_devices();
        world.open_input_socket();
        world.settle();

        world.host_sends_input_with_generation(0);
        world.settle();

        assert!(world.input_socket_is_closed());
        assert!(
            world.keyboard_events().is_empty(),
            "a record from a connection that was replaced must not reach a device"
        );
    }

    #[test]
    fn a_lost_input_channel_releases_what_the_guest_holds() {
        let mut world = World::with_session();
        world.broker_sends_input_devices();
        world.open_input_socket();
        world.settle();
        world.host_sends_key(30, true);
        world.settle();
        let _ = world.keyboard_events();

        world.host_closes_input_socket();
        world.settle();

        assert_eq!(
            world.keyboard_events(),
            vec![(1, 30, 0), (0, 0, 0)],
            "a channel that went must leave no key down"
        );
    }

    #[test]
    fn a_release_all_record_releases_both_devices() {
        let mut world = World::with_session();
        world.broker_sends_input_devices();
        world.open_input_socket();
        world.settle();
        world.host_sends_key(30, true);
        world.host_sends_button(0x110, true);
        world.settle();
        let _ = (world.keyboard_events(), world.pointer_events());

        world.host_sends_release_all();
        world.settle();

        assert_eq!(world.keyboard_events(), vec![(1, 30, 0), (0, 0, 0)]);
        assert_eq!(world.pointer_events(), vec![(1, 0x110, 0), (0, 0, 0)]);
    }

    #[test]
    fn a_session_with_no_devices_still_consumes_its_records() {
        // A guest whose broker found no uinput reads the channel and drops it,
        // rather than letting unread records stall the socket.
        let mut world = World::with_session();
        world.open_input_socket();
        world.settle();

        world.host_sends_key(30, true);
        world.settle();

        assert!(!world.input_socket_is_closed());
        assert_eq!(world.input_records_consumed(), 1);
    }
```

Extend the existing `send_input` helper with `host_sends_motion`, `host_sends_button`, `host_sends_release_all` and `host_closes_input_socket`, following how `host_sends_key` is written.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl session_main::`
Expected: FAIL — `no method named broker_sends_input_devices`.

- [ ] **Step 3: Write the implementation**

Give `Loop` the two devices:

```rust
    /// The guest's keyboard, once the broker has handed it over. `None` on a
    /// guest whose kernel has no uinput, where input is dropped rather than
    /// applied.
    keyboard: Option<Keyboard<File>>,
    /// Its pointer, on the same terms.
    pointer: Option<Pointer<File>>,
```

initialised to `None` in `new`, and adopted in `read_broker`:

```rust
            Message::InputDevices => {
                let mut descriptors = descriptors.into_iter();
                match (descriptors.next(), descriptors.next()) {
                    (Some(keyboard), Some(pointer)) => {
                        self.keyboard = Some(Keyboard::new(File::from(keyboard)));
                        self.pointer = Some(Pointer::new(File::from(pointer)));
                    }
                    _ => eprintln!(
                        "vmlord-display-session: the broker offered input devices without their descriptors"
                    ),
                }

                Ok(None)
            }
```

Replace `read_input`'s body after the generation check and the counter:

```rust
                self.input_records += 1;
                self.apply_input(header.message_type, &payload);
```

and add the two methods:

```rust
    /// Puts one input record on the devices, if there are any.
    ///
    /// A record type this build has no name for is ignored rather than
    /// refused: the protocol's forward-compatibility rule is that an unknown
    /// message changes nothing.
    fn apply_input(&mut self, message_type: u16, payload: &[u8]) {
        let Ok(record) = InputRecord::try_from(i32::from(message_type)) else {
            return;
        };
        let Some(parameters) = self.parameters.as_ref() else {
            return;
        };
        let (width, height) = (parameters.width, parameters.height);

        let applied = match record {
            InputRecord::KeyEvent => KeyEvent::decode(payload).map(|event| {
                self.keyboard.as_mut().map_or(Ok(()), |keyboard| {
                    keyboard.key(u16::try_from(event.keycode).unwrap_or(0), event.pressed)
                })
            }),
            InputRecord::PointerMotion => PointerMotion::decode(payload).map(|motion| {
                self.pointer.as_mut().map_or(Ok(()), |pointer| {
                    pointer.motion(motion.x, motion.y, width, height)
                })
            }),
            InputRecord::PointerButton => PointerButton::decode(payload).map(|event| {
                self.pointer.as_mut().map_or(Ok(()), |pointer| {
                    pointer.button(u16::try_from(event.button).unwrap_or(0), event.pressed)
                })
            }),
            InputRecord::PointerScroll => PointerScroll::decode(payload).map(|scroll| {
                self.pointer.as_mut().map_or(Ok(()), |pointer| {
                    pointer.scroll(scroll.horizontal, scroll.vertical)
                })
            }),
            InputRecord::ReleaseAll => {
                self.release_input();

                return;
            }
            _ => return,
        };

        match applied {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                eprintln!("vmlord-display-session: an input device refused an event: {error}");
            }
            Err(error) => {
                eprintln!("vmlord-display-session: an input record would not decode: {error}");
            }
        }
    }

    /// Releases everything both devices believe is held.
    fn release_input(&mut self) {
        if let Some(keyboard) = self.keyboard.as_mut()
            && let Err(error) = keyboard.release_all()
        {
            eprintln!("vmlord-display-session: the keyboard would not release: {error}");
        }
        if let Some(pointer) = self.pointer.as_mut()
            && let Err(error) = pointer.release_all()
        {
            eprintln!("vmlord-display-session: the pointer would not release: {error}");
        }
    }
```

Call `self.release_input();` as the first statement of `close_input`, and update that method's documentation and `read_input`'s — the comment saying a device is a later task's is no longer true.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl`
Expected: PASS, the crate's whole suite including the six new tests. The existing `an_input_record_is_read_and_dropped` now describes a session with no devices — rename it to say so rather than deleting it.

- [ ] **Step 5: Commit**

```bash
git add crates/display-services/src/session_main.rs
git commit -m "TASK-119: Put input records on the guest's devices"
```

---

### Task 13: The documentation and the whole suite

**Files:**
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: everything above.

- [ ] **Step 1: Update the architecture**

Find the display sections `#115` and `#117` added. Record, in the same voice as the sections around them:

* the two guest devices, why there are two, and that the broker creates them and hands the descriptors over;
* that a killed session leaves no key held because the kernel releases them when the last descriptor closes;
* the viewer's split: `placement.rs` and `input/` decide, `window.rs` and `hook.rs` catch;
* that scan codes, not virtual keys, carry the key — and that the layout is the guest's;
* the reserved `Ctrl+Alt+Left Shift`, and that `Ctrl+Alt+Del` is a menu action because the Secure Attention Sequence is not interceptable;
* that a guest with no `/dev/uinput` shows a read-only desktop rather than failing to start.

Check the compatibility matrix and the troubleshooting section for anything that says the native display has no input, and correct it.

- [ ] **Step 2: Run everything**

Run: `cargo test -p vmlord-display-services --target x86_64-unknown-linux-musl`
Expected: PASS.

Run: `cargo test-windows -p vmlord-display-viewer`
Expected: PASS.

Run: `cargo check-windows`
Expected: no errors, no new warnings.

Run: `cargo display-services`
Expected: both guest binaries build.

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md
git commit -m "TASK-119: Document keyboard and mouse input"
```

- [ ] **Step 4: The manual checks the task requires**

These cannot be automated here and are what the task asks for by name. Run them on a guest with the display payload installed, and record the result in the merge request:

1. **GDM login.** Connect a viewer to a guest at the greeter. Type a password, use `Tab` and `Enter`, and log in. The pointer must land where it is drawn, and no key may need a second press.
2. **No key held after a disconnect.** Hold a letter, close the viewer while it is down, reconnect. The guest must not be repeating it, and the letter must type normally.
3. **No key held after a crash.** Hold a letter, `kill -9` the `vmlord-display-session` process, wait for systemd to restart it, reconnect. Same expectation — this is the kernel's release on the last descriptor closing, and it is the check that proves it happens.
4. **`Super` reaches GNOME**, `Alt+Tab` switches windows inside the guest rather than on the host, and `Ctrl+Alt+Left Shift` gives the keyboard back to Windows.
5. **`Ctrl+Alt+Del` from the system menu** reaches the guest.
