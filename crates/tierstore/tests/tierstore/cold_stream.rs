//! A read that misses the hot tier streams the cold object to the client as it arrives: the cold
//! tier is read once, the client is never held back, and a body that fails is counted as an error
//! of the cold tier. The read copies nothing into the hot tier: the copy is the entry
//! reconciler's, on an entry the read asks for.
//!
//! The cold tier is a test backend whose progress the tests see as events: the body waits at a
//! chosen chunk until it is released. No test waits on a clock.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use tierstore::{
    Backend, ByteRange, Error, Listed, MemoryBackend, Meta, Opened, Proxy, ReadBody, Span, Window,
};
use tokio::sync::Notify;

use crate::support::{Rig, assert_absent};
use futures::StreamExt;

/// The cold tier: objects in memory, given by `open_read` alone, in chunks of `chunk` bytes. A
/// read of a data key (one that does not start with `_`) by any other call is refused and counted.
struct Cold {
    mem: Arc<MemoryBackend>,
    chunk: usize,
    /// Calls of `open_read`.
    opens: AtomicUsize,
    /// Calls of other reads of data keys.
    others: AtomicUsize,
    /// A body that waits for the notify after that many chunks.
    gate: Option<(usize, Arc<Notify>)>,
    /// A body that fails instead of giving the chunk with this index, and gives no more.
    cut_after: Option<usize>,
    /// Whether a chunk of no bytes follows every chunk of the body.
    empties: bool,
    /// A size for the meta other than the object's, for a cold tier whose meta and body differ.
    claims: Option<u64>,
}

impl Cold {
    fn refuse(&self, key: &str) -> Result<(), Error> {
        if key.starts_with('_') {
            return Ok(());
        }
        self.others.fetch_add(1, Ordering::SeqCst);
        Err(Error::Other(format!("{key} was read by another call")))
    }
}

/// The pieces of `bytes` of at most `size` bytes.
fn pieces(bytes: &Bytes, size: usize) -> Vec<Bytes> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let end = (at + size).min(bytes.len());
        out.push(bytes.slice(at..end));
        at = end;
    }
    out
}

#[async_trait]
impl Backend for Cold {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.mem.put(key, body).await
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.mem.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.refuse(key)?;
        self.mem.get(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.refuse(key)?;
        self.mem.head(key).await
    }

    async fn open_read(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let mut meta = self.mem.head(key).await?;
        let object = self.mem.get(key, None).await?;
        let window = Window::of(span, meta.size);
        let mut held = pieces(&window.slice(&object), self.chunk);
        if self.empties {
            held = held
                .into_iter()
                .flat_map(|piece| [piece, Bytes::new()])
                .collect();
        }
        let held = Arc::new(held);
        if let Some(size) = self.claims {
            meta.size = size;
        }
        let (gate, cut) = (self.gate.clone(), self.cut_after);
        let body = stream::unfold(0_usize, move |at| {
            let (gate, held) = (gate.clone(), Arc::clone(&held));
            async move {
                if let Some((after, release)) = &gate
                    && at == *after
                {
                    release.notified().await;
                }
                if cut == Some(at) {
                    return Some((Err(Error::Unavailable("cut".to_owned())), usize::MAX));
                }
                held.get(at)
                    .cloned()
                    .map(|piece| (Ok::<Bytes, Error>(piece), at + 1))
            }
        })
        .boxed();
        Ok(Opened { meta, window, body })
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.mem.delete(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.mem.list(prefix).await
    }
}

/// A proxy over a hot tier in memory and a cold test tier, and what the tests see of them.
struct Run {
    proxy: Proxy,
    hot: Arc<MemoryBackend>,
    cold: Arc<Cold>,
}

/// Starts a proxy whose cold tier holds `objects` and gives them in chunks of `chunk` bytes;
/// `tune` sets what a test needs of the cold tier before it is used.
async fn start(objects: &[(&str, Bytes)], chunk: usize, tune: impl FnOnce(&mut Cold)) -> Run {
    let rig = Rig::new();
    let cold_mem = Arc::new(MemoryBackend::with_clock(rig.clock.clone()));
    for (key, bytes) in objects {
        cold_mem.put(key, bytes.clone()).await.unwrap();
    }
    let mut cold = Cold {
        mem: cold_mem,
        chunk,
        opens: AtomicUsize::new(0),
        others: AtomicUsize::new(0),
        gate: None,
        cut_after: None,
        empties: false,
        claims: None,
    };
    tune(&mut cold);
    let hot = Arc::new(MemoryBackend::with_clock(rig.clock.clone()));
    let cold = Arc::new(cold);
    let config = rig.config("p1", 1 << 30);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    Run { proxy, hot, cold }
}

/// The bytes of a body and the error that ended it, if one did.
async fn drained(mut body: ReadBody) -> (Vec<u8>, Option<Error>) {
    let mut got = Vec::new();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(chunk) => got.extend_from_slice(&chunk),
            Err(error) => return (got, Some(error)),
        }
    }
    (got, None)
}

/// Bytes of a pattern that tells every position from its neighbours.
fn patterned(len: usize) -> Bytes {
    Bytes::from((0..len).map(|at| (at % 251) as u8).collect::<Vec<u8>>())
}

/// The line of a metric of the proxy, as `Proxy::metrics` renders it, with its value.
fn metric(proxy: &Proxy, line: &str) -> String {
    let rendered = proxy.metrics();
    let found = rendered.lines().find(|l| l.starts_with(line));
    found.unwrap_or("").to_owned()
}

/// Asserts that the client is given the first chunk of a cold object while the cold read is still
/// held back (the cold body gives its second chunk only when released), then the rest, that the
/// cold tier is opened once, and that nothing is written into the hot tier.
#[tokio::test(start_paused = true)]
async fn a_cold_hit_streams_before_the_cold_read_finishes() {
    let release = Arc::new(Notify::new());
    let gate = Some((1, Arc::clone(&release)));
    let run = start(&[("k", patterned(20))], 10, |cold| cold.gate = gate).await;

    let opened = run.proxy.open_read("k", None).await.unwrap();
    let mut client = opened.body;
    // A proxy that held the object whole would never get past this line: the cold body is held
    // back until `release`.
    assert_eq!(
        client.next().await.unwrap().unwrap(),
        patterned(20).slice(..10)
    );
    release.notify_one();
    assert_eq!(
        client.next().await.unwrap().unwrap(),
        patterned(20).slice(10..)
    );
    assert!(client.next().await.is_none());
    assert_eq!(run.cold.opens.load(Ordering::SeqCst), 1);
    assert_absent(&*run.hot, "k").await;
}

/// Asserts that a streamed read of a range of a cold object (the proxy's `get_stream`) is the
/// bytes of the range from one read of the cold tier, and that nothing is copied into the hot tier.
#[tokio::test(start_paused = true)]
async fn a_streamed_range_of_a_cold_object_is_one_cold_read() {
    let object = patterned(30);
    let run = start(&[("k", object.clone())], 10, |_| {}).await;
    let mut client = run
        .proxy
        .get_stream("k", Some(ByteRange::new(12, 4)))
        .await
        .unwrap();
    assert_eq!(client.next().await.unwrap().unwrap(), object.slice(12..16));
    assert!(client.next().await.is_none());
    drop(client);
    assert_absent(&*run.hot, "k").await;
    assert_eq!(run.cold.opens.load(Ordering::SeqCst), 1);
    assert_eq!(run.cold.others.load(Ordering::SeqCst), 0);
}

/// Asserts that a cold body that fails under the client counts a cold error, for a read of the
/// whole object and for a read of a range.
#[tokio::test(start_paused = true)]
async fn a_cold_body_that_fails_under_the_client_counts_a_cold_error() {
    let errors = "tierstore_read_errors_total{tier=\"cold\",op=\"get\"}";
    let first_five = Some(Span::From {
        first: 0,
        last: Some(4),
    });
    for (span, cut) in [(first_five, 0), (None, 2)] {
        let run = start(&[("k", patterned(30))], 10, |cold| {
            cold.cut_after = Some(cut);
        })
        .await;
        let opened = run.proxy.open_read("k", span).await.unwrap();
        let (_, error) = drained(opened.body).await;
        assert!(error.is_some(), "{span:?}");
        assert_eq!(metric(&run.proxy, errors), format!("{errors} 1"));
    }
}
