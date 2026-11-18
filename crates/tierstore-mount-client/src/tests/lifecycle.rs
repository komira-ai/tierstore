//! The table of open packs and the client's life, against the real server.
//!
//! Proves: one pack named by two trees is opened once (one `open-pack`, one region, one hold),
//! and opening it with another size, or a pack too large for a state page, is refused before any
//! request; `release` ends the holder on the server and every later request answers `released`;
//! dropping the client hangs up its lifeline, so the server ends the holder, and the tickets
//! still outstanding answer `stopped`; the client reaches a real listening socket and backs off on
//! the runtime's own timer until the socket is there.

use std::sync::Arc;

use crate::{ClientError, Config, OpenSpec, PackClient, SocketPath, TokioTimer};
use tierstore_fetch_api::clock::ManualClock;
use tierstore_fetch_api::entry::Entry;
use tierstore_fetch_api::ids::{
    ByteRange, Class, HolderId, JobId, PackId, PackKey, PackRoot, Rank, TreeRef,
};
use tierstore_fetch_api::list::PackRecord;
use tierstore_testkit::dir::TestDir;

use crate::tests::common::{FakeList, MIB, PACKS, ServerSide, layout, pack_id, spec, stats_until};

fn bytes(offset: u64, len: u64) -> ByteRange {
    ByteRange { offset, len }
}

#[test]
fn a_pack_named_by_two_trees_is_opened_once() {
    let side = ServerSide::new("lifecycle-two-trees");
    // Tree b names pack 0 of tree a as its index 1.
    let b = TreeRef("tree-b".into());
    let record = |key: &str, size: u64| PackRecord {
        key: PackKey(key.into()),
        pack_size: size,
        cell_first: 0,
        cell_count: layout(size).cell_count,
        cell_shift: 20,
        pack_flags: 0,
        upload_part_size: 0,
    };
    side.trees.set(
        b.clone(),
        FakeList {
            records: vec![
                (record("other", PACKS[2].1), PackRoot::Digest([9; 32])),
                (record(PACKS[0].0, PACKS[0].1), pack_id(0).root),
            ],
        },
    );
    let client = side.client(1, &ManualClock::new());
    let first = client.open(spec(0));
    let second = client.open(spec(0));
    let a = first.wait().unwrap();
    assert!(Arc::ptr_eq(&a, &second.wait().unwrap()));
    let via_b = client
        .open(OpenSpec {
            tree: b,
            index: 1,
            pack: pack_id(0),
            size: PACKS[0].1,
        })
        .wait()
        .unwrap();
    assert!(Arc::ptr_eq(&a, &via_b));
    let stats = client.stats();
    assert_eq!(stats.opens_sent, 1);
    assert_eq!(stats.packs_opened, 1);
    assert!(stats.opens_shared >= 1);
    assert_eq!(side.entry(0).holders(), 1);

    // Another size under the same id, and a pack of more cells than a state page counts.
    assert_eq!(
        client
            .open(OpenSpec {
                size: PACKS[0].1 + 1,
                ..spec(0)
            })
            .wait()
            .err(),
        Some(ClientError::SizeMismatch {
            pack: pack_id(0),
            expected: PACKS[0].1,
            got: PACKS[0].1 + 1
        })
    );
    let huge = PackId {
        key: PackKey("huge".into()),
        root: PackRoot::Digest([7; 32]),
    };
    assert_eq!(
        client
            .open(OpenSpec {
                tree: TreeRef("tree-a".into()),
                index: 9,
                pack: huge.clone(),
                size: u64::MAX,
            })
            .wait()
            .err(),
        Some(ClientError::TooLarge {
            pack: huge,
            size: u64::MAX
        })
    );
    assert_eq!(client.stats().opens_sent, 1);
}

#[test]
fn release_ends_the_holder_and_every_later_request() {
    let side = ServerSide::new("lifecycle-release");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(1)).wait().unwrap();
    assert_eq!(side.entry(1).holders(), 1);
    assert_eq!(client.release().wait(), Ok(()));
    let server = side.wait_for(|s| s.holders_released == 1);
    assert_eq!(server.holders_live(), 0);
    assert_eq!(side.entry(1).holders(), 0);
    assert_eq!(
        pack.wait(bytes(0, 1), PACKS[1].1, JobId(1)).wait(),
        Err(ClientError::Released)
    );
    assert_eq!(
        client.open(spec(0)).wait().err(),
        Some(ClientError::Released)
    );
}

#[test]
fn dropping_the_client_hangs_up_its_lifeline_and_stops_its_tickets() {
    let side = ServerSide::new("lifecycle-drop");
    let client = side.client(1, &ManualClock::new());
    let pack = client.open(spec(0)).wait().unwrap();
    side.engine.park(pack_id(0));
    let outstanding = pack.wait(bytes(0, 1), PACKS[0].1, JobId(1));
    side.parked(0, 1);
    drop(client);
    let server = side.wait_for(|s| s.holders_hung_up == 1);
    assert_eq!(server.holders_live(), 0);
    assert_eq!(outstanding.wait(), Err(ClientError::Stopped));
    assert_eq!(
        pack.wait(bytes(10 * MIB, 1), PACKS[0].1, JobId(1)).wait(),
        Err(ClientError::Stopped)
    );
    // A hint to a stopped client goes nowhere.
    pack.ensure(&[], Class::Demand, JobId(1), Rank(0));
    assert_eq!(side.entry(0).holders(), 0);
}

#[test]
fn the_client_reaches_a_listening_socket_and_backs_off_on_the_runtime_timer() {
    let side = ServerSide::new("lifecycle-socket");
    let (dir, path) =
        TestDir::for_socket(&[&std::env::temp_dir()], "lifecycle-socket", "cache.sock");
    // The socket is not there yet: the first connects fail, and the client backs off on the
    // runtime's timer (1 ms steps here).
    let client = PackClient::start(
        HolderId(1),
        Arc::new(SocketPath(path.clone())),
        Arc::new(TokioTimer),
        Config {
            backoff_first_ms: 1,
            backoff_max_ms: 1,
            jitter_seed: None,
        },
    )
    .unwrap();
    stats_until(&client, |s| s.connect_failures >= 1);
    let server = side.server();
    let listener = {
        let _guard = side.runtime.enter();
        tokio::net::UnixListener::bind(&path).unwrap()
    };
    side.runtime
        .spawn(async move { server.serve(listener).await });
    let pack = client.open(spec(2)).wait().unwrap();
    assert_eq!(pack.size(), PACKS[2].1);
    assert_eq!(client.stats().handshakes, 1);
    assert!(format!("{client:?}").contains("PackClient"));
    drop(dir);
}
