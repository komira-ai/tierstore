//! What the leader has seen of the registrations, and the removal of the ones whose owner left.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::faulty::{FaultyBackend, Op};
use crate::journal::{journal_key, journal_prefix, proxy_key};
use crate::registration::{PROTO, Registration};
use crate::registrations::{Registrations, remove_if_departed};
use crate::{Backend, Error, GateBackend, Listed, MemoryBackend, Phase};

const WAIT: Duration = Duration::from_secs(600);

fn listed(id: &str, size: u64, modified: u64) -> Listed {
    Listed {
        key: proxy_key(id),
        size,
        modified,
    }
}

/// Asserts that a registration seen again as it was keeps the time it was first seen, a changed
/// one (another time of the last write, or another size) is first seen now, and the ids that are
/// due are exactly those unchanged for the wait: at the wait itself, not a millisecond before.
#[test]
fn an_unchanged_registration_keeps_its_first_seen_time_and_a_changed_one_starts_again() {
    let mut seen = Registrations::default();
    seen.observe(1_000, &[listed("a", 10, 5), listed("b", 10, 5)]);
    seen.observe(300_000, &[listed("a", 10, 5), listed("b", 10, 6)]);
    let at_wait = 1_000 + 600_000;
    assert_eq!(seen.due(at_wait - 1, WAIT), Vec::<String>::new());
    assert_eq!(seen.due(at_wait, WAIT), ["a"]);
    assert_eq!(seen.due(300_000 + 600_000, WAIT), ["a", "b"]);

    seen.observe(at_wait, &[listed("a", 11, 5), listed("b", 10, 6)]);
    assert_eq!(seen.due(at_wait, WAIT), Vec::<String>::new());
    assert_eq!(seen.due(900_000, WAIT), ["b"]);
}

/// Asserts that a registration that is no longer listed is forgotten, so that one that comes
/// back is first seen when it does, and that an object outside the prefix is not a registration.
#[test]
fn a_registration_that_leaves_the_listing_is_forgotten_and_one_outside_the_prefix_is_ignored() {
    let mut seen = Registrations::default();
    let stray = Listed {
        key: "_tiering/usage".into(),
        size: 1,
        modified: 1,
    };
    seen.observe(0, &[listed("a", 1, 1), stray]);
    seen.observe(100, &[]);
    seen.observe(900_000, &[listed("a", 1, 1)]);
    assert_eq!(seen.due(900_000, WAIT), Vec::<String>::new());
    assert_eq!(seen.due(1_500_000, WAIT), ["a"]);
    assert_eq!(seen.due(1_500_000, Duration::ZERO), ["a"]);
}

/// Asserts that a registration that was asked about is not due again for the wait, that asking
/// about an id that is not known does nothing, and that a change of the registration clears the
/// note so that it is asked about once it has been unchanged again for the wait.
#[test]
fn a_registration_asked_about_is_due_again_after_the_wait_and_a_change_clears_the_note() {
    let mut seen = Registrations::default();
    seen.observe(0, &[listed("a", 1, 1)]);
    seen.ask("nobody", 700_000);
    assert_eq!(seen.due(700_000, WAIT), ["a"]);
    seen.ask("a", 700_000);
    assert_eq!(seen.due(700_001, WAIT), Vec::<String>::new());
    seen.observe(1_000_000, &[listed("a", 1, 1)]);
    assert_eq!(seen.due(1_299_999, WAIT), Vec::<String>::new());
    assert_eq!(seen.due(1_300_000, WAIT), ["a"]);

    seen.observe(1_300_000, &[listed("a", 1, 2)]);
    assert_eq!(seen.due(1_900_000, WAIT), ["a"]);
}

async fn hot_with(id: &str, body: Bytes) -> Arc<MemoryBackend> {
    let hot = Arc::new(MemoryBackend::new());
    hot.put(&proxy_key(id), body).await.unwrap();
    hot
}

fn beating() -> Bytes {
    Registration {
        proto: PROTO,
        boot: "b".into(),
        hb: 3,
    }
    .encode()
}

/// The version of the registration `p` as a leader that listed it would have noted it.
async fn seen_as(hot: &MemoryBackend) -> (u64, u64) {
    let meta = hot.head(&proxy_key("p")).await.unwrap();
    (meta.size, meta.modified)
}

async fn is_there(hot: &MemoryBackend, id: &str) -> bool {
    hot.head(&proxy_key(id)).await.is_ok()
}

/// Asserts that the registration of a process that beat and left no journal object is removed.
#[tokio::test]
async fn the_registration_of_a_process_that_beat_and_left_no_journal_is_removed() {
    let hot = hot_with("p", beating()).await;
    assert_eq!(
        remove_if_departed(hot.as_ref(), "p", seen_as(&hot).await).await,
        Ok(true)
    );
    assert!(!is_there(&hot, "p").await);
}

/// Asserts that a registration is kept when its owner never beat (the empty one of an older
/// release, the body of a proxy that did not start the heartbeat, a body that is not a
/// registration), and when a journal object of the proxy remains.
#[tokio::test]
async fn a_registration_of_a_process_that_never_beat_or_that_has_a_journal_is_kept() {
    let never = Registration {
        proto: PROTO,
        boot: "b".into(),
        hb: 0,
    };
    for body in [Bytes::new(), never.encode(), Bytes::from_static(b"junk")] {
        let hot = hot_with("p", body).await;
        assert_eq!(
            remove_if_departed(hot.as_ref(), "p", seen_as(&hot).await).await,
            Ok(false)
        );
        assert!(is_there(&hot, "p").await);
    }

    let hot = hot_with("p", beating()).await;
    hot.put(&journal_key("p", 1), Bytes::from_static(b"[]"))
        .await
        .unwrap();
    assert_eq!(
        remove_if_departed(hot.as_ref(), "p", seen_as(&hot).await).await,
        Ok(false)
    );
    assert!(is_there(&hot, "p").await);
}

/// Asserts that a registration that is gone by the time it is read is not an error and not a
/// removal.
#[tokio::test]
async fn a_registration_that_is_already_gone_is_not_removed_again() {
    let hot = MemoryBackend::new();
    assert_eq!(remove_if_departed(&hot, "p", (0, 0)).await, Ok(false));
}

/// Asserts that the errors of the heads, the read, the listing of the journal and of the delete are given
/// to the caller, and that the registration stays when the delete failed.
#[tokio::test]
async fn the_errors_of_the_store_are_given_and_the_registration_stays() {
    for op in [Op::Head, Op::Get, Op::List, Op::DeleteIfMatch] {
        let memory = hot_with("p", beating()).await;
        let seen = seen_as(&memory).await;
        let hot = FaultyBackend::new(memory.clone());
        hot.fail_next(op, 1);
        let answer = remove_if_departed(&hot, "p", seen).await;
        assert!(
            matches!(answer, Err(Error::Unavailable(_))),
            "{op:?}: {answer:?}"
        );
        assert!(is_there(&memory, "p").await, "{op:?}");
    }
}

/// Asserts that a registration that changes between the read and the delete is kept: a new
/// process of the same proxy id opens over it (its body is the zero count, with another tag) while
/// the cleanup is held at the listing of the journal, and the registration is still there, as the
/// new process wrote it, once the cleanup goes on. One that is deleted in between is no error.
#[tokio::test]
async fn a_registration_replaced_or_removed_after_it_was_read_is_kept_or_not_an_error() {
    let memory = hot_with("p", beating()).await;
    let seen = seen_as(&memory).await;
    let gated = GateBackend::new(memory.clone());
    let held = gated.gate(Op::List, &journal_prefix("p"), Phase::Pre);
    let reopened = Registration {
        proto: PROTO,
        boot: "new".into(),
        hb: 0,
    };
    let tag = memory.head(&proxy_key("p")).await.unwrap().tag;
    let cleanup = remove_if_departed(&gated, "p", seen);
    let reopen = async {
        held.parked().await;
        memory
            .put_if_match(&proxy_key("p"), reopened.encode(), &tag)
            .await
            .unwrap();
        assert!(held.release());
    };
    let (answer, ()) = tokio::join!(cleanup, reopen);
    assert_eq!(answer, Ok(false));
    let kept = memory.get(&proxy_key("p"), None).await.unwrap();
    assert_eq!(Registration::decode(&kept), Some(reopened));

    let memory = hot_with("p", beating()).await;
    let seen = seen_as(&memory).await;
    let gated = GateBackend::new(memory.clone());
    let held = gated.gate(Op::List, &journal_prefix("p"), Phase::Pre);
    let cleanup = remove_if_departed(&gated, "p", seen);
    let remove = async {
        held.parked().await;
        memory.delete(&proxy_key("p")).await.unwrap();
        assert!(held.release());
    };
    let (answer, ()) = tokio::join!(cleanup, remove);
    assert_eq!(answer, Ok(false));
}

/// Asserts that a registration written after the leader last saw it, and before the cleanup looks
/// at it, is kept: a beat that rewrites it with the same size at a later store time, and one that
/// changes its size, each make the version the leader counted from not the registration's any
/// more, so the cleanup deletes nothing and the next look of the leader starts the count again.
#[tokio::test]
async fn a_registration_written_after_the_leader_saw_it_is_not_removed() {
    let clock = Arc::new(crate::TestClock::new());
    let memory = Arc::new(MemoryBackend::with_clock(clock.clone()));
    for later in [
        beating(),
        Bytes::from(format!("{} ", String::from_utf8_lossy(&beating()))),
    ] {
        memory.put(&proxy_key("p"), beating()).await.unwrap();
        let seen = seen_as(&memory).await;
        clock.advance(1_000);
        let tag = memory.head(&proxy_key("p")).await.unwrap().tag;
        memory
            .put_if_match(&proxy_key("p"), later, &tag)
            .await
            .unwrap();
        assert_eq!(
            remove_if_departed(memory.as_ref(), "p", seen).await,
            Ok(false)
        );
        assert!(is_there(&memory, "p").await);
        memory.delete(&proxy_key("p")).await.unwrap();
    }
}

/// Asserts that a registration that is deleted between the head and the read is not an error and
/// not a removal.
#[tokio::test]
async fn a_registration_deleted_between_the_head_and_the_read_is_not_removed() {
    let memory = hot_with("p", beating()).await;
    let seen = seen_as(&memory).await;
    let gated = GateBackend::new(memory.clone());
    let held = gated.gate(Op::Get, &proxy_key("p"), Phase::Pre);
    let cleanup = remove_if_departed(&gated, "p", seen);
    let remove = async {
        held.parked().await;
        memory.delete(&proxy_key("p")).await.unwrap();
        assert!(held.release());
    };
    let (answer, ()) = tokio::join!(cleanup, remove);
    assert_eq!(answer, Ok(false));
}
