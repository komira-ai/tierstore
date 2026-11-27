//! The verb `gc legacy`: what a dry run says, what `--apply` removes and leaves, the refusal while
//! the cache lock is held, and the files and arguments it refuses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tierstore_testkit::dir::TestDir;

use super::{legacy, lowered, run};
use crate::server::lock;
use crate::server::verbs::answer;
use crate::server::verbs::testing::{args, status};

/// Runs `tierstore gc <words>` and gives its status and what it wrote to stdout and stderr.
fn gc(words: &[&str]) -> (String, String, String) {
    let mut all = vec!["gc"];
    all.extend_from_slice(words);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args(&all), &mut out, &mut err);
    (
        format!("{code:?}"),
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

/// Writes `bytes` to `path`, making its directories.
fn put(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// A scratch directory with a configuration file whose `local_cache.path` is `cache` under it.
struct Node {
    dir: TestDir,
    cache: PathBuf,
    config: PathBuf,
}

impl Node {
    fn new(name: &str) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let cache = dir.path().join("cache");
        let config = dir.path().join("node.toml");
        let text = format!(
            "[server]\nproxy_id = \"c1\"\n\n[local_cache]\npath = {:?}\nbudget = \"1MiB\"\n",
            cache.display().to_string()
        );
        std::fs::write(&config, text).unwrap();
        Self { dir, cache, config }
    }

    /// The legacy layout and the files beside it: 2 files of 7 bytes in `obj/`, 1 of 5 in
    /// `.stage/`, 2 of 6 in `extents-v1/`; and `packs-v2/`, `.lock` and a file of another kind.
    fn layout(&self) {
        let c = &self.cache;
        put(&c.join("obj/ab/cd/abcd"), b"hello");
        put(&c.join("obj/ab/cd/abcd.meta"), b"{}");
        put(&c.join(".stage/fill-1"), b"stage");
        put(&c.join("extents-v1/ef/01/ef01"), b"data");
        put(&c.join("extents-v1/ef/01/ef01.ext"), b"[]");
        self.new_layout();
    }

    /// What is not legacy: the new cache's entry, its lock, the directory's lock and a socket's
    /// stand-in.
    fn new_layout(&self) {
        let c = &self.cache;
        put(&c.join("packs-v2/aa/bb/aabb.pack"), b"pack bytes");
        put(&c.join("packs-v2/aa/bb/aabb.jnl"), b"journal");
        put(&c.join("packs-v2/.lock"), b"");
        put(&c.join(".lock"), b"");
        put(&c.join("fill.sock"), b"other");
    }

    /// `gc legacy --config <file>` and `extra`.
    fn legacy(&self, extra: &[&str]) -> (String, String, String) {
        let config = self.config.to_str().unwrap();
        let mut words = vec!["legacy", "--config", config];
        words.extend_from_slice(extra);
        gc(&words)
    }

    /// The lines a run says for the directories with these tallies (files, bytes), in order.
    fn said(&self, done: &str, tallies: [(u64, u64); 3]) -> String {
        let mut text = String::new();
        for (name, (files, bytes)) in ["obj", ".stage", "extents-v1"].iter().zip(tallies) {
            let dir = self.cache.join(name);
            text.push_str(&format!(
                "{done} {}: {files} files, {bytes} bytes\n",
                dir.display()
            ));
        }
        text.push_str(&format!(
            "left as it is: {}\n",
            self.cache.join("packs-v2").display()
        ));
        text
    }
}

/// Every file under `root` (links as the text of their target), with its bytes, by its path.
fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut found = BTreeMap::new();
    let mut todo = vec![root.to_owned()];
    while let Some(dir) = todo.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            if meta.is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                found.insert(path, target.into_os_string().into_encoded_bytes());
            } else if meta.is_dir() {
                todo.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                found.insert(path, bytes);
            }
        }
    }
    found
}

/// Asserts that a run without `--apply` says, for each legacy directory, its files and bytes, and
/// leaves every file of the cache directory as it was.
#[test]
fn a_dry_run_counts_the_legacy_layout_and_removes_nothing() {
    let node = Node::new("gc-legacy-dry");
    node.layout();
    let before = files(&node.cache);
    let (code, out, err) = node.legacy(&[]);
    assert_eq!(code, status(0), "{err}");
    assert_eq!(out, node.said("would remove", [(2, 7), (1, 5), (2, 6)]));
    assert_eq!(err, "");
    assert_eq!(files(&node.cache), before);
}

/// Asserts that `--apply` removes `obj/`, `.stage/` and `extents-v1/` with everything in them and
/// says what it removed, and leaves `packs-v2/` (its pack, journal and lock), `.lock` and the other
/// file byte for byte.
#[test]
fn apply_removes_the_legacy_layout_and_leaves_packs_v2_and_the_lock() {
    let node = Node::new("gc-legacy-apply");
    node.layout();
    let mut kept = files(&node.cache);
    kept.retain(|path, _| {
        !["obj", ".stage", "extents-v1"]
            .iter()
            .any(|name| path.starts_with(node.cache.join(name)))
    });
    assert_eq!(kept.len(), 5);
    let (code, out, err) = node.legacy(&["--apply"]);
    assert_eq!(code, status(0), "{err}");
    assert_eq!(out, node.said("removed", [(2, 7), (1, 5), (2, 6)]));
    for name in ["obj", ".stage", "extents-v1"] {
        assert!(!node.cache.join(name).exists(), "{name} is left");
    }
    assert_eq!(files(&node.cache), kept);
}

/// Asserts that while another open file holds the cache directory's `.lock` (a tierstore that
/// serves the cache), `--apply` is status 1, says the lock is held, and removes nothing.
#[test]
fn a_held_cache_lock_refuses_the_removal_and_removes_nothing() {
    let node = Node::new("gc-legacy-held");
    node.layout();
    let before = files(&node.cache);
    let lock_file = node.cache.join(".lock");
    let _held = lock::try_take(&lock_file).unwrap().unwrap();
    let (code, out, err) = node.legacy(&["--apply"]);
    assert_eq!(code, status(1));
    assert_eq!(out, "");
    assert_eq!(
        err,
        format!(
            "tierstore gc legacy: {} is held by another process, a tierstore that serves this \
             cache: stop it first; nothing was removed\n",
            lock_file.display()
        )
    );
    assert_eq!(files(&node.cache), before);
}

/// Asserts that legacy directories that are not there count as zero files and bytes, with and
/// without `--apply`, and that the new layout is left.
#[test]
fn legacy_directories_that_are_not_there_count_as_zero() {
    let node = Node::new("gc-legacy-none");
    node.new_layout();
    let before = files(&node.cache);
    for (extra, done) in [(&[][..], "would remove"), (&["--apply"][..], "removed")] {
        let (code, out, err) = node.legacy(extra);
        assert_eq!(code, status(0), "{err}");
        assert_eq!(out, node.said(done, [(0, 0); 3]));
    }
    assert_eq!(files(&node.cache), before);
}

/// Asserts that a cache directory that is not there is zero for a dry run, and that `--apply`,
/// which cannot take its lock, is status 1 and says so.
#[test]
fn a_cache_directory_that_is_not_there_cannot_be_locked_for_removal() {
    let node = Node::new("gc-legacy-missing");
    let (code, out, _) = node.legacy(&[]);
    assert_eq!(code, status(0));
    assert_eq!(out, node.said("would remove", [(0, 0); 3]));
    let (code, out, err) = node.legacy(&["--apply"]);
    assert_eq!(code, status(1));
    assert_eq!(out, "");
    let lock_file = node.cache.join(".lock");
    let prefix = format!("tierstore gc legacy: {}: ", lock_file.display());
    assert!(err.starts_with(&prefix), "{err}");
    assert!(err.ends_with("; nothing was removed\n"), "{err}");
    assert!(!node.cache.exists());
}

/// Asserts that a link inside `obj/`, to a file and to a directory outside the cache, is removed
/// as a file of its own, counted with the length of the link, and that neither target is
/// touched.
#[test]
fn a_link_inside_obj_is_removed_and_its_target_is_kept() {
    let node = Node::new("gc-legacy-link");
    let outside = node.dir.path().join("outside");
    put(&outside.join("keep"), b"keep me");
    std::fs::create_dir_all(node.cache.join("obj/ab")).unwrap();
    let file_link = node.cache.join("obj/ab/file");
    let dir_link = node.cache.join("obj/ab/dir");
    std::os::unix::fs::symlink(outside.join("keep"), &file_link).unwrap();
    std::os::unix::fs::symlink(&outside, &dir_link).unwrap();
    let lengths = [&file_link, &dir_link]
        .iter()
        .map(|link| std::fs::symlink_metadata(link).unwrap().len())
        .sum::<u64>();
    let (code, out, err) = node.legacy(&["--apply"]);
    assert_eq!(code, status(0), "{err}");
    assert_eq!(out, node.said("removed", [(2, lengths), (0, 0), (0, 0)]));
    assert!(!node.cache.join("obj").exists());
    assert_eq!(std::fs::read(outside.join("keep")).unwrap(), b"keep me");
}

/// Asserts that a file with no `[local_cache]` path, a file that is not TOML and a file that is
/// not there are status 2 and the message names the key or the file, never what the file says.
#[test]
fn a_file_that_names_no_cache_directory_is_refused() {
    let node = Node::new("gc-legacy-config");
    std::fs::write(&node.config, "[server]\nproxy_id = \"c1\"\n").unwrap();
    let (code, _, err) = node.legacy(&[]);
    assert_eq!(code, status(2));
    assert_eq!(
        err,
        "tierstore gc legacy: local_cache.path: the file names no cache directory, whose legacy \
         layout the verb removes\n"
    );
    std::fs::write(&node.config, "[local_cache\nsecret = \"s3cr3t\"\n").unwrap();
    let (code, _, err) = node.legacy(&[]);
    assert_eq!(code, status(2));
    assert_eq!(
        err,
        format!(
            "tierstore gc legacy: {}: the file is not TOML\n",
            node.config.display()
        )
    );
    std::fs::remove_file(&node.config).unwrap();
    let (code, _, err) = node.legacy(&[]);
    assert_eq!(code, status(2));
    let prefix = format!("tierstore gc legacy: {}: ", node.config.display());
    assert!(err.starts_with(&prefix), "{err}");
}

/// Asserts that help is a success and names the subcommand and its flags, that a missing
/// subcommand or `--config` is status 2, and that the verb is answered by name before the flags
/// of a start.
#[test]
fn the_arguments_of_the_verb_are_parsed() {
    let (code, out, _) = gc(&["--help"]);
    assert_eq!(code, status(0));
    assert!(out.contains("legacy"), "{out}");
    let (code, out, _) = gc(&["legacy", "--help"]);
    assert_eq!(code, status(0));
    assert!(out.contains("--config"), "{out}");
    assert!(out.contains("--apply"), "{out}");
    let (code, _, err) = gc(&[]);
    assert_eq!(code, status(2));
    assert!(!err.is_empty());
    let (code, _, err) = gc(&["legacy"]);
    assert_eq!(code, status(2));
    assert!(err.contains("--config"), "{err}");
    let answered = answer(&args(&["gc", "--help"])).unwrap();
    assert_eq!(format!("{answered:?}"), status(0));
}

/// Asserts that a priority that could not be lowered is one warning on stderr, and a priority
/// that was lowered says nothing.
#[test]
fn a_priority_that_cannot_be_lowered_is_a_warning() {
    let mut err = Vec::new();
    lowered(Err(rustix::io::Errno::PERM), &mut err);
    let text = String::from_utf8(err).unwrap();
    assert_eq!(
        text,
        format!(
            "tierstore gc legacy: warning: the CPU priority was not lowered: {}\n",
            rustix::io::Errno::PERM
        )
    );
    let mut err = Vec::new();
    lowered(Ok(()), &mut err);
    assert!(err.is_empty());
}

/// Asserts that a file the verb cannot remove (its directory may not be written) stops the
/// removal with status 1 and a message naming the legacy directory and the system's error, and
/// that what was not reached is left.
#[test]
fn a_file_that_cannot_be_removed_stops_the_removal_with_status_1() {
    use std::os::unix::fs::PermissionsExt as _;
    let node = Node::new("tsgc-legacy-unremovable");
    put(&node.cache.join("obj/ab/x"), b"kept");
    let locked_dir = node.cache.join("obj/ab");
    let mode = |m| std::fs::Permissions::from_mode(m);
    std::fs::set_permissions(&locked_dir, mode(0o500)).unwrap();
    let (code, _, err) = node.legacy(&["--apply"]);
    std::fs::set_permissions(&locked_dir, mode(0o755)).unwrap();
    assert_eq!(code, status(1));
    let obj = node.cache.join("obj");
    let want = format!(
        "tierstore gc legacy: {}: Permission denied (os error 13)\n",
        obj.display()
    );
    assert_eq!(err, want);
    assert_eq!(std::fs::read(node.cache.join("obj/ab/x")).unwrap(), b"kept");
}

/// A writer that refuses every write.
struct Refusing;

impl std::io::Write for Refusing {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("closed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Asserts that an output that cannot be written is the verb's failure, naming the output.
#[test]
fn an_output_that_cannot_be_written_is_a_failure() {
    use std::io::Write as _;
    let node = Node::new("tsgc-legacy-output");
    let why = legacy(&node.cache, false, &mut Refusing).unwrap_err();
    assert_eq!(why, "writing the output: closed");
    assert!(Refusing.flush().is_ok());
}
