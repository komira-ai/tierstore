//! The gate backend: a call it takes waits at the place the gate names until the test releases it,
//! and every other call goes through untouched.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;

use crate::faulty::{FaultyBackend, Op};
use crate::{Backend, Error, Gate, GateBackend, MemoryBackend, PartBody, Phase};

/// A memory bucket, a counter over it, and a gate wrapper over the counter, so that a test sees
/// whether the store has been reached.
struct Rig {
    memory: Arc<MemoryBackend>,
    store: Arc<FaultyBackend>,
    gated: GateBackend,
}

fn rig() -> Rig {
    let memory = Arc::new(MemoryBackend::new());
    let store = Arc::new(FaultyBackend::new(memory.clone()));
    let gated = GateBackend::new(store.clone());
    Rig {
        memory,
        store,
        gated,
    }
}

fn bytes(text: &'static str) -> Bytes {
    Bytes::from_static(text.as_bytes())
}

fn chunks(text: &'static str) -> PartBody<'static> {
    futures::stream::iter(vec![Ok(bytes(text))]).boxed()
}

/// Polls `call` until `gate` has taken it. The call must not finish on its own before that.
async fn until_parked<F: Future + Unpin>(gate: &Gate, call: &mut F) {
    tokio::select! {
        _ = &mut *call => panic!("the call finished instead of parking"),
        () = gate.parked() => {}
    }
}

/// Holds `call` at a gate of kind `op`, releases it and gives its answer.
async fn held<T>(backend: &GateBackend, op: Op, call: impl Future<Output = Result<T, Error>>) -> T {
    let gate = backend.gate(op, "", Phase::Pre);
    let mut call = Box::pin(call);
    until_parked(&gate, &mut call).await;
    assert!(gate.release());
    call.await.unwrap()
}

/// A gate before the call parks it ahead of the store: the wrapped backend has seen no call and
/// holds no object until the release, and the call then does what it would have done.
#[tokio::test]
async fn pre_gate_holds_the_call_until_released_and_the_store_sees_nothing_before() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Put, "k", Phase::Pre);
    let mut put = Box::pin(rig.gated.put("k", bytes("v")));
    until_parked(&gate, &mut put).await;
    assert_eq!(rig.store.calls(Op::Put), 0);
    assert!(matches!(
        rig.memory.head("k").await,
        Err(Error::NotFound(_))
    ));

    assert!(gate.release());
    put.await.unwrap();
    assert_eq!(rig.store.calls(Op::Put), 1);
    assert_eq!(rig.memory.get("k", None).await.unwrap(), bytes("v"));
}

/// A gate after the call lets the store do it and holds the answer: the object is stored and the
/// call has not returned until the release.
#[tokio::test]
async fn post_gate_lets_the_store_act_and_holds_the_answer_until_released() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Put, "k", Phase::Post);
    let mut put = Box::pin(rig.gated.put("k", bytes("v")));
    until_parked(&gate, &mut put).await;
    assert_eq!(rig.store.calls(Op::Put), 1);
    assert_eq!(rig.memory.get("k", None).await.unwrap(), bytes("v"));
    assert!(futures::poll!(&mut put).is_pending());

    assert!(gate.release());
    put.await.unwrap();
}

/// A release with an error before the call is the call failing without the store ever seeing it.
#[tokio::test]
async fn pre_gate_released_with_an_error_fails_the_call_and_the_store_never_sees_it() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Put, "k", Phase::Pre);
    let mut put = Box::pin(rig.gated.put("k", bytes("v")));
    until_parked(&gate, &mut put).await;

    assert!(gate.release_with(Error::Full("on purpose".into())));
    let answer = put.await;
    assert!(matches!(answer, Err(Error::Full(text)) if text == "on purpose"));
    assert_eq!(rig.store.calls(Op::Put), 0);
    assert!(rig.memory.head("k").await.is_err());
}

/// A release with an error after the call is a call that took effect and was reported as failed:
/// the object is stored and the caller is told otherwise.
#[tokio::test]
async fn post_gate_released_with_an_error_leaves_the_effect_and_reports_the_error() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Put, "k", Phase::Post);
    let mut put = Box::pin(rig.gated.put("k", bytes("v")));
    until_parked(&gate, &mut put).await;

    assert!(gate.release_with(Error::Full("on purpose".into())));
    assert!(matches!(put.await, Err(Error::Full(text)) if text == "on purpose"));
    assert_eq!(rig.memory.get("k", None).await.unwrap(), bytes("v"));
}

/// `fail_post` is an unavailable answer to a call whose effect was made.
#[tokio::test]
async fn fail_post_answers_unavailable_after_the_store_did_the_call() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Delete, "k", Phase::Post);
    rig.memory.put("k", bytes("v")).await.unwrap();
    let mut delete = Box::pin(rig.gated.delete("k"));
    until_parked(&gate, &mut delete).await;

    assert!(gate.fail_post());
    assert!(matches!(delete.await, Err(Error::Unavailable(_))));
    assert!(rig.memory.head("k").await.is_err());
}

/// A released call whose own answer was an error still gives that error.
#[tokio::test]
async fn released_call_gives_the_error_the_store_answered_with() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Get, "missing", Phase::Post);
    let mut get = Box::pin(rig.gated.get("missing", None));
    until_parked(&gate, &mut get).await;

    assert!(gate.release());
    assert!(matches!(get.await, Err(Error::NotFound(_))));
}

/// A gate takes one call: the next call that matches it, and every one after, is not held, and the
/// call that was held is not held again after its release.
#[tokio::test]
async fn gate_takes_the_first_matching_call_only() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Put, "k", Phase::Pre);
    let mut first = Box::pin(rig.gated.put("k1", bytes("1")));
    until_parked(&gate, &mut first).await;

    rig.gated.put("k2", bytes("2")).await.unwrap();
    assert_eq!(rig.memory.get("k2", None).await.unwrap(), bytes("2"));
    assert!(rig.memory.head("k1").await.is_err());

    assert!(gate.release());
    first.await.unwrap();
    rig.gated.put("k3", bytes("3")).await.unwrap();
    assert_eq!(rig.store.calls(Op::Put), 3);
}

/// A gate holds a call of its own kind on a key under its prefix and no other call.
#[tokio::test]
async fn gate_ignores_calls_of_another_kind_or_another_prefix() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Get, "a/", Phase::Pre);
    rig.gated.put("a/x", bytes("v")).await.unwrap();
    assert!(matches!(
        rig.gated.get("b/x", None).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        rig.gated.head("a/x").await,
        Ok(meta) if meta.size == 1
    ));

    let mut get = Box::pin(rig.gated.get("a/x", None));
    until_parked(&gate, &mut get).await;
    assert!(gate.release());
    assert_eq!(get.await.unwrap(), bytes("v"));
}

/// A gate of the other phase does not take the call: a gate at the end of the call never holds it
/// at the start, and the reverse.
#[tokio::test]
async fn gate_of_one_phase_does_not_hold_the_other() {
    let rig = rig();
    let post = rig.gated.gate(Op::Put, "k", Phase::Post);
    let mut put = Box::pin(rig.gated.put("k", bytes("v")));
    // The store has been reached by the time the post gate holds the call.
    until_parked(&post, &mut put).await;
    assert_eq!(rig.store.calls(Op::Put), 1);
    assert!(post.release());
    put.await.unwrap();

    let pre = rig.gated.gate(Op::Delete, "k", Phase::Pre);
    let mut delete = Box::pin(rig.gated.delete("k"));
    until_parked(&pre, &mut delete).await;
    assert_eq!(rig.store.calls(Op::Delete), 0);
    assert!(pre.release());
    delete.await.unwrap();
}

/// A release when nothing is parked does nothing and says so: before a call arrived, and a second
/// time after the call was released.
#[tokio::test]
async fn release_is_false_when_no_call_is_parked() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Put, "k", Phase::Pre);
    assert!(!gate.release());
    assert!(!gate.release_with(Error::Unavailable("x".into())));
    assert!(!gate.fail_post());

    let mut put = Box::pin(rig.gated.put("k", bytes("v")));
    until_parked(&gate, &mut put).await;
    assert!(gate.release());
    assert!(!gate.release());
    put.await.unwrap();
}

/// A call that was given up on while parked cannot be released: the release says so.
#[tokio::test]
async fn release_is_false_when_the_caller_gave_up() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Put, "k", Phase::Pre);
    let mut put = Box::pin(rig.gated.put("k", bytes("v")));
    until_parked(&gate, &mut put).await;
    drop(put);

    assert!(!gate.release());
    assert_eq!(rig.store.calls(Op::Put), 0);
}

/// `parked` returns at once for a gate that took its call earlier, also after the release.
#[tokio::test]
async fn parked_returns_again_after_the_release() {
    let rig = rig();
    let gate = rig.gated.gate(Op::Put, "k", Phase::Pre);
    let mut put = Box::pin(rig.gated.put("k", bytes("v")));
    until_parked(&gate, &mut put).await;
    gate.parked().await;
    assert!(gate.release());
    put.await.unwrap();
    gate.parked().await;
}

/// Two wrappers over one store are two actors: a gate on one holds that actor's call while the
/// other actor's call goes through, so the held call then meets the state the other left. This
/// is the interleaving a race test needs: a create-only put that was held loses to the one that
/// ran in between.
#[tokio::test]
async fn gate_on_one_actor_lets_the_other_actor_run_in_between() {
    let rig = rig();
    let other = GateBackend::new(rig.store.clone());
    let gate = rig.gated.gate(Op::Put, "k", Phase::Pre);
    let mut held_put = Box::pin(rig.gated.put("k", bytes("first")));
    until_parked(&gate, &mut held_put).await;

    other.put("k", bytes("second")).await.unwrap();
    assert!(gate.release());
    assert!(matches!(held_put.await, Err(Error::AlreadyExists(_))));
    assert_eq!(rig.memory.get("k", None).await.unwrap(), bytes("second"));
}

/// Two gates on different calls of one wrapper hold both, and each releases its own.
#[tokio::test]
async fn two_gates_hold_two_calls_and_release_each_its_own() {
    let rig = rig();
    let on_a = rig.gated.gate(Op::Put, "a", Phase::Pre);
    let on_b = rig.gated.gate(Op::Put, "b", Phase::Pre);
    let mut put_a = Box::pin(rig.gated.put("a", bytes("A")));
    let mut put_b = Box::pin(rig.gated.put("b", bytes("B")));
    until_parked(&on_a, &mut put_a).await;
    until_parked(&on_b, &mut put_b).await;

    assert!(on_b.release());
    put_b.await.unwrap();
    assert!(rig.memory.head("a").await.is_err());
    assert!(on_a.release());
    put_a.await.unwrap();
    assert_eq!(rig.memory.get("a", None).await.unwrap(), bytes("A"));
}

/// Every call of the backend is held under the kind it belongs to, and does what it does on the
/// wrapped backend once it is released: a call that took another kind's name would pass the
/// gate of its own kind unheld.
#[tokio::test]
async fn every_call_is_held_under_its_own_kind_and_then_runs() {
    let rig = rig();
    let gated = &rig.gated;
    rig.memory.put("o", bytes("abc")).await.unwrap();
    let tag = rig.memory.head("o").await.unwrap().tag;

    held(gated, Op::Put, gated.put("n1", bytes("1"))).await;
    held(gated, Op::Put, gated.put_tagged("n2", bytes("2"))).await;
    held(gated, Op::Put, gated.put_stream("n3", Some(1), chunks("3"))).await;
    let hot_only = gated.put_stream_hot_only("n4", None, chunks("4"));
    held(gated, Op::Put, hot_only).await;

    let replaced = gated.put_if_match("o", bytes("abd"), &tag);
    let tag = held(gated, Op::PutIfMatch, replaced).await;
    let streamed = gated.put_stream_if_match("o", Some(3), chunks("abe"), &tag);
    held(gated, Op::PutIfMatch, streamed).await;

    let reserved = held(
        gated,
        Op::PutReservation,
        gated.put_reservation("r", bytes("r1")),
    )
    .await;
    let kept = gated.put_reservation_if_match("r", bytes("r2"), &reserved);
    held(gated, Op::PutReservation, kept).await;

    assert_eq!(
        held(gated, Op::Get, gated.get("o", None)).await,
        bytes("abe")
    );
    let tagged = held(gated, Op::Get, gated.get_tagged("o", None)).await;
    assert_eq!(tagged.0, bytes("abe"));
    let mut body = held(gated, Op::Get, gated.get_stream("o", None)).await;
    assert_eq!(body.next().await.unwrap().unwrap(), bytes("abe"));
    assert_eq!(held(gated, Op::Head, gated.head("o")).await.size, 3);

    let listed = held(gated, Op::List, gated.list("n")).await;
    assert_eq!(listed.len(), 4);
    let page = held(gated, Op::List, gated.list_page("n", None, 2)).await;
    assert_eq!(page.len(), 2);

    let tag_n2 = rig.memory.head("n2").await.unwrap().tag;
    held(
        gated,
        Op::DeleteIfMatch,
        gated.delete_if_match("n2", &tag_n2),
    )
    .await;
    held(gated, Op::Delete, gated.delete("n1")).await;
    assert!(rig.memory.head("n1").await.is_err());
    assert!(rig.memory.head("n2").await.is_err());

    let id = held(gated, Op::CreateUpload, gated.create_upload("u")).await;
    let part = gated.upload_part("u", &id, 1, chunks("p1"));
    let tag_1 = held(gated, Op::UploadPart, part).await;
    let sized = gated.upload_part_with_len("u", &id, 2, Some(2), chunks("p2"));
    let tag_2 = held(gated, Op::UploadPart, sized).await;
    let parts = [(1, tag_1), (2, tag_2)];
    held(
        gated,
        Op::CompleteUpload,
        gated.complete_upload("u", &id, &parts),
    )
    .await;
    assert_eq!(rig.memory.get("u", None).await.unwrap(), bytes("p1p2"));

    let aborted = held(gated, Op::CreateUpload, gated.create_upload("v")).await;
    let open = held(gated, Op::ListUploads, gated.list_uploads("", None, 10)).await;
    assert_eq!(open.len(), 1);
    held(gated, Op::AbortUpload, gated.abort_upload("v", &aborted)).await;
    assert!(
        rig.memory
            .list_uploads("", None, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A backend whose answers to the calls the wrapper does not hold apart from the rest are
/// distinguishable from the provided ones.
struct Overrides;

#[async_trait::async_trait]
impl Backend for Overrides {
    fn needs_length(&self) -> bool {
        true
    }
    async fn put(&self, _: &str, _: Bytes) -> Result<(), Error> {
        Ok(())
    }
    async fn put_if_match(&self, _: &str, _: Bytes, _: &str) -> Result<String, Error> {
        Ok(String::new())
    }
    async fn get(&self, key: &str, _: Option<crate::ByteRange>) -> Result<Bytes, Error> {
        Err(Error::NotFound(key.to_owned()))
    }
    async fn head(&self, key: &str) -> Result<crate::Meta, Error> {
        Err(Error::NotFound(key.to_owned()))
    }
    async fn delete(&self, _: &str) -> Result<(), Error> {
        Ok(())
    }
    async fn list(&self, _: &str) -> Result<Vec<crate::Listed>, Error> {
        Ok(Vec::new())
    }
    async fn list_page(
        &self,
        _: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<crate::Listed>, Error> {
        let page = crate::Listed {
            key: "from-list-page".to_owned(),
            size: 0,
            modified: 0,
        };
        Ok(vec![page])
    }
}

/// The wrapper says what the wrapped backend says about the length of a streamed body.
#[tokio::test]
async fn needs_length_is_the_wrapped_backends() {
    assert!(GateBackend::new(Arc::new(Overrides)).needs_length());
    assert!(!rig().gated.needs_length());
}

/// A page is the wrapped backend's own page (the provided one would cut the whole listing, which
/// is empty here).
#[tokio::test]
async fn list_page_is_the_wrapped_backends_own_answer() {
    let gated = GateBackend::new(Arc::new(Overrides));
    let page = gated.list_page("p/", None, 10).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].key, "from-list-page");
}

/// A page is held at a gate of the list kind, ahead of the wrapped backend, and is then the
/// wrapped backend's own page.
#[tokio::test]
async fn list_page_is_held_under_the_list_kind() {
    let gated = GateBackend::new(Arc::new(Overrides));
    let page = held(&gated, Op::List, gated.list_page("p/", Some("a"), 5)).await;
    assert_eq!(page[0].key, "from-list-page");
}
