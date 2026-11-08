//! The `channels` verbs of the `xtask` program end to end: each is run as a process, and its exit
//! status and words are read. These hold the dispatch of the verbs and their statuses (0, 1 for a
//! failure, 2 for a usage error); the logic of each is tested beside it. `upload` and `tap` run
//! against the stand-in `gh` (src/bin/gh-fake.rs), which answers neither `aws` nor the tap, so
//! they fail where their first call reaches it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FAKE: &str = env!("CARGO_BIN_EXE_gh-fake");

/// An empty directory for the test `name`, whatever an earlier run left in it.
fn fresh(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("cli-channels-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn run(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_xtask"))
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

#[test]
fn the_gate_ends_green_for_all_and_for_none_of_the_settings_and_red_for_some() {
    // Catches: a gate whose answer never reaches GITHUB_OUTPUT, whose notice is not printed for
    // the runner to read, or whose status does not tell a half-made setting from none.
    let d = fresh("gate");
    let out = d.join("out");
    let gate = |settings: &[&str]| {
        let mut args = vec!["channels", "gate", "--label", "tap", "--output", "out"];
        for s in settings {
            args.extend(["--setting", s]);
        }
        run(&d, &args)
    };
    let o = gate(&["A=set", "B=set"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "enabled=true\n");

    let o = gate(&["A=", "B="]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        stdout(&o).starts_with("::notice title=The tap job is off::"),
        "{}",
        stdout(&o)
    );
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "enabled=true\nenabled=false\n"
    );

    let o = gate(&["A=set", "B="]);
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("tap: A set but B not"),
        "{}",
        stderr(&o)
    );

    let o = gate(&["A"]);
    assert_eq!(o.status.code(), Some(2));
    let o = run(&d, &["channels", "gate", "--label", "tap"]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "a gate with no setting is a usage error"
    );
}

#[test]
fn the_formula_is_rendered_from_the_sums_of_a_release() {
    // Catches: a formula verb that is not wired to the command line.
    let d = fresh("formula");
    let sums: String = [
        ("macos-arm64", "1"),
        ("linux-aarch64", "2"),
        ("linux-x86_64", "3"),
    ]
    .iter()
    .map(|(target, digit)| format!("{}  tierstore-1.2.3-{target}.tar.gz\n", digit.repeat(64)))
    .collect();
    std::fs::write(d.join("SHA256SUMS"), sums).unwrap();
    let o = run(
        &d,
        &[
            "channels",
            "formula",
            "--sums",
            "SHA256SUMS",
            "--version",
            "1.2.3",
            "--repo",
            "komira-ai/tierstore",
            "--out",
            "tap/tierstore.rb",
        ],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    let formula = std::fs::read_to_string(d.join("tap/tierstore.rb")).unwrap();
    assert!(
        formula.starts_with("class Tierstore < Formula\n"),
        "{formula}"
    );
    assert!(formula.contains(&format!("sha256 \"{}\"", "2".repeat(64))));
}

#[test]
fn upload_and_tap_report_the_first_call_that_fails_and_a_bad_flag_is_a_usage_error() {
    // Catches: verbs that are not wired to the command line, a failure of `aws` or `gh` that
    // does not end the process with status 1, and a flag out of its form that does not end it
    // with 2.
    let d = fresh("upload-tap");
    let o = run(
        &d,
        &[
            "channels", "upload", "dist", "--tag", "1.2.3", "--commit", "0", "--bucket", "b",
            "--work", "work", "--aws", FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));

    std::fs::write(d.join("tierstore.rb"), "not a formula").unwrap();
    let o = run(
        &d,
        &[
            "channels",
            "tap",
            "--version",
            "1.2.3",
            "--formula-file",
            "tierstore.rb",
            "--repo",
            "komira-ai/tierstore",
            "--tap-repo",
            "komira-ai/homebrew-tap",
            "--gh",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("holds 0 sha256 lines, not 3"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn upload_stops_at_the_first_aws_call_that_fails() {
    // Catches: the upload verb's `aws` program flag not reaching the calls, and an upload that
    // goes on to a later file after a put fails.
    let d = fresh("upload-fails");
    let dist = d.join("dist");
    std::fs::create_dir(&dist).unwrap();
    let mut lines = String::new();
    for name in [
        "tierstore-1.2.3-linux-aarch64.tar.gz",
        "tierstore-1.2.3-linux-x86_64.tar.gz",
        "tierstore-1.2.3-macos-arm64.tar.gz",
        "tierstore_1.2.3_amd64.deb",
        "tierstore_1.2.3_arm64.deb",
        "tierstore-1.2.3-1.aarch64.rpm",
        "tierstore-1.2.3-1.x86_64.rpm",
    ] {
        let bytes = format!("bytes of {name}");
        std::fs::write(dist.join(name), &bytes).unwrap();
        std::fs::write(dist.join(format!("{name}.sigstore.json")), "bundle").unwrap();
        lines.push_str(&format!("{}  {name}\n", sha256(bytes.as_bytes())));
    }
    std::fs::write(dist.join("SHA256SUMS"), lines).unwrap();
    let o = run(
        &d,
        &[
            "channels",
            "upload",
            "dist",
            "--tag",
            "v1.2.3",
            "--commit",
            "0123456789abcdef0123456789abcdef01234567",
            "--bucket",
            "b",
            "--work",
            "work",
            "--aws",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("put releases/1.2.3/tierstore-1.2.3-1.aarch64.rpm"),
        "{}",
        stderr(&o)
    );
    for later in [
        "tierstore-1.2.3-1.aarch64.rpm.sigstore.json",
        "tierstore-1.2.3-1.x86_64.rpm",
        "tierstore-1.2.3-linux-aarch64.tar.gz",
        "SHA256SUMS",
        "latest.json",
    ] {
        assert!(!stderr(&o).contains(later), "{later}: {}", stderr(&o));
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

#[test]
fn the_apt_verbs_are_dispatched_and_report_the_first_thing_that_fails() {
    // Catches: a verb of the apt channel that is not wired to its implementation, a failure that
    // does not reach the exit status, and a signing failure that leaves the private key behind.
    // `aws` and `gpg` are the stand-in program, which answers neither.
    let d = fresh("apt");
    let o = run(
        &d,
        &[
            "channels",
            "apt",
            "read",
            "--bucket",
            "apt-bucket",
            "--out",
            "current",
            "--aws",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("get-object apt/dists/stable/Release failed"),
        "{}",
        stderr(&o)
    );

    let o = run(
        &d,
        &[
            "channels",
            "apt",
            "index",
            "--current",
            "current",
            "--out",
            "next",
            "--debs",
            "dist",
            "--version",
            "1.2.3",
            "--origin",
            "o",
            "--label",
            "l",
            "--description",
            "d",
            "--date",
            "5",
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("etags.txt is missing: run `apt read` first"),
        "{}",
        stderr(&o)
    );

    std::fs::create_dir_all(d.join("stable")).unwrap();
    std::fs::write(d.join("stable/Release"), "Suite: stable\n").unwrap();
    std::fs::write(d.join("key.asc"), "key").unwrap();
    std::fs::write(d.join("pass"), "pass").unwrap();
    let sign = |fingerprint: &str| {
        run(
            &d,
            &[
                "channels",
                "apt",
                "sign",
                "--dir",
                "stable",
                "--work",
                "work",
                "--fingerprint",
                fingerprint,
                "--key-file",
                "key.asc",
                "--passphrase-file",
                "pass",
                "--gpg",
                FAKE,
                "--output",
                "out",
            ],
        )
    };
    let o = sign("0123456789ABCDEF0123456789ABCDEF01234567");
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("gpg could not import the key"),
        "{}",
        stderr(&o)
    );
    assert!(!d.join("key.asc").exists() && !d.join("pass").exists() && !d.join("out").exists());
    assert_eq!(sign("nonsense").status.code(), Some(2));

    let o = run(
        &d,
        &[
            "channels",
            "apt",
            "write",
            "--bucket",
            "apt-bucket",
            "--distribution-id",
            "E1",
            "--hostname",
            "packages.example",
            "--next",
            "next",
            "--debs",
            "dist",
            "--version",
            "1.2.3",
            "--last",
            "--aws",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("tierstore_1.2.3_amd64.deb is not in"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn a_gate_that_requires_an_earlier_job_is_off_and_green_unless_that_job_said_true() {
    // Catches: a job that runs, or fails, after the job it depends on was off.
    let d = fresh("gate-require");
    let gate = |require: &str| {
        run(
            &d,
            &[
                "channels",
                "gate",
                "--label",
                "apt index",
                "--output",
                "out",
                "--setting",
                "A=",
                "--require",
                require,
            ],
        )
    };
    let o = gate("READ_ENABLED=false");
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(
        stdout(&o).contains("READ_ENABLED is not true"),
        "{}",
        stdout(&o)
    );
    assert_eq!(
        std::fs::read_to_string(d.join("out")).unwrap(),
        "enabled=false\n"
    );
    assert_eq!(gate("READ_ENABLED").status.code(), Some(2));
}

/// The answer of `releases converge` for a farm that is healthy on `1.2.3+0123456789ab`, or not.
fn convergence(state: &str) -> String {
    format!(
        "{{\"convergence\":{{\"desired\":\"1.2.3+0123456789ab\",\"state\":\"CONVERGENCE_STATE_{state}\",\"roles\":[{{\"role\":\"hot\",\"atDesired\":2}}]}}}}"
    )
}

/// `channels soak` with a clock that starts at 1000 and a reading program that prints `reply`.
fn soak(d: &Path, hours: &str, reply: &str) -> Output {
    std::fs::write(d.join("reply"), reply).unwrap();
    run(
        d,
        &[
            "channels",
            "soak",
            "--hours",
            hours,
            "--reached-at",
            "1000",
            "--now",
            "1000",
            "--interval-secs",
            "1800",
            "--version",
            "1.2.3+0123456789ab",
            "--read-command",
            FAKE,
            "--read-arg",
            "api",
            "--read-arg",
            "reply",
        ],
    )
}

#[test]
fn soak_waits_out_the_hours_on_its_own_clock_and_ends_on_the_first_unhealthy_read() {
    // Catches: a soak verb that is not wired to the command line (its flags, its clock, its
    // reading program and its arguments), that waits in real time under `--now`, or whose status
    // does not tell a farm that stayed healthy (0) from one that did not (1).
    let d = fresh("soak");
    let o = soak(&d, "1", &convergence("CONVERGED"));
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        "soak: the farm stayed healthy on 1.2.3+0123456789ab for 1 hours (3 reads)\n"
    );

    let o = soak(&d, "1", &convergence("HALTED"));
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("read 1 shows the farm not healthy"),
        "{}",
        stderr(&o)
    );

    let o = soak(&d, "0", "never read");
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).starts_with("soak: 0 hours"), "{}", stdout(&o));
}

#[test]
fn soak_refuses_more_than_five_hours_and_a_soak_with_nothing_to_read_with() {
    // Catches: the limit of a hosted job and the missing reader being a failure (1) instead of a
    // usage error (2), and a soak that passes without ever reading the farm.
    let d = fresh("soak-usage");
    let o = soak(&d, "6", &convergence("CONVERGED"));
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("is more than 5"), "{}", stderr(&o));

    let o = run(
        &d,
        &[
            "channels",
            "soak",
            "--hours",
            "1",
            "--reached-at",
            "1000",
            "--now",
            "1000",
            "--version",
            "1.2.3+0123456789ab",
        ],
    );
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("needs --read-command"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn soak_on_the_machine_clock_with_zero_hours_ends_at_once() {
    // Catches: the machine's clock path of the verb (no `--now`) not being wired, which is the
    // one the workflow runs.
    let d = fresh("soak-machine");
    let o = run(
        &d,
        &[
            "channels",
            "soak",
            "--hours",
            "0",
            "--reached-at",
            "1000",
            "--version",
            "1.2.3+0123456789ab",
        ],
    );
    assert!(o.status.success(), "{}", stderr(&o));
    assert!(stdout(&o).starts_with("soak: 0 hours"), "{}", stdout(&o));
}

#[test]
fn the_apt_suite_and_promote_and_no_pool_are_dispatched_and_their_flags_are_held_together() {
    // Catches: `--suite` ignored (testing read from the stable path), `promote` not wired,
    // `--no-pool` still asking for packages, and `--no-pool` accepted beside `--debs`.
    let d = fresh("apt-suite");
    let o = run(
        &d,
        &[
            "channels",
            "apt",
            "read",
            "--suite",
            "testing",
            "--bucket",
            "apt-bucket",
            "--out",
            "current",
            "--aws",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("get-object apt/dists/testing/Release failed"),
        "{}",
        stderr(&o)
    );
    assert_eq!(
        run(
            &d,
            &[
                "channels", "apt", "read", "--suite", "beta", "--bucket", "b", "--out", "o"
            ]
        )
        .status
        .code(),
        Some(2)
    );

    let o = run(
        &d,
        &[
            "channels",
            "apt",
            "promote",
            "--testing",
            "t",
            "--stable",
            "s",
            "--out",
            "o",
            "--version",
            "1.2.3",
            "--sums",
            "SHA256SUMS",
            "--origin",
            "o",
            "--label",
            "l",
            "--description",
            "d",
            "--date",
            "5",
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("etags.txt is missing: run `apt read --suite stable` first"),
        "{}",
        stderr(&o)
    );

    let write = |extra: &[&str]| {
        let mut args = vec![
            "channels",
            "apt",
            "write",
            "--suite",
            "stable",
            "--bucket",
            "apt-bucket",
            "--distribution-id",
            "E1",
            "--hostname",
            "packages.example",
            "--next",
            "next",
            "--last",
            "--aws",
            FAKE,
        ];
        args.extend(extra);
        run(&d, &args)
    };
    let o = write(&["--no-pool"]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("Release"), "{}", stderr(&o));
    assert!(!stderr(&o).contains("is not in"), "{}", stderr(&o));
    assert_eq!(
        write(&["--no-pool", "--debs", "dist"]).status.code(),
        Some(2)
    );
}

/// `channels assume` as a process with the token offer of the runner set (or not) in its
/// environment, and an `aws` that is never reached.
fn assume(d: &Path, role: &str, offer: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
    command
        .current_dir(d)
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_URL")
        .env_remove("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        .envs(offer.iter().copied())
        .args([
            "channels",
            "assume",
            "--role-arn",
            role,
            "--session-name",
            "tierstore-release-1",
            "--region",
            "us-east-1",
            "--env-file",
            "github-env",
            "--aws",
            "aws-that-is-not-there",
        ]);
    command.output().expect("run xtask")
}

#[test]
fn assume_refuses_an_empty_role_and_a_job_without_an_oidc_offer_and_writes_nothing() {
    // Catches: an assume verb that goes on without a role (status 2, a usage error) or without
    // the token the runner offers (status 1), or that writes the env file before it has
    // credentials.
    let d = fresh("assume");
    let o = assume(&d, "", &[]);
    assert_eq!(o.status.code(), Some(2), "{}", stderr(&o));
    assert!(stderr(&o).contains("--role-arn is empty"), "{}", stderr(&o));

    let o = assume(&d, "arn:aws:iam::1:role/r", &[]);
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("the runner offers no OIDC token"),
        "{}",
        stderr(&o)
    );
    let o = assume(
        &d,
        "arn:aws:iam::1:role/r",
        &[("ACTIONS_ID_TOKEN_REQUEST_URL", "https://127.0.0.1:9/t")],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert!(
        stderr(&o).contains("the runner offers no OIDC token"),
        "{}",
        stderr(&o)
    );
    assert!(!d.join("github-env").exists());
}

#[test]
fn assume_masks_the_role_and_the_bearer_before_it_asks_for_the_token() {
    // Catches: the real offer not being read from the environment by its names, and a mask that
    // comes after the first request. The address refuses the connection, so the token request
    // fails and no `aws` is run.
    let d = fresh("assume-offer");
    let o = assume(
        &d,
        "arn:aws:iam::1:role/r",
        &[
            ("ACTIONS_ID_TOKEN_REQUEST_URL", "http://127.0.0.1:9/t"),
            ("ACTIONS_ID_TOKEN_REQUEST_TOKEN", "bearer-x"),
        ],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stderr(&o));
    assert_eq!(
        stdout(&o),
        "::add-mask::arn:aws:iam::1:role/r\n::add-mask::bearer-x\n"
    );
    assert!(
        stderr(&o).contains("the OIDC token request failed: "),
        "{}",
        stderr(&o)
    );
    assert!(!stderr(&o).contains("bearer-x"), "{}", stderr(&o));
    assert!(!d.join("github-env").exists());
}

#[test]
fn decide_latest_and_the_install_checks_are_dispatched_and_report_what_fails() {
    // Catches: a verb not wired to its implementation, a refusal that does not reach the exit
    // status, and a promotion decided without its answer in GITHUB_OUTPUT. The programs the
    // checks run are the stand-in program, which answers none of them.
    let d = fresh("decide-checks");
    let decide = |check_apt: &str, farm: &str| {
        let mut args = vec![
            "channels",
            "decide",
            "--farm-enabled",
            farm,
            "--output",
            "out",
        ];
        let results: Vec<String> = [
            "testing-upload=success",
            "testing-apt-write=success",
            "testing-apt-write-2=skipped",
            "testing-apt-write-3=skipped",
            "testing-rpm-write=success",
            "testing-rpm-write-2=skipped",
            "testing-rpm-write-3=skipped",
            "check-rpm=success",
            "check-brew=success",
            "check-install-sh=success",
            "farm-check=success",
            "farm=success",
            "soak=success",
        ]
        .iter()
        .map(|r| (*r).to_owned())
        .chain([format!("check-apt={check_apt}")])
        .collect();
        for r in &results {
            args.extend(["--result", r]);
        }
        run(&d, &args)
    };
    let o = decide("success", "true");
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(
        std::fs::read_to_string(d.join("out")).unwrap(),
        "promote=true\n"
    );
    let o = decide("failure", "true");
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("check-apt is failure"),
        "{}",
        stderr(&o)
    );
    assert_eq!(decide("success", "maybe").status.code(), Some(2));

    let latest = run(
        &d,
        &[
            "channels", "latest", "dist", "--tag", "1.2.3", "--commit", COMMIT_SHA, "--bucket",
            "b", "--work", "work", "--aws", FAKE,
        ],
    );
    assert_eq!(latest.status.code(), Some(2), "{}", stderr(&latest));
    let upload = run(
        &d,
        &[
            "channels",
            "upload",
            "dist",
            "--no-latest",
            "--tag",
            "1.2.3",
            "--commit",
            COMMIT_SHA,
            "--bucket",
            "b",
            "--work",
            "work",
            "--aws",
            FAKE,
        ],
    );
    assert_eq!(upload.status.code(), Some(2), "{}", stderr(&upload));

    let o = run(
        &d,
        &[
            "channels",
            "check-apt",
            "--arch",
            "amd64",
            "--version",
            "1.2.3",
            "--commit",
            COMMIT_SHA,
            "--root",
            "root",
            "--dpkg",
            FAKE,
            "--apt-get",
            FAKE,
            "--dpkg-query",
            FAKE,
            "--curl",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("--print-architecture failed"),
        "{}",
        stderr(&o)
    );

    std::fs::write(d.join("tierstore.rb"), "formula").unwrap();
    let o = run(
        &d,
        &[
            "channels",
            "check-brew",
            "--formula",
            "tierstore.rb",
            "--version",
            "1.2.3",
            "--commit",
            COMMIT_SHA,
            "--brew",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("tap-new --no-git local/tierstore failed"),
        "{}",
        stderr(&o)
    );

    let o = run(
        &d,
        &[
            "channels",
            "check-install-sh",
            "--version",
            "1.2.3",
            "--commit",
            COMMIT_SHA,
            "--work",
            "scratch",
            "--curl",
            FAKE,
            "--sh",
            FAKE,
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("failed"), "{}", stderr(&o));
}

const COMMIT_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
