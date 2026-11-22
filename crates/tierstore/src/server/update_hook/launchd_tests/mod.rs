//! The launchd arm's steps, over the fake of `fake`: the calls of an update, the rollback of a
//! failed build, the first install, uninstall and the boot step, the binary the plist runs, the
//! refusals of every step, and the convergence of every step after a failure at each of its
//! calls.

mod binary;
mod crash;
pub(in crate::server::update_hook) mod fake;
mod flow;
mod fs_job;
mod lifecycle;
mod refusals;
mod rollback;
