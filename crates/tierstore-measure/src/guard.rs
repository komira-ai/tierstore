//! The guard of a cell against foreign work on a shared node: silo's erasure-coded traffic
//! reaches every node's drives, and build jobs load the CPUs, whatever node is drained. Before a
//! cell the guard waits for quiet; during it, it counts the disk bytes of the named processes
//! (`/proc/<pid>/io`) and the load; a cell that saw more than the limits is run again. Every row
//! records what its cell saw.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::Error;
use crate::clock::Clock;

/// What a cell saw of foreign work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, serde::Deserialize)]
pub struct Seen {
    /// MB/s read and written to disk by the guarded processes during the cell.
    pub foreign_mb_s: f64,
    /// Write requests per second on the watched drives during the cell (foreign for a cell that
    /// only reads them).
    #[serde(default)]
    pub foreign_w_s: f64,
    /// The one-minute load average at the cell's end.
    pub load1: f64,
    /// Attempts the cell took (1 when the first was clean).
    pub attempts: u32,
}

/// The guard's settings.
#[derive(Debug, Clone)]
pub struct Guard {
    /// The proc file system.
    pub proc_root: PathBuf,
    /// The process names whose disk bytes are foreign (`silo`).
    pub comms: Vec<String>,
    /// The most foreign MB/s a clean cell may see.
    pub max_mb_s: f64,
    /// The highest load a clean cell may end with.
    pub max_load: f64,
    /// Attempts per cell before its last result is kept as it is (and marked by its count).
    pub attempts: u32,
    /// How long the quiet check before a cell looks.
    pub quiet_window: Duration,
    /// The longest a cell waits for quiet before it runs anyway.
    pub max_wait: Duration,
    /// Drives whose write requests are foreign (a read-only arm's drives; empty: none).
    pub write_devs: Vec<String>,
    /// The most write requests per second on them a clean cell may see.
    pub max_w_s: f64,
}

/// Foreign activity so far: the guarded processes' disk bytes and the watched drives' writes.
#[derive(Debug, Clone)]
struct Mark {
    bytes: BTreeMap<String, u64>,
    writes: u64,
    at: Duration,
}

/// The bytes the guarded processes moved between two marks: per process, so one that exits or
/// starts in between neither hides the others' bytes nor adds what it moved before the first mark.
/// A process that started in between counts from zero; one that exited counts nothing.
fn moved(a: &BTreeMap<String, u64>, b: &BTreeMap<String, u64>) -> u64 {
    b.iter()
        .map(|(pid, end)| end.saturating_sub(a.get(pid).copied().unwrap_or(0)))
        .sum()
}

fn io_bytes(dir: &Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(dir.join("io")) else {
        return 0;
    };
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            matches!(k, "read_bytes" | "write_bytes").then(|| v.trim().parse::<u64>().ok())?
        })
        .sum()
}

impl Guard {
    /// Whether foreign MB/s and load are both within the limits.
    #[must_use]
    pub fn quiet(&self, seen: &Seen) -> bool {
        seen.foreign_mb_s <= self.max_mb_s
            && seen.foreign_w_s <= self.max_w_s
            && seen.load1 <= self.max_load
    }

    /// Write requests so far on the watched drives (from `diskstats` in the proc root).
    ///
    /// # Errors
    /// When `diskstats` cannot be read or lacks a drive.
    pub fn writes(&self) -> Result<u64, Error> {
        if self.write_devs.is_empty() {
            return Ok(0);
        }
        let s = crate::diskstats::Sampler::new(&self.proc_root.join("diskstats"), &self.write_devs);
        Ok(s.snapshot()?.values().map(|c| c.writes).sum())
    }

    fn mark(&self, clock: &dyn Clock) -> Result<Mark, Error> {
        Ok(Mark {
            bytes: self.foreign_bytes()?,
            writes: self.writes()?,
            at: clock.now(),
        })
    }

    fn since(&self, a: &Mark, clock: &dyn Clock, attempts: u32) -> Result<Seen, Error> {
        let b = self.mark(clock)?;
        let secs = (b.at - a.at).as_secs_f64();
        Ok(Seen {
            foreign_mb_s: crate::stats::mb_per_s(moved(&a.bytes, &b.bytes), secs),
            foreign_w_s: b.writes.saturating_sub(a.writes) as f64 / secs.max(1e-9),
            load1: self.load1()?,
            attempts,
        })
    }

    /// The disk bytes so far of each guarded process, by its proc entry (the pid).
    ///
    /// # Errors
    /// When the proc file system cannot be listed.
    pub fn foreign_bytes(&self) -> Result<BTreeMap<String, u64>, Error> {
        let rd = std::fs::read_dir(&self.proc_root).map_err(Error::io("list", &self.proc_root))?;
        Ok(rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                std::fs::read_to_string(p.join("comm"))
                    .is_ok_and(|c| self.comms.iter().any(|n| n == c.trim()))
            })
            .map(|p| {
                let pid = p.file_name().map(|n| n.to_string_lossy().into_owned());
                (pid.unwrap_or_default(), io_bytes(&p))
            })
            .collect())
    }

    /// The one-minute load average.
    ///
    /// # Errors
    /// When `loadavg` cannot be read or parsed.
    pub fn load1(&self) -> Result<f64, Error> {
        let p = self.proc_root.join("loadavg");
        let t = std::fs::read_to_string(&p).map_err(Error::io("read", &p))?;
        t.split_whitespace()
            .next()
            .and_then(|x| x.parse().ok())
            .ok_or_else(|| Error::Invalid(format!("{} holds {t:?}", p.display())))
    }

    fn window(&self, clock: &dyn Clock, window: Duration) -> Result<Seen, Error> {
        let a = self.mark(clock)?;
        while clock.now() - a.at < window {
            clock.sleep(Duration::from_millis(250));
        }
        self.since(&a, clock, 0)
    }

    /// Waits until a quiet window (foreign MB/s and load under the limits), or `max_wait`.
    /// Returns whether it found one.
    ///
    /// # Errors
    /// When the counters cannot be read.
    pub fn wait_quiet(&self, clock: &dyn Clock) -> Result<bool, Error> {
        let t0 = clock.now();
        loop {
            let seen = self.window(clock, self.quiet_window)?;
            if self.quiet(&seen) {
                return Ok(true);
            }
            if clock.now() - t0 >= self.max_wait {
                return Ok(false);
            }
        }
    }

    /// Runs `cell` until one attempt sees no more than the limits, or the attempts run out;
    /// returns the last attempt's result and what it saw.
    ///
    /// # Errors
    /// The cell's error, or when the counters cannot be read.
    pub fn run<T>(
        &self,
        clock: &dyn Clock,
        mut cell: impl FnMut() -> Result<T, Error>,
    ) -> Result<(T, Seen), Error> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.wait_quiet(clock)?;
            let a = self.mark(clock)?;
            let r = cell()?;
            let seen = self.since(&a, clock, attempt)?;
            if self.quiet(&seen) || attempt >= self.attempts {
                return Ok((r, seen));
            }
        }
    }
}

/// Runs `cell` under `guard` if there is one, else once (its [`Seen`] then says one attempt and
/// nothing else).
///
/// # Errors
/// As [`Guard::run`], or the cell's error.
pub fn guarded<T>(
    guard: Option<&Guard>,
    clock: &dyn Clock,
    mut cell: impl FnMut() -> Result<T, Error>,
) -> Result<(T, Seen), Error> {
    match guard {
        Some(g) => g.run(clock, cell),
        None => Ok((
            cell()?,
            Seen {
                attempts: 1,
                ..Seen::default()
            },
        )),
    }
}

#[cfg(test)]
mod tests;
