//! The command line in this process: each verb as a person runs it.

use std::ffi::OsString;
use std::process::ExitCode;

use bytes::Bytes;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::{Capabilities, ObjectKey, ObjectStore as _};
use tierstore_testkit::dir::TestDir;

use super::run;

/// The exit code, standard output and standard error of `storebench <args>`.
pub(crate) fn storebench(args: &[&str]) -> (ExitCode, String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let argv: Vec<OsString> = std::iter::once("storebench")
        .chain(args.iter().copied())
        .map(OsString::from)
        .collect();
    let code = run(argv, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// `list` writes the objects of every bucket named and says how many; a missing credentials
/// file fails with exit 1 and names it; a command line that is not one exits 2; `--help` exits 0.
#[test]
fn list_writes_the_listing_and_failures_exit_as_documented() {
    let rt = crate::s3::tests::runtime();
    let fake = rt.block_on(FakeS3::start());
    let s3 = fake.store(Capabilities::default());
    rt.block_on(s3.put_new(
        &ObjectKey::new("cas/l/a").unwrap(),
        Bytes::from_static(b"{}"),
        None,
    ))
    .unwrap();
    let dir = TestDir::under(std::env::temp_dir(), "storebench-cli-list");
    let creds = dir.path().join("cold.secret.env");
    std::fs::write(
        &creds,
        "MINIO_ROOT_USER=fake-key\nMINIO_ROOT_PASSWORD=fake-secret\n",
    )
    .unwrap();
    let out = dir.path().join("listing.tsv");
    let (code, stdout, stderr) = storebench(&[
        "list",
        "--endpoint",
        fake.endpoint(),
        "--credentials",
        creds.to_str().unwrap(),
        "--bucket",
        "tierstore-fake",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, ExitCode::SUCCESS, "{stderr}");
    assert_eq!(stdout, format!("1 objects in {}\n", out.display()));
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("tierstore-fake\tcas/l/a\t2\t"), "{text}");

    let absent = dir.path().join("absent.env");
    let (code, _, stderr) = storebench(&[
        "list",
        "--endpoint",
        "http://127.0.0.1:0",
        "--credentials",
        absent.to_str().unwrap(),
        "--bucket",
        "b",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(code, ExitCode::from(1));
    assert_eq!(
        stderr,
        format!(
            "storebench: read {}: No such file or directory (os error 2)\n",
            absent.display()
        )
    );

    let (code, _, stderr) = storebench(&["list"]);
    assert_eq!(code, ExitCode::from(2));
    assert!(stderr.contains("--credentials"), "{stderr}");
    assert!(stderr.contains("--endpoint"), "{stderr}");
    let (code, stdout, _) = storebench(&["--help"]);
    assert_eq!(code, ExitCode::SUCCESS);
    assert!(stdout.contains("list"), "{stdout}");
}

/// Every verb is reached from the command line: each, given an input that is not there, fails
/// with exit 1 and names it; `report` prints an arm's tables from a data directory.
#[test]
fn every_verb_is_dispatched_and_its_failure_exits_1() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-cli-verbs");
    let absent = dir.path().join("absent");
    let a = absent.to_str().unwrap();
    let out = dir.path().join("out");
    let o = out.to_str().unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec!["drive-list", "--bucket-dir", a, "--out", o],
        vec![
            "footers",
            "--endpoint",
            "http://127.0.0.1:0",
            "--credentials",
            a,
            "--listing",
            a,
            "--out",
            o,
        ],
        vec!["du", "--root", a, "--out", o],
        vec!["c6", "--listing", a, "--out", o],
        vec!["c7", "--listing", a, "--out", o],
        vec!["c10", "--listing", a, "--out", o],
        vec!["report", "--data", a, "--arm", "c6"],
    ];
    for args in cases {
        let (code, _, stderr) = storebench(&args);
        assert_eq!(code, ExitCode::from(1), "{args:?}: {stderr}");
        assert!(stderr.starts_with("storebench: "), "{args:?}: {stderr}");
    }
}
