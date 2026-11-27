//! `verify` on a Mac build node with a mount provider: the mount check runs once the job is the
//! cache's writer, and its failure is the step's.

use super::fake::{CONFIG, Fake, assert_failed, assert_ok};
use crate::server::update_hook::provider_fake::PROVIDER_ENTRY;
use crate::server::update_hook::{Marker, ProviderCode, Reason, Step};

use Step::{Cutover, Prepare, Rollback, Verify};

/// Asserts that on a Mac build node `verify` fails with `provider_refused` when the mount provider
/// refuses the tree without an L2 sidecar, records nothing and leaves the `rollback` possible;
/// and that with a provider that places both trees it passes, records `verified`, and leads its
/// detail with what the check did. Catches a Mac update reported healthy while its provider
/// refuses every job's tree of that form.
#[test]
fn verify_runs_the_mount_check_on_a_mac_build_node() {
    let fake = Fake::new();
    fake.set_config(CONFIG, Some(&format!("[server]\n{PROVIDER_ENTRY}")));
    fake.advance(&[Prepare, Cutover]);
    fake.provider
        .answer("Mount", 1, ProviderCode::FailedPrecondition, "no tree list");
    let done = fake.run(Verify);
    assert_failed(&done, Reason::ProviderRefused);
    assert!(!fake.record().has(Marker::Verified));
    assert!(fake.provider.placed().is_empty());
    assert_ok(&fake.run(Rollback));

    let fake = Fake::new();
    fake.set_config(CONFIG, Some(&format!("[server]\n{PROVIDER_ENTRY}")));
    fake.advance(&[Prepare, Cutover]);
    let done = fake.run(Verify);
    assert_ok(&done);
    assert!(fake.record().has(Marker::Verified));
    let said = done.report.detail.clone().unwrap_or_default();
    assert!(said.starts_with("the mount provider at "), "{said}");
    assert!(said.contains("; "), "{said}");
}
