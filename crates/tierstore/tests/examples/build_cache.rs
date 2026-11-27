//! The compiler-cache example (examples/build-cache): sccache 0.19.0 with its S3 backend on
//! tierstore. CI installs sccache (its version and SHA-256 pinned in ci.yml); where it is not on
//! `PATH` each test says so on stderr and ends.

use std::path::PathBuf;

use tierstore_examples::Error;
use tierstore_examples::report::Report;
use tierstore_examples::run::{Options, run};

use crate::{examples, scratch, tierstore};

/// The example run with `config` in place of its tierstore.toml, or `None` where sccache is not
/// installed.
fn run_example(work: PathBuf, config: Option<PathBuf>) -> Option<Report> {
    let options = Options {
        example: examples().join("build-cache"),
        tierstore: tierstore(),
        work,
        config,
    };
    match run(&options) {
        Ok(report) => Some(report),
        Err(Error::ClientMissing { program, source }) => {
            eprintln!("skipped: the client {program} is not installed ({source})");
            None
        }
        Err(other) => panic!("{other}"),
    }
}

/// Asserts the example's report is its expected.txt, line for line: the second build is all
/// sccache hits and tierstore answers its 31 reads from the cache with no request and no byte to
/// the cold tier, the objects are those of the first build, the store lists them, and the
/// refused rewrite of `.sccache_check` (komira-ai/tierstore#599) is still refused.
#[test]
fn the_build_cache_example_gives_its_expected_output() {
    let dir = scratch("example-build-cache");
    let Some(report) = run_example(dir.path().join("work"), None) else {
        return;
    };
    assert_eq!(report.actual, report.expected, "{}", report.describe());
    assert_eq!(report.now_passing, Vec::<String>::new());
}

/// Asserts that the example's checks catch a cache that never hits: over the same tierstore.toml
/// with a policy that keeps another prefix, sccache still gets every object (from the cold tier,
/// so its own lines are the same) and the run fails at tierstore's lines alone: its hits of the
/// second build, and its reads and bytes of the cold tier.
#[test]
fn the_build_cache_example_over_a_cache_that_keeps_nothing_differs_at_tierstores_lines() {
    let dir = scratch("example-build-cache-keeps-nothing");
    let text = std::fs::read_to_string(examples().join("build-cache/tierstore.toml")).unwrap();
    let at = text.find("[[policy]]").expect("the example has a policy");
    let keeps_nothing = format!(
        "{}[[policy]]\nprefix = \"elsewhere/\"\ncache  = true\n",
        &text[..at]
    );
    let config = dir.path().join("keeps-nothing.toml");
    std::fs::write(&config, keeps_nothing).unwrap();
    let Some(report) = run_example(dir.path().join("work"), Some(config)) else {
        return;
    };
    assert!(!report.passed());
    assert_eq!(
        report.differing(),
        [
            "build2.tierstore.hits",
            "build2.tierstore.upstream_gets",
            "build2.tierstore.upstream_heads",
            "build2.tierstore.upstream_bytes",
        ]
    );
}
