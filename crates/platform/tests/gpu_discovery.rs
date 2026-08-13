//! Host GPU-PV discovery against the real host.
//!
//! `#[ignore]`d: what it reports depends on the machine it runs on, so it can
//! assert that the walk is sound and self-consistent but never that GPU-PV
//! exists. A test that demanded an adapter would be permanently red on a host
//! without one, and one that demanded none would be red on a host with one.
//!
//! Run it with:
//!
//! ```text
//! cargo test -p vmlord-platform --target=x86_64-pc-windows-gnu --test gpu_discovery -- --ignored --nocapture
//! ```

use vmlord_platform::discover_host_gpu;

#[test]
#[ignore = "reads the real host's devices"]
fn discovery_reports_a_self_consistent_picture() {
    let capabilities = discover_host_gpu();
    println!("{capabilities:#?}");

    for adapter in &capabilities.adapters {
        assert!(!adapter.name.is_empty(), "an adapter must be named");
        assert!(
            !adapter.instance_id.is_empty(),
            "an adapter must have an instance id"
        );
        assert!(
            !adapter.interface_path.is_empty(),
            "an adapter must have an interface path"
        );
        if let Some(driver_store) = &adapter.driver_store {
            assert!(
                driver_store.is_dir(),
                "a resolved driver package must be a directory: {}",
                driver_store.display()
            );
        }
    }

    if capabilities.assignment.is_available() {
        assert!(
            capabilities
                .adapters
                .iter()
                .any(|adapter| adapter.driver_store.is_some()),
            "assignment cannot be available with no resolved driver package"
        );
    }
}
