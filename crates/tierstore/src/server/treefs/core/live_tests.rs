//! The announcement of a start (`pins-live`, `places-live`): sent by a thread of its own, asked
//! again until the fill socket answers, before every request that changes what the node holds
//! for the mount, and never by a process whose successor is taking over.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use super::super::super::fill_client::{AskError, FillClient};
use super::super::super::table::TreeTable;
use super::super::{Core, Errno};
use crate::server::treefs::ino;
use crate::server::treefs::testing::{FakeFill, Rig, sample, until};

/// The verbs of the requests that make or end what the node holds for the mount, in the order the
/// rig's fill socket received them.
fn holds_asked(rig: &Rig) -> Vec<String> {
    rig.upstream
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter_map(|line| line.split(' ').next())
        .filter(|verb| ["pins-live", "places-live", "place", "pin"].contains(verb))
        .map(str::to_owned)
        .collect()
}

/// A core of the rig's mount whose fill socket is `path`, with no pause between tries but
/// `pause`, which the client takes between two tries of the announcement.
fn core_over(rig: &Rig, path: &Path, pause: Arc<dyn Fn(Duration) + Send + Sync>) -> Arc<Core> {
    let fills = FillClient::pausing(path.to_owned(), 0, Duration::from_secs(30), pause);
    Arc::new(Core::new(
        rig.core.settings().clone(),
        fills,
        TreeTable::open(&rig.runtime).unwrap(),
        &rig.runtime,
    ))
}

/// Asserts that the thread of a start sends the announcement, and that a lookup that asks the node
/// for a placement while the announcement is unanswered waits for it: the node receives
/// `pins-live`, then `places-live`, then the `place`, never the `place` before. Catches a hold
/// that a late announcement would end.
#[test]
fn a_place_waits_for_the_announcement_the_thread_sends() {
    let rig = Rig::new("tfs-live-order", sample());
    rig.upstream.gate_verb("pins-live");
    rig.core.tell_live_later().unwrap();
    rig.upstream.wait_for_verb("pins-live", 1);
    let core = Arc::clone(&rig.core);
    let lookup = std::thread::spawn(move || core.lookup(ino::ROOT, "jobs"));
    rig.upstream.permit_verb("pins-live", 1);
    assert_eq!(lookup.join().unwrap(), Err(Errno::NoEnt));
    assert_eq!(holds_asked(&rig), ["pins-live", "places-live", "place"]);
    rig.core.settled().unwrap();
    assert_eq!(rig.upstream.asked("pins-live").len(), 1, "sent once");
}

/// Asserts that the thread asks again while the fill socket is not there, resting between tries,
/// and sends the announcement once a socket is bound: one `pins-live` and one `places-live` reach
/// it, after the third rest.
#[test]
fn the_thread_asks_again_until_a_fill_socket_is_bound() {
    let rig = Rig::new("tfs-live-later", sample());
    let path = rig.dir.path().join("late.sock");
    let rests = Arc::new(AtomicUsize::new(0));
    let bound: Arc<Mutex<Option<FakeFill>>> = Arc::new(Mutex::new(None));
    let pause = {
        let (rests, bound, path) = (Arc::clone(&rests), Arc::clone(&bound), path.clone());
        let upstream = Arc::clone(&rig.upstream);
        Arc::new(move |_: Duration| {
            if rests.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
                *bound.lock().unwrap() = Some(FakeFill::start(&path, Arc::clone(&upstream)));
            }
        })
    };
    let core = core_over(&rig, &path, pause);
    core.tell_live_later().unwrap();
    until(|| !rig.upstream.asked("places-live").is_empty());
    core.settled().unwrap();
    assert_eq!(rests.load(Ordering::SeqCst), 3);
    assert_eq!(rig.upstream.asked("pins-live"), ["pins-live mount-1 "]);
    assert_eq!(rig.upstream.asked("places-live"), ["places-live mount-1 "]);
    drop(core);
    drop(bound.lock().unwrap().take());
}

/// Asserts that a request that changes a hold, while the fill socket is not there, fails as it
/// does with no announcement owed (the lookup of a placement is `EIO`, the announcement
/// `Unavailable`), and that nothing reaches a socket bound at another path.
#[test]
fn a_request_that_finds_no_fill_socket_fails_and_the_announcement_stays_owed() {
    let rig = Rig::new("tfs-live-away", sample());
    let (rest, resting) = mpsc::channel::<()>();
    let resting = Mutex::new(resting);
    let entered = Arc::new(AtomicUsize::new(0));
    let pause = {
        let entered = Arc::clone(&entered);
        Arc::new(move |_: Duration| {
            let _ = entered.fetch_add(1, Ordering::SeqCst);
            // The thread rests until the test ends.
            let _ = resting.lock().unwrap().recv();
        })
    };
    let core = core_over(&rig, &rig.dir.path().join("absent.sock"), pause);
    core.tell_live_later().unwrap();
    until(|| entered.load(Ordering::SeqCst) == 1);
    assert_eq!(core.lookup(ino::ROOT, "jobs"), Err(Errno::Io));
    assert_eq!(core.settled(), Err(AskError::Unavailable));
    assert!(holds_asked(&rig).is_empty());
    drop(core);
    drop(rest);
}

/// Asserts that a core whose successor is taking over sends no announcement, and that its thread
/// ends without one: `settled` is `Unavailable` while the hold is on. A hand-over that did not
/// complete lets the hold go, and the first request that needs it sends the announcement.
#[test]
fn a_held_core_sends_no_announcement_until_the_hold_is_let_go() {
    let rig = Rig::new("tfs-live-held", sample());
    rig.core.hold(true);
    rig.core.tell_live_later().unwrap();
    assert_eq!(rig.core.settled(), Err(AskError::Unavailable));
    assert!(holds_asked(&rig).is_empty());
    rig.core.hold(false);
    rig.core.settled().unwrap();
    rig.core.settled().unwrap();
    assert_eq!(holds_asked(&rig), ["pins-live", "places-live"]);
}

/// Asserts that a hold waits for an announcement that is being sent: once the hold is on, the
/// announcement has been answered, so no `pins-live` of this process follows the successor's.
#[test]
fn a_hold_waits_for_the_announcement_being_sent() {
    let rig = Rig::new("tfs-live-hold-waits", sample());
    rig.upstream.gate_verb("pins-live");
    rig.core.tell_live_later().unwrap();
    rig.upstream.wait_for_verb("pins-live", 1);
    let core = Arc::clone(&rig.core);
    let hold = std::thread::spawn(move || core.hold(true));
    rig.upstream.permit_verb("pins-live", 1);
    hold.join().unwrap();
    assert_eq!(holds_asked(&rig), ["pins-live", "places-live"]);
    assert_eq!(rig.core.settled(), Ok(()));
}
