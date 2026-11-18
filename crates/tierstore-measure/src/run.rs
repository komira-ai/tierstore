//! What every cell shares: the idle check before a run, the measured window with the drives'
//! statistics over exactly that window, and the programs an arm runs (`xfs_io`, `losetup`, ...).

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::Error;
use crate::clock::Clock;
use crate::diskstats::{IoStat, Sampler, delta};

/// The measured part of a cell, in the clock's time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// When measuring starts (after the warm-up).
    pub from: Duration,
    /// When it ends.
    pub end: Duration,
}

impl Span {
    /// A span that starts `warmup` from now and lasts `measure`.
    #[must_use]
    pub fn starting(clock: &dyn Clock, warmup: Duration, measure: Duration) -> Self {
        let from = clock.now() + warmup;
        Self {
            from,
            end: from + measure,
        }
    }

    /// Its length in seconds.
    #[must_use]
    pub fn secs(&self) -> f64 {
        (self.end - self.from).as_secs_f64()
    }

    /// Whether a request that ran from `t0` to `t1` lies inside it.
    #[must_use]
    pub fn holds(&self, t0: Duration, t1: Duration) -> bool {
        t0 >= self.from && t1 <= self.end
    }
}

/// Waits until `t`, or until `done` is set. A watcher of work (`done` given) only peeks at the
/// clock; one without work reads it.
fn sleep_until(clock: &dyn Clock, t: Duration, done: Option<&AtomicBool>) {
    loop {
        let now = match done {
            Some(d) if d.load(Ordering::SeqCst) => return,
            Some(_) => clock.peek(),
            None => clock.now(),
        };
        if now >= t {
            return;
        }
        clock.sleep((t - now).min(Duration::from_millis(50)));
    }
}

/// The drives' statistics over `span`: a read of the counters at its start and at its end (or
/// when `done` is set, if that is sooner).
///
/// # Errors
/// When the counters cannot be read.
pub fn sample_span(
    sampler: &Sampler,
    clock: &dyn Clock,
    span: Span,
    done: Option<&AtomicBool>,
) -> Result<BTreeMap<String, IoStat>, Error> {
    sleep_until(clock, span.from, done);
    let a = sampler.snapshot()?;
    let t0 = clock.peek();
    sleep_until(clock, span.end, done);
    let b = sampler.snapshot()?;
    let secs = (clock.peek() - t0).as_secs_f64();
    Ok(delta(&a, &b, secs))
}

/// Runs `work` over a span that starts `warmup` from now and lasts `measure`, sampling the
/// drives of `sampler` (if any) over exactly the span on a thread of its own.
///
/// # Errors
/// The error of `work`, else of the sampling.
pub fn measure<T: Send>(
    clock: &dyn Clock,
    sampler: Option<&Sampler>,
    warmup: Duration,
    measure: Duration,
    work: impl FnOnce(Span) -> Result<T, Error> + Send,
) -> Result<(T, BTreeMap<String, IoStat>), Error> {
    let span = Span::starting(clock, warmup, measure);
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let f = &finished;
        let sampled = sampler.map(|s| scope.spawn(move || sample_span(s, clock, span, Some(f))));
        let done = work(span);
        finished.store(true, Ordering::SeqCst);
        let stats = match sampled {
            Some(h) => h
                .join()
                .unwrap_or_else(|_| Err(Error::Invalid("the drive sampler panicked".into())))?,
            None => BTreeMap::new(),
        };
        Ok((done?, stats))
    })
}

/// Checks that the drives are idle before a run: their `%util` over `secs` must be at most
/// `max_util` (the plan's rule: abort above 5% over 60 s). Returns what it saw.
///
/// # Errors
/// When a drive is busier than that, or the counters cannot be read.
pub fn preflight(
    sampler: &Sampler,
    clock: &dyn Clock,
    secs: u64,
    max_util: f64,
) -> Result<BTreeMap<String, IoStat>, Error> {
    let span = Span::starting(clock, Duration::ZERO, Duration::from_secs(secs));
    let seen = sample_span(sampler, clock, span, None)?;
    check_idle(&seen, secs, max_util)?;
    Ok(seen)
}

/// Fails naming the first drive of `seen` busier than `max_util` percent.
///
/// # Errors
/// When one is.
pub fn check_idle(seen: &BTreeMap<String, IoStat>, secs: u64, max_util: f64) -> Result<(), Error> {
    for (dev, s) in seen {
        if s.util > max_util {
            return Err(Error::Invalid(format!(
                "{dev} was {:.1}% busy over {secs} s before the run (the limit is {max_util}%): \
                 the node is not idle",
                s.util
            )));
        }
    }
    Ok(())
}

/// The programs an arm runs.
pub trait Runner: Sync {
    /// Runs `argv` and returns its standard output.
    ///
    /// # Errors
    /// When it cannot start or exits unsuccessfully.
    fn run(&self, argv: &[&str]) -> Result<String, Error>;
}

/// Runs programs on this machine.
#[derive(Debug, Default, Clone, Copy)]
pub struct System;

impl Runner for System {
    fn run(&self, argv: &[&str]) -> Result<String, Error> {
        let cmd = argv.join(" ");
        let Some((prog, args)) = argv.split_first() else {
            return Err(Error::Invalid("an empty command".into()));
        };
        let out = Command::new(prog)
            .args(args)
            .output()
            .map_err(|e| Error::Command {
                cmd: cmd.clone(),
                detail: e.to_string(),
            })?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(Error::Command {
                cmd,
                detail: format!("{}: {}", out.status, err.lines().next().unwrap_or("")),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::StepClock;
    use tierstore_testkit::dir::TestDir;

    fn stats_file(d: &TestDir, io_ms: u64) -> std::path::PathBuf {
        let p = d.path().join("diskstats");
        std::fs::write(&p, format!("8 16 sdb 0 0 0 0 0 0 0 0 0 {io_ms} 0\n")).unwrap();
        p
    }

    #[test]
    fn span_bounds_and_length() {
        let c = StepClock::new(Duration::from_millis(1), 0);
        let s = Span::starting(&c, Duration::from_secs(5), Duration::from_secs(20));
        assert_eq!(s.from, Duration::from_secs(5));
        assert!((s.secs() - 20.0).abs() < 1e-9);
        assert!(s.holds(Duration::from_secs(5), Duration::from_secs(25)));
        assert!(!s.holds(Duration::from_secs(4), Duration::from_secs(6)));
        assert!(!s.holds(Duration::from_secs(6), Duration::from_secs(26)));
    }

    #[test]
    fn measure_runs_the_work_and_samples_the_span() {
        let d = TestDir::under(std::env::temp_dir(), "run");
        let s = Sampler::new(&stats_file(&d, 0), &["sdb".into()]);
        let c = StepClock::new(Duration::from_millis(1), 0);
        let (v, io) = measure(
            &c,
            Some(&s),
            Duration::from_millis(10),
            Duration::from_millis(100),
            |span| Ok(span.secs()),
        )
        .unwrap();
        assert!((v - 0.1).abs() < 1e-9);
        assert!(io.contains_key("sdb"));
        let (_, none) = measure(&c, None, Duration::ZERO, Duration::ZERO, |_| Ok(())).unwrap();
        assert!(none.is_empty());
        let e = measure(&c, None, Duration::ZERO, Duration::ZERO, |_| {
            Err::<(), _>(Error::Invalid("work failed".into()))
        })
        .unwrap_err();
        assert_eq!(e.to_string(), "work failed");
        let bad = Sampler::new(&d.path().join("none"), &["sdb".into()]);
        assert!(measure(&c, Some(&bad), Duration::ZERO, Duration::ZERO, |_| Ok(())).is_err());
    }

    #[test]
    fn preflight_passes_an_idle_drive_and_stops_a_busy_one() {
        let d = TestDir::under(std::env::temp_dir(), "run");
        let p = stats_file(&d, 0);
        let s = Sampler::new(&p, &["sdb".into()]);
        let c = StepClock::new(Duration::from_millis(1), 0);
        assert!(preflight(&s, &c, 60, 5.0).unwrap()["sdb"].util.abs() < 1e-9);
        let a = s.snapshot().unwrap();
        std::fs::write(&p, "8 16 sdb 0 0 0 0 0 0 0 0 0 54000 0\n").unwrap();
        let busy = delta(&a, &s.snapshot().unwrap(), 60.0);
        let e = check_idle(&busy, 60, 5.0).unwrap_err();
        assert_eq!(
            e.to_string(),
            "sdb was 90.0% busy over 60 s before the run (the limit is 5%): the node is not idle"
        );
        assert!(preflight(&Sampler::new(&d.path().join("x"), &[]), &c, 1, 5.0).is_err());
    }

    #[test]
    fn system_runner_returns_output_and_reports_failures() {
        assert_eq!(System.run(&["echo", "hi"]).unwrap(), "hi\n");
        let e = System.run(&["ls", "/nonexistent-ts-measure"]).unwrap_err();
        assert!(
            e.to_string()
                .starts_with("command `ls /nonexistent-ts-measure`: exit status: 2: ls: "),
            "{e}"
        );
        assert!(System.run(&["/nonexistent/prog"]).is_err());
        assert!(System.run(&[]).is_err());
        assert!(System.run(&["false"]).is_err());
    }
}
