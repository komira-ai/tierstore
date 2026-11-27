//! The steps of the systemd arm over the fake machine of [`fake`]: the update from one instance
//! to the other, the rollbacks, a first install, the boot step, `uninstall`, the binary the units
//! run, the second unit (`tierstore-fs`) and the binary its process runs, a step run again after
//! a crash at every call, and the refusals of the session.

mod binary_tests;
mod boot_tests;
mod crash_tests;
mod cutover_tests;
mod drain_tests;
pub(in crate::server::update_hook) mod fake;
mod fs_exe_tests;
mod fs_failure_tests;
mod fs_tests;
mod mount_check_tests;
mod prepare_tests;
mod rollback_tests;
mod service_tests;
mod session_tests;
mod update_tests;
