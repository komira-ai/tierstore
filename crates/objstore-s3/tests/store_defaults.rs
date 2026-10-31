//! What the `ObjectStore` trait gives a store that implements only its required methods,
//! how `listing::objects` refuses a store that breaks the listing contract, and the token
//! and prefix helpers a store builds on. The store here is a map in memory with a scripted
//! `list`.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::SystemTime;

use bytes::Bytes;
use futures::{StreamExt as _, stream};
use objstore_s3::{
    BodyStream, ByteRange, Capabilities, KeyPrefix, ListPage, ListToken, ObjectInfo, ObjectKey,
    ObjectStore, ObjectStoreError, PageSize, listing,
};

/// A store with only the required methods: objects in a map, and `list` answering the
/// pages it was given, in order.
#[derive(Default)]
struct MapStore {
    objects: Mutex<BTreeMap<String, Bytes>>,
    pages: Mutex<VecDeque<ListPage>>,
    asked: Mutex<Vec<Option<ListToken>>>,
}

impl ObjectStore for MapStore {
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    async fn put_new(
        &self,
        key: &ObjectKey,
        body: Bytes,
        _retain_until: Option<SystemTime>,
    ) -> Result<(), ObjectStoreError> {
        self.objects
            .lock()
            .unwrap()
            .insert(key.as_str().to_owned(), body);
        Ok(())
    }

    async fn get_range(
        &self,
        key: &ObjectKey,
        range: ByteRange,
    ) -> Result<Bytes, ObjectStoreError> {
        let objects = self.objects.lock().unwrap();
        let bytes = objects
            .get(key.as_str())
            .ok_or_else(|| ObjectStoreError::NotFound(key.clone()))?;
        let from = usize::try_from(range.offset()).unwrap().min(bytes.len());
        let to = usize::try_from(range.offset() + range.size())
            .unwrap()
            .min(bytes.len());
        Ok(bytes.slice(from..to))
    }

    async fn delete(&self, key: &ObjectKey) -> Result<(), ObjectStoreError> {
        self.objects.lock().unwrap().remove(key.as_str());
        Ok(())
    }

    async fn list(
        &self,
        _prefix: &KeyPrefix,
        after: Option<&ListToken>,
        _max_keys: PageSize,
    ) -> Result<ListPage, ObjectStoreError> {
        self.asked.lock().unwrap().push(after.cloned());
        Ok(self
            .pages
            .lock()
            .unwrap()
            .pop_front()
            .expect("a page was scripted"))
    }
}

fn key(text: &str) -> ObjectKey {
    ObjectKey::new(text).unwrap()
}

fn info(text: &str) -> ObjectInfo {
    ObjectInfo {
        key: key(text),
        size: 1,
    }
}

fn pieces(parts: &[&'static str]) -> BodyStream {
    let parts: Vec<Result<Bytes, ObjectStoreError>> = parts
        .iter()
        .map(|p| Ok(Bytes::from_static(p.as_bytes())))
        .collect();
    stream::iter(parts).boxed()
}

async fn listed(store: &MapStore, prefix: &str, from: Option<ListToken>) -> Vec<String> {
    let prefix = KeyPrefix::new(prefix).unwrap();
    let all = listing::objects(store, &prefix, from, PageSize::MAX)
        .collect::<Vec<_>>()
        .await;
    all.into_iter()
        .map(|r| match r {
            Ok(o) => o.key.to_string(),
            Err(e) => format!("error: {e}"),
        })
        .collect()
}

fn scripted(pages: Vec<ListPage>) -> MapStore {
    let store = MapStore::default();
    store.pages.lock().unwrap().extend(pages);
    store
}

/// Asserts that the default `put_stream` gathers the body and stores it through
/// `put_new`: the pieces arrive joined under the key, and a body of the wrong length
/// stores nothing and fails with the body's length error.
#[tokio::test]
async fn the_default_put_stream_gathers_the_body_into_put_new() {
    let store = MapStore::default();
    let k = key("seg/one");
    store
        .put_stream(&k, 5, pieces(&["he", "", "llo"]))
        .await
        .unwrap();
    let all = ByteRange::new(0, 5).unwrap();
    assert_eq!(store.get_range(&k, all).await.unwrap(), "hello");

    let short = key("seg/short");
    let err = store
        .put_stream(&short, 5, pieces(&["he"]))
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "the body of a streamed write failed: the body ended after 2 of its 5 bytes"
    );
    assert!(store.objects.lock().unwrap().get("seg/short").is_none());
}

/// Asserts that the default `may_hold` answers `true` for any key, present or not: a
/// store that cannot tell without a request leaves the read to find out.
#[tokio::test]
async fn the_default_may_hold_says_yes() {
    let store = MapStore::default();
    assert!(store.may_hold(&key("nothing/here")).await.unwrap());
    store
        .put_new(&key("there"), Bytes::from_static(b"x"), None)
        .await
        .unwrap();
    assert!(store.may_hold(&key("there")).await.unwrap());
}

/// Asserts that the default `len` refuses with `Unsupported("object length")` rather
/// than answering a length it did not look up, whether or not the object exists.
#[tokio::test]
async fn the_default_len_is_unsupported() {
    let store = MapStore::default();
    store
        .put_new(&key("there"), Bytes::from_static(b"x"), None)
        .await
        .unwrap();
    for name in ["there", "absent"] {
        let err = store.len(&key(name)).await.unwrap_err();
        assert!(
            matches!(err, ObjectStoreError::Unsupported("object length")),
            "{err:?}"
        );
        assert_eq!(err.to_string(), "the store does not support object length");
    }
}

/// Asserts that a listing ends with a protocol error, naming the prefix, when the store
/// answers an empty page whose next token is the one it was asked with (a store that
/// would page forever), and that the stream yields nothing after the error.
#[tokio::test]
async fn an_empty_page_with_the_token_it_was_asked_with_is_refused() {
    let token = ListToken::continuation("same");
    let store = scripted(vec![ListPage {
        objects: vec![],
        next: Some(token.clone()),
    }]);
    let got = listed(&store, "run/", Some(token.clone())).await;
    assert_eq!(
        got,
        [
            "error: store broke the protocol: listing \"run/\": an empty page whose token is \
             the one it was asked with"
        ]
    );
    assert_eq!(*store.asked.lock().unwrap(), [Some(token)]);
}

/// Asserts that an empty page with a different next token is not an error (the listing
/// goes on and yields what the next page holds), so the refusal above is about the
/// repeated token and not about emptiness.
#[tokio::test]
async fn an_empty_page_with_a_new_token_is_followed() {
    let store = scripted(vec![
        ListPage {
            objects: vec![],
            next: Some(ListToken::continuation("two")),
        },
        ListPage {
            objects: vec![info("run/a")],
            next: None,
        },
    ]);
    let got = listed(&store, "run/", Some(ListToken::continuation("one"))).await;
    assert_eq!(got, ["run/a"]);
    assert_eq!(
        *store.asked.lock().unwrap(),
        [
            Some(ListToken::continuation("one")),
            Some(ListToken::continuation("two"))
        ]
    );
}

/// Asserts that a listed key outside the prefix ends the listing with a protocol error
/// naming the prefix and the key; the page is checked whole before any of it is yielded,
/// so the key before the bad one is not yielded either.
#[tokio::test]
async fn a_key_outside_the_prefix_is_refused() {
    let store = scripted(vec![ListPage {
        objects: vec![info("run/a"), info("other/b")],
        next: None,
    }]);
    let got = listed(&store, "run/", None).await;
    assert_eq!(
        got,
        ["error: store broke the protocol: listing \"run/\" returned other/b, which is outside it"]
    );
}

/// Asserts that a key not greater than the one before it, across two pages, is refused
/// with the keys-must-ascend error, and that the keys of the first page were yielded.
#[tokio::test]
async fn a_key_that_does_not_ascend_is_refused() {
    let store = scripted(vec![
        ListPage {
            objects: vec![info("run/b")],
            next: Some(ListToken::after(&key("run/b"))),
        },
        ListPage {
            objects: vec![info("run/a")],
            next: None,
        },
    ]);
    let got = listed(&store, "run/", None).await;
    assert_eq!(
        got,
        [
            "run/b".to_owned(),
            "error: store broke the protocol: listing \"run/\" returned run/a after run/b: keys \
             must ascend"
                .to_owned()
        ]
    );
}

/// Asserts that `KeyPrefix::child` appends a name to the prefix and checks the result
/// as a prefix: `run/` and `1/` make `run/1/`, a name with a character that needs
/// escaping is refused, and a child of the empty prefix is the name alone.
#[test]
fn a_prefix_has_children() {
    let run = KeyPrefix::new("run/").unwrap();
    assert_eq!(run.child("1/").unwrap().as_str(), "run/1/");
    assert!(run.child("a b").is_err());
    assert_eq!(KeyPrefix::default().child("x").unwrap().as_str(), "x");
}

/// Asserts what each kind of token says: a continuation token has no local position and
/// no `FsStore` listing; an `after` token resumes after its key with no listing; a kept
/// token resumes after its key and names its listing.
#[test]
fn tokens_say_where_they_resume() {
    let k = key("run/a");
    let continuation = ListToken::continuation(String::from("opaque"));
    let err = continuation.local_after().unwrap_err();
    assert_eq!(
        err.to_string(),
        "store broke the protocol: a continuation token from another store"
    );
    assert_eq!(continuation.listing(), None);

    let after = ListToken::after(&k);
    assert_eq!(after.local_after().unwrap(), &k);
    assert_eq!(after.listing(), None);

    let kept = ListToken::kept(7, k.clone());
    assert_eq!(kept.local_after().unwrap(), &k);
    assert_eq!(kept.listing(), Some(7));
    assert_ne!(kept, after);
}
