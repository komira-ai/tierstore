//! What an arm writes: one JSON line per cell (`<arm>.jsonl`) and a Markdown table
//! (`<arm>.md`) in its output directory.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::Error;

/// A Markdown table built row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    title: String,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// An empty table with a title and column names.
    #[must_use]
    pub fn new(title: &str, columns: &[&str]) -> Self {
        Self {
            title: title.to_string(),
            columns: columns.iter().map(|c| (*c).to_string()).collect(),
            rows: Vec::new(),
        }
    }

    /// Adds a row; a short row is padded with empty cells, a long one cut.
    pub fn row(&mut self, cells: Vec<String>) {
        let mut cells = cells;
        cells.resize(self.columns.len(), String::new());
        self.rows.push(cells);
    }

    /// Its rows.
    #[must_use]
    pub fn rows(&self) -> &[Vec<String>] {
        &self.rows
    }

    /// The Markdown text: a `###` title, the header, and the rows.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut s = format!("### {}\n\n| {} |\n|", self.title, self.columns.join(" | "));
        for _ in &self.columns {
            s.push_str("---|");
        }
        s.push('\n');
        for r in &self.rows {
            let _ = writeln!(s, "| {} |", r.join(" | "));
        }
        s
    }
}

/// Formats a number with `places` decimals.
#[must_use]
pub fn num(x: f64, places: usize) -> String {
    format!("{x:.places$}")
}

/// An arm's output directory.
#[derive(Debug, Clone)]
pub struct Out {
    dir: PathBuf,
    arm: String,
}

impl Out {
    /// The outputs of `arm` under `dir` (made if missing).
    ///
    /// # Errors
    /// When the directory cannot be made.
    pub fn new(dir: &Path, arm: &str) -> Result<Self, Error> {
        std::fs::create_dir_all(dir).map_err(Error::io("mkdir", dir))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            arm: arm.to_string(),
        })
    }

    /// The path of the arm's file with `ext`.
    #[must_use]
    pub fn path(&self, ext: &str) -> PathBuf {
        self.dir.join(format!("{}.{ext}", self.arm))
    }

    /// Appends `row` as one JSON line to `<arm>.jsonl`.
    ///
    /// # Errors
    /// When it cannot be serialised or written.
    pub fn record(&self, row: &impl Serialize) -> Result<(), Error> {
        let line = serde_json::to_string(row)?;
        let p = self.path("jsonl");
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .map_err(Error::io("open", &p))?;
        writeln!(f, "{line}").map_err(Error::io("write", &p))
    }

    /// Writes the tables, one after another, to `<arm>.md`.
    ///
    /// # Errors
    /// When it cannot be written.
    pub fn tables(&self, tables: &[Table]) -> Result<(), Error> {
        let text: Vec<String> = tables.iter().map(Table::markdown).collect();
        let p = self.path("md");
        std::fs::write(&p, text.join("\n")).map_err(Error::io("write", &p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tierstore_testkit::dir::TestDir;

    #[test]
    fn markdown_pads_short_rows_and_cuts_long_ones() {
        let mut t = Table::new("A raw HDD", &["size", "MB/s"]);
        t.row(vec!["1".into()]);
        t.row(vec!["2".into(), "150.0".into(), "extra".into()]);
        assert_eq!(t.rows().len(), 2);
        assert_eq!(
            t.markdown(),
            "### A raw HDD\n\n| size | MB/s |\n|---|---|\n| 1 |  |\n| 2 | 150.0 |\n"
        );
        assert_eq!(num(1.0 / 3.0, 2), "0.33");
    }

    #[test]
    fn records_lines_and_tables_and_reports_failures() {
        let d = TestDir::under(std::env::temp_dir(), "report");
        let out = Out::new(&d.path().join("o"), "a").unwrap();
        out.record(&serde_json::json!({"x": 1})).unwrap();
        out.record(&serde_json::json!({"x": 2})).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path("jsonl")).unwrap(),
            "{\"x\":1}\n{\"x\":2}\n"
        );
        out.tables(&[Table::new("t", &["c"]), Table::new("u", &["d"])])
            .unwrap();
        assert!(
            std::fs::read_to_string(out.path("md"))
                .unwrap()
                .contains("### u")
        );
        let file = d.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert!(Out::new(&file.join("sub"), "a").is_err());
        let gone = Out {
            dir: file.join("sub"),
            arm: "a".into(),
        };
        assert!(gone.record(&1).is_err());
        assert!(gone.tables(&[]).is_err());
    }
}
