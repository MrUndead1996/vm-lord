//! Holds the checked-in descriptor set to the `.proto` it was made from.
//!
//! To refresh it after an intentional change:
//!
//! ```text
//! VMLORD_REFRESH_DESCRIPTOR=1 cargo test -p vmlord-display-protocol
//! ```

use std::{env, fs, path::Path};

/// What `build.rs` compiled from the `.proto` on this run.
const GENERATED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/display.descriptor.bin"));

const CHECKED_IN: &str = "proto/display.descriptor.bin";

#[test]
fn the_checked_in_descriptor_set_matches_the_schema() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(CHECKED_IN);

    if env::var_os("VMLORD_REFRESH_DESCRIPTOR").is_some() {
        fs::write(&path, GENERATED).expect("failed to refresh the descriptor set");
        return;
    }

    let checked_in = fs::read(&path).expect("failed to read the checked-in descriptor set");
    assert_eq!(
        checked_in, GENERATED,
        "{CHECKED_IN} is not what the .proto compiles to; refresh it with \
         VMLORD_REFRESH_DESCRIPTOR=1 cargo test -p vmlord-display-protocol"
    );
}

#[test]
fn the_crate_publishes_the_checked_in_descriptor_set() {
    assert_eq!(
        vmlord_display_protocol::FILE_DESCRIPTOR_SET,
        fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(CHECKED_IN))
            .expect("failed to read the checked-in descriptor set")
    );
}
