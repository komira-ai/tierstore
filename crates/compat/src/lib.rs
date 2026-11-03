//! The fixtures that the cross-release tests in `tests/` share: bodies and the chunks they are
//! streamed in. The tests hold this branch of `tierstore` and the release before it (the
//! dev-dependency `tierstore_prev`) to the same data: each must read, and replace, what the other
//! wrote, in a directory and on an S3 service.

/// The size of one chunk of a streamed body in the tests.
pub const CHUNK: usize = 1 << 16;

/// A body of `len` bytes that differ from place to place, and from one `seed` to another, so that
/// bytes read from the wrong place or the wrong object are not equal to the right ones.
#[must_use]
pub fn patterned(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| ((at % 251) as u8).wrapping_add(seed))
        .collect()
}

/// A body of `len` bytes, as [`patterned`] makes it, as the chunks of at most [`CHUNK`] bytes it is
/// streamed in.
#[must_use]
pub fn chunked(seed: u8, len: usize) -> Vec<Vec<u8>> {
    patterned(seed, len)
        .chunks(CHUNK)
        .map(<[u8]>::to_vec)
        .collect()
}
