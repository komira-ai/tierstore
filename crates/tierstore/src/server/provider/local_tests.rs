//! The provider with `local_trees`: a tree hydrated from the cache into plain files equal to its
//! list, version 1 and version 2; one write per key, shared by every source and by a new process;
//! the answer while a tree is written; a write ended half way, which leaves nothing another
//! account can read and nothing a source is answered with; and each refusal and failure by its
//! exact words, after which nothing of the tree is left.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tierstore_testkit::dir::TestDir;

use super::local::{self, LocalTrees};
use super::sources::Settings;
use super::testing::{READ_ONLY, Rig, list_of, member, refused, source, unavailable};
use super::{DEFAULT_FETCH_TIMEOUT, Provider, ProviderFlags};
use crate::server::local_cache::pack_testing::Pack;
use crate::server::local_cache::testing::until;
use crate::server::local_cache::trees::TreesConfig;
use crate::{
    BlobSource, Error, ListEntry, ListKind, Mode, Op, TreeDigest, TreeList, tree_list_digest,
    tree_list_key,
};

/// What a written tree holds at one path.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Dir(u32),
    File(u32, Vec<u8>),
    Link(String),
}

/// Every entry below `root`, by its path from `root`, with its permission bits and its bytes or
/// target; links are not followed.
fn seen(root: &Path) -> BTreeMap<String, Seen> {
    let mut found = BTreeMap::new();
    let mut pending = vec![PathBuf::new()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(root.join(&dir)).expect("a directory of the tree") {
            let entry = entry.expect("an entry");
            let below = dir.join(entry.file_name());
            let meta = std::fs::symlink_metadata(entry.path()).expect("its metadata");
            let mode = meta.permissions().mode() & 0o7777;
            let what = if meta.is_dir() {
                pending.push(below.clone());
                Seen::Dir(mode)
            } else if meta.is_symlink() {
                let target = std::fs::read_link(entry.path()).expect("a target");
                Seen::Link(target.to_string_lossy().into_owned())
            } else {
                Seen::File(mode, std::fs::read(entry.path()).expect("the bytes"))
            };
            found.insert(below.to_string_lossy().into_owned(), what);
        }
    }
    found
}

/// The tree [`list_of`] the pack makes, as it must be on disk: every directory 0755 (the listed
/// 01777 of `empty` too), files 0755 or 0644 with the pack's bytes, the link as listed.
fn wanted(pack: &Pack) -> BTreeMap<String, Seen> {
    let file = |mode: u32, blob: usize| Seen::File(mode, pack.content(blob).to_vec());
    BTreeMap::from([
        ("bin".to_owned(), Seen::Dir(0o755)),
        ("bin/other".to_owned(), file(0o644, 1)),
        ("bin/tool".to_owned(), file(0o755, 0)),
        ("empty".to_owned(), Seen::Dir(0o755)),
        ("lib".to_owned(), Seen::Dir(0o755)),
        ("lib/deep".to_owned(), Seen::Dir(0o755)),
        ("lib/deep/data".to_owned(), file(0o644, 2)),
        ("link".to_owned(), Seen::Link("bin/tool".to_owned())),
        ("zero".to_owned(), Seen::File(0o644, Vec::new())),
    ])
}

/// The names in `root`, sorted.
fn names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .map(|entry| entry.expect("an entry").file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// A provider that writes whole trees into a directory of its own scratch directory, over the
/// tree store's bench.
struct Local {
    /// Declared first: removed after the provider and its cache are gone.
    dir: TestDir,
    rig: Rig,
}

impl Local {
    async fn new(name: &str) -> Self {
        Self::with(name, |_| {}).await
    }

    /// The same, with the settings changed by `change` after `local_trees` is set.
    async fn with(name: &str, change: impl FnOnce(&mut Settings)) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let root = dir.path().join("job-trees");
        let rig = Rig::with(name, TreesConfig::default(), |settings| {
            settings.local_trees = Some(root);
            change(settings);
        })
        .await;
        Self { dir, rig }
    }

    /// The directory of whole trees.
    fn root(&self) -> PathBuf {
        self.dir.path().join("job-trees")
    }

    /// The directory of the whole tree of the list `key`.
    fn whole(&self, key: &str) -> PathBuf {
        self.root().join(digest_of(key).to_string())
    }

    /// The directory a write of the tree of the list `key` lays it out in.
    fn partial(&self, key: &str) -> PathBuf {
        self.root().join(format!(".partial-{}", digest_of(key)))
    }

    /// A second provider over the same cache, store and directory: this node's next process.
    fn restarted(&self) -> Provider {
        let mut settings = super::testing::settings();
        settings.local_trees = Some(self.root());
        Provider::with(
            self.rig.bench.store.clone(),
            self.rig.bench.rig.cache.clone(),
            settings,
        )
    }

    /// How many writes the provider made.
    fn writes(provider: &Provider) -> u64 {
        provider
            .shared
            .local
            .as_ref()
            .expect("local trees")
            .written
            .load(Ordering::SeqCst)
    }

    /// Prepares the source `index` of `run` with the list `key`, and mounts it.
    async fn mounted(&self, provider: &Provider, run: &str, index: u32, key: &str) -> PathBuf {
        provider
            .prepare(source(run, index), key, READ_ONLY)
            .expect("prepared");
        provider.mount(source(run, index)).await.expect("mounted")
    }

    /// Prepares the source 0 of `run` with the list `key`, and what its first `Mount` answers.
    async fn first_mount(&self, run: &str, key: &str) -> Result<PathBuf, super::ProviderError> {
        let provider = &self.rig.provider;
        provider
            .prepare(source(run, 0), key, READ_ONLY)
            .expect("prepared");
        provider.mount(source(run, 0)).await
    }

    /// GETs of the pack at the upstream.
    fn pack_gets(&self) -> u64 {
        self.rig
            .bench
            .rig
            .calls_on(Op::Get, &self.rig.bench.pack.key)
    }
}

/// The digest the list key `key` names.
fn digest_of(key: &str) -> TreeDigest {
    tree_list_digest(key).expect("a list key")
}

/// The list of one file, `path`, whose bytes are the blob `blob` of `pack`.
fn one_file(pack: &Pack, path: &str, blob: usize) -> TreeList {
    TreeList {
        entries: vec![member(pack, path, blob, 0o644)],
    }
}

/// A file of `size` bytes at `offset` of `key`, with the digest `sha256`.
fn file_at(path: &str, key: &str, offset: u64, size: u64, sha256: TreeDigest) -> ListEntry {
    ListEntry {
        path: path.to_owned(),
        kind: ListKind::File {
            mode: Mode::new(0o644).expect("a mode"),
            size,
            source: Some(BlobSource {
                key: Arc::from(key),
                offset,
            }),
            sha256: Some(sha256),
        },
    }
}

/// Asserts that a source of a version 1 list is answered the directory named by the list's digest
/// under `local_trees`, holding exactly the list's tree as plain files (the bytes of each file
/// those of its span, the modes 0755 or 0644, the link as written) and nothing else; that no
/// partial directory is left.
#[tokio::test]
async fn a_tree_is_hydrated_from_the_cache_into_plain_files_equal_to_its_list() {
    let local = Local::new("local-v1").await;
    let path = local.rig.mounted("js.1", 0).await;
    assert_eq!(path, local.whole(&local.rig.key));
    assert_eq!(seen(&path), wanted(&local.rig.bench.pack));
    let mode = std::fs::metadata(&path)
        .expect("the tree")
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);
    assert_eq!(
        names(&local.root()),
        [digest_of(&local.rig.key).to_string()]
    );
}

/// Asserts that a version 2 list of the same tree is written as the same plain files.
#[tokio::test]
async fn a_version_2_list_is_written_as_the_same_tree() {
    let local = Local::new("local-v2").await;
    let key = local.rig.put_list_v2(&list_of(&local.rig.bench.pack)).await;
    let path = local.mounted(&local.rig.provider, "js.1", 0, &key).await;
    assert_eq!(path, local.whole(&key));
    assert_eq!(seen(&path), wanted(&local.rig.bench.pack));
}

/// Asserts that a second source naming the key, of another run, is answered the same directory
/// without a second write, and that a new process over the same directory answers it too without
/// writing it or reading the pack again.
#[tokio::test]
async fn a_second_mount_of_the_key_reuses_the_written_tree() {
    let local = Local::new("local-share").await;
    let first = local.rig.mounted("js.1", 0).await;
    let read = local.pack_gets();
    let second = local.rig.mounted("js.2", 3).await;
    assert_eq!(second, first);
    assert_eq!(Local::writes(&local.rig.provider), 1);
    let next = local.restarted();
    let third = local.mounted(&next, "js.3", 0, &local.rig.key).await;
    assert_eq!(third, first);
    assert_eq!(Local::writes(&next), 0);
    assert_eq!(local.pack_gets(), read);
    assert_eq!(seen(&first), wanted(&local.rig.bench.pack));
}

/// Asserts that while a tree is being written (its pack's reads held) `Mount` answers "not ready"
/// and no directory of the key is there but the partial one, mode 0700; that a second source of
/// the key prepared meanwhile joins that write; and that once the reads go on both are answered
/// the whole tree, written once.
#[tokio::test]
async fn concurrent_prepares_of_one_key_write_once_and_mount_waits_for_the_whole_tree() {
    let local = Local::with("local-join", |settings| {
        settings.mount_wait = Duration::from_millis(50);
    })
    .await;
    let provider = &local.rig.provider;
    let gate = local
        .rig
        .bench
        .rig
        .tamper
        .hold_key(&local.rig.bench.pack.key);
    let key = local.rig.key.clone();
    provider
        .prepare(source("js.1", 0), &key, READ_ONLY)
        .expect("prepared");
    until("a read of the pack is at the upstream", || {
        local.pack_gets() > 0
    })
    .await;
    provider
        .prepare(source("js.2", 0), &key, READ_ONLY)
        .expect("prepared");
    for run in ["js.1", "js.2"] {
        let why = unavailable(provider.mount(source(run, 0)).await.expect_err("not ready"));
        assert_eq!(why, "not ready");
    }
    let partial = local.partial(&key);
    let mode = std::fs::metadata(&partial)
        .expect("the partial tree")
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o700);
    assert!(!local.whole(&key).exists());
    gate.add_permits(64);
    let mut answers = Vec::new();
    for run in ["js.1", "js.2"] {
        answers.push(until_whole(provider, run).await);
    }
    let whole = local.whole(&key);
    assert_eq!(answers, [whole.clone(), whole.clone()]);
    assert_eq!(Local::writes(provider), 1);
    assert_eq!(seen(&whole), wanted(&local.rig.bench.pack));
    assert!(!partial.exists());
}

/// Asserts that a write ended half way, as a crash of the process ends it (its task dropped while
/// a file's read is held), leaves no directory of the key but the partial one, mode 0700 so that
/// no other account reads it, which a `Mount` is never answered with; that the next process
/// removes it at its start; and that it then writes the tree whole.
#[tokio::test]
async fn a_crash_mid_write_leaves_no_readable_partial_and_the_next_process_writes_it_whole() {
    let local = Local::new("local-crash").await;
    let gate = local
        .rig
        .bench
        .rig
        .tamper
        .hold_key(&local.rig.bench.pack.key);
    let key = local.rig.key.clone();
    let shared = Arc::clone(&local.rig.provider.shared);
    let trees = Arc::clone(shared.local.as_ref().expect("local trees"));
    let (task_key, digest) = (key.clone(), digest_of(&key));
    let writing =
        tokio::spawn(async move { local::write(&shared, &trees, &task_key, digest).await });
    until("a read of the pack is at the upstream", || {
        local.pack_gets() > 0
    })
    .await;
    writing.abort();
    assert!(writing.await.expect_err("ended").is_cancelled());
    let partial = local.partial(&key);
    let mode = std::fs::metadata(&partial)
        .expect("the partial tree")
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o700);
    assert!(!local.whole(&key).exists());
    gate.add_permits(64);
    let next = local.restarted();
    assert!(!partial.exists());
    let path = local.mounted(&next, "js.1", 0, &key).await;
    assert_eq!(path, local.whole(&key));
    assert_eq!(seen(&path), wanted(&local.rig.bench.pack));
    assert_eq!(names(&local.root()), [digest_of(&key).to_string()]);
}

/// Asserts the refusals of a ref whose tree cannot be written, each by its exact words, and that
/// after each nothing of the tree is left under `local_trees`: no tree list at the key, a version
/// 1 list the format refuses, a version 2 list its walk refuses, an entry below a file or a link (which
/// would write through it, outside the tree), a file whose object is missing, a file whose bytes do not hash
/// to its digest, and a file longer than its object has bytes.
#[tokio::test]
async fn a_tree_that_cannot_be_written_is_refused_by_name_and_leaves_nothing() {
    let local = Local::new("local-refused").await;
    let pack = &local.rig.bench.pack;
    let (blob_sha, blob_offset, blob_size) = pack.blobs[2].clone();
    let blob_sha = TreeDigest::from_hex(&blob_sha).expect("a digest");
    let absent = tree_list_key(b"never stored");
    let not_a_list = local.rig.put_bytes(b"not a list\n").await;
    let v2 = list_of(pack).encode_v2().expect("an encoding");
    let cut = local.rig.put_bytes(&v2[..v2.len() - 1]).await;
    let below = local
        .rig
        .put_list(&TreeList {
            entries: vec![member(pack, "a", 0, 0o644), member(pack, "a/b", 1, 0o644)],
        })
        .await;
    let below_link = local
        .rig
        .put_list(&TreeList {
            entries: vec![
                ListEntry {
                    path: "l".to_owned(),
                    kind: ListKind::Symlink {
                        target: "/".to_owned(),
                    },
                },
                member(pack, "l/x", 1, 0o644),
            ],
        })
        .await;
    let missing = local
        .rig
        .put_list(&TreeList {
            entries: vec![file_at("f", "cas/pack/none", 0, blob_size, blob_sha)],
        })
        .await;
    let wrong_sha = TreeDigest::of(b"other bytes");
    let mismatched = local
        .rig
        .put_list(&TreeList {
            entries: vec![file_at("f", &pack.key, blob_offset, blob_size, wrong_sha)],
        })
        .await;
    let long = local
        .rig
        .put_list(&TreeList {
            entries: vec![file_at(
                "f",
                &pack.key,
                blob_offset,
                blob_size + 7,
                blob_sha,
            )],
        })
        .await;
    let cases = [
        (&absent, format!("no tree list at {absent}")),
        (
            &not_a_list,
            "tree list line 1: malformed: expected ident at line 1 column 2".to_owned(),
        ),
        (
            &cut,
            "tree list header: the range section is past the end".to_owned(),
        ),
        (
            &below,
            "\"a/b\" is below \"a\", which is not a directory".to_owned(),
        ),
        (
            &below_link,
            "\"l/x\" is below \"l\", which is not a directory".to_owned(),
        ),
        (&missing, "\"f\": no object cas/pack/none".to_owned()),
        (
            &mismatched,
            format!("\"f\": the bytes hash to {blob_sha}, not to the list's digest"),
        ),
        (
            &long,
            format!(
                "\"f\" is {} bytes in the list, and {blob_size} were read",
                blob_size + 7
            ),
        ),
    ];
    for (run, (key, words)) in cases.iter().enumerate() {
        let why = refused(
            local
                .first_mount(&format!("js.{run}"), key)
                .await
                .expect_err("refused"),
        );
        assert_eq!(&why, words, "{key}");
        assert!(
            names(&local.root()).is_empty(),
            "{key}: {:?}",
            names(&local.root())
        );
    }
}

/// Asserts that a file whose read passes `fetch_timeout` fails the write, which `Mount` answers
/// unavailable by the file's name, leaving nothing; and that the next `Mount`, once the reads go
/// on, writes the tree again from the start (asked again until it is whole) and answers it whole.
#[tokio::test]
async fn a_read_past_its_bound_fails_the_write_and_the_next_mount_writes_it_again() {
    let local = Local::with("local-bound", |settings| {
        settings.fill_timeout = Duration::from_millis(20);
    })
    .await;
    let pack = &local.rig.bench.pack;
    let key = local.rig.put_list(&one_file(pack, "only", 1)).await;
    let gate = local.rig.bench.rig.tamper.hold_key(&pack.key);
    let why = unavailable(local.first_mount("js.1", &key).await.expect_err("failed"));
    assert_eq!(why, "\"only\" was not read within 20ms");
    assert!(names(&local.root()).is_empty());
    gate.add_permits(64);
    let path = until_whole(&local.rig.provider, "js.1").await;
    assert_eq!(
        seen(&path),
        BTreeMap::from([(
            "only".to_owned(),
            Seen::File(0o644, pack.content(1).to_vec())
        )])
    );
    // The write was begun again: at least once more (a slow host may pass the bound again).
    assert!(Local::writes(&local.rig.provider) >= 2);
}

/// The tree a `Mount` of the source 0 of `run` answers, asking again while it answers
/// unavailable (a write under way, or one begun again).
async fn until_whole(provider: &Provider, run: &str) -> PathBuf {
    loop {
        match provider.mount(source(run, 0)).await {
            Ok(path) => return path,
            Err(super::ProviderError::Unavailable(_)) => tokio::task::yield_now().await,
            Err(other) => panic!("expected the tree, got {other:?}"),
        }
    }
}

/// Asserts that a read of a file the upstream fails, for a reason that may pass, fails the write by
/// the file's name and the reason, leaving nothing.
#[tokio::test]
async fn a_read_the_upstream_fails_fails_the_write_by_the_files_name() {
    let local = Local::new("local-upstream").await;
    let pack = &local.rig.bench.pack;
    let key = local.rig.put_list(&one_file(pack, "only", 1)).await;
    local.rig.bench.rig.upstream.fail_next_on(
        Op::Get,
        &pack.key,
        Error::Full("the upstream is full".to_owned()),
    );
    let why = unavailable(local.first_mount("js.1", &key).await.expect_err("failed"));
    assert!(why.starts_with("\"only\" was not read: "), "{why}");
    assert!(why.contains("the upstream is full"), "{why}");
    assert!(names(&local.root()).is_empty());
}

/// Asserts that a provider that is stopping writes no file: the write fails, saying so.
#[tokio::test]
async fn a_stopping_provider_fails_the_write() {
    let local = Local::new("local-halt").await;
    local.rig.provider.halt();
    let why = unavailable(
        local
            .first_mount("js.1", &local.rig.key)
            .await
            .expect_err("failed"),
    );
    assert_eq!(why, "the provider is stopping");
    assert!(names(&local.root()).is_empty());
}

/// Asserts the failures of the directories a write needs, by their exact words: a `local_trees`
/// that cannot be made (below a file), and a leftover at the partial path that is not a directory
/// and so cannot be removed (which the start of the provider says in the log and leaves).
#[tokio::test]
async fn a_directory_the_write_cannot_make_fails_it_by_name() {
    let dir = TestDir::under(std::env::temp_dir(), "local-dirs");
    let file = dir.path().join("file");
    std::fs::write(&file, b"").expect("a file");
    let below_file = file.join("job-trees");
    let rig = Rig::with("local-dirs-a", TreesConfig::default(), |settings| {
        settings.local_trees = Some(below_file.clone());
    })
    .await;
    let why = unavailable(
        rig.provider
            .prepare(source("js.1", 0), &rig.key, READ_ONLY)
            .map(|()| rig.provider.mount(source("js.1", 0)))
            .expect("prepared")
            .await
            .expect_err("failed"),
    );
    assert_eq!(
        why,
        format!(
            "{} cannot be made: Not a directory (os error 20)",
            below_file.display()
        )
    );
    let root = dir.path().join("trees");
    std::fs::create_dir(&root).expect("the trees");
    let leftover = root.join(format!(".partial-{}", digest_of(&rig.key)));
    std::fs::write(&leftover, b"").expect("a leftover");
    let rig = Rig::with("local-dirs-b", TreesConfig::default(), |settings| {
        settings.local_trees = Some(root.clone());
    })
    .await;
    assert!(leftover.is_file());
    rig.provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .expect("prepared");
    let why = unavailable(
        rig.provider
            .mount(source("js.1", 0))
            .await
            .expect_err("failed"),
    );
    assert_eq!(
        why,
        format!(
            "{} is left by a write before and cannot be removed: Not a directory (os error 20)",
            leftover.display()
        )
    );
}

/// Asserts that a whole tree removed by hand while a source holds it is written again: the next
/// `Mount` answers that it is gone and being prepared again, and a later one the whole tree.
#[tokio::test]
async fn a_tree_removed_by_hand_is_written_again() {
    let local = Local::new("local-gone").await;
    let path = local.rig.mounted("js.1", 0).await;
    std::fs::remove_dir_all(&path).expect("removed");
    let why = unavailable(
        local
            .rig
            .provider
            .mount(source("js.1", 0))
            .await
            .expect_err("gone"),
    );
    assert_eq!(why, "the tree is gone: it is being prepared again");
    let path = until_whole(&local.rig.provider, "js.1").await;
    assert_eq!(seen(&path), wanted(&local.rig.bench.pack));
    assert_eq!(Local::writes(&local.rig.provider), 2);
}

/// Asserts what `Release`, `Sweep` and an unknown source do with local trees: a source this
/// process never prepared is "not prepared here", even with a placement of the tree store at its
/// path (a node's earlier answer under `tierstore fs`); a released source is no longer prepared
/// here; a sweep releases the agent's other runs and that placement; and the tree stays for the
/// next source.
#[tokio::test]
async fn release_and_sweep_end_sources_and_keep_the_tree() {
    let local = Local::new("local-release").await;
    let provider = &local.rig.provider;
    let store = &local.rig.bench.store;
    let request = local.rig.bench.request(vec![
        super::names::SourceId::of(source("js.9", 0))
            .expect("a source")
            .location(),
    ]);
    store.create(request).expect("a placement");
    let why = unavailable(
        provider
            .mount(source("js.9", 0))
            .await
            .expect_err("unknown"),
    );
    assert_eq!(why, "the source is not prepared here");
    let path = local.rig.mounted("js.1", 0).await;
    local.rig.mounted("js.2", 0).await;
    provider.release(source("js.1", 0)).expect("released");
    let why = unavailable(
        provider
            .mount(source("js.1", 0))
            .await
            .expect_err("released"),
    );
    assert_eq!(why, "the source is not prepared here");
    let swept = provider.sweep(super::testing::AGENT, &[]).expect("swept");
    assert_eq!(swept, 2);
    let why = unavailable(provider.mount(source("js.2", 0)).await.expect_err("swept"));
    assert_eq!(why, "the source is not prepared here");
    assert_eq!(store.locate_fs(&Rig::place_of("js.9", 0)), None);
    assert_eq!(seen(&path), wanted(&local.rig.bench.pack));
    let again = local.rig.mounted("js.3", 0).await;
    assert_eq!(again, path);
}

/// Asserts that a provider made from an interface's flags with `local_trees` writes whole trees
/// there, whatever the node's `trees.prepare_prefetch`.
#[tokio::test]
async fn a_provider_of_the_interfaces_flags_writes_local_trees() {
    let dir = TestDir::under(std::env::temp_dir(), "local-flags");
    let config = TreesConfig {
        prepare_prefetch: crate::server::local_cache::trees::PreparePrefetch::None,
        ..TreesConfig::default()
    };
    let rig = Rig::with("local-flags", config, |_| {}).await;
    let flags = ProviderFlags {
        socket: dir.path().join("provider.sock"),
        mountpoint: PathBuf::from("/unused"),
        fetch_timeout: DEFAULT_FETCH_TIMEOUT,
        local_trees: Some(dir.path().join("trees")),
    };
    let provider = Provider::new(rig.bench.store.clone(), rig.bench.rig.cache.clone(), &flags);
    provider
        .prepare(source("js.1", 0), &rig.key, READ_ONLY)
        .expect("prepared");
    let path = provider.mount(source("js.1", 0)).await.expect("mounted");
    assert_eq!(
        path,
        dir.path()
            .join("trees")
            .join(digest_of(&rig.key).to_string())
    );
    assert_eq!(seen(&path), wanted(&rig.bench.pack));
}

/// Asserts the log line of a provider of local trees names the directory and what it does.
#[test]
fn the_log_says_where_whole_trees_are_written() {
    assert_eq!(
        local::mode_line(Path::new("/usr/local/tierstore/job-trees")),
        "tierstore: mount provider: local_trees = /usr/local/tierstore/job-trees: Prepare writes each tree whole there, and Mount answers it once it is whole"
    );
}

/// Asserts that a name the file system already has is a refusal that says the tree has two names
/// it takes as one (a Mac's volume, which ignores case, takes `A` and `a` so), and any other
/// failure to make an entry is a failure naming it.
#[test]
fn a_name_the_file_system_already_has_is_a_refusal() {
    let exists = std::io::Error::from(std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        local::made("lib/A", &exists),
        super::sources::Stage::Refused(
            "\"lib/A\" is a name this file system already has: the tree has two names it takes as one"
                .to_owned()
        )
    );
    let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    assert_eq!(
        local::made("lib/A", &denied),
        super::sources::Stage::Failed("\"lib/A\" cannot be made: permission denied".to_owned())
    );
}

/// Asserts the end of a write: the partial tree is renamed whole with mode 0755; a tree another
/// process made whole first is kept and the partial one removed; and a path that is a file where
/// the tree goes fails it.
#[test]
fn a_finished_tree_is_renamed_whole_unless_one_is_there() {
    let dir = TestDir::under(std::env::temp_dir(), "local-finish");
    let (partial, whole) = (dir.path().join(".partial-k"), dir.path().join("k"));
    std::fs::create_dir_all(partial.join("sub")).expect("a partial tree");
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o700)).expect("0700");
    local::finish(&partial, &whole).expect("finished");
    assert!(!partial.exists());
    let mode = std::fs::metadata(&whole)
        .expect("whole")
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o755);
    std::fs::create_dir_all(partial.join("other")).expect("a second partial tree");
    local::finish(&partial, &whole).expect("kept");
    assert!(!partial.exists());
    assert_eq!(names(&whole), ["sub"]);
    let file = dir.path().join("f");
    std::fs::write(&file, b"").expect("a file");
    std::fs::create_dir(&partial).expect("a third partial tree");
    assert!(local::finish(&partial, &file).is_err());
    assert!(partial.exists());
}

/// Asserts that the start of a provider removes the partial trees a process before it left, and
/// keeps the whole trees and anything else.
#[test]
fn the_start_removes_partial_trees_only() {
    let dir = TestDir::under(std::env::temp_dir(), "local-open");
    for name in [".partial-a", ".partial-b/sub", "whole/sub"] {
        std::fs::create_dir_all(dir.path().join(name)).expect("a directory");
    }
    std::fs::write(dir.path().join("note"), b"").expect("a file");
    let trees = LocalTrees::open(dir.path().to_path_buf());
    assert_eq!(trees.root(), dir.path());
    assert_eq!(names(dir.path()), ["note", "whole"]);
}

/// Asserts that a list whose bytes do not hash to the digest its key names is refused, version 1
/// and version 2 alike, with the format's words (the cache refuses such bytes first; the provider
/// does not rely on it).
#[test]
fn a_list_that_does_not_hash_to_its_key_is_refused() {
    let pack = Pack::new("cas/pack/one", &[10]);
    let limits = TreesConfig::default().limits();
    let other = TreeDigest::of(b"another list");
    for bytes in [
        one_file(&pack, "x", 0).encode(),
        one_file(&pack, "x", 0).encode_v2().expect("an encoding"),
    ] {
        let actual = TreeDigest::of(&bytes);
        let why = local::entries_of(bytes, &other, &limits).expect_err("refused");
        assert_eq!(
            why,
            super::sources::Stage::Refused(format!(
                "tree list digest mismatch: expected {other}, the bytes hash to {actual}"
            ))
        );
    }
}

/// Asserts the modes of written files, version 1 and version 2 alike: any execute bit gives 0755,
/// none gives 0644, and setuid, setgid and sticky bits are dropped.
#[tokio::test]
async fn written_files_are_0755_with_an_execute_bit_and_0644_without() {
    let local = Local::new("local-modes").await;
    let pack = &local.rig.bench.pack;
    let listed = [
        ("a", 0o4755),
        ("b", 0o2755),
        ("c", 0o1644),
        ("d", 0o600),
        ("e", 0o010),
        ("f", 0o001),
    ];
    let list = TreeList {
        entries: listed
            .iter()
            .map(|(path, mode)| member(pack, path, 0, *mode))
            .collect(),
    };
    let on_disk = |mode: u32| Seen::File(mode, pack.content(0).to_vec());
    let wanted = BTreeMap::from([
        ("a".to_owned(), on_disk(0o755)),
        ("b".to_owned(), on_disk(0o755)),
        ("c".to_owned(), on_disk(0o644)),
        ("d".to_owned(), on_disk(0o644)),
        ("e".to_owned(), on_disk(0o755)),
        ("f".to_owned(), on_disk(0o755)),
    ]);
    let v1 = local.rig.put_list(&list).await;
    let v2 = local.rig.put_list_v2(&list).await;
    for (run, key) in [("js.1", &v1), ("js.2", &v2)] {
        let path = local.mounted(&local.rig.provider, run, 0, key).await;
        assert_eq!(seen(&path), wanted, "{key}");
    }
}
