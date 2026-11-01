//! The rules every workflow of this repository is held to.
//!
//! [`lint`] reads one workflow file and returns one line per problem, none when it is clean.
//! A workflow is refused when:
//! - it lacks a top-level `permissions: {}`, so that jobs start with no token rights;
//! - its `on` names `pull_request_target` (it runs with the base repository's token and
//!   secrets on code from a fork), or any key or value names `self-hosted`;
//! - a step's (or a job's) `uses:` is not an action pinned to a full 40-character commit SHA
//!   followed on the same line by a version comment (`# v1.2.3`), or names a local path;
//! - a job's `permissions` is not a mapping, or grants `write` for a scope that is not in
//!   [`WRITE_ALLOWED`] for that workflow file and job, or grants it under another `if:` than
//!   the one the table gives. So `id-token` and `attestations` (the scopes that sign) are
//!   written by the one `attest` job of `artifacts.yml`, on a push to `main` alone, and by
//!   the site's deploy job; `contents: write` by the release's `publish` job alone.
//!
//! Keys are matched exactly. What this does not evaluate it refuses: a YAML tag, a second
//! document, an alias to an unfinished node, a document that does not parse.

use super::yaml::{self, Node, Value};

/// A write permission a job may hold: the workflow file, the job, the scope, and the `if:` of
/// the job, exactly.
pub struct Allowed {
    pub file: &'static str,
    pub job: &'static str,
    pub scope: &'static str,
    pub condition: &'static str,
}

/// Attestations are signed for a push to `main` and for nothing else.
pub const SIGN_IF: &str = "${{ github.event_name == 'push' && github.ref == 'refs/heads/main' }}";

/// The site's deploy runs for a push to `main` or a manual run of `main`, once the repository
/// variable that turns publishing on is set.
pub const SITE_IF: &str = "${{ (github.event_name == 'push' || github.event_name == 'workflow_dispatch') && github.ref == 'refs/heads/main' && vars.TIERSTORE_SITE_PUBLISH == 'true' }}";

/// A manual run of `main`.
pub const MAIN_ONLY: &str = "${{ github.ref == 'refs/heads/main' }}";

pub const WRITE_ALLOWED: &[Allowed] = &[
    Allowed {
        file: "artifacts.yml",
        job: "attest",
        scope: "id-token",
        condition: SIGN_IF,
    },
    Allowed {
        file: "artifacts.yml",
        job: "attest",
        scope: "attestations",
        condition: SIGN_IF,
    },
    Allowed {
        file: "tierstore-site-publish.yml",
        job: "deploy",
        scope: "id-token",
        condition: SITE_IF,
    },
    Allowed {
        file: "release.yml",
        job: "publish",
        scope: "contents",
        condition: RELEASE_IF,
    },
    Allowed {
        file: "advisories.yml",
        job: "advisories",
        scope: "issues",
        condition: MAIN_ONLY,
    },
];

fn entry<'a>(map: &'a Node, key: &str) -> Option<&'a Node> {
    match &map.value {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

fn entries(map: &Node) -> &[(Node, Node)] {
    match &map.value {
        Value::Map(entries) => entries,
        _ => &[],
    }
}

/// The names of the triggers `on` lists, in any of the three shapes GitHub reads.
fn triggers(on: &Node) -> Vec<String> {
    match &on.value {
        Value::Scalar { .. } => on.as_str().map(str::to_owned).into_iter().collect(),
        Value::Seq(items) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_owned))
            .collect(),
        Value::Map(entries) => entries
            .iter()
            .filter_map(|(k, _)| k.as_str().map(str::to_owned))
            .collect(),
    }
}

/// Every scalar of the tree, keys and values.
fn scalars<'a>(node: &'a Node, out: &mut Vec<&'a str>) {
    match &node.value {
        Value::Scalar { .. } => out.extend(node.as_str()),
        Value::Seq(items) => items.iter().for_each(|i| scalars(i, out)),
        Value::Map(entries) => {
            for (k, v) in entries {
                scalars(k, out);
                scalars(v, out);
            }
        }
    }
}

/// Every `uses:` value of the tree with its line.
fn uses<'a>(node: &'a Node, out: &mut Vec<(usize, &'a str)>) {
    match &node.value {
        Value::Scalar { .. } => {}
        Value::Seq(items) => items.iter().for_each(|i| uses(i, out)),
        Value::Map(entries) => {
            for (k, v) in entries {
                match (k.as_str(), v.as_str()) {
                    (Some("uses"), Some(value)) => out.push((v.line(), value)),
                    _ => uses(v, out),
                }
            }
        }
    }
}

/// What is wrong with the `uses:` value `value` on `line` of `text`, if anything.
fn pin_problem(text: &str, line: usize, value: &str) -> Option<String> {
    let Some((action, rest)) = value.split_once('@') else {
        return Some(format!(
            "line {line}: `uses: {value}` is not pinned (owner/repo@<40 hex digits>)"
        ));
    };
    if action.starts_with("./") || action.starts_with('.') {
        return Some(format!("line {line}: `uses: {value}` names a local path"));
    }
    let hex = rest.len() == 40 && rest.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !hex {
        return Some(format!(
            "line {line}: `uses: {value}` is not pinned to a 40-digit lowercase commit SHA"
        ));
    }
    let source = text.lines().nth(line - 1).unwrap_or_default();
    let comment = source.split_once(&format!("{value} # ")).map(|(_, c)| c);
    let versioned = comment.is_some_and(|c| {
        c.strip_prefix('v')
            .is_some_and(|v| v.starts_with(|ch: char| ch.is_ascii_digit()))
    });
    if versioned {
        None
    } else {
        Some(format!(
            "line {line}: `uses: {value}` has no version comment (`# v1.2.3`) on its line"
        ))
    }
}

/// One line per problem in the workflow `text` of the file named `file` (`artifacts.yml`).
pub fn lint(file: &str, text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let mut problems = Vec::new();
    if !matches!(doc.value, Value::Map(_)) {
        return vec!["the document is not a mapping".to_owned()];
    }
    match entry(&doc, "permissions") {
        Some(p) if p.is_empty_map() => {}
        _ => problems.push("no top-level `permissions: {}`".to_owned()),
    }
    if let Some(on) = entry(&doc, "on")
        && triggers(on).iter().any(|t| t == "pull_request_target")
    {
        problems.push("`on` names pull_request_target".to_owned());
    }
    let mut all = Vec::new();
    scalars(&doc, &mut all);
    for word in ["self-hosted", "pull_request_target"] {
        if all.iter().any(|s| s.contains(word)) {
            problems.push(format!("the document names {word}"));
        }
    }
    let mut pinned = Vec::new();
    uses(&doc, &mut pinned);
    for (line, value) in pinned {
        problems.extend(pin_problem(text, line, value));
    }
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or_default();
        problems.extend(permission_problems(file, name, job));
    }
    problems
}

fn permission_problems(file: &str, name: &str, job: &Node) -> Vec<String> {
    let Some(permissions) = entry(job, "permissions") else {
        return Vec::new();
    };
    if !matches!(permissions.value, Value::Map(_)) {
        return vec![format!(
            "job {name}: `permissions` is not a mapping of scopes"
        )];
    }
    let condition = entry(job, "if").and_then(Node::as_str);
    let mut problems = Vec::new();
    for (scope, level) in entries(permissions) {
        let (scope, level) = (scope.as_str().unwrap_or_default(), level.as_str());
        if level == Some("read") || level == Some("none") {
            continue;
        }
        let allowed = WRITE_ALLOWED
            .iter()
            .find(|a| a.file == file && a.job == name && a.scope == scope);
        match (level, allowed) {
            (Some("write"), Some(a)) if condition == Some(a.condition) => {}
            (Some("write"), Some(a)) => problems.push(format!(
                "job {name}: `{scope}: write` needs the exact `if: {}`",
                a.condition
            )),
            _ => problems.push(format!(
                "job {name}: `{scope}: {}` is not allowed in {file}",
                level.unwrap_or("(none)")
            )),
        }
    }
    problems
}

/// The condition of the jobs after the plan: the plan said to release, on a run of `main`.
pub const RELEASE_IF: &str =
    "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' }}";

/// What the `if:` of the plan job must say of the run that started it: it is a completed
/// run that succeeded, for a push to `main` of this repository.
const PLAN_GUARDS: [&str; 4] = [
    "github.event.workflow_run.conclusion == 'success'",
    "github.event.workflow_run.event == 'push'",
    "github.event.workflow_run.head_branch == 'main'",
    "github.event.workflow_run.head_repository.full_name == github.repository",
];

/// The `cargo xtask` verbs release.yml may run; the others build or fetch.
const XTASK_VERBS: [&str; 4] = ["--help", "release", "sums", "verify-assets"];

fn names(node: Option<&Node>) -> Vec<String> {
    let mut found = node.map(triggers).unwrap_or_default();
    found.sort();
    found
}

/// One line per way the text of `release.yml` is not the automatic release chain: started by
/// the completion of `ci` and `artifacts` on `main` and by hand and by nothing else, one run at
/// a time and none cancelled, a read-only `plan` job that answers whether and as which tag,
/// `verify` and `publish` after it (the write scope in `publish` alone), and no step that
/// builds or signs.
pub fn release_problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let mut problems = Vec::new();
    let on = entry(&doc, "on");
    let found = names(on);
    if found != ["workflow_dispatch", "workflow_run"] {
        problems.push(format!(
            "triggers are {found:?}, not workflow_dispatch and workflow_run"
        ));
    }
    let run = on.and_then(|o| entry(o, "workflow_run"));
    for (key, want) in [
        ("workflows", vec!["artifacts", "ci"]),
        ("types", vec!["completed"]),
        ("branches", vec!["main"]),
    ] {
        let got = names(run.and_then(|r| entry(r, key)));
        if got != want {
            problems.push(format!("workflow_run.{key} is {got:?}, not {want:?}"));
        }
    }
    let group = entry(&doc, "concurrency").and_then(|c| entry(c, "group"));
    let group = group.and_then(Node::as_str).unwrap_or_default();
    if !(group.starts_with("release-") && group.contains("'main'")) {
        problems.push(format!("concurrency group {group:?} is not release-main"));
    }
    let cancel = entry(&doc, "concurrency").and_then(|c| entry(c, "cancel-in-progress"));
    if cancel.and_then(Node::as_str) != Some("false") {
        problems.push("concurrency cancel-in-progress is not false".to_owned());
    }
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    let job = |name: &str| {
        jobs.iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let mut listed: Vec<&str> = jobs.iter().filter_map(|(k, _)| k.as_str()).collect();
    listed.sort_unstable();
    if listed != ["plan", "publish", "verify"] {
        problems.push(format!("jobs are {listed:?}, not plan, verify and publish"));
    }
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or_default();
        let permissions = entry(job, "permissions").map_or(&[][..], entries);
        for (scope, level) in permissions {
            let level = level.as_str().unwrap_or_default();
            let writes = level != "read" && level != "none";
            if writes && name != "publish" {
                problems.push(format!(
                    "{name}: permissions grant `{}: {level}`; only publish writes",
                    scope.as_str().unwrap_or_default()
                ));
            }
        }
    }
    let plan = job("plan");
    let plan_if = plan
        .and_then(|j| entry(j, "if"))
        .and_then(Node::as_str)
        .unwrap_or_default();
    for guard in PLAN_GUARDS {
        if !plan_if.contains(guard) {
            problems.push(format!("plan: `if` lacks {guard}"));
        }
    }
    if !plan_if.contains("github.event_name == 'workflow_dispatch'") {
        problems.push("plan: `if` lacks github.event_name == 'workflow_dispatch'".to_owned());
    }
    let mut held: Vec<String> = plan
        .and_then(|j| entry(j, "permissions"))
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
    if held != ["actions: read", "contents: read"] {
        problems.push(format!(
            "plan: permissions are {held:?}, not contents: read and actions: read"
        ));
    }
    for (name, needs) in [
        ("verify", vec!["plan"]),
        ("publish", vec!["plan", "verify"]),
    ] {
        let got = names(job(name).and_then(|j| entry(j, "needs")));
        if got != needs {
            problems.push(format!("{name}: needs are {got:?}, not {needs:?}"));
        }
        let condition = job(name)
            .and_then(|j| entry(j, "if"))
            .and_then(Node::as_str);
        if condition != Some(RELEASE_IF) {
            problems.push(format!("{name}: `if` is not {RELEASE_IF}"));
        }
    }
    if !text.contains("vars.RELEASE_FREEZE") {
        problems.push("plan does not read vars.RELEASE_FREEZE".to_owned());
    }
    problems.extend(build_problems(text));
    problems
}

/// A line of `text` that builds, fetches or signs: a `cargo` that is not `cargo xtask <verb>`
/// of [`XTASK_VERBS`], or the attest actions.
fn build_problems(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim_start().starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace().peekable();
        while let Some(word) = words.next() {
            let next = words.peek().copied().unwrap_or_default();
            let after = line
                .split_once("cargo xtask ")
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .unwrap_or_default();
            let bad = match word {
                "cargo" => next != "xtask" || !XTASK_VERBS.contains(&after),
                _ => word.contains("attest-") || word == "--release",
            };
            if bad {
                problems.push(format!("line {}: release.yml builds or signs", n + 1));
                break;
            }
        }
    }
    problems
}

/// One line per way the text of `artifacts.yml` does not build under the version the release
/// will tag.
pub fn artifacts_problems(_text: &str) -> Vec<String> {
    vec!["not written".to_owned()]
}

/// The workflows on the release path, each of which must sit in the one release group: the
/// release now, and the farm deploy when it is added to this list.
pub const RELEASE_PATH_WORKFLOWS: &[&str] = &["release.yml"];

/// The group every release-path workflow is in (the expression kbf's release.yml uses): a run
/// that succeeded joins `release-main`; any other takes a group of its own, so it cannot
/// replace the release that is waiting.
pub const RELEASE_GROUP: &str =
    "release-${{ github.event.workflow_run.conclusion == 'success' && 'main' || github.run_id }}";

/// One line per way the workflow `file` (of [`RELEASE_PATH_WORKFLOWS`]) is not in the shared
/// release group without cancelling.
pub fn concurrency_problems(_file: &str, _text: &str) -> Vec<String> {
    vec!["not written".to_owned()]
}
