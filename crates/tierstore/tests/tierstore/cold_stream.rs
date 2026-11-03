//! A read that misses the hot tier streams the cold object to the client as it arrives and, on the
//! way, into the hot tier: the cold tier is read once, the client is never held back by the copy,
//! and a copy that cannot be made whole leaves nothing in the hot tier.
//!
//! The tiers are test backends whose progress the tests see as events: the cold body waits at a
//! chosen chunk until it is released, the hot write waits at its start until it is released, and
//! the end of every hot write is told on a channel. No test waits on a clock.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use tierstore::{
    Backend, ByteRange, Error, Listed, MemoryBackend, Meta, Opened, PartBody, Proxy, ReadBody,
    Span, Window,
};
use tokio::sync::{Notify, mpsc};

use crate::support::{Rig, assert_absent};

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
        let held = Arc::new(pieces(&window.slice(&object), self.chunk));
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

/// The hot tier: objects in memory, whose create-only streamed write is watched. It tells the
/// outcome of every such write on `ended`, and every chunk its body gave on `taken`; it can be
/// made to wait at the start of the write, or to fail it.
struct Hot {
    mem: Arc<MemoryBackend>,
    /// A write that waits for the notify before it looks at its body.
    gate: Option<Arc<Notify>>,
    /// The error the next write fails with, without reading its body.
    fail: Mutex<Option<Error>>,
    ended: mpsc::UnboundedSender<Result<String, Error>>,
    taken: mpsc::UnboundedSender<()>,
    /// Calls of the streamed write.
    puts: AtomicUsize,
}

#[async_trait]
impl Backend for Hot {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.mem.put(key, body).await
    }

    async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        self.mem.put_tagged(key, body).await
    }

    async fn put_stream_tagged(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        let failure = self.fail.lock().unwrap().take();
        if let Some(error) = failure {
            let _ = self.ended.send(Err(error.clone()));
            return Err(error);
        }
        let taken = self.taken.clone();
        let body = body
            .inspect(move |item| {
                if item.is_ok() {
                    let _ = taken.send(());
                }
            })
            .boxed();
        let made = self.mem.put_stream_tagged(key, len, body).await;
        let _ = self.ended.send(made.clone());
        made
    }

    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error> {
        self.mem.put_if_match(key, body, etag).await
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.mem.get(key, range).await
    }

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        self.mem.get_stream(key, range).await
    }

    async fn open_read(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        self.mem.open_read(key, span).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.mem.head(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.mem.delete(key).await
    }

    async fn delete_if_match(&self, key: &str, etag: &str) -> Result<(), Error> {
        self.mem.delete_if_match(key, etag).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.mem.list(prefix).await
    }
}

/// A proxy over a hot and a cold test tier, and what the tests see of them.
struct Run {
    proxy: Proxy,
    hot: Arc<Hot>,
    cold: Arc<Cold>,
    /// The outcome of each streamed write into the hot tier, in order.
    ended: mpsc::UnboundedReceiver<Result<String, Error>>,
    /// One for each chunk a streamed write took from its body.
    taken: mpsc::UnboundedReceiver<()>,
}

/// Starts a proxy whose cold tier holds `objects` and gives them in chunks of `chunk` bytes;
/// `tune` sets what a test needs of the tiers before they are used.
async fn start(
    objects: &[(&str, Bytes)],
    chunk: usize,
    tune: impl FnOnce(&mut Cold, &mut Hot),
) -> Run {
    let rig = Rig::new();
    let cold_mem = Arc::new(MemoryBackend::with_clock(rig.clock.clone()));
    for (key, bytes) in objects {
        cold_mem.put(key, bytes.clone()).await.unwrap();
    }
    let (ended_tx, ended) = mpsc::unbounded_channel();
    let (taken_tx, taken) = mpsc::unbounded_channel();
    let mut cold = Cold {
        mem: cold_mem,
        chunk,
        opens: AtomicUsize::new(0),
        others: AtomicUsize::new(0),
        gate: None,
        cut_after: None,
        claims: None,
    };
    let mut hot = Hot {
        mem: Arc::new(MemoryBackend::with_clock(rig.clock.clone())),
        gate: None,
        fail: Mutex::new(None),
        ended: ended_tx,
        taken: taken_tx,
        puts: AtomicUsize::new(0),
    };
    tune(&mut cold, &mut hot);
    let (hot, cold) = (Arc::new(hot), Arc::new(cold));
    let config = rig.config("p1", 1 << 30);
    let proxy = Proxy::open(hot.clone(), cold.clone(), config)
        .await
        .unwrap();
    Run {
        proxy,
        hot,
        cold,
        ended,
        taken,
    }
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

/// Asserts that the client is given the first chunk of a cold object while the cold read is still
/// held back (the cold body gives its second chunk only when released), then the rest, and that
/// the cold tier is opened once.
#[tokio::test(start_paused = true)]
async fn a_cold_hit_streams_before_the_cold_read_finishes() {
    let release = Arc::new(Notify::new());
    let gate = Some((1, Arc::clone(&release)));
    let mut run = start(&[("k", patterned(20))], 10, |cold, _| cold.gate = gate).await;

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
    assert!(run.ended.recv().await.unwrap().is_ok());
}

/// Asserts that a cold hit is one open read of the cold tier and no other read of it, that the
/// copy in the hot tier is the exact bytes under the tag the hot write gave, that the next read is
/// served by the hot tier alone, and that a range is the window of the same single read while the
/// copy is the whole object (the client stops reading at the end of its range).
#[tokio::test(start_paused = true)]
async fn a_cold_hit_is_read_once_and_copied_back_with_its_exact_bytes() {
    let object = patterned(300_000);
    let objects = [("k", object.clone()), ("r", object.clone())];
    let mut run = start(&objects, 64 * 1024, |_, _| {}).await;

    let opened = run.proxy.open_read("k", None).await.unwrap();
    assert_eq!(opened.window, Window::Whole);
    let (got, error) = drained(opened.body).await;
    assert_eq!((got.as_slice(), error), (object.as_ref(), None));
    let tag = run.ended.recv().await.unwrap().unwrap();
    assert_eq!(run.hot.mem.get("k", None).await.unwrap(), object);
    assert_eq!(run.hot.mem.head("k").await.unwrap().tag, tag);
    assert_eq!(run.cold.opens.load(Ordering::SeqCst), 1);
    assert_eq!(run.cold.others.load(Ordering::SeqCst), 0);

    let again = run.proxy.open_read("k", None).await.unwrap();
    assert_eq!(drained(again.body).await.0, object.as_ref());
    assert_eq!(run.cold.opens.load(Ordering::SeqCst), 1);

    let span = Some(Span::From {
        first: 5,
        last: Some(9),
    });
    let ranged = run.proxy.open_read("r", span).await.unwrap();
    assert_eq!(ranged.window, Window::Part(ByteRange::new(5, 5)));
    let mut client = ranged.body;
    assert_eq!(client.next().await.unwrap().unwrap(), object.slice(5..10));
    drop(client);
    assert!(run.ended.recv().await.unwrap().is_ok());
    assert_eq!(run.hot.mem.get("r", None).await.unwrap(), object);
    assert_eq!(run.cold.opens.load(Ordering::SeqCst), 2);
    assert_eq!(run.cold.others.load(Ordering::SeqCst), 0);
}

/// Asserts that an empty cold object is given as an empty body and copied back as an empty object.
#[tokio::test(start_paused = true)]
async fn an_empty_cold_object_is_copied_back() {
    let mut run = start(&[("e", Bytes::new())], 10, |_, _| {}).await;
    let opened = run.proxy.open_read("e", None).await.unwrap();
    assert_eq!(opened.meta.size, 0);
    assert_eq!(drained(opened.body).await, (Vec::new(), None));
    assert!(run.ended.recv().await.unwrap().is_ok());
    assert_eq!(run.hot.mem.head("e").await.unwrap().size, 0);
}

/// Asserts that a read whose body is never taken (the front drops it for a 304, a 412 or a 416)
/// starts no write into the hot tier.
#[tokio::test(start_paused = true)]
async fn a_cold_read_whose_body_is_never_taken_starts_no_hot_write() {
    let run = start(&[("k", patterned(20))], 10, |_, _| {}).await;
    let opened = run.proxy.open_read("k", None).await.unwrap();
    drop(opened);
    assert_eq!(run.hot.puts.load(Ordering::SeqCst), 0);
    assert_absent(&*run.hot.mem, "k").await;
}

/// Asserts that a hot write that fails (here as soon as it starts, while the cold body is held
/// back) does not fail or cut the client's read: the client is given every byte, and nothing is
/// in the hot tier.
#[tokio::test(start_paused = true)]
async fn a_failed_hot_write_still_serves_the_client_every_byte() {
    let object = patterned(30);
    let release = Arc::new(Notify::new());
    let gate = Some((1, Arc::clone(&release)));
    let mut run = start(&[("k", object.clone())], 10, |cold, hot| {
        cold.gate = gate;
        *hot.fail.lock().unwrap() = Some(Error::Unavailable("hot is down".to_owned()));
    })
    .await;

    let opened = run.proxy.open_read("k", None).await.unwrap();
    let mut client = opened.body;
    let mut got = client.next().await.unwrap().unwrap().to_vec();
    let failed = run.ended.recv().await.unwrap().unwrap_err();
    assert!(failed.is_unavailable(), "{failed}");
    release.notify_one();
    while let Some(chunk) = client.next().await {
        got.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(got, object.as_ref());
    assert_absent(&*run.hot.mem, "k").await;
}

/// Asserts that a client that stops reading before the last byte (it went away) aborts the hot
/// write, which ends with an error, and leaves no object in the hot tier.
#[tokio::test(start_paused = true)]
async fn a_client_that_disconnects_mid_body_leaves_no_hot_object() {
    let mut run = start(&[("k", patterned(30))], 10, |_, _| {}).await;
    let opened = run.proxy.open_read("k", None).await.unwrap();
    let mut client = opened.body;
    client.next().await.unwrap().unwrap();
    // The hot write has taken the first chunk and waits for the next.
    run.taken.recv().await.unwrap();
    drop(client);
    let ended = run.ended.recv().await.unwrap();
    assert!(matches!(ended, Err(Error::Other(_))), "{ended:?}");
    assert_absent(&*run.hot.mem, "k").await;
}

/// Asserts that a slow hot write (it has not looked at its body) does not hold the client back,
/// whatever the size of the object, and that the buffering between the two is bounded: an object
/// of 20 MiB is read by the client to the end while the hot write has taken nothing, and the write
/// that was held back is then abandoned (more than 16 MiB were waiting) and leaves no object; an
/// object within the bound waits for the slow write, which then makes the exact copy.
#[tokio::test(start_paused = true)]
async fn a_slow_hot_write_never_stalls_the_client_and_is_abandoned_past_the_bound() {
    const MIB: usize = 1 << 20;
    let large = patterned(20 * MIB);
    let release = Arc::new(Notify::new());
    let gate = Arc::clone(&release);
    let mut run = start(&[("big", large.clone())], MIB, |_, hot| {
        hot.gate = Some(gate)
    })
    .await;
    let opened = run.proxy.open_read("big", None).await.unwrap();
    let (got, error) = drained(opened.body).await;
    assert!(error.is_none());
    assert!(got == large.as_ref(), "the client's bytes differ");
    release.notify_one();
    let ended = run.ended.recv().await.unwrap();
    assert!(matches!(ended, Err(Error::Other(_))), "{ended:?}");
    assert_absent(&*run.hot.mem, "big").await;

    let small = patterned(4 * MIB);
    let release = Arc::new(Notify::new());
    let gate = Arc::clone(&release);
    let mut run = start(&[("small", small.clone())], MIB, |_, hot| {
        hot.gate = Some(gate)
    })
    .await;
    let opened = run.proxy.open_read("small", None).await.unwrap();
    let (got, error) = drained(opened.body).await;
    assert!(error.is_none());
    assert!(got == small.as_ref(), "the client's bytes differ");
    assert_absent(&*run.hot.mem, "small").await;
    release.notify_one();
    assert!(run.ended.recv().await.unwrap().is_ok());
    assert!(run.hot.mem.get("small", None).await.unwrap() == small);
}

/// Asserts that a cold body that fails part way gives the client the error after the bytes that
/// came, and copies nothing; and so does one that fails after the end of the client's range, which
/// the client has been given whole.
#[tokio::test(start_paused = true)]
async fn a_cold_body_that_fails_copies_nothing() {
    let mut run = start(&[("k", patterned(30))], 10, |cold, _| {
        cold.cut_after = Some(1)
    })
    .await;
    let opened = run.proxy.open_read("k", None).await.unwrap();
    let mut client = opened.body;
    assert_eq!(
        client.next().await.unwrap().unwrap(),
        patterned(30).slice(..10)
    );
    let failed = client.next().await.unwrap().unwrap_err();
    assert!(failed.is_unavailable(), "{failed}");
    assert!(client.next().await.is_none());
    drop(client);
    let ended = run.ended.recv().await.unwrap();
    assert!(matches!(ended, Err(Error::Other(_))), "{ended:?}");
    assert_absent(&*run.hot.mem, "k").await;

    let mut run = start(&[("k", patterned(30))], 10, |cold, _| {
        cold.cut_after = Some(2)
    })
    .await;
    let span = Some(Span::From {
        first: 0,
        last: Some(4),
    });
    let opened = run.proxy.open_read("k", span).await.unwrap();
    let mut client = opened.body;
    assert_eq!(
        client.next().await.unwrap().unwrap(),
        patterned(30).slice(..5)
    );
    drop(client);
    let ended = run.ended.recv().await.unwrap();
    assert!(matches!(ended, Err(Error::Other(_))), "{ended:?}");
    assert_absent(&*run.hot.mem, "k").await;
}

/// Asserts that a cold body whose bytes are not the size its meta gave is not copied: a body
/// longer than the meta says, and a body shorter, each leave nothing in the hot tier and are
/// given to the client as they are.
#[tokio::test(start_paused = true)]
async fn a_cold_body_of_another_length_than_its_meta_is_not_copied() {
    for claimed in [5, 20] {
        let object = patterned(10);
        let mut run = start(&[("k", object.clone())], 10, |cold, _| {
            cold.claims = Some(claimed);
        })
        .await;
        let opened = run.proxy.open_read("k", None).await.unwrap();
        assert_eq!(opened.meta.size, claimed);
        let (got, error) = drained(opened.body).await;
        assert_eq!(
            (got.as_slice(), error),
            (object.as_ref(), None),
            "{claimed}"
        );
        let ended = run.ended.recv().await.unwrap();
        assert!(
            matches!(ended, Err(Error::Other(_))),
            "{claimed}: {ended:?}"
        );
        assert_absent(&*run.hot.mem, "k").await;
    }
}

/// Asserts that a streamed read of a range of a cold object (the proxy's `get_stream`) is the
/// bytes of the range from one read of the cold tier, and that the whole object is copied back.
#[tokio::test(start_paused = true)]
async fn a_streamed_range_of_a_cold_object_is_one_cold_read() {
    let object = patterned(30);
    let mut run = start(&[("k", object.clone())], 10, |_, _| {}).await;
    let mut client = run
        .proxy
        .get_stream("k", Some(ByteRange::new(12, 4)))
        .await
        .unwrap();
    assert_eq!(client.next().await.unwrap().unwrap(), object.slice(12..16));
    drop(client);
    assert!(run.ended.recv().await.unwrap().is_ok());
    assert_eq!(run.hot.mem.get("k", None).await.unwrap(), object);
    assert_eq!(run.cold.opens.load(Ordering::SeqCst), 1);
    assert_eq!(run.cold.others.load(Ordering::SeqCst), 0);
}
