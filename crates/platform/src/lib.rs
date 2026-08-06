//! Windows platform integration for VMLord.
//!
//! This crate is the sole home for native Windows API calls in the Rust-native
//! backend. It depends on the domain crate only and must not depend on `app` or
//! `ui`.

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(not(windows))]
compile_error!("vmlord-platform supports Windows only");

mod error;
mod event;
mod hcn;
mod hcs;
mod metadata;

pub use error::hresult_to_repository_error;
pub use event::{EventWaitResult, WindowsEvent};
pub use hcn::HcnNetwork;
pub use hcs::{HcsOperation, HcsSystem};
pub use metadata::{MetadataStore, VmComputeSystemMapping};
