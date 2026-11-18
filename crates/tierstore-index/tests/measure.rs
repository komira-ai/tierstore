//! The measurement tool of 10.2: the `tierstore-index-measure` binary and the library calls under
//! it, on both engines, in scratch directories.

#![allow(
    clippy::expect_used,
    reason = "an integration test: a failed expect is the failure it reports"
)]

use std::ffi::OsString;
use std::process::{Command, Output};

use tierstore_index::measure::{Engine, MeasureError, Options, Report, parse, run};
use tierstore_testkit::dir::TestDir;

fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), name)
}

fn tool(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tierstore-index-measure"))
        .args(args)
        .output()
        .expect("run the tool")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("utf8 stderr")
}

fn value<'a>(lines: &'a [String], name: &str) -> &'a str {
    let prefix = format!("{name}\t");
    let hits: Vec<&String> = lines.iter().filter(|l| l.starts_with(&prefix)).collect();
    assert_eq!(hits.len(), 1, "exactly one line for {name} in {lines:?}");
    &hits[0][prefix.len()..]
}

fn report_of_binary(engine: &str) -> Vec<String> {
    let dir = scratch("bin-run");
    let target = dir.path().join("store");
    let out = tool(&[
        "--engine",
        engine,
        "--dir",
        target.to_str().expect("utf8 path"),
        "--keys",
        "2000",
        "--reads",
        "200",
        "--cache-mib",
        "8",
        "--batch",
        "300",
    ]);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stderr(&out), "");
    String::from_utf8(out.stdout)
        .expect("utf8 stdout")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn check_report(engine: &str) {
    let lines = report_of_binary(engine);
    let names: Vec<&str> = lines
        .iter()
        .map(|l| l.split('\t').next().expect("name"))
        .collect();
    assert_eq!(
        names,
        [
            "engine",
            "keys",
            "load_ms",
            "disk_bytes",
            "bytes_per_key",
            "cache_bytes",
            "read_p50_us",
            "read_p99_us",
            "read_max_us",
            "peak_rss_bytes"
        ]
    );
    assert_eq!(value(&lines, "engine"), engine);
    assert_eq!(value(&lines, "keys"), "2000");
    assert_eq!(
        value(&lines, "cache_bytes"),
        (8u64 * 1024 * 1024).to_string()
    );
    let disk: u64 = value(&lines, "disk_bytes").parse().expect("disk bytes");
    assert!(disk > 0);
    let per_key: f64 = value(&lines, "bytes_per_key").parse().expect("per key");
    assert!(
        (per_key - disk as f64 / 2000.0).abs() < 0.01,
        "{per_key} vs {disk}/2000"
    );
    let p50: f64 = value(&lines, "read_p50_us").parse().expect("p50");
    let p99: f64 = value(&lines, "read_p99_us").parse().expect("p99");
    let max: f64 = value(&lines, "read_max_us").parse().expect("max");
    assert!(0.0 <= p50 && p50 <= p99 && p99 <= max, "{p50} {p99} {max}");
    let _: u64 = value(&lines, "load_ms").parse().expect("load ms");
    let _: u64 = value(&lines, "peak_rss_bytes").parse().expect("rss");
}

#[test]
fn the_binary_reports_every_field_for_fjall() {
    check_report("fjall");
}

#[test]
fn the_binary_reports_every_field_for_redb() {
    check_report("redb");
}

#[test]
fn the_binary_exits_2_naming_the_problem_for_each_usage_error() {
    let dir = scratch("usage");
    let busy = dir.path().join("busy");
    std::fs::create_dir(&busy).expect("mkdir");
    std::fs::write(busy.join("file"), b"x").expect("write");
    let busy = busy.to_str().expect("utf8 path");
    let cases: [(&[&str], &str); 6] = [
        (
            &["--engine", "fjall", "--keys", "10"],
            "missing required flag --dir",
        ),
        (
            &["--engine", "sqlite", "--dir", "d", "--keys", "10"],
            "flag --engine: \"sqlite\" is not fjall or redb",
        ),
        (
            &["--engine", "fjall", "--dir", "d", "--keys", "ten"],
            "flag --keys: \"ten\" is not a number",
        ),
        (
            &[
                "--engine", "fjall", "--dir", "d", "--keys", "10", "--wat", "1",
            ],
            "unknown flag --wat",
        ),
        (
            &["--engine", "redb", "--dir", busy, "--keys", "10"],
            &format!("directory {busy} is not empty"),
        ),
        (
            &["--engine", "redb", "--dir", "d", "--keys", "0"],
            "flag --keys: \"0\" is not a positive number",
        ),
    ];
    for (args, message) in cases {
        let out = tool(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert_eq!(
            stderr(&out),
            format!("tierstore-index-measure: {message}\n"),
            "{args:?}"
        );
        assert!(out.stdout.is_empty(), "{args:?}");
    }
    // Nothing was written into the directory that was refused.
    assert_eq!(std::fs::read_dir(busy).expect("dir").count(), 1);
}

#[test]
fn the_binary_exits_1_when_the_store_cannot_be_opened() {
    let dir = scratch("file-as-dir");
    let file = dir.path().join("plain");
    std::fs::write(&file, b"x").expect("write");
    for engine in ["fjall", "redb"] {
        let out = tool(&[
            "--engine",
            engine,
            "--dir",
            file.to_str().expect("utf8 path"),
            "--keys",
            "10",
        ]);
        assert_eq!(out.status.code(), Some(1), "{engine}");
        assert!(
            stderr(&out).starts_with(&format!("tierstore-index-measure: {engine}: ")),
            "{engine}: {}",
            stderr(&out)
        );
        assert!(out.stdout.is_empty());
    }
}

fn options(engine: Engine, dir: &TestDir, keys: u64, reads: u64) -> Options {
    Options {
        engine,
        dir: dir.path().join("store"),
        keys,
        reads,
        cache_bytes: 4 * 1024 * 1024,
        batch: 64,
        seed: 3,
    }
}

#[test]
fn run_loads_and_reads_on_both_engines_with_a_report_in_order() {
    for engine in [Engine::Fjall, Engine::Redb] {
        let dir = scratch("run");
        let opts = options(engine, &dir, 500, 100);
        let report = run(&opts).expect("run");
        assert_eq!(report.engine, engine);
        assert_eq!(report.keys, 500);
        assert_eq!(report.cache_bytes, 4 * 1024 * 1024);
        assert!(report.disk_bytes > 0);
        assert!((report.bytes_per_key - report.disk_bytes as f64 / 500.0).abs() < 1e-9);
        assert!(report.read_p50_us <= report.read_p99_us);
        assert!(report.read_p99_us <= report.read_max_us);
        assert_eq!(report.lines().len(), 10);
    }
}

#[test]
fn run_accepts_an_existing_empty_directory_and_refuses_a_used_one_untouched() {
    let dir = scratch("empty-then-used");
    let opts = Options {
        dir: dir.path().to_path_buf(),
        ..options(Engine::Redb, &dir, 50, 10)
    };
    run(&opts).expect("empty directory");
    let err = run(&opts).expect_err("now it is used");
    assert!(matches!(err, MeasureError::DirNotEmpty(ref d) if *d == opts.dir));
    assert!(err.is_usage());
}

#[test]
fn run_reports_a_failure_to_open_as_not_usage() {
    let dir = scratch("run-file");
    let file = dir.path().join("plain");
    std::fs::write(&file, b"x").expect("write");
    for engine in [Engine::Fjall, Engine::Redb] {
        let opts = Options {
            dir: file.clone(),
            ..options(engine, &dir, 10, 1)
        };
        let err = run(&opts).expect_err("a file is not a store");
        assert!(matches!(err, MeasureError::Backend(_)), "{err:?}");
        assert!(!err.is_usage());
    }
}

#[test]
fn parse_and_lines_round_trip_through_the_library_api() {
    let args: Vec<OsString> = ["--engine", "fjall", "--dir", "d", "--keys", "3"]
        .iter()
        .map(OsString::from)
        .collect();
    let opts = parse(args).expect("parse");
    assert_eq!(opts.engine, Engine::Fjall);
    let report = Report {
        engine: opts.engine,
        keys: opts.keys,
        load_ms: 1,
        disk_bytes: 30,
        bytes_per_key: 10.0,
        cache_bytes: opts.cache_bytes,
        read_p50_us: 0.5,
        read_p99_us: 1.0,
        read_max_us: 2.0,
        peak_rss_bytes: 0,
    };
    let lines = report.lines();
    assert_eq!(value(&lines, "engine"), "fjall");
    assert_eq!(value(&lines, "keys"), "3");
    assert_eq!(value(&lines, "peak_rss_bytes"), "0");
}
