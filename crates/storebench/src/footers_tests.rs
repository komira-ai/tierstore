//! The footer scan: segments built byte by byte, every status, the ranged GETs against an S3
//! service on loopback, and the footer file written and read back.

use std::time::Duration;

use bytes::Bytes;
use objstore_s3::s3::Credentials;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::{Capabilities, ObjectKey, ObjectStore as _};
use tierstore_testkit::dir::TestDir;

use super::*;

type Entry = ([u8; 32], u64, u64);

/// A digest that sorts by `n`.
fn hash(n: u32) -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..4].copy_from_slice(&n.to_be_bytes());
    h
}

/// `n` index entries in ascending order, record after record from offset 0.
fn entries(n: u32) -> Vec<Entry> {
    let mut at = 0;
    (0..n)
        .map(|i| {
            let size = 10 + u64::from(i % 7);
            let entry = (hash(i + 1), size, at);
            at += size;
            entry
        })
        .collect()
}

/// The bytes of the records of `entries`.
fn records_len(entries: &[Entry]) -> usize {
    entries.iter().map(|e| usize::try_from(e.1).unwrap()).sum()
}

/// Recomputes the footer checksum of `seg` from its trailer's entry count.
fn seal(seg: &mut [u8]) {
    let len = seg.len();
    let count = u32::from_le_bytes(seg[len - 24..len - 20].try_into().unwrap());
    let footer_len = FANOUT_LEN + TRAILER_LEN + ENTRY_LEN * usize::try_from(count).unwrap();
    let crc = crc32c::crc32c(&seg[len - footer_len..len - 16]);
    seg[len - 16..len - 12].copy_from_slice(&crc.to_le_bytes());
}

/// A segment of `records` bytes of records and an index of `entries` exactly as given, with a
/// correct fan-out and checksum.
fn segment(records: usize, entries: &[Entry]) -> Vec<u8> {
    let mut seg = vec![0xAB; records];
    for b in 0..=255u8 {
        let n = entries.iter().filter(|e| e.0[0] <= b).count();
        seg.extend_from_slice(&u32::try_from(n).unwrap().to_le_bytes());
    }
    for (digest, size, offset) in entries {
        seg.extend_from_slice(digest);
        seg.extend_from_slice(&size.to_le_bytes());
        seg.extend_from_slice(&offset.to_le_bytes());
        seg.extend_from_slice(&0u32.to_le_bytes());
    }
    seg.extend_from_slice(&(records as u64).to_le_bytes());
    seg.extend_from_slice(&u32::try_from(entries.len()).unwrap().to_le_bytes());
    seg.extend_from_slice(&[DIGEST_SHA256, 0, 0, 0]);
    seg.extend_from_slice(&0u32.to_le_bytes());
    seg.extend_from_slice(&VERSION.to_le_bytes());
    seg.extend_from_slice(&MAGIC);
    seal(&mut seg);
    seg
}

/// A valid segment of `n` blobs.
fn good(n: u32) -> (Vec<u8>, Vec<Entry>) {
    let e = entries(n);
    (segment(records_len(&e), &e), e)
}

/// The blobs a valid segment of entries `e` should give.
fn blobs_of(e: &[Entry]) -> Vec<Blob> {
    e.iter()
        .map(|(digest, size, _)| Blob {
            digest: *digest,
            size: *size,
        })
        .collect()
}

/// The status `parse_footer` refuses `seg` with.
fn refused(seg: &[u8]) -> Status {
    parse_footer(seg, seg.len() as u64).unwrap_err()
}

/// A footer that is whole gives the blobs of its index, in index order.
#[test]
fn a_whole_footer_gives_its_blobs_in_index_order() {
    let (seg, e) = good(3);
    let blobs = parse_footer(&seg, seg.len() as u64).unwrap();
    assert_eq!(blobs.len(), 3);
    assert_eq!(blobs, blobs_of(&e));
    assert_eq!(
        trailer(&seg, seg.len() as u64).unwrap(),
        Trailer {
            index_offset: 33,
            count: 3,
            footer_len: 1056 + 156
        }
    );
    assert_eq!(blobs[1].size, 11);
    assert_eq!(blobs[2].digest, hash(3));
}

/// A pack with no blobs is valid and has none; the same digest at two sizes is two entries.
#[test]
fn an_empty_footer_and_one_digest_at_two_sizes_are_valid() {
    let empty = segment(0, &[]);
    assert_eq!(empty.len(), 1056);
    assert_eq!(parse_footer(&empty, 1056).unwrap(), Vec::new());

    let twice = [(hash(7), 5, 0), (hash(7), 6, 5)];
    let seg = segment(11, &twice);
    assert_eq!(parse_footer(&seg, seg.len() as u64).unwrap().len(), 2);
}

/// Each way a footer can be wrong has its own status.
#[test]
fn each_way_a_footer_is_wrong_has_its_own_status() {
    let (seg, _) = good(3);
    let len = seg.len();

    let mut bad = seg.clone();
    bad[len - 1] ^= 1;
    assert_eq!(refused(&bad), Status::NoMagic);

    let mut bad = seg.clone();
    bad[len - 12..len - 8].copy_from_slice(&2u32.to_le_bytes());
    seal(&mut bad);
    assert_eq!(refused(&bad), Status::BadVersion);

    let mut bad = seg.clone();
    bad[len - 20] = 2;
    seal(&mut bad);
    assert_eq!(refused(&bad), Status::BadDigestFunction);

    let mut bad = seg.clone();
    bad[len - 32..len - 24].copy_from_slice(&34u64.to_le_bytes());
    seal(&mut bad);
    assert_eq!(refused(&bad), Status::BadLength);

    let mut bad = seg.clone();
    bad[len - 32..len - 24].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(refused(&bad), Status::BadLength);

    let mut bad = seg.clone();
    bad[len - 24..len - 20].copy_from_slice(&4u32.to_le_bytes());
    assert_eq!(refused(&bad), Status::BadLength);

    let mut bad = seg.clone();
    bad[33] ^= 1;
    assert_eq!(refused(&bad), Status::BadCrc);

    let mut bad = seg.clone();
    bad[len - 16] ^= 1;
    assert_eq!(refused(&bad), Status::BadCrc);
}

/// An index that is not strictly ascending, or an entry whose record leaves the record area, is
/// refused even with a correct checksum.
#[test]
fn an_unordered_index_and_a_record_outside_the_records_are_refused() {
    let e = |entries: &[Entry]| {
        let seg = segment(100, entries);
        refused(&seg)
    };
    let descending = [(hash(2), 5, 0), (hash(1), 5, 5)];
    assert_eq!(e(&descending), Status::BadOrder);
    let equal = [(hash(1), 5, 0), (hash(1), 5, 5)];
    assert_eq!(e(&equal), Status::BadOrder);
    let smaller_size_after = [(hash(1), 6, 0), (hash(1), 5, 6)];
    assert_eq!(e(&smaller_size_after), Status::BadOrder);

    assert_eq!(e(&[(hash(1), 5, 96)]), Status::BadRange);
    assert_eq!(e(&[(hash(1), 5, u64::MAX)]), Status::BadRange);
    assert_eq!(e(&[(hash(1), 101, 0)]), Status::BadRange);
    // The last byte of the record area is inside it.
    let seg = segment(100, &[(hash(1), 5, 95)]);
    assert_eq!(parse_footer(&seg, seg.len() as u64).unwrap().len(), 1);
}

/// An object, or a tail, shorter than a trailer is too small; a tail that stops short of the
/// footer its trailer announces is a length error.
#[test]
fn a_short_object_is_too_small_and_a_short_tail_is_a_bad_length() {
    assert_eq!(parse_footer(&[0; 31], 31), Err(Status::TooSmall));
    assert_eq!(parse_footer(&[], 0), Err(Status::TooSmall));
    let (seg, _) = good(3);
    let size = seg.len() as u64;
    assert_eq!(
        parse_footer(&seg[seg.len() - 31..], size),
        Err(Status::TooSmall)
    );
    assert_eq!(
        parse_footer(&seg[seg.len() - 100..], size),
        Err(Status::BadLength)
    );
    // The trailer alone is enough to read the trailer.
    assert_eq!(trailer(&seg[seg.len() - 32..], size).unwrap().count, 3);
}

/// Every status has its name, and a name gives its status back; `get-error` carries a text, so
/// it is no plain name.
#[test]
fn statuses_have_names_that_give_them_back() {
    let names: Vec<&str> = Status::plain().iter().map(Status::name).collect();
    assert_eq!(
        names,
        [
            "ok",
            "no-magic",
            "bad-version",
            "bad-digest-function",
            "bad-length",
            "bad-crc",
            "bad-order",
            "bad-range",
            "too-small"
        ]
    );
    for status in Status::plain() {
        assert_eq!(Status::from_name(status.name()), Some(status));
    }
    assert_eq!(Status::GetError("x".into()).name(), "get-error");
    assert_eq!(Status::from_name("get-error"), None);
    assert_eq!(Status::from_name("fine"), None);
}

/// A range of no bytes, or one that overflows, is refused with a text; any other is made.
#[test]
fn an_empty_or_overflowing_range_is_refused() {
    assert_eq!(range(5, 0).unwrap_err(), "range 5+0 is empty or overflows");
    assert!(range(u64::MAX, 2).is_err());
    let r = range(7, 3).unwrap();
    assert_eq!((r.offset(), r.size()), (7, 3));
}

/// Tabs and line breaks become spaces.
#[test]
fn a_text_becomes_one_field_of_one_line() {
    assert_eq!(one_field("a\tb\nc\r\nd"), "a b c  d");
    assert_eq!(one_field("plain"), "plain");
}

/// An S3 service on loopback holding some objects, and a client of it.
struct Farm {
    rt: tokio::runtime::Runtime,
    fake: FakeS3,
    store: S3Store,
}

/// A service holding `objects`.
fn farm(objects: &[(&str, Vec<u8>)]) -> Farm {
    let rt = crate::s3::tests::runtime();
    let fake = rt.block_on(FakeS3::start());
    let store = fake.store(Capabilities::default());
    rt.block_on(async {
        for (key, body) in objects {
            store
                .put_new(
                    &ObjectKey::new(*key).unwrap(),
                    Bytes::from(body.clone()),
                    None,
                )
                .await
                .unwrap();
        }
    });
    Farm { rt, fake, store }
}

/// The reads the service has answered (the ranged GETs; the listing and the puts are other
/// methods).
fn gets(farm: &Farm) -> usize {
    farm.fake
        .requests()
        .iter()
        .filter(|r| r.starts_with("GET"))
        .count()
}

/// The listing entry of an object of `size` bytes.
fn object(bucket: &str, key: &str, size: u64) -> Object {
    Object {
        bucket: bucket.to_owned(),
        key: key.to_owned(),
        size,
        modified_ms: 0,
        etag: "e".to_owned(),
    }
}

const PACK: &str = "cas/d/n1/b1/0000000000000001";

/// A pack read whole costs one ranged GET, and one more of exactly the footer when the footer
/// is longer than the 64 KiB first read: 1240 blobs make a footer of exactly 65536 bytes, 1241
/// do not fit.
#[test]
fn a_footer_longer_than_the_first_read_costs_one_more_get() {
    let (small, small_e) = good(3);
    let (edge, edge_e) = good(1240);
    let (long, long_e) = good(1241);
    assert_eq!(FANOUT_LEN + TRAILER_LEN + ENTRY_LEN * 1240, 65536);
    let farm = farm(&[
        ("small", small.clone()),
        ("edge", edge.clone()),
        ("long", long.clone()),
    ]);

    let found = farm.rt.block_on(scan_pack(
        &farm.store,
        &object("b", "small", small.len() as u64),
    ));
    assert_eq!(found.status, Status::Ok);
    assert_eq!(found.blobs, blobs_of(&small_e));
    assert_eq!((found.bucket.as_str(), found.key.as_str()), ("b", "small"));
    assert_eq!(gets(&farm), 1);

    let found = farm.rt.block_on(scan_pack(
        &farm.store,
        &object("b", "edge", edge.len() as u64),
    ));
    assert_eq!(found.status, Status::Ok);
    assert_eq!(found.blobs, blobs_of(&edge_e));
    assert_eq!(gets(&farm), 2);

    let found = farm.rt.block_on(scan_pack(
        &farm.store,
        &object("b", "long", long.len() as u64),
    ));
    assert_eq!(found.status, Status::Ok);
    assert_eq!(found.blobs, blobs_of(&long_e));
    assert_eq!(found.blobs.len(), 1241);
    assert_eq!(gets(&farm), 4);
}

/// A pack with a bad footer is reported with its status and no blobs; a pack too small to hold
/// a trailer is not read at all.
#[test]
fn a_bad_pack_has_its_status_and_no_blobs_and_a_tiny_one_is_not_read() {
    let (mut bad_crc, _) = good(3);
    bad_crc[33] ^= 1;
    let farm = farm(&[("crc", bad_crc.clone()), ("junk", vec![9; 100])]);

    let found = farm.rt.block_on(scan_pack(
        &farm.store,
        &object("b", "crc", bad_crc.len() as u64),
    ));
    assert_eq!((found.status, found.blobs.len()), (Status::BadCrc, 0));
    let found = farm
        .rt
        .block_on(scan_pack(&farm.store, &object("b", "junk", 100)));
    assert_eq!((found.status, found.blobs.len()), (Status::NoMagic, 0));
    assert_eq!(gets(&farm), 2);

    let found = farm
        .rt
        .block_on(scan_pack(&farm.store, &object("b", "tiny", 31)));
    assert_eq!(found.status, Status::TooSmall);
    assert_eq!(gets(&farm), 2);
}

/// A key that is not there, and a key the client refuses, are get errors with the error's text.
#[test]
fn a_pack_that_cannot_be_read_is_a_get_error_with_the_texts_of_the_error() {
    let farm = farm(&[]);
    let found = farm
        .rt
        .block_on(scan_pack(&farm.store, &object("b", PACK, 5000)));
    let key = ObjectKey::new(PACK).unwrap();
    assert_eq!(
        found.status,
        Status::GetError(format!("object {key} not found"))
    );
    assert!(found.blobs.is_empty());

    let refused_key = "cas/d/../b/0000000000000001";
    let found = farm
        .rt
        .block_on(scan_pack(&farm.store, &object("b", refused_key, 5000)));
    let why = ObjectKey::new(refused_key).unwrap_err().to_string();
    assert_eq!(found.status, Status::GetError(why));
    assert_eq!(gets(&farm), 1);
}

/// A tree list is JSON when its first byte is `{`; anything else is not; an absent one, an
/// empty one (no first byte to read) and a refused key are get errors.
#[test]
fn a_tree_list_is_json_when_it_starts_with_a_brace() {
    let farm = farm(&[
        ("json", b"{\"a\":1}".to_vec()),
        ("bin", vec![0x01, 0x7b, 0x7b]),
        ("empty", Vec::new()),
    ]);
    let check = |key: &str, size: u64| {
        farm.rt
            .block_on(scan_tree(&farm.store, &object("b", key, size)))
            .outcome
    };
    assert_eq!(check("json", 7), TreeOutcome::Json);
    assert_eq!(check("bin", 3), TreeOutcome::NotJson);
    assert_eq!(check("empty", 0), TreeOutcome::GetError);
    assert_eq!(check("absent", 7), TreeOutcome::GetError);
    assert_eq!(check("cas/l/../x", 7), TreeOutcome::GetError);
    let tree = farm
        .rt
        .block_on(scan_tree(&farm.store, &object("buck", "json", 7)));
    assert_eq!((tree.bucket.as_str(), tree.key.as_str()), ("buck", "json"));
}

/// Every form of an entry is written as its line, and a get error's text is one field.
#[test]
fn entries_are_written_as_their_lines() {
    let pack = |status: Status, blobs: Vec<Blob>| {
        Checked::Pack(PackFooter {
            bucket: "cas-store".into(),
            key: "cas/d/n/b/0000000000000001".into(),
            status,
            blobs,
        })
    };
    let tree = |outcome| {
        Checked::Tree(TreeCheck {
            bucket: "cas-store".into(),
            key: "cas/l/x".into(),
            outcome,
        })
    };
    let mut out = Vec::new();
    let blob = Blob {
        digest: hash(0xabcdef),
        size: 42,
    };
    for entry in [
        pack(Status::Ok, vec![blob.clone(), blob]),
        pack(Status::BadCrc, Vec::new()),
        pack(Status::GetError("a\tb\nc".into()), Vec::new()),
        tree(TreeOutcome::Json),
        tree(TreeOutcome::NotJson),
        tree(TreeOutcome::GetError),
    ] {
        write_entry(&mut out, &entry).unwrap();
    }
    let digest = format!("00abcdef{}", "00".repeat(28));
    let want = format!(
        "P\tcas-store\tcas/d/n/b/0000000000000001\tok\t2\n\
         B\t{digest}\t42\n\
         B\t{digest}\t42\n\
         P\tcas-store\tcas/d/n/b/0000000000000001\tbad-crc\t0\n\
         P\tcas-store\tcas/d/n/b/0000000000000001\tget-error\t0\ta b c\n\
         T\tcas-store\tcas/l/x\tjson\n\
         T\tcas-store\tcas/l/x\tnot-json\n\
         T\tcas-store\tcas/l/x\tget-error\n"
    );
    assert_eq!(String::from_utf8(out).unwrap(), want);
}

/// Every line form reads back to what was scanned, in file order, packs apart from tree lists.
#[test]
fn a_footer_file_reads_back_to_what_was_written() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-footers-read");
    let path = dir.path().join("footers.tsv");
    let blob = |n: u32, size: u64| Blob {
        digest: hash(n),
        size,
    };
    let packs = vec![
        PackFooter {
            bucket: "cas-store".into(),
            key: "cas/d/n/b/0000000000000001".into(),
            status: Status::Ok,
            blobs: vec![blob(1, 10), blob(2, 20)],
        },
        PackFooter {
            bucket: "cas-store".into(),
            key: "cas/d/n/b/0000000000000002".into(),
            status: Status::Ok,
            blobs: Vec::new(),
        },
        PackFooter {
            bucket: "team1".into(),
            key: "cas/d/n/b/0000000000000003".into(),
            status: Status::BadOrder,
            blobs: Vec::new(),
        },
        PackFooter {
            bucket: "team1".into(),
            key: "cas/d/n/b/0000000000000004".into(),
            status: Status::GetError("object x not found".into()),
            blobs: Vec::new(),
        },
        PackFooter {
            bucket: "team1".into(),
            key: "cas/d/n/b/0000000000000005".into(),
            status: Status::GetError(String::new()),
            blobs: Vec::new(),
        },
    ];
    let trees: Vec<TreeCheck> = [
        TreeOutcome::Json,
        TreeOutcome::NotJson,
        TreeOutcome::GetError,
    ]
    .into_iter()
    .enumerate()
    .map(|(i, outcome)| TreeCheck {
        bucket: "cas-store".into(),
        key: format!("cas/l/{i}"),
        outcome,
    })
    .collect();
    let mut text = Vec::new();
    write_entry(&mut text, &Checked::Pack(packs[0].clone())).unwrap();
    write_entry(&mut text, &Checked::Tree(trees[0].clone())).unwrap();
    write_entry(&mut text, &Checked::Pack(packs[1].clone())).unwrap();
    write_entry(&mut text, &Checked::Pack(packs[2].clone())).unwrap();
    write_entry(&mut text, &Checked::Tree(trees[1].clone())).unwrap();
    write_entry(&mut text, &Checked::Pack(packs[3].clone())).unwrap();
    write_entry(&mut text, &Checked::Pack(packs[4].clone())).unwrap();
    write_entry(&mut text, &Checked::Tree(trees[2].clone())).unwrap();
    std::fs::write(&path, text).unwrap();

    let scan = read(&path).unwrap();
    assert_eq!(scan.packs, packs);
    assert_eq!(scan.trees, trees);
    std::fs::write(&path, "").unwrap();
    assert_eq!(read(&path).unwrap(), Scan::default());
}

/// A line that is not one of the forms, a bad number or digest, a `B` line that belongs to no
/// pack, and a pack short of its blobs are refused with the line's number.
#[test]
fn a_malformed_footer_file_is_refused_with_the_line() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-footers-bad");
    let path = dir.path().join("footers.tsv");
    let d = "00".repeat(32);
    let hex_why = hex::decode_to_slice("zz", &mut [0u8; 32]).unwrap_err();
    let p = |n: usize| format!("P\tb\tk\tok\t{n}\n");
    let b = format!("B\t{d}\t1\n");
    let cases: Vec<(String, String)> = vec![
        (
            "X\ta\n".into(),
            "1: \"X\" with 2 fields is not a P, B or T line".into(),
        ),
        (
            "\n".into(),
            "1: \"\" with 1 fields is not a P, B or T line".into(),
        ),
        (
            "B\tx\n".into(),
            "1: \"B\" with 2 fields is not a P, B or T line".into(),
        ),
        (b.clone(), "1: a B line outside a pack's count".into()),
        (
            format!("{}{b}{b}", p(1)),
            "3: a B line outside a pack's count".into(),
        ),
        (
            format!("{}{b}T\tb\tk\tjson\n{b}", p(1)),
            "4: a B line outside a pack's count".into(),
        ),
        (
            format!("{}{b}", p(2)),
            "3: the pack above lacks 1 blob lines".into(),
        ),
        (
            format!("{}T\tb\tk\tjson\n", p(1)),
            "2: the pack above lacks 1 blob lines".into(),
        ),
        (
            format!("{}{}", p(3), p(0)),
            "2: the pack above lacks 3 blob lines".into(),
        ),
        (
            "P\tb\tk\tok\n".into(),
            "1: a P line has 5 fields, or 6 for get-error, not 4".into(),
        ),
        (
            "P\tb\tk\tbogus\t0\n".into(),
            "1: status \"bogus\" is not one with 5 fields".into(),
        ),
        (
            "P\tb\tk\tget-error\t0\n".into(),
            "1: status \"get-error\" is not one with 5 fields".into(),
        ),
        (
            "P\tb\tk\tok\t0\ttext\n".into(),
            "1: status \"ok\" is not one with 6 fields".into(),
        ),
        (
            "P\tb\tk\tbogus\t0\ttext\n".into(),
            "1: status \"bogus\" is not one with 6 fields".into(),
        ),
        (
            "P\tb\tk\tok\tmany\n".into(),
            "1: blob count \"many\" is not a number".into(),
        ),
        (
            format!("{}B\tzz\t1\n", p(1)),
            format!("2: digest \"zz\" is not 64 hex digits: {hex_why}"),
        ),
        (
            format!("{}B\t{d}\tbig\n", p(1)),
            "2: size \"big\" is not a number".into(),
        ),
        (
            "T\tb\tk\tmaybe\n".into(),
            "1: a T line has 4 fields, the last json, not-json or get-error".into(),
        ),
        (
            "T\tb\tk\n".into(),
            "1: a T line has 4 fields, the last json, not-json or get-error".into(),
        ),
    ];
    for (text, why) in cases {
        std::fs::write(&path, &text).unwrap();
        let err = read(&path).unwrap_err();
        assert!(matches!(err, Error::Parse { .. }), "{text:?}: {err}");
        assert_eq!(
            err.to_string(),
            format!("{}:{why}", path.display()),
            "{text:?}"
        );
    }
    std::fs::write(&path, [0xff, 0xfe, b'\n']).unwrap();
    assert!(matches!(read(&path), Err(Error::Io { what: "read", .. })));
    assert!(matches!(
        read(&dir.path().join("absent")),
        Err(Error::Io { what: "open", .. })
    ));
}

/// A writer whose writes fail.
struct Full;

impl Write for Full {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("full"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A writer that takes everything and fails to flush it.
struct NoFlush;

impl Write for NoFlush {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::other("no flush"))
    }
}

/// A file that cannot be written, and one that cannot be flushed, are I/O errors naming it; a
/// bad endpoint is refused before any request.
#[test]
fn scan_fails_on_an_unwritable_file_and_on_a_refused_endpoint() {
    let (seg, _) = good(1);
    let farm = farm(&[(PACK, seg.clone())]);
    let objects = [object("bkt", PACK, seg.len() as u64)];
    let creds = Credentials::new("a", "b");
    let path = std::path::Path::new("footers.tsv");
    let timeout = Duration::from_secs(5);
    let go = |endpoint: &str, out: &mut dyn Write| {
        farm.rt
            .block_on(scan(endpoint, &creds, timeout, &objects, 4, out, path))
    };
    let err = go(farm.fake.endpoint(), &mut Full).unwrap_err();
    assert_eq!(err.to_string(), "write footers.tsv: full");
    let err = go(farm.fake.endpoint(), &mut NoFlush).unwrap_err();
    assert_eq!(err.to_string(), "flush footers.tsv: no flush");
    let err = go("not a url", &mut Vec::new()).unwrap_err();
    assert!(matches!(err, Error::Usage(_)), "{err}");
    // No pack or tree list: no client is made, so the endpoint is never looked at.
    let none = [object("bkt", "history/x", 5)];
    let counts = farm
        .rt
        .block_on(scan(
            "not a url",
            &creds,
            timeout,
            &none,
            4,
            &mut Vec::new(),
            path,
        ))
        .unwrap();
    assert_eq!(counts, Counts::default());
}

/// The listing of a run: two buckets, packs and tree lists of every outcome, and objects that
/// are not read.
struct Run {
    farm: Farm,
    dir: TestDir,
    listing: std::path::PathBuf,
    creds: std::path::PathBuf,
    big: Vec<Entry>,
    small: Vec<Entry>,
}

fn run_setup() -> Run {
    let (small, small_e) = good(3);
    let (big, big_e) = good(1300);
    let (mut bad, _) = good(2);
    bad[33] ^= 1;
    let h = |c: char| c.to_string().repeat(64);
    let keys = [
        "cas/d/n1/b1/0000000000000001",
        "cas/f/w/b1/0000000000000002",
        "cas/d/n1/b1/0000000000000003",
        &format!("cas/l/{}", h('a')),
        &format!("cas/l/{}", h('b')),
    ];
    let farm = farm(&[
        (keys[0], small.clone()),
        (keys[1], big.clone()),
        (keys[2], bad.clone()),
        (keys[3], b"{\"tree\":[]}".to_vec()),
        (keys[4], b"\x01\x02".to_vec()),
    ]);
    let lines = [
        ("cas-store", keys[0].to_owned(), small.len() as u64),
        ("team1", keys[2].to_owned(), bad.len() as u64),
        ("cas-store", keys[3].to_owned(), 11),
        ("cas-store", "cas/d/n1/b1/0000000000000005".to_owned(), 10),
        ("cas-store", keys[1].to_owned(), big.len() as u64),
        ("team1", "cas/d/n1/b1/0000000000000004".to_owned(), 5000),
        (
            "cas-store",
            format!("cas/b/1-2/{}/L0", "ab".repeat(20)),
            100,
        ),
        ("history", "history/x".to_owned(), 7),
        ("team1", keys[4].to_owned(), 2),
        ("cas-store", format!("cas/l/{}", h('c')), 9),
    ];
    let dir = TestDir::under(std::env::temp_dir(), "storebench-footers-run");
    let listing = dir.path().join("listing.tsv");
    let text: String = lines
        .iter()
        .map(|(b, k, s)| format!("{b}\t{k}\t{s}\t0\te\n"))
        .collect();
    std::fs::write(&listing, text).unwrap();
    let creds = dir.path().join("cold.secret.env");
    std::fs::write(&creds, "MINIO_ROOT_USER=a\nMINIO_ROOT_PASSWORD=b\n").unwrap();
    Run {
        farm,
        dir,
        listing,
        creds,
        big: big_e,
        small: small_e,
    }
}

fn args(run: &Run, out: &str, conc: u32) -> Args {
    Args {
        endpoint: crate::s3::Endpoint {
            endpoint: run.farm.fake.endpoint().to_owned(),
            credentials: run.creds.clone(),
            request_timeout_secs: 30,
        },
        listing: run.listing.clone(),
        out: run.dir.path().join(out),
        conc,
        tree_every: 1,
        share: crate::drivewalk::Share { index: 0, of: 1 },
        min_pack_bytes: 0,
    }
}

/// With `--min-pack-bytes` the smaller packs are not read; the larger ones and the tree lists
/// still are.
#[test]
fn packs_below_the_minimum_are_not_read() {
    let run = run_setup();
    let mut says = Vec::new();
    let big_only = Args {
        min_pack_bytes: 5000,
        ..args(&run, "big.tsv", 4)
    };
    super::run(&big_only, &mut says).unwrap();
    assert_eq!(
        String::from_utf8(says).unwrap(),
        format!(
            "2 packs (1 ok), 3 tree lists (1 json) in {}\n",
            big_only.out.display()
        )
    );
}

/// Two shares of the listing read each pack and tree list once between them.
#[test]
fn two_shares_read_the_listing_once_between_them() {
    let run = run_setup();
    let mut lines = Vec::new();
    for index in 0..2 {
        let part = Args {
            share: crate::drivewalk::Share { index, of: 2 },
            ..args(&run, &format!("share{index}.tsv"), 2)
        };
        super::run(&part, &mut Vec::new()).unwrap();
        lines.extend(
            std::fs::read_to_string(&part.out)
                .unwrap()
                .lines()
                .filter(|l| !l.starts_with("B\t"))
                .map(str::to_owned),
        );
    }
    lines.sort();
    let mut whole = std::fs::read_to_string(run_whole(&run))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with("B\t"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    whole.sort();
    assert_eq!(lines, whole);
}

/// The footer file of the whole listing, scanned at once.
fn run_whole(run: &Run) -> std::path::PathBuf {
    let whole = args(run, "whole.tsv", 2);
    super::run(&whole, &mut Vec::new()).unwrap();
    whole.out
}

/// With `--tree-every 2` the first and the third of the three tree lists are checked, and
/// every pack still is.
#[test]
fn a_tree_list_sample_checks_one_in_n() {
    let run = run_setup();
    let mut says = Vec::new();
    let sampled = Args {
        tree_every: 2,
        ..args(&run, "sampled.tsv", 4)
    };
    super::run(&sampled, &mut says).unwrap();
    assert_eq!(
        String::from_utf8(says).unwrap(),
        format!(
            "5 packs (2 ok), 2 tree lists (1 json) in {}\n",
            sampled.out.display()
        )
    );
}

/// `run` reads every pack and tree list of the listing, writes them in listing order whatever
/// the concurrency, reads nothing else, and says what it found in one line.
#[test]
fn run_scans_the_packs_and_tree_lists_of_a_listing_in_order() {
    let run = run_setup();
    let mut says = Vec::new();
    let one = args(&run, "one.tsv", 1);
    super::run(&one, &mut says).unwrap();
    assert_eq!(
        String::from_utf8(says).unwrap(),
        format!(
            "5 packs (2 ok), 3 tree lists (1 json) in {}\n",
            one.out.display()
        )
    );
    let many = args(&run, "many.tsv", 16);
    super::run(&many, &mut Vec::new()).unwrap();
    assert_eq!(
        std::fs::read(&one.out).unwrap(),
        std::fs::read(&many.out).unwrap()
    );

    let scan = read(&one.out).unwrap();
    let keys: Vec<(&str, &str, &str)> = scan
        .packs
        .iter()
        .map(|p| {
            (
                p.bucket.as_str(),
                &p.key[p.key.len() - 1..],
                p.status.name(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        [
            ("cas-store", "1", "ok"),
            ("team1", "3", "bad-crc"),
            ("cas-store", "5", "too-small"),
            ("cas-store", "2", "ok"),
            ("team1", "4", "get-error"),
        ]
    );
    assert_eq!(scan.packs[0].blobs, blobs_of(&run.small));
    assert_eq!(scan.packs[3].blobs, blobs_of(&run.big));
    assert_eq!(scan.packs[3].blobs.len(), 1300);
    assert!(
        matches!(&scan.packs[4].status, Status::GetError(t) if t.ends_with(" not found")),
        "{:?}",
        scan.packs[4].status
    );
    let outcomes: Vec<(&str, TreeOutcome)> = scan
        .trees
        .iter()
        .map(|t| (t.bucket.as_str(), t.outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("cas-store", TreeOutcome::Json),
            ("team1", TreeOutcome::NotJson),
            ("cas-store", TreeOutcome::GetError),
        ]
    );
    // Two ranged GETs for the long footer, one for each other pack, one for each tree list, per
    // run: the large blob and the history object are never read.
    assert_eq!(gets(&run.farm), 2 * (1 + 1 + 2 + 1 + 3));
}

/// Failures of the command: no credentials file, no listing, an unwritable footer file, a bad
/// endpoint and a closed standard output are each an error naming what failed.
#[test]
fn run_fails_with_the_name_of_what_it_could_not_do() {
    let run = run_setup();
    let mut ok = args(&run, "ok.tsv", 2);

    let mut no_creds = args(&run, "x.tsv", 2);
    no_creds.endpoint.credentials = run.dir.path().join("absent.env");
    assert!(matches!(
        super::run(&no_creds, &mut Vec::new()),
        Err(Error::Io { what: "read", .. })
    ));

    let mut no_listing = args(&run, "x.tsv", 2);
    no_listing.listing = run.dir.path().join("absent.tsv");
    assert!(matches!(
        super::run(&no_listing, &mut Vec::new()),
        Err(Error::Io { what: "open", .. })
    ));

    let no_out = args(&run, "no/such/dir/x.tsv", 2);
    assert!(matches!(
        super::run(&no_out, &mut Vec::new()),
        Err(Error::Io { what: "create", .. })
    ));

    let mut bad_endpoint = args(&run, "bad.tsv", 2);
    bad_endpoint.endpoint.endpoint = "not a url".to_owned();
    assert!(matches!(
        super::run(&bad_endpoint, &mut Vec::new()),
        Err(Error::Usage(_))
    ));

    let err = super::run(&ok, &mut Full).unwrap_err();
    assert_eq!(err.to_string(), "write standard output: full");
    ok.conc = 1;
    assert!(super::run(&ok, &mut Vec::new()).is_ok());
}
