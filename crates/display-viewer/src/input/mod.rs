//! What the user does, turned into what the guest is told.

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

#[cfg(test)]
mod tests {
    use super::{BTN_LEFT, BTN_RIGHT, Event, Policy, Report};
    use crate::{input::keymap, placement::place};

    /// A policy over an 800x600 guest in a window of the same size, focused.
    ///
    /// The settled state: the window drives the guest's mode, so once a resize
    /// has been answered the picture is the client area at 1:1. A point beyond
    /// it is one a captured drag reported from outside the window.
    fn focused() -> Policy {
        let mut policy = Policy::new();
        policy.set_placement(place(800, 600, 800, 600));
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
        assert!(
            policy.drain().is_empty(),
            "an unfocused window is not typing"
        );

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
        assert!(
            !policy.keyboard_release_requested(),
            "the request is taken once"
        );
    }

    #[test]
    fn the_secure_attention_sequence_is_three_presses_and_three_releases() {
        let mut policy = focused();
        policy.report(Report::SecureAttention);

        assert_eq!(
            policy.drain(),
            vec![
                Event::Key {
                    keycode: keymap::KEY_LEFTCTRL,
                    pressed: true
                },
                Event::Key {
                    keycode: keymap::KEY_LEFTALT,
                    pressed: true
                },
                Event::Key {
                    keycode: keymap::KEY_DELETE,
                    pressed: true
                },
                Event::Key {
                    keycode: keymap::KEY_DELETE,
                    pressed: false
                },
                Event::Key {
                    keycode: keymap::KEY_LEFTALT,
                    pressed: false
                },
                Event::Key {
                    keycode: keymap::KEY_LEFTCTRL,
                    pressed: false
                },
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
        policy.report(Report::Button {
            button: BTN_LEFT,
            pressed: true,
        });
        assert!(policy.drain().is_empty());

        policy.report(Report::Pointer { x: 100, y: 20 });
        policy.report(Report::Button {
            button: BTN_LEFT,
            pressed: true,
        });
        assert_eq!(
            policy.drain(),
            vec![
                Event::Motion { x: 100, y: 20 },
                Event::Button {
                    button: BTN_LEFT,
                    pressed: true
                },
            ]
        );
    }

    #[test]
    fn a_drag_that_leaves_the_picture_keeps_moving_along_its_edge() {
        let mut policy = focused();
        policy.report(Report::Pointer { x: 100, y: 20 });
        policy.report(Report::Button {
            button: BTN_LEFT,
            pressed: true,
        });
        let _ = policy.drain();

        policy.report(Report::PointerLeft);
        policy.report(Report::Pointer { x: 2000, y: 900 });
        assert_eq!(policy.drain(), vec![Event::Motion { x: 799, y: 599 }]);

        policy.report(Report::Button {
            button: BTN_LEFT,
            pressed: false,
        });
        assert_eq!(
            policy.drain(),
            vec![Event::Button {
                button: BTN_LEFT,
                pressed: false
            }]
        );

        // With nothing held, motion off the picture stops again.
        policy.report(Report::Pointer { x: 2000, y: 900 });
        assert!(policy.drain().is_empty());
    }

    #[test]
    fn a_point_on_a_letterbox_bar_is_not_a_point_on_the_desktop() {
        // What a window that has been dragged and not yet answered looks like:
        // the picture is centred with ground at two edges, and the ground is
        // not the guest's.
        let mut policy = Policy::new();
        policy.set_placement(place(800, 600, 1000, 600));
        policy.report(Report::FocusGained);
        let _ = policy.drain();

        policy.report(Report::Pointer { x: 50, y: 300 });
        assert!(policy.drain().is_empty(), "the left bar is not the desktop");

        policy.report(Report::Pointer { x: 100, y: 300 });
        assert_eq!(policy.drain(), vec![Event::Motion { x: 0, y: 300 }]);
    }

    #[test]
    fn a_release_of_a_button_that_was_never_pressed_is_dropped() {
        let mut policy = focused();
        policy.report(Report::Button {
            button: BTN_RIGHT,
            pressed: false,
        });

        assert!(policy.drain().is_empty());
    }

    #[test]
    fn the_wheel_is_forwarded_over_the_picture_in_the_units_it_arrived_in() {
        let mut policy = focused();
        policy.report(Report::Pointer { x: 100, y: 20 });
        let _ = policy.drain();

        policy.report(Report::Wheel {
            horizontal: 0,
            vertical: -240,
        });
        assert_eq!(
            policy.drain(),
            vec![Event::Scroll {
                horizontal: 0,
                vertical: -240
            }]
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
        policy.report(Report::Button {
            button: BTN_LEFT,
            pressed: true,
        });

        assert!(policy.drain().is_empty());
    }
}
