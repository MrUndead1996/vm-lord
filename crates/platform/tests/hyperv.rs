//! Windows-only integration tests for the HCS platform layer.
//!
//! Run with a Windows host where Hyper-V and the Host Compute Service are
//! installed and running. Set `VMLORD_TEST_VM_ID` to an existing disposable HCS
//! compute-system identifier, then execute:
//!
//! `cargo test -p vmlord-platform --test hyperv -- --ignored`

#![cfg(windows)]

use vmlord_platform::{HcsClient, HcsOperation, HcsSystem};

const HCS_ACCESS_ALL: u32 = 0x000F_FFFF;

#[test]
#[ignore = "requires Windows with Hyper-V/HCS"]
fn initializes_when_host_compute_service_is_available() {
    let mut client = HcsClient::new();

    client
        .initialize()
        .expect("Host Compute Service should accept a service-properties query");

    assert!(client.is_initialized());
}

#[test]
#[ignore = "requires Windows with Hyper-V/HCS and VMLORD_TEST_VM_ID set to a disposable VM"]
fn opens_the_configured_hcs_compute_system() {
    let vm_id = std::env::var("VMLORD_TEST_VM_ID")
        .expect("VMLORD_TEST_VM_ID must identify a disposable HCS compute system");

    let _operation = HcsOperation::new();
    let _system = HcsSystem::open(&vm_id, HCS_ACCESS_ALL)
        .expect("configured HCS compute system should be openable");
}
