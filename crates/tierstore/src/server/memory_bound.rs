//! The bound on the process's anonymous memory (`server.memory_bound`), so that a leak restarts
//! tierstore instead of starving the host.
//!
//! A thread of its own looks at the process's memory once a second: on Linux `RssAnon` of
//! `/proc/self/status`, the resident pages of the heap, the thread stacks and the anonymous
//! mappings, and not the page cache of the files the process reads and writes, nor its mapped
//! files (`RssFile`) or shared memory (`RssShmem`), which the kernel can take back; on macOS the
//! peak resident size of the process (`getrusage`), which never goes down. A cgroup bound
//! (`MemoryMax=`, `MemoryHigh=`) counts the page cache too, which on a cache node is most of the
//! host's memory, so it would throttle or kill the process for doing its work: this one does not.
//!
//! - At the mark, half the bound, the process is degraded: `/memoryz` says 503 and the reason,
//!   `tierstore_memory_degraded` is 1, and stderr says so once. It is no longer degraded once the
//!   memory is back under seven eighths of the mark, so a size about the mark does not flap.
//! - At the bound, stderr says the sizes, the count of exits at the bound in the service's state
//!   directory is raised (`tierstore_memory_bound_exits_total` of the next start), and the process
//!   stops as on SIGTERM: the drain, and on a build node the mount left to the service manager's
//!   descriptor store for the next start. It then exits with [`EXIT_STATUS`], which the units
//!   restart. A drain that has not ended [`CUT_AFTER`] looks after the bound is cut: the process
//!   exits with the same status at once, and the descriptor store keeps the mount, as on a crash.
//!
//! The thread runs only in the binary, once it has its settings: [`install`] in `cli::main`. A
//! bound of 0 bytes (`"0B"`) turns it off, and then `/memoryz` says the memory is not watched.

use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hyper::StatusCode;
use tokio::sync::watch;

use super::config::Settings;

/// The default of `server.memory_bound`: 32 GiB of anonymous memory. A build node in normal work
/// holds 1.2 to 2.0 GiB, and its configured budgets allow at most about 10 GiB
/// (`crates/tierstore/docs/limits.md`, "Memory"), so the mark (16 GiB) is above both and the
/// bound is an eighth of a 252 GiB host.
pub(super) const DEFAULT_BOUND: u64 = 32 << 30;

/// The exit status of a process that reached the bound: `EX_TEMPFAIL`, a failure that a restart
/// mends. It is not 0, 2 or 3, so `Restart=on-failure` with `RestartPreventExitStatus=2 3` starts
/// the process again, and the units name it in `RestartForceExitStatus=` too.
pub(super) const EXIT_STATUS: u8 = 75;

/// How many looks (a second apart) after the bound the drain has before the process exits
/// without it.
pub(super) const CUT_AFTER: u32 = 30;

/// The file in the service's state directory that counts the exits at the bound: a decimal
/// number and a newline.
pub(super) const COUNT_FILE: &str = "memory-bound-exits";

/// How long the thread waits between two looks.
const INTERVAL: Duration = Duration::from_secs(1);

const ANON: &str = "tierstore_memory_anon_bytes";
const BOUND: &str = "tierstore_memory_bound_bytes";
const DEGRADED: &str = "tierstore_memory_degraded";
const EXITS: &str = "tierstore_memory_bound_exits_total";

/// One look at the memory of the process, in bytes: the anonymous resident size, which the bound
/// is held to, and for the line at the bound the resident size of mapped files and of shared
/// memory (both 0 on macOS, where `anon` is the peak resident size).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Sample {
    pub anon: u64,
    pub file: u64,
    pub shmem: u64,
}

/// The sample in the text of a Linux `/proc/<pid>/status`: its `RssAnon`, `RssFile` and
/// `RssShmem` lines, each a number of KiB and `kB`. None without a `RssAnon` line that reads; a
/// missing or unreadable `RssFile` or `RssShmem` is 0.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "the look of Linux; the tests read it on every system"
    )
)]
pub(super) fn sample_of_status(status: &str) -> Option<Sample> {
    let field = |name: &str| -> Option<u64> {
        let rest = status.lines().find_map(|line| line.strip_prefix(name))?;
        let kib: u64 = rest.trim().strip_suffix("kB")?.trim().parse().ok()?;
        kib.checked_mul(1024)
    };
    Some(Sample {
        anon: field("RssAnon:")?,
        file: field("RssFile:").unwrap_or(0),
        shmem: field("RssShmem:").unwrap_or(0),
    })
}

/// The sample in the status file at `path`, or None if it cannot be read or holds none.
#[cfg_attr(
    not(target_os = "linux"),
    allow(
        dead_code,
        reason = "the look of Linux; the tests read it on every system"
    )
)]
pub(super) fn sample_at(path: &Path) -> Option<Sample> {
    let status = std::fs::read_to_string(path).ok()?;
    sample_of_status(&status)
}

/// A look at this process's memory.
#[cfg(target_os = "linux")]
fn look() -> Option<Sample> {
    sample_at(Path::new("/proc/self/status"))
}

/// A look at this process's memory: its peak resident size, in bytes on macOS.
#[cfg(target_os = "macos")]
fn look() -> Option<Sample> {
    use nix::sys::resource::{UsageWho, getrusage};
    let usage = getrusage(UsageWho::RUSAGE_SELF).ok()?;
    let peak = u64::try_from(usage.max_rss()).ok()?;
    Some(Sample {
        anon: peak,
        file: 0,
        shmem: 0,
    })
}

/// No look on another system: the bound is never reached there.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn look() -> Option<Sample> {
    None
}

/// The state of the bound that the server shows and the stop waits on: the bound, the last look,
/// whether the process is degraded or has reached the bound, and the exits at the bound counted.
pub(crate) struct MemoryWatch {
    bound: u64,
    anon: AtomicU64,
    degraded: AtomicBool,
    exits: AtomicU64,
    reached: watch::Sender<bool>,
}

/// What a look leads to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Nothing changed.
    Steady,
    /// The memory could not be read: the look is skipped.
    Blind,
    /// The memory went over the mark.
    Degraded,
    /// The memory came back under seven eighths of the mark.
    Recovered,
    /// The memory reached the bound: the stop begins.
    Reached,
    /// The drain has had its [`CUT_AFTER`] looks: the process is to exit now.
    Cut,
}

impl MemoryWatch {
    /// The watch of `bound` bytes, with `exits` exits at the bound counted before this start.
    pub(super) fn new(bound: u64, exits: u64) -> Self {
        let (reached, _) = watch::channel(false);
        Self {
            bound,
            anon: AtomicU64::new(0),
            degraded: AtomicBool::new(false),
            exits: AtomicU64::new(exits),
            reached,
        }
    }

    /// The bound, in bytes.
    pub(super) fn bound(&self) -> u64 {
        self.bound
    }

    /// The mark, half the bound.
    pub(super) fn mark(&self) -> u64 {
        self.bound / 2
    }

    /// The anonymous memory of the last look, 0 before the first.
    pub(super) fn anon(&self) -> u64 {
        self.anon.load(Ordering::Relaxed)
    }

    /// Whether the process is over the mark, or has reached the bound.
    pub(super) fn degraded(&self) -> bool {
        self.degraded.load(Ordering::Relaxed)
    }

    /// Whether the process has reached the bound.
    pub(super) fn has_reached(&self) -> bool {
        *self.reached.borrow()
    }

    /// The exits at the bound: those counted before this start, and this one's if it reached it.
    pub(super) fn exits(&self) -> u64 {
        self.exits.load(Ordering::Relaxed)
    }

    /// Resolves once the process has reached the bound.
    pub(super) async fn reached(&self) {
        let mut seen = self.reached.subscribe();
        // The sender is this watch's, which outlives the borrow: the wait ends only at the bound.
        let _ = seen.wait_for(|reached| *reached).await;
    }

    /// Takes the look `look` into the watch. `after` is how many looks have been taken since the
    /// bound was reached, None before: once it is reached, every look counts toward the cut and
    /// nothing else changes.
    pub(super) fn observe(&self, look: Option<Sample>, after: &mut Option<u32>) -> Step {
        if let Some(sample) = look {
            self.anon.store(sample.anon, Ordering::Relaxed);
        }
        if let Some(looks) = after {
            *looks += 1;
            return if *looks >= CUT_AFTER {
                Step::Cut
            } else {
                Step::Steady
            };
        }
        let Some(sample) = look else {
            return Step::Blind;
        };
        if sample.anon >= self.bound {
            *after = Some(0);
            self.degraded.store(true, Ordering::Relaxed);
            self.exits.fetch_add(1, Ordering::Relaxed);
            self.reached.send_replace(true);
            return Step::Reached;
        }
        let mark = self.mark();
        let degraded = self.degraded();
        if !degraded && sample.anon >= mark {
            self.degraded.store(true, Ordering::Relaxed);
            return Step::Degraded;
        }
        if degraded && sample.anon < mark - mark / 8 {
            self.degraded.store(false, Ordering::Relaxed);
            return Step::Recovered;
        }
        Step::Steady
    }
}

/// What the thread does after a look.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Tick {
    /// It looks again.
    Again,
    /// The process exits now with [`EXIT_STATUS`].
    Exit,
}

/// The thread's side of the watch: the looks since the bound, where the exits are counted, and
/// whether it said that the memory cannot be read.
pub(super) struct Dog {
    watch: Arc<MemoryWatch>,
    state_dir: Option<PathBuf>,
    after: Option<u32>,
    said_blind: bool,
}

impl Dog {
    pub(super) fn new(watch: Arc<MemoryWatch>, state_dir: Option<PathBuf>) -> Self {
        Self {
            watch,
            state_dir,
            after: None,
            said_blind: false,
        }
    }

    /// Takes `look` into the watch and says on stderr what changed; at the bound it also counts
    /// the exit in the state directory.
    pub(super) fn tick(&mut self, look: Option<Sample>) -> Tick {
        let watch = &self.watch;
        let (bound, mark) = (watch.bound(), watch.mark());
        match watch.observe(look, &mut self.after) {
            Step::Steady => {}
            Step::Blind if self.said_blind => {}
            Step::Blind => {
                self.said_blind = true;
                tracing::warn!(
                    "tierstore: the memory of the process cannot be read; the memory bound of {bound} bytes is not held until it can"
                );
            }
            Step::Degraded => tracing::warn!(
                "tierstore: anonymous memory {} bytes is above the mark of {mark} bytes, half the memory bound of {bound} bytes",
                watch.anon()
            ),
            Step::Recovered => tracing::warn!(
                "tierstore: anonymous memory {} bytes is back under the mark of {mark} bytes",
                watch.anon()
            ),
            Step::Reached => {
                let sample = look.unwrap_or_default();
                tracing::error!(
                    "tierstore: memory bound reached: anonymous memory {} bytes, bound {bound} bytes (resident file {} bytes, shared {} bytes); draining for at most {CUT_AFTER} s, then exiting with status {EXIT_STATUS}",
                    sample.anon,
                    sample.file,
                    sample.shmem
                );
                self.count();
            }
            Step::Cut => {
                tracing::error!(
                    "tierstore: the drain at the memory bound did not end within {CUT_AFTER} s; exiting with status {EXIT_STATUS} now"
                );
                return Tick::Exit;
            }
        }
        Tick::Again
    }

    /// Raises the count of exits at the bound in the state directory, if there is one, from what
    /// the file says now (the other instance of the template may have raised it since this start);
    /// a failure is said and the exit goes on.
    fn count(&self) {
        let Some(dir) = &self.state_dir else {
            return;
        };
        if let Err(why) = write_count(dir, read_count(dir).saturating_add(1)) {
            let file = dir.join(COUNT_FILE);
            tracing::warn!(
                "tierstore: the exit at the memory bound is not counted in {}: {why}",
                file.display()
            );
        }
    }
}

/// The exits at the bound counted in the state directory `dir`: 0 when it has no count, or one
/// that does not read as a number.
pub(super) fn read_count(dir: &Path) -> u64 {
    let text = std::fs::read_to_string(dir.join(COUNT_FILE)).unwrap_or_default();
    text.trim().parse().unwrap_or(0)
}

/// Writes `count` to the count file of `dir`, through a file beside it named by the process and a
/// rename, so that a reader finds the old count or the new one, and two processes do not share
/// the file they write.
pub(super) fn write_count(dir: &Path, count: u64) -> std::io::Result<()> {
    let new = dir.join(format!("{COUNT_FILE}.new.{}", std::process::id()));
    std::fs::write(&new, format!("{count}\n"))?;
    std::fs::rename(&new, dir.join(COUNT_FILE))
}

/// The looks of the thread: `pause`, then a look from `look`, until the cut; it returns at the
/// cut. The binary pauses [`INTERVAL`] and then exits without the end of its drain.
pub(super) fn watch_until_cut(
    mut dog: Dog,
    mut look: impl FnMut() -> Option<Sample>,
    mut pause: impl FnMut(),
) {
    loop {
        pause();
        if dog.tick(look()) == Tick::Exit {
            return;
        }
    }
}

/// The thread of the binary: a look every [`INTERVAL`], the first after one, and at the cut the
/// exit with [`EXIT_STATUS`].
fn watching(dog: Dog) {
    watch_until_cut(dog, look, || std::thread::sleep(INTERVAL));
    std::process::exit(i32::from(EXIT_STATUS));
}

/// The watch of the binary, once [`install`] made it.
static INSTALLED: OnceLock<Arc<MemoryWatch>> = OnceLock::new();

/// The bound of `settings`: `server.memory_bound`, whatever the shape of the node.
pub(super) fn bound_of(settings: &Settings) -> u64 {
    match settings {
        Settings::Storage(cli) => cli.memory_bound,
        Settings::Cache(cache) => cache.front.memory_bound,
        Settings::Combined(combined) => combined.node.memory_bound,
        Settings::Cold(cold) => cold.front.memory_bound,
    }
}

/// Starts the thread that holds the process to `bound` bytes, counting its exits at the bound in
/// `state_dir`; nothing for a bound of 0. Called once, by the binary, before it serves. A thread
/// that cannot be started is said on stderr, and the process serves without the bound.
pub(super) fn install(bound: u64, state_dir: Option<PathBuf>) {
    if bound == 0 {
        return;
    }
    let exits = state_dir.as_deref().map_or(0, read_count);
    let watch = Arc::new(MemoryWatch::new(bound, exits));
    let dog = Dog::new(Arc::clone(&watch), state_dir);
    let started = std::thread::Builder::new()
        .name("memory-bound".to_owned())
        .spawn(move || watching(dog));
    match started {
        // The binary installs once, before it serves; the thread's first look is a second later.
        Ok(_) => {
            let _ = INSTALLED.set(watch);
        }
        Err(why) => tracing::error!(
            "tierstore: the memory bound is not held: its thread did not start: {why}"
        ),
    }
}

/// The watch of the binary, if it has one.
pub(crate) fn installed() -> Option<Arc<MemoryWatch>> {
    INSTALLED.get().cloned()
}

/// `stop`, or the bound of the binary's watch, whichever comes first.
pub(super) fn or_reached(
    stop: impl Future<Output = ()> + Send + 'static,
) -> impl Future<Output = ()> + Send + 'static {
    or_reached_of(stop, installed())
}

/// `stop`, or the bound of `watch`, whichever comes first; `stop` alone without a watch.
pub(super) async fn or_reached_of(
    stop: impl Future<Output = ()> + Send + 'static,
    watch: Option<Arc<MemoryWatch>>,
) {
    match watch {
        Some(watch) => tokio::select! {
            () = stop => {}
            () = watch.reached() => {}
        },
        None => stop.await,
    }
}

/// The exit status of a process whose serving ended with `status`: [`EXIT_STATUS`] once the
/// binary's watch reached the bound, whatever the drain gave.
pub(super) fn status(status: u8) -> u8 {
    status_of(installed().as_deref(), status)
}

/// [`status`], for `watch`.
pub(super) fn status_of(watch: Option<&MemoryWatch>, status: u8) -> u8 {
    if watch.is_some_and(MemoryWatch::has_reached) {
        EXIT_STATUS
    } else {
        status
    }
}

/// The answer of `/memoryz`: 200 and the sizes under the mark, 503 and the reason over it or at
/// the bound, 200 `memory not watched` without a watch. It does not depend on the drain.
pub(super) fn probe(watch: Option<&MemoryWatch>) -> (StatusCode, String) {
    let Some(watch) = watch else {
        return (StatusCode::OK, "memory not watched".to_owned());
    };
    let (anon, mark, bound) = (watch.anon(), watch.mark(), watch.bound());
    let down = StatusCode::SERVICE_UNAVAILABLE;
    if watch.has_reached() {
        let text = format!(
            "anonymous memory reached the bound: {anon} bytes, bound {bound} bytes; draining"
        );
        (down, text)
    } else if watch.degraded() {
        let text = format!(
            "anonymous memory is above the mark: {anon} bytes, mark {mark} bytes, bound {bound} bytes"
        );
        (down, text)
    } else {
        let text = format!("anonymous memory {anon} bytes, mark {mark} bytes, bound {bound} bytes");
        (StatusCode::OK, text)
    }
}

impl super::Server {
    /// The server's own series of `/metricz` after the drain's: the failed puts, then the bound.
    pub(super) fn own_families(&self) -> String {
        let mut text = self.put_failure_family();
        text.push_str(&family(self.config.memory.as_deref()));
        text
    }
}

/// The series of the bound on `/metricz`: the anonymous memory of the last look, the bound, 1
/// while degraded (and at the bound), and the exits at the bound; all 0 without a watch.
pub(super) fn family(watch: Option<&MemoryWatch>) -> String {
    let (anon, bound, degraded, exits) = watch.map_or((0, 0, 0, 0), |watch| {
        let degraded = u64::from(watch.degraded());
        (watch.anon(), watch.bound(), degraded, watch.exits())
    });
    let series = [
        (ANON, "gauge", anon),
        (BOUND, "gauge", bound),
        (DEGRADED, "gauge", degraded),
        (EXITS, "counter", exits),
    ];
    let mut text = String::new();
    for (name, kind, value) in series {
        let _ = write!(text, "# TYPE {name} {kind}\n{name} {value}\n");
    }
    text
}

#[cfg(test)]
#[path = "memory_bound_tests.rs"]
mod tests;
