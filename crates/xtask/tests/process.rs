//! The real programs behind the traits: `RealGh` (the release verbs), and `Process` (`cargo
//! deny` and `gh api` for the advisories verb). The program is the stand-in `gh`
//! (src/bin/gh-fake.rs), which answers from files named after what it is asked, so each
//! reply, 404, failure and spawn error of the wrappers is taken.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use xtask::Error;
use xtask::advisories::{Call, Method, Process, Tools};
use xtask::release::{Gh, RealGh};

const FAKE: &str = env!("CARGO_BIN_EXE_gh-fake");
const MISSING: &str = "/nonexistent/program";

fn dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("process-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn failed(result: Result<impl std::fmt::Debug, Error>) -> String {
    match result {
        Err(Error::Failed(why)) => why,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn spawn_error(result: Result<impl std::fmt::Debug, Error>) -> String {
    match result {
        Err(Error::Spawn { why, .. }) => why,
        other => panic!("expected a spawn error, got {other:?}"),
    }
}

#[test]
fn the_release_gh_returns_json_none_for_a_404_and_an_error_for_the_rest() {
    // Catches: a 404 taken for a failure (an unknown commit is a refusal, not a crash) and a
    // failure or a reply that is not JSON taken for an answer.
    let d = dir("realgh-api");
    let gh = RealGh(PathBuf::from(FAKE));
    let reply = d.join("reply");
    std::fs::write(&reply, "{\"a\":[1]}").expect("reply");
    assert_eq!(
        gh.api(reply.to_str().expect("utf8")).expect("api"),
        Some(json!({ "a": [1] }))
    );
    let missing = d.join("nothing");
    std::fs::write(d.join("nothing.404"), "").expect("404");
    assert_eq!(gh.api(missing.to_str().expect("utf8")).expect("404"), None);
    let broken = d.join("broken");
    std::fs::write(d.join("broken.fail"), "").expect("fail");
    assert!(failed(gh.api(broken.to_str().expect("utf8"))).contains("the call failed"));
    let text = d.join("text");
    std::fs::write(&text, "not json").expect("text");
    assert!(failed(gh.api(text.to_str().expect("utf8"))).contains("not JSON"));
    assert!(!spawn_error(RealGh(PathBuf::from(MISSING)).api("x")).is_empty());
}

#[test]
fn the_release_gh_runs_a_command_and_names_the_first_words_of_a_failed_one() {
    // Catches: a failed `gh release ...` returning its output as success, and an error that does
    // not say which command failed.
    let gh = RealGh(PathBuf::from(FAKE));
    let args = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
    assert_eq!(gh.run(&args(&["print", "hello"])).expect("run"), "hello\n");
    let why = failed(gh.run(&args(&["fail", "it", "broke"])));
    assert_eq!(why, "gh fail it: it broke");
    assert!(!spawn_error(RealGh(PathBuf::from(MISSING)).run(&args(&["x"]))).is_empty());
}

fn call(method: Method, path: &Path, body: Option<Value>) -> Call {
    Call {
        method,
        path: path.to_str().expect("utf8").to_owned(),
        body,
    }
}

#[test]
fn deny_reports_the_exit_code_and_the_stderr_and_a_missing_program_is_an_error() {
    // Catches: the exit code of `cargo deny` lost (1 means advisories, not a crash), and a
    // `cargo` that is not there reported as a clean run.
    let d = dir("deny");
    let mut tools = Process {
        cargo: OsStr::new(FAKE),
        gh: Path::new(FAKE),
        root: &d,
    };
    let clean = tools.deny().expect("deny");
    assert_eq!((clean.code, clean.stderr.as_str()), (Some(0), ""));
    std::fs::write(d.join("deny.code"), "1").expect("code");
    std::fs::write(d.join("deny.stderr"), "said").expect("stderr");
    let found = tools.deny().expect("deny");
    assert_eq!((found.code, found.stderr.as_str()), (Some(1), "said"));
    let mut tools = Process {
        cargo: OsStr::new(MISSING),
        gh: Path::new(FAKE),
        root: &d,
    };
    assert!(!spawn_error(tools.deny().map(|run| run.code)).is_empty());
}

#[test]
fn github_answers_json_an_empty_reply_a_404_for_a_get_and_errors_otherwise() {
    // Catches: each way of the `gh api` wrapper going wrong: a body that never reaches `gh`
    // (it would send an empty request), a 404 on a write taken for "absent", an empty reply
    // that is not `null`, and a reply that is not JSON.
    let d = dir("github");
    let mut tools = Process {
        cargo: OsStr::new(FAKE),
        gh: Path::new(FAKE),
        root: &d,
    };
    let reply = d.join("reply");
    std::fs::write(&reply, "[1,2]").expect("reply");
    assert_eq!(
        tools.github(&call(Method::Get, &reply, None)).expect("get"),
        Some(json!([1, 2]))
    );
    let sent = json!({ "state": "closed" });
    assert_eq!(
        tools
            .github(&call(Method::Patch, &reply, Some(sent.clone())))
            .expect("patch"),
        Some(json!([1, 2]))
    );
    assert_eq!(
        std::fs::read_to_string(d.join("reply.body")).expect("body"),
        sent.to_string()
    );
    assert_eq!(
        tools
            .github(&call(Method::Post, &reply, Some(sent)))
            .expect("post"),
        Some(json!([1, 2]))
    );

    let empty = d.join("empty");
    std::fs::write(&empty, "").expect("empty");
    assert_eq!(
        tools
            .github(&call(Method::Get, &empty, None))
            .expect("empty"),
        Some(Value::Null)
    );

    let gone = d.join("gone");
    std::fs::write(d.join("gone.404"), "").expect("404");
    assert_eq!(
        tools.github(&call(Method::Get, &gone, None)).expect("404"),
        None
    );
    let why = failed(tools.github(&call(Method::Post, &gone, Some(json!({})))));
    assert!(why.contains("(HTTP 404)"), "{why}");

    let broken = d.join("broken");
    std::fs::write(d.join("broken.fail"), "").expect("fail");
    assert!(failed(tools.github(&call(Method::Get, &broken, None))).contains("the call failed"));

    let text = d.join("text");
    std::fs::write(&text, "not json").expect("text");
    assert!(failed(tools.github(&call(Method::Get, &text, None))).contains("not JSON"));

    let mut tools = Process {
        cargo: OsStr::new(FAKE),
        gh: Path::new(MISSING),
        root: &d,
    };
    assert!(!spawn_error(tools.github(&call(Method::Get, &reply, None))).is_empty());
}
