//! The S3 front over a proxy: the keys the proxy keeps for itself are out of reach. The front has
//! no check of its own; the proxy refuses them and leaves them out of its listings.

use bytes::Bytes;
use hyper::StatusCode;

use crate::Backend;

use super::testing::{Rig, texts};

const MEBIBYTE: u64 = 1 << 20;

/// Asserts that no call can name a key under `_entries/`, `_journal/` or `_tiering/`: get, head,
/// a create-only put, delete and the multipart calls are `AccessDenied` (the proxy behind the
/// front refuses them, the front has no check of its own), and a listing leaves those keys out
/// though the hot tier and the cold tier hold them.
#[tokio::test(start_paused = true)]
async fn the_keys_of_the_proxy_are_out_of_reach() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.put("k", b"v").await;
    rig.hot_mem
        .put("_entries/h", Bytes::from("e"))
        .await
        .unwrap();
    rig.cold_mem
        .put("_entries/c", Bytes::from("e"))
        .await
        .unwrap();
    rig.cold_mem
        .put("_tiering/c", Bytes::from("e"))
        .await
        .unwrap();
    assert!(rig.hot_mem.get("_tiering/proxies/p1", None).await.is_ok());
    let complete = b"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber>\
        <ETag>\"aa\"</ETag></Part></CompleteMultipartUpload>";
    for key in [
        "_journal/p1/00000000000000000001",
        "_tiering/lease",
        "_tiering/c",
        "_entries/h",
        "_entries/c",
    ] {
        for method in ["GET", "HEAD", "DELETE"] {
            let reply = rig.call(method, key, b"x").await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {key}");
        }
        let put = rig.put(key, b"x").await;
        assert_eq!(put.status, StatusCode::FORBIDDEN, "PUT {key}");
        let start = rig.call("POST", &format!("{key}?uploads"), b"").await;
        assert_eq!(start.status, StatusCode::FORBIDDEN, "{key}");
        let part_uri = format!("{key}?partNumber=1&uploadId=u");
        let part = rig.call("PUT", &part_uri, b"x").await;
        assert_eq!(part.status, StatusCode::FORBIDDEN, "{key}");
        let done_uri = format!("{key}?uploadId=u");
        let done = rig.call("POST", &done_uri, complete).await;
        assert_eq!(done.status, StatusCode::FORBIDDEN, "{key}");
        let abort = rig.call("DELETE", &done_uri, b"").await;
        assert_eq!(abort.status, StatusCode::FORBIDDEN, "{key}");
    }
    let reply = rig.call("GET", "_tiering/lease", b"").await;
    assert_eq!(reply.code(), "AccessDenied");
    assert_eq!(texts(&rig.list("").await.body, "Key"), ["k"]);
    assert_eq!(
        texts(&rig.list("prefix=_entries/").await.body, "Key"),
        Vec::<String>::new()
    );
}
