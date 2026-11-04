//! Golden tests of every format the core stores.
//!
//! Each format has a fixture under `fixtures/`: the bytes a release has written. A test reads the
//! fixture with the current code and checks the value, and another has the current code write that
//! value and checks the bytes, so a field that is renamed, removed or given another meaning, or
//! a key that is renamed, fails here. A fixture is changed only by adding a field the readers may
//! ignore, as `docs/stored-formats.md` says; a changed fixture in a diff is the thing to review.

mod hook_records;
mod journal_objects;
mod local_cache_sidecar;
mod support;
mod tier_objects;
