//! Keys that a listing could not carry as XML 1.0 are refused where an object or an upload is
//! created, through `Server::handle`, over a proxy whose tiers are in memory.

use hyper::StatusCode;

use super::testing::{BUCKET, Rig, texts};

/// Asserts that a put of a key with a control character, or with U+FFFE or U+FFFF, is answered
/// 400 `InvalidArgument` and stores nothing, that the same holds for CreateMultipartUpload (no
/// upload is begun), and that the listings after the refusals are complete. It catches a key
/// that is stored and then makes every listing page that holds it malformed XML.
#[tokio::test(start_paused = true)]
async fn a_key_with_a_character_xml_forbids_is_refused_at_put_and_at_create_upload() {
    let rig = Rig::new(1 << 20).await;
    let keys = [
        "a%01b",
        "a%00b",
        "a%0Ab",
        "a%7Fb",
        "a%EF%BF%BEb",
        "a%EF%BF%BFb",
    ];
    for key in keys {
        let put = rig.put(key, b"data").await;
        assert_eq!(put.status, StatusCode::BAD_REQUEST, "{key}");
        assert_eq!(put.code(), "InvalidArgument", "{key}");
        let start = rig.call("POST", &format!("{key}?uploads"), b"").await;
        assert_eq!(start.status, StatusCode::BAD_REQUEST, "{key}");
        assert_eq!(start.code(), "InvalidArgument", "{key}");
    }
    let objects = rig
        .send("GET", &format!("/{BUCKET}?list-type=2"), &[], b"")
        .await;
    assert_eq!(objects.status, StatusCode::OK);
    assert!(texts(&objects.body, "Key").is_empty());
    let uploads = rig
        .send("GET", &format!("/{BUCKET}?uploads"), &[], b"")
        .await;
    assert_eq!(uploads.status, StatusCode::OK);
    assert!(texts(&uploads.body, "Key").is_empty());
}

/// Asserts that keys with characters that XML allows (a space, `&`, `<`
/// and non-ASCII text) are still put and listed as well-formed XML, so the refusal is no wider
/// than the characters XML forbids.
#[tokio::test(start_paused = true)]
async fn keys_with_markup_and_non_ascii_characters_are_still_stored_and_listed() {
    let rig = Rig::new(1 << 20).await;
    let keys = ["a%20b", "a%26b", "a%3Cb", "caf%C3%A9"];
    for key in keys {
        let put = rig.put(key, b"data").await;
        assert_eq!(put.status, StatusCode::OK, "{key}");
    }
    let listed = rig
        .send("GET", &format!("/{BUCKET}?list-type=2"), &[], b"")
        .await;
    assert_eq!(listed.status, StatusCode::OK);
    assert_eq!(texts(&listed.body, "Key"), ["a b", "a&b", "a<b", "café"]);
}
