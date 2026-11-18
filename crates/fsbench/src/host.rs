//! What the harness reads of the machine: the processes in D state, the CPU time of a process,
//! of its control group and of the node, the requests the kernel holds for a FUSE connection,
//! and the settings a run is recorded with (kernel, CPUs and their governor, the `fuse` module's
//! parameters, the mount's options). Every path is below the [`Host`]'s `/proc` and `/sys`, so a
//! test reads a tree it wrote.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;

/// Where `/proc` and `/sys` are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    /// `/proc`.
    pub proc: PathBuf,
    /// `/sys`.
    pub sys: PathBuf,
}

/// The CPU time of a process, in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCpu {
    /// `utime`.
    pub user_ms: u64,
    /// `stime`.
    pub system_ms: u64,
}

/// The CPU time of a control group (`cpu.stat`), in microseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CgroupCpu {
    /// `user_usec`.
    pub user_us: u64,
    /// `system_usec`.
    pub system_us: u64,
}

/// The CPU time of the whole node since it booted (`/proc/stat`), in milliseconds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCpu {
    /// `user` and `nice`.
    pub user_ms: u64,
    /// `system`, `irq` and `softirq`.
    pub system_ms: u64,
    /// Every field but `idle` and `iowait`.
    pub busy_ms: u64,
}

/// The settings of the machine and of the mount a run is recorded with; what could not be read
/// is left out.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    /// `/proc/sys/kernel/osrelease`.
    pub kernel: Option<String>,
    /// The CPUs online.
    pub cpus: Option<u32>,
    /// The CPU frequency governors, with the number of CPUs on each.
    pub governors: BTreeMap<String, u32>,
    /// `/sys/module/fuse/parameters/*`.
    pub fuse_parameters: BTreeMap<String, String>,
    /// The line of `/proc/self/mountinfo` of the mount the tree is on.
    pub mountinfo: Option<String>,
    /// The `max_background` and `congestion_threshold` of the FUSE connection.
    pub connection: BTreeMap<String, String>,
    /// `/proc/loadavg` when the run started, before its processes: other work on the node shows
    /// here.
    #[serde(default)]
    pub loadavg: Option<String>,
}

/// The text of `path`, trimmed; `None` when it cannot be read.
fn text(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_owned())
}

/// The names of the entries of `dir`, sorted; none when it cannot be read.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// The fields of a `/proc/<pid>/stat` line after the command (from the state on): the command
/// is in parentheses and may hold spaces and parentheses itself.
fn stat_fields(line: &str) -> Vec<&str> {
    line.rsplit_once(')')
        .map(|(_, rest)| rest.split_whitespace().collect())
        .unwrap_or_default()
}

/// The number of CPUs a `cpu/online` list (`0-3,8,10-11`) names; `None` when it is not one.
fn count_cpus(list: &str) -> Option<u32> {
    let mut count = 0;
    for part in list.split(',') {
        let (first, last) = part.split_once('-').unwrap_or((part, part));
        let (first, last): (u32, u32) = (first.parse().ok()?, last.parse().ok()?);
        count += last.checked_sub(first)? + 1;
    }
    Some(count)
}

/// The FUSE connection number of the file system `path` is on: its device number as the kernel
/// writes it (`major << 20 | minor`), the name of its directory in `/sys/fs/fuse/connections`.
///
/// # Errors
/// `path` cannot be read.
pub fn fuse_connection(path: &Path) -> Result<u64, Error> {
    let stat = rustix::fs::stat(path).map_err(|e| Error::io("stat", path, e.into()))?;
    let dev = stat.st_dev;
    Ok((u64::from(rustix::fs::major(dev)) << 20) | u64::from(rustix::fs::minor(dev)))
}

/// The SHA-256 of the file at `path`, read 1 MiB at a time; `None` when it cannot be read.
pub fn file_sha256(path: &Path) -> Option<String> {
    use sha2::Digest as _;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            return Some(hex::encode(hasher.finalize()));
        }
        hasher.update(&buf[..n]);
    }
}

impl Host {
    /// The machine's own `/proc` and `/sys`.
    #[must_use]
    pub fn real() -> Self {
        Self {
            proc: PathBuf::from("/proc"),
            sys: PathBuf::from("/sys"),
        }
    }

    /// The processes in D state (uninterruptible sleep), as `ps -e` shows a process: by the
    /// state of its main thread. A process that ends while it is read is not counted.
    #[must_use]
    pub fn d_state(&self) -> u32 {
        let mut count = 0;
        for name in names(&self.proc) {
            if !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let line = text(&self.proc.join(&name).join("stat")).unwrap_or_default();
            if stat_fields(&line).first() == Some(&"D") {
                count += 1;
            }
        }
        count
    }

    /// The CPU time of process `pid`.
    ///
    /// # Errors
    /// Its `stat` cannot be read (it ended) or is not of the format.
    pub fn process_cpu(&self, pid: u32) -> Result<ProcessCpu, Error> {
        let path = self.proc.join(pid.to_string()).join("stat");
        let line = std::fs::read_to_string(&path).map_err(Error::at("read", &path))?;
        let fields = stat_fields(&line);
        let tick = |i: usize| -> Result<u64, Error> {
            fields
                .get(i)
                .and_then(|f| f.parse::<u64>().ok())
                .ok_or_else(|| Error::Parse {
                    path: path.clone(),
                    line: 1,
                    why: format!("field {} is not a number of clock ticks", i + 3),
                })
        };
        let ms = |ticks: u64| ticks * 1000 / rustix::param::clock_ticks_per_second();
        // `utime` and `stime` are fields 14 and 15; the state, field 3, is the first here.
        Ok(ProcessCpu {
            user_ms: ms(tick(11)?),
            system_ms: ms(tick(12)?),
        })
    }

    /// The CPU time of the control group (version 2) of process `pid`; `None` when the process
    /// is in none or its `cpu.stat` cannot be read.
    #[must_use]
    pub fn cgroup_cpu(&self, pid: u32) -> Option<CgroupCpu> {
        let groups = text(&self.proc.join(pid.to_string()).join("cgroup"))?;
        let group = groups.lines().find_map(|l| l.strip_prefix("0::"))?;
        let stat = text(
            &self
                .sys
                .join("fs/cgroup")
                .join(group.trim_start_matches('/'))
                .join("cpu.stat"),
        )?;
        let field = |name: &str| -> Option<u64> {
            stat.lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.parse().ok())
        };
        Some(CgroupCpu {
            user_us: field("user_usec")?,
            system_us: field("system_usec")?,
        })
    }

    /// The CPU time of the node.
    ///
    /// # Errors
    /// `/proc/stat` cannot be read or has no `cpu` line of numbers.
    pub fn node_cpu(&self) -> Result<NodeCpu, Error> {
        let path = self.proc.join("stat");
        let all = std::fs::read_to_string(&path).map_err(Error::at("read", &path))?;
        let bad = || Error::Parse {
            path: path.clone(),
            line: 1,
            why: "no `cpu` line of eight or more numbers".to_owned(),
        };
        let line = all
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("cpu "))
            .ok_or_else(bad)?;
        let ticks: Vec<u64> = line
            .split_whitespace()
            .map(str::parse)
            .collect::<Result<_, _>>()
            .map_err(|_| bad())?;
        if ticks.len() < 8 {
            return Err(bad());
        }
        let ms = |t: u64| t * 1000 / rustix::param::clock_ticks_per_second();
        // user nice system idle iowait irq softirq steal [guest guest_nice]: guest time is
        // already counted in user and nice.
        let busy = ticks[0] + ticks[1] + ticks[2] + ticks[5] + ticks[6] + ticks[7];
        Ok(NodeCpu {
            user_ms: ms(ticks[0] + ticks[1]),
            system_ms: ms(ticks[2] + ticks[5] + ticks[6]),
            busy_ms: ms(busy),
        })
    }

    /// The requests the kernel holds for FUSE connection `connection` (sent to the server and
    /// not answered, or queued for it); `None` when the connection is not listed (no FUSE, or
    /// `fusectl` not mounted).
    #[must_use]
    pub fn fuse_waiting(&self, connection: u64) -> Option<u64> {
        text(&self.connection_dir(connection).join("waiting"))?
            .parse()
            .ok()
    }

    fn connection_dir(&self, connection: u64) -> PathBuf {
        self.sys
            .join("fs/fuse/connections")
            .join(connection.to_string())
    }

    /// The settings of the machine, and of the mount of FUSE connection (or device) `connection`.
    #[must_use]
    pub fn environment(&self, connection: u64) -> Environment {
        let cpu = self.sys.join("devices/system/cpu");
        let mut governors = BTreeMap::new();
        for name in names(&cpu) {
            if let Some(governor) = text(&cpu.join(&name).join("cpufreq/scaling_governor")) {
                *governors.entry(governor).or_insert(0) += 1;
            }
        }
        let parameters = self.sys.join("module/fuse/parameters");
        let fuse_parameters = names(&parameters)
            .into_iter()
            .filter_map(|name| Some((name.clone(), text(&parameters.join(&name))?)))
            .collect();
        let device = format!("{}:{}", connection >> 20, connection & 0xf_ffff);
        let mountinfo = text(&self.proc.join("self/mountinfo")).and_then(|all| {
            all.lines()
                .find(|l| l.split(' ').nth(2) == Some(device.as_str()))
                .map(str::to_owned)
        });
        let dir = self.connection_dir(connection);
        let connection = ["max_background", "congestion_threshold"]
            .into_iter()
            .filter_map(|name| Some((name.to_owned(), text(&dir.join(name))?)))
            .collect();
        Environment {
            kernel: text(&self.proc.join("sys/kernel/osrelease")),
            cpus: text(&cpu.join("online")).and_then(|l| count_cpus(&l)),
            governors,
            fuse_parameters,
            mountinfo,
            connection,
            loadavg: text(&self.proc.join("loadavg")),
        }
    }
}

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;
