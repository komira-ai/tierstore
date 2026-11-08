//! `channels decide`: whether a release that is in `testing` goes on to the public channels.
//!
//! The release was published to testing (`releases/<V>/` without `latest.json`, the apt suite
//! `testing`), installed from there on clean machines, and put on the lab through hostkeeper when
//! the lab is configured. The job `promote-check` runs this rule over the results of those jobs
//! (`--result NAME=RESULT`, the `needs.<job>.result` of each) and of the farm's setting
//! (`--farm-enabled`, the output of `farm-check`), and `promote=true` is what the jobs that make
//! the release public wait for.
//!
//! | stage | result that lets the release go on |
//! |---|---|
//! | `testing-upload`, `testing-apt-write` | `success` (a job without its settings ends green) |
//! | `testing-apt-write-2`, `testing-apt-write-3` | `success`, or `skipped` (the first write did not lose the race) |
//! | `check-apt`, `check-brew`, `check-install-sh` | `success`, or `skipped` (nothing was published to check) |
//! | `farm-check` | `success` |
//! | `farm`, `soak` | both `success` when the lab is configured; both `skipped` when it is not (a notice, and the release goes on) |
//!
//! Every other result, a stage not given, a result that is not one of `success`, `failure`,
//! `cancelled` and `skipped`, and a lab that is half on (`farm` or `soak` run for a lab that is not
//! configured, or skipped for one that is) is a refusal that names the stage; the job ends red.

use std::path::Path;

use crate::Error;
use crate::release::append_output;

/// The stages the rule reads, in the order it names them.
pub const STAGES: [&str; 10] = [
    "testing-upload",
    "testing-apt-write",
    "testing-apt-write-2",
    "testing-apt-write-3",
    "check-apt",
    "check-brew",
    "check-install-sh",
    "farm-check",
    "farm",
    "soak",
];

/// The results `needs.<job>.result` takes.
const RESULTS: [&str; 4] = ["success", "failure", "cancelled", "skipped"];

/// The stages that may be skipped.
const MAY_SKIP: [&str; 5] = [
    "testing-apt-write-2",
    "testing-apt-write-3",
    "check-apt",
    "check-brew",
    "check-install-sh",
];

fn parse(results: &[String]) -> Result<Vec<(&str, &str)>, Error> {
    let mut parsed: Vec<(&str, &str)> = Vec::new();
    for result in results {
        let Some((name, value)) = result.split_once('=') else {
            return Err(Error::Usage(format!(
                "--result {result:?} is not NAME=RESULT"
            )));
        };
        if !STAGES.contains(&name) {
            return Err(Error::Usage(format!(
                "--result names {name:?}, which is no stage"
            )));
        }
        if !RESULTS.contains(&value) {
            return Err(Error::Usage(format!(
                "--result {name} is {value:?}, not one of {RESULTS:?}"
            )));
        }
        if parsed.iter().any(|(n, _)| *n == name) {
            return Err(Error::Usage(format!("--result {name} is given twice")));
        }
        parsed.push((name, value));
    }
    if let Some(missing) = STAGES.iter().find(|s| !parsed.iter().any(|(n, _)| n == *s)) {
        return Err(Error::Usage(format!("--result {missing} is not given")));
    }
    Ok(parsed)
}

/// The `decide` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, or one refusal per stage whose result does not let the release go on.
pub fn decide(
    results: &[String],
    farm_enabled: &str,
    output: Option<&Path>,
) -> Result<Vec<String>, Error> {
    if !["true", "false", ""].contains(&farm_enabled) {
        return Err(Error::Usage(format!(
            "--farm-enabled {farm_enabled:?} is not true, false or empty"
        )));
    }
    let parsed = parse(results)?;
    let result_of = |stage: &str| {
        parsed
            .iter()
            .find(|(n, _)| *n == stage)
            .map_or("", |(_, v)| *v)
    };
    let mut problems = Vec::new();
    let mut said = Vec::new();
    for (stage, result) in &parsed {
        if matches!(*stage, "farm" | "soak") {
            continue;
        }
        let allowed = *result == "success" || (*result == "skipped" && MAY_SKIP.contains(stage));
        if !allowed {
            problems.push(format!("{stage} is {result}: the release does not go on"));
        }
    }
    let lab_on = farm_enabled == "true";
    for stage in ["farm", "soak"] {
        let want = if lab_on { "success" } else { "skipped" };
        if result_of(stage) != want {
            problems.push(format!(
                "{stage} is {}: the lab is {}, so it must be {want}",
                result_of(stage),
                if lab_on {
                    "configured"
                } else {
                    "not configured"
                }
            ));
        }
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    if !lab_on {
        said.push("::notice title=The lab stage was skipped::The hostkeeper settings are absent, so the release did not go through the lab; it goes on to the public channels.".to_owned());
    }
    if let Some(output) = output {
        append_output(output, &["promote=true".to_owned()])?;
    }
    said.push("promote: the release is promoted from testing".to_owned());
    Ok(said)
}

#[cfg(test)]
mod tests;
