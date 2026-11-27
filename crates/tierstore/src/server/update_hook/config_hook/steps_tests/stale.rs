//! A reload of another config package on record, in flight and not committed, whose updater is
//! gone: the next package's `prepare` (past that reload's cutover) and `rollback` roll it back in
//! place, as its own `rollback` does, and then do their own step; a package that races a reload
//! still in its cutover is refused as before.

use std::path::{Path, PathBuf};

use super::super::super::{Instance, Marker, Reason, Record, Step};
use super::reload::{GIB, cap, changed, changing, fs_file, set};
use super::{Cutover, Linux, PKG_NEW, PKG_OLD, Prepare, Verify, build, lin, staged_copy, update};
use Step::Rollback;

/// The next config package.
const PKG_NEWER: &str = "/var/lib/hostkeeper/packages/tierstore-config/1.2.0";

/// A node whose reload of the cap from 1 GiB to 2 GiB swapped and whose updater went away: the
/// cap is 2 GiB and the record has the reload in flight past its cutover. The next package
/// stages the same file as the one on record.
fn left_by_a_gone_updater() -> Linux {
    let node = changing(Some(GIB), Some(2 * GIB));
    node.go(&update(), &[Prepare, Cutover]);
    node.files.put(
        &staged_copy(Path::new(PKG_NEWER), Path::new(lin::CONFIG)),
        &fs_file(&cap(2 * GIB)),
    );
    let stale = node.fake.record();
    assert_eq!(
        (stale.updating, stale.markers.is_empty(), stale.committed()),
        (true, false, false)
    );
    node
}

/// The params of the next package, from `previous`, the package `current` names.
fn next(previous: &str) -> super::super::super::Params {
    lin::params(PKG_NEWER, Some(previous))
}

/// The note that leads the detail of a step that rolled the reload on record back.
fn rolled_back() -> String {
    format!(
        "the record is of the update to {PKG_NEW} {build}, not {PKG_NEWER} {build}: rolled back \
         with its own target and releases (rolled back in place: tierstore@a serves on \
         fs.tree_list_ram_bytes {GIB} as before)",
        build = build()
    )
}

/// Asserts that hostkeeper's `rollback` of the next package, after its `prepare` failed, rolls
/// back the reload on record in place: the cap of the file that served is set again, nothing is
/// started or stopped, and the record is at rest. Catches the `bad_params` of the record of
/// another package, after which hostkeeper halts the service.
#[test]
fn the_rollback_of_the_next_package_rolls_back_a_stale_reload() {
    let node = left_by_a_gone_updater();
    let done = node.step(Rollback, &next(PKG_OLD));
    lin::assert_ok(&done);
    assert_eq!(
        lin::detail(&done),
        format!(
            "{}; nothing to roll back: no update is in flight",
            rolled_back()
        )
    );
    assert_eq!(node.control.asked(), [set(2 * GIB), set(GIB)]);
    assert_eq!(node.fake.actions(), Vec::<String>::new());
    assert_eq!(node.fake.record(), Record::idle(Some(Instance::A)));
}

/// Asserts that the next package's `prepare`, which finds the reload on record past its cutover,
/// rolls it back in place first and then prepares its own reload from the record at rest.
/// Catches the `out_of_order` refusal that made hostkeeper run the `rollback` that refused.
#[test]
fn prepare_of_the_next_package_rolls_back_a_stale_reload_and_prepares_its_own() {
    let node = left_by_a_gone_updater();
    let done = node.step(Prepare, &next(PKG_OLD));
    lin::assert_ok(&done);
    assert!(
        lin::detail(&done).starts_with(&format!("{}; prepared in place: ", rolled_back())),
        "{done:?}"
    );
    assert_eq!(node.control.asked(), [set(2 * GIB), set(GIB)]);
    let record = node.fake.record();
    assert_eq!((record.updating, record.markers.len()), (true, 0));
    assert_eq!(record.to, Some(PathBuf::from(PKG_NEWER)));
    assert_eq!(record.reload, Some(changed(GIB, 2 * GIB)));
}

/// Asserts that a package that races a reload still in its cutover (its params name the staged
/// package on record as the one `current` names) is refused as before: `prepare` is
/// `out_of_order`, `rollback` is `bad_params` with why, and `cutover` is `bad_params`, with the
/// cap and the record left as they were. Catches a second order that rolls back a reload whose
/// updater is alive.
#[test]
fn a_package_that_races_a_reload_in_its_cutover_is_refused() {
    let node = left_by_a_gone_updater();
    let stale = node.fake.record();
    let racing = next(PKG_NEW);
    lin::assert_failed(&node.step(Prepare, &racing), Reason::OutOfOrder);
    let refused = node.step(Rollback, &racing);
    lin::assert_failed(&refused, Reason::BadParams);
    assert_eq!(
        lin::detail(&refused),
        format!(
            "the params name {PKG_NEWER} {build}, and the update on record is Some({PKG_NEW:?}) \
             Some({build:?}): the params name its staged release as the one the host runs: that \
             update is still in its cutover",
            build = build()
        )
    );
    lin::assert_failed(&node.step(Cutover, &next(PKG_OLD)), Reason::BadParams);
    assert_eq!(node.control.asked(), [set(2 * GIB)]);
    assert_eq!(node.fake.record(), stale);
}

/// Asserts that a node's first config package keeps the reload of another package on record:
/// whether that one was a first package too, which decides whether the guard is reloaded, is not
/// on record. Its `rollback` is refused with why, and nothing is set.
#[test]
fn a_first_package_keeps_a_stale_reload() {
    let node = left_by_a_gone_updater();
    let stale = node.fake.record();
    let refused = node.step(Rollback, &lin::params(PKG_NEWER, None));
    lin::assert_failed(&refused, Reason::BadParams);
    assert!(
        lin::detail(&refused).ends_with(
            "the params are of a node's first config package, and the package on record is \
             rolled back only with the params it was written under"
        ),
        "{refused:?}"
    );
    assert_eq!(node.control.asked(), [set(2 * GIB)]);
    assert_eq!(node.fake.record(), stale);
}

/// Asserts that a stale reload whose rollback fails (the control socket does not answer) is
/// reported with what was asked, and leaves the record as it was for a run again.
#[test]
fn a_stale_reload_whose_rollback_fails_keeps_the_record() {
    let node = left_by_a_gone_updater();
    let stale = node.fake.record();
    node.control.broken.set(true);
    let done = node.step(Rollback, &next(PKG_OLD));
    assert!(
        lin::detail(&done).starts_with(&format!(
            "the record is of the update to {PKG_NEW} {build}, not {PKG_NEWER} {build}, and its \
             rollback with its own target and releases is not done; ",
            build = build()
        )),
        "{done:?}"
    );
    assert_eq!(node.fake.record(), stale);
    let done = node.step(Prepare, &next(PKG_OLD));
    assert!(
        lin::detail(&done).contains("its rollback with its own target and releases is not done"),
        "{done:?}"
    );
    assert_eq!(node.fake.record(), stale);
}

/// Asserts that a reload of another package that is committed and still in flight is never
/// rolled back by the next package, even with params that name another previous package: its
/// `rollback` is refused (`bad_params`) as before, nothing is set and the record stays. Catches a
/// rule that rolls back what `cleanup` has committed.
#[test]
fn a_committed_stale_reload_is_never_rolled_back() {
    let node = changing(Some(GIB), Some(2 * GIB));
    node.go(&update(), &[Prepare, Cutover, Verify]);
    node.fake
        .set_record(&node.fake.record().with(Marker::Stopping));
    let committed = node.fake.record();
    let asked = node.control.asked();
    let refused = node.step(Rollback, &next(PKG_OLD));
    lin::assert_failed(&refused, Reason::BadParams);
    assert_eq!(node.control.asked(), asked);
    assert_eq!(node.fake.record(), committed);
}
