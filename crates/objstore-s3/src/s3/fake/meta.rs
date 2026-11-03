//! The user metadata the service keeps with an object: the reservation mark.
//!
//! S3 takes the metadata of an object from the request that writes it, so the mark is on an
//! object exactly when that request sent it with the value `1`.

use std::collections::BTreeMap;

/// The header that carries the mark, in a request and in the answer to a HEAD or a GET.
const MARK: &str = "x-amz-meta-tierstore-reservation";

/// Whether a request, whose headers are lowercase, sent the mark.
pub(super) fn sent(headers: &BTreeMap<String, String>) -> bool {
    headers.get(MARK).map(String::as_str) == Some("1")
}

/// The headers of an answer that give the mark of an object back: none for an unmarked one.
pub(super) fn echoed(marked: bool) -> Vec<(&'static str, String)> {
    if marked {
        vec![(MARK, "1".to_owned())]
    } else {
        Vec::new()
    }
}
