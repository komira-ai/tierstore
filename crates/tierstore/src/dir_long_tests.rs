//! The keys of a directory tier whose encoded name is over 255 bytes: their hashed file names and
//! the key in the file, and the files of the keys that are not hashed, which are what the
//! previous release wrote.

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};
use sha2::{Digest, Sha256};
use tierstore_testkit::dir::TestDir;

use crate::{Backend, ByteRange, DirBackend, Error, Listed, PartBody};

use super::names::{self, MAX_KEY, NAME_LIMIT, encode, file_name, hashed_name};
use super::{TAG_LEN, sha256};

fn backend() -> (TestDir, DirBackend) {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-long");
    let backend = DirBackend::open(dir.path()).unwrap();
    (dir, backend)
}

fn body(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

fn part(text: &'static str) -> PartBody<'static> {
    stream::iter(vec![Ok(body(text))]).boxed()
}

/// The sorted names of the files in a directory.
fn names_in(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The keys of `listed`.
fn keys_of(listed: &[Listed]) -> Vec<&str> {
    listed.iter().map(|object| object.key.as_str()).collect()
}

/// The key `stem` and `tail`: a key whose encoded name is over 255 bytes.
fn long_key(stem: char, tail: &str) -> String {
    format!("{}{tail}", stem.to_string().repeat(300))
}

/// The hex digits of the SHA-256 of `text`.
fn digest_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text))
}

/// The file the previous release made for `key`, or this release for a key of a plain name:
/// the SHA-256 of the body and then the body, named by the encoded key. Gives the tag.
fn plain_file(dir: &TestDir, key: &str, text: &str) -> String {
    let tag = sha256(text.as_bytes());
    let mut file = tag.to_vec();
    file.extend_from_slice(text.as_bytes());
    std::fs::write(dir.path().join(encode(key)), file).unwrap();
    hex::encode(tag)
}

/// The whole of the body that `get_stream` gives for `key`.
async fn streamed(backend: &DirBackend, key: &str, range: Option<ByteRange>) -> Vec<u8> {
    let chunks: Vec<Bytes> = backend
        .get_stream(key, range)
        .await
        .unwrap()
        .try_collect()
        .await
        .unwrap();
    chunks.concat()
}

/// Asserts the hashed name: the first 100 bytes of the encoded name, a `~` and the SHA-256 of the
/// whole key in 165 bytes, for a key over 255 encoded bytes, whatever the character, and for a
/// key under `_entries/` by its whole key; no hashed name for a name of 255 bytes; no file name
/// for the empty key and for a key over `MAX_KEY` bytes.
#[test]
fn a_key_over_255_encoded_bytes_has_the_hashed_name_of_its_first_100_bytes_and_its_digest() {
    let key = long_key('a', "");
    let name = hashed_name(&key).unwrap();
    assert_eq!(name, format!("{}~{}", "a".repeat(100), digest_hex(&key)));
    assert_eq!(name.len(), 165);
    assert_eq!(file_name(&key), Some(name));

    let wide = "\u{e9}".repeat(100);
    let name = hashed_name(&wide).unwrap();
    assert_eq!(
        name,
        format!("{}~{}", &encode(&wide)[..100], digest_hex(&wide))
    );

    let entry = format!("_entries/{}", "b".repeat(256));
    let name = hashed_name(&entry).unwrap();
    assert_eq!(name, format!("{}~{}", "b".repeat(100), digest_hex(&entry)));

    let longest = "a".repeat(NAME_LIMIT);
    assert_eq!(hashed_name(&longest), None);
    assert_eq!(file_name(&longest), Some(longest));
    assert_eq!(file_name(""), None);
    assert_eq!(file_name("_entries/"), None);
    assert!(file_name(&"a".repeat(MAX_KEY)).is_some());
    assert_eq!(file_name(&"a".repeat(MAX_KEY + 1)), None);
}

/// Asserts that two keys that differ only after the first 100 bytes of their names have two names.
#[test]
fn keys_that_differ_after_the_kept_bytes_have_different_names() {
    let (one, two) = (long_key('a', "1"), long_key('a', "2"));
    assert_ne!(hashed_name(&one), hashed_name(&two));
}

/// Asserts that the header behind the tag is empty for a plain name and for a long one holds the
/// length of the key as 8 bytes, big-endian, and the key.
#[test]
fn the_header_of_a_hashed_name_is_the_length_of_the_key_and_the_key() {
    assert!(names::header("short/key").is_empty());
    let key = long_key('c', "!");
    let header = names::header(&key);
    assert_eq!(header[..8], 301_u64.to_be_bytes());
    assert_eq!(&header[8..], key.as_bytes());
}

/// Asserts that a name has the shape of a hashed name only with 100 bytes, a `~` and 64 lower-case
/// hex digits.
#[test]
fn a_name_has_the_shape_of_a_hashed_name_only_with_a_tilde_and_64_lower_case_hex_digits() {
    let digits = "0123456789abcdef".repeat(4);
    let shaped = format!("{}~{digits}", "a".repeat(100));
    assert!(names::is_hashed_shape(&shaped));
    let upper = format!("{}~{}", "a".repeat(100), digits.to_uppercase());
    let not_hex = format!("{}~{}g", "a".repeat(100), &digits[1..]);
    let no_tilde = format!("{}-{digits}", "a".repeat(100));
    let short = format!("{}~{}", "a".repeat(100), &digits[1..]);
    let long = format!("{}~{digits}0", "a".repeat(100));
    for name in [upper, not_hex, no_tilde, short, long, "short".to_owned()] {
        assert!(!names::is_hashed_shape(&name), "{name}");
    }
}

/// Asserts that a key of 256 encoded bytes, of 1024 bytes, of 1024 bytes of two-byte characters
/// and one under `_entries/` are each one file, named by its hashed name and holding the content
/// tag, the length of the key, the key and the body; that a plain key beside them keeps its plain
/// name and the previous release's layout; and that nothing else is made.
#[tokio::test]
async fn a_long_key_is_one_file_with_a_hashed_name_that_holds_its_key() {
    let (dir, backend) = backend();
    let keys = [
        "a".repeat(256),
        "k".repeat(1024),
        "\u{e9}".repeat(512),
        format!("_entries/{}", "e".repeat(300)),
    ];
    for key in &keys {
        backend.put(key, body("data")).await.unwrap();
    }
    backend.put("plain", body("data")).await.unwrap();

    let tag = sha256(b"data");
    let root = names_in(dir.path());
    let entries = names_in(&dir.path().join(".entries"));
    for key in &keys {
        let name = hashed_name(key).unwrap();
        let home = if key.starts_with("_entries/") {
            assert!(entries.contains(&name), "{name}");
            dir.path().join(".entries")
        } else {
            assert!(root.contains(&name), "{name}");
            dir.path().to_owned()
        };
        let file = std::fs::read(home.join(&name)).unwrap();
        let mut expected = tag.to_vec();
        expected.extend_from_slice(&(key.len() as u64).to_be_bytes());
        expected.extend_from_slice(key.as_bytes());
        expected.extend_from_slice(b"data");
        assert_eq!(file, expected, "{name}");
    }
    let plain = std::fs::read(dir.path().join("plain")).unwrap();
    assert_eq!(plain, [&tag[..], b"data"].concat());
    assert_eq!(root.len(), 6, "{root:?}");
}

/// Asserts that a long key is found by every call that has the key: head gives its size and tag,
/// get gives the body whole and by range (the range counted from the body, not the header), the
/// streamed read and the open read give it, a second put finds the key taken, and a listing gives
/// the key, the size of the body and the tag, among plain keys and in key order.
#[tokio::test]
async fn a_long_key_is_found_by_head_get_streams_put_and_list() {
    let (_dir, backend) = backend();
    let key = long_key('a', "/x y");
    let tag = backend.put_tagged(&key, body("0123456789")).await.unwrap();
    backend.put("a", body("plain")).await.unwrap();
    backend.put("b", body("plain")).await.unwrap();

    let head = backend.head(&key).await.unwrap();
    assert_eq!((head.size, head.tag.as_str()), (10, tag.as_str()));
    assert_eq!(tag, hex::encode(sha256(b"0123456789")));
    assert_eq!(backend.get(&key, None).await.unwrap(), body("0123456789"));
    let range = Some(ByteRange::new(3, 4));
    assert_eq!(backend.get(&key, range).await.unwrap(), body("3456"));
    assert_eq!(streamed(&backend, &key, None).await, b"0123456789");
    assert_eq!(streamed(&backend, &key, range).await, b"3456");
    let tagged = backend.get_tagged(&key, range).await.unwrap();
    assert_eq!(tagged, (body("3456"), tag.clone()));
    let opened = backend.open_read(&key, None).await.unwrap();
    assert_eq!((opened.meta.size, opened.meta.tag), (10, tag.clone()));
    let read: Vec<Bytes> = opened.body.try_collect().await.unwrap();
    assert_eq!(read.concat(), b"0123456789");

    let again = backend.put(&key, body("other")).await;
    assert_eq!(again, Err(Error::AlreadyExists(key.clone())));
    assert_eq!(backend.get(&key, None).await.unwrap(), body("0123456789"));

    let all = backend.list("").await.unwrap();
    assert_eq!(keys_of(&all), ["a", key.as_str(), "b"].map(|k| k));
    let listed = &all[1];
    assert_eq!((listed.size, listed.tag.as_str()), (10, tag.as_str()));
    let under = backend.list(&key[..150]).await.unwrap();
    assert_eq!(keys_of(&under), [key.as_str()]);
    assert!(
        backend
            .list("b")
            .await
            .unwrap()
            .iter()
            .all(|o| o.key == "b")
    );
    let page = backend.list_page("", Some("a"), 1).await.unwrap();
    assert_eq!(keys_of(&page), [key.as_str()]);
}

/// Asserts that the replacements and the removals of a long key work on its file by the key: a
/// replacement by the tag (whole and streamed) keeps the key in the file and gives the new body;
/// a stale tag is refused; a conditional delete by the tag and a plain delete remove the file, and
/// a delete of a key that is not there is not an error; the key can be put again.
#[tokio::test]
async fn a_long_key_is_replaced_and_deleted_by_its_key() {
    let (dir, backend) = backend();
    let key = long_key('r', "");
    let first = backend.put_tagged(&key, body("one")).await.unwrap();
    let stale = backend.put_if_match(&key, body("x"), "stale").await;
    assert_eq!(stale, Err(Error::PreconditionFailed(key.clone())));
    let second = backend.put_if_match(&key, body("two"), &first).await;
    let second = second.unwrap();
    assert_eq!(backend.get(&key, None).await.unwrap(), body("two"));
    let streamed_in = backend.put_stream_if_match(&key, None, part("three"), &second);
    let third = streamed_in.await.unwrap();
    assert_eq!(backend.head(&key).await.unwrap().tag, third);
    assert_eq!(backend.get(&key, None).await.unwrap(), body("three"));
    let all = backend.list("").await.unwrap();
    assert_eq!((keys_of(&all), all[0].size), ([key.as_str()].to_vec(), 5));

    let refused = backend.delete_if_match(&key, &first).await;
    assert_eq!(refused, Err(Error::PreconditionFailed(key.clone())));
    backend.delete_if_match(&key, &third).await.unwrap();
    assert_eq!(names_in(dir.path()), [".uploads"]);
    assert!(matches!(backend.head(&key).await, Err(Error::NotFound(_))));
    let absent = backend.delete_if_match(&key, &third).await;
    assert_eq!(absent, Err(Error::NotFound(key.clone())));

    let again = backend.put_stream_tagged(&key, None, part("four")).await;
    again.unwrap();
    assert_eq!(backend.get(&key, None).await.unwrap(), body("four"));
    backend.delete(&key).await.unwrap();
    backend.delete(&key).await.unwrap();
    assert!(backend.list("").await.unwrap().is_empty());
    assert_eq!(names_in(dir.path()), [".uploads"]);
}

/// Asserts that a multipart upload of a long key keeps its key in the upload (listed with the
/// key), joins its parts into the one file of the hashed name with the key in it, and is read
/// back by the key; and that an upload of another key cannot use the id.
#[tokio::test]
async fn a_multipart_upload_of_a_long_key_completes_into_the_hashed_file() {
    let (dir, backend) = backend();
    let key = long_key('m', "/part");
    let id = backend.create_upload(&key).await.unwrap();
    let open = backend.list_uploads("", None, 10).await.unwrap();
    assert_eq!((open[0].key.as_str(), open.len()), (key.as_str(), 1));
    let one = backend.upload_part(&key, &id, 1, part("ab")).await.unwrap();
    let two = backend.upload_part(&key, &id, 2, part("cd")).await.unwrap();
    let other = backend
        .upload_part(&long_key('n', ""), &id, 1, part("x"))
        .await;
    assert!(matches!(other, Err(Error::NotFound(_))));
    let whole = backend
        .complete_upload(&key, &id, &[(1, one), (2, two)])
        .await
        .unwrap();
    assert_eq!(whole, hex::encode(sha256(b"abcd")));
    assert_eq!(backend.get(&key, None).await.unwrap(), body("abcd"));
    let head = backend.head(&key).await.unwrap();
    assert_eq!((head.size, head.tag), (4, whole));
    let name = hashed_name(&key).unwrap();
    assert_eq!(names_in(dir.path()), [".uploads".to_owned(), name]);
    let all = backend.list("").await.unwrap();
    assert_eq!((keys_of(&all), all[0].size), ([key.as_str()].to_vec(), 4));

    let gone = long_key('g', "");
    let id = backend.create_upload(&gone).await.unwrap();
    backend.abort_upload(&gone, &id).await.unwrap();
    assert!(backend.list_uploads("", None, 10).await.unwrap().is_empty());
}

/// Asserts that a long key under `_entries/` is a file of the subdirectory `.entries` with a
/// hashed name, found by head, get, replacement, listing and delete, and listed only where the
/// prefix can match it.
#[tokio::test]
async fn a_long_entry_key_is_a_hashed_file_of_the_entries_subdirectory() {
    let (dir, backend) = backend();
    let key = format!("_entries/{}", "z".repeat(400));
    let tag = backend.put_tagged(&key, body("entry")).await.unwrap();
    let inside = dir.path().join(".entries");
    assert_eq!(
        names_in(&inside),
        [".lock".to_owned(), hashed_name(&key).unwrap()]
    );
    assert_eq!(names_in(dir.path()), [".entries", ".uploads"]);
    assert_eq!(backend.get(&key, None).await.unwrap(), body("entry"));
    let new = backend
        .put_if_match(&key, body("next"), &tag)
        .await
        .unwrap();
    assert_eq!(backend.head(&key).await.unwrap().tag, new);
    for prefix in ["", "_entries/", "_entries/zz"] {
        let listed = backend.list(prefix).await.unwrap();
        assert_eq!(keys_of(&listed), [key.as_str()], "{prefix:?}");
        assert_eq!(listed[0].size, 4);
    }
    assert!(backend.list("a").await.unwrap().is_empty());
    let again = backend.put(&key, body("x")).await;
    assert_eq!(again, Err(Error::AlreadyExists(key.clone())));
    backend.delete_if_match(&key, &new).await.unwrap();
    assert!(backend.list("").await.unwrap().is_empty());
}

/// Asserts that objects stored by the previous release, which had no hashed names, stay readable,
/// listed and deletable: files written by hand as it wrote them (the tag and the body, named by
/// the encoded key) for a key of exactly 255 encoded bytes, a key with a `~` and 64 hex digits
/// after 100 bytes (a plain name of the shape of a hashed one, with a body that holds no key),
/// and one whose body begins as a header would; each is found by head, get, a range, a
/// replacement and a delete, listed once under its own key with the size of its whole body.
#[tokio::test]
async fn files_of_the_previous_release_stay_readable_listed_and_deletable() {
    let (dir, backend) = backend();
    let exact = "x".repeat(NAME_LIMIT);
    let digits = digest_hex("anything");
    let shaped = format!("{}/{digits}", "s".repeat(100));
    assert!(names::is_hashed_shape(&encode(&shaped)));
    let tags = [
        (&exact, plain_file(&dir, &exact, "exact body")),
        (&shaped, plain_file(&dir, &shaped, "shaped body")),
    ];
    let header_like = format!("{}/{}", "h".repeat(100), digest_hex("h"));
    let mut lookalike = 8_u64.to_be_bytes().to_vec();
    lookalike.extend_from_slice(b"hhhhhhhh");
    let tag = sha256(&lookalike);
    let mut file = tag.to_vec();
    file.extend_from_slice(&lookalike);
    std::fs::write(dir.path().join(encode(&header_like)), file).unwrap();

    let all = backend.list("").await.unwrap();
    let mut want = [exact.as_str(), shaped.as_str(), header_like.as_str()];
    want.sort_unstable();
    assert_eq!(keys_of(&all), want);
    for (key, tag) in &tags {
        let head = backend.head(key).await.unwrap();
        assert_eq!(&head.tag, tag);
        let listed = all.iter().find(|object| &object.key == *key).unwrap();
        assert_eq!((listed.size, &listed.tag), (head.size, tag));
    }
    assert_eq!(backend.get(&exact, None).await.unwrap(), body("exact body"));
    let range = Some(ByteRange::new(7, 4));
    assert_eq!(backend.get(&shaped, range).await.unwrap(), body("body"));
    assert_eq!(streamed(&backend, &shaped, None).await, b"shaped body");
    let held = backend.get(&header_like, None).await.unwrap();
    assert_eq!(held.len(), lookalike.len());
    let listed = all.iter().find(|object| object.key == header_like).unwrap();
    assert_eq!(listed.size, lookalike.len() as u64);

    let taken = backend.put(&exact, body("new")).await;
    assert_eq!(taken, Err(Error::AlreadyExists(exact.clone())));
    let replaced = backend.put_if_match(&shaped, body("replaced"), &tags[1].1);
    replaced.await.unwrap();
    assert_eq!(backend.get(&shaped, None).await.unwrap(), body("replaced"));
    backend.delete_if_match(&exact, &tags[0].1).await.unwrap();
    backend.delete(&shaped).await.unwrap();
    backend.delete(&header_like).await.unwrap();
    assert!(backend.list("").await.unwrap().is_empty());
}

/// Asserts that this release writes a key whose name is at most 255 bytes as the previous release
/// did: the file is named by the encoded key and holds the tag and the body, nothing between.
#[tokio::test]
async fn a_key_of_a_plain_name_is_written_as_the_previous_release_wrote_it() {
    let (dir, backend) = backend();
    let key = "x".repeat(NAME_LIMIT);
    backend.put(&key, body("body")).await.unwrap();
    backend.put_stream("s/t", None, part("body")).await.unwrap();
    let tag = sha256(b"body");
    for name in [key, "s~t".to_owned()] {
        let file = std::fs::read(dir.path().join(&name)).unwrap();
        assert_eq!(file.len(), TAG_LEN + 4, "{name}");
        assert_eq!(file, [&tag[..], b"body"].concat(), "{name}");
    }
}

/// Asserts that a file with the hashed name of a key that holds another key (as a name made by
/// hand to collide could) is not that key's object: a get, a head and a replacement by the key find
/// nothing, a put finds the name taken, and a listing gives the file as the plain key its name
/// decodes to (it holds no key with that hashed name).
#[tokio::test]
async fn a_hashed_file_that_holds_another_key_is_not_the_object_of_the_key() {
    let (dir, backend) = backend();
    let key = long_key('c', "1");
    let owner = long_key('c', "2");
    let mut file = sha256(b"theirs").to_vec();
    file.extend_from_slice(&names::header(&owner));
    file.extend_from_slice(b"theirs");
    std::fs::write(dir.path().join(hashed_name(&key).unwrap()), &file).unwrap();

    assert_eq!(
        backend.get(&key, None).await,
        Err(Error::NotFound(key.clone()))
    );
    assert_eq!(backend.head(&key).await, Err(Error::NotFound(key.clone())));
    let taken = backend.put(&key, body("mine")).await;
    assert_eq!(taken, Err(Error::AlreadyExists(key.clone())));
    let replaced = backend.put_if_match(&key, body("mine"), "tag").await;
    assert_eq!(replaced, Err(Error::NotFound(key.clone())));
    let name = hashed_name(&key).unwrap();
    let all = backend.list("").await.unwrap();
    assert_eq!(keys_of(&all), [names::decode(&name).unwrap()]);
}

/// Asserts that a file with a name of the shape of a hashed one whose header does not hold a key
/// with that hashed name is listed as the plain key its name decodes to, or not at all if the name
/// is no encoded key: a header too short to read, a key that is not UTF-8, a length that is not
/// the key's, a key that hashes to another name, a short key, a key under `_entries/` in the root,
/// and a directory of that name.
#[tokio::test]
async fn a_file_of_the_shape_of_a_hashed_name_is_listed_only_as_a_key_it_holds() {
    let (dir, backend) = backend();
    let key = long_key('d', "");
    let digits = digest_hex(&key);
    let entry = format!("_entries/{}", "d".repeat(300));
    let short = "short".to_owned();
    let tag = sha256(b"t");
    let make = |stem: &str, header: &[u8]| {
        let name = format!("{}~{digits}", stem.repeat(100));
        let mut file = tag.to_vec();
        file.extend_from_slice(header);
        std::fs::write(dir.path().join(&name), file).unwrap();
        name
    };
    let with_len = |len: u64, key: &[u8]| [&len.to_be_bytes()[..], key].concat();
    let cases = [
        ("a", Vec::new()),
        ("b", with_len(3, &[0xff, 0xfe, 0xfd])),
        ("c", with_len(7, key.as_bytes())),
        ("e", with_len(5, short.as_bytes())),
        ("f", with_len(entry.len() as u64, entry.as_bytes())),
        (
            "g",
            with_len(key.len() as u64, key.replace('d', "q").as_bytes()),
        ),
    ];
    let mut expected = Vec::new();
    for (stem, header) in &cases {
        let name = make(stem, header);
        expected.push(super::names::decode(&name).unwrap());
    }
    std::fs::create_dir(dir.path().join(format!("{}~{digits}", "w".repeat(100)))).unwrap();
    // The one that holds its own key, in the root, is the only long key.
    let own = hashed_name(&key).unwrap();
    let mut file = tag.to_vec();
    file.extend_from_slice(&names::header(&key));
    std::fs::write(dir.path().join(own), file).unwrap();
    expected.push(key.clone());
    expected.sort();

    let all = backend.list("").await.unwrap();
    assert_eq!(keys_of(&all), expected);
    let oversized = with_len(MAX_KEY as u64 + 1, b"x");
    let name = format!("{}~{digits}", "o".repeat(100));
    std::fs::write(dir.path().join(&name), [&tag[..], &oversized].concat()).unwrap();
    let all = backend.list("o").await.unwrap();
    assert_eq!(keys_of(&all), [super::names::decode(&name).unwrap()]);
}
