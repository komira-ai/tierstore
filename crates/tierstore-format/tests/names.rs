//! The file names of the hot and cold layouts: every kind of file is written and parsed back, and
//! only the exact form that is written is parsed.
//!
//! Catches:
//! - a path that differs from the documented layout of its kind, or that does not parse back to the
//!   value it was made from;
//! - a non-canonical form that parses (a wrong `<hh>` directory, uppercase hex, leading zeros in a
//!   decimal, a sign, an extra field or path part, a wrong suffix, an unknown directory), which would
//!   let the startup sweep treat one file under two names or take a foreign file for its own;
//! - a decimal field that does not hold its range (a shard index above 255, a number above `u64`);
//! - the fixed file names that are not the documented ones.

use tierstore_format::ids::{BlockId, Generation, KeyHash, StripeId};
use tierstore_format::names::{ColdName, DRIVE_FILE, FAILED_FILE, HotName, INDEX_DIR, NODE_FILE};
use tierstore_format::tomb::LargeFile;

const KH: &str = "abababababababababababababababab";
const GEN: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";
const ID: &str = "0102030405060708";

fn block() -> BlockId {
    BlockId::from_raw(0x0102_0304_0506_0708)
}

fn stripe() -> StripeId {
    StripeId::from_raw(0x0102_0304_0506_0708)
}

fn large() -> LargeFile {
    LargeFile {
        key_hash: KeyHash([0xab; 16]),
        generation: Generation([0xcd; 16]),
        size: 1_000_000,
    }
}

#[test]
fn the_fixed_file_names_are_the_documented_ones() {
    assert_eq!(NODE_FILE, "NODE");
    assert_eq!(DRIVE_FILE, "DRIVE");
    assert_eq!(FAILED_FILE, "FAILED");
    assert_eq!(INDEX_DIR, "index");
}

#[test]
fn every_hot_name_has_its_documented_path_and_parses_back() {
    let cases: Vec<(HotName, String)> = vec![
        (HotName::Segment(block()), format!("seg/{ID}.seg")),
        (HotName::SegmentIndex(block()), format!("seg/{ID}.idx")),
        (
            HotName::Large(large()),
            format!("big/ab/{KH}.{GEN}.1000000.obj"),
        ),
        (
            HotName::LargeUpload {
                key_hash: KeyHash([0xab; 16]),
                nonce: 0xfeed,
            },
            format!("big/.{KH}.000000000000feed.up"),
        ),
        (
            HotName::Cache { boot: 3, seq: 17 },
            "cache/3.17.seg".to_string(),
        ),
        (HotName::TombLog(5), "tomb/5.log".to_string()),
        (HotName::BlockIndex(block()), format!("blk/{ID}.idx")),
        (HotName::Descriptor(stripe()), format!("stripes/{ID}.desc")),
        (HotName::Free(stripe()), format!("stripes/{ID}.free")),
        (HotName::Seal(block()), format!("seals/{ID}.seal")),
    ];
    for (name, path) in cases {
        assert_eq!(name.path(), path);
        assert_eq!(HotName::parse(&path), Some(name), "{path}");
    }
}

#[test]
fn hot_names_at_the_edges_of_their_ranges_round_trip() {
    for name in [
        HotName::Cache { boot: 0, seq: 0 },
        HotName::Cache {
            boot: u64::MAX,
            seq: u64::MAX,
        },
        HotName::TombLog(0),
        HotName::TombLog(u64::MAX),
        HotName::Segment(BlockId::from_raw(0)),
        HotName::Segment(BlockId::from_raw(u64::MAX)),
        HotName::Large(LargeFile {
            key_hash: KeyHash([0; 16]),
            generation: Generation([0; 16]),
            size: 0,
        }),
        HotName::Large(LargeFile {
            key_hash: KeyHash([0xff; 16]),
            generation: Generation([0xff; 16]),
            size: u64::MAX,
        }),
        HotName::LargeUpload {
            key_hash: KeyHash([1; 16]),
            nonce: 0,
        },
        HotName::LargeUpload {
            key_hash: KeyHash([1; 16]),
            nonce: u64::MAX,
        },
    ] {
        assert_eq!(HotName::parse(&name.path()), Some(name), "{}", name.path());
    }
    // The large object's directory is the first byte of the key hash.
    let mut kh = [0xabu8; 16];
    kh[0] = 0x07;
    let n = HotName::Large(LargeFile {
        key_hash: KeyHash(kh),
        generation: Generation([0; 16]),
        size: 1,
    });
    assert!(n.path().starts_with("big/07/07abab"), "{}", n.path());
}

#[test]
fn a_hot_path_that_is_not_the_canonical_form_does_not_parse() {
    let bad = [
        // Not a path of the layout.
        "",
        "/",
        "NODE",
        "index",
        "foo/bar",
        "seg",
        "seg/",
        "/seg/0102030405060708.seg",
        "seg/0102030405060708.seg/",
        "seg//0102030405060708.seg",
        "x/seg/0102030405060708.seg",
        "seg/sub/0102030405060708.seg",
        "tmp/0102030405060708.seg",
        // seg and its index.
        "seg/0102030405060708.sg",
        "seg/0102030405060708.seg.tmp",
        "seg/0102030405060708",
        "seg/.seg",
        "seg/0102030405060708.IDX",
        "seg/010203040506070.seg",
        "seg/01020304050607080.seg",
        "seg/0102030405060A08.seg",
        "seg/0X02030405060708.seg",
        "seg/+102030405060708.seg",
        // blk, stripes, seals.
        "blk/0102030405060708.seg",
        "blk/0102030405060708.idx.old",
        "blk/zz.idx",
        "stripes/0102030405060708.dsc",
        "stripes/0102030405060708",
        "stripes/0102030405060708.desc.1",
        "stripes/0102030405060708.free.1",
        "stripes/.free",
        "seals/0102030405060708.seg",
        "seals/0102030405060708.seal.1",
        "seals/0102030405060708.SEAL",
        // tomb and cache: decimals.
        "tomb/05.log",
        "tomb/5.logs",
        "tomb/5.log.gz",
        "tomb/.log",
        "tomb/-1.log",
        "tomb/+1.log",
        "tomb/1.2.log",
        "tomb/18446744073709551616.log",
        "tomb/a.log",
        "cache/03.17.seg",
        "cache/3.017.seg",
        "cache/3.17.seg.x",
        "cache/3.seg",
        "cache/3.17.18.seg",
        "cache/3..seg",
        "cache/.17.seg",
        "cache/a.1.seg",
        "cache/3.1.log",
        "cache/18446744073709551616.1.seg",
        "cache/1.18446744073709551616.seg",
    ];
    for path in bad {
        assert_eq!(HotName::parse(path), None, "{path:?}");
    }
}

#[test]
fn a_large_object_path_that_is_not_canonical_does_not_parse() {
    let ok = format!("big/ab/{KH}.{GEN}.1000000.obj");
    assert!(HotName::parse(&ok).is_some());
    let bad = [
        // The directory is not the first byte of the key hash.
        format!("big/ac/{KH}.{GEN}.1000000.obj"),
        format!("big/AB/{KH}.{GEN}.1000000.obj"),
        format!("big/a/{KH}.{GEN}.1000000.obj"),
        format!("big/abc/{KH}.{GEN}.1000000.obj"),
        format!("big//{KH}.{GEN}.1000000.obj"),
        // Hex in the wrong case or length.
        format!("big/ab/{}.{GEN}.1000000.obj", KH.to_uppercase()),
        format!("big/ab/{KH}.{}.1000000.obj", GEN.to_uppercase()),
        format!("big/ab/{}.{GEN}.1000000.obj", &KH[2..]),
        format!("big/ab/{KH}.{}.1000000.obj", &GEN[2..]),
        // The size.
        format!("big/ab/{KH}.{GEN}.01000000.obj"),
        format!("big/ab/{KH}.{GEN}.+1000000.obj"),
        format!("big/ab/{KH}.{GEN}.-1.obj"),
        format!("big/ab/{KH}.{GEN}..obj"),
        format!("big/ab/{KH}.{GEN}.1e6.obj"),
        format!("big/ab/{KH}.{GEN}.18446744073709551616.obj"),
        // Fields and suffix.
        format!("big/ab/{KH}.{GEN}.obj"),
        format!("big/ab/{KH}.{GEN}.1000000.5.obj"),
        format!("big/ab/{KH}.{GEN}.1000000.objx"),
        format!("big/ab/{KH}.{GEN}.1000000"),
        format!("big/ab/{KH}.{GEN}.1000000.up"),
        format!("big/ab/sub/{KH}.{GEN}.1000000.obj"),
    ];
    for path in &bad {
        assert_eq!(HotName::parse(path), None, "{path}");
    }
    // A size of zero is written as "0" and only that.
    let zero = HotName::Large(LargeFile { size: 0, ..large() }).path();
    assert!(zero.ends_with(".0.obj"));
    assert!(HotName::parse(&zero).is_some());
    assert_eq!(HotName::parse(&zero.replace(".0.obj", ".00.obj")), None);
}

#[test]
fn a_large_upload_path_that_is_not_canonical_does_not_parse() {
    let ok = format!("big/.{KH}.000000000000feed.up");
    assert!(HotName::parse(&ok).is_some());
    let bad = [
        format!("big/{KH}.000000000000feed.up"),
        format!("big/.{KH}.000000000000feed"),
        format!("big/.{KH}.000000000000feed.up.1"),
        format!("big/.{KH}.00000000000feed.up"),
        format!("big/.{KH}.0000000000000feed.up"),
        format!("big/.{KH}.000000000000FEED.up"),
        format!("big/.{KH}.000000000000fe-d.up"),
        format!("big/.{KH}.feed.up"),
        format!("big/.{KH}.up"),
        format!("big/.{}.000000000000feed.up", &KH[1..]),
        format!("big/.{}.000000000000feed.up", KH.to_uppercase()),
        format!("big/..{KH}.000000000000feed.up"),
        format!("big/.{KH}.000000000000feed.upload"),
        "big/x.up".to_string(),
        "big/.up".to_string(),
        "big/".to_string(),
    ];
    for path in &bad {
        assert_eq!(HotName::parse(path), None, "{path}");
    }
}

#[test]
fn every_cold_name_has_its_documented_path_and_parses_back() {
    // The directory is the low byte of the id: 0x08.
    let cases: Vec<(ColdName, String)> = vec![
        (
            ColdName::Shard {
                stripe: stripe(),
                index: 3,
            },
            format!("s/08/{ID}.3.shard"),
        ),
        (ColdName::Copy(block()), format!("s/08/{ID}.copy")),
        (
            ColdName::ShardUpload {
                stripe: stripe(),
                index: 3,
                nonce: 0xbeef,
            },
            format!("s/08/.{ID}.3.000000000000beef.up"),
        ),
        (
            ColdName::CopyUpload {
                block: block(),
                nonce: 0xbeef,
            },
            format!("s/08/.{ID}.000000000000beef.up"),
        ),
    ];
    for (name, path) in cases {
        assert_eq!(name.path(), path);
        assert_eq!(ColdName::parse(&path), Some(name), "{path}");
    }
}

#[test]
fn cold_names_at_the_edges_of_their_ranges_round_trip() {
    for name in [
        ColdName::Shard {
            stripe: StripeId::from_raw(0),
            index: 0,
        },
        ColdName::Shard {
            stripe: StripeId::from_raw(u64::MAX),
            index: 255,
        },
        ColdName::Copy(BlockId::from_raw(0)),
        ColdName::Copy(BlockId::from_raw(0xff)),
        ColdName::Copy(BlockId::from_raw(u64::MAX)),
        ColdName::ShardUpload {
            stripe: StripeId::from_raw(0xab),
            index: 255,
            nonce: u64::MAX,
        },
        ColdName::ShardUpload {
            stripe: StripeId::from_raw(0xab),
            index: 0,
            nonce: 0,
        },
        ColdName::CopyUpload {
            block: BlockId::from_raw(0xab),
            nonce: 0,
        },
        ColdName::CopyUpload {
            block: BlockId::from_raw(u64::MAX),
            nonce: u64::MAX,
        },
    ] {
        assert_eq!(ColdName::parse(&name.path()), Some(name), "{}", name.path());
    }
    assert_eq!(
        ColdName::Copy(BlockId::from_raw(0x0a)).path(),
        "s/0a/000000000000000a.copy"
    );
    assert_eq!(
        ColdName::Copy(BlockId::from_raw(0x100)).path(),
        "s/00/0000000000000100.copy"
    );
}

#[test]
fn a_cold_path_that_is_not_the_canonical_form_does_not_parse() {
    let bad = [
        // Not a path of the layout.
        "",
        "s",
        "s/08",
        "s/08/",
        "DRIVE",
        "FAILED",
        "S/08/0102030405060708.copy",
        "t/08/0102030405060708.copy",
        "s/08/sub/0102030405060708.copy",
        "/s/08/0102030405060708.copy",
        "s/08/0102030405060708.copy/",
        "s//0102030405060708.copy",
        // The directory is not the low byte of the id.
        "s/09/0102030405060708.copy",
        "s/01/0102030405060708.copy",
        "s/8/0102030405060708.copy",
        "s/008/0102030405060708.copy",
        "s/0a/0102030405060708.copy",
        "s/09/0102030405060708.3.shard",
        "s/09/.0102030405060708.3.000000000000beef.up",
        "s/09/.0102030405060708.000000000000beef.up",
        // Hex digits.
        "s/0A/000000000000000a.copy",
        "s/0a/000000000000000A.copy",
        "s/08/010203040506070.copy",
        "s/08/01020304050607080.copy",
        "s/08/0102030405060708.3.shard.1",
        // Shard names: the index and the fields.
        "s/08/0102030405060708.03.shard",
        "s/08/0102030405060708.256.shard",
        "s/08/0102030405060708.-1.shard",
        "s/08/0102030405060708.+1.shard",
        "s/08/0102030405060708..shard",
        "s/08/0102030405060708.x.shard",
        "s/08/0102030405060708.3.4.shard",
        "s/08/0102030405060708.shard",
        "s/08/0102030405060708.3.shrd",
        "s/08/0102030405060708.3.shards",
        "s/08/0102030405060708.3.copy",
        // Copy names.
        "s/08/0102030405060708.copy.bak",
        "s/08/0102030405060708.cop",
        "s/08/0102030405060708",
        "s/08/.copy",
        // Upload names.
        "s/08/0102030405060708.3.000000000000beef.up",
        "s/08/.0102030405060708.3.000000000000beef",
        "s/08/.0102030405060708.03.000000000000beef.up",
        "s/08/.0102030405060708.256.000000000000beef.up",
        "s/08/.0102030405060708.3.00000000000beef.up",
        "s/08/.0102030405060708.3.000000000000BEEF.up",
        "s/08/.0102030405060708.3.4.000000000000beef.up",
        "s/08/.0102030405060708.beef.up",
        "s/08/.0102030405060708.000000000000BEEF.up",
        "s/08/.0102030405060708.00000000000beef.up",
        "s/08/.0102030405060708.up",
        "s/08/.0102030405060708.3.000000000000beef.upload",
        "s/08/..0102030405060708.000000000000beef.up",
        "s/08/.up",
    ];
    for path in bad {
        assert_eq!(ColdName::parse(path), None, "{path:?}");
    }
}

#[test]
fn a_hot_path_is_not_a_cold_name_and_the_reverse() {
    assert_eq!(ColdName::parse(&HotName::Segment(block()).path()), None);
    assert_eq!(ColdName::parse("seg/0102030405060708.copy"), None);
    assert_eq!(HotName::parse(&ColdName::Copy(block()).path()), None);
    assert_eq!(
        HotName::parse(
            &ColdName::Shard {
                stripe: stripe(),
                index: 1
            }
            .path()
        ),
        None
    );
}
