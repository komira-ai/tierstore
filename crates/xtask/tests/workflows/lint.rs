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
        file: "release.yml",
        job: "gamma",
        scope: "id-token",
        condition: GAMMA_IF,
    },
    Allowed {
        file: "advisories.yml",
        job: "advisories",
        scope: "issues",
        condition: MAIN_ONLY,
    },
];

pub fn entry<'a>(map: &'a Node, key: &str) -> Option<&'a Node> {
    match &map.value {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

pub fn entries(map: &Node) -> &[(Node, Node)] {
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

/// The condition of `gamma`: [`RELEASE_IF`] and a configured coordinator. An unconfigured
/// repository skips the job, so the `release` environment is never resolved for it (a reviewer
/// or wait rule added to the environment later must not hold every release while the stage is
/// off), and the job-level condition is the whole of the off switch: no step carries one.
pub const GAMMA_IF: &str = "${{ needs.plan.outputs.release == 'true' && github.ref == 'refs/heads/main' && vars.KBF_COORDINATOR_URL != '' }}";

/// What the `if:` of the plan job must say of the run that started it: it is a completed
/// run that succeeded, for a push to `main` of this repository.
const PLAN_GUARDS: [&str; 4] = [
    "github.event.workflow_run.conclusion == 'success'",
    "github.event.workflow_run.event == 'push'",
    "github.event.workflow_run.head_branch == 'main'",
    "github.event.workflow_run.head_repository.full_name == github.repository",
];

/// The `if:` of the plan job, whole: a manual run of `main`, or a finished run that succeeded
/// for a push to `main` of this repository.
const PLAN_IF: &str = "${{ github.ref == 'refs/heads/main' && (github.event_name == 'workflow_dispatch' || (github.event_name == 'workflow_run' && github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_branch == 'main' && github.event.workflow_run.head_repository.full_name == github.repository)) }}";

/// The checkout of the plan job: the commit the run built, or the one a manual run names.
const PLAN_REF: &str = "${{ github.event_name == 'workflow_run' && github.event.workflow_run.head_sha || inputs.commit }}";

/// The `cargo xtask` verbs release.yml may run; the others build or fetch.
const XTASK_VERBS: [&str; 5] = ["--help", "release", "sums", "verify-assets", "deploy-farm"];

pub fn names(node: Option<&Node>) -> Vec<String> {
    let mut found = node.map(triggers).unwrap_or_default();
    found.sort();
    found
}

/// One line per way the text of `release.yml` is not the automatic release chain: started by
/// the completion of `ci` and `artifacts` on `main` and by hand and by nothing else, one run at
/// a time and none cancelled, a read-only `plan` job that answers whether and as which tag,
/// `verify` and `publish` after it (the write scope in `publish` alone), `gamma` after them
/// (the OIDC token and nothing else that is not read), and no step that builds or signs.
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
    problems.extend(concurrency_problems("release.yml", text));
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    let job = |name: &str| {
        jobs.iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let mut listed: Vec<&str> = jobs.iter().filter_map(|(k, _)| k.as_str()).collect();
    listed.sort_unstable();
    if listed != ["gamma", "plan", "publish", "verify"] {
        problems.push(format!(
            "jobs are {listed:?}, not plan, verify, publish and gamma"
        ));
    }
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or_default();
        let permissions = entry(job, "permissions").map_or(&[][..], entries);
        for (scope, level) in permissions {
            let level = level.as_str().unwrap_or_default();
            let writes = level != "read" && level != "none";
            let scope = scope.as_str().unwrap_or_default();
            if writes && name != "publish" && !(name == "gamma" && scope == "id-token") {
                problems.push(format!(
                    "{name}: permissions grant `{scope}: {level}`; only publish writes (and gamma, the OIDC token)"
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
    // Pinned whole: every part above can be present while an `&&` has become an `||`.
    if plan_if != PLAN_IF {
        problems.push(format!("plan: `if` is not {PLAN_IF}"));
    }
    let steps: &[Node] = match plan.and_then(|j| entry(j, "steps")).map(|s| &s.value) {
        Some(Value::Seq(items)) => items,
        _ => &[],
    };
    let checkout = steps.iter().find(|s| {
        entry(s, "uses")
            .and_then(Node::as_str)
            .is_some_and(|u| u.starts_with("actions/checkout@"))
    });
    let reference = checkout
        .and_then(|s| entry(s, "with"))
        .and_then(|w| entry(w, "ref"))
        .and_then(Node::as_str);
    if reference != Some(PLAN_REF) {
        problems.push(format!("plan: checkout ref is not {PLAN_REF}"));
    }
    let auto = steps
        .iter()
        .find(|s| entry(s, "id").and_then(Node::as_str) == Some("auto"));
    let auto_env = |key: &str| {
        auto.and_then(|s| entry(s, "env"))
            .and_then(|e| entry(e, key))
            .and_then(Node::as_str)
    };
    if auto_env("COMMIT") != Some("${{ github.event.workflow_run.head_sha }}") {
        problems
            .push("plan: env COMMIT is not ${{ github.event.workflow_run.head_sha }}".to_owned());
    }
    if auto_env("FREEZE") != Some("${{ vars.RELEASE_FREEZE }}") {
        problems.push("plan: env FREEZE is not ${{ vars.RELEASE_FREEZE }}".to_owned());
    }
    let auto_run = auto
        .and_then(|s| entry(s, "run"))
        .and_then(Node::as_str)
        .unwrap_or_default();
    for argument in ["--freeze \"$FREEZE\"", "--commit \"$COMMIT\""] {
        if !auto_run.contains(argument) {
            problems.push(format!("plan: {argument} is not passed to release plan"));
        }
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
        ("gamma", vec!["plan", "publish"]),
    ] {
        let got = names(job(name).and_then(|j| entry(j, "needs")));
        if got != needs {
            problems.push(format!("{name}: needs are {got:?}, not {needs:?}"));
        }
        let condition = job(name)
            .and_then(|j| entry(j, "if"))
            .and_then(Node::as_str);
        let want = if name == "gamma" {
            GAMMA_IF
        } else {
            RELEASE_IF
        };
        if condition != Some(want) {
            problems.push(format!("{name}: `if` is not {want}"));
        }
    }
    // The gamma stage holds the run's OIDC token and nothing else that is not read, and runs
    // in the `release` environment, the one the coordinator's allow-list names.
    let gamma = job("gamma");
    let mut held: Vec<String> = gamma
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
    if held != ["contents: read", "id-token: write"] {
        problems.push(format!(
            "gamma: permissions are {held:?}, not contents: read and id-token: write"
        ));
    }
    let environment = gamma
        .and_then(|j| entry(j, "environment"))
        .and_then(Node::as_str);
    if environment != Some("release") {
        problems.push(format!(
            "gamma: environment is {environment:?}, not \"release\""
        ));
    }
    // The kbf checkout whose verb gamma runs is pinned in the file, as a commit, so a change of
    // it is a reviewed change and a kbf main that moves or breaks does not move the release.
    let pin = gamma
        .and_then(|j| entry(j, "env"))
        .and_then(|e| entry(e, "KBF_DEPLOY_REF"))
        .and_then(Node::as_str);
    if !pin
        .is_some_and(|p| p.len() == 40 && p.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    {
        problems.push(format!(
            "gamma: env KBF_DEPLOY_REF is {pin:?}, not 40 lowercase hex digits"
        ));
    }
    let checkout_ref = gamma
        .and_then(|j| entry(j, "steps"))
        .and_then(|s| match &s.value {
            Value::Seq(items) => items.iter().find(|i| {
                entry(i, "with")
                    .and_then(|w| entry(w, "repository"))
                    .and_then(Node::as_str)
                    == Some("komira-ai/kbf")
            }),
            _ => None,
        })
        .and_then(|i| entry(i, "with"))
        .and_then(|w| entry(w, "ref"))
        .and_then(Node::as_str);
    if checkout_ref != Some("${{ env.KBF_DEPLOY_REF }}") {
        problems.push("gamma: the kbf checkout ref is not ${{ env.KBF_DEPLOY_REF }}".to_owned());
    }
    let persist = gamma
        .and_then(|j| entry(j, "steps"))
        .and_then(|s| match &s.value {
            Value::Seq(items) => items.iter().find(|i| {
                entry(i, "with")
                    .and_then(|w| entry(w, "repository"))
                    .and_then(Node::as_str)
                    == Some("komira-ai/kbf")
            }),
            _ => None,
        })
        .and_then(|i| entry(i, "with"))
        .and_then(|w| entry(w, "persist-credentials"))
        .and_then(Node::as_str);
    if persist != Some("false") {
        problems.push(format!(
            "gamma: the kbf checkout persists credentials ({persist:?}, not \"false\")"
        ));
    }
    // The command: tierstore's own service and the version its hosts report, and none of kbf's
    // minimums (for another service the services file of the coordinator holds them).
    match runs(gamma)
        .into_iter()
        .find(|r| r.contains("cargo xtask deploy-farm"))
    {
        None => problems.push("gamma: no step runs `cargo xtask deploy-farm`".to_owned()),
        Some(run) => {
            if !run.contains("--service tierstore") {
                problems.push("gamma: deploy-farm is not run with --service tierstore".to_owned());
            }
            if !run.contains("--version \"${TAG#v}+${COMMIT:0:12}\"") {
                problems.push(
                    "gamma: deploy-farm is not passed --version \"${TAG#v}+${COMMIT:0:12}\""
                        .to_owned(),
                );
            }
            for flag in ["--min-up", "--require-coordinator-current"] {
                if run.contains(flag) {
                    problems.push(format!("gamma: deploy-farm is passed {flag}, a kbf flag"));
                }
            }
        }
    }
    // The text of `--version` is built from the plan's tag and commit, in the step's own env.
    let deploy = gamma
        .and_then(|j| entry(j, "steps"))
        .and_then(|s| match &s.value {
            Value::Seq(items) => items.iter().find(|i| {
                entry(i, "run")
                    .and_then(Node::as_str)
                    .is_some_and(|r| r.contains("cargo xtask deploy-farm"))
            }),
            _ => None,
        });
    for (var, source) in [
        ("TAG", "${{ needs.plan.outputs.tag }}"),
        ("COMMIT", "${{ needs.plan.outputs.commit }}"),
    ] {
        let got = deploy
            .and_then(|d| entry(d, "env"))
            .and_then(|e| entry(e, var))
            .and_then(Node::as_str);
        if got != Some(source) {
            problems.push(format!("gamma: env {var} is {got:?}, not {source}"));
        }
    }
    // The binary of the files about to be published says the tag and the commit, checked in
    // verify, before the publish job (which needs verify) can run.
    let verify = job("verify");
    let env_tag = verify
        .and_then(|j| entry(j, "env"))
        .and_then(|e| entry(e, "TAG"))
        .and_then(Node::as_str);
    if env_tag != Some("${{ needs.plan.outputs.tag }}") {
        problems.push("verify: env TAG is not ${{ needs.plan.outputs.tag }}".to_owned());
    }
    let check = runs(verify)
        .into_iter()
        .find(|r| r.contains("release check-binary"));
    match check {
        None => problems.push("verify: no step runs `cargo xtask release check-binary`".to_owned()),
        Some(run) if !(run.contains("--tag \"$TAG\"") && run.contains("--commit \"$COMMIT\"")) => {
            problems.push(
                "verify: the check-binary step must pass --tag \"$TAG\" and --commit \"$COMMIT\""
                    .to_owned(),
            );
        }
        Some(_) => {}
    }
    if !text.contains("vars.RELEASE_FREEZE") {
        problems.push("plan does not read vars.RELEASE_FREEZE".to_owned());
    }
    problems.extend(build_problems(text));
    problems
}

/// A line of `text` that builds, fetches or signs: a `cargo` that is not `cargo xtask <verb>`
/// of [`XTASK_VERBS`], or the attest actions.
///
/// The rule is about the release FILES: nothing in release.yml builds or signs what is
/// published. The `gamma` job does build kbf's xtask (`cargo xtask --help`, then `deploy-farm`),
/// which is a tool that talks to the coordinator and produces no release file; do not tighten
/// this rule to forbid it.
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
pub fn artifacts_problems(text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![e],
    };
    let jobs = entry(&doc, "jobs").map_or(&[][..], entries);
    let job = |name: &str| {
        jobs.iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let mut problems = Vec::new();
    let concurrency = entry(&doc, "concurrency");
    let group = concurrency
        .and_then(|c| entry(c, "group"))
        .and_then(Node::as_str);
    if group != Some(ARTIFACTS_GROUP) {
        problems.push(format!(
            "artifacts.yml: concurrency group is {group:?}, not {ARTIFACTS_GROUP}"
        ));
    }
    let cancel = concurrency
        .and_then(|c| entry(c, "cancel-in-progress"))
        .and_then(Node::as_str);
    if cancel != Some(ARTIFACTS_CANCEL) {
        problems.push(format!(
            "artifacts.yml: concurrency cancel-in-progress is {cancel:?}, not {ARTIFACTS_CANCEL}"
        ));
    }
    let version = job("version");
    let version_runs = runs(version);
    if !version_runs.iter().any(|r| r.contains("release version")) {
        problems.push("version job: no step runs `cargo xtask release version`".to_owned());
    }
    if !version_runs
        .iter()
        .any(|r| r.contains("--version-file VERSION"))
    {
        problems.push("version job: release version lacks --version-file VERSION".to_owned());
    }
    let output = version
        .and_then(|j| entry(j, "outputs"))
        .and_then(|o| entry(o, "version"))
        .and_then(Node::as_str);
    if output != Some("${{ steps.version.outputs.version }}") {
        problems.push("version job: output version is not the version step's".to_owned());
    }
    for platform in xtask::package::Platform::ALL {
        let name = platform.name();
        let build = job(name);
        let needs = names(build.and_then(|j| entry(j, "needs")));
        if needs != ["version"] {
            problems.push(format!("{name}: needs are {needs:?}, not [\"version\"]"));
        }
        let given = build
            .and_then(|j| entry(j, "env"))
            .and_then(|e| entry(e, "TIERSTORE_BUILD_VERSION_OVERRIDE"))
            .and_then(Node::as_str);
        if given != Some("${{ needs.version.outputs.version }}") {
            problems.push(format!(
                "{name}: env TIERSTORE_BUILD_VERSION_OVERRIDE is not ${{{{ needs.version.outputs.version }}}}"
            ));
        }
        let checked = runs(build).into_iter().any(|r| {
            r.contains("build-commit")
                && r.contains("--version \"$TIERSTORE_BUILD_VERSION_OVERRIDE\"")
        });
        if !checked {
            problems.push(format!(
                "{name}: the build-commit step does not check --version \"$TIERSTORE_BUILD_VERSION_OVERRIDE\""
            ));
        }
    }
    problems
}

/// The `run:` text of each step of `job`.
pub fn runs(job: Option<&Node>) -> Vec<&str> {
    let steps = job.and_then(|j| entry(j, "steps"));
    match steps.map(|s| &s.value) {
        Some(Value::Seq(items)) => items
            .iter()
            .filter_map(|s| entry(s, "run").and_then(Node::as_str))
            .collect(),
        _ => Vec::new(),
    }
}

/// Workflows that look like the release path to [`discovery_problems`] (or are named for it)
/// and are outside the release group on purpose: the file and the reason. A reason is required.
pub const RELEASE_EXEMPT: &[(&str, &str)] = &[(
    "artifacts.yml",
    "builds and attests one commit and never tags, publishes or deploys; its own per-commit group",
)];

/// The group every release-path workflow is in: a run that succeeded for a push to this
/// repository joins `release-main`; any other takes a group of its own, so it cannot replace
/// the release that is waiting. kbf's expression is
/// `release-${{ github.event.workflow_run.conclusion == 'success' && 'main' || github.run_id }}`;
/// this one is stricter: a pull request from a fork's branch named main also matches
/// `branches: [main]` of `workflow_run`, and would take the one pending slot.
pub const RELEASE_GROUP: &str = "release-${{ (github.event.workflow_run.conclusion == 'success' && github.event.workflow_run.event == 'push' && github.event.workflow_run.head_repository.full_name == github.repository) && 'main' || github.run_id }}";

/// The group of artifacts.yml: one per commit (a pull request: per ref), outside the release
/// group because it only builds and attests.
pub const ARTIFACTS_GROUP: &str =
    "artifacts-${{ github.event_name == 'pull_request' && github.ref || github.sha }}";

/// A push run of artifacts.yml is never cancelled; a pull request's newer run replaces its older.
pub const ARTIFACTS_CANCEL: &str = "${{ github.event_name == 'pull_request' }}";

/// One line per release-path workflow of `files` (name, text) that is neither in the release
/// group without cancelling nor on `exempt` with a reason, and per exempt entry that has no
/// reason or names no file. A workflow is on the release path when it can tag, publish or
/// deploy: `contents: write`, `gh release`, a `git tag` or a tag push, a farm deploy.
/// `id-token: write` with attest actions alone is not.
pub fn discovery_problems(files: &[(String, String)], exempt: &[(&str, &str)]) -> Vec<String> {
    let mut problems = Vec::new();
    for (file, reason) in exempt {
        if reason.trim().is_empty() {
            problems.push(format!(
                "{file}: exempt from the release group without a reason"
            ));
        }
        if !files.iter().any(|(name, _)| name == file) {
            problems.push(format!("{file}: exempt, but there is no such workflow"));
        }
    }
    for (file, text) in files {
        if !on_release_path(text) || exempt.iter().any(|(name, _)| name == file) {
            continue;
        }
        let found = concurrency_problems(file, text);
        if !found.is_empty() {
            problems.push(format!(
                "{file}: tags, publishes or deploys, so it must be in the release group (or exempt, with a reason):"
            ));
            problems.extend(found);
        }
    }
    problems
}

/// What marks a step or permission as tagging, publishing or deploying.
const RELEASE_MARKS: [&str; 6] = [
    "contents: write",
    "gh release",
    "git tag",
    "refs/tags",
    "deploy-farm",
    "farm-deploy",
];

/// Whether the workflow `text` can tag, publish or deploy: a line that is not a comment holds
/// one of [`RELEASE_MARKS`]. Signing alone (`id-token`, the attest actions) is not.
fn on_release_path(text: &str) -> bool {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .any(|l| RELEASE_MARKS.iter().any(|m| l.contains(m)))
}

/// One line per way the workflow `file` is not in the shared release group without cancelling.
pub fn concurrency_problems(file: &str, text: &str) -> Vec<String> {
    let doc = match yaml::load(text) {
        Ok(doc) => doc,
        Err(e) => return vec![format!("{file}: {e}")],
    };
    let Some(concurrency) = entry(&doc, "concurrency") else {
        return vec![format!(
            "{file}: no top-level `concurrency` (group: {RELEASE_GROUP})"
        )];
    };
    let mut problems = Vec::new();
    let group = entry(concurrency, "group").and_then(Node::as_str);
    if group != Some(RELEASE_GROUP) {
        problems.push(format!(
            "{file}: concurrency group is {group:?}, not {RELEASE_GROUP}"
        ));
    }
    let cancel = entry(concurrency, "cancel-in-progress").and_then(Node::as_str);
    if cancel != Some("false") {
        problems.push(format!(
            "{file}: concurrency cancel-in-progress is {cancel:?}, not false"
        ));
    }
    problems
}
