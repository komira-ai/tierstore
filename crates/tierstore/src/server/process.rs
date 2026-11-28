//! The process's own figures on `/metricz`, under the names the standard Prometheus clients give
//! them, so that any tool that reads those reads tierstore's:
//!
//! - `process_resident_memory_bytes` (gauge): the bytes of memory the process has resident now.
//! - `process_virtual_memory_bytes` (gauge): the bytes of its virtual address space now.
//! - `process_start_time_seconds` (gauge): when it started, in seconds since the Unix epoch.
//! - `process_cpu_seconds_total` (counter): the CPU time it has used, user and system together.
//! - `process_open_fds` (gauge): the file descriptors it has open now.
//!
//! On Linux they are read from procfs ([`Source::procfs`]): `self/stat` gives the CPU time
//! (fields 14 and 15, `utime` and `stime`, in clock ticks), the start (field 22, `starttime`, in
//! clock ticks since boot, added to the `btime` of `stat`, a whole second; the first start read is
//! kept, since the kernel moves `btime` with every step of the wall clock), the virtual size (field
//! 23, `vsize`, bytes) and the resident pages (field 24, `rss`, times the size of a page); the
//! descriptors are the entries of `self/fd`, the one the list is read through included, as the
//! standard clients count them. On macOS they are what the kernel's calls for the process give,
//! through `sysinfo` (`proc_pidinfo` of the task: the resident and the virtual size and the CPU
//! time; the start, a whole second; and the list of descriptors). A figure that cannot be read,
//! or reads as zero where a running process cannot have zero (its resident and virtual memory and
//! its start), is left out, never printed as a number it is not. A server that is not given a
//! source ([`Source::None`], as [`Server::new`](super::Server::new) makes) prints none of them.

use std::fmt::Write as _;
#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

/// The figures of the process, each `None` where it could not be read.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct Figures {
    /// `process_resident_memory_bytes`.
    pub resident_bytes: Option<u64>,
    /// `process_virtual_memory_bytes`.
    pub virtual_bytes: Option<u64>,
    /// `process_start_time_seconds`.
    pub start_seconds: Option<f64>,
    /// `process_cpu_seconds_total`.
    pub cpu_seconds: Option<f64>,
    /// `process_open_fds`.
    pub open_fds: Option<u64>,
}

impl Figures {
    /// The lines of `/metricz`: for each figure read, its `# TYPE` line and its sample; nothing
    /// for a figure that was not.
    pub(super) fn render(&self) -> String {
        let mut text = String::new();
        let mut line = |name: &str, kind: &str, value: Option<String>| {
            if let Some(value) = value {
                let _ = write!(text, "# TYPE {name} {kind}\n{name} {value}\n");
            }
        };
        let whole = |value: Option<u64>| value.map(|value| value.to_string());
        let seconds = |value: Option<f64>| value.map(|value| value.to_string());
        line(
            "process_resident_memory_bytes",
            "gauge",
            whole(self.resident_bytes),
        );
        line(
            "process_virtual_memory_bytes",
            "gauge",
            whole(self.virtual_bytes),
        );
        line(
            "process_start_time_seconds",
            "gauge",
            seconds(self.start_seconds),
        );
        line(
            "process_cpu_seconds_total",
            "counter",
            seconds(self.cpu_seconds),
        );
        line("process_open_fds", "gauge", whole(self.open_fds));
        text
    }
}

/// Where a server reads the figures of its process from.
#[derive(Debug, Clone)]
pub(super) enum Source {
    /// Nowhere: `/metricz` has no `process_` line.
    None,
    /// The procfs at `root` (`/proc` for the process itself), with the clock ticks of a second
    /// and the bytes of a page, which its counts are in.
    #[cfg(target_os = "linux")]
    Procfs {
        /// The procfs: its `stat` and `self/` are read.
        root: PathBuf,
        /// Clock ticks in a second (`sysconf(_SC_CLK_TCK)`).
        ticks: u64,
        /// Bytes in a page.
        page: u64,
        /// The start of the process, as first read: kept, so that a step of the wall clock,
        /// which moves `btime`, does not move it.
        start: OnceLock<f64>,
    },
    /// The calls of macOS for this process.
    #[cfg(target_os = "macos")]
    Darwin,
}

impl Source {
    /// The figures of the process this runs in: procfs at `/proc` on Linux.
    #[cfg(target_os = "linux")]
    pub(super) fn own() -> Self {
        Self::procfs(Path::new("/proc"))
    }

    /// The figures of the process this runs in: the calls of macOS.
    #[cfg(target_os = "macos")]
    pub(super) fn own() -> Self {
        Self::Darwin
    }

    /// The figures of the process this runs in: none, on a system this reads none of.
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(super) fn own() -> Self {
        Self::None
    }

    /// The procfs at `root`, with this system's clock ticks and page size.
    #[cfg(target_os = "linux")]
    pub(super) fn procfs(root: &Path) -> Self {
        Self::Procfs {
            root: root.to_path_buf(),
            ticks: rustix::param::clock_ticks_per_second(),
            page: rustix::param::page_size() as u64,
            start: OnceLock::new(),
        }
    }

    /// The figures now.
    pub(super) fn read(&self) -> Figures {
        match self {
            Self::None => Figures::default(),
            #[cfg(target_os = "linux")]
            Self::Procfs {
                root,
                ticks,
                page,
                start,
            } => {
                let mut figures = procfs(root, *ticks, *page);
                if let Some(read) = figures.start_seconds {
                    figures.start_seconds = Some(*start.get_or_init(|| read));
                }
                figures
            }
            #[cfg(target_os = "macos")]
            Self::Darwin => darwin(),
        }
    }
}

/// The fields of `self/stat` this reads, each `None` where it is missing or not a number.
#[cfg(target_os = "linux")]
#[derive(Debug, Default, PartialEq)]
pub(super) struct Stat {
    /// Field 14, `utime`, clock ticks.
    pub user_ticks: Option<u64>,
    /// Field 15, `stime`, clock ticks.
    pub system_ticks: Option<u64>,
    /// Field 22, `starttime`, clock ticks after boot.
    pub start_ticks: Option<u64>,
    /// Field 23, `vsize`, bytes.
    pub virtual_bytes: Option<u64>,
    /// Field 24, `rss`, pages.
    pub resident_pages: Option<u64>,
}

/// The fields of the text of `self/stat`. The second field, the command's name in parentheses,
/// may hold spaces and parentheses itself, so the fields are counted from the last `)`: the
/// third field (`state`) is the first word after it. A text with no `)` has none.
#[cfg(target_os = "linux")]
pub(super) fn parse_stat(text: &str) -> Stat {
    let Some((_, rest)) = text.rsplit_once(')') else {
        return Stat::default();
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    // Field n (from 1) is word n - 3 after the name.
    let field = |n: usize| words.get(n - 3).and_then(|word| word.parse::<u64>().ok());
    Stat {
        user_ticks: field(14),
        system_ticks: field(15),
        start_ticks: field(22),
        virtual_bytes: field(23),
        resident_pages: field(24),
    }
}

/// The `btime` of the text of procfs's `stat`: the time of the boot, in whole seconds since the
/// Unix epoch.
#[cfg(target_os = "linux")]
pub(super) fn parse_boot_time(text: &str) -> Option<u64> {
    text.lines()
        .find_map(|line| line.strip_prefix("btime "))
        .and_then(|value| value.trim().parse().ok())
}

/// The figures that the stat of the process (`stat`), the boot time (`boot`), the count of its
/// descriptors (`fds`), the clock ticks of a second and the bytes of a page make.
#[cfg(target_os = "linux")]
pub(super) fn figures_of(
    stat: &Stat,
    boot: Option<u64>,
    fds: Option<u64>,
    ticks: u64,
    page: u64,
) -> Figures {
    let in_seconds = |count: u64| (ticks > 0).then(|| count as f64 / ticks as f64);
    let cpu = stat
        .user_ticks
        .zip(stat.system_ticks)
        .and_then(|(user, system)| user.checked_add(system));
    let start = boot
        .filter(|boot| *boot > 0)
        .zip(stat.start_ticks.and_then(in_seconds))
        .map(|(boot, after)| boot as f64 + after);
    let resident = stat
        .resident_pages
        .and_then(|pages| pages.checked_mul(page));
    Figures {
        resident_bytes: resident.filter(|bytes| *bytes > 0),
        virtual_bytes: stat.virtual_bytes.filter(|bytes| *bytes > 0),
        start_seconds: start,
        cpu_seconds: cpu.and_then(in_seconds),
        open_fds: fds,
    }
}

/// The figures procfs at `root` gives of `self`.
#[cfg(target_os = "linux")]
fn procfs(root: &Path, ticks: u64, page: u64) -> Figures {
    let stat = std::fs::read_to_string(root.join("self/stat"))
        .map(|text| parse_stat(&text))
        .unwrap_or_default();
    let boot = std::fs::read_to_string(root.join("stat"))
        .ok()
        .and_then(|text| parse_boot_time(&text));
    let fds = std::fs::read_dir(root.join("self/fd"))
        .and_then(|entries| entries.collect::<Result<Vec<_>, _>>())
        .ok()
        .map(|entries| entries.len() as u64);
    figures_of(&stat, boot, fds, ticks, page)
}

/// The figures the calls of macOS give of this process (module docs).
#[cfg(target_os = "macos")]
fn darwin() -> Figures {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, get_current_pid};
    let Ok(pid) = get_current_pid() else {
        return Figures::default();
    };
    let mut system = System::new();
    let kind = ProcessRefreshKind::nothing().with_memory().with_cpu();
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&[pid]), false, kind);
    let Some(process) = system.process(pid) else {
        return Figures::default();
    };
    let positive = |value: u64| (value > 0).then_some(value);
    // The task's information gives the memory and the CPU time together: a resident size of zero
    // is a call that failed, and its CPU time is then left out too.
    let task = positive(process.memory()).is_some();
    Figures {
        resident_bytes: positive(process.memory()),
        virtual_bytes: positive(process.virtual_memory()),
        start_seconds: positive(process.start_time()).map(|seconds| seconds as f64),
        cpu_seconds: task.then(|| process.accumulated_cpu_time() as f64 / 1000.0),
        open_fds: process.open_files().map(|count| count as u64),
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
