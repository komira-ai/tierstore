//! The comparison step of the `channel` job of `.github/workflows/hostkeeper-desired.yml`, run as
//! the runner runs it: the `run:` text of the workflow file itself, with `bash --noprofile --norc
//! -eo pipefail -c` (GitHub's `bash` shell on `ubuntu-24.04`), the host's `sha256sum`, and the
//! step's two inputs, `INDEX_DIGEST` (what `check` handed on) and `RUNNER_TEMP` (where the wait
//! wrote `served-index.json`), set on the child alone, in a scratch directory.
//!
//! Catches: a comparison that passes when the channel serves bytes other than the index `check`
//! checked (one byte changed, a newline added, an empty file), when the served file is missing,
//! or when the digest `check` handed on is empty or not a SHA-256; and one that refuses the
//! index the channel serves unchanged.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tierstore_testkit::dir::TestDir;

const WORKFLOW: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/hostkeeper-desired.yml"
);

/// An index as `cargo xtask release desired` hands it on: JSON text with its final newline.
const INDEX: &[u8] = b"{\"service\":\"tierstore\",\"version\":\"0.3.96+0123456789ab\"}\n";

/// The `run:` of the comparison step, as the workflow file has it: the one step that runs
/// `sha256sum --check`.
fn comparison() -> String {
    let text = fs::read_to_string(WORKFLOW).unwrap();
    let runs: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("run: "))
        .filter(|r| r.contains("sha256sum --check"))
        .collect();
    assert_eq!(runs.len(), 1, "one comparison step: {runs:?}");
    runs[0].to_owned()
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The comparison run in `runner_temp` with `digest`, and `served` (if any) as the served index.
fn compare(name: &str, digest: &str, served: Option<&[u8]>) -> Output {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    if let Some(bytes) = served {
        fs::write(dir.join("served-index.json"), bytes).unwrap();
    }
    run(&dir, digest)
}

fn run(runner_temp: &Path, digest: &str) -> Output {
    Command::new("/bin/bash")
        .args([
            "--noprofile",
            "--norc",
            "-eo",
            "pipefail",
            "-c",
            &comparison(),
        ])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("RUNNER_TEMP", runner_temp)
        .env("INDEX_DIGEST", digest)
        .output()
        .unwrap()
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the step runs on ubuntu-24.04; macOS has no sha256sum"
)]
fn the_index_served_unchanged_passes() {
    let out = compare("same", &sha(INDEX), Some(INDEX));
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).ends_with("/served-index.json: OK\n"),
        "{out:?}"
    );
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the step runs on ubuntu-24.04; macOS has no sha256sum"
)]
fn other_bytes_served_fail() {
    let mut changed = INDEX.to_vec();
    changed[2] ^= 1;
    let mut longer = INDEX.to_vec();
    longer.push(b'\n');
    let shorter = &INDEX[..INDEX.len() - 1];
    for (name, served) in [
        ("one-byte", &changed[..]),
        ("newline-added", &longer[..]),
        ("newline-dropped", shorter),
        ("empty", &b""[..]),
    ] {
        let out = compare(name, &sha(INDEX), Some(served));
        assert!(!out.status.success(), "{name}: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).ends_with("/served-index.json: FAILED\n"),
            "{name}: {out:?}"
        );
    }
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the step runs on ubuntu-24.04; macOS has no sha256sum"
)]
fn a_missing_served_file_fails() {
    let out = compare("missing", &sha(INDEX), None);
    assert!(!out.status.success(), "{out:?}");
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "the step runs on ubuntu-24.04; macOS has no sha256sum"
)]
fn a_digest_that_is_empty_or_not_a_sha256_fails() {
    let digest = sha(INDEX);
    for (name, given) in [
        ("empty", String::new()),
        ("short", digest[..63].to_owned()),
        ("long", format!("{digest}0")),
        ("not-hex", format!("{}g", &digest[..63])),
    ] {
        let out = compare(name, &given, Some(INDEX));
        assert!(!out.status.success(), "{name}: {out:?}");
    }
}
