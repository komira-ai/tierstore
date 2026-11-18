//! The reconnect backoff (5.4): 50 ms doubling to 2 s, with jitter.
//!
//! The jitter is "equal jitter": a delay `d` of the doubling sequence is slept as `d/2` plus a
//! uniform draw from `0..=d - d/2`, so a delay is never above the cap and never below half the
//! step, and the mounts of a node that lost their server at the same moment do not reconnect in
//! one burst. The draws come from a xorshift64* generator seeded once per client (from the
//! system's random source, or by the caller), so a test sees a fixed sequence.

use std::io;

/// The backoff's state: the next step and the generator.
#[derive(Debug, Clone)]
pub(crate) struct Backoff {
    first_ms: u64,
    max_ms: u64,
    /// The delay of the next step before jitter.
    next_ms: u64,
    rng: u64,
}

impl Backoff {
    /// A backoff from `first_ms` doubling to `max_ms`, its jitter drawn from `seed`.
    pub(crate) fn new(first_ms: u64, max_ms: u64, seed: u64) -> Self {
        let first_ms = first_ms.min(max_ms);
        Self {
            first_ms,
            max_ms,
            next_ms: first_ms,
            // xorshift has one fixed point, 0.
            rng: if seed == 0 {
                0x9e37_79b9_7f4a_7c15
            } else {
                seed
            },
        }
    }

    /// The delay to sleep before the next attempt; the step after it doubles, up to the cap.
    pub(crate) fn next_delay(&mut self) -> u64 {
        let d = self.next_ms;
        self.next_ms = d.saturating_mul(2).min(self.max_ms);
        let half = d / 2;
        half + self.draw() % (d - half + 1)
    }

    /// Back to the first step: a handshake was accepted.
    pub(crate) fn reset(&mut self) {
        self.next_ms = self.first_ms;
    }

    fn draw(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

/// A seed from the system's random source.
pub(crate) fn random_seed() -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    let mut filled = 0;
    while filled < bytes.len() {
        filled +=
            rustix::rand::getrandom(&mut bytes[filled..], rustix::rand::GetRandomFlags::empty())?;
    }
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_to_the_cap_within_their_jitter_and_reset() {
        let mut b = Backoff::new(50, 2000, 7);
        let steps = [50, 100, 200, 400, 800, 1600, 2000, 2000];
        let delays: Vec<u64> = steps.iter().map(|_| b.next_delay()).collect();
        for (d, step) in delays.iter().zip(steps) {
            assert!(
                (step / 2..=step).contains(d),
                "{d} outside [{}, {step}]",
                step / 2
            );
        }
        // The jitter is drawn: the same seed gives the same delays, another seed others.
        let mut again = Backoff::new(50, 2000, 7);
        assert_eq!(
            steps.iter().map(|_| again.next_delay()).collect::<Vec<_>>(),
            delays
        );
        let mut other = Backoff::new(50, 2000, 8);
        assert_ne!(
            steps.iter().map(|_| other.next_delay()).collect::<Vec<_>>(),
            delays
        );
        b.reset();
        let d = b.next_delay();
        assert!((25..=50).contains(&d));
    }

    #[test]
    fn a_zero_seed_and_a_first_step_above_the_cap() {
        let mut b = Backoff::new(5000, 2000, 0);
        let d = b.next_delay();
        assert!((1000..=2000).contains(&d));
        let mut z = Backoff::new(0, 0, 0);
        assert_eq!(z.next_delay(), 0);
    }

    #[test]
    fn the_system_source_seeds() {
        // Two draws of 64 bits are equal with probability 2^-64.
        assert_ne!(random_seed().unwrap(), random_seed().unwrap());
    }
}
