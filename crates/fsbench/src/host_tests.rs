use std::path::Path;

use tierstore_testkit::dir::TestDir;

use super::{
    CgroupCpu, Host, NodeCpu, ProcessCpu, count_cpus, file_sha256, fuse_connection, stat_fields,
};
use crate::Error;

fn put(root: &Path, path: &str, text: &str) {
    let at = root.join(path);
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    std::fs::write(at, text).unwrap();
}

/// A `/proc` and `/sys` of three processes (one in D state, one with a command that holds a
/// space and a parenthesis), a control group, two CPUs on one governor and one with none, the
/// `fuse` module's parameters and one FUSE connection mounted at device 0:61.
fn fake(dir: &TestDir) -> Host {
    let host = Host {
        proc: dir.path().join("proc"),
        sys: dir.path().join("sys"),
    };
    let (p, s) = (&host.proc, &host.sys);
    let ticks = rustix::param::clock_ticks_per_second();
    put(
        p,
        "10/stat",
        &format!(
            "10 (tierstore) S 1 10 10 0 -1 0 0 0 0 0 {} {} 0 0",
            3 * ticks,
            ticks / 2
        ),
    );
    put(
        p,
        "11/stat",
        "11 (cc1 (x) y) D 1 11 11 0 -1 0 0 0 0 0 0 0 0 0",
    );
    put(p, "12/stat", "12 (sh) R 1 12 12 0 -1 0 0 0 0 0 0 0 0 0");
    put(p, "13/stat", "13 (short) S 1");
    put(p, "self/stat", "1 (not a pid) D");
    put(p, "10/cgroup", "0::/system.slice/bench.service\n");
    put(p, "12/cgroup", "1:name=other:/x\n");
    put(p, "13/cgroup", "0::/gone.service\n");
    put(
        s,
        "fs/cgroup/system.slice/bench.service/cpu.stat",
        "usage_usec 9\nuser_usec 7\nsystem_usec 2\nnr_periods 0\n",
    );
    put(
        p,
        "stat",
        &format!(
            "cpu  {t} {t} {t} {t} {t} {t} {t} {t} {t} {t}\ncpu0 1 1 1 1 1 1 1 1\n",
            t = ticks
        ),
    );
    put(p, "sys/kernel/osrelease", "7.0.0-test\n");
    put(p, "loadavg", "0.52 1.10 3.00 2/1500 4242\n");
    put(s, "devices/system/cpu/online", "0-1,3\n");
    put(
        s,
        "devices/system/cpu/cpu0/cpufreq/scaling_governor",
        "schedutil\n",
    );
    put(
        s,
        "devices/system/cpu/cpu1/cpufreq/scaling_governor",
        "schedutil\n",
    );
    std::fs::create_dir_all(s.join("devices/system/cpu/cpu3")).unwrap();
    put(s, "module/fuse/parameters/max_user_bgreq", "1024\n");
    put(s, "module/fuse/parameters/enable_uring", "N\n");
    put(s, "fs/fuse/connections/61/waiting", "17\n");
    put(s, "fs/fuse/connections/61/max_background", "12\n");
    put(s, "fs/fuse/connections/61/congestion_threshold", "9\n");
    put(
        p,
        "self/mountinfo",
        "22 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n\
         80 22 0:61 / /run/fsbench/mnt rw,nosuid,nodev,relatime - fuse.fsbench-null fsbench-null rw,user_id=0,group_id=0,max_read=131072\n",
    );
    host
}

/// D state is counted by the state of each process (a command with spaces and parentheses
/// read right), and an entry of `/proc` that is not a process is passed over.
#[test]
fn processes_in_d_state_are_counted_from_their_stat() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-host-d");
    assert_eq!(fake(&dir).d_state(), 1);
    assert_eq!(stat_fields("5 (a) b) R 1"), ["R", "1"]);
    assert!(stat_fields("no parenthesis").is_empty());
}

/// The CPU time of a process is its `utime` and `stime`, in milliseconds; a process that is not
/// there, or whose `stat` is cut short, is an error that names the file.
#[test]
fn the_cpu_time_of_a_process_is_read_in_milliseconds() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-host-cpu");
    let host = fake(&dir);
    assert_eq!(
        host.process_cpu(10).unwrap(),
        ProcessCpu {
            user_ms: 3000,
            system_ms: 500
        }
    );
    assert!(matches!(
        host.process_cpu(99),
        Err(Error::Io { what: "read", .. })
    ));
    let short = host.process_cpu(13).unwrap_err().to_string();
    assert!(
        short.ends_with("13/stat:1: field 14 is not a number of clock ticks"),
        "{short}"
    );
}

/// The control group's `user_usec` and `system_usec`; none for a process in no version-2 group,
/// one whose `cpu.stat` is not there, or one with no `cgroup` file.
#[test]
fn the_cpu_time_of_a_control_group_is_read_from_cpu_stat() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-host-cgroup");
    let host = fake(&dir);
    assert_eq!(
        host.cgroup_cpu(10),
        Some(CgroupCpu {
            user_us: 7,
            system_us: 2
        })
    );
    assert_eq!(host.cgroup_cpu(12), None);
    assert_eq!(host.cgroup_cpu(13), None);
    assert_eq!(host.cgroup_cpu(99), None);
}

/// The node's CPU time from the `cpu` line of `/proc/stat`: user and nice, system and the
/// interrupts, and every busy field (guest time is not counted twice); a `/proc/stat` with no
/// such line, or with too few numbers, or none at all, is an error.
#[test]
fn the_cpu_time_of_the_node_is_read_from_proc_stat() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-host-node");
    let host = fake(&dir);
    assert_eq!(
        host.node_cpu().unwrap(),
        NodeCpu {
            user_ms: 2000,
            system_ms: 3000,
            busy_ms: 6000
        }
    );
    for bad in ["intr 1 2 3\n", "cpu  1 2 3\n", "cpu  1 x 3 4 5 6 7 8\n"] {
        put(&host.proc, "stat", bad);
        let error = host.node_cpu().unwrap_err().to_string();
        assert!(
            error.ends_with("stat:1: no `cpu` line of eight or more numbers"),
            "{error}"
        );
    }
    std::fs::remove_file(host.proc.join("stat")).unwrap();
    assert!(matches!(
        host.node_cpu(),
        Err(Error::Io { what: "read", .. })
    ));
}

/// The requests waiting on a connection, and none for a connection that is not listed or whose
/// count is not a number.
#[test]
fn the_requests_waiting_on_a_connection_are_read_from_fusectl() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-host-waiting");
    let host = fake(&dir);
    assert_eq!(host.fuse_waiting(61), Some(17));
    assert_eq!(host.fuse_waiting(62), None);
    put(&host.sys, "fs/fuse/connections/63/waiting", "many\n");
    assert_eq!(host.fuse_waiting(63), None);
}

/// The environment of a run: the kernel, the CPUs online and their governors, the `fuse`
/// parameters, the mountinfo line of the connection's device and the connection's limits; an
/// empty machine gives an environment with nothing in it.
#[test]
fn the_environment_names_the_kernel_cpus_fuse_and_the_mount() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-host-env");
    let env = fake(&dir).environment(61);
    assert_eq!(env.kernel.as_deref(), Some("7.0.0-test"));
    assert_eq!(env.loadavg.as_deref(), Some("0.52 1.10 3.00 2/1500 4242"));
    assert_eq!(env.cpus, Some(3));
    assert_eq!(
        env.governors.into_iter().collect::<Vec<_>>(),
        [("schedutil".to_owned(), 2)]
    );
    assert_eq!(
        env.fuse_parameters.into_iter().collect::<Vec<_>>(),
        [
            ("enable_uring".to_owned(), "N".to_owned()),
            ("max_user_bgreq".to_owned(), "1024".to_owned())
        ]
    );
    assert!(
        env.mountinfo
            .unwrap()
            .starts_with("80 22 0:61 / /run/fsbench/mnt rw,nosuid")
    );
    assert_eq!(
        env.connection.into_iter().collect::<Vec<_>>(),
        [
            ("congestion_threshold".to_owned(), "9".to_owned()),
            ("max_background".to_owned(), "12".to_owned())
        ]
    );
    let empty = TestDir::under(std::env::temp_dir(), "fsbench-host-empty");
    let none = Host {
        proc: empty.path().join("proc"),
        sys: empty.path().join("sys"),
    }
    .environment(1 << 20);
    assert_eq!(none, super::Environment::default());
}

/// A CPU list counts each single CPU and each range; anything else is not a count.
#[test]
fn a_cpu_list_is_counted_by_its_ranges() {
    assert_eq!(count_cpus("0-87"), Some(88));
    assert_eq!(count_cpus("0,2,4-5"), Some(4));
    assert_eq!(count_cpus("3-1"), None);
    assert_eq!(count_cpus("x"), None);
    assert_eq!(count_cpus("1-y"), None);
}

/// The connection of a directory is its device number in the kernel's encoding, and a path
/// that is not there is an error; a file's SHA-256 is read in pieces, and none for a file that
/// is not there.
#[test]
fn a_connection_number_and_a_file_digest_are_read_from_the_file_system() {
    let dir = TestDir::under(std::env::temp_dir(), "fsbench-host-dev");
    let stat = rustix::fs::stat(dir.path()).unwrap();
    let want = (u64::from(rustix::fs::major(stat.st_dev)) << 20)
        | u64::from(rustix::fs::minor(stat.st_dev));
    assert_eq!(fuse_connection(dir.path()).unwrap(), want);
    assert!(matches!(
        fuse_connection(&dir.path().join("missing")),
        Err(Error::Io { what: "stat", .. })
    ));
    let file = dir.path().join("blob");
    std::fs::write(&file, vec![1u8; (1 << 20) + 5]).unwrap();
    assert_eq!(
        file_sha256(&file).unwrap(),
        crate::list::sha256(&vec![1u8; (1 << 20) + 5])
    );
    assert_eq!(file_sha256(&dir.path().join("missing")), None);
    assert_eq!(Host::real().proc, Path::new("/proc"));
}
