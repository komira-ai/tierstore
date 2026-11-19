//! The plan of a storm: its size, its CPUs and its phases. A run over a real mount is
//! `tests/storm_integration.rs`.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::{
    Gate, Plan, cpu_list, online_cpus, other_cpu, phases, pin, storm_per_cpu, usable_cpus,
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
