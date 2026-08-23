//! Turns the launch pipes' private schema into Rust, without `protoc`.
//!
//! The same in-process `protox` compile the other display crates use. The
//! include path reaches into `display-protocol`'s `proto` directory because the
//! hand-over names that schema's `Capability` and `Mode` rather than restating
//! them; `extern_path` then points those two names at the types that crate
//! already generates, so the wire contract has exactly one Rust definition.

const PROTO: &str = "proto/vmlord/display/viewer/viewer.proto";
const INCLUDE: &str = "proto";
const PROTOCOL_INCLUDE: &str = "../display-protocol/proto";

fn main() {
    println!("cargo::rerun-if-changed={PROTO}");
    println!("cargo::rerun-if-changed={PROTOCOL_INCLUDE}");

    let descriptor_set = protox::compile([PROTO], [INCLUDE, PROTOCOL_INCLUDE])
        .unwrap_or_else(|error| panic!("failed to compile {PROTO}: {error}"));

    prost_build::Config::new()
        .extern_path(".vmlord.display.v1", "::vmlord_display_protocol::v1")
        .compile_fds(descriptor_set)
        .expect("failed to generate Rust types");
}
