//! The read and delete cases: ranges, HEAD and DELETE.

use objstore_s3::{ByteRange, KeyPrefix, ObjectStore};

use crate::cases::{absent, body, holds, key, put, sized};
use crate::cases_list::listed;
use crate::violation::{failed, refused, same_bytes};
use crate::{Call, Case, Refusal, Violation};

/// The length of the object of [`range`].
const OBJECT: u64 = 1000;

/// Ranges `(offset, len)` of an object of [`OBJECT`] bytes that it holds bytes of: whole, the
/// first and the last byte, inner spans, and spans that run past the end and are cut there.
const WINDOWS: [(u64, u64); 8] = [
    (0, OBJECT),
    (0, 1),
    (OBJECT - 1, 1),
    (1, OBJECT - 2),
    (123, 456),
    (500, 500),
    (500, OBJECT),
    (OBJECT - 1, 4096),
];

/// Ranges that start at or past the end of the object: `InvalidRange`.
const PAST: [(u64, u64); 3] = [(OBJECT, 1), (OBJECT, 10), (5 * OBJECT, 1)];

/// Reads are byte-exact; a range past the end is cut; one that starts at or past the end is
/// `InvalidRange`; an absent key is `NotFound`.
pub(crate) async fn range<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Result<(), Violation> {
    let case = Case::Range;
    let k = key(case, prefix, "object")?;
    let whole = body(5, OBJECT as usize);
    put(case, store, &k, whole.clone()).await?;
    let windows = WINDOWS.iter().filter_map(|&(o, l)| ByteRange::new(o, l));
    for range in windows {
        let got = store
            .get_range(&k, range)
            .await
            .map_err(failed(case, Call::GetRange, &k))?;
        let end = (range.offset() + range.size()).min(OBJECT);
        let want = &whole[range.offset() as usize..end as usize];
        same_bytes(case, &k, range, want, &got)?;
    }
    for range in PAST.iter().filter_map(|&(o, l)| ByteRange::new(o, l)) {
        let read = store.get_range(&k, range).await;
        refused(case, Call::GetRange, &k, Refusal::InvalidRange, read)?;
    }
    let missing = key(case, prefix, "absent")?;
    for range in WINDOWS[..1]
        .iter()
        .filter_map(|&(o, l)| ByteRange::new(o, l))
    {
        let read = store.get_range(&missing, range).await;
        refused(case, Call::GetRange, &missing, Refusal::NotFound, read)?;
    }
    Ok(())
}

/// HEAD gives the length of an empty, a one-byte and a larger object, and `None` for an absent
/// key; a read of the empty object starts at its end (`InvalidRange`).
pub(crate) async fn head<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Result<(), Violation> {
    let case = Case::Head;
    for (name, len) in [("empty", 0), ("one", 1), ("some", 4096)] {
        let k = key(case, prefix, name)?;
        put(case, store, &k, body(6, len)).await?;
        sized(case, store, &k, len as u64).await?;
    }
    let empty = key(case, prefix, "empty")?;
    for range in WINDOWS[1..2]
        .iter()
        .filter_map(|&(o, l)| ByteRange::new(o, l))
    {
        let read = store.get_range(&empty, range).await;
        refused(case, Call::GetRange, &empty, Refusal::InvalidRange, read)?;
    }
    absent(case, store, &key(case, prefix, "absent")?).await
}

/// Deleting an absent key succeeds; a deleted key is gone from reads, HEAD and LIST; a second
/// delete succeeds; the key can then be written with other bytes.
pub(crate) async fn delete<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Result<(), Violation> {
    let case = Case::Delete;
    let k = key(case, prefix, "object")?;
    let gone = |k| failed(case, Call::Delete, k);
    store.delete(&k).await.map_err(gone(&k))?;
    put(case, store, &k, body(7, 100)).await?;
    store.delete(&k).await.map_err(gone(&k))?;
    absent(case, store, &k).await?;
    for range in WINDOWS[..1]
        .iter()
        .filter_map(|&(o, l)| ByteRange::new(o, l))
    {
        let read = store.get_range(&k, range).await;
        refused(case, Call::GetRange, &k, Refusal::NotFound, read)?;
    }
    listed(case, store, prefix, Vec::new()).await?;
    store.delete(&k).await.map_err(gone(&k))?;
    let again = body(8, 50);
    put(case, store, &k, again.clone()).await?;
    holds(case, store, &k, &again).await?;
    sized(case, store, &k, 50).await
}
