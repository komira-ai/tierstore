//! T1 of docs/design/tierstore-handover.md (section 10): an update of tierstore under live S3 and
//! OCI traffic, with no retry of any request, end to end. The real `tierstore update-hook` runs
//! its steps; the real `nft` applies its rules, in a network namespace of the test's own; two real
//! `tierstore` servers are the slots 9301 and 9302 behind the public port 9300. The stand-ins
//! (double/main.rs) are `systemctl`, which asks the test's service manager to start and stop the
//! servers (services.rs); the binary of each staged release, which answers `--capabilities` with
//! `handover-v1` and otherwise becomes the real server; and, in the run that kills the hook, an
//! `nft` that runs the real one and then kills the hook.
//!
//! It needs CAP_NET_ADMIN and the `nft` program, and runs only with the feature `handover-t1`:
//! without it every test is ignored, so the workspace's test run neither runs nor fails on them.
//! The CI job `handover-t1` runs it as the design's section 10 says: cargo builds the server
//! without the drain (`test-drain-off`) into `target/drain-off`, then builds this binary as the
//! user, and its runner runs only this binary with `sudo -E`.
//!
//! Everything privileged happens in the two namespaces a run makes and deletes (netns.rs); the
//! ruleset of the namespace the test started in is read before and after, and must not change.
//! Nothing here sleeps or asserts how long anything takes: the run waits on counted requests and
//! on the servers' start lines. Each server's settle (`--drain-lb-wait`) is the one real
//! duration, short and not zero, and no assertion depends on it.

#![cfg(target_os = "linux")]

mod checks;
#[allow(
    dead_code,
    reason = "shared with the conformance rig, which uses all of it"
)]
#[path = "../update_hook/host.rs"]
mod host;
mod load;
mod netns;
mod runs;
mod services;
#[allow(
    dead_code,
    reason = "shared with the S3 front's tests, which use all of it"
)]
#[path = "../s3_front/signer.rs"]
mod signer;
mod site;
mod wire;
