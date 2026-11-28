//! The `tierstore-example` binary: it forwards its command line, its output and its exit status
//! to the library (`tierstore_examples::cli::main`). The runs of examples are tested in
//! crates/tierstore/tests/examples, which have a `tierstore` to start; this test needs none.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::path::Path;
use std::process::Command;

use tierstore_testkit::dir::TestDir;

/// Runs the binary with `args` and gives its status, standard output and standard error.
fn run(args: &[&Path]) -> (Option<i32>, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tierstore-example"))
        .args(args)
        .output()
        .unwrap();
    let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
    (out.status.code(), text(out.stdout), text(out.stderr))
}

/// Asserts the binary checks a page and exits 0 with no output when the page shows its files,
/// 1 with the problem on standard error when it does not, and 2 with the usage when it is given
/// no verb.
#[test]
fn the_binary_checks_a_page_and_exits_with_the_library_status() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "binary-page");
    let example = dir.path();
    std::fs::write(example.join("page.md"), "```text file=a\na\n```\n").unwrap();
    std::fs::write(example.join("a"), "a\n").unwrap();
    let verb = Path::new("check-page");
    assert_eq!(
        run(&[verb, example]),
        (Some(0), String::new(), String::new())
    );

    std::fs::write(example.join("a"), "b\n").unwrap();
    let page = example.join("page.md");
    let said = format!(
        "tierstore-example: {}: the page's a differs from the file at line 1\n",
        page.display()
    );
    assert_eq!(run(&[verb, example]), (Some(1), String::new(), said));

    let (code, out, err) = run(&[]);
    assert_eq!((code, out.as_str()), (Some(2), ""));
    assert!(err.contains("Usage: tierstore-example"), "{err}");
}
