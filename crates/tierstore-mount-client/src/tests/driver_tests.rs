//! The driver's own tests: what it does with a request or a reply for a pack it has no slot
//! for, with no connection, and when a counter region cannot be allocated. These are the paths no
//! reader or server reaches through the protocol; the tests of the other modules of `tests` reach
//! the rest through a real or scripted server.

use super::*;
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::ids::{PackKey, PackRoot, TreeRef};
use tierstore_fetch_api::page::CounterRegion;

use crate::link::SocketPath;
use crate::ticket::Ticket;

/// A driver that never runs: no connection, so its requests go nowhere.
fn driver() -> (Driver, mpsc::UnboundedSender<Command>) {
    let (tx, _rx) = mpsc::unbounded_channel();
    let (stats, _) = watch::channel(Stats::default());
    let (_kept, sent) = rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    let driver = Driver::new(Parts {
        holder: HolderId(1),
        memfd: CounterMemfd::create().unwrap(),
        lifeline: Arc::new(sent),
        connector: Arc::new(SocketPath("/nonexistent/tierstore.sock".into())),
        timer: Arc::new(ManualClock::new()),
        backoff: Backoff::new(50, 2000, 1),
        commands: Commands::new(tx.downgrade(), Arc::default()),
        stats,
    });
    (driver, tx)
}

fn pack(name: &str) -> PackId {
    PackId {
        key: PackKey(name.into()),
        root: PackRoot::Digest([3; 32]),
    }
}

fn spec(name: &str) -> OpenSpec {
    OpenSpec {
        tree: TreeRef("t".into()),
        index: 0,
        pack: pack(name),
        size: 1 << 20,
    }
}

#[test]
fn requests_and_replies_for_a_pack_not_in_the_table_change_nothing() {
    let (mut d, _tx) = driver();
    let unknown = pack("unknown");
    d.send_open(&unknown);
    d.send_wait(&unknown, 0, false);
    let region = CounterRegion {
        offset: 0,
        groups: 1,
    };
    d.opened(&unknown, PackHandle(1), 1 << 20, region, Vec::new());
    d.fail_slot(&unknown, ClientError::Stopped);
    d.wait_reply(&unknown, 0, Message::Waited(WaitOutcome::Ready));
    assert!(d.slots.is_empty());
    assert_eq!(*d.stats.borrow(), Stats::default());
}

#[test]
fn with_no_connection_nothing_is_sent() {
    let (mut d, _tx) = driver();
    assert_eq!(
        d.wire.send(Message::Ensured, Vec::new(), Pending::Ensure),
        Ok(None)
    );
    let (answer, mut ticket) = Ticket::new();
    d.open(spec("p"), answer);
    // The open waits for a connection; a wait of a window, for the pack's handle.
    d.send_open(&pack("p"));
    d.send_wait(&pack("p"), 0, false);
    d.slots.get_mut(&pack("p")).unwrap().handle = Some(PackHandle(1));
    // A window with no wait outstanding sends nothing either.
    d.send_wait(&pack("p"), 0, false);
    assert!(ticket.try_take().is_none());
    assert!(d.wire.inflight.is_empty());
    assert_eq!(d.stats.borrow().opens_sent, 0);
    assert_eq!(d.stats.borrow().waits_sent, 0);
}

#[test]
fn a_counter_region_that_cannot_be_allocated_fails_the_open() {
    let (mut d, _tx) = driver();
    d.memfd.set_len(u64::MAX - 100);
    let (answer, ticket) = Ticket::new();
    d.open(spec("p"), answer);
    assert_eq!(
        ticket.wait().err(),
        Some(ClientError::Counters {
            pack: pack("p"),
            reason: "the counter memfd would pass u64::MAX".into(),
        })
    );
    assert!(d.slots.is_empty());
}
