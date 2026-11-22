//! The put of one committed file: what it stores, and every reason it refuses a file, leaves it
//! to be tried again, or finds it gone; and how each refusal reads.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{CWD, FileType, Mode, mknodat};
use tracing::Level;
use tracing::subscriber::{DefaultGuard, set_default};

use super::store::{Outcome, Refusal, put_file};
use super::testing::sha;
use crate::server::build_info::stderr_log::LineLog;
use crate::server::local_cache::testing::Rig;
use crate::{Backend as _, Error, Op, PutOutcome};

const KEY: &str = "cas/b/b1/n1/pack";

/// A subscriber that drops what it is told, so that the warnings of a failed put are made and not
/// shown.
fn quiet() -> DefaultGuard {
    set_default(LineLog::new(std::io::sink(), Level::INFO))
}

/// A file of `bytes` named `name` in the rig's scratch directory.
fn staged(rig: &Rig, name: &str, bytes: &[u8]) -> PathBuf {
    let path = rig.dir.path().join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

/// The refusal that the outcome is.
fn refused(outcome: Outcome) -> Refusal {
    match outcome {
        Outcome::Refused(why) => why,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Asserts that a file of the declared bytes is stored (`Created`), that the same bytes again are
/// `Existed`, and that the upstream holds exactly them.
#[tokio::test]
async fn a_file_of_the_declared_bytes_is_stored_and_stored_again() {
    let rig = Rig::writing("store-stored");
    let cache = rig.locked();
    let path = staged(&rig, "pack", b"pack bytes");
    let made = put_file(&cache, KEY, sha(b"pack bytes"), &path).await;
    assert!(
        matches!(made, Outcome::Stored(PutOutcome::Created)),
        "{made:?}"
    );
    let again = put_file(&cache, KEY, sha(b"pack bytes"), &path).await;
    assert!(
        matches!(again, Outcome::Stored(PutOutcome::Existed)),
        "{again:?}"
    );
    let got = rig.tamper.get(KEY, None).await.unwrap();
    assert_eq!(got, "pack bytes");
}

/// Asserts that bytes that are shorter than, longer than, or other than the declared digest are
/// refused as `NotItsDigest` and that the upstream stores nothing under the key.
#[tokio::test]
async fn bytes_that_are_not_the_declared_digest_are_refused_and_not_stored() {
    let rig = Rig::writing("store-not-digest");
    let cache = rig.locked();
    let _log = quiet();
    let whole = b"the whole pack".to_vec();
    let mut longer = whole.clone();
    longer.push(b'!');
    for bytes in [whole[..5].to_vec(), longer, b"the whole pacK".to_vec()] {
        let path = staged(&rig, "pack", &bytes);
        let outcome = put_file(&cache, KEY, sha(&whole), &path).await;
        assert_eq!(refused(outcome), Refusal::NotItsDigest);
        assert!(rig.tamper.get(KEY, None).await.is_err());
    }
}

/// Asserts that a key that holds other bytes already refuses the file as `OtherBytes`, and the
/// first object stays.
#[tokio::test]
async fn a_key_that_holds_other_bytes_refuses_the_file() {
    let rig = Rig::writing("store-other-bytes");
    let cache = rig.locked();
    let _log = quiet();
    let first = staged(&rig, "first", b"pack bytes");
    let made = put_file(&cache, KEY, sha(b"pack bytes"), &first).await;
    assert!(matches!(made, Outcome::Stored(PutOutcome::Created)));
    let second = staged(&rig, "second", b"pack bytez");
    let outcome = put_file(&cache, KEY, sha(b"pack bytez"), &second).await;
    assert_eq!(refused(outcome), Refusal::OtherBytes);
    assert_eq!(rig.tamper.get(KEY, None).await.unwrap(), "pack bytes");
}

/// Asserts that an upstream that refuses the object for its size is `TooLarge`, and that another
/// upstream error is `Again`, so that the file is tried again.
#[tokio::test]
async fn an_upstream_that_refuses_for_size_is_refused_and_another_error_is_again() {
    let rig = Rig::writing("store-upstream");
    let cache = rig.locked();
    let _log = quiet();
    let path = staged(&rig, "pack", b"abc");
    let too_large = Error::TooLarge(KEY.to_owned());
    rig.upstream.fail_next_with(Op::Put, too_large);
    let outcome = put_file(&cache, KEY, sha(b"abc"), &path).await;
    assert_eq!(refused(outcome), Refusal::TooLarge);
    rig.upstream.fail_next(Op::Put, 1);
    let outcome = put_file(&cache, KEY, sha(b"abc"), &path).await;
    assert!(matches!(outcome, Outcome::Again(_)), "{outcome:?}");
}

/// Asserts that a path with nothing at it is `Gone`.
#[tokio::test]
async fn a_missing_path_is_gone() {
    let rig = Rig::writing("store-gone");
    let cache = rig.locked();
    let path = rig.dir.path().join("nothing");
    let outcome = put_file(&cache, KEY, sha(b"abc"), &path).await;
    assert!(matches!(outcome, Outcome::Gone), "{outcome:?}");
}

/// Asserts that a symbolic link is refused as `a symbolic link`, even to a good file, and that its
/// target is not stored.
#[tokio::test]
async fn a_symbolic_link_is_refused() {
    let rig = Rig::writing("store-symlink");
    let cache = rig.locked();
    let target = staged(&rig, "target", b"abc");
    let link = rig.dir.path().join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let outcome = put_file(&cache, KEY, sha(b"abc"), &link).await;
    assert_eq!(refused(outcome), Refusal::NotPlain("a symbolic link"));
    assert!(rig.tamper.get(KEY, None).await.is_err());
}

/// Asserts that a FIFO (which is opened without blocking) and a directory are refused as `not a
/// regular file`.
#[tokio::test]
async fn a_fifo_or_a_directory_is_not_a_regular_file() {
    let rig = Rig::writing("store-not-regular");
    let cache = rig.locked();
    let fifo = rig.dir.path().join("fifo");
    mknodat(CWD, &fifo, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();
    let outcome = put_file(&cache, KEY, sha(b"abc"), &fifo).await;
    assert_eq!(refused(outcome), Refusal::NotPlain("not a regular file"));
    let dir = rig.dir.path().join("a-directory");
    std::fs::create_dir(&dir).unwrap();
    let outcome = put_file(&cache, KEY, sha(b"abc"), &dir).await;
    assert_eq!(refused(outcome), Refusal::NotPlain("not a regular file"));
}

/// Asserts that a file with a second hard link is refused as `a hard link`, and stored nothing.
#[tokio::test]
async fn a_file_with_two_links_is_refused() {
    let rig = Rig::writing("store-hard-link");
    let cache = rig.locked();
    let path = staged(&rig, "pack", b"abc");
    std::fs::hard_link(&path, rig.dir.path().join("second")).unwrap();
    let outcome = put_file(&cache, KEY, sha(b"abc"), &path).await;
    assert_eq!(refused(outcome), Refusal::NotPlain("a hard link"));
    assert!(rig.tamper.get(KEY, None).await.is_err());
}

/// Asserts that a file that cannot be opened (mode 000) is `Unreadable` and is left. Skipped, with
/// its reason printed, where the file can be opened anyway (a process that is root).
#[tokio::test]
#[expect(
    clippy::disallowed_macros,
    reason = "a skipped test says why on standard error (AGENTS.md T12)"
)]
async fn a_file_that_cannot_be_opened_is_unreadable() {
    let rig = Rig::writing("store-mode-000");
    let cache = rig.locked();
    let path = staged(&rig, "pack", b"abc");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o0)).unwrap();
    if std::fs::File::open(&path).is_ok() {
        eprintln!("skipped: a file of mode 000 can be opened by this process");
        return;
    }
    let outcome = put_file(&cache, KEY, sha(b"abc"), &path).await;
    assert!(matches!(outcome, Outcome::Unreadable(_)), "{outcome:?}");
    assert!(path.exists());
}

/// Asserts that a file that opens as a regular file but fails when read (the memory file of this
/// process, a regular file of one link that reads as an I/O error at offset 0) is `Unreadable`
/// with the read's reason, not a refusal of the digest. Skipped, with its reason printed, where
/// that file reads without an error.
#[tokio::test]
#[expect(
    clippy::disallowed_macros,
    reason = "a skipped test says why on standard error (AGENTS.md T12)"
)]
async fn a_read_that_fails_in_the_body_is_unreadable() {
    let rig = Rig::writing("store-read-fails");
    let cache = rig.locked();
    let path = Path::new("/proc/self/mem");
    if std::fs::read(path).is_ok() {
        eprintln!("skipped: the memory file of this process reads without an error");
        return;
    }
    let _log = quiet();
    let outcome = put_file(&cache, KEY, sha(b"abc"), path).await;
    match outcome {
        Outcome::Unreadable(why) => assert!(why.contains("cannot be read"), "{why}"),
        other => panic!("expected Unreadable, got {other:?}"),
    }
}

/// Asserts the words of every refusal, by exact equality.
#[test]
fn every_refusal_reads_as_its_words() {
    let words = [
        (Refusal::NoDigest, "no digest was declared for it"),
        (
            Refusal::NotItsDigest,
            "its bytes are not those of the declared digest",
        ),
        (Refusal::OtherBytes, "the key holds other bytes"),
        (Refusal::TooLarge, "the upstream refused it for its size"),
        (Refusal::NotPlain("a hard link"), "it is a hard link"),
        (Refusal::KeyTooLong, "its key is over 1024 bytes"),
    ];
    for (refusal, text) in words {
        assert_eq!(refusal.to_string(), text);
    }
}
