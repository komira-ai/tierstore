//! The tests of the attempt5 module: each plants a defect in the paper or its result file and
//! shows the check refusing it, and one holds the paper as it is.

use std::path::PathBuf;

use super::attempt5::{RESULTS, problems};
use super::{fixture, paper, scratch, trace_problems};

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
fn once(html: &str, old: &str, new: &str) -> String {
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
fn a_value_moved_to_another_arms_column_is_refused() {
    // Catches: two whole cells swapped between the baseline and cold columns, which keeps every
    // cell's id, attributes and text, every heading and every row label.
    let html = paper();
    let base = "<td data-cell=\"att5.wall_s.komira_sdk_exec.baseline\" data-result=\"ts-farm-a5.tsv\">6,219</td>";
    let cold = "<td data-cell=\"att5.wall_s.komira_sdk_exec.cold\" data-result=\"ts-farm-a5.tsv\">6,519</td>";
    let swapped = once(&html, &format!("{base}{cold}"), &format!("{cold}{base}"));
    let found = problems(&swapped, &fixture().join("results"));
    let joined = found.join("\n");
    assert!(
        joined.contains("the attempt-5 table puts its cells in the columns"),
        "{joined}"
    );
    // The whole-block record sees the same swap in the text it reads.
    assert!(
        joined.contains("the attempt-5 block differs from the record"),
        "{joined}"
    );
    assert_eq!(found.len(), 2, "{joined}");
}

#[test]
fn prose_about_the_run_that_changes_its_meaning_is_refused() {
    // Catches: the block's prose and method notes (outside any cell), its empty baseline cells,
    // the sentences elsewhere that scope the run, and the result file's header and method column
    // edited to say something else about the run while every number stays.
    let html = paper();
    for (old, new, says) in [
        (
            "larger share of lookups than of bytes",
            "smaller share of lookups than of bytes",
            "the attempt-5 block differs from the record",
        ),
        (
            "found no other build;",
            "found another build running;",
            "the attempt-5 block differs from the record",
        ),
        (
            "with the same pins, target and scheme",
            "with different pins, target and scheme",
            "the attempt-5 block differs from the record",
        ),
        (
            "<td>none (no tierstore)</td>",
            "<td>all (no tierstore)</td>",
            "the attempt-5 block differs from the record",
        ),
        (
            "and each arm is one run.</p>",
            "and each arm is the median of three runs.</p>",
            "the page lost the scope of the run",
        ),
        (
            "one build has been measured reading through it",
            "a day of builds has been measured reading through it",
            "the page lost the scope of the run",
        ),
        (
            "Low for the one-build local_cache hit rate: one run per arm,",
            "High for the one-build local_cache hit rate: three runs per arm,",
            "the page lost the scope of the run",
        ),
    ] {
        let joined = refused(&once(&html, old, new));
        assert!(joined.contains(says), "{old:?} -> {new:?}:\n{joined}");
    }
    for (name, old, new, says) in [
        (
            "a5-how",
            "MinIO GETs over the run, all buckets\tatt5.minio_get.komira_sdk_exec.baseline",
            "MinIO GETs over the run, one bucket\tatt5.minio_get.komira_sdk_exec.baseline",
            "minio_get.baseline says it was taken as \"MinIO GETs over the run, one bucket\"",
        ),
        (
            "a5-header",
            "One run per arm, so no medians.",
            "Median of three runs per arm.",
            "the header lost",
        ),
    ] {
        let dir = results_with(name, |t| Some(t.replacen(old, new, 1)));
        let joined = problems(&html, &dir).join("\n");
        assert!(joined.contains(says), "{old:?} -> {new:?}:\n{joined}");
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
            .contains("value \"many\" is not a finite number")
    );
    // A row that no cell prints, so the trace rule never reads it, set to NaN or infinity.
    for (name, value) in [("a5-nan", "NaN"), ("a5-inf", "inf")] {
        let dir = results_with(name, |t| {
            Some(t.replace(
                "lc.bytes_local.cold\t11517872",
                &format!("lc.bytes_local.cold\t{value}"),
            ))
        });
        let joined = problems(&html, &dir).join("\n");
        assert!(
            joined.contains(&format!("value \"{value}\" is not a finite number")),
            "{joined}"
        );
    }
}
