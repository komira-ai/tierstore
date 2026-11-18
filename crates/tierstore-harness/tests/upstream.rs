//! `CellUpstream` delivers a part's bytes one cell-piece at a time, only when released, in any
//! order across parts, with scripted corruption and failures, and counts what it delivered.
//!
//! Catches:
//! - a piece yielded before its release, or a body that is not woken by the release;
//! - pieces that are not the intersection of the range with one cell, or are out of order;
//! - a corruption that flips more or fewer than one byte, or a byte of another cell;
//! - a script that is applied to the wrong request, or not consumed;
//! - an `If-Match` of another generation that is served;
//! - accounting that disagrees with what the bodies yielded.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "an integration test: a failed unwrap, expect or index is the failure it reports"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use bytes::Bytes;
use futures::{StreamExt, poll};
use tierstore_fetch_api::ids::{ByteRange, Class, JobId, PackKey, Rank};
use tierstore_fetch_api::upstream::{
    DeviceRef, DriveSignals, DriveState, GetRequest, Located, Location, PartResponse, Report,
    Source, SourceKind, Upstream, UpstreamError,
};
use tierstore_format::ids::{DriveId, Generation, NodeId};
use tierstore_harness::upstream::{CellUpstream, FakeObject, PartId, Script};

const CELL: u64 = 4;
const LEN: u64 = 30;

fn key() -> PackKey {
    PackKey("pack".to_owned())
}

fn gen1() -> Generation {
    Generation([1; 16])
}

fn gen2() -> Generation {
    Generation([2; 16])
}

fn data() -> Vec<u8> {
    (0..LEN as u8).collect()
}

/// The device a request goes to unless a test names another.
const DEV: DeviceRef = DeviceRef::Hot(NodeId(9));

fn source(device: DeviceRef) -> Source {
    Source {
        device,
        kind: SourceKind::Hot {
            file: "seg".to_owned(),
            offset: 0,
        },
        extent: ByteRange {
            offset: 0,
            len: LEN,
        },
    }
}

fn location() -> Location {
    Location {
        generation: gen1(),
        size: Some(LEN),
        sources: vec![source(DEV)],
        conflicts: vec![],
    }
}

fn upstream() -> CellUpstream {
    let u = CellUpstream::new(CELL);
    u.insert(
        key(),
        FakeObject {
            generation: gen1(),
            bytes: Bytes::from(data()),
            location: location(),
        },
    );
    u
}

fn request_from(device: DeviceRef, offset: u64, len: u64) -> GetRequest {
    GetRequest {
        key: key(),
        source: source(device),
        range: ByteRange { offset, len },
        generation: gen1(),
        class: Class::Demand,
        job: JobId(1),
        rank: Rank(0),
    }
}

fn request(offset: u64, len: u64) -> GetRequest {
    request_from(DEV, offset, len)
}

async fn part(u: &CellUpstream, offset: u64, len: u64) -> PartResponse {
    u.get(request(offset, len)).await.unwrap()
}

async fn collect(r: PartResponse) -> Vec<u8> {
    r.body.map(|c| c.unwrap().to_vec()).concat().await
}

struct Counting(AtomicUsize);

impl Wake for Counting {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Part 2 delivers its whole range while part 1, requested first, has delivered nothing; part 1
/// then delivers its own bytes in order.
#[tokio::test]
async fn parts_complete_in_reverse_order() {
    let u = upstream();
    let mut p1 = part(&u, 0, 8).await;
    let p2 = part(&u, 8, 8).await;
    u.release_all(PartId(1));
    assert_eq!(collect(p2).await, data()[8..16]);
    assert_eq!(u.progress().count("piece"), 2);
    assert!(poll!(p1.body.next()).is_pending());
    assert_eq!(u.pending_pieces(PartId(0)), 2);
    u.release_all(PartId(0));
    assert_eq!(collect(p1).await, data()[0..8]);
}

/// Nothing is yielded before its release; each release lets exactly that many pieces through; the
/// release wakes the waiting body once, and a release of an unpolled body wakes nobody.
#[tokio::test]
async fn a_piece_is_yielded_only_after_its_release() {
    let u = upstream();
    let mut p = part(&u, 0, 12).await;
    u.release(PartId(0), 0);
    let counting = Arc::new(Counting(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counting));
    let mut cx = Context::from_waker(&waker);
    assert!(p.body.poll_next_unpin(&mut cx).is_pending());
    assert_eq!(counting.0.load(Ordering::SeqCst), 0);
    u.release(PartId(0), 1);
    assert_eq!(counting.0.load(Ordering::SeqCst), 1);
    let Poll::Ready(Some(Ok(first))) = p.body.poll_next_unpin(&mut cx) else {
        panic!("the released piece is ready");
    };
    assert_eq!(first.to_vec(), data()[0..4]);
    assert!(p.body.poll_next_unpin(&mut cx).is_pending());
    u.release(PartId(0), 1);
    u.release(PartId(0), 1);
    assert_eq!(
        counting.0.load(Ordering::SeqCst),
        2,
        "the second release had no waiter left to wake"
    );
    assert_eq!(u.pending_pieces(PartId(0)), 2);
    assert_eq!(collect(p).await, data()[4..12]);
    assert_eq!(u.pending_pieces(PartId(0)), 0);
}

/// An unaligned range yields a partial first piece, whole middle pieces and a partial last piece,
/// each inside one cell.
#[tokio::test]
async fn an_unaligned_range_has_partial_first_and_last_pieces() {
    let u = upstream();
    let mut p = part(&u, 2, 13).await;
    u.release_all(PartId(0));
    let mut sizes = Vec::new();
    while let Some(piece) = p.body.next().await {
        sizes.push(piece.unwrap().len());
    }
    assert_eq!(sizes, [2, 4, 4, 3]);
    assert_eq!(u.pending_pieces(PartId(0)), 0);
    let cells: Vec<u64> = u.cell_deliveries().keys().map(|(_, c)| *c).collect();
    assert_eq!(cells, [0, 1, 2, 3]);
}

/// A zero-length range has no piece: the body ends at once.
#[tokio::test]
async fn an_empty_range_ends_at_once() {
    let u = upstream();
    let p = part(&u, 5, 0).await;
    assert_eq!(u.pending_pieces(PartId(0)), 0);
    assert!(collect(p).await.is_empty());
}

/// Corruption flips exactly one byte, inside the scripted cell; a range that does not cover the
/// cell is served intact.
#[tokio::test]
async fn corruption_flips_one_byte_of_that_cell() {
    let u = upstream();
    u.script_global(Script::CorruptCell { cell: 2 });
    let p = part(&u, 0, LEN).await;
    u.release_all(PartId(0));
    let got = collect(p).await;
    let diff: Vec<usize> = (0..got.len()).filter(|i| got[*i] != data()[*i]).collect();
    assert_eq!(diff.len(), 1);
    assert!((8..12).contains(&diff[0]), "the byte is in cell 2");
    u.script_global(Script::CorruptCell { cell: 5 });
    let q = part(&u, 0, 8).await;
    u.release_all(PartId(1));
    assert_eq!(collect(q).await, data()[0..8]);
}

/// `FailAfter` yields that many pieces, then one transport error, then the end; a count above the
/// piece count still ends in the error.
#[tokio::test]
async fn fail_after_yields_pieces_then_a_transport_error() {
    let u = upstream();
    u.script_global(Script::FailAfter { pieces: 2 });
    u.script_global(Script::FailAfter { pieces: 99 });
    let mut p = part(&u, 0, 12).await;
    u.release_all(PartId(0));
    assert_eq!(p.body.next().await.unwrap().unwrap().to_vec(), data()[0..4]);
    assert_eq!(p.body.next().await.unwrap().unwrap().to_vec(), data()[4..8]);
    assert!(matches!(
        p.body.next().await,
        Some(Err(UpstreamError::Transport { .. }))
    ));
    assert!(p.body.next().await.is_none());
    let mut q = part(&u, 0, 8).await;
    u.release_all(PartId(1));
    assert!(q.body.next().await.unwrap().is_ok());
    assert!(q.body.next().await.unwrap().is_ok());
    assert!(q.body.next().await.unwrap().is_err());
}

/// Every scripted request failure comes at once from `get`, is logged, and takes a part id.
#[tokio::test]
async fn scripted_failures_answer_the_request() {
    let u = upstream();
    let dev = DEV;
    let errors = [
        UpstreamError::PreconditionFailed {
            key: key(),
            device: dev,
            actual: Some(gen2()),
        },
        UpstreamError::Unavailable {
            key: key(),
            device: dev,
            retry_after_ms: 50,
            signals: DriveSignals {
                drive_state: Some(DriveState::Stalled),
                ..DriveSignals::default()
            },
        },
        UpstreamError::NotFound {
            key: key(),
            device: dev,
        },
        UpstreamError::Transport {
            key: key(),
            device: dev,
            reason: "reset".to_owned(),
        },
    ];
    for e in &errors {
        u.script_global(Script::Fail(e.clone()));
    }
    for e in &errors {
        assert_eq!(u.get(request(0, 4)).await.unwrap_err(), *e);
    }
    assert_eq!(u.gets().len(), 4);
    assert_eq!(u.progress().count("get"), 4);
    assert_eq!(u.pending_pieces(PartId(3)), 0);
    assert!(!u.is_cancelled(PartId(3)));
    u.release_all(PartId(3));
    assert!(u.get(request(0, 4)).await.is_ok(), "the queue is consumed");
}

/// A hung body never yields, whatever is released, and dropping it is a cancel.
#[tokio::test]
async fn a_hung_body_never_yields() {
    let u = upstream();
    u.script_global(Script::Hang);
    let mut p = part(&u, 0, 8).await;
    u.release_all(PartId(0));
    for _ in 0..3 {
        assert!(poll!(p.body.next()).is_pending());
    }
    assert_eq!(u.bytes_delivered(), 0);
    drop(p);
    assert!(u.is_cancelled(PartId(0)));
}

/// A request that names another generation is a 412 carrying the object's; an unknown key is
/// `NotFound`; a range outside the object, or one whose end overflows, is a transport error.
#[tokio::test]
async fn requests_are_checked_like_the_fake_upstream() {
    let u = upstream();
    let mut other = request(0, 4);
    other.generation = gen2();
    assert_eq!(
        u.get(other).await.unwrap_err(),
        UpstreamError::PreconditionFailed {
            key: key(),
            device: DEV,
            actual: Some(gen1()),
        }
    );
    let mut unknown = request(0, 4);
    unknown.key = PackKey("nope".to_owned());
    assert!(matches!(
        u.get(unknown).await,
        Err(UpstreamError::NotFound { .. })
    ));
    assert!(matches!(
        u.get(request(28, 4)).await,
        Err(UpstreamError::Transport { .. })
    ));
    assert!(matches!(
        u.get(request(u64::MAX, 2)).await,
        Err(UpstreamError::Transport { .. })
    ));
}

/// `ServeAs` answers with the other generation despite `If-Match`, with the object's bytes.
#[tokio::test]
async fn another_generation_can_be_served() {
    let u = upstream();
    u.script_global(Script::ServeAs(gen2()));
    let p = part(&u, 0, 8).await;
    assert_eq!(p.generation, gen2());
    u.release_all(PartId(0));
    assert_eq!(collect(p).await, data()[0..8]);
}

/// Serve scripts carry their signals; the default is no signals.
#[tokio::test]
async fn serve_carries_its_signals() {
    let u = upstream();
    let signals = DriveSignals {
        service_ms: Some(7),
        ..DriveSignals::default()
    };
    u.script_global(Script::Serve(signals));
    assert_eq!(part(&u, 0, 4).await.signals, signals);
    assert_eq!(part(&u, 0, 4).await.signals, DriveSignals::default());
}

/// Parts that cover the object once deliver each cell once and `bytes_delivered` equals the
/// bytes needed; a cell split between two parts is delivered twice, once per piece; the progress
/// counts agree.
#[tokio::test]
async fn accounting_agrees_with_what_was_yielded() {
    let u = upstream();
    let parts = [
        part(&u, 0, 12).await,
        part(&u, 12, 12).await,
        part(&u, 24, 6).await,
    ];
    for i in 0..3 {
        u.release_all(PartId(i));
    }
    for p in parts {
        collect(p).await;
    }
    assert_eq!(u.bytes_delivered(), LEN);
    let cells = u.cell_deliveries();
    assert_eq!(cells.len(), 8);
    assert!(cells.values().all(|n| *n == 1));
    assert_eq!(u.progress().count("piece"), 8);
    u.progress().reached("get", 3).await;
    assert_eq!(u.gets().len(), 3);
    assert_eq!(
        u.gets()[1].range,
        ByteRange {
            offset: 12,
            len: 12
        }
    );

    let v = upstream();
    let a = part(&v, 0, 6).await;
    let b = part(&v, 6, 6).await;
    v.release_all(PartId(0));
    v.release_all(PartId(1));
    collect(a).await;
    collect(b).await;
    assert_eq!(v.cell_deliveries()[&(key(), 1)], 2);
    assert_eq!(v.bytes_delivered(), 12);
}

/// Dropping a body before its end cancels the part, and the bytes it never yielded are not
/// counted; a body read to its end is not cancelled; a body dropped unpolled is cancelled.
#[tokio::test]
async fn dropping_a_body_early_cancels_the_part() {
    let u = upstream();
    let mut a = part(&u, 0, 12).await;
    let b = part(&u, 0, 4).await;
    let c = part(&u, 0, 4).await;
    u.release(PartId(0), 1);
    a.body.next().await.unwrap().unwrap();
    drop(a);
    assert!(u.is_cancelled(PartId(0)));
    assert_eq!(u.bytes_delivered(), 4);
    assert_eq!(u.pending_pieces(PartId(0)), 2);
    u.release_all(PartId(1));
    collect(b).await;
    assert!(!u.is_cancelled(PartId(1)));
    drop(c);
    assert!(u.is_cancelled(PartId(2)));
}

/// `release_in_order` releases one piece per listed entry, in the order listed, across parts.
#[tokio::test]
async fn release_in_order_spans_parts() {
    let u = upstream();
    let mut p1 = part(&u, 0, 8).await;
    let mut p2 = part(&u, 8, 8).await;
    u.release_in_order(&[PartId(1), PartId(0), PartId(1)]);
    assert_eq!(u.progress().count("piece"), 0, "a release yields nothing");
    assert!(p2.body.next().await.unwrap().is_ok());
    assert!(p1.body.next().await.unwrap().is_ok());
    assert!(p2.body.next().await.unwrap().is_ok());
    assert!(poll!(p1.body.next()).is_pending());
    assert_eq!(u.pending_pieces(PartId(0)), 1);
    assert_eq!(u.pending_pieces(PartId(1)), 0);
}

/// A script of the request's own (key, device) is taken before the global queue; a request from
/// another device takes the global one; each script is used once.
#[tokio::test]
async fn device_scripts_beat_the_global_queue() {
    let u = upstream();
    let hot = DeviceRef::Hot(NodeId(1));
    let drive = DeviceRef::Drive(NodeId(2), DriveId([3; 16]));
    u.script(key(), hot, Script::ServeAs(gen2()));
    u.script_global(Script::ServeAs(Generation([9; 16])));
    let from_drive = u.get(request_from(drive, 0, 4)).await.unwrap();
    assert_eq!(from_drive.generation, Generation([9; 16]));
    let from_hot = u.get(request_from(hot, 0, 4)).await.unwrap();
    assert_eq!(from_hot.generation, gen2());
    assert_eq!(
        u.get(request_from(hot, 0, 4)).await.unwrap().generation,
        gen1()
    );
}

/// `locate` answers scripted answers first, in order, then the object's location, or a definitive
/// miss for an unknown key; every call is logged, and reports are logged in order.
#[tokio::test]
async fn locate_and_report_follow_their_scripts() {
    let u = upstream();
    u.script_locate(
        key(),
        Ok(Located::NotDefinitive(
            tierstore_fetch_api::upstream::NotDefinitive::Indexing,
        )),
    );
    u.script_locate(
        key(),
        Err(UpstreamError::NotFound {
            key: key(),
            device: DEV,
        }),
    );
    assert!(matches!(
        u.locate(&key()).await,
        Ok(Located::NotDefinitive(_))
    ));
    assert!(u.locate(&key()).await.is_err());
    assert_eq!(u.locate(&key()).await.unwrap(), Located::Found(location()));
    let other = PackKey("other".to_owned());
    assert_eq!(u.locate(&other).await.unwrap(), Located::DefinitiveMiss);
    assert_eq!(u.locates(), [key(), key(), key(), other]);
    let range = ByteRange { offset: 0, len: 4 };
    u.report(&key(), &source(DEV), range, Report::Bitrot).await;
    assert_eq!(u.reports(), [(key(), source(DEV), range, Report::Bitrot)]);
}

/// The cell size is what the upstream was made with, and it must be a power of two.
#[test]
#[should_panic(expected = "not a power of two")]
fn a_cell_size_that_is_not_a_power_of_two_is_refused() {
    assert_eq!(CellUpstream::new(8).cell_size(), 8);
    let _ = CellUpstream::new(6);
}

/// Releasing a part that was never requested is a test bug and panics with its id.
#[test]
#[should_panic(expected = "no part 3 was requested")]
fn releasing_an_unknown_part_panics() {
    CellUpstream::new(CELL).release(PartId(3), 1);
}
