//! Every metric of section 12 is registered and labelled without pack or key (section 18, row
//! T9).
//!
//! The table of section 12 of `docs/design/fetch-cache-serve.md` is read at test time: each
//! backquoted name of its first column, the labels in braces after a name and the labels of a
//! "by ..." phrase, and the kinds of its second column. The registry is held to it both ways:
//! every name of the table is a registered family (by its design name) carrying the labels the
//! table gives it, of a kind its row names, and every registered family is a name of the table.
//!
//! Catches: a family of section 12 missing from the registry, a registered family the design
//! does not have, a label the design names missing from its family, a family of the wrong kind,
//! a label naming a pack, a key or another unbounded value, two families exported under one name.

use std::collections::{BTreeMap, BTreeSet};

use tierstore_format::descriptor::Code;
use tierstore_format::ids::{DriveId, NodeId};
use tierstore_metrics::{
    ColdStoreConfig, FamilyInfo, FetchEngineConfig, HotStoreConfig, Kind, Metrics,
};

const DESIGN: &str = include_str!("../../../docs/design/fetch-cache-serve.md");

/// One name of the table: its labels (from braces or a "by" phrase) and its row's kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    labels: BTreeSet<String>,
    kinds: BTreeSet<String>,
}

/// The table rows of the section whose heading starts with `## 12.`, up to the next heading.
fn section_12_rows(doc: &str) -> Vec<&str> {
    doc.lines()
        .skip_while(|l| !l.starts_with("## 12."))
        .skip(1)
        .take_while(|l| !l.starts_with("## "))
        .filter(|l| l.starts_with("| ") && l.contains('`'))
        .collect()
}

/// The names of a table, each with its labels and its row's kinds.
fn parse_table(doc: &str) -> BTreeMap<String, Row> {
    let mut names = BTreeMap::new();
    for line in section_12_rows(doc) {
        let cells: Vec<&str> = line.split(" | ").collect();
        let first = cells[0].trim_start_matches("| ");
        let kinds: BTreeSet<String> = cells[1].split(", ").map(str::to_owned).collect();
        // Odd pieces are inside backquotes: a name, perhaps with `{labels}`, unless it is inside
        // parentheses (a note on the name before it). Even pieces are the text between them,
        // where a "by ..." phrase labels the name just before it, or every name of the run
        // before it when the phrase ends the cell ("`a`, `b` by class and size class").
        let pieces: Vec<&str> = first.split('`').collect();
        let mut pending: Vec<String> = Vec::new();
        let mut depth = 0_i32;
        for (i, piece) in pieces.iter().enumerate() {
            if i % 2 == 1 {
                if depth > 0 {
                    continue;
                }
                let (name, labels) = match piece.split_once('{') {
                    Some((name, rest)) => (
                        name,
                        rest.trim_end_matches('}')
                            .split(',')
                            .map(str::to_owned)
                            .collect(),
                    ),
                    None => (*piece, BTreeSet::new()),
                };
                // A name in two rows takes the labels and kinds of both.
                let row = names.entry(name.to_owned()).or_insert_with(|| Row {
                    labels: BTreeSet::new(),
                    kinds: BTreeSet::new(),
                });
                row.labels.extend(labels);
                row.kinds.extend(kinds.iter().cloned());
                pending.push(name.to_owned());
                continue;
            }
            depth += piece.matches('(').count() as i32 - piece.matches(')').count() as i32;
            if let Some(by) = piece.trim_start().strip_prefix("by ") {
                let (phrase, rest) = by.split_once(',').unwrap_or((by, ""));
                let ends_cell = rest.trim().is_empty() && i == pieces.len() - 1;
                let labelled = if ends_cell {
                    pending.clone()
                } else {
                    pending.last().cloned().into_iter().collect()
                };
                for label in phrase.trim().split(" and ") {
                    let label = label.trim().replace(' ', "_");
                    let rows = names.iter_mut().filter(|(n, _)| labelled.contains(n));
                    for (_, row) in rows {
                        row.labels.insert(label.clone());
                    }
                }
                pending.clear();
            } else if piece.trim() != "," {
                pending.clear();
            }
        }
    }
    names
}

/// The differences between a table and a registry: names of the table not registered, families
/// registered that the table does not have, labels of the table missing from their family, and
/// families whose kind their row does not name.
fn differences(table: &BTreeMap<String, Row>, families: &[FamilyInfo]) -> Vec<String> {
    let mut out = Vec::new();
    let by_design: BTreeMap<&str, &FamilyInfo> = families.iter().map(|f| (f.design, f)).collect();
    for (name, row) in table {
        let Some(family) = by_design.get(name.as_str()) else {
            out.push(format!("not registered: {name}"));
            continue;
        };
        for label in &row.labels {
            if !family.labels.contains(&label.as_str()) {
                out.push(format!("{name}: no label {label}"));
            }
        }
        if !row.kinds.contains(family.kind.as_str()) {
            out.push(format!(
                "{name}: {} is not one of {:?}",
                family.kind.as_str(),
                row.kinds
            ));
        }
    }
    for family in families {
        if !table.contains_key(family.design) {
            out.push(format!("not in section 12: {}", family.design));
        }
    }
    out
}

/// A registry with every component registered, with a hot device, one hard drive, one peer
/// and one stripe code.
fn combined() -> Metrics {
    let metrics = Metrics::new("0.0.0-test");
    let _ = metrics.hot_store(&HotStoreConfig {
        device: DriveId([9; 16]),
        replicas: 2,
        peers: vec![NodeId(2)],
    });
    let _ = metrics.cold_store(&ColdStoreConfig {
        drives: vec![DriveId([1; 16])],
        codes: vec![Code::Ec { k: 4, m: 2 }],
    });
    let _ = metrics.peer_service();
    let _ = metrics.s3_front();
    let _ = metrics.node_cache();
    let _ = metrics.fetch_engine(&FetchEngineConfig {
        drives: vec![DriveId([1; 16]), DriveId([2; 16])],
    });
    let _ = metrics.fuse_front();
    let _ = metrics.nfs_front();
    let _ = metrics.prepare_provider();
    metrics
}

/// Asserts that the registry and the table of section 12 have the same names, that each family
/// carries the labels the table gives it, and that each is of a kind its row names.
#[test]
fn every_name_of_section_12_is_a_registered_family_with_its_labels_and_kind() {
    let table = parse_table(DESIGN);
    assert_eq!(
        differences(&table, &combined().families()),
        Vec::<String>::new()
    );
}

/// Asserts that the table of section 12 is read whole: its names count matches the registry's
/// and includes names from its first, last, braced and "by" rows.
#[test]
fn the_table_of_section_12_is_read_with_its_braces_and_by_phrases() {
    let table = parse_table(DESIGN);
    assert_eq!(table.len(), combined().families().len());
    let labels = |name: &str| table[name].labels.iter().cloned().collect::<Vec<_>>();
    assert_eq!(labels("part_permit_wait_seconds"), ["class"]);
    assert_eq!(labels("part_net_seconds"), ["class", "size_class"]);
    assert_eq!(labels("part_first_byte_seconds"), ["class", "size_class"]);
    assert_eq!(labels("gets_total"), ["kind"]);
    assert!(labels("gets_per_pack").is_empty());
    assert_eq!(labels("read_wait_seconds"), ["cause"]);
    assert!(labels("reads_waiting").is_empty());
    assert_eq!(labels("admission_refusals_total"), ["reason"]);
    assert_eq!(labels("drive_state"), ["drive", "state"]);
    assert_eq!(labels("cold_stripes"), ["code", "state"]);
    assert!(table.contains_key("store_audit_keys_total"));
    assert!(table.contains_key("r_await"));
}

/// Asserts that a table with a name the registry lacks, a label its family lacks and a kind its
/// family is not, and a registry with a family the table lacks, are each reported; that a "by"
/// phrase in mid-cell labels only the name before it; that a name in two rows takes the labels
/// and kinds of both; and that a name in parentheses or after the section is not read.
#[test]
fn a_table_and_registry_that_differ_are_reported() {
    let doc = "## 12. Metrics\n\n| Metric | Kind | Answers |\n|---|---|---|\n\
        | `retries_total{cause}`, `made_up_total` by class | counter | x |\n\
        | `hedges_total` by kind, `stall_cancels_total` (see `not_a_name`) | gauge | x |\n\
        | `stall_cancels_total{reason}` | histogram | x |\n\
        ## 13. Next\n| `after_the_section` | counter | x |\n";
    let table = parse_table(doc);
    let names = [
        "retries_total",
        "hedges_total",
        "hedge_wins_total",
        "stall_cancels_total",
    ];
    let families: Vec<FamilyInfo> = combined()
        .families()
        .into_iter()
        .filter(|f| names.contains(&f.design))
        .collect();
    assert_eq!(
        differences(&table, &families),
        [
            "hedges_total: no label kind",
            "hedges_total: counter is not one of {\"gauge\"}",
            "not registered: made_up_total",
            "retries_total: no label cause",
            "retries_total: no label class",
            "stall_cancels_total: no label reason",
            "stall_cancels_total: counter is not one of {\"gauge\", \"histogram\"}",
            "not in section 12: hedge_wins_total",
        ]
    );
}

/// The names of the labels a family may have: closed sets, and drive, node and disk ids. None
/// can hold a pack, a key, a job, a holder or a path.
const ALLOWED_LABELS: &[&str] = &[
    "cause",
    "class",
    "code",
    "copies",
    "dir",
    "disk",
    "drive",
    "kind",
    "margin",
    "mode",
    "node",
    "op",
    "outcome",
    "phase",
    "priority",
    "reason",
    "result",
    "size_class",
    "state",
    "version",
];

/// Asserts that every label of every family is one of the closed or id labels, never a pack, a
/// key or another unbounded value.
#[test]
fn no_family_is_labelled_with_a_pack_a_key_or_another_unbounded_value() {
    for family in combined().families() {
        for label in &family.labels {
            assert!(
                ALLOWED_LABELS.contains(label),
                "{}: label {label} is not a closed or id label",
                family.name
            );
            for forbidden in ["pack", "key", "job", "holder", "tree", "path", "bucket"] {
                assert!(!label.contains(forbidden), "{}: {label}", family.name);
            }
        }
    }
}

/// Asserts that every rendered label of a registry with every component, a drive, a peer and a
/// stripe code, so that every family has a series, is one of the allowed labels or `le`: the text, not only
/// the declared names, carries no other label; and that every family has a series there.
#[test]
fn every_rendered_label_is_an_allowed_label() {
    let metrics = combined();
    let text = metrics.render();
    for family in metrics.families() {
        let has_series = text.lines().any(|l| {
            l.strip_prefix(family.name).is_some_and(|rest| {
                [" ", "{", "_bucket{", "_sum", "_count"]
                    .iter()
                    .any(|s| rest.starts_with(s))
            })
        });
        assert!(has_series, "{} has no series", family.name);
    }
    for line in text.lines().filter(|l| !l.starts_with('#')) {
        if let Some((_, rest)) = line.split_once('{') {
            let pairs = rest.split_once('}').unwrap().0;
            for pair in pairs.split("\",") {
                let label = pair.split_once('=').unwrap().0;
                assert!(label == "le" || ALLOWED_LABELS.contains(&label), "{line}");
            }
        }
    }
}

/// Asserts that the exported names are distinct and start with `tierstore_`, that counters and
/// only counters end with `_total`, and that each is the design's name behind the prefix except
/// the three per-disk gauges, which say what they measure.
#[test]
fn the_exported_names_are_distinct_prefixed_and_follow_the_design() {
    let families = combined().families();
    let names: BTreeSet<&str> = families.iter().map(|f| f.name).collect();
    assert_eq!(names.len(), families.len());
    let renamed: BTreeMap<&str, &str> = [
        ("r_await", "tierstore_disk_r_await_seconds"),
        ("aqu_sz", "tierstore_disk_aqu_sz"),
        ("util", "tierstore_disk_util"),
        ("tierstore_build_info", "tierstore_build_info"),
    ]
    .into_iter()
    .collect();
    for family in &families {
        let expected = renamed.get(family.design).map_or_else(
            || format!("tierstore_{}", family.design),
            |n| (*n).to_owned(),
        );
        assert_eq!(family.name, expected);
        assert_eq!(
            family.kind == Kind::Counter,
            family.name.ends_with("_total"),
            "{}",
            family.name
        );
        assert!(
            !family.help.is_empty() && !family.help.contains('\n'),
            "{}",
            family.name
        );
    }
}

/// Asserts that the text of a registry with every component has the `# HELP` and `# TYPE` lines of every family, in
/// the order of `families`, and at least one series of each family whose labels are all closed.
#[test]
fn a_registry_with_every_component_renders_every_family() {
    let metrics = combined();
    let text = metrics.render();
    let types: Vec<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("# TYPE "))
        .map(str::to_owned)
        .collect();
    let expected: Vec<String> = metrics
        .families()
        .iter()
        .map(|f| format!("{} {}", f.name, f.kind.as_str()))
        .collect();
    assert_eq!(types, expected);
    for family in metrics.families() {
        let closed = family
            .labels
            .iter()
            .all(|l| !["drive", "node", "disk", "code"].contains(l));
        if closed {
            let series = text.lines().filter(|l| !l.starts_with('#')).any(|l| {
                l.strip_prefix(family.name).is_some_and(|rest| {
                    [" ", "{", "_bucket{", "_sum", "_count"]
                        .iter()
                        .any(|s| rest.starts_with(s))
                })
            });
            assert!(series, "{} has no series", family.name);
        }
    }
}
