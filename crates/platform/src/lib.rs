//! Windows platform integration for VMLord.
//!
//! This crate is the sole home for native Windows API calls in the Rust-native
//! backend. It depends on the domain crate only and must not depend on `app` or
//! `ui`.

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(not(windows))]
compile_error!("vmlord-platform supports Windows only");

mod create;
mod enumerate;
mod error;
mod event;
mod force_stop;
mod hcn;
mod hcs;
mod hcs_config;
mod metadata;
mod reconnect;
mod shutdown;
mod start;
mod vhd;

pub use create::VmCreationPipeline;
pub use enumerate::{KnownVm, list_known_vms, open_by_vm_id, open_by_vm_name};
pub use error::hresult_to_repository_error;
pub use event::{EventWaitResult, WindowsEvent};
pub use force_stop::VmForceStopPipeline;
pub use hcn::HcnNetwork;
pub use hcs::{HcsClient, HcsOperation, HcsSystem};
pub use metadata::{MetadataStore, VmComputeSystemMapping};
pub use reconnect::{
    ReconnectOutcome, ReconnectReport, ReconnectedVm, VmConnections, reconnect_known_vms,
};
pub use shutdown::VmShutdownPipeline;
pub use start::VmStartPipeline;
