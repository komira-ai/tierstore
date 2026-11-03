//! The bounded backend: calls that answer pass through, calls that do not are cut with the limit
//! named, and a call that destroys is not sent when too little of the deadline is left.
//!
//! A call that does not answer is parked at a gate that is never released; the runtime's clock is
//! paused, so the timeout fires when every task waits, and no test waits.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::faulty::{FaultyBackend, Op};
use crate::{Backend, Bounded, Deadline, Error, GateBackend, MemoryBackend, Mono, Phase};

const TIMEOUT: Duration = Duration::from_secs(5);
const MARGIN: Duration = Duration::from_secs(10);

struct Rig {
    memory: Arc<MemoryBackend>,
    store: Arc<FaultyBackend>,
    gated: Arc<GateBackend>,
    bounded: Bounded,
}

fn rig() -> Rig {
    let memory = Arc::new(MemoryBackend::new());
    let store = Arc::new(FaultyBackend::new(memory.clone()));
    let gated = Arc::new(GateBackend::new(store.clone()));
    let bounded = Bounded::new(gated.clone(), TIMEOUT);
    Rig {
        memory,
        store,
        gated,
        bounded,
    }
}

fn body(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

/// A deadline with `left` of it left at the clock's zero.
fn deadline_with(left: Duration) -> Deadline {
    let deadline = Deadline::new(Arc::new(Mono::new()), MARGIN);
    deadline.extend_to(u64::try_from(left.as_millis()).unwrap());
    deadline
}

/// Asserts that calls that answer give what the wrapped backend gives, errors included: the
/// create-only put and its tag, a get, a head, a listing, a replacement that wins and one whose
/// tag is stale.
#[tokio::test(start_paused = true)]
async fn calls_that_answer_pass_through_unchanged() {
    let rig = rig();
    let first = rig.bounded.put_tagged("k", body("one")).await.unwrap();
    assert_eq!(first, rig.memory.head("k").await.unwrap().tag);
    let again = rig.bounded.put_tagged("k", body("two")).await;
    assert!(matches!(again, Err(Error::AlreadyExists(_))), "{again:?}");
    assert_eq!(rig.bounded.get("k", None).await.unwrap(), body("one"));
    assert_eq!(rig.bounded.head("k").await.unwrap().size, 3);
    let listed = rig.bounded.list("").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, "k");

    let second = rig
        .bounded
        .put_if_match("k", body("three"), &first)
        .await
        .unwrap();
    assert_ne!(second, first);
    let stale = rig.bounded.put_if_match("k", body("four"), &first).await;
    assert!(
        matches!(stale, Err(Error::PreconditionFailed(_))),
        "{stale:?}"
    );
}

/// Asserts that each call that reads or writes, parked at the store and never answered, is cut at
/// the timeout with `Unavailable` and a text that names the call, the key and the limit, and that
/// the store was not reached (the gate holds the call before it).
#[tokio::test(start_paused = true)]
async fn a_call_that_does_not_answer_is_cut_at_the_timeout() {
    let rig = rig();
    let _get = rig.gated.gate(Op::Get, "k", Phase::Pre);
    let _head = rig.gated.gate(Op::Head, "k", Phase::Pre);
    let _list = rig.gated.gate(Op::List, "p/", Phase::Pre);
    let _put = rig.gated.gate(Op::Put, "k", Phase::Pre);
    let _replace = rig.gated.gate(Op::PutIfMatch, "k", Phase::Pre);
    let cut = |what: &str, key: &str| {
        Err::<(), _>(Error::Unavailable(format!(
            "{what} of {key} did not answer within 5s"
        )))
    };
    assert_eq!(rig.bounded.get("k", None).await.map(drop), cut("get", "k"));
    assert_eq!(rig.bounded.head("k").await.map(drop), cut("head", "k"));
    assert_eq!(rig.bounded.list("p/").await.map(drop), cut("list", "p/"));
    let put = rig.bounded.put_tagged("k", body("v")).await;
    assert_eq!(put.map(drop), cut("put", "k"));
    let replace = rig.bounded.put_if_match("k", body("v"), "t").await;
    assert_eq!(replace.map(drop), cut("put", "k"));
    assert_eq!(rig.store.call_log(), Vec::<(Op, String)>::new());
}

/// Asserts that a delete with exactly the margin left is sent and done, and that a delete with a
/// millisecond less is refused with a text that gives what is left and what is needed, never sent
/// (the store counts no call and the object is still there).
#[tokio::test(start_paused = true)]
async fn a_delete_starts_only_with_the_margin_left() {
    let rig = rig();
    rig.memory.put("k", body("v")).await.unwrap();

    let short = deadline_with(MARGIN - Duration::from_millis(1));
    let refused = rig.bounded.delete("k", &short).await;
    let text = "delete of k not sent: 9.999s of the deadline is left, and 10s is needed";
    assert_eq!(refused, Err(Error::Unavailable(text.into())));
    assert_eq!(rig.store.calls(Op::Delete), 0);
    assert!(rig.memory.head("k").await.is_ok());

    let enough = deadline_with(MARGIN);
    rig.bounded.delete("k", &enough).await.unwrap();
    assert_eq!(rig.store.calls(Op::Delete), 1);
    assert!(rig.memory.head("k").await.is_err());
}

/// Asserts the same for the delete that names a content tag: refused unsent when too little is
/// left, otherwise sent, done when the tag is the object's and failing with the store's own error
/// when it is not.
#[tokio::test(start_paused = true)]
async fn a_delete_if_match_starts_only_with_the_margin_left_and_passes_the_tag_error() {
    let rig = rig();
    rig.memory.put("k", body("v")).await.unwrap();
    let tag = rig.memory.head("k").await.unwrap().tag;

    let short = deadline_with(Duration::from_secs(1));
    let refused = rig.bounded.delete_if_match("k", &tag, &short).await;
    let text = "delete of k not sent: 1s of the deadline is left, and 10s is needed";
    assert_eq!(refused, Err(Error::Unavailable(text.into())));
    assert_eq!(rig.store.calls(Op::DeleteIfMatch), 0);

    let enough = deadline_with(Duration::from_secs(30));
    let stale = rig.bounded.delete_if_match("k", "stale", &enough).await;
    assert!(
        matches!(stale, Err(Error::PreconditionFailed(_))),
        "{stale:?}"
    );
    rig.bounded
        .delete_if_match("k", &tag, &enough)
        .await
        .unwrap();
    assert!(rig.memory.head("k").await.is_err());
}

/// Asserts that a delete that does not answer is cut at the margin, not at the longer time that is
/// left, and not retried: parked before the store it is never made (no call counted); parked after
/// the store did it the object is gone while the caller was told `Unavailable`, and the store
/// counts the one call.
#[tokio::test(start_paused = true)]
async fn a_delete_that_does_not_answer_is_cut_at_the_margin_and_never_retried() {
    let rig = rig();
    rig.memory.put("k", body("v")).await.unwrap();
    let long = deadline_with(Duration::from_secs(300));
    let text = "delete of k did not answer within 10s";

    let _before = rig.gated.gate(Op::Delete, "k", Phase::Pre);
    let cut = rig.bounded.delete("k", &long).await;
    assert_eq!(cut, Err(Error::Unavailable(text.into())));
    assert_eq!(rig.store.calls(Op::Delete), 0);
    assert!(rig.memory.head("k").await.is_ok());

    let _after = rig.gated.gate(Op::Delete, "k", Phase::Post);
    let cut = rig.bounded.delete("k", &long).await;
    assert_eq!(cut, Err(Error::Unavailable(text.into())));
    assert_eq!(rig.store.calls(Op::Delete), 1);
    assert!(rig.memory.head("k").await.is_err());
}
