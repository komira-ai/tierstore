//! The adapter of `TreeFs` between the reader protocol of `tierstore-tree` and `tsfuse`'s reply
//! path, a step at a time and with no tree: what the reader protocol does with a staged reply it
//! cannot always make happen in a tree (a recheck that fails after the bytes are read; a `pread`
//! that fails). It proves:
//! - a staged reply that is discarded gives its replier back unanswered (nothing is written to the
//!   device), and the replier then stages and commits the same bytes, by `pread` and by splice.
//! - a fill that fails is answered `EIO`, to the request's own unique id.
//!
//! Catches: a discarded reply that answers, drops or double-answers its request, and a failed
//! `pread` that leaves the request unanswered.

use std::fs::File;
use std::io::Write as _;
use std::os::fd::AsFd as _;
use std::sync::Arc;

use tierstore_testkit::dir::TestDir;
use tierstore_tree::{ReadStep, Stage, Staged};
use tsfuse::{ReplyConfig, ReplyDevice, ReplyPath, Tokens};

use super::tree_fs_harness::{device, has_reply_on, pack_bytes, recv_from};
use super::{Next, Reply, settle};

const EIO: i32 = 5;

fn reply_path() -> ReplyPath {
    ReplyPath::new(ReplyConfig {
        splice_min: Some(16 * 1024),
        max_data: 128 * 1024,
        pool: 2,
        move_pages: false,
    })
    .expect("a reply path")
}

/// Asserts that a fill discarded (as a failed recheck does) writes nothing and gives the replier
/// back, and that filling again and committing answers once with the bytes, for a reply by
/// `pread` (100 bytes) and one by splice (20000).
#[test]
fn a_discarded_fill_leaves_the_request_unanswered_and_a_second_fill_answers_it() {
    let dir = TestDir::under(std::env::temp_dir(), "treefs-adapter-discard");
    let bytes = pack_bytes(5, 30_000);
    let path = dir.path().join("data");
    File::create(&path)
        .expect("a data file")
        .write_all(&bytes)
        .expect("written");
    let data = File::open(&path).expect("the data file");
    let path = reply_path();
    let (dev, kernel) = device();
    let tokens = Tokens::new();
    for (unique, len) in [(7, 100), (8, 20_000)] {
        let device: Arc<dyn ReplyDevice> = dev.clone();
        let replier = tokens.register(unique, device);
        let Ok(filling) = (Reply {
            path: &path,
            replier,
        })
        .fill(data.as_fd(), 10, len) else {
            panic!("the fill of {len} bytes");
        };
        let back = filling.discard();
        assert!(
            !has_reply_on(&kernel),
            "a discarded reply writes nothing ({len})"
        );
        let Ok(filling) = back.fill(data.as_fd(), 10, len) else {
            panic!("the second fill of {len} bytes");
        };
        let _ = filling.commit();
        assert!(
            has_reply_on(&kernel),
            "the committed reply is written ({len})"
        );
        let msg = recv_from(&kernel);
        assert_eq!((msg.unique, msg.errno()), (unique, 0));
        assert_eq!(msg.payload, bytes[10..10 + len]);
        assert!(!has_reply_on(&kernel), "one reply ({len})");
    }
    assert_eq!(path.counts().pread_bytes, 100);
    assert_eq!(path.counts().splice_bytes, 20_000);
}

/// Asserts that a fill whose `pread` fails (the descriptor is a directory) is answered `EIO` to
/// its own request, and nothing else.
#[test]
fn a_fill_that_fails_is_answered_eio() {
    let dir = TestDir::under(std::env::temp_dir(), "treefs-adapter-fail");
    let not_a_file = File::open(dir.path()).expect("the directory opens");
    let path = reply_path();
    let (dev, kernel) = device();
    let device: Arc<dyn ReplyDevice> = dev.clone();
    let replier = Tokens::new().register(11, device);
    let Err(failed) = (Reply {
        path: &path,
        replier,
    })
    .fill(not_a_file.as_fd(), 0, 100) else {
        panic!("a directory cannot be read");
    };
    let next = settle(ReadStep::FillFailed(failed));
    assert!(matches!(next, Next::Done));
    let msg = recv_from(&kernel);
    assert_eq!((msg.unique, msg.errno()), (11, EIO));
    assert!(!has_reply_on(&kernel));
}
