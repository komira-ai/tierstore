//! The minimal API of an S3-compatible store, as this crate uses it.

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use futures::stream::BoxStream;

use crate::Error;

/// The body of a part of a multipart upload, as the chunks it arrives in, so that a part of any
/// size is stored without being held in memory whole.
///
/// A chunk that is an error ends the body: the backend stores nothing of the part and gives
/// that error.
pub type PartBody<'a> = BoxStream<'a, Result<Bytes, Error>>;

/// The bytes of a read as the chunks they arrive in, so that a reader of an object of any size
/// holds one chunk at a time. A chunk that is an error ends the body: the bytes before it were
/// not the whole of what was asked, and a reader must not take the body for complete.
pub type ReadBody = BoxStream<'static, Result<Bytes, Error>>;

/// [`Backend::open_read`] as a HEAD and then a streamed read of the range the size resolves the
/// span to: the provided implementation, and what a backend that cannot ask for the span without
/// the size does for that span.
pub(crate) async fn open_by_head<B: Backend + ?Sized>(
    backend: &B,
    key: &str,
    span: Option<Span>,
) -> Result<Opened, Error> {
    let meta = backend.head(key).await?;
    let window = Window::of(span, meta.size);
    let body = match window {
        Window::Whole => backend.get_stream(key, None).await?,
        Window::Part(range) => backend.get_stream(key, Some(range)).await?,
        Window::Unsatisfiable => futures::stream::empty().boxed(),
    };
    Ok(Opened { meta, window, body })
}

/// The error of a multipart call that a backend does not override.
fn not_supported(call: &str) -> Error {
    let text = format!("{call}: this backend does not do multipart uploads");
    Error::Unsupported(text)
}

/// A byte range to read: `len` bytes from `offset`.
///
/// A read returns the bytes of `[offset, offset + len)` that exist, so a range running past
/// the end of the object is cut at the end, and a range of length zero gives no bytes. An
/// `offset` beyond the end of the object is an [`Error::Other`]; an `offset` at the end
/// gives no bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteRange {
    offset: u64,
    len: u64,
}

impl ByteRange {
    /// The range `[offset, offset + len)`.
    #[must_use]
    pub fn new(offset: u64, len: u64) -> Self {
        Self { offset, len }
    }

    /// The first byte.
    #[must_use]
    pub fn offset(self) -> u64 {
        self.offset
    }

    /// The number of bytes asked for.
    #[must_use]
    pub fn size(self) -> u64 {
        self.len
    }

    /// Whether the range starts within an object of `size` bytes (at its end is within, and gives
    /// no bytes).
    ///
    /// # Errors
    /// [`Error::Other`] if `offset` is beyond the end of the object.
    pub(crate) fn within(self, size: u64) -> Result<(), Error> {
        if self.offset > size {
            return Err(Error::Other(format!(
                "range starting at {} is past the end of the {size}-byte object",
                self.offset
            )));
        }
        Ok(())
    }

    /// The bytes of `body` this range covers.
    ///
    /// # Errors
    /// [`Error::Other`] if `offset` is beyond the end of `body`.
    pub fn slice(self, body: &Bytes) -> Result<Bytes, Error> {
        let size = body.len() as u64;
        self.within(size)?;
        let end = size.min(self.offset.saturating_add(self.len));
        // Both bounds are at most `size`, which came from a `usize`.
        Ok(body.slice(self.offset as usize..end as usize))
    }
}

/// A range as an HTTP `Range` header names it, which depends on the size of the object and so
/// is resolved against the size the read itself sees ([`Span::resolve`]): a read that must be
/// one call cannot be given the bytes to ask for by a HEAD before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Span {
    /// From byte `first` to byte `last`, the last of the range (it may be past the end of the
    /// object), or to the end of the object if there is none.
    From {
        /// The first byte.
        first: u64,
        /// The last byte asked for, if the range has an end.
        last: Option<u64>,
    },
    /// The last bytes of the object: this many, or all of it if it is shorter.
    Suffix(u64),
}

/// What a read of an optional [`Span`] came to, of an object of a known size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Window {
    /// All of the object: no span was asked for.
    Whole,
    /// These bytes of it, cut at the end of the object.
    Part(ByteRange),
    /// A span that starts at or past the end of the object, or that asks for the last
    /// none of it: no bytes are read.
    Unsatisfiable,
}

impl Span {
    /// The bytes of an object of `size` bytes that this span covers.
    #[must_use]
    pub fn resolve(self, size: u64) -> Window {
        let (start, last) = match self {
            Span::Suffix(count) => (size.saturating_sub(count), u64::MAX),
            Span::From { first, last } => (first, last.unwrap_or(u64::MAX)),
        };
        if start >= size {
            return Window::Unsatisfiable;
        }
        let end = last.min(size - 1) + 1;
        Window::Part(ByteRange::new(start, end.saturating_sub(start)))
    }
}

impl Window {
    /// What `span`, if there is one, covers of an object of `size` bytes.
    #[must_use]
    pub fn of(span: Option<Span>, size: u64) -> Self {
        span.map_or(Window::Whole, |span| span.resolve(size))
    }

    /// The bytes of `object`, the whole of it, that this window covers.
    #[must_use]
    pub fn slice(self, object: &Bytes) -> Bytes {
        match self {
            Window::Whole => object.clone(),
            Window::Part(range) => {
                let len = object.len() as u64;
                let start = range.offset().min(len);
                let end = range.offset().saturating_add(range.size()).min(len);
                object.slice(start as usize..end as usize)
            }
            Window::Unsatisfiable => Bytes::new(),
        }
    }

    /// The number of bytes the read gives, of an object of `size` bytes.
    #[must_use]
    pub fn bytes(self, size: u64) -> u64 {
        match self {
            Window::Whole => size,
            Window::Part(range) => range.size(),
            Window::Unsatisfiable => 0,
        }
    }
}

/// A read that is open: the object's [`Meta`], the [`Window`] of it that the body holds, and the
/// body as the chunks it arrives in. See [`Backend::open_read`].
pub struct Opened {
    /// What the object that the body is read from is.
    pub meta: Meta,
    /// Which bytes of that object the body holds.
    pub window: Window,
    /// The bytes, as in [`ReadBody`]; empty for an [`Window::Unsatisfiable`] window.
    pub body: ReadBody,
}

impl Opened {
    /// The number of bytes the body is to give.
    #[must_use]
    pub fn served(&self) -> u64 {
        self.window.bytes(self.meta.size)
    }
}

impl std::fmt::Debug for Opened {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Opened")
            .field("meta", &self.meta)
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

/// What a HEAD says about an object: its size, a content tag and when it was last written.
///
/// Two objects are taken to hold the same bytes when [`Meta::same_content`] says so, so a
/// backend's tag must be a function of the content (a checksum, or an entity tag that the
/// backend computes from the bytes) and the same function on both sides of a comparison. The
/// time of the last write is not part of the content: it is a backend's own clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Meta {
    /// The object's size in bytes.
    pub size: u64,
    /// The object's content tag, compared as it is.
    pub tag: String,
    /// When the object was last written, in milliseconds since the Unix epoch, as the
    /// backend's clock counts them (a backend with a coarser clock, whole seconds as S3 has,
    /// gives equal times to objects written close together).
    pub modified: u64,
}

/// What a backend's answer to a create-only put says of the object it made, so that a caller that
/// keeps a copy need not ask with a HEAD.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutAnswer {
    /// The object's content tag, as [`Meta::tag`] has it for the same object.
    pub tag: String,
    /// When the object was last written, in milliseconds, if the answer says (S3's answer to a PUT
    /// does not); `None` leaves the time to the caller.
    pub modified: Option<u64>,
}

impl Meta {
    /// Whether both describe the same bytes: the same size and the same content tag.
    #[must_use]
    pub fn same_content(&self, other: &Meta) -> bool {
        (self.size, &self.tag) == (other.size, &other.tag)
    }
}

/// One object in a listing.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Listed {
    /// The object's key.
    pub key: String,
    /// The object's size in bytes.
    pub size: u64,
    /// When the object was last written, as in [`Meta::modified`].
    pub modified: u64,
    /// The object's content tag, as [`Backend::head`] gives it in [`Meta::tag`] (the tier that
    /// answers the HEAD is the one whose listing entry is kept; see each backend).
    pub tag: String,
}

/// A multipart upload that was started and is neither completed nor aborted, as
/// [`Backend::list_uploads`] lists it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OpenUpload {
    /// The key of the object the upload is for.
    pub key: String,
    /// The id of the upload, which names it in the calls of [`Backend::upload_part`],
    /// [`Backend::complete_upload`] and [`Backend::abort_upload`].
    pub id: String,
    /// When the upload was started, in milliseconds since the Unix epoch, as the backend's
    /// clock counts them (as for [`Meta::modified`]).
    pub initiated: u64,
}

/// A part stored in an open upload, as [`Backend::list_parts`] lists it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct UploadedPart {
    /// The number of the part, from 1.
    pub number: u32,
    /// The content tag of the part, as [`Backend::upload_part`] gave it (the tag a completion
    /// names).
    pub tag: String,
    /// The size of the part in bytes.
    pub size: u64,
    /// When the part was stored, in milliseconds since the Unix epoch, as the backend's clock
    /// counts them (as for [`Meta::modified`]).
    pub modified: u64,
}

/// The parts of `parts` that a page of [`Backend::list_parts`] holds: those numbered above
/// `after`, in ascending order of number, at most `max_parts`.
pub(crate) fn part_page(
    parts: impl IntoIterator<Item = UploadedPart>,
    after: Option<u32>,
    max_parts: usize,
) -> Vec<UploadedPart> {
    let mut page: Vec<UploadedPart> = parts
        .into_iter()
        .filter(|part| after.is_none_or(|number| part.number > number))
        .collect();
    page.sort_by_key(|part| part.number);
    page.truncate(max_parts);
    page
}

/// Where a page of [`Backend::list_uploads`] starts: after this key and, if there is one, this
/// upload id, which is the last upload of the page before.
pub type UploadsAfter<'a> = (&'a str, Option<&'a str>);

/// The uploads of `uploads` that a page of [`Backend::list_uploads`] holds, for a backend that
/// orders the uploads of a key by id: those of a key under `prefix` that come after `after`, in
/// ascending order of key and id, at most `max_uploads`.
pub(crate) fn upload_page(
    uploads: impl IntoIterator<Item = OpenUpload>,
    prefix: &str,
    after: Option<UploadsAfter<'_>>,
    max_uploads: usize,
) -> Vec<OpenUpload> {
    let is_after = |upload: &OpenUpload| match after {
        None => true,
        Some((key, None)) => upload.key.as_str() > key,
        Some((key, Some(id))) => (upload.key.as_str(), upload.id.as_str()) > (key, id),
    };
    let mut page: Vec<OpenUpload> = uploads
        .into_iter()
        .filter(|upload| upload.key.starts_with(prefix) && is_after(upload))
        .collect();
    page.sort_by(|a, b| (&a.key, &a.id).cmp(&(&b.key, &b.id)));
    page.truncate(max_uploads);
    page
}

/// Whether `key` sorts after the key a listing is to start after, if there is one.
fn is_after(key: &str, start_after: Option<&str>) -> bool {
    start_after.is_none_or(|after| key > after)
}

/// An S3-compatible object store with the calls this crate needs, and no more.
///
/// Objects are immutable: a key is written once. The exception is the few small objects that
/// the proxy and the reconciler keep for themselves (the lease, the checkpoints, the usage),
/// which [`Backend::put_if_match`] rewrites in place. The trait is used as `dyn Backend`, so
/// a proxy holds its two tiers without a type parameter.
///
/// An implementation over a network bounds each call with a timeout of its own: the proxy
/// and the reconciler wait for the calls they make, and a call that never returns stops the
/// copies to the cold tier and the deletes behind it. The exception is [`Backend::upload_part`],
/// whose time is set by the client that sends the part (see there).
#[async_trait]
pub trait Backend: Send + Sync {
    /// Whether a streamed body (of [`Backend::put_stream`] or [`Backend::upload_part_with_len`])
    /// must come with its length for the backend to avoid holding it whole. A store that needs
    /// the length of a request before it is sent (S3 does) says `true`: given no length it
    /// collects the body first, which is memory the client chooses. The S3 front refuses a
    /// request without a `Content-Length` (411) before reading its body when this is `true`.
    /// The default is `false`.
    fn needs_length(&self) -> bool {
        false
    }

    /// The longest object, in bytes, that the backend writes with one request; it writes a longer
    /// one in several (an S3 multipart upload, whose completion is what a create-only write
    /// is refused at, after every part is sent). `None` for a backend with no such limit, which is
    /// the default.
    fn single_put_max(&self) -> Option<u64> {
        None
    }

    /// Writes an object, only if the key does not exist (`If-None-Match: *`).
    ///
    /// # Errors
    /// [`Error::AlreadyExists`] if the key exists; the stored object is left as it was.
    /// [`Error::Full`] if the backend has no room for the object.
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error>;

    /// As [`Backend::put`], and gives the content tag of the object this write created, the tag
    /// a later [`Backend::delete_if_match`] must name to delete exactly this object.
    ///
    /// A backend that can must bind the tag to the write itself (the memory backend and the
    /// directory backend know it from the bytes they publish, S3 sends it as the `ETag` of the
    /// answer to the PUT), so that an object another writer puts under the key afterwards is
    /// never taken for this one. The provided implementation reads the tag with a HEAD after the
    /// write, which is not bound to it (the object may have been replaced between the two); it
    /// serves the fakes of tests, and every backend a proxy uses as its hot tier overrides it.
    ///
    /// # Errors
    /// As [`Backend::put`], and the error of the HEAD for the provided implementation. A write
    /// that succeeded and whose tag was not obtained is an error too: the caller then holds no
    /// tag, and so must delete nothing.
    async fn put_tagged(&self, key: &str, body: Bytes) -> Result<String, Error> {
        self.put(key, body).await?;
        Ok(self.head(key).await?.tag)
    }

    /// As [`Backend::put`], for a body that arrives as chunks: `len` is the number of bytes
    /// `body` is to give, or `None` if the caller does not know.
    ///
    /// The object becomes visible under `key` only after `body` has ended, and a body that ends
    /// with an error leaves nothing: no object under `key`, and nothing left behind that a
    /// listing or a get could find. The call gives that error. That is what lets a caller
    /// verify the bytes as they pass (a checksum is known only at the end) and end the stream
    /// with an error if they are wrong. The implementations of the directory and the S3
    /// backend do not hold the body in memory: the directory backend writes the chunks to a
    /// temporary file and links it to `key` after the last chunk; the S3 backend sends them to
    /// the store as they arrive, as one request or as a multipart upload that is completed (with
    /// `If-None-Match: *`) only after the last chunk, and aborted on any failure. A store that
    /// must know the length of a request before it is sent (S3 does) needs `len`; given
    /// `None`, the S3 backend holds the whole body first.
    ///
    /// The provided implementation collects the chunks and calls [`Backend::put`], which holds
    /// the body whole; it serves a backend that holds its objects in memory anyway, and the
    /// wrappers of other backends override the call to pass the stream on.
    ///
    /// # Errors
    /// As [`Backend::put`], and the error of `body`, if it ends with one.
    async fn put_stream(
        &self,
        key: &str,
        _len: Option<u64>,
        mut body: PartBody<'_>,
    ) -> Result<(), Error> {
        let mut whole = BytesMut::new();
        while let Some(chunk) = body.next().await {
            whole.extend_from_slice(&chunk?);
        }
        self.put(key, whole.freeze()).await
    }

    /// [`Backend::put_stream`], or [`Backend::put_stream_hot_only`] if `hot_only`, giving what the
    /// answer to the write says of the object when that is the tag [`Backend::head`] gives for it
    /// (and `None` when the backend cannot say without another call). For a caller that keeps a
    /// copy of what it writes and would otherwise ask with a HEAD.
    ///
    /// The provided implementation writes and answers `None`: the caller asks. A backend overrides
    /// it only if the tag of the answer is the tag its HEAD gives, which it must be for as long as
    /// the object stands.
    ///
    /// # Errors
    /// As [`Backend::put_stream`].
    async fn put_stream_answered(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        hot_only: bool,
    ) -> Result<Option<PutAnswer>, Error> {
        if hot_only {
            self.put_stream_hot_only(key, len, body).await?;
        } else {
            self.put_stream(key, len, body).await?;
        }
        Ok(None)
    }

    /// [`Backend::put_stream`] for a writer that cannot be trusted to race: a backend with more
    /// than one tier writes the object to its first tier only, and answers [`Error::Full`] and
    /// stores nothing when that tier has no room (whatever part of the body it took), so that one
    /// create-only write into one tier decides which of two concurrent puts of a key wins. The
    /// default is [`Backend::put_stream`], for a backend with one place to put an object.
    ///
    /// # Errors
    /// As [`Backend::put_stream`].
    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.put_stream(key, len, body).await
    }

    /// [`Backend::put_tagged`] for a body that arrives as chunks (`len` is the number of bytes it
    /// is to give, or `None` if the caller does not know): create-only, and gives the content
    /// tag of the object this write made, with no read after it.
    ///
    /// As for [`Backend::put_stream`], the object becomes visible only after `body` has ended
    /// without an error, and a body that ends with an error, or a call that is dropped half way,
    /// leaves nothing. The directory and S3 backends do not hold the body whole (S3 streams a
    /// body of at most `server.s3_single_put_max` as one PUT, and sends a longer one as a multipart upload).
    ///
    /// The provided implementation collects the chunks and calls [`Backend::put_tagged`]; it
    /// serves a backend that holds its objects in memory anyway, and the wrappers of other
    /// backends override the call to pass the stream on.
    ///
    /// # Errors
    /// As [`Backend::put_tagged`], and the error of `body`, if it ends with one.
    async fn put_stream_tagged(
        &self,
        key: &str,
        _len: Option<u64>,
        mut body: PartBody<'_>,
    ) -> Result<String, Error> {
        let mut whole = BytesMut::new();
        while let Some(chunk) = body.next().await {
            whole.extend_from_slice(&chunk?);
        }
        self.put_tagged(key, whole.freeze()).await
    }

    /// Replaces the object, only if its content tag is `etag` (`If-Match`), and gives the
    /// content tag of the new object.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key. [`Error::PreconditionFailed`] if the
    /// object has another content tag; the stored object is left as it was.
    async fn put_if_match(&self, key: &str, body: Bytes, etag: &str) -> Result<String, Error>;

    /// As [`Backend::put_if_match`], for a body that arrives as chunks (`len` is the number of
    /// bytes it is to give, or `None` if the caller does not know), and gives the content tag of
    /// the new object.
    ///
    /// The object under `key` is replaced only after `body` has ended without an error and only if
    /// it has the content tag `etag` then: a body that ends with an error, or a tag that does not
    /// match, leaves the object as it was and leaves nothing behind. The directory backend writes
    /// the chunks to a temporary file and replaces the object under the lock and the tag check of
    /// [`Backend::put_if_match`]; the S3 backend sends one PUT with `If-Match` for a body of at
    /// most `server.s3_single_put_max` and a multipart upload whose completion carries `If-Match` for a longer one,
    /// aborted on any failure. A store that must know the length of a request before it is sent
    /// (S3 does) holds the body first when given `None`. The in-memory backend collects the body.
    ///
    /// The provided implementation is [`Error::Unsupported`]: a backend that wraps another must
    /// forward the call, and one that keeps a copy of what it reads (a cache) must forget the key.
    ///
    /// # Errors
    /// As [`Backend::put_if_match`], and the error of `body`, if it ends with one.
    /// [`Error::Unsupported`] unless the backend overrides this call.
    async fn put_stream_if_match(
        &self,
        _key: &str,
        _len: Option<u64>,
        _body: PartBody<'_>,
        _etag: &str,
    ) -> Result<String, Error> {
        Err(Error::Unsupported(
            "put_stream_if_match: this backend does not do it".to_owned(),
        ))
    }

    /// Reads the object, or the bytes of `range` of it (see [`ByteRange`]).
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key.
    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error>;

    /// As [`Backend::get`], with the content tag of the object ([`Meta::tag`]), both from one read
    /// of one object: the tag is that of the object the bytes were read from, never that of an
    /// object that was in the key before or after (a key that holds A, then B, then A again gives
    /// the tag of the A or of the B whose bytes it gives, not of the other). A backend that cannot
    /// give that (one that reads the bytes and asks for the tag in another call, where the key may
    /// be replaced, and replaced back, in between) must not make the call up from a HEAD and a
    /// read. The tag is the one [`Backend::head`] gives for that object, so a caller may use it as
    /// `etag` of the conditional calls.
    ///
    /// The provided implementation is [`Error::Unsupported`], so that a backend or a wrapper that
    /// forgets to forward the call fails loudly: a wrapper over another backend forwards it, and
    /// one that keeps a copy of what it reads (a cache) must not answer from the copy. The
    /// in-memory and the directory backend read both from one object (the directory backend
    /// from the one open file) and the S3 backend from the one answer of its GET, whole or ranged.
    ///
    /// # Errors
    /// As [`Backend::get`]. [`Error::Unsupported`] unless the backend overrides this call.
    async fn get_tagged(
        &self,
        _key: &str,
        _range: Option<ByteRange>,
    ) -> Result<(Bytes, String), Error> {
        Err(Error::Unsupported(
            "get_tagged: this backend does not do it".to_owned(),
        ))
    }

    /// As [`Backend::get`], for a read that is given as the chunks of the bytes: the bytes of
    /// `range` of the object, in order, ending with an error if they could not all be read. The
    /// body is complete only if it ends without an error; a caller that has the length it asked
    /// for (from a HEAD) must also count the bytes.
    ///
    /// The provided implementation reads the bytes whole with [`Backend::get`] and gives them as
    /// one chunk, which serves a backend that holds its objects in memory anyway. A backend whose
    /// objects may be larger than memory overrides it and gives the bytes as they are read, so
    /// that a reader holds one chunk and not the object: the directory backend reads its file in
    /// chunks of bounded size, and the S3 backend gives the body of its GET as it arrives.
    ///
    /// # Errors
    /// As [`Backend::get`].
    async fn get_stream(&self, key: &str, range: Option<ByteRange>) -> Result<ReadBody, Error> {
        let whole = self.get(key, range).await?;
        Ok(futures::stream::once(std::future::ready(Ok(whole))).boxed())
    }

    /// Whether `a` and `b`, two HEADs of one key, describe one version of the object. A reader
    /// that holds to a version while it reads (a cache filling from this backend) asks it of the
    /// backend the HEADs came from, since only the backend knows what its tags stand for.
    ///
    /// The provided implementation is [`Meta::same_content`]: the same size and tag. A backend
    /// whose tag of an object moves though the object does not (the proxy of two tiers, whose tag
    /// is the tag of the tier that answers) overrides it; a wrapper over another backend forwards
    /// the call.
    fn same_version(&self, a: &Meta, b: &Meta) -> bool {
        a.same_content(b)
    }

    /// Opens a read in one call: the object's [`Meta`] and the bytes of `span` of it (all of it
    /// without one) as a body that is read chunk by chunk, so that a reader that needs both (the
    /// S3 front answers with the headers of the object before the first byte) makes one request
    /// and not a HEAD and a GET, and holds one chunk and not the object.
    ///
    /// The [`Opened::meta`] and the body are those of one object: the in-memory backend reads
    /// both from one object, the directory backend from the one open file and the S3 backend from
    /// the one answer of its GET (whole, or ranged by the span). A span that starts at or past
    /// the end of the object gives [`Window::Unsatisfiable`] and an empty body, with the meta of
    /// the object.
    ///
    /// The provided implementation is a HEAD, then [`Backend::get_stream`] of the range the size
    /// resolves the span to: two calls, which may find two objects if the key is replaced between
    /// them. It serves the fakes of tests and the wrappers that pass a read on to the call of the
    /// backend they wrap.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key. Errors of the body come as its last chunk.
    async fn open_read(&self, key: &str, span: Option<Span>) -> Result<Opened, Error> {
        open_by_head(self, key, span).await
    }

    /// The object's size, content tag and time of last write.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key.
    async fn head(&self, key: &str) -> Result<Meta, Error>;

    /// Deletes the object. Deleting a key that does not exist succeeds, so a retried delete
    /// is safe.
    async fn delete(&self, key: &str) -> Result<(), Error>;

    /// Deletes the object, only if its content tag is `etag` (the tag [`Backend::head`] gives,
    /// without quotes), on a best-effort basis.
    ///
    /// The tag names the content, not a write: an object deleted and put again with the same
    /// bytes has the same tag, and the call takes it. A caller that must not take an object it
    /// did not read has to know the object was not replaced by equal bytes.
    ///
    /// The in-memory and the directory backend check the tag and remove the object as one step
    /// of the backend (the directory backend among the calls of one backend only, as
    /// [`Backend::put_if_match`]). The S3 backend sends `If-Match` when its tier's
    /// `conditional_delete` setting is on, and otherwise reads the tag first and deletes after,
    /// which leaves a window for a replacement between the two. It is not repeated after a
    /// failure that leaves the outcome unknown; the repeat reads [`Error::NotFound`] if it had
    /// taken effect.
    ///
    /// The provided implementation is [`Error::Unsupported`]: a backend that keeps a copy of
    /// what it reads (the caches) must forget the key as well, so it overrides this call or
    /// refuses it.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no object has the key (the S3 backend may give
    /// [`Error::PreconditionFailed`] instead, on a store that judges the tag first, so a caller
    /// takes both for "not the object it read"). [`Error::PreconditionFailed`] if the object
    /// has another content tag; it is left as it was. [`Error::Unsupported`] unless the backend
    /// overrides this call.
    async fn delete_if_match(&self, _key: &str, _etag: &str) -> Result<(), Error> {
        Err(Error::Unsupported(
            "delete_if_match: this backend does not do it".to_owned(),
        ))
    }

    /// Every object whose key starts with `prefix`, in ascending key order. The listing need
    /// not be consistent with recent writes.
    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error>;

    /// A page of the objects [`Backend::list`] gives: at most `max_keys` of those whose key
    /// sorts after `start_after` (all of them for `None`), in ascending key order.
    ///
    /// The last key of a page is the continuation token of the next one, and a page of fewer
    /// than `max_keys` objects is the last. The provided implementation cuts the whole
    /// listing; a backend over a network lists a page at a time instead, and one whose store
    /// caps a page (S3 gives at most 1000 keys) reads further pages until it has `max_keys`
    /// objects or the listing ends, because a caller takes a short page for the last.
    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let all = self.list(prefix).await?;
        let page: Vec<Listed> = all
            .into_iter()
            .filter(|object| is_after(&object.key, start_after))
            .take(max_keys)
            .collect();
        Ok(page)
    }

    /// Starts a multipart upload of an object `key`, and gives the id of the upload.
    ///
    /// The four calls of a multipart upload (this one, [`Backend::upload_part`],
    /// [`Backend::complete_upload`] and [`Backend::abort_upload`]) are provided: a backend
    /// that does not override them answers each with [`Error::Unsupported`] (so does
    /// [`Backend::upload_part_with_len`], which only adds the length of the part). A backend
    /// that does keeps the parts of an upload apart from the objects (they are not listed and not
    /// read), and the id names an upload of one key only. A backend may refuse a key it cannot
    /// store at once (the directory backend does, for a key that is empty or too long to be a
    /// file name); a key it would take is not looked at again until the upload is completed.
    ///
    /// # Errors
    /// [`Error::Unsupported`] unless the backend does multipart uploads. [`Error::Reserved`]
    /// for a key the backend keeps for itself. [`Error::Other`] for a key it cannot store.
    async fn create_upload(&self, _key: &str) -> Result<String, Error> {
        Err(not_supported("create_upload"))
    }

    /// Stores the part `number` of the upload `id` of `key` from `body`, in place of the part
    /// of that number if there is one, and gives the part's content tag, which a completion
    /// names.
    ///
    /// The body is read a chunk at a time and written as it arrives; an implementation does not
    /// hold the whole part in memory (the in-memory backend of the tests does). A body that
    /// ends with an error stores nothing, and the call gives that error.
    ///
    /// The time of the call is set by the client that sends the part, so a bound on it counts
    /// the time with no chunk taken, not the time of the whole call, which would cut a client
    /// that is slow and steady. The server already bounds the wait for each chunk of a body.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no upload `id` of `key` is open. [`Error::Unsupported`] unless
    /// the backend does multipart uploads. The error of `body`, if it ends with one.
    async fn upload_part(
        &self,
        _key: &str,
        _id: &str,
        _number: u32,
        _body: PartBody<'_>,
    ) -> Result<String, Error> {
        Err(not_supported("upload_part"))
    }

    /// As [`Backend::upload_part`], for a part whose length may be known before it is read:
    /// `len` is the number of bytes `body` is to give, or `None` if the caller does not know.
    ///
    /// A store that needs the length of a request body before it is sent (S3 does) is written
    /// to by a backend that overrides this call, and sends the part as it arrives; it answers
    /// [`Backend::upload_part`] as this call with `None`, for which it must hold the part,
    /// since a stream does not say how long it is. A backend that uses `len` stores nothing
    /// of a body that gives more or fewer bytes, and the call is an error. The provided method
    /// ignores `len`, as the in-memory and the directory backend have no use for it.
    ///
    /// # Errors
    /// As [`Backend::upload_part`].
    async fn upload_part_with_len(
        &self,
        key: &str,
        id: &str,
        number: u32,
        _len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        self.upload_part(key, id, number, body).await
    }

    /// Completes the upload `id` of `key`: creates the object `key` from the parts named, in
    /// the order given, which is ascending by part number, each with the content tag
    /// [`Backend::upload_part`] gave it. Gives the content tag of the new object, which
    /// [`Backend::head`] gives it too. The in-memory and the directory backend give the tag a
    /// [`Backend::put`] of the same bytes gives; a store that tags an object made of parts by
    /// its parts (S3 does: a hash of the tags of the parts and their number) gives that tag,
    /// which is not the tag of a put of the same bytes, so the tags of two tiers tell that two
    /// objects are the same only if both were made the same way. The parts and the upload are
    /// gone once the object is created.
    ///
    /// Like [`Backend::put`] the completion only creates: where the key exists the upload is
    /// left open.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no upload `id` of `key` is open. [`Error::InvalidPart`] if a part
    /// named was not uploaded or has another content tag. [`Error::AlreadyExists`] if the key
    /// exists. [`Error::Unsupported`] unless the backend does multipart uploads.
    async fn complete_upload(
        &self,
        _key: &str,
        _id: &str,
        _parts: &[(u32, String)],
    ) -> Result<String, Error> {
        Err(not_supported("complete_upload"))
    }

    /// Drops the upload `id` of `key` and the parts it holds.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no upload `id` of `key` is open. [`Error::Unsupported`] unless
    /// the backend does multipart uploads.
    async fn abort_upload(&self, _key: &str, _id: &str) -> Result<(), Error> {
        Err(not_supported("abort_upload"))
    }

    /// A page of the uploads that are open (started, and neither completed nor aborted) for a
    /// key that starts with `prefix`: at most `max_uploads` of them, in ascending order of
    /// key, and within a key in the order the backend keeps (by id for the in-memory and the
    /// directory backend, by the time of the start for a store over S3).
    ///
    /// `after` is the last upload of the page before: the page starts after that upload,
    /// or, where `after` names a key and no id, after every upload of that key. A page of
    /// fewer than `max_uploads` uploads is the last. The order within a key is the backend's,
    /// so the marker of a page is read by the backend that gave it. An upload that is started
    /// or ended while the listing is read may or may not be in it. Like the other multipart
    /// calls, this one is [`Error::Unsupported`] unless the backend overrides it.
    ///
    /// # Errors
    /// [`Error::Unsupported`] unless the backend does multipart uploads.
    async fn list_uploads(
        &self,
        _prefix: &str,
        _after: Option<UploadsAfter<'_>>,
        _max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        Err(not_supported("list_uploads"))
    }

    /// A page of the parts stored in the open upload `id` of `key`: those numbered above
    /// `after` (all of them when it is `None`), in ascending order of number, at most
    /// `max_parts`. A part sent again with a number replaces the one before, so each number is
    /// there once, with the tag, the size and the time of the part that stays.
    ///
    /// A page of fewer than `max_parts` parts is the last. A part stored or an upload ended
    /// while the listing is read may or may not be in it. Like the other multipart calls, this
    /// one is [`Error::Unsupported`] unless the backend overrides it.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no upload `id` of `key` is open. [`Error::Unsupported`] unless the
    /// backend does multipart uploads.
    async fn list_parts(
        &self,
        _key: &str,
        _id: &str,
        _after: Option<u32>,
        _max_parts: usize,
    ) -> Result<Vec<UploadedPart>, Error> {
        Err(not_supported("list_parts"))
    }
}
