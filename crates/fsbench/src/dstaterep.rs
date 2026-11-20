//! `fsbench dstate-report`: the tables of the real-build run, from the nodes' samples read by
//! [`crate::dstate`].
//!
//! A node's ticks fall in three phases: `before` the build (a tick before its start), `build`
//! (from its start to its end) and `after`. Every figure is over the ticks of one phase:
//!
//! - a count's mean, its 99th percentile (nearest rank) and its most;
//! - *FUSE wait %*: of the job threads doing or waiting for work in those ticks (running, in D
//!   state, or asleep on FUSE: [`Tick::job_active`]), the share that waited on FUSE; split into
//!   requests, pages and other waits, which add up to it;
//! - *D %*: of the same, the share in D state.
//!
//! A share is thread-samples over thread-samples: one thread waiting for two ticks counts twice,
//! as two threads waiting for one tick do. It is the share of the build's thread time that the
//! samples saw waiting, within what one sample per tick can see.

use std::fmt::Write as _;

use crate::dstate::{NodeRun, RunEntry, Tick};
use crate::report::number;

/// The phases, in order.
const PHASES: [&str; 3] = ["before", "build", "after"];

/// The phase of `tick` in a build of `build_ms`.
fn phase(tick: &Tick, build_ms: u64) -> &'static str {
    if tick.t_ms < 0 {
        PHASES[0]
    } else if u64::try_from(tick.t_ms).unwrap_or(u64::MAX) <= build_ms {
        PHASES[1]
    } else {
        PHASES[2]
    }
}

/// The mean of `values`, or `-` when there are none.
fn mean(values: &[f64]) -> String {
    if values.is_empty() {
        return "-".to_owned();
    }
    number(values.iter().sum::<f64>() / values.len() as f64)
}

/// The value at quantile `q` (nearest rank) of `values`, or `-` when there are none.
fn rank(values: &[f64], q: f64) -> String {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n == 0 {
        return "-".to_owned();
    }
    let at = ((q * n as f64).ceil() as usize).clamp(1, n) - 1;
    number(sorted[at])
}

/// `part` of `whole` in percent, or `-` when `whole` is 0.
fn share(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "-".to_owned();
    }
    number(part as f64 * 100.0 / whole as f64)
}

/// The figures of a set of ticks.
struct Figures<'a> {
    ticks: Vec<&'a Tick>,
}

impl Figures<'_> {
    fn of(&self, figure: impl Fn(&Tick) -> Option<f64>) -> Vec<f64> {
        self.ticks.iter().filter_map(|&t| figure(t)).collect()
    }

    fn sum(&self, figure: fn(&Tick) -> u32) -> u64 {
        self.ticks.iter().map(|&t| u64::from(figure(t))).sum()
    }

    /// FUSE wait %, its three parts and D %, over the job threads doing or waiting for work.
    fn shares(&self) -> [String; 5] {
        let active = self.sum(Tick::job_active);
        [
            share(self.sum(Tick::job_fuse), active),
            share(self.sum(|t| t.job_fuse_request), active),
            share(self.sum(|t| t.job_fuse_page), active),
            share(self.sum(|t| t.job_fuse_other), active),
            share(self.sum(|t| t.job_d), active),
        ]
    }
}

fn count(field: fn(&Tick) -> u32) -> impl Fn(&Tick) -> Option<f64> {
    move |t| Some(f64::from(field(t)))
}

/// The tables of `runs`: each run's entry of the index and its nodes.
#[must_use]
pub fn render(runs: &[(RunEntry, Vec<NodeRun>)]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "| run | build s | nodes | kernels | settings |");
    let _ = writeln!(out, "|---|---|---|---|---|");
    for (entry, nodes) in runs {
        let mut kernels: Vec<&str> = nodes.iter().map(|n| n.kernel.as_str()).collect();
        kernels.sort_unstable();
        kernels.dedup();
        let settings: Vec<String> = entry
            .settings
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let build_ms = entry.build_end_ns.saturating_sub(entry.build_start_ns) / 1_000_000;
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            entry.run,
            number(build_ms as f64 / 1000.0),
            nodes.len(),
            kernels.join(", "),
            settings.join(", ")
        );
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "| run | node | phase | ticks | load mean | jobs mean | job threads mean | D mean | D max | \
         fuse_d mean | fuse_d p99 | fuse_d max | D fault max | waiting mean | waiting p99 | \
         waiting max | FUSE wait % | D % |"
    );
    let _ = writeln!(out, "|---|---|---|{}", "---|".repeat(15));
    for node in runs.iter().flat_map(|(_, nodes)| nodes) {
        for name in PHASES {
            let figures = Figures {
                ticks: node
                    .ticks
                    .iter()
                    .filter(|t| phase(t, node.build_ms) == name)
                    .collect(),
            };
            if figures.ticks.is_empty() {
                continue;
            }
            let fuse_d = figures.of(count(|t| t.fuse_d));
            let d = figures.of(count(|t| t.d));
            let waiting = figures.of(|t| t.waiting.map(|w| w as f64));
            let [fuse, _, _, _, d_share] = figures.shares();
            let _ = writeln!(
                out,
                "| {} | {} | {name} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | \
                 {} | {fuse} | {d_share} |",
                node.run,
                node.node,
                figures.ticks.len(),
                mean(&figures.of(|t| Some(t.load1))),
                mean(&figures.of(count(|t| t.jobs))),
                mean(&figures.of(count(|t| t.job_threads))),
                mean(&d),
                rank(&d, 1.0),
                mean(&fuse_d),
                rank(&fuse_d, 0.99),
                rank(&fuse_d, 1.0),
                rank(&figures.of(count(|t| t.d_fault)), 1.0),
                mean(&waiting),
                rank(&waiting, 0.99),
                rank(&waiting, 1.0),
            );
        }
    }
    out.push('\n');
    let _ = writeln!(
        out,
        "| run | job thread samples | FUSE wait % | request % | page % | other % | D % | \
         fuse_d max | waiting max |"
    );
    let _ = writeln!(out, "|---|---|---|---|---|---|---|---|---|");
    for (entry, nodes) in runs {
        let figures = Figures {
            ticks: nodes
                .iter()
                .flat_map(|n| n.ticks.iter().filter(|t| phase(t, n.build_ms) == PHASES[1]))
                .collect(),
        };
        let [fuse, request, page, other, d] = figures.shares();
        let _ = writeln!(
            out,
            "| {} | {} | {fuse} | {request} | {page} | {other} | {d} | {} | {} |",
            entry.run,
            figures.sum(Tick::job_active),
            rank(&figures.of(count(|t| t.fuse_d)), 1.0),
            rank(&figures.of(|t| t.waiting.map(|w| w as f64)), 1.0),
        );
    }
    out
}

#[cfg(test)]
#[path = "dstaterep_tests.rs"]
mod tests;
