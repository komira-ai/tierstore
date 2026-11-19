//! The tables of the measurement document, made from a [`Summary`].

use crate::table::{Table, bytes, count, num, pct};

use super::{CLASS_NAMES, Count, Day, Summary, date};

/// `objects (bytes)` of a count, as a cell.
fn cell(c: &Count) -> String {
    format!("{} ({})", count(c.objects), bytes(c.bytes))
}

/// The date of a time in milliseconds since the epoch.
fn when(ms: Option<u64>) -> String {
    ms.map_or_else(|| "none".to_owned(), |ms| date(ms / super::DAY_MS))
}

/// A byte count's cell, or `absent`.
fn maybe_bytes(b: Option<u64>) -> String {
    b.map_or_else(|| "absent".to_owned(), bytes)
}

/// Rows of days: the date and the count.
fn day_table(title: &str, days: &[Day]) -> Table {
    let mut t = Table::new(title, &["date (UTC)", "objects", "bytes"]);
    for d in days {
        t.row(vec![
            d.date.clone(),
            count(d.count.objects),
            bytes(d.count.bytes),
        ]);
    }
    t
}

/// The tables of the C-6 section of the measurement document, in reading order.
#[must_use]
pub fn tables(s: &Summary) -> Vec<Table> {
    let mut out = Vec::new();

    let mut columns = vec!["kind", "objects", "bytes"];
    columns.extend(CLASS_NAMES);
    let mut t = Table::new(
        "C-6, silo's objects by kind and size class (objects and bytes)",
        &columns,
    );
    for k in &s.kinds {
        let mut row = vec![
            k.kind.name().to_owned(),
            count(k.total.objects),
            bytes(k.total.bytes),
        ];
        row.extend(k.classes.iter().map(cell));
        t.row(row);
    }
    t.row(vec!["all".to_owned(), count(s.objects), bytes(s.l_silo)]);
    out.push(t);

    let mut t = Table::new(
        "C-6, silo's objects by bucket and deployment prefix",
        &["bucket or bucket/prefix", "objects", "bytes"],
    );
    for n in s.buckets.iter().chain(&s.prefixes) {
        t.row(vec![
            n.name.clone(),
            count(n.count.objects),
            bytes(n.count.bytes),
        ]);
    }
    out.push(t);

    let mut t = Table::new("C-6, silo's ingest", &["measure", "value"]);
    t.row(vec!["first object".to_owned(), when(s.ingest.first_ms)]);
    t.row(vec!["last object".to_owned(), when(s.ingest.last_ms)]);
    t.row(vec!["multipart objects".to_owned(), cell(&s.multipart)]);
    t.row(vec![
        "last 7 days of the span".to_owned(),
        cell(&s.ingest.recent_total),
    ]);
    out.push(t);
    out.push(day_table(
        "C-6, ingest per day, last 7 days of the span",
        &s.ingest.recent_days,
    ));
    out.push(day_table(
        "C-6, ingest per day, whole span",
        &s.ingest.per_day,
    ));

    let l = &s.live;
    let mut t = Table::new("C-6, L_silo and L_live", &["measure", "value"]);
    let rows = [
        ("L_silo", bytes(s.l_silo)),
        (
            "packs listed",
            format!("{} ({})", count(l.packs_listed), bytes(l.pack_bytes)),
        ),
        (
            "packs without a footer (not read)",
            format!(
                "{} ({}, the largest {})",
                count(l.packs_missing),
                bytes(l.missing_bytes),
                bytes(l.missing_max)
            ),
        ),
        ("packs scanned", count(l.packs_scanned)),
        ("packs ok", count(l.packs_ok)),
        (
            "packs bad (footer not read: counted as unread)",
            format!("{} ({})", count(l.packs_bad), bytes(l.bad_bytes)),
        ),
        ("blobs in ok packs", count(l.blobs)),
        ("distinct digests", count(l.distinct_digests)),
        ("distinct blob bytes", bytes(l.distinct_bytes)),
        (
            "live packs",
            format!("{} ({})", count(l.live_packs), bytes(l.live_bytes)),
        ),
        (
            "dead packs",
            format!("{} ({})", count(l.dead_packs), bytes(l.dead_bytes)),
        ),
        (
            "dead share of pack bytes (%)",
            pct(l.dead_bytes, l.live_bytes + l.dead_bytes),
        ),
        ("large blob bytes", bytes(l.large_blob_bytes)),
        ("tree list bytes", bytes(l.tree_list_bytes)),
        (
            "live packs whose first blobs could be in an unread pack",
            format!(
                "{} ({})",
                count(l.uncertain_packs),
                bytes(l.uncertain_bytes)
            ),
        ),
        (
            "of them, the most the unread packs' bytes could make dead",
            bytes(l.could_be_dead_bytes),
        ),
        (
            "L_live, at most (unread packs counted live)",
            maybe_bytes(l.l_live),
        ),
        (
            "L_live, at least (unread packs and the most they could make dead left out)",
            maybe_bytes(l.l_live_low),
        ),
    ];
    for (name, value) in rows {
        t.row(vec![name.to_owned(), value]);
    }
    for st in &l.statuses {
        t.row(vec![
            format!("packs with status {}", st.status),
            count(st.packs),
        ]);
    }
    out.push(t);

    let mut t = Table::new(
        "C-6, tree lists checked",
        &["checked", "json", "not json", "errors"],
    );
    t.row(vec![
        count(s.trees.checked),
        count(s.trees.json),
        count(s.trees.not_json),
        count(s.trees.errors),
    ]);
    out.push(t);

    let mut t = Table::new(
        "C-6, large_min: objects at or below (segment records) and above (whole files)",
        &["large_min", "at or below", "above"],
    );
    for m in &s.large_min {
        t.row(vec![
            bytes(m.threshold),
            cell(&m.at_or_below),
            cell(&m.above),
        ]);
    }
    out.push(t);

    let p = &s.packing;
    let mut t = Table::new(
        "C-6, first-fit decreasing over the objects above large_min",
        &[
            "block",
            "window",
            "blocks",
            "chain blocks",
            "chains",
            "data",
            "mean fill (%)",
            "fill without chains (%)",
        ],
    );
    for f in &p.fits {
        t.row(vec![
            bytes(f.block_bytes),
            f.window.to_string(),
            count(f.blocks),
            count(f.chain_blocks),
            count(f.chains),
            bytes(f.data_bytes),
            num(f.mean_fill * 100.0, 1),
            num(f.packed_fill * 100.0, 1),
        ]);
    }
    out.push(t);
    let mut t = Table::new(
        "C-6, objects above large_min and of 400 to 700 MiB",
        &["objects", "above large_min", "of 400 to 700 MiB"],
    );
    t.row(vec![
        "count".to_owned(),
        count(p.objects.objects),
        count(p.mid.objects),
    ]);
    t.row(vec![
        "bytes".to_owned(),
        bytes(p.objects.bytes),
        bytes(p.mid.bytes),
    ]);
    out.push(t);

    let g = &s.gap;
    let mut t = Table::new(
        "C-6, the gap between silo's drives and its objects",
        &["measure", "value"],
    );
    t.row(vec![
        "erasure set".to_owned(),
        format!("{} drives, {}+{}", g.drives, g.data_shards, g.parity_shards),
    ]);
    t.row(vec!["L_silo".to_owned(), bytes(g.l_silo)]);
    t.row(vec![
        "expected raw (model)".to_owned(),
        format!(
            "{} in {} files",
            bytes(g.expected.bytes),
            count(g.expected.files)
        ),
    ]);
    t.row(vec![
        "expected raw / L_silo".to_owned(),
        num(g.expected_ratio, 2),
    ]);
    t.row(vec!["drives measured".to_owned(), g.du_files.to_string()]);
    if let Some(m) = &g.measured {
        t.row(vec![
            "measured allocated, all drives".to_owned(),
            format!(
                "{} in {} files",
                bytes(m.total.allocated),
                count(m.total.files)
            ),
        ]);
        t.row(vec![
            "of it, bucket directories".to_owned(),
            bytes(m.buckets_allocated),
        ]);
        t.row(vec![
            "of it, other top-level entries".to_owned(),
            bytes(m.others_allocated),
        ]);
        t.row(vec![
            "measured raw / L_silo".to_owned(),
            num(m.ratio_total, 2),
        ]);
        t.row(vec![
            "bucket directories / L_silo".to_owned(),
            num(m.ratio_buckets, 2),
        ]);
        t.row(vec![
            "file systems, size / used / free".to_owned(),
            format!(
                "{} / {} / {}",
                bytes(m.filesystem.size),
                bytes(m.filesystem.used),
                bytes(m.filesystem.free)
            ),
        ]);
    }
    out.push(t);

    if !g.layouts.is_empty() {
        let mut t = Table::new(
            "C-6, objects by the erasure layout their xl.meta states",
            &[
                "data + parity",
                "objects",
                "bytes",
                "share of bytes",
                "inlined objects",
            ],
        );
        for l in &g.layouts {
            t.row(vec![
                format!("{}+{}", l.data, l.parity),
                count(l.objects),
                bytes(l.bytes),
                pct(l.bytes, g.l_silo),
                count(l.inline),
            ]);
        }
        out.push(t);
    }

    let mut t = Table::new(
        "C-6, the gap by bucket",
        &[
            "bucket",
            "objects",
            "bytes",
            "expected raw",
            "measured allocated",
        ],
    );
    for b in &g.buckets {
        t.row(vec![
            b.bucket.clone(),
            count(b.objects),
            bytes(b.logical),
            bytes(b.expected.bytes),
            maybe_bytes(b.measured_allocated),
        ]);
    }
    out.push(t);

    // Without a du report nothing outside the buckets was measured, and the table is left out.
    if let Some(m) = &g.measured {
        let mut t = Table::new(
            "C-6, what silo's drives hold outside the bucket directories",
            &["entry", "allocated", "files"],
        );
        for e in &m.others {
            t.row(vec![e.path.clone(), bytes(e.allocated), count(e.files)]);
        }
        out.push(t);
    }
    out
}
