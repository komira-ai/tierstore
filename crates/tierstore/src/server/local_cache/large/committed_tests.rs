//! The entry store of the read path ([`Committed`]): every call of the store and of its entries
//! is passed to what they wrap and gives what it gives, and each publish that succeeds is told
//! to the committer, which wakes its loop once the pending bytes are reached.

use std::sync::Arc;
use std::time::Duration;

use tierstore_fetch_api::entry::{EntrySpec, EntryStore, EvictStep, Settle};
use tierstore_fetch_api::fake::{MemCounters, MemEntryStore};
use tierstore_fetch_api::ids::{CellRun, HolderId, PackId, PackKey, PackRoot};
use tierstore_fetch_api::page::CounterRegion;
use tierstore_format::ids::Generation;
use tierstore_journal::{CommitPolicy, Committer};

use super::committed::{Committed, Wake, Woken};

/// One cell.
const MIB: usize = 1 << 20;

fn spec(cells: u32) -> EntrySpec {
    EntrySpec {
        pack: PackId {
            key: PackKey("cas/big".to_owned()),
            root: PackRoot::Etag(Generation([3; 16])),
        },
        size: u64::from(cells) * MIB as u64,
    }
}

/// A committer, a wake and a store over a new in-memory store, with the publishes told.
fn committed() -> (Committed<dyn EntryStore>, Arc<Committer>, Arc<Wake>) {
    let committer = Arc::new(Committer::new(CommitPolicy::default()));
    let wake = Arc::new(Wake::default());
    let inner: Arc<dyn EntryStore> = Arc::new(MemEntryStore::new());
    let store = Committed::new(inner, committer.clone(), wake.clone());
    (store, committer, wake)
}

/// Runs every call of the store and of an entry it opens, in an order in which each changes
/// what the next sees, and writes down what each gave.
fn drive(store: &dyn EntryStore) -> Vec<String> {
    let mut seen = Vec::new();
    let spec = spec(2);
    let pack = spec.pack.clone();
    let entry = store.open(&spec).unwrap();
    seen.push(format!("{:?} {}", entry.pack(), entry.size()));
    seen.push(format!("{:?}", store.get(&pack).map(|e| e.size())));
    seen.push(format!("{:?}", store.packs()));
    let mut won = entry.claim(CellRun { first: 0, count: 2 }).into_iter();
    let (first, second) = (won.next().unwrap(), won.next().unwrap());
    seen.push(format!("{:?}", entry.publish(first, &vec![7; MIB])));
    entry.abandon(second);
    seen.push(format!(
        "{:?} {:?}",
        entry.state().load(0),
        entry.state().load(1)
    ));
    seen.push(format!("{:?}", entry.read_cell(0).map(|bytes| bytes.len())));
    let again = entry.claim(CellRun { first: 1, count: 1 }).pop().unwrap();
    entry.mark_bad(again);
    seen.push(format!("{:?}", entry.counts()));
    seen.push(format!("{}", entry.settle(1, Settle::ExpireBad)));
    seen.push(format!("{}", entry.evict_cell_cas(0, EvictStep::Begin)));
    seen.push(format!("{}", entry.group_readers(0)));
    seen.push(format!(
        "{:?}",
        entry.punch(&[CellRun { first: 0, count: 1 }])
    ));
    seen.push(format!("{}", entry.evict_cell_cas(0, EvictStep::Finish)));
    entry.touch(5_000, 0);
    seen.push(format!("{} {:?}", entry.last_use(), entry.touch_epoch(0)));
    seen.push(format!("{}", entry.fds().is_some()));
    entry.hold(HolderId(3));
    seen.push(format!("{}", entry.holders()));
    seen.push(format!(
        "{} {}",
        entry.release(HolderId(3)),
        entry.holders()
    ));
    seen.push(format!("{:?}", entry.commit_batch()));
    seen.push(format!("{:?}", store.entries()));
    let counters = Arc::new(MemCounters::new(64));
    seen.push(format!(
        "{:?}",
        store.register_holder(HolderId(4), counters)
    ));
    let region = CounterRegion {
        offset: 0,
        groups: 1,
    };
    seen.push(format!("{:?}", store.attach(&pack, HolderId(4), region)));
    seen.push(format!("{}", store.drop_holder(HolderId(4))));
    seen.push(format!("{:?}", store.remove(&pack)));
    seen.push(format!("{:?}", store.get(&pack).is_some()));
    seen
}

/// Asserts that the same calls on the store and on a bare in-memory store give the same answers
/// one by one: open, get, packs, claim, publish, abandon, the state, a read, mark bad, settle,
/// the evictor's steps and its punch, the readers, touch, the descriptors, holds, a commit,
/// the entries, the holders' calls and a removal.
#[test]
fn every_call_is_passed_to_what_it_wraps_and_gives_what_it_gives() {
    let bare = MemEntryStore::new();
    let (store, _, _) = committed();
    let wrapped = drive(&store);
    assert_eq!(wrapped, drive(&bare));
    assert_eq!(wrapped.len(), 23);
}

/// Asserts that a publish is told to the committer, and that one that fails (bytes of another
/// length than the cell's) is not.
#[test]
fn a_publish_is_told_to_the_committer_and_a_failed_one_is_not() {
    let (store, committer, _) = committed();
    let entry = store.open(&spec(2)).unwrap();
    let mut won = entry.claim(CellRun { first: 0, count: 2 }).into_iter();
    let (first, second) = (won.next().unwrap(), won.next().unwrap());
    assert!(entry.publish(first, &[1; 10]).is_err());
    assert_eq!(committer.dirty(), 0);
    entry.publish(second, &vec![1; MIB]).unwrap();
    assert_eq!(committer.dirty(), 1);
}

/// Asserts that the loop is not woken while the pending bytes are under the committer's 32 MiB,
/// and is woken by the publish that reaches them; the wake is taken by the wait that sees it.
#[test]
fn the_publish_that_reaches_the_pending_bytes_wakes_the_committers_loop() {
    let (store, _, wake) = committed();
    let entry = store.open(&spec(32)).unwrap();
    let won = entry.claim(CellRun {
        first: 0,
        count: 32,
    });
    let mut cells = won.into_iter();
    for cell in cells.by_ref().take(31) {
        entry.publish(cell, &vec![2; MIB]).unwrap();
    }
    assert_eq!(wake.wait(Duration::ZERO), Woken::default());
    entry.publish(cells.next().unwrap(), &vec![2; MIB]).unwrap();
    let woken = Woken {
        commit: true,
        stop: false,
    };
    assert_eq!(wake.wait(Duration::ZERO), woken);
    assert_eq!(wake.wait(Duration::ZERO), Woken::default());
}

/// Asserts that a stop is seen by every wait after it, with the round told before it taken once.
#[test]
fn a_stop_stays_told_and_a_round_is_taken_once() {
    let wake = Wake::default();
    wake.commit();
    wake.stop();
    let both = Woken {
        commit: true,
        stop: true,
    };
    assert_eq!(wake.wait(Duration::ZERO), both);
    let stopped = Woken {
        commit: false,
        stop: true,
    };
    assert_eq!(wake.wait(Duration::ZERO), stopped);
}
