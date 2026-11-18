//! The command line in this process: each command a person runs, and the two that `run` starts.
//! `run` itself, which starts processes of the executable, is in `tests/harness.rs`.

use std::ffi::OsString;
use std::io::Cursor;
use std::path::Path;
use std::process::ExitCode;

use tierstore_testkit::dir::TestDir;

use super::{run, setting, user};

/// The exit code, standard output and standard error of `fsbench <args>` with `stdin`.
fn fsbench(args: &[&str], stdin: &str) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let argv: Vec<OsString> = std::iter::once("fsbench")
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    let code = run(
        argv,
        Path::new("no-exe"),
        Box::new(Cursor::new(stdin.to_owned().into_bytes())),
        &mut out,
        &mut err,
    );
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

/// `list` writes the list of a tree and says how many files and bytes it holds, and its digest;
/// `report` prints the tables of a results file.
#[test]
fn list_and_report_print_what_they_made() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-cli-list");
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(tree.join("d")).unwrap();
    std::fs::write(tree.join("d/x"), b"12345").unwrap();
    let out = dir.path().join("list");
    let (code, stdout, stderr) = fsbench(&["list", "--tree", text(&tree), "--out", text(&out)], "");
    assert_eq!((code, stderr.as_str()), (ExitCode::SUCCESS, ""));
    let digest = crate::list::read(&out).unwrap().sha256;
    assert_eq!(stdout, format!("1 files, 5 bytes, sha256 {digest}\n"));
    let results = dir.path().join("results.jsonl");
    std::fs::write(
        &results,
        serde_json::to_string(&crate::record::example()).unwrap() + "\n",
    )
    .unwrap();
    let (code, stdout, _) = fsbench(
        &["report", "--results", text(&results), "--discard", "0"],
        "",
    );
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(
        stdout,
        crate::report::render(&[crate::record::example()], 0)
    );
}

/// A command that fails exits 1 and says why on standard error; a command line that is not one
/// exits 2 with clap's message; `--help` prints the usage on standard output and exits 0.
#[test]
fn failures_and_bad_command_lines_have_their_exit_codes() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-cli-fail");
    let missing = dir.path().join("missing");
    let (code, stdout, stderr) = fsbench(&["report", "--results", text(&missing)], "");
    assert_eq!((code, stdout.as_str()), (ExitCode::from(1), ""));
    assert!(
        stderr.starts_with(&format!("fsbench: read {}: ", missing.display())),
        "{stderr}"
    );
    let (code, _, stderr) = fsbench(&["report"], "");
    assert_eq!(code, ExitCode::from(2));
    assert!(stderr.contains("--results"), "{stderr}");
    let (code, stdout, stderr) = fsbench(&["--help"], "");
    assert_eq!((code, stderr.as_str()), (ExitCode::SUCCESS, ""));
    assert!(stdout.contains("Usage: fsbench"), "{stdout}");
    let (code, _, stderr) = fsbench(&["run", "--as-user", "12"], "");
    assert_eq!(code, ExitCode::from(2));
    assert!(stderr.contains("not UID:GID"), "{stderr}");
}

/// The `reader` command reads its list, says it is ready and answers each go on its input; the
/// `burn` command ends when its input does.
#[test]
fn the_reader_and_burner_commands_speak_on_their_streams() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-cli-reader");
    std::fs::write(dir.path().join("f"), b"abc").unwrap();
    let list = dir.path().join("list");
    std::fs::write(&list, format!("{}\n3\tf\n", crate::list::HEADER)).unwrap();
    let go = r#"{"phase":"whole","index":0,"of":1,"budget":10,"cold":false}"#;
    let (code, stdout, _) = fsbench(
        &["reader", "--list", text(&list), "--tree", text(dir.path())],
        &format!("{go}\n"),
    );
    assert_eq!(code, ExitCode::SUCCESS);
    assert_eq!(
        stdout,
        "{\"answer\":\"ready\"}\n{\"answer\":\"whole\",\"files\":1,\"bytes\":3,\"errors\":0}\n"
    );
    assert_eq!(fsbench(&["burn"], "").0, ExitCode::SUCCESS);
}

/// A writer that refuses every write.
struct Broken;

impl std::io::Write for Broken {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("refused"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What a command cannot write fails it: the line of `list` on a standard output that refuses
/// it, and the results file of `run` in a directory that is not there (before any process
/// starts).
#[test]
fn an_output_that_cannot_be_written_fails_the_command() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-cli-broken");
    std::fs::create_dir(dir.path().join("tree")).unwrap();
    let mut err = Vec::new();
    let (tree, list) = (dir.path().join("tree"), dir.path().join("list"));
    let argv = [
        "fsbench",
        "list",
        "--tree",
        text(&tree),
        "--out",
        text(&list),
    ];
    let code = run(
        argv.map(OsString::from),
        Path::new("no-exe"),
        Box::new(std::io::empty()),
        &mut Broken,
        &mut err,
    );
    assert_eq!(code, ExitCode::from(1));
    assert_eq!(
        String::from_utf8(err).unwrap(),
        "fsbench: write standard output: refused\n"
    );
    let missing = dir.path().join("missing/results.jsonl");
    let (code, _, stderr) = fsbench(
        &[
            "run",
            "--list",
            "l",
            "--tree",
            "t",
            "--variant",
            "v",
            "--regime",
            "r",
            "--server-pid",
            "1",
            "--out",
            text(&missing),
        ],
        "",
    );
    assert_eq!(code, ExitCode::from(1));
    assert!(
        stderr.starts_with(&format!("fsbench: open {}: ", missing.display())),
        "{stderr}"
    );
}

/// The program on this process's own arguments (those of the test harness, which are not a
/// command of `fsbench`) is a command line that is not one: exit 2.
#[test]
fn the_program_on_the_test_harness_arguments_exits_2() {
    assert_eq!(super::main(), ExitCode::from(2));
}

/// `--as-user` is `UID:GID` and `--setting` is `KEY=VALUE` (the value may hold `=`).
#[test]
fn users_and_settings_are_parsed() {
    assert_eq!(user("1000:1001"), Ok((1000, 1001)));
    assert_eq!(user("1000"), Err("not UID:GID".to_owned()));
    assert_eq!(user("a:1"), Err("the UID is not a number".to_owned()));
    assert_eq!(user("1:b"), Err("the GID is not a number".to_owned()));
    assert_eq!(
        setting("open=explicit=1"),
        Ok(("open".to_owned(), "explicit=1".to_owned()))
    );
    assert_eq!(setting("open"), Err("not KEY=VALUE".to_owned()));
}
