//! The forks of a test process, run one at a time together with the unmounts that need their
//! mount to be idle.
//!
//! From its fork until its exec, a child holds a copy of every descriptor of the process. Only
//! the exec closes the close-on-exec ones. So a file that one test of a binary has open in its
//! FUSE mount at that moment is also open in a child that another test of the same binary is
//! starting. If the first test then closes the file and unmounts (not lazily) before that child
//! has exec'd, it finds its mount busy: `umount2(2)` fails with `EBUSY`, and `fusermount3 -u`
//! exits 1.
//!
//! [`std::process::Command::spawn`] and [`std::process::Command::output`] return only once the
//! child has exec'd (or failed to). So a test binary that runs every fork through
//! [`one_at_a_time`] (every spawn, and every mount or unmount that may go through
//! `fusermount3`), and every unmount that must succeed, never has such an unmount meet a child
//! that still holds a descriptor of its mount.

use std::sync::{Mutex, PoisonError};

/// Held while one caller of [`one_at_a_time`] runs.
static FORKS: Mutex<()> = Mutex::new(());

/// Runs `f` while no other caller of this function in this process runs, and returns what it
/// returns. A caller that panicked inside it does not block the next one.
pub fn one_at_a_time<T>(f: impl FnOnce() -> T) -> T {
    let _one = FORKS.lock().unwrap_or_else(PoisonError::into_inner);
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::TryLockError;

    fn held() -> bool {
        matches!(FORKS.try_lock(), Err(TryLockError::WouldBlock))
    }

    /// One test, so no other test of this crate takes the lock between its checks.
    #[test]
    fn the_lock_is_held_while_a_caller_runs_and_a_panicked_caller_leaves_it_free() {
        assert!(!held());
        assert!(one_at_a_time(held));
        assert!(!held());
        let panicked = std::thread::spawn(|| one_at_a_time(|| panic!("a caller that panics")))
            .join()
            .unwrap_err();
        assert_eq!(
            panicked.downcast_ref::<&str>(),
            Some(&"a caller that panics")
        );
        assert!(FORKS.is_poisoned());
        assert!(!held());
        assert_eq!(one_at_a_time(|| (held(), 7)), (true, 7));
    }
}
