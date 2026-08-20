//! Turns `proto/vmlord/display/v1/display.proto` into Rust, without `protoc`.
//!
//! `protox` parses the schema in-process and hands `prost-build` the same
//! `FileDescriptorSet` a `protoc` invocation would have. The descriptor is
//! also written out whole, so that `tests/descriptor.rs` can hold the
//! checked-in copy to it.

use std::{env, fs, path::PathBuf};

use prost::Message;

const PROTO: &str = "proto/vmlord/display/v1/display.proto";
const INCLUDE: &str = "proto";

fn main() {
    println!("cargo::rerun-if-changed={PROTO}");

    let descriptor_set = protox::compile([PROTO], [INCLUDE])
        .unwrap_or_else(|error| panic!("failed to compile {PROTO}: {error}"));

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    fs::write(
        out_dir.join("display.descriptor.bin"),
        descriptor_set.encode_to_vec(),
    )
    .expect("failed to write the descriptor set");

    prost_build::Config::new()
        .compile_fds(descriptor_set)
        .expect("failed to generate Rust types");
}
