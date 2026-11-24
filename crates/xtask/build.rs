//! Generates the messages and the client and server stubs of the part of the Remote Execution API
//! that `mac-farm` speaks (src/mac_farm.rs), from `proto/`: the client is the verb's, the server
//! stubs are its tests' fake farm.
//!
//! The `.proto` files are compiled by `protox`, a Rust implementation of `protoc`, so the build
//! needs no `protoc` binary and downloads nothing. The well-known `google.protobuf` types map to
//! `prost-types`.

#![expect(
    clippy::disallowed_macros,
    reason = "cargo reads the directives of a build script from its standard output"
)]

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto");
    let protos = [
        "build/bazel/remote/execution/v2/remote_execution.proto",
        "google/bytestream/bytestream.proto",
        "google/longrunning/operations.proto",
        "google/rpc/status.proto",
    ];
    let descriptors = protox::compile(protos, ["proto"])?;
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        // The same message encodes to the same bytes, and the job's output tail prints in one
        // order.
        .btree_map(".google.rpc.ErrorInfo")
        .compile_fds(descriptors)?;
    Ok(())
}
