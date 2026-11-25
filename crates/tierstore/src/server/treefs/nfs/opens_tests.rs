//! The opens of the NFS front (`core/opens.rs`): NFS has no open, so the open of a file is the
//! fill of its descriptor that a first read starts, counted once however many reads wait on it,
//! and waiting from the read that started it to the descriptor or its failure.

use std::time::Duration;

use nfsserve::nfs::nfsstat3;
use nfsserve::vfs::NFSFileSystem as _;

use crate::server::treefs::ino;
use crate::server::treefs::nfs::config::NfsSettings;
use crate::server::treefs::nfs::fixtures::{attached, endless, failed, fs_of};
use crate::server::treefs::testing::sha;

/// The entries of the sample tree (`testing::sample`), numbered 1.
const CC: u32 = 3;
const TOOL: u32 = 4;

/// How many opens of each kind the core counted: ok and cached, ok and filled, error and cached,
/// error and filled.
fn counts(fs: &super::TreeNfs) -> [u64; 4] {
    fs.core
        .opens_report()
        .spans
        .map(|spans| spans.buckets.iter().sum::<u64>())
}

/// Asserts that the fill of a cold file's descriptor is one open, waiting while the node holds the
/// fill and counted `ok` and `filled` in the bucket of the 30 ms the clock moved meanwhile; that
/// the reads after it, which have the descriptor, open nothing; and that a fill that fails is one
/// open `error` and `filled`, after which none waits.
#[tokio::test]
async fn the_fill_of_a_files_descriptor_is_its_open() {
    let (rig, _) = attached("nfs-opens");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let tool = ino::of(1, TOOL);
    rig.upstream.gate_verb("fill");
    let mut done = fs.fill_of(tool);
    rig.upstream.wait_for_verb("fill", 1);
    assert_eq!(fs.core.opens_report().waiting, 1);
    rig.clock.advance(Duration::from_millis(30));
    rig.upstream.permit_verb("fill", 1);
    let filled = done.wait_for(Option::is_some).await.unwrap().clone();
    assert!(matches!(filled, Some(Ok(_))), "{filled:?}");
    let report = fs.core.opens_report();
    assert_eq!(report.waiting, 0);
    assert_eq!((report.spans[1].ms, report.spans[1].buckets[5]), (30, 1));
    for offset in [0, 3, 9] {
        let _ = fs.read(tool, offset, 1).await.unwrap();
    }
    assert_eq!(counts(&fs), [0, 1, 0, 0]);
    let _ = rig
        .upstream
        .failing
        .lock()
        .unwrap()
        .insert(sha(&cc_bytes()), "upstream 404".to_owned());
    rig.upstream.permit_verb("fill", 1);
    assert_eq!(
        failed(fs.read(ino::of(1, CC), 0, 10).await),
        nfsstat3::NFS3ERR_IO as u32
    );
    assert_eq!(fs.core.opens_report().waiting, 0);
    assert_eq!(counts(&fs), [0, 1, 0, 1]);
}

/// The bytes of `bin/cc` in the sample tree.
fn cc_bytes() -> Vec<u8> {
    (0..3000u32).map(|i| (i % 241) as u8).collect()
}
