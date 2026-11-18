//! The write cases: claims, create-only, identical writes, concurrent writers and streamed
//! writes; and the helpers every case writes and reads with.

use std::time::{Duration, SystemTime};

use bytes::Bytes;
use futures::future::join_all;
use futures::{StreamExt as _, stream};
use objstore_s3::{
    BodyStream, ByteRange, KeyError, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError,
};

use crate::violation::{Claim, failed, refused, same_bytes, same_len};
use crate::{Call, Case, IdenticalPut, Refusal, Violation};

/// The writers of [`create_race`].
const WRITERS: usize = 8;

/// The key `<prefix><name>`.
pub(crate) fn key(case: Case, prefix: &KeyPrefix, name: &str) -> Result<ObjectKey, Violation> {
    prefix.key(name).map_err(too_long(case))
}

/// The prefix `<prefix><name>`.
pub(crate) fn child(case: Case, prefix: &KeyPrefix, name: &str) -> Result<KeyPrefix, Violation> {
    prefix.child(name).map_err(too_long(case))
}

/// The violation of a key or prefix of `case` that the rules refuse (it is too long).
fn too_long(case: Case) -> impl FnOnce(KeyError) -> Violation {
    move |error| Violation::Key { case, error }
}

/// `len` bytes of a pattern of its own for each `seed`: two seeds differ in the first byte.
pub(crate) fn body(seed: u8, len: usize) -> Bytes {
    (0..len)
        .map(|i| {
            ((i % 251) as u8)
                .wrapping_mul(31)
                .wrapping_add(seed.wrapping_mul(97))
        })
        .collect()
}

/// `bytes` in pieces of `size` (the last one shorter).
pub(crate) fn pieces(bytes: &Bytes, size: usize) -> BodyStream {
    let bytes = bytes.clone();
    let len = bytes.len();
    let all: Vec<Result<Bytes, ObjectStoreError>> = (0..len)
        .step_by(size)
        .map(|at| Ok(bytes.slice(at..len.min(at + size))))
        .collect();
    stream::iter(all).boxed()
}

/// Writes `bytes` under `key`, which must succeed.
pub(crate) async fn put<S: ObjectStore>(
    case: Case,
    store: &S,
    key: &ObjectKey,
    bytes: Bytes,
) -> Result<(), Violation> {
    store
        .put_new(key, bytes, None)
        .await
        .map_err(failed(case, Call::PutNew, key))
}

/// Checks that `key` holds exactly `want` (not empty): a read of one byte more than its length
/// gives `want` and nothing after it.
pub(crate) async fn holds<S: ObjectStore>(
    case: Case,
    store: &S,
    key: &ObjectKey,
    want: &[u8],
) -> Result<(), Violation> {
    for range in span(0, want.len() as u64 + 1) {
        let got = store
            .get_range(key, range)
            .await
            .map_err(failed(case, Call::GetRange, key))?;
        same_bytes(case, key, range, want, &got)?;
    }
    Ok(())
}

/// The range `[offset, offset + len)`, once: every range the suite reads has `len` above zero
/// and an end that fits in `u64`, so this is never empty, and a read over it needs no `unwrap`.
pub(crate) fn span(offset: u64, len: u64) -> impl Iterator<Item = ByteRange> {
    ByteRange::new(offset, len).into_iter()
}

/// Checks that HEAD finds no object under `key`.
pub(crate) async fn absent<S: ObjectStore>(
    case: Case,
    store: &S,
    key: &ObjectKey,
) -> Result<(), Violation> {
    let got = store.len(key).await.map_err(failed(case, Call::Len, key))?;
    same_len(case, key, None, got)
}

/// Checks that HEAD gives `want` bytes under `key`.
pub(crate) async fn sized<S: ObjectStore>(
    case: Case,
    store: &S,
    key: &ObjectKey,
    want: u64,
) -> Result<(), Violation> {
    let got = store.len(key).await.map_err(failed(case, Call::Len, key))?;
    same_len(case, key, Some(want), got)
}

/// The store claims conditional writes and not Object Lock, and refuses a retention date.
pub(crate) async fn claims<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Result<(), Violation> {
    let case = Case::Claims;
    let claimed = store.capabilities();
    if !claimed.conditional_put {
        return Err(Violation::Claim {
            case,
            claim: Claim::NoConditionalPut,
        });
    }
    if claimed.object_lock {
        return Err(Violation::Claim {
            case,
            claim: Claim::ObjectLock,
        });
    }
    let k = key(case, prefix, "retained")?;
    // A retention a long way ahead (four thousand million seconds after the epoch).
    let until = SystemTime::UNIX_EPOCH + Duration::from_secs(4_000_000_000);
    let answer = store.put_new(&k, body(1, 16), Some(until)).await;
    refused(case, Call::PutNew, &k, Refusal::Unsupported, answer)?;
    absent(case, store, &k).await
}

/// Different bytes under a used key are refused, whole or streamed, and the key keeps its bytes.
pub(crate) async fn create_only<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
) -> Result<(), Violation> {
    let case = Case::CreateOnly;
    let k = key(case, prefix, "object")?;
    let first = body(1, 64);
    put(case, store, &k, first.clone()).await?;
    for other in [body(2, 64), body(1, 65), body(1, 63)] {
        let again = store.put_new(&k, other.clone(), None).await;
        refused(case, Call::PutNew, &k, Refusal::AlreadyExists, again)?;
        holds(case, store, &k, &first).await?;
        let len = other.len() as u64;
        let streamed = store.put_stream(&k, len, pieces(&other, 16)).await;
        refused(case, Call::PutStream, &k, Refusal::AlreadyExists, streamed)?;
        holds(case, store, &k, &first).await?;
    }
    sized(case, store, &k, 64).await
}

/// `Ok` if `answer` is what `identical` says a write of the bytes a key holds gets.
fn answered(
    case: Case,
    call: Call,
    key: &ObjectKey,
    identical: IdenticalPut,
    answer: Result<(), ObjectStoreError>,
) -> Result<(), Violation> {
    match identical {
        IdenticalPut::Accepted => answer.map_err(failed(case, call, key)),
        IdenticalPut::Refused => refused(case, call, key, Refusal::AlreadyExists, answer),
    }
}

/// A write of the bytes the key holds is answered as `identical` says, and the bytes stay.
pub(crate) async fn identical_put<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    identical: IdenticalPut,
) -> Result<(), Violation> {
    let case = Case::IdenticalPut;
    let k = key(case, prefix, "object")?;
    let first = body(3, 100);
    put(case, store, &k, first.clone()).await?;
    let again = store.put_new(&k, first.clone(), None).await;
    answered(case, Call::PutNew, &k, identical, again)?;
    holds(case, store, &k, &first).await?;
    let streamed = store.put_stream(&k, 100, pieces(&first, 9)).await;
    answered(case, Call::PutStream, &k, identical, streamed)?;
    holds(case, store, &k, &first).await?;
    sized(case, store, &k, 100).await
}

/// Writes each of `bodies` under `key` at once and checks the answers and the bytes the key then
/// holds. A writer that lost is refused as `AlreadyExists`, or answered `409`
/// (`ConditionalRequestConflict`, which S3 may give the writers of one key that race and which
/// does not say whether the write landed). At most `want` writers succeed, and at least `want`
/// succeed or were answered `409`. The key holds the bytes of a writer that succeeded or, if none
/// did, of one that was answered `409`.
async fn race<S: ObjectStore>(
    store: &S,
    key: &ObjectKey,
    bodies: &[Bytes],
    want: usize,
) -> Result<(), Violation> {
    let case = Case::CreateRace;
    let writes = bodies.iter().map(|b| store.put_new(key, b.clone(), None));
    let answers = join_all(writes).await;
    let mut created = Vec::new();
    let mut conflicted = Vec::new();
    for (answer, body) in answers.into_iter().zip(bodies) {
        match answer {
            Ok(()) => created.push(body),
            Err(ObjectStoreError::Service { status: 409, .. }) => conflicted.push(body),
            refusal => refused(case, Call::PutNew, key, Refusal::AlreadyExists, refusal)?,
        }
    }
    if created.len() > want || created.len() + conflicted.len() < want {
        return Err(Violation::Race {
            case,
            key: key.clone(),
            writers: bodies.len(),
            created: created.len(),
            conflicted: conflicted.len(),
            want,
        });
    }
    let candidates = if created.is_empty() {
        conflicted
    } else {
        created
    };
    holds_one_of(case, store, key, &candidates).await
}

/// Checks that `key` holds exactly one of `candidates` (bodies of one length, at least one).
async fn holds_one_of<S: ObjectStore>(
    case: Case,
    store: &S,
    key: &ObjectKey,
    candidates: &[&Bytes],
) -> Result<(), Violation> {
    let first = candidates.first().copied().cloned().unwrap_or_default();
    for range in span(0, first.len() as u64 + 1) {
        let got = store
            .get_range(key, range)
            .await
            .map_err(failed(case, Call::GetRange, key))?;
        if !candidates.iter().any(|c| **c == got) {
            same_bytes(case, key, range, &first, &got)?;
        }
    }
    Ok(())
}

/// Of concurrent writers of different bytes exactly one succeeds, and every read gives its bytes;
/// of concurrent writers of the same bytes, all succeed or one, as `identical` says.
pub(crate) async fn create_race<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
    identical: IdenticalPut,
) -> Result<(), Violation> {
    let case = Case::CreateRace;
    let different: Vec<Bytes> = (1..=WRITERS as u8).map(|seed| body(seed, 256)).collect();
    race(store, &key(case, prefix, "different")?, &different, 1).await?;
    let same = vec![body(9, 256); WRITERS];
    let want = match identical {
        IdenticalPut::Accepted => WRITERS,
        IdenticalPut::Refused => 1,
    };
    race(store, &key(case, prefix, "same")?, &same, want).await
}

/// A streamed write stores its bytes; one whose body ends short, runs long or fails leaves
/// nothing.
pub(crate) async fn streamed<S: ObjectStore>(
    store: &S,
    prefix: &KeyPrefix,
) -> Result<(), Violation> {
    let case = Case::Streamed;
    let whole = body(4, 1000);
    let k = key(case, prefix, "whole")?;
    store
        .put_stream(&k, 1000, pieces(&whole, 7))
        .await
        .map_err(failed(case, Call::PutStream, &k))?;
    holds(case, store, &k, &whole).await?;
    sized(case, store, &k, 1000).await?;
    let failing = pieces(&whole.slice(..500), 7).chain(stream::once(async {
        Err(ObjectStoreError::Body(
            "the conformance suite's body failed after 500 bytes".to_owned(),
        ))
    }));
    let broken = [
        ("short", 1000, pieces(&whole.slice(..999), 7)),
        ("long", 999, pieces(&whole, 7)),
        ("failing", 1000, failing.boxed()),
    ];
    for (name, len, body) in broken {
        let k = key(case, prefix, name)?;
        let answer = store.put_stream(&k, len, body).await;
        refused(case, Call::PutStream, &k, Refusal::Body, answer)?;
        absent(case, store, &k).await?;
        for first in span(0, 1) {
            let read = store.get_range(&k, first).await;
            refused(case, Call::GetRange, &k, Refusal::NotFound, read)?;
        }
    }
    Ok(())
}
