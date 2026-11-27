//! The launchd arm's steps, over the fake of `fake`: the calls of an update, the rollback of a
//! failed build, the first install (also over a copy installed by hand), uninstall and the boot
//! step, the binary the plist runs, the refusals of every step, and the convergence of every step
//! after a failure at each of its calls.

mod binary;
mod crash;
mod exec;
pub(in crate::server::update_hook) mod fake;
mod flow;
mod fs_job;
mod lifecycle;
mod refusals;
mod rollback;
mod stale;
mod takeover;
mod verb;
