//! The gap: the cost model of silo's erasure coding and the extrapolation of measured drives.

use super::*;
use crate::table::Table;

const MIB: u64 = 1 << 20;

fn model() -> Model {
    Model::new(12, 8, 4).unwrap()
}

fn object(bucket: &str, size: u64) -> Object {
    Object {
        bucket: bucket.to_owned(),
        key: format!("k{size}"),
        size,
        modified_ms: 0,
        etag: "e".to_owned(),
    }
}

fn entry(path: &str, files: u64, allocated: u64) -> Entry {
    Entry {
        path: path.to_owned(),
        files,
        dirs: 1,
        apparent: allocated / 2,
        allocated,
        errors: 0,
    }
}

fn report(entries: Vec<Entry>, total: u64, fs: (u64, u64, u64)) -> DuReport {
    DuReport {
        root: "/d".to_owned(),
        depth: 2,
        total: entry(".", 100, total),
        filesystem: Filesystem {
            size: fs.0,
            used: fs.1,
            free: fs.2,
        },
        entries,
    }
}

/// An object whose shard is inlined costs one file per drive, its metadata, shard and bitrot
/// rounded up to 4 KiB: 8000 bytes -> 1000-byte shards -> 4096 per drive on 12 drives.
#[test]
fn an_inlined_object_costs_one_block_file_per_drive() {
    let raw = model().object(8000);
    assert_eq!(
        raw,
        Raw {
            bytes: 12 * 4096,
            files: 12
        }
    );
}

/// A 0-byte object is an `xl.meta` of one block on every drive.
#[test]
fn a_zero_byte_object_costs_one_block_per_drive() {
    assert_eq!(
        model().object(0),
        Raw {
            bytes: 12 * 4096,
            files: 12
        }
    );
}

/// An 80 MiB object has 10 MiB shards, 80 started erasure blocks of 32 bytes of bitrot, an
/// `xl.meta` of 4 KiB and a part file of 10 MiB + 2560 bytes rounded up to 4 KiB, on 12 drives.
#[test]
fn a_larger_object_costs_an_xl_meta_and_a_part_file_per_drive() {
    let raw = model().object(80 * MIB);
    let per_drive = 4096 + (10 * MIB + 2560).next_multiple_of(4096);
    assert_eq!(per_drive, 10_493_952);
    assert_eq!(
        raw,
        Raw {
            bytes: 12 * per_drive,
            files: 24
        }
    );
    assert_eq!(raw.bytes, 125_927_424);
}

/// A shard of exactly 128 KiB is inlined; a byte more in the object makes it 128 KiB + 1 and
/// not inlined.
#[test]
fn the_inline_threshold_is_a_shard_of_128_kib_inclusive() {
    let at = model().object(8 * 128 * 1024);
    assert_eq!(
        at,
        Raw {
            bytes: 12 * 135_168,
            files: 12
        }
    );
    let over = model().object(8 * 128 * 1024 + 1);
    assert_eq!(
        over,
        Raw {
            bytes: 12 * (4096 + 135_168),
            files: 24
        }
    );
}

/// A model needs a drive and a data shard; parity may be zero.
#[test]
fn a_model_without_drives_or_data_shards_is_refused() {
    assert!(matches!(Model::new(0, 8, 4), Err(Error::Usage(_))));
    assert!(matches!(Model::new(12, 0, 4), Err(Error::Usage(_))));
    let plain = Model::new(4, 4, 0).unwrap();
    assert_eq!(plain.object(0).files, 4);
}

/// Sums scale by drives over files given, never overflow and never divide by zero.
#[test]
fn extrapolation_scales_by_drives_over_files_given() {
    assert_eq!(extrapolate(10, 12, 4), 30);
    assert_eq!(extrapolate(u64::MAX, 12, 1), u64::MAX);
    assert_eq!(extrapolate(5, 12, 0), 60);
}

/// Expected cost per bucket and in all, and the measured entries of two drives summed and
/// scaled to 12: the bucket directory, the other top-level entries (and their children), and
/// nothing counted twice for what lies under a bucket directory.
#[test]
fn the_gap_sets_the_modelled_cost_against_the_measured_drives() {
    let listing = vec![
        object("kbf-cas", 8000),
        object("kbf-cas", 80 * MIB),
        object("kbf-cas", 0),
        object("other-b", MIB),
    ];
    let one = report(
        vec![
            entry("kbf-cas", 10, 1000),
            entry("kbf-cas/ab", 5, 900),
            entry(".minio.sys", 3, 200),
            entry(".minio.sys/tmp", 1, 50),
        ],
        1500,
        (1000, 400, 600),
    );
    let two = report(
        vec![entry("kbf-cas", 20, 3000), entry(".minio.sys", 3, 400)],
        4000,
        (1000, 500, 500),
    );
    let gap = analyze(&listing, &[one, two], model());
    let l_silo = 8000 + 80 * MIB + MIB;
    assert_eq!(gap.l_silo, l_silo);
    assert_eq!(
        gap.expected,
        Raw {
            bytes: 49_152 + 125_927_424 + 49_152 + 1_622_016,
            files: 60
        }
    );
    assert!((gap.expected_ratio - 127_647_744.0 / l_silo as f64).abs() < 1e-12);
    assert_eq!((gap.drives, gap.data_shards, gap.parity_shards), (12, 8, 4));
    assert_eq!(gap.du_files, 2);
    let names: Vec<&str> = gap.buckets.iter().map(|b| b.bucket.as_str()).collect();
    assert_eq!(names, ["kbf-cas", "other-b"]);
    assert_eq!(gap.buckets[0].objects, 3);
    assert_eq!(gap.buckets[0].logical, 8000 + 80 * MIB);
    assert_eq!(gap.buckets[0].expected.files, 48);
    assert_eq!(gap.buckets[0].measured_allocated, Some(24_000));
    assert_eq!(gap.buckets[1].measured_allocated, None);
    assert_eq!(gap.buckets[1].expected.bytes, 1_622_016);
    let measured = gap.measured.unwrap();
    assert_eq!(measured.total.allocated, 33_000);
    assert_eq!(measured.total.files, 1200);
    assert_eq!(measured.buckets_allocated, 24_000);
    assert_eq!(measured.others_allocated, 3600);
    let others: Vec<(&str, u64)> = measured
        .others
        .iter()
        .map(|e| (e.path.as_str(), e.allocated))
        .collect();
    assert_eq!(others, [(".minio.sys", 3600), (".minio.sys/tmp", 300)]);
    assert_eq!(
        measured.filesystem,
        Filesystem {
            size: 12_000,
            used: 5400,
            free: 6600
        }
    );
    assert!((measured.ratio_total - 33_000.0 / l_silo as f64).abs() < 1e-12);
    assert!((measured.ratio_buckets - 24_000.0 / l_silo as f64).abs() < 1e-12);
}

/// Without du reports the expected cost is still modelled and nothing is measured.
#[test]
fn without_du_reports_nothing_is_measured() {
    let gap = analyze(&[object("kbf-cas", 8000)], &[], model());
    assert_eq!(gap.du_files, 0);
    assert!(gap.measured.is_none());
    assert_eq!(gap.buckets[0].measured_allocated, None);
    assert_eq!(gap.expected.bytes, 49_152);
}

/// An empty listing has no objects, no cost and a ratio of zero.
#[test]
fn an_empty_listing_has_no_expected_cost() {
    let gap = analyze(&[], &[], model());
    assert_eq!(gap.l_silo, 0);
    assert_eq!(gap.expected, Raw::default());
    assert!(gap.expected_ratio.abs() < 1e-12);
    assert!(gap.buckets.is_empty());
}

/// An object whose `xl.meta` states 7 + 5 costs 12 shards of `ceil(size / 7)`, inlined or not as
/// stated; an object without facts is costed at the model's 8 + 4; the layouts count both.
#[test]
fn stated_erasure_facts_cost_an_object_at_its_own_layout() {
    let a = object("b", 80 * MIB);
    let small = object("b", 300);
    let plain = object("b", 7);
    let mut facts = ErasureFacts::new();
    let seven = Erasure {
        data: 7,
        parity: 5,
        inline: false,
    };
    facts.insert(("b".into(), a.key.clone()), seven);
    facts.insert(
        ("b".into(), small.key.clone()),
        Erasure {
            data: 4,
            parity: 2,
            inline: true,
        },
    );
    let m = model();
    let shard = (80 * MIB).div_ceil(7);
    let bitrot = 80 * BITROT_BYTES;
    let per_drive = XL_META_FILE + (shard + bitrot).next_multiple_of(FS_BLOCK);
    assert_eq!(
        m.object_as(a.size, Some(&seven)),
        Raw {
            bytes: 12 * per_drive,
            files: 24
        }
    );
    assert_eq!(m.object_as(plain.size, None), m.object(plain.size));
    let gap = analyze_with(
        &[a.clone(), small.clone(), plain.clone()],
        &[],
        m,
        Some(&facts),
    );
    assert_eq!(gap.facts, 2);
    assert_eq!(
        gap.layouts,
        [
            Layout {
                data: 4,
                parity: 2,
                objects: 1,
                bytes: 300,
                inline: 1
            },
            Layout {
                data: 7,
                parity: 5,
                objects: 1,
                bytes: 80 * MIB,
                inline: 0
            },
        ]
    );
    let inline_cost = 6 * (META_BYTES + 75 + BITROT_BYTES).next_multiple_of(FS_BLOCK);
    assert_eq!(
        gap.expected.bytes,
        12 * per_drive + inline_cost + m.object(7).bytes
    );
    assert_eq!(analyze(&[plain], &[], m).facts, 0);
}

/// Erasure-fact files are read line by line; a line that is not one, a shard count that is not
/// a number or zero data shards are refused with the line; a missing file is an I/O error.
#[test]
fn erasure_fact_files_are_read_and_bad_lines_refused() {
    let dir = tierstore_testkit::dir::TestDir::under(std::env::temp_dir(), "storebench-erasure");
    let good = dir.path().join("m.tsv");
    std::fs::write(
        &good,
        "kbf-cas\tk\t7\t5\t1\t0\t1\nkbf-cas\tj\t8\t4\t1\t1\t1\n",
    )
    .unwrap();
    let facts = read_erasure(std::slice::from_ref(&good)).unwrap();
    assert_eq!(
        facts.get(&("kbf-cas".into(), "j".into())),
        Some(&Erasure {
            data: 8,
            parity: 4,
            inline: true
        })
    );
    let bad = dir.path().join("bad.tsv");
    for (text, why) in [
        ("b\tk\t7\t5\n", "4 fields, not 7"),
        ("b\tk\tx\t5\t1\t0\t1\n", "\"x\" is not a number"),
        ("b\tk\t0\t5\t1\t0\t1\n", "0 data shards"),
    ] {
        std::fs::write(&bad, text).unwrap();
        assert_eq!(
            read_erasure(std::slice::from_ref(&bad))
                .unwrap_err()
                .to_string(),
            format!("{}:1: {why}", bad.display())
        );
    }
    std::fs::write(&bad, [0xff, b'\n']).unwrap();
    assert!(matches!(
        read_erasure(&[bad]),
        Err(Error::Io { what: "read", .. })
    ));
    assert!(matches!(
        read_erasure(&[dir.path().join("absent")]),
        Err(Error::Io { what: "open", .. })
    ));
}

/// The tables show the layouts table when facts are given, and not otherwise.
#[test]
fn the_layouts_table_is_printed_only_with_facts() {
    let a = object("b", 9);
    let mut facts = ErasureFacts::new();
    facts.insert(
        ("b".into(), a.key.clone()),
        Erasure {
            data: 7,
            parity: 5,
            inline: true,
        },
    );
    let title = "**C-6, objects by the erasure layout their xl.meta states**";
    let (with, _) =
        crate::c6::summarize_with(std::slice::from_ref(&a), None, &[], model(), Some(&facts));
    let text: String = crate::c6::tables(&with)
        .iter()
        .map(Table::markdown)
        .collect();
    assert!(text.contains(&format!("{title}\n\n| data + parity | objects | bytes | share of bytes | inlined objects |\n| --- | --- | --- | --- | --- |\n| 7+5 | 1 | 9 B | 100.0 | 1 |\n")), "{text}");
    let (without, _) = crate::c6::summarize(&[a], None, &[], model());
    let text: String = crate::c6::tables(&without)
        .iter()
        .map(Table::markdown)
        .collect();
    assert!(!text.contains(title));
}
