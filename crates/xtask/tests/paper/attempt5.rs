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

use std::path::Path;

use super::{Tag, attr, element_end, element_text, numbers, plain, squeeze, tags};

/// The result file of the attempt-5 cells.
pub(super) const RESULTS: &str = "ts-farm-a5.tsv";

/// The id of the element that holds the attempt-5 subsection.
pub(super) const BLOCK: &str = "attempt5";

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

/// The whole text of the attempt-5 block, as `squeeze(plain(..))` reads it, in pieces that join
/// with nothing between them. Every word of the block is the record: its prose around the cells,
/// the empty baseline cells, the method notes.
const BLOCK_TEXT: &[&str] = &[
    "we built one target, komira_sdk_exec, three times and counted what reached the cold ",
    "store. the build has 5,194 actions, and in every run all of them were executed ",
    "remotely, on one node with 88 cores, with no action cache and none run locally; the ",
    "other nodes took no work during the runs. the baseline is kbf with its cold store on ",
    "the hdd minio and no tierstore, from an earlier session of runs, ending 9.0 h before ",
    "the two tierstore runs. the cold run read through the nodes tierstore with an empty ",
    "local_cache, and the warm run read through it again with the local_cache holding what ",
    "the cold run had filled. each arm is one run. quantity, one run per armbaseline: minio, ",
    "no tierstore (kbf v0.0.49)cold: tierstore, empty local_cache (kbf v0.0.52)warm: ",
    "tierstore, local_cache filled (kbf v0.0.52) actions, all executed ",
    "remotely5,1945,1945,194 gets that reached minio306,488177,930140,509 gets that reached ",
    "minio, against the baselinethe reference42% fewer54% fewer local_cache hit rate, by ",
    "objectno lookups (no tierstore)41.4% (22,928 of 55,430 lookups)44.0% (20,045 of 45,589 ",
    "lookups) bytes tierstore served from the local_cache, of all it servednone (no ",
    "tierstore)11.5 mb of 84.1 mb (13.7%)6.3 mb of 53.1 mb (11.8%) wall time of the build, ",
    "seconds (not a comparison; see below)6,2196,5196,748 what this shows. tierstore cut the ",
    "gets that reached minio in both of its runs, against the baseline, by the shares in the ",
    "table, and the warm run, with the local_cache already filled, cut more. the count is ",
    "minios cluster-wide counter and the baseline is from an earlier session (below). the ",
    "local_cache answered a larger share of lookups than of bytes, so the objects it hit ",
    "were smaller on average than those it served. what this does not show: build time. we ",
    "make no claim about build time from these runs, in either direction. each arm is a ",
    "single run. the baseline ran 9.0 h before the tierstore runs and on an older kbf, ",
    "v0.0.49 against v0.0.52. the build is compute-bound, and kbfs own 100 gib local hot ",
    "tier on the node serves most repeat reads before a read reaches tierstore at all. the ",
    "wall times are in the table as recorded and are not evidence about tierstore. kbf ",
    "v0.0.49 against v0.0.52. between v0.0.49 and v0.0.52 the storage read path was not ",
    "changed in what it reads, from where, how many requests it makes, or in what order, and ",
    "the object-store client crates are byte-identical. the release added counters of ",
    "requests, errors and latency around every cold-store request (a few atomic operations ",
    "and two clock reads per request, negligible against an object stores round-trip time), ",
    "and a key-file option that affects start-up only. how this was measured, and how far to ",
    "trust it every run executed every action. the action cache was off and each run started ",
    "a fresh build daemon, and the keys of most actions were changed between runs, so no run ",
    "reused anothers results. each builds own record says how many actions it executed ",
    "remotely and how many locally; the table has them. what the hit rate counts. the ",
    "tierstore figures are the difference of the node tierstores own counters read before ",
    "and after each run: hits, misses, and bytes served from the local_cache and from the ",
    "cold store. a read that kbfs own hot tier answered never reached tierstore and is in ",
    "none of them. what the get count counts. minio does not expose a get counter per bucket ",
    "here, so the counts are its cluster-wide counter and include any other client of that ",
    "minio during a run. one node, not the farm. the other nodes were taken out of service ",
    "for the runs. a check of the nodes leases on both kbf frontends before and after each ",
    "run found no other build; it is a before-and-after snapshot and can miss a build that ",
    "starts and ends inside a run. the baseline is older. it was not repeated with the cold ",
    "and warm runs: it is from the session before, with the same pins, target and scheme for ",
    "changing keys, on kbf v0.0.49. still open. a day of builds, the hit rate of each tier, ",
    "and how many of the gets that reached minio were tierstores own misses are not ",
    "measured.",
];

/// Sentences outside the block that scope the run: how many builds, runs and nodes it covers,
/// and what the conclusion draws from it. Each must appear word for word in the page.
const SCOPE: &[&str] = &[
    "our build farm is the first application and the only workload we have measured, \
     and one build read through v1 (section 5) is the only farm traffic through it \
     we have measured, so sections 1 and 5 are a case study marked with a case study \
     label, and farm-specific detail elsewhere sits in side notes.",
    "v1 runs in place on all six nodes; one build has been measured reading through \
     it (section 5).",
    "for the build runs below, kbf on the one node that ran them read its cold store \
     through that nodes tierstore; before them the farm read straight from the hdd \
     minio.",
    "for the build runs above, kbf on the one node that ran them read its cold store \
     through that nodes tierstore.",
    "we hold the local_caches hit rate and the gets at the cold minio for one build \
     per arm (the build runs above) and no count over a day of builds or by tier.",
    "the count of what it did serve exists for one build per arm only.",
    "this paper measures how many reads reached the cold store for one build only, \
     on one node, one run per arm (section 5).",
    "for one build per arm, section 5 has the local_caches hit rate by object and by \
     byte and the gets that reached the cold minio.",
    "low for the one-build local_cache hit rate: one run per arm, one node, a \
     cluster-wide get counter.",
    "a get the local_cache answers is a request that the object stores disks and the \
     network to them do not carry, so that capacity is left for everything else on \
     the farm.",
    "the runs do not show a change in build time, and we draw none from them \
     (section 5 says why).",
    "a read cache does nothing for that path, and how much of a build it takes we \
     did not measure.",
    "we counted requests at the object store, not its bytes or its bandwidth, and \
     each arm is one run.",
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

/// The cells of each body row of the table, column by column (label, baseline, cold, warm): a
/// value moved to another arm's column is refused even when its own cell is unchanged.
const ROW_CELLS: &[[Option<&str>; 4]] = &[
    [
        None,
        Some("att5.actions.komira_sdk_exec.baseline"),
        Some("att5.actions.komira_sdk_exec.cold"),
        Some("att5.actions.komira_sdk_exec.warm"),
    ],
    [
        None,
        Some("att5.minio_get.komira_sdk_exec.baseline"),
        Some("att5.minio_get.komira_sdk_exec.cold"),
        Some("att5.minio_get.komira_sdk_exec.warm"),
    ],
    [
        None,
        None,
        Some("att5.minio_get_cut.komira_sdk_exec.cold"),
        Some("att5.minio_get_cut.komira_sdk_exec.warm"),
    ],
    [
        None,
        None,
        Some("att5.lc_object_hit_rate.komira_sdk_exec.cold"),
        Some("att5.lc_object_hit_rate.komira_sdk_exec.warm"),
    ],
    [
        None,
        None,
        Some("att5.lc_bytes.komira_sdk_exec.cold"),
        Some("att5.lc_bytes.komira_sdk_exec.warm"),
    ],
    [
        None,
        Some("att5.wall_s.komira_sdk_exec.baseline"),
        Some("att5.wall_s.komira_sdk_exec.cold"),
        Some("att5.wall_s.komira_sdk_exec.warm"),
    ],
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

/// The fifth column of each row: how the value was taken. It is part of the record (a per-bucket
/// count or a median of runs would be another measurement).
const HOWS: &[(&str, &str)] = &[
    (
        "actions",
        "actions of the build in every arm; every one executed remotely, none from the action cache, none locally",
    ),
    (
        "node_cores",
        "cores of the one node that executed every action of every arm",
    ),
    (
        "baseline_age_h",
        "hours from the end of the baseline run to the start of the cold run",
    ),
    (
        "kbf_hot_tier_gib",
        "size of kbf's own local hot tier on the node, which kbf reads before it asks its cold store",
    ),
    ("wall_s.baseline", "wall time of the build, one run"),
    ("wall_s.cold", "wall time of the build, one run"),
    ("wall_s.warm", "wall time of the build, one run"),
    ("minio_get.baseline", "MinIO GETs over the run, all buckets"),
    ("minio_get.cold", "MinIO GETs over the run, all buckets"),
    ("minio_get.warm", "MinIO GETs over the run, all buckets"),
    ("lc.hits.cold", "local_cache hits (cache_hits_total)"),
    ("lc.misses.cold", "local_cache misses (cache_misses_total)"),
    ("lc.lookups.cold", "hits plus misses"),
    ("lc.hits.warm", "local_cache hits (cache_hits_total)"),
    ("lc.misses.warm", "local_cache misses (cache_misses_total)"),
    ("lc.lookups.warm", "hits plus misses"),
    (
        "lc.bytes_local.cold",
        "bytes served from the local_cache (cache_bytes_served_local_total)",
    ),
    (
        "lc.bytes_upstream.cold",
        "bytes served from the cold store through tierstore (cache_bytes_served_upstream_total)",
    ),
    ("lc.bytes_served.cold", "local plus upstream"),
    (
        "lc.bytes_local.warm",
        "bytes served from the local_cache (cache_bytes_served_local_total)",
    ),
    (
        "lc.bytes_upstream.warm",
        "bytes served from the cold store through tierstore (cache_bytes_served_upstream_total)",
    ),
    ("lc.bytes_served.warm", "local plus upstream"),
    (
        "minio_get_cut_pct.cold",
        "(306488 - 177930) / 306488 = 41.9%, rounded",
    ),
    (
        "minio_get_cut_pct.warm",
        "(306488 - 140509) / 306488 = 54.2%, rounded",
    ),
    ("lc.hit_rate_obj_pct.cold", "22928 / 55430"),
    ("lc.hit_rate_obj_pct.warm", "20045 / 45589"),
    ("lc.mb_local.cold", "11517872 bytes / 10^6"),
    ("lc.mb_served.cold", "84119989 bytes / 10^6"),
    ("lc.mb_local.warm", "6289943 bytes / 10^6"),
    ("lc.mb_served.warm", "53127706 bytes / 10^6"),
    ("lc.hit_rate_bytes_pct.cold", "11517872 / 84119989"),
    ("lc.hit_rate_bytes_pct.warm", "6289943 / 53127706"),
];

/// A line the result file's header must keep: it says what each value is.
const HEADER: &str = "One run per arm, so no medians.";

/// The indexes of the start tags whose `data-cell` is `cell`.
fn cells_named(all: &[Tag<'_>], cell: &str) -> Vec<usize> {
    (0..all.len())
        .filter(|&k| !all[k].closing && attr(all[k].attrs, "data-cell") == Some(cell))
        .collect()
}

fn known(id: &str) -> bool {
    CELLS.iter().any(|c| c.id == id)
}

/// The rows of a result file as (id, value, unit, how), or why it cannot be read.
fn rows_with_units(text: &str) -> Result<Vec<(String, f64, String, String)>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        let [_, id, value, unit, ref rest @ ..] = columns[..] else {
            return Err(format!(
                "line {}: fewer than 4 tab-separated columns",
                n + 1
            ));
        };
        // NaN and infinity parse, and compare unequal to everything, so they are refused here.
        let value = value
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("line {}: value {value:?} is not a finite number", n + 1))?;
        let how = rest.first().copied().unwrap_or_default();
        out.push((id.to_owned(), value, unit.to_owned(), how.to_owned()));
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
    let page = squeeze(&plain(html));
    for sentence in SCOPE {
        if !page.contains(sentence) {
            problems.push(format!("the page lost the scope of the run: {sentence:?}"));
        }
    }
    if page.contains("nothing on the storage read path changed") {
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
    let record = BLOCK_TEXT.concat();
    if text != record {
        let at = text
            .char_indices()
            .zip(record.chars())
            .find(|((_, a), b)| a != b)
            .map_or(text.len().min(record.len()), |((i, _), _)| i);
        let from = text[..at]
            .char_indices()
            .rev()
            .nth(40)
            .map_or(0, |(i, _)| i);
        problems.push(format!(
            "the attempt-5 block differs from the record at {:?}",
            text[from..].chars().take(100).collect::<String>()
        ));
    }
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
    // Each value sits in its arm's column: the cells of each body row, in order, are the record's.
    let rows: Vec<Vec<Option<&str>>> = (0..inner_tags.len().saturating_sub(1))
        .filter(|&k| {
            let (row, first) = (&inner_tags[k], &inner_tags[k + 1]);
            row.name == "tr" && !row.closing && first.name == "td" && !first.closing
        })
        .map(|k| {
            inner_tags[k + 1..]
                .iter()
                .take_while(|t| !(t.name == "tr" && t.closing))
                .filter(|t| t.name == "td" && !t.closing)
                .map(|t| attr(t.attrs, "data-cell"))
                .collect()
        })
        .collect();
    let want: Vec<Vec<Option<&str>>> = ROW_CELLS.iter().map(|r| r.to_vec()).collect();
    if rows != want {
        problems.push(format!(
            "the attempt-5 table puts its cells in the columns {rows:?}, the record is {want:?}"
        ));
    }
    problems
}

/// The problems of the result file: a row missing, doubled, with another value or unit, or one
/// the run does not have (a wall-time difference or ratio among them).
fn row_problems(results: &Path) -> Vec<String> {
    let text = match std::fs::read_to_string(results.join(RESULTS)) {
        Ok(text) => text,
        Err(why) => return vec![format!("{RESULTS}: {why}")],
    };
    let rows = match rows_with_units(&text) {
        Ok(rows) => rows,
        Err(why) => return vec![format!("{RESULTS}: {why}")],
    };
    let mut problems = Vec::new();
    if !text
        .lines()
        .any(|l| l.starts_with('#') && l.contains(HEADER))
    {
        problems.push(format!("{RESULTS}: the header lost {HEADER:?}"));
    }
    for (id, how) in HOWS {
        if let Some(row) = rows.iter().find(|r| r.0 == *id)
            && row.3 != *how
        {
            problems.push(format!(
                "{RESULTS}: {id} says it was taken as {:?}, the record is {how:?}",
                row.3
            ));
        }
    }
    for (id, value, unit) in ROWS {
        match rows.iter().filter(|r| r.0 == *id).collect::<Vec<_>>()[..] {
            [row] => {
                let same = (row.1 - value).abs() < 1e-9;
                if !same {
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
    for (id, _, _, _) in &rows {
        if !ROWS.iter().any(|r| r.0 == id) {
            problems.push(format!(
                "{RESULTS}: {id} is a row the run does not have (no wall-time difference or ratio is one)"
            ));
        }
    }
    problems
}
