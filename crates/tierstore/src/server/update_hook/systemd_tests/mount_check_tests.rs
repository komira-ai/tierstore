//! `verify` on a build node with a mount provider: the mount check runs once the new instance is
//! the cache's writer, and its failure or deferral is the step's.

use crate::server::provider::check::WITHOUT_SIDECAR;
use crate::server::update_hook::provider_fake::PROVIDER_ENTRY;
use crate::server::update_hook::{Marker, ProviderCode, Reason, Step};

use super::fake::{
    NEW, OLD, SystemdFake, WITH_SOCKET, assert_deferred, assert_failed, assert_ok, detail, params,
};

use Step::{Cleanup, Cutover, Prepare, Rollback, Verify};

/// A host that runs `OLD` on a build node with a mount provider, the update to `NEW` cut over.
fn cut_over() -> SystemdFake {
    let fake = SystemdFake::installed();
    fake.set_config(Some(&format!("{WITH_SOCKET}{PROVIDER_ENTRY}")));
    fake.drive(&params(NEW, Some(OLD)), &[Prepare, Cutover]);
    fake
}

/// Asserts that `verify` fails, with `provider_refused` and the provider's reason, reversible and
/// recording nothing, when the new release's mount provider refuses the tree without an L2
/// sidecar (as a release that placed only trees with a sidecar refused every other job's tree),
/// and that the `rollback` hostkeeper runs next brings the old instance back. Catches a verify
/// that reports such a release healthy because it is active and answers `/readyz`, so that a
/// rollout converges on it while every job of that form fails.
#[test]
fn verify_fails_when_the_mount_provider_refuses_the_tree_without_a_sidecar() {
    let fake = cut_over();
    let p = params(NEW, Some(OLD));
    let reason = format!("no tree list at {}", WITHOUT_SIDECAR.key());
    fake.provider
        .answer("Mount", 1, ProviderCode::FailedPrecondition, &reason);
    let done = fake.run(Verify, &p);
    assert_failed(&done, Reason::ProviderRefused);
    assert!(done.report.reversible);
    assert!(detail(&done).ends_with(&reason), "{done:?}");
    assert!(!fake.record().has(Marker::Verified));
    assert!(fake.provider.placed().is_empty());
    assert_ok(&fake.run(Rollback, &p));
}

/// Asserts that on a build node with a mount provider `verify` passes once the check passes: it
/// records `verified`, its detail leads with what the check did, the provider holds nothing of the
/// check afterwards, and `cleanup` commits. A `verify` run again runs the check again.
#[test]
fn verify_passes_and_records_once_the_mount_check_passes() {
    let fake = cut_over();
    let p = params(NEW, Some(OLD));
    let done = fake.run(Verify, &p);
    assert_ok(&done);
    assert!(fake.record().has(Marker::Verified));
    assert!(
        detail(&done).starts_with("the mount provider at /var/lib/tierstore/provider.sock placed"),
        "{done:?}"
    );
    assert!(fake.provider.placed().is_empty());
    let calls = fake.provider.calls().len();
    assert_ok(&fake.run(Verify, &p));
    assert_eq!(fake.provider.calls().len(), calls * 2);
    assert_ok(&fake.run(Cleanup, &p));
}

/// Asserts that `verify` defers with `not_ready`, recording nothing, while the provider answers a
/// `Mount` `UNAVAILABLE`, and that the check is not run while the new instance is read-only (it
/// does not bind the provider's socket until it is the writer), nor after the commit, nor on a
/// node without a provider.
#[test]
fn verify_defers_while_the_provider_is_unavailable_and_asks_it_only_as_the_writer() {
    let fake = cut_over();
    let p = params(NEW, Some(OLD));
    fake.provider
        .answer("Mount", 0, ProviderCode::Unavailable, "not ready");
    assert_deferred(&fake.run(Verify, &p), Reason::NotReady, 5);
    assert!(!fake.record().has(Marker::Verified));

    let fake = cut_over();
    fake.set_read_only(true);
    assert_deferred(&fake.run(Verify, &p), Reason::NotReady, 5);
    assert!(fake.provider.calls().is_empty());
    fake.set_read_only(false);
    fake.drive(&p, &[Verify, Cleanup]);
    let calls = fake.provider.calls().len();
    assert_ok(&fake.run(Verify, &p));
    assert_eq!(fake.provider.calls().len(), calls);

    let fake = SystemdFake::installed();
    fake.drive(&p, &[Prepare, Cutover, Verify, Cleanup]);
    assert!(fake.provider.calls().is_empty());
}
