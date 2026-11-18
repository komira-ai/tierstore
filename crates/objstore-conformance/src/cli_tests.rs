//! The client configuration the runner builds from its flags.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser as _;

use super::{Flags, s3_config};

/// The flags of a run, with `more` after the required ones.
fn flags(more: &[&str]) -> Flags {
    let mut args = vec![
        "objstore-conformance",
        "--endpoint",
        "http://127.0.0.1:9",
        "--bucket",
        "conformance",
        "--access-key-id",
        "AKID",
        "--secret-access-key-file",
        "secret",
        "--prefix",
        "runs/",
        "--identical",
        "refused",
    ];
    args.extend_from_slice(more);
    Flags::parse_from(args)
}

/// Asserts that `--timeout-secs` is the limit on opening a connection and on each request, and
/// that without it both are 30 seconds.
#[test]
fn the_timeout_flag_limits_connections_and_requests() {
    let config = s3_config(flags(&["--timeout-secs", "7"]), "secret");
    assert_eq!(config.connect_timeout, Duration::from_secs(7));
    assert_eq!(config.request_timeout, Duration::from_secs(7));
    assert_eq!(config.min_body_rate, None);
    let config = s3_config(flags(&[]), "secret");
    assert_eq!(config.connect_timeout, Duration::from_secs(30));
    assert_eq!(config.request_timeout, Duration::from_secs(30));
}

/// Asserts that the region, endpoint and bucket are the flags' (the region `us-east-1` without
/// `--region`), that the store claims conditional writes and not Object Lock, and that the flags
/// name the secret's file, not the secret.
#[test]
fn the_other_flags_are_the_clients_configuration() {
    let named = flags(&["--region", "eu-west-3"]);
    assert_eq!(named.secret_access_key_file, PathBuf::from("secret"));
    let config = s3_config(named, "s3cr3t\n");
    assert_eq!(config.region, "eu-west-3");
    assert_eq!(config.endpoint, "http://127.0.0.1:9");
    assert_eq!(config.bucket, "conformance");
    assert!(config.capabilities.conditional_put);
    assert!(!config.capabilities.object_lock);
    assert_eq!(s3_config(flags(&[]), "s").region, "us-east-1");
}
