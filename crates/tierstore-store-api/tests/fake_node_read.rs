//! Proves the read routes of the fake storage node: `LOCATE` answers per key, hot, cold and
//! degraded GETs map a range onto the object's bytes and apply `If-Match`, key checks report
//! records and tombstones, tombstone pulls page by sequence number, and the ping reports what a
//! test set.
//!
//! Catches:
//! - a locate that answers out of order, answers a deleted key, or answers a key while indexing;
//! - a range mapped with the wrong offset (file, block or object coordinates), an off-by-one at
//!   either end of a span, a range past the span or the bytes that is served instead of `416`;
//! - a GET that serves another generation than `If-Match` names, or reports a missing file as
//!   `416`;
//! - a report that is dropped, or logged for a GET that was not answered;
//! - a tombstone page that repeats an entry, loses `more`, or moves `last` backwards;
//! - a ping drive that is appended instead of replaced.

mod fake_node_support;

use bytes::Bytes;
use fake_node_support::*;
use futures::FutureExt;
use tierstore_fetch_api::upstream::{DriveSignals, Report};
use tierstore_format::FormatError;
use tierstore_format::ids::NodeId;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{BlockDrop, LogEntry, LogRecord};
use tierstore_store_api::fake::{Route, Step};
use tierstore_store_api::peer::{GetTarget, PeerError};
use tierstore_store_api::proto::{
    DrivePing, KeyCheck, KeyRecord, LocateAnswer, LocateRequest, TombPull,
};

fn keys(names: &[&str]) -> LocateRequest {
    LocateRequest {
        keys: names.iter().map(|k| (*k).to_owned()).collect(),
    }
}

fn drop_record(origin: u8, seq: u64) -> LogRecord {
    LogRecord {
        origin: NodeId(origin),
        seq,
        entry: LogEntry::BlockDrop(BlockDrop {
            block: block(2, seq),
            stamp: Stamp::from_raw(seq),
        }),
    }
}

fn seqs(records: &[LogRecord]) -> Vec<u64> {
    records.iter().map(|r| r.seq).collect()
}

#[tokio::test]
async fn locate_answers_each_key_in_order_with_its_tier_or_absent() {
    let (_clock, node) = node(1);
    let copies = vec![hot_copy("seg", 4, 3)];
    let extents = vec![extent(block(1, 1), 0, 5, 0, drive(1), "shard")];
    node.put_hot(
        "hot",
        generation(1),
        Bytes::from_static(b"abc"),
        copies.clone(),
    );
    node.put_cold(
        "cold",
        generation(2),
        Bytes::from_static(b"12345"),
        extents.clone(),
    );
    node.put_hot("gone", generation(3), Bytes::from_static(b"zz"), vec![]);
    node.delete("gone", Stamp::from_raw(50));
    let answer = node
        .locate(keys(&["cold", "nothing", "hot", "gone"]))
        .await
        .unwrap();
    assert_eq!(
        answer.body.answers,
        vec![
            LocateAnswer::Cold {
                generation: generation(2),
                size: 5,
                extents
            },
            LocateAnswer::Absent,
            LocateAnswer::Hot {
                generation: generation(1),
                size: 3,
                copies
            },
            LocateAnswer::Absent,
        ]
    );
}

#[tokio::test]
async fn locate_answers_indexing_for_every_key_while_the_index_is_not_built() {
    let (_clock, node) = node(1);
    node.put_hot("k", generation(1), Bytes::new(), vec![]);
    node.set_indexing(true);
    let during = node.locate(keys(&["k", "other"])).await.unwrap();
    assert_eq!(
        during.body.answers,
        vec![LocateAnswer::Indexing, LocateAnswer::Indexing]
    );
    node.set_indexing(false);
    let after = node.locate(keys(&["k", "other"])).await.unwrap();
    assert!(matches!(after.body.answers[0], LocateAnswer::Hot { .. }));
    assert_eq!(after.body.answers[1], LocateAnswer::Absent);
}

#[tokio::test]
async fn a_hot_get_maps_the_file_range_onto_the_object() {
    let (_clock, node) = node(1);
    node.put_hot(
        "k",
        generation(7),
        Bytes::from_static(b"0123456789"),
        vec![hot_copy("seg", 100, 10)],
    );
    let got = node
        .get(get(hot("seg"), 103, 4, Some(generation(7))))
        .await
        .unwrap();
    assert_eq!(got.generation, generation(7));
    assert_eq!(
        got.signals,
        DriveSignals {
            service_ms: Some(0),
            queue_wait_ms: Some(0),
            drive_queue: None,
            drive_state: None,
        }
    );
    assert_eq!(body_bytes(got).await, b"3456");
}

#[tokio::test]
async fn a_get_reports_its_scripted_service_time_in_the_signals() {
    let (clock, node) = node(1);
    node.put_hot(
        "k",
        generation(1),
        Bytes::from_static(b"ab"),
        vec![hot_copy("s", 0, 2)],
    );
    node.script(Route::Get, Step::answer(7));
    let mut pending = Box::pin(node.get(get(hot("s"), 0, 2, None)));
    assert!((&mut pending).now_or_never().is_none());
    clock.advance(7);
    let got = (&mut pending).now_or_never().expect("released").unwrap();
    assert_eq!(got.signals.service_ms, Some(7));
    assert_eq!(got.signals.queue_wait_ms, Some(0));
}

#[tokio::test]
async fn a_hot_get_serves_the_whole_span_and_an_empty_range_at_its_end() {
    let (_clock, node) = node(1);
    node.put_hot(
        "k",
        generation(1),
        Bytes::from_static(b"abcd"),
        vec![hot_copy("s", 10, 4)],
    );
    let whole = node.get(get(hot("s"), 10, 4, None)).await.unwrap();
    assert_eq!(body_bytes(whole).await, b"abcd");
    let empty = node.get(get(hot("s"), 14, 0, None)).await.unwrap();
    assert_eq!(body_bytes(empty).await, b"");
}

#[tokio::test]
async fn a_hot_range_outside_the_span_is_416() {
    let (_clock, node) = node(1);
    node.put_hot(
        "k",
        generation(1),
        Bytes::from_static(b"abcd"),
        vec![hot_copy("s", 10, 4)],
    );
    for (offset, len) in [
        (10, 5),
        (11, 4),
        (15, 0),
        (9, 2),
        (0, 1),
        (10, u64::MAX),
        (u64::MAX, 2),
    ] {
        let err = node
            .get(get(hot("s"), offset, len, None))
            .await
            .unwrap_err();
        assert!(
            matches!(err, PeerError::Protocol { status: 416, node, .. } if node == NodeId(1)),
            "range {offset}+{len}: {err:?}"
        );
    }
}

#[tokio::test]
async fn a_range_past_the_stored_bytes_is_416_even_when_the_copy_claims_more() {
    let (_clock, node) = node(1);
    node.put_hot(
        "k",
        generation(1),
        Bytes::from_static(b"abcd"),
        vec![hot_copy("s", 0, 10)],
    );
    let ok = node.get(get(hot("s"), 0, 4, None)).await.unwrap();
    assert_eq!(body_bytes(ok).await, b"abcd");
    let err = node.get(get(hot("s"), 2, 4, None)).await.unwrap_err();
    assert!(matches!(err, PeerError::Protocol { status: 416, .. }));
}

#[tokio::test]
async fn objects_sharing_a_file_are_told_apart_by_the_range() {
    let (_clock, node) = node(1);
    node.put_hot(
        "a",
        generation(1),
        Bytes::from_static(b"AAAAA"),
        vec![hot_copy("s", 0, 5)],
    );
    node.put_hot(
        "b",
        generation(2),
        Bytes::from_static(b"BBBBB"),
        vec![hot_copy("s", 5, 5)],
    );
    let b = node.get(get(hot("s"), 6, 2, None)).await.unwrap();
    assert_eq!(b.generation, generation(2));
    assert_eq!(body_bytes(b).await, b"BB");
    let a = node.get(get(hot("s"), 0, 5, None)).await.unwrap();
    assert_eq!(a.generation, generation(1));
}

#[tokio::test]
async fn if_match_must_name_the_generation_the_copy_holds() {
    let (_clock, node) = node(1);
    node.put_hot(
        "k",
        generation(7),
        Bytes::from_static(b"abc"),
        vec![hot_copy("s", 0, 3)],
    );
    let err = node
        .get(get(hot("s"), 0, 3, Some(generation(8))))
        .await
        .unwrap_err();
    assert_eq!(
        err,
        PeerError::PreconditionFailed {
            node: NodeId(1),
            actual: Some(generation(7))
        }
    );
    assert!(
        node.get(get(hot("s"), 0, 3, Some(generation(7))))
            .await
            .is_ok()
    );
    assert!(node.get(get(hot("s"), 0, 3, None)).await.is_ok());
}

#[tokio::test]
async fn a_get_of_a_file_shard_or_block_the_node_does_not_hold_is_not_found() {
    let (_clock, node) = node(1);
    node.put_hot(
        "h",
        generation(1),
        Bytes::from_static(b"abc"),
        vec![hot_copy("s", 0, 3)],
    );
    let b = block(1, 1);
    node.put_cold(
        "c",
        generation(2),
        Bytes::from_static(b"12345678"),
        vec![extent(b, 0, 8, 0, drive(1), "shard")],
    );
    let missing = [
        get(hot("other"), 0, 1, None),
        get(cold(drive(1), "other"), 0, 1, None),
        get(cold(drive(2), "shard"), 0, 1, None),
        get(
            GetTarget::Degraded {
                block: block(1, 2),
                sources: vec![],
            },
            0,
            1,
            None,
        ),
        // A target of the other tier does not reach the object.
        get(cold(drive(1), "s"), 0, 1, None),
        get(hot("shard"), 0, 1, None),
    ];
    for req in missing {
        let err = node.get(req.clone()).await.unwrap_err();
        assert_eq!(err, PeerError::NotFound { node: NodeId(1) }, "{req:?}");
    }
}

#[tokio::test]
async fn a_cold_get_maps_the_block_range_onto_the_object_through_the_extent() {
    let (_clock, node) = node(1);
    // The extent holds object bytes 8..14 at block offset 100..106 of shard "sh" of drive 5.
    let object: Vec<u8> = (0u8..16).collect();
    node.put_cold(
        "k",
        generation(3),
        Bytes::from(object),
        vec![extent(block(1, 1), 8, 6, 100, drive(5), "sh")],
    );
    let got = node
        .get(get(cold(drive(5), "sh"), 102, 3, Some(generation(3))))
        .await
        .unwrap();
    assert_eq!(got.generation, generation(3));
    assert_eq!(body_bytes(got).await, vec![10, 11, 12]);
    let edge = node
        .get(get(cold(drive(5), "sh"), 105, 1, None))
        .await
        .unwrap();
    assert_eq!(body_bytes(edge).await, vec![13]);
    for (offset, len) in [(99, 1), (100, 7), (106, 1)] {
        let err = node
            .get(get(cold(drive(5), "sh"), offset, len, None))
            .await
            .unwrap_err();
        assert!(
            matches!(err, PeerError::Protocol { status: 416, .. }),
            "{offset}+{len}"
        );
    }
}

#[tokio::test]
async fn a_degraded_get_finds_the_extent_of_the_block() {
    let (_clock, node) = node(1);
    let object: Vec<u8> = (0u8..10).collect();
    node.put_cold(
        "k",
        generation(3),
        Bytes::from(object),
        vec![
            extent(block(1, 1), 0, 4, 20, drive(1), "a"),
            extent(block(1, 2), 4, 6, 0, drive(2), "b"),
        ],
    );
    let target = |counter| GetTarget::Degraded {
        block: block(1, counter),
        sources: vec![],
    };
    let first = node.get(get(target(1), 21, 2, None)).await.unwrap();
    assert_eq!(body_bytes(first).await, vec![1, 2]);
    let second = node.get(get(target(2), 1, 3, None)).await.unwrap();
    assert_eq!(body_bytes(second).await, vec![5, 6, 7]);
    let err = node.get(get(target(2), 5, 2, None)).await.unwrap_err();
    assert!(matches!(err, PeerError::Protocol { status: 416, .. }));
}

#[tokio::test]
async fn a_report_is_logged_with_its_get_when_the_get_is_answered() {
    let (_clock, node) = node(1);
    node.put_hot(
        "k",
        generation(1),
        Bytes::from_static(b"abc"),
        vec![hot_copy("s", 0, 3)],
    );
    let plain = get(hot("s"), 0, 1, None);
    let mut reported = get(hot("s"), 1, 2, None);
    reported.report = Some(Report::Bitrot);
    node.get(plain).await.unwrap();
    assert!(node.reports().is_empty());
    node.get(reported.clone()).await.unwrap();
    let mut failed = get(hot("nope"), 0, 1, None);
    failed.report = Some(Report::Bitrot);
    node.get(failed).await.unwrap_err();
    assert_eq!(node.reports(), vec![reported]);
}

#[tokio::test]
async fn a_key_check_reports_the_record_the_tombstone_and_the_index_state() {
    let (_clock, node) = node(1);
    node.put_hot("live", generation(4), Bytes::new(), vec![]);
    node.put_hot("dead", generation(5), Bytes::new(), vec![]);
    node.delete("dead", Stamp::from_raw(77));
    node.delete("never", Stamp::from_raw(78));
    let check = |key: &str| {
        env(
            2,
            1,
            KeyCheck {
                key: key.to_owned(),
            },
        )
    };
    let live = node.key_check(check("live")).await.unwrap().body;
    assert_eq!(
        live.records,
        vec![KeyRecord {
            generation: generation(4),
            stamp: Stamp::from_raw(1)
        }]
    );
    assert_eq!(live.tombstone, None);
    assert!(!live.indexing);
    let dead = node.key_check(check("dead")).await.unwrap().body;
    assert!(dead.records.is_empty());
    assert_eq!(dead.tombstone, Some(Stamp::from_raw(77)));
    let never = node.key_check(check("never")).await.unwrap().body;
    assert!(never.records.is_empty());
    assert_eq!(never.tombstone, Some(Stamp::from_raw(78)));
    let unknown = node.key_check(check("unknown")).await.unwrap().body;
    assert!(unknown.records.is_empty());
    assert_eq!(unknown.tombstone, None);
    node.set_indexing(true);
    assert!(node.key_check(check("live")).await.unwrap().body.indexing);
}

#[tokio::test]
async fn a_tomb_pull_pages_by_sequence_number_with_more_and_last() {
    let (_clock, node) = node(1);
    for seq in 1..=5 {
        node.append_tomb(drop_record(2, seq)).unwrap();
    }
    node.append_tomb(drop_record(3, 1)).unwrap();
    let pull = |after, limit| TombPull {
        origin: NodeId(2),
        after,
        limit,
    };
    let page = |after, limit| {
        let node = &node;
        async move { node.tomb_pull(pull(after, limit)).await.unwrap().body }
    };

    let first = page(0, 2).await;
    assert_eq!(seqs(&first.decode(&pull(0, 2)).unwrap()), vec![1, 2]);
    assert_eq!((first.last, first.more), (2, true));

    let rest = page(first.last, 10).await;
    assert_eq!(
        seqs(&rest.decode(&pull(first.last, 10)).unwrap()),
        vec![3, 4, 5]
    );
    assert_eq!((rest.last, rest.more), (5, false));

    let exact = page(2, 3).await;
    assert_eq!(seqs(&exact.decode(&pull(2, 3)).unwrap()), vec![3, 4, 5]);
    assert!(!exact.more);

    let drained = page(5, 10).await;
    assert!(drained.entries.is_empty());
    assert_eq!((drained.last, drained.more), (5, false));

    let none_wanted = page(1, 0).await;
    assert!(none_wanted.entries.is_empty());
    assert_eq!((none_wanted.last, none_wanted.more), (1, true));

    let other_pull = TombPull {
        origin: NodeId(3),
        after: 0,
        limit: 10,
    };
    let other = node.tomb_pull(other_pull).await.unwrap().body;
    assert_eq!(other.decode(&other_pull).unwrap(), vec![drop_record(3, 1)]);
    let unknown = node
        .tomb_pull(TombPull {
            origin: NodeId(9),
            after: 4,
            limit: 10,
        })
        .await
        .unwrap()
        .body;
    assert!(unknown.entries.is_empty());
    assert_eq!((unknown.last, unknown.more), (4, false));
}

#[tokio::test]
async fn a_tomb_log_accepts_only_the_next_sequence_number_of_its_origin() {
    let (_clock, node) = node(1);
    let refused = FormatError::Invalid {
        what: "tomb log entry",
        reason: "not the next sequence number of its origin",
    };
    // An empty log starts at 1.
    assert_eq!(node.append_tomb(drop_record(2, 2)).unwrap_err(), refused);
    assert_eq!(
        node.append_tomb(drop_record(2, 0)).unwrap_err().to_string(),
        "tomb log entry: sequence 0 is not a position"
    );
    node.append_tomb(drop_record(2, 1)).unwrap();
    // A repeat, a gap; each origin counts alone.
    assert_eq!(node.append_tomb(drop_record(2, 1)).unwrap_err(), refused);
    assert_eq!(node.append_tomb(drop_record(2, 3)).unwrap_err(), refused);
    node.append_tomb(drop_record(3, 1)).unwrap();
    node.append_tomb(drop_record(2, 2)).unwrap();
}

#[tokio::test]
async fn a_tomb_pull_keeps_entries_of_a_kind_it_does_not_know() {
    let (_clock, node) = node(1);
    let unknown = LogRecord {
        origin: NodeId(2),
        seq: 1,
        entry: LogEntry::Unknown {
            kind: 200,
            body: vec![1, 2, 3],
        },
    };
    node.append_tomb(unknown.clone()).unwrap();
    let pull = TombPull {
        origin: NodeId(2),
        after: 0,
        limit: 5,
    };
    let page = node.tomb_pull(pull).await.unwrap().body;
    assert_eq!(page.decode(&pull).unwrap(), vec![unknown]);
}

fn drive_ping(id: u8, state: &str, queued: u32) -> DrivePing {
    DrivePing {
        drive: drive(id),
        state: state.to_owned(),
        oldest_call_ms: Some(u64::from(queued)),
        queued: [queued, 0, 0, 0],
        streams: 2,
    }
}

#[tokio::test]
async fn a_ping_reports_the_indexing_flag_the_drives_and_the_hot_queue() {
    let (_clock, node) = node(1);
    let empty = node.ping().await.unwrap().body;
    assert!(!empty.indexing);
    assert!(empty.drives.is_empty());
    assert_eq!(empty.hot_queued, 0);

    node.set_drive(drive_ping(1, "ok", 3));
    node.set_drive(drive_ping(2, "ok", 0));
    node.set_drive(drive_ping(1, "stalled", 9));
    node.set_hot_queued(11);
    node.set_indexing(true);
    let ping = node.ping().await.unwrap().body;
    assert!(ping.indexing);
    assert_eq!(
        ping.drives,
        vec![drive_ping(1, "stalled", 9), drive_ping(2, "ok", 0)]
    );
    assert_eq!(ping.hot_queued, 11);
}
