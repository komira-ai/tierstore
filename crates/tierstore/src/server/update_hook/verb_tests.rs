//! The verb: from the command line to the last line.

use std::ffi::OsString;

use tierstore_testkit::dir::TestDir;

use super::{Done, Invocation, Machine, Reason, Step, run};
use crate::server::cli;

fn args(words: &[&str]) -> Vec<OsString> {
    ["tierstore", "update-hook"]
        .iter()
        .chain(words)
        .map(OsString::from)
        .collect()
}

struct Ran {
    status: String,
    line: serde_json::Value,
    err: String,
}

fn ran(words: &[&str], machine: &Machine<'_>) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = run(&args(words), &mut out, &mut err, machine);
    Ran {
        status: format!("{status:?}"),
        line: serde_json::from_slice(&out).unwrap(),
        err: String::from_utf8(err).unwrap(),
    }
}

fn status(code: u8) -> String {
    format!("{:?}", std::process::ExitCode::from(code))
}

const PARAMS: &str = "promotion_bound_secs = 30\n\
\"updater.staged_release\" = \"/releases/new\"\n\
\"updater.target_version\" = \"0.4.0\"\n";

/// Asserts that a step is handed what the command line and the params file say, and that its
/// report is the last line and its outcome the status. It would catch a verb that runs a step
/// with the wrong params or loses the report.
#[test]
fn the_step_is_run_with_the_params_and_its_report_is_the_line() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-hook-verb");
    let file = dir.path().join("params.toml");
    std::fs::write(&file, PARAMS).unwrap();
    let file = file.to_str().unwrap().to_owned();
    let seen = std::cell::RefCell::new(Vec::new());
    let step = |inv: &Invocation, params: Option<&super::Params>| {
        seen.borrow_mut()
            .push((inv.step, params.map(|p| p.target_version.clone())));
        Done::deferred(Reason::HostNotReady, "later", 7)
    };
    let machine = Machine { step: &step };
    let got = ran(&["prepare", "/releases/new", "--params", &file], &machine);
    assert_eq!(got.status, status(75));
    assert_eq!(got.line["reason"], "host_not_ready");
    assert_eq!(got.line["retry_after_secs"], 7);
    let restored = ran(&["restore"], &machine);
    assert_eq!(restored.status, status(75));
    assert_eq!(
        *seen.borrow(),
        [
            (Step::Prepare, Some("0.4.0".to_owned())),
            (Step::Restore, None)
        ]
    );
}

/// Asserts that a refusal runs no step, writes the words to the error stream and gives a
/// failure line with the class `environment`. It would catch a bad command line that still
/// runs a step.
#[test]
fn a_refusal_runs_no_step() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-hook-verb-refused");
    let file = dir.path().join("params.toml");
    std::fs::write(&file, PARAMS).unwrap();
    let file = file.to_str().unwrap().to_owned();
    let step = |_: &Invocation, _: Option<&super::Params>| -> Done { panic!("a step ran") };
    let machine = Machine { step: &step };
    let bad = ran(&["bounce"], &machine);
    assert_eq!(bad.status, status(1));
    assert_eq!(bad.line["reason"], "bad_args");
    assert_eq!(bad.line["class"], "environment");
    assert!(bad.err.contains("unknown step"));
    let missing = ran(
        &["verify", "--params", "/nonexistent/params.toml"],
        &machine,
    );
    assert_eq!(missing.line["reason"], "bad_params");
    assert!(missing.err.contains("cannot read the params file"));
    let other = ran(&["prepare", "/releases/other", "--params", &file], &machine);
    assert_eq!(other.line["reason"], "bad_args");
    assert!(other.err.contains("is not the staged release"));
}

/// Asserts that the machine this build runs on answers every step `not_implemented` for now, and
/// that the binary's entry reaches the verb before its own flags. It would catch a verb that
/// falls through to the server's flags, which would start a server or print its usage.
#[test]
fn the_binary_answers_the_verb_before_its_flags() {
    let got = ran(&["restore"], &Machine::system());
    assert_eq!(got.status, status(1));
    assert_eq!(got.line["reason"], "not_implemented");
    assert!(got.line["detail"].as_str().unwrap().contains("restore"));
    let through_main = cli::main(args(&["bounce"]));
    assert_eq!(format!("{through_main:?}"), status(1));
}
