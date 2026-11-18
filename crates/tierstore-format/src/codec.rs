//! Little-endian writing and bounds-checked reading, shared by every format of the crate.

use crate::FormatError;

/// A growing byte buffer that integers and byte strings are appended to, little-endian.
#[derive(Debug, Default)]
pub(crate) struct Enc {
    buf: Vec<u8>,
}

impl Enc {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub(crate) fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub(crate) fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub(crate) fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    pub(crate) fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(v);
        self
    }

    /// Zero bytes up to the next multiple of `align` (a power of two) of the length.
    pub(crate) fn pad_to(&mut self, align: usize) -> &mut Self {
        let len = self.buf.len().next_multiple_of(align);
        self.buf.resize(len, 0);
        self
    }

    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// A cursor over bytes being decoded as the format `what`; every read that runs past the end is
/// [`FormatError::Short`].
#[derive(Debug)]
pub(crate) struct Dec<'a> {
    buf: &'a [u8],
    pos: usize,
    what: &'static str,
}

impl<'a> Dec<'a> {
    pub(crate) fn new(buf: &'a [u8], what: &'static str) -> Self {
        Self { buf, pos: 0, what }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        let rest = &self.buf[self.pos..];
        if rest.len() < n {
            return Err(FormatError::Short {
                what: self.what,
                need: (self.pos + n) as u64,
                have: self.buf.len() as u64,
            });
        }
        self.pos += n;
        Ok(&rest[..n])
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], FormatError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.array::<1>()?[0])
    }

    pub(crate) fn u16(&mut self) -> Result<u16, FormatError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// Skips zero padding up to the next multiple of `align` of the position.
    pub(crate) fn skip_to(&mut self, align: usize) -> Result<(), FormatError> {
        let to = self.pos.next_multiple_of(align);
        self.take(to - self.pos).map(|_| ())
    }

    pub(crate) fn pos(&self) -> usize {
        self.pos
    }
}

/// Checks a magic number read from the bytes.
pub(crate) fn expect_magic(what: &'static str, found: u32, magic: u32) -> Result<(), FormatError> {
    if found == magic {
        Ok(())
    } else {
        Err(FormatError::BadMagic { what, found })
    }
}

/// Checks that the crc32c of `covered` equals `stored`.
pub(crate) fn expect_crc(
    what: &'static str,
    covered: &[u8],
    stored: u32,
) -> Result<(), FormatError> {
    let computed = crate::crc32c(covered);
    if computed == stored {
        Ok(())
    } else {
        Err(FormatError::BadCrc {
            what,
            stored,
            computed,
        })
    }
}

/// The suffix of `buf` that a self-locating trailer of `len` bytes occupies, or `Short`.
pub(crate) fn tail<'a>(
    buf: &'a [u8],
    len: u64,
    what: &'static str,
) -> Result<&'a [u8], FormatError> {
    let have = buf.len() as u64;
    if len > have {
        return Err(FormatError::Short {
            what,
            need: len,
            have,
        });
    }
    // `len <= have`, so the subtraction stays within the slice.
    Ok(&buf[(have - len) as usize..])
}
