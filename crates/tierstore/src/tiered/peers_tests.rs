//! The boots a process names as possibly alive: when one is named, when it is named again, when
//! it is dropped. The beats are counted, never timed.

use super::peers::Peers;
use crate::registration::{PROTO, Registration};

fn found(boot: &str, peers: Option<&[&str]>) -> Registration {
    Registration {
        proto: PROTO,
        boot: boot.to_owned(),
        hb: 5,
        peers: peers.map(|boots| boots.iter().map(|boot| (*boot).to_owned()).collect()),
    }
}

fn beats(peers: &mut Peers, count: u32) {
    for _ in 0..count {
        peers.beat();
    }
}

/// Asserts that a boot that was heard is named for exactly `keep` acknowledged beats after it, and
/// not one fewer, and that a keep of zero is taken as one.
#[test]
fn a_heard_boot_is_named_for_the_keep_beats_after_it() {
    let mut peers = Peers::new(3);
    peers.heard("me", &found("a", Some(&[])));
    assert_eq!(peers.names(), ["a"]);
    beats(&mut peers, 2);
    assert_eq!(peers.names(), ["a"]);
    peers.beat();
    assert!(peers.names().is_empty());

    let mut least = Peers::new(0);
    least.heard("me", &found("a", None));
    assert_eq!(least.names(), ["a"]);
    least.beat();
    assert!(least.names().is_empty());
}

/// Asserts that hearing a boot again starts its count again, so that a process that goes on
/// writing is named for as long as it does.
#[test]
fn hearing_a_boot_again_starts_its_count_again() {
    let mut peers = Peers::new(3);
    peers.heard("me", &found("a", None));
    beats(&mut peers, 2);
    peers.heard("me", &found("a", None));
    beats(&mut peers, 2);
    assert_eq!(peers.names(), ["a"]);
    peers.beat();
    assert!(peers.names().is_empty());
}

/// Asserts that a boot that was only told (found at open, or named by a body that was heard) is
/// named once for the keep beats and is not renewed by being named again, so that two processes
/// that name each other's dead predecessor do not keep it for ever; a boot that is heard is
/// renewed and its peers are told.
#[test]
fn a_boot_that_was_only_told_is_not_renewed_by_being_told_again() {
    let mut peers = Peers::new(3);
    peers.found_at_open("me", &found("old", Some(&["older"])));
    assert_eq!(peers.names(), ["old", "older"]);
    beats(&mut peers, 2);
    peers.heard("me", &found("other", Some(&["old", "new"])));
    assert_eq!(peers.names(), ["new", "old", "older", "other"]);
    peers.beat();
    assert_eq!(peers.names(), ["new", "other"]);
    beats(&mut peers, 2);
    assert!(peers.names().is_empty());
}

/// Asserts that a process never names itself: not as the boot of a registration it finds, nor as
/// a peer in one.
#[test]
fn a_process_never_names_its_own_boot() {
    let mut peers = Peers::new(3);
    peers.found_at_open("me", &found("me", Some(&["me", "a"])));
    peers.heard("me", &found("me", Some(&["me"])));
    assert_eq!(peers.names(), ["a"]);
}
