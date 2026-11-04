//! The proxy: puts, gets, heads, deletes and listings over the two tiers.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;
use tierstore::{
    Backend, ByteRange, CachedStore, Error, FaultyBackend, JournalEntry, JournalOp, MemoryBackend,
    Meta, Op, PartBody,
};

use crate::support::{
    Rig, assert_absent, body, check_ranges, journal, journal_key, journal_keys, keys, page, read,
};

/// A body of one chunk, for a part.
fn part_body(text: &'static str) -> PartBody<'static> {
    stream::once(async move { Ok(Bytes::from_static(text.as_bytes())) }).boxed()
}

fn entry(op: JournalOp, key: &str, size: u64, time: u64) -> JournalEntry {
    JournalEntry {
        op,
        key: key.to_owned(),
        size,
        time,
        cold_tag: None,
        hot_tag: None,
    }
}

/// Asserts that a put is acknowledged after the journal object that holds its entry is in the
/// hot tier: when the put returns, the object is in the hot tier and not in the cold tier,
/// and journal object 1 of the proxy holds one put entry with the key, the size and the time.
#[tokio::test(start_paused = true)]
async fn a_put_is_acknowledged_after_its_journal_entry_is_in_the_hot_tier() {
    let rig = Rig::new();
    rig.clock.set(42);
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("value")).await.unwrap();

    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
    assert_absent(&*rig.cold_mem, "k").await;
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    assert_eq!(entries, [entry(JournalOp::Put, "k", 5, 42)]);
}

/// Asserts that a put is refused when the key exists only in the cold tier (the hot tier may
/// have evicted it): the error is `AlreadyExists`, the hot tier gets no object, and nothing is
/// journaled.
#[tokio::test(start_paused = true)]
async fn a_put_is_refused_when_the_key_is_only_in_the_cold_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("old")).await.unwrap();

    let refused = proxy.put("k", body("new")).await;
    assert_eq!(refused, Err(Error::AlreadyExists("k".to_owned())));
    assert_absent(&*rig.hot_mem, "k").await;
    assert_eq!(read(&*rig.cold_mem, "k").await, body("old"));
    let journal_objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(journal_objects, Vec::<String>::new());
}

/// Asserts that a put is refused when the key exists in the hot tier, that the first object
/// is kept, and that the refused put adds nothing to the journal.
#[tokio::test(start_paused = true)]
async fn a_put_is_refused_when_the_key_is_in_the_hot_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("one")).await.unwrap();

    let refused = proxy.put("k", body("two")).await;
    assert_eq!(refused, Err(Error::AlreadyExists("k".to_owned())));
    assert_eq!(proxy.get("k", None).await.unwrap(), body("one"));
    let journal_objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(journal_objects, [journal_key("p1", 1)]);
}

/// Asserts that a get of an object in the hot tier is answered by the hot tier alone.
#[tokio::test(start_paused = true)]
async fn a_get_of_a_hot_object_does_not_touch_the_cold_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("value")).await.unwrap();

    // The put asked the cold tier twice (before the create, and again after it).
    let (gets, heads) = (rig.cold.calls(Op::Get), rig.cold.calls(Op::Head));
    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    assert_eq!(proxy.head("k").await.unwrap().size, 5);
    assert_eq!(rig.cold.calls(Op::Get), gets);
    assert_eq!(rig.cold.calls(Op::Head), heads);
    assert_eq!((gets, heads), (0, 2));
}

/// Asserts that a get of an object only the cold tier holds returns it and copies it back into
/// the hot tier, journaling the copy as a mirror of the cold object (with its content tag), so
/// that a second get is answered by the hot
/// tier; and that a head of such an object copies nothing back.
#[tokio::test(start_paused = true)]
async fn a_get_copies_a_cold_object_back_and_a_head_does_not() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.cold_mem.put("k", body("value")).await.unwrap();

    assert_eq!(proxy.head("k").await.unwrap().size, 5);
    assert_absent(&*rig.hot_mem, "k").await;

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body("value"));
    let entries = journal(&*rig.hot_mem, "p1", 1).await;
    let mut mirror = entry(JournalOp::PutMirror, "k", 5, 0);
    mirror.cold_tag = Some(rig.cold_mem.head("k").await.unwrap().tag);
    mirror.hot_tag = Some(rig.hot_mem.head("k").await.unwrap().tag);
    assert_eq!(entries, [mirror]);

    assert_eq!(proxy.get("k", None).await.unwrap(), body("value"));
    assert_eq!(rig.cold.calls(Op::Get), 1);
}

/// Asserts that range reads are byte-exact from the hot tier, and again from an object only
/// the cold tier holds (the first read of it copies the whole object back): the first and last
/// byte, a middle slice, a range past the end, empty ranges, and an error for a range
/// starting past the end.
#[tokio::test(start_paused = true)]
async fn range_reads_are_byte_exact_at_both_tiers() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("hot", body("0123456789")).await.unwrap();
    check_ranges(&proxy, "hot").await;

    rig.cold_mem.put("cold", body("0123456789")).await.unwrap();
    check_ranges(&proxy, "cold").await;
    assert_eq!(read(&*rig.hot_mem, "cold").await, body("0123456789"));
}

/// Asserts that a get and a head of an object neither tier holds are `NotFound`.
#[tokio::test(start_paused = true)]
async fn an_object_neither_tier_holds_is_not_found() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(proxy.get("k", None).await.unwrap_err(), missing);
    assert_eq!(proxy.head("k").await.unwrap_err(), missing);
}

/// Asserts that a delete removes the object from both tiers and appends a delete entry with
/// size zero, that the key is then not found, and that deleting a key neither tier holds
/// succeeds and is journaled too.
#[tokio::test(start_paused = true)]
async fn a_delete_removes_both_copies_and_is_journaled() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("value")).await.unwrap();
    rig.cold_mem.put("k", body("value")).await.unwrap();

    proxy.delete("k").await.unwrap();
    assert_absent(&*rig.hot_mem, "k").await;
    assert_absent(&*rig.cold_mem, "k").await;
    let missing = Error::NotFound("k".to_owned());
    assert_eq!(proxy.get("k", None).await.unwrap_err(), missing);
    let entries = journal(&*rig.hot_mem, "p1", 2).await;
    assert_eq!(entries, [entry(JournalOp::Delete, "k", 0, 0)]);

    proxy.delete("k").await.unwrap();
    let again = journal(&*rig.hot_mem, "p1", 3).await;
    assert_eq!(again, [entry(JournalOp::Delete, "k", 0, 0)]);
}

/// Asserts that a client put or delete of a key under `_journal/`, `_tiering/` or `_entries/` is
/// refused with `Reserved` and changes nothing, and that the proxy does not replace objects.
#[tokio::test(start_paused = true)]
async fn a_client_write_under_a_reserved_prefix_is_refused() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for key in [
        "_journal/p1/x",
        "_tiering/lease",
        "_tiering/usage",
        "_entries/0123",
    ] {
        let put = proxy.put(key, body("v")).await;
        assert_eq!(put, Err(Error::Reserved(key.to_owned())));
        let delete = proxy.delete(key).await;
        assert_eq!(delete, Err(Error::Reserved(key.to_owned())));
        assert_absent(&*rig.hot_mem, key).await;
    }
    let journal_objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(journal_objects, Vec::<String>::new());

    let replaced = proxy.put_if_match("k", body("v"), "tag").await;
    assert!(matches!(replaced, Err(Error::Other(_))));
}

/// Asserts that a get (whole and of a range), a head and the four multipart calls of a key under
/// `_journal/` or `_tiering/` are refused with `Reserved`, whichever tier holds the
/// key, and that the object the tier holds is left as it is.
#[tokio::test(start_paused = true)]
async fn a_client_read_or_upload_of_a_reserved_key_is_refused_whichever_tier_holds_it() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    rig.hot_mem.put("_journal/h", body("hot")).await.unwrap();
    rig.cold_mem.put("_tiering/c", body("cold")).await.unwrap();
    rig.cold_mem.put("_journal/c", body("cold")).await.unwrap();
    rig.cold_mem.put("_entries/c", body("cold")).await.unwrap();
    rig.hot_mem.put("_entries/h", body("hot")).await.unwrap();
    let parts = [(1, "t".to_owned())];
    for key in [
        "_journal/h",
        "_tiering/c",
        "_tiering/proxies/p1",
        "_journal/c",
        "_entries/c",
        "_entries/h",
    ] {
        let refused = Some(Error::Reserved(key.to_owned()));
        assert_eq!(proxy.get(key, None).await.err(), refused, "get {key}");
        let range = Some(ByteRange::new(0, 1));
        assert_eq!(proxy.get(key, range).await.err(), refused, "range {key}");
        assert_eq!(proxy.head(key).await.err(), refused, "head {key}");
        let created = proxy.create_upload(key).await;
        assert_eq!(created.err(), refused, "create {key}");
        let part = proxy.upload_part(key, "u", 1, part_body("x")).await;
        assert_eq!(part.err(), refused, "part {key}");
        let done = proxy.complete_upload(key, "u", &parts).await;
        assert_eq!(done.err(), refused, "complete {key}");
        let aborted = proxy.abort_upload(key, "u").await;
        assert_eq!(aborted.err(), refused, "abort {key}");
    }
    assert_eq!(read(&*rig.hot_mem, "_journal/h").await, body("hot"));
    assert_eq!(read(&*rig.cold_mem, "_journal/c").await, body("cold"));
}

/// Asserts that a reserved key that only the cold tier holds is left out of a listing, of a page
/// of a listing (also when a whole cold page is reserved keys, so the page is made up from the
/// next one) and of a listing under a reserved prefix, as a reserved key of the hot tier is.
#[tokio::test(start_paused = true)]
async fn a_listing_leaves_out_the_reserved_keys_that_only_the_cold_tier_holds() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("a", body("v")).await.unwrap();
    for key in ["_journal/c", "_tiering/c", "_entries/c", "b", "c"] {
        rig.cold_mem.put(key, body("v")).await.unwrap();
    }
    assert_eq!(keys(&proxy).await, ["a", "b", "c"]);
    assert_eq!(page(&proxy, None, 2).await, ["a", "b"]);
    assert_eq!(page(&proxy, Some("a"), 2).await, ["b", "c"]);
    for prefix in ["_journal/", "_tiering/", "_entries/", "_"] {
        let listed = proxy.list(prefix).await.unwrap();
        assert!(listed.is_empty(), "{prefix}");
        let paged = proxy.list_page(prefix, None, 10).await.unwrap();
        assert!(paged.is_empty(), "{prefix}");
    }
}

/// Asserts that a listing is the union of both tiers without duplicates, in key order, with
/// the prefix applied to both, the hot tier's entry where both have the key, and without the
/// keys under the reserved prefixes.
#[tokio::test(start_paused = true)]
async fn a_listing_merges_the_tiers_and_leaves_out_the_reserved_keys() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("x/1", body("one")).await.unwrap();
    proxy.put("x/3", body("three")).await.unwrap();
    rig.cold_mem.put("x/2", body("two")).await.unwrap();
    rig.cold_mem.put("x/3", body("3")).await.unwrap();
    rig.cold_mem.put("y/1", body("other")).await.unwrap();

    assert_eq!(keys(&proxy).await, ["x/1", "x/2", "x/3", "y/1"]);
    let listed = proxy.list("x/").await.unwrap();
    let sizes: Vec<u64> = listed.iter().map(|object| object.size).collect();
    assert_eq!(sizes, [3, 3, 5]);
    let names: Vec<&str> = listed.iter().map(|object| object.key.as_str()).collect();
    assert_eq!(names, ["x/1", "x/2", "x/3"]);
    assert_eq!(keys(&*rig.hot_mem).await.len(), 5);
}

/// Asserts that pages of the merged listing continue after the last key of the page before,
/// are cut at `max_keys` after the reserved keys of the hot tier (which sort before the
/// lowercase keys) are left out, and that a page shorter than `max_keys` is the last.
#[tokio::test(start_paused = true)]
async fn pages_of_the_merged_listing_are_continued_by_the_last_key() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    for key in ["a", "b", "c"] {
        proxy.put(key, body("v")).await.unwrap();
    }
    rig.cold_mem.put("d", body("v")).await.unwrap();
    rig.cold_mem.put("e", body("v")).await.unwrap();

    assert_eq!(page(&proxy, None, 2).await, ["a", "b"]);
    assert_eq!(page(&proxy, Some("b"), 2).await, ["c", "d"]);
    assert_eq!(page(&proxy, Some("d"), 2).await, ["e"]);
    assert_eq!(page(&proxy, Some("e"), 2).await, Vec::<String>::new());
    assert_eq!(page(&proxy, None, 0).await, Vec::<String>::new());
}

/// Asserts that a proxy is not started with an id that is empty or has a slash, and that it
/// reads nothing from the hot tier before it refuses.
#[tokio::test(start_paused = true)]
async fn a_proxy_id_must_be_a_name_without_a_slash() {
    let rig = Rig::new();
    for id in ["", "a/b"] {
        let opened = rig.open(rig.config(id, 10)).await;
        assert!(matches!(opened.err(), Some(Error::Other(_))));
    }
    assert_eq!(rig.hot.calls(Op::List), 0);
}

/// Asserts that the proxy does not take a conditional delete: it is `Unsupported`, nothing is
/// journaled, and the object stays in the hot tier (the proxy's own delete is the one that
/// removes from both tiers and journals).
#[tokio::test(start_paused = true)]
async fn a_conditional_delete_through_the_proxy_is_unsupported_and_changes_nothing() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    proxy.put("k", body("v")).await.unwrap();
    let tag = proxy.head("k").await.unwrap().tag;
    let refused = proxy.delete_if_match("k", &tag).await.unwrap_err();
    assert!(matches!(refused, Error::Unsupported(_)), "{refused}");
    assert_eq!(read(&*rig.hot_mem, "k").await, body("v"));
    let objects = journal_keys(&*rig.hot_mem, "p1").await;
    assert_eq!(objects, [journal_key("p1", 1)]);
}

fn meta(size: u64, tag: &str) -> Meta {
    Meta {
        size,
        tag: tag.to_owned(),
        modified: 0,
        reservation: false,
    }
}

/// Asserts what the proxy says of two HEADs of a key: they are one version when the sizes are
/// equal, whatever the tags (a tier's tag moves when the proxy copies the object to the other
/// tier), and two when the sizes differ; and that a plain backend says the same size and tag.
#[tokio::test(start_paused = true)]
async fn the_proxy_tells_versions_apart_by_size_and_a_plain_backend_by_size_and_tag() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    assert!(proxy.same_version(&meta(3, "hot"), &meta(3, "cold")));
    assert!(!proxy.same_version(&meta(3, "hot"), &meta(4, "hot")));

    let memory = MemoryBackend::new();
    assert!(memory.same_version(&meta(3, "a"), &meta(3, "a")));
    assert!(!memory.same_version(&meta(3, "a"), &meta(3, "b")));
    assert!(!memory.same_version(&meta(3, "a"), &meta(4, "a")));
}

/// Asserts that the wrappers of a backend (the fault injector and the in-memory cache) give the
/// answer of the backend they wrap, the proxy's by size here, and not the default.
#[tokio::test(start_paused = true)]
async fn the_wrappers_of_a_backend_forward_its_reading_of_a_version() {
    let rig = Rig::new();
    let proxy: Arc<dyn Backend> = Arc::new(rig.proxy("p1", 1000).await);
    let faulty = FaultyBackend::new(Arc::clone(&proxy));
    let cached = CachedStore::new(proxy, 100);
    for wrapper in [&faulty as &dyn Backend, &cached] {
        assert!(wrapper.same_version(&meta(3, "hot"), &meta(3, "cold")));
        assert!(!wrapper.same_version(&meta(3, "hot"), &meta(4, "hot")));
    }
}
