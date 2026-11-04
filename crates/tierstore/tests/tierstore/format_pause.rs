//! The pause of writes: the proxy answers writes with 503 while `_tiering/format` says `drain` or
//! `drained`, serves reads, and reads the object again after `format_refresh`.

use std::time::Duration;

use tierstore::{Backend, Error, Op, Proxy};

use crate::support::{Rig, assert_absent, body, chunks, keys, read};

const FORMAT: &str = "_tiering/format";

/// The error of a write that is paused.
fn paused() -> Error {
    let why = "writes are paused while the store changes format; retry later";
    Error::Unavailable(why.to_owned())
}

/// Replaces the format object in the hot tier directly.
async fn set_format(rig: &Rig, json: &str) {
    let _ = rig.hot_mem.delete(FORMAT).await;
    rig.hot_mem.put(FORMAT, body(json)).await.unwrap();
}

/// A proxy that reads the format object before every write.
async fn eager_proxy(rig: &Rig) -> Proxy {
    let mut config = rig.config("p1", 1000);
    config.format_refresh = Duration::ZERO;
    rig.open(config).await.unwrap()
}

/// Asserts that, while the format says `drain`, a put, a streamed put (both kinds), a delete and
/// the three multipart calls that write are answered `Unavailable` with the pause text and change
/// no object, and that a get, a head and a listing still answer.
#[tokio::test(start_paused = true)]
async fn every_write_is_answered_503_while_the_format_says_drain_and_reads_still_work() {
    let rig = Rig::new();
    let proxy = eager_proxy(&rig).await;
    proxy.put("k", body("v")).await.unwrap();
    set_format(&rig, r#"{"phase":"drain"}"#).await;

    assert_eq!(proxy.put("n", body("v")).await, Err(paused()));
    let streamed = proxy.put_stream("n", Some(1), chunks(&["v"])).await;
    assert_eq!(streamed, Err(paused()));
    let hot_only = proxy
        .put_stream_hot_only("n", Some(1), chunks(&["v"]))
        .await;
    assert_eq!(hot_only, Err(paused()));
    assert_eq!(proxy.delete("k").await, Err(paused()));
    assert_eq!(proxy.create_upload("n").await, Err(paused()));
    let part = proxy.upload_part("n", "u", 1, chunks(&["v"])).await;
    assert_eq!(part, Err(paused()));
    let parts = [(1, "t".to_owned())];
    assert_eq!(proxy.complete_upload("n", "u", &parts).await, Err(paused()));

    assert_eq!(proxy.get("k", None).await.unwrap(), body("v"));
    assert_eq!(proxy.head("k").await.unwrap().size, 1);
    assert_eq!(keys(&proxy).await, ["k"]);
    assert_eq!(read(&*rig.hot_mem, "k").await, body("v"));
    assert_absent(&*rig.hot_mem, "n").await;
}

/// Asserts which phases pause a put: `drain` and `drained` do, an `active` format with its epoch
/// and a phase this release does not know do not.
#[tokio::test(start_paused = true)]
async fn drained_pauses_writes_and_active_and_unknown_phases_do_not() {
    let cases = [
        (r#"{"phase":"drain"}"#, true),
        (r#"{"phase":"drained"}"#, true),
        (r#"{"epoch":2,"phase":"active"}"#, false),
        (r#"{"phase":"someday","extra":[1]}"#, false),
    ];
    for (json, is_paused) in cases {
        let rig = Rig::new();
        let proxy = eager_proxy(&rig).await;
        set_format(&rig, json).await;
        let put = proxy.put("k", body("v")).await;
        assert_eq!(
            put,
            if is_paused { Err(paused()) } else { Ok(()) },
            "{json}"
        );
    }
}

/// Asserts that deleting the format object (what an operator's cancel does) lets puts and deletes
/// through again.
#[tokio::test(start_paused = true)]
async fn writes_resume_when_the_format_object_is_deleted() {
    let rig = Rig::new();
    let proxy = eager_proxy(&rig).await;
    set_format(&rig, r#"{"phase":"drain"}"#).await;
    assert_eq!(proxy.put("k", body("v")).await, Err(paused()));
    rig.hot_mem.delete(FORMAT).await.unwrap();
    proxy.put("k", body("v")).await.unwrap();
    proxy.delete("k").await.unwrap();
}

/// Asserts that the format object is read again only after `format_refresh` of the proxy's
/// monotonic clock: a put right after the object turned to `drain` still goes, and one after the
/// clock moved by the refresh is answered 503.
#[tokio::test(start_paused = true)]
async fn the_format_is_read_again_after_the_refresh_and_not_before() {
    let rig = Rig::new();
    let mut config = rig.config("p1", 1000);
    config.format_refresh = Duration::from_secs(5);
    let proxy = rig.open(config).await.unwrap();
    proxy.put("a", body("v")).await.unwrap();

    set_format(&rig, r#"{"phase":"drain"}"#).await;
    tokio::time::advance(Duration::from_secs(4)).await;
    proxy.put("b", body("v")).await.unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(proxy.put("c", body("v")).await, Err(paused()));
}

/// Asserts that a format object that cannot be read (the hot tier fails the read, or the body is
/// not a format) leaves the proxy as it was: paused if the last object read paused, open if it
/// never read one.
#[tokio::test(start_paused = true)]
async fn an_unreadable_format_keeps_what_the_proxy_read_last() {
    let rig = Rig::new();
    let proxy = eager_proxy(&rig).await;
    let down = Error::Unavailable("hot".to_owned());
    rig.hot.fail_next_on(Op::Get, FORMAT, down.clone());
    proxy.put("a", body("v")).await.unwrap();

    set_format(&rig, r#"{"phase":"drain"}"#).await;
    assert_eq!(proxy.put("b", body("v")).await, Err(paused()));
    rig.hot.fail_next_on(Op::Get, FORMAT, down);
    assert_eq!(proxy.put("b", body("v")).await, Err(paused()));
    set_format(&rig, "not json").await;
    assert_eq!(proxy.put("b", body("v")).await, Err(paused()));
    assert_absent(&*rig.hot_mem, "b").await;
}
