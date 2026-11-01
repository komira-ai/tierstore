//! The stand-in `gh` (src/bin/gh-fake.rs) itself: the tests that use it for a reply, a failure
//! or a verification are only as good as it is, so each thing it answers is checked here, with
//! the exit status and the words.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn fake() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gh-fake"))
}

fn dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("gh-fake-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn run(args: &[&str]) -> Output {
    fake().args(args).output().expect("run gh-fake")
}

#[test]
fn the_version_ends_with_a_commit_like_the_real_binary() {
    // Catches: a stand-in binary whose `--version` the build-commit verb cannot read.
    let o = run(&["--version"]);
    assert_eq!(stdout(&o), "gh-fake 0.1.0+0123456789ab\n");
}

#[test]
fn print_and_fail_say_what_they_are_given() {
    // Catches: the generic commands answering with the wrong stream or status.
    let o = run(&["print", "a", "b"]);
    assert_eq!((o.status.code(), stdout(&o).as_str()), (Some(0), "a b\n"));
    let o = run(&["fail", "it", "broke"]);
    assert_eq!(
        (o.status.code(), stderr(&o).as_str()),
        (Some(1), "it broke\n")
    );
}

#[test]
fn api_answers_with_the_prepared_file_and_saves_a_body() {
    // Catches: a reply not read from its file, a method or `--input` taken for the path, and a
    // body that does not reach the file the test reads it from.
    let d = dir("api");
    let path = d.join("reply");
    std::fs::write(&path, "{\"a\":1}").expect("reply");
    let o = fake()
        .args(["api", "-X", "POST"])
        .arg(&path)
        .args(["--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.take().expect("stdin").write_all(b"{\"b\":2}")?;
            child.wait_with_output()
        })
        .expect("run");
    assert_eq!(
        (o.status.code(), stdout(&o).as_str()),
        (Some(0), "{\"a\":1}")
    );
    assert_eq!(
        std::fs::read_to_string(d.join("reply.body")).expect("body"),
        "{\"b\":2}"
    );
}

#[test]
fn api_without_a_reply_fails_and_says_which_kind_of_failure() {
    // Catches: a missing reply read as an empty success, and the 404 and the other failure
    // not told apart (the real programs tell them apart by the words `(HTTP 404)`).
    let d = dir("api-fail");
    let (not_found, failing, nothing) = (d.join("a"), d.join("b"), d.join("c"));
    std::fs::write(d.join("a.404"), "").expect("404");
    std::fs::write(d.join("b.fail"), "").expect("fail");
    let o = run(&["api", not_found.to_str().expect("utf8")]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("(HTTP 404)"), "{}", stderr(&o));
    let o = run(&["api", failing.to_str().expect("utf8")]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("the call failed"), "{}", stderr(&o));
    let o = run(&["api", nothing.to_str().expect("utf8")]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("no reply is prepared"),
        "{}",
        stderr(&o)
    );
    let o = run(&["api"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn deny_exits_with_the_prepared_code_and_prints_the_prepared_stderr() {
    // Catches: a `cargo deny` stand-in that cannot be told what to answer.
    let d = dir("deny");
    let o = fake().arg("deny").current_dir(&d).output().expect("run");
    assert_eq!((o.status.code(), stderr(&o).as_str()), (Some(0), ""));
    std::fs::write(d.join("deny.code"), "3\n").expect("code");
    std::fs::write(d.join("deny.stderr"), "{\"type\":\"log\"}\n").expect("stderr");
    let o = fake().arg("deny").current_dir(&d).output().expect("run");
    assert_eq!(o.status.code(), Some(3));
    assert_eq!(stderr(&o), "{\"type\":\"log\"}\n");
}

/// A verification of a file in a directory of its own whose ledger is `ledger` and whose
/// `expect.txt` is `expect`, run with `env` added to the environment.
fn verify_expecting(
    name: &str,
    ledger: &str,
    expect: &str,
    flags: &[&str],
    env: &[(&str, &str)],
) -> Output {
    let d = dir(name);
    let file: &Path = &d.join("asset");
    std::fs::write(file, "bytes").expect("asset");
    std::fs::write(d.join("ledger.txt"), ledger).expect("ledger");
    std::fs::write(d.join("expect.txt"), expect).expect("expect");
    fake()
        .args(["attestation", "verify"])
        .arg(file)
        .args(flags)
        .envs(env.iter().copied())
        .output()
        .expect("run")
}

fn verify(name: &str, ledger: &str, flags: &[&str]) -> Output {
    verify_expecting(name, ledger, "", flags, &[])
}

const BYTES_SHA: &str = "bb2a1d4b0e45c9a0a3a55c5e1c1b9d8f0e1d1c0f6f0c8a7d2e6b1a9c4d3e2f10";
const SIGNER: &str = "example/tierstore/.github/workflows/artifacts.yml";

#[test]
fn a_verification_needs_the_branch_and_the_runner_flag_and_a_ledger_line() {
    // Catches: a stand-in that accepts without the flags a deployment passes, or without a
    // line that describes the attestation.
    let good = [
        "--source-ref",
        "refs/heads/main",
        "--deny-self-hosted-runners",
        "--signer-workflow",
        SIGNER,
    ];
    let ledger = format!("*|{SIGNER}|https://slsa.dev/provenance/v1|-|-\n");
    let o = verify("ok", &ledger, &good);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    let o = verify(
        "branch",
        &ledger,
        &[
            "--source-ref",
            "refs/heads/dev",
            "--deny-self-hosted-runners",
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("wrong --source-ref"), "{}", stderr(&o));
    let o = verify("runner", &ledger, &["--source-ref", "refs/heads/main"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("wrong --source-ref"), "{}", stderr(&o));

    let o = verify("noline", "", &good);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("no attestation in the ledger"),
        "{}",
        stderr(&o)
    );
    let wrong = format!("{BYTES_SHA}|{SIGNER}|https://slsa.dev/provenance/v1|-|-\nnot a line\n");
    let o = verify("wrongsubject", &wrong, &good);
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn a_verification_of_nothing_or_of_an_unreadable_file_fails() {
    // Catches: arguments the stand-in does not understand, or a file it cannot read, accepted.
    let o = run(&["attestation", "verify"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        stderr(&o).contains("only `attestation verify"),
        "{}",
        stderr(&o)
    );
    let o = run(&["list", "all", "of", "it"]);
    assert_eq!(o.status.code(), Some(2));
    let o = run(&["attestation", "list", "file"]);
    assert_eq!(o.status.code(), Some(2));
    let o = run(&["attestation", "verify", "/nonexistent/file", "--repo", "x"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("cannot read"), "{}", stderr(&o));
}

#[test]
fn a_line_that_names_a_bundle_is_found_with_that_bundle_only() {
    // Catches: an attestation bound to a bundle and a commit satisfying a check made without
    // them, and a bundle given without its commit being accepted.
    let flags = [
        "--source-ref",
        "refs/heads/main",
        "--deny-self-hosted-runners",
        "--signer-workflow",
        SIGNER,
    ];
    let ledger = format!("*|{SIGNER}|https://slsa.dev/provenance/v1|abc|0123\n");
    let o = verify("bundled-online", &ledger, &flags);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    let mut with_bundle = flags.to_vec();
    with_bundle.extend([
        "--bundle",
        concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
    ]);
    let o = verify("bundle-no-commit", &ledger, &with_bundle);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
}

const BUNDLED: &str =
    "*|example/tierstore/.github/workflows/artifacts.yml|https://slsa.dev/provenance/v1|*|*\n";

/// The flags of a good verification of a bundle, then `more`.
fn bundle_flags<'a>(more: &[&'a str]) -> Vec<&'a str> {
    let mut flags = vec![
        "--source-ref",
        "refs/heads/main",
        "--deny-self-hosted-runners",
        "--signer-workflow",
        SIGNER,
        "--repo",
        "example/tierstore",
        "--source-digest",
        "0123",
        "--bundle",
        concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
    ];
    flags.extend_from_slice(more);
    flags
}

fn refused_for(o: &Output, why: &str) {
    assert_eq!(o.status.code(), Some(1), "{}", stderr(o));
    assert!(stderr(o).contains(why), "{}", stderr(o));
}

#[test]
fn the_repository_the_branch_and_the_issuer_are_held_to_the_expectation() {
    // Catches: a stand-in that ignores `--repo`, `--cert-oidc-issuer` or `--source-ref`, so a
    // verb that drops or alters them goes unnoticed.
    let expect =
        "repo=example/tierstore\nissuer=https://issuer.example\nsource-ref=refs/heads/dev\n";
    let flags = |repo: &'static str, issuer: &'static str, branch: &'static str| {
        let mut f = vec![
            "--source-ref",
            branch,
            "--deny-self-hosted-runners",
            "--signer-workflow",
            SIGNER,
        ];
        f.extend(["--repo", repo, "--source-digest", "0123", "--bundle"]);
        f.push(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
        f.extend(["--cert-oidc-issuer", issuer]);
        f
    };
    let good = flags(
        "example/tierstore",
        "https://issuer.example",
        "refs/heads/dev",
    );
    let o = verify_expecting("exp-good", BUNDLED, expect, &good, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let o = verify_expecting(
        "exp-repo",
        BUNDLED,
        expect,
        &flags("example/other", "https://issuer.example", "refs/heads/dev"),
        &[],
    );
    refused_for(&o, "wrong --repo");
    let o = verify_expecting(
        "exp-issuer",
        BUNDLED,
        expect,
        &flags(
            "example/tierstore",
            "https://other.example",
            "refs/heads/dev",
        ),
        &[],
    );
    refused_for(&o, "wrong --cert-oidc-issuer");
    let o = verify_expecting(
        "exp-branch",
        BUNDLED,
        expect,
        &flags(
            "example/tierstore",
            "https://issuer.example",
            "refs/heads/main",
        ),
        &[],
    );
    refused_for(&o, "wrong --source-ref");
    // A bundle verification that names no issuer at all, and one with no repository.
    let mut no_issuer = bundle_flags(&[]);
    let o = verify_expecting(
        "exp-no-issuer",
        BUNDLED,
        "repo=example/tierstore\nissuer=https://issuer.example\n",
        &no_issuer,
        &[],
    );
    refused_for(&o, "wrong --cert-oidc-issuer");
    no_issuer.drain(5..7);
    let o = verify_expecting(
        "exp-no-repo",
        BUNDLED,
        "repo=example/tierstore\n",
        &no_issuer,
        &[],
    );
    refused_for(&o, "wrong --repo");
}

#[test]
fn the_trusted_root_and_the_proxies_are_held_to_the_expectation_for_bundles_only() {
    // Catches: a stand-in that does not look at `--custom-trusted-root` or at the environment,
    // so a verb that drops the root or leaves a bypass list in place goes unnoticed; and
    // expectations applied to a verification without a bundle, which has no root.
    let proxies = [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .map(|name| (name, "http://127.0.0.1:9"));
    let expect = "root=/some/root.jsonl\nproxy=http://127.0.0.1:9\n";
    let with_root = bundle_flags(&["--custom-trusted-root", "/some/root.jsonl"]);
    let o = verify_expecting("root-good", BUNDLED, expect, &with_root, &proxies);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    let o = verify_expecting(
        "root-missing",
        BUNDLED,
        expect,
        &bundle_flags(&[]),
        &proxies,
    );
    refused_for(&o, "wrong --custom-trusted-root");
    let other = bundle_flags(&["--custom-trusted-root", "/other/root.jsonl"]);
    let o = verify_expecting("root-other", BUNDLED, expect, &other, &proxies);
    refused_for(&o, "wrong --custom-trusted-root");
    let o = verify_expecting("root-unwanted", BUNDLED, "root=-\n", &with_root, &[]);
    refused_for(&o, "wrong --custom-trusted-root");
    let o = verify_expecting("root-none", BUNDLED, "root=-\n", &bundle_flags(&[]), &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));

    let o = verify_expecting("proxy-unset", BUNDLED, expect, &with_root, &[]);
    refused_for(&o, "wrong proxy environment");
    let mut bypass = proxies.to_vec();
    bypass.push(("NO_PROXY", "localhost"));
    let o = verify_expecting("proxy-bypass", BUNDLED, expect, &with_root, &bypass);
    refused_for(&o, "wrong proxy environment");
    let mut lower = proxies.to_vec();
    lower.push(("no_proxy", "localhost"));
    let o = verify_expecting("proxy-bypass-lower", BUNDLED, expect, &with_root, &lower);
    refused_for(&o, "wrong proxy environment");

    // Without a bundle, nothing about a root is held.
    let online =
        "*|example/tierstore/.github/workflows/artifacts.yml|https://slsa.dev/provenance/v1|-|-\n";
    let flags = [
        "--source-ref",
        "refs/heads/main",
        "--deny-self-hosted-runners",
        "--signer-workflow",
        SIGNER,
    ];
    let o = verify_expecting(
        "online",
        online,
        "root=/some/root.jsonl\nissuer=x\n",
        &flags,
        &[],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
}

#[test]
fn a_flag_with_no_value_at_the_end_is_ignored() {
    // Catches: a stand-in that panics on, or invents a value for, a trailing flag.
    let ledger = format!("*|{SIGNER}|https://slsa.dev/provenance/v1|-|-\n");
    let flags = [
        "--source-ref",
        "refs/heads/main",
        "--deny-self-hosted-runners",
        "--signer-workflow",
        SIGNER,
        "--repo",
    ];
    let o = verify("trailing", &ledger, &flags);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
}

#[test]
fn a_verification_appends_its_arguments_to_the_record_file_when_one_is_named() {
    // Catches: a record that misses or reorders an argument, splits a value from its flag,
    // replaces what an earlier verification wrote instead of appending to it, or is written
    // when no test asked for one.
    let path = dir("record-file").join("args.txt");
    let expect = format!("record={}\n", path.display());
    let first = bundle_flags(&["--custom-trusted-root", "/some/root.jsonl"]);
    let second = bundle_flags(&["--cert-oidc-issuer", "https://issuer.example"]);
    let mut want = String::new();
    for (name, flags) in [("record-a", &first), ("record-b", &second)] {
        let o = verify_expecting(name, BUNDLED, &expect, flags, &[]);
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
        let asset = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("gh-fake-{name}"))
            .join("asset");
        let mut args = vec!["attestation", "verify", asset.to_str().expect("utf8")];
        args.extend_from_slice(flags);
        want.push_str(&format!("{}\n\n", args.join("\n")));
    }
    assert_eq!(std::fs::read_to_string(&path).expect("the record"), want);

    let o = verify_expecting("record-none", BUNDLED, "", &first, &[]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let none = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gh-fake-record-none");
    assert_eq!(
        std::fs::read_dir(&none).expect("dir").count(),
        3,
        "only the asset, the ledger and the expectation"
    );
}
