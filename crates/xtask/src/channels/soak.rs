//! `channels soak`: the farm keeps a release healthy for N hours before the public channels
//! follow (`docs/design/distribution.md`, section 5.3).
//!
//! The soak is a timer, not a verdict (AGENTS.md B12): nobody approves it. It starts at
//! `--reached-at`, the moment the farm was read converged on the version, and ends when
//! `--hours` have passed since. With `--hours 0` it ends at once and reads nothing.
//!
//! With more, it reads the coordinator's convergence of the service every `--interval-secs`,
//! the first read at once and the last at the end of the N hours, and ends non-zero on the first
//! read that does not show the farm healthy on `--version`:
//! - the desired version is `--version` (an automatic rollback or a newer release changes it),
//! - the state is `converged` (not `halted`, not `in_progress`),
//! - no role has a host `down`, `failed` or `updating`.
//!
//! A read that gets no answer is tolerated up to `--max-silent-reads` in a row. N above
//! [`MAX_HOURS`] is refused: a hosted job lives at most 6 hours, and a job that ends silently at
//! the limit would let the public channels follow without the soak having passed.
//!
//! The reads are made by a program, `--read-command` with its `--read-arg`s, that prints the
//! JSON `hostkeeper ctl releases converge` prints. This repository calls none yet: hostkeeper's
//! CI client sets a version and then reads, and has no call that only reads, so `release.yml`
//! passes no `--read-command` and a soak of more than 0 hours is refused for it (section 11 of
//! the design says what is asked of hostkeeper). The clock is a value handed in ([`Clock`]), so
//! a test moves time without waiting for it (AGENTS.md T10); `--now` starts a clock that only
//! the soak's own sleeps advance.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use serde_json::Value;

use crate::Error;

/// The most hours one run soaks: a hosted job lives 6 hours, and the rest is the job's own start.
pub const MAX_HOURS: u32 = 5;

/// The default seconds between two reads.
pub const INTERVAL_SECS: u64 = 60;

/// The default number of reads in a row that may get no answer.
pub const MAX_SILENT_READS: u32 = 5;

const HOUR: u64 = 3600;

/// The time, in seconds since the epoch, and a way to wait.
pub trait Clock {
    /// Seconds since the epoch.
    fn now(&mut self) -> u64;
    /// Waits `secs` seconds.
    fn wait(&mut self, secs: u64);
}

/// The machine's clock.
pub struct SystemClock;

impl Clock for SystemClock {
    #[expect(
        clippy::disallowed_methods,
        reason = "the one place the soak reads the machine's time; everything else is handed a Clock"
    )]
    fn now(&mut self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn wait(&mut self, secs: u64) {
        std::thread::sleep(Duration::from_secs(secs));
    }
}

/// A clock that starts at a given time and moves only when the soak sleeps.
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now(&mut self) -> u64 {
        self.0
    }

    fn wait(&mut self, secs: u64) {
        self.0 = self.0.saturating_add(secs);
    }
}

/// What one read came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// The coordinator's answer, the JSON text.
    Reply(String),
    /// No answer (the program failed), and what it said.
    Silent(String),
}

/// What the soak asks of the coordinator.
pub trait Reader {
    /// One read of the convergence of the service.
    ///
    /// # Errors
    /// The program could not be started.
    fn read(&self) -> Result<Read, Error>;
}

/// The program that reads: `program args...`, whose standard output is the answer.
pub struct RealReader {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Reader for RealReader {
    fn read(&self) -> Result<Read, Error> {
        let out = Command::new(&self.program)
            .args(&self.args)
            .output()
            .map_err(Error::spawn(self.program.display().to_string()))?;
        if out.status.success() {
            Ok(Read::Reply(
                String::from_utf8_lossy(&out.stdout).into_owned(),
            ))
        } else {
            let said = String::from_utf8_lossy(&out.stderr);
            Ok(Read::Silent(said.trim().to_owned()))
        }
    }
}

/// What `soak` is asked.
pub struct Request<'a> {
    pub hours: u32,
    /// Seconds since the epoch at which the farm was read converged.
    pub reached_at: u64,
    /// The version the farm must stay on, as the coordinator names it (`X.Y.Z+<12 hex>`).
    pub version: &'a str,
    pub interval_secs: u64,
    pub max_silent_reads: u32,
}

/// The verb: the lines to print.
///
/// # Errors
/// Flags that do not fit, a read that shows the farm not healthy, too many reads in a row with
/// no answer, or a reading program that cannot be started.
pub fn soak(
    request: &Request<'_>,
    clock: &mut dyn Clock,
    reader: Option<&dyn Reader>,
) -> Result<Vec<String>, Error> {
    if request.hours > MAX_HOURS {
        return Err(Error::Usage(format!(
            "--hours {} is more than {MAX_HOURS}: one hosted job lives at most 6 hours, so a longer soak needs a continuation that is not a job waiting",
            request.hours
        )));
    }
    if request.interval_secs == 0 || request.max_silent_reads == 0 {
        return Err(Error::Usage(
            "--interval-secs and --max-silent-reads are at least 1".to_owned(),
        ));
    }
    if request.version.is_empty() {
        return Err(Error::Usage("--version is empty".to_owned()));
    }
    if request.hours == 0 {
        return Ok(vec![
            "soak: 0 hours, so the public channels follow at once".to_owned(),
        ]);
    }
    let Some(reader) = reader else {
        return Err(Error::Usage(format!(
            "a soak of {} hours reads the farm's convergence and needs --read-command; none is given, so the public channels do not follow",
            request.hours
        )));
    };
    let deadline = request
        .reached_at
        .saturating_add(u64::from(request.hours) * HOUR);
    let (mut reads, mut silent) = (0_u32, 0_u32);
    loop {
        reads += 1;
        match reader.read()? {
            Read::Reply(text) => {
                silent = 0;
                judge(&text, request.version).map_err(|why| {
                    Error::Failed(format!(
                        "soak: read {reads} shows the farm not healthy on {}: {why}",
                        request.version
                    ))
                })?;
            }
            Read::Silent(said) => {
                silent += 1;
                if silent >= request.max_silent_reads {
                    return Err(Error::Failed(format!(
                        "soak: the coordinator gave no answer to {silent} reads in a row (last: {said:?})"
                    )));
                }
            }
        }
        let now = clock.now();
        if now >= deadline {
            break;
        }
        clock.wait(request.interval_secs.min(deadline - now));
    }
    Ok(vec![format!(
        "soak: the farm stayed healthy on {} for {} hours ({reads} reads)",
        request.version, request.hours
    )])
}

/// Whether the coordinator's answer shows the farm healthy on `version`: the reason it does not,
/// or none. The answer is the canonical JSON of `releases converge`; proto3 leaves out a field
/// that has its default, so an absent count is 0.
fn judge(text: &str, version: &str) -> Result<(), String> {
    let reply: Value =
        serde_json::from_str(text).map_err(|e| format!("the answer is not JSON ({e})"))?;
    let convergence = &reply["convergence"];
    let desired = convergence["desired"].as_str().unwrap_or("none");
    if desired != version {
        return Err(format!(
            "the desired version is {desired}, not {version} (a rollback or a newer release replaced it)"
        ));
    }
    let state = convergence["state"]
        .as_str()
        .unwrap_or("CONVERGENCE_STATE_UNSPECIFIED")
        .trim_start_matches("CONVERGENCE_STATE_")
        .to_ascii_lowercase();
    if state != "converged" {
        return Err(format!("the state is {state}, not converged"));
    }
    let roles = convergence["roles"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    for role in roles {
        let name = role["role"].as_str().unwrap_or("");
        for key in ["down", "failed", "updating"] {
            let count = role[key].as_u64().unwrap_or(0);
            if count > 0 {
                return Err(format!("role {name:?} has {count} host(s) {key}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
