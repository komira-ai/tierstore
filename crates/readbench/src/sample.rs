//! The samplers of the farm: a node's drives, read from `/proc/diskstats` once an interval, and
//! the series of a node's metrics page, read before and after a load. The parsing and the
//! arithmetic are pure functions; [`sample_disks`] is the loop that reads the file and writes
//! one [`DiskSample`] line per device per interval, its pause between readings handed to it.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use futures::future::BoxFuture;

use crate::Error;
use crate::clock::Clock;
use crate::record::{DiskSample, MetricDelta, write_line};

/// The counters of one block device that the harness uses, as `/proc/diskstats` counts them
/// since boot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiskStat {
    /// Reads completed.
    pub reads: u64,
    /// Sectors (512 bytes) read.
    pub sectors_read: u64,
    /// Milliseconds spent reading.
    pub read_ms: u64,
    /// Milliseconds the device had I/O in flight.
    pub io_ms: u64,
    /// Milliseconds of I/O in flight, each weighted by the number in flight.
    pub weighted_ms: u64,
}

/// The value of `x` as a float; the counts here are far below the range where it rounds.
#[must_use]
#[expect(
    clippy::cast_precision_loss,
    reason = "counts of reads, sectors and milliseconds stay far below 2^53"
)]
pub fn to_f64(x: u64) -> f64 {
    x as f64
}

/// The devices of a `/proc/diskstats` text by name. A line with fewer than the 14 fields of
/// the format, or with a field that is not a number, is left out.
#[must_use]
pub fn parse_diskstats(text: &str) -> BTreeMap<String, DiskStat> {
    text.lines().filter_map(diskstats_line).collect()
}

/// One line of `/proc/diskstats`: major, minor, name, then the counters from `reads completed`.
fn diskstats_line(line: &str) -> Option<(String, DiskStat)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if f.len() < 14 {
        return None;
    }
    let n = |i: usize| f[i].parse::<u64>().ok();
    Some((
        f[2].to_owned(),
        DiskStat {
            reads: n(3)?,
            sectors_read: n(5)?,
            read_ms: n(6)?,
            io_ms: n(12)?,
            weighted_ms: n(13)?,
        },
    ))
}

/// The interval of device `dev` between readings `a` and `b`, `dt_ms` apart and ending at
/// `t_ms`. A counter that went backwards counts as no change. With `dt_ms` 0 the rates are 0.
/// `util_pct` is at most 100.
#[must_use]
pub fn interval(
    run: &str,
    node: &str,
    dev: &str,
    a: &DiskStat,
    b: &DiskStat,
    t_ms: u64,
    dt_ms: u64,
) -> DiskSample {
    let reads = b.reads.saturating_sub(a.reads);
    let sectors = b.sectors_read.saturating_sub(a.sectors_read);
    let read_ms = b.read_ms.saturating_sub(a.read_ms);
    let io_ms = b.io_ms.saturating_sub(a.io_ms);
    let weighted_ms = b.weighted_ms.saturating_sub(a.weighted_ms);
    let dt = to_f64(dt_ms);
    let per_dt = |x: f64| if dt_ms == 0 { 0.0 } else { x / dt };
    DiskSample {
        run: run.to_owned(),
        node: node.to_owned(),
        device: dev.to_owned(),
        t_ms,
        reads,
        rkb_s: per_dt(to_f64(sectors) * 512.0 / 1024.0 * 1000.0),
        r_await_ms: if reads == 0 {
            0.0
        } else {
            to_f64(read_ms) / to_f64(reads)
        },
        aqu_sz: per_dt(to_f64(weighted_ms)),
        util_pct: per_dt(100.0 * to_f64(io_ms)).min(100.0),
    }
}

/// One series of a metrics page: `name{labels} value`.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    /// The series' name.
    pub name: String,
    /// Its labels as written, braces included; empty for none.
    pub labels: String,
    /// Its value.
    pub value: f64,
}

/// The series of a Prometheus text page. Comment lines (`#`), blank lines and lines that are not
/// `name[{labels}] value [timestamp]` are skipped. The labels may hold spaces, commas, braces
/// and escaped quotes inside their quoted values.
#[must_use]
pub fn parse_metrics(text: &str) -> Vec<Series> {
    text.lines().filter_map(metrics_line).collect()
}

/// The index of the `}` that closes the label set at the start of `rest`, skipping quoted values.
fn labels_end(rest: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in rest.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted {
            match c {
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
        } else {
            match c {
                '"' => quoted = true,
                '}' => return Some(i),
                _ => {}
            }
        }
    }
    None
}

/// One line of a metrics page.
fn metrics_line(line: &str) -> Option<Series> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    let name_end = line.find(|c: char| c == '{' || c.is_whitespace())?;
    let name = &line[..name_end];
    if name.is_empty() {
        return None;
    }
    let rest = &line[name_end..];
    let (labels, after) = if rest.starts_with('{') {
        let end = labels_end(rest)?;
        (&rest[..=end], &rest[end + 1..])
    } else {
        ("", rest)
    };
    let value = after.split_whitespace().next()?.parse::<f64>().ok()?;
    Some(Series {
        name: name.to_owned(),
        labels: labels.to_owned(),
        value,
    })
}

/// Whether a series is one that only grows: a counter (`_total`), or a part of a histogram
/// (`_bucket`, `_count`, `_sum`). The page's `# TYPE` lines are not kept, so the name says it.
fn is_cumulative(name: &str) -> bool {
    ["_total", "_bucket", "_count", "_sum"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

/// The change of every counter and histogram series between `before` and `after`, `window_ms`
/// apart, in the order of `after`. Gauges, series in only one reading, series that did not change
/// and series that went down (the counter restarted) are left out.
#[must_use]
pub fn deltas(
    run: &str,
    node: &str,
    before: &[Series],
    after: &[Series],
    window_ms: u64,
) -> Vec<MetricDelta> {
    let first: BTreeMap<(&str, &str), f64> = before
        .iter()
        .map(|s| ((s.name.as_str(), s.labels.as_str()), s.value))
        .collect();
    after
        .iter()
        .filter(|s| is_cumulative(&s.name))
        .filter_map(|s| {
            let was = first.get(&(s.name.as_str(), s.labels.as_str()))?;
            let delta = s.value - was;
            (delta > 0.0).then(|| MetricDelta {
                run: run.to_owned(),
                node: node.to_owned(),
                name: s.name.clone(),
                labels: s.labels.clone(),
                delta,
                window_ms,
            })
        })
        .collect()
}

/// The named devices of the file at `path`, in one reading.
fn read_devices(path: &Path, devices: &[String]) -> Result<BTreeMap<String, DiskStat>, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io("read", path, e))?;
    let all = parse_diskstats(&text);
    devices
        .iter()
        .map(|d| match all.get(d) {
            Some(stat) => Ok((d.clone(), *stat)),
            None => Err(Error::Usage(format!(
                "{}: no device {d} in the file",
                path.display()
            ))),
        })
        .collect()
}

/// Reads the `/proc/diskstats` file at `path` `count + 1` times and writes to `out` one
/// [`DiskSample`] line for each of `devices` for each pair of readings; `out_path` names `out` in
/// errors. Before every reading after the first, `pause(interval_ms)` is awaited (the command
/// line passes a sleep; a test passes a function that changes the file). `dt_ms` of an interval is
/// what `clock` says passed since the reading before, `t_ms` what passed since the first reading.
/// Returns the lines written.
///
/// # Errors
/// The file cannot be read, a device is not in it, or a line cannot be written.
#[expect(
    clippy::too_many_arguments,
    reason = "the sampler's binding signature: the file, what to sample, how, and where to write"
)]
pub async fn sample_disks(
    path: &Path,
    devices: &[String],
    run: &str,
    node: &str,
    interval_ms: u64,
    count: u64,
    clock: &dyn Clock,
    pause: &mut dyn FnMut(u64) -> BoxFuture<'static, ()>,
    out: &mut dyn Write,
    out_path: &Path,
) -> Result<u64, Error> {
    let mut prev = read_devices(path, devices)?;
    let start_us = clock.now_us();
    let mut prev_us = start_us;
    let mut written = 0;
    for _ in 0..count {
        pause(interval_ms).await;
        let next = read_devices(path, devices)?;
        let now_us = clock.now_us();
        let t_ms = now_us.saturating_sub(start_us) / 1000;
        let dt_ms = now_us.saturating_sub(prev_us) / 1000;
        for ((dev, a), (_, b)) in prev.iter().zip(&next) {
            let sample = interval(run, node, dev, a, b, t_ms, dt_ms);
            write_line(out, out_path, &sample)?;
            written += 1;
        }
        prev = next;
        prev_us = now_us;
    }
    Ok(written)
}

#[cfg(test)]
#[path = "sample_tests.rs"]
mod tests;
