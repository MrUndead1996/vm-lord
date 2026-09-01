//! The three programs a VMLord guest runs to put its desktop on the wire.
//!
//! `vmlord-display-broker` is privileged and small; `vmlord-display-session`
//! runs hot and holds nothing worth stealing; `vmlord-display-clipboard` lives
//! in the user's graphical session, because that is where a selection exists. What crosses between them is
//! [`ipc`], and it is typed operations only: no device descriptor and no ioctl
//! passthrough leaves the broker.

pub mod alsa;
pub mod audio_main;
pub mod broker_main;
pub mod capture;
pub mod channel;
pub mod clipboard_files;
pub mod clipboard_main;
pub mod control;
pub mod cursor;
pub mod drm;
pub mod guest_probe;
pub mod ipc;
pub mod mutter;
pub mod output;
pub mod pipeline;
pub mod pipeline_bench;
pub mod seat;
pub mod session_main;
pub mod systemd;
pub mod uinput;
pub mod unix;
pub mod vsock;

/// The generated types for the broker's private schema.
mod broker {
    // Generated code is not written to this repository's standards and cannot
    // be, so it is not linted against them.
    #![allow(clippy::all, clippy::pedantic, missing_docs)]

    include!(concat!(env!("OUT_DIR"), "/vmlord.display.broker.rs"));
}
