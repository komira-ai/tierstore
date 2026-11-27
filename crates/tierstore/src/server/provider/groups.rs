//! What a preparation reads: the spans of its files, grouped into reads of one pack each.
//!
//! A version 2 list carries the ranges of the packs its files are in, merged by its writer across
//! small gaps ([`ListRange`]). The spans in one range are read as one group: one read of the
//! pack from the first span's start to the last one's end, each span checked against its own
//! digest as the bytes pass ([`TreeStore::fetch_parts`]). A group is cut at [`GROUP_MAX_BYTES`],
//! so that an open of a file late in a long range does not wait for all of it, and the groups
//! of a list are read [`fill_concurrency`] at a time. A span in no range (the ranges are a hint)
//! is read alone.
//!
//! [`ListRange`]: crate::ListRange
//! [`TreeStore::fetch_parts`]: crate::server::local_cache::trees::TreeStore::fetch_parts
//! [`fill_concurrency`]: crate::server::local_cache::trees::TreesConfig::fill_concurrency

use std::sync::Arc;

use crate::{ListRange, ListSpan, TreeDigest};

/// The most bytes one group reads, from its first span's start to its last one's end.
pub(super) const GROUP_MAX_BYTES: u64 = 8 << 20;

/// One read of a preparation: spans of one pack, each `(offset, size, sha256)`, sorted by offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Group {
    pub pack: Arc<str>,
    pub parts: Vec<(u64, u64, TreeDigest)>,
}

/// The groups of a version 2 list's `spans`: the spans in one of `ranges` (and of the same key)
/// grouped while a group spans at most [`GROUP_MAX_BYTES`], every other span alone; in the order
/// of the keys' numbers and the offsets. A span that several files have (the same key, offset,
/// size and digest) is read once.
pub(super) fn groups(
    keys: &[Arc<str>],
    spans: Vec<ListSpan>,
    ranges: impl Iterator<Item = ListRange>,
) -> Vec<Group> {
    grouped(keys, spans, ranges.collect())
}

/// [`groups`], made once for every kind of iterator of ranges.
fn grouped(keys: &[Arc<str>], mut spans: Vec<ListSpan>, mut ranges: Vec<ListRange>) -> Vec<Group> {
    spans.sort_unstable_by(|a, b| {
        (a.key, a.offset, a.size, a.sha256.as_bytes()).cmp(&(
            b.key,
            b.offset,
            b.size,
            b.sha256.as_bytes(),
        ))
    });
    spans.dedup();
    ranges.sort_unstable_by_key(|range| (range.key, range.offset));
    let mut groups: Vec<Group> = Vec::new();
    // The range and the first byte of the last group, if it is in a range.
    let mut open: Option<(usize, u64)> = None;
    let mut at = 0;
    for span in spans {
        let end = span.offset + span.size;
        let before = |range: &ListRange| {
            (range.key, range.offset.saturating_add(range.len)) <= (span.key, span.offset)
        };
        while at < ranges.len() && before(&ranges[at]) {
            at += 1;
        }
        let inside = ranges.get(at).is_some_and(|range| {
            range.key == span.key && range.offset <= span.offset && end <= range.offset + range.len
        });
        let part = (span.offset, span.size, span.sha256);
        let joins = open
            .is_some_and(|(range, start)| inside && range == at && end - start <= GROUP_MAX_BYTES);
        if joins {
            // A group is open, so there is a last one.
            let last = groups.len() - 1;
            groups[last].parts.push(part);
            continue;
        }
        open = inside.then_some((at, span.offset));
        groups.push(Group {
            pack: keys[span.key as usize].clone(),
            parts: vec![part],
        });
    }
    groups
}
