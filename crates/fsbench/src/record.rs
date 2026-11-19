//! The record of one run: one JSON object per line of a results file, format 2.
//!
//! A record holds what was run (the variant, the regime, the fixed inputs and the settings the
//! operator declared), where (the environment the harness read), and what was measured: the
//! cycles of the readers with a latency histogram per call, the cold and warm whole-file reads
//! with the CPU time the server and its control group spent on each, the CPU time of the server,
//! its control group and the node over the cycles, and the samples of D state and of the FUSE
//! requests waiting. A later format only adds fields, so a reader of format 1 reads its records:
//! format 2 added [`Input::throughput_rounds`] (1 in a record of format 1) and
//! [`Throughput::cpu`] (none in a record of format 1).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::hist::Histogram;
use crate::host::{CgroupCpu, Environment, NodeCpu, ProcessCpu};

/// The format of the records this harness writes.
pub const FORMAT: u32 = 2;

/// One run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// [`FORMAT`].
    pub format: u32,
    /// What served the files (`v1`, `v2`, `null`): a label of the operator's.
    pub variant: String,
    /// The kernel's caching of entries and attributes and the open mode (`caches-on`,
    /// `caches-off`, `no-open`): a label of the operator's.
    pub regime: String,
    /// The run's number in its series, from 0; `report` leaves out the first runs.
    pub run: u32,
    /// What the operator declared of the server (`server`, `library`, `entry_timeout`, ...).
    pub settings: BTreeMap<String, String>,
    /// The fixed inputs.
    pub input: Input,
    /// The machine and the mount.
    pub env: Environment,
    /// The server process.
    pub server: Server,
    /// The open/read/close cycles.
    pub cycles: Cycles,
    /// The whole-file reads after the cache of each file was dropped.
    pub cold: Throughput,
    /// The same reads again, at once after.
    pub warm: Throughput,
    /// The CPU time over the cycles.
    pub cpu: Cpu,
    /// The samples taken during the cycles.
    pub samples: Samples,
}

/// The fixed inputs of a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    /// The SHA-256 of the file list.
    pub list_sha256: String,
    /// The files of the list.
    pub files: u64,
    /// The bytes of the files of the list.
    pub bytes: u64,
    /// The reader processes of the cycles.
    pub readers: u32,
    /// The CPU burners beside them.
    pub burners: u32,
    /// How long the cycles last, in milliseconds.
    pub cycle_ms: u64,
    /// The most cycles a reader makes (0: no limit, the time alone ends them).
    pub max_cycles: u64,
    /// The bytes a cycle reads from the start of its file.
    pub read_bytes: u32,
    /// The bytes of the largest files the whole-file reads take.
    pub throughput_budget: u64,
    /// The reader processes of the whole-file reads.
    pub throughput_readers: u32,
    /// How many times each whole-file phase reads its files (1 in a record of format 1).
    #[serde(default = "one")]
    pub throughput_rounds: u32,
}

/// The rounds of a record that does not say.
fn one() -> u32 {
    1
}

/// The process that serves the mount.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    /// Its process id.
    pub pid: u32,
    /// The SHA-256 of its executable, when it could be read.
    pub exe_sha256: Option<String>,
}

/// What the readers' cycles did.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cycles {
    /// The cycles that ended without an error.
    pub count: u64,
    /// The cycles a call of failed.
    pub errors: u64,
    /// The first error a reader met, with its path.
    pub first_error: Option<String>,
    /// From the go to the last reader's answer, in nanoseconds.
    pub wall_ns: u64,
    /// `count` per second of `wall_ns`.
    pub per_s: f64,
    /// The latency of `stat` (the kernel's `GETATTR` or `LOOKUP` when it is not cached).
    pub stat: Histogram,
    /// The latency of `open` (`OPEN`, unless the server took no-open).
    pub open: Histogram,
    /// The latency of the read of the first bytes.
    pub read: Histogram,
    /// The latency of `close` (`FLUSH`; the `RELEASE` is not waited for).
    pub close: Histogram,
}

/// A phase of whole-file reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Throughput {
    /// The files read whole.
    pub files: u64,
    /// The bytes read.
    pub bytes: u64,
    /// The files that failed.
    pub errors: u64,
    /// From the go to the last reader's answer, in nanoseconds.
    pub wall_ns: u64,
    /// `bytes` per second of `wall_ns`, in units of 10^6 bytes.
    pub mb_per_s: f64,
    /// The CPU time the server and its control group spent during the phase (none in a record
    /// of format 1).
    #[serde(default)]
    pub cpu: Option<PhaseCpu>,
}

/// The CPU time spent during a phase of whole-file reads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhaseCpu {
    /// The server process's own (`utime`, `stime`).
    pub server: ProcessCpu,
    /// The server's control group's, when it is in one.
    pub cgroup: Option<CgroupCpu>,
    /// The server's user and system milliseconds per 10^9 bytes read.
    pub server_ms_per_gb: f64,
    /// The control group's system (kernel) milliseconds per 10^9 bytes read.
    pub cgroup_system_ms_per_gb: Option<f64>,
}

/// The CPU time spent during the cycles.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cpu {
    /// The server process's own (`utime`, `stime`).
    pub server: ProcessCpu,
    /// The server's control group's, when it is in one.
    pub cgroup: Option<CgroupCpu>,
    /// The whole node's.
    pub node: NodeCpu,
    /// The server's user and system milliseconds per 1000 cycles.
    pub server_ms_per_1k: f64,
    /// The control group's system (kernel) milliseconds per 1000 cycles.
    pub cgroup_system_ms_per_1k: Option<f64>,
    /// The node's busy milliseconds per 1000 cycles (the burners included).
    pub node_busy_ms_per_1k: f64,
}

/// The samples of the machine during the cycles, one per interval.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Samples {
    /// How many were taken.
    pub count: u64,
    /// The mean of the processes in D state.
    pub d_state_mean: f64,
    /// The most processes in D state in one sample.
    pub d_state_max: u32,
    /// The mean of the requests waiting on the connection, when it is listed.
    pub waiting_mean: Option<f64>,
    /// The most requests waiting in one sample.
    pub waiting_max: Option<u64>,
}

/// A record of a run of the null file system with round numbers, for the tests of what reads
/// records.
#[cfg(test)]
pub(crate) fn example() -> RunRecord {
    let latency = |ns: u64| {
        let mut h = Histogram::default();
        h.add(ns);
        h.sealed()
    };
    RunRecord {
        format: FORMAT,
        variant: "null".to_owned(),
        regime: "caches-off".to_owned(),
        run: 1,
        settings: [("open".to_owned(), "explicit".to_owned())].into(),
        input: Input {
            list_sha256: "ab".repeat(32),
            files: 3,
            bytes: 300,
            readers: 2,
            burners: 1,
            cycle_ms: 1000,
            max_cycles: 0,
            read_bytes: 4096,
            throughput_budget: 300,
            throughput_readers: 1,
            throughput_rounds: 1,
        },
        env: Environment {
            kernel: Some("7.0.0".to_owned()),
            cpus: Some(88),
            governors: [("schedutil".to_owned(), 88)].into(),
            fuse_parameters: [("enable_uring".to_owned(), "N".to_owned())].into(),
            mountinfo: Some("80 22 0:61 / /mnt rw - fuse.fsbench-null fsbench-null rw".to_owned()),
            connection: [("max_background".to_owned(), "1024".to_owned())].into(),
            loadavg: Some("0.10 0.20 0.30 1/900 77".to_owned()),
        },
        server: Server {
            pid: 42,
            exe_sha256: None,
        },
        cycles: Cycles {
            count: 1200,
            errors: 0,
            first_error: None,
            wall_ns: 1_000_000_000,
            per_s: 1200.0,
            stat: latency(1000),
            open: latency(2000),
            read: latency(3000),
            close: latency(500),
        },
        cold: Throughput {
            files: 3,
            bytes: 300,
            errors: 0,
            wall_ns: 3,
            mb_per_s: 100.0,
            cpu: None,
        },
        warm: Throughput {
            files: 3,
            bytes: 300,
            errors: 0,
            wall_ns: 1,
            mb_per_s: 400.0,
            cpu: None,
        },
        cpu: Cpu {
            server: ProcessCpu {
                user_ms: 2,
                system_ms: 1,
            },
            cgroup: Some(CgroupCpu {
                user_us: 2000,
                system_us: 1000,
            }),
            node: NodeCpu {
                user_ms: 80,
                system_ms: 10,
                busy_ms: 100,
            },
            server_ms_per_1k: 2.5,
            cgroup_system_ms_per_1k: Some(0.83),
            node_busy_ms_per_1k: 83.3,
        },
        samples: Samples {
            count: 1,
            d_state_mean: 3.0,
            d_state_max: 3,
            waiting_mean: Some(5.0),
            waiting_max: Some(5),
        },
    }
}
