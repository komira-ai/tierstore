//! The requests of the NFS front (`core/requests.rs`): every call of the protocol layer into the
//! file system timed under its kind, from its start to its end on the core's clock, whatever its
//! answer: a lookup, a getattr, an access, a read (the fill of a cold file's descriptor timed in
//! it), a listing, a link's target, and FSINFO and the refused changes as `other`.

use std::time::Duration;

use nfsserve::nfs::sattr3;
use nfsserve::vfs::NFSFileSystem as _;

use crate::server::local_cache::requests::{KINDS, RequestKind};
use crate::server::treefs::ino;
use crate::server::treefs::nfs::config::NfsSettings;
use crate::server::treefs::nfs::fixtures::{UID, attached, auth, endless, fs_of};

/// The entries of the sample tree (`testing::sample`), numbered 1.
const BIN: u32 = 2;
const TOOL: u32 = 4;
const LIB: u32 = 8;

/// How many requests of each kind the core counted, in the order of [`KINDS`].
fn counts(fs: &super::TreeNfs) -> [u64; KINDS.len()] {
    fs.core
        .requests_report()
        .spans
        .map(|spans| spans.buckets.iter().sum::<u64>())
}

/// Asserts that each call of the protocol layer is counted once under its kind, whether it is
/// answered with what it asked or refused: a LOOKUP, and one of a name that is not there; a
/// GETATTR; an ACCESS, and one with no user (`NFS3ERR_ACCES`); a READDIR; a READLINK; an FSINFO;
/// a SETATTR, a WRITE and a MKDIR (`NFS3ERR_ROFS`) as `other`. And that a READ of a cold file is
/// timed from its start to its answer, the 30 ms its fill held the clock included.
#[tokio::test]
async fn each_call_of_the_protocol_layer_is_counted_under_its_kind() {
    let (rig, _) = attached("nfs-requests");
    let fs = fs_of(&rig, &NfsSettings::default(), endless());
    let (bin, tool, lib) = (ino::of(1, BIN), ino::of(1, TOOL), ino::of(1, LIB));
    assert_eq!(counts(&fs), [0; KINDS.len()]);
    assert_eq!(
        fs.lookup(bin, &b"tool".as_slice().into()).await.ok(),
        Some(tool)
    );
    assert!(fs.lookup(bin, &b"none".as_slice().into()).await.is_err());
    assert!(fs.getattr(tool).await.is_ok());
    let user = auth(UID, UID, &[]);
    assert!(fs.access(tool, Some(&user), 0x3f).await.is_ok());
    assert!(fs.access(tool, None, 0x3f).await.is_err());
    assert!(fs.readdir(bin, 0, 10).await.is_ok());
    assert!(fs.readlink(lib).await.is_ok());
    assert!(fs.fsinfo(ino::ROOT).await.is_ok());
    assert!(fs.setattr(tool, sattr3::default()).await.is_err());
    assert!(fs.write(tool, 0, b"x").await.is_err());
    assert!(fs.mkdir(bin, &b"d".as_slice().into()).await.is_err());
    let mut want = [0; KINDS.len()];
    want[RequestKind::Lookup.index()] = 2;
    want[RequestKind::Getattr.index()] = 1;
    want[RequestKind::Access.index()] = 2;
    want[RequestKind::Readdir.index()] = 1;
    want[RequestKind::Readlink.index()] = 1;
    want[RequestKind::Other.index()] = 4;
    assert_eq!(counts(&fs), want);

    rig.upstream.gate_verb("fill");
    let clock = std::sync::Arc::clone(&rig.clock);
    let upstream = std::sync::Arc::clone(&rig.upstream);
    let driver = std::thread::spawn(move || {
        upstream.wait_for_verb("fill", 1);
        clock.advance(Duration::from_millis(30));
        upstream.permit_verb("fill", 1);
    });
    let read = fs.read(tool, 0, 100).await;
    driver.join().unwrap();
    assert_eq!(read.ok(), Some((b"tool bytes".to_vec(), true)));
    let read = fs.core.requests_report().spans[RequestKind::Read.index()];
    // 30 ms: in the bucket up to 0.05 s.
    assert_eq!((read.ms, read.buckets[5]), (30, 1));
    assert_eq!(read.buckets.iter().sum::<u64>(), 1);
}
