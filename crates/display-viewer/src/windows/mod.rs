//! The four modules that touch Win32, and the only `unsafe` in this crate.
//!
//! The workspace denies `unsafe_code`; each declaration below re-allows it for
//! one module and says what crosses that door. Everything else in the crate is
//! safe Rust, and every decision the viewer makes lives on that side.

/// Winsock, and the `AF_HYPERV` addresses the metadata does not describe.
#[allow(unsafe_code)]
pub mod hvsocket;

// A named mutex and a named pipe: one window per VM, focused rather than
// duplicated. Task 8.
// #[allow(unsafe_code)]
// pub mod ipc;

// A window class, a message pump, and the messages the session posts into it.
// Task 9.
// #[allow(unsafe_code)]
// pub mod window;

// A D3D11 device, a swapchain, one texture, and the Direct2D overlay over it.
// Task 10.
// #[allow(unsafe_code)]
// pub mod d3d;
