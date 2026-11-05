//! The verb: from the command line to the last line.

use std::cell::RefCell;
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use tierstore_testkit::dir::TestDir;

use super::{
    ArgsError, Done, Invocation, Machine, Params, ParamsError, Reason, Refusal, Step, main, run,
    step_of,
};
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
    out: String,
    err: String,
}

fn ran(words: &[&str], machine: &Machine<'_>) -> Ran {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let status = run(&args(words), &mut out, &mut err, machine);
    let out = String::from_utf8(out).unwrap();
    Ran {
        status: format!("{status:?}"),
        line: serde_json::from_str(out.lines().last().unwrap()).unwrap(),
        out,
        err: String::from_utf8(err).unwrap(),
    }
}

fn status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

const PARAMS: &str = "\"updater.staged_release\" = \"/releases/new\"\n\
\"updater.target_version\" = \"0.5.0\"\n";

/// A scratch directory with a params file of `text`, and the file's path as a word.
fn params_file(name: &str, text: &str) -> (TestDir, String) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let file = dir.path().join("params.toml");
    std::fs::write(&file, text).unwrap();
    let word = file.to_str().unwrap().to_owned();
    (dir, word)
}

/// Asserts that a step is handed what the command line and the params file say, and that its
/// report is the last line, one object and a newline, and its outcome the status: 0 for a
/// success, 1 for a failure, 75 for a deferral. It would catch a verb that runs a step with the
/// wrong params, loses the report, or exits with a status hostkeeper reads as another outcome.
#[test]
fn the_step_is_run_with_the_params_and_its_report_is_the_line() {
    let (_dir, file) = params_file("tierstore-hook-verb", PARAMS);
    let seen = RefCell::new(Vec::new());
    let step = |inv: &Invocation, params: &Params| {
        seen.borrow_mut().push((
            inv.step,
            inv.artifact.clone(),
            inv.params.clone(),
            params.target_version.clone(),
        ));
        match inv.step {
            Step::Cutover => Done::ok("served"),
            Step::Verify => Done::fail(Reason::WrongBuild, "old"),
            _ => Done::deferred(Reason::HostNotReady, "later", 7),
        }
    };
    let machine = Machine { step: &step };

    let ok = ran(&["cutover", "--params", &file], &machine);
    assert_eq!(ok.status, status(0));
    assert_eq!(ok.line["detail"], "served");
    assert!(ok.line.get("reason").is_none());
    assert_eq!(ok.err, "");
    let failed = ran(&["verify", "--params", &file], &machine);
    assert_eq!(failed.status, status(1));
    assert_eq!(failed.line["reason"], "wrong_build");
    assert_eq!(failed.line["class"], "release");
    let deferred = ran(&["prepare", "/releases/new", "--params", &file], &machine);
    assert_eq!(deferred.status, status(75));
    assert_eq!(deferred.line["reason"], "host_not_ready");
    assert_eq!(deferred.line["retry_after_secs"], 7);
    assert!(deferred.out.ends_with("}\n"));
    assert_eq!(deferred.out.lines().count(), 1);
    let restore = ran(&["restore", "--params", &file], &machine);
    assert_eq!(restore.status, status(75));

    let target = |step, artifact: Option<&str>| {
        (
            step,
            artifact.map(PathBuf::from),
            PathBuf::from(&file),
            "0.5.0".to_owned(),
        )
    };
    assert_eq!(
        *seen.borrow(),
        [
            target(Step::Cutover, None),
            target(Step::Verify, None),
            target(Step::Prepare, Some("/releases/new")),
            target(Step::Restore, None),
        ]
    );
}

/// Asserts that a command line that is refused runs no step, writes the words to the error stream
/// and gives a `bad_args` failure of the class `environment` as the last line. It would catch a
/// bad command line that still runs a step.
#[test]
fn a_bad_command_line_runs_no_step() {
    let (_dir, file) = params_file("tierstore-hook-verb-args", PARAMS);
    let step = |_: &Invocation, _: &Params| -> Done { panic!("a step ran") };
    let machine = Machine { step: &step };
    for (words, words_err) in [
        (vec!["bounce"], "unknown step"),
        (vec![], "no step given"),
        (vec!["verify"], "--params <file> is required"),
        (
            vec!["prepare", "--params", file.as_str()],
            "needs the staged release",
        ),
    ] {
        let got = ran(&words, &machine);
        assert_eq!(got.status, status(1), "{words:?}");
        assert_eq!(got.line["reason"], "bad_args");
        assert_eq!(got.line["class"], "environment");
        assert!(
            got.err.starts_with("tierstore update-hook: "),
            "{}",
            got.err
        );
        assert!(got.err.contains(words_err), "{}", got.err);
        assert!(got.err.ends_with('\n'));
    }
}

/// Asserts that a params file that cannot be read, or is not a table of scalars with the required
/// keys, runs no step and is a `bad_params` failure with the reason in the words. It would catch a
/// step that runs on params it could not read.
#[test]
fn a_bad_params_file_runs_no_step() {
    let step = |_: &Invocation, _: &Params| -> Done { panic!("a step ran") };
    let machine = Machine { step: &step };
    let missing = ran(
        &["verify", "--params", "/nonexistent/params.toml"],
        &machine,
    );
    assert_eq!(missing.status, status(1));
    assert_eq!(missing.line["reason"], "bad_params");
    assert_eq!(missing.line["class"], "environment");
    assert!(missing.err.contains("cannot read the params file"));
    for (text, why) in [
        ("not = = toml", "not a TOML table"),
        (
            "\"updater.target_version\" = \"0.5.0\"\n",
            "\"updater.staged_release\" is required",
        ),
        (
            "\"updater.staged_release\" = \"relative\"\n\"updater.target_version\" = \"1\"\n",
            "plain components",
        ),
    ] {
        let (_dir, file) = params_file("tierstore-hook-verb-params", text);
        let got = ran(&["cutover", "--params", &file], &machine);
        assert_eq!(got.status, status(1));
        assert_eq!(got.line["reason"], "bad_params");
        assert!(got.err.contains(why), "{}", got.err);
    }
}

/// Asserts that the staged release `prepare` is given on the command line must be the one the
/// params name: another directory runs no step and is a `bad_args` failure naming both, and the
/// same directory runs the step. It would catch a prepare of one tree under the params of another.
#[test]
fn the_artifact_must_be_the_staged_release_of_the_params() {
    let (_dir, file) = params_file("tierstore-hook-verb-artifact", PARAMS);
    let step = |_: &Invocation, _: &Params| -> Done { panic!("a step ran") };
    let other = ran(
        &["prepare", "/releases/other", "--params", &file],
        &Machine { step: &step },
    );
    assert_eq!(other.status, status(1));
    assert_eq!(other.line["reason"], "bad_args");
    assert!(
        other
            .err
            .contains("\"/releases/other\" is not the staged release")
    );
    assert!(other.err.contains("(\"/releases/new\")"));

    let step = |_: &Invocation, _: &Params| Done::ok("same tree");
    let same = ran(
        &["prepare", "/releases/new", "--params", &file],
        &Machine { step: &step },
    );
    assert_eq!(same.status, status(0));
    assert_eq!(same.line["detail"], "same tree");
}

/// Asserts what each refusal is told as: a command line or an artifact is `bad_args`, the params
/// are `bad_params`, both of the class `environment`, and the words are the error's.
#[test]
fn a_refusal_names_its_reason() {
    let step = |_: &Invocation, _: &Params| -> Done { panic!("a step ran") };
    let machine = Machine { step: &step };
    let args_refusal = step_of(&args(&["bounce"]), &machine).unwrap_err();
    assert!(matches!(
        args_refusal,
        Refusal::Args(ArgsError::UnknownStep(_))
    ));
    assert_eq!(args_refusal.done().report.reason, Some(Reason::BadArgs));
    let params_refusal = Refusal::Params(ParamsError::Missing("x"));
    let done = params_refusal.done();
    assert_eq!(done.report.reason, Some(Reason::BadParams));
    assert_eq!(
        done.report.detail.as_deref(),
        Some("params key \"x\" is required")
    );
    let artifact = Refusal::Artifact {
        given: "/a".into(),
        staged: "/b".into(),
    };
    assert_eq!(artifact.done().report.reason, Some(Reason::BadArgs));
    assert_eq!(
        artifact.to_string(),
        "the artifact \"/a\" is not the staged release updater.staged_release names (\"/b\")"
    );
}

/// Asserts that a standard output that is closed changes nothing about the status: the step ran,
/// and its outcome is the exit status, though the line cannot be written anywhere.
#[test]
fn a_closed_standard_output_does_not_change_the_status() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let (_dir, file) = params_file("tierstore-hook-verb-closed", PARAMS);
    let step = |_: &Invocation, _: &Params| Done::deferred(Reason::Draining, "later", 10);
    let machine = Machine { step: &step };
    let mut err = Vec::new();
    let code = run(
        &args(&["cutover", "--params", &file]),
        &mut Closed,
        &mut err,
        &machine,
    );
    assert_eq!(format!("{code:?}"), status(75));
    assert!(err.is_empty());
}

/// Asserts that the binary's entry reaches the verb before its own flags, and that the verb's own
/// entry does the same: a refused command line is exit 1, not the server's usage or a start. It
/// would catch a verb that falls through to the server's flags.
#[test]
fn the_binary_answers_the_verb_before_its_flags() {
    let through_cli = cli::main(args(&["bounce"]));
    assert_eq!(format!("{through_cli:?}"), status(1));
    let through_main = main(&args(&["bounce"]));
    assert_eq!(format!("{through_main:?}"), status(1));
}
