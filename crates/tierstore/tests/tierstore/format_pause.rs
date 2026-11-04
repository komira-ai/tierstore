//! The pause of writes: the proxy answers writes with 503 while `_tiering/format` says `drain` or
//! `drained`, serves reads, and reads the object again after `format_refresh`.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{Backend, Error, Op, Proxy};
use tokio::sync::Notify;

use crate::hold::HoldGet;
use crate::support::{Rig, assert_absent, body, chunks, keys, read};

const FORMAT: &str = "_tiering/format";

/// The error of a write that is paused.
fn paused() -> Error {
    let why = "writes are paused while the store changes format; retry later";
    Error::Unavailable(why.to_owned())
}

/// The error of a write while the format does not say `active` at epoch 2.
fn waiting() -> Error {
    let why = "writes wait until the format object says active at epoch 2 \
               (tierstore format-epoch --activate); reads are served";
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

/// Asserts which phases pause a put: `drain` and `drained` do (the pause's text), a phase this
/// release does not know is answered with the text that says writes wait for the active format,
/// and an `active` format with its epoch lets a live proxy put on an entry.
#[tokio::test(start_paused = true)]
async fn drained_pauses_writes_and_active_and_unknown_phases_do_not() {
    let cases = [
        (r#"{"phase":"drain"}"#, Err(paused())),
        (r#"{"phase":"drained"}"#, Err(paused())),
        (r#"{"phase":"someday","extra":[1]}"#, Err(waiting())),
    ];
    for (json, want) in cases {
        let rig = Rig::new();
        let proxy = eager_proxy(&rig).await;
        set_format(&rig, json).await;
        let put = proxy.put("k", body("v")).await;
        assert_eq!(put, want, "{json}");
    }
    let rig = Rig::new();
    let proxy = eager_proxy(&rig).await;
    set_format(&rig, r#"{"epoch":2,"phase":"active"}"#).await;
    assert_eq!(proxy.put("k", body("v")).await, Ok(()));
}

/// Asserts that writing the active format over a drain lets puts and deletes through again, and
/// that deleting the format object (what an operator's cancel does) does not: a proxy of this
/// release takes no write without the active format.
#[tokio::test(start_paused = true)]
async fn writes_resume_when_the_format_is_active_again_and_not_when_it_is_deleted() {
    let rig = Rig::new();
    let proxy = eager_proxy(&rig).await;
    set_format(&rig, r#"{"phase":"drain"}"#).await;
    assert_eq!(proxy.put("k", body("v")).await, Err(paused()));
    rig.hot_mem.delete(FORMAT).await.unwrap();
    assert_eq!(proxy.put("k", body("v")).await, Err(waiting()));
    set_format(&rig, r#"{"epoch":2,"phase":"active"}"#).await;
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

/// Asserts that two callers never read the format object at once, so that a read that was
/// answered before the object turned to `drain` cannot be stored after a read that saw `drain`.
/// A write is held with its read of an `active` format of epoch 1 on the way back; the object
/// turns to `drain`, the refresh passes, and a second write arrives. It must wait for the first
/// (the hot tier has seen one read of the object, not two) and, once the first is let go and is
/// answered on what it read (the wait for epoch 2), read the object itself and be answered with
/// the pause. Defects it catches: a refresh
/// that does not serialize the readers (a second read while the first is out, and the older
/// result stored over the newer).
#[tokio::test(start_paused = true)]
async fn a_second_writer_waits_for_the_read_of_the_format_that_is_out() {
    let rig = Rig::new();
    set_format(&rig, r#"{"phase":"active","epoch":1}"#).await;
    let (entered, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let held = Arc::new(HoldGet::new(
        rig.hot.clone(),
        FORMAT,
        entered.clone(),
        release.clone(),
    ));
    let mut config = rig.config("p1", 1000);
    config.format_refresh = Duration::from_secs(5);
    let proxy = Arc::new(
        Proxy::open(held.clone(), rig.cold.clone(), config)
            .await
            .unwrap(),
    );

    let first = tokio::spawn({
        let proxy = proxy.clone();
        async move { proxy.put("a", body("v")).await }
    });
    entered.notified().await;
    set_format(&rig, r#"{"phase":"drain"}"#).await;
    tokio::time::advance(Duration::from_secs(6)).await;
    let second = tokio::spawn({
        let proxy = proxy.clone();
        async move { proxy.put("b", body("v")).await }
    });
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        held.gets(),
        1,
        "the second writer read the format while the first read was out"
    );

    release.notify_one();
    assert_eq!(first.await.unwrap(), Err(waiting()));
    assert_eq!(second.await.unwrap(), Err(paused()));
    assert_eq!(held.gets(), 2);
    assert_absent(&*rig.hot_mem, "a").await;
    assert_absent(&*rig.hot_mem, "b").await;
}
