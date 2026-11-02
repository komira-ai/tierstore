//! What the proxy and the reconciler know of a reservation: the marked object under a key in the
//! cold tier that holds the key for a put that was acknowledged and not yet copied (see
//! [`Backend::put_reservation`](crate::Backend::put_reservation)).

/// The size of a reservation, in bytes. A listing shows no mark, so a cold object of exactly this
/// size that the hot listing does not show is asked about with a HEAD, which does: only the mark
/// says whether it is a reservation, never the size.
pub(crate) const RESERVATION_LEN: u64 = 56;

/// How a reservation body starts.
const MAGIC: &str = "tierstore-reservation/1\n";

/// The body of a reservation that proxy `owner` writes: the magic, 16 hex digits of a random
/// nonce and 16 hex digits of the FNV-1a hash of the owner's id, 56 bytes in all (the size
/// [`RESERVATION_LEN`] that a reader of the cold tier counts on).
///
/// The nonce makes the body, and so the content tag, of every reservation different, so that a
/// conditional write or delete on one reservation's tag can never hit a later reservation of the
/// key. The owner's hash is there so that a later release can tell whose reservation it is
/// without a longer body: the id itself does not fit in the 56 bytes.
pub(crate) fn body(owner: &str) -> bytes::Bytes {
    let nonce = crate::lease::new_nonce();
    let hash = owner.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    let text = format!("{MAGIC}{}{hash:016x}", &nonce[..16]);
    bytes::Bytes::from(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The body is as long as the reader of the cold tier expects, starts with the magic and
    /// differs from one call to the next; the owner's hash is the same for one owner and not
    /// for two.
    #[test]
    fn a_reservation_body_is_56_bytes_with_a_nonce_and_the_hash_of_its_owner() {
        let (first, second) = (body("p1"), body("p1"));
        assert_eq!(first.len() as u64, RESERVATION_LEN);
        assert!(first.starts_with(MAGIC.as_bytes()));
        assert_ne!(first, second);
        assert_eq!(first[40..], second[40..]);
        assert_ne!(first[40..], body("p2")[40..]);
    }
}
