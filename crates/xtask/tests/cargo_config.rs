//! The cargo configuration of the repository (`.cargo/config.toml`) tries a git or registry
//! fetch again enough times to outlast a short refusal by GitHub (HTTP 403 for one runner while
//! others fetch the same commit), which with cargo's default of 3 tries ended a release job.
//!
//! Catches:
//! - `[net] retry` removed, or lowered below `MIN_RETRY`;
//! - `retry` given as something other than an integer, or `[net]` not a table;
//! - a check that cannot see a defect: the same check, run on each of those, names it.

use std::path::PathBuf;

/// The fewest tries again `[net] retry` may ask for.
const MIN_RETRY: i64 = 10;

/// What is wrong with the `[net] retry` of the cargo configuration `text`, if anything.
fn problem(text: &str) -> Option<String> {
    let config: toml::Table = match text.parse() {
        Ok(config) => config,
        Err(e) => return Some(format!("does not parse: {e}")),
    };
    let Some(net) = config.get("net") else {
        return Some("has no [net] table".to_owned());
    };
    let Some(net) = net.as_table() else {
        return Some("has a `net` that is not a table".to_owned());
    };
    match net.get("retry").map(toml::Value::as_integer) {
        None => Some("[net] has no `retry`".to_owned()),
        Some(None) => Some("[net] `retry` is not an integer".to_owned()),
        Some(Some(n)) if n < MIN_RETRY => {
            Some(format!("[net] `retry` is {n}, fewer than {MIN_RETRY}"))
        }
        Some(Some(_)) => None,
    }
}

#[test]
fn the_repository_tries_a_fetch_again_at_least_ten_times() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.cargo/config.toml");
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(problem(&text), None, "{}", path.display());
}

/// Asserts that 10 and more are accepted, and that each defect is named: no `[net]`, `net`
/// not a table, no `retry`, `retry` not an integer, `retry` of 9 and of cargo's default 3, and
/// a file that does not parse.
#[test]
fn a_missing_or_low_retry_is_named() {
    assert_eq!(problem("[net]\nretry = 10\n"), None);
    assert_eq!(problem("[net]\nretry = 11\n"), None);
    assert_eq!(
        problem("[alias]\nx = \"y\"\n"),
        Some("has no [net] table".to_owned())
    );
    assert_eq!(
        problem("net = 10\n"),
        Some("has a `net` that is not a table".to_owned())
    );
    assert_eq!(
        problem("[net]\ngit-fetch-with-cli = true\n"),
        Some("[net] has no `retry`".to_owned())
    );
    assert_eq!(
        problem("[net]\nretry = \"10\"\n"),
        Some("[net] `retry` is not an integer".to_owned())
    );
    assert_eq!(
        problem("[net]\nretry = 9\n"),
        Some("[net] `retry` is 9, fewer than 10".to_owned())
    );
    assert_eq!(
        problem("[net]\nretry = 3\n"),
        Some("[net] `retry` is 3, fewer than 10".to_owned())
    );
    assert!(problem("[net\n").is_some_and(|p| p.starts_with("does not parse: ")));
}
