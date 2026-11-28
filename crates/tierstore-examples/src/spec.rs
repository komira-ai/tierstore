//! `example.toml`: the client of an example, what the runner generates, the steps it runs and the
//! checks whose lines it compares with `expected.txt`.
//!
//! ```toml
//! [client]                      # checked before anything runs
//! program      = "sccache"      # found on the search path
//! version_args = ["--version"]
//! version      = "sccache 0.19.0"   # the first line it must print
//!
//! [[generate]]                  # one file for each n of 1..=count
//! template = "unit.c.in"        # a file of the example; `${n}` in it is n
//! path     = "project/src/unit_${n}.c"   # under the work directory
//! count    = 30
//!
//! [env]                         # for every command, on top of PATH, HOME, TMPDIR and LC_ALL
//! AWS_ACCESS_KEY_ID = "${access_key_id}"
//!
//! [[step]]                      # /metricz is read before and after each step
//! name     = "build1"
//! env      = { SCCACHE_DIR = "${work}/sccache-1" }
//! commands = [
//!   { argv = ["make", "-C", "${work}/project"] },
//!   { argv = ["sccache", "--show-stats", "--stats-format", "json"], stdout = "build1-stats.json" },
//! ]
//!
//! [[cleanup]]                   # runs at the end whatever happened; its failures are logged
//! argv = ["sccache", "--stop-server"]
//!
//! [[check]]                     # one line of expected.txt; exactly one kind of check
//! name = "build1.sccache.misses"
//! json = { file = "build1-stats.json", pointer = "/stats/cache_misses/counts/C", default = "0" }
//! ```
//!
//! The kinds of check: `json` (the value at a JSON pointer of a file a command wrote, or `default`
//! where there is none), `metric` (how much a series of `/metricz` rose over a step), `same`
//! (whether two files or directory trees of the work directory hold the same bytes, and how many
//! files) and `objects`
//! (how many objects the bucket lists under a prefix). A check with `expected_failure` stands for
//! a call that fails because tierstore lacks a feature: its line names the issue, and the run fails
//! when its value becomes `passing`, so the check is updated when the feature lands.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::Deserialize;

/// The file name of an example's spec.
pub const SPEC_FILE: &str = "example.toml";
/// The file name of an example's tierstore configuration.
pub const CONFIG_FILE: &str = "tierstore.toml";
/// The file name of an example's expected lines.
pub const EXPECTED_FILE: &str = "expected.txt";
/// The file name of an example's page.
pub const PAGE_FILE: &str = "page.md";

/// An example's `example.toml`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Example {
    /// The client and its pinned version.
    pub client: Client,
    /// The inputs the runner writes before the steps.
    #[serde(default)]
    pub generate: Vec<Generate>,
    /// The environment of every command, on top of the runner's own.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The steps, in order.
    pub step: Vec<Step>,
    /// The commands that run after the steps, or after the step that failed.
    #[serde(default)]
    pub cleanup: Vec<Command>,
    /// The checks, in the order of their lines.
    pub check: Vec<Check>,
}

/// The program an example runs against tierstore.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Client {
    /// Its name on the search path, or its path.
    pub program: String,
    /// The arguments with which it prints its version.
    pub version_args: Vec<String>,
    /// The first line it must print with them.
    pub version: String,
}

/// Files made from a template of the example.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Generate {
    /// The template, a file of the example.
    pub template: String,
    /// Where each file goes, under the work directory; holds `${n}`.
    pub path: String,
    /// How many files: n runs from 1 to `count`.
    pub count: u32,
}

/// One step: a mark of `/metricz` before and after, and the commands between.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Its name: lowercase letters, digits and `-`.
    pub name: String,
    /// The environment of its commands, on top of the example's.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Its commands, in order; the first that fails ends the run.
    pub commands: Vec<Command>,
}

/// One command.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// The program and its arguments.
    pub argv: Vec<String>,
    /// A file under the work directory for its standard output; without it the output goes to
    /// the step's log.
    pub stdout: Option<String>,
}

/// One check: one line of the report.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// The first word of its line.
    pub name: String,
    /// A value of a JSON file.
    pub json: Option<JsonCheck>,
    /// The rise of a series of `/metricz` over a step.
    pub metric: Option<MetricCheck>,
    /// Whether two paths hold the same bytes.
    pub same: Option<SameCheck>,
    /// How many objects are listed under a prefix.
    pub objects: Option<ObjectsCheck>,
    /// The check stands for a call that fails for want of a tierstore feature.
    pub expected_failure: Option<ExpectedFailure>,
}

/// The value at `pointer` (RFC 6901) of the JSON file `file`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct JsonCheck {
    /// The file, under the work directory.
    pub file: String,
    /// The JSON pointer.
    pub pointer: String,
    /// The value where the pointer finds nothing; without it, that is an error.
    pub default: Option<String>,
}

/// The rise of `series` from the reading before the step `step` to the reading after it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MetricCheck {
    /// The series as `/metricz` writes it: the name and, if it has them, the labels in braces.
    pub series: String,
    /// The step.
    pub step: String,
}

/// Whether `a` and `b` (files or directory trees under the work directory) hold the same bytes.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SameCheck {
    /// One path.
    pub a: String,
    /// The other.
    pub b: String,
}

/// How many objects the bucket lists under `prefix`, after the closing commands.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ObjectsCheck {
    /// The prefix; empty for the whole bucket.
    pub prefix: String,
}

/// A check that stands for a missing tierstore feature.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExpectedFailure {
    /// The issue of the feature, `owner/repo#n`.
    pub issue: String,
    /// The value the check gives once the feature is there.
    pub passing: String,
}

/// The kind of a [`Check`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind<'a> {
    /// [`Check::json`].
    Json(&'a JsonCheck),
    /// [`Check::metric`].
    Metric(&'a MetricCheck),
    /// [`Check::same`].
    Same(&'a SameCheck),
    /// [`Check::objects`].
    Objects(&'a ObjectsCheck),
}

impl Check {
    /// The one kind the check has.
    ///
    /// # Errors
    /// The check has no kind, or more than one.
    pub fn kind(&self) -> Result<Kind<'_>, String> {
        let kinds: Vec<Kind<'_>> = [
            self.json.as_ref().map(Kind::Json),
            self.metric.as_ref().map(Kind::Metric),
            self.same.as_ref().map(Kind::Same),
            self.objects.as_ref().map(Kind::Objects),
        ]
        .into_iter()
        .flatten()
        .collect();
        match kinds.as_slice() {
            [one] => Ok(*one),
            _ => Err(format!(
                "check {}: give exactly one of json, metric, same and objects",
                self.name
            )),
        }
    }
}

impl Example {
    /// Parses and checks the text of an `example.toml`.
    ///
    /// # Errors
    /// The text is not TOML of this shape, or a value is out of range (see [`Example::check`]).
    pub fn parse(text: &str) -> Result<Self, String> {
        let example: Self = toml::from_str(text).map_err(|e| e.message().to_owned())?;
        example.check()?;
        Ok(example)
    }

    /// What is wrong with the spec, if anything: a step or check name that is empty, badly
    /// formed or used twice; a step with no command, or a command with no program; a path that is
    /// not relative and inside the work directory, or a `stdout` file named twice; a generator
    /// whose `path` has no `${n}` or whose `count` is 0; a check of no kind or of two, or a
    /// `metric` check of a step that does not exist.
    ///
    /// # Errors
    /// The first such problem.
    pub fn check(&self) -> Result<(), String> {
        let mut steps = BTreeSet::new();
        let mut outputs = BTreeSet::new();
        for generate in &self.generate {
            relative(&generate.template)?;
            relative(&generate.path)?;
            if !generate.path.contains("${n}") || generate.count == 0 {
                return Err(format!(
                    "generate {}: the path needs ${{n}} and the count at least 1",
                    generate.path
                ));
            }
        }
        for step in &self.step {
            let named = !step.name.is_empty()
                && step
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            if !named || !steps.insert(step.name.as_str()) {
                return Err(format!(
                    "step {:?}: a name is lowercase letters, digits and -, and is not used twice",
                    step.name
                ));
            }
            if step.commands.is_empty() {
                return Err(format!("step {}: it has no command", step.name));
            }
        }
        let commands = self.step.iter().flat_map(|s| &s.commands);
        for command in commands.chain(&self.cleanup) {
            if command.argv.is_empty() {
                return Err("a command has no program".to_owned());
            }
            if let Some(file) = &command.stdout {
                relative(file)?;
                if !outputs.insert(file.as_str()) {
                    return Err(format!("{file} is the stdout of two commands"));
                }
            }
        }
        let mut names = BTreeSet::new();
        for check in &self.check {
            if check.name.is_empty() || check.name.contains(char::is_whitespace) {
                return Err(format!("check {:?}: a name is one word", check.name));
            }
            if !names.insert(check.name.as_str()) {
                return Err(format!("check {}: the name is used twice", check.name));
            }
            match check.kind()? {
                Kind::Json(json) => relative(&json.file)?,
                Kind::Metric(metric) if !steps.contains(metric.step.as_str()) => {
                    return Err(format!(
                        "check {}: there is no step {}",
                        check.name, metric.step
                    ));
                }
                Kind::Same(same) => {
                    relative(&same.a)?;
                    relative(&same.b)?;
                }
                Kind::Metric(_) | Kind::Objects(_) => {}
            }
        }
        Ok(())
    }

    /// Every text of the spec that [`expand`] fills in: the arguments of every command and the
    /// values of every environment.
    #[must_use]
    pub fn expanded_texts(&self) -> Vec<&str> {
        let commands = self.step.iter().flat_map(|s| &s.commands);
        let argv = commands.chain(&self.cleanup).flat_map(|c| &c.argv);
        let env = self
            .env
            .values()
            .chain(self.step.iter().flat_map(|s| s.env.values()));
        argv.chain(env).map(String::as_str).collect()
    }
}

/// Refuses a path that is empty, absolute, or that has a `.` or `..` part.
fn relative(path: &str) -> Result<(), String> {
    let mut parts = Path::new(path).components();
    let plain = parts.all(|c| matches!(c, Component::Normal(_)));
    if path.is_empty() || !plain {
        return Err(format!(
            "{path:?} is not a relative path inside the directory"
        ));
    }
    Ok(())
}

/// `text` with each `${name}` replaced by its value in `vars` and each `$$` by `$`; a `$` before
/// anything else stays as it is.
///
/// # Errors
/// A name that `vars` does not hold, or a `${` with no `}` after it.
pub fn expand(text: &str, vars: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
        } else if let Some(tail) = after.strip_prefix('{') {
            let end = tail
                .find('}')
                .ok_or_else(|| "a ${ has no } after it".to_owned())?;
            let name = &tail[..end];
            let value = vars
                .get(name)
                .ok_or_else(|| format!("${{{name}}} is not a value of the run"))?;
            out.push_str(value);
            rest = &tail[end + 1..];
        } else {
            out.push('$');
            rest = after;
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The names `NAME` of every `${port:NAME}` in `texts`, each once, in order.
#[must_use]
pub fn port_names(texts: &[&str]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for text in texts {
        let mut rest = *text;
        while let Some(at) = rest.find("${port:") {
            let tail = &rest[at + "${port:".len()..];
            let Some(end) = tail.find('}') else { break };
            names.insert(tail[..end].to_owned());
            rest = &tail[end..];
        }
    }
    names
}
