//! Reads and listings.

use super::*;

/// The key of a test.
const K: &str = "builds/k";

/// Asserts that a GET and a HEAD of a key the hot tier holds are one hot request each and ask for
/// nothing else, whatever the marker says.
#[tokio::test]
async fn a_read_of_a_key_hot_holds_is_one_hot_request() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    let mut seen = world.cursor();
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("v1"));
    assert_eq!(world.since(&mut seen), [req(Store::Hot, Op::Get, K)]);
    assert_eq!(proxy.head(K).await.unwrap().size, 2);
    assert_eq!(world.since(&mut seen), [req(Store::Hot, Op::Head, K)]);
}

/// Asserts what a GET of a key that exists nowhere asks: the hot GET, the marker GET and the cold
/// GET, which is the design's "hot 404, then the marker with the cold read", two hot-store
/// requests where the entry protocol made one; and the same for a HEAD.
#[tokio::test]
async fn a_read_of_an_absent_key_asks_hot_the_marker_and_cold() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    let mut seen = world.cursor();
    assert_eq!(
        served(&proxy, "no/key").await,
        Err(Error::NotFound("no/key".into()))
    );
    assert_eq!(
        world.since(&mut seen),
        [
            req(Store::Hot, Op::Get, "no/key"),
            req(Store::Marks, Op::Get, "no/key"),
            req(Store::Cold, Op::Get, "no/key"),
        ]
    );
    assert_eq!(
        proxy.head("no/key").await,
        Err(Error::NotFound("no/key".into()))
    );
    assert_eq!(
        world.since(&mut seen),
        [
            req(Store::Hot, Op::Head, "no/key"),
            req(Store::Marks, Op::Get, "no/key"),
            req(Store::Cold, Op::Head, "no/key"),
        ]
    );
}

/// Asserts that a key the leader copied and the hot tier then lost (an eviction made by hand)
/// is served from the cold tier, and that the read leaves a `WANT` marker, the ask for the copy
/// back, written once: a second read within `want_retry` asks nothing.
#[tokio::test]
async fn a_read_of_a_cold_only_key_is_served_by_cold_and_asks_once_for_a_copy_back() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    world.cold_mem.put(K, body("cold")).await.unwrap();
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("cold"));
    marked(&world, K).await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Want, false)));
    let asks = world.marks.calls(Op::Put);
    assert_eq!(served(&proxy, K).await.as_deref(), Ok("cold"));
    tokio::task::yield_now().await;
    assert_eq!(world.marks.calls(Op::Put), asks);
}

/// Asserts that a key whose marker says `gone`, while the cold tier still holds the old object,
/// is absent to a GET and a HEAD, and that the answer is reached by one more hot request of the
/// same kind after the marker was read (a put may have landed since the first miss): two hot
/// reads, one marker read and the cold read whose answer is dropped.
#[tokio::test]
async fn a_read_of_a_deleted_key_with_a_cold_copy_is_absent_after_one_more_hot_read() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    proxy.put(K, body("v1")).await.unwrap();
    assert_eq!(world.leader("r1").step().await.changed, 1);
    proxy.delete(K).await.unwrap();
    released(&world, K).await;
    let mut seen = world.cursor();
    assert_eq!(served(&proxy, K).await, Err(Error::NotFound(K.into())));
    assert_eq!(
        world.since(&mut seen),
        [
            req(Store::Hot, Op::Get, K),
            req(Store::Hot, Op::Get, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Cold, Op::Get, K),
        ]
    );
    assert_eq!(proxy.head(K).await, Err(Error::NotFound(K.into())));
    assert_eq!(
        world.since(&mut seen),
        [
            req(Store::Hot, Op::Head, K),
            req(Store::Hot, Op::Head, K),
            req(Store::Marks, Op::Get, K),
            req(Store::Cold, Op::Head, K),
        ]
    );
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v1"));
}

/// Asserts that a listing merges the hot objects and the cold ones, lists a key once, leaves out
/// a deleted key whose cold copy stands, and shows a key put since (whose marker the cold tier
/// does not know) once; and that a page after a key, with a delimiter, agrees.
#[tokio::test]
async fn a_listing_merges_hot_and_cold_once_and_leaves_out_the_deleted() {
    let world = World::new().await;
    let proxy = world.proxy("p1").await;
    for key in ["p/a", "p/b", "p/c"] {
        proxy.put(key, body(key)).await.unwrap();
    }
    assert_eq!(world.leader("r1").step().await.changed, 3);
    proxy.delete("p/b").await.unwrap();
    released(&world, "p/b").await;
    proxy.put("p/d", body("d")).await.unwrap();
    world.cold_mem.put("p/e", body("e")).await.unwrap();
    world.cold_mem.put("q/z", body("z")).await.unwrap();

    let keys = |listed: Vec<tierstore::Listed>| -> Vec<String> {
        listed.into_iter().map(|l| l.key).collect()
    };
    assert_eq!(
        keys(proxy.list("p/").await.unwrap()),
        ["p/a", "p/c", "p/d", "p/e"]
    );
    assert_eq!(
        keys(proxy.list("").await.unwrap()),
        ["p/a", "p/c", "p/d", "p/e", "q/z"]
    );
    let page = proxy.list_page("p/", Some("p/a"), 2).await.unwrap();
    assert_eq!(keys(page), ["p/c", "p/d"]);
}

/// Asserts that a proxy whose marks bucket is not the one the format record names, or whose record
/// does not open the markers (no epoch, or no name), writes no marker at all: every write is
/// `Unavailable`, and a read of a key only the cold tier has is served without asking for a copy
/// back, so that two names for the marks bucket are never in use at once. Its reads still go
/// through the markers.
#[tokio::test]
async fn a_proxy_that_the_format_record_does_not_open_markers_for_writes_no_marker_not_even_a_want()
{
    let formats = [
        (
            r#"{"epoch":3,"marks_bucket":"elsewhere","phase":"active-marks"}"#,
            "hot-marks",
        ),
        (
            r#"{"phase":"active-marks","marks_bucket":"hot-marks"}"#,
            "hot-marks",
        ),
        (r#"{"epoch":3,"phase":"active-marks"}"#, "hot-marks"),
        (FORMAT, "elsewhere"),
    ];
    for (format, name) in formats {
        let world = World::bare();
        world.set_format(format).await;
        let mut config = world.config("p1");
        config.marks_name = Some(name.to_owned());
        let proxy = world.proxy_with(config).await;
        world.cold_mem.put(K, body("cold")).await.unwrap();

        assert_eq!(
            served(&proxy, K).await.as_deref(),
            Ok("cold"),
            "{format} {name}"
        );
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(world.marker(K).await, None, "{format} {name}");
        let put = proxy.put("builds/new", body("x")).await;
        assert!(
            matches!(put, Err(Error::Unavailable(_))),
            "{format} {name}: {put:?}"
        );
        let delete = proxy.delete(K).await;
        assert!(
            matches!(delete, Err(Error::Unavailable(_))),
            "{format} {name}: {delete:?}"
        );
        let claim = tierstore::PutClaim::default();
        let plain = proxy
            .put_plain("builds/new", tierstore::PutBody::Held(body("x")), &claim)
            .await;
        assert!(
            matches!(plain, Err(Error::Unavailable(_))),
            "{format} {name}: {plain:?}"
        );
        assert_eq!(world.marks.calls(Op::Put), 0, "{format} {name}");
    }
}
