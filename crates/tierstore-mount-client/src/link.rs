//! How the client reaches the server ([`Connector`]) and how it sleeps between attempts
//! ([`Timer`]). Both are handed in, so a test connects to a server it controls and moves time by
//! hand (section 14: no test waits on time).

use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use tierstore_fetch_api::clock::{Clock, ManualClock};
use tokio::net::UnixStream;

/// A sleep of the client's runtime.
pub type Sleep = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Connects to the server. Called on the client's runtime, so the stream is registered with it.
#[async_trait::async_trait]
pub trait Connector: Send + Sync + 'static {
    /// A new connection to the server.
    async fn connect(&self) -> io::Result<UnixStream>;
}

/// The server's listening socket (the node cache's shared socket, 5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPath(pub PathBuf);

#[async_trait::async_trait]
impl Connector for SocketPath {
    async fn connect(&self) -> io::Result<UnixStream> {
        UnixStream::connect(&self.0).await
    }
}

/// Sleeps of the client's runtime: the reconnect backoff.
pub trait Timer: Send + Sync + 'static {
    /// A future that completes `ms` milliseconds from now.
    fn sleep(&self, ms: u64) -> Sleep;
}

/// The runtime's own timer: production.
#[derive(Debug, Clone, Copy, Default)]
pub struct TokioTimer;

impl Timer for TokioTimer {
    fn sleep(&self, ms: u64) -> Sleep {
        Box::pin(tokio::time::sleep(Duration::from_millis(ms)))
    }
}

/// A test's clock: the sleep completes when the test moves the clock past it.
impl Timer for ManualClock {
    fn sleep(&self, ms: u64) -> Sleep {
        Box::pin(self.sleep_until(self.now_ms().saturating_add(ms)))
    }
}
