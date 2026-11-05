//! The continuation token of ListObjectsV2: how S3 clients are handed a place in a listing and
//! give it back. The walk ([`super::listing`]) knows only the place ([`Start`]); the token is its
//! spelling on the S3 wire, and a bad one is the S3 error `InvalidArgument`.

use super::listing::Start;
use super::reply::S3Error;
use super::request::Query;

/// The mark of the token of a common prefix. It is not a hexadecimal digit, so it tells that
/// token from the token of an object, which is the hexadecimal of its key (the only token
/// before common prefixes were listed, and still the token of an object).
const PREFIX_MARK: char = 'p';

/// The token of the page that starts at `start`.
pub(super) fn token(start: &Start) -> String {
    match start {
        Start::After(key) => hex::encode(key),
        Start::PastPrefix(prefix) => format!("{PREFIX_MARK}{}", hex::encode(prefix)),
    }
}

/// Where the page a token stands for starts.
fn untoken(token: &str) -> Result<Start, S3Error> {
    let bad = || S3Error::invalid("the continuation token is not one this server gave");
    let (hexed, prefix) = match token.strip_prefix(PREFIX_MARK) {
        Some(rest) => (rest, true),
        None => (token, false),
    };
    let bytes = hex::decode(hexed).map_err(|_| bad())?;
    let text = String::from_utf8(bytes).map_err(|_| bad())?;
    // A common prefix has the delimiter in it, so it is never empty.
    if prefix && text.is_empty() {
        return Err(bad());
    }
    Ok(if prefix {
        Start::PastPrefix(text)
    } else {
        Start::After(text)
    })
}

/// Where a listing starts: after what its continuation token stands for, else after
/// `start-after`, else at the first key.
pub(super) fn start(query: &Query) -> Result<Option<Start>, S3Error> {
    match query.get("continuation-token") {
        Some(text) => untoken(text).map(Some),
        None => Ok(query
            .get("start-after")
            .map(|key| Start::After(key.to_owned()))),
    }
}
