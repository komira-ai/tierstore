//! The server's epoch (5.4): a random non-zero `u64`, drawn once per server.

use std::io;

use tierstore_fetch_api::wire::Epoch;

use crate::MountServerError;

/// A random epoch for a new server, from the system's random source. Never 0: a mount stamps its
/// requests with epoch 0 before its first handshake, so 0 is never a server's.
///
/// # Errors
/// [`MountServerError::Random`] when the random source fails.
pub fn random_epoch() -> Result<Epoch, MountServerError> {
    nonzero(getrandom_u64).map_err(MountServerError::Random)
}

/// Eight bytes of the system's random source.
fn getrandom_u64() -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    let mut filled = 0;
    while filled < bytes.len() {
        filled +=
            rustix::rand::getrandom(&mut bytes[filled..], rustix::rand::GetRandomFlags::empty())?;
    }
    Ok(u64::from_le_bytes(bytes))
}

/// The first non-zero value `next` gives, as an epoch.
fn nonzero(mut next: impl FnMut() -> io::Result<u64>) -> io::Result<Epoch> {
    loop {
        let v = next()?;
        if v != 0 {
            return Ok(Epoch(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_draw_is_drawn_again() {
        let mut draws = [0, 0, 7].into_iter();
        let epoch = nonzero(|| Ok(draws.next().unwrap())).unwrap();
        assert_eq!(epoch, Epoch(7));
        assert_eq!(draws.next(), None);
    }

    #[test]
    fn a_failing_source_is_the_error() {
        let err = nonzero(|| Err(io::Error::from(io::ErrorKind::Unsupported))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        let wrapped = MountServerError::Random(err);
        assert_eq!(wrapped.to_string(), "drawing the server epoch: unsupported");
    }

    #[test]
    fn the_system_source_gives_a_nonzero_epoch() {
        assert_ne!(random_epoch().unwrap(), Epoch(0));
    }
}
