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

/// The transport flags: `/dev/fuse` by default, and `--transport uring` with the design's depth
/// (5) and memory limit (64 MiB) unless others are given.
#[test]
fn the_transport_flags_make_the_transport_asked_for() {
    use clap::Parser as _;

    use crate::tsnull::Transport;

    let transport = |extra: &[&str]| {
        let args = [
            "fsbench",
            "tsfuse-null-fs",
            "--list",
            "l",
            "--mountpoint",
            "m",
        ]
        .iter()
        .chain(["--data", "d"].iter())
        .chain(extra.iter());
        match super::Cli::try_parse_from(args).unwrap().command {
            super::Command::TsfuseNullFs(a) => a.transport.transport(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(transport(&[]), Transport::Dev);
    assert_eq!(
        transport(&["--transport", "uring"]),
        Transport::Uring(tsfuse::UringConfig::default())
    );
    assert_eq!(
        transport(&["--transport", "uring", "--uring-depth", "16"]),
        Transport::Uring(tsfuse::UringConfig {
            depth: std::num::NonZeroU16::new(16).unwrap(),
            memory_limit: 64 << 20,
        })
    );
    assert_eq!(
        transport(&["--transport", "uring", "--uring-memory-mib", "512"]),
        Transport::Uring(tsfuse::UringConfig {
            depth: tsfuse::UringConfig::DEPTH,
            memory_limit: 512 << 20,
        })
    );
    assert_eq!(
        transport(&["--uring-depth", "16"]),
        Transport::Dev,
        "the depth alone asks for nothing"
    );
}

/// The storm's flags make its plan: the storm of the depth asked for unless `--per-cpu` says
/// otherwise, the CPUs this process may run on, the other probe's CPU, and the times; a storm
/// CPU that is not online is refused.
#[test]
fn the_storm_flags_make_its_plan() {
    use clap::Parser as _;

    let dir = TestDir::under(std::env::temp_dir(), "fsbench-cli-storm");
    let sys = dir.path().join("sys");
    std::fs::create_dir_all(sys.join("devices/system/cpu")).unwrap();
    std::fs::write(sys.join("devices/system/cpu/online"), "0\n").unwrap();
    let host = crate::host::Host {
        proc: dir.path().join("proc"),
        sys,
    };
    let plan = |extra: &[&str]| {
        let args = ["fsbench", "storm", "--mountpoint", "m", "--out", "o"]
            .iter()
            .chain(["--label", "l"].iter())
            .chain(extra.iter());
        match super::Cli::try_parse_from(args).unwrap().command {
            super::Command::Storm(a) => a.plan(host.clone()),
            other => panic!("{other:?}"),
        }
    };
    let p = plan(&[
        "--uring-depth",
        "4",
        "--park-us",
        "1500",
        "--setting",
        "a=b",
    ])
    .unwrap();
    assert_eq!(p.per_cpu, 6, "ceil(4 x 1.5)");
    assert_eq!(
        (p.cpus.clone(), p.storm_cpu, p.other_cpu),
        (vec![0], 0, None)
    );
    assert_eq!(p.park, std::time::Duration::from_micros(1500));
    assert_eq!(p.window, std::time::Duration::from_secs(20));
    assert_eq!(p.sample_every, std::time::Duration::from_millis(100));
    assert_eq!((p.runs, p.max_probes, p.readers.get()), (3, 0, 1));
    assert_eq!(p.transport, crate::tsnull::Transport::Dev);
    assert_eq!(p.settings["a"], "b");
    assert_eq!(p.read, crate::stormfs::ColdRead::Buffered);
    let p = plan(&["--per-cpu", "3", "--read", "direct", "--secs", "2"]).unwrap();
    assert_eq!((p.per_cpu, p.read), (3, crate::stormfs::ColdRead::Direct));
    assert_eq!(
        plan(&["--storm-cpu", "1"]).unwrap_err().to_string(),
        "CPU 1 is not online"
    );
}

/// `storm` with a results file that cannot be opened, and `storm-report` of a file that is not
/// there or not a storm record, fail before anything is mounted, and say why.
#[test]
fn the_storm_commands_fail_on_files_they_cannot_use() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-cli-storm-files");
    let out = dir.path().join("missing/x.jsonl");
    let (code, _, err) = fsbench(
        &[
            "storm",
            "--mountpoint",
            "m",
            "--label",
            "l",
            "--out",
            text(&out),
        ],
        "",
    );
    assert_eq!(code, ExitCode::from(1));
    assert!(
        err.starts_with(&format!("fsbench: open {}: ", out.display())),
        "{err}"
    );
    let missing = dir.path().join("none.jsonl");
    let (code, _, err) = fsbench(&["storm-report", "--results", text(&missing)], "");
    assert_eq!(code, ExitCode::from(1));
    assert!(
        err.starts_with(&format!("fsbench: read {}: ", missing.display())),
        "{err}"
    );
    let bad = dir.path().join("bad.jsonl");
    std::fs::write(&bad, "{}\n").unwrap();
    let (code, _, err) = fsbench(&["storm-report", "--results", text(&bad)], "");
    assert_eq!(code, ExitCode::from(1));
    assert!(
        err.starts_with(&format!(
            "fsbench: {}:1: not a storm record: ",
            bad.display()
        )),
        "{err}"
    );
}

/// `summary` puts the series of each results file in one table, each row named by its file;
/// `dstate-report` prints the tables of the runs an index names. A file that either command
/// cannot read fails it.
#[test]
fn summary_and_dstate_report_print_their_tables() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-cli-summary");
    let one = dir.path().join("2026-11-one.jsonl");
    let mut record = crate::record::example();
    std::fs::write(&one, serde_json::to_string(&record).unwrap() + "\n").unwrap();
    record.variant = "v3-r4".to_owned();
    let two = dir.path().join("two.jsonl");
    std::fs::write(&two, serde_json::to_string(&record).unwrap() + "\n").unwrap();
    let (code, stdout, stderr) = fsbench(
        &[
            "summary",
            "--results",
            text(&one),
            "--results",
            text(&two),
            "--discard",
            "0",
        ],
        "",
    );
    assert_eq!((code, stderr.as_str()), (ExitCode::SUCCESS, ""));
    let files = [
        ("2026-11-one".to_owned(), crate::report::read(&one).unwrap()),
        ("two".to_owned(), crate::report::read(&two).unwrap()),
    ];
    assert_eq!(stdout, crate::summary::render(&files, 0));
    assert!(stdout.contains("| two | v3-r4 | caches-off |"), "{stdout}");
    let missing = dir.path().join("missing.jsonl");
    let (code, _, stderr) = fsbench(&["summary", "--results", text(&missing)], "");
    assert_eq!(code, ExitCode::from(1));
    assert!(stderr.starts_with("fsbench: read "), "{stderr}");

    std::fs::write(
        dir.path().join("n1.txt"),
        "@node 7.0.0 4\n@tick 2000000000\n#count\n3 1\n@end 2000000001\n",
    )
    .unwrap();
    let index = dir.path().join("runs.json");
    std::fs::write(
        &index,
        r#"{"runs": [{"run": "run1", "build_start_ns": 1000000000,
            "build_end_ns": 3000000000, "settings": {},
            "nodes": [{"node": "n1", "raw": "n1.txt"}]}]}"#,
    )
    .unwrap();
    let (code, stdout, stderr) = fsbench(&["dstate-report", "--index", text(&index)], "");
    assert_eq!((code, stderr.as_str()), (ExitCode::SUCCESS, ""));
    assert_eq!(
        stdout,
        crate::dstaterep::render(&crate::dstate::load(&index).unwrap())
    );
    assert!(
        stdout.contains("| run1 | 2.00 | 1 | 7.0.0 |  |"),
        "{stdout}"
    );
    let (code, _, stderr) = fsbench(&["dstate-report", "--index", text(&missing)], "");
    assert_eq!(code, ExitCode::from(1));
    assert!(stderr.starts_with("fsbench: read "), "{stderr}");
}
