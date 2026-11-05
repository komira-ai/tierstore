//! The `aws-chunked` framing of a request body: the three streaming payload types of S3
//! (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`, `STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER` and
//! `STREAMING-UNSIGNED-PAYLOAD-TRAILER`), decoded as the pieces of the body arrive.
//!
//! The body is a run of chunks, `<size in hex>[;chunk-signature=<hex>]\r\n<data>\r\n`, ended by a
//! chunk of size 0, then the trailer (`<name>:<value>\r\n` lines, and for a signed stream
//! `x-amz-trailer-signature:<hex>\r\n`), then an empty line. In a signed stream the signature of
//! each chunk covers the one before it, from the signature of the request's head (the seed), so
//! that a chunk can be neither changed nor moved; the trailer is signed in the same chain.
//!
//! The decoder holds a line at a time (at most `server.aws_chunked_max_line_bytes`) and the
//! hash state of the chunk it is in, and nothing of the data: the data of a chunk is passed on
//! as it arrives, before the signature of that chunk, which comes first in a signed stream but
//! is checked against the whole chunk, has been checked. A stream whose chunk fails is ended
//! with the error, and the caller stores nothing of a body that ends in an error (the same
//! rule as for the checksums of a body, which are known only at its end).

use std::collections::VecDeque;

use bytes::Bytes;
use hmac::Mac;
use sha2::{Digest, Sha256};

use super::auth::{MISMATCH, StreamAuth, keyed};
use super::reply::S3Error;
use super::request::header;

const SIGNED: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD";
const SIGNED_TRAILER: &str = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER";
const UNSIGNED_TRAILER: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";

/// The trailing checksums S3 takes, as the headers that would carry them.
pub(crate) const TRAILABLE: [&str; 5] = [
    "x-amz-checksum-crc32",
    "x-amz-checksum-crc32c",
    "x-amz-checksum-crc64nvme",
    "x-amz-checksum-sha1",
    "x-amz-checksum-sha256",
];

/// The name of the line that signs the trailer.
const TRAILER_SIGNATURE: &str = "x-amz-trailer-signature";

/// The hex SHA-256 of nothing, which every chunk signature has in its string to sign.
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// What the head of a request says of its streamed body.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Framing {
    signed: bool,
    trailer: Option<String>,
    decoded: u64,
}

impl Framing {
    /// The framing of a body whose `x-amz-content-sha256` is `claim`: `None` for a payload that
    /// is not streamed.
    ///
    /// # Errors
    /// `NotImplemented` for a streaming payload type this server does not do (`...-ECDSA-...`);
    /// `MissingContentLength` for no `x-amz-decoded-content-length`; `InvalidArgument` for one
    /// that is not a whole number; `InvalidRequest` for a trailer that `x-amz-trailer` does not
    /// name as one checksum.
    pub(crate) fn parse(
        claim: &str,
        headers: &hyper::header::HeaderMap,
    ) -> Result<Option<Self>, S3Error> {
        let (signed, trailing) = match claim {
            SIGNED => (true, false),
            SIGNED_TRAILER => (true, true),
            UNSIGNED_TRAILER => (false, true),
            other if other.starts_with("STREAMING-") => {
                let text = format!("the payload type {other} is not supported");
                return Err(S3Error::unsupported(text));
            }
            _ => return Ok(None),
        };
        let name = "x-amz-decoded-content-length";
        let text = header(headers, name).ok_or_else(S3Error::decoded_length_required)?;
        let decoded = text.trim().parse::<u64>().ok();
        let decoded = decoded.ok_or_else(|| S3Error::invalid(format!("{name} is not a length")))?;
        let trailer = if trailing {
            Some(trailer_name(headers)?)
        } else {
            None
        };
        Ok(Some(Self {
            signed,
            trailer,
            decoded,
        }))
    }

    /// The header the trailing checksum would be in, if the body has one.
    pub(crate) fn trailer(&self) -> Option<&str> {
        self.trailer.as_deref()
    }

    /// The length of the object, which is not the length of the body: the length the request
    /// gave in `x-amz-decoded-content-length`.
    pub(crate) fn decoded(&self) -> u64 {
        self.decoded
    }

    /// Whether the chunks are signed.
    pub(crate) fn signed(&self) -> bool {
        self.signed
    }
}

/// The checksum that `x-amz-trailer` announces: one of [`TRAILABLE`].
fn trailer_name(headers: &hyper::header::HeaderMap) -> Result<String, S3Error> {
    let text = header(headers, "x-amz-trailer").unwrap_or_default();
    let name = text.trim().to_ascii_lowercase();
    if TRAILABLE.contains(&name.as_str()) {
        Ok(name)
    } else {
        let text = "x-amz-trailer must name one checksum (x-amz-checksum-crc32, -crc32c, \
                    -crc64nvme, -sha1 or -sha256) for a body with a trailer.";
        Err(S3Error::invalid_request(text))
    }
}

/// Where the decoder is in the body.
#[derive(Clone, Copy)]
enum Phase {
    /// Reading the line with the size of a chunk.
    Header,
    /// In the data of a chunk, this many bytes still to come.
    Data(u64),
    /// Reading the line end after the data.
    DataEnd,
    /// Reading the lines of the trailer, up to the empty one.
    Trailer,
    /// The empty line was read: nothing may follow.
    Done,
}

/// The chain of signatures of a signed stream.
struct Chain {
    auth: StreamAuth,
    /// The signature the next one covers, as lower case hex.
    previous: String,
    /// The hash of the data of the chunk being read.
    chunk: Sha256,
    /// The signature the stream gave for what is being read, as bytes.
    wanted: Vec<u8>,
}

impl Chain {
    /// Checks that the signature `wanted` is that of the string `text`, and makes it the one
    /// the next covers.
    fn check(&mut self, text: &str) -> Result<(), S3Error> {
        let mut mac = keyed(&self.auth.key);
        mac.update(text.as_bytes());
        let wanted = std::mem::take(&mut self.wanted);
        mac.verify_slice(&wanted)
            .map_err(|_| S3Error::from(MISMATCH))?;
        self.previous = hex::encode(wanted);
        Ok(())
    }

    /// Checks the chunk whose data was hashed against the signature its head gave.
    fn check_chunk(&mut self) -> Result<(), S3Error> {
        let hash = hex::encode(std::mem::take(&mut self.chunk).finalize());
        let (stamp, scope) = (&self.auth.stamp, &self.auth.scope);
        let previous = &self.previous;
        let text =
            format!("AWS4-HMAC-SHA256-PAYLOAD\n{stamp}\n{scope}\n{previous}\n{EMPTY}\n{hash}");
        self.check(&text)
    }

    /// Checks the trailer, as the lines `canonical` (each ended by a newline), against the
    /// signature the stream gave for it.
    fn check_trailer(
        &mut self,
        canonical: &str,
        signature: Option<Vec<u8>>,
    ) -> Result<(), S3Error> {
        self.wanted = signature.ok_or_else(|| S3Error::from(MISMATCH))?;
        let hash = hex::encode(Sha256::digest(canonical.as_bytes()));
        let (stamp, scope) = (&self.auth.stamp, &self.auth.scope);
        let previous = &self.previous;
        let text = format!("AWS4-HMAC-SHA256-TRAILER\n{stamp}\n{scope}\n{previous}\n{hash}");
        self.check(&text)
    }
}

/// The decoder of one `aws-chunked` body.
pub(crate) struct Decoder {
    /// The checksum the trailer must carry, if the request announced one.
    trailer: Option<String>,
    max_line: usize,
    expected: u64,
    decoded: u64,
    phase: Phase,
    /// The line being read, up to its newline.
    line: Vec<u8>,
    chain: Option<Chain>,
    /// The text of the trailing checksum, once read.
    value: Option<String>,
    /// The trailer as the signature of a signed stream covers it.
    canonical: String,
    trailer_signature: Option<Vec<u8>>,
}

fn malformed(text: &str) -> S3Error {
    S3Error::invalid_request(format!("The aws-chunked body is malformed: {text}."))
}

/// The text of a line without its `\r\n`.
fn unframed(line: &[u8]) -> Result<&str, S3Error> {
    let line = line.strip_suffix(b"\r\n");
    let text = line.and_then(|bytes| std::str::from_utf8(bytes).ok());
    text.ok_or_else(|| malformed("a line is not text ended by CR LF"))
}

/// The text of a line of the trailer without its line end, which is `\r\n` but may be `\n`:
/// minio-go (so `mc`) ends the checksum line of an unsigned trailer with `\n`, and S3 takes it.
fn trailer_text(line: &[u8]) -> Result<&str, S3Error> {
    let line = line.strip_suffix(b"\n").unwrap_or_default();
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let text = std::str::from_utf8(line).ok();
    text.ok_or_else(|| malformed("a line of the trailer is not text"))
}

/// The size of a chunk, in hex digits only.
fn chunk_size(text: &str) -> Result<u64, S3Error> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_hexdigit());
    let size = u64::from_str_radix(text, 16).ok().filter(|_| digits);
    size.ok_or_else(|| malformed("the size of a chunk is not a number"))
}

impl Decoder {
    /// A decoder for a body with this `framing`, which holds a line of at most `max_line` bytes.
    /// `stream` is what the signatures of a signed stream are made from.
    ///
    /// # Errors
    /// `NotImplemented` if the stream is signed and the request gave nothing to check it by.
    pub(crate) fn new(
        framing: &Framing,
        stream: Option<StreamAuth>,
        max_line: usize,
    ) -> Result<Self, S3Error> {
        let chain = if framing.signed {
            let auth =
                stream.ok_or_else(|| S3Error::unsupported("this request cannot sign a stream"))?;
            Some(Chain {
                previous: auth.seed.clone(),
                auth,
                chunk: Sha256::new(),
                wanted: Vec::new(),
            })
        } else {
            None
        };
        Ok(Self {
            trailer: framing.trailer.clone(),
            max_line,
            expected: framing.decoded,
            decoded: 0,
            phase: Phase::Header,
            line: Vec::new(),
            chain,
            value: None,
            canonical: String::new(),
            trailer_signature: None,
        })
    }

    /// Takes the next piece of the body, and puts the data in it on `out`, in order.
    ///
    /// # Errors
    /// `InvalidRequest` for a body that is not framed as it must be or has a line over the limit;
    /// `SignatureDoesNotMatch` for a chunk or a trailer whose signature is not the one that
    /// follows from the chain; `IncompleteBody` for more data than the request said.
    pub(crate) fn feed(
        &mut self,
        mut piece: Bytes,
        out: &mut VecDeque<Bytes>,
    ) -> Result<(), S3Error> {
        while !piece.is_empty() {
            if let Phase::Data(left) = self.phase {
                let taken =
                    piece.split_to(piece.len().min(usize::try_from(left).unwrap_or(usize::MAX)));
                self.take_data(&taken, left)?;
                out.push_back(taken);
                continue;
            }
            if matches!(self.phase, Phase::Done) {
                // Line ends may follow the empty line (minio-go sends one more `\r\n`); nothing else.
                if piece.iter().all(|byte| matches!(byte, b'\r' | b'\n')) {
                    return Ok(());
                }
                return Err(malformed("bytes follow the end of the trailer"));
            }
            let end = piece.iter().position(|byte| *byte == b'\n');
            let part = piece.split_to(end.map_or(piece.len(), |at| at + 1));
            if self.line.len() + part.len() > self.max_line {
                return Err(malformed(
                    "a line is longer than server.aws_chunked_max_line_bytes",
                ));
            }
            self.line.extend_from_slice(&part);
            if end.is_some() {
                let line = std::mem::take(&mut self.line);
                self.read_line(&line)?;
            }
        }
        Ok(())
    }

    /// Counts and hashes `data`, which is part of a chunk with `left` bytes still to come.
    fn take_data(&mut self, data: &[u8], left: u64) -> Result<(), S3Error> {
        self.decoded = self.decoded.saturating_add(data.len() as u64);
        if self.decoded > self.expected {
            let text = "The body has more data than x-amz-decoded-content-length says.";
            return Err(S3Error::incomplete_body(text));
        }
        let left = left - data.len() as u64;
        self.phase = if left == 0 {
            Phase::DataEnd
        } else {
            Phase::Data(left)
        };
        if let Some(chain) = &mut self.chain {
            chain.chunk.update(data);
            if left == 0 {
                chain.check_chunk()?;
            }
        }
        Ok(())
    }

    fn read_line(&mut self, line: &[u8]) -> Result<(), S3Error> {
        match self.phase {
            Phase::Header => self.read_header(unframed(line)?),
            Phase::DataEnd if line == b"\r\n" => {
                self.phase = Phase::Header;
                Ok(())
            }
            Phase::DataEnd => Err(malformed("a chunk is not followed by CR LF")),
            _ => self.read_trailer(trailer_text(line)?),
        }
    }

    /// The line with the size of a chunk, and its signature if the stream is signed.
    fn read_header(&mut self, text: &str) -> Result<(), S3Error> {
        let (size, signature) = match self.chain {
            Some(_) => {
                let (size, signature) = text.split_once(";chunk-signature=").unzip();
                let signature =
                    signature.ok_or_else(|| malformed("a chunk has no chunk-signature"))?;
                (size.unwrap_or_default(), Some(signature))
            }
            None => (text, None),
        };
        let size = chunk_size(size)?;
        if let (Some(chain), Some(signature)) = (&mut self.chain, signature) {
            chain.wanted = hex::decode(signature).map_err(|_| S3Error::from(MISMATCH))?;
        }
        if size == 0 {
            self.phase = Phase::Trailer;
            if let Some(chain) = &mut self.chain {
                chain.check_chunk()?;
            }
        } else {
            self.phase = Phase::Data(size);
        }
        Ok(())
    }

    /// A line of the trailer: the checksum, its signature, or the empty line that ends the body.
    fn read_trailer(&mut self, text: &str) -> Result<(), S3Error> {
        if text.is_empty() {
            return self.end_trailer();
        }
        let (name, value) = text.split_once(':').unwrap_or((text, ""));
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        let announced = self.trailer.as_deref() == Some(name.as_str()) && self.value.is_none();
        if announced {
            self.canonical = format!("{name}:{value}\n");
            self.value = Some(value.to_owned());
        } else if name == TRAILER_SIGNATURE
            && self.chain.is_some()
            && self.trailer.is_some()
            && self.trailer_signature.is_none()
        {
            let signature = hex::decode(value).map_err(|_| S3Error::from(MISMATCH))?;
            self.trailer_signature = Some(signature);
        } else {
            return Err(malformed(
                "a line of the trailer is not the one x-amz-trailer announced",
            ));
        }
        Ok(())
    }

    /// The empty line: the trailer is whole.
    fn end_trailer(&mut self) -> Result<(), S3Error> {
        self.phase = Phase::Done;
        if let Some(name) = &self.trailer
            && self.value.is_none()
        {
            let text =
                format!("The trailer {name} that x-amz-trailer announced is not in the body.");
            return Err(S3Error::invalid_request(text));
        }
        let signature = self.trailer_signature.take();
        match (&mut self.chain, &self.trailer) {
            (Some(chain), Some(_)) => chain.check_trailer(&self.canonical, signature),
            _ => Ok(()),
        }
    }

    /// The end of the body: whether it was whole, and the text of the trailing checksum if it
    /// had one.
    ///
    /// # Errors
    /// `IncompleteBody` for a body that ended before its last chunk and trailer were read, or
    /// that decoded to less than `x-amz-decoded-content-length`.
    pub(crate) fn finish(self) -> Result<Option<String>, S3Error> {
        if !matches!(self.phase, Phase::Done) {
            let text = "The aws-chunked body ended before its last chunk and trailer.";
            return Err(S3Error::incomplete_body(text));
        }
        if self.decoded != self.expected {
            let (got, said) = (self.decoded, self.expected);
            let text = format!(
                "The body decoded to {got} bytes, and x-amz-decoded-content-length says {said}."
            );
            return Err(S3Error::incomplete_body(text));
        }
        Ok(self.value)
    }
}
