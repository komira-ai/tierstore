use super::*;
use crate::channels::rpm::testing::*;

const STABLE: Suite = Suite::Stable;

#[test]
fn a_repository_that_does_not_exist_yet_reads_as_absent_objects() {
    // Catches: a first publish that fails for lack of an index, or that is written with
    // `If-Match` on an ETag nobody read.
    let w = World::new("read-empty");
    let out = w.dir("read");
    let said = read(&w.aws, STABLE, BUCKET, &out).expect("read");
    assert_eq!(said.len(), 4);
    assert_eq!(
        said[0],
        "read aarch64/repodata/repomd.xml: does not exist yet"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("etags.txt")).expect("etags"),
        "aarch64/repodata/repomd.xml\t-\naarch64/repodata/repomd.xml.asc\t-\nx86_64/repodata/repomd.xml\t-\nx86_64/repodata/repomd.xml.asc\t-\n"
    );
    // The publish role may not list the bucket: a missing key is `AccessDenied`.
    let w = World::new("read-denied");
    w.aws.missing.replace(DENIED.to_owned());
    let out = w.dir("read");
    read(&w.aws, STABLE, BUCKET, &out).expect("read");
    assert!(
        std::fs::read_to_string(out.join("etags.txt"))
            .expect("etags")
            .contains("\t-\n")
    );
}

#[test]
fn the_published_repodata_is_copied_with_the_etag_of_each_replaced_object() {
    // Catches: an index merged into something other than what was published, and a write
    // conditional on the wrong version.
    let w = World::new("read-published");
    w.publish(STABLE, "1.2.3");
    let out = w.dir("read");
    let said = read(&w.aws, STABLE, BUCKET, &out).expect("read");
    assert_eq!(said.len(), 2 * (2 + 3));
    for arch in ["aarch64", "x86_64"] {
        let key = format!("rpm/stable/{arch}/repodata/repomd.xml");
        let stored = w.aws.get(&key).expect("stored");
        assert_eq!(
            std::fs::read(out.join(format!("stable/{arch}/repodata/repomd.xml"))).expect("copy"),
            stored
        );
        let etags = std::fs::read_to_string(out.join("etags.txt")).expect("etags");
        assert!(
            etags.contains(&format!("{arch}/repodata/repomd.xml\t{}\n", etag(&stored))),
            "{etags}"
        );
        assert!(
            out.join(format!("stable/{arch}/repodata/repomd.xml.asc"))
                .is_file()
        );
    }
    let fetched = w.aws.calls_of("get-object");
    assert!(
        fetched
            .iter()
            .all(|c| flag(c, "--bucket").as_deref() == Some(BUCKET))
    );
}

#[test]
fn a_read_that_fails_or_finds_the_repodata_inconsistent_says_why() {
    // Catches: a half-read index that is merged anyway, and an error page taken for an object.
    let w = World::new("read-errors");
    assert!(matches!(
        read(&w.aws, STABLE, "Bad Bucket", &w.dir("r")),
        Err(Error::Usage(_))
    ));
    w.aws.refuse_when(
        "--key rpm/stable/aarch64/repodata/repomd.xml.asc",
        "could not connect",
    );
    let e = read(&w.aws, STABLE, BUCKET, &w.dir("r"))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(
            "get-object rpm/stable/aarch64/repodata/repomd.xml.asc failed: could not connect"
        ),
        "{e}"
    );
    let w = World::new("read-no-etag");
    w.aws.put("rpm/stable/aarch64/repodata/repomd.xml", b"x");
    w.aws.stdout.replace(Some("{}".to_owned()));
    let e = read(&w.aws, STABLE, BUCKET, &w.dir("r"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("no ETag in the answer"), "{e}");
}

#[test]
fn a_repomd_that_lists_a_file_that_is_missing_or_changed_is_refused() {
    // Catches: an index merged from metadata a half-finished write left behind.
    let w = World::new("read-inconsistent");
    w.publish(STABLE, "1.2.3");
    let key = w
        .aws
        .keys_put()
        .into_iter()
        .find(|k| k.ends_with("-primary.xml.gz") && k.contains("x86_64"))
        .expect("primary");
    let held = w.aws.get(&key).expect("held");
    w.aws.store.borrow_mut().remove(&key);
    let e = read(&w.aws, STABLE, BUCKET, &w.dir("r"))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("lists the primary file") && e.contains("which does not exist"),
        "{e}"
    );
    w.aws.put(&key, b"other bytes");
    let e = read(&w.aws, STABLE, BUCKET, &w.dir("r"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("does not have the digest"), "{e}");
    w.aws.put(&key, &held);
    w.aws
        .put("rpm/stable/x86_64/repodata/repomd.xml", &[0xff, 0xfe]);
    let e = read(&w.aws, STABLE, BUCKET, &w.dir("r"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("is not UTF-8"), "{e}");
}
