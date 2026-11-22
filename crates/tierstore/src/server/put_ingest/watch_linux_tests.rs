//! The inotify watcher of the put root: which names it reports and as what, which directories it
//! watches (and when the watch is in place), and what it does when a watched directory goes.

use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use tierstore_testkit::dir::TestDir;

use super::Seen;
use super::watch_linux::Watcher;

/// The scratch directory of a test, as a put root.
fn root(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), name)
}

/// The next things the watcher sees.
async fn next(watcher: &mut Watcher) -> Vec<Seen> {
    let mut seen = Vec::new();
    watcher.next(&mut seen).await.unwrap();
    seen
}

/// Asserts that a root that is not there cannot be watched.
#[tokio::test]
async fn a_missing_root_cannot_be_watched() {
    let dir = root("watch-missing");
    assert!(Watcher::new(&dir.path().join("nothing")).is_err());
}

/// Asserts that, when `new` returns, the job's directory that was there is watched: a file made in
/// it and a file renamed into it are each a `Seen::Path` of their path under the root.
#[tokio::test]
async fn a_job_directory_that_was_there_is_watched_when_new_returns() {
    let dir = root("watch-job");
    let job = dir.path().join("cas/b/b1/n1");
    std::fs::create_dir_all(&job).unwrap();
    let mut watcher = Watcher::new(dir.path()).unwrap();
    std::fs::write(job.join("pack.part"), b"x").unwrap();
    let want = Seen::Path(PathBuf::from("cas/b/b1/n1/pack.part"));
    assert_eq!(next(&mut watcher).await, [want]);
    std::fs::rename(job.join("pack.part"), job.join("pack")).unwrap();
    let want = Seen::Path(PathBuf::from("cas/b/b1/n1/pack"));
    assert_eq!(next(&mut watcher).await, [want]);
}

/// Asserts that a directory made under `cas/b` is a `Seen::Dir`, and that it is watched at once:
/// a directory made in it afterwards is seen too, and a file made in that job directory is seen.
#[tokio::test]
async fn a_directory_that_appears_is_seen_and_watched_at_once() {
    let dir = root("watch-dir");
    std::fs::create_dir_all(dir.path().join("cas/b")).unwrap();
    let mut watcher = Watcher::new(dir.path()).unwrap();
    std::fs::create_dir(dir.path().join("cas/b/b1")).unwrap();
    assert_eq!(
        next(&mut watcher).await,
        [Seen::Dir(PathBuf::from("cas/b/b1"))]
    );
    std::fs::create_dir(dir.path().join("cas/b/b1/n1")).unwrap();
    assert_eq!(
        next(&mut watcher).await,
        [Seen::Dir(PathBuf::from("cas/b/b1/n1"))]
    );
    std::fs::write(dir.path().join("cas/b/b1/n1/pack"), b"x").unwrap();
    let want = Seen::Path(PathBuf::from("cas/b/b1/n1/pack"));
    assert_eq!(next(&mut watcher).await, [want]);
}

/// Asserts that a directory made with directories already in it (moved into place whole) is
/// watched below, so that the file made in its job directory is seen.
#[tokio::test]
async fn a_tree_moved_into_place_is_watched_below_its_top() {
    let dir = root("watch-moved-tree");
    std::fs::create_dir_all(dir.path().join("cas/b")).unwrap();
    std::fs::create_dir_all(dir.path().join("made/n1")).unwrap();
    std::fs::create_dir_all(dir.path().join("made/n1/inner")).unwrap();
    let mut watcher = Watcher::new(dir.path()).unwrap();
    std::fs::rename(dir.path().join("made"), dir.path().join("cas/b/b1")).unwrap();
    assert_eq!(
        next(&mut watcher).await,
        [Seen::Dir(PathBuf::from("cas/b/b1"))]
    );
    std::fs::write(dir.path().join("cas/b/b1/n1/pack"), b"x").unwrap();
    let want = Seen::Path(PathBuf::from("cas/b/b1/n1/pack"));
    assert_eq!(next(&mut watcher).await, [want]);
}

/// Asserts that a directory made in a job's directory is a `Seen::Path` (not a `Dir`) and is not
/// watched, so that a file made in it is not reported.
#[tokio::test]
async fn a_directory_deeper_than_a_job_directory_is_a_path_and_not_watched() {
    let dir = root("watch-deep");
    let job = dir.path().join("cas/b/b1/n1");
    std::fs::create_dir_all(&job).unwrap();
    let mut watcher = Watcher::new(dir.path()).unwrap();
    std::fs::create_dir(job.join("sub")).unwrap();
    let want = Seen::Path(PathBuf::from("cas/b/b1/n1/sub"));
    assert_eq!(next(&mut watcher).await, [want]);
    std::fs::write(job.join("sub/inner"), b"x").unwrap();
    std::fs::write(job.join("after"), b"x").unwrap();
    let after = Seen::Path(PathBuf::from("cas/b/b1/n1/after"));
    assert_eq!(next(&mut watcher).await, [after]);
}

/// Asserts that a directory made under a name outside the layout (`other`) is a `Seen::Path` and
/// is not watched.
#[tokio::test]
async fn a_directory_outside_the_layout_is_a_path() {
    let dir = root("watch-other");
    let mut watcher = Watcher::new(dir.path()).unwrap();
    std::fs::create_dir(dir.path().join("other")).unwrap();
    let want = Seen::Path(PathBuf::from("other"));
    assert_eq!(next(&mut watcher).await, [want]);
    std::fs::write(dir.path().join("other/inner"), b"x").unwrap();
    std::fs::write(dir.path().join("marker"), b"x").unwrap();
    let marker = Seen::Path(PathBuf::from("marker"));
    assert_eq!(next(&mut watcher).await, [marker]);
}

/// Asserts that a file whose name is not UTF-8 is not reported, while a name made after it is.
#[tokio::test]
async fn a_name_that_is_not_utf8_is_not_reported() {
    let dir = root("watch-not-utf8");
    let job = dir.path().join("cas/b/b1/n1");
    std::fs::create_dir_all(&job).unwrap();
    let mut watcher = Watcher::new(dir.path()).unwrap();
    std::fs::write(job.join(std::ffi::OsStr::from_bytes(b"bad-\xff")), b"x").unwrap();
    std::fs::write(job.join("good"), b"x").unwrap();
    let want = Seen::Path(PathBuf::from("cas/b/b1/n1/good"));
    assert_eq!(next(&mut watcher).await, [want]);
}

/// Asserts that when a watched directory is removed, its end is taken without a report for it and
/// the watcher goes on: a directory made afterwards is seen, and only it.
#[tokio::test]
async fn a_removed_watched_directory_is_dropped_quietly() {
    let dir = root("watch-removed");
    let job = dir.path().join("cas/b/b1/n1");
    std::fs::create_dir_all(&job).unwrap();
    let mut watcher = Watcher::new(dir.path()).unwrap();
    std::fs::remove_dir(&job).unwrap();
    std::fs::remove_dir(dir.path().join("cas/b/b1")).unwrap();
    std::fs::create_dir(dir.path().join("cas/b/b2")).unwrap();
    let mut seen = Vec::new();
    while !seen.contains(&Seen::Dir(PathBuf::from("cas/b/b2"))) {
        watcher.next(&mut seen).await.unwrap();
    }
    assert_eq!(seen, [Seen::Dir(PathBuf::from("cas/b/b2"))]);
}

/// Asserts that a directory of the layout that cannot be watched (mode 000) is left, without an
/// error, and that the directories beside it are watched: one made unwatchable at the start, and
/// one made so after it. Skipped, with its reason printed, where the directory can be read anyway
/// (a process that is root).
#[tokio::test]
#[expect(
    clippy::disallowed_macros,
    reason = "a skipped test says why on standard error (AGENTS.md T12)"
)]
async fn a_directory_that_cannot_be_watched_is_left() {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let dir = root("watch-unwatchable");
    let shut = dir.path().join("cas/b/shut");
    let open = dir.path().join("cas/b/open/n1");
    std::fs::create_dir_all(&shut).unwrap();
    std::fs::create_dir_all(&open).unwrap();
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o0)).unwrap();
    if std::fs::read_dir(&shut).is_ok() {
        eprintln!("skipped: a directory of mode 000 can be read by this process");
        return;
    }
    let mut watcher = Watcher::new(dir.path()).unwrap();
    std::fs::write(open.join("pack"), b"x").unwrap();
    let want = Seen::Path(PathBuf::from("cas/b/open/n1/pack"));
    assert_eq!(next(&mut watcher).await, [want]);
    let later = dir.path().join("cas/b/later");
    std::fs::DirBuilder::new().mode(0o0).create(&later).unwrap();
    let want = Seen::Dir(PathBuf::from("cas/b/later"));
    assert_eq!(next(&mut watcher).await, [want]);
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&later, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Asserts that more events than the queue of inotify holds, made before the watcher reads any,
/// end with a `Seen::Lost` among what it reports. Skipped, with its reason printed, where the size
/// of the queue cannot be read.
#[tokio::test]
#[expect(
    clippy::disallowed_macros,
    reason = "a skipped test says why on standard error (AGENTS.md T12)"
)]
async fn an_overflow_of_the_queue_is_lost() {
    let limit = std::fs::read_to_string("/proc/sys/fs/inotify/max_queued_events")
        .ok()
        .and_then(|text| text.trim().parse::<usize>().ok());
    let Some(limit) = limit.filter(|limit| *limit <= 1 << 20) else {
        eprintln!("skipped: the size of the inotify queue is not readable or is very large");
        return;
    };
    let dir = root("watch-overflow");
    let job = dir.path().join("cas/b/b1/n1");
    std::fs::create_dir_all(&job).unwrap();
    let mut watcher = Watcher::new(dir.path()).unwrap();
    for number in 0..=limit {
        std::fs::write(job.join(format!("f{number}")), b"").unwrap();
    }
    let seen = next(&mut watcher).await;
    assert!(
        seen.contains(&Seen::Lost),
        "no Lost among {} items",
        seen.len()
    );
}
