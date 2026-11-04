//! The directory backend: the names of the files, an object's calls, and the errors of a
//! directory that cannot be read or written. The multipart calls are held to the contract of the
//! trait, with the in-memory backend, in `tests/tierstore/multipart.rs`.

use std::fs::Permissions;
use std::io::{Error as IoError, ErrorKind};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use bytes::Bytes;
use futures::{StreamExt, future, stream};
use tierstore_testkit::dir::TestDir;
use tokio::sync::oneshot;

use crate::{Backend, ByteRange, DirBackend, Error, PartBody};

use super::{
    NAME_LIMIT, classify, create_new, decode, encode, encode_legacy, failed, part_name, sha256,
};

fn backend() -> (TestDir, DirBackend) {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-dir");
    let backend = DirBackend::open(dir.path()).unwrap();
    (dir, backend)
}

fn body(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// A body of these pieces.
fn pieces(texts: &[&'static str]) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = texts.iter().map(|text| Ok(body(text))).collect();
    stream::iter(items).boxed()
}

/// A body of the piece `first` and then the piece `second`. When the body is asked for `second`
/// it tells `started`, and it gives `second` only after `release` is sent or dropped.
fn gated(
    first: &'static str,
    second: &'static str,
    started: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = vec![Ok(body(first))];
    let rest = stream::once(async move {
        let _ = started.send(());
        let _ = release.await;
        Ok(body(second))
    });
    stream::iter(items).chain(rest).boxed()
}

/// The names of the files in a directory, in order.
fn names(dir: &Path) -> Vec<String> {
    let entries = std::fs::read_dir(dir).unwrap();
    let mut names: Vec<String> = entries
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
}

/// Asserts that a key is written as a name with no `/`, no `.` first and no byte but letters,
/// digits and `_-.~%`, that `/` is `~` and the other bytes are `%` and two hex digits, and that
/// the name decodes back to the key.
#[test]
fn a_key_is_a_file_name_that_decodes_back() {
    let keys = [
        "cas/d/n1/b7/00012",
        ".hidden",
        "a.b",
        "..",
        "é/ü",
        "a~b%c d",
        "x-y_z",
    ];
    for key in keys {
        let name = encode(key);
        assert!(!name.contains('/'), "{name}");
        assert!(!name.starts_with('.'), "{name}");
        assert_eq!(decode(&name).as_deref(), Some(key), "{key}");
    }
    assert_eq!(encode("a/b.c"), "a~b.c");
    assert_eq!(encode(".x"), "%2Ex");
    assert_eq!(encode("a~ b%"), "a%7E%20b%25");
    assert_eq!(encode("é"), "%C3%A9");
    assert_eq!(encode("Foo"), "%46oo");
    assert_eq!(decode("%46oo").as_deref(), Some("Foo"));
    assert_eq!(encode_legacy("Foo/b"), "Foo~b");
    assert_eq!(encode_legacy("a%"), "a%25");
    assert_eq!(decode("Foo~b").as_deref(), Some("Foo/b"));
}

/// Asserts that a name that `encode` does not make is not a key: a `%` without two hex digits
/// after it, and escapes that are not UTF-8.
#[test]
fn a_file_name_that_is_not_an_encoded_key_does_not_decode() {
    assert_eq!(decode("a~b").as_deref(), Some("a/b"));
    for name in ["%", "%4", "%zz", "%FF", "a%+f"] {
        assert_eq!(decode(name), None, "{name}");
    }
}

/// Asserts that the error of a failed call names the file, that a full disk is `Full` and any
/// other failure `Other`, and that a failure of the kind that tells a key is absent or taken is
/// that error for the key.
#[test]
fn the_error_of_a_failed_call_names_the_file_and_says_whether_the_disk_is_full() {
    let path = Path::new("/disk/file");
    let full = failed(path, &IoError::from(ErrorKind::StorageFull));
    assert!(matches!(full, Error::Full(_)), "{full}");
    assert!(full.to_string().contains("/disk/file"), "{full}");
    let denied = failed(path, &IoError::from(ErrorKind::PermissionDenied));
    assert!(matches!(denied, Error::Other(_)), "{denied}");
    assert!(denied.to_string().contains("/disk/file"), "{denied}");
    let absent = IoError::from(ErrorKind::NotFound);
    let found = classify("k", path, &absent, ErrorKind::NotFound, Error::NotFound);
    assert_eq!(found, Error::NotFound("k".to_owned()));
    let other = classify(
        "k",
        path,
        &absent,
        ErrorKind::AlreadyExists,
        Error::AlreadyExists,
    );
    assert!(matches!(other, Error::Other(_)));
}

/// Asserts that a directory is opened where it exists and made, with the one for uploads, where
/// it does not, and that a name that is empty and one that is under a file are refused.
#[test]
fn a_directory_is_made_and_a_name_that_cannot_be_one_is_refused() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-open");
    let nested = dir.path().join("a/b");
    assert!(DirBackend::open(&nested).is_ok());
    assert!(nested.join(".uploads").is_dir());
    assert!(DirBackend::open(&nested).is_ok());
    let empty = DirBackend::open("").err().unwrap();
    assert!(empty.to_string().contains("no name"), "{empty}");
    let file = dir.path().join("file");
    std::fs::write(&file, "x").unwrap();
    let blocked = DirBackend::open(file.join("inside")).err().unwrap();
    assert!(blocked.to_string().contains("inside"), "{blocked}");
}

/// Asserts that a tagged put gives the tag a HEAD of the file it published gives, refuses a second
/// put of the key and leaves the first file and its tag, and refuses a key that cannot be a file
/// name without giving a tag.
#[tokio::test]
async fn a_tagged_put_gives_the_tag_of_the_file_it_published() {
    let (_dir, backend) = backend();
    let tag = backend.put_tagged("k", body("one")).await.unwrap();
    assert_eq!(backend.head("k").await.unwrap().tag, tag);
    let again = backend.put_tagged("k", body("two")).await;
    assert_eq!(again, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), body("one"));
    assert_eq!(backend.head("k").await.unwrap().tag, tag);
    backend.delete_if_match("k", &tag).await.unwrap();
    assert!(backend.head("k").await.is_err());
    let refused = backend.put_tagged("", body("v")).await;
    assert!(matches!(refused, Err(Error::Other(_))));
}

/// Asserts that a key of 255 bytes is stored and one of 256, and the empty key, are refused with
/// an error for every call that names the key, and that nothing is made for them.
#[tokio::test]
async fn a_key_that_cannot_be_a_file_name_is_refused() {
    let (dir, backend) = backend();
    let longest = "a".repeat(NAME_LIMIT);
    backend.put(&longest, body("v")).await.unwrap();
    for key in ["a".repeat(NAME_LIMIT + 1), String::new()] {
        let refused = backend.put(&key, body("v")).await.unwrap_err();
        assert!(matches!(refused, Error::Other(_)), "{refused}");
        assert!(backend.get(&key, None).await.is_err());
        assert!(backend.head(&key).await.is_err());
        assert!(backend.delete(&key).await.is_err());
        assert!(backend.create_upload(&key).await.is_err());
    }
    assert_eq!(names(dir.path()), [".uploads".to_owned(), longest]);
}

/// Asserts that an object is stored, read whole and by range (byte-exact at both ends, cut at the
/// end, an error past it), described with the content tag the in-memory backend gives the same
/// bytes, and listed with its size; that a put never replaces it; and that a delete removes it
/// and succeeds for a key that is not there.
#[tokio::test]
async fn an_object_is_stored_read_described_listed_and_deleted() {
    let (_dir, backend) = backend();
    backend.put("cas/k", body("0123456789")).await.unwrap();
    let again = backend.put("cas/k", body("other")).await;
    assert_eq!(again, Err(Error::AlreadyExists("cas/k".to_owned())));
    let cases = [
        (0, 1, "0"),
        (9, 1, "9"),
        (4, 3, "456"),
        (8, 100, "89"),
        (3, 0, ""),
        (10, 5, ""),
    ];
    for (offset, len, want) in cases {
        let range = Some(ByteRange::new(offset, len));
        let got = backend.get("cas/k", range).await.unwrap();
        assert_eq!(got, want.as_bytes(), "range {offset}+{len}");
    }
    assert_eq!(backend.get("cas/k", None).await.unwrap(), "0123456789");
    let past = backend.get("cas/k", Some(ByteRange::new(11, 1))).await;
    assert!(matches!(past, Err(Error::Other(_))));
    let memory = crate::MemoryBackend::new();
    memory.put("m", body("0123456789")).await.unwrap();
    let head = backend.head("cas/k").await.unwrap();
    assert_eq!(head.size, 10);
    assert!(head.same_content(&memory.head("m").await.unwrap()));
    assert!(head.modified > 1_577_836_800_000);
    let listed = backend.list("cas/").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].key.as_str(), listed[0].size), ("cas/k", 10));
    backend.delete("cas/k").await.unwrap();
    backend.delete("cas/k").await.unwrap();
    let gone = Error::NotFound("cas/k".to_owned());
    assert_eq!(backend.get("cas/k", None).await.unwrap_err(), gone);
    assert_eq!(backend.head("cas/k").await.unwrap_err(), gone);
    assert!(backend.list("").await.unwrap().is_empty());
}

/// Asserts that a listing holds the keys with the prefix in key order, with the odd bytes of
/// keys decoded, and leaves out what is not an object: a file whose name is not an encoded key,
/// a hidden file, and a directory.
#[tokio::test]
async fn a_listing_holds_the_keys_with_the_prefix_and_skips_what_is_not_an_object() {
    let (dir, backend) = backend();
    let keys = ["cas/d/n 1/é.pack", ".hidden", "a.b", "~", "cas/a"];
    for key in keys {
        backend.put(key, body("v")).await.unwrap();
    }
    std::fs::write(dir.path().join("%zz"), "not an object").unwrap();
    std::fs::write(dir.path().join(".tmp-1-1"), "a temporary file").unwrap();
    std::fs::create_dir(dir.path().join("folder")).unwrap();
    let all = backend.list("").await.unwrap();
    let all: Vec<&str> = all.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(all, [".hidden", "a.b", "cas/a", "cas/d/n 1/é.pack", "~"]);
    let cas = backend.list("cas/").await.unwrap();
    assert_eq!(cas.len(), 2);
    assert!(backend.list("zzz").await.unwrap().is_empty());
    let page = backend.list_page("cas/", Some("cas/a"), 5).await.unwrap();
    assert_eq!(page.len(), 1);
}

/// Asserts that a put with the content tag of the object replaces it and gives the new tag, that
/// another tag is refused and changes nothing, that an absent key is `NotFound`, and that no
/// temporary file is left behind.
#[tokio::test]
async fn a_put_if_match_replaces_only_the_object_with_that_tag() {
    let (dir, backend) = backend();
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    let wrong = backend.put_if_match("k", body("x"), "not-the-tag").await;
    assert_eq!(wrong, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), "one");
    let new_tag = backend.put_if_match("k", body("two"), &tag).await.unwrap();
    assert_ne!(new_tag, tag);
    assert_eq!(backend.head("k").await.unwrap().tag, new_tag);
    assert_eq!(backend.get("k", None).await.unwrap(), "two");
    let absent = backend.put_if_match("other", body("x"), &tag).await;
    assert_eq!(absent, Err(Error::NotFound("other".to_owned())));
    assert_eq!(names(dir.path()), [".uploads".to_owned(), "k".to_owned()]);
}

/// Asserts that a conditional delete removes the object only if it has the content tag: another
/// tag is refused and changes nothing, the right tag removes it (and the key can be put again),
/// a tag the object had before a replacement is refused, an absent key is `NotFound`, and
/// no temporary file is left behind.
#[tokio::test]
async fn a_conditional_delete_removes_only_the_object_with_that_tag() {
    let (dir, backend) = backend();
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    let wrong = backend.delete_if_match("k", "not-the-tag").await;
    assert_eq!(wrong, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), "one");

    backend.delete_if_match("k", &tag).await.unwrap();
    assert!(matches!(backend.head("k").await, Err(Error::NotFound(_))));
    backend.put("k", body("two")).await.unwrap();
    let stale = backend.delete_if_match("k", &tag).await;
    assert_eq!(stale, Err(Error::PreconditionFailed("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), "two");

    let absent = backend.delete_if_match("other", &tag).await;
    assert_eq!(absent, Err(Error::NotFound("other".to_owned())));
    assert_eq!(names(dir.path()), [".uploads".to_owned(), "k".to_owned()]);
}

/// Asserts that a conditional delete does not start while a replacement or a delete holds the
/// lock of the backend, and goes on once it is released: its check and its removal are not
/// split by them. Polled once while the lock is held, with no clock and no wait.
#[tokio::test]
async fn a_conditional_delete_waits_for_the_lock_of_a_replacement_or_a_delete() {
    let (_dir, backend) = backend();
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    let held = backend.replacing.lock().await;
    let mut call = Box::pin(backend.delete_if_match("k", &tag));
    assert!(futures::poll!(&mut call).is_pending());
    drop(held);
    call.await.unwrap();
    assert!(matches!(backend.head("k").await, Err(Error::NotFound(_))));
}

/// Asserts that a plain delete does not remove the name while a replacement or a conditional
/// delete holds the lock of the backend, so that a conditional delete that has checked its tag
/// cannot have the object deleted and put again before it removes the file.
#[tokio::test]
async fn a_delete_waits_for_the_lock_of_a_replacement_or_a_conditional_delete() {
    let (_dir, backend) = backend();
    backend.put("k", body("one")).await.unwrap();
    let held = backend.replacing.lock().await;
    let mut call = Box::pin(backend.delete("k"));
    assert!(futures::poll!(&mut call).is_pending());
    assert_eq!(backend.get("k", None).await.unwrap(), "one");
    drop(held);
    call.await.unwrap();
    assert!(matches!(backend.head("k").await, Err(Error::NotFound(_))));
}

/// Asserts that a file too short to hold the content tag is an error, not an object.
#[tokio::test]
async fn a_file_too_short_for_its_content_tag_is_an_error() {
    let (dir, backend) = backend();
    std::fs::write(dir.path().join("short"), "tiny").unwrap();
    let got = backend.get("short", None).await.unwrap_err();
    assert!(matches!(got, Error::Other(_)), "{got}");
    assert!(matches!(backend.head("short").await, Err(Error::Other(_))));
}

/// Asserts that a directory that cannot be written fails the calls that change it with an error
/// that names the file, and leaves nothing behind: a put, a streamed put, a put that replaces
/// and a delete.
#[tokio::test]
async fn a_directory_that_cannot_be_written_fails_the_changes_and_leaves_nothing() {
    let (dir, backend) = backend();
    backend.put("k", body("one")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    chmod(dir.path(), 0o500);
    let put = backend.put("j", body("two")).await;
    let streamed = backend.put_stream("s", None, pieces(&["two"])).await;
    let replace = backend.put_if_match("k", body("two"), &tag).await;
    let delete = backend.delete("k").await;
    let listed = names(dir.path());
    chmod(dir.path(), 0o700);
    let errors = [put, streamed, replace.map(|_| ()), delete];
    for error in errors.map(Result::unwrap_err) {
        assert!(matches!(error, Error::Other(_)), "{error}");
    }
    assert_eq!(listed, [".uploads".to_owned(), "k".to_owned()]);
    assert_eq!(backend.get("k", None).await.unwrap(), "one");
}

/// Asserts that a directory that cannot be read fails a listing with an error, and that an
/// object in it can still be read by its name, and that a file that cannot be read is an error
/// and not an absent key.
#[tokio::test]
async fn a_directory_that_cannot_be_read_fails_the_listing() {
    let (dir, backend) = backend();
    backend.put("k", body("one")).await.unwrap();
    chmod(dir.path(), 0o300);
    let listing = backend.list("").await;
    let got = backend.get("k", None).await;
    chmod(dir.path(), 0o700);
    assert!(matches!(listing, Err(Error::Other(_))));
    assert_eq!(got.unwrap(), "one");
    chmod(&dir.path().join("k"), 0o000);
    let unreadable = backend.get("k", None).await;
    chmod(&dir.path().join("k"), 0o600);
    assert!(matches!(unreadable, Err(Error::Other(_))));
}

/// Asserts that the calls on an upload report the files they cannot write: a directory of
/// uploads that cannot be written fails the start and the abort, the directory of an upload that
/// cannot be written fails a part, and one that cannot be read fails a part and a completion,
/// as the directory of the objects that cannot be written fails a completion.
#[tokio::test]
async fn an_upload_that_cannot_be_written_or_read_fails_with_an_error() {
    let (dir, backend) = backend();
    let uploads = dir.path().join(".uploads");
    let id = backend.create_upload("k").await.unwrap();
    let doomed = backend.create_upload("d").await.unwrap();
    let upload = uploads.join(&id);
    let tag = backend
        .upload_part("k", &id, 1, pieces(&["abc"]))
        .await
        .unwrap();
    let parts = [(1, tag)];

    chmod(&uploads, 0o500);
    let start = backend.create_upload("j").await;
    let abort = backend.abort_upload("d", &doomed).await;
    chmod(&uploads, 0o700);
    assert!(matches!(start, Err(Error::Other(_))));
    assert!(matches!(abort, Err(Error::Other(_))));

    chmod(&upload, 0o500);
    let part = backend.upload_part("k", &id, 2, pieces(&["abc"])).await;
    chmod(&upload, 0o300);
    let again = backend.upload_part("k", &id, 3, pieces(&["abc"])).await;
    let complete = backend.complete_upload("k", &id, &parts).await;
    chmod(&upload, 0o700);
    assert!(matches!(part, Err(Error::Other(_))));
    assert!(matches!(again, Err(Error::Other(_))));
    assert!(matches!(complete, Err(Error::Other(_))));

    chmod(dir.path(), 0o500);
    let joined = backend.complete_upload("k", &id, &parts).await;
    chmod(dir.path(), 0o700);
    assert!(matches!(joined, Err(Error::Other(_))));
    assert_eq!(names(&upload).len(), 3);
    backend.complete_upload("k", &id, &parts).await.unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), "abc");
}

/// Asserts that the temporary files of two backends over one directory have names of their own,
/// and that a new file is never made over a name that is taken.
#[tokio::test]
async fn backends_over_one_directory_make_their_temporary_files_apart() {
    let (dir, first) = backend();
    let second = DirBackend::open(dir.path()).unwrap();
    assert_ne!(first.temp(dir.path()), second.temp(dir.path()));
    let taken = dir.path().join("taken");
    std::fs::write(&taken, "x").unwrap();
    let refused = create_new(&taken).await.unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::AlreadyExists);
    assert!(create_new(&dir.path().join("fresh")).await.is_ok());
}

/// Asserts that two uploads of one part number that overlap leave one file for the number, and
/// that the content tag of one of them, and not of the other, completes the upload. The overlap
/// is forced by gates: each body gives its first piece and then tells that it waits for its
/// second, and the test sends both releases only after both have told.
#[tokio::test]
async fn two_overlapping_uploads_of_one_part_number_leave_one_part() {
    let (dir, backend) = backend();
    let id = backend.create_upload("k").await.unwrap();
    let (started_a, a_started) = oneshot::channel();
    let (release_a, a_released) = oneshot::channel();
    let (started_b, b_started) = oneshot::channel();
    let (release_b, b_released) = oneshot::channel();
    let body_a = gated("aa", "bb", started_a, a_released);
    let body_b = gated("cc", "dd", started_b, b_released);
    let releasing = async {
        let _ = a_started.await;
        let _ = b_started.await;
        let _ = release_a.send(());
        let _ = release_b.send(());
    };
    let (a, b, _) = futures::join!(
        backend.upload_part("k", &id, 1, body_a),
        backend.upload_part("k", &id, 1, body_b),
        releasing,
    );
    let upload = dir.path().join(".uploads").join(&id);
    assert_eq!(names(&upload).len(), 2);
    let first = backend.complete_upload("k", &id, &[(1, a.unwrap())]).await;
    let second = backend.complete_upload("k", &id, &[(1, b.unwrap())]).await;
    assert_ne!(first.is_ok(), second.is_ok());
}

/// Asserts that a part whose call is dropped before its body ends leaves no temporary file in the
/// directory of the upload. The call is polled until its body, which never ends, is asked for the
/// piece after the first and tells a gate; the temporary file is then there, and it is gone once
/// the call is dropped.
#[tokio::test]
async fn a_part_dropped_half_way_leaves_no_temporary_file() {
    let (dir, backend) = backend();
    let id = backend.create_upload("k").await.unwrap();
    let (started, first_taken) = oneshot::channel();
    let items: Vec<Result<Bytes, Error>> = vec![Ok(body("ab"))];
    let endless = stream::once(async move {
        let _ = started.send(());
        std::future::pending::<Result<Bytes, Error>>().await
    });
    let stalled: PartBody<'static> = stream::iter(items).chain(endless).boxed();
    let waiting = backend.upload_part("k", &id, 1, stalled);
    let upload = dir.path().join(".uploads").join(&id);
    let parked = future::select(waiting, first_taken).await;
    assert!(matches!(parked, future::Either::Right((Ok(()), _))));
    assert_eq!(names(&upload).len(), 2);
    drop(parked);
    assert_eq!(names(&upload), ["key".to_owned()]);
}

/// Asserts that a part whose upload is removed while the part arrives is an error, and that no
/// file is left behind.
#[tokio::test]
async fn a_part_whose_upload_is_removed_while_it_arrives_is_an_error() {
    let (dir, backend) = backend();
    let id = backend.create_upload("k").await.unwrap();
    let upload = dir.path().join(".uploads").join(&id);
    let removing = stream::once(async move {
        std::fs::remove_dir_all(&upload).unwrap();
        Ok(body("x"))
    });
    let removing: PartBody<'static> = removing.boxed();
    let stored = backend.upload_part("k", &id, 1, removing).await;
    assert!(matches!(stored, Err(Error::Other(_))));
    assert!(names(&dir.path().join(".uploads")).is_empty());
}

/// Asserts that a completion that names a part that cannot be opened, and one that names a part
/// that cannot be read, is an error, and that no temporary file is left behind.
#[tokio::test]
async fn a_part_that_cannot_be_opened_or_read_fails_the_completion() {
    let (dir, backend) = backend();
    let id = backend.create_upload("k").await.unwrap();
    let tag = backend
        .upload_part("k", &id, 1, pieces(&["abc"]))
        .await
        .unwrap();
    let parts = [(1, tag.clone())];
    let upload = dir.path().join(".uploads").join(&id);
    let file = upload.join(part_name(1, &tag));
    chmod(&file, 0o000);
    let unopened = backend.complete_upload("k", &id, &parts).await;
    chmod(&file, 0o600);
    std::fs::remove_file(&file).unwrap();
    std::fs::create_dir(&file).unwrap();
    let unread = backend.complete_upload("k", &id, &parts).await;
    assert!(matches!(unopened, Err(Error::Other(_))));
    assert!(matches!(unread, Err(Error::Other(_))));
    assert_eq!(names(dir.path()), [".uploads".to_owned()]);
}

/// Whether the directory holds a temporary file of a put.
fn has_temporary_file(dir: &Path) -> bool {
    names(dir).iter().any(|name| name.starts_with(".tmp-"))
}

/// Asserts that an object put as a stream is not visible while its body is half way (a head finds
/// nothing, the chunks so far are in a temporary file), is visible after the last chunk with the
/// bytes and the content tag a put of the same bytes gives, and leaves no temporary file.
#[tokio::test]
async fn a_streamed_put_is_visible_only_after_the_last_chunk() {
    let (dir, backend) = backend();
    let (started, midway) = oneshot::channel();
    let (release, resume) = oneshot::channel();
    let putting = backend.put_stream("k", Some(4), gated("ab", "cd", started, resume));
    let watching = async {
        midway.await.unwrap();
        let absent = backend.head("k").await;
        assert_eq!(absent.unwrap_err(), Error::NotFound("k".to_owned()));
        assert!(has_temporary_file(dir.path()));
        release.send(()).unwrap();
    };
    let (put, ()) = tokio::join!(putting, watching);
    put.unwrap();
    assert_eq!(backend.get("k", None).await.unwrap(), "abcd");
    backend.put("whole", body("abcd")).await.unwrap();
    let (streamed, whole) = (backend.head("k").await, backend.head("whole").await);
    assert!(streamed.unwrap().same_content(&whole.unwrap()));
    assert!(!has_temporary_file(dir.path()));
}

/// Asserts that a body that fails half way, or at once, gives its error and leaves no object and
/// no temporary file, that the key can be put again, and that a key that exists is refused
/// after its body was read, with the first object kept and no temporary file left.
#[tokio::test]
async fn a_streamed_put_that_fails_or_finds_the_key_taken_leaves_only_what_was_there() {
    let (dir, backend) = backend();
    let cut = Error::Other("cut".to_owned());
    let items = vec![Ok(body("ab")), Err(cut.clone())];
    let half: PartBody<'static> = stream::iter(items).boxed();
    assert_eq!(backend.put_stream("k", None, half).await, Err(cut.clone()));
    let at_once: PartBody<'static> = stream::iter(vec![Err(cut.clone())]).boxed();
    assert_eq!(backend.put_stream("k", None, at_once).await, Err(cut));
    assert_eq!(names(dir.path()), [".uploads".to_owned()]);

    backend
        .put_stream("k", None, pieces(&["one"]))
        .await
        .unwrap();
    let taken = backend.put_stream("k", None, pieces(&["two"])).await;
    assert_eq!(taken, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(backend.get("k", None).await.unwrap(), "one");
    assert_eq!(names(dir.path()), [".uploads".to_owned(), "k".to_owned()]);
}

/// Asserts that a streamed put that is dropped half way (its caller gave up) removes its
/// temporary file and leaves no object.
#[tokio::test]
async fn a_streamed_put_that_is_dropped_half_way_leaves_nothing() {
    let (dir, backend) = backend();
    let (started, midway) = oneshot::channel();
    let (_release, resume) = oneshot::channel::<()>();
    let putting = backend.put_stream("k", None, gated("ab", "cd", started, resume));
    tokio::select! {
        _ = putting => panic!("the body was not given its second chunk"),
        _ = midway => {}
    }
    assert_eq!(names(dir.path()), [".uploads".to_owned()]);
    let absent = backend.head("k").await;
    assert_eq!(absent.unwrap_err(), Error::NotFound("k".to_owned()));
}

/// Asserts that a streamed put of a key that cannot be a file name is refused before the body is
/// taken.
#[tokio::test]
async fn a_streamed_put_of_a_key_that_is_no_file_name_is_refused() {
    let (_dir, backend) = backend();
    let refused = backend.put_stream("", None, pieces(&["x"])).await;
    assert!(matches!(refused, Err(Error::Other(_))));
}

/// Asserts that the mark of a reservation is the owner-execute bit of its file: a reservation
/// and a marked replace have it, a plain put has not, a plain replace clears it, and no temporary
/// file is left behind.
#[tokio::test]
async fn the_mark_is_the_owner_execute_bit_and_a_plain_replace_clears_it() {
    let (dir, backend) = backend();
    let bit = |name: &str| {
        let meta = std::fs::metadata(dir.path().join(name)).unwrap();
        meta.permissions().mode() & 0o100
    };
    backend.put("plain", body("x")).await.unwrap();
    let tag = backend.put_reservation("k", body("r")).await.unwrap();
    assert_eq!((bit("plain"), bit("k")), (0, 0o100));
    let kept = backend.put_reservation_if_match("k", body("s"), &tag);
    let kept = kept.await.unwrap();
    assert_eq!(bit("k"), 0o100);
    backend
        .put_if_match("k", body("real"), &kept)
        .await
        .unwrap();
    assert_eq!(bit("k"), 0);
    assert_eq!(names(dir.path()), [".uploads", "k", "plain"]);
}

/// Asserts that a reservation whose file lost its mode (a copy that drops modes) reads as a plain
/// object: it heads unmarked, and a read says so.
#[tokio::test]
async fn a_reservation_whose_mode_was_dropped_reads_as_a_plain_object() {
    let (dir, backend) = backend();
    let tag = backend.put_reservation("k", body("r")).await.unwrap();
    chmod(&dir.path().join("k"), 0o644);
    assert!(!backend.head("k").await.unwrap().reservation);
    let read = backend.get_marked("k", None).await.unwrap();
    assert_eq!(read, (body("r"), false, tag));
}

/// Asserts that a key that cannot be a file name is refused by a reservation, with nothing
/// written.
#[tokio::test]
async fn a_key_that_cannot_be_a_file_name_is_refused_by_a_reservation() {
    let (dir, backend) = backend();
    let created = backend.put_reservation("", body("r")).await;
    assert!(matches!(created, Err(Error::Other(_))), "{created:?}");
    assert_eq!(names(dir.path()), [".uploads"]);
}

/// A body of `len` bytes that differ from place to place, so that a chunk read from the wrong
/// place is not equal to the right one.
fn patterned(len: u64) -> Bytes {
    Bytes::from((0..len).map(|at| (at % 251) as u8).collect::<Vec<u8>>())
}

/// The sizes of the chunks `read` gives, and the bytes they make.
async fn drained(mut read: crate::ReadBody) -> (Vec<usize>, Vec<u8>) {
    let (mut sizes, mut bytes) = (Vec::new(), Vec::new());
    while let Some(chunk) = read.next().await {
        let chunk = chunk.unwrap();
        sizes.push(chunk.len());
        bytes.extend_from_slice(&chunk);
    }
    (sizes, bytes)
}

/// Asserts that a streamed read gives an object larger than one chunk as chunks of the bounded
/// size and a last one that is what is left, whole and by range, and that the chunks join to the
/// bytes a get gives.
#[tokio::test]
async fn a_streamed_read_gives_chunks_of_the_bounded_size() {
    let (_dir, backend) = backend();
    let chunk = super::READ_CHUNK;
    let held = patterned(2 * chunk + 5);
    backend.put("k", held.clone()).await.unwrap();

    let whole = backend.get_stream("k", None).await.unwrap();
    let (sizes, bytes) = drained(whole).await;
    assert_eq!(sizes, [chunk as usize, chunk as usize, 5]);
    assert_eq!(bytes, held);

    let range = ByteRange::new(100, chunk + 7);
    let part = backend.get_stream("k", Some(range)).await.unwrap();
    let (sizes, bytes) = drained(part).await;
    assert_eq!(sizes, [chunk as usize, 7]);
    assert_eq!(bytes, held.slice(100..100 + chunk as usize + 7));

    let none = backend.get_stream("k", Some(ByteRange::new(3, 0))).await;
    assert_eq!(drained(none.unwrap()).await, (Vec::new(), Vec::new()));
    let beyond = ByteRange::new(2 * chunk + 6, 1);
    let past = backend.get_stream("k", Some(beyond)).await;
    assert!(matches!(past, Err(Error::Other(_))), "{:?}", past.err());
    let absent = backend.get_stream("absent", None).await;
    assert_eq!(absent.err(), Some(Error::NotFound("absent".to_owned())));
}

/// Asserts that a streamed read takes a chunk from the file only when the chunk is asked for: the
/// file is cut after the first chunk was given, and the second chunk, which is where the file
/// ends, is an error that ends the body, with no chunk after it.
#[tokio::test]
async fn a_streamed_read_takes_each_chunk_from_the_file_when_it_is_asked_for() {
    let (dir, backend) = backend();
    let chunk = super::READ_CHUNK;
    backend.put("k", patterned(3 * chunk)).await.unwrap();
    let mut read = backend.get_stream("k", None).await.unwrap();
    let first = read.next().await.unwrap().unwrap();
    assert_eq!(first.len() as u64, chunk);

    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.path().join(encode("k")))
        .unwrap();
    file.set_len(super::TAG_LEN as u64 + chunk + 3).unwrap();
    let second = read.next().await.unwrap();
    assert!(matches!(second, Err(Error::Other(_))), "{second:?}");
    assert!(read.next().await.is_none());
}

/// Asserts that a read stays on the file it opened: the object is replaced, with a mark and then
/// without, and deleted after the read opened it, and the read still gives the bytes and the mark
/// of the object it opened, whole and by chunks.
#[tokio::test]
async fn a_read_stays_on_the_file_it_opened_when_the_key_is_replaced() {
    let (_dir, backend) = backend();
    let chunk = super::READ_CHUNK;
    let held = patterned(chunk + 9);
    let tag = backend.put_reservation("k", held.clone()).await.unwrap();
    let opened = backend.stored("k").await.unwrap();
    let mut streaming = backend.get_stream("k", None).await.unwrap();
    let first = streaming.next().await.unwrap().unwrap();
    assert_eq!(first, held.slice(..chunk as usize));

    let plain = backend.put_if_match("k", body("plain"), &tag);
    let plain = plain.await.unwrap();
    let restored = backend.put_reservation_if_match("k", body("again"), &plain);
    restored.await.unwrap();
    backend.delete("k").await.unwrap();

    let read = super::read_marked(opened, Some(ByteRange::new(2, 5))).await;
    assert_eq!(read.unwrap(), (held.slice(2..7), true, tag));
    let rest = streaming.next().await.unwrap().unwrap();
    assert_eq!(rest, held.slice(chunk as usize..));
    assert!(streaming.next().await.is_none());
}

/// Asserts that a read with the mark gives the content tag of the file it read, whatever the key
/// holds afterwards: reads opened on plain A, then on marked B, then the key is made plain A again
/// (the same bytes and tag as the first), and each read gives the bytes, the mark and the tag of
/// its own file, the tags of A and B differ, and a read of the key now gives A's.
#[tokio::test]
async fn a_read_with_the_mark_gives_the_tag_of_the_file_it_read_when_the_key_goes_a_b_a() {
    let (_dir, backend) = backend();
    backend.put("k", body("alpha")).await.unwrap();
    let tag_a = backend.head("k").await.unwrap().tag;
    let opened_a = backend.stored("k").await.unwrap();
    let to_b = backend.put_reservation_if_match("k", body("bravo"), &tag_a);
    let tag_b = to_b.await.unwrap();
    let opened_b = backend.stored("k").await.unwrap();
    let to_a = backend.put_if_match("k", body("alpha"), &tag_b);
    assert_eq!(to_a.await.unwrap(), tag_a);

    let read_b = super::read_marked(opened_b, None).await.unwrap();
    assert_eq!(read_b, (body("bravo"), true, tag_b.clone()));
    let read_a = super::read_marked(opened_a, None).await.unwrap();
    assert_eq!(read_a, (body("alpha"), false, tag_a.clone()));
    assert_ne!(tag_a, tag_b);
    let now = backend.get_marked("k", None).await.unwrap();
    assert_eq!(now, (body("alpha"), false, tag_a));
}

/// Asserts that a streamed replacement writes the chunks to a temporary file and replaces only
/// the object with the tag: a tag that does not match, a body that fails and an absent key leave
/// the object as it was and no temporary file; the right tag gives an unmarked object and its tag.
#[tokio::test]
async fn a_streamed_replace_leaves_no_temporary_file_and_takes_only_the_tag() {
    let (dir, backend) = backend();
    let old = backend.put_reservation("k", body("old")).await.unwrap();
    let wrong = backend.put_stream_if_match("k", None, pieces(&["a", "b"]), "no");
    assert_eq!(wrong.await, Err(Error::PreconditionFailed("k".to_owned())));
    let cut = Error::Other("cut".to_owned());
    let items: Vec<Result<Bytes, Error>> = vec![Ok(body("a")), Err(cut.clone())];
    let failing: PartBody<'static> = stream::iter(items).boxed();
    let failed = backend.put_stream_if_match("k", None, failing, &old).await;
    assert_eq!(failed, Err(cut));
    let absent = backend.put_stream_if_match("none", None, pieces(&["a"]), &old);
    assert_eq!(absent.await, Err(Error::NotFound("none".to_owned())));
    assert_eq!(names(dir.path()), [".uploads", "k"]);
    let kept = backend.get_marked("k", None).await.unwrap();
    assert_eq!(kept, (body("old"), true, old.clone()));

    let new = backend.put_stream_if_match("k", None, pieces(&["n", "ew"]), &old);
    let new = new.await.unwrap();
    assert_eq!(backend.head("k").await.unwrap().tag, new);
    let read = backend.get_marked("k", None).await.unwrap();
    assert_eq!(read, (body("new"), false, new.clone()));
    assert_eq!(names(dir.path()), [".uploads", "k"]);

    let refused = backend.put_stream_if_match("", None, pieces(&["x"]), &new);
    let refused = refused.await;
    assert!(matches!(refused, Err(Error::Other(_))), "{refused:?}");
}

/// Asserts that a streamed replacement that is dropped half way (its caller gave up) removes its
/// temporary file and leaves the object as it was.
#[tokio::test]
async fn a_streamed_replace_that_is_dropped_half_way_leaves_the_object() {
    let (dir, backend) = backend();
    backend.put("k", body("old")).await.unwrap();
    let tag = backend.head("k").await.unwrap().tag;
    let (started, midway) = oneshot::channel();
    let (_release, resume) = oneshot::channel::<()>();
    let gated = gated("ab", "cd", started, resume);
    let replacing = backend.put_stream_if_match("k", None, gated, &tag);
    tokio::select! {
        _ = replacing => panic!("the body was not given its second chunk"),
        _ = midway => {}
    }
    assert_eq!(names(dir.path()), [".uploads", "k"]);
    assert_eq!(backend.get("k", None).await.unwrap(), "old");
}

/// Asserts that two keys that differ only by case are stored under file names that differ in more
/// than case, so that on a case-insensitive file system they are still two objects.
#[tokio::test]
async fn keys_that_differ_only_by_case_are_two_files() {
    let (dir, backend) = backend();
    backend.put("Foo", body("upper")).await.unwrap();
    backend.put("foo", body("lower")).await.unwrap();
    assert_eq!(backend.get("Foo", None).await.unwrap(), body("upper"));
    assert_eq!(backend.get("foo", None).await.unwrap(), body("lower"));
    assert_eq!(names(dir.path()), ["%46oo", "foo"]);
    let folded: Vec<String> = names(dir.path())
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let mut distinct = folded.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(folded.len(), distinct.len(), "{folded:?}");
}

/// Writes the file of an object as the releases before this one did: under `name`, which has
/// its upper-case letters as they are.
fn write_legacy(dir: &Path, name: &str, text: &str) {
    let mut data = sha256(text.as_bytes()).to_vec();
    data.extend_from_slice(text.as_bytes());
    std::fs::write(dir.join(name), data).unwrap();
}

/// Asserts that a directory written by the previous release, with upper-case letters in the
/// file names, is read by this release: get, head, list, a replacement, a create-only put that
/// finds the key taken, and a delete all act on the old file and make no second one.
#[tokio::test]
async fn this_release_reads_a_directory_the_previous_release_wrote() {
    let (dir, backend) = backend();
    write_legacy(dir.path(), "Foo", "old");
    write_legacy(dir.path(), "a~B", "deep");
    assert_eq!(encode_legacy("Foo"), "Foo");
    assert_eq!(backend.get("Foo", None).await.unwrap(), body("old"));
    assert_eq!(backend.get("a/B", None).await.unwrap(), body("deep"));
    let head = backend.head("Foo").await.unwrap();
    assert_eq!((head.size, head.tag), (3, hex::encode(sha256(b"old"))));
    let keys: Vec<String> = backend
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|listed| listed.key)
        .collect();
    assert_eq!(keys, ["Foo", "a/B"]);
    let taken = backend.put("Foo", body("new")).await;
    assert!(matches!(taken, Err(Error::AlreadyExists(_))), "{taken:?}");
    let tag = hex::encode(sha256(b"old"));
    let replaced = backend.put_if_match("Foo", body("newer"), &tag).await;
    replaced.unwrap();
    assert_eq!(backend.get("Foo", None).await.unwrap(), body("newer"));
    assert_eq!(names(dir.path()), ["Foo", "a~B"]);
    backend.delete("Foo").await.unwrap();
    backend.delete("a/B").await.unwrap();
    assert_eq!(names(dir.path()), Vec::<String>::new());
    backend.put("Foo", body("again")).await.unwrap();
    assert_eq!(names(dir.path()), ["%46oo"]);
}

/// Asserts that the data this release writes is read by the previous release's rules where it
/// can be: a key with no upper-case letter has the file name it always had, and the name of a
/// key with one is `%XX`, which the previous release lists as that key. (The previous release
/// looks a key with an upper-case letter up under its literal name, so it does not get such a
/// key.)
#[tokio::test]
async fn the_previous_release_lists_what_this_release_writes() {
    let (dir, backend) = backend();
    backend.put("cas/d/n1/00012", body("x")).await.unwrap();
    backend.put("Up/Per", body("y")).await.unwrap();
    assert_eq!(names(dir.path()), ["%55p~%50er", "cas~d~n1~00012"]);
    let keys: Vec<String> = backend
        .list("")
        .await
        .unwrap()
        .into_iter()
        .map(|listed| listed.key)
        .collect();
    assert_eq!(keys, ["Up/Per", "cas/d/n1/00012"]);
    for name in names(dir.path()) {
        let key = decode(&name).unwrap();
        assert!(encode(&key) == name, "{name}");
    }
}

/// Asserts that an upper-case key the previous release took, whose name is within 255 bytes with
/// the letters as they are but over it with `%XX`, is still taken (under the old name), and that
/// a key too long for both is refused.
#[tokio::test]
async fn an_upper_case_key_that_fits_only_under_the_old_name_is_kept() {
    let (dir, backend) = backend();
    let key = "A".repeat(100);
    assert!(encode(&key).len() > NAME_LIMIT);
    backend.put(&key, body("long")).await.unwrap();
    assert_eq!(backend.get(&key, None).await.unwrap(), body("long"));
    assert_eq!(names(dir.path()), std::slice::from_ref(&key));
    let refused = backend.put(&"A".repeat(NAME_LIMIT + 1), body("x")).await;
    assert!(matches!(refused, Err(Error::Other(_))), "{refused:?}");
}
