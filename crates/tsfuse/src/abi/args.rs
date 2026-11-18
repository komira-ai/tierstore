//! The arguments of a request body, taken in order: fixed structs, NUL-terminated names and the
//! bytes that remain. A struct is copied out, so the body needs no alignment.

use zerocopy::FromBytes;

/// A cursor over the bytes of a request body.
pub(crate) struct Args<'a> {
    data: &'a [u8],
}

impl<'a> Args<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data }
    }

    /// The next struct; `None` (and nothing taken) if fewer bytes remain than it needs.
    pub(crate) fn fetch<T: FromBytes>(&mut self) -> Option<T> {
        let (value, rest) = T::read_from_prefix(self.data).ok()?;
        self.data = rest;
        Some(value)
    }

    /// The next name, without its NUL; `None` (and nothing taken) if no NUL remains.
    pub(crate) fn fetch_str(&mut self) -> Option<&'a [u8]> {
        let end = self.data.iter().position(|&b| b == 0)?;
        let name = &self.data[..end];
        self.data = &self.data[end + 1..];
        Some(name)
    }

    /// The next `n` bytes; `None` (and nothing taken) if fewer remain.
    pub(crate) fn fetch_bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let (bytes, rest) = self.data.split_at_checked(n)?;
        self.data = rest;
        Some(bytes)
    }
}

/// The tests of `src/ll/argument.rs` of fuser 0.18.0 (MIT; notice in `crates/tsfuse/FUSER-LICENSE.md`), ported: a fixed struct, a
/// NUL-terminated name, the rest bytes, a short buffer and a missing NUL.
#[cfg(test)]
mod tests {
    use zerocopy::{FromBytes, Immutable, KnownLayout};

    use super::Args;

    /// `foo\0bar\0ba`.
    const TEST_DATA: [u8; 10] = [0x66, 0x6f, 0x6f, 0x00, 0x62, 0x61, 0x72, 0x00, 0x62, 0x61];

    #[repr(C)]
    #[derive(FromBytes, KnownLayout, Immutable)]
    struct TestArgument {
        p1: u8,
        p2: u8,
        p3: u16,
    }

    /// That `it` has exactly `rest` left: no more bytes, and no fewer.
    fn assert_rest(it: &mut Args<'_>, rest: &[u8]) {
        assert!(it.fetch_bytes(rest.len() + 1).is_none());
        assert_eq!(it.fetch_bytes(rest.len()), Some(rest));
        assert_eq!(it.fetch_bytes(0), Some(&[][..]));
        assert!(it.fetch_bytes(1).is_none());
    }

    #[test]
    fn all_data() {
        let mut it = Args::new(&TEST_DATA);
        assert_eq!(it.fetch_str(), Some(&b"foo"[..]));
        assert_rest(&mut it, &[0x62, 0x61, 0x72, 0x00, 0x62, 0x61]);
    }

    #[test]
    fn generic_argument() {
        let mut it = Args::new(&TEST_DATA);
        let arg: TestArgument = it.fetch().unwrap();
        assert_eq!((arg.p1, arg.p2), (0x66, 0x6f));
        assert_eq!(arg.p3, u16::from_ne_bytes([0x6f, 0x00]));
        let arg: TestArgument = it.fetch().unwrap();
        assert_eq!((arg.p1, arg.p2), (0x62, 0x61));
        assert_eq!(arg.p3, u16::from_ne_bytes([0x72, 0x00]));
        assert_rest(&mut it, &[0x62, 0x61]);
    }

    #[test]
    fn string_argument() {
        let mut it = Args::new(&TEST_DATA);
        assert_eq!(it.fetch_str(), Some(&b"foo"[..]));
        assert_eq!(it.fetch_str(), Some(&b"bar"[..]));
        assert_rest(&mut it, &[0x62, 0x61]);
    }

    #[test]
    fn mixed_arguments() {
        let mut it = Args::new(&TEST_DATA);
        let arg: TestArgument = it.fetch().unwrap();
        assert_eq!((arg.p1, arg.p2), (0x66, 0x6f));
        assert_eq!(arg.p3, u16::from_ne_bytes([0x6f, 0x00]));
        assert_eq!(it.fetch_str(), Some(&b"bar"[..]));
        assert_rest(&mut it, &[0x62, 0x61]);
    }

    #[test]
    fn out_of_data() {
        let mut it = Args::new(&TEST_DATA);
        it.fetch::<u64>().unwrap();
        // Too short for a struct or a name: nothing is taken.
        assert!(it.fetch::<TestArgument>().is_none());
        assert!(it.fetch_str().is_none());
        assert_rest(&mut it, &[0x62, 0x61]);
        assert!(it.fetch::<u8>().is_none());
    }

    #[test]
    fn an_empty_name_is_a_name() {
        let mut it = Args::new(&[0, b'a', 0]);
        assert_eq!(it.fetch_str(), Some(&b""[..]));
        assert_eq!(it.fetch_str(), Some(&b"a"[..]));
        assert!(it.fetch_str().is_none());
    }
}
