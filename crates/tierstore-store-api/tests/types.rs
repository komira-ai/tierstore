//! Proves the small behaviours of the storage tier's interface types: the hand-written `Debug`
//! of the types that hold a stream, the mapping of a fetch class to a drive class, and the text of
//! every error a store, scheduler, placement, index or peer call can return.
//!
//! Catches:
//! - a `Debug` of a stream-holding type that prints the stream, or hides the identifying fields;
//! - a fetch class mapped to another drive class (a prepare read queued as demand);
//! - an error variant whose message drops the field an operator needs (node, drive, offset,
//!   retry delay, generation).

use bytes::Bytes;
use futures::StreamExt;
use tierstore_fetch_api::ids::{ByteRange, Class};
use tierstore_fetch_api::upstream::{DriveSignals, DriveState};
use tierstore_format::descriptor::{Code, ShardLoc};
use tierstore_format::footer::{FooterSubject, ShardFooter};
use tierstore_format::free::StripeFree;
use tierstore_format::ids::{BlockId, DriveId, ExpiryClass, Generation, NodeId, StripeId};
use tierstore_format::names::ColdName;
use tierstore_format::seal::{Ballot, SealProposal};
use tierstore_format::stamp::Stamp;
use tierstore_store_api::cold::{ColdError, ColdRead, ShardWrite};
use tierstore_store_api::hot::{HotError, LargePut, ReplicaReject, SealRefused, Unavailable};
use tierstore_store_api::index::IndexError;
use tierstore_store_api::peer::{PeerBody, PeerError};
use tierstore_store_api::placement::PlacementError;
use tierstore_store_api::sched::{DriveClass, DriveError, IoKind, Refused};

fn drive() -> DriveId {
    DriveId([0xab; 16])
}

fn name() -> ColdName {
    ColdName::Copy(BlockId::from_raw(0x10))
}

fn stripe() -> StripeId {
    StripeId::from_raw(0x2a)
}

fn range() -> ByteRange {
    ByteRange { offset: 8, len: 16 }
}

#[test]
fn a_fetch_class_maps_to_the_drive_class_of_the_same_name() {
    assert_eq!(DriveClass::from(Class::Demand), DriveClass::Demand);
    assert_eq!(DriveClass::from(Class::Prepare), DriveClass::Prepare);
    assert_eq!(DriveClass::from(Class::Background), DriveClass::Background);
}

#[test]
fn drive_classes_order_demand_before_write_before_prepare_before_background() {
    assert!(DriveClass::Demand < DriveClass::Write);
    assert!(DriveClass::Write < DriveClass::Prepare);
    assert!(DriveClass::Prepare < DriveClass::Background);
}

#[test]
fn the_debug_of_an_io_kind_names_its_file_and_range_but_not_its_body() {
    let read = format!(
        "{:?}",
        IoKind::Read {
            file: name(),
            range: range()
        }
    );
    assert!(read.starts_with("Read { file: Copy("), "{read}");
    assert!(
        read.contains("range: ByteRange { offset: 8, len: 16 }"),
        "{read}"
    );

    let small = format!(
        "{:?}",
        IoKind::SmallRead {
            file: name(),
            range: range()
        }
    );
    assert!(small.starts_with("SmallRead { file: Copy("), "{small}");

    let write = format!(
        "{:?}",
        IoKind::Write {
            file: name(),
            len: 99,
            body: futures::stream::empty().boxed(),
        }
    );
    assert!(write.starts_with("Write { file: Copy("), "{write}");
    assert!(write.contains("len: 99"), "{write}");
    assert!(write.ends_with(".. }"), "{write}");
    assert!(!write.contains("body"), "{write}");
}

#[test]
fn the_debug_of_a_large_put_omits_the_body() {
    let put = LargePut {
        key: b"k".to_vec(),
        size: Some(5),
        expiry: ExpiryClass(2),
        stamp: Stamp::ZERO,
        body: futures::stream::empty().boxed(),
    };
    let text = format!("{put:?}");
    assert!(
        text.starts_with("LargePut { key: [107], size: Some(5), expiry: ExpiryClass(2)"),
        "{text}"
    );
    assert!(!text.contains("body"), "{text}");
}

#[test]
fn the_debug_of_a_shard_write_omits_the_body_and_names_drive_file_and_class() {
    let footer = ShardFooter {
        subject: FooterSubject::Copy {
            block: BlockId::from_raw(0x10),
            length: 0,
            expiry: ExpiryClass(0),
            copies: Vec::new(),
        },
        data_len: 0,
        cell_crcs: Vec::new(),
        blake3: [0; 32],
    };
    let w = ShardWrite {
        drive: drive(),
        name: name(),
        body: futures::stream::empty().boxed(),
        footer,
        class: DriveClass::Write,
    };
    let text = format!("{w:?}");
    assert!(text.starts_with("ShardWrite { drive: "), "{text}");
    assert!(text.contains("class: Write"), "{text}");
    assert!(text.contains("footer: ShardFooter"), "{text}");
    assert!(!text.contains("body"), "{text}");
}

#[test]
fn the_debug_of_a_cold_read_shows_signals_only() {
    let r = ColdRead {
        signals: DriveSignals {
            queue_wait_ms: Some(3),
            ..DriveSignals::default()
        },
        body: futures::stream::empty().boxed(),
    };
    let text = format!("{r:?}");
    assert!(
        text.starts_with("ColdRead { signals: DriveSignals { queue_wait_ms: Some(3)"),
        "{text}"
    );
    assert!(!text.contains("body"), "{text}");
}

#[test]
fn the_debug_of_a_peer_body_shows_generation_and_signals_only() {
    let b = PeerBody {
        generation: Generation([1; 16]),
        signals: DriveSignals::default(),
        body: futures::stream::empty::<Result<Bytes, PeerError>>().boxed(),
    };
    let text = format!("{b:?}");
    assert!(
        text.starts_with("PeerBody { generation: Generation("),
        "{text}"
    );
    assert!(text.contains("signals: DriveSignals"), "{text}");
    assert!(!text.contains("body"), "{text}");
}

fn ballot(round: u32, node: u8) -> Ballot {
    Ballot {
        round,
        node: NodeId(node),
    }
}

#[test]
fn replica_rejections_say_why() {
    assert_eq!(
        ReplicaReject::Offset {
            start: 5,
            committed: 9
        }
        .to_string(),
        "batch starts at 5, the copy has 9 committed"
    );
    assert_eq!(
        ReplicaReject::Crc {
            sent: 0xab,
            computed: 0xcd
        }
        .to_string(),
        "running crc32c 0x000000ab does not match 0x000000cd"
    );
    assert_eq!(
        ReplicaReject::Ballot {
            ballot: ballot(1, 2),
            promised: ballot(3, 4)
        }
        .to_string(),
        "ballot Ballot { round: 1, node: NodeId(2) } is below the promise Ballot { round: 3, node: NodeId(4) }"
    );
    assert_eq!(ReplicaReject::Sealed.to_string(), "the segment is sealed");
}

#[test]
fn seal_refusals_say_why() {
    assert_eq!(
        SealRefused::Promised {
            promised: ballot(3, 4)
        }
        .to_string(),
        "promised Ballot { round: 3, node: NodeId(4) }"
    );
    assert_eq!(
        SealRefused::Committed(SealProposal {
            length: 7,
            running_crc: 1
        })
        .to_string(),
        "committed at SealProposal { length: 7, running_crc: 1 }"
    );
}

#[test]
fn hot_errors_say_what_failed() {
    let unavailable = HotError::Unavailable {
        reason: Unavailable::Minority,
        retry_after_ms: 250,
    };
    assert_eq!(
        unavailable.to_string(),
        "unavailable (Minority), retry after 250 ms"
    );
    let conflict = HotError::Conflict {
        existing: Generation([0x0f; 16]),
    };
    assert_eq!(
        conflict.to_string(),
        "the key holds generation 0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f"
    );
    assert_eq!(HotError::Full.to_string(), "the hot tier is full");
    assert_eq!(
        HotError::GenerationMismatch { node: NodeId(3) }.to_string(),
        "node 3: generation mismatch"
    );
    let io = HotError::Io {
        op: "fdatasync",
        source: std::io::Error::other("disk gone"),
    };
    assert_eq!(io.to_string(), "fdatasync: disk gone");
    assert!(std::error::Error::source(&io).is_some());
}

#[test]
fn drive_and_refusal_errors_say_what_failed() {
    assert_eq!(DriveError::Stalled.to_string(), "the drive is stalled");
    assert!(
        DriveError::Eio { file: name() }
            .to_string()
            .starts_with("I/O error on Copy(")
    );
    assert_eq!(
        DriveError::ParkedOut.to_string(),
        "the parked stream was ended"
    );
    assert_eq!(
        Refused::QueueFull { retry_after_ms: 40 }.to_string(),
        "queue full, retry after 40 ms"
    );
    assert_eq!(
        Refused::State(DriveState::Stalled).to_string(),
        "drive is Stalled"
    );
}

#[test]
fn cold_errors_name_the_drive_and_what_failed() {
    let d = drive().to_string();
    assert_eq!(
        ColdError::Refused {
            drive: drive(),
            refused: Refused::State(DriveState::Failed)
        }
        .to_string(),
        format!("drive {d}: drive is Failed")
    );
    assert_eq!(
        ColdError::Drive {
            drive: drive(),
            error: DriveError::Stalled
        }
        .to_string(),
        format!("drive {d}: the drive is stalled")
    );
    assert!(
        ColdError::FooterMismatch {
            drive: drive(),
            name: name()
        }
        .to_string()
        .ends_with("does not match its footer")
    );
    let corrupt = ColdError::Corrupt {
        drive: drive(),
        name: name(),
        offset: 4096,
    }
    .to_string();
    assert!(
        corrupt.starts_with(&format!("drive {d}: Copy(")),
        "{corrupt}"
    );
    assert!(
        corrupt.ends_with("failed its cell crc at 4096"),
        "{corrupt}"
    );
    let missing = ColdError::NotFound {
        drive: drive(),
        name: name(),
    }
    .to_string();
    assert!(
        missing.starts_with(&format!("drive {d}: no Copy(")),
        "{missing}"
    );
    assert_eq!(
        ColdError::Unreadable {
            block: BlockId::from_raw(0x10)
        }
        .to_string(),
        "block 0000000000000010: fewer than k shards readable"
    );
    let stale = ColdError::StaleFree(StripeFree {
        stripe: stripe(),
        revision: 3,
        owner: NodeId(1),
        stamp: Stamp::ZERO,
    })
    .to_string();
    assert!(stale.starts_with("stale free of StripeFree {"), "{stale}");
}

#[test]
fn placement_errors_name_the_stripe_and_the_shortage() {
    assert_eq!(
        PlacementError::TooFewDomains {
            stripe: stripe(),
            live: 2
        }
        .to_string(),
        "stripe 000000000000002a: 2 live domains cannot hold the code safely"
    );
    assert_eq!(
        PlacementError::TooFewDrives {
            stripe: stripe(),
            node: NodeId(5)
        }
        .to_string(),
        "stripe 000000000000002a: node 5 has too few drives below the fill limit"
    );
}

#[test]
fn index_errors_say_what_failed() {
    assert_eq!(IndexError::Indexing.to_string(), "the index is being built");
    assert_eq!(
        IndexError::Store {
            op: "put",
            reason: "full".into()
        }
        .to_string(),
        "index store: put: full"
    );
}

#[test]
fn peer_errors_name_the_node_and_what_failed() {
    let n = NodeId(4);
    assert_eq!(
        PeerError::Unreachable {
            node: n,
            reason: "reset".into()
        }
        .to_string(),
        "node 4: unreachable (reset)"
    );
    let signals = DriveSignals {
        drive_state: Some(DriveState::Indexing),
        ..DriveSignals::default()
    };
    let unavailable = PeerError::Unavailable {
        node: n,
        retry_after_ms: 90,
        signals,
    }
    .to_string();
    assert!(
        unavailable.starts_with("node 4: unavailable, retry after 90 ms (DriveSignals {"),
        "{unavailable}"
    );
    assert!(unavailable.contains("Indexing"), "{unavailable}");
    assert_eq!(
        PeerError::PreconditionFailed {
            node: n,
            actual: None
        }
        .to_string(),
        "node 4: holds generation None"
    );
    assert_eq!(
        PeerError::NotFound { node: n }.to_string(),
        "node 4: not found"
    );
    assert_eq!(
        PeerError::Corrupt {
            node: n,
            offset: 12
        }
        .to_string(),
        "node 4: crc mismatch at 12"
    );
    assert_eq!(
        PeerError::Protocol {
            node: n,
            status: 418,
            reason: "teapot".into()
        }
        .to_string(),
        "node 4: status 418: teapot"
    );
}

#[test]
fn a_shard_location_names_a_node_and_a_drive() {
    let loc = ShardLoc {
        node: NodeId(1),
        drive: drive(),
    };
    assert_eq!(Code::Replicas(2).width(), 2);
    assert_eq!(loc.node, NodeId(1));
}
