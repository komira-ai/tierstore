//! The last step of hostkeeper's CI client (`set-desired.yml` of komira-ai/hostkeeper, the revision
//! `.github/workflows/hostkeeper-desired.yml` pins), run as the runner runs it, on the answers the
//! coordinator gives: whether the job `set-desired` of `hostkeeper-desired.yml` ends red or green.
//!
//! `set_desired_client/set-desired.yml` is that file, byte for byte, from komira-ai/hostkeeper at
//! [`CLIENT_COMMIT`] (Apache-2.0; its git blob is `0b11047ad3e9dd92c0ec928da4956192c1e61a34`, and
//! [`CLIENT_SHA256`] is the SHA-256 of its bytes). The step `Write the job's outputs` is the one
//! that decides: its `run:` text is run with `bash --noprofile --norc -eo pipefail -c` (GitHub's
//! `bash` shell on `ubuntu-24.04`) and the host's `jq`, with the step's environment as the client
//! sets it: its `OUTPUTS_PROGRAM`, the outcomes of the set and of the wait, the files holding the
//! coordinator's answers (`/dev/null` for a step that did not succeed, as the step's own
//! expressions give), and `WAIT` the `wait` input `hostkeeper-desired.yml` passes, read from that
//! file. Each case is in a scratch directory of its own.
//!
//! When no host can be ordered (every host `installer_dropped` or `no_build_for_host`, so
//! `hosts.picked` is 0) the coordinator never reports the version converged, and the wait ends at
//! its deadline (`WAIT_OUTCOME_DEADLINE`).
//!
//! Catches: `hostkeeper-desired.yml` passing a `wait` with which the client ends green
//! (`not_waited`) whatever the coordinator answers, so that a version no host can be ordered
//! passes; a pin moved to a client whose outputs step does not fail on a wait that did not
//! converge (the pin and the copy must move together); and a set that converged ending red.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::fs;
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use tierstore_testkit::dir::TestDir;

const WORKFLOW: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../.github/workflows/hostkeeper-desired.yml"
);
const CLIENT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/set_desired_client/set-desired.yml"
);
/// The commit of komira-ai/hostkeeper the copy was taken at, the one `hostkeeper-desired.yml` pins.
const CLIENT_COMMIT: &str = "30141591c292d8881ce5bc16c9d0b3ba7fcfdca1";
/// The SHA-256 of the copy's bytes.
const CLIENT_SHA256: &str = "331a40823f75e2ff2f2b9c5689e3caa42a11125e7aa1f79c50a61229e5d71540";
/// The version set in every case.
const VERSION: &str = "0.4.0";

/// The text of the block scalar `key: |` (the first one) of `text`, its indentation removed.
fn block(text: &str, key: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.trim_start() == format!("{key}: |"))
        .unwrap_or_else(|| panic!("no block {key}"));
    let outer = lines[at].len() - lines[at].trim_start().len();
    let body: Vec<&str> = lines[at + 1..]
        .iter()
        .take_while(|l| l.trim().is_empty() || l.len() - l.trim_start().len() > outer)
        .copied()
        .collect();
    let inner = body
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap();
    let mut out: String = body
        .iter()
        .map(|l| format!("{}\n", l.get(inner..).unwrap_or("")))
        .collect();
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// The `wait` input `hostkeeper-desired.yml` passes the client.
fn wait_input() -> String {
    let text = fs::read_to_string(WORKFLOW).unwrap();
    let values: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("      wait: "))
        .collect();
    assert_eq!(values.len(), 1, "one `wait` input: {values:?}");
    values[0].trim().to_owned()
}

/// What the coordinator answered, as the client's steps leave it.
struct Answers {
    /// The outcome of the step `set-desired`, and its answer.
    set: (&'static str, &'static str),
    /// The outcome of the step `wait`, and its answer.
    wait: (&'static str, &'static str),
}

/// The client's outputs step run on `answers` with the input `wait`: its output and the lines it
/// appended to `GITHUB_OUTPUT`.
fn outputs(name: &str, wait: &str, answers: &Answers) -> (Output, String) {
    let client = fs::read_to_string(CLIENT).unwrap();
    let program = block(&client, "OUTPUTS_PROGRAM");
    // The step's run text: the last `run: |` of the file, the step after OUTPUTS_PROGRAM.
    let after = &client[client.find("OUTPUTS_PROGRAM: |").unwrap()..];
    let script = block(after, "run");
    assert!(
        script.starts_with("jq --null-input --raw-output"),
        "{script}"
    );
    let d = TestDir::under(env!("CARGO_TARGET_TMPDIR"), name);
    let write = |file: &str, outcome: &str, body: &str| {
        if outcome == "success" {
            let path = d.join(file);
            fs::write(&path, body).unwrap();
            path.display().to_string()
        } else {
            "/dev/null".to_owned()
        }
    };
    let github_output = d.join("github-output");
    fs::write(&github_output, "").unwrap();
    let out = Command::new("/bin/bash")
        .args(["--noprofile", "--norc", "-eo", "pipefail", "-c", &script])
        .env_clear()
        // jq where the runners have it: /usr/bin on ubuntu-24.04 and macOS 15, Homebrew's too.
        .env("PATH", "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin")
        .env("GITHUB_OUTPUT", &github_output)
        .env("OUTPUTS_PROGRAM", &program)
        .env("SERVICE", "tierstore")
        .env("VERSION", VERSION)
        .env("WAIT", wait)
        .env("SET_OUTCOME", answers.set.0)
        .env("WAIT_OUTCOME", answers.wait.0)
        .env(
            "SET_BODY",
            write("hkctl-set.json", answers.set.0, answers.set.1),
        )
        .env(
            "SET_ERR",
            if answers.set.0 == "failure" {
                write("hkctl-set.err", "success", answers.set.1)
            } else {
                "/dev/null".to_owned()
            },
        )
        .env(
            "WAIT_BODY",
            write("hkctl-wait.json", answers.wait.0, answers.wait.1),
        )
        .env(
            "WAIT_ERR",
            if answers.wait.0 == "failure" {
                write("hkctl-wait.err", "success", answers.wait.1)
            } else {
                "/dev/null".to_owned()
            },
        )
        .output()
        .expect("bash runs");
    (out, fs::read_to_string(&github_output).unwrap())
}

/// The set stored the version.
const STORED: (&str, &str) = ("success", r#"{"desired":{"version":"0.4.0"}}"#);

/// The wait's answer when every host is held `installer_dropped`: none picked, never converged.
const NONE_PICKED_DROPPED: &str = r#"{"outcome":"WAIT_OUTCOME_DEADLINE","convergence":{"desired":"0.4.0","state":"CONVERGENCE_STATE_IN_PROGRESS","hosts":{"picked":0,"installerDropped":3},"installerDropped":["n1","n2","n3"]}}"#;

/// The wait's answer when every host is held `no_build_for_host`.
const NONE_PICKED_NO_BUILD: &str = r#"{"outcome":"WAIT_OUTCOME_DEADLINE","convergence":{"desired":"0.4.0","state":"CONVERGENCE_STATE_IN_PROGRESS","hosts":{"picked":0,"noBuildForHost":3}}}"#;

/// The wait's answer when the fleet converged on the version.
const CONVERGED: &str = r#"{"outcome":"WAIT_OUTCOME_CONVERGED","convergence":{"desired":"0.4.0","state":"CONVERGENCE_STATE_CONVERGED","hosts":{"picked":3}}}"#;

#[test]
fn the_copy_is_the_client_hostkeeper_desired_yml_pins() {
    let bytes = fs::read(CLIENT).unwrap();
    let digest: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(digest, CLIENT_SHA256, "the copy was edited");
    let workflow = fs::read_to_string(WORKFLOW).unwrap();
    assert!(
        workflow.contains(&format!(
            "uses: komira-ai/hostkeeper/.github/workflows/set-desired.yml@{CLIENT_COMMIT} "
        )),
        "hostkeeper-desired.yml pins another client than the copy: replace the copy with that revision's file"
    );
}

#[test]
fn a_version_no_host_can_be_ordered_ends_the_job_red() {
    let wait = wait_input();
    for (name, answer) in [
        ("installer-dropped", NONE_PICKED_DROPPED),
        ("no-build-for-host", NONE_PICKED_NO_BUILD),
    ] {
        let (out, lines) = outputs(
            name,
            &wait,
            &Answers {
                set: STORED,
                wait: ("success", answer),
            },
        );
        assert_eq!(out.status.code(), Some(1), "{name}: {out:?}");
        assert_eq!(
            lines, "desired_version=0.4.0\nconverged=false\nreason=not_converged\n",
            "{name}"
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "::error::the version 0.4.0 of tierstore is set; the wait ended with not_converged\n",
            "{name}"
        );
    }
}

#[test]
fn a_version_the_fleet_converged_on_ends_the_job_green() {
    let (out, lines) = outputs(
        "converged",
        &wait_input(),
        &Answers {
            set: STORED,
            wait: ("success", CONVERGED),
        },
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        lines,
        "desired_version=0.4.0\nconverged=true\nreason=converged\n"
    );
}

#[test]
fn a_halt_a_newer_version_or_a_cut_wait_ends_the_job_red() {
    for (name, wait, reason) in [
        (
            "halted",
            ("success", r#"{"outcome":"WAIT_OUTCOME_HALTED"}"#),
            "halted",
        ),
        (
            "superseded",
            ("success", r#"{"outcome":"WAIT_OUTCOME_SUPERSEDED"}"#),
            "superseded",
        ),
        ("cut", ("failure", "hkctl: no answer\n"), "no_answer"),
    ] {
        let (out, lines) = outputs(name, &wait_input(), &Answers { set: STORED, wait });
        assert_eq!(out.status.code(), Some(1), "{name}: {out:?}");
        assert!(
            lines.ends_with(&format!("converged=false\nreason={reason}\n")),
            "{name}: {lines}"
        );
    }
}

#[test]
fn without_the_wait_the_same_answer_would_end_the_job_green() {
    // The setting before: the client does not wait (its wait step is skipped), and the job is
    // green whatever the hosts do. This is what the `wait` input of hostkeeper-desired.yml must
    // not be.
    let (out, lines) = outputs(
        "not-waited",
        "false",
        &Answers {
            set: STORED,
            wait: ("skipped", ""),
        },
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        lines,
        "desired_version=0.4.0\nconverged=false\nreason=not_waited\n"
    );
    assert_ne!(wait_input(), "false");
}

#[test]
fn a_set_the_coordinator_refused_ends_the_job_red_with_its_token() {
    let (out, lines) = outputs(
        "refused",
        &wait_input(),
        &Answers {
            set: (
                "failure",
                "hkctl: the coordinator answered FAILED_PRECONDITION\nrefused: release_exists\n",
            ),
            wait: ("skipped", ""),
        },
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert_eq!(
        lines,
        "desired_version=\nconverged=false\nreason=release_exists\n"
    );
}
