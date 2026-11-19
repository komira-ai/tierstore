//! `c6`: the size classes, the summary of a synthetic listing, the dates and the verb's files.

use tierstore_testkit::dir::TestDir;

use crate::du::{Entry, Filesystem};
use crate::table::{Table, bytes};

use super::*;

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;
const D0: u64 = 20_770;

/// A writer that refuses everything.
struct Refuses;

impl Write for Refuses {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("refused"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn model() -> Model {
    Model::new(12, 8, 4).unwrap()
}

/// A time on day `D0 + day`, `n` milliseconds in.
fn at(day: u64, n: u64) -> u64 {
    (D0 + day) * DAY_MS + n
}

/// The daemon-form pack key numbered `n`.
fn pk(n: u64) -> String {
    format!("cas/d/n1/b1/{n:016x}")
}

fn obj(bucket: &str, key: &str, size: u64, modified_ms: u64, etag: &str) -> Object {
    Object {
        bucket: bucket.to_owned(),
        key: key.to_owned(),
        size,
        modified_ms,
        etag: etag.to_owned(),
    }
}

/// Deployment prefixes are labelled by bytes, ties by name, after the keys with no prefix; a
/// prefix's own name is not in the summary.
#[test]
fn prefixes_are_labelled_by_bytes_after_the_unprefixed_keys() {
    let c = |bytes| Count { objects: 1, bytes };
    let prefixes = BTreeMap::from([
        (("b", "zeta/"), c(5)),
        (("b", "alpha/"), c(5)),
        (("a", ""), c(1)),
        (("b", ""), c(2)),
        (("a", "big/"), c(9)),
    ]);
    let names: Vec<String> = label_prefixes(prefixes)
        .into_iter()
        .map(|n| n.name)
        .collect();
    assert_eq!(
        names,
        [
            "a/",
            "b/",
            "a/deployment 1",
            "b/deployment 2",
            "b/deployment 3"
        ]
    );
    assert!(label_prefixes(BTreeMap::new()).is_empty());
}

/// Twelve objects across the size-class boundaries, two buckets, two prefixes, two multipart
/// objects and days 0, 1, 2, 5 and 10 of the span (2026-11-13 to 2026-11-23).
fn listing() -> Vec<Object> {
    let h = "0000000000000041";
    vec![
        obj("cas-store", &pk(1), 64 * KIB, at(0, 0), "a"),
        obj("cas-store", &pk(2), 64 * KIB + 1, at(0, 1), "a"),
        obj("cas-store", &pk(3), MIB, at(1, 2), "b-3"),
        obj("cas-store", &pk(4), MIB + 1, at(1, 3), "a"),
        obj("cas-store", &pk(5), 4 * MIB, at(2, 4), "a"),
        obj("cas-store", &pk(6), 4 * MIB + 1, at(10, 6), "a"),
        obj("cas-store", &pk(7), 16 * MIB, at(10, 7), "a"),
        obj("cas-store", &pk(8), 16 * MIB + 1, at(10, 8), "a"),
        obj("cas-store", &pk(9), 256 * MIB, at(10, 9), "c-12"),
        obj("cas-store", &pk(10), 256 * MIB + 1, at(10, 10), "a"),
        obj("store-b", &format!("team1/cas/{h}/{h}"), 10, at(10, 11), "a"),
        obj("store-b", "history/history/x", 5, at(5, 12), "a"),
    ]
}

fn sizes() -> Vec<u64> {
    listing().iter().map(|o| o.size).collect()
}

fn summary() -> Summary {
    summarize(&listing(), None, &[], model()).0
}

/// The table whose title starts with `title`.
fn table(tables: &[Table], title: &str) -> String {
    tables
        .iter()
        .map(Table::markdown)
        .find(|m| m.starts_with(&format!("**{title}")))
        .unwrap_or_else(|| panic!("no table {title}"))
}

/// Every boundary of the size classes is inclusive: 64 KiB is in the first class and 64 KiB + 1
/// in the second, and so on up to 256 MiB; anything larger is the last.
#[test]
fn size_class_boundaries_are_inclusive() {
    let cases = [
        (0, 0),
        (64 * KIB, 0),
        (64 * KIB + 1, 1),
        (MIB, 1),
        (MIB + 1, 2),
        (4 * MIB, 2),
        (4 * MIB + 1, 3),
        (16 * MIB, 3),
        (16 * MIB + 1, 4),
        (256 * MIB, 4),
        (256 * MIB + 1, 5),
        (u64::MAX, 5),
    ];
    for (size, class) in cases {
        assert_eq!(size_class(size), class, "{size}");
    }
}

/// Days since the epoch print as UTC dates, across a leap day, a year end and a month of two
/// digits.
#[test]
fn dates_print_as_utc_days() {
    let cases = [
        (20_697, "2026-09-01"),
        (20_770, "2026-11-13"),
        (20_818, "2026-12-31"),
        (20_819, "2027-01-01"),
        (21_243, "2028-02-29"),
        (21_244, "2028-03-01"),
    ];
    for (day, text) in cases {
        assert_eq!(date(day), text, "{day}");
    }
}

/// A ratio is the part over the whole, and zero over nothing.
#[test]
fn a_ratio_of_nothing_is_zero() {
    assert!((ratio(1, 4) - 0.25).abs() < 1e-12);
    assert!(ratio(1, 0).abs() < 1e-12);
}

/// Objects count once in their kind, size class, bucket and prefix, with their bytes; the
/// multipart ones count apart; `L_silo` is every byte.
#[test]
fn objects_are_counted_by_kind_class_bucket_and_prefix() {
    let s = summary();
    assert_eq!(s.objects, 12);
    assert_eq!(s.l_silo, sizes().iter().sum::<u64>());
    let kinds: Vec<Kind> = s.kinds.iter().map(|k| k.kind).collect();
    assert_eq!(kinds, Kind::ALL);
    let pack = &s.kinds[0];
    assert_eq!(pack.total.objects, 11);
    let per_class: Vec<u64> = pack.classes.iter().map(|c| c.objects).collect();
    assert_eq!(per_class, [2, 2, 2, 2, 2, 1]);
    assert_eq!(pack.classes[5].bytes, 256 * MIB + 1);
    assert_eq!(pack.classes[0].bytes, 64 * KIB + 10);
    let other = &s.kinds[3];
    assert_eq!((other.total.objects, other.total.bytes), (1, 5));
    assert_eq!(other.classes[0].objects, 1);
    assert_eq!(s.kinds[1].total.objects + s.kinds[2].total.objects, 0);
    let cas_bytes: u64 = sizes()[..10].iter().sum();
    let named: Vec<(&str, u64, u64)> = s
        .buckets
        .iter()
        .map(|n| (n.name.as_str(), n.count.objects, n.count.bytes))
        .collect();
    assert_eq!(named, [("cas-store", 10, cas_bytes), ("store-b", 2, 15)]);
    let named: Vec<(&str, u64, u64)> = s
        .prefixes
        .iter()
        .map(|n| (n.name.as_str(), n.count.objects, n.count.bytes))
        .collect();
    assert_eq!(
        named,
        [
            ("cas-store/", 10, cas_bytes),
            ("store-b/deployment 1", 1, 10),
            ("store-b/deployment 2", 1, 5)
        ]
    );
    assert_eq!(
        s.multipart,
        Count {
            objects: 2,
            bytes: MIB + 256 * MIB
        }
    );
}

/// The span runs from the oldest to the newest object, per day with objects; the last 7 days of
/// the span are days 4 to 10 here, holding the object of day 5 and those of day 10.
#[test]
fn ingest_is_counted_per_day_and_over_the_last_week_of_the_span() {
    let s = summary();
    assert_eq!(s.ingest.first_ms, Some(at(0, 0)));
    assert_eq!(s.ingest.last_ms, Some(at(10, 11)));
    let per_day: Vec<(&str, u64)> = s
        .ingest
        .per_day
        .iter()
        .map(|d| (d.date.as_str(), d.count.objects))
        .collect();
    assert_eq!(
        per_day,
        [
            ("2026-11-13", 2),
            ("2026-11-14", 2),
            ("2026-11-15", 1),
            ("2026-11-18", 1),
            ("2026-11-23", 6)
        ]
    );
    let recent: Vec<&str> = s
        .ingest
        .recent_days
        .iter()
        .map(|d| d.date.as_str())
        .collect();
    assert_eq!(recent, ["2026-11-18", "2026-11-23"]);
    let want: u64 = sizes()[5..].iter().sum();
    assert_eq!(
        s.ingest.recent_total,
        Count {
            objects: 7,
            bytes: want
        }
    );
}

/// An empty listing has no span, no days and no objects.
#[test]
fn an_empty_listing_has_no_span() {
    let (s, dead) = summarize(&[], None, &[], model());
    assert_eq!((s.objects, s.l_silo), (0, 0));
    assert_eq!(s.ingest, Ingest::default());
    assert!(dead.is_empty());
    assert_eq!(s.large_min.len(), 4);
    assert!(s.large_min[0].per_day.is_empty());
}

/// Each `large_min` candidate splits the objects at or below it from those above it, in all
/// and per day.
#[test]
fn each_large_min_candidate_splits_the_objects() {
    let s = summary();
    let all = sizes();
    for (m, threshold) in s.large_min.iter().zip([256 * KIB, MIB, 4 * MIB, 16 * MIB]) {
        assert_eq!(m.threshold, threshold);
        let below: Vec<u64> = all.iter().copied().filter(|x| *x <= threshold).collect();
        let above: Vec<u64> = all.iter().copied().filter(|x| *x > threshold).collect();
        assert_eq!(
            m.at_or_below,
            Count {
                objects: below.len() as u64,
                bytes: below.iter().sum()
            }
        );
        assert_eq!(
            m.above,
            Count {
                objects: above.len() as u64,
                bytes: above.iter().sum()
            }
        );
    }
    let counts: Vec<u64> = s.large_min.iter().map(|m| m.at_or_below.objects).collect();
    assert_eq!(counts, [4, 5, 7, 9]);
    let four = &s.large_min[2];
    let days: Vec<(&str, u64, u64)> = four
        .per_day
        .iter()
        .map(|d| (d.date.as_str(), d.at_or_below.objects, d.above.objects))
        .collect();
    assert_eq!(
        days,
        [
            ("2026-11-13", 2, 0),
            ("2026-11-14", 2, 0),
            ("2026-11-15", 1, 0),
            ("2026-11-18", 1, 0),
            ("2026-11-23", 1, 5)
        ]
    );
}

/// The objects above 4 MiB are the ones packed, five here, none of 400 to 700 MiB; there is
/// a fit for each block size and window.
#[test]
fn the_objects_above_four_mib_are_the_ones_packed() {
    let p = summary().packing;
    assert_eq!(p.large_min, 4 * MIB);
    assert_eq!(p.objects.objects, 5);
    assert_eq!(p.objects.bytes, sizes()[5..10].iter().sum::<u64>());
    assert_eq!(p.mid, Count::default());
    assert_eq!(p.fits.len(), 12);
}

/// Objects are placed in arrival order, by time and then by key and not by key alone: sixteen
/// 100 MiB objects and a 200 MiB one written last (though its key sorts first) make a window
/// of sixteen and a window of one, nine blocks of 256 MiB; written first they would make ten.
#[test]
fn objects_arrive_by_time_then_key() {
    let mut objects: Vec<Object> = (1..=16)
        .map(|n| obj("cas-store", &pk(n), 100 * MIB, at(0, 1), "a"))
        .collect();
    objects.push(obj("cas-store", &pk(0), 200 * MIB, at(0, 2), "a"));
    let blocks = |objects: &[Object]| {
        let p = summarize(objects, None, &[], model()).0.packing;
        p.fits
            .iter()
            .find(|f| f.block_bytes == 256 * MIB && f.window == 16)
            .map(|f| f.blocks)
    };
    assert_eq!(blocks(&objects), Some(9));
    objects[16].modified_ms = at(0, 0);
    assert_eq!(blocks(&objects), Some(10));
}

/// Footers and du reports given to the summary reach `L_live`, the tree check and the gap.
#[test]
fn footers_and_du_reports_reach_the_summary() {
    let listing = [
        obj("cas-store", &pk(1), 1000, at(0, 1), "a"),
        obj("cas-store", &pk(2), 2000, at(0, 2), "a"),
    ];
    let blob = hex::encode([7u8; 32]);
    let text = format!(
        "P\tcas-store\t{}\tok\t1\nB\t{blob}\t10\nP\tcas-store\t{}\tok\t1\nB\t{blob}\t10\n\
         T\tcas-store\tcas/l/x\tjson\n",
        pk(1),
        pk(2)
    );
    let footers = live::parse_footers(text.as_bytes(), Path::new("f")).unwrap();
    let report = DuReport {
        root: "/d".to_owned(),
        depth: 1,
        total: Entry {
            path: ".".to_owned(),
            allocated: 500,
            ..Entry::default()
        },
        filesystem: Filesystem::default(),
        entries: Vec::new(),
    };
    let (s, dead) = summarize(&listing, Some(&footers), &[report], model());
    assert_eq!(s.live.l_live, Some(1000));
    assert_eq!(s.trees.checked, 1);
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].key, pk(2));
    assert_eq!(s.gap.measured.unwrap().total.allocated, 6000);
}

/// The dead packs are written one `bucket TAB key` per line; an unwritable path is an error.
#[test]
fn dead_packs_are_written_one_per_line() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-c6-dead");
    let path = dir.path().join("dead.tsv");
    let dead = [
        DeadPack {
            bucket: "b1".to_owned(),
            key: "k1".to_owned(),
        },
        DeadPack {
            bucket: "b2".to_owned(),
            key: "k2".to_owned(),
        },
    ];
    write_dead(&path, &dead).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "b1\tk1\nb2\tk2\n");
    let e = write_dead(&dir.path().join("no/dead.tsv"), &dead).unwrap_err();
    assert!(matches!(e, Error::Io { what: "write", .. }), "{e}");
}

/// The tables of a summary with footers, du reports and objects of every kind: their titles in
/// reading order, and the cells of the rows that carry the measurement.
#[test]
fn the_tables_carry_the_measurement() {
    let mut listing = listing();
    listing.push(obj(
        "cas-store",
        &format!("cas/l/{}", "ab".repeat(32)),
        100,
        at(3, 0),
        "a",
    ));
    let text = format!(
        "P\tcas-store\t{}\tok\t1\nB\t{}\t10\nT\tcas-store\tcas/l/x\tjson\n",
        pk(1),
        hex::encode([1u8; 32])
    );
    let footers = live::parse_footers(text.as_bytes(), Path::new("f")).unwrap();
    let e = |path: &str, allocated| Entry {
        path: path.to_owned(),
        files: 3,
        allocated,
        ..Entry::default()
    };
    let report = DuReport {
        root: "/d".to_owned(),
        depth: 2,
        total: e(".", 9000),
        filesystem: Filesystem {
            size: 1000,
            used: 600,
            free: 400,
        },
        entries: vec![
            e("cas-store", 5000),
            e(".minio.sys", 300),
            e(".minio.sys/tmp", 100),
        ],
    };
    let s = summarize(&listing, Some(&footers), &[report], model()).0;
    let tables = tables(&s);
    let titles: Vec<String> = tables
        .iter()
        .map(|t| t.markdown().lines().next().unwrap().to_owned())
        .collect();
    assert_eq!(titles.len(), 13);
    assert!(
        titles.iter().all(|t| t.starts_with("**C-6, ")),
        "{titles:?}"
    );
    let kinds = table(&tables, "C-6, silo's objects by kind and size class");
    assert!(
        kinds.contains("| kind | objects | bytes | <= 64 KiB |"),
        "{kinds}"
    );
    assert!(
        kinds.contains(&format!("| 1 ({}) |", bytes(256 * MIB + 1))),
        "{kinds}"
    );
    assert!(kinds.contains("| large blob | 0 | 0 B |"), "{kinds}");
    assert!(kinds.contains("| all | 13 |"), "{kinds}");
    let by_bucket = table(&tables, "C-6, silo's objects by bucket");
    assert!(
        by_bucket.contains("| store-b/deployment 1 | 1 | 10 B |"),
        "{by_bucket}"
    );
    let ingest = table(&tables, "C-6, silo's ingest");
    assert!(ingest.contains("| first object | 2026-11-13 |"), "{ingest}");
    assert!(ingest.contains("| last object | 2026-11-23 |"), "{ingest}");
    let live = table(&tables, "C-6, L_silo and L_live");
    assert!(live.contains("| packs listed | 11 ("), "{live}");
    assert!(
        live.contains("| packs without a footer (not read) | 10 ("),
        "{live}"
    );
    assert!(
        live.contains("| L_live, at most (unread packs counted live) | "),
        "{live}"
    );
    assert!(!live.contains("absent"), "{live}");
    assert!(live.contains("| packs with status ok | 1 |"), "{live}");
    let trees = table(&tables, "C-6, tree lists checked");
    assert!(trees.contains("| 1 | 1 | 0 | 0 |"), "{trees}");
    let large = table(&tables, "C-6, large_min");
    assert!(large.contains("| 4.19 MB | 8 ("), "{large}");
    let fit = table(&tables, "C-6, first-fit decreasing");
    assert!(fit.contains("| 268 MB | 16 |"), "{fit}");
    let gap = table(&tables, "C-6, the gap between");
    assert!(gap.contains("| erasure set | 12 drives, 8+4 |"), "{gap}");
    assert!(gap.contains("| drives measured | 1 |"), "{gap}");
    assert!(
        gap.contains("| of it, bucket directories | 60.0 kB |"),
        "{gap}"
    );
    assert!(
        gap.contains("| of it, other top-level entries | 3.60 kB |"),
        "{gap}"
    );
    assert!(
        gap.contains("| file systems, size / used / free | 12.0 kB / 7.20 kB / 4.80 kB |"),
        "{gap}"
    );
    let by_gap = table(&tables, "C-6, the gap by bucket");
    assert!(by_gap.contains("| cas-store | 11 |"), "{by_gap}");
    assert!(by_gap.contains("| store-b | 2 | 15 B |"), "{by_gap}");
    let others = table(&tables, "C-6, what silo's drives hold");
    assert!(
        others.contains("| .minio.sys/tmp | 1.20 kB | 36 |"),
        "{others}"
    );
}

/// Without footers or du reports the tables say so: no span, `L_live` absent without packs
/// scanned, nothing measured, and no table of entries outside the buckets.
#[test]
fn the_tables_of_an_empty_summary_say_nothing_was_measured() {
    let s = summarize(&[], None, &[], model()).0;
    let tables = tables(&s);
    let ingest = table(&tables, "C-6, silo's ingest");
    assert!(ingest.contains("| first object | none |"), "{ingest}");
    let live = table(&tables, "C-6, L_silo and L_live");
    assert!(
        live.contains("| L_live, at most (unread packs counted live) | 0 B |"),
        "{live}"
    );
    assert!(
        live.contains(
            "| L_live, at least (unread packs and the most they could make dead left out) | 0 B |"
        ),
        "{live}"
    );
    let gap = table(&tables, "C-6, the gap between");
    assert!(!gap.contains("measured allocated"), "{gap}");
    assert!(
        !tables
            .iter()
            .any(|t| t.markdown().contains("what silo's drives hold")),
        "without a du report the table of what the drives hold is left out"
    );
}

/// The files of a verb run: a listing of three packs, their footers and a du report.
struct Inputs {
    dir: TestDir,
    args: Args,
}

fn inputs(name: &str) -> Inputs {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let root = dir.path();
    let lines: Vec<String> = [(pk(1), 1000, 1), (pk(2), 2000, 2), (pk(3), 3000, 3)]
        .iter()
        .map(|(key, size, t)| format!("cas-store\t{key}\t{size}\t{t}\ta\n"))
        .collect();
    std::fs::write(root.join("listing.tsv"), lines.concat()).unwrap();
    let d = |n: u8| hex::encode([n; 32]);
    let footers = format!(
        "P\tcas-store\t{}\tok\t1\nB\t{}\t10\nP\tcas-store\t{}\tok\t1\nB\t{}\t10\n\
         P\tcas-store\t{}\tok\t1\nB\t{}\t20\n",
        pk(1),
        d(1),
        pk(2),
        d(1),
        pk(3),
        d(2)
    );
    std::fs::write(root.join("footers.tsv"), footers).unwrap();
    let report = DuReport {
        root: "/d".to_owned(),
        depth: 1,
        total: Entry {
            path: ".".to_owned(),
            allocated: 700,
            ..Entry::default()
        },
        filesystem: Filesystem::default(),
        entries: Vec::new(),
    };
    write_json(&root.join("du.json"), &report).unwrap();
    let args = Args {
        listing: root.join("listing.tsv"),
        footers: vec![root.join("footers.tsv")],
        du: vec![root.join("du.json")],
        erasure: Vec::new(),
        drives: 12,
        data_shards: 8,
        parity_shards: 4,
        dead_out: Some(root.join("dead.tsv")),
        out: root.join("summary.json"),
    };
    Inputs { dir, args }
}

/// `run` writes the summary, the dead packs and the tables to standard output.
#[test]
fn run_writes_the_summary_the_dead_packs_and_the_tables() {
    let Inputs { dir, args } = inputs("storebench-c6-run");
    let mut stdout = Vec::new();
    run(&args, &mut stdout).unwrap();
    let s: Summary = read_json(&args.out).unwrap();
    assert_eq!(s.objects, 3);
    assert_eq!(s.live.l_live, Some(1000 + 3000));
    assert_eq!(s.live.dead_packs, 1);
    assert_eq!(s.gap.measured.as_ref().unwrap().total.allocated, 8400);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("dead.tsv")).unwrap(),
        format!("cas-store\t{}\n", pk(2))
    );
    let text = String::from_utf8(stdout).unwrap();
    assert!(
        text.starts_with("**C-6, silo's objects by kind and size class"),
        "{text}"
    );
    assert!(
        text.contains("| L_live, at most (unread packs counted live) | 4.00 kB |"),
        "{text}"
    );
}

/// The summary written is the one the library computes; without footers or a dead file `run`
/// still works and `L_live` is absent.
#[test]
fn run_without_footers_leaves_l_live_absent() {
    let Inputs { dir: _dir, args } = inputs("storebench-c6-run-bare");
    let bare = Args {
        footers: Vec::new(),
        dead_out: None,
        du: Vec::new(),
        ..args
    };
    run(&bare, &mut Vec::new()).unwrap();
    let s: Summary = read_json(&bare.out).unwrap();
    assert_eq!(s.live.l_live, None);
    assert_eq!(s.live.packs_missing, 3);
    assert!(s.gap.measured.is_none());
}

/// `run` refuses a model without drives, a dead file without footers, an input that is
/// missing or malformed, and an output that cannot be written.
#[test]
fn run_fails_on_every_bad_input_and_output() {
    let Inputs { dir, args } = inputs("storebench-c6-run-fails");
    let root = dir.path();
    let run_with = |change: &dyn Fn(&mut Args)| {
        let mut a = Args {
            listing: args.listing.clone(),
            footers: args.footers.clone(),
            du: args.du.clone(),
            erasure: args.erasure.clone(),
            dead_out: args.dead_out.clone(),
            out: args.out.clone(),
            ..args
        };
        change(&mut a);
        run(&a, &mut Vec::new()).unwrap_err()
    };
    let e = run_with(&|a| a.drives = 0);
    assert!(matches!(e, Error::Usage(_)), "{e}");
    let e = run_with(&|a| a.footers = Vec::new());
    assert_eq!(
        e.to_string(),
        "--dead-out needs --footers: dead packs are found from the footers"
    );
    let e = run_with(&|a| a.listing = root.join("none.tsv"));
    assert!(matches!(e, Error::Io { what: "open", .. }), "{e}");
    std::fs::write(root.join("bad.tsv"), "cas-store\tonly-two-fields\n").unwrap();
    let e = run_with(&|a| a.listing = root.join("bad.tsv"));
    assert!(matches!(e, Error::Parse { line: 1, .. }), "{e}");
    let e = run_with(&|a| a.footers = vec![root.join("none.tsv")]);
    assert!(matches!(e, Error::Io { what: "open", .. }), "{e}");
    let e = run_with(&|a| a.du = vec![root.join("none.json")]);
    assert!(matches!(e, Error::Io { what: "read", .. }), "{e}");
    std::fs::write(root.join("bad.json"), "[]").unwrap();
    let e = run_with(&|a| a.du = vec![root.join("bad.json")]);
    assert!(matches!(e, Error::Parse { .. }), "{e}");
    let e = run_with(&|a| a.dead_out = Some(root.join("no/dead.tsv")));
    assert!(matches!(e, Error::Io { what: "write", .. }), "{e}");
    let e = run_with(&|a| a.out = root.join("no/summary.json"));
    assert!(matches!(e, Error::Io { what: "write", .. }), "{e}");
    let e = run_with(&|a| a.erasure = vec![root.join("none.tsv")]);
    assert!(matches!(e, Error::Io { what: "open", .. }), "{e}");
    let a = Args {
        listing: args.listing.clone(),
        footers: args.footers.clone(),
        du: args.du.clone(),
        dead_out: args.dead_out.clone(),
        out: args.out.clone(),
        ..args
    };
    let e = run(&a, &mut Refuses).unwrap_err();
    assert!(matches!(e, Error::Io { what: "write", .. }), "{e}");
}

/// With erasure facts the summary costs the stated objects at their layout and counts them.
#[test]
fn run_with_erasure_facts_counts_the_stated_layouts() {
    let Inputs { dir, mut args } = inputs("storebench-c6-run-erasure");
    let first = listing::read(&args.listing).unwrap().remove(0);
    let facts = dir.path().join("erasure.tsv");
    std::fs::write(
        &facts,
        format!("{}\t{}\t7\t5\t1\t0\t1\n", first.bucket, first.key),
    )
    .unwrap();
    args.erasure = vec![facts];
    run(&args, &mut Vec::new()).unwrap();
    let summary: Summary =
        serde_json::from_str(&std::fs::read_to_string(&args.out).unwrap()).unwrap();
    assert_eq!(summary.gap.facts, 1);
    assert_eq!(
        (summary.gap.layouts[0].data, summary.gap.layouts[0].parity),
        (7, 5)
    );
}
