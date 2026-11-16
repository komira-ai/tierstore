//! Generates the messages, and the client and server stubs of `TreesService`, from
//! `proto/tierstore/trees/v1/trees.proto`.
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
    let descriptors = protox::compile(["tierstore/trees/v1/trees.proto"], ["proto"])?;
    tonic_prost_build::configure()
        .build_client(cfg!(feature = "client"))
        .build_server(cfg!(feature = "server"))
        .compile_fds(descriptors)?;
    Ok(())
}
