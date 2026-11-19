//! The footer scan of every pack (design section 16 C-6): the blobs each pack holds, the input
//! of `L_live`.
//!
//! A pack is a *segment* of the CAS service, all integers little-endian:
//!
//! ```text
//! records | fan-out | index | trailer
//! records   the blobs' bytes, back to back from offset 0
//! fan-out   256 x u32: fanout[b] is the number of index entries whose hash[0] <= b
//! index     N x 52 bytes, strictly ascending by (hash, size):
//!           hash[32] | size u64 | offset u64 | crc32c u32
//! trailer   32 bytes: index_offset u64 | entry_count u32 | digest_function u8 (1 = sha256)
//!           | reserved [3] = 0 | footer_crc32c u32 | version u32 (1) | magic b"KBFSEG\r\n"
//! ```
//!
//! The footer is the fan-out, the index and the trailer: `1024 + 32 + 52 N` bytes, and
//! `index_offset + footer_len` is the object's size. `footer_crc32c` is the CRC-32C of the
//! footer's first `footer_len - 16` bytes (the fan-out, the index and the trailer up to the
//! checksum itself). The scan reads the last 64 KiB of each pack with one ranged GET, and the
//! whole footer with a second when it is longer; a tree list under `cas/l/` is read for its first
//! byte, which is `{` when it is JSON. Other objects are not read.
//!
//! The scan writes one TSV file, in listing order ([`write_entry`], read back by [`read`]):
//!
//! ```text
//! P TAB bucket TAB key TAB status TAB n      then n lines  B TAB <64 hex digits> TAB <size>
//! T TAB bucket TAB key TAB json | not-json | get-error
//! ```
//!
//! `status` is a [`Status`] name; `n` is 0 unless it is `ok`. A `get-error` status has a sixth
//! field, the error's text with tabs and newlines replaced by spaces.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt as _;
use objstore_s3::s3::{Credentials, S3Store};
use objstore_s3::{ByteRange, ObjectKey, ObjectStore as _};

use crate::Error;
use crate::kinds::{Kind, classify};
use crate::listing::{self, Object};

/// The bytes of the fan-out table.
pub const FANOUT_LEN: usize = 1024;
/// The bytes of one index entry.
pub const ENTRY_LEN: usize = 52;
/// The bytes of the trailer.
pub const TRAILER_LEN: usize = 32;
/// The bytes the first ranged GET of a pack asks for (fewer when the pack is smaller).
pub const TAIL_FETCH: u64 = 65536;
/// The last eight bytes of a segment.
pub const MAGIC: [u8; 8] = *b"KBFSEG\r\n";
/// The only segment version.
pub const VERSION: u32 = 1;
/// The only digest function: SHA-256.
pub const DIGEST_SHA256: u8 = 1;

/// The flags of `storebench footers`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Silo's endpoint and credentials.
    #[command(flatten)]
    pub endpoint: crate::s3::Endpoint,
    /// The listing (`storebench list`) whose packs and tree lists are read.
    #[arg(long)]
    pub listing: PathBuf,
    /// Where the footer file is written.
    #[arg(long)]
    pub out: PathBuf,
    /// The most requests in flight at once.
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u32).range(1..))]
    pub conc: u32,
    /// Check one tree list in this many, in listing order (the first, then every n-th): a
    /// sample is enough to confirm what the key layout says they are.
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
    pub tree_every: u64,
    /// This scan's share of the listing, `i/n`: the objects whose line number (from 0) is `i`
    /// modulo `n`, so `n` scans on different nodes read each object once.
    #[arg(long, default_value = "0/1", value_parser = crate::drivewalk::parse_share)]
    pub share: crate::drivewalk::Share,
    /// Packs smaller than this many bytes are not read (and not written to the footer file).
    #[arg(long, default_value_t = 0)]
    pub min_pack_bytes: u64,
}

/// One blob of a pack's index: its digest and its size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blob {
    /// The SHA-256 of the blob.
    pub digest: [u8; 32],
    /// The blob's size in bytes.
    pub size: u64,
}

/// What the scan found of one pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The footer is whole and valid.
    Ok,
    /// The last eight bytes are not the segment magic.
    NoMagic,
    /// The version is not 1.
    BadVersion,
    /// The digest function is not 1 (SHA-256).
    BadDigestFunction,
    /// `index_offset + footer_len` is not the object's size, or the bytes read are shorter than
    /// the footer.
    BadLength,
    /// The footer's checksum does not match its bytes.
    BadCrc,
    /// The index is not strictly ascending by (hash, size).
    BadOrder,
    /// An entry's record reaches past the record area.
    BadRange,
    /// The object is shorter than a trailer.
    TooSmall,
    /// A request failed; the text is the error's.
    GetError(String),
}

impl Status {
    /// The status's name in the footer file.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::NoMagic => "no-magic",
            Status::BadVersion => "bad-version",
            Status::BadDigestFunction => "bad-digest-function",
            Status::BadLength => "bad-length",
            Status::BadCrc => "bad-crc",
            Status::BadOrder => "bad-order",
            Status::BadRange => "bad-range",
            Status::TooSmall => "too-small",
            Status::GetError(_) => "get-error",
        }
    }

    /// Every status that is just a name (all but [`Status::GetError`]).
    #[must_use]
    pub fn plain() -> [Status; 9] {
        [
            Status::Ok,
            Status::NoMagic,
            Status::BadVersion,
            Status::BadDigestFunction,
            Status::BadLength,
            Status::BadCrc,
            Status::BadOrder,
            Status::BadRange,
            Status::TooSmall,
        ]
    }

    /// The status called `name`, for every name but `get-error` (which carries a text).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Status> {
        Status::plain().into_iter().find(|s| s.name() == name)
    }
}

/// What a pack's trailer says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Trailer {
    /// Where the fan-out starts: the end of the record area.
    pub index_offset: u64,
    /// The number of index entries.
    pub count: u32,
    /// The footer's length in bytes: `1024 + 32 + 52 * count`.
    pub footer_len: u64,
}

/// The bytes `at..at + N` of `b`.
fn le<const N: usize>(b: &[u8], at: usize) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(&b[at..at + N]);
    out
}

/// The trailer at the end of `tail` (the last bytes of an object of `object_size` bytes), checked
/// for magic, version, digest function and length.
///
/// # Errors
/// [`Status::TooSmall`] when the object or `tail` is shorter than a trailer;
/// [`Status::NoMagic`], [`Status::BadVersion`], [`Status::BadDigestFunction`] or
/// [`Status::BadLength`] (`index_offset + footer_len` is not `object_size`).
pub fn trailer(tail: &[u8], object_size: u64) -> Result<Trailer, Status> {
    if object_size < TRAILER_LEN as u64 || tail.len() < TRAILER_LEN {
        return Err(Status::TooSmall);
    }
    let t = &tail[tail.len() - TRAILER_LEN..];
    if t[24..32] != MAGIC {
        return Err(Status::NoMagic);
    }
    if u32::from_le_bytes(le(t, 20)) != VERSION {
        return Err(Status::BadVersion);
    }
    if t[12] != DIGEST_SHA256 {
        return Err(Status::BadDigestFunction);
    }
    let index_offset = u64::from_le_bytes(le(t, 0));
    let count = u32::from_le_bytes(le(t, 8));
    let footer_len = (FANOUT_LEN + TRAILER_LEN) as u64 + ENTRY_LEN as u64 * u64::from(count);
    if index_offset.checked_add(footer_len) != Some(object_size) {
        return Err(Status::BadLength);
    }
    Ok(Trailer {
        index_offset,
        count,
        footer_len,
    })
}

/// The blobs of the footer at the end of `tail`, the last bytes of an object of `object_size`
/// bytes that reach back at least to the footer's start.
///
/// # Errors
/// Every [`Status`] but [`Status::Ok`] and [`Status::GetError`]: a [`trailer`] failure;
/// [`Status::BadLength`] when `tail` is shorter than the footer; [`Status::BadCrc`];
/// [`Status::BadOrder`] when the index is not strictly ascending by (hash, size);
/// [`Status::BadRange`] when an entry's record does not end inside the record area.
pub fn parse_footer(tail: &[u8], object_size: u64) -> Result<Vec<Blob>, Status> {
    let t = trailer(tail, object_size)?;
    let want = usize::try_from(t.footer_len).unwrap_or(usize::MAX);
    if want > tail.len() {
        return Err(Status::BadLength);
    }
    let footer = &tail[tail.len() - want..];
    if crc32c::crc32c(&footer[..want - 16]) != u32::from_le_bytes(le(footer, want - 16)) {
        return Err(Status::BadCrc);
    }
    let index = &footer[FANOUT_LEN..want - TRAILER_LEN];
    let mut blobs = Vec::with_capacity(index.len() / ENTRY_LEN);
    let mut previous: Option<([u8; 32], u64)> = None;
    for entry in index.chunks_exact(ENTRY_LEN) {
        let digest: [u8; 32] = le(entry, 0);
        let size = u64::from_le_bytes(le(entry, 32));
        let offset = u64::from_le_bytes(le(entry, 40));
        if previous.is_some_and(|p| p >= (digest, size)) {
            return Err(Status::BadOrder);
        }
        if offset
            .checked_add(size)
            .is_none_or(|end| end > t.index_offset)
        {
            return Err(Status::BadRange);
        }
        previous = Some((digest, size));
        blobs.push(Blob { digest, size });
    }
    Ok(blobs)
}

/// What the scan found of one pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackFooter {
    /// The pack's bucket.
    pub bucket: String,
    /// The pack's key.
    pub key: String,
    /// What the scan found.
    pub status: Status,
    /// The blobs of its index; empty unless the status is [`Status::Ok`].
    pub blobs: Vec<Blob>,
}

/// What a tree list's first byte says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeOutcome {
    /// It is `{`: a JSON object.
    Json,
    /// It is something else.
    NotJson,
    /// The request failed.
    GetError,
}

/// The check of one tree list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeCheck {
    /// The object's bucket.
    pub bucket: String,
    /// The object's key.
    pub key: String,
    /// What its first byte is.
    pub outcome: TreeOutcome,
}

/// One object the scan read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checked {
    /// A pack.
    Pack(PackFooter),
    /// A tree list.
    Tree(TreeCheck),
}

/// Everything a footer file holds, in file order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    /// The packs.
    pub packs: Vec<PackFooter>,
    /// The tree lists.
    pub trees: Vec<TreeCheck>,
}

/// The range `[offset, offset + len)`, or a text saying it is empty.
///
/// # Errors
/// The text of the refusal when `len` is 0 or the range's end overflows.
pub fn range(offset: u64, len: u64) -> Result<ByteRange, String> {
    ByteRange::new(offset, len).ok_or_else(|| format!("range {offset}+{len} is empty or overflows"))
}

/// The bytes `[offset, offset + len)` of `key`, or the failure's text.
async fn get(store: &S3Store, key: &ObjectKey, offset: u64, len: u64) -> Result<Vec<u8>, Status> {
    let range = range(offset, len).map_err(Status::GetError)?;
    store
        .get_range(key, range)
        .await
        .map(|b| b.to_vec())
        .map_err(|e| Status::GetError(e.to_string()))
}

/// The blobs of the pack `object`: one ranged GET of its last [`TAIL_FETCH`] bytes, and a second
/// of exactly the footer when it is longer.
async fn fetch_footer(store: &S3Store, object: &Object) -> Result<Vec<Blob>, Status> {
    if object.size < TRAILER_LEN as u64 {
        return Err(Status::TooSmall);
    }
    let key = ObjectKey::new(&object.key).map_err(|e| Status::GetError(e.to_string()))?;
    let mut want = object.size.min(TAIL_FETCH);
    loop {
        let tail = get(store, &key, object.size - want, want).await?;
        let footer_len = trailer(&tail, object.size)?.footer_len;
        if footer_len <= want {
            return parse_footer(&tail, object.size);
        }
        // `trailer` checked that `footer_len` is at most the object's size.
        want = footer_len;
    }
}

/// Reads the footer of the pack `object` from `store` (the client of its bucket).
pub async fn scan_pack(store: &S3Store, object: &Object) -> PackFooter {
    let (status, blobs) = match fetch_footer(store, object).await {
        Ok(blobs) => (Status::Ok, blobs),
        Err(status) => (status, Vec::new()),
    };
    PackFooter {
        bucket: object.bucket.clone(),
        key: object.key.clone(),
        status,
        blobs,
    }
}

/// Reads the first byte of the tree list `object` from `store`.
pub async fn scan_tree(store: &S3Store, object: &Object) -> TreeCheck {
    let first = async {
        let key = ObjectKey::new(&object.key).map_err(|e| Status::GetError(e.to_string()))?;
        get(store, &key, 0, 1).await
    };
    let outcome = match first.await {
        Ok(bytes) if bytes.first() == Some(&b'{') => TreeOutcome::Json,
        Ok(_) => TreeOutcome::NotJson,
        Err(_) => TreeOutcome::GetError,
    };
    TreeCheck {
        bucket: object.bucket.clone(),
        key: object.key.clone(),
        outcome,
    }
}

/// `text` with tabs and line breaks replaced by spaces, so it is one field of one line.
#[must_use]
pub fn one_field(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

/// Writes `entry` to `out` in the footer file's format.
///
/// # Errors
/// The error of `out`.
pub fn write_entry(out: &mut dyn Write, entry: &Checked) -> std::io::Result<()> {
    match entry {
        Checked::Pack(p) => {
            write!(
                out,
                "P\t{}\t{}\t{}\t{}",
                p.bucket,
                p.key,
                p.status.name(),
                p.blobs.len()
            )?;
            if let Status::GetError(text) = &p.status {
                write!(out, "\t{}", one_field(text))?;
            }
            writeln!(out)?;
            for b in &p.blobs {
                writeln!(out, "B\t{}\t{}", hex::encode(b.digest), b.size)?;
            }
            Ok(())
        }
        Checked::Tree(t) => {
            let outcome = match t.outcome {
                TreeOutcome::Json => "json",
                TreeOutcome::NotJson => "not-json",
                TreeOutcome::GetError => "get-error",
            };
            writeln!(out, "T\t{}\t{}\t{outcome}", t.bucket, t.key)
        }
    }
}

/// A `P` line's fields as a pack header and the number of `B` lines that follow it.
fn pack_header(fields: &[&str], path: &Path, line: usize) -> Result<(PackFooter, usize), Error> {
    let (bucket, key, name, n, text) = match fields {
        [_, bucket, key, name, n] => (bucket, key, name, n, None),
        [_, bucket, key, name, n, text] => (bucket, key, name, n, Some(text)),
        _ => {
            return Err(Error::parse(
                path,
                line,
                format!(
                    "a P line has 5 fields, or 6 for get-error, not {}",
                    fields.len()
                ),
            ));
        }
    };
    let status = match (text, Status::from_name(name)) {
        (None, Some(status)) => status,
        (Some(text), None) if *name == "get-error" => Status::GetError((*text).to_owned()),
        _ => {
            return Err(Error::parse(
                path,
                line,
                format!("status {name:?} is not one with {} fields", fields.len()),
            ));
        }
    };
    let n = n
        .parse::<usize>()
        .map_err(|_| Error::parse(path, line, format!("blob count {n:?} is not a number")))?;
    let pack = PackFooter {
        bucket: (*bucket).to_owned(),
        key: (*key).to_owned(),
        status,
        blobs: Vec::new(),
    };
    Ok((pack, n))
}

/// A `B` line's fields as a blob.
fn blob_line(digest: &str, size: &str, path: &Path, line: usize) -> Result<Blob, Error> {
    let mut bytes = [0u8; 32];
    hex::decode_to_slice(digest, &mut bytes).map_err(|e| {
        Error::parse(
            path,
            line,
            format!("digest {digest:?} is not 64 hex digits: {e}"),
        )
    })?;
    let size = size
        .parse::<u64>()
        .map_err(|_| Error::parse(path, line, format!("size {size:?} is not a number")))?;
    Ok(Blob {
        digest: bytes,
        size,
    })
}

/// A `T` line's fields as a tree check.
fn tree_line(fields: &[&str], path: &Path, line: usize) -> Result<TreeCheck, Error> {
    let outcome = match fields {
        [_, _, _, "json"] => TreeOutcome::Json,
        [_, _, _, "not-json"] => TreeOutcome::NotJson,
        [_, _, _, "get-error"] => TreeOutcome::GetError,
        _ => {
            return Err(Error::parse(
                path,
                line,
                "a T line has 4 fields, the last json, not-json or get-error",
            ));
        }
    };
    Ok(TreeCheck {
        bucket: fields[1].to_owned(),
        key: fields[2].to_owned(),
        outcome,
    })
}

/// Refuses a file that ends, or starts another object, while a pack lacks `missing` blobs.
fn complete(missing: usize, path: &Path, line: usize) -> Result<(), Error> {
    if missing == 0 {
        return Ok(());
    }
    Err(Error::parse(
        path,
        line,
        format!("the pack above lacks {missing} blob lines"),
    ))
}

/// The footer file at `path`, read back.
///
/// # Errors
/// [`Error::Io`] when the file cannot be read; [`Error::Parse`], with the line's number, for a
/// line that is not one of the three forms, a number or digest that is not one, a `B` line
/// outside a pack or past its count, or a pack with fewer `B` lines than its count.
pub fn read(path: &Path) -> Result<Scan, Error> {
    let file = std::fs::File::open(path).map_err(|e| Error::io("open", path, e))?;
    let mut scan = Scan::default();
    let mut missing = 0usize;
    let mut last = 0usize;
    for (i, text) in BufReader::new(file).lines().enumerate() {
        let text = text.map_err(|e| Error::io("read", path, e))?;
        let line = i + 1;
        last = line;
        let fields: Vec<&str> = text.split('\t').collect();
        match (fields[0], fields.len()) {
            ("B", 3) => {
                let blob = blob_line(fields[1], fields[2], path, line)?;
                match scan.packs.last_mut() {
                    Some(pack) if missing > 0 => pack.blobs.push(blob),
                    _ => return Err(Error::parse(path, line, "a B line outside a pack's count")),
                }
                missing -= 1;
            }
            ("P", _) => {
                complete(missing, path, line)?;
                let (pack, n) = pack_header(&fields, path, line)?;
                missing = n;
                scan.packs.push(pack);
            }
            ("T", _) => {
                complete(missing, path, line)?;
                scan.trees.push(tree_line(&fields, path, line)?);
            }
            _ => {
                return Err(Error::parse(
                    path,
                    line,
                    format!(
                        "{:?} with {} fields is not a P, B or T line",
                        fields[0],
                        fields.len()
                    ),
                ));
            }
        }
    }
    complete(missing, path, last + 1)?;
    Ok(scan)
}

/// The writer of the footer file, with the path its errors name.
struct Sink<'a> {
    out: &'a mut dyn Write,
    path: &'a Path,
}

impl Sink<'_> {
    /// Writes one scanned object.
    fn put(&mut self, entry: &Checked) -> Result<(), Error> {
        write_entry(self.out, entry).map_err(|e| Error::io("write", self.path, e))
    }

    /// Flushes what was written.
    fn finish(&mut self) -> Result<(), Error> {
        self.out
            .flush()
            .map_err(|e| Error::io("flush", self.path, e))
    }
}

/// How many packs and tree lists a scan wrote, and how many were whole.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Packs scanned.
    pub packs: u64,
    /// Packs whose status is `ok`.
    pub ok: u64,
    /// Tree lists scanned.
    pub trees: u64,
    /// Tree lists whose first byte is `{`.
    pub json: u64,
}

/// Reads the packs and tree lists among `objects` with at most `conc` requests in flight and
/// writes what it finds to `out` (named `path` in an error) in `objects` order. One client is
/// made per bucket, with `endpoint`'s address and `timeout`.
///
/// # Errors
/// [`Error::Usage`] when a client cannot be made; [`Error::Io`] when `out` cannot be written.
pub async fn scan(
    endpoint: &str,
    credentials: &Credentials,
    timeout: Duration,
    objects: &[Object],
    conc: usize,
    out: &mut dyn Write,
    path: &Path,
) -> Result<Counts, Error> {
    let mut at: BTreeMap<&str, usize> = BTreeMap::new();
    let mut stores: Vec<S3Store> = Vec::new();
    let mut work: Vec<(usize, Kind, &Object)> = Vec::new();
    for object in objects {
        let kind = classify(&object.key, object.size);
        if kind != Kind::Pack && kind != Kind::TreeList {
            continue;
        }
        let index = match at.get(object.bucket.as_str()) {
            Some(&i) => i,
            None => {
                let store =
                    crate::s3::store(endpoint, &object.bucket, credentials.clone(), timeout)?;
                stores.push(store);
                at.insert(&object.bucket, stores.len() - 1);
                stores.len() - 1
            }
        };
        work.push((index, kind, object));
    }
    let stores = &stores;
    let mut checked = futures::stream::iter(work)
        .map(|(index, kind, object)| async move {
            if kind == Kind::Pack {
                Checked::Pack(scan_pack(&stores[index], object).await)
            } else {
                Checked::Tree(scan_tree(&stores[index], object).await)
            }
        })
        .buffered(conc.max(1));
    let mut sink = Sink { out, path };
    let mut counts = Counts::default();
    while let Some(entry) = checked.next().await {
        sink.put(&entry)?;
        match &entry {
            Checked::Pack(p) => {
                counts.packs += 1;
                counts.ok += u64::from(p.status == Status::Ok);
            }
            Checked::Tree(t) => {
                counts.trees += 1;
                counts.json += u64::from(t.outcome == TreeOutcome::Json);
            }
        }
    }
    sink.finish()?;
    Ok(counts)
}

/// Runs `storebench footers`: scans every pack and tree list of the listing and writes the
/// footer file, then one summary line to `stdout`.
///
/// # Errors
/// [`Error`] of the credentials, the listing, the footer file, the store clients, or `stdout`.
pub fn run(args: &Args, stdout: &mut dyn Write) -> Result<(), Error> {
    let credentials = crate::s3::credentials(&args.endpoint.credentials)?;
    let mut trees = 0u64;
    let share = args.share;
    let objects: Vec<Object> = listing::read(&args.listing)?
        .into_iter()
        .enumerate()
        .filter(|(i, _)| *i as u64 % share.of == share.index)
        .map(|(_, o)| o)
        .filter(|o| match classify(&o.key, o.size) {
            Kind::Pack => o.size >= args.min_pack_bytes,
            Kind::TreeList => {
                trees += 1;
                (trees - 1).is_multiple_of(args.tree_every.max(1))
            }
            Kind::LargeBlob | Kind::Other => true,
        })
        .collect();
    let rt = tokio::runtime::Builder::new_multi_thread()
        // The work is requests to silo: a few threads keep them in flight, and a farm node has
        // 88 CPUs, which is not the number of threads this needs.
        .worker_threads(4)
        .enable_all()
        .build()
        .map_err(|e| Error::io("start", "the async runtime", e))?;
    let mut file = listing::create(&args.out)?;
    let timeout = Duration::from_secs(args.endpoint.request_timeout_secs);
    let counts = rt.block_on(scan(
        &args.endpoint.endpoint,
        &credentials,
        timeout,
        &objects,
        args.conc as usize,
        &mut file,
        &args.out,
    ))?;
    writeln!(
        stdout,
        "{} packs ({} ok), {} tree lists ({} json) in {}",
        counts.packs,
        counts.ok,
        counts.trees,
        counts.json,
        args.out.display()
    )
    .map_err(|e| Error::io("write", "standard output", e))
}

#[cfg(test)]
#[path = "footers_tests.rs"]
mod tests;
