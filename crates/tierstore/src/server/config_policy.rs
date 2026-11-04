//! The `[[policy]]` tables of the configuration file and the rules every shape applies to them.

use serde::Deserialize;

use super::config::{ConfigError, invalid};
use crate::journal::RESERVED_PREFIXES;

/// `[[policy]]`: what is done with the keys under a prefix.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Policy {
    /// The keys it is about: empty for all the rest, or ending in `/`.
    pub prefix: String,
    /// Whether to keep the keys under the prefix in the local cache. Only a build node has one.
    #[serde(default)]
    pub cache: bool,
}

/// Refuses the policies that are duplicates, overlap or name a reserved prefix, and those that
/// ask for a cache when the shape has none (`caching` says whether it has).
pub(super) fn check_policies(policies: &[Policy], caching: bool) -> Result<(), ConfigError> {
    for (at, policy) in policies.iter().enumerate() {
        let key = format!("policy[{at}]");
        let prefix = policy.prefix.as_str();
        if !prefix.is_empty() && !prefix.ends_with('/') {
            return Err(invalid(
                &format!("{key}.prefix"),
                format!("{prefix:?} does not end in /"),
            ));
        }
        if let Some(reserved) = RESERVED_PREFIXES
            .iter()
            .find(|name| prefix.starts_with(**name))
        {
            let why = format!("{prefix:?} is under {reserved:?}, which tierstore keeps for itself");
            return Err(invalid(&format!("{key}.prefix"), why));
        }
        if policy.cache && !caching {
            let why = "a node without [local_cache] keeps no cache: give it [local_cache]";
            return Err(invalid(&format!("{key}.cache"), why));
        }
        for (before, other) in policies.iter().enumerate().take(at) {
            let (theirs, ours) = (other.prefix.as_str(), prefix);
            if theirs == ours {
                let why = format!("{ours:?} is also the prefix of policy[{before}]");
                return Err(invalid(&format!("{key}.prefix"), why));
            }
            let nested = !theirs.is_empty() && !ours.is_empty();
            if nested && (theirs.starts_with(ours) || ours.starts_with(theirs)) {
                let why = format!("{ours:?} overlaps {theirs:?} of policy[{before}]");
                return Err(invalid(&format!("{key}.prefix"), why));
            }
        }
    }
    Ok(())
}

/// Refuses a file whose `[[policy]]` tables keep nothing: a cache with no `cache = true` policy
/// would hold nothing.
pub(super) fn require_keeping(policies: &[Policy]) -> Result<(), ConfigError> {
    if policies.iter().any(|policy| policy.cache) {
        return Ok(());
    }
    let why = "no policy has cache = true, so nothing would be kept";
    Err(invalid("policy", why))
}
