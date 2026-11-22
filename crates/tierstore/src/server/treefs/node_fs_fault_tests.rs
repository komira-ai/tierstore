//! The lines NodeFs says for the `EIO`s it answers itself.

use super::*;
use crate::server::treefs::testing::{Logged, logged};

/// Asserts that an open refused because its adopted backing id is not one this process can answer
/// with (too large, or of another kind) is `EIO` and says so on stderr, whole, once in ten seconds
/// for its tree.
#[test]
fn an_adopted_id_this_process_cannot_answer_with_is_said() {
    let wire = Wire::new("nodefs-adopted-said");
    wire.adopt(u32::MAX, &|id| -> Guard { Arc::new(BackingId(id)) });
    // The tree stays in memory with no run (the kernel holds a lookup of it and it is not idle),
    // so the open asks no pin and is answered on the reader, whose lines the log takes.
    let _ = entry_out(&wire.ask(op::LOOKUP, node(0), &cstr(b"bin")));
    assert_eq!(wire.kept_after_detach(), 1);
    let log = Logged::default();
    let guard = logged(&log, tracing::Level::WARN);
    for _ in 0..2 {
        let refused = wire.error(op::OPEN, node(CC), &open_in(0));
        assert_eq!(refused, reads(rustix::io::Errno::IO));
    }
    drop(guard);
    let said: Vec<String> = log.text().lines().map(str::to_owned).collect();
    assert_eq!(
        said,
        [format!(
            "tierstore fs: cannot open the inode {}: its backing id of an earlier release is not \
             one this process can answer with",
            node(CC)
        )]
    );
}
