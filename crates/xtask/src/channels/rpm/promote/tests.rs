use super::*;
use crate::channels::rpm::sign::SIGNED_SUMS;
use crate::channels::rpm::testing::*;

const TESTING: Suite = Suite::Testing;
const STABLE: Suite = Suite::Stable;

struct Ready {
    w: World,
    testing: std::path::PathBuf,
    stable: std::path::PathBuf,
}

/// `1.2.3` published in testing; stable read before.
fn ready(name: &str) -> Ready {
    let w = World::new(name);
    w.publish(TESTING, "1.2.3");
    let (testing, stable) = (w.read(TESTING), w.read(STABLE));
    Ready { w, testing, stable }
}

fn request<'a>(r: &'a Ready, out: &'a Path, sums: &'a Path, signed: &'a Path) -> Request<'a> {
    Request {
        testing: &r.testing,
        stable: &r.stable,
        out,
        version: "1.2.3",
        sums,
        signed_sums: signed,
        date: 1_794_400_000,
        keep: 25,
    }
}

#[test]
fn the_entries_testing_lists_become_the_entries_of_stable_and_nothing_is_built() {
    // Catches: a promotion that lists other bytes than the ones installed and checked in testing,
    // that drops another product's entries from stable, or that writes a package.
    let r = ready("promote-ok");
    let (dist, signed) = r.w.release("1.2.3");
    let foreign = crate::channels::rpm::repodata::tests_foreign("othertool", "0.9.1");
    crate::channels::rpm::index::write_repo(
        &r.stable,
        STABLE,
        "x86_64",
        std::slice::from_ref(&foreign),
        5,
    )
    .expect("foreign");
    let out = r.w.dir("promoted");
    let said = promote(&request(
        &r,
        &out,
        &dist.join("SHA256SUMS"),
        &signed.join(SIGNED_SUMS),
    ))
    .expect("promote");
    assert_eq!(said, ["rpm promote: added 2, dropped 0, listed 3"]);
    let stable = crate::channels::rpm::index::read_repo(&out, STABLE, "x86_64").expect("repo");
    assert_eq!(stable[0], foreign);
    let in_testing =
        crate::channels::rpm::index::read_repo(&r.testing, TESTING, "x86_64").expect("repo");
    assert_eq!(stable[1], in_testing[0]);
    assert_eq!(
        std::fs::read(out.join("etags.txt")).expect("etags"),
        std::fs::read(r.stable.join("etags.txt")).expect("etags")
    );
    assert!(!out.join("stable/x86_64/repodata/repomd.xml.asc").exists());
    // Promoting what stable lists already changes no entry.
    let again = r.w.dir("again");
    let promoted = Ready {
        stable: out.clone(),
        ..ready("promote-again")
    };
    let said = promote(&Request {
        stable: &out,
        out: &again,
        ..request(
            &promoted,
            &again,
            &dist.join("SHA256SUMS"),
            &signed.join(SIGNED_SUMS),
        )
    })
    .expect("again");
    assert_eq!(said, ["rpm promote: added 0, dropped 0, listed 3"]);
}

#[test]
fn a_release_testing_does_not_list_or_lists_with_other_bytes_is_not_promoted() {
    // Catches: a stable that lists a package nobody installed, or one whose bytes are not the
    // ones the release was verified against.
    let r = ready("promote-refused");
    let (dist, signed) = r.w.release("1.2.3");
    let (sums, signed_sums) = (dist.join("SHA256SUMS"), signed.join(SIGNED_SUMS));
    let out = r.w.dir("promoted");
    let refused = |r: &Ready, sums: &Path, signed: &Path, version: &str| {
        promote(&Request {
            version,
            ..request(r, &out, sums, signed)
        })
        .unwrap_err()
        .to_string()
    };
    let e = refused(&r, &sums, &signed_sums, "1.2.4");
    assert!(e.contains("lists no tierstore-1.2.4-1.aarch64.rpm"), "{e}");
    let (dist4, signed4) = r.w.release("1.2.4");
    let e = refused(
        &r,
        &dist4.join("SHA256SUMS"),
        &signed4.join(SIGNED_SUMS),
        "1.2.4",
    );
    assert!(
        e.contains("testing does not list tierstore-1.2.4-1.aarch64.rpm"),
        "{e}"
    );
    // Signed sums that list other bytes than testing does.
    let other = r.w.root.join("other-signed-sums");
    std::fs::write(
        &other,
        format!(
            "{}  tierstore-1.2.3-1.aarch64.rpm\n{}  tierstore-1.2.3-1.x86_64.rpm\n",
            "a".repeat(64),
            "b".repeat(64)
        ),
    )
    .expect("write");
    let e = refused(&r, &sums, &other, "1.2.3");
    assert!(
        e.contains("testing lists tierstore-1.2.3-1.aarch64.rpm with sha256")
            && e.contains("the signed sums list"),
        "{e}"
    );
    // The attested sums without the package, and signed sums without it.
    let none = r.w.root.join("no-lines");
    std::fs::write(&none, format!("{}  other.rpm\n", "c".repeat(64))).expect("write");
    assert!(
        refused(&r, &none, &signed_sums, "1.2.3")
            .contains("lists no tierstore-1.2.3-1.aarch64.rpm: nothing to promote")
    );
    let e = refused(&r, &sums, &none, "1.2.3");
    assert!(
        e.contains("the signed sums list no tierstore-1.2.3-1.aarch64.rpm"),
        "{e}"
    );
    assert!(!out.exists());
}

#[test]
fn a_stable_that_is_newer_or_has_no_etags_is_not_promoted_into() {
    // Catches: the channel moving down, and an index that is not conditional on what was read.
    let r = ready("promote-stable");
    let (dist, signed) = r.w.release("1.2.3");
    r.w.publish(STABLE, "1.2.4");
    let newer = r.w.read(STABLE);
    let out = r.w.dir("promoted");
    let e = promote(&Request {
        stable: &newer,
        ..request(
            &r,
            &out,
            &dist.join("SHA256SUMS"),
            &signed.join(SIGNED_SUMS),
        )
    })
    .unwrap_err()
    .to_string();
    assert!(e.contains("would move the channel down"), "{e}");
    std::fs::remove_file(r.stable.join("etags.txt")).expect("remove");
    let e = promote(&request(
        &r,
        &out,
        &dist.join("SHA256SUMS"),
        &signed.join(SIGNED_SUMS),
    ))
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("is missing: run `rpm repo read --suite stable` first"),
        "{e}"
    );
    assert!(matches!(
        promote(&Request {
            version: "v1",
            ..request(
                &r,
                &out,
                &dist.join("SHA256SUMS"),
                &signed.join(SIGNED_SUMS)
            )
        }),
        Err(Error::Usage(_))
    ));
}
