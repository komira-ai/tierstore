//! A credential endpoint on loopback for the tests: an HTTP/1.1 server on `127.0.0.1:0` that
//! answers each request as its route says and keeps every request it took, in order.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A request the fake took.
#[derive(Clone, Debug)]
pub(crate) struct Seen {
    pub method: String,
    /// The path and query, as the request line has them.
    pub path: String,
    /// The headers, by lower-case name.
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

impl Seen {
    /// The header `name` (lower case), if the request had it.
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// What the fake answers a request with: a status and a body.
pub(crate) type Route = Arc<dyn Fn(&Seen) -> (u16, String) + Send + Sync>;

/// The server. Dropping it stops it.
pub(crate) struct Fake {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    route: Arc<Mutex<Route>>,
    task: JoinHandle<()>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Fake {
    /// A server that answers as `route` says.
    pub(crate) async fn start(
        route: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let route: Arc<Mutex<Route>> = Arc::new(Mutex::new(Arc::new(route)));
        let (kept, routing) = (Arc::clone(&seen), Arc::clone(&route));
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (kept, routing) = (Arc::clone(&kept), Arc::clone(&routing));
                tokio::spawn(serve(stream, kept, routing));
            }
        });
        Self {
            url,
            seen,
            route,
            task,
        }
    }

    /// `http://127.0.0.1:<port>`, with no `/` at the end.
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The requests taken so far, in order.
    pub(crate) fn seen(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Answers the requests from now on as `route` says.
    pub(crate) fn reroute(&self, route: impl Fn(&Seen) -> (u16, String) + Send + Sync + 'static) {
        *self.route.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(route);
    }
}

/// Serves the requests of one connection until it closes.
async fn serve(mut stream: TcpStream, seen: Arc<Mutex<Vec<Seen>>>, route: Arc<Mutex<Route>>) {
    let mut buffer = Vec::new();
    loop {
        let Some(request) = read_request(&mut stream, &mut buffer).await else {
            return;
        };
        let answer = Arc::clone(&*route.lock().unwrap_or_else(PoisonError::into_inner));
        let (status, body) = answer(&request);
        seen.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
        let head = format!(
            "HTTP/1.1 {status} Answer\r\ncontent-length: {}\r\n\r\n",
            body.len()
        );
        if stream.write_all(head.as_bytes()).await.is_err()
            || stream.write_all(body.as_bytes()).await.is_err()
        {
            return;
        }
    }
}

/// The next request of `stream`, whose bytes read so far and not yet used are in `buffer`; `None`
/// when the connection ends first.
async fn read_request(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Option<Seen> {
    let mut chunk = [0_u8; 4096];
    let end = loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let path = request_line.next()?.to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map_or(0, |value| value.parse().unwrap());
    let start = end + 4;
    while buffer.len() < start + length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buffer[start..start + length]).into_owned();
    buffer.drain(..start + length);
    Some(Seen {
        method,
        path,
        headers,
        body,
    })
}
