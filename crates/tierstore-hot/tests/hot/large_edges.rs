//! The failure and boundary paths of the large-file stream, of hot reads and of reclaim, which
//! the happy-path suites do not reach: a replica's side of a large stream refusing a key it
//! cannot store, a failing disk at each step of its copy, an empty body, a key deleted after the
//! stream began; the writer committing over a file left by a deleted key and re-sending the
//! finished file when the disk or the replica fails; a read and an unlink failing on the
//! device; hot-only mode counting clean bytes as dirty.
//!
//! Catches: a key longer than a record header can name written as a truncated key; a replica
//! answering `Committed` for a copy whose disk write, sync or directory sync failed, or leaving
//! its `.up` behind; a zero-byte body refused; a PUT older than a delete committed over it; a
//! re-put of a deleted object failing on the file the delete left; a re-send of an unreadable
//! file blocking the PUT or counting a copy that was never made; a read error answered as a
//! missing file; an unlink error swallowed by reclaim (the file forgotten while still on disk);
//! a clean file in hot-only mode not counted as dirty.

use crate::support::{Kind, Rendezvous, Sim, config};
use bytes::Bytes;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::stream;
use tierstore_fetch_api::ids::ByteRange;
use tierstore_format::ids::{ExpiryClass, Generation, KeyHash, NodeId};
use tierstore_format::names::HotName;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{LargeFile, Tombstone};
use tierstore_harness::disk::{Fault, OpKind};
use tierstore_hot::node::ReadError;
use tierstore_hot::peers::{LargeAnswer, LargeChunk, LargeOffer};
use tierstore_store_api::hot::{Acked, HotError, HotStore, LargePut, Unavailable};
use tierstore_store_api::index::{EntryLocation, Lookup};
use tierstore_store_api::placement::Placement;
use tierstore_store_api::proto::Envelope;

const MIB: usize = 1 << 20;

fn bytes_of(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn put_with(
    sim: &mut Sim,
    id: u8,
    key: &str,
    stamp: Stamp,
    bytes: &[u8],
) -> Result<Acked, HotError> {
    let node = sim.node(id);
    let put = LargePut {
        key: key.as_bytes().to_vec(),
        size: Some(bytes.len() as u64),
        expiry: ExpiryClass(0),
        stamp,
        body: stream::iter(
            bytes
                .chunks(40_000)
                .map(|c| Ok(Bytes::copy_from_slice(c)))
                .collect::<Vec<_>>(),
        )
        .boxed(),
    };
    let h = sim.exec.start(async move { node.put_file(put).await });
    sim.run();
    h.take().expect("the PUT did not finish")
}

fn put_large(sim: &mut Sim, id: u8, key: &str, bytes: &[u8]) -> Result<Acked, HotError> {
    let stamp = sim.stamp();
    put_with(sim, id, key, stamp, bytes)
}

fn path_of(acked: &Acked) -> String {
    match acked.location {
        EntryLocation::Large(file) => HotName::Large(file).path(),
        other => panic!("not a large file: {other:?}"),
    }
}

fn ls(sim: &Sim, id: u8, ext: &str) -> Vec<String> {
    sim.disk(id)
        .list("big/")
        .unwrap()
        .into_iter()
        .filter(|p| p.ends_with(ext))
        .collect()
}

fn tombstone(sim: &mut Sim, key: &str, generation: Generation) -> Tombstone {
    Tombstone {
        key: key.as_bytes().to_vec(),
        generation,
        stamp: sim.stamp(),
        names: Vec::new(),
    }
}

/// What node `id` answers to a large stream of `chunks` for `key`.
fn receive(sim: &mut Sim, id: u8, key: &[u8], chunks: Vec<LargeChunk>) -> LargeAnswer {
    let stamp = sim.stamp();
    let offer = Envelope {
        from: NodeId(9),
        stamp,
        mem_epoch: 1,
        body: LargeOffer {
            key: key.to_vec(),
            size: None,
            expiry: ExpiryClass(0),
            stamp,
        },
    };
    let node = sim.node(id);
    sim.exec.block(async move {
        node.receive_large(offer, stream::iter(chunks).boxed())
            .await
    })
}

fn data(b: &[u8]) -> LargeChunk {
    LargeChunk::Data(Bytes::copy_from_slice(b))
}

// ---------------------------------------------------------------------------------------------
// A replica's side of a large stream.

#[test]
fn a_replica_refuses_a_key_longer_than_a_record_header_can_name_and_leaves_nothing() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = vec![b'k'; 70_000];
    let answer = receive(
        &mut sim,
        2,
        &key,
        vec![data(b"abc"), LargeChunk::Trailer(Generation::of(b"abc"))],
    );
    assert_eq!(answer, LargeAnswer::Abandoned);
    assert!(sim.disk(2).list("big/").unwrap().is_empty());
}

#[test]
fn a_replica_commits_a_zero_byte_body_as_an_empty_object() {
    let mut sim = Sim::new(3, config(3, 2));
    let answer = receive(
        &mut sim,
        2,
        b"empty",
        vec![LargeChunk::Trailer(Generation::of(b""))],
    );
    assert_eq!(answer, LargeAnswer::Committed);
    let file = LargeFile {
        key_hash: KeyHash::of(b"empty"),
        generation: Generation::of(b""),
        size: 0,
    };
    assert_eq!(ls(&sim, 2, ".obj"), vec![HotName::Large(file).path()]);
    assert!(ls(&sim, 2, ".up").is_empty());
    let Ok(Lookup::Live(e)) = sim.node(2).index().get(b"empty") else {
        panic!("the empty object is live");
    };
    assert_eq!(e.length, 0);
}

#[test]
fn a_replica_whose_chunk_write_fails_abandons_the_copy_and_removes_its_up() {
    let mut sim = Sim::new(3, config(3, 2));
    // The first write is the key; the second is the full chunk's.
    sim.disk(2).inject(OpKind::WriteAt, 1, Fault::Eio);
    let big = vec![7u8; 4 * MIB];
    let answer = receive(
        &mut sim,
        2,
        b"chunky",
        vec![
            LargeChunk::Data(Bytes::from(big)),
            LargeChunk::Trailer(Generation::of(b"x")),
        ],
    );
    assert_eq!(answer, LargeAnswer::Abandoned);
    assert!(sim.disk(2).list("big/").unwrap().is_empty());
    assert_eq!(sim.node(2).index().get(b"chunky").unwrap(), Lookup::Absent);
}

#[test]
fn a_replica_whose_sync_fails_abandons_the_copy_and_removes_its_up() {
    let mut sim = Sim::new(3, config(3, 2));
    sim.disk(2).inject(OpKind::Fdatasync, 0, Fault::Eio);
    let answer = receive(
        &mut sim,
        2,
        b"unsynced",
        vec![data(b"abc"), LargeChunk::Trailer(Generation::of(b"abc"))],
    );
    assert_eq!(answer, LargeAnswer::Abandoned);
    assert!(sim.disk(2).list("big/").unwrap().is_empty());
    assert_eq!(
        sim.node(2).index().get(b"unsynced").unwrap(),
        Lookup::Absent
    );
}

#[test]
fn a_replica_whose_directory_sync_fails_does_not_answer_committed_or_index_the_key() {
    let mut sim = Sim::new(3, config(3, 2));
    sim.disk(2).inject(OpKind::FsyncDir, 0, Fault::Eio);
    let answer = receive(
        &mut sim,
        2,
        b"undurable",
        vec![data(b"abc"), LargeChunk::Trailer(Generation::of(b"abc"))],
    );
    assert_eq!(answer, LargeAnswer::Abandoned);
    assert!(ls(&sim, 2, ".up").is_empty());
    assert_eq!(
        sim.node(2).index().get(b"undurable").unwrap(),
        Lookup::Absent
    );
}

#[test]
fn a_replica_that_holds_a_newer_delete_of_the_key_abandons_the_copy() {
    let mut sim = Sim::new(3, config(3, 2));
    let old = sim.stamp();
    let tomb = tombstone(&mut sim, "gone", Generation::of(b""));
    sim.node(2).index().delete(&tomb).unwrap();
    let node = sim.node(2);
    let offer = Envelope {
        from: NodeId(9),
        stamp: old,
        mem_epoch: 1,
        body: LargeOffer {
            key: b"gone".to_vec(),
            size: None,
            expiry: ExpiryClass(0),
            stamp: old,
        },
    };
    let chunks = vec![data(b"abc"), LargeChunk::Trailer(Generation::of(b"abc"))];
    let answer = sim.exec.block(async move {
        node.receive_large(offer, stream::iter(chunks).boxed())
            .await
    });
    assert_eq!(answer, LargeAnswer::Abandoned);
    assert!(sim.disk(2).list("big/").unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// The writer's commit.

#[test]
fn a_put_older_than_a_delete_of_its_key_answers_503_not_definitive_and_leaves_nothing() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "older");
    let old = sim.stamp();
    let tomb = tombstone(&mut sim, &key, Generation::of(b""));
    sim.node(1).index().delete(&tomb).unwrap();
    let r = put_with(&mut sim, 1, &key, old, &bytes_of(5_000, 1));
    assert!(
        matches!(
            r,
            Err(HotError::Unavailable {
                reason: Unavailable::NotDefinitive,
                ..
            })
        ),
        "{r:?}"
    );
    assert!(sim.disk(1).list("big/").unwrap().is_empty());
}

#[test]
fn a_put_of_the_bytes_of_a_deleted_key_commits_over_the_file_the_delete_left() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "again");
    let bytes = bytes_of(9_000, 2);
    let first = put_large(&mut sim, 1, &key, &bytes).unwrap();
    let tomb = tombstone(&mut sim, &key, first.generation);
    sim.node(1).index().delete(&tomb).unwrap();
    assert!(matches!(
        sim.node(1).index().get(key.as_bytes()),
        Ok(Lookup::Deleted(_))
    ));
    let second = put_large(&mut sim, 1, &key, &bytes).unwrap();
    assert_eq!(second.generation, first.generation);
    assert_eq!(ls(&sim, 1, ".obj"), vec![path_of(&first)]);
    assert!(ls(&sim, 1, ".up").is_empty());
    assert!(matches!(
        sim.node(1).index().get(key.as_bytes()),
        Ok(Lookup::Live(_))
    ));
}

/// A 3-node cluster whose replica `lagged` (not holding anything) is held, and a 9 MiB body fed
/// in 1 MiB pieces until the writer waits for it; the tick abandons it and the body ends.
/// Returns what the test needs to finish the PUT: the handle, the key and the lagging node.
fn lagged_big_put(
    sim: &mut Sim,
    key: &str,
) -> (crate::support::Handle<Result<Acked, HotError>>, u8) {
    let rank = Rendezvous.rank(key.as_bytes(), &sim.view);
    let lagged = rank[1].0;
    sim.net.hold(Some(1), lagged, Kind::Large);
    let body = bytes_of(9 * MIB, 3);
    let (tx, rx) = mpsc::unbounded::<Result<Bytes, std::io::Error>>();
    let stamp = sim.stamp();
    let node = sim.node(1);
    let put = LargePut {
        key: key.as_bytes().to_vec(),
        size: Some(body.len() as u64),
        expiry: ExpiryClass(0),
        stamp,
        body: rx.boxed(),
    };
    let h = sim.exec.start(async move { node.put_file(put).await });
    sim.run();
    for piece in body.chunks(MIB) {
        tx.unbounded_send(Ok(Bytes::copy_from_slice(piece)))
            .unwrap();
        sim.run();
    }
    sim.clock.advance(sim.cfg.put_copy_stall_ms);
    assert_eq!(sim.node(1).tick().abandoned, vec![NodeId(lagged)]);
    sim.run();
    drop(tx);
    sim.run();
    (h, lagged)
}

#[test]
fn a_finished_file_that_cannot_be_read_for_the_resend_leaves_the_put_short_and_no_up() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "unreadable");
    let (h, lagged) = lagged_big_put(&mut sim, &key);
    sim.net.unhold();
    sim.disk(1).inject(OpKind::ReadAt, 0, Fault::Eio);
    sim.net.release(0);
    sim.run();
    let r = h.take().expect("the PUT is waiting for something");
    assert!(
        matches!(
            r,
            Err(HotError::Unavailable {
                reason: Unavailable::TooFewCopies,
                ..
            })
        ),
        "{r:?}"
    );
    assert!(sim.disk(1).list("big/").unwrap().is_empty());
    assert!(sim.disk(lagged).list("big/").unwrap().is_empty());
    assert_eq!(sim.net.sent(1, lagged, Kind::Large), 2);
}

#[test]
fn a_resend_to_a_replica_that_cannot_be_reached_counts_no_copy() {
    let mut sim = Sim::new(3, config(3, 3));
    let key = sim.key_on(1, "unreachable");
    let (h, lagged) = lagged_big_put(&mut sim, &key);
    sim.net.cut(1, lagged);
    sim.net.drop_held(0);
    sim.run();
    let r = h.take().expect("the PUT is waiting for something");
    assert!(
        matches!(
            r,
            Err(HotError::Unavailable {
                reason: Unavailable::TooFewCopies,
                ..
            })
        ),
        "{r:?}"
    );
    assert!(sim.disk(1).list("big/").unwrap().is_empty());
    assert_eq!(sim.net.sent(1, lagged, Kind::Large), 2);
}

// ---------------------------------------------------------------------------------------------
// Reads, reclaim and the marks.

#[test]
fn a_read_that_fails_on_the_device_is_a_device_error_not_a_missing_file() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "readme");
    let acked = put_large(&mut sim, 1, &key, &bytes_of(3_000, 4)).unwrap();
    let EntryLocation::Large(file) = acked.location else {
        panic!("large");
    };
    sim.disk(1).inject(OpKind::ReadAt, 0, Fault::Eio);
    let node = sim.node(1);
    let err = sim
        .exec
        .block(async move {
            node.serve(HotName::Large(file), ByteRange { offset: 0, len: 8 })
                .await
        })
        .unwrap_err();
    assert!(matches!(err, ReadError::Hot(_)), "{err:?}");
}

fn clean_large(sim: &mut Sim, key: &str, seed: u8) -> String {
    let acked = put_large(sim, 1, key, &bytes_of(4_000, seed)).unwrap();
    let EntryLocation::Large(file) = acked.location else {
        panic!("large");
    };
    let node = sim.node(1);
    assert!(
        sim.exec
            .block(async move { node.mark_clean(HotName::Large(file)).await })
            .unwrap()
    );
    path_of(&acked)
}

#[test]
fn reclaim_that_cannot_unlink_a_file_fails_and_keeps_it_known_until_a_retry_succeeds() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "stuck");
    let path = clean_large(&mut sim, &key, 5);
    let total = sim.node(1).fill().total;
    sim.disk(1).inject(OpKind::Unlink, 0, Fault::Eio);
    let node = sim.node(1);
    let r = sim.exec.block(async move { node.reclaim(u64::MAX).await });
    assert!(r.is_err(), "{r:?}");
    assert!(sim.disk(1).exists(&path).unwrap());
    assert_eq!(sim.node(1).fill().total, total);
    let node = sim.node(1);
    let done = sim
        .exec
        .block(async move { node.reclaim(u64::MAX).await })
        .unwrap();
    assert_eq!(done.files, vec![path.clone()]);
    assert!(!sim.disk(1).exists(&path).unwrap());
    assert_eq!(sim.node(1).fill().total, 0);
}

#[test]
fn reclaim_of_a_file_already_gone_from_the_disk_forgets_it() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "vanished");
    let path = clean_large(&mut sim, &key, 6);
    sim.disk(1).unlink(&path).unwrap();
    let node = sim.node(1);
    let done = sim
        .exec
        .block(async move { node.reclaim(u64::MAX).await })
        .unwrap();
    assert_eq!(done.files, vec![path]);
    assert_eq!(sim.node(1).fill().total, 0);
}

#[test]
fn in_hot_only_mode_a_clean_file_still_counts_as_dirty() {
    let mut cfg = config(3, 2);
    cfg.hot_only = true;
    let mut sim = Sim::new(3, cfg);
    let key = sim.key_on(1, "hotonly");
    clean_large(&mut sim, &key, 7);
    let fill = sim.node(1).fill();
    assert!(fill.total > 0);
    assert_eq!(fill.dirty, fill.total);
}

#[test]
fn a_put_whose_rename_fails_answers_an_error_and_indexes_nothing() {
    let mut sim = Sim::new(3, config(3, 2));
    let key = sim.key_on(1, "norename");
    sim.disk(1).inject(OpKind::Rename, 0, Fault::Eio);
    let r = put_large(&mut sim, 1, &key, &bytes_of(3_000, 8));
    assert!(matches!(r, Err(HotError::Io { .. })), "{r:?}");
    assert!(ls(&sim, 1, ".obj").is_empty());
    assert_eq!(
        sim.node(1).index().get(key.as_bytes()).unwrap(),
        Lookup::Absent
    );
}
