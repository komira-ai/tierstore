//! The rules for the channel jobs of `release.yml` (`channel-upload` and `tap`), which put a
//! published release on tierstore.dev and in the Homebrew tap (docs/design/distribution.md,
//! section 5). [`channel_problems`] is called from `release_problems`, so the one lint holds the
//! whole file.
//!
//! Each job is held to the shape that makes it safe to leave in the workflow before its owner has
//! made the settings, and safe to run after:
//! - it needs `plan` and `publish` (the release exists) and has the `if:` of `publish`;
//! - it names its environment, which holds the settings, and holds exactly the permissions the
//!   table below gives (the cloud token for `channel-upload`, nothing to write for `tap`);
//! - its first step that is not setup is the gate (`cargo xtask channels gate`), which is given
//!   each of the job's settings and none of their values, and every step after the gate runs only
//!   when the gate said `enabled=true`; no step before the gate assumes a role or mints a token;
//! - the role is assumed by one step of `channel-upload` alone, with the role of the environment;
//!   the release bot's token is minted by one step of `tap` alone, for the tap repository alone
//!   with the two permissions the pull request needs, in the second to last step, and read by the
//!   last step alone, which opens the pull request;
//! - the key of the bot, the role and the token action appear in no other job.

use super::lint::{RELEASE_IF, entries, entry, names};
use super::yaml::{Node, Value};

/// The `if:` of every step after the gate.
pub const GATE_IF: &str = "${{ steps.gate.outputs.enabled == 'true' }}";

/// The role the upload assumes.
const ROLE: &str = "${{ vars.TIERSTORE_RELEASES_ROLE_ARN }}";

/// What a channel job is held to.
pub struct Channel {
    pub job: &'static str,
    pub environment: &'static str,
    /// The permissions, sorted, `scope: level`.
    pub permissions: &'static [&'static str],
    /// The settings the gate is given.
    pub settings: &'static [&'static str],
}

pub const CHANNELS: &[Channel] = &[
    Channel {
        job: "channel-upload",
        environment: "prod-releases-tierstore",
        permissions: &["contents: read", "id-token: write"],
        settings: &["TIERSTORE_RELEASES_BUCKET", "TIERSTORE_RELEASES_ROLE_ARN"],
    },
    Channel {
        job: "tap",
        environment: "release",
        permissions: &["contents: read"],
        settings: &["RELEASE_BOT_APP_ID", "RELEASE_BOT_PRIVATE_KEY"],
    },
];

/// Words that belong to one channel job and to no other: `(word, job)`.
const OWNED: &[(&str, &str)] = &[
    ("RELEASE_BOT_PRIVATE_KEY", "tap"),
    ("create-github-app-token", "tap"),
    ("configure-aws-credentials", "channel-upload"),
    ("TIERSTORE_RELEASES_", "channel-upload"),
];

pub(super) fn steps(job: &Node) -> &[Node] {
    match entry(job, "steps").map(|s| &s.value) {
        Some(Value::Seq(items)) => items,
        _ => &[],
    }
}

/// Every scalar of `node`, keys and values.
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

pub(super) fn mentions(node: &Node, word: &str) -> bool {
    let mut all = Vec::new();
    scalars(node, &mut all);
    all.iter().any(|s| s.contains(word))
}

pub(super) fn text<'a>(step: &'a Node, key: &str) -> &'a str {
    entry(step, key).and_then(Node::as_str).unwrap_or_default()
}

pub(super) fn uses(step: &Node, action: &str) -> bool {
    text(step, "uses").starts_with(&format!("{action}@"))
}

/// One line per way the channel jobs of the `release.yml` document `doc` are not as the module
/// says.
pub fn channel_problems(doc: &Node) -> Vec<String> {
    let jobs = entry(doc, "jobs").map_or(&[][..], entries);
    let job = |name: &str| {
        jobs.iter()
            .find(|(k, _)| k.as_str() == Some(name))
            .map(|(_, v)| v)
    };
    let mut problems = Vec::new();
    for channel in CHANNELS {
        match job(channel.job) {
            Some(found) => problems.extend(job_problems(channel, found)),
            None => problems.push(format!("{}: there is no such job", channel.job)),
        }
    }
    for (name, found) in jobs {
        let name = name.as_str().unwrap_or_default();
        for (word, owner) in OWNED {
            // The apt jobs that hold the cloud role assume it with the same action.
            let apt_role = *word == "configure-aws-credentials" && super::apt::is_role_job(name);
            if name != *owner && !apt_role && mentions(found, word) {
                problems.push(format!("{name}: names {word}, which only {owner} may"));
            }
        }
    }
    problems
}

fn job_problems(channel: &Channel, job: &Node) -> Vec<String> {
    let name = channel.job;
    let mut problems = Vec::new();
    if names(entry(job, "needs")) != ["plan", "publish"] {
        problems.push(format!("{name}: needs are not plan and publish"));
    }
    if entry(job, "if").and_then(Node::as_str) != Some(RELEASE_IF) {
        problems.push(format!("{name}: `if` is not {RELEASE_IF}"));
    }
    if entry(job, "environment").and_then(Node::as_str) != Some(channel.environment) {
        problems.push(format!(
            "{name}: environment is not {} (it holds the settings)",
            channel.environment
        ));
    }
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
    if held != channel.permissions {
        problems.push(format!(
            "{name}: permissions are {held:?}, not {:?}",
            channel.permissions
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
    problems.extend(gate_problems(channel, &all[gate]));
    for step in &all[..gate] {
        for word in ["aws-actions/", "create-github-app-token", "secrets."] {
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
    match name {
        "channel-upload" => problems.extend(upload_problems(&all[gate + 1..])),
        _ => problems.extend(tap_problems(&all[gate + 1..])),
    }
    problems
}

fn gate_problems(channel: &Channel, gate: &Node) -> Vec<String> {
    let name = channel.job;
    let run = text(gate, "run");
    let mut problems = Vec::new();
    let start = format!("cargo xtask channels gate --label {name} ");
    if !run.starts_with(&start) {
        problems.push(format!("{name}: the gate does not run `{start}...`"));
    }
    for setting in channel.settings {
        let wanted = format!("--setting \"{setting}=${{");
        if !run.contains(&wanted) {
            problems.push(format!(
                "{name}: the gate is not given the setting {setting}"
            ));
        }
    }
    // The gate reads whether a value is set, never the value: `NAME=${VAR:+set}`.
    if run.matches("--setting ").count() != channel.settings.len()
        || run.matches(":+set}\"").count() != channel.settings.len()
    {
        problems.push(format!(
            "{name}: the gate must be given each setting as NAME=${{VAR:+set}}, and no other"
        ));
    }
    if run.contains("secrets.") || run.contains("vars.") {
        problems.push(format!("{name}: the gate's command line holds a value"));
    }
    problems
}

fn upload_problems(after_gate: &[Node]) -> Vec<String> {
    let mut problems = Vec::new();
    let roles: Vec<usize> = after_gate
        .iter()
        .enumerate()
        .filter(|(_, s)| uses(s, "aws-actions/configure-aws-credentials"))
        .map(|(i, _)| i)
        .collect();
    let uploads: Vec<usize> = after_gate
        .iter()
        .enumerate()
        .filter(|(_, s)| text(s, "run").contains("cargo xtask channels upload"))
        .map(|(i, _)| i)
        .collect();
    match (roles.as_slice(), uploads.as_slice()) {
        ([role], [upload]) => {
            let given = entry(&after_gate[*role], "with")
                .and_then(|w| entry(w, "role-to-assume"))
                .and_then(Node::as_str);
            if given != Some(ROLE) {
                problems.push(format!(
                    "channel-upload: the role assumed is not {ROLE}, the environment's"
                ));
            }
            if role > upload {
                problems.push("channel-upload: the role is assumed after the upload".to_owned());
            }
            let run = text(&after_gate[*upload], "run");
            for argument in [
                "--tag \"$TAG\"",
                "--commit \"$COMMIT\"",
                "--bucket \"$BUCKET\"",
            ] {
                if !run.contains(argument) {
                    problems.push(format!(
                        "channel-upload: the upload is not given {argument}"
                    ));
                }
            }
        }
        _ => problems.push(
            "channel-upload: it needs one step that assumes the role and one that uploads"
                .to_owned(),
        ),
    }
    if !after_gate
        .iter()
        .any(|s| text(s, "run").contains("cargo xtask release check dist"))
    {
        problems.push("channel-upload: no step checks the downloaded files".to_owned());
    }
    problems
}

fn tap_problems(after_gate: &[Node]) -> Vec<String> {
    let mut problems = Vec::new();
    let [.., mint, last] = after_gate else {
        problems.push("tap: it needs a mint step and a last step after the gate".to_owned());
        return problems;
    };
    if !uses(mint, "actions/create-github-app-token") || text(mint, "id") != "bot" {
        problems.push(
            "tap: the second to last step is not the token of the release bot (id `bot`)"
                .to_owned(),
        );
    }
    let with = |key: &str| {
        entry(mint, "with")
            .and_then(|w| entry(w, key))
            .and_then(Node::as_str)
    };
    if with("repositories") != Some("homebrew-tap") {
        problems.push("tap: the token is not for `repositories: homebrew-tap` alone".to_owned());
    }
    let mut permissions: Vec<String> = entry(mint, "with")
        .map_or(&[][..], entries)
        .iter()
        .filter_map(|(k, v)| {
            let key = k.as_str()?;
            key.starts_with("permission-")
                .then(|| format!("{key}: {}", v.as_str().unwrap_or_default()))
        })
        .collect();
    permissions.sort();
    if permissions
        != [
            "permission-contents: write",
            "permission-pull-requests: write",
        ]
    {
        problems.push(format!(
            "tap: the token's permissions are {permissions:?}, not contents and pull requests write"
        ));
    }
    if !text(last, "run").contains("cargo xtask channels tap ") {
        problems.push("tap: the last step does not run `cargo xtask channels tap`".to_owned());
    }
    for step in after_gate {
        let reads = mentions(step, "steps.bot.outputs.token");
        let is_last = std::ptr::eq(step, last);
        if reads && !is_last {
            problems.push(format!(
                "tap: the step `{}` reads the bot's token; only the last step may",
                text(step, "name")
            ));
        }
        if is_last && !reads {
            problems.push("tap: the last step is not given the bot's token".to_owned());
        }
    }
    problems
}
