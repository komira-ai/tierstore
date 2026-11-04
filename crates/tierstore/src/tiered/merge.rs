//! The merge of the listings of the hot tier and the cold tier.

use std::collections::BTreeMap;

use crate::Listed;

/// The objects of both listings, each key once (the hot tier's entry where both have it), in
/// key order, at most `max_keys` of them.
pub(super) fn merge(in_hot: Vec<Listed>, in_cold: Vec<Listed>, max_keys: usize) -> Vec<Listed> {
    let mut merged: BTreeMap<String, Listed> = BTreeMap::new();
    for object in in_hot.into_iter().chain(in_cold) {
        merged.entry(object.key.clone()).or_insert(object);
    }
    merged.into_values().take(max_keys).collect()
}
