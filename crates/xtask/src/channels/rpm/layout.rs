//! The bytes of an RPM file as far as signing needs them: the lead, the signature header, the main
//! header and the payload, and the one change signing makes, a signature entry in the signature
//! header.
//!
//! A package is `lead` (96 bytes), the signature header (padded to a multiple of 8), the main
//! header, and the payload to the end of the file. Every header is `8e ad e8 01`, four reserved
//! bytes, the number of index entries and the size of the store (both big-endian `u32`), the
//! entries (`tag`, `type`, `offset`, `count`, 16 bytes each, sorted by tag) and the store they point
//! into. The header-only signature (`RPMSIGTAG_RSA`, or `RPMSIGTAG_DSA` for the other public-key
//! algorithms) is an OpenPGP signature of the main header's bytes. The digests of the signature
//! header (`SHA256` and `SHA1` of the main header, the sizes) are not touched, so a package with its
//! signature entry removed is the unsigned package, byte for byte: [`without_signatures`].
//!
//! The signature header is read into its entries and written again by [`Sig::write`] in one
//! canonical form (entries by tag, each datum aligned for its type, the region trailer last); a
//! file whose signature header is not already in that form is refused instead of rewritten, so the
//! package `release rpm` builds is the one form this module signs and un-signs.

use crate::Error;

/// The length of the lead.
const LEAD: usize = 96;
/// The magic of the lead.
const LEAD_MAGIC: [u8; 4] = [0xed, 0xab, 0xee, 0xdb];
/// The magic and version of a header.
const HEADER_MAGIC: [u8; 8] = [0x8e, 0xad, 0xe8, 0x01, 0, 0, 0, 0];
/// The tag of the signature header's region entry, whose datum is the trailer.
const REGION: u32 = 62;
/// The type of binary data.
const BIN: u32 = 7;
/// `RPMSIGTAG_DSA`: the header-only signature of a DSA, ECDSA or EdDSA key.
pub const TAG_DSA: u32 = 267;
/// `RPMSIGTAG_RSA`: the header-only signature of an RSA key.
pub const TAG_RSA: u32 = 268;

fn bad(why: impl std::fmt::Display) -> Error {
    Error::Failed(format!("not an RPM package: {why}"))
}

fn be32(bytes: &[u8], at: usize) -> Result<u32, Error> {
    bytes
        .get(at..at + 4)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_be_bytes)
        .ok_or_else(|| bad("a header is cut short"))
}

/// One entry of the signature header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub tag: u32,
    pub kind: u32,
    pub count: u32,
    pub data: Vec<u8>,
}

/// The alignment of the data of a type.
fn alignment(kind: u32) -> usize {
    match kind {
        3 => 2,
        4 => 4,
        5 => 8,
        _ => 1,
    }
}

/// The signature header: its entries but the region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sig {
    pub entries: Vec<Entry>,
}

impl Sig {
    /// The header in its canonical form, without the padding.
    fn write(&self) -> Vec<u8> {
        let mut entries: Vec<&Entry> = self.entries.iter().collect();
        entries.sort_by_key(|e| e.tag);
        let mut store: Vec<u8> = Vec::new();
        let mut index: Vec<u8> = Vec::new();
        let total = u32::try_from(entries.len() + 1).unwrap_or(u32::MAX);
        let trailer = |count: u32| -> Vec<u8> {
            let offset = -i32::try_from(count * 16).unwrap_or(i32::MAX);
            [REGION, BIN]
                .into_iter()
                .chain([offset as u32, 16])
                .flat_map(u32::to_be_bytes)
                .collect()
        };
        for e in entries {
            while !store.len().is_multiple_of(alignment(e.kind)) {
                store.push(0);
            }
            for word in [e.tag, e.kind, store.len() as u32, e.count] {
                index.extend(word.to_be_bytes());
            }
            store.extend(&e.data);
        }
        let region: Vec<u8> = [REGION, BIN, store.len() as u32, 16]
            .into_iter()
            .flat_map(u32::to_be_bytes)
            .collect();
        store.extend(trailer(total));
        let mut out = HEADER_MAGIC.to_vec();
        out.extend(total.to_be_bytes());
        out.extend((store.len() as u32).to_be_bytes());
        out.extend(region);
        out.extend(index);
        out.extend(store);
        out
    }

    /// The entry with `tag`, if any.
    fn entry(&self, tag: u32) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }
}

/// The length of the data of an entry of `kind` and `count` at the start of `data`.
fn data_len(kind: u32, count: u32, data: &[u8]) -> Result<usize, Error> {
    let count = count as usize;
    let len = match kind {
        0 => 0,
        1 | 2 | BIN => count,
        3 => count * 2,
        4 => count * 4,
        5 => count * 8,
        6 | 8 | 9 => {
            let mut at = 0;
            for _ in 0..count {
                let end = data
                    .get(at..)
                    .and_then(|rest| rest.iter().position(|b| *b == 0))
                    .ok_or_else(|| bad("a string without its end"))?;
                at += end + 1;
            }
            at
        }
        _ => return Err(bad(format!("a datum of type {kind}"))),
    };
    if len > data.len() {
        return Err(bad("a datum is cut short"));
    }
    Ok(len)
}

/// The parts of a package.
#[derive(Debug)]
pub struct Parts<'a> {
    lead: &'a [u8],
    pub sig: Sig,
    /// The main header, from its magic to the end of its store.
    pub header: &'a [u8],
    pub payload: &'a [u8],
}

/// The length of a header at the start of `bytes`: intro, index and store.
fn header_len(bytes: &[u8]) -> Result<usize, Error> {
    if bytes.get(..8) != Some(&HEADER_MAGIC) {
        return Err(bad("a header without its magic"));
    }
    let (entries, store) = (be32(bytes, 8)? as usize, be32(bytes, 12)? as usize);
    let len = 16 + entries * 16 + store;
    if len > bytes.len() {
        return Err(bad("a header is cut short"));
    }
    Ok(len)
}

/// Reads the signature header at the start of `bytes`: the header and its length.
fn read_sig(bytes: &[u8]) -> Result<(Sig, usize), Error> {
    let len = header_len(bytes)?;
    let n = be32(bytes, 8)? as usize;
    let store = &bytes[16 + n * 16..len];
    let mut entries = Vec::new();
    for i in 0..n {
        let at = 16 + i * 16;
        let (tag, kind, offset, count) = (
            be32(bytes, at)?,
            be32(bytes, at + 4)?,
            be32(bytes, at + 8)? as usize,
            be32(bytes, at + 12)?,
        );
        if tag == REGION {
            continue;
        }
        let rest = store
            .get(offset..)
            .ok_or_else(|| bad("an offset outside the store"))?;
        let data = rest[..data_len(kind, count, rest)?].to_vec();
        entries.push(Entry {
            tag,
            kind,
            count,
            data,
        });
    }
    let sig = Sig { entries };
    if sig.write() != bytes[..len] {
        return Err(bad(
            "the signature header is not in the form `release rpm` writes",
        ));
    }
    Ok((sig, len))
}

/// Splits `bytes` into its parts.
///
/// # Errors
/// Bytes that are not a package, or one whose signature header is not in the canonical form.
pub fn split(bytes: &[u8]) -> Result<Parts<'_>, Error> {
    if bytes.get(..4) != Some(&LEAD_MAGIC) || bytes.len() < LEAD {
        return Err(bad("no lead"));
    }
    let (sig, len) = read_sig(&bytes[LEAD..])?;
    let start = LEAD + len.next_multiple_of(8);
    let rest = bytes.get(start..).unwrap_or_default();
    let header = header_len(rest)?;
    Ok(Parts {
        lead: &bytes[..LEAD],
        sig,
        header: &rest[..header],
        payload: &rest[header..],
    })
}

impl Parts<'_> {
    /// The package again, with the signature header as `self.sig` has it.
    pub fn assemble(&self) -> Vec<u8> {
        let sig = self.sig.write();
        let mut out = self.lead.to_vec();
        out.extend(&sig);
        out.resize(LEAD + sig.len().next_multiple_of(8), 0);
        out.extend(self.header);
        out.extend(self.payload);
        out
    }

    /// The byte range of the main header in the file, as `rpm:header-range` lists it.
    pub fn header_range(&self) -> (usize, usize) {
        let start = LEAD + self.sig.write().len().next_multiple_of(8);
        (start, start + self.header.len())
    }

    /// The signature entry (tag and data), if the package has one.
    pub fn signature(&self) -> Option<(u32, &[u8])> {
        [TAG_RSA, TAG_DSA]
            .into_iter()
            .find_map(|tag| self.sig.entry(tag).map(|e| (tag, e.data.as_slice())))
    }
}

/// `bytes` with the header-only signature `signature` (an OpenPGP signature packet of the main
/// header) as the entry `tag`.
///
/// # Errors
/// Bytes that are not a package, or one that has a signature already.
pub fn with_signature(bytes: &[u8], tag: u32, signature: &[u8]) -> Result<Vec<u8>, Error> {
    let mut parts = split(bytes)?;
    if parts.signature().is_some() {
        return Err(Error::Failed("the package is signed already".to_owned()));
    }
    parts.sig.entries.push(Entry {
        tag,
        kind: BIN,
        count: u32::try_from(signature.len()).map_err(|_| bad("a signature too long"))?,
        data: signature.to_vec(),
    });
    Ok(parts.assemble())
}

/// `bytes` without its header-only signatures: the package as it was before it was signed.
///
/// # Errors
/// Bytes that are not a package.
pub fn without_signatures(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut parts = split(bytes)?;
    parts
        .sig
        .entries
        .retain(|e| e.tag != TAG_RSA && e.tag != TAG_DSA);
    Ok(parts.assemble())
}

/// Who made an OpenPGP signature packet.
#[derive(Debug, PartialEq, Eq)]
pub struct Signer {
    /// The public-key algorithm (1 is RSA).
    pub algorithm: u8,
    /// The 16 hex digits of the issuer's key id, upper case.
    pub key_id: String,
}

/// The body of the signature packet (tag 2) `packet` is, in the forms GnuPG writes: an old-format
/// header with a one- or two-octet length, or a new-format one with a one- or two-octet length.
fn packet_body(packet: &[u8]) -> Result<&[u8], Error> {
    let bad = || Error::Failed("the signature is not an OpenPGP signature packet".to_owned());
    let (&first, rest) = packet.split_first().ok_or_else(bad)?;
    let one = || rest.first().map(|b| usize::from(*b)).ok_or_else(bad);
    let (len, skip) = match first {
        0x88 => (one()?, 1),
        0x89 => (
            rest.get(..2)
                .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
                .ok_or_else(bad)?,
            2,
        ),
        0xc2 => match one()? {
            n @ 0..=191 => (n, 1),
            n @ 192..=223 => (
                ((n - 192) << 8) + usize::from(*rest.get(1).ok_or_else(bad)?) + 192,
                2,
            ),
            _ => return Err(bad()),
        },
        _ => return Err(bad()),
    };
    rest.get(skip..skip + len).ok_or_else(bad)
}

/// The subpackets of one area of a version 4 signature: `(type, data)`. Lengths of one or two
/// octets, which is what a signature of this size has.
fn subpackets(mut area: &[u8]) -> Result<Vec<(u8, &[u8])>, Error> {
    let bad = || Error::Failed("a signature subpacket is not in a form this reads".to_owned());
    let mut out = Vec::new();
    while let Some(&first) = area.first() {
        let (start, len) = match first {
            0..=191 => (1, usize::from(first)),
            192..=254 => (
                2,
                ((usize::from(first) - 192) << 8)
                    + usize::from(*area.get(1).ok_or_else(bad)?)
                    + 192,
            ),
            255 => return Err(bad()),
        };
        let body = area.get(start..start + len).ok_or_else(bad)?;
        let (kind, data) = body.split_first().ok_or_else(bad)?;
        out.push((kind & 0x7f, data));
        area = &area[start + len..];
    }
    Ok(out)
}

/// The algorithm and issuer of a version 4 signature packet.
///
/// # Errors
/// Bytes that are not a signature packet of version 4, or one that names no issuer.
pub fn signer(packet: &[u8]) -> Result<Signer, Error> {
    let body = packet_body(packet)?;
    let bad = |why: &str| Error::Failed(format!("the signature: {why}"));
    if body.first() != Some(&4) || body.len() < 6 {
        return Err(bad("not a version 4 signature"));
    }
    let area = |at: usize| -> Result<(&[u8], usize), Error> {
        let len = body
            .get(at..at + 2)
            .map(|b| usize::from(u16::from_be_bytes([b[0], b[1]])))
            .ok_or_else(|| bad("cut short"))?;
        let data = body
            .get(at + 2..at + 2 + len)
            .ok_or_else(|| bad("cut short"))?;
        Ok((data, at + 2 + len))
    };
    let (hashed, next) = area(4)?;
    let (unhashed, _) = area(next)?;
    let mut key_id = None;
    for (kind, data) in subpackets(hashed)?.into_iter().chain(subpackets(unhashed)?) {
        match (kind, data.len()) {
            (33, 21) => key_id = Some(hex::encode_upper(&data[13..21])),
            (16, 8) if key_id.is_none() => key_id = Some(hex::encode_upper(data)),
            _ => {}
        }
    }
    Ok(Signer {
        algorithm: body[2],
        key_id: key_id.ok_or_else(|| bad("names no issuer"))?,
    })
}

/// The signature tag for the public-key algorithm of a signature.
///
/// # Errors
/// An algorithm no RPM reader takes in a header signature.
pub fn tag_of(algorithm: u8) -> Result<u32, Error> {
    match algorithm {
        1 => Ok(TAG_RSA),
        17 | 19 | 22 => Ok(TAG_DSA),
        other => Err(Error::Failed(format!(
            "the signature uses the public-key algorithm {other}, which RPM does not take in a header signature"
        ))),
    }
}

#[cfg(test)]
mod tests;
