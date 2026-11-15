//! What a placement answers about a path: locate, resolve, stat and list, over the small tree
//! of the fixture, the span of the cached pack a file is, and what a damaged tree answers.

use super::files::{SOURCES, Sources};
use super::testing::{Bench, built, fs_at, s3_at, tree_path};
use super::{
    CreateRequest, FileSpan, Kind, Listing, Located, PlacementId, ReadError, Resolved, Stat,
    TreeStore,
};
use crate::{Mode, NewEntry, NewKind, RecordChild, TreeDigest};

const BUCKET: &str = "build-cas";

async fn placed(name: &str) -> (Bench, PlacementId) {
    let bench = Bench::new(name).await;
    bench.fill_all().await;
    let at = vec![s3_at("jobs/1/root/"), fs_at("jobs/1/root")];
    let made = bench.store.create(bench.request(at)).unwrap();
    (bench, made.placement)
}

/// Asserts that a key under the prefix, and a path under the mount location, find the placement
/// and the path below the root, at each depth; that the root is found as the prefix itself and as
/// the location's own path; and that a key of another bucket, one that only begins like the prefix
/// and one above it find nothing.
#[tokio::test]
async fn a_key_or_a_path_finds_the_placement_it_falls_under() {
    let (bench, id) = placed("trees-locate").await;
    let tree = bench.built().root;
    let found = |path: &str| {
        Some(Located {
            placement: id.clone(),
            tree,
            path: path.to_owned(),
        })
    };
    let store = &bench.store;
    assert_eq!(
        store.locate_s3(BUCKET, "jobs/1/root/bin/tool"),
        found("bin/tool")
    );
    assert_eq!(
        store.locate_s3(BUCKET, "jobs/1/root/lib/deep/data"),
        found("lib/deep/data")
    );
    assert_eq!(store.locate_s3(BUCKET, "jobs/1/root/"), found(""));
    assert_eq!(store.locate_s3(BUCKET, "jobs/1/root"), None);
    assert_eq!(store.locate_s3(BUCKET, "jobs/1/rootx/a"), None);
    assert_eq!(store.locate_s3(BUCKET, "jobs/1/"), None);
    assert_eq!(store.locate_s3("other", "jobs/1/root/a"), None);
    assert_eq!(store.locate_fs("jobs/1/root/bin/tool"), found("bin/tool"));
    assert_eq!(store.locate_fs("jobs/1/root"), found(""));
    assert_eq!(store.locate_fs("jobs/1/rootx"), None);
    assert_eq!(store.locate_fs("jobs/1"), None);
    assert_eq!(store.locate_fs(""), None);
}

/// Asserts what each kind of path resolves to: the root and a directory with their modes and entry
/// counts, a file with its mode (the setuid bit kept), size, digest and the pack and offset its
/// bytes are at, a symbolic link with its target, the empty file with no span, and the empty
/// directory.
#[tokio::test]
async fn resolve_says_what_a_path_is_and_where_a_files_bytes_are() {
    let (bench, id) = placed("trees-resolve").await;
    let pack = bench.pack.key.clone();
    let blob = |i: usize| {
        let (sha, offset, size) = &bench.pack.blobs[i];
        (TreeDigest::from_hex(sha).unwrap(), *offset, *size)
    };
    let at = |path: &str| bench.store.resolve(&id, path).unwrap();
    assert_eq!(
        at(""),
        Resolved::Dir {
            mode: Mode::DIR,
            entries: 5
        }
    );
    assert_eq!(
        at("bin"),
        Resolved::Dir {
            mode: Mode::DIR,
            entries: 2
        }
    );
    assert_eq!(
        at("empty"),
        Resolved::Dir {
            mode: Mode::DIR,
            entries: 0
        }
    );
    let (sha, offset, size) = blob(0);
    assert_eq!(
        at("bin/tool"),
        Resolved::File {
            mode: Mode::new(0o4755).unwrap(),
            size,
            sha256: sha,
            span: Some(FileSpan {
                pack: pack.clone(),
                offset
            }),
        }
    );
    let (sha, offset, size) = blob(2);
    assert_eq!(
        at("lib/deep/data"),
        Resolved::File {
            mode: Mode::FILE,
            size,
            sha256: sha,
            span: Some(FileSpan { pack, offset }),
        }
    );
    assert_eq!(
        at("link"),
        Resolved::Symlink {
            target: "bin/tool".to_owned()
        }
    );
    assert_eq!(
        at("zero"),
        Resolved::File {
            mode: Mode::FILE,
            size: 0,
            sha256: TreeDigest::of(b""),
            span: None
        }
    );
}

/// Asserts that stat gives the kind, the size and the mode with no source read: a directory's size
/// is 0, a link's is its target's length and its mode is 0777.
#[tokio::test]
async fn stat_gives_kind_size_and_mode() {
    let (bench, id) = placed("trees-stat").await;
    let stat = |path: &str| bench.store.stat(&id, path).unwrap();
    assert_eq!(
        stat("lib"),
        Stat {
            kind: Kind::Dir,
            size: 0,
            mode: Mode::DIR
        }
    );
    assert_eq!(
        stat("bin/tool"),
        Stat {
            kind: Kind::File,
            size: 1000,
            mode: Mode::new(0o4755).unwrap()
        }
    );
    assert_eq!(
        stat("link"),
        Stat {
            kind: Kind::Symlink,
            size: 8,
            mode: Mode::SYMLINK
        }
    );
    assert_eq!(
        stat("zero"),
        Stat {
            kind: Kind::File,
            size: 0,
            mode: Mode::FILE
        }
    );
    assert_eq!(bench.store.stat(&id, "").unwrap().kind, Kind::Dir);
}

/// Asserts the refusals of a path: one that is not there, one below a file or a link, a name
/// with an empty part, and a placement that is not there; each of its kind.
#[tokio::test]
async fn a_path_that_is_not_there_is_not_found_and_one_below_a_file_is_not_a_directory() {
    let (bench, id) = placed("trees-notfound").await;
    let store = &bench.store;
    for path in ["nope", "bin/nope", "bin//tool", "Bin", "lib/deep/x"] {
        assert_eq!(
            store.resolve(&id, path),
            Err(ReadError::NotFound),
            "{path:?}"
        );
        assert_eq!(store.stat(&id, path), Err(ReadError::NotFound), "{path:?}");
    }
    for path in ["bin/tool/x", "bin/tool/", "link/x", "zero/x"] {
        assert_eq!(
            store.resolve(&id, path),
            Err(ReadError::NotADirectory),
            "{path:?}"
        );
    }
    let nothing = PlacementId::parse(&"ab".repeat(16)).unwrap();
    assert_eq!(store.resolve(&nothing, ""), Err(ReadError::NoPlacement));
    assert_eq!(store.stat(&nothing, ""), Err(ReadError::NoPlacement));
    assert_eq!(
        store.list(&nothing, "", None, 10),
        Err(ReadError::NoPlacement)
    );
    assert!(ReadError::NoPlacement.to_string().contains("placement"));
}

fn names(listing: &Listing) -> Vec<&str> {
    listing.entries.iter().map(RecordChild::name).collect()
}

/// Asserts that a listing is the children of the directory in name order, pages of the limit's
/// size with the name to continue after, a limit of 0 taken as 1, a name after the last giving
/// none, a directory with none, and that a file is not listed.
#[tokio::test]
async fn list_pages_the_children_in_name_order() {
    let (bench, id) = placed("trees-list").await;
    let list = |after: Option<&str>, limit: usize| bench.store.list(&id, "", after, limit).unwrap();
    let all = list(None, 100);
    assert_eq!(names(&all), ["bin", "empty", "lib", "link", "zero"]);
    assert_eq!(all.next, None);
    let first = list(None, 2);
    assert_eq!(names(&first), ["bin", "empty"]);
    assert_eq!(first.next.as_deref(), Some("empty"));
    let second = list(first.next.as_deref(), 2);
    assert_eq!(names(&second), ["lib", "link"]);
    let last = list(second.next.as_deref(), 2);
    assert_eq!(names(&last), ["zero"]);
    assert_eq!(last.next, None);
    assert_eq!(names(&list(Some("a"), 0)), ["bin"]);
    assert_eq!(names(&list(Some("zero"), 5)), [] as [&str; 0]);
    assert_eq!(names(&list(Some("bin"), 1)), ["empty"]);
    let deep = bench.store.list(&id, "lib/deep", None, 5).unwrap();
    assert_eq!(names(&deep), ["data"]);
    assert!(matches!(
        deep.entries[0],
        RecordChild::File { size: 3000, .. }
    ));
    assert!(
        bench
            .store
            .list(&id, "empty", None, 5)
            .unwrap()
            .entries
            .is_empty()
    );
    assert_eq!(
        bench.store.list(&id, "bin/tool", None, 5),
        Err(ReadError::NotADirectory)
    );
    assert_eq!(
        bench.store.list(&id, "none", None, 5),
        Err(ReadError::NotFound)
    );
}

/// Asserts that two files that are members of one pack resolve to spans of one pack key at their
/// own offsets, that the cache's own file of that pack serves both (the same file, at those
/// offsets, whole or not), and that no file of the tree is a copy: the cache directory holds the
/// pack's entry and nothing for the files.
#[tokio::test]
async fn two_members_of_one_pack_resolve_to_two_spans_of_one_cache_file() {
    let (bench, id) = placed("trees-spans").await;
    let span = |path: &str| match bench.store.resolve(&id, path).unwrap() {
        Resolved::File { size, span, .. } => (span.unwrap(), size),
        other => panic!("{other:?}"),
    };
    let ((one, one_size), (two, two_size)) = (span("bin/tool"), span("bin/other"));
    assert_eq!(
        (one.pack.as_str(), two.pack.as_str()),
        (bench.pack.key.as_str(), bench.pack.key.as_str())
    );
    assert_eq!((one.offset, two.offset), (0, 1000));
    let cache = &bench.rig.cache;
    let first = cache.span_of(&one.pack, one.offset, one_size).unwrap();
    let second = cache.span_of(&two.pack, two.offset, two_size).unwrap();
    assert_eq!(first.path, second.path);
    assert_eq!((first.offset, second.offset), (0, 1000));
    let file = std::fs::read(&first.path).unwrap();
    assert_eq!(file[..1000], bench.pack.bytes()[..1000]);
    assert_eq!(file[1000..3000], bench.pack.bytes()[1000..3000]);
    assert_eq!(cache.extents_held(), (0, 0));
    assert_eq!(cache.held().1, 1);
}

/// Asserts that a file below a tree held by digest resolves through the child's own records and
/// sources, and that the child's directory is read for it, not the parent's.
#[tokio::test]
async fn a_path_below_a_child_tree_resolves_through_the_childs_files() {
    let bench = Bench::new("trees-child-read").await;
    bench.fill_all().await;
    let base = bench
        .store
        .create(bench.request(vec![fs_at("base")]))
        .unwrap();
    let parent = bench
        .store
        .create(CreateRequest {
            built: built(vec![NewEntry {
                path: "tc".to_owned(),
                kind: NewKind::Tree { digest: base.tree },
            }]),
            at: vec![fs_at("jobs/1")],
            ttl: None,
            ready: false,
        })
        .unwrap();
    let id = parent.placement;
    let through = bench.store.resolve(&id, "tc/bin/other").unwrap();
    let direct = bench.store.resolve(&base.placement, "bin/other").unwrap();
    assert_eq!(through, direct);
    assert!(matches!(
        through,
        Resolved::File {
            size: 2000,
            span: Some(_),
            ..
        }
    ));
    assert_eq!(
        bench.store.resolve(&id, "tc").unwrap(),
        bench.store.resolve(&base.placement, "").unwrap()
    );
    assert_eq!(
        bench.store.list(&id, "tc", None, 9).unwrap().entries.len(),
        5
    );
    assert_eq!(bench.store.list(&id, "", None, 9).unwrap().entries.len(), 1);
}

/// Asserts what a tree whose files were damaged after the store read them answers: a records file
/// that is gone, or cut so that a record does not read, gives the `Corrupt` answer naming the record,
/// never a panic or a wrong answer.
#[tokio::test]
async fn a_tree_whose_records_were_damaged_answers_corrupt() {
    let (bench, id) = placed("trees-damaged").await;
    let tree = bench.built().root;
    let file = tree_path(&bench.rig, &tree).join(super::files::RECORDS);
    let good = std::fs::read(&file).unwrap();
    let mut cut = good.clone();
    cut.truncate(cut.len() - 4);
    std::fs::write(&file, &cut).unwrap();
    assert!(matches!(
        bench.store.resolve(&id, "lib/deep"),
        Err(ReadError::Corrupt(_))
    ));
    std::fs::write(&file, &good).unwrap();
    assert!(bench.store.resolve(&id, "lib/deep").is_ok());
    // The record of `lib/deep` no longer begins with its magic.
    let mut garbled = good;
    let places = super::files::parse_records(&garbled).unwrap();
    let deep = bench.store.resolve(&id, "lib").unwrap();
    assert!(matches!(deep, Resolved::Dir { entries: 1, .. }));
    for (_, place) in &places {
        garbled[place.offset as usize] = b'X';
    }
    std::fs::write(&file, &garbled).unwrap();
    assert!(matches!(
        bench.store.resolve(&id, "lib/deep"),
        Err(ReadError::Corrupt(_))
    ));
    std::fs::remove_file(&file).unwrap();
    let gone = bench.store.resolve(&id, "").unwrap_err();
    assert!(
        matches!(&gone, ReadError::Corrupt(why) if why.contains("record ")),
        "{gone:?}"
    );
    assert!(gone.to_string().contains("damaged"));
}

/// Asserts that a file whose digest the tree's sources do not list (a sources file that lost it)
/// is answered `Corrupt`, and that the same file as an empty one needs none.
#[tokio::test]
async fn a_file_with_no_source_answers_corrupt() {
    let (bench, id) = placed("trees-nosource").await;
    let tree = bench.built().root;
    let path = tree_path(&bench.rig, &tree).join(SOURCES);
    let mut sources = Sources::parse(&std::fs::read(&path).unwrap()).unwrap();
    let (sha, _, _) = &bench.pack.blobs[0];
    let sha = *TreeDigest::from_hex(sha).unwrap().as_bytes();
    sources.files.remove(&sha);
    std::fs::write(&path, sources.encode()).unwrap();
    let store = TreeStore::open(bench.rig.reopen(), super::TreesConfig::default()).unwrap();
    let why = store.resolve(&id, "bin/tool").unwrap_err();
    assert!(
        matches!(&why, ReadError::Corrupt(text) if text.starts_with("no source for")),
        "{why:?}"
    );
    assert!(store.resolve(&id, "bin/other").is_ok());
    assert!(store.resolve(&id, "zero").is_ok());
}
