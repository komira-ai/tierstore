//! The parts of a run that need no child process: the checks of an answer, the samples, the
//! sums and the CPU arithmetic. A whole run, with its readers and burners, is in
//! `tests/harness.rs`, which has the `fsbench` executable to start them with.

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{
    CpuAt, Plan, cpu_between, cycles_of, ended, merge, ready, run, sample, summary, throughput,
    whole_of,
};
use crate::hist::Histogram;
use crate::host::{CgroupCpu, Host, NodeCpu, ProcessCpu};
use crate::reader::{Answer, CycleCounts, WholeCounts};

fn put(root: &Path, path: &str, text: &str) {
    let at = root.join(path);
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::fs::write(at, text).unwrap();
}

/// An answer of the kind a phase waits for is taken, any other is an error that names the child
/// and what it said; a child that ended with an error status is an error that says how.
#[test]
fn an_answer_of_another_phase_and_a_failed_exit_are_errors() {
    let cycles = Answer::Cycles(Box::default());
    let whole = Answer::Whole(WholeCounts::default());
    assert!(ready("reader 0", Answer::Ready).is_ok());
    assert_eq!(
        ready("reader 0", whole.clone()).unwrap_err().to_string(),
        "reader 0: answered Whole(WholeCounts { files: 0, bytes: 0, errors: 0 }) before its ready"
    );
    assert_eq!(
        cycles_of("reader 1", cycles.clone()).unwrap(),
        CycleCounts::default()
    );
    assert_eq!(
        cycles_of("reader 1", Answer::Ready)
            .unwrap_err()
            .to_string(),
        "reader 1: answered Ready to a cycles go"
    );
    assert_eq!(whole_of("reader 2", whole).unwrap(), WholeCounts::default());
    assert_eq!(
        whole_of("reader 2", Answer::Ready).unwrap_err().to_string(),
        "reader 2: answered Ready to a whole-file go"
    );
    assert!(ended("burner 0", std::process::ExitStatus::from_raw(0)).is_ok());
    assert_eq!(
        ended("burner 0", std::process::ExitStatus::from_raw(2 << 8))
            .unwrap_err()
            .to_string(),
        "burner 0: ended with exit status: 2"
    );
}

/// The samples count the processes in D state and the requests waiting on the connection, the
/// mean and the most of each; when the connection is not listed there is no waiting figure.
#[test]
fn the_samples_hold_the_mean_and_the_most_of_d_state_and_waiting() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-run-samples");
    let host = Host {
        proc: dir.path().join("proc"),
        sys: dir.path().join("sys"),
    };
    put(&host.proc, "7/stat", "7 (cc) D 1");
    put(&host.sys, "fs/fuse/connections/40/waiting", "9\n");
    let samples = sample(&host, 40, Duration::ZERO, &|taken| taken == 3);
    assert_eq!(
        (
            samples.count,
            samples.d_state_mean,
            samples.d_state_max,
            samples.waiting_mean,
            samples.waiting_max
        ),
        (3, 1.0, 1, Some(9.0), Some(9))
    );
    let unlisted = sample(&host, 41, Duration::ZERO, &|_| true);
    assert_eq!(
        (unlisted.count, unlisted.waiting_mean, unlisted.waiting_max),
        (1, None, None)
    );
}

/// The readers' cycles add up: counts, errors, the first error met, the histograms of each
/// call merged, and the rate over the wall time.
#[test]
fn the_cycles_of_the_readers_add_up() {
    let counts = |count: u64, errors, first: Option<&str>, latency| {
        let mut h = Histogram::default();
        for _ in 0..count {
            h.add(latency);
        }
        let h = h.sealed();
        CycleCounts {
            count,
            errors,
            first_error: first.map(str::to_owned),
            stat: h.clone(),
            open: h.clone(),
            read: h.clone(),
            close: h,
        }
    };
    let total = merge(
        vec![
            counts(30, 0, None, 1000),
            counts(10, 2, Some("open x: gone"), 5000),
            counts(0, 1, Some("stat y: gone"), 0),
        ],
        2_000_000_000,
    );
    assert_eq!((total.count, total.errors), (40, 3));
    assert_eq!(total.first_error.as_deref(), Some("open x: gone"));
    assert_eq!(total.per_s, 20.0);
    for h in [&total.stat, &total.open, &total.read, &total.close] {
        assert_eq!((h.count, h.max), (40, 5000));
    }
    let empty = merge(Vec::new(), 0);
    assert_eq!((empty.count, empty.per_s), (0, 0.0));
}

/// The whole-file reads add up, and their rate is in 10^6 bytes per second of wall time.
#[test]
fn the_whole_file_reads_add_up_to_a_rate() {
    let t = throughput(
        &[
            WholeCounts {
                files: 2,
                bytes: 3_000_000,
                errors: 1,
            },
            WholeCounts {
                files: 1,
                bytes: 1_000_000,
                errors: 0,
            },
        ],
        500_000_000,
    );
    assert_eq!(
        (t.files, t.bytes, t.errors, t.wall_ns, t.mb_per_s),
        (3, 4_000_000, 1, 500_000_000, 8.0)
    );
}

/// The CPU of a run is the difference of the two readings, per 1000 cycles; a control group
/// read at one end only is left out; a count that went down reads as none spent.
#[test]
fn the_cpu_of_a_run_is_the_difference_per_thousand_cycles() {
    let at = |user_ms, system_ms, cgroup: Option<(u64, u64)>, busy_ms| CpuAt {
        server: ProcessCpu { user_ms, system_ms },
        cgroup: cgroup.map(|(user_us, system_us)| CgroupCpu { user_us, system_us }),
        node: NodeCpu {
            user_ms: busy_ms / 2,
            system_ms: busy_ms / 4,
            busy_ms,
        },
    };
    let cpu = cpu_between(
        &at(100, 50, Some((0, 1000)), 10_000),
        &at(300, 150, Some((5000, 401_000)), 50_000),
        2000,
    );
    assert_eq!(
        (cpu.server, cpu.cgroup, cpu.node.busy_ms),
        (
            ProcessCpu {
                user_ms: 200,
                system_ms: 100
            },
            Some(CgroupCpu {
                user_us: 5000,
                system_us: 400_000
            }),
            40_000
        )
    );
    assert_eq!(
        (
            cpu.server_ms_per_1k,
            cpu.cgroup_system_ms_per_1k,
            cpu.node_busy_ms_per_1k
        ),
        (150.0, Some(200.0), 20_000.0)
    );
    let one_end = cpu_between(&at(0, 0, None, 0), &at(0, 0, Some((1, 1)), 0), 0);
    assert_eq!(
        (one_end.cgroup, one_end.cgroup_system_ms_per_1k),
        (None, None)
    );
    let backwards = cpu_between(&at(5, 5, None, 9), &at(1, 1, None, 1), 1);
    assert_eq!(
        (backwards.server_ms_per_1k, backwards.node_busy_ms_per_1k),
        (0.0, 0.0)
    );
}

fn plan(dir: &Path) -> Plan {
    Plan {
        exe: dir.join("no-such-exe"),
        list: dir.join("list"),
        tree: dir.to_path_buf(),
        variant: "v".to_owned(),
        regime: "r".to_owned(),
        runs: 1,
        readers: 1,
        burners: 0,
        cycle_ms: 0,
        max_cycles: 1,
        read_bytes: 1,
        throughput_budget: 0,
        throughput_readers: 1,
        sample_every: Duration::ZERO,
        server_pid: std::process::id(),
        as_user: None,
        settings: BTreeMap::new(),
        host: Host::real(),
    }
}

/// A plan with no run, no reader, or more whole-file readers than readers is refused before
/// anything starts; so is one whose list cannot be read, or whose readers cannot be started.
#[test]
fn a_plan_that_cannot_run_is_refused_before_anything_starts() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-run-refused");
    let refused = "a run needs at least one run and one reader, and from one whole-file reader to as many as there are readers";
    for change in [
        |p: &mut Plan| p.runs = 0,
        |p: &mut Plan| p.readers = 0,
        |p: &mut Plan| p.throughput_readers = 2,
        |p: &mut Plan| p.throughput_readers = 0,
    ] {
        let mut p = plan(dir.path());
        change(&mut p);
        let error = run(&p, std::io::sink(), std::io::sink()).unwrap_err();
        assert_eq!(error.to_string(), refused);
    }
    let error = run(&plan(dir.path()), std::io::sink(), std::io::sink()).unwrap_err();
    assert!(
        matches!(error, crate::Error::Io { what: "read", .. }),
        "{error}"
    );
    std::fs::write(
        dir.path().join("list"),
        format!("{}\n", crate::list::HEADER),
    )
    .unwrap();
    let error = run(&plan(dir.path()), std::io::sink(), std::io::sink()).unwrap_err();
    let exe: PathBuf = dir.path().join("no-such-exe");
    assert!(
        error
            .to_string()
            .starts_with(&format!("reader 0: start {}: ", exe.display())),
        "{error}"
    );
}

/// The line about a run names the variant, the regime and its numbers.
#[test]
fn the_summary_of_a_run_names_its_numbers() {
    let record = crate::record::example();
    assert_eq!(
        summary(&record),
        "fsbench: null caches-off run 1: 1200 cycles/s (0 errors), open p50 0.00 ms p99 0.00 ms, \
         cold 100 MB/s, warm 400 MB/s, server 2.5 ms per 1k cycles, D 3"
    );
}
