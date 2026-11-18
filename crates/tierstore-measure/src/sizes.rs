//! Arm C-6's object-size distribution: silo's logical bytes, by size class and by top-level
//! prefix, from a listing (`key<TAB>size` lines). It checks the cold tier's capacity at two
//! copies and sets the small-object policy (how many objects, and how many bytes, are small).

use std::collections::BTreeMap;

use serde::Serialize;

use crate::report::{Table, num};
use crate::stats::quantile;

/// The upper bounds of the size classes, in bytes (the last class is everything above).
pub const BOUNDS: [u64; 9] = [
    4 << 10,
    64 << 10,
    1 << 20,
    4 << 20,
    16 << 20,
    64 << 20,
    256 << 20,
    1 << 30,
    4 << 30,
];

/// Counts and bytes, by class and by prefix.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Sizes {
    /// Objects.
    pub count: u64,
    /// Their bytes.
    pub bytes: u64,
    /// Objects and bytes per class, by upper bound (`u64::MAX` for the last).
    pub classes: BTreeMap<u64, (u64, u64)>,
    /// Objects and bytes per first path segment.
    pub prefixes: BTreeMap<String, (u64, u64)>,
    /// Median, 90th and 99th percentile size.
    pub p50: u64,
    /// The 90th percentile.
    pub p90: u64,
    /// The 99th percentile.
    pub p99: u64,
}

/// The class of a size: the first bound at or above it.
#[must_use]
pub fn class(size: u64) -> u64 {
    BOUNDS
        .iter()
        .copied()
        .find(|&b| size <= b)
        .unwrap_or(u64::MAX)
}

/// The distribution of `(key, size)` pairs.
#[must_use]
pub fn analyse<'a>(objects: impl IntoIterator<Item = (&'a str, u64)>) -> Sizes {
    let mut s = Sizes::default();
    let mut all = Vec::new();
    for (key, size) in objects {
        s.count += 1;
        s.bytes += size;
        let c = s.classes.entry(class(size)).or_default();
        c.0 += 1;
        c.1 += size;
        let top = key.split('/').next().unwrap_or("").to_string();
        let p = s.prefixes.entry(top).or_default();
        p.0 += 1;
        p.1 += size;
        all.push(size as f64);
    }
    if !all.is_empty() {
        all.sort_by(f64::total_cmp);
        let q = |x| quantile(&all, x) as u64;
        s.p50 = q(0.5);
        s.p90 = q(0.9);
        s.p99 = q(0.99);
    }
    s
}

/// Parses `key<TAB>size` lines; a line without a size is skipped.
#[must_use]
pub fn parse(text: &str) -> Vec<(&str, u64)> {
    text.lines()
        .filter_map(|l| {
            let (k, n) = l.rsplit_once('\t')?;
            Some((k, n.trim().parse().ok()?))
        })
        .collect()
}

fn human(b: u64) -> String {
    match b {
        u64::MAX => "larger".into(),
        b if b >= 1 << 30 => format!("{} GiB", b >> 30),
        b if b >= 1 << 20 => format!("{} MiB", b >> 20),
        b => format!("{} KiB", b >> 10),
    }
}

/// The tables: totals, classes, prefixes.
#[must_use]
pub fn tables(title: &str, s: &Sizes) -> Vec<Table> {
    let gib = |b: u64| b as f64 / f64::from(1u32 << 30);
    let mut t = Table::new(
        &format!("C-6, {title}: totals"),
        &["objects", "logical GiB", "p50 size", "p90 size", "p99 size"],
    );
    t.row(vec![
        s.count.to_string(),
        num(gib(s.bytes), 1),
        human(s.p50),
        human(s.p90),
        human(s.p99),
    ]);
    let pct = |a: u64, b: u64| {
        if b == 0 {
            0.0
        } else {
            a as f64 * 100.0 / b as f64
        }
    };
    let mut c = Table::new(
        &format!("C-6, {title}: by size"),
        &["up to", "objects", "% objects", "GiB", "% bytes"],
    );
    for (bound, (n, b)) in &s.classes {
        c.row(vec![
            human(*bound),
            n.to_string(),
            num(pct(*n, s.count), 1),
            num(gib(*b), 1),
            num(pct(*b, s.bytes), 1),
        ]);
    }
    let mut p = Table::new(
        &format!("C-6, {title}: by prefix"),
        &["prefix", "objects", "GiB"],
    );
    for (name, (n, b)) in &s.prefixes {
        p.row(vec![name.clone(), n.to_string(), num(gib(*b), 1)]);
    }
    vec![t, c, p]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_bound_from_above() {
        assert_eq!(class(0), 4 << 10);
        assert_eq!(class(4 << 10), 4 << 10);
        assert_eq!(class((4 << 10) + 1), 64 << 10);
        assert_eq!(class(5 << 30), u64::MAX);
    }

    #[test]
    fn analyses_counts_bytes_prefixes_and_percentiles() {
        let text = "cas/a\t100\ncas/b\t2097152\nbench/c\t5368709120\nnosize\nbad\tx\n";
        let objs = parse(text);
        assert_eq!(objs.len(), 3);
        let s = analyse(objs);
        assert_eq!(s.count, 3);
        assert_eq!(s.bytes, 100 + 2_097_152 + 5_368_709_120);
        assert_eq!(s.classes[&(4 << 10)], (1, 100));
        assert_eq!(s.classes[&(4 << 20)], (1, 2_097_152));
        assert_eq!(s.classes[&u64::MAX].0, 1);
        assert_eq!(s.prefixes["cas"], (2, 2_097_252));
        assert_eq!(
            (s.p50, s.p90, s.p99),
            (2_097_152, 5_368_709_120, 5_368_709_120)
        );
        let t = tables("kbf-cas", &s);
        assert_eq!(t.len(), 3);
        assert!(
            t[0].markdown()
                .contains("| 3 | 5.0 | 2 MiB | 5 GiB | 5 GiB |")
        );
        assert!(t[1].markdown().contains("| larger | 1 | 33.3 |"));
        assert!(t[1].markdown().contains("| 4 KiB | 1 | 33.3 | 0.0 | 0.0 |"));
        assert!(t[2].markdown().contains("| bench | 1 | 5.0 |"));
    }

    #[test]
    fn an_empty_listing_has_zeros() {
        let s = analyse(Vec::new());
        assert_eq!(s, Sizes::default());
        assert!(
            tables("x", &s)[0]
                .markdown()
                .contains("| 0 | 0.0 | 0 KiB | 0 KiB | 0 KiB |")
        );
        assert!(tables("x", &s)[1].rows().is_empty());
        let zero = analyse(vec![("a", 0)]);
        assert!(
            tables("x", &zero)[1]
                .markdown()
                .contains("| 4 KiB | 1 | 100.0 | 0.0 | 0.0 |")
        );
    }
}
