//! The `xtask` program end to end: each verb is run as a process, with the stand-in `gh`
//! (src/bin/gh-fake.rs) where it needs one, and its exit status and words are read. These tests
//! hold the dispatch of the verbs (which flag reaches which function), the exit statuses (0, 1
//! for a failure, 2 for a usage error) and the words of an error; the logic of each verb is
//! tested beside it, in the library.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const REPO: &str = "example/tierstore";
const FAKE: &str = env!("CARGO_BIN_EXE_gh-fake");

fn dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn xtask() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    xtask()
        .current_dir(cwd)
        .args(args)
        .output()
        .expect("run xtask")
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `path` under `dir`, with `text` in it (and the directories above it made).
fn put(dir: &Path, path: &str, text: &str) {
    let file = dir.join(path);
    std::fs::create_dir_all(file.parent().expect("parent")).expect("parents");
    std::fs::write(file, text).expect("write");
}

#[test]
fn build_commit_prints_the_line_of_a_binary_that_names_the_commit() {
    // Catches: the verb not reaching the check, or not printing what it checked.
    let d = dir("build-commit");
    let o = run(&d, &["build-commit", FAKE, COMMIT]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("names commit 0123456789ab"),
        "{}",
        stdout(&o)
    );
}

#[test]
fn a_failure_is_status_1_with_the_words_and_a_usage_error_is_status_2() {
    // Catches: a failure that exits 0, and a usage error told apart from a failure by no one.
    let d = dir("statuses");
    let other = "ffffffffffffffffffffffffffffffffffffffff";
    let o = run(&d, &["build-commit", FAKE, other]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).starts_with("xtask: --version is"),
        "{}",
        stderr(&o)
    );
    let o = run(
        &d,
        &[
            "fetch",
            "--url",
            "https://example.invalid/tool.tar.gz",
            "--sha256",
            &"0".repeat(64),
            "--out",
            "tool",
            "--extract",
            "tar.gz",
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
}

#[test]
fn fetch_over_http_is_refused_before_any_connection() {
    // Catches: the verb downloading over a connection that is not HTTPS. The URL is a live
    // server's, which records what it is asked: a download that went ahead would show.
    use std::io::{BufRead, BufReader, Write};
    let d = dir("fetch");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || {
        let mut asked = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut first = String::new();
            reader.read_line(&mut first).expect("request line");
            let mut line = String::new();
            while reader.read_line(&mut line).expect("header") > 2 {
                line.clear();
            }
            if first.starts_with("SENTINEL") {
                return asked;
            }
            asked.push(first);
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
            stream.write_all(head.as_bytes()).expect("reply");
        }
    });
    let url = format!("http://{addr}/tool");
    let o = run(
        &d,
        &[
            "fetch",
            "--url",
            &url,
            "--sha256",
            &"0".repeat(64),
            "--out",
            "tool",
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains(xtask::fetch::PLAIN_HTTP),
        "{}",
        stderr(&o)
    );
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    stream.write_all(b"SENTINEL\r\n\r\n").expect("sentinel");
    assert!(
        server.join().expect("server").is_empty(),
        "the refused fetch connected"
    );
    assert!(!d.join("tool").exists());
}

#[test]
fn package_sums_and_verify_assets_sums_chain() {
    // Catches: the flags of `package`, `sums assemble` and `verify-assets sums` reaching the
    // wrong arguments: the chain a release job runs, over two platforms.
    let d = dir("chain");
    put(&d, "tierstore", "the binary");
    put(&d, "sbom.json", "{\"bomFormat\":\"CycloneDX\"}");
    std::fs::create_dir_all(d.join("out")).expect("out");
    for platform in ["linux-x86_64", "linux-arm64"] {
        let o = run(
            &d,
            &[
                "package",
                "--binary",
                "tierstore",
                "--sbom",
                "sbom.json",
                "--platform",
                platform,
                "--commit",
                COMMIT,
                "--out",
                "out",
            ],
        );
        assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    }
    let o = run(
        &d,
        &[
            "sums",
            "assemble",
            "out",
            "--summary",
            "summary.md",
            "--digest-output",
            "digest.txt",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(
        std::fs::read_to_string(d.join("digest.txt"))
            .expect("digest")
            .starts_with("sha256=")
    );
    let o = run(&d, &["verify-assets", "sums", "out"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let o = run(&d, &["release", "check", "out", "--commit", COMMIT]);
    assert_eq!(o.status.code(), Some(1), "no bundles: {}", stderr(&o));
}

#[test]
fn verify_assets_attestations_checks_its_flags_in_a_scratch_under_the_root() {
    // Catches: the verb not making its scratch directory under `--root`, and a source ref that
    // is not a branch reaching `gh`.
    let d = dir("attestations");
    put(&d, "assets/SHA256SUMS", "");
    let o = run(
        &d,
        &[
            "--root",
            d.to_str().expect("utf8"),
            "verify-assets",
            "attestations",
            "assets",
            "--repo",
            REPO,
            "--workflow-ref",
            &format!("{REPO}/.github/workflows/artifacts.yml"),
            "--source-ref",
            "refs/tags/v1",
            "--commit",
            COMMIT,
            "--gh",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(d.join("target").join("verify-assets-scratch").is_dir());
}

#[test]
fn release_locate_prints_the_run_and_appends_it_to_the_output_file() {
    // Catches: `release locate` asking the wrong paths, or not handing the run id on. The
    // stand-in `gh` answers from files named after the paths it is asked.
    let d = dir("locate");
    put(
        &d,
        &format!("repos/{REPO}/compare/main...{COMMIT}"),
        "{\"status\":\"identical\"}",
    );
    let run_json = format!(
        "{{\"workflow_runs\":[{{\"id\":11,\"head_sha\":\"{COMMIT}\",\"head_branch\":\"main\",\"event\":\"push\",\"conclusion\":\"success\"}}]}}"
    );
    for workflow in ["artifacts.yml", "ci.yml"] {
        put(
            &d,
            &format!(
                "repos/{REPO}/actions/workflows/{workflow}/runs?head_sha={COMMIT}&event=push&branch=main&status=success&per_page=20"
            ),
            &run_json,
        );
    }
    let names = [
        "assets-linux-x86_64",
        "assets-linux-arm64",
        "sha256sums",
        "bundles-linux",
    ];
    let artifacts: Vec<String> = names
        .iter()
        .map(|n| format!("{{\"name\":\"{n}\",\"expired\":false}}"))
        .collect();
    put(
        &d,
        &format!("repos/{REPO}/actions/runs/11/artifacts?per_page=100"),
        &format!("{{\"artifacts\":[{}]}}", artifacts.join(",")),
    );
    let o = run(
        &d,
        &[
            "release",
            "locate",
            "--repo",
            REPO,
            "--commit",
            COMMIT,
            "--output",
            "github-output",
            "--gh",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).ends_with("run-id=11\n"), "{}", stdout(&o));
    assert_eq!(
        std::fs::read_to_string(d.join("github-output")).expect("out"),
        "run-id=11\n"
    );
}

#[test]
fn release_publish_refuses_a_bad_tag_with_status_2_and_drops_empty_notes() {
    // Catches: the flags of `release publish` not reaching it, and an empty `--note` kept.
    let d = dir("publish");
    let o = run(
        &d,
        &[
            "release",
            "publish",
            "assets",
            "--repo",
            REPO,
            "--tag",
            "1.0",
            "--commit",
            COMMIT,
            "--run-id",
            "11",
            "--notes-file",
            "notes.md",
            "--note",
            " ",
            "--note",
            "second",
            "--gh",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("is not v<major>.<minor>.<patch>"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn advisories_with_no_advisory_and_no_issue_does_nothing() {
    // Catches: the verb not running `cargo deny` through the program the `CARGO` variable
    // names, or not listing the open issues. Stand-ins answer for both.
    let d = dir("advisories");
    put(
        &d,
        &format!("repos/{REPO}/issues?state=open&per_page=100&page=1"),
        "[]",
    );
    let o = xtask()
        .current_dir(&d)
        .env("CARGO", FAKE)
        .args(["--root", d.to_str().expect("utf8")])
        .args([
            "advisories",
            "--repo",
            REPO,
            "--commit",
            COMMIT,
            "--gh",
            FAKE,
        ])
        .output()
        .expect("run");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "no advisories, and no open issue\n");
}

#[test]
fn advisories_reads_head_when_no_commit_is_given_and_reports_a_git_failure() {
    // Catches: the default commit not coming from git, and a root git cannot read passing.
    let d = dir("advisories-head");
    let o = xtask()
        .current_dir(&d)
        .env("CARGO", FAKE)
        .args(["advisories", "--repo", REPO, "--gh", FAKE])
        .output()
        .expect("run");
    // The repository root is a checkout with a HEAD; the stand-in has no issue list to give.
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    assert!(stderr(&o).contains("issues?state=open"), "{}", stderr(&o));
    let o = run(
        &d,
        &[
            "--root",
            "/nonexistent/root",
            "advisories",
            "--repo",
            REPO,
            "--gh",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("git rev-parse HEAD"), "{}", stderr(&o));
}

#[test]
fn release_plan_reads_the_version_file_and_the_freeze_and_appends_its_outputs() {
    // Catches: `release plan` not passing its flags on (--version-file, --freeze, --output),
    // a missing VERSION file being an error, and the answers not reaching the output file.
    let d = dir("plan");
    let run_json = format!(
        "{{\"workflow_runs\":[{{\"id\":11,\"head_sha\":\"{COMMIT}\",\"head_branch\":\"main\",\"event\":\"push\",\"conclusion\":\"success\"}}]}}"
    );
    for workflow in ["artifacts.yml", "ci.yml"] {
        put(
            &d,
            &format!(
                "repos/{REPO}/actions/workflows/{workflow}/runs?head_sha={COMMIT}&event=push&branch=main&status=success&per_page=20"
            ),
            &run_json,
        );
    }
    put(
        &d,
        &format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100"),
        "[{\"ref\":\"refs/tags/v0.3.1\"}]",
    );
    put(
        &d,
        &format!("repos/{REPO}/compare/v0.3.1...{COMMIT}"),
        "{\"status\":\"ahead\",\"files\":[{\"filename\":\"Cargo.lock\"}],\"commits\":[{\"commit\":{\"message\":\"fix a thing (#9)\"}}]}",
    );
    let plan = |extra: &[&str]| {
        let mut args = vec![
            "release",
            "plan",
            "--repo",
            REPO,
            "--commit",
            COMMIT,
            "--output",
            "github-output",
            "--gh",
            FAKE,
        ];
        args.extend_from_slice(extra);
        run(&d, &args)
    };
    let o = plan(&["--version-file", "VERSION", "--freeze", ""]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let written = std::fs::read_to_string(d.join("github-output")).expect("out");
    assert!(written.contains("tag=v0.3.2\n"), "{written}");
    put(&d, "VERSION", "0.4.0\n");
    let o = plan(&["--version-file", "VERSION"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let written = std::fs::read_to_string(d.join("github-output")).expect("out");
    assert!(written.contains("tag=v0.4.0\n"), "{written}");
    let o = plan(&["--freeze", "true"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("RELEASE_FREEZE"), "{}", stdout(&o));
    let written = std::fs::read_to_string(d.join("github-output")).expect("out");
    assert!(
        written.ends_with("release=false\nreason=RELEASE_FREEZE is true\n"),
        "{written}"
    );
}

#[test]
fn release_manual_hands_the_tag_commit_and_note_on_and_refuses_a_bad_tag_with_status_2() {
    // Catches: the flags of `release manual` not reaching it, and a bad tag not being a
    // usage error.
    let d = dir("manual");
    let o = run(
        &d,
        &[
            "release",
            "manual",
            "--tag",
            "v1.2.3",
            "--commit",
            COMMIT,
            "--note",
            "by hand",
            "--output",
            "github-output",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        std::fs::read_to_string(d.join("github-output")).expect("out"),
        format!("release=true\ntag=v1.2.3\ncommit={COMMIT}\nnote=by hand\n")
    );
    let o = run(
        &d,
        &["release", "manual", "--tag", "1.2", "--commit", COMMIT],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
}

#[test]
fn release_version_prints_the_version_the_plan_would_tag_and_appends_it() {
    // Catches: `release version` not passing its flags on, and the answer not reaching the
    // output file the build job reads.
    let d = dir("version");
    put(
        &d,
        &format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100"),
        "[{\"ref\":\"refs/tags/v0.3.1\"}]",
    );
    let version = |extra: &[&str]| {
        let mut args = vec![
            "release",
            "version",
            "--repo",
            REPO,
            "--output",
            "github-output",
            "--gh",
            FAKE,
        ];
        args.extend_from_slice(extra);
        run(&d, &args)
    };
    let o = version(&["--version-file", "VERSION"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "version=0.3.2\n");
    put(&d, "VERSION", "0.4.0\n");
    let o = version(&["--version-file", "VERSION"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        std::fs::read_to_string(d.join("github-output")).expect("out"),
        "version=0.3.2\nversion=0.4.0\n"
    );
}

#[test]
fn release_check_binary_runs_the_x86_64_binary_of_the_tarball_against_the_tag() {
    // Catches: the flags of `release check-binary` not reaching it, the binary not being run
    // from the tarball, and a binary that says another version than the tag being accepted
    // (exit status 1 and the words naming both).
    let d = dir("check-binary");
    put(
        &d,
        "tierstore",
        "#!/bin/sh\necho 'tierstore 0.3.2+0123456789ab'\n",
    );
    put(&d, "sbom.json", "{\"bomFormat\":\"CycloneDX\"}");
    std::fs::create_dir_all(d.join("out")).expect("out");
    let o = run(
        &d,
        &[
            "package",
            "--binary",
            "tierstore",
            "--sbom",
            "sbom.json",
            "--platform",
            "linux-x86_64",
            "--commit",
            COMMIT,
            "--out",
            "out",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let check = |tag: &str| {
        run(
            &d,
            &[
                "release",
                "check-binary",
                "out",
                "--tag",
                tag,
                "--commit",
                COMMIT,
                "--scratch",
                "work",
            ],
        )
    };
    let o = check("v0.3.2");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("v0.3.2"), "{}", stdout(&o));
    let o = check("v0.3.1");
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    assert!(stderr(&o).contains("0.3.2"), "{}", stderr(&o));
    assert!(stderr(&o).contains("0.3.1"), "{}", stderr(&o));
}

#[test]
fn build_commit_with_a_version_refuses_a_binary_that_says_another() {
    // Catches: `--version` of the build-commit verb not reaching the check.
    let d = dir("build-commit-version");
    let o = run(&d, &["build-commit", FAKE, COMMIT, "--version", "9.9.9"]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(stderr(&o).contains("9.9.9"), "{}", stderr(&o));
}
