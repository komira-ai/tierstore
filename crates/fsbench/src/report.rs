//! `fsbench report`: the tables of a measurement document, from a results file.
//!
//! The runs are grouped by variant and regime, in the order they first appear. The first
//! `discard` runs of each group (the warm-up: the first run fills the server's cache and the
//! kernel's) are left out, and every figure is given as the median of the runs kept, with their
//! least and most: `median (min-max)`. A figure a run did not measure (no control group, no FUSE
//! connection listed) is `-`. The table of the CPU of the whole-file phases is shown only when
//! a record has it (format 2), so the tables of records of format 1 are as they were.

use std::fmt::Write as _;
use std::path::Path;

use crate::Error;
use crate::hist::Histogram;
use crate::record::RunRecord;

/// The records of the results file at `path`, one per line.
///
/// # Errors
/// The file cannot be read, or a line is not a record (the error names it).
pub fn read(path: &Path) -> Result<Vec<RunRecord>, Error> {
    let text = std::fs::read_to_string(path).map_err(Error::at("read", path))?;
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(n, line)| {
            serde_json::from_str(line).map_err(|e| Error::Parse {
                path: path.to_path_buf(),
                line: n + 1,
                why: format!("not a run record: {e}"),
            })
        })
        .collect()
}

/// A figure of a run.
type Figure = fn(&RunRecord) -> Option<f64>;

fn ms(h: &Histogram, q: f64) -> Option<f64> {
    Some(h.quantile(q) as f64 / 1e6)
}

fn max_ms(h: &Histogram) -> Option<f64> {
    Some(h.max as f64 / 1e6)
}

/// The columns of the first table: the rate of the cycles and their latency.
const LATENCY: &[(&str, Figure)] = &[
    ("opens/s", |r| Some(r.cycles.per_s)),
    ("stat p50 ms", |r| ms(&r.cycles.stat, 0.5)),
    ("stat p99 ms", |r| ms(&r.cycles.stat, 0.99)),
    ("stat max ms", |r| max_ms(&r.cycles.stat)),
    ("open p50 ms", |r| ms(&r.cycles.open, 0.5)),
    ("open p99 ms", |r| ms(&r.cycles.open, 0.99)),
    ("open max ms", |r| max_ms(&r.cycles.open)),
];

/// The columns of the second table: whole-file reads, CPU and the samples.
const LOAD: &[(&str, Figure)] = &[
    ("cold MB/s", |r| Some(r.cold.mb_per_s)),
    ("warm MB/s", |r| Some(r.warm.mb_per_s)),
    ("server CPU ms/1k", |r| Some(r.cpu.server_ms_per_1k)),
    ("cgroup kernel ms/1k", |r| r.cpu.cgroup_system_ms_per_1k),
    ("node CPU ms/1k", |r| Some(r.cpu.node_busy_ms_per_1k)),
    ("D mean", |r| Some(r.samples.d_state_mean)),
    ("D max", |r| Some(f64::from(r.samples.d_state_max))),
    ("waiting mean", |r| r.samples.waiting_mean),
    ("waiting max", |r| r.samples.waiting_max.map(|w| w as f64)),
];

/// The columns of the table of the whole-file phases' CPU, shown when a record has it (format 2).
const READ_CPU: &[(&str, Figure)] = &[
    ("cold MB/s", |r| Some(r.cold.mb_per_s)),
    ("cold server CPU ms/GB", |r| {
        Some(r.cold.cpu.as_ref()?.server_ms_per_gb)
    }),
    ("cold cgroup kernel ms/GB", |r| {
        r.cold.cpu.as_ref()?.cgroup_system_ms_per_gb
    }),
    ("warm MB/s", |r| Some(r.warm.mb_per_s)),
    ("warm server CPU ms/GB", |r| {
        Some(r.warm.cpu.as_ref()?.server_ms_per_gb)
    }),
    ("warm cgroup kernel ms/GB", |r| {
        r.warm.cpu.as_ref()?.cgroup_system_ms_per_gb
    }),
    ("rounds", |r| Some(f64::from(r.input.throughput_rounds))),
];

/// A number as a table shows it: no decimals from 100 up, one from 10, two below.
fn number(value: f64) -> String {
    if value >= 100.0 {
        format!("{value:.0}")
    } else if value >= 10.0 {
        format!("{value:.1}")
    } else {
        format!("{value:.2}")
    }
}

/// `median (min-max)` of `values`, or `-` when there are none.
fn cell(mut values: Vec<f64>) -> String {
    if values.is_empty() {
        return "-".to_owned();
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    let median = (values[(n - 1) / 2] + values[n / 2]) / 2.0;
    format!(
        "{} ({}-{})",
        number(median),
        number(values[0]),
        number(values[n - 1])
    )
}

/// The groups of `records`: variant, regime and their runs, in the order of first appearance.
fn groups(records: &[RunRecord]) -> Vec<((&str, &str), Vec<&RunRecord>)> {
    let mut groups: Vec<((&str, &str), Vec<&RunRecord>)> = Vec::new();
    for record in records {
        let key = (record.variant.as_str(), record.regime.as_str());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, runs)) => runs.push(record),
            None => groups.push((key, vec![record])),
        }
    }
    groups
}

/// A table of `columns`, a row per group of the runs kept.
fn table(out: &mut String, groups: &[((&str, &str), Vec<&RunRecord>)], columns: &[(&str, Figure)]) {
    let names: Vec<&str> = columns.iter().map(|(name, _)| *name).collect();
    let _ = writeln!(out, "| variant | regime | {} |", names.join(" | "));
    let _ = writeln!(out, "|---|---|{}", "---|".repeat(columns.len()));
    for ((variant, regime), runs) in groups {
        let cells: Vec<String> = columns
            .iter()
            .map(|(_, figure)| cell(runs.iter().filter_map(|r| figure(r)).collect()))
            .collect();
        let _ = writeln!(out, "| {variant} | {regime} | {} |", cells.join(" | "));
    }
}

/// The first 12 hex digits of a digest.
fn short(digest: Option<&str>) -> &str {
    digest.map_or("-", |d| d.get(..12).unwrap_or(d))
}

/// The tables of `records`, leaving out the first `discard` runs of each group.
#[must_use]
pub fn render(records: &[RunRecord], discard: u32) -> String {
    let all = groups(records);
    let kept: Vec<_> = all
        .iter()
        .map(|(key, runs)| {
            (
                *key,
                runs.iter()
                    .copied()
                    .filter(|r| r.run >= discard)
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Median of the runs kept (least-most); the first {discard} run(s) of each series left out.\n"
    );
    table(&mut out, &kept, LATENCY);
    out.push('\n');
    table(&mut out, &kept, LOAD);
    out.push('\n');
    if records.iter().any(|r| r.cold.cpu.is_some()) {
        table(&mut out, &kept, READ_CPU);
        out.push('\n');
    }
    let _ = writeln!(
        out,
        "| variant | regime | runs kept | errors | load at start | file list | server executable | settings |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
    for (((variant, regime), runs), (_, every)) in kept.iter().zip(&all) {
        let errors: u64 = runs
            .iter()
            .map(|r| r.cycles.errors + r.cold.errors + r.warm.errors)
            .sum();
        let first = runs.first().copied();
        let settings = first.map_or_else(String::new, |r| {
            r.settings
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        });
        let _ = writeln!(
            out,
            "| {variant} | {regime} | {} of {} | {errors} | {} | {} | {} | {settings} |",
            runs.len(),
            every.len(),
            cell(
                runs.iter()
                    .filter_map(|r| load_of(r.env.loadavg.as_deref()?))
                    .collect()
            ),
            short(first.map(|r| r.input.list_sha256.as_str())),
            short(first.and_then(|r| r.server.exe_sha256.as_deref())),
        );
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "| variant | regime | kernel | CPUs | governors | fuse parameters | mount | connection |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|");
    for ((variant, regime), runs) in &kept {
        let Some(env) = runs.first().map(|r| &r.env) else {
            continue;
        };
        let pairs = |map: &std::collections::BTreeMap<String, String>| {
            map.iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let governors: Vec<String> = env
            .governors
            .iter()
            .map(|(g, n)| format!("{g} x{n}"))
            .collect();
        let _ = writeln!(
            out,
            "| {variant} | {regime} | {} | {} | {} | {} | {} | {} |",
            env.kernel.as_deref().unwrap_or("-"),
            env.cpus.map_or_else(|| "-".to_owned(), |c| c.to_string()),
            governors.join(", "),
            pairs(&env.fuse_parameters),
            env.mountinfo.as_deref().map_or("-", mount_of),
            pairs(&env.connection),
        );
    }
    out
}

/// The one-minute load average of a `/proc/loadavg` line.
fn load_of(line: &str) -> Option<f64> {
    line.split(' ').next()?.parse().ok()
}

/// What a mountinfo line says of the mount: its options, its type and the super block's options
/// (the fields that do not name a place).
fn mount_of(line: &str) -> &str {
    let options_from = line.split(' ').take(5).map(|f| f.len() + 1).sum::<usize>();
    line.get(options_from..).unwrap_or(line)
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
