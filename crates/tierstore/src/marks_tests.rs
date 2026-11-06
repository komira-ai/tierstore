//! The store calls on markers: create, change by tag, remove and read, on the in-memory backend
//! behind a fault-injecting wrapper and a gate per actor, with the races between actors driven one
//! call at a time.

use std::future::Future;
use std::sync::Arc;

use crate::faulty::{FaultyBackend, Op};
use crate::{
    Backend, Creation, Error, Gate, GateBackend, MARKER_VERSION, MarkBirth, MarkMove, MarkSeen,
    Marker, MarkerState, MarkerWhy, Markers, MarkersConfig, MemoryBackend, Mono, Outcome, Phase,
    RecordingSleeper, TestClock, Written, decode_marker, encode_marker,
};

/// One memory bucket for the marks, a fault-injecting wrapper over it, and a gated view of the
/// wrapper per actor so that a test holds the call of one actor and not of another.
struct Rig {
    mono: Arc<Mono>,
    memory: Arc<MemoryBackend>,
    store: Arc<FaultyBackend>,
}

struct Actor {
    markers: Markers,
    gated: Arc<GateBackend>,
}

impl Rig {
    fn new() -> Self {
        let memory = Arc::new(MemoryBackend::with_clock(Arc::new(TestClock::new())));
        let store = Arc::new(FaultyBackend::new(memory.clone()));
        Self {
            mono: Arc::new(Mono::new()),
            memory,
            store,
        }
    }

    /// The actor `id`, whose nonces are `<id>.0`, `<id>.1` and so on.
    fn actor(&self, id: &str) -> Actor {
        let gated = Arc::new(GateBackend::new(self.store.clone()));
        let markers = Markers::with_boot(
            gated.clone(),
            id,
            MarkersConfig::default(),
            self.mono.clone(),
            Arc::new(RecordingSleeper::new()),
            id,
        )
        .expect("the default config holds together");
        Actor { markers, gated }
    }

    /// Stores `marker` as a writer of the format would, under the name of its key.
    async fn seed(&self, marker: &Marker) {
        let body = encode_marker(marker);
        self.memory.put(&marker.key, body).await.unwrap();
    }

    /// The marker object of `key` as the store holds it, with its tag, read behind the back of
    /// every actor.
    async fn stored(&self, key: &str) -> Option<MarkSeen> {
        let (body, tag) = self.memory.get_tagged(key, None).await.ok()?;
        let marker = decode_marker(&body).unwrap();
        Some(MarkSeen { marker, tag })
    }
}

/// Polls `call` until `gate` has taken it. The call must not finish on its own before that.
async fn until_parked<F: Future + Unpin>(gate: &Gate, call: &mut F) {
    tokio::select! {
        _ = &mut *call => panic!("the call finished instead of parking"),
        () = gate.parked() => {}
    }
}

fn marker(key: &str, st: MarkerState, gone: bool, dirty: bool) -> Marker {
    Marker {
        v: MARKER_VERSION,
        key: key.to_owned(),
        st,
        gone,
        dirty,
        why: None,
        owner: None,
        nonce: "seed".to_owned(),
        extra: serde_json::Map::new(),
    }
}

fn dirty(key: &str) -> Marker {
    marker(key, MarkerState::Dirty, false, true)
}

fn made(created: Creation) -> Written {
    match created {
        Creation::Made(written) => written,
        Creation::Exists => panic!("the marker exists"),
    }
}

fn done<T>(outcome: Outcome<T>) -> T {
    match outcome {
        Outcome::Done(value) => value,
        Outcome::Lost => panic!("lost"),
        Outcome::Refused => panic!("refused"),
    }
}

/// The marker an actor holds after it created `key` as `birth`.
async fn create(actor: &Actor, key: &str, birth: MarkBirth) -> Written {
    made(actor.markers.create(key, birth).await.unwrap())
}

/// The marker `actor` makes of `seen` by `mv`.
async fn advance(actor: &Actor, seen: &MarkSeen, mv: MarkMove) -> Written {
    done(actor.markers.advance(seen, mv).await.unwrap())
}

/// Asserts that a create stores the marker as its birth says, create-only, under the name of the
/// key (with a `/` in it), gives the tag the store gave, writes a nonce of its own, and records the
/// reading of the monotonic clock from before the request.
#[tokio::test]
async fn a_create_stores_the_marker_under_the_name_of_the_key_with_a_nonce_and_its_tag() {
    let rig = Rig::new();
    let a = rig.actor("a");
    rig.mono.advance(std::time::Duration::from_secs(7));
    let first = create(&a, "models/a.bin", MarkBirth::Dirty).await;
    let second = create(&a, "models/b.bin", MarkBirth::Want).await;

    let want = Marker {
        nonce: "a.0".to_owned(),
        ..dirty("models/a.bin")
    };
    assert_eq!(first.seen.marker, want);
    assert_eq!(first.sent_at, 7000);
    assert_eq!(rig.stored("models/a.bin").await, Some(first.seen));
    assert_eq!(second.seen.marker.nonce, "a.1");
    assert_eq!(second.seen.marker.st, MarkerState::Want);
    assert_eq!(rig.stored("models/b.bin").await, Some(second.seen));
}

/// Asserts that a lock is created owned by the writer with the reason, and a marker stays where it
/// was when it is created again: the second writer is told it exists and the first is untouched.
#[tokio::test]
async fn a_create_where_a_marker_exists_is_exists_and_changes_nothing() {
    let rig = Rig::new();
    let (a, b) = (rig.actor("a"), rig.actor("b"));
    let locked = create(&a, "k", MarkBirth::Lock(MarkerWhy::Overwrite)).await;
    assert_eq!(locked.seen.marker.owner.as_deref(), Some("a"));
    assert_eq!(locked.seen.marker.why, Some(MarkerWhy::Overwrite));

    let second = b.markers.create("k", MarkBirth::Dirty).await.unwrap();
    assert_eq!(second, Creation::Exists);
    assert_eq!(rig.stored("k").await, Some(locked.seen));
}

/// Asserts that a create the store refuses (full, a key it cannot hold) gives that error as it is,
/// stores nothing, and does not read back: a refusal is not an unknown outcome.
#[tokio::test]
async fn a_create_the_store_refuses_gives_the_refusal_and_stores_nothing() {
    for refusal in [
        Error::Full("no room".to_owned()),
        Error::InvalidKey("k".to_owned()),
    ] {
        let rig = Rig::new();
        let a = rig.actor("a");
        rig.store.fail_next_with(Op::Put, refusal.clone());
        let error = a.markers.create("k", MarkBirth::Dirty).await.unwrap_err();
        assert_eq!(error, refusal);
        assert_eq!(rig.stored("k").await, None);
        assert_eq!(rig.store.calls(Op::Get), 0);
    }
}

/// Asserts that a create whose reply was lost after the store did it is taken, with the marker read
/// back, and that the nonce is what tells it from another's.
#[tokio::test]
async fn a_create_whose_reply_was_lost_after_it_landed_is_made() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let gate = a.gated.gate(Op::Put, "", Phase::Post);
    let mut creating = Box::pin(a.markers.create("k", MarkBirth::Dirty));
    until_parked(&gate, &mut creating).await;
    assert!(gate.fail_post());
    let written = made(creating.await.unwrap());
    assert_eq!(written.seen.marker.nonce, "a.0");
    assert_eq!(rig.stored("k").await, Some(written.seen));
}

/// Asserts that a lock whose creation ended in an unknown outcome is not assumed taken: when the
/// request never reached the store, the answer is the original error and nothing is stored; when
/// it did and the read-back fails too, the answer is the original error again, and the lock that
/// is stored is nobody's but the lease's.
#[tokio::test]
async fn an_ambiguous_lock_is_not_assumed_taken() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let gate = a.gated.gate(Op::Put, "", Phase::Pre);
    let mut creating = Box::pin(a.markers.create("k", MarkBirth::Lock(MarkerWhy::Delete)));
    until_parked(&gate, &mut creating).await;
    assert!(gate.release_with(Error::Other("reset".to_owned())));
    let error = creating.await.unwrap_err();
    assert_eq!(error, Error::Other("reset".to_owned()));
    assert_eq!(rig.stored("k").await, None);

    let gate = a.gated.gate(Op::Put, "", Phase::Post);
    let mut creating = Box::pin(a.markers.create("k", MarkBirth::Lock(MarkerWhy::Delete)));
    until_parked(&gate, &mut creating).await;
    rig.store
        .fail_next_with(Op::Get, Error::Unavailable("still down".to_owned()));
    assert!(gate.release_with(Error::Unavailable("reply lost".to_owned())));
    let error = creating.await.unwrap_err();
    assert_eq!(error, Error::Unavailable("reply lost".to_owned()));
    let stored = rig.stored("k").await.unwrap();
    assert_eq!(stored.marker.owner.as_deref(), Some("a"));
}

/// Asserts that a create that failed ambiguously, while another actor took the key, is `Exists`,
/// and the other's marker stands.
#[tokio::test]
async fn a_create_that_failed_while_another_took_the_key_says_exists() {
    let rig = Rig::new();
    let (a, b) = (rig.actor("a"), rig.actor("b"));
    let gate = a.gated.gate(Op::Put, "", Phase::Pre);
    let mut creating = Box::pin(a.markers.create("k", MarkBirth::Dirty));
    until_parked(&gate, &mut creating).await;
    let theirs = create(&b, "k", MarkBirth::Want).await;
    assert!(gate.release_with(Error::Other("reset".to_owned())));
    assert_eq!(creating.await.unwrap(), Creation::Exists);
    assert_eq!(rig.stored("k").await, Some(theirs.seen));
}

/// Asserts that two instances of one actor id write nonces that differ, as a process that restarts
/// under its configured id does: the counter starts again, the boot does not repeat.
#[tokio::test]
async fn markers_made_apart_for_one_actor_write_different_nonces() {
    let rig = Rig::new();
    let make = || {
        Markers::new(
            rig.store.clone(),
            "same",
            MarkersConfig::default(),
            rig.mono.clone(),
            Arc::new(RecordingSleeper::new()),
        )
        .unwrap()
    };
    let (before, after) = (make(), make());
    let first = made(before.create("x", MarkBirth::Dirty).await.unwrap());
    let second = made(after.create("y", MarkBirth::Dirty).await.unwrap());
    assert_ne!(first.seen.marker.nonce, second.seen.marker.nonce);
    assert!(first.seen.marker.nonce.ends_with(".0"));
    assert!(second.seen.marker.nonce.ends_with(".0"));
}

/// Asserts that a change stores the new marker over the old by its tag, gives the new tag, keeps
/// the key, and writes a nonce that is new (so the tag moved).
#[tokio::test]
async fn a_change_writes_the_new_marker_by_the_tag_and_gives_its_tag() {
    let rig = Rig::new();
    let a = rig.actor("a");
    rig.seed(&dirty("k")).await;
    let seen = a.markers.read("k").await.unwrap().unwrap();

    let locked = advance(&a, &seen, MarkMove::Lock(MarkerWhy::Overwrite)).await;
    assert_eq!(locked.seen.marker.st, MarkerState::Lock);
    assert_eq!(locked.seen.marker.owner.as_deref(), Some("a"));
    assert_eq!(locked.seen.marker.nonce, "a.0");
    assert_ne!(locked.seen.tag, seen.tag);
    assert_eq!(rig.stored("k").await, Some(locked.seen));
}

/// Asserts that a change carries over what a later release put in the marker: its `v`, a field this
/// release does not know and a state it does not know, whatever the move.
#[tokio::test]
async fn a_change_keeps_the_version_the_fields_and_the_words_a_later_release_added() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let body = br#"{"v":2,"key":"k","st":"DIRTY","gone":true,"dirty":true,"nonce":"n","lease_ms":9,"note":{"x":[1,2]}}"#;
    rig.memory.put("k", body.to_vec().into()).await.unwrap();
    let seen = a.markers.read("k").await.unwrap().unwrap();
    let bumped = advance(&a, &seen, MarkMove::Bump).await;
    let stored = rig.stored("k").await.unwrap();
    assert_eq!(stored, bumped.seen);
    assert_eq!(stored.marker.v, 2);
    assert_eq!(stored.marker.extra, seen.marker.extra);
    assert_eq!(stored.marker.extra.len(), 2);
    assert!(stored.marker.gone);

    let newer = include_bytes!("../tests/stored_formats/fixtures/marker_newer.json");
    rig.memory
        .put("models/a.bin", newer.to_vec().into())
        .await
        .unwrap();
    let seen = a.markers.read("models/a.bin").await.unwrap().unwrap();
    assert_eq!(seen.marker.st, MarkerState::Other("FROZEN".to_owned()));
    let reaped = advance(&a, &seen, MarkMove::Reap).await;
    assert_eq!(reaped.seen.marker.st, MarkerState::Dirty);
    assert_eq!(reaped.seen.marker.extra, seen.marker.extra);
    assert_eq!(reaped.seen.marker.extra.len(), 2);
    assert_eq!(rig.stored("models/a.bin").await, Some(reaped.seen));
}

/// Asserts that a move that does not apply to the marker as it was read is refused without a call
/// to the store.
#[tokio::test]
async fn a_change_that_does_not_apply_makes_no_call() {
    let rig = Rig::new();
    let a = rig.actor("a");
    rig.seed(&dirty("k")).await;
    let seen = a.markers.read("k").await.unwrap().unwrap();
    let refused = a
        .markers
        .advance(&seen, MarkMove::Release { changed: false })
        .await
        .unwrap();
    assert_eq!(refused, Outcome::Refused);
    assert_eq!(rig.store.calls(Op::PutIfMatch), 0);
    assert_eq!(rig.stored("k").await, Some(seen));
}

/// Asserts a lost compare-and-swap race: two writers read one `DIRTY` marker, the first to reach
/// the store takes the lock, and the second's change, which reaches the store after it, is lost and
/// changes nothing.
#[tokio::test]
async fn of_two_changes_of_one_version_the_second_to_reach_the_store_is_lost() {
    let rig = Rig::new();
    let (a, b) = (rig.actor("a"), rig.actor("b"));
    rig.seed(&dirty("k")).await;
    let seen_by_a = a.markers.read("k").await.unwrap().unwrap();
    let seen_by_b = b.markers.read("k").await.unwrap().unwrap();

    let gate = b.gated.gate(Op::PutIfMatch, "", Phase::Pre);
    let mut racing = Box::pin(
        b.markers
            .advance(&seen_by_b, MarkMove::Lock(MarkerWhy::Delete)),
    );
    until_parked(&gate, &mut racing).await;
    let winner = advance(&a, &seen_by_a, MarkMove::Lock(MarkerWhy::Overwrite)).await;
    assert!(gate.release());
    assert_eq!(racing.await.unwrap(), Outcome::Lost);
    assert_eq!(rig.stored("k").await, Some(winner.seen));
}

/// Asserts that a change of a marker that has been removed since it was read is lost.
#[tokio::test]
async fn a_change_of_a_marker_that_is_gone_is_lost() {
    let rig = Rig::new();
    let a = rig.actor("a");
    rig.seed(&dirty("k")).await;
    let seen = a.markers.read("k").await.unwrap().unwrap();
    rig.memory.delete("k").await.unwrap();
    let lost = a.markers.advance(&seen, MarkMove::Bump).await.unwrap();
    assert_eq!(lost, Outcome::Lost);
    assert_eq!(rig.stored("k").await, None);
}

/// Asserts that a change the store refuses (full) gives that error as it is, leaves the marker
/// as it was, and does not read back.
#[tokio::test]
async fn a_change_the_store_refuses_gives_the_refusal_and_changes_nothing() {
    let rig = Rig::new();
    let a = rig.actor("a");
    rig.seed(&dirty("k")).await;
    let seen = a.markers.read("k").await.unwrap().unwrap();
    let gets = rig.store.calls(Op::Get);
    rig.store
        .fail_next_with(Op::PutIfMatch, Error::Full("no room".to_owned()));
    let error = a.markers.advance(&seen, MarkMove::Bump).await.unwrap_err();
    assert_eq!(error, Error::Full("no room".to_owned()));
    assert_eq!(rig.stored("k").await, Some(seen));
    assert_eq!(rig.store.calls(Op::Get), gets);
}

/// Asserts that a change whose reply was lost after the store did it is done, with the marker read
/// back, and keeps the time its request was sent.
#[tokio::test]
async fn a_change_whose_reply_was_lost_after_it_landed_is_done() {
    let rig = Rig::new();
    let a = rig.actor("a");
    rig.seed(&dirty("k")).await;
    let seen = a.markers.read("k").await.unwrap().unwrap();
    rig.mono.advance(std::time::Duration::from_secs(3));
    let gate = a.gated.gate(Op::PutIfMatch, "", Phase::Post);
    let mut changing = Box::pin(a.markers.advance(&seen, MarkMove::Lock(MarkerWhy::Evict)));
    until_parked(&gate, &mut changing).await;
    rig.mono.advance(std::time::Duration::from_secs(1));
    assert!(gate.fail_post());
    let written = done(changing.await.unwrap());
    assert_eq!(written.seen.marker.nonce, "a.0");
    assert_eq!(written.sent_at, 3000);
    assert_eq!(rig.stored("k").await, Some(written.seen));
}

/// Asserts the other ends of a change whose reply was lost: it did not land and the marker stands
/// (lost, read again), another actor changed the marker meanwhile (lost, theirs stands), the marker
/// was removed meanwhile (lost), and the read-back fails (the original error).
#[tokio::test]
async fn a_change_whose_reply_was_lost_and_did_not_land_is_lost_or_the_original_error() {
    // It never reached the store.
    let rig = Rig::new();
    let a = rig.actor("a");
    rig.seed(&dirty("k")).await;
    let seen = a.markers.read("k").await.unwrap().unwrap();
    let gate = a.gated.gate(Op::PutIfMatch, "", Phase::Pre);
    let mut changing = Box::pin(a.markers.advance(&seen, MarkMove::Bump));
    until_parked(&gate, &mut changing).await;
    assert!(gate.release_with(Error::Other("reset".to_owned())));
    assert_eq!(changing.await.unwrap(), Outcome::Lost);
    assert_eq!(rig.stored("k").await, Some(seen.clone()));

    // Another actor changed it first.
    let b = rig.actor("b");
    let gate = a.gated.gate(Op::PutIfMatch, "", Phase::Pre);
    let mut changing = Box::pin(a.markers.advance(&seen, MarkMove::Bump));
    until_parked(&gate, &mut changing).await;
    let theirs = advance(&b, &seen, MarkMove::Lock(MarkerWhy::Delete)).await;
    assert!(gate.release_with(Error::Other("reset".to_owned())));
    assert_eq!(changing.await.unwrap(), Outcome::Lost);
    assert_eq!(rig.stored("k").await, Some(theirs.seen.clone()));

    // It was removed.
    let gate = a.gated.gate(Op::PutIfMatch, "", Phase::Pre);
    let mut changing = Box::pin(a.markers.advance(&seen, MarkMove::Bump));
    until_parked(&gate, &mut changing).await;
    rig.memory.delete("k").await.unwrap();
    assert!(gate.release_with(Error::Other("reset".to_owned())));
    assert_eq!(changing.await.unwrap(), Outcome::Lost);

    // The read-back fails.
    rig.seed(&dirty("k")).await;
    let seen = a.markers.read("k").await.unwrap().unwrap();
    let gate = a.gated.gate(Op::PutIfMatch, "", Phase::Post);
    let mut changing = Box::pin(a.markers.advance(&seen, MarkMove::Bump));
    until_parked(&gate, &mut changing).await;
    rig.store
        .fail_next_with(Op::Get, Error::Unavailable("still down".to_owned()));
    assert!(gate.release_with(Error::Unavailable("reply lost".to_owned())));
    let error = changing.await.unwrap_err();
    assert_eq!(error, Error::Unavailable("reply lost".to_owned()));
}

/// The first of two actors holds a `CLEARING` marker it made from a `DIRTY` one.
async fn clearing(rig: &Rig, actor: &Actor, key: &str) -> Written {
    rig.seed(&dirty(key)).await;
    let seen = actor.markers.read(key).await.unwrap().unwrap();
    advance(actor, &seen, MarkMove::Clear).await
}

/// Asserts that the owner of a `CLEARING` marker removes it, and that nothing is left to read.
#[tokio::test]
async fn the_owner_removes_its_clearing_marker() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let cleared = clearing(&rig, &a, "k").await;
    let removed = a.markers.remove(&cleared.seen).await.unwrap();
    assert_eq!(removed, Outcome::Done(()));
    assert_eq!(rig.stored("k").await, None);
}

/// Asserts that a remove of a marker that is not the actor's own `CLEARING` (a `DIRTY` marker,
/// a lock, another's clearing) is refused with no call, and removes nothing.
#[tokio::test]
async fn a_remove_of_anything_but_the_actors_own_clearing_makes_no_call() {
    let rig = Rig::new();
    let (a, b) = (rig.actor("a"), rig.actor("b"));
    let theirs = clearing(&rig, &b, "k").await;
    let locked = create(&a, "l", MarkBirth::Lock(MarkerWhy::Evict)).await;
    rig.seed(&dirty("d")).await;
    let plain = a.markers.read("d").await.unwrap().unwrap();
    for seen in [&theirs.seen, &locked.seen, &plain] {
        assert_eq!(a.markers.remove(seen).await.unwrap(), Outcome::Refused);
    }
    assert_eq!(rig.store.calls(Op::Delete), 0);
    assert_eq!(rig.stored("k").await, Some(theirs.seen));
}

/// Asserts that the leader finishes a `CLEARING` whose owner it judged departed, and only a
/// `CLEARING`.
#[tokio::test]
async fn the_leader_removes_a_clearing_whose_owner_departed_and_nothing_else() {
    let rig = Rig::new();
    let (dead, leader) = (rig.actor("dead"), rig.actor("leader"));
    let left = clearing(&rig, &dead, "k").await;
    let locked = create(&dead, "l", MarkBirth::Lock(MarkerWhy::Evict)).await;
    assert_eq!(
        leader.markers.remove_departed(&locked.seen).await.unwrap(),
        Outcome::Refused
    );
    assert_eq!(rig.store.calls(Op::Delete), 0);
    assert_eq!(
        leader.markers.remove_departed(&left.seen).await.unwrap(),
        Outcome::Done(())
    );
    assert_eq!(rig.stored("k").await, None);
    assert_eq!(rig.stored("l").await, Some(locked.seen));
}

/// Asserts the ends of a remove whose reply was lost: it landed (done), it did not land and the
/// marker is still the one that was read (the original error), another marker has taken the key
/// (lost), and the read-back fails (the original error).
#[tokio::test]
async fn a_remove_whose_reply_was_lost_is_settled_by_reading_the_marker_back() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let cleared = clearing(&rig, &a, "k").await;

    let gate = a.gated.gate(Op::Delete, "", Phase::Post);
    let mut removing = Box::pin(a.markers.remove(&cleared.seen));
    until_parked(&gate, &mut removing).await;
    assert!(gate.fail_post());
    assert_eq!(removing.await.unwrap(), Outcome::Done(()));
    assert_eq!(rig.stored("k").await, None);

    let cleared = clearing(&rig, &a, "k").await;
    let gate = a.gated.gate(Op::Delete, "", Phase::Pre);
    let mut removing = Box::pin(a.markers.remove(&cleared.seen));
    until_parked(&gate, &mut removing).await;
    assert!(gate.release_with(Error::Other("reset".to_owned())));
    let error = removing.await.unwrap_err();
    assert_eq!(error, Error::Other("reset".to_owned()));
    assert_eq!(rig.stored("k").await, Some(cleared.seen.clone()));

    let gate = a.gated.gate(Op::Delete, "", Phase::Pre);
    let mut removing = Box::pin(a.markers.remove(&cleared.seen));
    until_parked(&gate, &mut removing).await;
    rig.memory.delete("k").await.unwrap();
    rig.seed(&dirty("k")).await;
    assert!(gate.release_with(Error::Other("reset".to_owned())));
    assert_eq!(removing.await.unwrap(), Outcome::Lost);
    assert_eq!(rig.stored("k").await.unwrap().marker.nonce, "seed");

    let cleared = clearing(&rig, &a, "k2").await;
    let gate = a.gated.gate(Op::Delete, "", Phase::Pre);
    let mut removing = Box::pin(a.markers.remove(&cleared.seen));
    until_parked(&gate, &mut removing).await;
    rig.store
        .fail_next_with(Op::Get, Error::Unavailable("still down".to_owned()));
    assert!(gate.release_with(Error::Unavailable("reply lost".to_owned())));
    let error = removing.await.unwrap_err();
    assert_eq!(error, Error::Unavailable("reply lost".to_owned()));
}

/// Asserts that a delete the store refuses outright gives that error as it is.
#[tokio::test]
async fn a_remove_the_store_refuses_gives_the_refusal() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let cleared = clearing(&rig, &a, "k").await;
    rig.store
        .fail_next_with(Op::Delete, Error::Reserved("k".to_owned()));
    let error = a.markers.remove(&cleared.seen).await.unwrap_err();
    assert_eq!(error, Error::Reserved("k".to_owned()));
    assert_eq!(rig.stored("k").await, Some(cleared.seen));
}

/// Asserts that a read of a missing marker is none, and that a read is one request that gives the
/// body and the tag together.
#[tokio::test]
async fn a_read_is_one_request_for_the_body_and_its_tag_and_a_missing_marker_is_none() {
    let rig = Rig::new();
    let a = rig.actor("a");
    assert_eq!(a.markers.read("k").await.unwrap(), None);
    rig.seed(&dirty("k")).await;
    let seen = a.markers.read("k").await.unwrap().unwrap();
    assert_eq!(seen.marker, dirty("k"));
    assert_eq!(rig.store.calls(Op::Get), 2);
    assert_eq!(rig.store.calls(Op::Head), 0);
    assert_eq!(rig.stored("k").await, Some(seen));
}

/// Asserts that a read whose request fails gives the error, and that a body that is not a marker, or
/// that names another key, is an error naming the marker and not a marker.
#[tokio::test]
async fn a_read_of_a_failed_request_or_a_corrupt_body_is_an_error() {
    let rig = Rig::new();
    let a = rig.actor("a");
    rig.store
        .fail_next_with(Op::Get, Error::Unavailable("down".to_owned()));
    let error = a.markers.read("k").await.unwrap_err();
    assert_eq!(error, Error::Unavailable("down".to_owned()));

    rig.memory.put("k", "not json".into()).await.unwrap();
    let Err(Error::Other(text)) = a.markers.read("k").await else {
        panic!("a corrupt body was read");
    };
    assert!(text.starts_with("corrupt marker: "), "{text}");

    let body = encode_marker(&dirty("elsewhere"));
    rig.memory.put("k2", body).await.unwrap();
    let error = a.markers.read("k2").await.unwrap_err();
    let text = "corrupt marker: the object k2 names the key elsewhere";
    assert_eq!(error, Error::Other(text.to_owned()));
}
