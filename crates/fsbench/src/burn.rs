//! A CPU burner (`fsbench burn`): one thread that spins until its standard input closes, so the
//! node's CPUs are busy while the readers run, as they are under a build.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Spins until `done` says so after a round (it is asked with the rounds so far); returns the
/// rounds.
fn spin_until(done: impl Fn(u64) -> bool) -> u64 {
    let mut rounds = 0u64;
    loop {
        rounds = std::hint::black_box(rounds.wrapping_add(1));
        if done(rounds) {
            return rounds;
        }
    }
}

/// Spins until `input` ends (or fails); returns how many times it went round.
pub fn spin(mut input: impl Read + Send + 'static) -> u64 {
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let _ = std::io::copy(&mut input, &mut std::io::sink());
            stop.store(true, Ordering::Relaxed);
        })
    };
    let rounds = spin_until(|_| stop.load(Ordering::Relaxed));
    let _ = watcher.join();
    rounds
}

#[cfg(test)]
mod tests {
    use super::{spin, spin_until};

    /// The spin goes round until it is told to stop, and a burner whose input ends stops.
    #[test]
    fn a_burner_spins_until_it_is_told_to_stop() {
        assert_eq!(spin_until(|rounds| rounds == 1000), 1000);
        assert!(spin(std::io::empty()) >= 1);
    }
}
