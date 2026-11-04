//! The launchd arm's steps, over the fake of `fake`: the calls of an update, the rollback of a
//! failed build, the first install, uninstall and the boot step, the refusals of every step, and
//! the convergence of every step after a failure at each of its calls.

mod crash;
mod fake;
mod flow;
mod lifecycle;
mod refusals;
mod rollback;
