//! A writer that is killed, and what it leaves behind.
//!
//! The testkit can stop a task and drop a connection's body, and it can plant what a crashed
//! process leaves on disk; it cannot kill a process, and no test here does. A killed writer is a
//! task that is aborted at a point the test names (the body is stuck after its first piece, or
//! the journal write is), which drops its futures exactly as a client that went away or a
//! server task that was cancelled does. A crash that leaves a file behind is planted by hand.

use std::sync::Arc;
use std::time::Duration;

use tierstore::{DirBackend, Proxy, ProxyConfig, TestClock};
use tierstore_testkit::dir::TestDir;

use crate::support::{assert_absent, body, keys, read};
use tierstore::Backend;

/// Two directories as the tiers of a proxy.
struct Dirs {
    hot_dir: TestDir,
    _cold_dir: TestDir,
    hot: Arc<DirBackend>,
    cold: Arc<DirBackend>,
}

impl Dirs {
    fn new() -> Self {
        let hot_dir = TestDir::under(std::env::temp_dir(), "tierstore-kill-hot");
        let cold_dir = TestDir::under(std::env::temp_dir(), "tierstore-kill-cold");
        let hot = Arc::new(DirBackend::open(hot_dir.path()).unwrap());
        let cold = Arc::new(DirBackend::open(cold_dir.path()).unwrap());
        Self {
            hot_dir,
            _cold_dir: cold_dir,
            hot,
            cold,
        }
    }

    fn config(id: &str) -> ProxyConfig {
        let mut config = ProxyConfig::new(id, 1 << 30, Arc::new(TestClock::new()));
        config.usage_refresh = Duration::ZERO;
        config
    }

    /// A proxy over the directories, as a process that has just started would open it.
    async fn proxy(&self, id: &str) -> Arc<Proxy> {
        let (hot, cold) = (self.hot.clone(), self.cold.clone());
        Arc::new(
            crate::support::open_live(hot, cold, Self::config(id))
                .await
                .unwrap(),
        )
    }
}

/// Asserts that what a crashed process leaves in a directory tier, a temporary file with part of
/// a body in it, is not an object: it is not listed, not read, not found by the proxy, a restart
/// over the directory neither counts it nor stumbles on it, and a put of any key afterwards is
/// stored whole. The file is planted by hand, because the test cannot kill a process. Defect it
/// catches: a listing or a read that treats a `.tmp-` file as an object.
#[tokio::test(start_paused = true)]
async fn a_temporary_file_left_by_a_crashed_process_is_not_an_object() {
    let dirs = Dirs::new();
    std::fs::write(dirs.hot_dir.path().join(".tmp-dead-0"), b"half of an obj").unwrap();

    assert_eq!(keys(&*dirs.hot).await, Vec::<String>::new());
    let proxy = dirs.proxy("p1").await;
    assert_eq!(keys(&*proxy).await, Vec::<String>::new());
    assert_absent(&*proxy, ".tmp-dead-0").await;
    proxy.put("k", body("whole")).await.unwrap();
    assert_eq!(keys(&*proxy).await, ["k"]);
    assert_eq!(read(&*proxy, "k").await, body("whole"));
}
