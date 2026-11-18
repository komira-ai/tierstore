//! Proves the peer protocol's wire forms: hex bytes, envelopes and every JSON message round trip,
//! the replica answer converts to and from the store's result type, and the header lists of a
//! batch, a GET and a GET response, and the `x-ts-sources` value, encode and decode exactly, with
//! each malformed input naming the header it refused.
//!
//! Catches:
//! - uppercase or odd-length hex accepted for a binary format;
//! - a message that does not survive its JSON form, or a tag (`kind`) that changed;
//! - a replica rejection turned into another one, or into an ack, on its way over the wire;
//! - a missing or malformed header reported as another header, or accepted;
//! - a crc header that is not exactly 8 lowercase hex digits;
//! - defaults of a GET (class, job, rank) not applied, or applied to a malformed value;
//! - an absent drive signal decoded as a present one;
//! - a corrupt descriptor, free or log entry inside a commit or tomb page decoded as valid.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use bytes::Bytes;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tierstore_fetch_api::ids::{Class, JobId, Rank};
use tierstore_fetch_api::upstream::{DriveSignals, DriveState};
use tierstore_format::FormatError;
use tierstore_format::descriptor::{Code, PlacedShard, ShardLoc, StripeDescriptor};
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{BlockId, DriveId, ExpiryClass, Generation, NodeId, StripeId};
use tierstore_format::seal::{Ballot, SealProposal};
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::{BlockDrop, LogEntry, LogRecord, TombDrop, Tombstone};
use tierstore_store_api::hot::{ReplicaAck, ReplicaBatch, ReplicaReject};
use tierstore_store_api::proto::{
    BatchHeaders, ColdExtent, CommitAnswer, CommitRecord, CopyWire, DrivePing, Envelope,
    GetHeaders, HexBytes, HotCopy, KeyCheck, KeyFacts, KeyRecord, LocateAnswer, LocateRequest,
    LocateResponse, PingAnswer, ProtoError, ReplicaAnswer, ResponseHeaders, SealAnswer,
    SealMessage, SourceShard, TombPage, TombPull, decode_sources, encode_sources, header,
};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, value, "{json}");
}

fn stamp(wall: u64) -> Stamp {
    Stamp::from_parts(wall, 3).unwrap()
}

fn generation(b: u8) -> Generation {
    Generation([b; 16])
}

fn block(counter: u64) -> BlockId {
    BlockId::new(NodeId(2), counter).unwrap()
}

fn ballot(round: u32, node: u8) -> Ballot {
    Ballot {
        round,
        node: NodeId(node),
    }
}

fn proposal() -> SealProposal {
    SealProposal {
        length: 4096,
        running_crc: 0xdead_beef,
    }
}

fn descriptor() -> StripeDescriptor {
    StripeDescriptor {
        stripe: StripeId::new(NodeId(1), 9).unwrap(),
        revision: 2,
        code: Code::Replicas(2),
        blocks: vec![(block(5), 1000)],
        shards: vec![
            PlacedShard {
                index: 0,
                loc: ShardLoc {
                    node: NodeId(1),
                    drive: DriveId([1; 16]),
                },
            },
            PlacedShard {
                index: 1,
                loc: ShardLoc {
                    node: NodeId(2),
                    drive: DriveId([2; 16]),
                },
            },
        ],
        expiry: ExpiryClass(1),
        coordinator: NodeId(1),
    }
}

fn free() -> StripeFree {
    StripeFree {
        stripe: StripeId::new(NodeId(1), 9).unwrap(),
        revision: 4,
        owner: NodeId(3),
        stamp: stamp(77),
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
        .collect()
}

fn with(mut h: Vec<(String, String)>, name: &str, value: &str) -> Vec<(String, String)> {
    h.retain(|(n, _)| n != name);
    h.push((name.to_owned(), value.to_owned()));
    h
}

fn without(mut h: Vec<(String, String)>, name: &str) -> Vec<(String, String)> {
    h.retain(|(n, _)| n != name);
    h
}

// ---- hex bytes and envelopes ----

#[test]
fn hex_bytes_travel_as_lowercase_hex_and_come_back_equal() {
    let b = HexBytes(vec![0x00, 0xab, 0xff]);
    assert_eq!(serde_json::to_string(&b).unwrap(), "\"00abff\"");
    round_trip(&b);
    round_trip(&HexBytes(Vec::new()));
}

#[test]
fn uppercase_hex_is_refused() {
    let err = serde_json::from_str::<HexBytes>("\"00AB\"").unwrap_err();
    assert!(err.to_string().contains("not lowercase hex"), "{err}");
}

#[test]
fn odd_length_and_non_hex_text_is_refused() {
    assert!(serde_json::from_str::<HexBytes>("\"abc\"").is_err());
    assert!(serde_json::from_str::<HexBytes>("\"zz\"").is_err());
}

#[test]
fn an_envelope_carries_sender_stamp_epoch_and_body() {
    let e = Envelope {
        from: NodeId(2),
        stamp: stamp(1234),
        mem_epoch: 9,
        body: KeyCheck { key: "a/b".into() },
    };
    round_trip(&e);
    let v: serde_json::Value = serde_json::to_value(&e).unwrap();
    assert_eq!(v["from"], 2);
    assert_eq!(v["mem_epoch"], 9);
    assert_eq!(v["body"]["key"], "a/b");
}

// ---- messages ----

fn copy() -> HotCopy {
    HotCopy {
        node: NodeId(1),
        file: "seg".into(),
        offset: 10,
        length: 20,
        promoted: true,
    }
}

fn extent() -> ColdExtent {
    ColdExtent {
        block: block(1),
        object_offset: 0,
        length: 100,
        block_offset: 4096,
        node: NodeId(1),
        drive: DriveId([7; 16]),
        shard: "s.shard".into(),
        reconstructor: NodeId(2),
        sources: vec![SourceShard {
            node: NodeId(3),
            drive: DriveId([8; 16]),
            index: 2,
        }],
        margin: -1,
    }
}

#[test]
fn locate_request_and_every_kind_of_locate_answer_round_trip() {
    round_trip(&LocateRequest {
        keys: vec!["a".into(), "b".into()],
    });
    let answers = vec![
        LocateAnswer::Hot {
            generation: generation(1),
            size: 30,
            copies: vec![copy()],
        },
        LocateAnswer::Cold {
            generation: generation(2),
            size: 100,
            extents: vec![extent()],
        },
        LocateAnswer::Absent,
        LocateAnswer::Indexing,
    ];
    round_trip(&LocateResponse {
        answers: answers.clone(),
    });
    let kinds: Vec<String> = answers
        .iter()
        .map(|a| {
            serde_json::to_value(a).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(kinds, ["hot", "cold", "absent", "indexing"]);
}

#[test]
fn key_check_and_key_facts_round_trip() {
    round_trip(&KeyCheck { key: "k".into() });
    round_trip(&KeyFacts {
        records: vec![KeyRecord {
            generation: generation(3),
            stamp: stamp(5),
        }],
        tombstone: Some(stamp(6)),
        indexing: false,
    });
    round_trip(&KeyFacts {
        records: Vec::new(),
        tombstone: None,
        indexing: true,
    });
}

#[test]
fn every_seal_message_round_trips_under_its_own_tag() {
    let msgs = [
        (
            "prepare",
            SealMessage::Prepare {
                segment: block(1),
                ballot: ballot(2, 3),
            },
        ),
        (
            "accept",
            SealMessage::Accept {
                segment: block(1),
                ballot: ballot(2, 3),
                proposal: proposal(),
            },
        ),
        (
            "sealed",
            SealMessage::Sealed {
                segment: block(1),
                proposal: proposal(),
            },
        ),
    ];
    for (tag, m) in &msgs {
        round_trip(m);
        assert_eq!(serde_json::to_value(m).unwrap()["kind"], *tag);
    }
}

#[test]
fn every_seal_answer_round_trips() {
    for a in [
        SealAnswer::Promise {
            accepted: Some((ballot(1, 1), proposal())),
            copy: Some(CopyWire {
                committed: 10,
                verified: 8,
                running_crc: 99,
            }),
        },
        SealAnswer::Promise {
            accepted: None,
            copy: None,
        },
        SealAnswer::Ok,
        SealAnswer::Promised {
            ballot: ballot(4, 2),
        },
        SealAnswer::Committed {
            proposal: proposal(),
        },
    ] {
        round_trip(&a);
    }
}

#[test]
fn a_descriptor_record_round_trips_and_decodes_to_the_descriptor() {
    let d = descriptor();
    let rec = CommitRecord::descriptor(&d).unwrap();
    round_trip(&rec);
    assert_eq!(rec.as_descriptor().unwrap().unwrap(), d);
    assert!(rec.as_free().is_none());
}

#[test]
fn a_free_record_round_trips_and_decodes_to_the_free() {
    let f = free();
    let rec = CommitRecord::free(&f);
    round_trip(&rec);
    assert_eq!(rec.as_free().unwrap().unwrap(), f);
    assert!(rec.as_descriptor().is_none());
}

#[test]
fn a_descriptor_that_does_not_check_is_not_made_into_a_record() {
    let mut d = descriptor();
    d.blocks.clear();
    assert!(matches!(
        CommitRecord::descriptor(&d),
        Err(ProtoError::Format(FormatError::Invalid { .. }))
    ));
}

#[test]
fn corrupt_record_bytes_decode_to_a_format_error() {
    let bad = CommitRecord::Descriptor {
        bytes: HexBytes(vec![1, 2, 3]),
    };
    assert!(matches!(
        bad.as_descriptor(),
        Some(Err(ProtoError::Format(FormatError::Short { .. })))
    ));
    let bad = CommitRecord::Free {
        bytes: HexBytes(vec![1, 2, 3]),
    };
    assert!(matches!(
        bad.as_free(),
        Some(Err(ProtoError::Format(FormatError::Short { .. })))
    ));
}

#[test]
fn every_commit_answer_round_trips() {
    for a in [
        CommitAnswer::Durable,
        CommitAnswer::FreeWins,
        CommitAnswer::StaleRevision { held: 7 },
    ] {
        round_trip(&a);
    }
}

fn drop_record(origin: u8, seq: u64) -> LogRecord {
    LogRecord {
        origin: NodeId(origin),
        seq,
        entry: LogEntry::BlockDrop(BlockDrop {
            block: block(8),
            stamp: stamp(10),
        }),
    }
}

fn pull(origin: u8, after: u64) -> TombPull {
    TombPull {
        origin: NodeId(origin),
        after,
        limit: 100,
    }
}

#[test]
fn a_tomb_page_decodes_to_the_records_it_was_made_from() {
    round_trip(&pull(2, 5));
    let records = vec![
        drop_record(2, 6),
        LogRecord {
            origin: NodeId(2),
            seq: 7,
            entry: LogEntry::TombDrop(TombDrop {
                target_origin: NodeId(3),
                target_seq: 9,
            }),
        },
    ];
    let page = TombPage::new(5, &records, true).unwrap();
    round_trip(&page);
    assert_eq!(page.last, 7);
    assert!(page.more);
    assert_eq!(page.decode(&pull(2, 5)).unwrap(), records);
    let empty = TombPage::new(5, &[], false).unwrap();
    assert_eq!(empty.last, 5);
    assert!(empty.decode(&pull(2, 5)).unwrap().is_empty());
}

#[test]
fn a_tomb_page_of_another_origin_or_with_a_gap_or_a_wrong_last_fails() {
    let ok = TombPage::new(5, &[drop_record(2, 6), drop_record(2, 7)], false).unwrap();
    let invalid = |reason| {
        Err(ProtoError::Format(FormatError::Invalid {
            what: "tomb log entry",
            reason,
        }))
    };
    assert_eq!(
        ok.decode(&pull(3, 5)),
        invalid("a record of another origin")
    );
    let not_next = "a sequence that is not the previous plus one";
    assert_eq!(ok.decode(&pull(2, 4)), invalid(not_next));
    assert_eq!(ok.decode(&pull(2, 6)), invalid(not_next));
    let gap = TombPage::new(5, &[drop_record(2, 6), drop_record(2, 8)], false).unwrap();
    assert_eq!(gap.decode(&pull(2, 5)), invalid(not_next));
    let mut wrong_last = ok.clone();
    wrong_last.last = 8;
    let mismatch = Err(ProtoError::Format(FormatError::Invalid {
        what: "tomb page",
        reason: "last or entries do not match the records",
    }));
    assert_eq!(wrong_last.decode(&pull(2, 5)), mismatch);
    let mut empty_wrong_last = TombPage::new(5, &[], false).unwrap();
    empty_wrong_last.last = 6;
    assert_eq!(empty_wrong_last.decode(&pull(2, 5)), mismatch);
    // One entry holding two records is not two entries.
    let mut both = ok.entries[0].0.clone();
    both.extend_from_slice(&ok.entries[1].0);
    let page = TombPage {
        entries: vec![HexBytes(both)],
        last: 7,
        more: false,
    };
    assert_eq!(page.decode(&pull(2, 5)), mismatch);
}

#[test]
fn a_corrupt_tomb_entry_fails_the_whole_page() {
    let mut page = TombPage::new(5, &[drop_record(2, 6)], false).unwrap();
    page.entries.push(HexBytes(vec![0, 1]));
    assert!(matches!(
        page.decode(&pull(2, 5)),
        Err(ProtoError::Format(FormatError::Short { .. }))
    ));
}

#[test]
fn a_tomb_record_that_cannot_encode_fails_the_page() {
    let bad = LogRecord {
        origin: NodeId(1),
        seq: 1,
        entry: LogEntry::Tombstone(Tombstone {
            key: vec![0; 70_000],
            generation: generation(0),
            stamp: stamp(1),
            names: Vec::new(),
        }),
    };
    assert!(matches!(
        TombPage::new(0, &[bad], false),
        Err(ProtoError::Format(_))
    ));
}

#[test]
fn a_tomb_pull_is_the_query_origin_after_limit_in_that_order() {
    let p = TombPull {
        origin: NodeId(2),
        after: 18_446_744_073_709_551_615,
        limit: 4_294_967_295,
    };
    assert_eq!(
        p.query(),
        "origin=2&after=18446744073709551615&limit=4294967295"
    );
    assert_eq!(TombPull::from_query(&p.query()), Ok(p));
    let zero = TombPull {
        origin: NodeId(255),
        after: 0,
        limit: 0,
    };
    assert_eq!(zero.query(), "origin=255&after=0&limit=0");
    assert_eq!(TombPull::from_query("origin=255&after=0&limit=0"), Ok(zero));
}

#[test]
fn a_tomb_query_in_any_other_form_is_refused() {
    for q in [
        "",
        "origin=2",
        "origin=2&after=5",
        "after=5&origin=2&limit=1",
        "origin=2&limit=1&after=5",
        "origin=2&after=5&limit=1&x=1",
        "origin=2&after=5&limit=1&",
        "origin=2&after=5&limit=",
        "origin=&after=5&limit=1",
        "origin=02&after=5&limit=1",
        "origin=2&after=05&limit=1",
        "origin=2&after=5&limit=01",
        "origin=+2&after=5&limit=1",
        "origin=-2&after=5&limit=1",
        "origin=256&after=5&limit=1",
        "origin=2&after=18446744073709551616&limit=1",
        "origin=2&after=5&limit=4294967296",
        "origin=a&after=5&limit=1",
        "Origin=2&after=5&limit=1",
        "origin=2&after=5&limit=1 ",
    ] {
        assert_eq!(TombPull::from_query(q), Err(ProtoError::BadQuery), "{q:?}");
    }
    assert_eq!(ProtoError::BadQuery.to_string(), "query is malformed");
}

#[test]
fn keys_are_utf8_strings_on_the_wire() {
    let keys = LocateRequest {
        keys: vec!["bucket/\u{e9}t\u{e9}/\u{1f600}".to_owned()],
    };
    assert_eq!(
        serde_json::to_string(&keys).unwrap(),
        "{\"keys\":[\"bucket/\u{e9}t\u{e9}/\u{1f600}\"]}"
    );
    round_trip(&keys);
    assert!(serde_json::from_slice::<KeyCheck>(b"{\"key\":\"\xff\"}").is_err());
}

#[test]
fn a_ping_answer_round_trips() {
    round_trip(&PingAnswer {
        indexing: false,
        drives: vec![
            DrivePing {
                drive: DriveId([1; 16]),
                state: "up".into(),
                oldest_call_ms: Some(12),
                queued: [1, 2, 3, 4],
                streams: 2,
            },
            DrivePing {
                drive: DriveId([2; 16]),
                state: "stalled".into(),
                oldest_call_ms: None,
                queued: [0; 4],
                streams: 0,
            },
        ],
        hot_queued: 5,
    });
}

#[test]
fn every_replica_answer_round_trips_under_its_own_tag() {
    let cases = [
        ("acked", ReplicaAnswer::Acked { committed: 10 }),
        (
            "offset",
            ReplicaAnswer::Offset {
                start: 5,
                committed: 10,
            },
        ),
        (
            "crc",
            ReplicaAnswer::Crc {
                sent: 1,
                computed: 2,
            },
        ),
        (
            "ballot",
            ReplicaAnswer::Ballot {
                ballot: ballot(1, 1),
                promised: ballot(2, 2),
            },
        ),
        ("sealed", ReplicaAnswer::Sealed),
    ];
    for (tag, a) in &cases {
        round_trip(a);
        assert_eq!(serde_json::to_value(a).unwrap()["kind"], *tag);
    }
}

#[test]
fn a_replica_answer_converts_to_the_result_and_back_for_every_variant() {
    let results: Vec<Result<ReplicaAck, ReplicaReject>> = vec![
        Ok(ReplicaAck { committed: 10 }),
        Err(ReplicaReject::Offset {
            start: 5,
            committed: 10,
        }),
        Err(ReplicaReject::Crc {
            sent: 1,
            computed: 2,
        }),
        Err(ReplicaReject::Ballot {
            ballot: ballot(1, 1),
            promised: ballot(2, 2),
        }),
        Err(ReplicaReject::Sealed),
    ];
    let answers = [
        ReplicaAnswer::Acked { committed: 10 },
        ReplicaAnswer::Offset {
            start: 5,
            committed: 10,
        },
        ReplicaAnswer::Crc {
            sent: 1,
            computed: 2,
        },
        ReplicaAnswer::Ballot {
            ballot: ballot(1, 1),
            promised: ballot(2, 2),
        },
        ReplicaAnswer::Sealed,
    ];
    for (r, a) in results.into_iter().zip(answers) {
        assert_eq!(ReplicaAnswer::from(r), a);
        let back: Result<ReplicaAck, ReplicaReject> = a.into();
        assert_eq!(back, r);
    }
}

// ---- batch headers ----

fn batch() -> ReplicaBatch {
    ReplicaBatch {
        segment: block(11),
        ballot: ballot(3, 2),
        start: 4096,
        running_crc: 0x0000_00ab,
        stamp: stamp(555),
        bytes: Bytes::from_static(b"payload"),
    }
}

fn batch_headers() -> Vec<(String, String)> {
    BatchHeaders::of(NodeId(2), &batch()).encode()
}

#[test]
fn batch_headers_encode_to_the_documented_values_and_decode_back() {
    let h = BatchHeaders::of(NodeId(2), &batch());
    assert_eq!(h.from, NodeId(2));
    assert_eq!(h.start, 4096);
    let list = h.encode();
    let get = |n: &str| list.iter().find(|(k, _)| k == n).unwrap().1.clone();
    assert_eq!(get(header::FROM), "2");
    assert_eq!(get(header::BALLOT), "3.2");
    assert_eq!(get(header::OFFSET), "4096");
    assert_eq!(get(header::RUNNING_CRC), "000000ab");
    assert_eq!(get(header::SEGMENT), block(11).to_string());
    assert_eq!(BatchHeaders::decode(&list).unwrap(), h);
}

#[test]
fn each_missing_batch_header_is_named() {
    for name in [
        header::BALLOT,
        header::RUNNING_CRC,
        header::FROM,
        header::SEGMENT,
        header::OFFSET,
        header::STAMP,
    ] {
        assert_eq!(
            BatchHeaders::decode(&without(batch_headers(), name)),
            Err(ProtoError::MissingHeader(name)),
            "{name}"
        );
    }
}

#[test]
fn each_malformed_batch_header_is_named() {
    let bad: [(&str, &str); 14] = [
        (header::BALLOT, "3"),
        (header::BALLOT, "x.2"),
        (header::BALLOT, "3.x"),
        (header::BALLOT, "3.300"),
        (header::RUNNING_CRC, "abc"),
        (header::RUNNING_CRC, "000000AB"),
        (header::RUNNING_CRC, "0000000g"),
        (header::RUNNING_CRC, "000000abc"),
        (header::OFFSET, "-1"),
        (header::STAMP, "nope"),
        (header::SEGMENT, "123"),
        (header::FROM, "300"),
        (header::FROM, "x"),
        (header::OFFSET, ""),
    ];
    for (name, value) in bad {
        assert_eq!(
            BatchHeaders::decode(&with(batch_headers(), name, value)),
            Err(ProtoError::BadHeader(name)),
            "{name}={value}"
        );
    }
}

// ---- GET headers ----

fn get_headers(generation: Option<Generation>) -> GetHeaders {
    GetHeaders {
        generation,
        class: Class::Prepare,
        job: JobId(42),
        rank: Rank(7),
    }
}

#[test]
fn get_headers_round_trip_with_a_pinned_generation() {
    let h = get_headers(Some(generation(0xab)));
    let list = h.encode();
    assert!(list.contains(&("x-ts-class".into(), "prepare".into())));
    assert!(list.contains(&("x-ts-job".into(), "42".into())));
    assert!(list.contains(&("x-ts-rank".into(), "7".into())));
    assert!(list.contains(&("if-match".into(), format!("\"{}\"", generation(0xab)))));
    assert_eq!(GetHeaders::decode(&list).unwrap(), h);
}

#[test]
fn get_headers_round_trip_without_a_generation() {
    let h = get_headers(None);
    let list = h.encode();
    assert!(list.iter().all(|(n, _)| n != "if-match"));
    assert_eq!(GetHeaders::decode(&list).unwrap(), h);
}

#[test]
fn a_get_without_class_job_or_rank_is_background_job_zero_rank_zero() {
    let h = GetHeaders::decode(&[]).unwrap();
    assert_eq!(
        h,
        GetHeaders {
            generation: None,
            class: Class::Background,
            job: JobId(0),
            rank: Rank(0),
        }
    );
}

#[test]
fn malformed_get_headers_are_named() {
    let quoted = format!("\"{}\"", generation(1));
    let unquoted = generation(1).to_string();
    let open = format!("\"{unquoted}");
    let close = format!("{unquoted}\"");
    let (unquoted, open, close) = (unquoted.as_str(), open.as_str(), close.as_str());
    let cases: Vec<(Vec<(String, String)>, &str)> = vec![
        (pairs(&[("x-ts-class", "urgent")]), "x-ts-class"),
        (pairs(&[("if-match", unquoted)]), "if-match"),
        (pairs(&[("if-match", "\"zz\"")]), "if-match"),
        (pairs(&[("if-match", open)]), "if-match"),
        (pairs(&[("if-match", close)]), "if-match"),
        (pairs(&[("x-ts-job", "-1")]), "x-ts-job"),
        (pairs(&[("x-ts-rank", "x")]), "x-ts-rank"),
    ];
    for (list, name) in cases {
        assert_eq!(
            GetHeaders::decode(&list),
            Err(ProtoError::BadHeader(name)),
            "{list:?}"
        );
    }
    assert!(GetHeaders::decode(&pairs(&[("if-match", quoted.as_str())])).is_ok());
}

// ---- response headers ----

#[test]
fn response_headers_with_every_signal_round_trip() {
    let r = ResponseHeaders {
        generation: generation(9),
        signals: DriveSignals {
            queue_wait_ms: Some(5),
            service_ms: Some(6),
            drive_queue: Some(7),
            drive_state: Some(DriveState::Stalled),
        },
    };
    let list = r.encode();
    assert_eq!(list.len(), 5);
    assert!(list.contains(&("x-ts-drive-state".into(), "stalled".into())));
    assert_eq!(ResponseHeaders::decode(&list).unwrap(), r);
}

#[test]
fn response_headers_without_signals_carry_only_the_etag() {
    let r = ResponseHeaders {
        generation: generation(9),
        signals: DriveSignals::default(),
    };
    let list = r.encode();
    assert_eq!(
        list,
        vec![("etag".to_owned(), format!("\"{}\"", generation(9)))]
    );
    assert_eq!(ResponseHeaders::decode(&list).unwrap(), r);
}

#[test]
fn response_headers_name_what_is_missing_or_malformed() {
    let etag = format!("\"{}\"", generation(1));
    let etag = etag.as_str();
    let bare = generation(1).to_string();
    assert_eq!(
        ResponseHeaders::decode(&[]),
        Err(ProtoError::MissingHeader("etag"))
    );
    for bad in ["zz", "\"zz\"", bare.as_str()] {
        assert_eq!(
            ResponseHeaders::decode(&pairs(&[("etag", bad)])),
            Err(ProtoError::BadHeader("etag")),
            "{bad}"
        );
    }
    for (name, value) in [
        ("x-ts-drive-state", "asleep"),
        ("x-ts-queue-wait-ms", "x"),
        ("x-ts-service-ms", "-1"),
        ("x-ts-drive-queue", "4294967296"),
    ] {
        assert_eq!(
            ResponseHeaders::decode(&pairs(&[("etag", etag), (name, value)])),
            Err(ProtoError::BadHeader(match name {
                "x-ts-drive-state" => header::DRIVE_STATE,
                "x-ts-queue-wait-ms" => header::QUEUE_WAIT_MS,
                "x-ts-service-ms" => header::SERVICE_MS,
                _ => header::DRIVE_QUEUE,
            })),
            "{name}={value}"
        );
    }
}

// ---- sources ----

fn source(node: u8, drive: u8, index: u8) -> SourceShard {
    SourceShard {
        node: NodeId(node),
        drive: DriveId([drive; 16]),
        index,
    }
}

#[test]
fn sources_encode_joined_by_commas_and_decode_back() {
    let s = vec![source(1, 1, 0), source(2, 2, 5)];
    let text = encode_sources(&s);
    assert_eq!(
        text,
        format!("1:{}:0,2:{}:5", DriveId([1; 16]), DriveId([2; 16]))
    );
    assert_eq!(decode_sources(&text).unwrap(), s);
    assert_eq!(
        decode_sources(&encode_sources(&s[..1])).unwrap(),
        s[..1].to_vec()
    );
}

#[test]
fn no_sources_encode_to_the_empty_value_and_decode_back() {
    assert_eq!(encode_sources(&[]), "");
    assert_eq!(decode_sources(""), Ok(Vec::new()));
    assert_eq!(decode_sources(&encode_sources(&[])), Ok(Vec::new()));
}

#[test]
fn malformed_sources_are_refused() {
    let d = DriveId([1; 16]);
    let bad = [
        ",".to_owned(),
        format!("1:{d}:0,"),
        "1".to_owned(),
        format!("1:{d}"),
        format!("1:{d}:0:9"),
        format!("x:{d}:0"),
        "1:nothex:0".to_owned(),
        format!("1:{d}:256"),
        format!("1:{d}:x"),
    ];
    for text in bad {
        assert_eq!(
            decode_sources(&text),
            Err(ProtoError::BadHeader(header::SOURCES)),
            "{text}"
        );
    }
}

// ---- errors ----

#[test]
fn proto_errors_display_what_was_wrong() {
    assert_eq!(
        ProtoError::MissingHeader("x-ts-ballot").to_string(),
        "header x-ts-ballot is missing"
    );
    assert_eq!(
        ProtoError::BadHeader("x-ts-offset").to_string(),
        "header x-ts-offset is malformed"
    );
    assert_eq!(ProtoError::BadHex.to_string(), "not lowercase hex");
    let f = FormatError::Short {
        what: "w",
        need: 4,
        have: 1,
    };
    assert_eq!(ProtoError::from(f.clone()).to_string(), f.to_string());
}
