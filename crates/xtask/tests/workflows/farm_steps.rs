//! Two rules for the steps that reach the build farm, held in every workflow by [`problems`].
//!
//! - No `run:` runs buck2's `build`, `test`, `bxl` or `run` itself. A buck2 build goes through
//!   komira's buck2-farm action ([`BUCK2_FARM`]): buck2 on its own waits for ever on a connection
//!   to the farm that died silently, while the action's watchdog ends such a run (when the
//!   runner's connections to the farm have heard nothing for 5 minutes) and runs it again. A run is
//!   read as shell words, split at `;`, `|`, `&`, `(`, `)` and backquotes. A word that is `buck2`
//!   or ends in `/buck2`, quotes aside, is buck2, and its first word after it that is not a flag
//!   is the subcommand. So `buck2 kill`, `buck2 audit ...` and a path to buck2 passed as a flag's
//!   value are allowed.
//! - A workflow that reaches the farm ([`reaches_farm`]: a step uses farm-connect
//!   ([`FARM_CONNECT`]) or buck2-farm, or runs `cargo xtask mac-farm`) gives every job that has
//!   steps a whole-number `timeout-minutes`, and every step of it one of its own, from 1 and under
//!   the job's. A step that hangs (on the farm, or not) then ends at its own bound, with the job's
//!   remaining steps and its post steps still run, and never holds the job to its own limit.

use super::lint::{entries, entry};
use super::yaml::{Node, Value};

/// komira's action that joins the farm's network and writes the farm's buckconfig.
pub const FARM_CONNECT: &str = "komira-ai/komira/.github/actions/farm-connect";
/// komira's action that runs one buck2 command on the farm, with its watchdog.
pub const BUCK2_FARM: &str = "komira-ai/komira/.github/actions/buck2-farm";
/// The run of a step that sends one action to a farm Mac (crates/xtask/src/mac_farm.rs).
pub const MAC_FARM_RUN: &str = "cargo xtask mac-farm";
/// The subcommands of buck2 that build, and so reach the farm.
pub const BUILDING: [&str; 4] = ["build", "test", "bxl", "run"];

/// The steps of `job`.
fn steps(job: &Node) -> &[Node] {
    match entry(job, "steps").map(|s| &s.value) {
        Some(Value::Seq(items)) => items,
        _ => &[],
    }
}

/// The jobs of `doc`, by name.
fn jobs(doc: &Node) -> impl Iterator<Item = (&str, &Node)> {
    entry(doc, "jobs")
        .map_or(&[][..], entries)
        .iter()
        .map(|(name, job)| (name.as_str().unwrap_or_default(), job))
}

/// Whether the `uses:` value `value` is the action `action` at some ref.
fn is_action(value: &str, action: &str) -> bool {
    value
        .strip_prefix(action)
        .is_some_and(|rest| rest.starts_with('@'))
}

/// Whether a step of `doc` uses farm-connect or buck2-farm, or runs [`MAC_FARM_RUN`].
pub fn reaches_farm(doc: &Node) -> bool {
    jobs(doc).flat_map(|(_, job)| steps(job)).any(|step| {
        let uses = entry(step, "uses").and_then(Node::as_str);
        let run = entry(step, "run").and_then(Node::as_str);
        uses.is_some_and(|u| is_action(u, FARM_CONNECT) || is_action(u, BUCK2_FARM))
            || run.is_some_and(|r| r.contains(MAC_FARM_RUN))
    })
}

/// The buck2 subcommand in [`BUILDING`] that the shell text `run` runs, if any.
pub fn buck2_build(run: &str) -> Option<&'static str> {
    let words: Vec<&str> = run
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '`'))
        .map(|w| w.trim_matches(|c| c == '"' || c == '\''))
        .collect();
    words.iter().enumerate().find_map(|(at, word)| {
        if *word != "buck2" && !word.ends_with("/buck2") {
            return None;
        }
        let sub = words[at + 1..]
            .iter()
            .find(|w| !w.is_empty() && !w.starts_with('-'))?;
        BUILDING.iter().copied().find(|b| b == sub)
    })
}

/// How `step`, the `index`th of its job (from 1), is named in a problem.
fn label(step: &Node, index: usize) -> String {
    let named = ["name", "uses", "run"]
        .iter()
        .find_map(|key| entry(step, key).and_then(Node::as_str));
    match named {
        Some(name) => format!("step {index} ({})", name.lines().next().unwrap_or_default()),
        None => format!("step {index}"),
    }
}

/// The whole number of minutes of `node`'s `timeout-minutes`; `None` when it has none, or one
/// that is not a whole number written in digits.
fn minutes(node: &Node) -> Option<u64> {
    entry(node, "timeout-minutes")
        .and_then(Node::as_str)
        .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|t| t.parse().ok())
}

/// One line per step of `doc` that runs buck2 itself and, when `doc` reaches the farm, per job or
/// step without its own bound.
pub fn problems(doc: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, job) in jobs(doc) {
        for (index, step) in steps(job).iter().enumerate() {
            let run = entry(step, "run")
                .and_then(Node::as_str)
                .unwrap_or_default();
            if let Some(sub) = buck2_build(run) {
                problems.push(format!(
                    "job {name}, {}: runs `buck2 {sub}` itself; a buck2 build goes through {BUCK2_FARM}, whose watchdog ends a run hung on a dead connection to the farm",
                    label(step, index + 1)
                ));
            }
        }
    }
    if reaches_farm(doc) {
        problems.extend(bound_problems(doc));
    }
    problems
}

/// One line per job with steps and no whole-number `timeout-minutes`, and per step of a job
/// without one of its own from 1 and under the job's.
fn bound_problems(doc: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, job) in jobs(doc).filter(|(_, job)| !steps(job).is_empty()) {
        let Some(limit) = minutes(job) else {
            problems.push(format!(
                "job {name}: reaches the farm with no whole-number timeout-minutes"
            ));
            continue;
        };
        for (index, step) in steps(job).iter().enumerate() {
            let step_label = label(step, index + 1);
            match minutes(step) {
                Some(m) if (1..limit).contains(&m) => {}
                Some(m) => problems.push(format!(
                    "job {name}, {step_label}: timeout-minutes {m} is not from 1 and under the job's {limit}"
                )),
                None => problems.push(format!(
                    "job {name}, {step_label}: no whole-number timeout-minutes of its own"
                )),
            }
        }
    }
    problems
}
