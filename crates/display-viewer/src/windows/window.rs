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
    cell::OnceCell,
    mem::ManuallyDrop,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::Sender,
    },
};

use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            CreateSolidBrush, EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR,
            MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow, UpdateWindow,
        },
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            },
            LibraryLoader::GetModuleHandleW,
        },
        UI::{
            Controls::WM_MOUSELEAVE,
            HiDpi::{
                AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                GetDpiForWindow, SetProcessDpiAwarenessContext,
            },
            Input::KeyboardAndMouse::{
                ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
            },
            Shell::{ITaskbarList2, TaskbarList},
            WindowsAndMessaging::{
                AdjustWindowRect, AppendMenuW, CW_USEDEFAULT, CheckMenuItem, CheckMenuRadioItem,
                CreatePopupMenu, CreateWindowExW, DefWindowProcW, DeleteMenu, DestroyMenu,
                DestroyWindow, DispatchMessageW, GWL_EXSTYLE, GWL_STYLE, GWLP_USERDATA,
                GetClientRect, GetSystemMenu, GetWindowLongPtrW, GetWindowPlacement, GetWindowRect,
                HMENU, HWND_TOP, IDC_ARROW, IsIconic, LoadCursorW, MB_ICONERROR, MB_OK,
                MF_BYCOMMAND, MF_BYPOSITION, MF_CHECKED, MF_POPUP, MF_SEPARATOR, MF_STRING,
                MF_UNCHECKED, MONITORINFOF_PRIMARY, MSG, MessageBoxW, PM_REMOVE, PeekMessageW,
                PostMessageW, PostQuitMessage, RegisterClassW, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL,
                SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER,
                SetForegroundWindow, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos,
                ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WINDOWPLACEMENT,
                WM_APP, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ERASEBKGND,
                WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
                WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOVE, WM_QUIT, WM_RBUTTONDOWN,
                WM_RBUTTONUP, WM_SETFOCUS, WM_SIZE, WM_SYSCOMMAND, WM_XBUTTONDOWN, WM_XBUTTONUP,
                WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_POPUP,
            },
        },
    },
    core::{HSTRING, PCWSTR},
};

use crate::{
    display_modes::{DisplayMode, MAX_MENU_MODES, label, menu_command, menu_index},
    fullscreen::{self, Frame},
    input::{self, Report},
    monitors::{self, opening_position},
    state::{Quality, WindowState},
    status::{self, Button},
};

/// The session thread has something for the window to draw.
pub const WM_SIGNAL: u32 = WM_APP + 1;

/// Another VMLord asked this window to come forward.
pub const WM_FOCUS_REQUEST: u32 = WM_APP + 2;

/// Another VMLord asked this window to close.
pub const WM_CLOSE_REQUEST: u32 = WM_APP + 3;

/// The system-menu item that sends `Ctrl+Alt+Del` to the guest.
pub const SC_SEND_SAS: usize = 0x9010;

/// The one that hands the keyboard back to Windows.
pub const SC_RELEASE_KEYBOARD: usize = 0x9020;

/// The one that fills the monitor, and leaves it again.
///
/// `WM_SYSCOMMAND` masks the low four bits off a command, so every id here is
/// a multiple of sixteen below `0xF000`, where the system's own live.
pub const SC_FULLSCREEN: usize = 0x9030;

/// Let the viewer choose the encoding mode.
pub const SC_QUALITY_AUTO: usize = 0x9040;

/// Encode a desktop, whatever the picture is doing.
pub const SC_QUALITY_DESKTOP: usize = 0x9050;

/// Silences the guest's sound, or lets it play again.
pub const SC_MUTE_AUDIO: usize = 0x9060;

/// The submenu the host monitor's modes are offered in.
///
/// Its own popup rather than more items on the system menu: a monitor with
/// thirty modes would otherwise bury Full screen and the quality items under
/// a list nobody reads.
const RESOLUTION_MENU: &str = "Resolution";

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
    /// Whether `TrackMouseEvent` is armed, so that it is armed once per entry.
    tracking: AtomicBool,
    /// Which buttons are down, one bit each, so that the capture is released
    /// when the last of them lifts rather than when the first does.
    buttons: AtomicU32,
    /// The modes the resolution submenu currently offers, in menu order.
    ///
    /// A lock the pump takes, which everything else in this module refuses to
    /// do -- and it is safe here because the only other holder is the main
    /// loop that rebuilds the menu, which is the thread that pumps. Nothing
    /// waits on this: it is a short read of at most 32 modes.
    modes: Mutex<Vec<DisplayMode>>,
    events: Sender<UiEvent>,
}

impl Shared {
    /// What the window reports through, for a reader on another thread.
    #[must_use]
    pub fn new(events: Sender<UiEvent>) -> Self {
        Self {
            failed: AtomicBool::new(false),
            tracking: AtomicBool::new(false),
            buttons: AtomicU32::new(0),
            modes: Mutex::new(Vec::new()),
            events,
        }
    }

    /// Reports one event, dropping it if nobody is listening any more.
    ///
    /// A window whose reader is gone is one that is closing, and blocking the
    /// pump to say so would be the wrong answer to it.
    ///
    /// The keyboard hook reports through it too: it watches the same window
    /// and its keys belong in the same queue as that window's mouse.
    pub(crate) fn report(&self, event: UiEvent) {
        if self.events.send(event).is_err() {
            tracing::debug!("a {event:?} had nowhere to go: the session is already over");
        }
    }
}

/// Something the user did to the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiEvent {
    /// A button on the failed screen was pressed.
    Pressed(Button),
    /// The client area is now this big, in physical pixels.
    Resized(i32, i32),
    /// The restored window's top left is now here, in virtual-desktop pixels.
    ///
    /// Reported only for a window that is neither full screen nor maximised:
    /// what is remembered between sessions is where the user put the window,
    /// not the monitor a full-screen one happens to be covering.
    Moved(i32, i32),
    /// The user asked to fill the monitor, or to stop.
    ToggleFullscreen,
    /// The user picked an encoding mode from the system menu.
    Quality(Quality),
    /// The user turned the guest's sound off, or back on.
    ToggleMute,
    /// The user picked a resolution from the system menu.
    DisplayMode(DisplayMode),
    /// The monitor the window is on may not be the one it was on.
    ///
    /// A signal rather than a snapshot: what the monitor now is takes a
    /// dozen Win32 calls, and nothing that slow runs on the message pump.
    MonitorChanged,
    /// The user closed the window.
    Closing,
    /// Something the user did with the keyboard or the mouse.
    Input(Report),
}

/// One viewer window.
pub struct Window {
    hwnd: HWND,
    /// Kept so that the shared state outlives every message the window handles.
    shared: Arc<Shared>,
    /// The frame and the placement to go back to, while the window is filling
    /// a monitor. `None` when it is not.
    ///
    /// Both are read off the window on the way in and put back untouched on
    /// the way out: the placement carries the restored rectangle *and* whether
    /// the window was maximised, and the frame carries the two style words
    /// exactly as they were, extended styles included.
    restore: Option<(Frame, WINDOWPLACEMENT)>,
    /// The resolution submenu, owned by the system menu it hangs off.
    resolution: HMENU,
}

impl Window {
    /// Opens a window where `state` left one.
    ///
    /// The size in `state` is the *client* area in physical pixels, which is
    /// what the guest's mode is set from: this process is per-monitor DPI
    /// aware, so a client rectangle is pixels rather than the scaled units an
    /// unaware process would be handed. A viewer that asked for its logical
    /// size would put a 150% desktop on a 1707x960 output and then scale it
    /// back up to the 2560x1440 panel it was already on.
    ///
    /// # Errors
    ///
    /// A message naming the Win32 call that refused.
    pub fn open(title: &str, state: &WindowState, shared: Arc<Shared>) -> Result<Self, String> {
        let class = HSTRING::from(CLASS_NAME);
        register_class(&class)?;

        let width = i32::try_from(state.size.0).unwrap_or(1920);
        let height = i32::try_from(state.size.1).unwrap_or(1080);
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

        // A desktop can change between two sessions: the monitor the window
        // was on is unplugged, or the arrangement is rebuilt the other way
        // round. A window opened where nobody can see it looks like a viewer
        // that failed to start.
        let position = state
            .position
            .and_then(|position| opening_position(position, state.size, &work_areas()));

        let title = HSTRING::from(title);
        // SAFETY: both strings are NUL-terminated and live across the call, and
        // the class was registered above.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                position.map_or(CW_USEDEFAULT, |(x, _)| x),
                position.map_or(CW_USEDEFAULT, |(_, y)| y),
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

        // SAFETY: the window's own menu, which belongs to it until it is
        // destroyed, and two strings that live across their calls.
        let mut resolution = HMENU::default();
        unsafe {
            let menu = GetSystemMenu(hwnd, false);
            if !menu.is_invalid() {
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                let send = HSTRING::from("Send Ctrl+Alt+Del");
                let _ = AppendMenuW(menu, MF_STRING, SC_SEND_SAS, PCWSTR(send.as_ptr()));
                let release = HSTRING::from("Release keyboard\tCtrl+Alt+Shift");
                let _ = AppendMenuW(
                    menu,
                    MF_STRING,
                    SC_RELEASE_KEYBOARD,
                    PCWSTR(release.as_ptr()),
                );
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                let full = HSTRING::from("Full screen");
                let _ = AppendMenuW(menu, MF_STRING, SC_FULLSCREEN, PCWSTR(full.as_ptr()));
                // Empty until the host has published a monitor's modes, which
                // is a submenu the user finds greyed rather than one that
                // appears halfway through a session.
                if let Ok(popup) = CreatePopupMenu() {
                    let text = HSTRING::from(RESOLUTION_MENU);
                    if AppendMenuW(menu, MF_POPUP, popup.0 as usize, PCWSTR(text.as_ptr())).is_ok()
                    {
                        resolution = popup;
                    } else {
                        let _ = DestroyMenu(popup);
                    }
                }
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                // Listed whether or not this session negotiated audio: a menu
                // item that appears halfway through a session is worse than
                // one that does nothing on a guest with no daemon.
                let mute = HSTRING::from("Mute audio");
                let _ = AppendMenuW(menu, MF_STRING, SC_MUTE_AUDIO, PCWSTR(mute.as_ptr()));
                let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                // Two modes and not three: Motion is task #123's, and a menu
                // offering a mode the guest refuses is a menu that lies.
                let auto = HSTRING::from("Quality: Auto");
                let _ = AppendMenuW(menu, MF_STRING, SC_QUALITY_AUTO, PCWSTR(auto.as_ptr()));
                let desktop = HSTRING::from("Quality: Desktop");
                let _ = AppendMenuW(
                    menu,
                    MF_STRING,
                    SC_QUALITY_DESKTOP,
                    PCWSTR(desktop.as_ptr()),
                );
            }
        }

        // SAFETY: `hwnd` is a window this process owns.
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = UpdateWindow(hwnd);
        }
        correct_client_size(hwnd, width, height);

        let window = Self {
            hwnd,
            shared,
            restore: None,
            resolution,
        };
        // Said once here rather than left to the first drag: a session that
        // never moves the window still has a place worth remembering, and
        // whether showing a window reports its own `WM_MOVE` is Windows'
        // business rather than something to depend on.
        if let Some((x, y)) = window.position() {
            window.shared.report(UiEvent::Moved(x, y));
        }
        window.check_quality(state.quality);
        window.check_muted(state.muted);
        if state.fullscreen {
            let mut window = window;
            window.set_fullscreen(true);

            return Ok(window);
        }

        Ok(window)
    }

    /// Rebuilds the resolution submenu from the modes the host published.
    ///
    /// The whole submenu each time rather than a diff: a monitor change is a
    /// new list, and a menu built by patching the last one is a menu that
    /// drifts from what the guest is being told.
    pub fn set_modes(&self, modes: &[DisplayMode], selected: Option<DisplayMode>) {
        let offered: Vec<_> = modes.iter().copied().take(MAX_MENU_MODES).collect();
        *self
            .shared
            .modes
            .lock()
            .expect("the window's mode list is not poisoned") = offered.clone();

        // SAFETY: the window's own menu and its submenu, which belong to it
        // until it is destroyed, and one string per item that lives across
        // its call.
        unsafe {
            let menu = GetSystemMenu(self.hwnd, false);
            if menu.is_invalid() {
                return;
            }
            let submenu = self.resolution;
            if submenu.is_invalid() {
                return;
            }
            while DeleteMenu(submenu, 0, MF_BYPOSITION).is_ok() {}

            for (index, mode) in offered.iter().enumerate() {
                let Some(command) = menu_command(index) else {
                    break;
                };
                let text = HSTRING::from(label(*mode));
                let _ = AppendMenuW(submenu, MF_STRING, command, PCWSTR(text.as_ptr()));
            }

            if let Some(chosen) = selected
                && let Some(index) = offered.iter().position(|mode| *mode == chosen)
                && let Some(command) = menu_command(index)
            {
                let _ = CheckMenuRadioItem(
                    submenu,
                    0,
                    u32::try_from(offered.len().saturating_sub(1)).unwrap_or(0),
                    u32::try_from(command).unwrap_or(0),
                    MF_BYCOMMAND.0,
                );
            }
        }
    }

    /// Whether the window is minimised, and so has no picture to deliver.
    #[must_use]
    pub fn is_minimised(&self) -> bool {
        // SAFETY: `hwnd` names a window of this process.
        unsafe { IsIconic(self.hwnd) }.as_bool()
    }

    /// Marks which encoding mode is in force.
    pub fn check_quality(&self, quality: Quality) {
        let chosen = match quality {
            Quality::Auto => SC_QUALITY_AUTO,
            Quality::Desktop => SC_QUALITY_DESKTOP,
        };

        // SAFETY: the window's own menu, which belongs to it until it is
        // destroyed.
        unsafe {
            let menu = GetSystemMenu(self.hwnd, false);
            if !menu.is_invalid() {
                let _ = CheckMenuRadioItem(
                    menu,
                    SC_QUALITY_AUTO as u32,
                    SC_QUALITY_DESKTOP as u32,
                    chosen as u32,
                    MF_BYCOMMAND.0,
                );
            }
        }
    }

    /// Marks whether the guest's sound is muted.
    pub fn check_muted(&self, muted: bool) {
        // SAFETY: the window's own menu, which belongs to it until it is
        // destroyed.
        unsafe {
            let menu = GetSystemMenu(self.hwnd, false);
            if !menu.is_invalid() {
                let state = if muted { MF_CHECKED } else { MF_UNCHECKED };
                CheckMenuItem(menu, SC_MUTE_AUDIO as u32, (MF_BYCOMMAND | state).0);
            }
        }
    }

    /// Whether the window is filling a monitor.
    #[must_use]
    pub fn is_fullscreen(&self) -> bool {
        self.restore.is_some()
    }

    /// Fills the monitor the window is on, or goes back to where it was.
    ///
    /// Borderless rather than exclusive: an exclusive mode would take the
    /// display for this process, and a viewer that owns the screen is one the
    /// user cannot alt-tab out of when the guest stops answering. Nothing here
    /// touches the monitor's own mode. What is taken is a frame and a
    /// rectangle, and both are given back.
    ///
    /// The monitor is the one the window is mostly on, read each time: a
    /// window dragged to the second monitor fills the second monitor.
    pub fn set_fullscreen(&mut self, on: bool) {
        if on == self.is_fullscreen() {
            return;
        }

        if on {
            self.enter_fullscreen();
        } else {
            self.leave_fullscreen();
        }
    }

    /// Covers the monitor this window is on, keeping what it takes off.
    fn enter_fullscreen(&mut self) {
        let Some(monitor) = self.monitor_rectangle() else {
            tracing::warn!("the window is on no monitor; full screen is not available");

            return;
        };
        let frame = self.frame();
        // Read before the window is restored down, so that a window that was
        // maximised is maximised again on the way out: `showCmd` is what
        // remembers that, and `rcNormalPosition` is where it came from.
        let Some(placement) = self.placement() else {
            tracing::warn!(
                "the window's placement could not be read; full screen is not available"
            );

            return;
        };

        // A maximised window is sized by Win32 to its monitor's *work* area
        // and does not answer `SetWindowPos`, so it is put down before the
        // borderless frame goes on rather than after, while Win32 can still
        // see the state it is being asked to leave.
        if fullscreen::is_a_state(frame.style) {
            // SAFETY: `self.hwnd` names a window of this process.
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
        }

        let full = fullscreen::borderless(frame);
        // SAFETY: two style words and a position on this process's own window.
        // `SWP_FRAMECHANGED` is what makes Win32 recompute the non-client area
        // it has just been told there is none of.
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWL_STYLE, full.style as isize);
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, full.ex_style as isize);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOP),
                monitor.left,
                monitor.top,
                monitor.right - monitor.left,
                monitor.bottom - monitor.top,
                SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
            );
        }
        mark_fullscreen(self.hwnd, true);
        self.restore = Some((frame, placement));
    }

    /// Puts back the frame and the place the window had before.
    fn leave_fullscreen(&mut self) {
        let Some((frame, placement)) = self.restore.take() else {
            return;
        };
        mark_fullscreen(self.hwnd, false);
        // SAFETY: two style words and a placement this window handed over, put
        // back on the window they came from. The frame goes on first: a
        // placement applied to a `WS_POPUP` window would be a rectangle
        // measured against a frame that is not there yet.
        unsafe {
            SetWindowLongPtrW(self.hwnd, GWL_STYLE, frame.style as isize);
            SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, frame.ex_style as isize);
            let _ = SetWindowPlacement(self.hwnd, &raw const placement);
            let _ = SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
            );
        }
    }

    /// The two style words this window is wearing.
    fn frame(&self) -> Frame {
        // SAFETY: `self.hwnd` names a window of this process.
        unsafe {
            Frame {
                style: GetWindowLongPtrW(self.hwnd, GWL_STYLE) as u32,
                ex_style: GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32,
            }
        }
    }

    /// Where this window is restored to, and whether it is maximised.
    fn placement(&self) -> Option<WINDOWPLACEMENT> {
        let mut placement = WINDOWPLACEMENT {
            length: u32::try_from(std::mem::size_of::<WINDOWPLACEMENT>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: `self.hwnd` names a window of this process and the placement
        // lives across the call.
        unsafe { GetWindowPlacement(self.hwnd, &raw mut placement) }.ok()?;

        Some(placement)
    }

    /// Where the window sits when it is not filling anything.
    ///
    /// `None` while it is full screen or maximised: neither is a place the
    /// user left the window, and remembering one would open the next session
    /// covering a monitor the window was never really on.
    #[must_use]
    pub fn position(&self) -> Option<(i32, i32)> {
        restored_origin(self.hwnd)
    }

    /// What Windows knows about the monitor this window is mostly on.
    fn monitor_info(&self) -> Option<MONITORINFO> {
        // SAFETY: `self.hwnd` names a window of this process.
        let monitor: HMONITOR = unsafe { MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST) };
        if monitor.is_invalid() {
            return None;
        }

        let mut info = MONITORINFO {
            cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: `monitor` came from `MonitorFromWindow` and `info` lives
        // across the call with its size filled in.
        if !unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
            return None;
        }

        Some(info)
    }

    /// The monitor this window is mostly on, in virtual-desktop pixels.
    fn monitor_rectangle(&self) -> Option<RECT> {
        self.monitor_info().map(|info| info.rcMonitor)
    }

    /// The part of that monitor a window can have, taskbar excluded.
    fn work_rectangle(&self) -> Option<monitors::Rect> {
        self.monitor_info().map(|info| monitors::Rect {
            left: info.rcWork.left,
            top: info.rcWork.top,
            right: info.rcWork.right,
            bottom: info.rcWork.bottom,
        })
    }

    /// Where the whole window is, and what its frame adds to the client area.
    ///
    /// Measured rather than computed: `AdjustWindowRectEx` would need this
    /// window's styles and this monitor's DPI, and the difference between the
    /// two rectangles Windows is already keeping is the same number without
    /// either. It is only the truth for a window that is neither maximised nor
    /// full screen, which is the only kind this is asked about.
    fn outside(&self) -> Option<((i32, i32), (i32, i32))> {
        let mut rectangle = RECT::default();
        // SAFETY: `self.hwnd` names a window of this process and `rectangle`
        // lives across the call.
        unsafe { GetWindowRect(self.hwnd, &raw mut rectangle) }.ok()?;
        let (across, down) = self.client_size();

        Some((
            (rectangle.left, rectangle.top),
            (
                (rectangle.right - rectangle.left) - across,
                (rectangle.bottom - rectangle.top) - down,
            ),
        ))
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

    /// Takes a client area of this size, and answers the one it got.
    ///
    /// What a mode chosen rather than dragged asks of the window: the guest is
    /// on a geometry nobody's window asked for, and letterboxing it into the
    /// size the window happened to be would be showing a 2560x1440 desktop
    /// scaled down inside a 1848x1048 rectangle.
    ///
    /// `None` when the window is in no position to take a size, which is three
    /// cases and all of them deliberate. Full screen already covers a monitor
    /// and its letterbox is the honest answer. A maximised window is sized by
    /// Windows, and un-maximising one because the guest changed mode would be
    /// the viewer overruling the user's own window state. A minimised one has
    /// no client area to give.
    ///
    /// The size that comes back is read off the window rather than assumed: it
    /// is what the caller has to tell the guest it is already on, and on a
    /// monitor too small for the mode the two are not the same number.
    pub fn set_client_size(&self, width: u32, height: u32) -> Option<(u32, u32)> {
        if self.is_fullscreen() || fullscreen::is_a_state(self.frame().style) {
            return None;
        }

        let work = self.work_rectangle()?;
        let (at, frame) = self.outside()?;
        let fit = monitors::fitted((width, height), frame, at, work);

        // SAFETY: a position and a size on this process's own window. The
        // z-order flags leave the stack alone: a guest changing mode must not
        // raise a window the user has put behind something else.
        unsafe {
            SetWindowPos(
                self.hwnd,
                None,
                fit.x,
                fit.y,
                fit.width,
                fit.height,
                SWP_NOZORDER | SWP_NOOWNERZORDER,
            )
        }
        .ok()?;

        let (across, down) = self.client_size();

        (across > 0 && down > 0).then_some((across.unsigned_abs(), down.unsigned_abs()))
    }

    /// Brings the window forward. What a repeated Connect means.
    ///
    /// A full-screen window is only raised. `SW_RESTORE` on one would be
    /// Win32 undoing a state, and the state it would undo is the full screen
    /// the user asked for: a second Connect, or a reconnect that focuses the
    /// window again, must not be what drops the viewer back into a frame.
    pub fn focus(&self) {
        // SAFETY: `self.hwnd` is a window this process owns.
        unsafe {
            if !self.is_fullscreen() {
                let _ = ShowWindow(self.hwnd, SW_RESTORE);
            }
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

/// Sizes a window whose client area came out other than what was asked for.
///
/// `AdjustWindowRect` above worked from 96 DPI frame metrics, because the
/// window it was sizing did not exist yet and so was on no monitor. Now it is
/// on one, and any difference is the frame's: without this correction, a
/// window remembered at its own client size comes back a few pixels smaller on
/// a scaled monitor -- and smaller again on every restart after that.
fn correct_client_size(hwnd: HWND, width: i32, height: i32) {
    let (actual_width, actual_height) = client_size(hwnd);
    if actual_width <= 0 || actual_height <= 0 || (actual_width, actual_height) == (width, height) {
        return;
    }

    let mut rectangle = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: height,
    };
    // SAFETY: `hwnd` names a window of this process and `rectangle` lives
    // across the call.
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        if AdjustWindowRectExForDpi(
            &raw mut rectangle,
            WS_OVERLAPPEDWINDOW,
            false,
            WINDOW_EX_STYLE(0),
            dpi,
        )
        .is_err()
        {
            return;
        }

        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            rectangle.right - rectangle.left,
            rectangle.bottom - rectangle.top,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOOWNERZORDER,
        );
    }
}

/// Makes this process see the screen in pixels rather than in scaled units.
///
/// Called before any window exists, and once. Without it Windows hands an
/// unaware process a virtualised client rectangle -- 1707x960 on a 2560x1440
/// panel at 150% -- and blits it back up, so a viewer that set the guest's
/// mode from it would put a small desktop on a big screen and then blur it.
/// Per-monitor v2 rather than system-wide, because a window dragged between
/// monitors of different scales must keep reporting the pixels it covers.
///
/// A refusal is not fatal: the awareness may already have been set by a
/// manifest, which is the same answer by another route.
pub fn become_dpi_aware() {
    // SAFETY: a process-wide setting with a documented context value.
    if let Err(error) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        tracing::debug!("this process is already DPI aware, or cannot be told to be: {error}");
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

/// The top left of a window that is sitting on the desktop normally.
///
/// `None` for a window that is full screen -- which this viewer makes with
/// `WS_POPUP` -- or maximised or minimised: those are states rather than
/// places, and the place to come back to is the one from before them.
fn restored_origin(hwnd: HWND) -> Option<(i32, i32)> {
    // SAFETY: `hwnd` names a window of this process.
    let style = WINDOW_STYLE(unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32);
    if style & WS_POPUP != WINDOW_STYLE(0) {
        return None;
    }

    let mut placement = WINDOWPLACEMENT {
        length: u32::try_from(std::mem::size_of::<WINDOWPLACEMENT>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: as above, and the placement lives across the call.
    unsafe { GetWindowPlacement(hwnd, &raw mut placement) }.ok()?;
    if placement.showCmd != SW_SHOWNORMAL.0 as u32 {
        return None;
    }

    let mut rectangle = RECT::default();
    // SAFETY: as above, and the rectangle lives across the call. The window
    // rectangle rather than the placement's, because a placement is in
    // workspace coordinates -- the same window, offset by whatever the
    // taskbar takes -- and what opens a window is screen coordinates.
    unsafe { GetWindowRect(hwnd, &raw mut rectangle) }.ok()?;

    Some((rectangle.left, rectangle.top))
}

/// The work area of every monitor attached right now, the primary one first.
///
/// Work areas rather than whole monitors: a title bar under the taskbar is one
/// the user cannot grab, so it does not count as somewhere the window is.
fn work_areas() -> Vec<monitors::Rect> {
    let mut found: Vec<(bool, monitors::Rect)> = Vec::new();
    // SAFETY: `collect_monitor` only ever writes through the pointer handed
    // to it, which is this vector for as long as the enumeration runs.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&raw mut found as isize),
        );
    }
    // The primary monitor first, so that a window with no monitor left of its
    // own lands on the one the user is most likely looking at.
    found.sort_by_key(|(primary, _)| !primary);

    found.into_iter().map(|(_, area)| area).collect()
}

/// One monitor, appended to the vector `lparam` names.
extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _context: HDC,
    _rectangle: *mut RECT,
    lparam: LPARAM,
) -> windows::core::BOOL {
    let mut info = MONITORINFO {
        cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).unwrap_or(0),
        ..Default::default()
    };
    // SAFETY: `monitor` came from the enumeration and `info` lives across the
    // call with its size filled in.
    if unsafe { GetMonitorInfoW(monitor, &raw mut info) }.as_bool() {
        // SAFETY: the pointer is the vector `work_areas` passed in, alive for
        // the whole enumeration, and this callback is the only writer.
        let found = unsafe { &mut *(lparam.0 as *mut Vec<(bool, monitors::Rect)>) };
        found.push((
            info.dwFlags & MONITORINFOF_PRIMARY != 0,
            monitors::Rect {
                left: info.rcWork.left,
                top: info.rcWork.top,
                right: info.rcWork.right,
                bottom: info.rcWork.bottom,
            },
        ));
    }

    true.into()
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
            LRESULT(isize::from(message == WM_XBUTTONDOWN))
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
            LRESULT(isize::from(message == WM_XBUTTONUP))
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
                SC_FULLSCREEN => {
                    shared.report(UiEvent::ToggleFullscreen);

                    return LRESULT(0);
                }
                SC_MUTE_AUDIO => {
                    shared.report(UiEvent::ToggleMute);

                    return LRESULT(0);
                }
                SC_QUALITY_AUTO => {
                    shared.report(UiEvent::Quality(Quality::Auto));

                    return LRESULT(0);
                }
                SC_QUALITY_DESKTOP => {
                    shared.report(UiEvent::Quality(Quality::Desktop));

                    return LRESULT(0);
                }
                command => {
                    // The mode block, resolved here rather than in the main
                    // loop: what the user picked is a mode, and an index into
                    // a list that may have been rebuilt in between is not.
                    if let Some(index) = menu_index(command)
                        && let Some(mode) = shared
                            .modes
                            .lock()
                            .expect("the window's mode list is not poisoned")
                            .get(index)
                            .copied()
                    {
                        shared.report(UiEvent::DisplayMode(mode));

                        return LRESULT(0);
                    }
                }
            }

            // SAFETY: the default handler, which owns Move, Size and Close.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
        WM_MOVE => {
            // Read back rather than taken from `lparam`: that is the client
            // area's corner, and what opens a window again is the frame's.
            if let Some((x, y)) = restored_origin(hwnd) {
                shared.report(UiEvent::Moved(x, y));
            }
            // A window dragged across an edge is on another monitor without
            // anything having changed about the desktop.
            shared.report(UiEvent::MonitorChanged);
            LRESULT(0)
        }
        // The desktop was rearranged, or the window crossed onto a monitor
        // that scales differently. Either way the modes may not be the same.
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            shared.report(UiEvent::MonitorChanged);

            // SAFETY: the default handler, which moves the window to the
            // rectangle a DPI change suggests.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
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
    let bit = 1u32 << u32::from(button - input::BTN_LEFT).min(31);
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

/// Tells the shell whether this window is filling a monitor.
///
/// The rectangle alone is usually enough -- the shell notices a foreground
/// window the exact size of its monitor and takes the taskbar out of the way
/// -- but that is detection rather than a contract, and detection is what
/// leaves a taskbar drawn over a viewer that is otherwise correct.
/// `MarkFullscreenWindow` is the documented way to say it, and saying it is
/// also how the taskbar comes back.
///
/// Nothing here is load-bearing: a shell that will not answer costs a taskbar
/// on top of the picture, not a viewer, so every failure is a warning.
fn mark_fullscreen(hwnd: HWND, on: bool) {
    let Some(taskbar) = taskbar() else {
        return;
    };
    // SAFETY: an interface this thread created and a window this process owns.
    if let Err(error) = unsafe { taskbar.MarkFullscreenWindow(hwnd, on) } {
        tracing::warn!("the shell would not be told about the full-screen window: {error}");
    }
}

thread_local! {
    /// The shell's taskbar object, asked for once per thread that needs it.
    ///
    /// Once, because a shell that has no answer has no answer, and a warning
    /// per full-screen toggle is a log nobody reads.
    static TASKBAR: OnceCell<Option<ITaskbarList2>> = const { OnceCell::new() };
}

/// The shell's taskbar object, or `None` with the reason already logged.
fn taskbar() -> Option<ITaskbarList2> {
    TASKBAR.with(|cell| cell.get_or_init(create_taskbar).clone())
}

/// Creates it, on a thread that may or may not have COM up already.
fn create_taskbar() -> Option<ITaskbarList2> {
    // SAFETY: COM on this thread, and an in-process class the shell registers.
    // `CoInitializeEx` is not undone: the pump thread lives as long as the
    // process, and an apartment torn down under the shell's object would be
    // worse than one that outlives it. An apartment somebody else already
    // chose is theirs to keep, so the result is not checked -- the create
    // below is what says whether this worked.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let taskbar: ITaskbarList2 = match CoCreateInstance(
            &TaskbarList,
            None,
            CLSCTX_INPROC_SERVER,
        ) {
            Ok(taskbar) => taskbar,
            Err(error) => {
                tracing::warn!(
                    "the shell's taskbar list is not available, so the taskbar may stay over a full-screen window: {error}"
                );

                return None;
            }
        };
        // Documented as the first call on the interface, before any other.
        if let Err(error) = taskbar.HrInit() {
            tracing::warn!("the shell's taskbar list would not start: {error}");

            return None;
        }

        Some(taskbar)
    }
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, atomic::Ordering, mpsc};

    use windows::Win32::{
        Foundation::{LPARAM, WPARAM},
        UI::WindowsAndMessaging::{
            IsZoomed, SC_MAXIMIZE, SendMessageW, WM_CLOSE, WM_DISPLAYCHANGE, WM_KILLFOCUS,
            WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOVE, WM_RBUTTONDOWN,
            WM_RBUTTONUP, WM_SETFOCUS, WM_SYSCOMMAND,
        },
    };

    use super::{
        SC_FULLSCREEN, SC_QUALITY_DESKTOP, SC_RELEASE_KEYBOARD, SC_SEND_SAS, Shared, UiEvent,
        WM_SIGNAL, Window,
    };
    use crate::{
        display_modes::{DisplayMode, MAX_MENU_MODES, menu_command},
        fullscreen::{EDGES, Frame, OVERLAPPED_WINDOW, POPUP},
        input::{BTN_RIGHT, Report},
        state::{Quality, WindowState},
        status::{self, Button},
    };

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

    /// A state that opens a window of this size where Windows chooses.
    fn sized(width: u32, height: u32) -> WindowState {
        WindowState {
            size: (width, height),
            ..WindowState::default()
        }
    }

    /// A shown window, with whatever showing it reported already taken.
    fn opened(events: &mpsc::Receiver<UiEvent>, shared: &Arc<Shared>) -> Window {
        let window = Window::open("test", &sized(320, 240), Arc::clone(shared)).expect("a window");
        let _ = drain(events);

        window
    }

    #[test]
    fn a_move_over_the_picture_is_reported_as_a_pointer_position() {
        let (shared, events) = shared();
        let window = opened(&events, &shared);

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_MOUSEMOVE,
                None,
                Some(LPARAM((30 << 16) | 20)),
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
        let window = opened(&events, &shared);

        // SAFETY: messages sent to this process's own window.
        unsafe {
            SendMessageW(window.handle(), WM_RBUTTONDOWN, None, Some(LPARAM(0)));
            SendMessageW(window.handle(), WM_RBUTTONUP, None, Some(LPARAM(0)));
        }

        assert_eq!(
            drain(&events),
            vec![
                UiEvent::Input(Report::Button {
                    button: BTN_RIGHT,
                    pressed: true
                }),
                UiEvent::Input(Report::Button {
                    button: BTN_RIGHT,
                    pressed: false
                }),
            ]
        );
    }

    #[test]
    fn a_click_on_the_failed_screen_is_never_a_guest_press() {
        let (shared, events) = shared();
        let window = opened(&events, &shared);
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
        let window = opened(&events, &shared);

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_MOUSEWHEEL,
                Some(WPARAM(((-240i32) as u32 as usize) << 16)),
                Some(LPARAM(0)),
            );
        }

        assert_eq!(
            drain(&events),
            vec![UiEvent::Input(Report::Wheel {
                horizontal: 0,
                vertical: -240
            })]
        );
    }

    #[test]
    fn focus_is_reported_both_ways() {
        let (shared, events) = shared();
        let window = opened(&events, &shared);

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
        let window = opened(&events, &shared);

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

    #[test]
    fn a_window_opens_at_the_size_it_was_asked_for() {
        let (shared, _events) = shared();
        let window = Window::open("test - VMLord Display", &sized(640, 480), shared)
            .expect("a window class and a window");

        assert_eq!(window.client_size(), (640, 480));
    }

    #[test]
    fn a_posted_signal_reaches_the_pump() {
        let (shared, _events) = shared();
        let window =
            Window::open("test - VMLord Display", &sized(320, 240), shared).expect("a window");
        let poster = window.poster();

        poster.post(WM_SIGNAL);

        // The pump runs the queue dry and answers whether the window is still
        // open. A posted signal is not a quit.
        assert!(window.pump());
    }

    #[test]
    fn a_click_on_retry_is_reported_only_while_the_failed_screen_is_up() {
        let (shared, events) = shared();
        let window = Window::open(
            "test - VMLord Display",
            &sized(800, 600),
            Arc::clone(&shared),
        )
        .expect("a window");
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
    fn the_menu_reports_full_screen_and_the_quality_the_user_picked() {
        let (shared, events) = shared();
        let window = opened(&events, &shared);

        // SAFETY: messages sent to this process's own window.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_SYSCOMMAND,
                Some(WPARAM(SC_FULLSCREEN)),
                None,
            );
            SendMessageW(
                window.handle(),
                WM_SYSCOMMAND,
                Some(WPARAM(SC_QUALITY_DESKTOP)),
                None,
            );
        }

        let reported = drain(&events);
        assert!(reported.contains(&UiEvent::ToggleFullscreen));
        assert!(reported.contains(&UiEvent::Quality(Quality::Desktop)));
    }

    #[test]
    fn a_window_that_moves_reports_where_it_went() {
        // What is remembered is reported while the window is still there: at
        // the end of a session the window has already been destroyed, and a
        // destroyed window has no position to read back.
        let (shared, events) = shared();
        let window = opened(&events, &shared);

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(window.handle(), WM_MOVE, None, None);
        }

        let reported = drain(&events);
        let position = window.position().expect("a restored window has a position");
        assert!(
            reported.contains(&UiEvent::Moved(position.0, position.1)),
            "a moved window reports its place: {reported:?}"
        );
    }

    #[test]
    fn a_resolution_picked_from_the_menu_is_reported_as_the_mode_it_names() {
        let (shared, events) = shared();
        let window = opened(&events, &shared);
        let offered = [
            DisplayMode::new(1280, 720, 60).expect("a valid fixture"),
            DisplayMode::new(1920, 1080, 144).expect("a valid fixture"),
        ];
        window.set_modes(&offered, Some(offered[0]));

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_SYSCOMMAND,
                Some(WPARAM(menu_command(1).expect("a command"))),
                None,
            );
        }

        assert!(drain(&events).contains(&UiEvent::DisplayMode(offered[1])));
    }

    #[test]
    fn a_mode_the_menu_no_longer_offers_is_not_reported() {
        // The list is rebuilt whenever the monitor changes, and a command for
        // an entry that is gone is one the window has nothing to answer with.
        let (shared, events) = shared();
        let window = opened(&events, &shared);
        window.set_modes(
            &[DisplayMode::new(1280, 720, 60).expect("a valid fixture")],
            None,
        );

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_SYSCOMMAND,
                Some(WPARAM(menu_command(5).expect("a command"))),
                None,
            );
        }

        assert!(
            !drain(&events)
                .iter()
                .any(|event| matches!(event, UiEvent::DisplayMode(_))),
            "an entry that is gone is not a mode"
        );
    }

    #[test]
    fn a_menu_longer_than_the_guest_holds_is_cut_to_what_it_holds() {
        let (shared, events) = shared();
        let window = opened(&events, &shared);
        let offered: Vec<_> = (0..MAX_MENU_MODES + 8)
            .map(|step| DisplayMode::new(640 + step as u32 * 8, 480, 60).expect("a valid fixture"))
            .collect();
        window.set_modes(&offered, None);

        assert_eq!(
            window
                .shared
                .modes
                .lock()
                .expect("the window's mode list")
                .len(),
            MAX_MENU_MODES
        );
    }

    #[test]
    fn a_desktop_that_changed_marks_the_monitor_stale() {
        // Only stale: what the monitor now is gets enumerated off the message
        // pump, because a mode list is a dozen Win32 calls and this thread is
        // the one that must never stop.
        let (shared, events) = shared();
        let window = opened(&events, &shared);

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(window.handle(), WM_DISPLAYCHANGE, None, None);
        }

        assert!(drain(&events).contains(&UiEvent::MonitorChanged));
    }

    #[test]
    fn a_window_dragged_onto_another_monitor_marks_it_stale_too() {
        // A move is how a window changes screens without the desktop changing
        // at all, and the screen it is on now is the one to publish.
        let (shared, events) = shared();
        let window = opened(&events, &shared);

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(window.handle(), WM_MOVE, None, None);
        }

        assert!(drain(&events).contains(&UiEvent::MonitorChanged));
    }

    #[test]
    fn a_window_reports_where_it_opened_before_anyone_moves_it() {
        // A session that never drags the window still has a place to remember:
        // the one Windows chose for it the first time.
        let (shared, events) = shared();
        let window =
            Window::open("test - VMLord Display", &sized(320, 240), shared).expect("a window");

        let position = window.position().expect("a restored window has a position");
        assert!(
            drain(&events).contains(&UiEvent::Moved(position.0, position.1)),
            "the place a window opened at is reported like any other"
        );
    }

    #[test]
    fn a_full_screen_window_reports_no_place_to_remember() {
        // The monitor it is covering is not where the user left the window.
        let (shared, events) = shared();
        let mut window = opened(&events, &shared);
        window.set_fullscreen(true);
        let _ = drain(&events);

        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(window.handle(), WM_MOVE, None, None);
        }

        assert!(
            !drain(&events)
                .iter()
                .any(|event| matches!(event, UiEvent::Moved(_, _))),
            "a full-screen window has no restored place to report"
        );
        assert_eq!(window.position(), None);
    }

    #[test]
    fn a_window_remembered_where_no_monitor_is_any_more_opens_on_one() {
        // The unplugged second monitor, or the arrangement rebuilt the other
        // way round: the coordinates are real and nobody can see them.
        let (shared, _events) = shared();
        let state = WindowState {
            position: Some((-30_000, -30_000)),
            size: (320, 240),
            ..WindowState::default()
        };

        let window = Window::open("test - VMLord Display", &state, shared).expect("a window");

        let position = window.position().expect("a restored window has a position");
        assert!(
            position != (-30_000, -30_000) && position.0 > -30_000 && position.1 > -30_000,
            "a window with no monitor left opens on one that is there: {position:?}"
        );
    }

    #[test]
    fn a_window_that_filled_a_monitor_goes_back_to_where_it_was() {
        let (shared, events) = shared();
        let mut window = opened(&events, &shared);
        let before = window.position().expect("a restored window has a position");
        let frame = window.frame();

        window.set_fullscreen(true);
        assert!(window.is_fullscreen());

        window.set_fullscreen(false);
        assert!(!window.is_fullscreen());
        assert_eq!(
            window.position(),
            Some(before),
            "what comes back is where the window was, not the monitor"
        );
        assert_eq!(window.client_size(), (320, 240));
        assert_eq!(
            window.frame(),
            frame,
            "the frame that comes back is the one that went away, both words of it"
        );
    }

    #[test]
    fn a_full_screen_window_wears_no_frame_at_all() {
        // The whole of the bug: taking `WS_OVERLAPPEDWINDOW` off `GWL_STYLE`
        // leaves `WS_EX_WINDOWEDGE` on `GWL_EXSTYLE`, and that is a border
        // drawn round what is supposed to be the monitor.
        let (shared, events) = shared();
        let mut window = opened(&events, &shared);
        assert_ne!(
            window.frame().style & OVERLAPPED_WINDOW,
            0,
            "the window under test starts with a title bar to lose"
        );

        window.set_fullscreen(true);

        let Frame { style, ex_style } = window.frame();
        assert_eq!(style & OVERLAPPED_WINDOW, 0, "no caption and no border");
        assert_eq!(style & POPUP, POPUP);
        assert_eq!(ex_style & EDGES, 0, "no raised edge either");
    }

    #[test]
    fn a_full_screen_window_covers_the_monitor_and_not_its_work_area() {
        // The work area is the monitor minus the taskbar, and a full screen
        // that stopped there is one with the taskbar still on it.
        let (shared, events) = shared();
        let mut window = opened(&events, &shared);

        window.set_fullscreen(true);

        let monitor = window.monitor_rectangle().expect("a monitor");
        assert_eq!(
            window.client_size(),
            (monitor.right - monitor.left, monitor.bottom - monitor.top),
            "the client area is the whole monitor: with no frame there is nothing else"
        );
    }

    #[test]
    fn a_maximised_window_still_fills_the_monitor_and_comes_back_maximised() {
        // Win32 sizes a maximised window to the work area and ignores what
        // `SetWindowPos` asks of it, so entering full screen from one used to
        // leave the taskbar drawn over the guest.
        let (shared, events) = shared();
        let mut window = opened(&events, &shared);
        // SAFETY: a message sent to this process's own window.
        unsafe {
            SendMessageW(
                window.handle(),
                WM_SYSCOMMAND,
                Some(WPARAM(SC_MAXIMIZE as usize)),
                None,
            );
        }
        assert!(is_maximised(&window), "the window under test is maximised");

        window.set_fullscreen(true);
        let monitor = window.monitor_rectangle().expect("a monitor");
        assert_eq!(
            window.client_size(),
            (monitor.right - monitor.left, monitor.bottom - monitor.top),
            "a full screen entered from a maximised window is still the whole monitor"
        );

        window.set_fullscreen(false);
        assert!(
            is_maximised(&window),
            "a window that was maximised is maximised again"
        );
    }

    #[test]
    fn focusing_a_full_screen_window_leaves_it_full_screen() {
        // A second Connect, or a reconnect, focuses the window that is already
        // open. `SW_RESTORE` on a full-screen one would undo the state the
        // user asked for.
        let (shared, events) = shared();
        let mut window = opened(&events, &shared);
        window.set_fullscreen(true);
        let full = window.frame();

        window.focus();

        assert!(window.is_fullscreen());
        assert_eq!(window.frame(), full);
        let monitor = window.monitor_rectangle().expect("a monitor");
        assert_eq!(
            window.client_size(),
            (monitor.right - monitor.left, monitor.bottom - monitor.top)
        );
    }

    #[test]
    fn asking_for_the_state_the_window_is_already_in_changes_nothing() {
        // The guard matters: a second `set_fullscreen(true)` that saved the
        // borderless frame would have nothing left to restore.
        let (shared, events) = shared();
        let mut window = opened(&events, &shared);
        let frame = window.frame();

        window.set_fullscreen(true);
        window.set_fullscreen(true);
        window.set_fullscreen(false);
        window.set_fullscreen(false);

        assert!(!window.is_fullscreen());
        assert_eq!(window.frame(), frame);
    }

    /// Whether Win32 thinks this window is maximised.
    fn is_maximised(window: &Window) -> bool {
        // SAFETY: the window is open and owned by the test that asks.
        unsafe { IsZoomed(window.handle()) }.as_bool()
    }

    #[test]
    fn a_window_opens_full_screen_when_that_is_where_it_was_left() {
        let (shared, _events) = shared();
        let state = WindowState {
            size: (320, 240),
            fullscreen: true,
            ..WindowState::default()
        };

        let window = Window::open("test - VMLord Display", &state, shared).expect("a window");

        assert!(window.is_fullscreen());
    }

    #[test]
    fn closing_the_window_is_reported_before_the_pump_ends() {
        let (shared, events) = shared();
        let window =
            Window::open("test - VMLord Display", &sized(320, 240), shared).expect("a window");

        // SAFETY: the window is open and owned by this test.
        unsafe {
            SendMessageW(window.handle(), WM_CLOSE, None, None);
        }

        assert!(drain(&events).contains(&UiEvent::Closing));
    }
}
