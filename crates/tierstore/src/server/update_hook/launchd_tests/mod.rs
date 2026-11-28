//! The launchd arm's steps, over the fake of `fake`: the calls of an update, the rollback of a
//! failed build, the first install (also over a copy installed by hand), uninstall and the boot
//! step, the binary the plist runs, the refusals of every step, and the convergence of every step
//! after a failure at each of its calls; and the volume of a Mac: the refusal of a node of
//! the two-process layout, and the volume `verify` requires served; and every step of a Mac of the
//! macOS installer package, over the machine's own host and the layout the release ships, and its
//! mount check, read from the node's NFS front.

mod binary;
mod crash;
mod exec;
pub(in crate::server::update_hook) mod fake;
mod flow;
mod fs_job;
mod lifecycle;
mod mount_check;
mod package;
mod package_check;
mod package_provider;
mod refusals;
mod rollback;
mod stale;
mod takeover;
mod verb;
mod volume;
