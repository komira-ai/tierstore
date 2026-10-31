//! Per-crate totals, the comparison with the threshold, and the table CI prints.
//!
//! A crate is a directory under `<root>/crates` holding a `Cargo.toml`. A crate passes when
//! its line coverage and its branch coverage each reach [`Percent::TARGET`]. A metric with
//! nothing to measure passes, except lines: a crate with no instrumented line at all fails
//! ([`Status::NoData`]), because a crate whose tests stopped being instrumented would
//! otherwise slip past the gate. A crate named as excluded is listed and not judged
//! ([`Status::Excluded`]); naming one the workspace does not have fails ([`Status::Stale`]),
//! so an exclusion does not outlive its crate.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::io;
use std::path::{Path, PathBuf};

use crate::lcov::FileRecord;
use crate::{Counts, CrateCoverage, Percent, show};

/// The workspace's crates: the directories under `<root>/crates` with a `Cargo.toml`.
pub fn workspace_crates(root: &Path) -> io::Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for entry in std::fs::read_dir(root.join("crates"))? {
        let entry = entry?;
        if entry.path().join("Cargo.toml").is_file() {
            out.insert(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(out)
}

/// A tracefile source that is not inside a workspace crate.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not under <root>/crates/<crate>/ for any workspace crate")]
pub struct Unattributed(pub PathBuf);

/// Sums the records per crate. Every crate in `crates` gets an entry, zero when no
/// record names it; a record outside every crate is an error rather than dropped.
pub fn by_crate(
    root: &Path,
    crates: &BTreeSet<String>,
    files: &[FileRecord],
) -> Result<BTreeMap<String, CrateCoverage>, Unattributed> {
    let base = root.join("crates");
    let mut out: BTreeMap<String, CrateCoverage> = crates
        .iter()
        .map(|c| (c.clone(), CrateCoverage::default()))
        .collect();
    for f in files {
        let total = f
            .path
            .strip_prefix(&base)
            .ok()
            .filter(|rest| rest.components().count() > 1)
            .and_then(|rest| rest.iter().next())
            .and_then(|name| out.get_mut(name.to_str()?));
        let Some(total) = total else {
            return Err(Unattributed(f.path.clone()));
        };
        *total = total.plus(f.coverage);
    }
    Ok(out)
}

/// Lines or branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Lines,
    Branches,
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Metric::Lines => "lines",
            Metric::Branches => "branches",
        })
    }
}

/// How one crate stands against the threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Every metric meets the threshold.
    Ok,
    /// These metrics are under the threshold: fails.
    Below(Vec<Metric>),
    /// No line of the crate was instrumented: fails.
    NoData,
    /// Named as excluded: not judged.
    Excluded,
    /// An exclusion for a crate the workspace does not have: fails.
    Stale,
}

impl Status {
    pub fn fails(&self) -> bool {
        !matches!(self, Status::Ok | Status::Excluded)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Ok => f.write_str("ok"),
            Status::Below(m) => {
                let names: Vec<String> = m.iter().map(Metric::to_string).collect();
                write!(f, "BELOW THRESHOLD ({})", names.join(", "))
            }
            Status::NoData => f.write_str("NOTHING MEASURED"),
            Status::Excluded => f.write_str("excluded"),
            Status::Stale => f.write_str("EXCLUDED BUT NOT IN WORKSPACE"),
        }
    }
}

/// One crate: what was measured (`None` if it is not in the workspace) and whether it is
/// excluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub krate: String,
    pub measured: Option<CrateCoverage>,
    pub excluded: bool,
}

impl Row {
    pub fn status(&self) -> Status {
        let Some(measured) = self.measured else {
            return Status::Stale;
        };
        if self.excluded {
            return Status::Excluded;
        }
        if measured.lines.found() == 0 {
            return Status::NoData;
        }
        let below: Vec<Metric> = [
            (Metric::Lines, measured.lines),
            (Metric::Branches, measured.branches),
        ]
        .into_iter()
        .filter(|(_, counts)| !counts.meets(Percent::TARGET))
        .map(|(metric, _)| metric)
        .collect();
        if below.is_empty() {
            Status::Ok
        } else {
            Status::Below(below)
        }
    }
}

/// Every crate in the workspace or the exclusions, in name order.
pub fn rows(measured: &BTreeMap<String, CrateCoverage>, excluded: &BTreeSet<String>) -> Vec<Row> {
    let names: BTreeSet<&String> = measured.keys().chain(excluded.iter()).collect();
    names
        .into_iter()
        .map(|k| Row {
            krate: k.clone(),
            measured: measured.get(k).copied(),
            excluded: excluded.contains(k),
        })
        .collect()
}

/// `hit/found` and the percentage for one metric; `-` for both where nothing was measured.
fn cells(c: Counts) -> [String; 2] {
    if c.found() == 0 {
        return ["-".into(), "-".into()];
    }
    [format!("{}/{}", c.hit(), c.found()), show(c.percent())]
}

/// The report as a Markdown table (readable in a log, rendered in a job summary), with a
/// workspace total and a closing verdict line.
pub fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    out.push_str(
        "| crate | lines | line % | branches | branch % | status |\n\
         |---|---:|---:|---:|---:|---|\n",
    );
    let mut line = |name: &str, m: CrateCoverage, status: String| {
        let [l, lp] = cells(m.lines);
        let [b, bp] = cells(m.branches);
        // Writing to a String cannot fail.
        let _ = writeln!(out, "| {name} | {l} | {lp} | {b} | {bp} | {status} |");
    };
    let mut total = CrateCoverage::default();
    let mut failing = 0;
    for r in rows {
        let status = r.status();
        failing += usize::from(status.fails());
        let measured = r.measured.unwrap_or_default();
        total = total.plus(measured);
        line(&r.krate, measured, status.to_string());
    }
    line("**workspace**", total, String::new());
    let _ = write!(
        out,
        "\n{}\n",
        if failing == 0 {
            format!(
                "coverage gate: PASS (threshold {} of lines and branches)",
                Percent::TARGET
            )
        } else {
            format!("coverage gate: FAIL ({failing} crate(s) need attention; see status)")
        }
    );
    out
}

#[cfg(test)]
mod tests;
