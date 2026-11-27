//! [`CheckUpstream`]: the reads of the check's objects answered from memory, and every other call
//! handed to the upstream.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use sha2::{Digest, Sha256};

use super::check::{CheckUpstream, TREES, WITH_SIDECAR, WITHOUT_SIDECAR, pack, pack_key};
use crate::{
    Backend, BodyDigests, ByteRange, CopyTarget, Error, Listed, MemoryBackend, Meta, OpenUpload,
    Opened, PartBody, PutAnswer, PutClaim, PutKind, PutOutcome, ReadBody, ShortObjects,
    SourceConditions, Span, StoreCopy, TreeDigest, UploadedPart, UploadsAfter, Window,
};

/// The bytes of a read stream.
async fn collected(mut body: ReadBody) -> Vec<u8> {
    let mut all = Vec::new();
    while let Some(chunk) = body.next().await {
        all.extend_from_slice(&chunk.expect("a chunk"));
    }
    all
}

/// Asserts that every read of a check object (the pack, both lists, the one sidecar) is answered
/// with its bytes, whole, ranged, streamed and opened over a span, its size, and the hex of its
/// SHA-256 as its tag, though the store under it holds none of them; that every read of the sidecar
/// of the tree without one is `NotFound`; and that a range past the end is refused as the store
/// refuses it. Catches a check whose objects are asked of the upstream (the network), and a tree
/// without a sidecar that is found to have one.
#[tokio::test]
async fn the_reads_of_the_check_objects_are_answered_from_memory() {
    let inner = Arc::new(MemoryBackend::new());
    let up = CheckUpstream::new(inner.clone());
    let pack = pack();
    let key = pack_key();
    let tag = hex::encode(Sha256::digest(&pack));
    let range = ByteRange::new(2, 5);
    assert!(matches!(inner.head(&key).await, Err(Error::NotFound(_))));
    assert_eq!(up.get(&key, None).await.unwrap(), pack);
    assert_eq!(up.get(&key, Some(range)).await.unwrap(), pack[2..7]);
    let (bytes, tagged) = up.get_tagged(&key, Some(range)).await.unwrap();
    assert_eq!(
        (bytes.as_ref(), tagged.as_str()),
        (&pack[2..7], tag.as_str())
    );
    let (bytes, _) = up.get_tagged(&key, None).await.unwrap();
    assert_eq!(bytes, pack);
    let streamed = up.get_stream(&key, Some(range)).await.unwrap();
    assert_eq!(collected(streamed).await, pack[2..7]);
    let meta = up.head(&key).await.unwrap();
    assert_eq!(
        meta,
        Meta {
            size: pack.len() as u64,
            tag,
            modified: 0
        }
    );
    let span = Span::From {
        first: 2,
        last: Some(6),
    };
    let opened = up.open_read(&key, Some(span)).await.unwrap();
    assert_eq!(opened.meta, meta);
    assert_eq!(opened.window, Window::of(Some(span), meta.size));
    assert_eq!(collected(opened.body).await, pack[2..7]);
    let whole = up.open_read(&key, None).await.unwrap();
    assert_eq!(collected(whole.body).await, pack);
    for tree in TREES {
        assert_eq!(up.get(&tree.key(), None).await.unwrap(), tree.list);
    }
    let sidecar = up.get(&WITH_SIDECAR.sidecar_key(), None).await.unwrap();
    assert_eq!(Some(sidecar.as_ref()), WITH_SIDECAR.sidecar);
    let none = WITHOUT_SIDECAR.sidecar_key();
    assert!(matches!(up.get(&none, None).await, Err(Error::NotFound(k)) if k == none));
    assert!(matches!(
        up.get_tagged(&none, None).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        up.get_stream(&none, None).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(
        up.open_read(&none, None).await,
        Err(Error::NotFound(_))
    ));
    assert!(matches!(up.head(&none).await, Err(Error::NotFound(_))));
    let past = Some(ByteRange::new(10_000, 1));
    assert!(matches!(up.get(&key, past).await, Err(Error::Other(_))));
    assert!(matches!(
        up.get_tagged(&key, past).await,
        Err(Error::Other(_))
    ));
    assert!(matches!(
        up.get_stream(&key, past).await,
        Err(Error::Other(_))
    ));
}

/// A store that answers every call with the name of the call and its key, and records it.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
}

impl Recorder {
    /// Records `call` on `key`, and refuses it with the same words.
    fn saw<T>(&self, call: &str, key: &str) -> Result<T, Error> {
        let said = format!("{call} {key}");
        let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        calls.push(said.clone());
        Err(Error::Unsupported(said))
    }
}

#[async_trait]
impl Backend for Recorder {
    fn needs_length(&self) -> bool {
        true
    }

    fn single_put_max(&self) -> Option<u64> {
        Some(7)
    }

    async fn put(&self, key: &str, _body: Bytes) -> Result<(), Error> {
        self.saw("put", key)
    }

    async fn put_tagged(&self, key: &str, _body: Bytes) -> Result<String, Error> {
        self.saw("put_tagged", key)
    }

    async fn put_stream(&self, key: &str, _: Option<u64>, _: PartBody<'_>) -> Result<(), Error> {
        self.saw("put_stream", key)
    }

    async fn put_stream_answered(
        &self,
        key: &str,
        _: Option<u64>,
        _: PartBody<'_>,
        _: bool,
    ) -> Result<Option<PutAnswer>, Error> {
        self.saw("put_stream_answered", key)
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        _: Option<u64>,
        _: PartBody<'_>,
    ) -> Result<(), Error> {
        self.saw("put_stream_hot_only", key)
    }

    async fn put_stream_replacing(
        &self,
        key: &str,
        _: Option<u64>,
        _: PartBody<'_>,
        _: &PutClaim,
    ) -> Result<Option<PutAnswer>, Error> {
        self.saw("put_stream_replacing", key)
    }

    async fn put_stream_digested(
        &self,
        key: &str,
        _: Option<u64>,
        _: PartBody<'_>,
        _: PutKind<'_>,
        _: BodyDigests,
    ) -> Result<Option<PutAnswer>, Error> {
        self.saw("put_stream_digested", key)
    }

    fn short_objects(&self) -> ShortObjects {
        ShortObjects {
            removed: 1,
            left: 2,
            failed: 3,
            skipped: 4,
            failed_aborts: 5,
            aborts_dropped: 6,
        }
    }

    async fn put_stream_tagged(
        &self,
        key: &str,
        _: Option<u64>,
        _: PartBody<'_>,
    ) -> Result<String, Error> {
        self.saw("put_stream_tagged", key)
    }

    async fn put_stream_sha256(
        &self,
        key: &str,
        _: u64,
        _: PartBody<'_>,
        _: TreeDigest,
    ) -> Result<PutOutcome, Error> {
        self.saw("put_stream_sha256", key)
    }

    async fn put_if_match(&self, key: &str, _: Bytes, _: &str) -> Result<String, Error> {
        self.saw("put_if_match", key)
    }

    async fn put_stream_if_match(
        &self,
        key: &str,
        _: Option<u64>,
        _: PartBody<'_>,
        _: &str,
    ) -> Result<String, Error> {
        self.saw("put_stream_if_match", key)
    }

    async fn get(&self, key: &str, _: Option<ByteRange>) -> Result<Bytes, Error> {
        self.saw("get", key)
    }

    async fn get_tagged(&self, key: &str, _: Option<ByteRange>) -> Result<(Bytes, String), Error> {
        self.saw("get_tagged", key)
    }

    async fn get_stream(&self, key: &str, _: Option<ByteRange>) -> Result<ReadBody, Error> {
        self.saw("get_stream", key)
    }

    fn same_version(&self, _: &Meta, _: &Meta) -> bool {
        true
    }

    async fn open_read(&self, key: &str, _: Option<Span>) -> Result<Opened, Error> {
        self.saw("open_read", key)
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.saw("head", key)
    }

    async fn copy_in_store(&self, copy: &StoreCopy<'_>) -> Result<Option<PutAnswer>, Error> {
        self.saw("copy_in_store", copy.key)
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.saw("delete", key)
    }

    async fn delete_if_match(&self, key: &str, _: &str) -> Result<(), Error> {
        self.saw("delete_if_match", key)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.saw("list", prefix)
    }

    async fn list_page(
        &self,
        prefix: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<Vec<Listed>, Error> {
        self.saw("list_page", prefix)
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        self.saw("create_upload", key)
    }

    async fn upload_part(
        &self,
        key: &str,
        _: &str,
        _: u32,
        _: PartBody<'_>,
    ) -> Result<String, Error> {
        self.saw("upload_part", key)
    }

    async fn upload_part_with_len(
        &self,
        key: &str,
        _: &str,
        _: u32,
        _: Option<u64>,
        _: PartBody<'_>,
    ) -> Result<String, Error> {
        self.saw("upload_part_with_len", key)
    }

    async fn complete_upload(
        &self,
        key: &str,
        _: &str,
        _: &[(u32, String)],
    ) -> Result<String, Error> {
        self.saw("complete_upload", key)
    }

    async fn complete_upload_if_match(
        &self,
        key: &str,
        _: &str,
        _: &[(u32, String)],
        _: &str,
    ) -> Result<String, Error> {
        self.saw("complete_upload_if_match", key)
    }

    async fn complete_upload_replacing(
        &self,
        key: &str,
        _: &str,
        _: &[(u32, String)],
    ) -> Result<String, Error> {
        self.saw("complete_upload_replacing", key)
    }

    async fn abort_upload(&self, key: &str, _: &str) -> Result<(), Error> {
        self.saw("abort_upload", key)
    }

    async fn list_uploads(
        &self,
        prefix: &str,
        _: Option<UploadsAfter<'_>>,
        _: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        self.saw("list_uploads", prefix)
    }

    async fn list_parts(
        &self,
        key: &str,
        _: &str,
        _: Option<u32>,
        _: usize,
    ) -> Result<Vec<UploadedPart>, Error> {
        self.saw("list_parts", key)
    }
}

/// A body of one chunk.
fn body() -> PartBody<'static> {
    futures::stream::once(std::future::ready(Ok(Bytes::from_static(b"x")))).boxed()
}

/// The words a call that reached the recorder was refused with.
fn said<T>(answer: Result<T, Error>) -> String {
    match answer {
        Err(Error::Unsupported(said)) => said,
        Err(other) => panic!("another error: {other:?}"),
        Ok(_) => panic!("an answer the recorder never gives"),
    }
}

/// Asserts that every call of the store that is not a read of a check object reaches the store
/// under the wrapper as that same call, with its key: each put, the conditional ones, the reads of
/// other keys (one of them under `cas/check/`), the deletes, the listings and the calls of a
/// multipart upload, and that the store's own answers of what it is (`needs_length`,
/// `single_put_max`, `short_objects`, `same_version`) are the wrapper's. Catches a call the
/// wrapper answers by a default of the trait instead of the store's own call (a streamed put made
/// whole, a page cut from a whole listing, a streamed read made of a whole one).
#[tokio::test]
async fn every_other_call_reaches_the_upstream_as_itself() {
    let inner = Arc::new(Recorder::default());
    let up = CheckUpstream::new(inner.clone());
    assert!(up.needs_length());
    assert_eq!(up.single_put_max(), Some(7));
    assert_eq!(up.short_objects(), inner.short_objects());
    let meta = |tag: &str| Meta {
        size: 1,
        tag: tag.to_owned(),
        modified: 0,
    };
    assert!(up.same_version(&meta("a"), &meta("b")));
    let x = || Bytes::from_static(b"x");
    let parts = [(1, "e".to_owned())];
    let conditions = SourceConditions::default();
    let copy = StoreCopy {
        source: "k/from",
        source_size: 1,
        conditions: &conditions,
        key: "k/copy",
        target: CopyTarget::Object,
    };
    let answers = [
        said(up.put("k", x()).await),
        said(up.put_tagged("k", x()).await),
        said(up.put_stream("k", Some(1), body()).await),
        said(up.put_stream_answered("k", Some(1), body(), true).await),
        said(up.put_stream_hot_only("k", Some(1), body()).await),
        said(
            up.put_stream_replacing("k", Some(1), body(), &PutClaim::default())
                .await,
        ),
        said(
            up.put_stream_digested(
                "k",
                Some(1),
                body(),
                PutKind::Creates,
                BodyDigests::default(),
            )
            .await,
        ),
        said(up.put_stream_tagged("k", Some(1), body()).await),
        said(
            up.put_stream_sha256("k", 1, body(), TreeDigest::of(b"x"))
                .await,
        ),
        said(up.put_if_match("k", x(), "e").await),
        said(up.put_stream_if_match("k", Some(1), body(), "e").await),
        said(up.get("cas/check/other", None).await),
        said(up.get_tagged("k", None).await),
        said(up.get_stream("k", None).await),
        said(up.open_read("k", None).await),
        said(up.head("k").await),
        said(up.copy_in_store(&copy).await),
        said(up.delete("k").await),
        said(up.delete_if_match("k", "e").await),
        said(up.list("p/").await),
        said(up.list_page("p/", Some("p/a"), 2).await),
        said(up.create_upload("k").await),
        said(up.upload_part("k", "u", 1, body()).await),
        said(up.upload_part_with_len("k", "u", 1, Some(1), body()).await),
        said(up.complete_upload("k", "u", &parts).await),
        said(up.complete_upload_if_match("k", "u", &parts, "e").await),
        said(up.complete_upload_replacing("k", "u", &parts).await),
        said(up.abort_upload("k", "u").await),
        said(up.list_uploads("p/", None, 2).await),
        said(up.list_parts("k", "u", None, 2).await),
    ];
    let expected = [
        "put k",
        "put_tagged k",
        "put_stream k",
        "put_stream_answered k",
        "put_stream_hot_only k",
        "put_stream_replacing k",
        "put_stream_digested k",
        "put_stream_tagged k",
        "put_stream_sha256 k",
        "put_if_match k",
        "put_stream_if_match k",
        "get cas/check/other",
        "get_tagged k",
        "get_stream k",
        "open_read k",
        "head k",
        "copy_in_store k/copy",
        "delete k",
        "delete_if_match k",
        "list p/",
        "list_page p/",
        "create_upload k",
        "upload_part k",
        "upload_part_with_len k",
        "complete_upload k",
        "complete_upload_if_match k",
        "complete_upload_replacing k",
        "abort_upload k",
        "list_uploads p/",
        "list_parts k",
    ];
    assert_eq!(answers, expected);
    let calls = inner.calls.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(*calls, expected);
}
