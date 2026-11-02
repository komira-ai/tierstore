//! Attempt 5: one build of `komira_sdk_exec`, a baseline on MinIO and a cold and a warm run
//! through tierstore, one run per arm.
//!
//! The trace rule of the parent module only says that a number a cell prints is a value of some
//! row naming the cell; it passes a result file edited together with the page. This module holds
//! the attempt-5 cells and their result file to the run's record, written here a second time:
//! each cell's whole text (so its numbers, units, signs and labels), the table's arm headings, the
//! setup and caveat phrases of the block, and every row of the result file with its value and
//! unit. An edit to the page or the result file that changes any of them fails until it is made
//! here as well.

use std::path::{Path, PathBuf};

use super::{
    Tag, attr, element_end, element_text, fixture, numbers, paper, plain, scratch, squeeze, tags,
    trace_problems,
};

/// The result file of the attempt-5 cells.
pub(super) const RESULTS: &str = "ts-farm-a5.tsv";

/// The id of the element that holds the attempt-5 subsection.
const BLOCK: &str = "attempt5";

/// A cell this module holds: its id, its basis (`None` for a measured cell, traced to
/// `RESULTS`), the numbers it prints in order, and its whole text, lower case with white space
/// squeezed and entities dropped, as `squeeze(plain(..))` reads it.
struct Cell {
    id: &'static str,
    basis: Option<&'static str>,
    numbers: &'static [f64],
    text: &'static str,
}

const fn measured(id: &'static str, numbers: &'static [f64], text: &'static str) -> Cell {
    Cell {
        id,
        basis: None,
        numbers,
        text,
    }
}

/// Every cell of the attempt-5 block, and the cells elsewhere that quote the run.
const CELLS: &[Cell] = &[
    measured(
        "att5.setup.komira_sdk_exec.one_node",
        &[5194.0, 88.0, 9.0],
        "we built one target, komira_sdk_exec, three times and counted what reached the cold store. \
         the build has 5,194 actions, and in every run all of them were executed remotely, on one \
         node with 88 cores, with no action cache and none run locally; the other nodes took no \
         work during the runs. the baseline is kbf with its cold store on the hdd minio and no \
         tierstore, from an earlier session of runs, ending 9.0 h before the two tierstore runs. \
         the cold run read through the nodes tierstore with an empty local_cache, and the warm run \
         read through it again with the local_cache holding what the cold run had filled. each \
         arm is one run.",
    ),
    measured("att5.actions.komira_sdk_exec.baseline", &[5194.0], "5,194"),
    measured("att5.actions.komira_sdk_exec.cold", &[5194.0], "5,194"),
    measured("att5.actions.komira_sdk_exec.warm", &[5194.0], "5,194"),
    measured(
        "att5.minio_get.komira_sdk_exec.baseline",
        &[306_488.0],
        "306,488",
    ),
    measured(
        "att5.minio_get.komira_sdk_exec.cold",
        &[177_930.0],
        "177,930",
    ),
    measured(
        "att5.minio_get.komira_sdk_exec.warm",
        &[140_509.0],
        "140,509",
    ),
    // The only comparison the page may draw: fewer GETs reached MinIO.
    measured(
        "att5.minio_get_cut.komira_sdk_exec.cold",
        &[42.0],
        "42% fewer",
    ),
    measured(
        "att5.minio_get_cut.komira_sdk_exec.warm",
        &[54.0],
        "54% fewer",
    ),
    measured(
        "att5.lc_object_hit_rate.komira_sdk_exec.cold",
        &[41.4, 22_928.0, 55_430.0],
        "41.4% (22,928 of 55,430 lookups)",
    ),
    measured(
        "att5.lc_object_hit_rate.komira_sdk_exec.warm",
        &[44.0, 20_045.0, 45_589.0],
        "44.0% (20,045 of 45,589 lookups)",
    ),
    measured(
        "att5.lc_bytes.komira_sdk_exec.cold",
        &[11.5, 84.1, 13.7],
        "11.5 mb of 84.1 mb (13.7%)",
    ),
    measured(
        "att5.lc_bytes.komira_sdk_exec.warm",
        &[6.3, 53.1, 11.8],
        "6.3 mb of 53.1 mb (11.8%)",
    ),
    // Wall seconds: reported, not compared.
    measured("att5.wall_s.komira_sdk_exec.baseline", &[6219.0], "6,219"),
    measured("att5.wall_s.komira_sdk_exec.cold", &[6519.0], "6,519"),
    measured("att5.wall_s.komira_sdk_exec.warm", &[6748.0], "6,748"),
    measured(
        "att5.claim.komira_sdk_exec.cold_warm",
        &[],
        "tierstore cut the gets that reached minio in both of its runs, against the baseline, by \
         the shares in the table, and the warm run, with the local_cache already filled, cut more. \
         the count is minios cluster-wide counter and the baseline is from an earlier session \
         (below).",
    ),
    // Why there is no build-time claim: single runs, a 9.0 h older baseline on an older kbf, a
    // compute-bound build, kbf's own 100 GiB hot tier in front of tierstore.
    measured(
        "att5.no_time_claim.komira_sdk_exec.baseline_cold_warm",
        &[9.0, 100.0],
        "we make no claim about build time from these runs, in either direction. each arm is a \
         single run. the baseline ran 9.0 h before the tierstore runs and on an older kbf, v0.0.49 \
         against v0.0.52. the build is compute-bound, and kbfs own 100 gib local hot tier on the \
         node serves most repeat reads before a read reaches tierstore at all. the wall times are \
         in the table as recorded and are not evidence about tierstore.",
    ),
    // The kbf note, from code reading: the cold-store counters were added, so it must not say
    // that nothing on the read path changed.
    Cell {
        id: "att5.kbf_versions.na.v0_0_49_vs_v0_0_52",
        basis: Some("code-reading"),
        numbers: &[],
        text: "between v0.0.49 and v0.0.52 the storage read path was not changed in what it \
               reads, from where, how many requests it makes, or in what order, and the \
               object-store client crates are byte-identical. the release added counters of \
               requests, errors and latency around every cold-store request (a few atomic \
               operations and two clock reads per request, negligible against an object stores \
               round-trip time), and a key-file option that affects start-up only.",
    },
    // Section 5's cell that the run fills.
    measured(
        "lc.hit_rate_cold_reads_per_build.na.local_cache_over_cold",
        &[41.4, 44.0, 177_930.0, 140_509.0],
        "for one build per arm (the cold and warm runs above), a local_cache hit rate of 41.4% and \
         44.0% of object lookups, with 177,930 and 140,509 gets reaching the cold minio; over a \
         day of builds, not measured",
    ),
    // The conclusion quotes the GET cut.
    measured(
        "concl.minio_get_cut.komira_sdk_exec.cold_warm",
        &[42.0, 54.0],
        "tierstore needs far fewer reads from the object store. in one build read through a nodes \
         tierstore, the gets that reached minio fell by 42% with an empty local_cache and by 54% \
         with a filled one, against the baseline run.",
    ),
];

/// The headings of the table's columns: which arm is which, and on which kbf.
const ARMS: &[&str] = &[
    "quantity, one run per arm",
    "baseline: minio, no tierstore (kbf v0.0.49)",
    "cold: tierstore, empty local_cache (kbf v0.0.52)",
    "warm: tierstore, local_cache filled (kbf v0.0.52)",
];

/// The labels of the table's rows, in order: which quantity each line of cells is.
const ROW_LABELS: &[&str] = &[
    "actions, all executed remotely",
    "gets that reached minio",
    "gets that reached minio, against the baseline",
    "local_cache hit rate, by object",
    "bytes tierstore served from the local_cache, of all it served",
    "wall time of the build, seconds (not a comparison; see below)",
];

/// Phrases of the block's method notes that carry a caveat the numbers depend on.
const CAVEATS: &[&str] = &[
    "the action cache was off and each run started a fresh build daemon",
    "the difference of the node tierstores own counters read before and after each run",
    "a read that kbfs own hot tier answered never reached tierstore and is in none of them",
    "the counts are its cluster-wide counter and include any other client of that minio during a run",
    "the other nodes were taken out of service for the runs",
    "it is a before-and-after snapshot and can miss a build that starts and ends inside a run",
    "it was not repeated with the cold and warm runs",
    "a day of builds, the hit rate of each tier, and how many of the gets that reached minio were tierstores own misses are not measured",
];

/// Every row of the result file: id, value, unit. The file holds these and no others.
const ROWS: &[(&str, f64, &str)] = &[
    ("actions", 5194.0, "count"),
    ("node_cores", 88.0, "count"),
    ("baseline_age_h", 9.0, "h"),
    ("kbf_hot_tier_gib", 100.0, "GiB"),
    ("wall_s.baseline", 6219.0, "s"),
    ("wall_s.cold", 6519.0, "s"),
    ("wall_s.warm", 6748.0, "s"),
    ("minio_get.baseline", 306_488.0, "count"),
    ("minio_get.cold", 177_930.0, "count"),
    ("minio_get.warm", 140_509.0, "count"),
    ("lc.hits.cold", 22_928.0, "count"),
    ("lc.misses.cold", 32_502.0, "count"),
    ("lc.lookups.cold", 55_430.0, "count"),
    ("lc.hits.warm", 20_045.0, "count"),
    ("lc.misses.warm", 25_544.0, "count"),
    ("lc.lookups.warm", 45_589.0, "count"),
    ("lc.bytes_local.cold", 11_517_872.0, "bytes"),
    ("lc.bytes_upstream.cold", 72_602_117.0, "bytes"),
    ("lc.bytes_served.cold", 84_119_989.0, "bytes"),
    ("lc.bytes_local.warm", 6_289_943.0, "bytes"),
    ("lc.bytes_upstream.warm", 46_837_763.0, "bytes"),
    ("lc.bytes_served.warm", 53_127_706.0, "bytes"),
    ("minio_get_cut_pct.cold", 42.0, "percent"),
    ("minio_get_cut_pct.warm", 54.0, "percent"),
    ("lc.hit_rate_obj_pct.cold", 41.4, "percent"),
    ("lc.hit_rate_obj_pct.warm", 44.0, "percent"),
    ("lc.mb_local.cold", 11.5, "MB"),
    ("lc.mb_served.cold", 84.1, "MB"),
    ("lc.mb_local.warm", 6.3, "MB"),
    ("lc.mb_served.warm", 53.1, "MB"),
    ("lc.hit_rate_bytes_pct.cold", 13.7, "percent"),
    ("lc.hit_rate_bytes_pct.warm", 11.8, "percent"),
];

/// The indexes of the start tags whose `data-cell` is `cell`.
fn cells_named(all: &[Tag<'_>], cell: &str) -> Vec<usize> {
    (0..all.len())
        .filter(|&k| !all[k].closing && attr(all[k].attrs, "data-cell") == Some(cell))
        .collect()
}

fn known(id: &str) -> bool {
    CELLS.iter().any(|c| c.id == id)
}

/// The rows of a result file as (id, value, unit), or why it cannot be read.
fn rows_with_units(text: &str) -> Result<Vec<(String, f64, String)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        let [_, id, value, unit, ..] = columns[..] else {
            return Err(format!(
                "line {}: fewer than 4 tab-separated columns",
                n + 1
            ));
        };
        let value = value
            .parse::<f64>()
            .map_err(|_| format!("line {}: value {value:?} is not a number", n + 1))?;
        out.push((id.to_owned(), value, unit.to_owned()));
    }
    Ok(out)
}

/// One line per way the attempt-5 cells, the block that holds them and their result file
/// disagree with the run's record.
pub(super) fn problems(html: &str, results: &Path) -> Vec<String> {
    let all = tags(html);
    let mut problems = Vec::new();
    for cell in CELLS {
        let found = cells_named(&all, cell.id);
        let [k] = found[..] else {
            problems.push(format!(
                "{}: {} elements carry this cell, not one",
                cell.id,
                found.len()
            ));
            continue;
        };
        match cell.basis {
            None if attr(all[k].attrs, "data-result") != Some(RESULTS) => {
                problems.push(format!("{}: not traced to {RESULTS}", cell.id));
            }
            Some(basis) if attr(all[k].attrs, "data-basis") != Some(basis) => {
                problems.push(format!("{}: not data-basis=\"{basis}\"", cell.id));
            }
            _ => {}
        }
        let Some(text) = element_text(html, &all, k) else {
            problems.push(format!("{}: the element is never closed", cell.id));
            continue;
        };
        let got = numbers(&text);
        let same = got.len() == cell.numbers.len()
            && got
                .iter()
                .zip(cell.numbers)
                .all(|(a, b)| (a - b).abs() < 1e-9);
        if !same {
            problems.push(format!(
                "{}: prints {got:?}, the run measured {:?}",
                cell.id, cell.numbers
            ));
        }
        let text = squeeze(&text);
        if text != cell.text {
            problems.push(format!(
                "{}: reads {text:?}, the record is {:?}",
                cell.id, cell.text
            ));
        }
    }
    if squeeze(&plain(html)).contains("nothing on the storage read path changed") {
        problems.push(
            "the page says nothing on the storage read path changed; the cold-store counters did"
                .to_owned(),
        );
    }
    // A cell that prints the run under another name, or quotes the result file, is held here
    // too: an unknown one is refused, whatever its prefix.
    for tag in all.iter().filter(|t| !t.closing) {
        if let Some(id) = attr(tag.attrs, "data-cell")
            && !known(id)
            && (id.starts_with("att5.") || attr(tag.attrs, "data-result") == Some(RESULTS))
        {
            problems.push(format!("{id}: an attempt-5 cell this check does not hold"));
        }
    }
    let block: Vec<usize> = (0..all.len())
        .filter(|&k| !all[k].closing && attr(all[k].attrs, "id") == Some(BLOCK))
        .collect();
    let [b] = block[..] else {
        problems.push(format!(
            "{} elements have id=\"{BLOCK}\", not one",
            block.len()
        ));
        problems.extend(row_problems(results));
        return problems;
    };
    match element_end(&all, b) {
        Some(end) => problems.extend(block_problems(html, &all, b, end)),
        None => problems.push("the attempt-5 block is never closed".to_owned()),
    }
    problems.extend(row_problems(results));
    problems
}

/// The problems of the block whose start tag is `all[b]` and whose end tag starts at `end`.
fn block_problems(html: &str, all: &[Tag<'_>], b: usize, end: usize) -> Vec<String> {
    let mut problems = Vec::new();
    // Every number of the block sits in a cell this module holds, so a number in the prose
    // around them, or in a cell it does not know (whatever its basis), is refused.
    let mut outside = String::new();
    let mut at = all[b].end;
    for (k, tag) in all.iter().enumerate().skip(b + 1) {
        if tag.start >= end {
            break;
        }
        if tag.closing || tag.start < at {
            continue;
        }
        let Some(id) = attr(tag.attrs, "data-cell") else {
            continue;
        };
        if !known(id) {
            problems.push(format!(
                "{id}: a cell in the attempt-5 block this check does not hold"
            ));
            continue;
        }
        outside.push_str(&html[at..tag.start]);
        outside.push(' ');
        at = element_end(all, k).unwrap_or(end).min(end);
    }
    outside.push_str(&html[at.min(end)..end]);
    let stray = numbers(&plain(&outside));
    if !stray.is_empty() {
        problems.push(format!(
            "the attempt-5 block prints {stray:?} outside its cells"
        ));
    }
    let inner = &html[all[b].end..end];
    let text = squeeze(&plain(inner));
    for word in super::claims::SPEED_WORDS {
        if super::claims::has_word(&text, word) {
            problems.push(format!(
                "the attempt-5 block says {word:?}: one run per arm supports no comparison of time"
            ));
        }
    }
    for caveat in CAVEATS {
        if !text.contains(caveat) {
            problems.push(format!("the attempt-5 block lost the caveat {caveat:?}"));
        }
    }
    let inner_tags = tags(inner);
    let heads: Vec<String> = (0..inner_tags.len())
        .filter(|&k| !inner_tags[k].closing && inner_tags[k].name == "th")
        .filter_map(|k| element_text(inner, &inner_tags, k))
        .map(|t| squeeze(&t))
        .collect();
    if heads != ARMS {
        problems.push(format!(
            "the attempt-5 table heads the arms {heads:?}, the record is {ARMS:?}"
        ));
    }
    // A body row starts with its label cell; a head row starts with a th and is not one.
    let labels: Vec<String> = (0..inner_tags.len().saturating_sub(1))
        .filter(|&k| {
            let (row, first) = (&inner_tags[k], &inner_tags[k + 1]);
            row.name == "tr" && !row.closing && first.name == "td" && !first.closing
        })
        .filter_map(|k| element_text(inner, &inner_tags, k + 1))
        .map(|t| squeeze(&t))
        .collect();
    if labels != ROW_LABELS {
        problems.push(format!(
            "the attempt-5 table labels its rows {labels:?}, the record is {ROW_LABELS:?}"
        ));
    }
    problems
}

/// The problems of the result file: a row missing, doubled, with another value or unit, or one
/// the run does not have (a wall-time difference or ratio among them).
fn row_problems(results: &Path) -> Vec<String> {
    let rows = match std::fs::read_to_string(results.join(RESULTS))
        .map_err(|e| e.to_string())
        .and_then(|text| rows_with_units(&text))
    {
        Ok(rows) => rows,
        Err(why) => return vec![format!("{RESULTS}: {why}")],
    };
    let mut problems = Vec::new();
    for (id, value, unit) in ROWS {
        match rows.iter().filter(|r| r.0 == *id).collect::<Vec<_>>()[..] {
            [row] => {
                if (row.1 - value).abs() >= 1e-9 {
                    problems.push(format!(
                        "{RESULTS}: {id} is {}, the run measured {value}",
                        row.1
                    ));
                }
                if row.2 != *unit {
                    problems.push(format!(
                        "{RESULTS}: {id} is in {}, the run's unit is {unit}",
                        row.2
                    ));
                }
            }
            ref other => problems.push(format!("{RESULTS}: {} rows {id}, not one", other.len())),
        }
    }
    for (id, _, _) in &rows {
        if !ROWS.iter().any(|r| r.0 == id) {
            problems.push(format!(
                "{RESULTS}: {id} is a row the run does not have (no wall-time difference or ratio is one)"
            ));
        }
    }
    problems
}

/// The fixture's results copied to a scratch directory, with `edit` applied to the attempt-5
/// file (`None` deletes it).
fn results_with(name: &str, edit: impl Fn(&str) -> Option<String>) -> PathBuf {
    let dir = scratch(name);
    for entry in std::fs::read_dir(fixture().join("results")).expect("results") {
        let path = entry.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("result file");
        let file = path.file_name().expect("name");
        let text = if file == RESULTS {
            edit(&text)
        } else {
            Some(text)
        };
        if let Some(text) = text {
            std::fs::write(dir.join(file), text).expect("write");
        }
    }
    dir
}

/// `html` with the one occurrence of `old` replaced by `new`.
pub(super) fn once(html: &str, old: &str, new: &str) -> String {
    assert_eq!(
        html.matches(old).count(),
        1,
        "{old:?} occurs once in the page"
    );
    html.replacen(old, new, 1)
}

/// What the check says when the row `old` (id, tab, value) is edited to `new`.
fn edit_says(old: &str, new: &str) -> String {
    let (id, was) = old.split_once('\t').expect("id and value");
    let now = new.split_once('\t').expect("id and value").1;
    format!("ts-farm-a5.tsv: {id} is {now}, the run measured {was}")
}

/// The problems of `html` against the fixture's results, as one text.
fn refused(html: &str) -> String {
    problems(html, &fixture().join("results")).join("\n")
}

#[test]
fn the_attempt_5_cells_hold_the_measured_numbers() {
    // Catches: a filled attempt-5 cell, a heading, a caveat or a row of the result file edited
    // away from what the run measured; a cell dropped, doubled, untraced or added unchecked; a
    // number in the attempt-5 prose that no rule reads; the kbf-version note weakened or
    // overstated. Holds today.
    let html = paper();
    let found = problems(&html, &fixture().join("results"));
    assert!(found.is_empty(), "{}", found.join("\n"));
}

#[test]
fn a_cell_with_the_right_digits_and_the_wrong_meaning_is_refused() {
    // Catches: a check that compares only the digits of a cell, so that a unit, a sign, a label,
    // an arm heading or a direction word can change under the same numbers.
    let html = paper();
    for (old, new, says) in [
        (
            ">6,519</td>",
            ">6,159</td>",
            "att5.wall_s.komira_sdk_exec.cold: prints [6159.0]",
        ),
        (
            "11.5 MB of 84.1 MB",
            "11.5 GiB of 84.1 GiB",
            "att5.lc_bytes.komira_sdk_exec.cold: reads \"11.5 gib of 84.1 gib (13.7%)\"",
        ),
        (
            ">54% fewer<",
            ">54% more<",
            "att5.minio_get_cut.komira_sdk_exec.warm: reads \"54% more\"",
        ),
        (
            ">54% fewer<",
            ">-54% fewer<",
            "att5.minio_get_cut.komira_sdk_exec.warm: reads \"-54% fewer\"",
        ),
        (
            "41.4% (22,928 of 55,430 lookups)",
            "41.4% (22,928 of 55,430 bytes)",
            "att5.lc_object_hit_rate.komira_sdk_exec.cold: reads",
        ),
        (
            "<td>local_cache hit rate, by object</td>",
            "<td>local_cache hit rate, by byte</td>",
            "the attempt-5 table labels its rows",
        ),
        (
            "<th>Baseline: MinIO, no tierstore (kbf v0.0.49)</th>",
            "<th>Baseline: MinIO, no tierstore (kbf v0.0.52)</th>",
            "the attempt-5 table heads the arms",
        ),
        (
            "all of them were executed remotely",
            "all of them were executed locally",
            "att5.setup.komira_sdk_exec.one_node: reads",
        ),
        (
            "The build is compute-bound, and",
            "The build is I/O-bound, and",
            "att5.no_time_claim.komira_sdk_exec.baseline_cold_warm: reads",
        ),
        (
            "The action cache was off and each run",
            "The action cache was on and each run",
            "lost the caveat \"the action cache was off",
        ),
        (
            "The release added counters of requests, errors and latency around every cold-store request",
            "The release added nothing on the read path",
            "att5.kbf_versions.na.v0_0_49_vs_v0_0_52: reads",
        ),
        (
            "the object-store client crates are byte-identical",
            "nothing on the storage read path changed",
            "the page says nothing on the storage read path changed",
        ),
    ] {
        let joined = refused(&once(&html, old, new));
        assert!(joined.contains(says), "{old:?} -> {new:?}:\n{joined}");
    }
}

#[test]
fn a_cell_that_is_missing_doubled_unclosed_or_untraced_is_refused() {
    // Catches: a required cell dropped, carried by two elements, never closed, or traced to
    // another result file or to a basis; an unknown cell in the block whatever its prefix and
    // basis; a number in the prose of the block.
    let html = paper();
    let cases = [
        (
            once(
                &html,
                "data-cell=\"att5.minio_get_cut.komira_sdk_exec.warm\" ",
                "",
            ),
            "att5.minio_get_cut.komira_sdk_exec.warm: 0 elements carry this cell, not one",
        ),
        (
            once(
                &html,
                "<td>the reference</td>",
                "<td data-cell=\"att5.minio_get_cut.komira_sdk_exec.warm\" data-result=\"ts-farm-a5.tsv\">54% fewer</td>",
            ),
            "att5.minio_get_cut.komira_sdk_exec.warm: 2 elements carry this cell, not one",
        ),
        (
            once(
                &html,
                "<td data-cell=\"att5.wall_s.komira_sdk_exec.warm\" data-result=\"ts-farm-a5.tsv\">6,748</td>",
                "<xcell data-cell=\"att5.wall_s.komira_sdk_exec.warm\" data-result=\"ts-farm-a5.tsv\">6,748</td>",
            ),
            "att5.wall_s.komira_sdk_exec.warm: the element is never closed",
        ),
        (
            once(
                &html,
                "data-cell=\"att5.wall_s.komira_sdk_exec.warm\" data-result=\"ts-farm-a5.tsv\"",
                "data-cell=\"att5.wall_s.komira_sdk_exec.warm\" data-result=\"ts-m2-phase-b-a3.tsv\"",
            ),
            "att5.wall_s.komira_sdk_exec.warm: not traced to ts-farm-a5.tsv",
        ),
        (
            once(
                &html,
                "data-cell=\"att5.kbf_versions.na.v0_0_49_vs_v0_0_52\" data-basis=\"code-reading\"",
                "data-cell=\"att5.kbf_versions.na.v0_0_49_vs_v0_0_52\" data-basis=\"decision\"",
            ),
            "att5.kbf_versions.na.v0_0_49_vs_v0_0_52: not data-basis=\"code-reading\"",
        ),
        (
            once(
                &html,
                "<p><strong>What this shows.</strong>",
                "<p><strong>What this shows.</strong> <span data-cell=\"design.speed\" data-basis=\"design\">It took 5,800 s.</span>",
            ),
            "design.speed: a cell in the attempt-5 block this check does not hold",
        ),
        (
            once(
                &html,
                "<p><strong>What this shows.</strong>",
                "<p><strong>What this shows.</strong> In 12 of 13 runs.",
            ),
            "the attempt-5 block prints [12.0, 13.0] outside its cells",
        ),
        (
            once(
                &html,
                "<div id=\"attempt5\" class=\"attempt5\">",
                "<div id=\"attempt5\" class=\"attempt5\"><p data-cell=\"att5.extra.na.na\" data-result=\"ts-farm-a5.tsv\">x</p>",
            ),
            "att5.extra.na.na: an attempt-5 cell this check does not hold",
        ),
    ];
    for (page, says) in cases {
        let joined = refused(&page);
        assert!(joined.contains(says), "{says}:\n{joined}");
    }
}

#[test]
fn a_missing_or_doubled_block_is_refused() {
    // Catches: the block's id lost (its prose then escapes every rule), the block doubled, or
    // the block never closed.
    let html = paper();
    let gone = once(
        &html,
        "<div id=\"attempt5\" class=\"attempt5\">",
        "<div class=\"attempt5\">",
    );
    assert!(refused(&gone).contains("0 elements have id=\"attempt5\", not one"));
    let twice = once(
        &html,
        "<p><strong>What this shows.</strong>",
        "<div id=\"attempt5\"></div><p><strong>What this shows.</strong>",
    );
    assert!(refused(&twice).contains("2 elements have id=\"attempt5\", not one"));
    let open = once(
        &html,
        "<div id=\"attempt5\" class=\"attempt5\">",
        "<xblock id=\"attempt5\">",
    );
    assert!(refused(&open).contains("the attempt-5 block is never closed"));
}

#[test]
fn a_result_file_that_disagrees_with_the_run_is_refused() {
    // Catches: a row edited together with the page (which the trace rule alone accepts), a row
    // that no cell prints (the misses and the upstream bytes) edited or deleted, a unit changed,
    // a row doubled, a derived wall-time row added, and a result file missing or unreadable.
    let html = paper();
    let both = once(&html, ">306,488</td>", ">306,000</td>");
    let edited = results_with("a5-rows", |t| {
        Some(t.replace("minio_get.baseline\t306488", "minio_get.baseline\t306000"))
    });
    assert!(trace_problems(&both, &edited).is_empty());
    let joined = problems(&both, &edited).join("\n");
    assert!(
        joined.contains("ts-farm-a5.tsv: minio_get.baseline is 306000, the run measured 306488"),
        "{joined}"
    );
    for (name, edit, says) in [
        (
            "a5-misses",
            "lc.misses.cold\t32502",
            "lc.misses.cold\t32000",
        ),
        (
            "a5-upstream",
            "lc.bytes_upstream.warm\t46837763",
            "lc.bytes_upstream.warm\t1",
        ),
    ] {
        let dir = results_with(name, |t| Some(t.replace(edit, says)));
        let joined = problems(&html, &dir).join("\n");
        assert!(joined.contains(&edit_says(edit, says)), "{joined}");
    }
    let deleted = results_with("a5-deleted", |t| {
        Some(
            t.lines()
                .filter(|l| !l.contains("\tlc.misses.warm\t"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    });
    assert!(
        problems(&html, &deleted)
            .join("\n")
            .contains("ts-farm-a5.tsv: 0 rows lc.misses.warm, not one")
    );
    let doubled = results_with("a5-doubled", |t| {
        let row = t
            .lines()
            .find(|l| l.contains("\tlc.hits.cold\t"))
            .unwrap()
            .to_owned();
        Some(format!("{t}{row}\n"))
    });
    assert!(
        problems(&html, &doubled)
            .join("\n")
            .contains("ts-farm-a5.tsv: 2 rows lc.hits.cold, not one")
    );
    let unit = results_with("a5-unit", |t| {
        Some(t.replace(
            "lc.mb_served.cold\t84.1\tMB",
            "lc.mb_served.cold\t84.1\tGiB",
        ))
    });
    assert!(
        problems(&html, &unit)
            .join("\n")
            .contains("ts-farm-a5.tsv: lc.mb_served.cold is in GiB, the run's unit is MB")
    );
    let ratio = results_with("a5-ratio", |t| {
        Some(format!(
            "{t}derived\twall_ratio.warm\t1.09\tratio\t6748 / 6219\n"
        ))
    });
    assert!(
        problems(&html, &ratio)
            .join("\n")
            .contains("ts-farm-a5.tsv: wall_ratio.warm is a row the run does not have")
    );
    let gone = results_with("a5-gone", |_| None);
    assert!(
        problems(&html, &gone)
            .join("\n")
            .contains("ts-farm-a5.tsv: No such file")
    );
    let bad = results_with("a5-bad", |t| {
        Some(t.replace("wall_s.cold\t6519", "wall_s.cold\tmany"))
    });
    assert!(
        problems(&html, &bad)
            .join("\n")
            .contains("ts-farm-a5.tsv: line")
    );
    assert!(
        problems(&html, &bad)
            .join("\n")
            .contains("value \"many\" is not a number")
    );
}
