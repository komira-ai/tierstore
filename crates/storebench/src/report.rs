//! The tables of the measurement document, from the summaries committed beside the crate.
//!
//! The document `docs/measurements/2026-11-storage-tier-t0-f-a.md` holds, for each arm, the
//! tables [`render`] makes of that arm's summary in `crates/storebench/data/`, word for word
//! (a test holds it to them), so no figure of the document is typed by hand.

use std::path::Path;

use clap::ValueEnum;
use serde::de::DeserializeOwned;

use crate::Error;
use crate::table::Table;

/// An arm of the measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Arm {
    /// Silo's objects: `c6.json`.
    C6,
    /// Placement: `c7.json`.
    C7,
    /// The replay of garbage collection: `c10.json`.
    C10,
}

/// Reads the JSON file at `path` as a `T`.
///
/// # Errors
/// [`Error::Io`] when it cannot be read; [`Error::Parse`] when it is not a `T`.
pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io("read", path, e))?;
    serde_json::from_str(&text).map_err(|e| Error::parse(path, e.line(), e.to_string()))
}

/// The Markdown of `tables`, each followed by a blank line.
#[must_use]
pub fn markdown(tables: &[Table]) -> String {
    tables
        .iter()
        .map(|t| format!("{}\n", t.markdown()))
        .collect()
}

/// The tables of `arm`, from the summaries in `data`.
///
/// # Errors
/// [`Error::Io`] and [`Error::Parse`] when a summary cannot be read.
pub fn render(data: &Path, arm: Arm) -> Result<String, Error> {
    let tables = match arm {
        Arm::C6 => crate::c6::tables(&read_json(&data.join("c6.json"))?),
        Arm::C7 => crate::placement::tables(&read_json(&data.join("c7.json"))?),
        Arm::C10 => crate::gc::tables(&read_json(&data.join("c10.json"))?),
    };
    Ok(markdown(&tables))
}

/// The arms whose tables, as [`render`] makes them from `data`, are not in `document` word for
/// word: empty when the document holds every arm's tables.
///
/// # Errors
/// As [`render`].
pub fn missing_arms(document: &str, data: &Path) -> Result<Vec<Arm>, Error> {
    let mut missing = Vec::new();
    for arm in [Arm::C6, Arm::C7, Arm::C10] {
        if !document.contains(&render(data, arm)?) {
            missing.push(arm);
        }
    }
    Ok(missing)
}

#[cfg(test)]
mod tests {
    use tierstore_testkit::dir::TestDir;

    use super::*;

    /// A summary that is not there, or not the arm's JSON, is an error naming the file.
    #[test]
    fn a_missing_or_wrong_summary_is_an_error() {
        let dir = TestDir::under(std::env::temp_dir(), "storebench-report-missing");
        for arm in [Arm::C6, Arm::C7, Arm::C10] {
            assert!(matches!(
                render(dir.path(), arm),
                Err(Error::Io { what: "read", .. })
            ));
        }
        std::fs::write(dir.path().join("c6.json"), b"{\"not\": 1}").unwrap();
        assert!(matches!(
            render(dir.path(), Arm::C6),
            Err(Error::Parse { .. })
        ));
    }

    /// The Markdown of tables is each table and a blank line.
    #[test]
    fn markdown_separates_tables_with_a_blank_line() {
        let mut t = Table::new("T", &["a"]);
        t.row(vec!["1".into()]);
        assert_eq!(
            markdown(&[t.clone(), t]),
            "**T**\n\n| a |\n| --- |\n| 1 |\n\n**T**\n\n| a |\n| --- |\n| 1 |\n\n"
        );
    }
}
