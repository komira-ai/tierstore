//! `fsbench summary`: one table of every variant of the FUSE benchmark, from the results files of
//! several measurements (`docs/design/fetch-cache-serve.md`, 17.1: the result tables).
//!
//! Each measurement document has its own tables (`fsbench report`); this one puts the figures
//! the decision rules of 17.1 read side by side: the cycles' rate and tails, the whole-file read
//! rates, and the server's CPU per 1000 cycles and per GB of cold reads. A row is one series of
//! one file, its runs grouped by variant and regime as `fsbench report` groups them, the first
//! `discard` left out, every figure `median (min-max)` of the runs kept. The first column names
//! the file (its name without its extension), as the same label (the `null` control, `v3-r8`) was run
//! again in later measurements.

use std::fmt::Write as _;

use crate::hist::Histogram;
use crate::record::RunRecord;
use crate::report::cell;

/// A figure of a run.
type Figure = fn(&RunRecord) -> Option<f64>;

fn p99_ms(h: &Histogram) -> Option<f64> {
    Some(h.quantile(0.99) as f64 / 1e6)
}

/// The columns, after the file, the variant and the regime.
const COLUMNS: &[(&str, Figure)] = &[
    ("opens/s", |r| Some(r.cycles.per_s)),
    ("stat p99 ms", |r| p99_ms(&r.cycles.stat)),
    ("open p99 ms", |r| p99_ms(&r.cycles.open)),
    ("cold MB/s", |r| Some(r.cold.mb_per_s)),
    ("warm MB/s", |r| Some(r.warm.mb_per_s)),
    ("server CPU ms/1k", |r| Some(r.cpu.server_ms_per_1k)),
    ("cold server CPU ms/GB", |r| {
        Some(r.cold.cpu.as_ref()?.server_ms_per_gb)
    }),
];

/// The table of `files`: each a name and its records, in order, leaving out the first `discard`
/// runs of each series.
#[must_use]
pub fn render(files: &[(String, Vec<RunRecord>)], discard: u32) -> String {
    let mut out = String::new();
    let names: Vec<&str> = COLUMNS.iter().map(|(name, _)| *name).collect();
    let _ = writeln!(
        out,
        "| measurement | variant | regime | {} |",
        names.join(" | ")
    );
    let _ = writeln!(out, "|---|---|---|{}", "---|".repeat(COLUMNS.len()));
    for (name, records) in files {
        let mut series: Vec<((&str, &str), Vec<&RunRecord>)> = Vec::new();
        for record in records {
            let key = (record.variant.as_str(), record.regime.as_str());
            let at = match series.iter().position(|(k, _)| *k == key) {
                Some(at) => at,
                None => {
                    series.push((key, Vec::new()));
                    series.len() - 1
                }
            };
            if record.run >= discard {
                series[at].1.push(record);
            }
        }
        for ((variant, regime), runs) in &series {
            let cells: Vec<String> = COLUMNS
                .iter()
                .map(|(_, figure)| cell(runs.iter().filter_map(|r| figure(r)).collect()))
                .collect();
            let _ = writeln!(
                out,
                "| {name} | {variant} | {regime} | {} |",
                cells.join(" | ")
            );
        }
    }
    out
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
