//! `server.header_timeout` on a connection that is kept alive: the time a connection may wait,
//! idle, for its next request, on the paused clock over in-memory pipes. The test that the key
//! reaches the settings is in `front_tests.rs`; the one that a connection which never sends a
//! request is closed is in `door_tests.rs`.

use std::io::ErrorKind;
use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

use crate::server::pipes::{Serving, read_answer, read_response, status_of, until};

/// A probe that leaves the connection open after its answer.
const KEPT_PROBE: &str = "GET /healthz HTTP/1.1\r\nhost: tier.test\r\n\r\n";

/// The header timeout of the test.
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);

/// Asserts what a client sees when it reuses a connection that was idle for longer than
/// `header_timeout`: the server has closed it after the last answer, so the client reads the end
/// of the stream with nothing sent (not an answer, not an error status), and a request it writes
/// then finds the connection gone (`BrokenPipe`). A connection that waited for less is still
/// served, and each request starts the wait again, so a connection that is used at that pace is
/// never closed. It would catch a server that closes a kept-alive connection before the time the
/// key gives, or never closes it.
#[tokio::test(start_paused = true)]
async fn a_connection_idle_for_longer_than_the_header_timeout_is_closed_and_a_reused_one_is_not() {
    let serving = Serving::start(|config| config.header_timeout = HEADER_TIMEOUT).await;
    let mut client = serving.connect().expect("a server that takes connections");
    until(&serving.rig.server, |server| server.open_connections() == 1).await;
    for _ in 0..3 {
        client.write_all(KEPT_PROBE.as_bytes()).await.unwrap();
        assert_eq!(status_of(&read_response(&mut client).await), 200);
        tokio::time::advance(HEADER_TIMEOUT - Duration::from_secs(1)).await;
        assert_eq!(serving.rig.server.open_connections(), 1, "still waiting");
    }

    tokio::time::advance(Duration::from_secs(2)).await;
    assert_eq!(read_answer(&mut client).await, "", "closed, nothing sent");
    until(&serving.rig.server, |server| server.open_connections() == 0).await;
    let late = client.write_all(KEPT_PROBE.as_bytes()).await;
    assert_eq!(late.unwrap_err().kind(), ErrorKind::BrokenPipe);
}
