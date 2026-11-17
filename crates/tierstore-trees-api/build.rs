//! Generates the messages, and the client and server stubs of `TreesService`, from
//! `proto/tierstore/trees/v1/trees.proto`, and those of jobscheduler's `MountProvider`, which
//! tierstore serves, from `proto/jobscheduler/provider/v1/provider.proto`.
//!
//! The `.proto` is compiled by `protox`, a Rust implementation of `protoc`, so the build needs no
//! `protoc` binary and downloads nothing. The client stub is generated with the `client` feature
//! and the server stub with the `server` feature, so a consumer compiles only the half it uses.

#![expect(
    clippy::disallowed_macros,
    reason = "cargo reads the directives of a build script from its standard output"
)]

use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto");
    let protos = [
        "tierstore/trees/v1/trees.proto",
        "jobscheduler/provider/v1/provider.proto",
    ];
    let descriptors = protox::compile(protos, ["proto"])?;
    tonic_prost_build::configure()
        .build_client(cfg!(feature = "client"))
        .build_server(cfg!(feature = "server"))
        .compile_fds(descriptors)?;
    Ok(())
}
