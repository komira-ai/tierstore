use std::path::PathBuf;

use super::*;

fn cov(lf: u64, lh: u64, brf: u64, brh: u64) -> CrateCoverage {
    CrateCoverage {
        lines: Counts::new(lf, lh).expect("valid"),
        branches: Counts::new(brf, brh).expect("valid"),
    }
}

fn record(path: &str, coverage: CrateCoverage) -> FileRecord {
    FileRecord {
        path: PathBuf::from(path),
        coverage,
    }
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

fn row(measured: Option<CrateCoverage>, excluded: bool) -> Row {
    Row {
        krate: "a".to_owned(),
        measured,
        excluded,
    }
}

/// Catches: a record attributed to the wrong crate, a crate with no record missing from the
/// result (it would then never be judged), and two files of one crate not summed.
#[test]
fn records_are_summed_per_crate_and_every_crate_is_listed() {
    let root = Path::new("/w");
    let files = [
        record("/w/crates/a/src/lib.rs", cov(10, 9, 2, 1)),
        record("/w/crates/a/tests/t.rs", cov(5, 5, 0, 0)),
        record("/w/crates/b/src/lib.rs", cov(4, 4, 1, 1)),
    ];
    let got = by_crate(root, &names(&["a", "b", "c"]), &files).expect("attributed");
    assert_eq!(got["a"], cov(15, 14, 2, 1));
    assert_eq!(got["b"], cov(4, 4, 1, 1));
    assert_eq!(got["c"], CrateCoverage::default());
}

/// Catches: a source outside every crate dropped (its lines would escape the gate), a file
/// directly under `crates/`, and a path under a directory that is not a workspace crate.
#[test]
fn a_record_outside_every_crate_is_an_error() {
    let root = Path::new("/w");
    for path in [
        "/elsewhere/src/lib.rs",
        "/w/crates/lib.rs",
        "/w/crates/ghost/src/lib.rs",
        "/w/src/lib.rs",
    ] {
        let files = [record(path, cov(1, 1, 0, 0))];
        assert_eq!(
            by_crate(root, &names(&["a"]), &files),
            Err(Unattributed(PathBuf::from(path))),
            "{path}"
        );
    }
}

/// Catches: the threshold applied to the wrong metric, `>` for `>=`, and a crate with no
/// branches failing for want of them.
#[test]
fn a_crate_passes_at_99_percent_of_each_metric_and_fails_under_it() {
    let at = row(Some(cov(100, 99, 100, 99)), false);
    assert_eq!(at.status(), Status::Ok);
    let no_branches = row(Some(cov(100, 100, 0, 0)), false);
    assert_eq!(no_branches.status(), Status::Ok);
    let lines = row(Some(cov(101, 99, 100, 100)), false);
    assert_eq!(lines.status(), Status::Below(vec![Metric::Lines]));
    let branches = row(Some(cov(100, 100, 101, 99)), false);
    assert_eq!(branches.status(), Status::Below(vec![Metric::Branches]));
    let both = row(Some(cov(100, 50, 100, 50)), false);
    assert_eq!(
        both.status(),
        Status::Below(vec![Metric::Lines, Metric::Branches])
    );
}

/// Catches: a crate whose tests stopped being instrumented passing with zero lines.
#[test]
fn a_crate_with_no_instrumented_line_fails() {
    let nothing = row(Some(CrateCoverage::default()), false);
    assert_eq!(nothing.status(), Status::NoData);
    assert!(nothing.status().fails());
}

/// Catches: an excluded crate judged, an exclusion of a missing crate passing, and an
/// excluded crate's failing status leaking into the verdict.
#[test]
fn an_excluded_crate_is_listed_not_judged_and_a_stale_exclusion_fails() {
    let excluded = row(Some(cov(100, 1, 100, 1)), true);
    assert_eq!(excluded.status(), Status::Excluded);
    assert!(!excluded.status().fails());
    let stale = row(None, true);
    assert_eq!(stale.status(), Status::Stale);
    assert!(stale.status().fails());
}

/// Catches: rows missing a workspace crate or an exclusion, or out of name order.
#[test]
fn rows_cover_the_workspace_and_the_exclusions_in_name_order() {
    let measured: BTreeMap<String, CrateCoverage> = [
        ("b".to_owned(), cov(1, 1, 0, 0)),
        ("a".to_owned(), cov(2, 2, 0, 0)),
    ]
    .into();
    let got = rows(&measured, &names(&["b", "z"]));
    let summary: Vec<(&str, bool, bool)> = got
        .iter()
        .map(|r| (r.krate.as_str(), r.measured.is_some(), r.excluded))
        .collect();
    assert_eq!(
        summary,
        [("a", true, false), ("b", true, true), ("z", false, true)]
    );
}

/// Catches: a table that drops the status, the workspace total or the verdict line.
#[test]
fn the_table_has_a_row_per_crate_a_total_and_a_verdict() {
    let measured: BTreeMap<String, CrateCoverage> = [
        ("a".to_owned(), cov(100, 100, 10, 10)),
        ("b".to_owned(), cov(100, 90, 0, 0)),
    ]
    .into();
    let report = render(&rows(&measured, &BTreeSet::new()));
    assert!(
        report.contains("| a | 100/100 | 100.00 | 10/10 | 100.00 | ok |"),
        "{report}"
    );
    assert!(
        report.contains("| b | 90/100 | 90.00 | - | - | BELOW THRESHOLD (lines) |"),
        "{report}"
    );
    assert!(
        report.contains("| **workspace** | 190/200 | 95.00 | 10/10 | 100.00 |  |"),
        "{report}"
    );
    assert!(
        report.ends_with("coverage gate: FAIL (1 crate(s) need attention; see status)\n"),
        "{report}"
    );
    let passing: BTreeMap<String, CrateCoverage> = [("a".to_owned(), cov(1, 1, 0, 0))].into();
    let report = render(&rows(&passing, &BTreeSet::new()));
    assert!(
        report.ends_with("coverage gate: PASS (threshold 99.00 of lines and branches)\n"),
        "{report}"
    );
}

/// Catches: the crate list taking files or directories without a manifest for crates.
#[test]
fn workspace_crates_are_the_directories_with_a_manifest() {
    let beside = std::env::current_exe().expect("test binary");
    let root = beside
        .parent()
        .expect("deps")
        .join("coverage-gate-unit-crates");
    let _ = std::fs::remove_dir_all(&root);
    for dir in ["a", "b", "no-manifest"] {
        std::fs::create_dir_all(root.join("crates").join(dir)).expect("dir");
    }
    std::fs::write(root.join("crates/a/Cargo.toml"), b"").expect("manifest");
    std::fs::write(root.join("crates/b/Cargo.toml"), b"").expect("manifest");
    std::fs::write(root.join("crates/file"), b"").expect("file");
    assert_eq!(workspace_crates(&root).expect("list"), names(&["a", "b"]));
    assert!(workspace_crates(&root.join("absent")).is_err());
    let _ = std::fs::remove_dir_all(&root);
}
