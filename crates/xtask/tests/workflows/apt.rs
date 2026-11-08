//! The rules for the apt jobs of `release.yml` (`testing-apt-read`, `testing-apt-index` and
//! `testing-apt-write`, each in up to three attempts), which put the release's Debian packages in
//! the apt repository the organisation's products share (docs/design/distribution.md, sections 5
//! and 8). [`apt_problems`] is called from `release_problems`, so the one lint holds the whole
//! file.
//!
//! The split is the point: no job holds both the signing key and the cloud role, so a compromise
//! of one step cannot sign and publish.
//! - The jobs that read and write (environment `apt-publish`) hold `id-token: write` and the
//!   role of the environment, assumed by one step; they name no signing key.
//! - The index job (environment `apt-signing`) holds `contents: read` alone, assumes no role and
//!   names no variable of the role; the key is read by its gate and its signing step alone, and
//!   the signing step comes after the merge.
//! - Each job has the `needs` and the `if:` of the table, its gate is the first step that is not
//!   setup and is given each setting as `NAME=${VAR:+set}` and each earlier job's answer as
//!   `--require`, and every step after the gate runs only when the gate said `enabled=true`.
//! - The last attempt's write is the one that passes `--last` (it fails on a lost race instead of
//!   asking for a fourth attempt).
//! - The verbs of the apt channel are run by the jobs of their kind alone.

use super::channels::{GATE_IF, mentions, steps, text, uses};
use super::lint::{RELEASE_IF, entries, entry, names};
use super::yaml::Node;

/// The `if:` of the read job of attempt 2: the first write lost the race for the index.
pub const RETRY_2_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.testing-apt-write.outputs.conflict == 'true' }}";

/// The `if:` of the read job of attempt 3.
pub const RETRY_3_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.testing-apt-write-2.outputs.conflict == 'true' }}";

/// The `if:` of the promotion's jobs of the first attempt: a release, on main, that
/// `promote-check` promoted.
pub const PROMOTE_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.promote-check.outputs.promote == 'true' }}";

/// The `if:` of the read job of the second attempt of the promotion.
pub const STABLE_RETRY_2_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.stable-apt-write.outputs.conflict == 'true' }}";

/// The `if:` of the read job of the third attempt of the promotion.
pub const STABLE_RETRY_3_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.stable-apt-write-2.outputs.conflict == 'true' }}";

/// The role the read and write jobs assume.
pub(super) const ROLE: &str = "${{ vars.APT_PUBLISH_ROLE_ARN }}";

/// The settings of the read and write jobs, all in the environment `apt-publish`.
const PUBLISH_SETTINGS: [&str; 4] = [
    "APT_PUBLISH_ROLE_ARN",
    "APT_BUCKET",
    "APT_DISTRIBUTION_ID",
    "APT_HOSTNAME",
];

/// The settings of the index job, in the environment `apt-signing`.
const SIGNING_SETTINGS: [&str; 3] = [
    "APT_GPG_FINGERPRINT",
    "APT_GPG_PRIVATE_KEY",
    "APT_GPG_PASSPHRASE",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Read,
    Index,
    Write,
}

/// What one apt job is held to.
pub struct Job {
    pub name: &'static str,
    pub kind: Kind,
    pub needs: &'static [&'static str],
    pub condition: &'static str,
    /// The answers of earlier jobs the gate is given with `--require`.
    pub requires: &'static [&'static str],
    /// Does the write pass `--last`?
    pub last: bool,
    /// The suite its verbs are given with `--suite`, and its index is signed in.
    pub suite: &'static str,
}

pub const JOBS: [Job; 18] = [
    Job {
        name: "testing-apt-read",
        kind: Kind::Read,
        needs: &["plan", "publish"],
        condition: RELEASE_IF,
        requires: &[],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-index",
        kind: Kind::Index,
        needs: &["plan", "testing-apt-read"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-write",
        kind: Kind::Write,
        needs: &["plan", "testing-apt-index", "testing-apt-read"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-read-2",
        kind: Kind::Read,
        needs: &["plan", "testing-apt-write"],
        condition: RETRY_2_IF,
        requires: &[],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-index-2",
        kind: Kind::Index,
        needs: &["plan", "testing-apt-read-2"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-write-2",
        kind: Kind::Write,
        needs: &["plan", "testing-apt-index-2", "testing-apt-read-2"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-read-3",
        kind: Kind::Read,
        needs: &["plan", "testing-apt-write-2"],
        condition: RETRY_3_IF,
        requires: &[],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-index-3",
        kind: Kind::Index,
        needs: &["plan", "testing-apt-read-3"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "testing",
    },
    Job {
        name: "testing-apt-write-3",
        kind: Kind::Write,
        needs: &["plan", "testing-apt-index-3", "testing-apt-read-3"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: true,
        suite: "testing",
    },
    Job {
        name: "stable-apt-read",
        kind: Kind::Read,
        needs: &["plan", "promote-check", "testing-apt-read"],
        condition: PROMOTE_IF,
        requires: &["TESTING_ENABLED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-index",
        kind: Kind::Index,
        needs: &[
            "plan",
            "promote-check",
            "stable-apt-read",
            "testing-apt-index",
        ],
        condition: PROMOTE_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-write",
        kind: Kind::Write,
        needs: &[
            "plan",
            "promote-check",
            "stable-apt-index",
            "stable-apt-read",
            "testing-apt-write",
        ],
        condition: PROMOTE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-read-2",
        kind: Kind::Read,
        needs: &["plan", "stable-apt-write"],
        condition: STABLE_RETRY_2_IF,
        requires: &[],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-index-2",
        kind: Kind::Index,
        needs: &["plan", "stable-apt-read-2"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-write-2",
        kind: Kind::Write,
        needs: &["plan", "stable-apt-index-2", "stable-apt-read-2"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-read-3",
        kind: Kind::Read,
        needs: &["plan", "stable-apt-write-2"],
        condition: STABLE_RETRY_3_IF,
        requires: &[],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-index-3",
        kind: Kind::Index,
        needs: &["plan", "stable-apt-read-3"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED"],
        last: false,
        suite: "stable",
    },
    Job {
        name: "stable-apt-write-3",
        kind: Kind::Write,
        needs: &["plan", "stable-apt-index-3", "stable-apt-read-3"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: true,
        suite: "stable",
    },
];

/// Is `name` an apt or rpm job that holds the cloud role (a read or a write)?
pub fn is_role_job(name: &str) -> bool {
    JOBS.iter()
        .chain(super::rpm::JOBS.iter())
        .any(|j| j.name == name && j.kind != Kind::Index)
}

/// The verb each kind of job runs, and no other kind: the index job of the suite `testing` merges
/// the release's packages (`apt index`), the one of `stable` promotes the entries testing lists
/// (`apt promote`).
fn verb(kind: Kind, suite: &str) -> &'static str {
    match (kind, suite) {
        (Kind::Read, _) => "cargo xtask channels apt read ",
        (Kind::Index, "stable") => "cargo xtask channels apt promote ",
        (Kind::Index, _) => "cargo xtask channels apt index ",
        (Kind::Write, _) => "cargo xtask channels apt write ",
    }
}

/// The verbs, with the kind and the suite of the only jobs that may run each.
const VERBS: [(&str, Kind, &str); 4] = [
    ("cargo xtask channels apt read ", Kind::Read, ""),
    ("cargo xtask channels apt index ", Kind::Index, "testing"),
    ("cargo xtask channels apt promote ", Kind::Index, "stable"),
    ("cargo xtask channels apt write ", Kind::Write, ""),
];

const SIGN: &str = "cargo xtask channels apt sign ";

pub(super) fn runs(node: &Node, needle: &str) -> bool {
    steps(node).iter().any(|s| text(s, "run").contains(needle))
}

/// One line per way the apt jobs of the `release.yml` document `doc` are not as the module says.
pub fn apt_problems(doc: &Node) -> Vec<String> {
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let mut problems = Vec::new();
    for want in &JOBS {
        match jobs.iter().find(|(k, _)| k.as_str() == Some(want.name)) {
            Some((_, found)) => problems.extend(job_problems(want, found)),
            None => problems.push(format!("{}: there is no such job", want.name)),
        }
    }
    for (name, found) in jobs {
        let name = name.as_str().unwrap_or_default();
        let own = JOBS.iter().find(|j| j.name == name);
        let kind = own
            .or_else(|| super::rpm::JOBS.iter().find(|j| j.name == name))
            .map(|j| j.kind);
        if kind != Some(Kind::Index) && mentions(found, "APT_GPG_") {
            problems.push(format!(
                "{name}: names APT_GPG_, the signing key, which only the apt and rpm index jobs may"
            ));
        }
        if !is_role_job(name) && mentions(found, "APT_PUBLISH_ROLE_ARN") {
            problems.push(format!(
                "{name}: names APT_PUBLISH_ROLE_ARN, which only the apt and rpm jobs that read and write may"
            ));
        }
        for (word, other, suite) in VERBS {
            let may =
                own.is_some_and(|j| j.kind == other && (suite.is_empty() || j.suite == suite));
            if !may && runs(found, word) {
                let of = if suite.is_empty() {
                    String::new()
                } else {
                    format!(" of the suite {suite}")
                };
                problems.push(format!(
                    "{name}: runs `{}...`, which only the {other:?} jobs{of} of the apt repository may",
                    word.trim_end()
                ));
            }
        }
        if kind != Some(Kind::Index) && runs(found, SIGN) {
            problems.push(format!(
                "{name}: runs `{}...`, which only the apt index jobs may",
                SIGN.trim_end()
            ));
        }
    }
    problems
}

pub(super) fn permissions(job: &Node) -> Vec<String> {
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

fn job_problems(want: &Job, job: &Node) -> Vec<String> {
    let name = want.name;
    let mut problems = Vec::new();
    if names(entry(job, "needs")) != want.needs {
        problems.push(format!("{name}: needs are not {:?}", want.needs));
    }
    if entry(job, "if").and_then(Node::as_str) != Some(want.condition) {
        problems.push(format!("{name}: `if` is not {}", want.condition));
    }
    let (environment, held): (&str, &[&str]) = match want.kind {
        Kind::Index => ("apt-signing", &["contents: read"]),
        _ => ("apt-publish", &["contents: read", "id-token: write"]),
    };
    if entry(job, "environment").and_then(Node::as_str) != Some(environment) {
        problems.push(format!("{name}: environment is not {environment}"));
    }
    if permissions(job) != held {
        problems.push(format!(
            "{name}: permissions are {:?}, not {held:?}",
            permissions(job)
        ));
    }
    let all = steps(job);
    let Some(gate) = all
        .iter()
        .position(|s| entry(s, "id").and_then(Node::as_str) == Some("gate"))
    else {
        problems.push(format!("{name}: no step has the id `gate`"));
        return problems;
    };
    problems.extend(suite_problems(want, all));
    problems.extend(gate_problems(want, &all[gate]));
    for step in &all[..gate] {
        for word in ["aws-actions/", "secrets.", "APT_GPG_", "apt-key"] {
            if mentions(step, word) {
                problems.push(format!("{name}: a step before the gate names {word}"));
            }
        }
    }
    for step in &all[gate + 1..] {
        if entry(step, "if").and_then(Node::as_str) != Some(GATE_IF) {
            problems.push(format!(
                "{name}: the step `{}` after the gate does not have `if: {GATE_IF}`",
                text(step, "name").to_owned() + text(step, "uses")
            ));
        }
    }
    let after = &all[gate + 1..];
    match want.kind {
        Kind::Index => problems.extend(index_problems(want, job, after)),
        _ => {
            problems.extend(role_problems(want, after));
            for step in all.iter().filter(|s| mentions(s, "secrets.")) {
                problems.push(format!(
                    "{name}: the step `{}` names a secret; this job holds the role, not a key",
                    text(step, "name")
                ));
            }
        }
    }
    problems
}

/// A job writes one suite and no other: every verb of the job is given `--suite <suite>` (the
/// promotion's read job reads both suites, and its index job is given the two directories and the
/// sums), the index is signed in the directory of that suite, a write of `testing` puts the
/// packages in the pool, and a write of `stable` puts none (`--no-pool`): the promotion lists in
/// `stable` the entries `testing` holds, and writes no package.
fn suite_problems(want: &Job, all: &[Node]) -> Vec<String> {
    let name = want.name;
    let stable = want.suite == "stable";
    let other = if stable { "testing" } else { "stable" };
    let own = verb(want.kind, want.suite);
    let mut problems = Vec::new();
    for step in all.iter().filter(|s| text(s, "run").contains(own)) {
        let run = text(step, "run");
        let shown = own.trim_end();
        match (want.kind, stable) {
            (Kind::Index, true) => {
                for argument in [
                    "--testing \"$RUNNER_TEMP/apt-read/testing\" ",
                    "--stable \"$RUNNER_TEMP/apt-read/stable\" ",
                    "--sums dist/SHA256SUMS ",
                ] {
                    if !run.contains(argument) {
                        problems.push(format!("{name}: `{shown}...` is not given {argument}"));
                    }
                }
            }
            (Kind::Read, true) => {
                for suite in ["testing", "stable"] {
                    if !run.contains(&format!("{own}--suite {suite} ")) {
                        problems.push(format!(
                            "{name}: `{shown}...` is not given `--suite {suite}`: the promotion reads both"
                        ));
                    }
                }
            }
            _ => {
                if !run.contains(&format!("{own}--suite {} ", want.suite)) {
                    problems.push(format!(
                        "{name}: `{shown}...` is not given `--suite {}`",
                        want.suite
                    ));
                }
                if run.contains(&format!("--suite {other} ")) {
                    problems.push(format!(
                        "{name}: names `--suite {other}`; this job is for the suite {}",
                        want.suite
                    ));
                }
            }
        }
        if want.kind == Kind::Write {
            if stable && !run.contains(" --no-pool ") {
                problems.push(format!(
                    "{name}: does not pass --no-pool; the promotion writes the index only"
                ));
            }
            if stable && run.contains("--debs ") {
                problems.push(format!(
                    "{name}: passes --debs; the promotion uploads no package"
                ));
            }
            if !stable && run.contains("--no-pool") {
                problems.push(format!(
                    "{name}: passes --no-pool; the release's write puts the packages in the pool"
                ));
            }
        }
    }
    let dir = format!("--dir \"$RUNNER_TEMP/apt-next/dists/{}\" ", want.suite);
    for step in all.iter().filter(|s| text(s, "run").contains(SIGN)) {
        if !text(step, "run").contains(&dir) {
            problems.push(format!(
                "{name}: the index is not signed in dists/{}, the suite the job writes",
                want.suite
            ));
        }
    }
    problems
}

pub(super) fn gate_problems(want: &Job, gate: &Node) -> Vec<String> {
    let name = want.name;
    let run = text(gate, "run");
    let mut problems = Vec::new();
    let start = format!("cargo xtask channels gate --label {name} ");
    if !run.starts_with(&start) {
        problems.push(format!("{name}: the gate does not run `{start}...`"));
    }
    let settings: &[&str] = match want.kind {
        Kind::Index => &SIGNING_SETTINGS,
        _ => &PUBLISH_SETTINGS,
    };
    for setting in settings {
        if !run.contains(&format!("--setting \"{setting}=${{{setting}:+set}}\"")) {
            problems.push(format!(
                "{name}: the gate is not given the setting {setting} as NAME=${{NAME:+set}}"
            ));
        }
    }
    // The gate reads whether a value is set, never the value.
    if run.matches("--setting ").count() != settings.len() {
        problems.push(format!(
            "{name}: the gate is given a setting it should not be"
        ));
    }
    for require in want.requires {
        if !run.contains(&format!("--require \"{require}=${require}\"")) {
            problems.push(format!(
                "{name}: the gate is not given --require \"{require}=${require}\""
            ));
        }
    }
    if run.matches("--require ").count() != want.requires.len() {
        problems.push(format!(
            "{name}: the gate is given a requirement it should not be"
        ));
    }
    if run.contains("secrets.") || run.contains("vars.") {
        problems.push(format!("{name}: the gate's command line holds a value"));
    }
    problems
}

/// The jobs that read and write: one step assumes the role, before the verb, and no step names
/// a secret.
fn role_problems(want: &Job, after: &[Node]) -> Vec<String> {
    let name = want.name;
    let mut problems = Vec::new();
    let roles: Vec<usize> = after
        .iter()
        .enumerate()
        .filter(|(_, s)| uses(s, "aws-actions/configure-aws-credentials"))
        .map(|(i, _)| i)
        .collect();
    let verbs: Vec<usize> = after
        .iter()
        .enumerate()
        .filter(|(_, s)| text(s, "run").contains(verb(want.kind, want.suite)))
        .map(|(i, _)| i)
        .collect();
    match (roles.as_slice(), verbs.as_slice()) {
        ([role], [at]) => {
            let given = entry(&after[*role], "with")
                .and_then(|w| entry(w, "role-to-assume"))
                .and_then(Node::as_str);
            if given != Some(ROLE) {
                problems.push(format!(
                    "{name}: the role assumed is not {ROLE}, the environment's"
                ));
            }
            if role > at {
                problems.push(format!("{name}: the role is assumed after the verb runs"));
            }
            let last = text(&after[*at], "run").contains(" --last ");
            if want.kind == Kind::Write && last != want.last {
                problems.push(format!(
                    "{name}: `--last` must be passed by the last attempt's write and by no other"
                ));
            }
        }
        _ => problems.push(format!(
            "{name}: it needs one step that assumes the role and one that runs `{}...`",
            verb(want.kind, want.suite).trim_end()
        )),
    }
    // The write of the suite `testing` uploads the release's packages, checked against the
    // sums; the promotion's write has no file of the release.
    if want.kind == Kind::Write
        && want.suite == "testing"
        && !after
            .iter()
            .any(|s| text(s, "run").contains("cargo xtask release check dist"))
    {
        problems.push(format!("{name}: no step checks the downloaded files"));
    }
    problems
}

/// The index jobs: no role, the key in the gate and the signing step alone, the merge before the
/// signature.
fn index_problems(want: &Job, job: &Node, after: &[Node]) -> Vec<String> {
    let name = want.name;
    let mut problems = Vec::new();
    for word in ["aws-actions/", "id-token", "AWS_", "APT_PUBLISH_"] {
        if mentions(job, word) {
            problems.push(format!(
                "{name}: names {word}; this job holds the key, not the role"
            ));
        }
    }
    let merge = after
        .iter()
        .position(|s| text(s, "run").contains(verb(Kind::Index, want.suite)));
    let sign = after.iter().position(|s| text(s, "run").contains(SIGN));
    match (merge, sign) {
        (Some(m), Some(s)) if m < s => {
            if text(&after[s], "id") != "sign" {
                problems.push(format!(
                    "{name}: the signing step does not have the id `sign`"
                ));
            }
            for secret in ["secrets.APT_GPG_PRIVATE_KEY", "secrets.APT_GPG_PASSPHRASE"] {
                if !mentions(&after[s], secret) {
                    problems.push(format!("{name}: the signing step does not read {secret}"));
                }
            }
            for (i, step) in after.iter().enumerate() {
                if i != s && mentions(step, "secrets.") {
                    problems.push(format!(
                        "{name}: the step `{}` names a secret; only the gate and the signing step may",
                        text(step, "name")
                    ));
                }
            }
        }
        _ => problems.push(format!(
            "{name}: it needs a step that merges (`{}...`) and then one that signs (`{}...`)",
            verb(Kind::Index, want.suite).trim_end(),
            SIGN.trim_end()
        )),
    }
    if !after
        .iter()
        .any(|s| text(s, "run").contains("cargo xtask release check dist"))
    {
        problems.push(format!("{name}: no step checks the downloaded files"));
    }
    problems
}
