//! The tables of the results document, made from the records a run wrote: one function per
//! table over slices of records, each returning Markdown, and [`render`] with the four under
//! their headings. Numbers are printed with fixed decimals so the same records give the same
//! text.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use crate::Error;
use crate::record::{
    ClientRun, DemandRead, DiskSample, MetricDelta, Mode, PlanRecord, Summary, nearest_rank,
};
use crate::sample::to_f64;

const MIB: f64 = 1_048_576.0;
const GIB: f64 = 1_073_741_824.0;

/// A Markdown table: the header, the rule under it and a line per row.
fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let line = |cells: Vec<&str>| format!("| {} |\n", cells.join(" | "));
    let mut out = line(header.to_vec());
    out.push_str(&line(vec!["---"; header.len()]));
    for row in rows {
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

/// The median of `values` (0 for none).
fn median(values: &[u64]) -> u64 {
    Summary::of(values).p50
}

/// `bytes` in MiB with two decimals.
fn mib(bytes: u64) -> String {
    format!("{:.2}", to_f64(bytes) / MIB)
}

/// `us` microseconds as milliseconds with one decimal.
fn ms1(us: u64) -> String {
    format!("{:.1}", to_f64(us) / 1000.0)
}

/// `us` microseconds as seconds with two decimals.
fn secs2(us: u64) -> String {
    format!("{:.2}", to_f64(us) / 1_000_000.0)
}

/// A count of GETs of one kind, median over the clients of a group.
fn median_gets(runs: &[&ClientRun], kind: &str) -> u64 {
    let v: Vec<u64> = runs
        .iter()
        .map(|r| r.gets.get(kind).copied().unwrap_or(0))
        .collect();
    median(&v)
}

/// The pack GETs of a group's clients: `grid/sub/retry/hedge` for the engine's kinds, or
/// `<n> chunk` when the clients read the shipped way.
fn gets_cell(runs: &[&ClientRun]) -> String {
    if runs.iter().any(|r| r.gets.contains_key("chunk")) {
        return format!("{} chunk", median_gets(runs, "chunk"));
    }
    format!(
        "{}/{}/{}/{}",
        median_gets(runs, "grid_part"),
        median_gets(runs, "demand_sub"),
        median_gets(runs, "retry"),
        median_gets(runs, "hedge"),
    )
}

/// The name of a fetch mode as the tables print it.
fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Engine => "engine",
        Mode::Shipped => "shipped",
    }
}

/// The table of the clients' runs: a row for each tree, mode and number of clients, with the
/// median over its clients of the counts (files, packs, manifest GETs, pack GETs as
/// `grid/sub/retry/hedge` or `<n> chunk`, MiB needed and fetched) and the amplification (bytes
/// fetched over bytes needed, per client), the tree-ready time at its median and 99th percentile
/// over the clients (seconds), the first-byte time (milliseconds: the median of the clients'
/// medians, and the largest of the clients' 99th percentiles, over the clients that sent a GET)
/// and the sum of the failed GETs.
#[must_use]
pub fn trees_table(runs: &[ClientRun]) -> String {
    let mut groups: BTreeMap<(&str, Mode, u32), Vec<&ClientRun>> = BTreeMap::new();
    for r in runs {
        groups
            .entry((r.tree.as_str(), r.mode, r.clients))
            .or_default()
            .push(r);
    }
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|((tree, mode, clients), rs)| {
            let col = |f: fn(&ClientRun) -> u64| -> Vec<u64> { rs.iter().map(|r| f(r)).collect() };
            let ready = Summary::of(&col(|r| r.tree_ready_us));
            let seen: Vec<&ClientRun> = rs
                .iter()
                .copied()
                .filter(|r| r.first_byte_us.n > 0)
                .collect();
            let first_p50 = median(
                &seen
                    .iter()
                    .map(|r| r.first_byte_us.p50)
                    .collect::<Vec<u64>>(),
            );
            let first_p99 = seen.iter().map(|r| r.first_byte_us.p99).max().unwrap_or(0);
            let amp = median(&col(ClientRun::amplification_milli));
            vec![
                (*tree).chars().take(12).collect(),
                mode_name(*mode).to_owned(),
                clients.to_string(),
                median(&col(|r| r.files)).to_string(),
                median(&col(|r| r.packs)).to_string(),
                median(&col(|r| r.manifests)).to_string(),
                gets_cell(rs),
                mib(median(&col(|r| r.bytes_needed))),
                mib(median(&col(|r| r.bytes_fetched))),
                format!("{:.2}", to_f64(amp) / 1000.0),
                secs2(ready.p50),
                secs2(ready.p99),
                ms1(first_p50),
                ms1(first_p99),
                col(|r| r.get_errors).iter().sum::<u64>().to_string(),
            ]
        })
        .collect();
    table(
        &[
            "tree",
            "mode",
            "clients",
            "files",
            "packs",
            "manifest GETs",
            "pack GETs (grid/sub/retry/hedge)",
            "needed MiB",
            "fetched MiB",
            "amplification",
            "ready p50 s",
            "ready p99 s",
            "first byte p50 ms",
            "first byte p99 ms",
            "errors",
        ],
        &rows,
    )
}

/// The table of the demand reads: a row for each run and length class (up to and including
/// 1 MiB, or more), with the clients loading the store during the run (the largest number its
/// reads name), the reads' count and their wait's median, 99th percentile and largest
/// (milliseconds).
#[must_use]
pub fn demand_table(reads: &[DemandRead]) -> String {
    let mut groups: BTreeMap<(&str, bool), (u32, Vec<u64>)> = BTreeMap::new();
    for r in reads {
        let e = groups
            .entry((r.run.as_str(), r.len > 1_048_576))
            .or_default();
        e.0 = e.0.max(r.load_clients);
        e.1.push(r.wait_us);
    }
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|((run, big), (load, waits))| {
            let s = Summary::of(waits);
            vec![
                (*run).to_owned(),
                load.to_string(),
                (if *big { ">1MiB" } else { "<=1MiB" }).to_owned(),
                s.n.to_string(),
                ms1(s.p50),
                ms1(s.p99),
                ms1(s.max),
            ]
        })
        .collect();
    table(
        &[
            "run",
            "load clients",
            "read length",
            "reads",
            "wait p50 ms",
            "wait p99 ms",
            "wait max ms",
        ],
        &rows,
    )
}

/// `bytes` in GiB with two decimals.
fn gib(bytes: u64) -> String {
    format!("{:.2}", to_f64(bytes) / GIB)
}

/// The table of the runs: a row for each run name with its mode, the `verified` and
/// `drive_streams` of its configuration (`-` when absent, and `-` for drive streams of the shipped
/// way), the most clients, the distinct trees, the median over its clients of the pack GETs (all
/// kinds) and of the manifest GETs, the tree-ready time at its median and 99th percentile
/// (seconds), the GiB needed and fetched summed over the clients, the amplification of those sums,
/// the first-byte time (milliseconds: the median of the clients' medians and the largest of their
/// 99th percentiles) and the sum of the failed GETs.
#[must_use]
pub fn runs_table(runs: &[ClientRun]) -> String {
    let mut groups: BTreeMap<&str, Vec<&ClientRun>> = BTreeMap::new();
    for r in runs {
        groups.entry(r.run.as_str()).or_default().push(r);
    }
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|(run, rs)| {
            let first = rs[0];
            let col = |f: fn(&ClientRun) -> u64| -> Vec<u64> { rs.iter().map(|r| f(r)).collect() };
            let config = |key: &str| first.config.get(key).cloned().unwrap_or_else(|| "-".into());
            let streams = match first.mode {
                Mode::Engine => config("drive_streams"),
                Mode::Shipped => "-".to_owned(),
            };
            let trees: BTreeSet<&str> = rs.iter().map(|r| r.tree.as_str()).collect();
            let ready = Summary::of(&col(|r| r.tree_ready_us));
            let needed: u64 = col(|r| r.bytes_needed).iter().sum();
            let fetched: u64 = col(|r| r.bytes_fetched).iter().sum();
            let first_p50 = median(&rs.iter().map(|r| r.first_byte_us.p50).collect::<Vec<u64>>());
            let first_p99 = rs.iter().map(|r| r.first_byte_us.p99).max().unwrap_or(0);
            vec![
                (*run).to_owned(),
                mode_name(first.mode).to_owned(),
                config("verified"),
                streams,
                rs.iter().map(|r| r.clients).max().unwrap_or(0).to_string(),
                trees.len().to_string(),
                median(&col(ClientRun::total_gets)).to_string(),
                median(&col(|r| r.manifests)).to_string(),
                secs2(ready.p50),
                secs2(ready.p99),
                gib(needed),
                gib(fetched),
                ratio2(milli(fetched, needed)),
                ms1(first_p50),
                ms1(first_p99),
                col(|r| r.get_errors).iter().sum::<u64>().to_string(),
            ]
        })
        .collect();
    table(
        &[
            "run",
            "mode",
            "verified",
            "drive streams",
            "clients",
            "trees",
            "pack GETs per tree p50",
            "manifest GETs per tree p50",
            "ready p50 s",
            "ready p99 s",
            "needed GiB",
            "fetched GiB",
            "amplification",
            "first byte p50 ms",
            "first byte p99 ms",
            "errors",
        ],
        &rows,
    )
}

/// The value at the `pct`-th percentile of `v` by nearest rank (0 for none).
fn percentile(v: &[f64], pct: usize) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut sorted = v.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (pct * sorted.len()).div_ceil(100).clamp(1, sorted.len());
    sorted[rank - 1]
}

/// The mean of `v` (0 for none).
fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / to_f64(v.len() as u64)
    }
}

/// The table of the drives: a row for each run, node and device, over every interval sampled,
/// with the intervals' count, the read wait's median and 99th percentile (milliseconds), the
/// queue length's 99th percentile, the mean utilisation (percent) and the mean read rate
/// (MiB/s). With `devices` not empty, only those devices are listed.
#[must_use]
pub fn disks_table(samples: &[DiskSample], devices: &[String]) -> String {
    let mut groups: BTreeMap<(&str, &str, &str), Vec<&DiskSample>> = BTreeMap::new();
    for s in samples
        .iter()
        .filter(|s| devices.is_empty() || devices.contains(&s.device))
    {
        groups
            .entry((s.run.as_str(), s.node.as_str(), s.device.as_str()))
            .or_default()
            .push(s);
    }
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|((run, node, device), ss)| {
            let col = |f: fn(&DiskSample) -> f64| -> Vec<f64> { ss.iter().map(|s| f(s)).collect() };
            let wait = col(|s| s.r_await_ms);
            vec![
                (*run).to_owned(),
                (*node).to_owned(),
                (*device).to_owned(),
                ss.len().to_string(),
                format!("{:.2}", percentile(&wait, 50)),
                format!("{:.2}", percentile(&wait, 99)),
                format!("{:.2}", percentile(&col(|s| s.aqu_sz), 99)),
                format!("{:.1}", mean(&col(|s| s.util_pct))),
                format!("{:.2}", mean(&col(|s| s.rkb_s)) / 1024.0),
            ]
        })
        .collect();
    table(
        &[
            "run",
            "node",
            "device",
            "intervals",
            "r_await p50 ms",
            "r_await p99 ms",
            "aqu-sz p99",
            "util mean %",
            "MiB/s mean",
        ],
        &rows,
    )
}

/// The table of the drive classes: a row for each run and class (the runs in order, the classes
/// as given; a class is a name and its `(node, device)` pairs), over the intervals of the class's
/// devices, with the devices that have samples, the intervals, the read rate of the class (MiB/s:
/// the sum over its devices of each one's mean), the read wait's median and 99th percentile
/// (milliseconds, over the intervals with reads; `-` when none), the queue length's 99th
/// percentile and the utilisation's mean and 99th percentile (percent). Samples of a device in no
/// class are left out.
#[must_use]
pub fn drive_classes_table(
    samples: &[DiskSample],
    classes: &[(String, Vec<(String, String)>)],
) -> String {
    let class_of = |s: &DiskSample| -> Option<usize> {
        classes.iter().position(|(_, devices)| {
            devices
                .iter()
                .any(|(node, device)| *node == s.node && *device == s.device)
        })
    };
    let mut groups: BTreeMap<&str, Vec<Vec<&DiskSample>>> = BTreeMap::new();
    for s in samples {
        if let Some(i) = class_of(s) {
            groups
                .entry(s.run.as_str())
                .or_insert_with(|| vec![Vec::new(); classes.len()])[i]
                .push(s);
        }
    }
    let mut rows: Vec<Vec<String>> = Vec::new();
    for (run, per_class) in &groups {
        for ((name, _), ss) in classes.iter().zip(per_class) {
            let mut devices: BTreeMap<(&str, &str), Vec<f64>> = BTreeMap::new();
            for s in ss {
                devices
                    .entry((s.node.as_str(), s.device.as_str()))
                    .or_default()
                    .push(s.rkb_s);
            }
            let rate = devices.values().fold(0.0, |sum, v| sum + mean(v) / 1024.0);
            let waits: Vec<f64> = ss
                .iter()
                .filter(|s| s.reads > 0)
                .map(|s| s.r_await_ms)
                .collect();
            let wait = |pct| {
                if waits.is_empty() {
                    "-".to_owned()
                } else {
                    format!("{:.1}", percentile(&waits, pct))
                }
            };
            let col = |f: fn(&DiskSample) -> f64| -> Vec<f64> { ss.iter().map(|s| f(s)).collect() };
            let util = col(|s| s.util_pct);
            rows.push(vec![
                (*run).to_owned(),
                name.clone(),
                devices.len().to_string(),
                ss.len().to_string(),
                format!("{rate:.1}"),
                wait(50),
                wait(99),
                format!("{:.2}", percentile(&col(|s| s.aqu_sz), 99)),
                format!("{:.1}", mean(&util)),
                format!("{:.1}", percentile(&util, 99)),
            ]);
        }
    }
    table(
        &[
            "run",
            "class",
            "devices",
            "intervals",
            "read MiB/s",
            "r_await p50 ms",
            "r_await p99 ms",
            "aqu-sz p99",
            "util mean %",
            "util p99 %",
        ],
        &rows,
    )
}

/// The drive class of a `--class` argument, `NAME=node:dev,node:dev,...`.
///
/// # Errors
/// [`Error::Usage`] when the argument has no name, no device, or a device that is not
/// `<node>:<device>`.
pub fn parse_class(arg: &str) -> Result<(String, Vec<(String, String)>), Error> {
    let bad = |why: &str| Error::Usage(format!("--class {arg:?}: {why}"));
    let (name, list) = arg
        .split_once('=')
        .filter(|(name, _)| !name.is_empty())
        .ok_or_else(|| bad("expected NAME=node:dev,node:dev,..."))?;
    let devices = list
        .split(',')
        .map(|d| {
            d.split_once(':')
                .filter(|(node, dev)| !node.is_empty() && !dev.is_empty())
                .map(|(node, dev)| (node.to_owned(), dev.to_owned()))
                .ok_or_else(|| bad("a device is not <node>:<device>"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((name.to_owned(), devices))
}

/// A metric's change: a whole number without decimals, anything else with three.
fn number(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.3}")
    }
}

/// The table of the nodes' metrics: a row for each node and series (name and labels) with the
/// sum of its changes over the runs and that sum per second of the summed windows. With `names`
/// not empty, only the series of those names are listed.
#[must_use]
pub fn metrics_table(deltas: &[MetricDelta], names: &[String]) -> String {
    let mut groups: BTreeMap<(&str, String), (f64, u64)> = BTreeMap::new();
    for d in deltas
        .iter()
        .filter(|d| names.is_empty() || names.contains(&d.name))
    {
        let e = groups
            .entry((d.node.as_str(), format!("{}{}", d.name, d.labels)))
            .or_default();
        e.0 += d.delta;
        e.1 += d.window_ms;
    }
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|((node, series), (delta, window_ms))| {
            let per_s = if *window_ms == 0 {
                0.0
            } else {
                delta * 1000.0 / to_f64(*window_ms)
            };
            vec![
                (*node).to_owned(),
                series.clone(),
                number(*delta),
                format!("{per_s:.2}"),
            ]
        })
        .collect();
    table(&["node", "series", "delta", "per second"], &rows)
}

/// `planned` over `needed` in thousandths (0 when nothing is needed).
fn milli(planned: u64, needed: u64) -> u64 {
    planned
        .saturating_mul(1000)
        .checked_div(needed)
        .unwrap_or(0)
}

/// Thousandths as a ratio with two decimals.
fn ratio2(milli: u64) -> String {
    format!("{:.2}", to_f64(milli) / 1000.0)
}

/// `part` of `whole` in percent with one decimal (0.0 of nothing).
fn percent1(part: u64, whole: u64) -> String {
    let share = (part.saturating_mul(1000)).checked_div(whole).unwrap_or(0);
    format!("{:.1}", to_f64(share) / 10.0)
}

/// The table of the plans: a row for each variant with its number of trees, the amplification
/// of all its trees together (bytes planned over bytes needed), the amplification of single trees
/// at the 50th, 90th and 99th percentile (nearest rank) and the largest, the trees above 1.50
/// (count and percent of the trees) and the share of packs fetched whole.
#[must_use]
pub fn plan_table(records: &[PlanRecord]) -> String {
    let mut groups: BTreeMap<&str, Vec<&PlanRecord>> = BTreeMap::new();
    for r in records {
        groups.entry(r.variant.as_str()).or_default().push(r);
    }
    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|(variant, rs)| {
            let sum = |f: fn(&PlanRecord) -> u64| rs.iter().map(|r| f(r)).sum::<u64>();
            let mut per_tree: Vec<u64> = rs
                .iter()
                .map(|r| milli(r.bytes_planned, r.bytes_needed))
                .collect();
            per_tree.sort_unstable();
            let trees = per_tree.len() as u64;
            let above = per_tree.iter().filter(|m| **m > 1500).count() as u64;
            vec![
                (*variant).to_owned(),
                trees.to_string(),
                ratio2(milli(sum(|r| r.bytes_planned), sum(|r| r.bytes_needed))),
                ratio2(nearest_rank(&per_tree, 50)),
                ratio2(nearest_rank(&per_tree, 90)),
                ratio2(nearest_rank(&per_tree, 99)),
                ratio2(per_tree[per_tree.len() - 1]),
                format!("{above} ({}%)", percent1(above, trees)),
                format!("{}%", percent1(sum(|r| r.packs_whole), sum(|r| r.packs))),
            ]
        })
        .collect();
    table(
        &[
            "variant",
            "trees",
            "aggregate amplification",
            "tree p50",
            "tree p90",
            "tree p99",
            "tree max",
            "trees above 1.50",
            "packs whole",
        ],
        &rows,
    )
}

/// The four tables under their headings, for the results document.
#[must_use]
pub fn render(
    runs: &[ClientRun],
    reads: &[DemandRead],
    samples: &[DiskSample],
    deltas: &[MetricDelta],
) -> String {
    let mut out = String::new();
    for (heading, body) in [
        ("Trees", trees_table(runs)),
        ("Demand reads", demand_table(reads)),
        ("Drives", disks_table(samples, &[])),
        ("Node metrics", metrics_table(deltas, &[])),
    ] {
        // Writing to a String does not fail.
        let _ = write!(out, "### {heading}\n\n{body}\n");
    }
    out
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod tests;
