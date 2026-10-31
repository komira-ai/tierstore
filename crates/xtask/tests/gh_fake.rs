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

/// A verification of a file `name` in a directory whose ledger is `ledger`.
fn verify(name: &str, ledger: &str, flags: &[&str]) -> Output {
    let d = dir(name);
    let file: &Path = &d.join("asset");
    std::fs::write(file, "bytes").expect("asset");
    std::fs::write(d.join("ledger.txt"), ledger).expect("ledger");
    fake()
        .args(["attestation", "verify"])
        .arg(file)
        .args(flags)
        .output()
        .expect("run")
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
    let o = run(&["attestation", "verify", "/nonexistent/file", "--repo", "x"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("cannot read"), "{}", stderr(&o));
}
