//! Lifelines (5.1; section 14 row "Mount killed (lifeline hangup): counters dropped, eviction
//! proceeds; mount whose protocol connection flaps keeps counters, regions and holds").
//!
//! Catches: a holder ended while its process lives (on data or an end of stream it sends, or on a
//! reconnect); a holder that is never ended after its process is gone (its stuck counter would
//! block eviction for ever); a re-handshake that replaces the counters or the lifeline, or takes a
//! second hold; a reap that forgets a lifeline but leaves the holder's counters, regions or holds,
//! or that reports a holder twice.

use std::io::Write;
use std::net::Shutdown;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use tierstore_fetch_api::cell::CellState;
use tierstore_fetch_api::entry::{Entry, EntryError, EntryStore, EvictStep};
use tierstore_fetch_api::ids::HolderId;
use tierstore_fetch_api::page::CounterRegion;

use super::{MIB, MountCounters, open, pack, publish, run, scratch, state};
use crate::{FileEntryStore, Lifeline, Lifelines, MemfdCounters};

/// A lifeline pair: the server's end and the mount's end.
fn pair() -> (Lifeline, UnixStream) {
    let (server, mount) = UnixStream::pair().unwrap();
    (Lifeline::new(OwnedFd::from(server)), mount)
}

#[test]
fn a_lifeline_hangs_up_only_when_the_mounts_end_is_closed() {
    let (line, mut mount) = pair();
    assert!(!line.hung_up().unwrap());
    // Data on the lifeline is not a hangup, and is never read.
    mount.write_all(b"x").unwrap();
    assert!(!line.hung_up().unwrap());
    // Nor is an end of stream from a mount that still holds its end.
    mount.shutdown(Shutdown::Write).unwrap();
    assert!(!line.hung_up().unwrap());
    drop(mount);
    assert!(line.hung_up().unwrap());
    assert!(line.as_fd().try_clone_to_owned().is_ok());
}

#[test]
fn a_second_lifeline_for_a_holder_is_dropped_and_the_first_kept() {
    let lines = Lifelines::new();
    let (first, _first_mount) = pair();
    let (second, second_mount) = pair();
    assert!(lines.add(HolderId(4), first));
    assert!(!lines.add(HolderId(4), second));
    assert_eq!(lines.holders(), vec![HolderId(4)]);
    // The second was dropped: its mount's end sees the hangup.
    let mount_side = Lifeline::new(OwnedFd::from(second_mount));
    assert!(mount_side.hung_up().unwrap());
}

#[test]
fn a_killed_mount_is_reaped_its_counters_dropped_and_eviction_proceeds() {
    let dir = scratch("lifeline-kill");
    let (store, entry) = open(&dir, "p", 20 * MIB);
    publish(&entry, 17, 1);
    let mount = MountCounters::new(4096);
    let counters = Arc::new(MemfdCounters::new(mount.fd()).unwrap());
    store.register_holder(HolderId(7), counters).unwrap();
    store
        .attach(
            &pack("p"),
            HolderId(7),
            CounterRegion {
                offset: 0,
                groups: 2,
            },
        )
        .unwrap();
    let lines = Lifelines::new();
    let (line, mount_end) = pair();
    assert!(lines.add(HolderId(7), line));
    // The mount is reading group 1 when it is killed: its counter stays raised.
    mount.enter(2);
    assert!(lines.reap(&store).unwrap().is_empty());
    assert!(entry.evict_cell_cas(17, EvictStep::Begin));
    assert_eq!(entry.group_readers(1), 1);
    assert!(entry.evict_cell_cas(17, EvictStep::Abort));
    assert_eq!(entry.holders(), 1);
    // The process dies: its end of the lifeline closes.
    drop(mount_end);
    assert_eq!(lines.reap(&store).unwrap(), vec![HolderId(7)]);
    assert!(lines.holders().is_empty());
    assert_eq!(entry.holders(), 0);
    // The stuck counter is no longer summed: eviction proceeds.
    assert_eq!(entry.group_readers(1), 0);
    assert!(entry.evict_cell_cas(17, EvictStep::Begin));
    entry.punch(&[run(17, 1)]).unwrap();
    assert!(entry.evict_cell_cas(17, EvictStep::Finish));
    assert_eq!(state(&entry, 17), CellState::Absent);
    // The holder is gone: it cannot attach again, and a second reap reports nothing.
    let err = store
        .attach(
            &pack("p"),
            HolderId(7),
            CounterRegion {
                offset: 0,
                groups: 2,
            },
        )
        .unwrap_err();
    assert!(
        matches!(
            err,
            EntryError::UnknownHolder {
                holder: HolderId(7)
            }
        ),
        "{err}"
    );
    assert!(lines.reap(&store).unwrap().is_empty());
}

#[test]
fn a_mount_whose_protocol_connection_flaps_keeps_its_counters_regions_and_holds() {
    let dir = scratch("lifeline-flap");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let entry = store.open(&super::spec("p", 3 * MIB)).unwrap();
    let mount = MountCounters::new(4096);
    let first = Arc::new(MemfdCounters::new(mount.fd()).unwrap());
    store.register_holder(HolderId(2), first.clone()).unwrap();
    let region = CounterRegion {
        offset: 8,
        groups: 1,
    };
    store.attach(&pack("p"), HolderId(2), region).unwrap();
    let lines = Lifelines::new();
    let (line, _mount_end) = pair();
    lines.add(HolderId(2), line);
    mount.enter(8);
    // Reconnect: the mount re-handshakes with the same holder id and re-sends open-pack.
    let other = MountCounters::new(4096);
    let second = Arc::new(MemfdCounters::new(other.fd()).unwrap());
    store.register_holder(HolderId(2), second.clone()).unwrap();
    store.attach(&pack("p"), HolderId(2), region).unwrap();
    let (line_again, _again_end) = pair();
    assert!(!lines.add(HolderId(2), line_again));
    // The first counters are still the ones read; one hold, not two; nothing was reaped.
    assert_eq!(entry.group_readers(0), 1);
    assert_eq!(second.maps(), 0);
    assert_eq!(entry.holders(), 1);
    // A pack opened after the reconnect is read from the first memfd too, remapped as it grew.
    let mut mount = mount;
    let q = store.open(&super::spec("q", 3 * MIB)).unwrap();
    mount.grow(2 * 4096);
    let region_q = CounterRegion {
        offset: 4096,
        groups: 1,
    };
    store.attach(&pack("q"), HolderId(2), region_q).unwrap();
    mount.enter(4096);
    mount.enter(4096);
    other.enter(4096 - 8);
    assert_eq!(q.group_readers(0), 2);
    assert_eq!(first.maps(), 2);
    assert_eq!(second.maps(), 0);
    assert!(lines.reap(&store).unwrap().is_empty());
    assert!(!store.remove(&pack("p")).unwrap());
}

#[test]
fn a_reap_with_no_lifeline_ends_nothing() {
    let dir = scratch("lifeline-none");
    let store = FileEntryStore::new(dir.path()).unwrap();
    assert!(Lifelines::new().reap(&store).unwrap().is_empty());
}

#[test]
fn a_reap_ends_only_the_holders_that_hung_up_in_ascending_order() {
    let dir = scratch("lifeline-some");
    let store = FileEntryStore::new(dir.path()).unwrap();
    let lines = Lifelines::new();
    let mut alive = Vec::new();
    let mut dead = Vec::new();
    for h in 0..6 {
        let (line, mount) = pair();
        lines.add(HolderId(h), line);
        if h % 2 == 1 {
            dead.push(mount);
        } else {
            alive.push(mount);
        }
    }
    drop(dead);
    assert_eq!(
        lines.reap(&store).unwrap(),
        vec![HolderId(1), HolderId(3), HolderId(5)]
    );
    assert_eq!(lines.holders(), vec![HolderId(0), HolderId(2), HolderId(4)]);
}
