//! The S3 front's GET as one call to the backend: the meta and the body come from one open read,
//! so a hot hit is one hot call, whatever the range or the condition.

use hyper::StatusCode;

use crate::Op;

use super::testing::{BUCKET, Rig};

const MEBIBYTE: u64 = 1 << 20;

/// A request's extra headers, the status of its answer and the body it is answered with.
type Case<'a> = (&'a [(&'a str, &'a str)], StatusCode, &'a str);

fn tag_of(body: &[u8]) -> String {
    hex::encode(<sha2::Sha256 as sha2::Digest>::digest(body))
}

/// Asserts that a GET of an object in the hot tier is one call to the hot tier and none to the
/// cold tier, and no HEAD, whole, by range (a suffix too), with a condition that is met and one
/// that is not (the 304 and the unsatisfiable range are decided on that one call's meta).
#[tokio::test(start_paused = true)]
async fn a_get_of_a_hot_object_is_one_call_to_the_hot_tier() {
    let rig = Rig::new(MEBIBYTE).await;
    rig.put("k", b"0123456789").await;
    let own = format!("\"{}\"", tag_of(b"0123456789"));
    let uri = format!("/{BUCKET}/k");
    let requests: [Case<'_>; 5] = [
        (&[], StatusCode::OK, "0123456789"),
        (
            &[("range", "bytes=2-4")],
            StatusCode::PARTIAL_CONTENT,
            "234",
        ),
        (&[("range", "bytes=-3")], StatusCode::PARTIAL_CONTENT, "789"),
        (
            &[("if-none-match", own.as_str())],
            StatusCode::NOT_MODIFIED,
            "",
        ),
        (
            &[("range", "bytes=10-")],
            StatusCode::RANGE_NOT_SATISFIABLE,
            "",
        ),
    ];
    for (headers, status, body) in requests {
        let before = [
            rig.hot.calls(Op::Get),
            rig.hot.calls(Op::Head),
            rig.cold.calls(Op::Get),
            rig.cold.calls(Op::Head),
        ];
        let reply = rig.send("GET", &uri, headers, b"").await;
        assert_eq!(reply.status, status, "{headers:?}");
        if status != StatusCode::RANGE_NOT_SATISFIABLE {
            assert_eq!(reply.body, body, "{headers:?}");
        }
        let after = [
            rig.hot.calls(Op::Get),
            rig.hot.calls(Op::Head),
            rig.cold.calls(Op::Get),
            rig.cold.calls(Op::Head),
        ];
        let made: Vec<u64> = after.iter().zip(before).map(|(a, b)| a - b).collect();
        assert_eq!(made, [1, 0, 0, 0], "{headers:?}");
    }
}
