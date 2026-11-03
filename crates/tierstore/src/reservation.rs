//! What the proxy and the reconciler know of a reservation: the marked object under a key in the
//! cold tier that holds the key for a put that was acknowledged and not yet copied (see
//! [`Backend::put_reservation`](crate::Backend::put_reservation)).

/// The size of a reservation, in bytes. A listing shows no mark, so a cold object of exactly this
/// size that the hot listing does not show is asked about with a HEAD, which does: only the mark
/// says whether it is a reservation, never the size.
pub(crate) const RESERVATION_LEN: u64 = 56;
