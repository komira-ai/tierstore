//! The guard of a cell against foreign work on a shared node: silo's erasure-coded traffic
//! reaches every node's drives, and build jobs load the CPUs, whatever node is drained. Before a
//! cell the guard waits for quiet; during it, it counts the disk bytes of the named processes
//! (`/proc/<pid>/io`) and the load; a cell that saw more than the limits is run again. Every row
//! records what its cell saw.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::Error;
use crate::clock::Clock;

/// What a cell saw of foreign work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Seen {
    /// MB/s read and written to disk by the guarded processes during the cell.
    pub foreign_mb_s: f64,
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
    /// The disk bytes so far of every guarded process.
    ///
    /// # Errors
    /// When the proc file system cannot be listed.
    pub fn foreign_bytes(&self) -> Result<u64, Error> {
        let rd = std::fs::read_dir(&self.proc_root).map_err(Error::io("list", &self.proc_root))?;
        Ok(rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                std::fs::read_to_string(p.join("comm"))
                    .is_ok_and(|c| self.comms.iter().any(|n| n == c.trim()))
            })
            .map(|p| io_bytes(&p))
            .sum())
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

    fn rate(&self, clock: &dyn Clock, window: Duration) -> Result<f64, Error> {
        let a = self.foreign_bytes()?;
        let t0 = clock.now();
        while clock.now() - t0 < window {
            clock.sleep(Duration::from_millis(250));
        }
        let secs = (clock.now() - t0).as_secs_f64();
        Ok(crate::stats::mb_per_s(
            self.foreign_bytes()?.saturating_sub(a),
            secs,
        ))
    }

    /// Waits until a quiet window (foreign MB/s and load under the limits), or `max_wait`.
    /// Returns whether it found one.
    ///
    /// # Errors
    /// When the counters cannot be read.
    pub fn wait_quiet(&self, clock: &dyn Clock) -> Result<bool, Error> {
        let t0 = clock.now();
        loop {
            if self.rate(clock, self.quiet_window)? <= self.max_mb_s
                && self.load1()? <= self.max_load
            {
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
            let a = self.foreign_bytes()?;
            let t0 = clock.now();
            let r = cell()?;
            let secs = (clock.now() - t0).as_secs_f64();
            let seen = Seen {
                foreign_mb_s: crate::stats::mb_per_s(self.foreign_bytes()?.saturating_sub(a), secs),
                load1: self.load1()?,
                attempts: attempt,
            };
            if (seen.foreign_mb_s <= self.max_mb_s && seen.load1 <= self.max_load)
                || attempt >= self.attempts
            {
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
