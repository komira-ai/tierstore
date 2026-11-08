//! The rules for the rpm jobs of `release.yml` (`testing-rpm-read`, `testing-rpm-index` and
//! `testing-rpm-write`, each in up to three attempts, and the same nine for the suite `stable`),
//! which put the release's RPM packages in the rpm tree of the repository the organisation's
//! products share (docs/design/distribution.md, sections 5 and 8). [`rpm_problems`] is called from
//! `release_problems`, so the one lint holds the whole file.
//!
//! They are the apt jobs' rules ([`super::apt`]) for the rpm verbs, and the split is the same: no
//! job holds both the signing key and the cloud role.
//! - The jobs that read and write (environment `apt-publish`) hold `id-token: write` and the
//!   role of the environment, assumed by one step; they name no signing key.
//! - The index job (environment `apt-signing`) holds `contents: read` alone, assumes no role and
//!   names no variable of the role. The key is read by its gate and its two signing steps alone:
//!   the packages are signed (`rpm sign`, at the commit's time), then the packages are merged into
//!   the repodata, or promoted from testing, and then `repomd.xml` is signed (`rpm repo sign`).
//! - Each job has the `needs` and the `if:` of the table, its gate is the first step that is not
//!   setup and is given each setting as `NAME=${VAR:+set}` and each earlier job's answer as
//!   `--require`, and every step after the gate runs only when the gate said `enabled=true`. The
//!   first read of the suite `testing` is also given `RPM_REPO_READY`, the repository variable that
//!   says the rpm tree is deployed: until it is `true` no rpm job writes, and none fails.
//! - The write of `testing` checks the signed packages with `rpm check-signed` before it assumes
//!   the role. The last attempt's write is the one that passes `--last`.
//! - The verbs of the rpm channel are run by the jobs of their kind alone.

use super::apt::{Job, Kind, PROMOTE_IF, ROLE, gate_problems, permissions, runs};
use super::channels::{GATE_IF, mentions, steps, text, uses};
use super::lint::{RELEASE_IF, entries, entry, names};
use super::yaml::Node;

/// The `if:` of the read job of attempt 2: the first write lost the race for `repomd.xml`.
pub const RETRY_2_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.testing-rpm-write.outputs.conflict == 'true' }}";

/// The `if:` of the read job of attempt 3.
pub const RETRY_3_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.testing-rpm-write-2.outputs.conflict == 'true' }}";

/// The `if:` of the read job of the second attempt of the promotion.
pub const STABLE_RETRY_2_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.stable-rpm-write.outputs.conflict == 'true' }}";

/// The `if:` of the read job of the third attempt of the promotion.
pub const STABLE_RETRY_3_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && needs.stable-rpm-write-2.outputs.conflict == 'true' }}";

macro_rules! job {
    ($name:expr, $kind:ident, [$($need:expr),*], $cond:expr, [$($req:expr),*], $last:expr, $suite:expr) => {
        Job {
            name: $name,
            kind: Kind::$kind,
            needs: &[$($need),*],
            condition: $cond,
            requires: &[$($req),*],
            last: $last,
            suite: $suite,
        }
    };
}

pub const JOBS: [Job; 18] = [
    job!(
        "testing-rpm-read",
        Read,
        ["plan", "publish"],
        RELEASE_IF,
        ["RPM_REPO_READY"],
        false,
        "testing"
    ),
    job!(
        "testing-rpm-index",
        Index,
        ["plan", "testing-rpm-read"],
        RELEASE_IF,
        ["READ_ENABLED"],
        false,
        "testing"
    ),
    job!(
        "testing-rpm-write",
        Write,
        ["plan", "testing-rpm-index", "testing-rpm-read"],
        RELEASE_IF,
        ["READ_ENABLED", "INDEX_SIGNED"],
        false,
        "testing"
    ),
    job!(
        "testing-rpm-read-2",
        Read,
        ["plan", "testing-rpm-write"],
        RETRY_2_IF,
        [],
        false,
        "testing"
    ),
    job!(
        "testing-rpm-index-2",
        Index,
        ["plan", "testing-rpm-read-2"],
        RELEASE_IF,
        ["READ_ENABLED"],
        false,
        "testing"
    ),
    job!(
        "testing-rpm-write-2",
        Write,
        ["plan", "testing-rpm-index-2", "testing-rpm-read-2"],
        RELEASE_IF,
        ["READ_ENABLED", "INDEX_SIGNED"],
        false,
        "testing"
    ),
    job!(
        "testing-rpm-read-3",
        Read,
        ["plan", "testing-rpm-write-2"],
        RETRY_3_IF,
        [],
        false,
        "testing"
    ),
    job!(
        "testing-rpm-index-3",
        Index,
        ["plan", "testing-rpm-read-3"],
        RELEASE_IF,
        ["READ_ENABLED"],
        false,
        "testing"
    ),
    job!(
        "testing-rpm-write-3",
        Write,
        ["plan", "testing-rpm-index-3", "testing-rpm-read-3"],
        RELEASE_IF,
        ["READ_ENABLED", "INDEX_SIGNED"],
        true,
        "testing"
    ),
    job!(
        "stable-rpm-read",
        Read,
        ["plan", "promote-check", "testing-rpm-read"],
        PROMOTE_IF,
        ["TESTING_ENABLED"],
        false,
        "stable"
    ),
    job!(
        "stable-rpm-index",
        Index,
        [
            "plan",
            "promote-check",
            "stable-rpm-read",
            "testing-rpm-index"
        ],
        PROMOTE_IF,
        ["READ_ENABLED"],
        false,
        "stable"
    ),
    job!(
        "stable-rpm-write",
        Write,
        [
            "plan",
            "promote-check",
            "stable-rpm-index",
            "stable-rpm-read",
            "testing-rpm-write"
        ],
        PROMOTE_IF,
        ["READ_ENABLED", "INDEX_SIGNED"],
        false,
        "stable"
    ),
    job!(
        "stable-rpm-read-2",
        Read,
        ["plan", "stable-rpm-write"],
        STABLE_RETRY_2_IF,
        [],
        false,
        "stable"
    ),
    job!(
        "stable-rpm-index-2",
        Index,
        ["plan", "stable-rpm-read-2"],
        RELEASE_IF,
        ["READ_ENABLED"],
        false,
        "stable"
    ),
    job!(
        "stable-rpm-write-2",
        Write,
        ["plan", "stable-rpm-index-2", "stable-rpm-read-2"],
        RELEASE_IF,
        ["READ_ENABLED", "INDEX_SIGNED"],
        false,
        "stable"
    ),
    job!(
        "stable-rpm-read-3",
        Read,
        ["plan", "stable-rpm-write-2"],
        STABLE_RETRY_3_IF,
        [],
        false,
        "stable"
    ),
    job!(
        "stable-rpm-index-3",
        Index,
        ["plan", "stable-rpm-read-3"],
        RELEASE_IF,
        ["READ_ENABLED"],
        false,
        "stable"
    ),
    job!(
        "stable-rpm-write-3",
        Write,
        ["plan", "stable-rpm-index-3", "stable-rpm-read-3"],
        RELEASE_IF,
        ["READ_ENABLED", "INDEX_SIGNED"],
        true,
        "stable"
    ),
];

/// The verb each kind of job runs for the repodata, and no other kind: the index job of the suite
/// `testing` merges the release's packages (`rpm repo index`), the one of `stable` promotes the
/// entries testing lists (`rpm promote`).
fn verb(kind: Kind, suite: &str) -> &'static str {
    match (kind, suite) {
        (Kind::Read, _) => "cargo xtask channels rpm repo read ",
        (Kind::Index, "stable") => "cargo xtask channels rpm promote ",
        (Kind::Index, _) => "cargo xtask channels rpm repo index ",
        (Kind::Write, _) => "cargo xtask channels rpm repo write ",
    }
}

/// The verbs, with the kind and the suite of the only jobs that may run each.
const VERBS: [(&str, Kind, &str); 7] = [
    ("cargo xtask channels rpm repo read ", Kind::Read, ""),
    (
        "cargo xtask channels rpm repo index ",
        Kind::Index,
        "testing",
    ),
    ("cargo xtask channels rpm promote ", Kind::Index, "stable"),
    ("cargo xtask channels rpm repo write ", Kind::Write, ""),
    ("cargo xtask channels rpm sign ", Kind::Index, ""),
    ("cargo xtask channels rpm repo sign ", Kind::Index, ""),
    (
        "cargo xtask channels rpm check-signed ",
        Kind::Write,
        "testing",
    ),
];

const SIGN_PACKAGES: &str = "cargo xtask channels rpm sign ";
const SIGN_REPO: &str = "cargo xtask channels rpm repo sign ";
const CHECK_SIGNED: &str = "cargo xtask channels rpm check-signed ";

/// One line per way the rpm jobs of the `release.yml` document `doc` are not as the module says.
pub fn rpm_problems(doc: &Node) -> Vec<String> {
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
        for (word, kind, suite) in VERBS {
            let may = own.is_some_and(|j| j.kind == kind && (suite.is_empty() || j.suite == suite));
            if !may && runs(found, word) {
                let of = if suite.is_empty() {
                    String::new()
                } else {
                    format!(" of the suite {suite}")
                };
                problems.push(format!(
                    "{name}: runs `{}...`, which only the {kind:?} jobs{of} of the rpm repository may",
                    word.trim_end()
                ));
            }
        }
    }
    problems
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
        for word in ["aws-actions/", "secrets.", "APT_GPG_", "rpm-key"] {
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
/// promotion's read job reads both suites, and its index job is given the two directories, the
/// sums and the signed sums), the repodata is signed in the directory of that suite, a write of
/// `testing` puts the packages in the pool, and a write of `stable` puts none (`--no-pool`): the
/// promotion lists in `stable` the entries `testing` holds, and writes no package.
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
                    "--testing \"$RUNNER_TEMP/rpm-read/testing\" ",
                    "--stable \"$RUNNER_TEMP/rpm-read/stable\" ",
                    "--sums dist/SHA256SUMS ",
                    "--signed-sums \"$RUNNER_TEMP/rpm-signed/SHA256SUMS.signed\" ",
                ] {
                    if !run.contains(argument) {
                        problems.push(format!("{name}: `{shown}...` is not given {argument}"));
                    }
                }
            }
            (Kind::Index, false) => {
                if !run.contains("--rpms \"$RUNNER_TEMP/rpm-signed\" ") {
                    problems.push(format!(
                        "{name}: `{shown}...` is not given the signed packages (--rpms \"$RUNNER_TEMP/rpm-signed\")"
                    ));
                }
                if !run.contains(&format!("{own}--suite testing ")) {
                    problems.push(format!(
                        "{name}: `{shown}...` is not given `--suite testing`"
                    ));
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
            if stable && !run.contains(" --no-pool") {
                problems.push(format!(
                    "{name}: does not pass --no-pool; the promotion writes the repodata only"
                ));
            }
            if stable && run.contains("--rpms ") {
                problems.push(format!(
                    "{name}: passes --rpms; the promotion uploads no package"
                ));
            }
            if !stable && run.contains("--no-pool") {
                problems.push(format!(
                    "{name}: passes --no-pool; the release's write puts the packages in the pool"
                ));
            }
        }
    }
    let dir = format!("--dir \"$RUNNER_TEMP/rpm-next/{}\" ", want.suite);
    for step in all.iter().filter(|s| text(s, "run").contains(SIGN_REPO)) {
        if !text(step, "run").contains(&dir) {
            problems.push(format!(
                "{name}: the repodata is not signed in rpm-next/{}, the suite the job writes",
                want.suite
            ));
        }
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
            let check = after
                .iter()
                .position(|s| text(s, "run").contains(CHECK_SIGNED));
            if want.kind == Kind::Write && want.suite == "testing" {
                match check {
                    Some(c) if c < *role => {
                        let run = text(&after[c], "run");
                        for argument in [
                            "--sums dist/SHA256SUMS ",
                            "--signed-sums \"$RUNNER_TEMP/rpm-signed/SHA256SUMS.signed\"",
                        ] {
                            if !run.contains(argument) {
                                problems.push(format!(
                                    "{name}: `{}...` is not given {argument}",
                                    CHECK_SIGNED.trim_end()
                                ));
                            }
                        }
                    }
                    _ => problems.push(format!(
                        "{name}: a step must run `{}...` before the role is assumed",
                        CHECK_SIGNED.trim_end()
                    )),
                }
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

/// The index jobs: no role, the key in the gate and the two signing steps alone, the packages
/// signed first, then merged (or promoted), then `repomd.xml` signed.
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
    let at = |needle: &str| after.iter().position(|s| text(s, "run").contains(needle));
    match (at(SIGN_PACKAGES), at(verb(Kind::Index, want.suite)), at(SIGN_REPO)) {
        (Some(packages), Some(merge), Some(repo)) if packages < merge && merge < repo => {
            for (index, id) in [(packages, "sign-rpms"), (repo, "sign")] {
                if text(&after[index], "id") != id {
                    problems.push(format!("{name}: the signing step does not have the id `{id}`"));
                }
                for secret in ["secrets.APT_GPG_PRIVATE_KEY", "secrets.APT_GPG_PASSPHRASE"] {
                    if !mentions(&after[index], secret) {
                        problems.push(format!("{name}: the signing step `{id}` does not read {secret}"));
                    }
                }
            }
            if !text(&after[packages], "run").contains("--signed-at \"$(git show --no-patch --format=%ct HEAD)\"") {
                problems.push(format!(
                    "{name}: the packages are not signed at the time of the commit (--signed-at \"$(git show --no-patch --format=%ct HEAD)\"), so a re-run would give other bytes"
                ));
            }
            for (i, step) in after.iter().enumerate() {
                if i != packages && i != repo && mentions(step, "secrets.") {
                    problems.push(format!(
                        "{name}: the step `{}` names a secret; only the gate and the two signing steps may",
                        text(step, "name")
                    ));
                }
            }
        }
        _ => problems.push(format!(
            "{name}: it needs a step that signs the packages (`{}...`), then one that merges (`{}...`), then one that signs the repodata (`{}...`)",
            SIGN_PACKAGES.trim_end(),
            verb(Kind::Index, want.suite).trim_end(),
            SIGN_REPO.trim_end()
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
