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
//! One key is the viewer's rather than the guest's: `F11` toggles full
//! screen, and is swallowed in both directions so the guest never sees half a
//! key. It is a real key in a guest browser, and taking it is the cost of
//! having a full-screen shortcut at all while the keyboard is the guest's.
//!
//! `Ctrl+Alt+Del` is not here and cannot be: the Secure Attention Sequence is
//! routed by the kernel, no hook sees it, and reaching for undocumented means
//! is out of the question. It is a menu action instead.
//!
//! The callback runs on the thread that installed the hook -- the message
//! pump, where by construction nothing blocks. That matters: a low-level hook
//! slower than `LowLevelHooksTimeout` is removed by the system without asking.
//! So it does one thing, reports it, and returns.

use std::{cell::RefCell, sync::Arc};

use windows::Win32::{
    Foundation::{LPARAM, LRESULT, WPARAM},
    UI::Input::KeyboardAndMouse::VK_F11,
    UI::WindowsAndMessaging::{
        CallNextHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, KBDLLHOOKSTRUCT_FLAGS, LLKHF_EXTENDED,
        LLKHF_INJECTED, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN,
        WM_SYSKEYDOWN,
    },
};

use crate::{
    input::Report,
    windows::window::{Shared, UiEvent},
};

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
        let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), None, 0) }.map_err(
            |error| {
                LISTENER.with(|listener| *listener.borrow_mut() = None);

                format!("the keyboard hook was refused: {error}")
            },
        )?;

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
/// what makes `Alt+Tab` and `Super` reach GNOME. The exceptions are an
/// injected event, which belongs to whatever injected it, and `F11`, which is
/// the viewer's own.
fn deliver(make: u16, virtual_key: u16, extended: bool, pressed: bool) -> bool {
    LISTENER.with(|listener| {
        let Some(shared) = listener.borrow().clone() else {
            return false;
        };

        if virtual_key == VK_F11.0 {
            // Both edges are swallowed: a guest sent a press with no release
            // is a guest holding a key nobody is pressing.
            if pressed {
                shared.report(UiEvent::ToggleFullscreen);
            }

            return true;
        }

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
    fn f11_is_the_viewers_own_key_and_never_the_guests() {
        let (events, receiver) = mpsc::channel();
        let shared = Arc::new(Shared::new(events));
        let hook = Hook::install(&shared).expect("a hook");

        assert!(super::deliver(0x57, 0x7a, false, true));
        assert!(
            super::deliver(0x57, 0x7a, false, false),
            "a press the guest never saw must not be followed by a release it did"
        );

        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            vec![UiEvent::ToggleFullscreen]
        );
        drop(hook);
    }

    #[test]
    fn a_key_that_arrives_with_no_hook_installed_is_left_to_windows() {
        assert!(!super::deliver(0x1e, 0x41, false, true));
    }
}
