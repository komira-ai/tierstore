use super::*;
use crate::channels::rpm::testing::*;

const TESTING: Suite = Suite::Testing;

fn request<'a>(
    w: &'a World,
    current: &'a Path,
    out: &'a Path,
    rpms: &'a Path,
    version: &'a str,
) -> Request<'a> {
    let _ = w;
    Request {
        suite: TESTING,
        current,
        out,
        rpms,
        version,
        date: 1_794_300_000,
        keep: KEEP,
    }
}

fn listed(dir: &Path, arch: &str) -> Vec<String> {
    read_repo(dir, TESTING, arch)
        .expect("repo")
        .iter()
        .map(|p| p.href.clone())
        .collect()
}

#[test]
fn a_release_is_merged_into_an_empty_repository_one_package_per_architecture() {
    // Catches: a package listed under the wrong architecture, a repository without one of its
    // architectures (a client of that architecture finds nothing), and ETags that are not the ones
    // that were read.
    let w = World::new("index-first");
    let current = w.read(TESTING);
    let (_, signed) = w.release("1.2.3");
    let out = w.dir("next");
    let said = build(&request(&w, &current, &out, &signed, "1.2.3")).expect("index");
    assert_eq!(said, ["rpm repo index: added 2, dropped 0, listed 2"]);
    assert_eq!(
        listed(&out, "x86_64"),
        ["../../pool/tierstore/tierstore-1.2.3-1.x86_64.rpm"]
    );
    assert_eq!(
        listed(&out, "aarch64"),
        ["../../pool/tierstore/tierstore-1.2.3-1.aarch64.rpm"]
    );
    assert_eq!(
        std::fs::read(out.join("etags.txt")).expect("etags"),
        std::fs::read(current.join("etags.txt")).expect("etags")
    );
    let repomd =
        std::fs::read_to_string(out.join("testing/x86_64/repodata/repomd.xml")).expect("repomd");
    assert!(repomd.contains("<revision>1794300000</revision>"));
    // No signature yet.
    assert!(!out.join("testing/x86_64/repodata/repomd.xml.asc").exists());
}

#[test]
fn the_packages_of_other_products_are_kept_as_they_are_and_a_second_release_is_added() {
    // Catches: a release of this product removing, reordering or rewriting what another product
    // published into the same repository.
    let w = World::new("index-shared");
    let current = w.read(TESTING);
    let foreign = crate::channels::rpm::repodata::tests_foreign("othertool", "0.9.1");
    write_repo(
        &current,
        TESTING,
        "x86_64",
        std::slice::from_ref(&foreign),
        5,
    )
    .expect("write");
    let (_, signed) = w.release("1.2.3");
    let out = w.dir("next");
    build(&request(&w, &current, &out, &signed, "1.2.3")).expect("index");
    assert_eq!(
        listed(&out, "x86_64"),
        [
            "../../pool/othertool/othertool-0.9.1-1.x86_64.rpm",
            "../../pool/tierstore/tierstore-1.2.3-1.x86_64.rpm"
        ]
    );
    let kept = read_repo(&out, TESTING, "x86_64").expect("repo");
    assert_eq!(kept[0], foreign);
    // The same release again, on top of itself: nothing is added.
    let again = w.dir("again");
    std::fs::copy(current.join("etags.txt"), out.join("etags.txt")).expect("etags");
    let said = build(&request(&w, &out, &again, &signed, "1.2.3")).expect("again");
    assert_eq!(said, ["rpm repo index: added 0, dropped 0, listed 3"]);
    // A newer release: the older one stays listed.
    let (_, signed4) = w.release("1.2.4");
    let newer = w.dir("newer");
    build(&request(&w, &out, &newer, &signed4, "1.2.4")).expect("newer");
    assert_eq!(listed(&newer, "aarch64").len(), 2);
    // An older release is refused, and nothing is written.
    let (_, signed2) = w.release("1.2.2");
    let older = w.dir("older");
    std::fs::copy(current.join("etags.txt"), newer.join("etags.txt")).expect("etags");
    let e = build(&request(&w, &newer, &older, &signed2, "1.2.2"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("would move the channel down"), "{e}");
    assert!(!older.exists());
}

#[test]
fn an_index_is_not_built_without_the_etags_or_from_a_package_that_is_not_there() {
    // Catches: an index written without the versions it must be conditional on, and a package
    // of another version taken for this one.
    let w = World::new("index-missing");
    let current = w.read(TESTING);
    let (_, signed) = w.release("1.2.3");
    let out = w.dir("next");
    std::fs::remove_file(current.join("etags.txt")).expect("remove");
    let e = build(&request(&w, &current, &out, &signed, "1.2.3"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("is missing: run `rpm repo read` first"), "{e}");
    std::fs::write(current.join("etags.txt"), "").expect("etags");
    let e = build(&request(&w, &current, &out, &signed, "1.2.9"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("is not in"), "{e}");
    let e = build(&Request {
        keep: 0,
        ..request(&w, &current, &out, &signed, "1.2.3")
    })
    .unwrap_err();
    assert!(matches!(e, Error::Usage(_)), "{e}");
    // A package whose header says another name than its file: not listed.
    let renamed = w.dir("renamed");
    std::fs::create_dir_all(&renamed).expect("dir");
    std::fs::copy(
        signed.join("tierstore-1.2.3-1.x86_64.rpm"),
        renamed.join("tierstore-1.2.9-1.x86_64.rpm"),
    )
    .expect("copy");
    std::fs::copy(
        signed.join("tierstore-1.2.3-1.aarch64.rpm"),
        renamed.join("tierstore-1.2.9-1.aarch64.rpm"),
    )
    .expect("copy");
    let e = build(&request(&w, &current, &out, &renamed, "1.2.9"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("its header does not say the name it has"), "{e}");
}

#[test]
fn a_repository_that_was_read_wrongly_is_refused_by_what_is_wrong() {
    // Catches: a repomd.xml that lacks a file kind or lists a file that is not the one in the
    // directory, built on instead of stopped at.
    let w = World::new("index-damaged");
    w.publish(TESTING, "1.2.3");
    let current = w.read(TESTING);
    let dir = current.join("testing/x86_64/repodata");
    let repomd = std::fs::read_to_string(dir.join("repomd.xml")).expect("repomd");
    std::fs::write(
        dir.join("repomd.xml"),
        repomd.replace("data type=\"other\"", "data type=\"misc\""),
    )
    .expect("write");
    let e = read_repo(&current, TESTING, "x86_64")
        .unwrap_err()
        .to_string();
    assert!(e.contains("lists no other"), "{e}");
    std::fs::write(dir.join("repomd.xml"), &repomd).expect("write");
    let primary = std::fs::read_dir(&dir)
        .expect("dir")
        .flatten()
        .find(|e| e.file_name().to_string_lossy().ends_with("-primary.xml.gz"))
        .expect("primary")
        .path();
    std::fs::write(&primary, b"changed").expect("write");
    let e = read_repo(&current, TESTING, "x86_64")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("does not have the digest its repomd.xml lists"),
        "{e}"
    );
    // A file with the right digest that is not gzip, and one that is not UTF-8.
    for (bytes, expected) in [
        (b"not gzip".to_vec(), "gunzip"),
        (
            crate::channels::rpm::repodata::gzip(&[0xff, 0xfe]),
            "is not UTF-8",
        ),
    ] {
        let name = primary
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let digest = Sha256::of(&bytes).to_string();
        let fixed = repomd
            .replace(name.trim_end_matches("-primary.xml.gz"), &digest)
            .replace(
                &format!(
                    "<checksum type=\"sha256\">{}</checksum>",
                    name.trim_end_matches("-primary.xml.gz")
                ),
                &format!("<checksum type=\"sha256\">{digest}</checksum>"),
            );
        std::fs::write(dir.join(format!("{digest}-primary.xml.gz")), &bytes).expect("write");
        std::fs::write(dir.join("repomd.xml"), fixed).expect("write");
        let e = read_repo(&current, TESTING, "x86_64")
            .unwrap_err()
            .to_string();
        assert!(e.contains(expected), "{expected}: {e}");
    }
}
