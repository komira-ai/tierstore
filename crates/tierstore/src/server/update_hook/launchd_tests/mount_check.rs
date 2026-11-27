//! `verify` on a Mac build node with a mount provider: the mount check runs once the job is the
//! cache's writer, and its failure is the step's.

use super::fake::{CONFIG, FS_MOUNT, Fake, NEW, OLD, Volume, assert_failed, assert_ok};
use crate::server::update_hook::{HANDOFF_EXEC_CAPABILITY, Marker, ProviderCode, Reason, Step};

use Step::{Cutover, Prepare, Rollback, Verify};

/// A Mac's file of one process: the `fs` interface, and the mount provider under its mount.
fn one_process() -> String {
    format!(
        "[server]\n\n[[interface]]\ntype = \"fs\"\nparams = {{ mountpoint = \"{FS_MOUNT}\" }}\n\n\
         [[interface]]\ntype = \"mount-provider\"\n\
         params = {{ socket = \"/usr/local/tierstore/run/provider.sock\", mountpoint = \"{FS_MOUNT}\" }}\n"
    )
}

/// A Mac of one process with a mount provider, on which both releases hand over.
fn build_node() -> Fake {
    let fake = Fake::new();
    fake.set_config(CONFIG, Some(&one_process()));
    fake.set_capabilities(OLD, &[HANDOFF_EXEC_CAPABILITY]);
    fake.set_capabilities(NEW, &[HANDOFF_EXEC_CAPABILITY]);
    fake
}

/// Asserts that on a Mac the proof of the volume comes first and the mount check after it: with
/// no volume mounted at the fs interface's mount point, `verify` fails with `not_ready` and the
/// provider is asked nothing; once the volume is served, the provider is asked. Catches a check
/// that asks a provider whose mount nothing serves, and an order that hides the volume's failure
/// behind the provider's.
#[test]
fn on_a_mac_the_volume_is_proved_before_the_tree_is_mounted() {
    let fake = build_node();
    fake.advance(&[Prepare, Cutover]);
    fake.set_volume(Volume::NotMounted);
    assert_failed(&fake.run(Verify), Reason::NotReady);
    assert!(fake.provider.calls().is_empty());
    fake.set_volume(Volume::Served);
    assert_ok(&fake.run(Verify));
    assert!(!fake.provider.calls().is_empty());
}

/// Asserts that on a Mac build node `verify` fails with `provider_refused` when the mount provider
/// refuses the tree without an L2 sidecar, records nothing and leaves the `rollback` possible;
/// and that with a provider that places both trees it passes, records `verified`, and leads its
/// detail with what the check did. Catches a Mac update reported healthy while its provider
/// refuses every job's tree of that form.
#[test]
fn verify_runs_the_mount_check_on_a_mac_build_node() {
    let fake = build_node();
    fake.advance(&[Prepare, Cutover]);
    fake.provider
        .answer("Mount", 1, ProviderCode::FailedPrecondition, "no tree list");
    let done = fake.run(Verify);
    assert_failed(&done, Reason::ProviderRefused);
    assert!(!fake.record().has(Marker::Verified));
    assert!(fake.provider.placed().is_empty());
    assert_ok(&fake.run(Rollback));

    let fake = build_node();
    fake.advance(&[Prepare, Cutover]);
    let done = fake.run(Verify);
    assert_ok(&done);
    assert!(fake.record().has(Marker::Verified));
    let said = done.report.detail.clone().unwrap_or_default();
    assert!(said.starts_with("the mount provider at "), "{said}");
    assert!(said.contains("; "), "{said}");
}
