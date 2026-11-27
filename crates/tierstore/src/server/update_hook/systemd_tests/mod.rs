//! The steps of the systemd arm over the fake machine of [`fake`]: the update from one instance
//! to the other, the rollbacks, a first install, the boot step, `uninstall`, the binary the units
//! run, the refusal of a node that still runs the second unit (`tierstore-fs`), a step run again
//! after a crash at every call, and the refusals of the session.

mod binary_tests;
mod boot_tests;
mod crash_tests;
mod cutover_tests;
mod drain_tests;
pub(in crate::server::update_hook) mod fake;
mod old_layout_tests;
mod prepare_tests;
mod rollback_tests;
mod service_tests;
mod session_tests;
mod stale_tests;
mod update_tests;
