//! Turns the broker's private schema into Rust, without `protoc`.
//!
//! The same `protox` in-process compile `vmlord-display-protocol` uses, and for
//! the same reason: nothing has to be installed on the machine that builds a
//! guest.

const PROTO: &str = "proto/vmlord/display/broker/broker.proto";
const INCLUDE: &str = "proto";

fn main() {
    println!("cargo::rerun-if-changed={PROTO}");

    let descriptor_set = protox::compile([PROTO], [INCLUDE])
        .unwrap_or_else(|error| panic!("failed to compile {PROTO}: {error}"));

    prost_build::Config::new()
        .compile_fds(descriptor_set)
        .expect("failed to generate Rust types");
}
