//! `L_live`: the bytes of silo that the CAS service still needs, by the CAS service's own rule.
//!
//! The CAS service keeps no reference count. A blob lives at the first location it was written
//! to (a later pack that holds the same digest is a copy nobody reads), and a pack is alive
//! while it holds the first location of at least one blob. So walking the packs in the order they
//! were written (`modified`, then bucket, then key) and noting which digest each pack is the
//! first to hold finds every pack that holds nothing the others do not. A pack of no blobs holds
//! no first location and is dead. Large blobs and tree lists are not packed and count whole.
//!
//! The blobs of a pack come from the footers file (a scan of every pack's footer, written by
//! another verb); the minimal format this module reads is
//!
//! ```text
//! P TAB bucket TAB key TAB status TAB n [TAB error]      then n lines:
//! B TAB <64 hex digits> TAB size
//! T TAB bucket TAB key TAB json | not-json | get-error   (a tree list's check)
//! ```
//!
//! where the status `ok` says the blobs of the `P` line are the pack's.

use std::collections::{BTreeMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::kinds::{Kind, classify};
use crate::listing::Object;

use super::ratio;

/// The status of a pack whose footer was read and whose blobs are valid.
pub const STATUS_OK: &str = "ok";

/// What a footer scan says about one pack.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PackRecord {
    /// `ok`, or why the footer could not be used.
    pub status: String,
    /// Each blob's digest and size, in the pack's order.
    pub blobs: Vec<([u8; 32], u64)>,
}

/// The check of one tree list's first byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// It is JSON, as a `tree_manifest` should be.
    Json,
    /// It is not.
    NotJson,
    /// It could not be read.
    GetError,
}

impl Verdict {
    /// The verdict a footers file names.
    fn parse(text: &str) -> Option<Self> {
        match text {
            "json" => Some(Self::Json),
            "not-json" => Some(Self::NotJson),
            "get-error" => Some(Self::GetError),
            _ => None,
        }
    }
}

/// A footers file, read: packs and tree-list checks by `(bucket, key)`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Footers {
    /// The packs.
    pub packs: BTreeMap<(String, String), PackRecord>,
    /// The tree lists.
    pub trees: BTreeMap<(String, String), Verdict>,
}

/// The pack whose blob lines are being read.
#[derive(Default)]
struct Open {
    key: (String, String),
    record: PackRecord,
    remaining: usize,
}

impl Open {
    /// An error unless every blob line of the open pack has been read.
    fn require_closed(&self, path: &Path, line: usize) -> Result<(), Error> {
        if self.remaining > 0 {
            return Err(Error::parse(
                path,
                line,
                format!(
                    "pack {} {} is short of {} blob lines",
                    self.key.0, self.key.1, self.remaining
                ),
            ));
        }
        Ok(())
    }

    /// Moves the pack into `footers` once all its blob lines are read.
    fn close_if_done(&mut self, footers: &mut Footers) {
        if self.remaining == 0 {
            footers.packs.insert(
                std::mem::take(&mut self.key),
                std::mem::take(&mut self.record),
            );
        }
    }
}

/// The number `text` (`what` names it in an error).
fn number<T: FromStr>(what: &str, text: &str, path: &Path, line: usize) -> Result<T, Error> {
    text.parse::<T>()
        .map_err(|_| Error::parse(path, line, format!("{what} {text:?} is not a number")))
}

/// Parses a footers file's lines (`path` names it in an error).
///
/// # Errors
/// [`Error::Io`] when a line cannot be read; [`Error::Parse`] for a line that is not one, a
/// pack that is listed twice or whose blob lines are missing, and a blob line outside a pack.
pub fn parse_footers(reader: impl BufRead, path: &Path) -> Result<Footers, Error> {
    let mut footers = Footers::default();
    let mut open = Open::default();
    let mut last = 0;
    for (i, text) in reader.lines().enumerate() {
        let line = i + 1;
        last = line;
        let text = text.map_err(|e| Error::io("read", path, e))?;
        if text.is_empty() {
            continue;
        }
        let fields: Vec<&str> = text.split('\t').collect();
        match fields.as_slice() {
            ["B", digest, size] => {
                if open.remaining == 0 {
                    return Err(Error::parse(path, line, "a blob line outside a pack"));
                }
                let mut d = [0u8; 32];
                hex::decode_to_slice(digest, &mut d)
                    .map_err(|e| Error::parse(path, line, format!("digest {digest:?}: {e}")))?;
                let size = number("size", size, path, line)?;
                open.record.blobs.push((d, size));
                open.remaining -= 1;
                open.close_if_done(&mut footers);
            }
            ["P", bucket, key, status, n, rest @ ..] if rest.len() <= 1 => {
                open.require_closed(path, line)?;
                let k = ((*bucket).to_owned(), (*key).to_owned());
                if footers.packs.contains_key(&k) {
                    return Err(Error::parse(
                        path,
                        line,
                        format!("pack {bucket} {key} is listed twice"),
                    ));
                }
                open.remaining = number("blob count", n, path, line)?;
                open.key = k;
                open.record = PackRecord {
                    status: (*status).to_owned(),
                    blobs: Vec::new(),
                };
                open.close_if_done(&mut footers);
            }
            ["T", bucket, key, verdict] => {
                open.require_closed(path, line)?;
                let Some(verdict) = Verdict::parse(verdict) else {
                    return Err(Error::parse(
                        path,
                        line,
                        format!("tree check {verdict:?} is not json, not-json or get-error"),
                    ));
                };
                let k = ((*bucket).to_owned(), (*key).to_owned());
                if footers.trees.insert(k, verdict).is_some() {
                    return Err(Error::parse(
                        path,
                        line,
                        format!("tree list {bucket} {key} is listed twice"),
                    ));
                }
            }
            _ => {
                return Err(Error::parse(
                    path,
                    line,
                    "not a P, B or T line with the fields its kind has",
                ));
            }
        }
    }
    open.require_closed(path, last)?;
    Ok(footers)
}

impl Footers {
    /// Adds the records of a second scan. A pack or tree list that only `other` holds is added;
    /// one that both hold keeps this scan's record unless only `other`'s was read (a pack `ok`
    /// over another status, a tree list's verdict over `get-error`), so a pack read in either
    /// scan counts as read.
    pub fn merge(&mut self, other: Footers) {
        for (key, record) in other.packs {
            match self.packs.get_mut(&key) {
                None => {
                    self.packs.insert(key, record);
                }
                Some(have) if have.status != STATUS_OK && record.status == STATUS_OK => {
                    *have = record;
                }
                Some(_) => {}
            }
        }
        for (key, verdict) in other.trees {
            match self.trees.get_mut(&key) {
                None => {
                    self.trees.insert(key, verdict);
                }
                Some(have) if *have == Verdict::GetError => *have = verdict,
                Some(_) => {}
            }
        }
    }
}

/// Reads the footers files `paths` and merges them in order ([`Footers::merge`]); `None` when
/// there are none.
///
/// # Errors
/// As [`read_footers`], for the first file that cannot be read.
pub fn read_all_footers(paths: &[PathBuf]) -> Result<Option<Footers>, Error> {
    let mut all: Option<Footers> = None;
    for path in paths {
        let footers = read_footers(path)?;
        match &mut all {
            None => all = Some(footers),
            Some(have) => have.merge(footers),
        }
    }
    Ok(all)
}

/// Reads the footers file `path`.
///
/// # Errors
/// [`Error::Io`] when it cannot be opened; otherwise as [`parse_footers`].
pub fn read_footers(path: &Path) -> Result<Footers, Error> {
    let file = std::fs::File::open(path).map_err(|e| Error::io("open", path, e))?;
    parse_footers(std::io::BufReader::new(file), path)
}

/// The tree-list check of the footers file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeCheck {
    /// Tree lists checked.
    pub checked: u64,
    /// Of them, JSON.
    pub json: u64,
    /// Of them, not JSON.
    pub not_json: u64,
    /// Of them, unreadable.
    pub errors: u64,
}

/// Counts the verdicts of `footers` (all zero without a footers file).
#[must_use]
pub fn tree_check(footers: Option<&Footers>) -> TreeCheck {
    let mut check = TreeCheck::default();
    for verdict in footers.iter().flat_map(|f| f.trees.values()) {
        check.checked += 1;
        match verdict {
            Verdict::Json => check.json += 1,
            Verdict::NotJson => check.not_json += 1,
            Verdict::GetError => check.errors += 1,
        }
    }
    check
}

/// Packs with one footer status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusCount {
    /// The status.
    pub status: String,
    /// Packs with it.
    pub packs: u64,
}

/// What the walk of the packs found.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Live {
    /// Packs in the listing.
    pub packs_listed: u64,
    /// Their bytes.
    pub pack_bytes: u64,
    /// Listed packs with no footer in the footers file (all of them without one).
    pub packs_missing: u64,
    /// Listed packs whose footer is in the file.
    pub packs_scanned: u64,
    /// Scanned packs with the status `ok`.
    pub packs_ok: u64,
    /// Scanned packs with another status.
    pub packs_bad: u64,
    /// The bytes of the bad packs: their footers were not read, so they bound `L_live` as the
    /// packs without a footer do.
    pub bad_bytes: u64,
    /// Scanned packs per status, in status order.
    pub statuses: Vec<StatusCount>,
    /// Blobs in the ok packs, repeats included.
    pub blobs: u64,
    /// Different digests among them.
    pub distinct_digests: u64,
    /// The bytes of the different digests (each counted once).
    pub distinct_bytes: u64,
    /// Ok packs that hold the first location of a blob.
    pub live_packs: u64,
    /// Their bytes.
    pub live_bytes: u64,
    /// Ok packs that hold none.
    pub dead_packs: u64,
    /// Their bytes.
    pub dead_bytes: u64,
    /// `dead_bytes / (live_bytes + dead_bytes)`.
    pub dead_fraction: f64,
    /// The bytes of the large blobs (whole objects).
    pub large_blob_bytes: u64,
    /// The bytes of the tree lists.
    pub tree_list_bytes: u64,
    /// The bytes of the listed packs with no footer in the file.
    #[serde(default)]
    pub missing_bytes: u64,
    /// The largest pack without a footer or with a bad one: no blob of an unread pack is
    /// larger.
    #[serde(default)]
    pub missing_max: u64,
    /// Live packs whose first-located blobs are all no larger than the largest unread pack
    /// written before them: that pack could have held those blobs first (an unread pack written
    /// later cannot), so these could be dead.
    #[serde(default)]
    pub uncertain_packs: u64,
    /// Their bytes.
    #[serde(default)]
    pub uncertain_bytes: u64,
    /// `L_live`, at most: live pack bytes, the unread (missing and bad) packs' bytes, large blob
    /// bytes and tree list bytes. Absent without a footers file while the listing has packs.
    pub l_live: Option<u64>,
    /// The most bytes of the uncertain packs that the unread packs could make dead (a
    /// fractional knapsack over the unread bytes; `could_be_dead`). At most `uncertain_bytes`.
    #[serde(default)]
    pub could_be_dead_bytes: u64,
    /// `L_live`, at least: the live packs without [`Live::could_be_dead_bytes`], large blob
    /// bytes and tree list bytes; the unread packs are left out.
    #[serde(default)]
    pub l_live_low: Option<u64>,
}

/// A pack found dead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeadPack {
    /// The bucket.
    pub bucket: String,
    /// The key.
    pub key: String,
}

/// Where a pack falls in the order the packs were written: `modified`, then bucket, then key.
fn order(o: &Object) -> (u64, &str, &str) {
    (o.modified_ms, &o.bucket, &o.key)
}

/// The walk of the packs of `listing` against `footers`; and the dead packs, in walk order.
#[must_use]
pub fn analyze(listing: &[Object], footers: Option<&Footers>) -> (Live, Vec<DeadPack>) {
    let mut live = Live::default();
    let mut statuses: BTreeMap<&str, u64> = BTreeMap::new();
    let mut ok: Vec<(&Object, &PackRecord)> = Vec::new();
    let mut unread: Vec<&Object> = Vec::new();
    for o in listing {
        match classify(&o.key, o.size) {
            Kind::LargeBlob => live.large_blob_bytes += o.size,
            Kind::TreeList => live.tree_list_bytes += o.size,
            Kind::Pack => {
                live.packs_listed += 1;
                live.pack_bytes += o.size;
                let record = footers.and_then(|f| f.packs.get(&(o.bucket.clone(), o.key.clone())));
                match record {
                    None => {
                        live.packs_missing += 1;
                        live.missing_bytes += o.size;
                        live.missing_max = live.missing_max.max(o.size);
                        unread.push(o);
                    }
                    Some(r) => {
                        *statuses.entry(r.status.as_str()).or_default() += 1;
                        if r.status == STATUS_OK {
                            ok.push((o, r));
                        } else {
                            live.bad_bytes += o.size;
                            live.missing_max = live.missing_max.max(o.size);
                            unread.push(o);
                        }
                    }
                }
            }
            Kind::Other => {}
        }
    }
    ok.sort_by(|(a, _), (b, _)| order(a).cmp(&order(b)));
    unread.sort_by(|a, b| order(a).cmp(&order(b)));
    // The unread packs written before the pack being walked (or in its millisecond), and the
    // largest of them: only such a pack can have held one of its blobs first. Write order is
    // the objects' modified times, as the first-location rule takes it.
    let mut unread_before = unread.iter().peekable();
    let mut largest_unread_before: Option<u64> = None;
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let mut dead = Vec::new();
    // The uncertain packs: their bytes, and the bytes of their first-located blobs.
    let mut uncertain: Vec<(u64, u64)> = Vec::new();
    for (pack, record) in &ok {
        // Written in the same millisecond is not known to be after: it counts as before.
        while let Some(u) = unread_before.next_if(|u| u.modified_ms <= pack.modified_ms) {
            largest_unread_before = largest_unread_before.max(Some(u.size));
        }
        let mut first = false;
        let mut largest_first = 0;
        let mut first_bytes = 0;
        for (digest, size) in &record.blobs {
            live.blobs += 1;
            if seen.insert(*digest) {
                first = true;
                largest_first = largest_first.max(*size);
                first_bytes += size;
                live.distinct_bytes += size;
            }
        }
        if first {
            live.live_packs += 1;
            live.live_bytes += pack.size;
            if largest_unread_before.is_some_and(|max| largest_first <= max) {
                live.uncertain_packs += 1;
                live.uncertain_bytes += pack.size;
                uncertain.push((pack.size, first_bytes));
            }
        } else {
            live.dead_packs += 1;
            live.dead_bytes += pack.size;
            dead.push(DeadPack {
                bucket: pack.bucket.clone(),
                key: pack.key.clone(),
            });
        }
    }
    live.packs_ok = ok.len() as u64;
    live.packs_scanned = live.packs_listed - live.packs_missing;
    live.packs_bad = live.packs_scanned - live.packs_ok;
    live.distinct_digests = seen.len() as u64;
    live.dead_fraction = ratio(live.dead_bytes, live.live_bytes + live.dead_bytes);
    live.statuses = statuses
        .into_iter()
        .map(|(status, packs)| StatusCount {
            status: status.to_owned(),
            packs,
        })
        .collect();
    let unpacked = live.large_blob_bytes + live.tree_list_bytes;
    let known = footers.is_some() || live.packs_listed == 0;
    live.l_live = known.then_some(live.live_bytes + live.missing_bytes + live.bad_bytes + unpacked);
    live.could_be_dead_bytes =
        could_be_dead(uncertain, live.missing_bytes + live.bad_bytes).min(live.uncertain_bytes);
    live.l_live_low = known.then_some(live.live_bytes - live.could_be_dead_bytes + unpacked);
    (live, dead)
}

/// The most pack bytes that unread packs of `unread_bytes` in all could make dead, given the
/// uncertain packs as `(pack bytes, bytes of its first-located blobs)`.
///
/// An uncertain pack is dead only if every one of its first-located blobs was first in an unread
/// pack instead. Each such blob is a distinct digest, so it takes its own bytes in an unread
/// pack: the first-located bytes taken away from all the packs made dead together are at most
/// `unread_bytes`. The pack bytes that budget can buy are at most what the fractional knapsack
/// buys (packs in order of pack bytes per first-located byte, the last one in part), rounded up,
/// so the bound never claims more live bytes than there are.
fn could_be_dead(mut uncertain: Vec<(u64, u64)>, unread_bytes: u64) -> u64 {
    // Most pack bytes per first-located byte first: `a/b > c/d` is `a*d > c*b`, and a pack
    // whose first-located blobs are empty costs nothing.
    uncertain.sort_by(|&(a, b), &(c, d)| {
        (u128::from(c) * u128::from(b)).cmp(&(u128::from(a) * u128::from(d)))
    });
    let mut budget = unread_bytes;
    let mut bought = 0u64;
    for (pack, first) in uncertain {
        if first <= budget {
            budget -= first;
            bought += pack;
        } else {
            // The part of this pack the rest of the budget buys, rounded up.
            let part = (u128::from(pack) * u128::from(budget)).div_ceil(u128::from(first));
            bought += u64::try_from(part).unwrap_or(pack);
            break;
        }
    }
    bought
}

#[cfg(test)]
#[path = "live_tests.rs"]
mod tests;
