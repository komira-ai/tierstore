//! A hot mount whose lock the kernel refuses: the image opens unlocked, says why, and serves.
//!
//! Proves: with the memlock limit at 0, `ImageOptions::lock` is reported as `lock_refused` with
//! nothing locked, and lookups, nodes and link targets still read. The limit is lowered for the
//! whole process, so this is the only test of its binary. A process that may lock regardless of
//! the limit (root with `CAP_IPC_LOCK`) cannot be refused: the test then says so and passes
//! having proven nothing about refusal.
//!
//! Catches: a refused lock that fails the open, leaves a half-locked image, or reports bytes
//! locked that are not.

// Linux only, as the crate is (its library and its dependencies are `cfg(target_os = "linux")`).
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::fs::File;

use rustix::process::{Resource, Rlimit, setrlimit};
use support::{find, small_tree, tree_id, write_image};
use tierstore_fetch_api::list::NodeIndex;
use tierstore_testkit::dir::TestDir;
use tierstore_tree::{Image, ImageOptions, NodeKind};

#[expect(
    clippy::disallowed_macros,
    reason = "a skip says why on the test's output, as a test has no operator"
)]
#[test]
fn a_refused_lock_is_reported_and_the_image_serves_unlocked() {
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "image-lock-refused");
    let l2 = small_tree().lists().1;
    let path = write_image(dir.path(), &l2);
    setrlimit(
        Resource::Memlock,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )
    .unwrap();
    let image = Image::open(
        File::open(path).unwrap(),
        &tree_id(&l2),
        ImageOptions { lock: true },
    )
    .unwrap();
    let advice = image.advice();
    if advice.lock_refused.is_none() {
        eprintln!(
            "skip: the lock was not refused with memlock 0 (the process may lock regardless of the limit)"
        );
        return;
    }
    assert_eq!(advice.locked_bytes, 0);
    assert!(advice.willneed_bytes > 0);
    let link = find(&image, "lib/self");
    assert_eq!(
        image.read_link(&image.node(link).unwrap()).unwrap(),
        b"a.txt"
    );
    assert!(matches!(
        image.node(find(&image, "lib/a.txt")).unwrap().kind,
        NodeKind::File { .. }
    ));
    assert_eq!(image.lookup(NodeIndex(0), b"nope"), None);
}
