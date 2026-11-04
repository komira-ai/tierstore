//! The listings of the proxy: the objects of the hot and the cold tier as one, each key once.

use std::collections::BTreeMap;

use super::{Inner, is_data};
use crate::{Backend, Error, Listed};

impl Inner {
    pub(super) async fn list_objects(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let mut in_hot = self.hot.list(prefix).await?;
        in_hot.retain(is_data);
        let mut in_cold = self.cold.list(prefix).await?;
        in_cold.retain(is_data);
        Ok(merge(in_hot, in_cold, usize::MAX))
    }

    pub(super) async fn list_objects_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let in_hot = data_page(self.hot.as_ref(), prefix, start_after, max_keys).await?;
        let in_cold = data_page(self.cold.as_ref(), prefix, start_after, max_keys).await?;
        Ok(merge(in_hot, in_cold, max_keys))
    }
}

/// The first `max_keys` objects of the listing of `tier` after `start_after`, without the
/// objects under the reserved prefixes: pages of the tier are read until enough objects
/// are left or the listing ends.
async fn data_page(
    tier: &dyn Backend,
    prefix: &str,
    start_after: Option<&str>,
    max_keys: usize,
) -> Result<Vec<Listed>, Error> {
    let mut found: Vec<Listed> = Vec::new();
    let mut after = start_after.map(str::to_owned);
    loop {
        let page = tier.list_page(prefix, after.as_deref(), max_keys).await?;
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
