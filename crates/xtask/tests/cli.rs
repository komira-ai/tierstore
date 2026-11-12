//! The `xtask` program end to end: each verb is run as a process, with the stand-in `gh`
//! (src/bin/gh-fake.rs) where it needs one, and its exit status and words are read. These tests
//! hold the dispatch of the verbs (which flag reaches which function), the exit statuses (0, 1
//! for a failure, 2 for a usage error) and the words of an error; the logic of each verb is
//! tested beside it, in the library.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

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
    // wrong arguments: the chain a release job runs, over every platform.
    let d = dir("chain");
    put(&d, "tierstore", "the binary");
    put(&d, "sbom.json", "{\"bomFormat\":\"CycloneDX\"}");
    std::fs::create_dir_all(d.join("out")).expect("out");
    for platform in ["linux-x86_64", "linux-arm64", "macos-arm64"] {
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
        "assets-macos-arm64",
        "sha256sums",
        "bundles",
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
    // a missing VERSION file being an error, the answers not reaching the output file, and the
    // build run's version record not read through the real `gh` path.
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
        "{\"status\":\"ahead\",\"merge_base_commit\":{\"sha\":\"fedcba9876543210fedcba9876543210fedcba98\"},\"files\":[{\"filename\":\"Cargo.lock\"}],\"commits\":[{\"commit\":{\"message\":\"fix a thing (#9)\"}}]}",
    );
    // The build run recorded the version it built under, as `release version --record` and
    // artifacts.yml's upload name it.
    let recorded = |version: &str| {
        put(
            &d,
            &format!("repos/{REPO}/actions/runs/11/artifacts?per_page=100"),
            &format!("{{\"artifacts\":[{{\"name\":\"version-{version}\"}}]}}"),
        );
    };
    recorded("0.3.2");
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
    recorded("0.4.0");
    let o = plan(&["--version-file", "VERSION"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let written = std::fs::read_to_string(d.join("github-output")).expect("out");
    assert!(written.contains("tag=v0.4.0\n"), "{written}");
    let o = plan(&["--freeze", "true"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert!(stdout(&o).contains("RELEASE_FREEZE"), "{}", stdout(&o));
    let written = std::fs::read_to_string(d.join("github-output")).expect("out");
    assert!(
        written.ends_with("release=false\nreason=RELEASE_FREEZE is set\n"),
        "{written}"
    );
}

#[test]
fn release_manual_hands_the_tag_commit_and_note_on_and_refuses_a_bad_tag_with_status_2() {
    // Catches: the flags of `release manual` not reaching it, and a bad tag not being a
    // usage error.
    let d = dir("manual");
    put(
        &d,
        &format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100"),
        "[]",
    );
    let o = run(
        &d,
        &[
            "release",
            "manual",
            "--repo",
            REPO,
            "--tag",
            "v1.2.3",
            "--commit",
            COMMIT,
            "--note",
            "by hand",
            "--output",
            "github-output",
            "--gh",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        std::fs::read_to_string(d.join("github-output")).expect("out"),
        format!("release=true\ntag=v1.2.3\ncommit={COMMIT}\nnote=by hand\n")
    );
    let o = run(
        &d,
        &[
            "release", "manual", "--repo", REPO, "--tag", "1.2", "--commit", COMMIT, "--gh", FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    // A tag already covers the commit: refused, exit status 1, naming the tag.
    put(
        &d,
        &format!("repos/{REPO}/git/matching-refs/tags/v?per_page=100"),
        "[{\"ref\":\"refs/tags/v1.2.0\"}]",
    );
    put(
        &d,
        &format!("repos/{REPO}/compare/v1.2.0...{COMMIT}"),
        "{\"status\":\"identical\"}",
    );
    let o = run(
        &d,
        &[
            "release", "manual", "--repo", REPO, "--tag", "v1.2.1", "--commit", COMMIT, "--gh",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("v1.2.0 already covers"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn release_version_prints_the_version_the_plan_would_tag_and_appends_it() {
    // Catches: `release version` not passing its flags on, and the answer not reaching the
    // output file the build job reads or the record file it uploads.
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
    let o = version(&["--version-file", "VERSION", "--record", "version"]);
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "version=0.3.2\n");
    assert_eq!(
        std::fs::read_to_string(d.join("version")).expect("record"),
        "0.3.2\n"
    );
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

#[test]
fn release_set_version_writes_the_version_into_the_manifest_and_the_lock_of_the_root() {
    // Catches: `--version` and `--root` not reaching the verb (another directory edited, or a
    // version not written), the line printed not naming both files, and a version of another
    // shape being written (exit status 1, nothing changed).
    let d = dir("set-version");
    let manifest = "[package]\nname = \"tierstore\"\nversion = \"0.3.0\"\n";
    let lock = "[[package]]\nname = \"tierstore\"\nversion = \"0.3.0\"\n";
    put(&d, "crates/tierstore/Cargo.toml", manifest);
    put(&d, "Cargo.lock", lock);
    let root = d.to_str().expect("path");
    let set = |version: &str| {
        run(
            &d,
            &[
                "--root",
                root,
                "release",
                "set-version",
                "--version",
                version,
            ],
        )
    };
    let o = set("0.3.4");
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        "crates/tierstore/Cargo.toml and Cargo.lock: tierstore is 0.3.4\n"
    );
    let read = |name: &str| std::fs::read_to_string(d.join(name)).expect("read");
    let manifest = manifest.replace("0.3.0", "0.3.4");
    let lock = lock.replace("0.3.0", "0.3.4");
    assert_eq!(read("crates/tierstore/Cargo.toml"), manifest);
    assert_eq!(read("Cargo.lock"), lock);
    let o = set("v1.0.0");
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(read("Cargo.lock"), lock);
}

#[test]
fn licenses_writes_the_notice_that_package_puts_in_the_tarball_and_a_refused_one_exits_1() {
    // Catches: the flags of `licenses` (--metadata, --package, --out) and of `package
    // --licenses` not reaching their functions, a tarball listing more or less than the binary
    // and the notice, and a crate that declares no licence exiting 0.
    let d = dir("licenses");
    put(&d, "crates/x/LICENSE", "MIT text of x");
    put(&d, "crates/y/README", "y declares no licence");
    let manifest = |name: &str| d.join("crates").join(name).join("Cargo.toml");
    let meta = |dep: &str| {
        let package = |name: &str, source: &str| {
            format!(
                "{{\"id\":\"{name}\",\"name\":\"{name}\",\"version\":\"1.0.0\",\"source\":{source},\
                 \"manifest_path\":{:?},\"license\":\"MIT\",\"license_file\":null,\"repository\":null}}",
                manifest(name).display().to_string()
            )
        };
        format!(
            "{{\"packages\":[{},{}],\"resolve\":{{\"nodes\":[\
             {{\"id\":\"app\",\"deps\":[{{\"pkg\":\"{dep}\",\"dep_kinds\":[{{\"kind\":null,\"target\":null}}]}}]}},\
             {{\"id\":\"{dep}\",\"deps\":[]}}],\"root\":null}}}}",
            package("app", "null"),
            package(dep, "\"registry+crates.io\"")
        )
    };
    put(&d, "x.json", &meta("x"));
    put(
        &d,
        "y.json",
        &meta("y").replace("\"license\":\"MIT\"", "\"license\":null"),
    );
    let o = run(
        &d,
        &[
            "licenses",
            "--metadata",
            "x.json",
            "--package",
            "app",
            "--out",
            "NOTICE.txt",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(stdout(&o), "wrote NOTICE.txt\n");
    let notice = std::fs::read_to_string(d.join("NOTICE.txt")).expect("notice");
    assert!(
        notice.contains("--- LICENSE ---\nMIT text of x\n"),
        "{notice}"
    );
    let o = run(
        &d,
        &[
            "licenses",
            "--metadata",
            "y.json",
            "--package",
            "app",
            "--out",
            "REFUSED.txt",
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("y 1.0.0: declares no licence"),
        "{}",
        stderr(&o)
    );
    assert!(!d.join("REFUSED.txt").exists());
    put(&d, "tierstore", "the binary");
    put(&d, "sbom.json", "{\"bomFormat\":\"CycloneDX\"}");
    let o = run(
        &d,
        &[
            "package",
            "--binary",
            "tierstore",
            "--sbom",
            "sbom.json",
            "--licenses",
            "NOTICE.txt",
            "--platform",
            "linux-x86_64",
            "--commit",
            COMMIT,
            "--out",
            "out",
        ],
    );
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let listed = Command::new("tar")
        .arg("-tzf")
        .arg(d.join(format!("out/tierstore-{COMMIT}-linux-x86_64.tar.gz")))
        .output()
        .expect("tar -t");
    assert_eq!(stdout(&listed), "tierstore\nTHIRD-PARTY-LICENSES.txt\n");
}

#[test]
fn release_hostkeeper_package_writes_the_package_and_a_malformed_version_exits_2() {
    // Catches: the verb's flags not reaching it (the package is not written, or under another
    // name), or a malformed --version not reported as a usage error.
    let d = dir("release-hostkeeper-package");
    put(
        &d,
        "tierstore",
        "stand-in binary of 0.4.0+0123456789ab and more",
    );
    let args = |version: &'static str| {
        [
            "release",
            "hostkeeper-package",
            "--binary",
            "tierstore",
            "--version",
            version,
            "--min-hostkeeper",
            "0.1.0",
            "--platform",
            "linux-arm64",
            "--out",
            "out",
        ]
    };
    let o = run(&d, &args("0.4.0+0123456789ab"));
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    let name = "tierstore-0.4.0+0123456789ab-linux-arm64.tar.zst";
    assert!(
        stdout(&o).starts_with(&format!("{name}: sha256 ")),
        "{}",
        stdout(&o)
    );
    assert!(d.join("out").join(name).is_file());
    let o = run(&d, &args("0.4.0"));
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("--version \"0.4.0\" is not"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn release_desired_refuses_a_release_without_an_index_with_status_1_and_a_bad_version_with_status_2()
 {
    // Catches: the flags of `release desired` not reaching it (it asks GitHub for the release the
    // version names, through the `gh` given), a release made before the hostkeeper packages
    // passed, and a malformed --version not reported as a usage error.
    let d = dir("release-desired");
    put(
        &d,
        &format!("repos/{REPO}/releases/tags/v0.3.53"),
        &format!(
            "{{\"tag_name\":\"v0.3.53\",\"target_commitish\":\"{COMMIT}\",\"draft\":false,\"prerelease\":false,\"assets\":[{{\"name\":\"SHA256SUMS\",\"size\":10}}]}}"
        ),
    );
    let args = |version: &'static str| {
        [
            "release",
            "desired",
            "--repo",
            REPO,
            "--version",
            version,
            "--dir",
            "desired",
            "--output",
            "github-output",
            "--gh",
            FAKE,
        ]
    };
    let o = run(&d, &args("0.3.53"));
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(
        stderr(&o),
        "xtask: release v0.3.53 has no index.json: it has no hostkeeper packages for a host to take\n"
    );
    assert!(!d.join("desired").exists() && !d.join("github-output").exists());
    let o = run(&d, &args("v0.3.53"));
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert_eq!(
        stderr(&o),
        "xtask: --version \"v0.3.53\" is not <major>.<minor>.<patch>, a release tag without its v\n"
    );
}

#[test]
fn release_desired_after_a_run_that_published_nothing_sets_nothing_and_both_ways_are_refused() {
    // Catches: `--after-run` not reaching the verb (it asks GitHub for the jobs of that run,
    // through the `gh` given), a run whose publish was skipped handing on anything but
    // `set=false`, and `--version` with `--after-run` not reported as a usage error.
    let d = dir("release-desired-after-run");
    put(
        &d,
        &format!("repos/{REPO}/actions/runs/7/jobs?per_page=100"),
        "{\"total_count\":1,\"jobs\":[{\"name\":\"publish\",\"conclusion\":\"skipped\"}]}",
    );
    let args = |version: &'static str| {
        [
            "release",
            "desired",
            "--repo",
            REPO,
            "--version",
            version,
            "--after-run",
            "7",
            "--dir",
            "desired",
            "--output",
            "github-output",
            "--gh",
            FAKE,
        ]
    };
    let o = run(&d, &args(""));
    assert_eq!(o.status.code(), Some(0), "{}", stderr(&o));
    assert_eq!(
        std::fs::read_to_string(d.join("github-output")).expect("the output"),
        "set=false\n"
    );
    assert!(!d.join("desired").exists());
    let o = run(&d, &args("0.3.53"));
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert_eq!(
        stderr(&o),
        "xtask: give one of --version and --after-run, not both and not neither\n"
    );
}

#[test]
fn release_desired_apt_refuses_a_release_without_sums_with_status_1_and_a_bad_version_or_floor_with_status_2()
 {
    // Catches: the flags of `release desired-apt` not reaching it, a release without the digests
    // to hold the testing suite to passing, and a malformed --version or --min-hostkeeper not a
    // usage error.
    let d = dir("release-desired-apt");
    put(
        &d,
        &format!("repos/{REPO}/releases/tags/v0.3.53"),
        &format!(
            "{{\"tag_name\":\"v0.3.53\",\"target_commitish\":\"{COMMIT}\",\"draft\":false,\"prerelease\":false,\"assets\":[]}}"
        ),
    );
    let args = |version: &'static str| {
        [
            "release",
            "desired-apt",
            "--repo",
            REPO,
            "--version",
            version,
            "--min-hostkeeper",
            "0.1.216",
            "--dir",
            "desired",
            "--output",
            "github-output",
            "--gh",
            FAKE,
            "--curl",
            FAKE,
        ]
    };
    let o = run(&d, &args("0.3.53"));
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("release v0.3.53 has no SHA256SUMS"),
        "{}",
        stderr(&o)
    );
    assert!(!d.join("github-output").exists());
    assert_eq!(run(&d, &args("v0.3.53")).status.code(), Some(2));
    let mut floor = args("0.3.53");
    floor[7] = "v0.1.216";
    assert_eq!(run(&d, &floor).status.code(), Some(2));
}
