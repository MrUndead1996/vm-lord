//! Windows platform integration for VMLord.
//!
//! This crate is the sole home for native Windows API calls in the Rust-native
//! backend. It depends on the domain crate only and must not depend on `app` or
//! `ui`.

#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(not(windows))]
compile_error!("vmlord-platform supports Windows only");

mod cleanup;
mod create;
mod delete;
mod dhcp;
mod enumerate;
mod error;
mod event;
mod force_stop;
mod hcn;
mod hcn_endpoint;
mod hcs;
mod hcs_config;
mod host_dns;
mod import;
mod layout;
mod metadata;
mod password_hash;
mod reconnect;
mod repository;
mod shutdown;
mod start;
mod subnet;
mod vhd;
mod vm_key;
mod watch;

pub use create::VmCreationPipeline;
pub use delete::VmDeletionPipeline;
pub use enumerate::{KnownVm, list_known_vms, open_by_vm_id, open_by_vm_name};
pub use error::hresult_to_repository_error;
pub use event::{EventWaitResult, WindowsEvent};
pub use force_stop::VmForceStopPipeline;
pub use hcn::{HcnNetwork, VMLORD_NETWORK_ID};
pub use hcn_endpoint::{EndpointAddress, HcnEndpoint};
pub use hcs::{
    HcsClient, HcsOperation, HcsStartFailure, HcsSystem, HcsSystemState, HcsSystemSummary,
};
pub use import::{ImportSummary, import_image};
pub use metadata::{MetadataStore, VmComputeSystemMapping};
pub use password_hash::hash_password;
pub use reconnect::{
    ReconnectOutcome, ReconnectReport, ReconnectedVm, VmConnections, reconnect_known_vms,
};
pub use repository::HcsVmRepository;
pub use shutdown::VmShutdownPipeline;
pub use start::VmStartPipeline;
pub use vm_key::{read_public_key, write_key_pair};
pub use watch::{HcsEventKind, HcsVmEvent, SystemWatch, VmEventSink};
