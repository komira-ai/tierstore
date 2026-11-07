//! The listing of the proxy where markers may be in force (`docs/design/put-without-lock.md`, 5.8):
//! the keys of the hot and the cold tier as one, without the keys of the cold tier that a marker
//! says are deleted.
//!
//! A page is read in rounds. A round reads the hot page and a page of marker names in parallel,
//! then the cold page, merges them (hot wins), and for each key that only the cold page lists and
//! the marker names list too, reads its marker: `gone` means the key is dropped unless the hot tier
//! has it by now (a HEAD of the hot tier; found, it is listed from the hot tier's answer), no
//! marker (it vanished since the page) means the cold tier's HEAD decides, and any other marker
//! keeps the key. A key the marker names do not list is kept, if the marker names reach it: the
//! names are read again, one more page at a time and at most `list_marker_pages` times, until they
//! reach the last merged key. A round whose names still stop short of it keeps only the keys they
//! reach, and the next round starts after them.
//!
//! A page of the [`Backend`](crate::Backend) is the whole of `max_keys` keys unless the listing has
//! ended (a caller takes a short page for the last), so the rounds go on until the page is full or
//! no round has more: a round that dropped keys, or was cut short, is followed by another from
//! the last key it looked at. The design says a cut page is returned with a continuation token; a
//! page of this trait cannot say it is short and not the last, so the proxy continues itself, each
//! round being bounded as the design bounds a page.
//!
//! The reads after the cold page (more marker names, the markers of cold-only keys) are the
//! cold-then-marker pair of I9. They are dropped and the round read again, at most `read_retries`
//! more times, when they ended more than `read_window` after the cold page began; then the listing
//! is [`Error::Unavailable`].
//!
//! A delimiter is the front's: it folds the keys this listing gives into common prefixes
//! (`server/listing.rs`), so a prefix can be made only of keys this listing has kept. A prefix
//! made of deleted keys that the cold tier still lists is therefore never produced, and the
//! design's probe of such prefixes (`list_prefix_probe`) is not needed.

use std::collections::{BTreeMap, BTreeSet};

use futures::future::try_join_all;

use super::Inner;
use super::listing::data_page;
use crate::deadline::millis;
use crate::entry::found;
use crate::{Error, Listed, Markers, Meta};

/// What a round of a listing came to.
struct Round {
    /// The keys of the round that the page keeps, in key order.
    keep: Vec<Listed>,
    /// Where the next round starts: the last key the round looked at; none if the listing has
    /// ended.
    resume: Option<String>,
}

/// The listing of a key that the hot tier gave, from its HEAD.
fn listed(key: String, meta: Meta) -> Listed {
    Listed {
        key,
        size: meta.size,
        modified: meta.modified,
        tag: meta.tag,
    }
}

impl Inner {
    /// The page of at most `max_keys` keys after `start_after`: full, unless the listing ended.
    pub(super) async fn list_marked(
        &self,
        markers: &Markers,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let mut page: Vec<Listed> = Vec::new();
        let mut after = start_after.map(str::to_owned);
        while page.len() < max_keys {
            let want = max_keys - page.len();
            let round = self
                .list_round(markers, prefix, after.as_deref(), want)
                .await?;
            page.extend(round.keep);
            match round.resume {
                Some(last) => after = Some(last),
                None => break,
            }
        }
        Ok(page)
    }

    /// One round for `want` keys, read again while its reads after the cold page are too late.
    async fn list_round(
        &self,
        markers: &Markers,
        prefix: &str,
        after: Option<&str>,
        want: usize,
    ) -> Result<Round, Error> {
        let config = &self.config.mark_reads;
        let rounds = config.read_retries.saturating_add(1);
        for _ in 0..rounds {
            if let Some(round) = self.try_round(markers, prefix, after, want).await? {
                return Ok(round);
            }
        }
        let why = format!(
            "the cold page of a listing of {prefix:?} and the markers read after it were further \
             apart than read_window ({} ms), {rounds} times",
            millis(config.read_window)
        );
        Err(Error::Unavailable(why))
    }

    /// One round, or none if the reads after the cold page took longer than `read_window`.
    async fn try_round(
        &self,
        markers: &Markers,
        prefix: &str,
        after: Option<&str>,
        want: usize,
    ) -> Result<Option<Round>, Error> {
        let config = &self.config.mark_reads;
        // Wave 1: the hot page and the marker names, together. Wave 2: the cold page, after both.
        let hot = data_page(self.hot.as_ref(), prefix, after, want);
        let (hot, first) = futures::join!(hot, markers.names(prefix, after, want));
        let (hot, first) = (hot?, first?);
        let cold_began = self.config.mono.now();
        let cold = data_page(self.cold.as_ref(), prefix, after, want).await?;

        // Hot wins: the merged page is the first `want` keys of the two, each marked with the tier
        // it is listed from.
        let mut merged: BTreeMap<String, (Listed, bool)> = BTreeMap::new();
        for (object, from_hot) in hot
            .into_iter()
            .map(|o| (o, true))
            .chain(cold.into_iter().map(|o| (o, false)))
        {
            merged
                .entry(object.key.clone())
                .or_insert((object, from_hot));
        }
        let mut merged: Vec<(Listed, bool)> = merged.into_values().take(want).collect();
        let full = merged.len() >= want;
        let last = merged.last().map(|(object, _)| object.key.clone());

        // The marker names reach `covered`; none means the marker listing has ended.
        let reach = |names: &[String]| names.last().filter(|_| names.len() >= want).cloned();
        let mut covered = reach(&first);
        let mut names: BTreeSet<String> = first.into_iter().collect();
        let mut extended = false;
        for _ in 0..config.list_marker_pages {
            let Some(from) = covered.as_deref() else {
                break;
            };
            if last.as_deref().is_none_or(|last| from >= last) {
                break;
            }
            let more = markers.names(prefix, Some(from), want).await?;
            extended = true;
            covered = reach(&more);
            names.extend(more);
        }
        let cut =
            covered.filter(|covered| last.as_deref().is_some_and(|last| covered.as_str() < last));
        if let Some(covered) = &cut {
            merged.retain(|(object, _)| object.key <= *covered);
        }

        // Step 3: a key only the cold page lists and a marker name lists too has its marker read.
        let needs_marker =
            |object: &Listed, from_hot: bool| !from_hot && names.contains(&object.key);
        let reads = merged
            .iter()
            .filter(|(object, from_hot)| needs_marker(object, *from_hot))
            .count();
        let decided = merged.into_iter().map(|(object, from_hot)| async move {
            if needs_marker(&object, from_hot) {
                self.decide_cold_only(markers, object).await
            } else {
                Ok(Some(object))
            }
        });
        let decided = try_join_all(decided).await?;
        let window = millis(config.read_window);
        let late = self.config.mono.now().saturating_sub(cold_began) > window;
        if (extended || reads > 0) && late {
            return Ok(None);
        }
        let resume = match cut {
            Some(covered) => Some(covered),
            None if full => last,
            None => None,
        };
        let keep = decided.into_iter().flatten().collect();
        Ok(Some(Round { keep, resume }))
    }

    /// Whether the key `object`, listed by the cold tier alone and named by a marker, is listed:
    /// the marker says (see the module).
    async fn decide_cold_only(
        &self,
        markers: &Markers,
        object: Listed,
    ) -> Result<Option<Listed>, Error> {
        let key = object.key.clone();
        let Some(seen) = markers.read(&key).await? else {
            // The marker vanished since the page was read: the cold tier decides.
            return Ok(found(self.cold.head(&key).await)?.map(|_| object));
        };
        if !seen.marker.gone {
            return Ok(Some(object));
        }
        // A put may have landed since the hot page was read.
        Ok(found(self.hot.head(&key).await)?.map(|meta| listed(key, meta)))
    }
}

#[cfg(test)]
#[path = "mark_listing_tests.rs"]
mod tests;
