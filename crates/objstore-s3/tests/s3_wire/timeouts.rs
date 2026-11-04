//! Per-request time limits: the base limit and the floor-rate allowance, on a moved clock.

use super::*;

/// A store that reads each request whole, then holds its answer until the test lets it go:
/// `read` is notified when a request has been read, `release` lets the held answer out (200,
/// no body). Returns its endpoint, `read` and `release`.
async fn gated_store() -> (String, Arc<Notify>, Arc<Notify>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let read = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let (told, gate) = (Arc::clone(&read), Arc::clone(&release));
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let (told, gate) = (Arc::clone(&told), Arc::clone(&gate));
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 65536];
                let (head_end, body_len) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                        break (i + 4, len);
                    }
                };
                while buf.len() < head_end + body_len {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                told.notify_one();
                gate.notified().await;
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")
                    .await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (endpoint, read, release)
}

/// The limits of the three tests below, and the time that passes at the store before it
/// would answer: the base limit of a request, the floor rate in bytes a second, and the
/// time the store keeps its answer. Nothing waits for them: the test moves tokio's clock
/// forward by `SLOW` in one jump once the store holds the request (see [`jump`]), so each is
/// a point on a clock the test drives.
const BASE: Duration = Duration::from_secs(40);
const RATE: u64 = 1024;
const SLOW: Duration = Duration::from_secs(120);
/// A base limit above [`SLOW`] by itself, for the test where only the base can cover the jump.
const LONG_BASE: Duration = Duration::from_secs(200);

/// Moves tokio's clock forward by `by` in one jump, and lets it run again. A request whose
/// limit is less than `by` has timed out afterwards; one whose limit is more has not, and
/// keeps its limit, minus `by`, to meet the answer in. The clock is paused only for the
/// jump: with a real socket in the test, a clock left paused would move on by itself
/// whenever the runtime waited for the socket.
async fn jump(by: Duration) {
    tokio::time::pause();
    tokio::time::advance(by).await;
    tokio::time::resume();
}

/// Catches: one flat limit for every PUT (the 60 s that cut 16 to 128 MiB segments on a
/// store that was slow but progressing). The clock jumps [`SLOW`] while the store holds its
/// answer to a 256 KiB body, the base limit is [`BASE`] (less than `SLOW`), and the floor
/// rate allows 256 s more: the PUT is not cut, and meets its answer.
#[tokio::test]
async fn a_slow_put_within_the_floor_rate_is_not_cut() {
    let (endpoint, read, release) = gated_store().await;
    let s = timed_store(endpoint, BASE, Some(RATE));
    let putting = tokio::spawn(async move {
        s.put_new(&key(), Bytes::from(vec![7u8; 256 << 10]), None)
            .await
    });
    read.notified().await;
    jump(SLOW).await;
    release.notify_one();
    putting
        .await
        .unwrap()
        .expect("a PUT inside body/rate + base was cut");
}

/// Catches: the base limit dropped from a PUT's allowance (leaving body/rate alone). A 1 KiB
/// body at the floor rate adds one second only, so only [`LONG_BASE`] (above [`SLOW`]) covers
/// the jump: the PUT is not cut, and meets its answer.
#[tokio::test]
async fn a_small_put_is_given_the_base_timeout() {
    let (endpoint, read, release) = gated_store().await;
    let s = timed_store(endpoint, LONG_BASE, Some(RATE));
    let putting = tokio::spawn(async move {
        s.put_new(&key(), Bytes::from(vec![7u8; 1 << 10]), None)
            .await
    });
    read.notified().await;
    jump(SLOW).await;
    release.notify_one();
    putting
        .await
        .unwrap()
        .expect("a PUT inside its base limit was cut");
}

/// Catches: the allowance scaled by something other than the body (a fixed bonus, or a
/// rate applied to bodiless requests), and a PUT slower than the floor rate allows
/// being waited for without end. The same store, a 64 KiB body at the floor rate allows
/// [`BASE`] + 64 s, under the [`SLOW`] the clock jumps: the PUT is cut at once, though the
/// store never answers. A limit above the jump leaves the PUT waiting for the store, and the
/// hang guard of the test runner (`.config/nextest.toml`) ends the test.
#[tokio::test]
async fn a_put_slower_than_the_floor_rate_still_times_out() {
    let (endpoint, read, _release) = gated_store().await;
    let s = timed_store(endpoint, BASE, Some(RATE));
    let putting = tokio::spawn(async move {
        s.put_new(&key(), Bytes::from(vec![7u8; 64 << 10]), None)
            .await
    });
    read.notified().await;
    jump(SLOW).await;
    let err = putting.await.unwrap().unwrap_err();
    assert!(
        matches!(err, ObjectStoreError::Transport(_)) && err.to_string().contains("timed out"),
        "{err}"
    );
}

/// Catches: the rate applied to a request with no body, which would give a GET the
/// same room as a PUT. A bodiless read the store holds for [`SLOW`] is cut at the [`BASE`]
/// limit, with a floor rate of one byte a second.
#[tokio::test]
async fn a_request_without_a_body_gets_only_the_base_timeout() {
    let (endpoint, read, _release) = gated_store().await;
    let s = timed_store(endpoint, BASE, Some(1));
    let reading =
        tokio::spawn(async move { s.get_range(&key(), ByteRange::new(0, 4).unwrap()).await });
    read.notified().await;
    jump(SLOW).await;
    let err = reading.await.unwrap().unwrap_err();
    assert!(matches!(err, ObjectStoreError::Transport(_)), "{err}");
}
