//! The proxy's one-call read: a hit in the hot tier is one call to it (the object's meta and a
//! body that is the tier's own stream), a range is resolved by the read itself, a miss falls to
//! the cold tier as a read always did, and the streaming `get_stream` of the proxy never holds
//! the object.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use tierstore::{
    Backend, ByteRange, DirBackend, Error, Listed, MemoryBackend, Meta, Op, Opened, Proxy,
    ReadBody, Span, Window,
};
use tierstore_testkit::dir::TestDir;

use crate::conformance::{joined, reservation};
use crate::support::{Rig, assert_absent, body, copied, read, set_usage};

/// The bytes the objects of these tests hold.
const TEN: &str = "0123456789";

/// A tier over memory that gives an open read as chunks of two bytes, counting the chunks that
/// were taken from the body, and that refuses every other read of an object of the data (a key
/// that does not start with `_`) and counts the attempts, so that a test sees which calls a read
/// makes and whether the proxy took the body whole.
struct Counted {
    mem: Arc<MemoryBackend>,
    /// Calls of `open_read`.
    opens: AtomicUsize,
    /// Calls of `get`, `get_stream` and `head` for the data.
    others: AtomicUsize,
    /// Chunks taken from the bodies given.
    taken: Arc<AtomicUsize>,
    /// Whether a body ends with an error after its chunks.
    cut: bool,
}

impl Counted {
    fn new(mem: &Arc<MemoryBackend>, cut: bool) -> Arc<Self> {
        Arc::new(Self {
            mem: Arc::clone(mem),
            opens: AtomicUsize::new(0),
            others: AtomicUsize::new(0),
            taken: Arc::new(AtomicUsize::new(0)),
            cut,
        })
    }

    fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }

    fn others(&self) -> usize {
        self.others.load(Ordering::SeqCst)
    }

    fn taken(&self) -> usize {
        self.taken.load(Ordering::SeqCst)
    }

    fn refuse(&self, key: &str) -> Result<(), Error> {
        if key.starts_with('_') {
            return Ok(());
        }
        self.others.fetch_add(1, Ordering::SeqCst);
        Err(Error::Other(format!(
            "{key} was read by a call that is not an open read"
        )))
    }
}

#[async_trait]
impl Backend for Counted {
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

    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        self.refuse(key)?;
        let whole = self.mem.get(key, range).await?;
        Ok(stream::once(std::future::ready(Ok(whole))).boxed())
    }

    async fn open_read(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let meta = self.mem.head(key).await?;
        let object = self.mem.get(key, None).await?;
        let window = Window::of(span, meta.size);
        let held = window.slice(&object);
        let taken = Arc::clone(&self.taken);
        let mut pieces: Vec<Result<Bytes, Error>> = held
            .chunks(2)
            .map(|piece| Ok(Bytes::copy_from_slice(piece)))
            .collect();
        if self.cut {
            // Cut: two of the chunks, and then the error.
            pieces.truncate(2);
            pieces.push(Err(Error::Unavailable("cut".to_owned())));
        }
        let body = stream::iter(pieces).inspect(move |_| {
            taken.fetch_add(1, Ordering::SeqCst);
        });
        Ok(Opened {
            meta,
            window,
            body: body.boxed(),
        })
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.refuse(key)?;
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

/// The calls a rig's tiers have had so far: the hot tier's gets and heads, the cold tier's.
fn calls(rig: &Rig) -> [u64; 4] {
    [
        rig.hot.calls(Op::Get),
        rig.hot.calls(Op::Head),
        rig.cold.calls(Op::Get),
        rig.cold.calls(Op::Head),
    ]
}

/// The calls made since `before`, as `calls` counts them. A read that misses the hot tier makes
/// one more get of it than the open read: the one of the usage, which decides whether the object
/// is copied back.
fn since(rig: &Rig, before: [u64; 4]) -> [u64; 4] {
    let now = calls(rig);
    [0, 1, 2, 3].map(|at| now[at] - before[at])
}

/// Asserts that a read of an object in the hot tier is one call to the hot tier, an open read,
/// and none to the cold tier and no HEAD: the meta is the object's own, and the bytes are
/// those of the object, whether it is read by the open read or as a stream of the proxy.
#[tokio::test(start_paused = true)]
async fn a_hot_hit_is_one_call_to_the_hot_tier() {
    let rig = Rig::new();
    rig.hot_mem.put("k", body(TEN)).await.unwrap();
    let proxy = rig.proxy("p1", 1000).await;
    let head = rig.hot_mem.head("k").await.unwrap();

    let before = calls(&rig);
    let opened = proxy.open_read("k", None).await.unwrap();
    assert_eq!(since(&rig, before), [1, 0, 0, 0]);
    assert_eq!(opened.meta, head);
    assert_eq!(opened.window, Window::Whole);
    let shown = format!("{opened:?}");
    assert!(shown.starts_with("Opened { meta: Meta {"), "{shown}");
    assert_eq!(joined(opened).await, body(TEN));

    let before = calls(&rig);
    let mut stream = proxy.get_stream("k", None).await.unwrap();
    assert_eq!(since(&rig, before), [1, 0, 0, 0]);
    assert_eq!(stream.next().await.unwrap().unwrap(), body(TEN));
    assert!(stream.next().await.is_none());
}

/// Asserts that the proxy gives the hot tier's body as it is and does not collect it: when the
/// open read returns, no chunk of the body has been taken; each chunk the caller asks for is
/// one taken from the tier, in order, and the rest stay in the tier. The same holds for the
/// proxy's `get_stream`, whole and by range, and no read but the open read is made.
#[tokio::test(start_paused = true)]
async fn a_hot_body_is_streamed_and_not_collected() {
    let rig = Rig::new();
    let mem = Arc::new(MemoryBackend::with_clock(rig.clock.clone()));
    mem.put("k", body(TEN)).await.unwrap();
    let hot = Counted::new(&mem, false);
    let proxy = Proxy::open(hot.clone(), rig.cold.clone(), rig.config("p1", 1000))
        .await
        .unwrap();

    let opened = proxy.open_read("k", None).await.unwrap();
    assert_eq!(hot.taken(), 0);
    let mut stream = opened.body;
    assert_eq!(stream.next().await.unwrap().unwrap(), body("01"));
    assert_eq!(hot.taken(), 1);
    assert_eq!(stream.next().await.unwrap().unwrap(), body("23"));
    assert_eq!(hot.taken(), 2);
    let mut rest = Vec::new();
    while let Some(chunk) = stream.next().await {
        rest.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(rest, b"456789");
    assert_eq!(hot.taken(), 5);

    let mut part = proxy
        .get_stream("k", Some(ByteRange::new(3, 4)))
        .await
        .unwrap();
    assert_eq!(hot.taken(), 5);
    assert_eq!(part.next().await.unwrap().unwrap(), body("34"));
    assert_eq!(hot.taken(), 6);
    assert_eq!(part.next().await.unwrap().unwrap(), body("56"));
    assert!(part.next().await.is_none());
    assert_eq!((hot.opens(), hot.others()), (2, 0));
}

/// Asserts that the open read resolves a range against the size it finds: the window and the
/// bytes of a range cut at the end, an open-ended one, the last bytes, and one that starts at the
/// end (unsatisfiable, with the object's meta and no bytes), each in one call to the hot tier.
#[tokio::test(start_paused = true)]
async fn a_ranged_hot_read_is_resolved_by_the_one_call() {
    let rig = Rig::new();
    rig.hot_mem.put("k", body(TEN)).await.unwrap();
    let proxy = rig.proxy("p1", 1000).await;
    let from = |first, last| Some(Span::From { first, last });
    let part = |offset, len| Window::Part(ByteRange::new(offset, len));
    let cases = [
        (from(2, Some(4)), part(2, 3), "234"),
        (from(7, None), part(7, 3), "789"),
        (from(8, Some(99)), part(8, 2), "89"),
        (Some(Span::Suffix(4)), part(6, 4), "6789"),
        (from(10, None), Window::Unsatisfiable, ""),
    ];
    for (span, window, want) in cases {
        let before = calls(&rig);
        let opened = proxy.open_read("k", span).await.unwrap();
        assert_eq!(since(&rig, before), [1, 0, 0, 0], "{span:?}");
        assert_eq!(opened.window, window, "{span:?}");
        assert_eq!(opened.meta.size, 10, "{span:?}");
        assert_eq!(joined(opened).await, body(want), "{span:?}");
    }
}

/// Asserts that the proxy's streamed read of a byte range gives what `get` gives: the bytes of
/// the range cut at the end, none for a range of no bytes and for one that starts at the end, an
/// error for one that starts past it, whether the object is in the hot tier or only in the cold.
#[tokio::test(start_paused = true)]
async fn a_streamed_read_of_a_range_gives_what_a_get_gives() {
    let rig = Rig::new();
    rig.hot_mem.put("hot", body(TEN)).await.unwrap();
    rig.cold_mem.put("cold", body(TEN)).await.unwrap();
    let proxy = rig.proxy("p1", 1000).await;
    for key in ["hot", "cold"] {
        let cases = [
            (0, 10, TEN),
            (3, 4, "3456"),
            (8, 100, "89"),
            (4, 0, ""),
            (10, 5, ""),
        ];
        for (offset, len, want) in cases {
            let range = Some(ByteRange::new(offset, len));
            let mut read = proxy.get_stream(key, range).await.unwrap();
            let mut got = Vec::new();
            while let Some(chunk) = read.next().await {
                got.extend_from_slice(&chunk.unwrap());
            }
            assert_eq!(got, want.as_bytes(), "{key} {offset}+{len}");
            assert_eq!(proxy.get(key, range).await.unwrap(), body(want));
        }
        let past = proxy.get_stream(key, Some(ByteRange::new(11, 1))).await;
        assert!(
            matches!(past, Err(Error::Other(_))),
            "{key}: {:?}",
            past.err()
        );
    }
}

/// Asserts that a key neither tier holds is `NotFound` from the open read and from the streamed
/// read, after one call to each tier and no HEAD.
#[tokio::test(start_paused = true)]
async fn a_missing_key_is_not_found_after_one_call_to_each_tier() {
    let rig = Rig::new();
    let proxy = rig.proxy("p1", 1000).await;
    let missing = Error::NotFound("absent".to_owned());

    let before = calls(&rig);
    let opened = proxy.open_read("absent", None).await;
    assert_eq!(opened.err(), Some(missing.clone()));
    assert_eq!(since(&rig, before), [2, 0, 1, 0]);
    let streamed = proxy.get_stream("absent", None).await;
    assert_eq!(streamed.err(), Some(missing));
}

/// Asserts that a miss in the hot tier is read from the cold tier in one call, with the cold
/// object's meta, and copied back whole into the hot tier, so that the next read is one call to
/// the hot tier and none to the cold one; and that a range of the cold object is the cold tier's
/// own read of that range, which copies nothing.
#[tokio::test(start_paused = true)]
async fn a_hot_miss_falls_to_the_cold_tier_and_is_copied_back() {
    let rig = Rig::new();
    rig.cold_mem.put("k", body(TEN)).await.unwrap();
    rig.cold_mem.put("r", body(TEN)).await.unwrap();
    let proxy = rig.proxy("p1", 1000).await;
    let head = rig.cold_mem.head("k").await.unwrap();

    let before = calls(&rig);
    let opened = proxy.open_read("k", None).await.unwrap();
    assert_eq!(since(&rig, before), [2, 0, 1, 0]);
    assert_eq!(opened.meta, head);
    assert_eq!(opened.window, Window::Whole);
    assert_eq!(joined(opened).await, body(TEN));
    copied(&rig, "k").await;
    assert_eq!(read(&*rig.hot_mem, "k").await, body(TEN));

    let before = calls(&rig);
    let again = proxy.open_read("k", None).await.unwrap();
    assert_eq!(since(&rig, before), [1, 0, 0, 0]);
    assert_eq!(joined(again).await, body(TEN));

    let opened = proxy.open_read("r", Some(Span::Suffix(3))).await.unwrap();
    assert_eq!(opened.window, Window::Part(ByteRange::new(7, 3)));
    assert_eq!(opened.meta.size, 10);
    assert_eq!(joined(opened).await, body("789"));
    assert_absent(&*rig.hot_mem, "r").await;
}

/// Asserts that with the hot tier at its critical mark a read of the cold tier copies nothing
/// back: a range is the cold tier's own stream of that range (one call, no HEAD) and a whole
/// read is the bytes read once.
#[tokio::test(start_paused = true)]
async fn at_the_critical_mark_a_cold_read_copies_nothing_back() {
    let rig = Rig::new();
    rig.cold_mem.put("k", body(TEN)).await.unwrap();
    let proxy = rig.proxy("p1", 1000).await;
    set_usage(&rig, 960, 0).await;

    let before = calls(&rig);
    let span = Some(Span::From {
        first: 2,
        last: Some(4),
    });
    let opened = proxy.open_read("k", span).await.unwrap();
    assert_eq!(since(&rig, before), [2, 0, 1, 0]);
    assert_eq!(opened.window, Window::Part(ByteRange::new(2, 3)));
    assert_eq!(opened.meta.size, 10);
    assert_eq!(joined(opened).await, body("234"));

    let whole = proxy.open_read("k", None).await.unwrap();
    assert_eq!(joined(whole).await, body(TEN));
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts that a reservation in the cold tier is no object to a read: `NotFound` for the open
/// read of the whole and of a range at the critical mark, and nothing is copied back.
#[tokio::test(start_paused = true)]
async fn a_reservation_in_the_cold_tier_is_not_found() {
    let rig = Rig::new();
    rig.cold_mem
        .put_reservation("k", reservation(1))
        .await
        .unwrap();
    let proxy = rig.proxy("p1", 1000).await;
    let missing = Error::NotFound("k".to_owned());

    assert_eq!(
        proxy.open_read("k", None).await.err(),
        Some(missing.clone())
    );
    set_usage(&rig, 960, 0).await;
    let span = Some(Span::Suffix(1));
    assert_eq!(proxy.open_read("k", span).await.err(), Some(missing));
    assert_absent(&*rig.hot_mem, "k").await;
}

/// Asserts that a failing tier fails the read with its error: a hot tier that fails is not
/// followed by the cold tier, and a cold tier that fails after a hot miss is the error of the
/// read.
#[tokio::test(start_paused = true)]
async fn a_tier_that_fails_fails_the_read() {
    let rig = Rig::new();
    rig.hot_mem.put("hot", body(TEN)).await.unwrap();
    rig.cold_mem.put("cold", body(TEN)).await.unwrap();
    let proxy = rig.proxy("p1", 1000).await;

    rig.hot.fail_next(Op::Get, 1);
    let before = calls(&rig);
    let failed = proxy.open_read("hot", None).await.err().unwrap();
    assert!(failed.is_unavailable(), "{failed}");
    assert_eq!(since(&rig, before), [1, 0, 0, 0]);

    rig.cold.fail_next(Op::Get, 1);
    let failed = proxy.open_read("cold", None).await.err().unwrap();
    assert!(failed.is_unavailable(), "{failed}");
    assert_absent(&*rig.hot_mem, "cold").await;
}

/// Asserts that a cold body that is cut gives the client the bytes that came and then the error
/// of the cut, and that the cold tier was opened once. (That nothing is copied into the hot tier
/// is asserted in `cold_stream.rs`, where the end of the write can be waited for.)
#[tokio::test(start_paused = true)]
async fn a_cold_body_that_is_cut_fails_the_read() {
    let rig = Rig::new();
    let mem = Arc::new(MemoryBackend::with_clock(rig.clock.clone()));
    mem.put("k", body(TEN)).await.unwrap();
    let cold = Counted::new(&mem, true);
    let proxy = Proxy::open(rig.hot.clone(), cold.clone(), rig.config("p1", 1000))
        .await
        .unwrap();

    let mut stream = proxy.open_read("k", None).await.unwrap().body;
    assert_eq!(stream.next().await.unwrap().unwrap(), body("01"));
    assert_eq!(stream.next().await.unwrap().unwrap(), body("23"));
    let failed = stream.next().await.unwrap().unwrap_err();
    assert!(failed.is_unavailable(), "{failed}");
    assert!(stream.next().await.is_none());
    assert_eq!(cold.opens(), 1);
}

/// Asserts that an object much larger than one chunk, in a directory hot tier, reaches the
/// caller as chunks of a bounded size, many of them, that join to the object, and that a range of
/// it is the chunks of the range alone: the proxy gives the tier's stream and does not collect it.
#[tokio::test(start_paused = true)]
async fn a_large_hot_object_arrives_as_bounded_chunks() {
    let rig = Rig::new();
    let dir = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "open-read-dir");
    let hot = Arc::new(DirBackend::open(dir.path()).unwrap());
    let large: Vec<u8> = (0..300_000_u32).map(|at| (at % 251) as u8).collect();
    hot.put("big", Bytes::from(large.clone())).await.unwrap();
    let proxy = Proxy::open(hot, rig.cold.clone(), rig.config("p1", 10_000_000))
        .await
        .unwrap();

    let opened = proxy.open_read("big", None).await.unwrap();
    assert_eq!(opened.meta.size, 300_000);
    let mut sizes = Vec::new();
    let mut joined = Vec::new();
    let mut stream = opened.body;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        sizes.push(chunk.len());
        joined.extend_from_slice(&chunk);
    }
    assert!(sizes.len() >= 4, "{sizes:?}");
    assert!(sizes.iter().all(|size| *size <= 64 * 1024), "{sizes:?}");
    assert_eq!(joined, large);

    let span = Some(Span::From {
        first: 100,
        last: Some(199_999),
    });
    let part = proxy.open_read("big", span).await.unwrap();
    assert_eq!(part.window, Window::Part(ByteRange::new(100, 199_900)));
    let mut stream = part.body;
    let mut got = Vec::new();
    let mut chunks = 0;
    while let Some(chunk) = stream.next().await {
        got.extend_from_slice(&chunk.unwrap());
        chunks += 1;
    }
    assert!(chunks >= 3, "{chunks}");
    assert_eq!(got, large[100..200_000]);
}

/// The line of a metric of the proxy, as `Proxy::metrics` renders it, with its value.
fn metric(proxy: &Proxy, line: &str) -> String {
    let rendered = proxy.metrics();
    let found = rendered.lines().find(|l| l.starts_with(line));
    found.unwrap_or("").to_owned()
}

/// Asserts that a streamed read of a range of no bytes, or of one that starts at or past the end,
/// of an object that only the cold tier holds is answered from the size the open read gives: an
/// empty body, or an error if the range starts past the end, with a cold tier that refuses every
/// read but the open read (nothing is collected), below the critical mark and at it.
#[tokio::test(start_paused = true)]
async fn an_empty_streamed_range_of_a_cold_object_collects_nothing() {
    for usage in [0, 960] {
        let rig = Rig::new();
        let mem = Arc::new(MemoryBackend::with_clock(rig.clock.clone()));
        mem.put("k", body(TEN)).await.unwrap();
        let cold = Counted::new(&mem, false);
        let proxy = Proxy::open(rig.hot.clone(), cold.clone(), rig.config("p1", 1000))
            .await
            .unwrap();
        set_usage(&rig, usage, 0).await;
        let cases = [
            (4, 0, true),
            (10, 0, true),
            (11, 0, false),
            (10, 5, true),
            (12, 5, false),
        ];
        for (offset, len, within) in cases {
            let read = proxy
                .get_stream("k", Some(ByteRange::new(offset, len)))
                .await;
            let case = format!("{usage} {offset}+{len}");
            assert_eq!(read.is_ok(), within, "{case}");
            if let Ok(mut read) = read {
                assert!(read.next().await.is_none(), "{case}");
            }
        }
        assert_eq!((cold.opens(), cold.others()), (cases.len(), 0), "{usage}");
        assert_absent(&*rig.hot_mem, "k").await;
    }
}

/// Asserts that a cold body that fails under a ranged streamed read made with the hot tier at its
/// critical mark (the cold tier's own stream of the range, which copies nothing) counts a cold
/// error, as a body that fails under a whole read does.
#[tokio::test(start_paused = true)]
async fn a_critical_ranged_cold_stream_that_fails_counts_a_cold_error() {
    let rig = Rig::new();
    let mem = Arc::new(MemoryBackend::with_clock(rig.clock.clone()));
    mem.put("k", body(TEN)).await.unwrap();
    let cold = Counted::new(&mem, true);
    let proxy = Proxy::open(rig.hot.clone(), cold.clone(), rig.config("p1", 1000))
        .await
        .unwrap();
    set_usage(&rig, 960, 0).await;

    let mut stream = proxy
        .get_stream("k", Some(ByteRange::new(0, 10)))
        .await
        .unwrap();
    assert_eq!(stream.next().await.unwrap().unwrap(), body("01"));
    assert_eq!(stream.next().await.unwrap().unwrap(), body("23"));
    let failed = stream.next().await.unwrap().unwrap_err();
    assert!(failed.is_unavailable(), "{failed}");
    let errors = "tierstore_read_errors_total{tier=\"cold\",op=\"get\"}";
    assert_eq!(metric(&proxy, errors), format!("{errors} 1"));
}
