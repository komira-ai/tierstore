//! The steps of the systemd arm over the fake machine of [`fake`]: the update from one instance
//! to the other, the rollbacks, a first install, the boot step, `uninstall`, a step run again
//! after a crash at every call, and the refusals of the session.

mod boot_tests;
mod crash_tests;
mod cutover_tests;
mod fake;
mod prepare_tests;
mod rollback_tests;
mod session_tests;
mod update_tests;
