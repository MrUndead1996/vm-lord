//! The window, and the thread that pumps it.
//!
//! The window lives on the thread that pumps it, which is the process's first
//! thread. Nothing that can block runs there: sockets, decode and the launch
//! pipes are threads of their own, and they reach the window by posting
//! [`WM_SIGNAL`] to it. That is what keeps the buttons on a `Failed` screen
//! alive -- the pump never stopped, because nothing on it can stop.
//!
//! What the window decides is small: which button a click landed on, how big
//! the client area is, and when the user closed it. Everything else is a
//! message on its way somewhere else.

use std::{
    mem::ManuallyDrop,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
};

use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{CreateSolidBrush, UpdateWindow},
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            AdjustWindowRect, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow,
            DispatchMessageW, GWLP_USERDATA, GetClientRect, GetWindowLongPtrW, IDC_ARROW,
            LoadCursorW, MB_ICONERROR, MB_OK, MSG, MessageBoxW, PM_REMOVE, PeekMessageW,
            PostMessageW, PostQuitMessage, RegisterClassW, SW_RESTORE, SW_SHOW,
            SetForegroundWindow, SetWindowLongPtrW, ShowWindow, TranslateMessage, WINDOW_EX_STYLE,
            WM_APP, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_LBUTTONUP, WM_QUIT, WM_SIZE, WNDCLASSW,
            WS_OVERLAPPEDWINDOW,
        },
    },
    core::{HSTRING, PCWSTR},
};

use crate::status::{self, Button};

/// The session thread has something for the window to draw.
pub const WM_SIGNAL: u32 = WM_APP + 1;

/// Another VMLord asked this window to come forward.
pub const WM_FOCUS_REQUEST: u32 = WM_APP + 2;

/// Another VMLord asked this window to close.
pub const WM_CLOSE_REQUEST: u32 = WM_APP + 3;

/// The class every viewer window is registered under.
const CLASS_NAME: &str = "VMLordDisplayWindow";

/// What the window procedure and the rest of the process share.
///
/// The window procedure runs on the pump's thread and must not wait on
/// anything, so what it needs is a flag it can read and a channel it can drop
/// an event into.
pub struct Shared {
    /// Whether the overlay is showing a failed screen with its two buttons.
    ///
    /// A click is only ever a button while this is set: with the picture on
    /// screen there is nothing to press.
    pub failed: AtomicBool,
    events: Sender<UiEvent>,
}

impl Shared {
    /// What the window reports through, for a reader on another thread.
    #[must_use]
    pub fn new(events: Sender<UiEvent>) -> Self {
        Self {
            failed: AtomicBool::new(false),
            events,
        }
    }

    /// Reports one event, dropping it if nobody is listening any more.
    ///
    /// A window whose reader is gone is one that is closing, and blocking the
    /// pump to say so would be the wrong answer to it.
    fn report(&self, event: UiEvent) {
        if self.events.send(event).is_err() {
            log::debug!("a {event:?} had nowhere to go: the session is already over");
        }
    }
}

/// Something the user did to the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiEvent {
    /// A button on the failed screen was pressed.
    Pressed(Button),
    /// The client area is now this big.
    Resized(i32, i32),
    /// The user closed the window.
    Closing,
}

/// One viewer window.
pub struct Window {
    hwnd: HWND,
    /// Kept so that the shared state outlives every message the window handles.
    shared: Arc<Shared>,
}

impl Window {
    /// Opens a window whose *client* area is `width` by `height`.
    ///
    /// # Errors
    ///
    /// A message naming the Win32 call that refused.
    pub fn open(title: &str, width: i32, height: i32, shared: Arc<Shared>) -> Result<Self, String> {
        let class = HSTRING::from(CLASS_NAME);
        register_class(&class)?;

        // The size asked for is the client area; this is what the frame around
        // it adds.
        let mut rectangle = RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        // SAFETY: `rectangle` is a valid `RECT` living across the call.
        unsafe { AdjustWindowRect(&raw mut rectangle, WS_OVERLAPPEDWINDOW, false) }
            .map_err(|error| format!("the window size could not be worked out: {error}"))?;

        let title = HSTRING::from(title);
        // SAFETY: both strings are NUL-terminated and live across the call, and
        // the class was registered above.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rectangle.right - rectangle.left,
                rectangle.bottom - rectangle.top,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|error| format!("the window could not be created: {error}"))?;

        // The window procedure reads this back on every message. The `Arc` is
        // released in `WM_DESTROY`, which is the last message a window sees.
        let pointer = Arc::into_raw(Arc::clone(&shared)) as isize;
        // SAFETY: `hwnd` was just created and `pointer` came from
        // `Arc::into_raw`, so it is valid until `WM_DESTROY` takes it back.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, pointer) };

        // SAFETY: `hwnd` is a window this process owns.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = UpdateWindow(hwnd);
        }

        Ok(Self { hwnd, shared })
    }

    /// The window itself, for the renderer's swapchain.
    #[must_use]
    pub fn handle(&self) -> HWND {
        self.hwnd
    }

    /// The shared state, for the parts of the process that set `failed`.
    #[must_use]
    pub fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

    /// How big the client area is now.
    #[must_use]
    pub fn client_size(&self) -> (i32, i32) {
        client_size(self.hwnd)
    }

    /// Brings the window forward. What a repeated Connect means.
    pub fn focus(&self) {
        // SAFETY: `self.hwnd` is a window this process owns.
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }

    /// A handle another thread may wake this window through.
    #[must_use]
    pub fn poster(&self) -> Poster {
        Poster(self.hwnd.0 as isize)
    }

    /// Runs the message queue dry.
    ///
    /// Answers `false` once the window has gone, which is what ends the
    /// process's main loop.
    #[must_use]
    pub fn pump(&self) -> bool {
        let mut message = MSG::default();

        loop {
            // SAFETY: `message` is a valid `MSG` living across the call, and
            // `None` asks for every window of this thread.
            let waiting = unsafe { PeekMessageW(&raw mut message, None, 0, 0, PM_REMOVE) };
            if !waiting.as_bool() {
                return true;
            }
            if message.message == WM_QUIT {
                return false;
            }

            // SAFETY: `message` was just filled in by `PeekMessageW`.
            unsafe {
                let _ = TranslateMessage(&raw const message);
                DispatchMessageW(&raw const message);
            }
        }
    }
}

/// A way to wake one window from another thread.
///
/// The handle travels as a number rather than an `HWND`, which is a raw
/// pointer and so is not `Send`.
pub struct Poster(isize);

// SAFETY: a window handle is process-wide, and `PostMessageW` is the documented
// way to reach a window from a thread that does not own it.
unsafe impl Send for Poster {}

impl Poster {
    /// Puts one message on the window's queue.
    pub fn post(&self, message: u32) {
        // SAFETY: the handle names a window of this process; a window that has
        // already gone makes this fail, which is nothing to act on.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.0 as *mut core::ffi::c_void)),
                message,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// Puts one message in front of the user and waits for them to dismiss it.
///
/// The only thing a viewer that cannot start has to say. A message box rather
/// than a log line: a program with no console and no window has nowhere else to
/// put it.
pub fn report(message: &str) {
    let text = HSTRING::from(message);
    let title = HSTRING::from("VMLord Display");

    // SAFETY: both strings are NUL-terminated and live across the call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_ICONERROR | MB_OK,
        );
    }
}

/// Registers the window class, once for the process.
fn register_class(class: &HSTRING) -> Result<(), String> {
    use std::sync::OnceLock;
    static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();

    REGISTERED
        .get_or_init(|| {
            // SAFETY: `None` asks for this process's own module handle.
            let instance = unsafe { GetModuleHandleW(None) }
                .map_err(|error| format!("this process has no module handle: {error}"))?;

            // SAFETY: `IDC_ARROW` is a built-in cursor and needs no module.
            let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }
                .map_err(|error| format!("the arrow cursor could not be loaded: {error}"))?;

            let descriptor = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance.into(),
                // Black, so that a window with no frame yet is not a hole in
                // the desktop.
                // SAFETY: a plain GDI object creation.
                hbrBackground: unsafe { CreateSolidBrush(COLORREF(0)) },
                hCursor: cursor,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };

            // SAFETY: `descriptor` is a valid `WNDCLASSW` whose strings outlive
            // the call.
            if unsafe { RegisterClassW(&raw const descriptor) } == 0 {
                return Err("the window class could not be registered".to_owned());
            }

            Ok(())
        })
        .clone()
}

/// The client area of one window.
fn client_size(hwnd: HWND) -> (i32, i32) {
    let mut rectangle = RECT::default();
    // SAFETY: `hwnd` names a window of this process and `rectangle` lives
    // across the call.
    if unsafe { GetClientRect(hwnd, &raw mut rectangle) }.is_err() {
        return (0, 0);
    }

    (
        rectangle.right - rectangle.left,
        rectangle.bottom - rectangle.top,
    )
}

/// What the window does with each message.
///
/// The shared state is read back from `GWLP_USERDATA` without taking ownership:
/// the `Arc` this pointer came from belongs to the window until `WM_DESTROY`.
extern "system" fn wnd_proc(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: `hwnd` names a window of this process. The value is either zero,
    // before `Window::open` has set it, or a pointer from `Arc::into_raw`.
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    if pointer == 0 {
        // SAFETY: the default handler, for the messages that arrive before the
        // window has any state of its own.
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }

    // SAFETY: the pointer came from `Arc::into_raw` and the `Arc` it names is
    // alive until `WM_DESTROY` reclaims it. `ManuallyDrop` is what keeps this
    // borrow from releasing a reference the window still holds.
    let shared = ManuallyDrop::new(unsafe { Arc::from_raw(pointer as *const Shared) });

    match message {
        WM_LBUTTONUP => {
            // A click is a button only while the failed screen is up: with the
            // guest's picture on screen there is nothing to press.
            if shared.failed.load(Ordering::Relaxed) {
                let (width, height) = client_size(hwnd);
                let x = (lparam.0 & 0xffff) as i16 as i32;
                let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
                if let Some(button) = status::hit_test(width, height, x, y) {
                    shared.report(UiEvent::Pressed(button));
                }
            }
            LRESULT(0)
        }
        WM_SIZE => {
            let width = (lparam.0 & 0xffff) as i32;
            let height = ((lparam.0 >> 16) & 0xffff) as i32;
            shared.report(UiEvent::Resized(width, height));
            LRESULT(0)
        }
        WM_CLOSE => {
            shared.report(UiEvent::Closing);
            // SAFETY: `hwnd` names a window of this process.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: the window is going; nothing reads this pointer again.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            // SAFETY: the reference `Window::open` handed the window, released
            // exactly once, here.
            drop(unsafe { Arc::from_raw(pointer as *const Shared) });
            // SAFETY: a plain post to this thread's own queue.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // The renderer paints every pixel of the client area, and letting
        // Windows erase first is a flash of black on every resize.
        WM_ERASEBKGND => LRESULT(1),
        // SAFETY: the default handler, for everything this window does not act
        // on itself.
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering, mpsc};

    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::{SendMessageW, WM_CLOSE, WM_LBUTTONUP},
    };

    use super::{Shared, UiEvent, WM_SIGNAL, Window};
    use crate::status::{self, Button};

    fn shared() -> (Arc<Shared>, mpsc::Receiver<UiEvent>) {
        let (events, received) = mpsc::channel();
        (Arc::new(Shared::new(events)), received)
    }

    /// Everything reported so far.
    ///
    /// Showing a window reports its first `Resized`, so a test asserts on the
    /// event it is about rather than on an empty channel.
    fn drain(events: &mpsc::Receiver<UiEvent>) -> Vec<UiEvent> {
        events.try_iter().collect()
    }

    #[test]
    fn a_window_opens_at_the_size_it_was_asked_for() {
        let (shared, _events) = shared();
        let window = Window::open("test - VMLord Display", 640, 480, shared)
            .expect("a window class and a window");

        assert_eq!(window.client_size(), (640, 480));
    }

    #[test]
    fn a_posted_signal_reaches_the_pump() {
        let (shared, _events) = shared();
        let window = Window::open("test - VMLord Display", 320, 240, shared).expect("a window");
        let poster = window.poster();

        poster.post(WM_SIGNAL);

        // The pump runs the queue dry and answers whether the window is still
        // open. A posted signal is not a quit.
        assert!(window.pump());
    }

    #[test]
    fn a_click_on_retry_is_reported_only_while_the_failed_screen_is_up() {
        let (shared, events) = shared();
        let window =
            Window::open("test - VMLord Display", 800, 600, Arc::clone(&shared)).expect("a window");
        let (_, (x, y, w, h)) = status::buttons(800, 600)[0];
        let point = isize::try_from(((y + h / 2) << 16) | (x + w / 2)).expect("a client point");

        // Nothing is on screen but the picture: a click is not a button.
        // SAFETY: the window is open and owned by this test.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_LBUTTONUP,
                Some(WPARAM(0)),
                Some(LPARAM(point)),
            );
        }
        assert!(
            !drain(&events)
                .iter()
                .any(|event| matches!(event, UiEvent::Pressed(_))),
            "a click on the guest's picture is not a button"
        );

        shared.failed.store(true, Ordering::Relaxed);
        // SAFETY: as above.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_LBUTTONUP,
                Some(WPARAM(0)),
                Some(LPARAM(point)),
            );
        }

        assert!(drain(&events).contains(&UiEvent::Pressed(Button::Retry)));
    }

    #[test]
    fn closing_the_window_is_reported_before_the_pump_ends() {
        let (shared, events) = shared();
        let window = Window::open("test - VMLord Display", 320, 240, shared).expect("a window");

        // SAFETY: the window is open and owned by this test.
        unsafe {
            SendMessageW(window.handle(), WM_CLOSE, None, None);
        }

        assert!(drain(&events).contains(&UiEvent::Closing));
    }
}
