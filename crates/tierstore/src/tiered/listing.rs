//! The listings of the proxy: the objects of the hot and the cold tier as one, each key once.

use std::collections::{BTreeMap, BTreeSet};

use futures::{StreamExt, TryStreamExt, stream};

use super::{Inner, is_data};
use crate::reservation::RESERVATION_LEN;
use crate::{Backend, Error, Listed};

impl Inner {
    /// The object of a cold listing if it is one: a reservation (a marked object of
    /// [`RESERVATION_LEN`] bytes) and an object that is gone are left out. Only an object of that
    /// size that the hot listing does not show is asked about, at every listing: a listing gives no
    /// identity of the version (a key, a size and a time of last write do not tell a real object
    /// from a reservation that replaced it within the same time), so no answer is kept. A key the
    /// hot tier shows is shown whatever the cold tier holds under it.
    async fn visible(
        &self,
        object: Listed,
        in_hot: &BTreeSet<&str>,
    ) -> Result<Option<Listed>, Error> {
        let may_hold = object.size == RESERVATION_LEN && !in_hot.contains(object.key.as_str());
        if !may_hold {
            return Ok(Some(object));
        }
        match self.cold.head(&object.key).await {
            Ok(meta) if meta.reservation => Ok(None),
            Ok(_) => Ok(Some(object)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The objects of a cold listing that are objects (see `visible`), in the order given, asking
    /// about [`ProxyConfig::listing_heads`] of them at a time.
    async fn visible_all(
        &self,
        listed: Vec<Listed>,
        in_hot: &[Listed],
    ) -> Result<Vec<Listed>, Error> {
        let shown: BTreeSet<&str> = in_hot.iter().map(|object| object.key.as_str()).collect();
        let checks = stream::iter(listed).map(|object| self.visible(object, &shown));
        let checked: Vec<Option<Listed>> = checks
            .buffered(self.config.listing_heads.max(1))
            .try_collect()
            .await?;
        Ok(checked.into_iter().flatten().collect())
    }

    /// The first `max_keys` objects of the listing of the cold tier after `start_after` that are
    /// objects: pages of the cold tier are read until enough are left or the listing ends, so
    /// that a page the filter cut short is made up, and a short page is the last.
    async fn cold_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
        in_hot: &[Listed],
    ) -> Result<Vec<Listed>, Error> {
        let mut found: Vec<Listed> = Vec::new();
        let mut after = start_after.map(str::to_owned);
        loop {
            let page = self.cold.list_page(prefix, after.as_deref(), max_keys);
            let page = page.await?;
            let full_page = page.len() >= max_keys;
            after = page.last().map(|object| object.key.clone());
            let data: Vec<Listed> = page.into_iter().filter(is_data).collect();
            found.extend(self.visible_all(data, in_hot).await?);
            if !full_page || found.len() >= max_keys {
                break;
            }
        }
        found.truncate(max_keys);
        Ok(found)
    }

    pub(super) async fn list_objects(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let mut in_hot = self.hot.list(prefix).await?;
        in_hot.retain(is_data);
        let mut in_cold = self.cold.list(prefix).await?;
        in_cold.retain(is_data);
        let in_cold = self.visible_all(in_cold, &in_hot).await?;
        Ok(merge(in_hot, in_cold, usize::MAX))
    }

    pub(super) async fn list_objects_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let in_hot = data_page(self.hot.as_ref(), prefix, start_after, max_keys).await?;
        let in_cold = self
            .cold_page(prefix, start_after, max_keys, &in_hot)
            .await?;
        Ok(merge(in_hot, in_cold, max_keys))
    }
}

/// The first `max_keys` objects of the listing of `hot` after `start_after`, without the
/// objects under the reserved prefixes: pages of the hot tier are read until enough objects
/// are left or the listing ends.
async fn data_page(
    hot: &dyn Backend,
    prefix: &str,
    start_after: Option<&str>,
    max_keys: usize,
) -> Result<Vec<Listed>, Error> {
    let mut found: Vec<Listed> = Vec::new();
    let mut after = start_after.map(str::to_owned);
    loop {
        let page = hot.list_page(prefix, after.as_deref(), max_keys).await?;
        let full_page = page.len() >= max_keys;
        after = page.last().map(|object| object.key.clone());
        found.extend(page.into_iter().filter(is_data));
        if !full_page || found.len() >= max_keys {
            break;
        }
    }
    found.truncate(max_keys);
    Ok(found)
}

/// The objects of both listings, each key once (the hot tier's entry where both have it), in
/// key order, at most `max_keys` of them.
fn merge(in_hot: Vec<Listed>, in_cold: Vec<Listed>, max_keys: usize) -> Vec<Listed> {
    let mut merged: BTreeMap<String, Listed> = BTreeMap::new();
    for object in in_hot.into_iter().chain(in_cold) {
        merged.entry(object.key.clone()).or_insert(object);
    }
    merged.into_values().take(max_keys).collect()
}
