//! Set-1 scan codes to evdev keycodes.
//!
//! Not virtual keys: a virtual key has already had the *host's* layout applied
//! to it, and the guest then applies its own, so a host on a non-US layout
//! sends the wrong keys and breaks `Ctrl`+letter. A scan code is a position on
//! the keyboard, and evdev's low keycodes were numbered after this very set,
//! which is why most of this file is a range rather than a table.

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
        0x1c => 96,        // Keypad Enter
        0x1d => 97,        // Right Ctrl
        0x20 => 113,       // Mute
        0x2e => 114,       // Volume down
        0x30 => 115,       // Volume up
        0x35 => 98,        // Keypad /
        0x38 => 100,       // Right Alt
        0x46 => KEY_PAUSE, // Ctrl+Break
        0x47 => 102,       // Home
        0x48 => 103,       // Up
        0x49 => 104,       // Page Up
        0x4b => 105,       // Left
        0x4d => 106,       // Right
        0x4f => 107,       // End
        0x50 => 108,       // Down
        0x51 => 109,       // Page Down
        0x52 => 110,       // Insert
        0x53 => 111,       // Delete
        0x5b => 125,       // Left Super
        0x5c => 126,       // Right Super
        0x5d => 127,       // Menu
        _ => return None,
    })
}

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
