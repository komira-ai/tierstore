//! The lines of the lookups that fail, said through `Core::say_failed`: once in ten seconds, with
//! the count of those not said.

use std::time::Duration;

use super::super::Errno;
use super::SAY_EVERY_MS;
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Logged, Rig, logged, sample, sha};

/// The lines said while `run` runs, at warn.
fn said(run: impl FnOnce()) -> Vec<String> {
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    run();
    drop(guard);
    log.text().lines().map(str::to_owned).collect()
}

/// Asserts that a lookup of a placement the fill socket cannot answer is `EIO` said whole, that a
/// second within ten seconds is not said, and that the next after them carries the count.
#[test]
fn a_placement_lookup_that_fails_is_said_once_in_ten_seconds_with_the_count() {
    let rig = Rig::new("tfs-said-asked", sample());
    let _ = rig
        .upstream
        .answers
        .lock()
        .unwrap()
        .insert("place".to_owned(), "busy".to_owned());
    let lines = said(|| {
        for _ in 0..2 {
            assert_eq!(rig.core.lookup(ino::ROOT, "builds"), Err(Errno::Io));
        }
        rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
        assert_eq!(rig.core.lookup(ino::ROOT, "builds"), Err(Errno::Io));
    });
    let line = r#"tierstore fs: cannot look up the placement at "builds": busy"#;
    assert_eq!(
        lines,
        [
            line.to_owned(),
            format!("{line}; 1 more other failed request since the last line"),
        ]
    );
}

/// Asserts that a lookup of a tree whose root manifest cannot be had now is `EIO` said once in
/// ten seconds, with the count of those not said on the next line.
#[test]
fn a_tree_lookup_that_fails_is_said_once_in_ten_seconds_with_the_count() {
    let rig = Rig::new("tfs-said-refusal", sample());
    let digest = sha(b"a tree that is not built");
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(digest.clone(), "upstream 503".to_owned());
    let lines = said(|| {
        for _ in 0..3 {
            assert_eq!(rig.core.lookup(ino::TREES, &digest), Err(Errno::Io));
        }
        rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
        assert_eq!(rig.core.lookup(ino::TREES, &digest), Err(Errno::Io));
    });
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(
        lines[0].starts_with(&format!(
            "tierstore fs: cannot attach {digest} on a lookup: "
        )),
        "{lines:?}"
    );
    assert_eq!(
        lines[1],
        format!(
            "{}; 2 more other failed requests since the last line",
            lines[0]
        )
    );
}
