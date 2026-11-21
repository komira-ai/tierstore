//! The opens answered with no request of the fill socket: what they refuse at once, and the line
//! of a table of tree numbers that cannot be read, said once in ten seconds.

use std::time::Duration;

use super::super::blob::SAY_EVERY_MS;
use super::super::{Errno, Opened};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{Logged, Rig, logged, sample};

const RUN: &str = "run-1";

/// A rig over the sample tree with [`RUN`] attached, which is in memory.
fn attached(name: &str) -> Rig {
    let rig = Rig::new(name, sample());
    let root = rig.upstream.built.lock().unwrap().root.clone();
    let _ = rig.core.attach(&root, RUN).unwrap();
    rig
}

/// The errno of an open answered now, or why it was not answered now.
fn refused(opened: Option<Result<Opened, Errno>>) -> Errno {
    match opened {
        Some(Err(errno)) => errno,
        Some(Ok(_)) => panic!("an open answered where a refusal was due"),
        None => panic!("an open parked where a refusal was due"),
    }
}

/// Asserts that an open of a directory of the namespace (the root, `trees/`, a directory above a
/// placement's location) is `EISDIR` and one of an index no entry of the tree has is `ENOENT`, at
/// once; and that an open for writing is `EROFS` before anything is looked at.
#[test]
fn an_open_of_the_namespace_or_of_no_entry_is_refused_at_once() {
    let rig = attached("tfs-now-refused");
    let core = &rig.core;
    for dir in [ino::ROOT, ino::TREES, ino::placed_dir(7)] {
        assert_eq!(refused(core.open_now(dir, false)), Errno::IsDir, "{dir:#x}");
    }
    assert_eq!(refused(core.open_now(ino::of(1, 99), false)), Errno::NoEnt);
    assert_eq!(refused(core.open_now(ino::of(1, 99), true)), Errno::RoFs);
    assert_eq!(core.open_files(), 0);
}

/// Asserts that a table of tree numbers that cannot be read is said once in ten seconds: a second
/// failure within them is `EIO` and not said, and one after them is said with the count.
#[test]
fn a_table_that_cannot_be_read_is_said_once_in_ten_seconds() {
    let rig = attached("tfs-now-table");
    let core = &rig.core;
    assert!(core.detach(RUN).is_some());
    assert_eq!(core.trees_in_memory(), 0);
    let table = std::fs::OpenOptions::new()
        .write(true)
        .open(rig.runtime.join("trees"))
        .unwrap();
    table.set_len(16).unwrap();
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    let root = ino::of(1, 0);
    assert_eq!(core.getattr(root), Err(Errno::Io));
    assert_eq!(core.getattr(root), Err(Errno::Io));
    rig.clock.advance(Duration::from_millis(SAY_EVERY_MS));
    assert_eq!(core.getattr(root), Err(Errno::Io));
    drop(guard);
    let line = "tierstore fs: cannot read the tree number 1 from the table: the table of tree \
                numbers: failed to fill whole buffer";
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(
        said,
        [
            line.to_owned(),
            format!("{line}; 1 more failed read of this tree since the last line"),
        ]
    );
}
