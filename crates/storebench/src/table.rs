//! A Markdown table, as the measurement document prints it, and the number formats its cells use.

/// A table with a title line, a header and rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    title: String,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Table {
    /// An empty table titled `title` with the columns `columns`.
    #[must_use]
    pub fn new(title: impl Into<String>, columns: &[&str]) -> Self {
        Self {
            title: title.into(),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            rows: Vec::new(),
        }
    }

    /// Adds a row; a row with fewer cells than columns is padded with empty cells.
    pub fn row(&mut self, mut cells: Vec<String>) {
        cells.resize(self.columns.len().max(cells.len()), String::new());
        self.rows.push(cells);
    }

    /// The rows added so far.
    #[must_use]
    pub fn rows(&self) -> &[Vec<String>] {
        &self.rows
    }

    /// The table as Markdown: `**title**`, a blank line, the header, the rule and the rows.
    #[must_use]
    pub fn markdown(&self) -> String {
        let line = |cells: &[String]| format!("| {} |\n", cells.join(" | "));
        let mut out = format!("**{}**\n\n", self.title);
        out.push_str(&line(&self.columns));
        out.push_str(&line(&vec!["---".to_owned(); self.columns.len()]));
        for row in &self.rows {
            out.push_str(&line(row));
        }
        out
    }
}

/// `x` with `places` decimals.
#[must_use]
pub fn num(x: f64, places: usize) -> String {
    format!("{x:.places$}")
}

/// `part / whole` as a percentage with one decimal (`0.0` when `whole` is 0).
#[must_use]
pub fn pct(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0.0".to_owned();
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a percentage printed with one decimal"
    )]
    let p = part as f64 * 100.0 / whole as f64;
    num(p, 1)
}

/// `bytes` in decimal units with three significant figures (`1.34 TB`, `85.9 GB`, `512 B`).
#[must_use]
pub fn bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "kB", "MB", "GB", "TB", "PB"];
    #[expect(clippy::cast_precision_loss, reason = "printed with three figures")]
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1000.0 && unit + 1 < UNITS.len() {
        v /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        return format!("{bytes} B");
    }
    let places = if v >= 100.0 {
        0
    } else if v >= 10.0 {
        1
    } else {
        2
    };
    format!("{} {}", num(v, places), UNITS[unit])
}

/// `n` with a comma every three digits (`1,390,214`).
#[must_use]
pub fn count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Markdown of a table: title, header, rule and rows, a short row padded.
    #[test]
    fn a_table_prints_as_markdown() {
        let mut t = Table::new("C-6, sizes", &["kind", "objects", "bytes"]);
        t.row(vec!["pack".into(), "3".into(), "4 MB".into()]);
        t.row(vec!["other".into()]);
        assert_eq!(t.rows().len(), 2);
        assert_eq!(
            t.markdown(),
            "**C-6, sizes**\n\n| kind | objects | bytes |\n| --- | --- | --- |\n\
             | pack | 3 | 4 MB |\n| other |  |  |\n"
        );
    }

    /// Numbers, percentages, byte counts and counts as the tables print them.
    #[test]
    fn numbers_print_as_the_tables_need() {
        assert_eq!(num(1.23456, 2), "1.23");
        assert_eq!(pct(1, 3), "33.3");
        assert_eq!(pct(1, 0), "0.0");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1_340_000_000_000), "1.34 TB");
        assert_eq!(bytes(85_900_000_000), "85.9 GB");
        assert_eq!(bytes(999_499), "999 kB");
        assert_eq!(bytes(u64::MAX), "18447 PB");
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_390_214), "1,390,214");
    }
}
