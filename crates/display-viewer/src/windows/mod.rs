//! The seven modules that touch Win32, and the only `unsafe` in this crate.
//!
//! The workspace denies `unsafe_code`; each declaration below re-allows it for
//! one module and says what crosses that door. Everything else in the crate is
//! safe Rust, and every decision the viewer makes lives on that side.

/// Winsock, and the `AF_HYPERV` addresses the metadata does not describe.
#[allow(unsafe_code)]
pub mod hvsocket;

/// A named mutex and a named pipe: one window per VM, focused rather than
/// duplicated.
#[allow(unsafe_code)]
pub mod ipc;

/// A window class, a message pump, and the messages the session posts into it.
#[allow(unsafe_code)]
pub mod window;

/// A D3D11 device, a swapchain, one texture, and the Direct2D overlay over it.
#[allow(unsafe_code)]
pub mod d3d;

/// A low-level keyboard hook, so that the keys the shell takes first reach the
/// guest instead.
#[allow(unsafe_code)]
pub mod hook;

/// The desktop's clipboard, a message-only window to watch it, and the thread
/// that carries selections to and from the guest.
#[allow(unsafe_code)]
pub mod clipboard;

/// Opening, creating and enumerating filesystem objects for the file
/// clipboard, without ever following what stands for something else.
#[allow(unsafe_code)]
pub mod files;

/// The monitor the window is on, and the modes it drives.
#[allow(unsafe_code)]
pub mod display_modes;
