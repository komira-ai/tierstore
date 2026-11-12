//! The rules for the lab jobs of `release.yml` (`farm-check`, `farm` and `soak`), which move the
//! hostkeeper lab onto a release that is in testing and installed, before the promotion
//! (docs/design/distribution.md, section 5.3). [`farm_problems`] is called from
//! `release_problems`, so the one lint holds the whole file.
//!
//! - `farm-check` waits for the three install checks, reads the repository (`contents: read`, no
//!   environment: its settings are repository variables, which a job that calls a workflow can
//!   read too), has the `if:` of the jobs after skippable jobs, and starts with the gate: the coordinator's two variables, then the
//!   tailnet's three. The gate is also given `--require TIERSTORE_LAB_INSTALL=...`, `true` only
//!   when that repository variable is exactly `true`: with it off, the lab is off whatever the
//!   other settings are (they serve `hostkeeper-desired.yml` too). Every step after the first gate runs only when it said `enabled=true`, and
//!   one of them is `cargo xtask release desired-apt` (the testing suite lists the package), whose two outputs
//!   (`package-version` and `targets`, the apt installer's entry per architecture) the job hands on.
//! - `farm` is hostkeeper's reusable workflow, pinned to a commit with its release as the comment
//!   (a pin of its own: `hostkeeper-desired.yml` pins another revision of the client, and the
//!   coordinator's allow-list has an entry for each, by its audience), called with the version and
//!   the `targets` `farm-check` handed on (never a `package_digest`), at a release of hostkeeper no older than [`APT_FLOOR`], the
//!   coordinator's variables, the audience of this workflow and `wait: true`, and only when the
//!   gate said `enabled=true`. It holds `contents: read` and `id-token: write`; no other job of
//!   the farm does (the lint's write table pins the `if:` of the scope).
//! - `soak` holds `contents: read` alone and runs only when `farm` converged; it runs
//!   `cargo xtask channels soak`.
//! - Every job that makes the release public (`promote-latest`, `tap` and the nine apt and the nine rpm jobs
//!   of the suite `stable`) waits for `promote-check`, which waits for the lab (`soak`) and for the
//!   install checks, so that a promotion without them is named. With no lab configured `farm` and
//!   `soak` are skipped, and `promote-check` accepts exactly that (its own rules are in
//!   `checks.rs`).

use super::apt::JOBS;
use super::channels::{GATE_IF, steps, text};
use super::hostkeeper::at;
use super::lint::{AFTER_IF, entries, entry, names};
use super::rpm::JOBS as RPM_JOBS;
use super::yaml::Node;

/// The `if:` of `farm`: a release, on main, once the gate of `farm-check` said the farm is set up.
pub const FARM_IF: &str = after_if!("needs.farm-check.outputs.enabled == 'true'");

/// The `if:` of `soak`: a release, on main, and `farm` converged.
pub const SOAK_IF: &str = after_if!("needs.farm.outputs.converged == 'true'");

/// The audience of the OIDC token of `farm`, which the coordinator's allow-list entry for this
/// workflow names (not the one of `hostkeeper-desired.yml`, whose entry admits another event).
pub const AUDIENCE: &str = "tierstore-hostkeeper-release";

/// The first release of hostkeeper whose daemon runs the hook command of an `apt` entry (an
/// absolute path of a program the package installs, run from the version's sealed tree): an older
/// daemon refuses the entry, whose hook is `/usr/libexec/tierstore/tierstore update-hook`. The pin
/// of `farm` is at this release or later, and `desired-apt` is given it as `--min-hostkeeper`.
pub const APT_FLOOR: &str = "0.1.225";

/// The repository variable that switches the lab stage on.
const SWITCH: &str = "TIERSTORE_LAB_INSTALL";

/// What the gate's environment holds for it: `true` only when the variable is exactly `true`.
const SWITCH_VALUE: &str = "${{ vars.TIERSTORE_LAB_INSTALL == 'true' && 'true' || 'false' }}";

/// The settings of the coordinator that the first gate is given.
const COORDINATOR_SETTINGS: [&str; 2] = ["HOSTKEEPER_COORDINATOR_URL", "HOSTKEEPER_COORDINATOR_CA"];

/// The settings of the tailnet that the second gate is given: all or none.
const TAILNET_SETTINGS: [&str; 3] = [
    "HOSTKEEPER_TAILNET_CLIENT_ID",
    "HOSTKEEPER_TAILNET_AUDIENCE",
    "HOSTKEEPER_TAILNET_COORDINATOR",
];

/// The job `name` of `doc`.
fn job<'a>(doc: &'a Node, name: &str) -> Option<&'a Node> {
    entry(doc, "jobs")
        .map_or(&[][..], entries)
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

/// The permissions of `job`, as `scope: level`, sorted.
fn held(job: &Node) -> Vec<String> {
    let mut held: Vec<String> = entry(job, "permissions")
        .map_or(&[][..], entries)
        .iter()
        .map(|(k, v)| {
            format!(
                "{}: {}",
                k.as_str().unwrap_or_default(),
                v.as_str().unwrap_or_default()
            )
        })
        .collect();
    held.sort();
    held
}

/// One line per way the farm jobs and the order of the public jobs of the `release.yml` document
/// `doc` (whose text is `text`) are not as the module says.
pub fn farm_problems(doc: &Node, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    match job(doc, "farm-check") {
        Some(found) => problems.extend(check_problems(found)),
        None => problems.push("farm-check: there is no such job".to_owned()),
    }
    match job(doc, "farm") {
        Some(found) => problems.extend(farm_job_problems(found, text)),
        None => problems.push("farm: there is no such job".to_owned()),
    }
    match job(doc, "soak") {
        Some(found) => problems.extend(soak_problems(found)),
        None => problems.push("soak: there is no such job".to_owned()),
    }
    problems.extend(order_problems(doc));
    problems
}

fn check_problems(check: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    if names(entry(check, "needs")) != ["check-apt", "check-brew", "check-install-sh", "plan"] {
        problems.push(
            "farm-check: needs are not plan and the three install checks: the lab is told about a release that was installed from testing"
                .to_owned(),
        );
    }
    if entry(check, "if").and_then(Node::as_str) != Some(AFTER_IF) {
        problems.push(format!("farm-check: `if` is not {AFTER_IF}"));
    }
    if entry(check, "environment").is_some() {
        problems.push(
            "farm-check: names an environment; its settings are repository variables".to_owned(),
        );
    }
    if held(check) != ["contents: read"] {
        problems.push(format!(
            "farm-check: permissions are {:?}, not contents: read alone",
            held(check)
        ));
    }
    for (output, want) in [
        ("enabled", "${{ steps.gate.outputs.enabled }}"),
        ("tailnet", "${{ steps.tailnet.outputs.enabled }}"),
        (
            "package-version",
            "${{ steps.desired.outputs.package-version }}",
        ),
        ("targets", "${{ steps.desired.outputs.targets }}"),
    ] {
        if at(check, &["outputs", output]) != Some(want) {
            problems.push(format!("farm-check: output {output} is not {want}"));
        }
    }
    let all = steps(check);
    let Some(gate) = all.iter().position(|s| at(s, &["id"]) == Some("gate")) else {
        problems.push("farm-check: no step has the id `gate`".to_owned());
        return problems;
    };
    problems.extend(gate_problems(
        &all[gate],
        "farm",
        &COORDINATOR_SETTINGS,
        "farm-check: the gate",
    ));
    problems.extend(switch_problems(&all[gate]));
    for step in &all[..gate] {
        for word in ["vars.HOSTKEEPER", "secrets.", "id-token"] {
            if super::channels::mentions(step, word) {
                problems.push(format!("farm-check: a step before the gate names {word}"));
            }
        }
    }
    for step in &all[gate + 1..] {
        if entry(step, "if").and_then(Node::as_str) != Some(GATE_IF) {
            problems.push(format!(
                "farm-check: the step `{}` after the gate does not have `if: {GATE_IF}`",
                text(step, "name")
            ));
        }
    }
    let after = &all[gate + 1..];
    match after.iter().find(|s| at(s, &["id"]) == Some("tailnet")) {
        Some(tailnet) => problems.extend(gate_problems(
            tailnet,
            "farm-tailnet",
            &TAILNET_SETTINGS,
            "farm-check: the tailnet gate",
        )),
        None => problems.push("farm-check: no step has the id `tailnet`".to_owned()),
    }
    let desired = after.iter().find(|s| at(s, &["id"]) == Some("desired"));
    if !desired.is_some_and(|s| {
        text(s, "run").starts_with(
            "cargo xtask release desired-apt --repo \"$GITHUB_REPOSITORY\" --version \"${TAG#v}\" ",
        ) && text(s, "run").contains("--output \"$GITHUB_OUTPUT\"")
    }) {
        problems.push(
            "farm-check: no step `desired` runs `cargo xtask release desired-apt` for the tag of this release"
                .to_owned(),
        );
    }
    let floor = format!("--min-hostkeeper {APT_FLOOR} ");
    if !desired.is_some_and(|s| format!("{} ", text(s, "run")).contains(&floor)) {
        problems.push(format!(
            "farm-check: the step `desired` is not given `{}`: the entries would name a hostkeeper floor that is not the first with the apt installer",
            floor.trim_end()
        ));
    }
    problems
}

/// The lab is off unless the repository variable `TIERSTORE_LAB_INSTALL` is exactly `true`: the
/// gate step is given that as `--require` (a gate that is not required to be on judges nothing, so
/// the settings of `hostkeeper-desired.yml` alone would start the lab), from an environment value
/// that is `true` only for exactly that, and it is the only `--require` of the step.
fn switch_problems(gate: &Node) -> Vec<String> {
    let run = text(gate, "run");
    let mut problems = Vec::new();
    if !run.contains(&format!("--require \"{SWITCH}=${SWITCH}\"")) {
        problems.push(format!(
            "farm-check: the gate is not given --require \"{SWITCH}=${SWITCH}\": the lab starts without the switch"
        ));
    }
    if run.matches("--require ").count() != 1 {
        problems.push("farm-check: the gate is given a requirement it should not be".to_owned());
    }
    if at(gate, &["env", SWITCH]) != Some(SWITCH_VALUE) {
        problems.push(format!(
            "farm-check: the gate's {SWITCH} is not {SWITCH_VALUE}: anything but exactly `true` must leave the lab off"
        ));
    }
    problems
}

/// The gate step `gate`, labelled `label`, given each of `settings` as `NAME=${NAME:+set}` and no
/// other, and never a value.
fn gate_problems(gate: &Node, label: &str, settings: &[&str], who: &str) -> Vec<String> {
    let run = text(gate, "run");
    let mut problems = Vec::new();
    let start = format!("cargo xtask channels gate --label {label} ");
    if !run.starts_with(&start) {
        problems.push(format!("{who} does not run `{start}...`"));
    }
    for setting in settings {
        let wanted = format!("--setting \"{setting}=${{{setting}:+set}}\"");
        if !run.contains(&wanted) {
            problems.push(format!("{who} is not given {wanted}"));
        }
    }
    if run.matches("--setting ").count() != settings.len() {
        problems.push(format!("{who} is given a setting it should not be"));
    }
    if run.contains("secrets.") || run.contains("vars.") {
        problems.push(format!("{who}'s command line holds a value"));
    }
    problems
}

/// What each input of hostkeeper's workflow must be in `farm`, for the pin `commit` whose version
/// comment says `release`.
fn with_wanted<'a>(commit: &'a str, release: &'a str) -> [(&'static str, &'a str); 15] {
    [
        ("service", "tierstore"),
        ("version", "${{ needs.farm-check.outputs.package-version }}"),
        ("targets", "${{ needs.farm-check.outputs.targets }}"),
        ("hostkeeper_version", release),
        ("hostkeeper_commit", commit),
        ("coordinator_url", "${{ vars.HOSTKEEPER_COORDINATOR_URL }}"),
        ("coordinator_ca", "${{ vars.HOSTKEEPER_COORDINATOR_CA }}"),
        ("audience", AUDIENCE),
        ("wait", "true"),
        ("max_polls", "${{ fromJSON(vars.FARM_MAX_POLLS || '360') }}"),
        (
            "join_tailnet",
            "${{ needs.farm-check.outputs.tailnet == 'true' }}",
        ),
        (
            "tailnet_client_id",
            "${{ vars.HOSTKEEPER_TAILNET_CLIENT_ID }}",
        ),
        (
            "tailnet_audience",
            "${{ vars.HOSTKEEPER_TAILNET_AUDIENCE }}",
        ),
        (
            "tailnet_coordinator",
            "${{ vars.HOSTKEEPER_TAILNET_COORDINATOR }}",
        ),
        ("tailnet_tag", "${{ vars.HOSTKEEPER_TAILNET_TAG }}"),
    ]
}

/// The commit of the pin of the `uses:` of `job` (a node of the document of `text`), and the
/// release its comment names, when it is hostkeeper's workflow.
fn pin<'a>(job: &'a Node, text: &'a str) -> Option<(&'a str, &'a str)> {
    let uses = entry(job, "uses")?;
    let commit = uses
        .as_str()?
        .strip_prefix(super::desired::SET_DESIRED)?
        .strip_prefix('@')?;
    let release = text
        .lines()
        .nth(uses.line() - 1)
        .and_then(|line| line.split_once(" # v"))
        .map_or("", |(_, version)| version.trim_end());
    Some((commit, release))
}

/// Whether the plain version `version` is `floor` or later; `None` when either is not
/// `<major>.<minor>.<patch>`.
fn version_at_least(version: &str, floor: &str) -> Option<bool> {
    let parts = |text: &str| -> Option<Vec<u64>> {
        let numbers: Vec<u64> = text
            .split('.')
            .map(|p| p.parse().ok())
            .collect::<Option<_>>()?;
        (numbers.len() == 3).then_some(numbers)
    };
    Some(parts(version)? >= parts(floor)?)
}

fn farm_job_problems(farm: &Node, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if names(entry(farm, "needs")) != ["farm-check", "plan"] {
        problems.push("farm: needs are not plan and farm-check".to_owned());
    }
    if entry(farm, "if").and_then(Node::as_str) != Some(FARM_IF) {
        problems.push(format!("farm: `if` is not {FARM_IF}"));
    }
    if held(farm) != ["contents: read", "id-token: write"] {
        problems.push(format!(
            "farm: permissions are {:?}, not contents: read and id-token: write",
            held(farm)
        ));
    }
    let Some((commit, release)) = pin(farm, text) else {
        problems.push(format!(
            "farm: `uses` is not {}@<commit>",
            super::desired::SET_DESIRED
        ));
        return problems;
    };
    if version_at_least(release, APT_FLOOR) != Some(true) {
        problems.push(format!(
            "farm: the pin is release {release:?}, not {APT_FLOOR} or later: before it no daemon runs the hook command of an apt entry, so the entry would be refused"
        ));
    }
    let wanted = with_wanted(commit, release);
    for (key, want) in wanted {
        let got = at(farm, &["with", key]);
        if got != Some(want) {
            problems.push(format!("farm: with.{key} is {got:?}, not {want:?}"));
        }
    }
    for (key, _) in entry(farm, "with").map_or(&[][..], entries) {
        let key = key.as_str().unwrap_or_default();
        if !wanted.iter().any(|(k, _)| *k == key) {
            problems.push(format!("farm: with.{key} is an input it should not give"));
        }
    }
    problems
}

fn soak_problems(soak: &Node) -> Vec<String> {
    let mut problems = Vec::new();
    if names(entry(soak, "needs")) != ["farm", "farm-check", "plan"] {
        problems.push("soak: needs are not plan, farm-check and farm".to_owned());
    }
    if entry(soak, "if").and_then(Node::as_str) != Some(SOAK_IF) {
        problems.push(format!("soak: `if` is not {SOAK_IF}"));
    }
    if held(soak) != ["contents: read"] {
        problems.push(format!(
            "soak: permissions are {:?}, not contents: read alone",
            held(soak)
        ));
    }
    if entry(soak, "environment").is_some() {
        problems.push("soak: names an environment; it needs no secret".to_owned());
    }
    let soaks: Vec<&Node> = steps(soak)
        .iter()
        .filter(|s| text(s, "run").contains("cargo xtask channels soak"))
        .collect();
    let [step] = soaks.as_slice() else {
        problems.push("soak: it needs one step that runs `cargo xtask channels soak`".to_owned());
        return problems;
    };
    let run = text(step, "run");
    for argument in [
        "--hours \"${FARM_SOAK_HOURS:-0}\"",
        "--reached-at \"$(date +%s)\"",
        "--version \"$PACKAGE_VERSION\"",
    ] {
        if !run.contains(argument) {
            problems.push(format!("soak: the soak is not given {argument}"));
        }
    }
    for (name, want) in [
        ("FARM_SOAK_HOURS", "${{ vars.FARM_SOAK_HOURS }}"),
        (
            "PACKAGE_VERSION",
            "${{ needs.farm-check.outputs.package-version }}",
        ),
    ] {
        if at(step, &["env", name]) != Some(want) {
            problems.push(format!("soak: env {name} is not {want}"));
        }
    }
    problems
}

/// The jobs `name` waits for, directly.
fn needs_of(doc: &Node, name: &str) -> Vec<String> {
    names(job(doc, name).and_then(|j| entry(j, "needs")))
}

/// Whether `name` is `target` or waits for it, through any chain of `needs`.
fn reaches(doc: &Node, name: &str, target: &str, seen: &mut Vec<String>) -> bool {
    if name == target {
        return true;
    }
    if seen.iter().any(|s| s == name) {
        return false;
    }
    seen.push(name.to_owned());
    needs_of(doc, name)
        .iter()
        .any(|n| reaches(doc, n, target, seen))
}

/// The jobs that make the release public.
const PROMOTION: [&str; 8] = [
    "promote-latest",
    "tap",
    "stable-apt-read",
    "stable-apt-index",
    "stable-apt-write",
    "stable-rpm-read",
    "stable-rpm-index",
    "stable-rpm-write",
];

/// Every job that makes the release public waits for `promote-check`, and `promote-check` waits
/// for the lab and for the install checks.
fn order_problems(doc: &Node) -> Vec<String> {
    let public = PROMOTION.iter().copied().chain(
        JOBS.iter()
            .chain(RPM_JOBS.iter())
            .filter(|j| j.suite == "stable")
            .map(|j| j.name),
    );
    let mut problems = Vec::new();
    for name in public {
        if job(doc, name).is_some() && !reaches(doc, name, "promote-check", &mut Vec::new()) {
            problems.push(format!(
                "{name}: does not wait for promote-check, so it could make a release public that was not installed from testing and put on the lab"
            ));
        }
    }
    for before in [
        "soak",
        "check-apt",
        "check-rpm",
        "check-brew",
        "check-install-sh",
    ] {
        if job(doc, "promote-check").is_some()
            && !reaches(doc, "promote-check", before, &mut Vec::new())
        {
            problems.push(format!(
                "promote-check: does not wait for {before}, so it would decide without its result"
            ));
        }
    }
    problems
}
