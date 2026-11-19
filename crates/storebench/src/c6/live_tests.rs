//! `L_live`: the footers file, the first-location rule and the tree-list check.

use std::io::Cursor;

use tierstore_testkit::dir::TestDir;

use super::*;

const MIB: u64 = 1 << 20;

/// The digest whose 32 bytes are all `n`, as 64 hex digits.
fn hex_of(n: u8) -> String {
    hex::encode([n; 32])
}

/// A pack key of the CAS service's daemon form, numbered `n`.
fn pack_key(n: u64) -> String {
    format!("cas/d/n1/b1/{n:016x}")
}

fn object(key: String, size: u64, modified_ms: u64) -> Object {
    Object {
        bucket: "kbf-cas".to_owned(),
        key,
        size,
        modified_ms,
        etag: "e".to_owned(),
    }
}

fn footers_of(text: &str) -> Footers {
    parse_footers(Cursor::new(text.to_owned()), Path::new("footers.tsv")).unwrap()
}

fn error_of(text: &str) -> String {
    parse_footers(Cursor::new(text.to_owned()), Path::new("footers.tsv"))
        .unwrap_err()
        .to_string()
}

/// A pack with its blobs (digest byte, size) as a footers-file block.
fn block(key: u64, status: &str, blobs: &[(u8, u64)]) -> String {
    let mut text = format!("P\tkbf-cas\t{}\t{status}\t{}\n", pack_key(key), blobs.len());
    for (d, size) in blobs {
        text.push_str(&format!("B\t{}\t{size}\n", hex_of(*d)));
    }
    text
}

/// Packs, their blobs, a pack with a bad status and an error note, and the three tree verdicts
/// all read; blank lines are skipped.
#[test]
fn a_footers_file_reads_packs_blobs_and_tree_checks() {
    let mut text = block(1, "ok", &[(1, 10), (2, 20)]);
    text.push('\n');
    text.push_str(&block(2, "ok", &[]));
    text.push_str(&format!(
        "P\tkbf-cas\t{}\tget-error\t0\ttimed out\n",
        pack_key(3)
    ));
    text.push_str("T\tkbf-cas\tcas/l/a\tjson\nT\tkbf-cas\tcas/l/b\tnot-json\nT\tkbf-cas\tcas/l/c\tget-error\n");
    let f = footers_of(&text);
    let key = |n| ("kbf-cas".to_owned(), pack_key(n));
    assert_eq!(f.packs.len(), 3);
    assert_eq!(f.packs[&key(1)].status, "ok");
    assert_eq!(f.packs[&key(1)].blobs, [([1; 32], 10), ([2; 32], 20)]);
    assert!(f.packs[&key(2)].blobs.is_empty());
    assert_eq!(f.packs[&key(3)].status, "get-error");
    let check = tree_check(Some(&f));
    assert_eq!(
        check,
        TreeCheck {
            checked: 3,
            json: 1,
            not_json: 1,
            errors: 1
        }
    );
    assert_eq!(tree_check(None), TreeCheck::default());
}

/// Each malformed footers file is refused with the line and what is wrong.
#[test]
fn a_malformed_footers_file_is_refused_with_its_line() {
    let h = hex_of(1);
    let cases = [
        (
            format!("B\t{h}\t5\n"),
            "footers.tsv:1: a blob line outside a pack",
        ),
        (
            "P\tb\tk\tok\t1\nB\tzz\t5\n".to_owned(),
            "footers.tsv:2: digest \"zz\": Invalid string length",
        ),
        (
            format!("P\tb\tk\tok\t1\nB\t{h}\tmany\n"),
            "footers.tsv:2: size \"many\" is not a number",
        ),
        (
            "P\tb\tk\tok\tx\n".to_owned(),
            "footers.tsv:1: blob count \"x\" is not a number",
        ),
        (
            format!("P\tb\tk\tok\t2\nB\t{h}\t5\nP\tb\tk2\tok\t0\n"),
            "footers.tsv:3: pack b k is short of 1 blob lines",
        ),
        (
            format!("P\tb\tk\tok\t2\nB\t{h}\t5\nT\tb\tk2\tjson\n"),
            "footers.tsv:3: pack b k is short of 1 blob lines",
        ),
        (
            format!("P\tb\tk\tok\t2\nB\t{h}\t5\n"),
            "footers.tsv:2: pack b k is short of 1 blob lines",
        ),
        (
            "P\tb\tk\tok\t0\nP\tb\tk\tok\t0\n".to_owned(),
            "footers.tsv:2: pack b k is listed twice",
        ),
        (
            "T\tb\tk\tjson\nT\tb\tk\tjson\n".to_owned(),
            "footers.tsv:2: tree list b k is listed twice",
        ),
        (
            "T\tb\tk\tmaybe\n".to_owned(),
            "footers.tsv:1: tree check \"maybe\" is not json, not-json or get-error",
        ),
        (
            "X\tb\tk\n".to_owned(),
            "footers.tsv:1: not a P, B or T line with the fields its kind has",
        ),
        (
            "P\tb\tk\tok\t0\te\textra\n".to_owned(),
            "footers.tsv:1: not a P, B or T line with the fields its kind has",
        ),
        (
            "P\tb\tk\n".to_owned(),
            "footers.tsv:1: not a P, B or T line with the fields its kind has",
        ),
    ];
    for (text, want) in cases {
        assert_eq!(error_of(&text), want, "{text:?}");
    }
}

/// A footers file is read from disk; a missing one and one that is not text are refused.
#[test]
fn a_footers_file_is_read_from_disk() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-live-file");
    let path = dir.path().join("footers.tsv");
    std::fs::write(&path, block(1, "ok", &[(1, 10)])).unwrap();
    assert_eq!(read_footers(&path).unwrap().packs.len(), 1);
    let e = read_footers(&dir.path().join("none.tsv")).unwrap_err();
    assert!(matches!(e, Error::Io { what: "open", .. }), "{e}");
    std::fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
    let e = read_footers(&path).unwrap_err();
    assert!(matches!(e, Error::Io { what: "read", .. }), "{e}");
}

/// The listing and footers of the walk below: bucket `kbf-cas`, packs numbered by key.
///
/// | pack | modified | bytes | blobs (digest: size) | ok |
/// |---|---|---|---|---|
/// | 1 | 100 | 1000 | d1: 10 | yes |
/// | 0 | 100 | 700 | d1: 10, d2: 20 | yes |
/// | 2 | 200 | 2000 | d2: 20 | yes |
/// | 3 | 300 | 3000 | d2: 20, d3: 30 | yes |
/// | 4 | 50 | 500 | none | `get-error` |
/// | 5 | 400 | 4000 | none | yes |
fn scenario() -> (Vec<Object>, String) {
    let mut listing = vec![
        object(pack_key(1), 1000, 100),
        object(pack_key(0), 700, 100),
        object(pack_key(2), 2000, 200),
        object(pack_key(3), 3000, 300),
        object(pack_key(4), 500, 50),
        object(pack_key(5), 4000, 400),
        // A large blob, two tree lists and an object of no kind the walk counts.
        object(format!("cas/l/{}", hex_of(9)), 70 * MIB, 10),
        object(format!("cas/l/{}", hex_of(8)), 100, 10),
        object(format!("cas/l/{}", hex_of(7)), 200, 10),
        object("kbf-history/history/x".to_owned(), 999, 10),
    ];
    listing.rotate_left(3);
    let mut text = String::new();
    text.push_str(&block(1, "ok", &[(1, 10)]));
    text.push_str(&block(0, "ok", &[(1, 10), (2, 20)]));
    text.push_str(&block(2, "ok", &[(2, 20)]));
    text.push_str(&block(3, "ok", &[(2, 20), (3, 30)]));
    text.push_str(&block(4, "get-error", &[]));
    text.push_str(&block(5, "ok", &[]));
    (listing, text)
}

/// The first location of a digest wins: packs are walked by (modified, bucket, key), so of two
/// packs written in the same millisecond the one with the lower key is first; a pack that
/// repeats only digests seen before is dead, one with a single new digest is live, an ok pack
/// of no blobs is dead, and a pack with another status is left out of the walk.
#[test]
fn a_pack_is_live_while_it_holds_the_first_location_of_a_blob() {
    let (listing, text) = scenario();
    let (live, dead) = analyze(&listing, Some(&footers_of(&text)));
    assert_eq!(
        (live.packs_listed, live.pack_bytes),
        (6, 1000 + 700 + 2000 + 3000 + 500 + 4000)
    );
    assert_eq!(
        (
            live.packs_missing,
            live.packs_scanned,
            live.packs_ok,
            live.packs_bad
        ),
        (0, 6, 5, 1)
    );
    assert_eq!(live.bad_bytes, 500);
    assert_eq!(
        live.statuses,
        [
            StatusCount {
                status: "get-error".to_owned(),
                packs: 1
            },
            StatusCount {
                status: "ok".to_owned(),
                packs: 5
            }
        ]
    );
    assert_eq!(
        (live.blobs, live.distinct_digests, live.distinct_bytes),
        (6, 3, 60)
    );
    assert_eq!((live.live_packs, live.live_bytes), (2, 700 + 3000));
    assert_eq!((live.dead_packs, live.dead_bytes), (3, 1000 + 2000 + 4000));
    assert!((live.dead_fraction - 7000.0 / 10_700.0).abs() < 1e-12);
    assert_eq!(live.large_blob_bytes, 70 * MIB);
    assert_eq!(live.tree_list_bytes, 300);
    // The bad pack's footer was not read: it is counted live in the upper bound.
    assert_eq!(live.l_live, Some(3700 + 500 + 70 * MIB + 300));
    let dead: Vec<&str> = dead.iter().map(|d| d.key.as_str()).collect();
    assert_eq!(dead, [pack_key(1), pack_key(2), pack_key(5)]);
}

/// A listed pack that the footers file does not hold is counted live in the upper bound of
/// `L_live`; the lower bound leaves it out, and also the live packs whose first-located blobs
/// are all small enough to have been in it.
#[test]
fn a_pack_without_a_footer_bounds_l_live() {
    let (listing, text) = scenario();
    let mut footers = footers_of(&text);
    footers.packs.remove(&("kbf-cas".to_owned(), pack_key(5)));
    let (live, dead) = analyze(&listing, Some(&footers));
    assert_eq!((live.packs_missing, live.packs_scanned), (1, 5));
    let missing = listing.iter().find(|o| o.key == pack_key(5)).unwrap().size;
    // The scenario's bad pack (500 bytes, its footer not read) is unread too.
    assert_eq!(
        (live.missing_bytes, live.bad_bytes, live.missing_max),
        (missing, 500, missing.max(500))
    );
    let unpacked = live.large_blob_bytes + live.tree_list_bytes;
    assert_eq!(
        live.l_live,
        Some(live.live_bytes + missing + 500 + unpacked)
    );
    assert_eq!(
        live.l_live_low,
        Some(live.live_bytes - live.could_be_dead_bytes + unpacked)
    );
    assert!(live.uncertain_packs <= live.live_packs);
    assert!(live.could_be_dead_bytes <= live.uncertain_bytes);
    assert_eq!(dead.len(), 2);
    assert_eq!(live.dead_packs, 2);
}

/// A live pack is uncertain only when an unread pack written before it is at least as large as
/// its largest first-located blob; a larger unread pack written after it does not count, and a
/// live pack with a larger first-located blob is not uncertain.
#[test]
fn a_live_pack_is_uncertain_only_when_an_earlier_unread_pack_could_hold_its_first_blobs() {
    // (key, size, written at, blobs; `None` for a pack the footers file does not hold)
    let packs = [
        (pack_key(1), 500, 1, None),
        (pack_key(2), 300, 2, Some(vec![(2u8, 100)])),
        (pack_key(3), 9000, 3, Some(vec![(3u8, 8000)])),
        (pack_key(4), 100_000, 4, None),
        (pack_key(5), 200, 5, Some(vec![(5u8, 600)])),
    ];
    let listing: Vec<Object> = packs
        .iter()
        .map(|(k, size, t, _)| object(k.clone(), *size, *t))
        .collect();
    let mut text = String::new();
    for (k, _, _, blobs) in &packs {
        if let Some(blobs) = blobs {
            text.push_str(&format!("P\tkbf-cas\t{k}\tok\t{}\n", blobs.len()));
            for (d, size) in blobs {
                text.push_str(&format!("B\t{}\t{size}\n", hex_of(*d)));
            }
        }
    }
    let (live, _) = analyze(&listing, Some(&footers_of(&text)));
    assert_eq!((live.packs_missing, live.missing_max), (2, 100_000));
    // Pack 2 (first blob 100) follows the 500-byte unread pack 1: uncertain. Pack 3 (8000)
    // follows only pack 1: certain, though the later pack 4 is larger. Pack 5 (600) follows
    // pack 4: uncertain.
    assert_eq!(
        (live.live_packs, live.uncertain_packs, live.uncertain_bytes),
        (3, 2, 300 + 200)
    );
    assert_eq!(live.l_live, Some(300 + 9000 + 200 + 500 + 100_000));
    assert_eq!(live.l_live_low, Some(9000));
}

/// The unread bytes bound what they can make dead: the uncertain packs with the most pack bytes
/// per first-located byte go first, and the last one only in part (rounded up).
#[test]
fn the_unread_bytes_bound_what_they_could_make_dead() {
    // One 500-byte unread pack first; then A (1000 bytes, first blob 400), B (1000, 300) and C
    // (9000, 8000, too large for the unread pack).
    let listing = [
        object(pack_key(1), 500, 1),
        object(pack_key(2), 1000, 2),
        object(pack_key(3), 1000, 3),
        object(pack_key(4), 9000, 4),
    ];
    let text: String = [(2u64, 2u8, 400), (3, 3, 300), (4, 4, 8000)]
        .iter()
        .map(|(k, d, size)| {
            format!(
                "P\tkbf-cas\t{}\tok\t1\nB\t{}\t{size}\n",
                pack_key(*k),
                hex_of(*d)
            )
        })
        .collect();
    let (live, _) = analyze(&listing, Some(&footers_of(&text)));
    assert_eq!((live.uncertain_packs, live.uncertain_bytes), (2, 2000));
    // B (1000 per 300) is bought whole for 300; the remaining 200 buy half of A (1000 per 400).
    assert_eq!(live.could_be_dead_bytes, 1000 + 500);
    assert_eq!(live.l_live_low, Some(11_000 - 1500));
    assert_eq!(live.l_live, Some(11_000 + 500));
}

/// The bound rounds a part of a pack up, never claims more than the uncertain packs hold, and
/// takes a pack whose first-located blobs are empty for nothing.
#[test]
fn could_be_dead_rounds_up_and_stops_at_the_uncertain_bytes() {
    assert_eq!(could_be_dead(vec![(10, 3)], 1), 4);
    assert_eq!(could_be_dead(vec![(10, 3), (7, 0)], 0), 7);
    assert_eq!(could_be_dead(vec![(10, 3), (20, 3)], 1_000), 30);
    assert_eq!(could_be_dead(Vec::new(), 1_000), 0);
    // The largest sizes do not overflow, and an exact part is not rounded.
    assert_eq!(
        could_be_dead(vec![(u64::MAX, u64::MAX)], u64::MAX - 1),
        u64::MAX - 1
    );
}

/// A pack written before every unread pack is never uncertain, whatever its blobs.
#[test]
fn a_live_pack_written_before_every_unread_pack_is_certain() {
    let listing = [object(pack_key(1), 300, 1), object(pack_key(2), 900, 2)];
    let text = format!("P\tkbf-cas\t{}\tok\t1\nB\t{}\t1\n", pack_key(1), hex_of(1));
    let (live, _) = analyze(&listing, Some(&footers_of(&text)));
    assert_eq!((live.live_packs, live.uncertain_packs), (1, 0));
    assert_eq!(live.l_live_low, Some(300));
    assert_eq!(live.l_live, Some(300 + 900));
}

/// A pack whose footer read failed bounds `L_live` as an unread pack does: its bytes are in the
/// upper bound, its size bounds the blobs it could hold, and a later live pack whose first blobs
/// fit in it is uncertain; with every footer read and ok, nothing is uncertain.
#[test]
fn a_pack_with_a_bad_footer_is_counted_as_unread() {
    let packs = [
        (pack_key(1), 300, 1, "ok", vec![(1u8, 100)]),
        (pack_key(2), 9000, 2, "ok", vec![(2u8, 8000)]),
        (pack_key(3), 500, 0, "get-error", vec![]),
    ];
    let listing: Vec<Object> = packs
        .iter()
        .map(|(k, size, t, _, _)| object(k.clone(), *size, *t))
        .collect();
    let mut text = String::new();
    for (k, _, _, status, blobs) in &packs {
        if *status == "get-error" {
            text.push_str(&format!("P\tkbf-cas\t{k}\tget-error\t0\ttimed out\n"));
            continue;
        }
        text.push_str(&format!("P\tkbf-cas\t{k}\t{status}\t{}\n", blobs.len()));
        for (d, size) in blobs {
            text.push_str(&format!("B\t{}\t{size}\n", hex_of(*d)));
        }
    }
    let (live, _) = analyze(&listing, Some(&footers_of(&text)));
    assert_eq!(
        (live.packs_missing, live.packs_bad, live.bad_bytes),
        (0, 1, 500)
    );
    assert_eq!(live.missing_max, 500);
    assert_eq!(
        (live.live_packs, live.uncertain_packs, live.uncertain_bytes),
        (2, 1, 300)
    );
    assert_eq!(live.l_live, Some(300 + 9000 + 500));
    assert_eq!(live.l_live_low, Some(9000));

    let read: String = text
        .lines()
        .filter(|l| !l.contains("get-error"))
        .map(|l| format!("{l}\n"))
        .collect();
    let (all_read, _) = analyze(&listing[..2], Some(&footers_of(&read)));
    assert_eq!((all_read.packs_bad, all_read.missing_max), (0, 0));
    assert_eq!(all_read.uncertain_packs, 0);
    assert_eq!(all_read.l_live, all_read.l_live_low);
    assert_eq!(all_read.l_live, Some(300 + 9000));
}

/// Two scans merge: a pack only one holds is added, a pack read `ok` in either counts as read
/// (whichever scan comes first), a pack bad in both keeps the first scan's record, and a tree
/// list's verdict replaces a `get-error`, never the other way round.
#[test]
fn two_scans_merge_and_a_read_pack_counts_as_read() {
    let ok = |n: u8| {
        format!(
            "P\tkbf-cas\t{}\tok\t1\nB\t{}\t10\n",
            pack_key(u64::from(n)),
            hex_of(n)
        )
    };
    let bad = |n: u8, why: &str| {
        format!(
            "P\tkbf-cas\t{}\tget-error\t0\t{why}\n",
            pack_key(u64::from(n))
        )
    };
    let tree = |n: u8, verdict: &str| format!("T\tkbf-cas\tcas/l/{n}\t{verdict}\n");
    let first = footers_of(
        &[
            ok(1),
            bad(2, "first"),
            bad(3, "first"),
            tree(1, "json"),
            tree(2, "get-error"),
        ]
        .concat(),
    );
    let second = footers_of(
        &[
            bad(1, "second"),
            ok(2),
            bad(3, "second"),
            ok(4),
            tree(1, "get-error"),
            tree(2, "not-json"),
            tree(3, "get-error"),
        ]
        .concat(),
    );
    let mut merged = first.clone();
    merged.merge(second);
    let status = |n: u8| {
        merged.packs[&("kbf-cas".to_owned(), pack_key(u64::from(n)))]
            .status
            .clone()
    };
    assert_eq!(merged.packs.len(), 4);
    assert_eq!(
        [status(1), status(2), status(3), status(4)],
        ["ok", "ok", "get-error", "ok"]
    );
    assert_eq!(
        merged.packs[&("kbf-cas".to_owned(), pack_key(2))].blobs,
        [([2u8; 32], 10)]
    );
    let verdict = |n: u8| merged.trees[&("kbf-cas".to_owned(), format!("cas/l/{n}"))];
    assert_eq!(
        [verdict(1), verdict(2), verdict(3)],
        [Verdict::Json, Verdict::NotJson, Verdict::GetError]
    );
}

/// Several footers files are read in order and merged; none gives no footers, and a file that
/// cannot be read is an error naming it.
#[test]
fn footers_files_are_read_and_merged_in_order() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-live-footers");
    let a = dir.path().join("a.tsv");
    let b = dir.path().join("b.tsv");
    std::fs::write(&a, format!("P\tkbf-cas\t{}\tno-magic\t0\n", pack_key(1))).unwrap();
    std::fs::write(
        &b,
        format!("P\tkbf-cas\t{}\tok\t1\nB\t{}\t7\n", pack_key(1), hex_of(1)),
    )
    .unwrap();
    assert_eq!(read_all_footers(&[]).unwrap(), None);
    let merged = read_all_footers(&[a.clone(), b.clone()]).unwrap().unwrap();
    assert_eq!(
        merged.packs[&("kbf-cas".to_owned(), pack_key(1))].status,
        "ok"
    );
    let missing = dir.path().join("absent.tsv");
    assert!(matches!(
        read_all_footers(&[a, missing]),
        Err(Error::Io { what: "open", .. })
    ));
}

/// Without a footers file no pack is scanned and nothing is walked.
#[test]
fn without_a_footers_file_no_pack_is_scanned() {
    let (listing, _) = scenario();
    let (live, dead) = analyze(&listing, None);
    assert_eq!(
        (live.packs_listed, live.packs_missing, live.packs_scanned),
        (6, 6, 0)
    );
    assert_eq!((live.blobs, live.live_packs, live.dead_packs), (0, 0, 0));
    assert!(live.statuses.is_empty() && dead.is_empty());
    assert_eq!(live.l_live, None);
    assert!(live.dead_fraction.abs() < 1e-12);
    assert_eq!(live.large_blob_bytes, 70 * MIB);
}

/// A listing with no packs needs no footers: `L_live` is the large blobs and tree lists.
#[test]
fn a_listing_without_packs_has_an_l_live_of_its_unpacked_objects() {
    let listing = vec![
        object(format!("cas/l/{}", hex_of(9)), 70 * MIB, 10),
        object(format!("cas/l/{}", hex_of(8)), 100, 10),
    ];
    let (live, dead) = analyze(&listing, None);
    assert_eq!(live.l_live, Some(70 * MIB + 100));
    assert!(dead.is_empty());
}
