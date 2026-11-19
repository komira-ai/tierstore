//! The listing: written from an S3 service on loopback and read back, and its refusals.

use bytes::Bytes;
use objstore_s3::s3::Credentials;
use objstore_s3::s3::fake::FakeS3;
use objstore_s3::{Capabilities, ObjectKey, ObjectStore as _};
use tierstore_testkit::dir::TestDir;

use super::*;
use crate::s3::store;

/// Every object is listed once, in key order, across pages, with its size, time and tag; the
/// file reads back to the same objects.
#[test]
fn a_listing_holds_every_object_once_and_reads_back() {
    let rt = crate::s3::tests::runtime();
    let fake = rt.block_on(FakeS3::start());
    fake.set_time(1_790_000_000);
    let s3 = fake.store(Capabilities::default());
    // More than one page of 1000.
    rt.block_on(async {
        for i in 0..1003u32 {
            let key = ObjectKey::new(format!("cas/b/x/{i:05}/pack")).unwrap();
            s3.put_new(&key, Bytes::from(vec![7u8; (i % 5) as usize]), None)
                .await
                .unwrap();
        }
    });
    let dir = TestDir::under(std::env::temp_dir(), "storebench-listing");
    let path = dir.path().join("listing.tsv");
    let mut out = create(&path).unwrap();
    let n = rt.block_on(list(&s3, "kbf-cas", &mut out, &path)).unwrap();
    drop(out);
    assert_eq!(n, 1003);
    let objects = read(&path).unwrap();
    assert_eq!(objects.len(), 1003);
    assert_eq!(objects[0].key, "cas/b/x/00000/pack");
    assert_eq!(objects[1002].key, "cas/b/x/01002/pack");
    assert_eq!(objects[4].size, 4);
    assert_eq!(objects[4].bucket, "kbf-cas");
    assert_eq!(objects[4].modified_ms, 1_790_000_000_000);
    assert!(!objects[4].etag.contains('"'));
    assert!(!objects[4].multipart());
}

/// A store that refuses the listing (wrong credentials) is a store error naming the bucket; an
/// unwritable output is an I/O error naming the file.
#[test]
fn a_refused_listing_and_an_unwritable_file_are_errors() {
    let rt = crate::s3::tests::runtime();
    let fake = rt.block_on(FakeS3::start());
    fake.require_credentials("right", "key");
    let wrong = store(
        fake.endpoint(),
        "tierstore-fake",
        Credentials::new("no", "no"),
        std::time::Duration::from_secs(5),
    )
    .unwrap();
    let dir = TestDir::under(std::env::temp_dir(), "storebench-listing-bad");
    let path = dir.path().join("l.tsv");
    let mut sink = Vec::new();
    let err = rt
        .block_on(list(&wrong, "kbf-cas", &mut sink, &path))
        .unwrap_err();
    assert!(err.to_string().starts_with("list kbf-cas: "), "{err}");

    let s3 = fake.store(Capabilities::default());
    fake.require_credentials("fake-key", "fake-secret");
    rt.block_on(s3.put_new(
        &ObjectKey::new("k").unwrap(),
        Bytes::from_static(b"x"),
        None,
    ))
    .unwrap();
    let err = rt
        .block_on(list(&s3, "kbf-cas", &mut Full, &path))
        .unwrap_err();
    assert!(matches!(err, Error::Io { what: "write", .. }), "{err}");
    assert!(matches!(
        create(&dir.path().join("no/such/dir/x")),
        Err(Error::Io { what: "create", .. })
    ));
}

/// A writer that is always full.
struct Full;

impl Write for Full {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("full"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A line with too few fields, or a size that is not a number, is refused with its line
/// number; an empty line is skipped; a missing file is an I/O error.
#[test]
fn a_bad_listing_line_is_refused_with_its_number() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-listing-parse");
    let path = dir.path().join("l.tsv");
    std::fs::write(&path, "b\tk\t1\t2\te\n\nb\tk\t1\t2\n").unwrap();
    let err = read(&path).unwrap_err().to_string();
    assert_eq!(err, format!("{}:3: 4 fields, not 5", path.display()));
    std::fs::write(&path, "b\tk\tbig\t2\te\n").unwrap();
    let err = read(&path).unwrap_err().to_string();
    assert_eq!(
        err,
        format!("{}:1: size \"big\" is not a number", path.display())
    );
    std::fs::write(&path, "b\tk\t1\tlate\te\n").unwrap();
    assert!(read(&path).is_err());
    assert!(matches!(
        read(&dir.path().join("absent")),
        Err(Error::Io { what: "open", .. })
    ));
}

/// A multipart tag ends `-<digits>`; a plain one, or one ending in a dash or letters, does not.
#[test]
fn a_multipart_tag_is_recognised_by_its_part_count() {
    let o = |etag: &str| Object {
        bucket: "b".into(),
        key: "k".into(),
        size: 1,
        modified_ms: 0,
        etag: etag.into(),
    };
    assert!(o("0dda52c707b6afc737e9d71ace1a0335-3").multipart());
    assert!(!o("0dda52c707b6afc737e9d71ace1a0335").multipart());
    assert!(!o("abc-").multipart());
    assert!(!o("abc-x1").multipart());
}
