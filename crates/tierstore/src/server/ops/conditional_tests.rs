//! What the conditions of a put and of a completion ask the backend for, and the answer of the
//! store of two tiers, which does not replace on a tag.

use hyper::StatusCode;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use sha2::{Digest as _, Sha256};

use super::Writes;
use super::conditional::{Completes, completes, refused, streamed};
use crate::server::ops_tests::{completion, start, upload};
use crate::server::testing::{BUCKET, Reply, Rig, texts};

const MEBIBYTE: u64 = 1 << 20;

/// What the two-tier store tells a client that sends `If-Match` with the ETag the object has.
const NO_SWAP: &str = "If-Match is not supported over a hot and a cold tier: the ETag of an \
     object is that of the tier that holds it and changes when the object moves between the \
     tiers, so it does not name one version of the object";

/// Headers with these names and values.
fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        let value = HeaderValue::from_str(value).unwrap();
        map.insert(HeaderName::from_static(name), value);
    }
    map
}

/// The headers of a put, and how the full key and a key that only creates store it.
type Case = (
    &'static [(&'static str, &'static str)],
    &'static str,
    &'static str,
);

/// How a streamed put is stored, as text.
fn stored(writes: Option<Writes<'_>>) -> String {
    match writes {
        None => "refused".to_owned(),
        Some(Writes::Creates) => "creates".to_owned(),
        Some(Writes::Full) => "full".to_owned(),
        Some(Writes::Presigned(_)) => "presigned".to_owned(),
        Some(Writes::Swaps(tag)) => format!("swaps {tag}"),
    }
}

/// Asserts how the conditions of a put decide how it is stored, for the full key and for a key
/// that only creates: none is a plain put of the full key; `If-None-Match: *` creates; `If-Match`
/// alone is a replace on its ETag for the full key, with the quotes and the spaces around it taken
/// off; `If-Match` beside `If-None-Match`, any other `If-None-Match`, and `If-Match` from a key
/// that only creates are not streamed (refused). The refusal names `If-Match` and
/// `If-None-Match` together where the request has both.
#[test]
fn the_conditions_of_a_put_decide_how_it_is_stored() {
    let cases: [Case; 9] = [
        (&[], "full", "creates"),
        (&[("if-none-match", "*")], "creates", "creates"),
        (&[("if-match", "\"abc\"")], "swaps abc", "refused"),
        (&[("if-match", "abc")], "swaps abc", "refused"),
        (&[("if-match", " \"abc\" ")], "swaps abc", "refused"),
        (&[("if-match", "*")], "swaps *", "refused"),
        (&[("if-none-match", "\"abc\"")], "refused", "refused"),
        (
            &[("if-match", "\"abc\""), ("if-none-match", "*")],
            "refused",
            "refused",
        ),
        (
            &[("if-match", "\"abc\""), ("if-none-match", "\"abc\"")],
            "refused",
            "refused",
        ),
    ];
    for (pairs, full, creates) in cases {
        let map = headers(pairs);
        assert_eq!(stored(streamed(&map, Writes::Full)), full, "{pairs:?}");
        assert_eq!(
            stored(streamed(&map, Writes::Creates)),
            creates,
            "{pairs:?}"
        );
    }
    let both = refused(&headers(&[("if-match", "a"), ("if-none-match", "*")]));
    let text = "If-Match and If-None-Match are not supported together";
    assert_eq!(
        (both.kind().0, both.message()),
        (StatusCode::NOT_IMPLEMENTED, text)
    );
    let other = refused(&headers(&[("if-none-match", "\"a\"")]));
    let text = "only If-None-Match: * is supported";
    assert_eq!(
        (other.kind().0, other.message()),
        (StatusCode::NOT_IMPLEMENTED, text)
    );
}

/// Asserts how the conditions of a completion decide how it makes its object: for the full key a
/// plain completion replaces, one with an `If-None-Match` creates, one with `If-Match` alone
/// replaces on its ETag (without quotes), and one with both is 501; a key that only creates
/// creates whatever the conditions say.
#[test]
fn the_conditions_of_a_completion_decide_how_it_makes_its_object() {
    let full = |pairs: &[(&'static str, &str)]| completes(&headers(pairs), Writes::Full);
    assert_eq!(full(&[]).unwrap(), Completes::Replaces);
    assert_eq!(full(&[("if-none-match", "*")]).unwrap(), Completes::Creates);
    let swaps = full(&[("if-match", "\"abc\"")]).unwrap();
    assert_eq!(swaps, Completes::Swaps("abc".to_owned()));
    let both = full(&[("if-match", "\"abc\""), ("if-none-match", "*")]).unwrap_err();
    assert_eq!(both.kind(), (StatusCode::NOT_IMPLEMENTED, "NotImplemented"));
    for pairs in [&[][..], &[("if-match", "\"abc\"")][..]] {
        let made = completes(&headers(pairs), Writes::Creates).unwrap();
        assert_eq!(made, Completes::Creates, "{pairs:?}");
    }
}

fn tag_of(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// The status, the code and the message of an answer.
fn refusal(reply: &Reply) -> (u16, String, String) {
    let message = texts(&reply.body, "Message").concat();
    (reply.status.as_u16(), reply.code(), message)
}

/// Asserts that the store of two tiers does not replace on a tag and says why: a put and a
/// completion with `If-Match` of the ETag the object has are 501 `NotImplemented` with the reason
/// as the message, one with another ETag is 412, and one of a key no tier holds is 404
/// `NoSuchKey`, as a HEAD finds them; the object keeps its bytes and the upload stays open.
#[tokio::test(start_paused = true)]
async fn if_match_over_two_tiers_is_refused_and_says_why() {
    let rig = Rig::new(MEBIBYTE).await;
    assert_eq!(rig.put("k", b"one").await.status, StatusCode::OK);
    let own = format!("\"{}\"", tag_of(b"one"));
    let not_implemented = (501, "NotImplemented".to_owned(), NO_SWAP.to_owned());
    let failed = (412, "PreconditionFailed".to_owned());
    let missing = (404, "NoSuchKey".to_owned());
    let (uri, absent) = (format!("/{BUCKET}/k"), format!("/{BUCKET}/absent"));

    let same = rig
        .send("PUT", &uri, &[("if-match", own.as_str())], b"two")
        .await;
    assert_eq!(refusal(&same), not_implemented);
    let other = rig
        .send("PUT", &uri, &[("if-match", "\"nope\"")], b"two")
        .await;
    assert_eq!((other.status.as_u16(), other.code()), failed);
    let none = rig
        .send("PUT", &absent, &[("if-match", "\"x\"")], b"two")
        .await;
    assert_eq!((none.status.as_u16(), none.code()), missing);

    let id = start(&rig, "k").await;
    let part = upload(&rig, "k", &id, 1, b"parts").await;
    let body = completion(&[(1, part.header("etag"))]);
    let target = |key: &str| format!("/{BUCKET}/{key}?uploadId={id}");
    let ask = |key: &str, tag: &str| {
        let (target, body, tag) = (target(key), body.clone(), tag.to_owned());
        let rig = &rig;
        async move {
            let extra = [("if-match", tag.as_str())];
            rig.send("POST", &target, &extra, body.as_bytes()).await
        }
    };
    assert_eq!(refusal(&ask("k", &own).await), not_implemented);
    let other = ask("k", "\"nope\"").await;
    assert_eq!((other.status.as_u16(), other.code()), failed);
    let none = ask("absent", "\"x\"").await;
    assert_eq!((none.status.as_u16(), none.code()), missing);

    assert_eq!(rig.get("k", None).await.body, "one");
    let parts = rig.call("GET", &format!("k?uploadId={id}"), b"").await;
    assert_eq!(parts.status, StatusCode::OK, "{}", parts.body);
    assert_eq!(texts(&parts.body, "PartNumber"), ["1"]);
}
