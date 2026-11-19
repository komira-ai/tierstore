//! Unit tests that need the crate's private protocol: the loom model of 5.1, and the cell runs.

mod loom_model;

use tierstore_fetch_api::ids::CellRun;

use crate::protocol::runs_of;

fn run(first: u32, count: u32) -> CellRun {
    CellRun { first, count }
}

#[test]
fn cells_are_grouped_into_contiguous_runs() {
    assert_eq!(runs_of(&[]), []);
    assert_eq!(runs_of(&[5]), [run(5, 1)]);
    assert_eq!(
        runs_of(&[0, 1, 2, 4, 5, 9]),
        [run(0, 3), run(4, 2), run(9, 1)]
    );
    assert_eq!(runs_of(&[u32::MAX - 1, u32::MAX]), [run(u32::MAX - 1, 2)]);
}
