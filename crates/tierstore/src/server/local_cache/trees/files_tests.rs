//! The layouts of the files of the store: the records file, the sources file and the placement's
//! JSON, what each reads back and what each refuses.

use super::files::{
    At, Corrupt, PlacementFile, Source, Sources, encode_records, parse_records, write_new,
};
use super::location::Location;
use super::testing::built;
use crate::{NewEntry, NewKind, TreeDigest};
use tierstore_testkit::dir::TestDir;

fn two_dirs() -> crate::BuiltTree {
    let file = |path: &str, text: &[u8]| NewEntry {
        path: path.to_owned(),
        kind: NewKind::File {
            mode: None,
            sha256: TreeDigest::of(text),
            size: text.len() as u64,
            from: None,
        },
    };
    built(vec![file("a/one", b"1"), file("two", b"22")])
}

/// Asserts that the records file lists each record under its digest with where its bytes are, and
/// that those bytes, cut out of the file, are the encoding of the record.
#[test]
fn the_records_file_gives_each_record_by_digest_and_place() {
    let tree = two_dirs();
    let (bytes, places) = encode_records(&tree.records);
    assert_eq!(places.len(), 2);
    assert_eq!(parse_records(&bytes).unwrap(), places);
    for ((digest, record), (named, place)) in tree.records.iter().zip(&places) {
        assert_eq!(digest, named);
        let start = place.offset as usize;
        assert_eq!(&bytes[start..start + place.len as usize], record.encode());
    }
    let (empty, none) = encode_records(&[]);
    assert!(none.is_empty());
    assert_eq!(parse_records(&empty).unwrap(), []);
}

/// Asserts that a records file is refused when it begins with another magic or version, is cut
/// inside its header, lists more records than it could hold, or is not the length its index says
/// (a body cut short or one byte too many).
#[test]
fn a_records_file_that_does_not_read_is_refused() {
    let (good, _) = encode_records(&two_dirs().records);
    let mut magic = good.clone();
    magic[0] = b'X';
    assert_eq!(parse_records(&magic), Err(Corrupt("bad magic")));
    let mut version = good.clone();
    version[4] = 2;
    assert_eq!(parse_records(&version), Err(Corrupt("unsupported version")));
    assert_eq!(parse_records(&good[..6]), Err(Corrupt("truncated")));
    assert_eq!(parse_records(&good[..2]), Err(Corrupt("truncated")));
    let mut many = good.clone();
    many[5..9].copy_from_slice(&u32::MAX.to_be_bytes());
    assert_eq!(parse_records(&many), Err(Corrupt("truncated")));
    let short = &good[..good.len() - 1];
    assert!(parse_records(short).is_err());
    let mut long = good;
    long.push(0);
    assert_eq!(
        parse_records(&long),
        Err(Corrupt("the records are not the length the index says"))
    );
}

fn sources() -> Sources {
    let mut files = std::collections::BTreeMap::new();
    files.insert(
        [1; 32],
        Source {
            size: 10,
            pack: 1,
            offset: 100,
        },
    );
    files.insert(
        [2; 32],
        Source {
            size: 20,
            pack: 0,
            offset: 0,
        },
    );
    Sources {
        packs: vec!["cas/pack/a".to_owned(), "cas/pack/b".to_owned()],
        files,
    }
}

/// Asserts that a sources file reads back as written, empty or not, and is the same bytes every
/// time.
#[test]
fn the_sources_file_reads_back_as_written() {
    let made = sources();
    assert_eq!(Sources::parse(&made.encode()).unwrap(), made);
    assert_eq!(made.encode(), sources().encode());
    let empty = Sources::default();
    assert_eq!(Sources::parse(&empty.encode()).unwrap(), empty);
}

/// Asserts that a sources file is refused for another magic or version, a cut anywhere in it, a
/// pack key that is not UTF-8, digests that are not strictly ascending (equal or lower), a source
/// that names a pack the file does not list, and bytes after the last source.
#[test]
fn a_sources_file_that_does_not_read_is_refused() {
    let good = sources().encode();
    let mut magic = good.clone();
    magic[1] = b'X';
    assert_eq!(Sources::parse(&magic), Err(Corrupt("bad magic")));
    let mut version = good.clone();
    version[4] = 9;
    assert_eq!(
        Sources::parse(&version),
        Err(Corrupt("unsupported version"))
    );
    for cut in [3, 8, 12, 20, good.len() - 1] {
        assert_eq!(
            Sources::parse(&good[..cut]),
            Err(Corrupt("truncated")),
            "{cut}"
        );
    }
    let mut utf8 = good.clone();
    utf8[13] = 0xff;
    assert_eq!(
        Sources::parse(&utf8),
        Err(Corrupt("a pack key that is not UTF-8"))
    );
    let mut unordered = sources();
    unordered.files.remove(&[1; 32]);
    unordered.files.insert(
        [0; 32],
        Source {
            size: 1,
            pack: 0,
            offset: 0,
        },
    );
    let mut bytes = unordered.encode();
    // Swap the two digests so the second is lower than the first.
    let first = bytes.len() - 2 * 52;
    let (low, high) = (
        bytes[first..first + 32].to_vec(),
        bytes[first + 52..first + 84].to_vec(),
    );
    bytes[first..first + 32].copy_from_slice(&high);
    bytes[first + 52..first + 84].copy_from_slice(&low);
    assert_eq!(
        Sources::parse(&bytes),
        Err(Corrupt("the digests are not in order"))
    );
    let mut equal = sources().encode();
    let at = equal.len() - 52;
    let first = equal[at - 52..at - 52 + 32].to_vec();
    equal[at..at + 32].copy_from_slice(&first);
    assert_eq!(
        Sources::parse(&equal),
        Err(Corrupt("the digests are not in order"))
    );
    let mut pack = sources();
    pack.files.get_mut(&[2; 32]).unwrap().pack = 2;
    assert_eq!(
        Sources::parse(&pack.encode()),
        Err(Corrupt("a source names a pack that is not listed"))
    );
    let mut trailing = good;
    trailing.push(0);
    assert_eq!(
        Sources::parse(&trailing),
        Err(Corrupt("bytes after the last source"))
    );
}

/// Asserts that the JSON of a placement keeps the locations of both interfaces, and reads a file
/// that has a key this release does not know; and that a location written that is not a location
/// is refused.
#[test]
fn a_placement_file_holds_its_locations_and_ignores_a_key_it_does_not_know() {
    let locations = [
        Location::s3("build-cas", "jobs/1/").unwrap(),
        Location::fs("jobs/1").unwrap(),
    ];
    let file = PlacementFile {
        v: 1,
        tree: "00".repeat(32),
        at: locations.iter().map(At::of).collect(),
        created_ms: 5,
        expires_ms: 9,
    };
    let text = serde_json::to_string(&file).unwrap();
    assert_eq!(
        text,
        format!(
            r#"{{"v":1,"tree":"{}","at":[{{"interface":"s3","bucket":"build-cas","prefix":"jobs/1/"}},{{"interface":"fs","path":"jobs/1"}}],"created_ms":5,"expires_ms":9}}"#,
            "00".repeat(32)
        )
    );
    let newer = text.replace(r#""v":1"#, r#""v":1,"owner":"x""#);
    let read: PlacementFile = serde_json::from_str(&newer).unwrap();
    let back: Vec<Location> = read.at.iter().map(|at| at.location().unwrap()).collect();
    assert_eq!(back, locations);
    let bad = At::Fs {
        path: "trees".to_owned(),
    };
    assert!(bad.location().is_err());
    let bad = At::S3 {
        bucket: "b".to_owned(),
        prefix: "no-slash".to_owned(),
    };
    assert!(bad.location().is_err());
}

/// Asserts that a file written by `write_new` is there whole under its name with nothing beside
/// it, replaces one that was there, and that a name in a directory that is not there is an error.
#[test]
fn a_file_is_written_beside_its_name_and_renamed() {
    let dir = TestDir::under(std::env::temp_dir(), "trees-files");
    let path = dir.path().join("x");
    write_new(&path, b"one").unwrap();
    write_new(&path, b"two").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"two");
    let names: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
    assert_eq!(names.len(), 1);
    assert!(write_new(&dir.path().join("none/x"), b"").is_err());
}
