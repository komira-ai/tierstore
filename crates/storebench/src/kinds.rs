//! What an object of silo is, from its key and size: the CAS service's key layout.
//!
//! The CAS service (the build farm's frontend, storage nodes and task runners) writes under an
//! optional deployment prefix (`front/<log>/`, `team1/`, ...) and then `cas/`:
//!
//! | key after the prefix | kind |
//! |---|---|
//! | `cas/d/<node>/<boot>/<16 hex>`, `cas/f/<writer>/<boot>/<16 hex>` | pack (daemon, frontend) |
//! | `cas/<16 hex>/<16 hex>/<16 hex>`, `cas/<16 hex>/<16 hex>` | pack (storage member segment) |
//! | `cas/b/<build>/<40 hex>/pack`, `.../pack<k>`, `cas/b/<build>/<40 hex>-<k>` | pack (task output) |
//! | `cas/b/<build>/<40 hex>/L<i>`, `cas/b/<build>/<40 hex>-L<i>` | large blob (task output) |
//! | `cas/l/<64 hex>`, larger than 64 MiB | large blob (one blob, its own object) |
//! | `cas/l/<64 hex>`, at most 64 MiB | tree list (a `tree_manifest` JSON) |
//! | anything else | other |
//!
//! Every pack is a segment: blob records back to back and a footer (`crate::footers`). A blob
//! over 64 MiB is never packed, so a `cas/l/` object of at most 64 MiB is a manifest; the
//! footer scan checks that by reading the first byte of each. No released CAS service writes
//! an L2 sidecar (design section 3) yet, so no key is one.

use serde::{Deserialize, Serialize};

/// The largest blob the CAS service packs; a larger one is its own object under `cas/l/`.
pub const MAX_PACKED_BLOB: u64 = 64 << 20;

/// The kind of an object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// A pack: blob records and a footer.
    Pack,
    /// One blob larger than [`MAX_PACKED_BLOB`], stored as its own object.
    LargeBlob,
    /// A tree list (a `tree_manifest` JSON under `cas/l/`).
    TreeList,
    /// Anything else (probes, self-tests, history).
    Other,
}

impl Kind {
    /// Every kind, in the order the tables print them.
    pub const ALL: [Kind; 4] = [Kind::Pack, Kind::LargeBlob, Kind::TreeList, Kind::Other];

    /// The kind's name in a table.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Kind::Pack => "pack",
            Kind::LargeBlob => "large blob",
            Kind::TreeList => "tree list",
            Kind::Other => "other",
        }
    }
}

/// Whether `s` is `n` lowercase hexadecimal digits.
fn hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether `s` is one or more decimal digits.
fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// The deployment prefix of `key`: everything before its `cas/` component (empty when the key
/// starts with it, the whole key's first component when it has none).
#[must_use]
pub fn prefix(key: &str) -> &str {
    match key.split('/').position(|c| c == "cas") {
        Some(i) => {
            let end: usize = key.split('/').take(i).map(|c| c.len() + 1).sum();
            &key[..end]
        }
        None => key.split_once('/').map_or(key, |(first, _)| first),
    }
}

/// The kind of the object `key` of `size` bytes.
#[must_use]
pub fn classify(key: &str, size: u64) -> Kind {
    let parts: Vec<&str> = key.split('/').collect();
    let Some(at) = parts.iter().position(|c| *c == "cas") else {
        return Kind::Other;
    };
    match &parts[at + 1..] {
        ["d" | "f", _, _, seq] if hex(seq, 16) => Kind::Pack,
        [a, b, c] if hex(a, 16) && hex(b, 16) && hex(c, 16) => Kind::Pack,
        [a, b] if hex(a, 16) && hex(b, 16) => Kind::Pack,
        ["b", _, run, last] if hex(run, 40) => task_output(last),
        ["b", _, slot] => match slot.split_once('-') {
            Some((run, last)) if hex(run, 40) => task_output(last),
            _ => Kind::Other,
        },
        ["l", digest] if hex(digest, 64) => {
            if size > MAX_PACKED_BLOB {
                Kind::LargeBlob
            } else {
                Kind::TreeList
            }
        }
        _ => Kind::Other,
    }
}

/// The kind of a task output named `last` (`pack`, `pack<k>`, `<k>`, `L<i>`).
fn task_output(last: &str) -> Kind {
    if last == "pack" || last.strip_prefix("pack").is_some_and(digits) || digits(last) {
        Kind::Pack
    } else if last.strip_prefix('L').is_some_and(digits) {
        Kind::LargeBlob
    } else {
        Kind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H16: &str = "0000000000000041";

    /// Every key form of the CAS service gets its kind, under any deployment prefix.
    #[test]
    fn each_key_form_of_the_cas_service_has_its_kind() {
        let h40 = "62867f9ca65aae3d0b2d487064249bdd01c87576";
        let h64 = "ab".repeat(32);
        let cases = [
            (format!("cas/d/n7/b2/{H16}"), 10, Kind::Pack),
            (format!("cas/f/frontend/b2/{H16}"), 10, Kind::Pack),
            (format!("team1/cas/{H16}/{H16}/{H16}"), 10, Kind::Pack),
            (
                format!("front/293299bb865ff265/cas/{H16}/{H16}"),
                10,
                Kind::Pack,
            ),
            (format!("cas/b/1-2/{h40}/pack"), 10, Kind::Pack),
            (format!("cas/b/1-2/{h40}/pack3"), 10, Kind::Pack),
            (format!("cas/b/1-2/{h40}-3"), 10, Kind::Pack),
            (format!("cas/b/1-2/{h40}/L0"), 10, Kind::LargeBlob),
            (format!("cas/b/1-2/{h40}-L12"), 10, Kind::LargeBlob),
            (format!("cas/l/{h64}"), MAX_PACKED_BLOB + 1, Kind::LargeBlob),
            (format!("cas/l/{h64}"), MAX_PACKED_BLOB, Kind::TreeList),
            ("cas/d/n7/b2/selftest".to_owned(), 10, Kind::Other),
            (format!("cas/b/1-2/{h40}/manifest"), 10, Kind::Other),
            (format!("cas/b/1-2/{h40}/packx"), 10, Kind::Other),
            (format!("cas/b/1-2/{h40}-x"), 10, Kind::Other),
            ("cas/b/1-2/nodash".to_owned(), 10, Kind::Other),
            ("cas/b/1-2/short-1".to_owned(), 10, Kind::Other),
            (format!("cas/l/{H16}"), 10, Kind::Other),
            (format!("cas/{H16}/XYZ"), 10, Kind::Other),
            // The right lengths with a digit that is not hex, in each place a hash goes.
            (format!("cas/{H16}/XYZ/{H16}"), 10, Kind::Other),
            (format!("cas/{H16}/{H16}/XYZ"), 10, Kind::Other),
            (format!("cas/l/{}", "g".repeat(64)), 10, Kind::Other),
            ("cas/b/1-2/not-a-run/pack".to_owned(), 10, Kind::Other),
            ("history/history/preflight-put-x".to_owned(), 1, Kind::Other),
        ];
        for (key, size, kind) in cases {
            assert_eq!(classify(&key, size), kind, "{key}");
        }
    }

    /// The prefix is what comes before `cas/`; a key without `cas/` has its first component.
    #[test]
    fn the_prefix_is_what_comes_before_cas() {
        assert_eq!(prefix("cas/b/x/y/pack"), "");
        assert_eq!(prefix("team1/cas/1/2/3"), "team1/");
        assert_eq!(prefix("front/2932/cas/1/2"), "front/2932/");
        assert_eq!(prefix("history/history/x"), "history");
        assert_eq!(prefix("lonely"), "lonely");
    }

    /// The kinds print in a fixed order with their names.
    #[test]
    fn kinds_have_names_in_order() {
        let names: Vec<&str> = Kind::ALL.iter().map(|k| k.name()).collect();
        assert_eq!(names, ["pack", "large blob", "tree list", "other"]);
    }
}
