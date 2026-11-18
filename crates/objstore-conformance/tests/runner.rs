//! The runner `objstore-conformance` against the loopback fake S3 service: its report, its
//! `ETAG` line and its exit codes, run in process (`cli::run`) for the exact output and as the
//! binary for the exit codes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use clap::Parser as _;
use md5::{Digest as _, Md5};
use objstore_conformance::cli::{self, CliError, Flags};
use objstore_s3::s3::fake::FakeS3;
use tierstore_testkit::dir::TestDir;

const BIN: &str = env!("CARGO_BIN_EXE_objstore-conformance");

const PASSED: &str = "PASS claims\nPASS create-only\nPASS identical-put\nPASS create-race\n\
                      PASS streamed\nPASS range\nPASS head\nPASS delete\nPASS list\nPASS pages\n";

/// The fake service, requiring the key pair `AKIDRUNNER` / `runner-secret`, and a scratch
/// directory holding that secret, with a trailing newline, in `secret`.
async fn service() -> (FakeS3, TestDir, PathBuf) {
    let s3 = FakeS3::start().await;
    s3.require_credentials("AKIDRUNNER", "runner-secret");
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "conformance-runner");
    let secret = dir.join("secret");
    std::fs::write(&secret, "runner-secret\n").unwrap();
    (s3, dir, secret)
}

/// The flags of a run against `endpoint` with the secret in `secret`, and `more`.
fn flags(endpoint: &str, secret: &Path, more: &[&str]) -> Vec<String> {
    let mut all: Vec<String> = [
        "objstore-conformance",
        "--endpoint",
        endpoint,
        "--bucket",
        "tierstore-fake",
        "--access-key-id",
        "AKIDRUNNER",
        "--secret-access-key-file",
        secret.to_str().unwrap(),
    ]
    .iter()
    .map(ToString::to_string)
    .collect();
    all.extend(more.iter().map(ToString::to_string));
    all
}

/// Runs the binary with `args` (the first, the program name, is dropped) off the runtime.
async fn binary(args: Vec<String>) -> Output {
    let run = move || Command::new(BIN).args(&args[1..]).output().unwrap();
    tokio::task::spawn_blocking(run).await.unwrap()
}

/// The `ETAG` line of the fake service, which gives the quoted MD5 of the body.
fn md5_line() -> String {
    let md5 = hex::encode(Md5::digest(b"the entity tag of this body"));
    format!("ETAG \"{md5}\" for a body whose MD5 is \"{md5}\"\n")
}

/// Asserts that a run in process against the fake service, with the secret read from its file
/// (its newline dropped, or the service would refuse the signature), reports every case passed
/// and the entity tag the service gave beside the MD5 of the body, and leaves nothing behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_reports_every_case_and_the_entity_tag() {
    let (s3, _dir, secret) = service().await;
    let args = flags(
        s3.endpoint(),
        &secret,
        &[
            "--prefix",
            "runs/1/",
            "--identical",
            "refused",
            "--create-bucket",
        ],
    );
    let mut out = Vec::new();
    let passed = cli::run(Flags::parse_from(args), &mut out).await.unwrap();
    assert!(passed);
    assert_eq!(
        String::from_utf8(out).unwrap(),
        format!("{PASSED}{}", md5_line())
    );
    // `--create-bucket` sent `PUT /tierstore-fake`, which this fake keeps as an object of no name.
    assert_eq!(s3.keys(), [""]);
}

/// Asserts the exit codes of the binary: 0 when every case passed, 1 when one did not (the store
/// declared to accept an identical write refuses it), 2 when it could not run (a secret file that
/// is not there, a prefix too long, an unknown flag), and 0 for `--help`, with what it says on
/// standard error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_exits_by_what_it_found() {
    let (s3, dir, secret) = service().await;
    let run = |more: &[&str]| binary(flags(s3.endpoint(), &secret, more));
    let passed = run(&["--prefix", "runs/2/", "--identical", "refused"]).await;
    assert_eq!(passed.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(passed.stdout).unwrap(),
        format!("{PASSED}{}", md5_line())
    );

    let failed = run(&["--prefix", "runs/3/", "--identical", "accepted"]).await;
    assert_eq!(failed.status.code(), Some(1));
    let out = String::from_utf8(failed.stdout).unwrap();
    assert!(
        out.contains("\nFAIL identical-put: put_new of runs/3/identical-put/object failed: "),
        "{out}"
    );

    let missing = dir.join("no-such-secret");
    let args = flags(
        s3.endpoint(),
        &missing,
        &["--prefix", "p/", "--identical", "refused"],
    );
    let unread = binary(args).await;
    assert_eq!(unread.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(unread.stderr).unwrap(),
        format!(
            "objstore-conformance: reading the secret access key from {missing:?}: No such file \
             or directory (os error 2)\n"
        )
    );

    let long = "p".repeat(1025);
    let too_long = run(&["--prefix", &long, "--identical", "refused"]).await;
    assert_eq!(too_long.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(too_long.stderr).unwrap(),
        "objstore-conformance: --prefix: object key has 1025 bytes; at most 1024 are allowed\n"
    );

    let unknown = run(&["--prefix", "p/", "--identical", "refused", "--colour"]).await;
    assert_eq!(unknown.status.code(), Some(2));
    let said = String::from_utf8(unknown.stderr).unwrap();
    assert!(
        said.starts_with("error: unexpected argument '--colour' found"),
        "{said}"
    );

    let help = binary(vec!["objstore-conformance".into(), "--help".into()]).await;
    assert_eq!(help.status.code(), Some(0));
    assert!(
        String::from_utf8(help.stderr)
            .unwrap()
            .contains("--secret-access-key-file")
    );
}

/// Asserts that a root that leaves no room for a key fails every case and makes the entity tag
/// unknown, saying why, and that a client the S3 configuration refuses is an error that names it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_root_with_no_room_and_a_bad_endpoint_are_reported() {
    let (s3, _dir, secret) = service().await;
    let root = "r".repeat(1021);
    let args = flags(
        s3.endpoint(),
        &secret,
        &["--prefix", &root, "--identical", "refused"],
    );
    let mut out = Vec::new();
    let passed = cli::run(Flags::parse_from(args), &mut out).await.unwrap();
    assert!(!passed);
    let out = String::from_utf8(out).unwrap();
    assert_eq!(out.matches("\nFAIL ").count() + 1, 10, "{out}");
    assert!(
        out.ends_with(
            "\nETAG unknown: store broke the protocol: object key has 1025 bytes; at most 1024 \
             are allowed\n"
        ),
        "{out}"
    );

    let args = flags(
        "ftp://nowhere",
        &secret,
        &["--prefix", "p/", "--identical", "refused"],
    );
    let refused = cli::run(Flags::parse_from(args), &mut Vec::new()).await;
    assert!(matches!(refused, Err(CliError::Client(_))), "{refused:?}");
    assert!(
        refused
            .unwrap_err()
            .to_string()
            .starts_with("the S3 client: ")
    );
}

/// Asserts that a run signs every request for the region `--region` names, and only that one, and
/// still passes every case (the fake service takes any region, and keeps the ones it was given).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_signs_for_the_region_named() {
    let (s3, _dir, secret) = service().await;
    let more = [
        "--prefix",
        "runs/4/",
        "--identical",
        "refused",
        "--region",
        "eu-west-3",
    ];
    let args = flags(s3.endpoint(), &secret, &more);
    let mut out = Vec::new();
    let passed = cli::run(Flags::parse_from(args), &mut out).await.unwrap();
    assert!(passed, "{}", String::from_utf8_lossy(&out));
    assert_eq!(s3.regions(), ["eu-west-3"]);
}
