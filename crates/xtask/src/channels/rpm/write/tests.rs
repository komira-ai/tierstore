use super::*;
use crate::channels::rpm::testing::*;

const TESTING: Suite = Suite::Testing;
const STABLE: Suite = Suite::Stable;
const ARM: &str = "rpm/pool/tierstore/tierstore-1.2.3-1.aarch64.rpm";
const X86: &str = "rpm/pool/tierstore/tierstore-1.2.3-1.x86_64.rpm";

/// A signed `repodata` for 1.2.3 built from an empty repository, ready to write.
fn next(w: &World, suite: Suite) -> std::path::PathBuf {
    let current = w.read(suite);
    let next = w.index(suite, &current, "1.2.3");
    w.sign_repo(suite, &next);
    next
}

#[test]
fn a_release_is_written_in_the_order_that_keeps_a_reader_consistent() {
    // Catches: a `repomd.xml` that names a file or a package that is not there yet, a package
    // overwritten, an unconditional write, and a signature that is replaced before the file it
    // signs.
    let w = World::new("write-order");
    let next = next(&w, TESTING);
    let said = w
        .write(TESTING, &next, Some("1.2.3"), false)
        .expect("write");
    let keys = w.aws.keys_put();
    assert_eq!(keys[..2], [ARM, X86]);
    for (i, arch) in ["aarch64", "x86_64"].into_iter().enumerate() {
        let at = 2 + i * 5;
        let base = format!("rpm/testing/{arch}/repodata/");
        assert!(
            keys[at..at + 3]
                .iter()
                .all(|k| k.starts_with(&base) && k.ends_with(".xml.gz")),
            "{keys:?}"
        );
        assert_eq!(keys[at + 3], format!("{base}repomd.xml"));
        assert_eq!(keys[at + 4], format!("{base}repomd.xml.asc"));
    }
    assert_eq!(keys.len(), 12);
    // Every put is conditional; with nothing read before, all are create-only.
    for call in w.aws.calls_of("put-object") {
        assert_eq!(
            flag(&call, "--if-none-match").as_deref(),
            Some("*"),
            "{call:?}"
        );
        assert!(flag(&call, "--if-match").is_none());
    }
    let invalidation = &w.aws.calls_of("create-invalidation")[0];
    assert_eq!(
        invalidation[invalidation.len() - 2..],
        [
            "/rpm/testing/aarch64/repodata/*",
            "/rpm/testing/x86_64/repodata/*"
        ]
    );
    assert_eq!(
        flag(invalidation, "--distribution-id").as_deref(),
        Some("E1ABC")
    );
    assert!(
        said.last()
            .expect("last")
            .contains("https://packages-test.example/rpm/testing/<arch>/repodata/repomd.xml")
    );
    assert!(said[0].starts_with("uploaded rpm/pool/"));
    assert_eq!(
        w.aws.get(X86).expect("pool"),
        std::fs::read(w.release("1.2.3").1.join("tierstore-1.2.3-1.x86_64.rpm")).expect("rpm")
    );
}

#[test]
fn a_later_release_replaces_repomd_and_its_signature_only_if_they_are_still_what_was_read() {
    // Catches: a write that overwrites another product's release made in between.
    let w = World::new("write-if-match");
    w.publish(TESTING, "1.2.3");
    let current = w.read(TESTING);
    let next = w.index(TESTING, &current, "1.2.4");
    w.sign_repo(TESTING, &next);
    let before = w.aws.calls_of("put-object").len();
    w.write(TESTING, &next, Some("1.2.4"), false)
        .expect("write");
    let puts: Vec<_> = w
        .aws
        .calls_of("put-object")
        .into_iter()
        .skip(before)
        .collect();
    for arch in ["aarch64", "x86_64"] {
        for file in ["repomd.xml", "repomd.xml.asc"] {
            let call = puts
                .iter()
                .find(|c| {
                    flag(c, "--key").as_deref()
                        == Some(&format!("rpm/testing/{arch}/repodata/{file}"))
                })
                .expect("put");
            let held = w
                .aws
                .get(&format!("rpm/testing/{arch}/repodata/{file}"))
                .expect("held");
            assert!(
                flag(call, "--if-match").is_some() && flag(call, "--if-none-match").is_none(),
                "{call:?}"
            );
            assert_ne!(held, Vec::<u8>::new());
        }
    }
    // The content-named files stay create-only.
    assert!(
        puts.iter()
            .filter(|c| flag(c, "--key").is_some_and(|k| k.ends_with(".xml.gz")))
            .all(|c| flag(c, "--if-none-match").is_some())
    );
}

#[test]
fn a_lost_race_on_the_repomd_ends_green_for_another_attempt_or_red_on_the_last() {
    // Catches: another product's release overwritten, an attempt that goes on after losing, and a
    // third attempt that asks for a fourth.
    let w = World::new("write-lost");
    w.publish(TESTING, "1.2.3");
    let current = w.read(TESTING);
    let next = w.index(TESTING, &current, "1.2.4");
    w.sign_repo(TESTING, &next);
    // Someone else replaces the aarch64 repomd.xml after it was read.
    w.aws.put(
        "rpm/testing/aarch64/repodata/repomd.xml",
        b"another product's release",
    );
    let out = w.root.join("output");
    let said = crate::channels::rpm::write::write(
        &w.aws,
        &w.curl,
        &Request {
            suite: TESTING,
            bucket: BUCKET,
            distribution_id: "E1ABC",
            hostname: "packages-test.example",
            next: &next,
            pool: Some(Pool {
                rpms: &w.release("1.2.4").1,
                version: "1.2.4",
            }),
            last: false,
            curl: Path::new("curl"),
        },
        Some(&out),
    )
    .expect("a lost race is not a failure");
    assert_eq!(
        std::fs::read_to_string(&out).expect("out"),
        "conflict=true\n"
    );
    assert!(said.last().expect("last").starts_with(
        "::notice title=rpm repodata::rpm/testing/aarch64/repodata/repomd.xml changed"
    ));
    assert_eq!(
        w.aws.calls_of("create-invalidation").len(),
        1,
        "only the first publish invalidated"
    );
    assert_eq!(
        w.aws
            .get("rpm/testing/aarch64/repodata/repomd.xml")
            .expect("held"),
        b"another product's release"
    );
    // (The packages of the first attempt are taken out so this one uploads them again.)
    w.aws.store.borrow_mut().retain(|k, _| !k.contains("1.2.4"));
    let e = w
        .write(TESTING, &next, Some("1.2.4"), true)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("changed while the repodata was built, and this was the last attempt"),
        "{e}"
    );
    // Without an output file the first attempt still ends green.
    w.aws.store.borrow_mut().retain(|k, _| !k.contains("1.2.4"));
    assert!(w.write(TESTING, &next, Some("1.2.4"), false).is_ok());
    // A write that lands appends conflict=false.
    let w = World::new("write-landed");
    let next = next_for(&w);
    let landed = w.root.join("landed");
    let (_, signed) = w.release("1.2.3");
    crate::channels::rpm::write::write(
        &w.aws,
        &w.curl,
        &Request {
            suite: TESTING,
            bucket: BUCKET,
            distribution_id: "E1ABC",
            hostname: "packages-test.example",
            next: &next,
            pool: Some(Pool {
                rpms: &signed,
                version: "1.2.3",
            }),
            last: false,
            curl: Path::new("curl"),
        },
        Some(&landed),
    )
    .expect("write");
    assert_eq!(
        std::fs::read_to_string(&landed).expect("out"),
        "conflict=false\n"
    );
}

#[test]
fn a_package_in_the_pool_is_kept_only_if_it_is_ours() {
    // Catches: a rerun of a half-finished write failing for ever, and a different file under the
    // name of a published package being taken for it.
    let w = World::new("write-pool");
    let next = next(&w, TESTING);
    let ours =
        std::fs::read(w.release("1.2.3").1.join("tierstore-1.2.3-1.aarch64.rpm")).expect("rpm");
    w.aws.put(ARM, &ours);
    w.curl.serves.replace(Some(ours.clone()));
    let said = w
        .write(TESTING, &next, Some("1.2.3"), false)
        .expect("write");
    assert_eq!(
        said[0],
        format!("{ARM} exists already, with the same bytes; it is kept as it is")
    );
    let fetched = w.curl.calls.borrow()[0].clone();
    assert_eq!(
        fetched.last().map(String::as_str),
        Some("https://packages-test.example/rpm/pool/tierstore/tierstore-1.2.3-1.aarch64.rpm")
    );
    assert!(fetched.contains(&"--fail".to_owned()) && fetched.contains(&"=https".to_owned()));
    assert!(!next.join("pool-check.rpm").exists());

    let w = World::new("write-pool-other");
    let next = next_for(&w);
    w.aws.put(ARM, b"another build");
    w.curl.serves.replace(Some(b"another build".to_vec()));
    let e = w
        .write(TESTING, &next, Some("1.2.3"), false)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("exists already with different bytes") && e.contains("never overwritten"),
        "{e}"
    );
    assert!(
        w.aws.keys_put().len() == 1,
        "nothing else is written after it"
    );

    let w = World::new("write-pool-unreadable");
    let next = next_for(&w);
    w.aws.put(ARM, b"x");
    let e = w
        .write(TESTING, &next, Some("1.2.3"), false)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("could not be fetched from https://packages-test.example/rpm/pool/tierstore/")
            && e.contains("404"),
        "{e}"
    );
}

fn next_for(w: &World) -> std::path::PathBuf {
    next(w, TESTING)
}

#[test]
fn the_promotion_writes_the_repodata_of_stable_and_no_package() {
    // Catches: a promotion that uploads packages (the role may not, and bytes must not move), and
    // one that leaves a suite's cache stale.
    let w = World::new("write-no-pool");
    w.publish(TESTING, "1.2.3");
    let after = w.aws.calls_of("put-object").len();
    let (testing, stable) = (w.read(TESTING), w.read(STABLE));
    let out = w.dir("promoted");
    let (dist, signed) = w.release("1.2.3");
    crate::channels::rpm::promote::promote(&crate::channels::rpm::promote::Request {
        testing: &testing,
        stable: &stable,
        out: &out,
        version: "1.2.3",
        sums: &dist.join("SHA256SUMS"),
        signed_sums: &signed.join("SHA256SUMS.signed"),
        date: 1_794_400_000,
        keep: 25,
    })
    .expect("promote");
    w.sign_repo(STABLE, &out);
    w.write(STABLE, &out, None, false).expect("write");
    let keys: Vec<String> = w.aws.keys_put().into_iter().skip(after).collect();
    assert_eq!(keys.len(), 10);
    assert!(
        keys.iter().all(|k| k.starts_with("rpm/stable/")),
        "{keys:?}"
    );
    let invalidation = &w.aws.calls_of("create-invalidation")[1];
    assert!(invalidation.contains(&"/rpm/stable/x86_64/repodata/*".to_owned()));
    // Stable lists the pool file testing's index listed: the primary.xml of stable names it.
    let primary = w
        .aws
        .keys_put()
        .into_iter()
        .find(|k| k.starts_with("rpm/stable/x86_64") && k.ends_with("-primary.xml.gz"))
        .expect("primary");
    let xml = String::from_utf8(
        crate::channels::rpm::repodata::gunzip(&w.aws.get(&primary).expect("primary"))
            .expect("gunzip"),
    )
    .expect("utf8");
    assert!(xml.contains("<location href=\"../../pool/tierstore/tierstore-1.2.3-1.x86_64.rpm\"/>"));
}

#[test]
fn a_new_repodata_that_is_incomplete_or_not_what_it_says_is_not_written() {
    // Catches: a repository published half-built, a file changed after it was built, and an index
    // that does not list the packages being uploaded.
    let w = World::new("write-bad-next");
    let next = next(&w, TESTING);
    let calls = w.aws.calls.borrow().len();
    let dir = next.join("testing/x86_64/repodata");
    let write = |w: &World, next: &Path| {
        w.write(TESTING, next, Some("1.2.3"), false)
            .unwrap_err()
            .to_string()
    };
    let asc = std::fs::read(dir.join("repomd.xml.asc")).expect("asc");
    std::fs::remove_file(dir.join("repomd.xml.asc")).expect("remove");
    assert!(write(&w, &next).contains("repomd.xml.asc is missing from the new repodata"));
    std::fs::write(dir.join("repomd.xml.asc"), asc).expect("restore");
    let primary = std::fs::read_dir(&dir)
        .expect("dir")
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with("-primary.xml.gz"))
        .expect("primary")
        .path();
    let held = std::fs::read(&primary).expect("read");
    std::fs::write(&primary, b"changed").expect("write");
    assert!(write(&w, &next).contains("does not have the digest its repomd.xml lists"));
    std::fs::remove_file(&primary).expect("remove");
    assert!(write(&w, &next).contains("is missing from the new repodata"));
    std::fs::write(&primary, &held).expect("restore");
    std::fs::remove_file(dir.join("repomd.xml")).expect("remove");
    assert!(write(&w, &next).contains("repomd.xml is missing from the new repodata"));
    assert_eq!(w.aws.calls.borrow().len(), calls);
    // Packages other than the ones the index lists.
    let w = World::new("write-other-rpms");
    let next = next_for(&w);
    let calls = w.aws.calls.borrow().len();
    let other = w.release("1.2.4");
    let e = crate::channels::rpm::write::write(
        &w.aws,
        &w.curl,
        &Request {
            suite: TESTING,
            bucket: BUCKET,
            distribution_id: "E1ABC",
            hostname: "packages-test.example",
            next: &next,
            pool: Some(Pool {
                rpms: &other.1,
                version: "1.2.4",
            }),
            last: false,
            curl: Path::new("curl"),
        },
        None,
    )
    .unwrap_err()
    .to_string();
    assert!(
        e.contains(
            "does not list tierstore-1.2.4-1.aarch64.rpm with the digest of the file to upload"
        ),
        "{e}"
    );
    assert_eq!(w.aws.calls.borrow().len(), calls);
    // No ETags.
    std::fs::remove_file(next.join("etags.txt")).expect("remove");
    let e = w
        .write(TESTING, &next, Some("1.2.3"), false)
        .unwrap_err()
        .to_string();
    assert!(e.contains("etags.txt is missing"), "{e}");
    std::fs::write(next.join("etags.txt"), "not a line").expect("write");
    assert!(w.write(TESTING, &next, Some("1.2.3"), false).is_err());
}

#[test]
fn flags_out_of_their_form_and_failed_calls_are_told_by_what_they_say() {
    // Catches: a write to a bucket, distribution or host that is not one, a put that fails for any
    // reason other than a lost race taken for one, and an invalidation that failed unnoticed.
    let w = World::new("write-flags");
    let next = next(&w, TESTING);
    let (_, signed) = w.release("1.2.3");
    let calls = w.aws.calls.borrow().len();
    let ask = |bucket: &str, id: &str, host: &str| {
        crate::channels::rpm::write::write(
            &w.aws,
            &w.curl,
            &Request {
                suite: TESTING,
                bucket,
                distribution_id: id,
                hostname: host,
                next: &next,
                pool: Some(Pool {
                    rpms: &signed,
                    version: "1.2.3",
                }),
                last: false,
                curl: Path::new("curl"),
            },
            None,
        )
    };
    assert!(matches!(
        ask("Bad Bucket", "E1", "h.example"),
        Err(Error::Usage(_))
    ));
    assert!(matches!(
        ask(BUCKET, "e1", "h.example"),
        Err(Error::Usage(_))
    ));
    assert!(matches!(ask(BUCKET, "E1", ""), Err(Error::Usage(_))));
    assert!(matches!(
        ask(BUCKET, "E1", "h/example"),
        Err(Error::Usage(_))
    ));
    assert_eq!(w.aws.calls.borrow().len(), calls);
    w.aws.refuse_when(
        "create-invalidation",
        "An error occurred (AccessDenied) when calling CreateInvalidation",
    );
    let e = w
        .write(TESTING, &next, Some("1.2.3"), false)
        .unwrap_err()
        .to_string();
    assert!(e.contains("the invalidation of /rpm/testing/aarch64/repodata/* /rpm/testing/x86_64/repodata/* failed"), "{e}");
    let w = World::new("write-put-fails");
    let next = next_for(&w);
    w.aws.refuse_when(
        "repomd.xml.asc",
        "An error occurred (InternalError) when calling PutObject",
    );
    let e = w
        .write(TESTING, &next, Some("1.2.3"), false)
        .unwrap_err()
        .to_string();
    assert!(e.contains("put-object rpm/testing/aarch64/repodata/repomd.xml.asc failed: An error occurred (InternalError)"), "{e}");
    assert!(matches!(
        crate::channels::rpm::state::pool_key("a/b.rpm"),
        Err(Error::Failed(_))
    ));
    for bad in ["", ".hidden.rpm", "a.deb"] {
        assert!(crate::channels::rpm::state::pool_key(bad).is_err(), "{bad}");
    }
}
