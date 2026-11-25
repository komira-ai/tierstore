//! `cargo xtask fetch --cache` as a process: the flags reach the cached fetch, a cache hit needs
//! no network, a cached file of other bytes is exit 1, and several `--url` without `--cache` is a
//! usage error (exit 2). The logic itself is tested beside it, in `src/fetch/cache_tests.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::PathBuf;
use std::process::{Command, Output};

use xtask::fetch::Sha256;

const TOOL: &[u8] = b"a pinned toolchain's tarball";
const PIN: &str = "9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f";

/// An empty directory of its own for one test.
fn dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-fetch-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

/// `cargo xtask fetch` with `args`, then two `--url` on ports where nothing answers.
fn fetch(args: &[&std::ffi::OsStr]) -> Output {
    let one = format!("https://{}/one", tierstore_testkit::port::dead_addr());
    let two = format!("https://{}/two", tierstore_testkit::port::dead_addr());
    Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("fetch")
        .args(args)
        .args(["--url", &one, "--url", &two])
        .output()
        .expect("run xtask")
}

#[test]
fn the_verb_installs_from_a_cache_hit_with_no_connection() {
    // Asserts: with --cache holding the pinned bytes the verb exits 0, prints the hit line
    // exactly and installs the file, though no URL answers; with another pin it exits 1 with the
    // exact refusal and installs nothing.
    let d = dir("hit");
    let out = d.join("bin").join("zig");
    let cache = d.join("cache").join("zig.tar.xz");
    std::fs::create_dir_all(d.join("cache")).expect("mkdir");
    std::fs::write(&cache, TOOL).expect("seed the cache");
    let run = |sha256: &str| {
        fetch(&[
            "--sha256".as_ref(),
            sha256.as_ref(),
            "--out".as_ref(),
            out.as_os_str(),
            "--cache".as_ref(),
            cache.as_os_str(),
        ])
    };
    let pin = Sha256::of(TOOL).to_string();
    let hit = run(&pin);
    assert_eq!(
        String::from_utf8_lossy(&hit.stdout),
        format!(
            "ok: the cache hit: {} has SHA-256 {pin}; nothing was downloaded; installed {}\n",
            cache.display(),
            out.display()
        ),
        "{}",
        String::from_utf8_lossy(&hit.stderr)
    );
    assert_eq!(hit.status.code(), Some(0));
    assert_eq!(std::fs::read(&out).expect("installed"), TOOL);
    std::fs::remove_file(&out).expect("remove");
    let refused = run(PIN);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr),
        format!(
            "xtask: the cached {cache:?} has SHA-256 {pin}, not the pinned {PIN}; nothing was \
             installed\n"
        )
    );
    assert!(!out.exists());
}

#[test]
fn several_urls_without_a_cache_exit_2() {
    // Asserts: a second --url without --cache is a usage error (exit 2) before any download,
    // rather than dropped or tried by the one-attempt fetch.
    let d = dir("urls");
    let out = d.join("tool");
    let o = fetch(&[
        "--sha256".as_ref(),
        PIN.as_ref(),
        "--out".as_ref(),
        out.as_os_str(),
    ]);
    let err = String::from_utf8_lossy(&o.stderr);
    assert_eq!(o.status.code(), Some(2), "{err}");
    assert!(
        err.contains("only a cached fetch tries several URLs"),
        "{err}"
    );
    assert!(!out.exists());
}
