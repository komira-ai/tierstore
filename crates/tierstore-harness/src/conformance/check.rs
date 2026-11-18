//! The checks the cases make. Each panics with a message that starts `conformance:` and names the
//! contract that broke, so a failing implementation says which clause it violates.
//!
//! # Panics
//!
//! Every function here panics when its check fails; that is their purpose.

use std::fmt::{Debug, Display};

use tierstore_fetch_api::entry::EntryError;

/// Panics with `what` unless `cond`.
pub fn ensure(cond: bool, what: impl Display) {
    if !cond {
        panic!("conformance: {what}");
    }
}

/// Panics with `what` and both values unless `got == want`.
pub fn ensure_eq<T: PartialEq + Debug>(got: T, want: T, what: impl Display) {
    if got != want {
        panic!("conformance: {what}: got {got:?}, want {want:?}");
    }
}

/// The value of `result`; panics with `what` and the error when it is an error.
pub fn expect_ok<T>(result: Result<T, EntryError>, what: impl Display) -> T {
    match result {
        Ok(v) => v,
        Err(e) => panic!("conformance: {what}: unexpected error: {e}"),
    }
}

/// Checks that `result` is an error that `kind` accepts; panics with `what` when the result is a
/// success or when `kind` refuses the error.
pub fn expect_err<T>(
    result: Result<T, EntryError>,
    kind: impl FnOnce(&EntryError) -> bool,
    what: impl Display,
) {
    match result {
        Ok(_) => panic!("conformance: {what}: expected an error, got a success"),
        Err(e) if !kind(&e) => panic!("conformance: {what}: wrong error: {e:?}"),
        Err(_) => {}
    }
}
