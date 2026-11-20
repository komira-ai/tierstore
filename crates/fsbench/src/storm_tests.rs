//! The plan of a storm: its size, its CPUs and its phases. A run over a real mount is
//! `tests/storm_integration.rs`.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{
    Gate, Plan, StormShared, allowed_of, cpu_list, online_cpus, other_cpu, phases, pin,
    probe_thread, probes_done, raise_open_files, storm_per_cpu, storm_thread, usable_cpus,
};
use crate::host::Host;
use crate::stormfs::ColdRead;
use crate::tsnull::Transport;

fn plan(cpus: Vec<usize>, storm_cpu: usize, other: Option<usize>, per_cpu: u32) -> Plan {
    Plan {
        mountpoint: PathBuf::from("/mnt"),
        label: "x".to_owned(),
        runs: 1,
        transport: Transport::Dev,
        readers: NonZeroUsize::MIN,
        park: Duration::from_millis(60),
        read: ColdRead::Buffered,
        per_cpu,
        cpus,
        storm_cpu,
        other_cpu: other,
        window: Duration::from_secs(1),
        max_probes: 0,
        sample_every: Duration::from_millis(100),
        settings: BTreeMap::new(),
        host: Host::real(),
    }
}

/// The storm of a queue of `depth` entries is `ceil(depth x 1.5)` threads: 8 for the default
/// depth of 5.
#[test]
fn the_storm_of_a_queue_is_one_and_a_half_times_its_depth_rounded_up() {
    assert_eq!(storm_per_cpu(5), 8);
    assert_eq!(storm_per_cpu(1), 2);
    assert_eq!(storm_per_cpu(2), 3);
    assert_eq!(storm_per_cpu(16), 24);
    assert_eq!(storm_per_cpu(u16::MAX), 98_303);
}

/// A CPU list as `/sys` writes it reads as its CPUs, ascending and once each; anything else is
/// none.
#[test]
fn a_cpu_list_reads_as_its_cpus() {
    assert_eq!(cpu_list("0-3\n"), Some(vec![0, 1, 2, 3]));
    assert_eq!(cpu_list("8,0-1,1"), Some(vec![0, 1, 8]));
    assert_eq!(cpu_list("5"), Some(vec![5]));
    for bad in ["", "3-1", "a", "1-", "0,,2", "-1"] {
        assert_eq!(cpu_list(bad), None, "{bad:?}");
    }
}

/// The online CPUs are read from the host's `/sys`; a file that is missing or not a list is an
/// error naming it.
#[test]
fn the_online_cpus_are_read_from_sys() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-storm-online");
    let host = Host {
        proc: dir.path().join("proc"),
        sys: dir.path().join("sys"),
    };
    let path = host.sys.join("devices/system/cpu/online");
    let error = online_cpus(&host).unwrap_err().to_string();
    assert!(
        error.starts_with(&format!("read {}: ", path.display())),
        "{error}"
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "0-2,4\n").unwrap();
    assert_eq!(online_cpus(&host).unwrap(), [0, 1, 2, 4]);
    let allowed = rustix::thread::sched_getaffinity(None).unwrap();
    let usable: Vec<usize> = [0, 1, 2, 4]
        .into_iter()
        .filter(|&c| allowed.is_set(c))
        .collect();
    assert_eq!(
        usable_cpus(&host).unwrap(),
        usable,
        "and this process may run on"
    );
    std::fs::write(&path, "two\n").unwrap();
    assert_eq!(
        online_cpus(&host).unwrap_err().to_string(),
        format!("{}:1: not a CPU list: two", path.display())
    );
}

/// The other probe's CPU is the one asked for, or the last online CPU that is not the storm's,
/// or none on a machine of one CPU; a CPU that is not online, or the storm's own, is refused.
#[test]
fn the_other_cpu_is_online_and_not_the_storms() {
    let cpus = [0, 1, 2, 3];
    assert_eq!(other_cpu(&cpus, 0, None).unwrap(), Some(3));
    assert_eq!(other_cpu(&cpus, 3, None).unwrap(), Some(2));
    assert_eq!(other_cpu(&cpus, 0, Some(1)).unwrap(), Some(1));
    assert_eq!(other_cpu(&[0], 0, None).unwrap(), None);
    assert_eq!(
        other_cpu(&cpus, 4, None).unwrap_err().to_string(),
        "CPU 4 is not online"
    );
    assert_eq!(
        other_cpu(&cpus, 0, Some(9)).unwrap_err().to_string(),
        "CPU 9 is not online"
    );
    assert_eq!(
        other_cpu(&cpus, 2, Some(2)).unwrap_err().to_string(),
        "the other probe's CPU 2 is the storm's"
    );
}

/// The phases: idle with no storm; the one-CPU storm of `per_cpu` threads on the storm's CPU,
/// which its probe there feels and the other does not; the every-CPU storm of `per_cpu` threads
/// on each CPU, felt by both probes. With one CPU there is one probe; with no thread per CPU no
/// phase storms anything.
#[test]
fn the_phases_storm_one_cpu_then_every_cpu() {
    let specs = phases(&plan(vec![0, 1, 2], 1, Some(2), 2));
    let summary: Vec<_> = specs
        .iter()
        .map(|s| (s.name, s.storm.clone(), s.stormed_cpus, s.probes.clone()))
        .collect();
    assert_eq!(
        summary,
        [
            (
                "idle",
                vec![],
                0,
                vec![("storm-cpu", 1, false), ("other-cpu", 2, false)]
            ),
            (
                "one-cpu",
                vec![1, 1],
                1,
                vec![("storm-cpu", 1, true), ("other-cpu", 2, false)]
            ),
            (
                "every-cpu",
                vec![0, 0, 1, 1, 2, 2],
                3,
                vec![("storm-cpu", 1, true), ("other-cpu", 2, true)]
            ),
        ]
    );
    let single = phases(&plan(vec![0], 0, None, 3));
    assert_eq!(single[2].storm, [0, 0, 0]);
    assert_eq!(single[2].probes, [("storm-cpu", 0, true)]);
    let none = phases(&plan(vec![0, 1], 0, Some(1), 0));
    assert!(
        none.iter()
            .all(|s| s.storm.is_empty() && s.stormed_cpus == 0)
    );
    assert!(none.iter().all(|s| s.probes.iter().all(|p| !p.2)));
}

/// A thread is pinned to a CPU it may run on; a CPU past the kernel's set is refused with
/// EINVAL.
#[test]
fn a_thread_is_pinned_to_a_cpu_it_may_run_on() {
    let allowed = rustix::thread::sched_getaffinity(None).unwrap();
    let cpu = (0..rustix::thread::CpuSet::MAX_CPU)
        .find(|&c| allowed.is_set(c))
        .unwrap();
    let ran_on = std::thread::spawn(move || {
        pin(cpu).unwrap();
        rustix::thread::sched_getcpu()
    })
    .join()
    .unwrap();
    assert_eq!(ran_on, cpu);
    let error = std::thread::spawn(|| pin(rustix::thread::CpuSet::MAX_CPU))
        .join()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(22));
}

/// A gate lets its waiter through once as many threads have arrived as it waits for.
#[test]
fn a_gate_opens_when_every_thread_has_arrived() {
    let gate = Arc::new(Gate::default());
    gate.wait_for(0);
    let threads: Vec<_> = (0..3)
        .map(|_| {
            let gate = gate.clone();
            std::thread::spawn(move || gate.arrive())
        })
        .collect();
    gate.wait_for(3);
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(*gate.count.lock().unwrap(), 3);
}

/// The limit of open files is raised to the hard limit, and stays there when asked again.
#[test]
fn the_limit_of_open_files_is_raised_to_the_hard_limit() {
    let hard = rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum;
    assert_eq!(raise_open_files().unwrap(), hard);
    assert_eq!(raise_open_files().unwrap(), hard);
}

/// A storm thread on a file it can read counts every read answered: once its first read is in,
/// it arrives at the gate, and it goes on reading (each 1 MiB past the last) until it is
/// stopped, failing none.
#[test]
fn a_storm_thread_counts_each_read_answered_until_it_is_stopped() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-storm-reads");
    let path = dir.path().join("cold");
    // Sparse, and long enough (1 GiB) that the reads waited for are all inside it.
    std::fs::File::create(&path)
        .unwrap()
        .set_len(1 << 30)
        .unwrap();
    let shared = StormShared::default();
    let allowed = rustix::thread::sched_getaffinity(None).unwrap();
    let cpu = (0..rustix::thread::CpuSet::MAX_CPU)
        .find(|&c| allowed.is_set(c))
        .unwrap();
    let wanted = 64;
    std::thread::scope(|scope| {
        scope.spawn(|| storm_thread(&path, cpu, ColdRead::Buffered, &shared));
        shared.started.wait_for(1);
        assert!(shared.reads.load(std::sync::atomic::Ordering::Relaxed) >= 1);
        while shared.reads.load(std::sync::atomic::Ordering::Relaxed) < wanted {
            std::thread::yield_now();
        }
        shared
            .stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let stopped_at = shared.reads.load(std::sync::atomic::Ordering::Relaxed);
    assert!(stopped_at >= wanted, "{stopped_at}");
    assert_eq!(
        shared.failures.into_inner().unwrap(),
        std::collections::BTreeMap::new()
    );
}

/// A storm thread that cannot pin to its CPU, or cannot open its file, counts the failure by
/// step and error and arrives at once; one whose reads fail counts each failure and arrives
/// after its first, and stops when told.
#[test]
fn a_storm_thread_counts_each_failure_by_step_and_error() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-storm-thread");
    let shared = StormShared::default();
    let allowed = rustix::thread::sched_getaffinity(None).unwrap();
    let cpu = (0..rustix::thread::CpuSet::MAX_CPU)
        .find(|&c| allowed.is_set(c))
        .unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            storm_thread(
                &dir.path().join("x"),
                rustix::thread::CpuSet::MAX_CPU,
                ColdRead::Buffered,
                &shared,
            );
        });
        scope.spawn(|| storm_thread(&dir.path().join("missing"), cpu, ColdRead::Direct, &shared));
        shared.started.wait_for(2);
        // A directory opens, and every read of it fails.
        scope.spawn(|| storm_thread(dir.path(), cpu, ColdRead::Buffered, &shared));
        shared.started.wait_for(3);
        shared
            .stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let failures = shared.failures.into_inner().unwrap();
    let keys: Vec<&String> = failures.keys().collect();
    assert_eq!(
        keys,
        [
            "open: No such file or directory (os error 2)",
            "pin: Invalid argument (os error 22)",
            "read: Is a directory (os error 21)",
        ]
    );
    assert_eq!(failures["open: No such file or directory (os error 2)"], 1);
    assert_eq!(failures["pin: Invalid argument (os error 22)"], 1);
    assert!(failures["read: Is a directory (os error 21)"] >= 1);
    assert_eq!(shared.reads.into_inner(), 0);
}

/// A probe makes as many `stat`s as it is let, timing those that succeed and counting those
/// that fail; one that cannot pin to its CPU makes none and counts one failure.
#[test]
fn a_probe_times_its_stats_and_counts_its_failures() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-storm-probe");
    let allowed = rustix::thread::sched_getaffinity(None).unwrap();
    let cpu = (0..rustix::thread::CpuSet::MAX_CPU)
        .find(|&c| allowed.is_set(c))
        .unwrap();
    let clock = crate::clock::Clock::start();
    let (hist, errors) = probe_thread(dir.path(), cpu, clock, &|made| made >= 7);
    assert_eq!((hist.count, errors), (7, 0));
    let (hist, errors) = probe_thread(&dir.path().join("missing"), cpu, clock, &|made| made >= 3);
    assert_eq!((hist.count, errors), (0, 3));
    let (hist, errors) = probe_thread(dir.path(), rustix::thread::CpuSet::MAX_CPU, clock, &|_| {
        false
    });
    assert_eq!((hist.count, errors), (0, 1));
}

/// The CPUs a storm may use are the online CPUs the affinity holds, never one past the kernel's
/// set; with no affinity known, every online CPU.
#[test]
fn the_usable_cpus_are_the_online_ones_the_affinity_holds() {
    let mut allowed = rustix::thread::CpuSet::new();
    allowed.set(1);
    allowed.set(3);
    let max = rustix::thread::CpuSet::MAX_CPU;
    assert_eq!(allowed_of(vec![0, 1, 2, 3, max], Some(&allowed)), [1, 3]);
    assert_eq!(allowed_of(vec![0, 1, max], None), [0, 1, max]);
}

/// A probe has done when it has made its count of `stat`s (a count of 0 is no limit) or its
/// window is over, whichever comes first.
#[test]
fn a_probe_is_done_at_its_count_or_at_the_end_of_its_window() {
    assert!(!probes_done(4, 5, 10, 100));
    assert!(probes_done(5, 5, 10, 100));
    assert!(!probes_done(1_000_000, 0, 10, 100), "0 is no limit");
    assert!(probes_done(0, 0, 100, 100));
    assert!(probes_done(1, 5, 101, 100));
}
