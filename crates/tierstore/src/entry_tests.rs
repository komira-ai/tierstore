//! The entry of a key: its body, the shape of each step, and the store calls, with the races between
//! actors driven one call at a time through gates.

use std::future::Future;
use std::sync::Arc;

use bytes::Bytes;

use crate::faulty::{FaultyBackend, Op};
use crate::{
    Backend, Created, Entries, Entry, EntryOp, EntryState, Error, Gate, GateBackend, MemoryBackend,
    Outcome, Phase, Seen, Step, Take, TestClock, decode_entry, encode_entry, entry_key,
};

/// One memory bucket, a counter over it, and a wrapper per actor so that a test holds the call of
/// one actor and not of another.
struct Rig {
    clock: Arc<TestClock>,
    memory: Arc<MemoryBackend>,
    store: Arc<FaultyBackend>,
}

struct Actor {
    entries: Entries,
    gated: Arc<GateBackend>,
}

impl Rig {
    fn new() -> Self {
        let clock = Arc::new(TestClock::new());
        let memory = Arc::new(MemoryBackend::with_clock(clock.clone()));
        let store = Arc::new(FaultyBackend::new(memory.clone()));
        Self {
            clock,
            memory,
            store,
        }
    }

    fn actor(&self, id: &str) -> Actor {
        let gated = Arc::new(GateBackend::new(self.store.clone()));
        let entries = Entries::new(gated.clone(), id, self.clock.clone());
        Actor { entries, gated }
    }
}

/// Polls `call` until `gate` has taken it. The call must not finish on its own before that.
async fn until_parked<F: Future + Unpin>(gate: &Gate, call: &mut F) {
    tokio::select! {
        _ = &mut *call => panic!("the call finished instead of parking"),
        () = gate.parked() => {}
    }
}

fn taken(created: Created) -> Seen {
    match created {
        Created::Taken(seen) => seen,
        Created::Exists => panic!("the entry exists"),
    }
}

fn done<T>(outcome: Outcome<T>) -> T {
    match outcome {
        Outcome::Done(value) => value,
        Outcome::Lost => panic!("lost"),
        Outcome::Refused => panic!("refused"),
    }
}

async fn take(actor: &Actor, key: &str, how: Take) -> Seen {
    taken(actor.entries.create(key, how).await.unwrap())
}

async fn step(actor: &Actor, seen: &Seen, step: Step) -> Seen {
    done(actor.entries.advance(seen, step).await.unwrap())
}

async fn read(actor: &Actor, key: &str) -> Seen {
    actor.entries.read(key).await.unwrap().unwrap()
}

fn entry(op: EntryOp, st: EntryState, owner: Option<&str>) -> Entry {
    Entry {
        v: 1,
        key: "k".to_owned(),
        op,
        st,
        owner: owner.map(str::to_owned),
        nonce: "seed".to_owned(),
        created_ms: 5,
    }
}

/// Stores `entry` as a writer of the format would, under the name of its key.
async fn seed(rig: &Rig, entry: &Entry) {
    let body = encode_entry(entry);
    rig.memory.put(&entry_key(&entry.key), body).await.unwrap();
}

/// The first of the two actors holds a put that landed: `(PUT, PENDING)` with no owner.
async fn pending_put(client: &Actor) -> Seen {
    let held = take(client, "k", Take::Put).await;
    step(client, &held, Step::LandPut).await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Who {
    Me,
    Other,
    Nobody,
}

impl Who {
    fn owner(self) -> Option<&'static str> {
        match self {
            Who::Me => Some("me"),
            Who::Other => Some("them"),
            Who::Nobody => None,
        }
    }

    fn of(owner: Option<&str>) -> Who {
        match owner {
            None => Who::Nobody,
            Some("me") => Who::Me,
            Some(_) => Who::Other,
        }
    }
}

const OPS: [EntryOp; 5] = [
    EntryOp::Put,
    EntryOp::Del,
    EntryOp::Cb,
    EntryOp::Evict,
    EntryOp::Unknown,
];
const STATES: [EntryState; 4] = [
    EntryState::Held,
    EntryState::Pending,
    EntryState::Sealed,
    EntryState::Unknown,
];
const WHOS: [Who; 3] = [Who::Me, Who::Other, Who::Nobody];

// ---- the name and the body ----

/// Asserts that the name of an entry is the prefix and the SHA-256 of the key in hex (the digest of
/// "abc" is the well-known one), that it has the same length for a key of 1024 bytes, and that two
/// keys have two names.
#[test]
fn the_entry_name_is_the_prefix_and_the_sha256_hex_of_the_key_at_any_length() {
    let abc = "_entries/ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(entry_key("abc"), abc);
    assert_eq!(entry_key(&"k".repeat(1024)).len(), "_entries/".len() + 64);
    assert_ne!(entry_key("abc"), entry_key("abd"));
}

/// Asserts the exact bytes of a held entry and of a pending one, whose owner is written as `null`,
/// and that both read back as the entries written.
#[test]
fn an_entry_is_written_as_json_with_a_null_owner_and_reads_back() {
    let held = entry(EntryOp::Put, EntryState::Held, Some("p1"));
    let want =
        r#"{"v":1,"key":"k","op":"PUT","st":"HELD","owner":"p1","nonce":"seed","created_ms":5}"#;
    assert_eq!(encode_entry(&held), want);
    assert_eq!(decode_entry(want.as_bytes()).unwrap(), held);

    let pending = entry(EntryOp::Del, EntryState::Pending, None);
    let want =
        r#"{"v":1,"key":"k","op":"DEL","st":"PENDING","owner":null,"nonce":"seed","created_ms":5}"#;
    assert_eq!(encode_entry(&pending), want);
    assert_eq!(decode_entry(want.as_bytes()).unwrap(), pending);
}

/// Asserts that every operation and state is spelled as the format says.
#[test]
fn every_operation_and_state_has_its_word() {
    let ops = [
        (EntryOp::Put, "PUT"),
        (EntryOp::Del, "DEL"),
        (EntryOp::Cb, "CB"),
        (EntryOp::Evict, "EVICT"),
    ];
    for (op, word) in ops {
        let body = encode_entry(&entry(op, EntryState::Sealed, None));
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains(&format!(r#""op":"{word}""#)), "{text}");
    }
    let states = [
        (EntryState::Held, "HELD"),
        (EntryState::Pending, "PENDING"),
        (EntryState::Sealed, "SEALED"),
    ];
    for (st, word) in states {
        let body = encode_entry(&entry(EntryOp::Put, st, None));
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains(&format!(r#""st":"{word}""#)), "{text}");
    }
}

/// Asserts that a body a newer writer made, with a field this reader does not know, a later
/// version, and an operation and a state it does not know, still reads: the unknown words are
/// `Unknown`, and the field is ignored.
#[test]
fn a_body_with_added_fields_and_words_still_reads() {
    let body = r#"{"v":2,"key":"k","op":"XFER","st":"FROZEN","owner":null,"nonce":"n","created_ms":9,"extra":[1]}"#;
    let read = decode_entry(body.as_bytes()).unwrap();
    let want = Entry {
        v: 2,
        key: "k".to_owned(),
        op: EntryOp::Unknown,
        st: EntryState::Unknown,
        owner: None,
        nonce: "n".to_owned(),
        created_ms: 9,
    };
    assert_eq!(read, want);
}

/// Asserts that a body that is not an entry is refused with an error that says it is a corrupt
/// entry.
#[test]
fn a_body_that_is_not_an_entry_is_refused() {
    let error = decode_entry(b"{}").unwrap_err();
    assert!(error.to_string().contains("corrupt entry"), "{error}");
}

// ---- the steps ----

struct Row {
    op: EntryOp,
    st: EntryState,
    who: &'static [Who],
    to: (EntryOp, EntryState, Who),
}

const ANY: &[Who] = &[Who::Me, Who::Other, Who::Nobody];

fn row(op: EntryOp, st: EntryState, who: &'static [Who], to: (EntryOp, EntryState, Who)) -> Row {
    Row { op, st, who, to }
}

fn table() -> Vec<(Step, Vec<Row>)> {
    use EntryOp::{Cb, Del, Put};
    use EntryState::{Held, Pending, Sealed};
    vec![
        (
            Step::LandPut,
            vec![row(Put, Held, &[Who::Me], (Put, Pending, Who::Nobody))],
        ),
        (
            Step::Amend,
            vec![
                row(Put, Pending, ANY, (Del, Held, Who::Me)),
                row(Cb, Pending, ANY, (Del, Held, Who::Me)),
            ],
        ),
        (
            Step::AmendCopy,
            vec![row(Put, Held, &[Who::Other], (Del, Held, Who::Me))],
        ),
        (
            Step::Claim,
            vec![row(Del, Pending, ANY, (Del, Held, Who::Me))],
        ),
        (
            Step::Handover,
            vec![row(Del, Held, &[Who::Me], (Del, Pending, Who::Nobody))],
        ),
        (
            Step::HoldForCopy,
            vec![row(Put, Pending, ANY, (Put, Held, Who::Me))],
        ),
        (
            Step::Seal,
            vec![
                row(Put, Pending, ANY, (Put, Sealed, Who::Nobody)),
                row(Del, Pending, ANY, (Del, Sealed, Who::Nobody)),
                row(Cb, Pending, ANY, (Cb, Sealed, Who::Nobody)),
                row(Put, Held, &[Who::Me], (Put, Sealed, Who::Nobody)),
            ],
        ),
        (
            Step::Reap,
            vec![
                row(Put, Held, &[Who::Other], (Put, Pending, Who::Nobody)),
                row(Del, Held, &[Who::Other], (Del, Pending, Who::Nobody)),
            ],
        ),
    ]
}

/// Asserts, for every step and every operation, state and owner an entry can have (an owner that
/// is the actor, another, or none; the words a newer writer adds included), that the step gives
/// the shape of the table below or none, and that no other entry is accepted. This is the table of
/// the design's transitions E2 to E7.
#[test]
fn each_step_accepts_exactly_the_entries_of_its_row_and_gives_the_shape_of_its_row() {
    for (step, rows) in table() {
        for op in OPS {
            for st in STATES {
                for who in WHOS {
                    let from = entry(op, st, who.owner());
                    let got = step
                        .apply(&from, "me")
                        .map(|(op, st, owner)| (op, st, Who::of(owner)));
                    let want = rows
                        .iter()
                        .find(|row| row.op == op && row.st == st && row.who.contains(&who))
                        .map(|row| row.to);
                    assert_eq!(got, want, "{step:?} on {op:?} {st:?} {who:?}");
                }
            }
        }
    }
}

/// Asserts that only the amend of a copy that may be running and the claim of a handed-over delete
/// make a delete wait for the release of the entry.
#[test]
fn only_amending_a_copy_and_claiming_a_handover_wait_for_the_release() {
    let steps = [
        (Step::LandPut, false),
        (Step::Amend, false),
        (Step::AmendCopy, true),
        (Step::Claim, true),
        (Step::Handover, false),
        (Step::HoldForCopy, false),
        (Step::Seal, false),
        (Step::Reap, false),
    ];
    for (step, waits) in steps {
        assert_eq!(step.waits(), waits, "{step:?}");
    }
}

// ---- create and read ----

/// Asserts that each take writes the entry of its shape, with the owner only where the caller
/// holds the key, the first nonce of the actor and the wall clock for `created_ms`, and that a read
/// gives the same entry and tag.
#[tokio::test]
async fn each_take_creates_the_entry_of_its_shape() {
    use EntryOp::{Cb, Del, Evict, Put};
    use EntryState::{Held, Pending, Sealed};
    let shapes = [
        (Take::Put, Put, Held, Some("p1")),
        (Take::Del, Del, Held, Some("p1")),
        (Take::CopyBack, Cb, Pending, None),
        (Take::Evict, Evict, Sealed, None),
        (Take::Stray, Put, Pending, None),
    ];
    for (how, op, st, owner) in shapes {
        let rig = Rig::new();
        rig.clock.set(1234);
        let actor = rig.actor("p1");
        let seen = take(&actor, "k", how).await;
        let want = Entry {
            v: 1,
            key: "k".to_owned(),
            op,
            st,
            owner: owner.map(str::to_owned),
            nonce: "p1.0".to_owned(),
            created_ms: 1234,
        };
        assert_eq!(seen.entry, want, "{how:?}");
        assert_eq!(read(&actor, "k").await, seen, "{how:?}");
    }
}

/// Asserts that a take of a key that has an entry says it exists and leaves the entry as it was.
#[tokio::test]
async fn a_take_of_a_key_with_an_entry_says_exists_and_changes_nothing() {
    let rig = Rig::new();
    let (a, b) = (rig.actor("a"), rig.actor("b"));
    let first = take(&a, "k", Take::Put).await;
    let second = b.entries.create("k", Take::Del).await.unwrap();
    assert_eq!(second, Created::Exists);
    assert_eq!(read(&b, "k").await, first);
}

/// Asserts that a create the store refuses for a reason that says nothing was written (here a full
/// store) is that error, and no entry exists.
#[tokio::test]
async fn a_create_the_store_refuses_gives_its_error_and_leaves_no_entry() {
    let rig = Rig::new();
    let a = rig.actor("a");
    rig.store
        .fail_next_with(Op::Put, Error::Full("hot".to_owned()));
    let error = a.entries.create("k", Take::Put).await.unwrap_err();
    assert_eq!(error, Error::Full("hot".to_owned()));
    assert_eq!(a.entries.read("k").await.unwrap(), None);
}

/// Asserts that a key with no entry reads as none.
#[tokio::test]
async fn a_key_with_no_entry_reads_as_none() {
    let rig = Rig::new();
    assert_eq!(rig.actor("a").entries.read("k").await.unwrap(), None);
}

/// Asserts that a failed head and a failed get of a read are the backend's errors, not an absent
/// entry.
#[tokio::test]
async fn a_read_whose_head_or_get_fails_gives_that_error() {
    for op in [Op::Head, Op::Get] {
        let rig = Rig::new();
        let a = rig.actor("a");
        take(&a, "k", Take::Put).await;
        rig.store
            .fail_next_with(op, Error::Unavailable("down".to_owned()));
        let error = a.entries.read("k").await.unwrap_err();
        assert_eq!(error, Error::Unavailable("down".to_owned()), "{op:?}");
    }
}

/// Asserts that a body that is not an entry is an error of the read that names a corrupt entry.
#[tokio::test]
async fn a_read_of_a_body_that_is_not_an_entry_is_an_error() {
    let rig = Rig::new();
    let body = Bytes::from_static(b"nonsense");
    rig.memory.put(&entry_key("k"), body).await.unwrap();
    let error = rig.actor("a").entries.read("k").await.unwrap_err();
    assert!(error.to_string().contains("corrupt entry"), "{error}");
}

/// Asserts that an entry that is deleted between the head and the get of a read reads as none.
#[tokio::test]
async fn an_entry_deleted_between_the_head_and_the_get_of_a_read_reads_as_none() {
    let rig = Rig::new();
    let a = rig.actor("a");
    take(&a, "k", Take::Put).await;
    let gate = a.gated.gate(Op::Get, "", Phase::Pre);
    let mut reading = Box::pin(a.entries.read("k"));
    until_parked(&gate, &mut reading).await;
    rig.memory.delete(&entry_key("k")).await.unwrap();
    assert!(gate.release());
    assert_eq!(reading.await.unwrap(), None);
}

/// Asserts that when the entry is replaced between the head and the get, the read has the newer
/// body with the older tag, and a step taken on it is lost and changes nothing.
#[tokio::test]
async fn a_read_with_a_newer_body_than_its_tag_is_lost_when_it_acts() {
    let rig = Rig::new();
    let (a, b) = (rig.actor("a"), rig.actor("b"));
    let held = take(&b, "k", Take::Put).await;
    let gate = a.gated.gate(Op::Get, "", Phase::Pre);
    let mut reading = Box::pin(a.entries.read("k"));
    until_parked(&gate, &mut reading).await;
    let landed = step(&b, &held, Step::LandPut).await;
    assert!(gate.release());
    let seen = reading.await.unwrap().unwrap();
    assert_eq!((seen.entry.st, &seen.tag), (EntryState::Pending, &held.tag));

    let lost = a.entries.advance(&seen, Step::Seal).await.unwrap();
    assert_eq!(lost, Outcome::Lost);
    assert_eq!(read(&a, "k").await, landed);
}

/// Asserts that a create whose reply was lost after the store did it is taken, with the entry read
/// back, and that the nonce is what tells it from another's.
#[tokio::test]
async fn a_create_whose_reply_was_lost_after_it_landed_is_taken() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let gate = a.gated.gate(Op::Put, "", Phase::Post);
    let mut creating = Box::pin(a.entries.create("k", Take::Put));
    until_parked(&gate, &mut creating).await;
    assert!(gate.release_with(Error::Unavailable("reply lost".to_owned())));
    let seen = taken(creating.await.unwrap());
    assert_eq!(seen.entry.nonce, "a.0");
    assert_eq!(read(&a, "k").await, seen);
}

/// Asserts that a create whose reply was lost and which did not land, with no entry stored, is the
/// original error and creates nothing.
#[tokio::test]
async fn a_create_that_failed_before_the_store_and_found_no_entry_gives_its_error() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let gate = a.gated.gate(Op::Put, "", Phase::Pre);
    let mut creating = Box::pin(a.entries.create("k", Take::Put));
    until_parked(&gate, &mut creating).await;
    assert!(gate.release_with(Error::Unavailable("timed out".to_owned())));
    let error = creating.await.unwrap_err();
    assert_eq!(error, Error::Unavailable("timed out".to_owned()));
    assert_eq!(a.entries.read("k").await.unwrap(), None);
}

/// Asserts that a create that failed ambiguously, while another actor took the entry, is
/// `Exists`, and the other's entry stands.
#[tokio::test]
async fn a_create_that_failed_while_another_took_the_entry_says_exists() {
    let rig = Rig::new();
    let (a, b) = (rig.actor("a"), rig.actor("b"));
    let gate = a.gated.gate(Op::Put, "", Phase::Pre);
    let mut creating = Box::pin(a.entries.create("k", Take::Put));
    until_parked(&gate, &mut creating).await;
    let theirs = take(&b, "k", Take::Del).await;
    assert!(gate.release_with(Error::Other("reset".to_owned())));
    assert_eq!(creating.await.unwrap(), Created::Exists);
    assert_eq!(read(&a, "k").await, theirs);
}

/// Asserts that a create that failed ambiguously and whose read-back fails too gives the original
/// error.
#[tokio::test]
async fn a_create_that_failed_ambiguously_and_cannot_read_back_gives_its_error() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let gate = a.gated.gate(Op::Put, "", Phase::Post);
    let mut creating = Box::pin(a.entries.create("k", Take::Put));
    until_parked(&gate, &mut creating).await;
    rig.store
        .fail_next_with(Op::Head, Error::Unavailable("still down".to_owned()));
    assert!(gate.release_with(Error::Unavailable("reply lost".to_owned())));
    let error = creating.await.unwrap_err();
    assert_eq!(error, Error::Unavailable("reply lost".to_owned()));
}

// ---- advance ----

/// Asserts the life of a put: taken, landed, held by the reconciler for the copy, sealed after it
/// and released. Each step is stored as it is returned, the nonce is new at each, `created_ms` stays
/// the one of the take while the clock moves, and nothing is left.
#[tokio::test]
async fn a_put_goes_from_held_to_pending_to_held_by_the_reconciler_to_sealed_to_gone() {
    let rig = Rig::new();
    rig.clock.set(100);
    let (client, recon) = (rig.actor("c1"), rig.actor("r1"));
    let held = take(&client, "k", Take::Put).await;
    rig.clock.set(900);
    let pending = step(&client, &held, Step::LandPut).await;
    let copying = step(&recon, &pending, Step::HoldForCopy).await;
    let sealed = step(&recon, &copying, Step::Seal).await;
    let all = [&held, &pending, &copying, &sealed];
    let nonces: Vec<&str> = all.iter().map(|seen| seen.entry.nonce.as_str()).collect();
    assert_eq!(nonces, ["c1.0", "c1.1", "r1.0", "r1.1"]);
    for seen in all {
        assert_eq!(seen.entry.created_ms, 100);
    }
    let shapes: Vec<_> = [&pending, &copying, &sealed]
        .iter()
        .map(|seen| (seen.entry.op, seen.entry.st, seen.entry.owner.clone()))
        .collect();
    let want = [
        (EntryOp::Put, EntryState::Pending, None),
        (EntryOp::Put, EntryState::Held, Some("r1".to_owned())),
        (EntryOp::Put, EntryState::Sealed, None),
    ];
    assert_eq!(shapes, want);
    assert_eq!(read(&recon, "k").await, sealed);

    assert_eq!(
        recon.entries.release(&sealed).await.unwrap(),
        Outcome::Done(())
    );
    assert_eq!(recon.entries.read("k").await.unwrap(), None);
}

/// Asserts the life of a delete that amends a landed put and hands it over, and of a retried
/// delete that claims it and releases it.
#[tokio::test]
async fn a_delete_amends_a_pending_put_hands_it_over_and_a_retry_claims_and_releases_it() {
    let rig = Rig::new();
    let (client, d1, d2) = (rig.actor("c1"), rig.actor("d1"), rig.actor("d2"));
    let pending = pending_put(&client).await;

    let amended = step(&d1, &pending, Step::Amend).await;
    assert_eq!(amended.entry.owner.as_deref(), Some("d1"));
    assert_eq!(amended.entry.op, EntryOp::Del);
    let handed = step(&d1, &amended, Step::Handover).await;
    assert_eq!(handed.entry.st, EntryState::Pending);
    assert_eq!(handed.entry.owner, None);

    let claimed = step(&d2, &handed, Step::Claim).await;
    assert_eq!(claimed.entry.owner.as_deref(), Some("d2"));
    assert_eq!(
        d2.entries.release(&claimed).await.unwrap(),
        Outcome::Done(())
    );
    assert_eq!(d2.entries.read("k").await.unwrap(), None);
}

/// Asserts that a step that does not apply to the entry as it was read is refused without any call
/// to the store.
#[tokio::test]
async fn a_step_that_does_not_apply_is_refused_and_makes_no_store_call() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let held = take(&a, "k", Take::Put).await;
    let before = rig.store.calls(Op::PutIfMatch);
    let refused = a.entries.advance(&held, Step::Handover).await.unwrap();
    assert_eq!(refused, Outcome::Refused);
    assert_eq!(rig.store.calls(Op::PutIfMatch), before);
}

/// Asserts that a step on an entry that another actor has changed since it was read is lost, and the
/// other's entry stands.
#[tokio::test]
async fn a_step_on_a_stale_tag_is_lost_and_the_winner_stands() {
    let rig = Rig::new();
    let (client, r1, d1) = (rig.actor("c1"), rig.actor("r1"), rig.actor("d1"));
    let pending = pending_put(&client).await;
    let copying = step(&r1, &pending, Step::HoldForCopy).await;
    let lost = d1.entries.advance(&pending, Step::Amend).await.unwrap();
    assert_eq!(lost, Outcome::Lost);
    assert_eq!(read(&d1, "k").await, copying);
}

/// Asserts that a step on an entry that is gone is lost.
#[tokio::test]
async fn a_step_on_an_entry_that_is_gone_is_lost() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let held = take(&a, "k", Take::Put).await;
    rig.memory.delete(&entry_key("k")).await.unwrap();
    let lost = a.entries.advance(&held, Step::LandPut).await.unwrap();
    assert_eq!(lost, Outcome::Lost);
}

/// Asserts that a replace the store refuses for a reason that says nothing was written is that
/// error.
#[tokio::test]
async fn a_replace_the_store_refuses_gives_its_error_and_the_entry_stands() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let held = take(&a, "k", Take::Put).await;
    rig.store
        .fail_next_with(Op::PutIfMatch, Error::Full("hot".to_owned()));
    let error = a.entries.advance(&held, Step::LandPut).await.unwrap_err();
    assert_eq!(error, Error::Full("hot".to_owned()));
    assert_eq!(read(&a, "k").await, held);
}

/// Asserts the race the hold before a copy exists for: the reconciler read a pending put, a delete
/// amended it before the reconciler's replace reached the store, and the replace is lost, so no
/// copy starts. The entry is the delete's.
#[tokio::test]
async fn the_hold_before_a_copy_loses_to_a_delete_that_amended_the_pending_put() {
    let rig = Rig::new();
    let (client, recon, d1) = (rig.actor("c1"), rig.actor("r1"), rig.actor("d1"));
    let pending = pending_put(&client).await;

    let gate = recon.gated.gate(Op::PutIfMatch, "", Phase::Pre);
    let mut holding = Box::pin(recon.entries.advance(&pending, Step::HoldForCopy));
    until_parked(&gate, &mut holding).await;
    let amended = step(&d1, &pending, Step::Amend).await;
    assert!(gate.release());

    assert_eq!(holding.await.unwrap(), Outcome::Lost);
    assert_eq!(read(&d1, "k").await, amended);
}

/// Asserts the race of a copy and a delete: the reconciler holds the put and copies, a delete
/// amends the held entry (and its step says it waits), the reconciler's seal after the copy is lost,
/// the delete hands over, and the reconciler then seals and releases what is now the delete's.
#[tokio::test]
async fn a_delete_amending_a_held_copy_makes_the_seal_after_the_copy_lose_and_waits_for_the_release()
 {
    let rig = Rig::new();
    let (client, recon, d1) = (rig.actor("c1"), rig.actor("r1"), rig.actor("d1"));
    let pending = pending_put(&client).await;
    let copying = step(&recon, &pending, Step::HoldForCopy).await;

    let seen = read(&d1, "k").await;
    assert_eq!(seen, copying);
    assert!(Step::AmendCopy.waits());
    let amended = step(&d1, &seen, Step::AmendCopy).await;

    let lost = recon.entries.advance(&copying, Step::Seal).await.unwrap();
    assert_eq!(lost, Outcome::Lost);
    assert_eq!(read(&recon, "k").await, amended);
    let refused = recon.entries.release(&amended).await.unwrap();
    assert_eq!(refused, Outcome::Refused);

    let handed = step(&d1, &amended, Step::Handover).await;
    let sealed = step(&recon, &handed, Step::Seal).await;
    assert_eq!(sealed.entry.op, EntryOp::Del);
    assert_eq!(
        recon.entries.release(&sealed).await.unwrap(),
        Outcome::Done(())
    );
    assert_eq!(d1.entries.read("k").await.unwrap(), None);
}

/// Asserts that of two retried deletes that both read a handed-over delete, the one whose claim
/// reaches the store second is lost, and the first holds the entry.
#[tokio::test]
async fn of_two_claims_of_a_handed_over_delete_the_second_to_reach_the_store_is_lost() {
    let rig = Rig::new();
    let (d1, d2, d3) = (rig.actor("d1"), rig.actor("d2"), rig.actor("d3"));
    let held = take(&d1, "k", Take::Del).await;
    let handed = step(&d1, &held, Step::Handover).await;

    let gate = d2.gated.gate(Op::PutIfMatch, "", Phase::Pre);
    let mut slow = Box::pin(d2.entries.advance(&handed, Step::Claim));
    until_parked(&gate, &mut slow).await;
    let won = step(&d3, &handed, Step::Claim).await;
    assert!(gate.release());

    assert_eq!(slow.await.unwrap(), Outcome::Lost);
    assert_eq!(read(&d2, "k").await, won);
}

/// Asserts that when the reconciler reaps a held put whose owner is thought departed while the owner
/// lands it, the owner's replace, which reaches the store second, is lost (the put is then answered
/// 503) and the entry is the reaper's pending one.
#[tokio::test]
async fn a_reap_that_reaches_the_store_before_the_owners_landing_makes_the_landing_lose() {
    let rig = Rig::new();
    let (client, recon) = (rig.actor("c1"), rig.actor("r1"));
    let held = take(&client, "k", Take::Put).await;
    let seen = read(&recon, "k").await;

    let gate = client.gated.gate(Op::PutIfMatch, "", Phase::Pre);
    let mut landing = Box::pin(client.entries.advance(&held, Step::LandPut));
    until_parked(&gate, &mut landing).await;
    let reaped = step(&recon, &seen, Step::Reap).await;
    assert!(gate.release());

    assert_eq!(landing.await.unwrap(), Outcome::Lost);
    assert_eq!(read(&client, "k").await, reaped);
    assert_eq!(reaped.entry.st, EntryState::Pending);
}

/// Asserts that a replace whose reply was lost after the store did it is done, with the entry read
/// back, and that its nonce is the one written.
#[tokio::test]
async fn a_replace_whose_reply_was_lost_after_it_landed_is_done() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let held = take(&a, "k", Take::Put).await;
    let gate = a.gated.gate(Op::PutIfMatch, "", Phase::Post);
    let mut landing = Box::pin(a.entries.advance(&held, Step::LandPut));
    until_parked(&gate, &mut landing).await;
    assert!(gate.release_with(Error::Unavailable("reply lost".to_owned())));
    let seen = done(landing.await.unwrap());
    assert_eq!(seen.entry.nonce, "a.1");
    assert_eq!(seen.entry.st, EntryState::Pending);
    assert_eq!(read(&a, "k").await, seen);
}

/// Asserts that a replace that failed ambiguously without reaching the store is lost, and so is one
/// that failed while another actor changed the entry: in neither is the entry the replace meant
/// there, and the entry stands.
#[tokio::test]
async fn a_replace_that_failed_ambiguously_and_did_not_land_is_lost() {
    for other_acts in [false, true] {
        let rig = Rig::new();
        let (a, d1) = (rig.actor("a"), rig.actor("d1"));
        let pending = pending_put(&a).await;
        let gate = a.gated.gate(Op::PutIfMatch, "", Phase::Pre);
        let mut holding = Box::pin(a.entries.advance(&pending, Step::HoldForCopy));
        until_parked(&gate, &mut holding).await;
        let standing = if other_acts {
            step(&d1, &pending, Step::Amend).await
        } else {
            pending.clone()
        };
        assert!(gate.release_with(Error::Other("reset".to_owned())));
        assert_eq!(holding.await.unwrap(), Outcome::Lost, "{other_acts}");
        assert_eq!(read(&a, "k").await, standing, "{other_acts}");
    }
}

/// Asserts that a replace that failed ambiguously and whose read-back fails too gives the original
/// error.
#[tokio::test]
async fn a_replace_that_failed_ambiguously_and_cannot_read_back_gives_its_error() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let held = take(&a, "k", Take::Put).await;
    let gate = a.gated.gate(Op::PutIfMatch, "", Phase::Post);
    let mut landing = Box::pin(a.entries.advance(&held, Step::LandPut));
    until_parked(&gate, &mut landing).await;
    rig.store
        .fail_next_with(Op::Head, Error::Unavailable("still down".to_owned()));
    assert!(gate.release_with(Error::Unavailable("reply lost".to_owned())));
    let error = landing.await.unwrap_err();
    assert_eq!(error, Error::Unavailable("reply lost".to_owned()));
}

// ---- release ----

/// Asserts, for every operation, state and owner, that an entry is released only if it is sealed or
/// a put or a delete the actor holds (never one in a state or with an operation a newer writer
/// added), that a refused release makes no store call, and that a released entry is gone.
#[tokio::test]
async fn an_entry_is_released_only_when_sealed_or_held_by_the_actor_as_a_put_or_a_delete() {
    for op in OPS {
        for st in STATES {
            for who in WHOS {
                let rig = Rig::new();
                let actor = rig.actor("me");
                seed(&rig, &entry(op, st, who.owner())).await;
                let seen = read(&actor, "k").await;
                let sealed = st == EntryState::Sealed && op != EntryOp::Unknown;
                let held = st == EntryState::Held && who == Who::Me;
                let held = held && matches!(op, EntryOp::Put | EntryOp::Del);
                let released = actor.entries.release(&seen).await.unwrap();
                let case = format!("{op:?} {st:?} {who:?}");
                if sealed || held {
                    assert_eq!(released, Outcome::Done(()), "{case}");
                    assert_eq!(actor.entries.read("k").await.unwrap(), None, "{case}");
                } else {
                    assert_eq!(released, Outcome::Refused, "{case}");
                    assert_eq!(rig.store.calls(Op::DeleteIfMatch), 0, "{case}");
                    assert_eq!(rig.store.calls(Op::Delete), 0, "{case}");
                    assert_eq!(read(&actor, "k").await, seen, "{case}");
                }
            }
        }
    }
}

/// Asserts that a release of an entry that has changed since it was read is lost and leaves the
/// entry, and that a release of an entry that is gone is lost.
#[tokio::test]
async fn a_release_on_a_stale_tag_or_of_a_gone_entry_is_lost() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let sealed = take(&a, "k", Take::Evict).await;
    let other = entry(EntryOp::Evict, EntryState::Sealed, None);
    let changed = Entry {
        nonce: "other".to_owned(),
        ..other
    };
    rig.memory
        .put_if_match(&entry_key("k"), encode_entry(&changed), &sealed.tag)
        .await
        .unwrap();
    assert_eq!(a.entries.release(&sealed).await.unwrap(), Outcome::Lost);
    assert_eq!(read(&a, "k").await.entry, changed);

    rig.memory.delete(&entry_key("k")).await.unwrap();
    assert_eq!(a.entries.release(&sealed).await.unwrap(), Outcome::Lost);
}

/// Asserts that a store that does not do a delete on the tag gets a plain delete, once, and the
/// entry is gone.
#[tokio::test]
async fn a_store_without_a_delete_on_the_tag_gets_a_plain_delete() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let sealed = take(&a, "k", Take::Evict).await;
    rig.store.fail_next_with(
        Op::DeleteIfMatch,
        Error::Unsupported("no conditional delete".to_owned()),
    );
    assert_eq!(a.entries.release(&sealed).await.unwrap(), Outcome::Done(()));
    assert_eq!(rig.store.calls(Op::DeleteIfMatch), 1);
    assert_eq!(rig.store.calls(Op::Delete), 1);
    assert_eq!(a.entries.read("k").await.unwrap(), None);
}

/// Asserts the residual of a store without a delete on the tag: a stale `Seen` of an entry that was
/// removed and created again deletes the new entry too, and answers `Done`. The tag cannot be
/// checked there; the exclusive-holder rule is what makes it safe.
#[tokio::test]
async fn a_stale_seen_on_a_store_without_a_delete_on_the_tag_removes_the_replacement() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let old = take(&a, "k", Take::Evict).await;
    assert_eq!(a.entries.release(&old).await.unwrap(), Outcome::Done(()));
    let _replacement = take(&a, "k", Take::Evict).await;
    rig.store.fail_next_with(
        Op::DeleteIfMatch,
        Error::Unsupported("no conditional delete".to_owned()),
    );
    assert_eq!(a.entries.release(&old).await.unwrap(), Outcome::Done(()));
    assert_eq!(a.entries.read("k").await.unwrap(), None);
}

/// Asserts that a delete the store refuses for a reason that says nothing was deleted is that error
/// and the entry stands.
#[tokio::test]
async fn a_delete_the_store_refuses_gives_its_error_and_the_entry_stands() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let sealed = take(&a, "k", Take::Evict).await;
    rig.store
        .fail_next_with(Op::DeleteIfMatch, Error::Full("odd".to_owned()));
    let error = a.entries.release(&sealed).await.unwrap_err();
    assert_eq!(error, Error::Full("odd".to_owned()));
    assert_eq!(read(&a, "k").await, sealed);
}

/// Asserts that a delete whose reply was lost after the store did it is done, because the read-back
/// finds no entry.
#[tokio::test]
async fn a_delete_whose_reply_was_lost_after_it_landed_is_done() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let sealed = take(&a, "k", Take::Evict).await;
    let gate = a.gated.gate(Op::DeleteIfMatch, "", Phase::Post);
    let mut releasing = Box::pin(a.entries.release(&sealed));
    until_parked(&gate, &mut releasing).await;
    assert!(gate.release_with(Error::Unavailable("reply lost".to_owned())));
    assert_eq!(releasing.await.unwrap(), Outcome::Done(()));
    assert_eq!(a.entries.read("k").await.unwrap(), None);
}

/// Asserts that a delete that failed before the store, with the same entry still there, gives the
/// original error, and that one that failed while another entry replaced it is lost.
#[tokio::test]
async fn a_delete_that_failed_ambiguously_is_an_error_if_the_entry_is_there_and_lost_if_replaced() {
    let rig = Rig::new();
    let (a, b) = (rig.actor("a"), rig.actor("b"));
    let sealed = take(&a, "k", Take::Evict).await;
    let gate = a.gated.gate(Op::DeleteIfMatch, "", Phase::Pre);
    let mut releasing = Box::pin(a.entries.release(&sealed));
    until_parked(&gate, &mut releasing).await;
    assert!(gate.release_with(Error::Unavailable("timed out".to_owned())));
    let error = releasing.await.unwrap_err();
    assert_eq!(error, Error::Unavailable("timed out".to_owned()));
    assert_eq!(read(&a, "k").await, sealed);

    let gate = a.gated.gate(Op::DeleteIfMatch, "", Phase::Pre);
    let mut releasing = Box::pin(a.entries.release(&sealed));
    until_parked(&gate, &mut releasing).await;
    rig.memory.delete(&entry_key("k")).await.unwrap();
    let theirs = take(&b, "k", Take::Evict).await;
    assert!(gate.release_with(Error::Other("reset".to_owned())));
    assert_eq!(releasing.await.unwrap(), Outcome::Lost);
    assert_eq!(read(&a, "k").await, theirs);
}

/// Asserts that a delete that failed ambiguously and whose read-back fails too gives the original
/// error.
#[tokio::test]
async fn a_delete_that_failed_ambiguously_and_cannot_read_back_gives_its_error() {
    let rig = Rig::new();
    let a = rig.actor("a");
    let sealed = take(&a, "k", Take::Evict).await;
    let gate = a.gated.gate(Op::DeleteIfMatch, "", Phase::Post);
    let mut releasing = Box::pin(a.entries.release(&sealed));
    until_parked(&gate, &mut releasing).await;
    rig.store
        .fail_next_with(Op::Head, Error::Unavailable("still down".to_owned()));
    assert!(gate.release_with(Error::Unavailable("reply lost".to_owned())));
    let error = releasing.await.unwrap_err();
    assert_eq!(error, Error::Unavailable("reply lost".to_owned()));
}
