//! The listing cases: the live keys of a prefix after PUTs and DELETEs ([`LiveSet`]), and pages.

use std::collections::BTreeMap;

use bytes::Bytes;
use futures::TryStreamExt as _;
use objstore_s3::{KeyPrefix, ListToken, ObjectInfo, ObjectKey, ObjectStore, PageSize, listing};

use crate::cases::{body, child, holds, key, put, sized};
use crate::violation::failed;
use crate::{Call, Case, Violation};

/// The keys [`LiveSet::seed`] writes, with the seed and length of each body. Their order by
/// bytes is not their order here; `k1`, `k10` and `k2` make a prefix that is a string prefix; one
/// is empty; one has a space and non-ASCII text, which a listing must give back as written.
const SEEDED: [(&str, u8, usize); 10] = [
    ("k2", 20, 2),
    ("a/b", 21, 3),
    ("a", 22, 1),
    ("k10", 23, 10),
    ("b", 24, 0),
    ("k1", 25, 11),
    ("c", 26, 4),
    ("d", 27, 5),
    ("e", 28, 6),
    ("x y/caf\u{e9}", 29, 7),
];

/// The live keys of a prefix after a known history of writes and deletes, so that a target can
/// check them again after it restarted its store (10.9: "a LIST of a prefix returns exactly the
/// live keys after PUTs and DELETEs, including across a node restart").
#[derive(Clone, Debug)]
pub struct LiveSet {
    prefix: KeyPrefix,
    live: BTreeMap<ObjectKey, Bytes>,
}

impl LiveSet {
    /// Writes the keys of the set under `prefix` (which should hold nothing), deletes `c` and `d`,
    /// writes `d` again with other bytes and `e` again with the same bytes after deleting it, and
    /// returns the live keys that leaves, having checked the listing once.
    ///
    /// # Errors
    /// The first [`Violation`] found, as [`Case::List`].
    pub async fn seed<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Result<Self, Violation> {
        let case = Case::List;
        let mut live = BTreeMap::new();
        for (name, seed, len) in SEEDED {
            let k = key(case, prefix, name)?;
            put(case, store, &k, body(seed, len)).await?;
            live.insert(k, body(seed, len));
        }
        for (name, again) in [
            ("c", None),
            ("d", Some(body(30, 9))),
            ("e", Some(body(28, 6))),
        ] {
            let k = key(case, prefix, name)?;
            store
                .delete(&k)
                .await
                .map_err(failed(case, Call::Delete, &k))?;
            live.remove(&k);
            if let Some(bytes) = again {
                put(case, store, &k, bytes.clone()).await?;
                live.insert(k, bytes);
            }
        }
        let set = Self {
            prefix: prefix.clone(),
            live,
        };
        set.check(store).await?;
        Ok(set)
    }

    /// Checks that the listing of the prefix is exactly the live keys and their sizes in key
    /// order, that each holds its bytes, and that a listing of a narrower prefix (`k1`) and of one
    /// that holds nothing give exactly their keys.
    ///
    /// # Errors
    /// The first [`Violation`] found, as [`Case::List`].
    pub async fn check<S: ObjectStore>(&self, store: &S) -> Result<(), Violation> {
        let case = Case::List;
        listed(case, store, &self.prefix, self.infos(&self.prefix)).await?;
        for (k, bytes) in &self.live {
            sized(case, store, k, bytes.len() as u64).await?;
            if !bytes.is_empty() {
                holds(case, store, k, bytes).await?;
            }
        }
        for name in ["k1", "none/"] {
            let narrower = child(case, &self.prefix, name)?;
            listed(case, store, &narrower, self.infos(&narrower)).await?;
        }
        Ok(())
    }

    /// The live keys under `prefix` and their sizes, in key order.
    fn infos(&self, prefix: &KeyPrefix) -> Vec<ObjectInfo> {
        let under = self.live.iter().filter(|(k, _)| prefix.matches(k));
        under
            .map(|(k, b)| ObjectInfo {
                key: k.clone(),
                size: b.len() as u64,
            })
            .collect()
    }
}

/// Every object under `prefix`, as the store lists it, a whole page at a time.
async fn all<S: ObjectStore>(
    case: Case,
    store: &S,
    prefix: &KeyPrefix,
    from: Option<ListToken>,
) -> Result<Vec<ObjectInfo>, Violation> {
    listing::objects(store, prefix, from, PageSize::MAX)
        .try_collect()
        .await
        .map_err(failed(case, Call::List, &format!("{:?}", prefix.as_str())))
}

/// `Ok` if the listing of `prefix` is exactly `want`.
pub(crate) async fn listed<S: ObjectStore>(
    case: Case,
    store: &S,
    prefix: &KeyPrefix,
    want: Vec<ObjectInfo>,
) -> Result<(), Violation> {
    let got = all(case, store, prefix, None).await?;
    same_listing(case, prefix, want, got)
}

/// `Ok` if `got` is `want`, else the violation.
fn same_listing(
    case: Case,
    prefix: &KeyPrefix,
    want: Vec<ObjectInfo>,
    got: Vec<ObjectInfo>,
) -> Result<(), Violation> {
    if got == want {
        return Ok(());
    }
    Err(Violation::Listing {
        case,
        prefix: prefix.clone(),
        want,
        got,
    })
}

/// The LIST case: [`LiveSet::seed`], which checks the listing once.
pub(crate) async fn list<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Result<(), Violation> {
    LiveSet::seed(store, prefix).await.map(drop)
}

/// The keys of [`pages`].
const PAGED: [&str; 5] = ["p0", "p1", "p2", "p3", "p4"];

/// The page sizes [`pages`] walks with.
const SIZES: [u16; 5] = [1, 2, 3, 5, 1000];

/// No page is longer than its `max_keys`; pages joined give the whole listing; a listing after a
/// key (present, absent, the last one) starts right after it.
pub(crate) async fn pages<S: ObjectStore>(store: &S, prefix: &KeyPrefix) -> Result<(), Violation> {
    let case = Case::Pages;
    let mut want = Vec::new();
    for (i, name) in PAGED.iter().enumerate() {
        let k = key(case, prefix, name)?;
        put(case, store, &k, body(40, i + 1)).await?;
        want.push(ObjectInfo {
            key: k,
            size: i as u64 + 1,
        });
    }
    for max in SIZES.iter().filter_map(|&n| PageSize::new(n)) {
        let got = walk(case, store, prefix, max).await?;
        same_listing(case, prefix, want.clone(), got)?;
    }
    for (after, from) in [("p1", 2), ("p1x", 2), ("p4", 5)] {
        let after = key(case, prefix, after)?;
        let got = all(case, store, prefix, Some(ListToken::after(&after))).await?;
        same_listing(case, prefix, want[from..].to_vec(), got)?;
    }
    Ok(())
}

/// The listing of `prefix` page by page with `max` keys a page, checking each page's length. A
/// store that never ends the listing is stopped after twice as many pages as there are keys, and
/// what it listed by then is not the listing.
async fn walk<S: ObjectStore>(
    case: Case,
    store: &S,
    prefix: &KeyPrefix,
    max: PageSize,
) -> Result<Vec<ObjectInfo>, Violation> {
    let mut got = Vec::new();
    let mut token: Option<ListToken> = None;
    for _ in 0..2 * PAGED.len() {
        let page = store
            .list(prefix, token.as_ref(), max)
            .await
            .map_err(failed(case, Call::List, &format!("{:?}", prefix.as_str())))?;
        if page.objects.len() > usize::from(max.get()) {
            return Err(Violation::Page {
                case,
                prefix: prefix.clone(),
                max_keys: max.get(),
                got: page.objects.len(),
            });
        }
        got.extend(page.objects);
        token = page.next;
        if token.is_none() {
            break;
        }
    }
    Ok(got)
}
