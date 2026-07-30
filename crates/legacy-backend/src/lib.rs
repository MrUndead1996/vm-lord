//! Temporary Windows-only adapter for the AppSandbox C backend.
//!
//! All FFI, raw pointers, and dynamic library lifetime management are contained here.

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(not(windows))]
compile_error!("vmlord-legacy-backend supports Windows only");

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::AppSandboxBackend;
