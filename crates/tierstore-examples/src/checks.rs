//! The checks of an example, each evaluated to one line of the report: `name value`, and for a
//! check that stands for a missing feature, `name value (expected failure: owner/repo#n)`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::error::Error;
use crate::metrics::{Reading, rise};
use crate::spec::{Check, JsonCheck, Kind, SameCheck};

/// The readings of `/metricz` before and after each step, by step name.
pub type Marks = BTreeMap<String, (Reading, Reading)>;

/// The lines of the checks, and the checks that stand for a missing feature and pass now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evaluated {
    /// One line for each check, in order, each ended by a newline.
    pub lines: String,
    /// For each check with `expected_failure` whose value is its `passing`, why the run fails.
    pub now_passing: Vec<String>,
}

/// Evaluates `checks` over the files under `work`, the readings `marks`, and `objects`, which
/// counts the objects listed under a prefix.
///
/// # Errors
/// A check cannot be evaluated: its file cannot be read or is not JSON, its pointer finds
/// nothing and it has no default, or the listing fails.
pub fn evaluate(
    checks: &[Check],
    work: &Path,
    marks: &Marks,
    objects: &mut dyn FnMut(&str) -> Result<u64, String>,
) -> Result<Evaluated, Error> {
    let mut evaluated = Evaluated::default();
    for check in checks {
        let fail = |why: String| Error::Check {
            check: check.name.clone(),
            why,
        };
        let value = match check.kind().map_err(fail)? {
            Kind::Json(json) => json_value(work, json).map_err(fail)?,
            Kind::Metric(metric) => {
                let empty = (Reading::new(), Reading::new());
                let (before, after) = marks.get(&metric.step).unwrap_or(&empty);
                rise(before, after, &metric.series)
            }
            Kind::Same(same) => same_bytes(work, same).map_err(fail)?,
            Kind::Objects(listed) => objects(&listed.prefix).map_err(fail)?.to_string(),
        };
        let line = match &check.expected_failure {
            Some(expected) => {
                if value == expected.passing {
                    evaluated.now_passing.push(format!(
                        "{} is {value}, which it is once {} is done: make it a check like the others",
                        check.name, expected.issue
                    ));
                }
                format!(
                    "{} {value} (expected failure: {})\n",
                    check.name, expected.issue
                )
            }
            None => format!("{} {value}\n", check.name),
        };
        evaluated.lines.push_str(&line);
    }
    Ok(evaluated)
}

/// The value at the check's pointer, as text: a string as it is, a number, `true`, `false` or
/// `null` as JSON writes it.
fn json_value(work: &Path, check: &JsonCheck) -> Result<String, String> {
    let path = work.join(&check.file);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    match value.pointer(&check.pointer) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(Value::Array(_) | Value::Object(_)) => Err(format!(
            "{} holds a list or a table at {}, not one value",
            check.file, check.pointer
        )),
        Some(scalar) => Ok(scalar.to_string()),
        None => check
            .default
            .clone()
            .ok_or_else(|| format!("{} holds nothing at {}", check.file, check.pointer)),
    }
}

/// `same` when the two paths hold the same files with the same bytes; else the first file found
/// in one of them only, or the first file that differs.
fn same_bytes(work: &Path, check: &SameCheck) -> Result<String, String> {
    let (a, b) = (files(&work.join(&check.a))?, files(&work.join(&check.b))?);
    let only_a = a.keys().find(|name| !b.contains_key(*name));
    let only_b = b.keys().find(|name| !a.contains_key(*name));
    let differs = a
        .iter()
        .find(|(name, bytes)| b.get(*name).is_some_and(|other| other != *bytes));
    // A file compared as itself has the empty name: joined, it would gain a trailing `/`.
    let shown = |root: &str, name: &Path| {
        let parts = Path::new(root).components().chain(name.components());
        parts.collect::<PathBuf>().display().to_string()
    };
    Ok(match (only_a, only_b, differs) {
        (Some(name), _, _) => format!("only {}", shown(&check.a, name)),
        (None, Some(name), _) => format!("only {}", shown(&check.b, name)),
        (None, None, Some((name, _))) => format!("differ at {}", shown(&check.a, name)),
        (None, None, None) => "same".to_owned(),
    })
}

/// The files under `root` (or `root` itself, if it is a file) by their path below it, with
/// their bytes.
fn files(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
    let fail = |path: &Path, e: std::io::Error| format!("{}: {e}", path.display());
    let mut found = BTreeMap::new();
    let mut todo = vec![(root.to_path_buf(), PathBuf::new())];
    while let Some((path, name)) = todo.pop() {
        let meta = fs::metadata(&path).map_err(|e| fail(&path, e))?;
        if meta.is_dir() {
            for entry in fs::read_dir(&path).map_err(|e| fail(&path, e))? {
                let entry = entry.map_err(|e| fail(&path, e))?;
                todo.push((entry.path(), name.join(entry.file_name())));
            }
        } else {
            let bytes = fs::read(&path).map_err(|e| fail(&path, e))?;
            found.insert(name, bytes);
        }
    }
    Ok(found)
}
