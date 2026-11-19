//! Silo's per-object metadata file, `xl.meta`, read for the facts of a listing.
//!
//! Silo keeps MinIO's on-drive format: every drive of an erasure set has, for each object, a
//! directory named by the object's key holding `xl.meta` (and, unless the object's bytes are
//! inlined in it, a data directory of part files). `xl.meta` is:
//!
//! ```text
//! "XL2 " | major u16 | minor u16 | msgpack bin: header version, meta version, version count,
//!                                  then per version a bin (its header) and a bin (its metadata)
//!                                | crc | inline data
//! ```
//!
//! A version's metadata is a msgpack map with `Type` (1 an object, 2 a delete marker) and, for
//! an object, `V2Obj`: `Size`, `MTime` (nanoseconds since the Unix epoch), `EcM` (data shards),
//! `EcN` (parity shards), `PartNums`, `MetaSys` (`x-minio-internal-inline-data` when the bytes
//! are in `xl.meta`) and `MetaUsr` (`etag`). The versions are newest first. Only what a listing
//! and the raw-to-logical gap need is read; anything else is skipped.

use crate::Error;

/// A msgpack value, as far as `xl.meta` needs one.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// `nil`.
    Nil,
    /// A boolean.
    Bool(bool),
    /// An integer (both signs fit: `xl.meta` holds sizes, times and counts).
    Int(i128),
    /// A float (skipped by every reader here).
    Float(f64),
    /// A string.
    Str(String),
    /// Bytes (`bin`).
    Bin(Vec<u8>),
    /// An array.
    Array(Vec<Value>),
    /// A map, in file order.
    Map(Vec<(Value, Value)>),
    /// An extension value (type, bytes).
    Ext(i8, Vec<u8>),
}

impl Value {
    /// The value of `key` in a map with string keys.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Map(entries) => entries.iter().find_map(|(k, v)| match k {
                Value::Str(s) if s == key => Some(v),
                _ => None,
            }),
            _ => None,
        }
    }

    /// The integer, if this is one.
    #[must_use]
    pub fn int(&self) -> Option<i128> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// The string, if this is one.
    #[must_use]
    pub fn str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// The deepest nesting a value may have: `xl.meta` nests four deep; a deeper one is damage.
const MAX_DEPTH: usize = 16;

/// A reader of msgpack values from a byte slice.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .at
            .checked_add(n)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| format!("{n} bytes wanted at offset {}, past the end", self.at))?;
        let s = &self.bytes[self.at..end];
        self.at = end;
        Ok(s)
    }

    fn uint(&mut self, n: usize) -> Result<u64, String> {
        Ok(self
            .take(n)?
            .iter()
            .fold(0u64, |acc, b| acc << 8 | u64::from(*b)))
    }

    fn len(&mut self, n: usize) -> Result<usize, String> {
        usize::try_from(self.uint(n)?).map_err(|e| e.to_string())
    }

    fn sint(&mut self, n: usize) -> Result<i128, String> {
        let raw = self.uint(n)?;
        let bits = 8 * n as u32;
        let shift = 128 - bits;
        Ok((i128::from(raw) << shift) >> shift)
    }

    fn text(&mut self, n: usize) -> Result<Value, String> {
        let raw = self.take(n)?;
        String::from_utf8(raw.to_vec())
            .map(Value::Str)
            .map_err(|e| e.to_string())
    }

    fn array(&mut self, n: usize, depth: usize) -> Result<Value, String> {
        let mut items = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            items.push(self.value(depth + 1)?);
        }
        Ok(Value::Array(items))
    }

    fn map(&mut self, n: usize, depth: usize) -> Result<Value, String> {
        let mut entries = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            let k = self.value(depth + 1)?;
            let v = self.value(depth + 1)?;
            entries.push((k, v));
        }
        Ok(Value::Map(entries))
    }

    fn ext(&mut self, n: usize) -> Result<Value, String> {
        let t = i8::from_be_bytes([self.take(1)?[0]]);
        Ok(Value::Ext(t, self.take(n)?.to_vec()))
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err(format!("nested deeper than {MAX_DEPTH}"));
        }
        let tag = self.take(1)?[0];
        match tag {
            0x00..=0x7f => Ok(Value::Int(i128::from(tag))),
            0x80..=0x8f => self.map(usize::from(tag & 0x0f), depth),
            0x90..=0x9f => self.array(usize::from(tag & 0x0f), depth),
            0xa0..=0xbf => self.text(usize::from(tag & 0x1f)),
            0xc0 => Ok(Value::Nil),
            0xc2 => Ok(Value::Bool(false)),
            0xc3 => Ok(Value::Bool(true)),
            0xc4..=0xc6 => {
                let n = self.len(1 << (tag - 0xc4))?;
                Ok(Value::Bin(self.take(n)?.to_vec()))
            }
            0xc7..=0xc9 => {
                let n = self.len(1 << (tag - 0xc7))?;
                self.ext(n)
            }
            0xca => {
                let raw = self.uint(4)?;
                Ok(Value::Float(f64::from(f32::from_bits(raw as u32))))
            }
            0xcb => Ok(Value::Float(f64::from_bits(self.uint(8)?))),
            0xcc..=0xcf => Ok(Value::Int(i128::from(self.uint(1 << (tag - 0xcc))?))),
            0xd0..=0xd3 => Ok(Value::Int(self.sint(1 << (tag - 0xd0))?)),
            0xd4..=0xd8 => self.ext(1 << (tag - 0xd4)),
            0xd9..=0xdb => {
                let n = self.len(1 << (tag - 0xd9))?;
                self.text(n)
            }
            0xdc | 0xdd => {
                let n = self.len(2 << (tag - 0xdc))?;
                self.array(n, depth)
            }
            0xde | 0xdf => {
                let n = self.len(2 << (tag - 0xde))?;
                self.map(n, depth)
            }
            0xe0..=0xff => Ok(Value::Int(i128::from(i8::from_be_bytes([tag])))),
            0xc1 => Err("0xc1 is not a msgpack type".to_owned()),
        }
    }
}

/// Decodes one msgpack value at the start of `bytes`, and how many bytes it took.
///
/// # Errors
/// What is wrong, as text: a value cut short, a string that is not UTF-8, the unused type
/// `0xc1`, or nesting deeper than any `xl.meta` has.
pub fn decode(bytes: &[u8]) -> Result<(Value, usize), String> {
    let mut r = Reader { bytes, at: 0 };
    let v = r.value(0)?;
    Ok((v, r.at))
}

/// What a listing and the gap need of one object, from its `xl.meta`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
    /// The object's size in bytes.
    pub size: u64,
    /// Its last write, in milliseconds since the Unix epoch.
    pub modified_ms: u64,
    /// Its entity tag (`<md5>` or `<md5>-<parts>`).
    pub etag: String,
    /// Its data shards.
    pub data: u32,
    /// Its parity shards.
    pub parity: u32,
    /// Its parts (a multipart upload's part count, else 1).
    pub parts: u32,
    /// Whether its bytes are inlined in `xl.meta` (no part file).
    pub inline: bool,
    /// The versions `xl.meta` holds (1 unless the bucket keeps versions).
    pub versions: u32,
}

/// What `xl.meta` says the object is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Meta {
    /// An object, as its newest version describes it.
    Object(ObjectMeta),
    /// The newest version is a delete marker: the object is not listed.
    Deleted {
        /// The versions `xl.meta` holds.
        versions: u32,
    },
}

/// The facts of the object whose `xl.meta` is `bytes`.
///
/// # Errors
/// [`Error::Parse`] naming `path` when the file is not an `xl.meta` of format 1, its msgpack is
/// damaged, it has no version, or its newest object version lacks a size, time or tag.
pub fn parse(bytes: &[u8], path: &std::path::Path) -> Result<Meta, Error> {
    let bad = |why: String| Error::parse(path, 0, why);
    if bytes.len() < 8 || &bytes[..4] != b"XL2 " {
        return Err(bad("not an xl.meta (no XL2 magic)".to_owned()));
    }
    let major = u16::from_le_bytes([bytes[4], bytes[5]]);
    if major != 1 {
        return Err(bad(format!("xl.meta major version {major}, not 1")));
    }
    let (block, _) = decode(&bytes[8..]).map_err(&bad)?;
    let Value::Bin(block) = block else {
        return Err(bad("the version block is not a bin".to_owned()));
    };
    let mut r = Reader {
        bytes: &block,
        at: 0,
    };
    let mut next = |what: &str| r.value(0).map_err(|e| bad(format!("{what}: {e}")));
    let _header_version = next("header version")?;
    let _meta_version = next("meta version")?;
    let count = next("version count")?
        .int()
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| bad("the version count is not a count".to_owned()))?;
    if count == 0 {
        return Err(bad("no version".to_owned()));
    }
    let _header = next("version header")?;
    let Value::Bin(newest) = next("version metadata")? else {
        return Err(bad("the version metadata is not a bin".to_owned()));
    };
    let (version, _) = decode(&newest).map_err(&bad)?;
    match version.get("Type").and_then(Value::int) {
        Some(1) => {}
        Some(2) => return Ok(Meta::Deleted { versions: count }),
        other => return Err(bad(format!("version type {other:?}, not an object"))),
    }
    let obj = version
        .get("V2Obj")
        .ok_or_else(|| bad("an object version without V2Obj".to_owned()))?;
    let field = |name: &str| {
        obj.get(name)
            .and_then(Value::int)
            .ok_or_else(|| bad(format!("V2Obj without an integer {name}")))
    };
    let size = u64::try_from(field("Size")?).map_err(|e| bad(format!("Size: {e}")))?;
    let mtime_ns = u64::try_from(field("MTime")?).map_err(|e| bad(format!("MTime: {e}")))?;
    let small = |name: &str| -> Result<u32, Error> {
        u32::try_from(field(name)?).map_err(|e| bad(format!("{name}: {e}")))
    };
    let parts = match obj.get("PartNums") {
        Some(Value::Array(a)) => u32::try_from(a.len()).unwrap_or(u32::MAX),
        _ => 1,
    };
    let etag = obj
        .get("MetaUsr")
        .and_then(|m| m.get("etag"))
        .and_then(Value::str)
        .ok_or_else(|| bad("V2Obj without MetaUsr etag".to_owned()))?
        .to_owned();
    let inline = obj
        .get("MetaSys")
        .and_then(|m| m.get("x-minio-internal-inline-data"))
        .is_some_and(|v| matches!(v, Value::Bin(b) if b == b"true") || v.str() == Some("true"));
    Ok(Meta::Object(ObjectMeta {
        size,
        modified_ms: mtime_ns / 1_000_000,
        etag,
        data: small("EcM")?,
        parity: small("EcN")?,
        parts,
        inline,
        versions: count,
    }))
}

#[cfg(test)]
#[path = "xlmeta_tests.rs"]
pub(crate) mod tests;
