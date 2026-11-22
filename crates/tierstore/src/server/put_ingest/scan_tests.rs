//! The whole scan of a put root: the builds and jobs it finds with their changed times, the
//! committed and declared names of a job, and what it leaves out (`.part` names, names that are
//! not components, links, subdirectories, a directory that cannot be read).

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use tierstore_testkit::dir::TestDir;

use super::names::JobDir;
use super::scan::{Found, scan};
use super::testing::sha;

/// The scratch directory of a test, with the put root `cas/b` made in it.
fn root(name: &str) -> TestDir {
    let dir = TestDir::under(std::env::temp_dir(), name);
    std::fs::create_dir_all(dir.path().join("cas/b")).unwrap();
    dir
}

/// The changed time of `path` as the file system gives it, in milliseconds since the epoch.
fn mtime(path: &Path) -> u64 {
    let since = std::fs::symlink_metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap();
    u64::try_from(since.as_millis()).unwrap()
}

/// The job directory of `build` and `nonce`.
fn job_dir(build: &str, job_name: &str) -> JobDir {
    JobDir::new(build, job_name).unwrap()
}

/// Asserts that a root with no `cas/b` directory, and one whose `cas/b` is empty, scan to nothing.
#[test]
fn a_root_without_builds_scans_to_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "scan-no-builds");
    assert_eq!(scan(dir.path()), Found::default());
    std::fs::create_dir_all(dir.path().join("cas/b")).unwrap();
    assert_eq!(scan(dir.path()), Found::default());
}

/// Asserts that every build and job directory is found, each with the changed time of the
/// directory itself, and that a build with no job is listed as a build without a job.
#[test]
fn builds_and_jobs_are_found_with_their_changed_times() {
    let dir = root("scan-builds-jobs");
    let base = dir.path().join("cas/b");
    std::fs::create_dir_all(base.join("b2/n1")).unwrap();
    std::fs::create_dir_all(base.join("b1/n2")).unwrap();
    std::fs::create_dir_all(base.join("b1/n1")).unwrap();
    std::fs::create_dir_all(base.join("empty")).unwrap();
    let found = scan(dir.path());
    let keys: Vec<&JobDir> = found.jobs.keys().collect();
    assert_eq!(
        keys,
        [
            &job_dir("b1", "n1"),
            &job_dir("b1", "n2"),
            &job_dir("b2", "n1")
        ]
    );
    for (job, seen) in &found.jobs {
        assert_eq!(seen.changed, mtime(&dir.path().join(job.rel())));
        assert!(seen.committed.is_empty() && seen.declared.is_empty());
    }
    let mut builds = found.builds.clone();
    builds.sort();
    let want: Vec<(PathBuf, u64)> = ["b1", "b2", "empty"]
        .iter()
        .map(|build| {
            let rel = PathBuf::from("cas/b").join(build);
            let when = mtime(&dir.path().join(&rel));
            (rel, when)
        })
        .collect();
    assert_eq!(builds, want);
}

/// Asserts that a job's committed names and declared digests are listed sorted, that a plain
/// `.part` name is left out, and that a declaration is listed with the name it is for.
#[test]
fn committed_and_declared_names_are_sorted_and_part_names_are_ignored() {
    let dir = root("scan-names");
    let job = dir.path().join("cas/b/b1/n1");
    std::fs::create_dir_all(&job).unwrap();
    let (a, b) = (sha(b"a"), sha(b"b"));
    for name in [
        "zeta".to_owned(),
        "alpha".to_owned(),
        "pack.part".to_owned(),
        format!("zed.sha256-{b}.part"),
        format!("ace.sha256-{a}.part"),
    ] {
        std::fs::write(job.join(name), b"x").unwrap();
    }
    let found = scan(dir.path());
    let seen = &found.jobs[&job_dir("b1", "n1")];
    assert_eq!(seen.committed, ["alpha", "zeta"]);
    assert_eq!(
        seen.declared,
        [("ace".to_owned(), a), ("zed".to_owned(), b)]
    );
}

/// Asserts that a file whose name is not a component (it has white space) and a file whose name
/// is not UTF-8 are not listed, and that a build or a job directory of such a name is not walked.
#[test]
fn names_that_are_not_components_are_ignored() {
    let dir = root("scan-bad-names");
    let base = dir.path().join("cas/b");
    let job = base.join("b1/n1");
    std::fs::create_dir_all(&job).unwrap();
    std::fs::create_dir_all(base.join("b 2/n1")).unwrap();
    std::fs::create_dir_all(base.join("b1/n 2")).unwrap();
    std::fs::write(job.join("has space"), b"x").unwrap();
    std::fs::write(job.join("fine"), b"x").unwrap();
    // APFS refuses a name that is not UTF-8 (EILSEQ), so on macOS there is none to pass over.
    let raw = std::ffi::OsStr::from_bytes(b"bad-\xff-name");
    if std::fs::write(job.join(raw), b"x").is_ok() {
        std::fs::create_dir_all(base.join(raw)).unwrap();
    }
    let found = scan(dir.path());
    let keys: Vec<&JobDir> = found.jobs.keys().collect();
    assert_eq!(keys, [&job_dir("b1", "n1")]);
    assert_eq!(found.jobs[&job_dir("b1", "n1")].committed, ["fine"]);
    assert_eq!(found.builds.len(), 1);
}

/// Asserts that a symbolic link to a directory at the build level and at the nonce level is not
/// walked, that a link to a file is not listed as committed, and that a subdirectory of a job's
/// directory is not listed.
#[test]
fn links_and_subdirectories_are_not_walked_or_listed() {
    let dir = root("scan-links");
    let base = dir.path().join("cas/b");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(elsewhere.join("n9")).unwrap();
    let job = base.join("b1/n1");
    std::fs::create_dir_all(&job).unwrap();
    std::os::unix::fs::symlink(&elsewhere, base.join("linked")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("n9"), base.join("b1/linked")).unwrap();
    std::fs::write(elsewhere.join("target"), b"x").unwrap();
    std::os::unix::fs::symlink(elsewhere.join("target"), job.join("link")).unwrap();
    std::fs::create_dir_all(job.join("sub")).unwrap();
    std::fs::write(job.join("real"), b"x").unwrap();
    let found = scan(dir.path());
    let keys: Vec<&JobDir> = found.jobs.keys().collect();
    assert_eq!(keys, [&job_dir("b1", "n1")]);
    assert_eq!(found.jobs[&job_dir("b1", "n1")].committed, ["real"]);
    let builds: Vec<&PathBuf> = found.builds.iter().map(|(rel, _)| rel).collect();
    assert_eq!(builds, [&PathBuf::from("cas/b/b1")]);
}

/// Asserts that a job directory that cannot be read is left out of the jobs (and the rest is
/// found), and that an unreadable `cas/b` finds nothing. Skipped, with its reason printed, where
/// the directory can be read anyway (a process that is root).
#[test]
#[expect(
    clippy::disallowed_macros,
    reason = "a skipped test says why on standard error (AGENTS.md T12)"
)]
fn an_unreadable_directory_is_left_out() {
    let dir = root("scan-unreadable");
    let base = dir.path().join("cas/b");
    std::fs::create_dir_all(base.join("b1/open")).unwrap();
    let shut = base.join("b1/shut");
    std::fs::create_dir_all(&shut).unwrap();
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o0)).unwrap();
    if std::fs::read_dir(&shut).is_ok() {
        eprintln!("skipped: a directory of mode 000 can be read by this process");
    } else {
        let found = scan(dir.path());
        let keys: Vec<&JobDir> = found.jobs.keys().collect();
        assert_eq!(keys, [&job_dir("b1", "open")]);
        assert_eq!(found.builds.len(), 1);
    }
    std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o0)).unwrap();
    if std::fs::read_dir(&base).is_ok() {
        eprintln!("skipped: a directory of mode 000 can be read by this process");
    } else {
        assert_eq!(scan(dir.path()), Found::default());
    }
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
}
