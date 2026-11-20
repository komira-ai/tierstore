//! The tables of the real-build run: each node's ticks split by phase, the shares over the job
//! threads doing or waiting for work, and every figure that has no sample shown as `-`.

use std::collections::BTreeMap;

use super::{mean, phase, rank, render, share};
use crate::dstate::{NodeRun, RunEntry, Tick};

fn entry(run: &str, build_ms: u64) -> RunEntry {
    RunEntry {
        run: run.to_owned(),
        build_start_ns: 5_000_000_000,
        build_end_ns: 5_000_000_000 + build_ms * 1_000_000,
        settings: BTreeMap::from([("targets".to_owned(), "//a:b //c:d".to_owned())]),
        nodes: Vec::new(),
    }
}

fn node(run: &str, name: &str, kernel: &str, ticks: Vec<Tick>) -> NodeRun {
    NodeRun {
        run: run.to_owned(),
        node: name.to_owned(),
        kernel: kernel.to_owned(),
        cpus: 88,
        connections: vec![519],
        build_ms: 10_000,
        ticks,
    }
}

/// A tick at `t_ms` with `running` job threads running, `d` in D state of which `fuse_d` wait on
/// FUSE by request, and `waiting` requests waiting.
fn tick(t_ms: i64, running: u32, d: u32, fuse_d: u32, waiting: Option<u64>) -> Tick {
    Tick {
        t_ms,
        load1: 2.0,
        waiting,
        jobs: 2,
        job_threads: 100,
        job_running: running,
        job_d: d,
        job_fuse_request: fuse_d,
        d,
        fuse_d,
        ..Tick::default()
    }
}

/// A tick is before the build when it is earlier than its start, in it up to its end, after it
/// later.
#[test]
fn a_tick_s_phase_is_its_time_against_the_build() {
    let at = |t_ms| {
        phase(
            &Tick {
                t_ms,
                ..Tick::default()
            },
            1000,
        )
    };
    assert_eq!(
        [at(-1), at(0), at(1000), at(1001)],
        ["before", "build", "build", "after"]
    );
}

/// A mean, a nearest-rank quantile and a share; `-` when there is nothing to take them of.
#[test]
fn means_ranks_and_shares_say_dash_when_there_is_nothing() {
    let values: Vec<f64> = (1..=199).map(f64::from).collect();
    assert_eq!(mean(&values), "100");
    assert_eq!(rank(&values, 0.99), "198");
    assert_eq!(rank(&values, 1.0), "199");
    assert_eq!(rank(&[3.0, 1.0, 2.0], 0.0), "1.00");
    assert_eq!(
        (mean(&[]), rank(&[], 0.5)),
        ("-".to_owned(), "-".to_owned())
    );
    assert_eq!(
        (share(1, 3), share(0, 0)),
        ("33.3".to_owned(), "-".to_owned())
    );
}

/// The first table says each run's build time, nodes, kernels and settings; the second has a
/// row per node and phase with ticks (a phase with none has no row); the third puts each run's
/// build phase of every node together.
#[test]
fn the_tables_split_each_node_by_phase_and_put_the_build_together() {
    let n1 = node(
        "run1",
        "n1",
        "7.0.0-42-generic",
        vec![
            tick(-1000, 1, 0, 0, Some(0)),
            tick(0, 6, 2, 1, Some(3)),
            tick(1000, 2, 0, 0, Some(1)),
            tick(11_000, 0, 0, 0, Some(0)),
        ],
    );
    let n2 = node(
        "run1",
        "n2",
        "7.0.0-43-generic",
        vec![tick(500, 0, 1, 1, None)],
    );
    let quiet = node(
        "run2",
        "n1",
        "7.0.0-42-generic",
        vec![tick(0, 0, 0, 0, None)],
    );
    let text = render(&[
        (entry("run1", 10_000), vec![n1, n2]),
        (entry("run2", 2_500), vec![quiet]),
    ]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[..4],
        [
            "| run | build s | nodes | kernels | settings |",
            "|---|---|---|---|---|",
            "| run1 | 10.0 | 2 | 7.0.0-42-generic, 7.0.0-43-generic | targets=//a:b //c:d |",
            "| run2 | 2.50 | 1 | 7.0.0-42-generic | targets=//a:b //c:d |",
        ]
    );
    let row = |start: &str| -> String {
        lines
            .iter()
            .find(|l| l.starts_with(start))
            .unwrap_or_else(|| panic!("no line {start:?} in\n{text}"))
            .to_string()
    };
    assert_eq!(
        row("| run1 | n1 | before |"),
        "| run1 | n1 | before | 1 | 2.00 | 2.00 | 100 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 | \
         0.00 | 0.00 | 0.00 | 0.00 | 0.00 | 0.00 |"
    );
    // Build: running 6 and 2, D 2 and 0, one FUSE wait: 1 of 10 active thread samples.
    assert_eq!(
        row("| run1 | n1 | build |"),
        "| run1 | n1 | build | 2 | 2.00 | 2.00 | 100 | 1.00 | 2.00 | 0.50 | 1.00 | 1.00 | \
         0.00 | 2.00 | 3.00 | 3.00 | 10.0 | 20.0 |"
    );
    assert!(row("| run1 | n1 | after | 1 |").ends_with("| 0.00 | 0.00 | 0.00 | - | - |"));
    // n2 has no tick before or after, and no connection read: no rows for those, `-` waiting.
    assert_eq!(text.matches("| run1 | n2 |").count(), 1);
    assert!(
        row("| run1 | n2 | build | 1 |").ends_with("| 0.00 | - | - | - | 100 | 100 |"),
        "{text}"
    );
    // Both nodes' build ticks: 11 active, 2 waiting on FUSE by request, 3 in D state.
    assert_eq!(
        row("| run1 | 11 |"),
        "| run1 | 11 | 18.2 | 18.2 | 0.00 | 0.00 | 27.3 | 1.00 | 3.00 |"
    );
    assert_eq!(
        row("| run2 | 0 |"),
        "| run2 | 0 | - | - | - | - | - | 0.00 | - |"
    );
}
