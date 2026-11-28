//! The mount check of a Mac of the macOS installer package (`package.rs`), as the farm's Macs are
//! set up: the hook's process, which hostkeeper starts, may not read the volume (macOS answers it
//! `Operation not permitted`, `package_provider.rs`), so `verify` reads the trees the node's real
//! provider placed from the node's real NFS front, and ends: verified when the front serves them,
//! failed at once, with what was missing, when it does not.

use super::fake::{assert_failed, assert_ok};
use super::package::{CURRENT, MOUNT, Mac, NEXT, farm_mac_with, params};
use super::package_provider::{Arranged, FILE_FRONT, SOCKET};
use crate::server::provider::check::{AGENT, RUN};
use crate::server::update_hook::{Marker, Reason, Step};

/// Asserts that a farm Mac's `verify` (after `prepare`, hostkeeper's `activate` and `cutover`)
/// passes the mount check in one run, its detail saying that both trees were read from the NFS
/// front the node's file names, with nothing placed after it, and that `cleanup` then commits.
/// The read of the volume is answered as macOS answers the hook's process, so a check that reads
/// the volume on a Mac fails here. Catches the update of 0.3.212 on a farm Mac: its check read
/// the volume as root, was refused every time, deferred every few seconds, and held the update
/// until hostkeeper's verify deadline.
#[test]
fn a_farm_mac_verifies_its_trees_from_the_nfs_front_and_never_the_volume() {
    let mac = farm_mac_with(Mac::with_provider("mac-check-front", Arranged::Built));
    let params = params(NEXT, Some(CURRENT));
    mac.stage(NEXT);
    assert_ok(&mac.step(Step::Prepare, &params));
    mac.activate(NEXT);
    assert_ok(&mac.step(Step::Cutover, &params));
    let verified = mac.step(Step::Verify, &params);
    assert_ok(&verified);
    let said = verified.report.detail.clone().unwrap_or_default();
    assert!(
        said.starts_with(&format!(
            "the mount provider at {SOCKET} placed, served and released a tree with its L2 \
             sidecar and one without an L2 sidecar, read from the NFS front at {FILE_FRONT}; "
        )),
        "{said}"
    );
    assert_eq!(verified.report.retry_after_secs, None);
    assert!(mac.record().has(Marker::Verified));
    assert_eq!(mac.provider.placements(), 0);
    assert_ok(&mac.step(Step::Cleanup, &params));
    assert!(mac.record().committed());
}

/// Asserts that a farm Mac whose NFS front finds no tree at the sources' paths when the hook reads
/// them (the placements were let go of between the `Mount` and the read) fails `verify` in that
/// one run with `mounted_tree_wrong`, not deferred, its detail naming the tree, its directory, the
/// front of the node's file, the path read, the front it asked and the status it answered; that
/// nothing is placed after it and nothing verified is recorded; and that the `rollback`
/// hostkeeper runs next hands the same process back to 0.3.202. Catches a check that defers, and
/// so holds the update for `verify_deadline_secs`, on a front that will not serve the tree.
#[test]
fn a_farm_mac_whose_front_finds_no_tree_fails_verify_at_once_and_rolls_back() {
    let mac = farm_mac_with(Mac::with_provider(
        "mac-check-gone",
        Arranged::GoneBeforeRead,
    ));
    let (pid, _) = mac.serving().expect("the job serves");
    let params = params(NEXT, Some(CURRENT));
    mac.stage(NEXT);
    assert_ok(&mac.step(Step::Prepare, &params));
    mac.activate(NEXT);
    assert_ok(&mac.step(Step::Cutover, &params));
    let done = mac.step(Step::Verify, &params);
    assert_failed(&done, Reason::MountedTreeWrong);
    assert_eq!(done.report.retry_after_secs, None);
    let below = format!(
        "mount-provider/{}/{}/0",
        hex::encode(AGENT),
        hex::encode(RUN)
    );
    let said = done.report.detail.clone().unwrap_or_default();
    let front = mac.provider.front();
    assert_eq!(
        said,
        format!(
            "the tree with its L2 sidecar at {MOUNT}/{below} could not be read from the NFS front \
             at {FILE_FRONT}: {below} could not be looked up: the NFS front at {front} answered \
             NFS3ERR_NOENT (2)"
        )
    );
    assert!(!mac.record().has(Marker::Verified));
    assert_eq!(mac.provider.placements(), 0);
    mac.activate(CURRENT);
    assert_ok(&mac.step(Step::Rollback, &params));
    assert_eq!(mac.serving(), Some((pid, CURRENT.to_owned())));
}
