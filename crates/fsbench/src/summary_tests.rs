//! The table of several results files: a row per series of each file, in the files' order and
//! the series' order of first appearance, the first runs left out, `-` where no run measured a
//! figure.

use super::render;
use crate::host::ProcessCpu;
use crate::record::{PhaseCpu, RunRecord, example};

fn record(variant: &str, run: u32, per_s: f64, cold_cpu: Option<f64>) -> RunRecord {
    let mut r = example();
    r.variant = variant.to_owned();
    r.run = run;
    r.cycles.per_s = per_s;
    r.cold.cpu = cold_cpu.map(|ms| PhaseCpu {
        server: ProcessCpu {
            user_ms: 1,
            system_ms: 1,
        },
        cgroup: None,
        server_ms_per_gb: ms,
        cgroup_system_ms_per_gb: None,
    });
    r
}

/// The same label in two files is two rows, each named by its file; a series whose every run is
/// left out has `-` figures; a figure only some runs have is taken over those.
#[test]
fn the_table_has_a_row_per_series_of_each_file() {
    let first = vec![
        record("null", 0, 1.0, None),
        record("v1", 0, 1.0, None),
        record("null", 1, 8000.0, None),
        record("null", 2, 7000.0, Some(600.0)),
        record("null", 3, 9000.0, Some(700.0)),
    ];
    let second = vec![
        record("null", 0, 5.0, None),
        record("null", 1, 6000.0, None),
    ];
    let text = render(
        &[("first".to_owned(), first), ("second".to_owned(), second)],
        1,
    );
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines,
        [
            "| measurement | variant | regime | opens/s | stat p99 ms | open p99 ms | cold MB/s | \
             warm MB/s | server CPU ms/1k | cold server CPU ms/GB |",
            "|---|---|---|---|---|---|---|---|---|---|",
            "| first | null | caches-off | 8000 (7000-9000) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | \
             100 (100-100) | 400 (400-400) | 2.50 (2.50-2.50) | 650 (600-700) |",
            "| first | v1 | caches-off | - | - | - | - | - | - | - |",
            "| second | null | caches-off | 6000 (6000-6000) | 0.00 (0.00-0.00) | 0.00 (0.00-0.00) | \
             100 (100-100) | 400 (400-400) | 2.50 (2.50-2.50) | - |",
        ]
    );
}
