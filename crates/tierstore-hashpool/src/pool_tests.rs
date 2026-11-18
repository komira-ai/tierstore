//! The pool's own errors and debug text (the threads are tested in `tests/`).
//!
//! Catches: a refused thread reported without its index, the pool's size or the system's reason.

use std::num::NonZeroUsize;

use super::refused;

#[test]
fn a_refused_thread_names_its_index_the_pool_size_and_the_reason() {
    let e = refused(
        2,
        NonZeroUsize::new(3).unwrap(),
        std::io::Error::other("out of threads"),
    );
    assert_eq!(e.to_string(), "start hash thread 2 of 3: out of threads");
    assert!(std::error::Error::source(&e).is_some());
}
