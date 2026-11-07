//! What the tests of the verbs share: arguments, a tier with a clock, and the objects the verbs read.

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::Arc;

use bytes::Bytes;

use crate::entry::{Entry, EntryOp, EntryState, encode_entry, entry_key};
use crate::format::{Format, format_body};
use crate::journal::FORMAT_KEY;
use crate::{Backend, MemoryBackend, TestClock};

/// The arguments of a process: the program name, then `words`.
pub(super) fn args(words: &[&str]) -> Vec<OsString> {
    std::iter::once("tierstore")
        .chain(words.iter().copied())
        .map(OsString::from)
        .collect()
}

/// The text of an exit status, which has no equality of its own.
pub(super) fn status(code: u8) -> String {
    format!("{:?}", ExitCode::from(code))
}

/// A memory tier whose times come from a clock the test holds.
pub(super) fn tier() -> (Arc<MemoryBackend>, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new());
    clock.set(100_000);
    (
        Arc::new(MemoryBackend::with_clock(Arc::clone(&clock))),
        clock,
    )
}

/// A registration of a release with a heartbeat, at protocol `proto`.
pub(super) async fn register(hot: &dyn Backend, id: &str, proto: u32) {
    let body = format!(r#"{{"proto":{proto},"boot":"b","hb":1}}"#);
    hot.put(&format!("_tiering/proxies/{id}"), Bytes::from(body))
        .await
        .unwrap();
}

/// A registration of a release with a heartbeat that reports the `phase` it acts on and the
/// number of `writes` in flight.
pub(super) async fn register_reporting(
    hot: &dyn Backend,
    id: &str,
    proto: u32,
    phase: &str,
    writes: u64,
) {
    let body = format!(
        r#"{{"proto":{proto},"boot":"b","hb":1,"phase":"{phase}","writes_in_flight":{writes}}}"#
    );
    let key = format!("_tiering/proxies/{id}");
    hot.delete(&key).await.unwrap();
    hot.put(&key, Bytes::from(body)).await.unwrap();
}

/// A registration of a release before the heartbeat: an empty object.
pub(super) async fn register_old(hot: &dyn Backend, id: &str) {
    hot.put(&format!("_tiering/proxies/{id}"), Bytes::from_static(b"{}"))
        .await
        .unwrap();
}

/// Writes the format object.
pub(super) async fn set_format(hot: &dyn Backend, format: &Format) {
    hot.put(FORMAT_KEY, format_body(format)).await.unwrap();
}

/// The format object as it is stored.
pub(super) async fn stored_format(hot: &dyn Backend) -> Option<Format> {
    match hot.get(FORMAT_KEY, None).await {
        Ok(body) => Some(serde_json::from_slice(&body).unwrap()),
        Err(crate::Error::NotFound(_)) => None,
        Err(other) => panic!("reading the format: {other}"),
    }
}

/// Writes the entry of `key`.
pub(super) async fn put_entry(
    hot: &dyn Backend,
    key: &str,
    op: EntryOp,
    st: EntryState,
    owner: Option<&str>,
) {
    let entry = Entry {
        v: 1,
        key: key.to_owned(),
        op,
        st,
        owner: owner.map(str::to_owned),
        nonce: "n1".to_owned(),
        created_ms: 7,
    };
    hot.put(&entry_key(key), encode_entry(&entry))
        .await
        .unwrap();
}
