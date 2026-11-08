//! The rules for the apt jobs of `release.yml` (`apt-deploy-read`, `apt-index` and
//! `apt-deploy-write`, each in up to three attempts), which put the release's Debian packages in
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
pub const RETRY_2_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.apt-deploy-write.outputs.conflict == 'true' }}";

/// The `if:` of the read job of attempt 3.
pub const RETRY_3_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.apt-deploy-write-2.outputs.conflict == 'true' }}";

/// The role the read and write jobs assume.
const ROLE: &str = "${{ vars.APT_PUBLISH_ROLE_ARN }}";

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
}

pub const JOBS: [Job; 9] = [
    Job {
        name: "apt-deploy-read",
        kind: Kind::Read,
        needs: &["plan", "soak"],
        condition: RELEASE_IF,
        requires: &[],
        last: false,
    },
    Job {
        name: "apt-index",
        kind: Kind::Index,
        needs: &["apt-deploy-read", "plan"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED"],
        last: false,
    },
    Job {
        name: "apt-deploy-write",
        kind: Kind::Write,
        needs: &["apt-deploy-read", "apt-index", "plan"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
    },
    Job {
        name: "apt-deploy-read-2",
        kind: Kind::Read,
        needs: &["apt-deploy-write", "plan"],
        condition: RETRY_2_IF,
        requires: &[],
        last: false,
    },
    Job {
        name: "apt-index-2",
        kind: Kind::Index,
        needs: &["apt-deploy-read-2", "plan"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED"],
        last: false,
    },
    Job {
        name: "apt-deploy-write-2",
        kind: Kind::Write,
        needs: &["apt-deploy-read-2", "apt-index-2", "plan"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: false,
    },
    Job {
        name: "apt-deploy-read-3",
        kind: Kind::Read,
        needs: &["apt-deploy-write-2", "plan"],
        condition: RETRY_3_IF,
        requires: &[],
        last: false,
    },
    Job {
        name: "apt-index-3",
        kind: Kind::Index,
        needs: &["apt-deploy-read-3", "plan"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED"],
        last: false,
    },
    Job {
        name: "apt-deploy-write-3",
        kind: Kind::Write,
        needs: &["apt-deploy-read-3", "apt-index-3", "plan"],
        condition: RELEASE_IF,
        requires: &["READ_ENABLED", "INDEX_SIGNED"],
        last: true,
    },
];

/// Is `name` an apt job that holds the cloud role (a read or a write)?
pub fn is_role_job(name: &str) -> bool {
    JOBS.iter().any(|j| j.name == name && j.kind != Kind::Index)
}

/// The verb each kind of job runs, and no other kind.
fn verb(kind: Kind) -> &'static str {
    match kind {
        Kind::Read => "cargo xtask channels apt read ",
        Kind::Index => "cargo xtask channels apt index ",
        Kind::Write => "cargo xtask channels apt write ",
    }
}

const SIGN: &str = "cargo xtask channels apt sign ";

fn runs(node: &Node, needle: &str) -> bool {
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
        let kind = JOBS.iter().find(|j| j.name == name).map(|j| j.kind);
        if kind != Some(Kind::Index) && mentions(found, "APT_GPG_") {
            problems.push(format!(
                "{name}: names APT_GPG_, the signing key, which only the apt index jobs may"
            ));
        }
        if !is_role_job(name) && mentions(found, "APT_PUBLISH_ROLE_ARN") {
            problems.push(format!(
                "{name}: names APT_PUBLISH_ROLE_ARN, which only the apt jobs that read and write may"
            ));
        }
        for other in [Kind::Read, Kind::Index, Kind::Write] {
            if kind != Some(other) && runs(found, verb(other)) {
                problems.push(format!(
                    "{name}: runs `{}...`, which only the {other:?} jobs of the apt repository may",
                    verb(other).trim_end()
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

fn permissions(job: &Node) -> Vec<String> {
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
        Kind::Index => problems.extend(index_problems(name, job, after)),
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

fn gate_problems(want: &Job, gate: &Node) -> Vec<String> {
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
        .filter(|(_, s)| text(s, "run").contains(verb(want.kind)))
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
            verb(want.kind).trim_end()
        )),
    }
    if want.kind == Kind::Write
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
fn index_problems(name: &str, job: &Node, after: &[Node]) -> Vec<String> {
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
        .position(|s| text(s, "run").contains(verb(Kind::Index)));
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
            verb(Kind::Index).trim_end(),
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
