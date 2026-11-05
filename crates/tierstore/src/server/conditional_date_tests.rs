//! `If-Modified-Since` and `If-Unmodified-Since` on a get and a head, and their order against
//! `If-Match` and `If-None-Match` (RFC 7232, section 6), as S3 answers them.

use hyper::StatusCode;

use super::testing::{BUCKET, Reply, Rig};

/// The time the rig stamps the object with, as `Last-Modified` writes it.
const STAMP: &str = "Fri, 15 Jan 2027 08:00:00 GMT";
const HOUR_BEFORE: &str = "Fri, 15 Jan 2027 07:00:00 GMT";
const HOUR_AFTER: &str = "Fri, 15 Jan 2027 09:00:00 GMT";

/// The status of the answer to a read with the headers `extra`.
async fn status(rig: &Rig, method: &str, extra: &[(&str, &str)]) -> StatusCode {
    read(rig, method, extra).await.status
}

/// A rig with the object `k` in it.
async fn rig_with_object() -> (Rig, String) {
    let rig = Rig::new(1 << 20).await;
    let put = rig.put("k", b"one").await;
    assert_eq!(put.status, StatusCode::OK);
    let head = rig.call("HEAD", "k", b"").await;
    assert_eq!(head.header("last-modified"), STAMP);
    let tag = head.header("etag").to_owned();
    (rig, tag)
}

async fn read(rig: &Rig, method: &str, extra: &[(&str, &str)]) -> Reply {
    let uri = format!("/{BUCKET}/k");
    rig.send(method, &uri, extra, b"").await
}

/// Asserts what S3 answers to the date conditions alone, for a get and for a head:
/// `If-Modified-Since` at or after the time of the object is 304 with its tag and time and no
/// body, and before it is served; `If-Unmodified-Since` before the time of the object is 412
/// `PreconditionFailed`, and at or after it is served.
#[tokio::test(start_paused = true)]
async fn the_date_conditions_alone_are_answered_as_s3_does() {
    let (rig, tag) = rig_with_object().await;
    for method in ["GET", "HEAD"] {
        for date in [STAMP, HOUR_AFTER] {
            let reply = read(&rig, method, &[("if-modified-since", date)]).await;
            assert_eq!(reply.status, StatusCode::NOT_MODIFIED, "{method} {date}");
            assert_eq!(reply.header("etag"), tag);
            assert_eq!(reply.header("last-modified"), STAMP);
            assert_eq!(reply.body, "");
        }
        let modified = read(&rig, method, &[("if-modified-since", HOUR_BEFORE)]).await;
        assert_eq!(modified.status, StatusCode::OK, "{method}");
        let before = read(&rig, method, &[("if-unmodified-since", HOUR_BEFORE)]).await;
        assert_eq!(before.status, StatusCode::PRECONDITION_FAILED, "{method}");
        assert_eq!(before.code(), "PreconditionFailed", "{method}");
        for date in [STAMP, HOUR_AFTER] {
            let reply = read(&rig, method, &[("if-unmodified-since", date)]).await;
            assert_eq!(reply.status, StatusCode::OK, "{method} {date}");
        }
    }
}

/// Asserts that the dates are compared to the second, as `Last-Modified` shows them: an object
/// stored half a second after a whole second is not modified since that second, and is not
/// unmodified since the second before it.
#[tokio::test(start_paused = true)]
async fn the_dates_are_compared_to_the_second() {
    let rig = Rig::new(1 << 20).await;
    rig.hot_mem.clock().advance(500);
    rig.put("k", b"one").await;
    let head = rig.call("HEAD", "k", b"").await;
    assert_eq!(head.header("last-modified"), STAMP);
    let same = read(&rig, "GET", &[("if-modified-since", STAMP)]).await;
    assert_eq!(same.status, StatusCode::NOT_MODIFIED);
    let unmodified = read(&rig, "GET", &[("if-unmodified-since", STAMP)]).await;
    assert_eq!(unmodified.status, StatusCode::OK);
    let second_before = "Fri, 15 Jan 2027 07:59:59 GMT";
    let earlier = read(&rig, "GET", &[("if-unmodified-since", second_before)]).await;
    assert_eq!(earlier.status, StatusCode::PRECONDITION_FAILED);
}

/// Asserts the order of RFC 7232 and of the S3 documentation, for a get and a head: with
/// `If-Match` true, `If-Unmodified-Since` false is served (200); with `If-Match` false,
/// `If-Unmodified-Since` true is still 412; with `If-None-Match` true (the tag is listed),
/// `If-Modified-Since` true is 304 and false is 304 too; with `If-None-Match` false (another
/// tag) `If-Modified-Since` is not looked at, so a date that alone would give 304 is served;
/// and a failed `If-Match` is 412 before an `If-None-Match` is looked at.
#[tokio::test(start_paused = true)]
async fn the_date_conditions_come_in_the_order_of_rfc_7232() {
    let (rig, tag) = rig_with_object().await;
    let other = "\"x\"";
    for method in ["GET", "HEAD"] {
        let both = [
            ("if-match", tag.as_str()),
            ("if-unmodified-since", HOUR_BEFORE),
        ];
        assert_eq!(
            status(&rig, method, &both).await,
            StatusCode::OK,
            "{method} match, unmodified false"
        );
        let wrong = [("if-match", other), ("if-unmodified-since", HOUR_AFTER)];
        let failed = StatusCode::PRECONDITION_FAILED;
        assert_eq!(
            status(&rig, method, &wrong).await,
            failed,
            "{method} match false, unmodified true"
        );
        let listed = [
            ("if-none-match", tag.as_str()),
            ("if-modified-since", HOUR_BEFORE),
        ];
        assert_eq!(
            status(&rig, method, &listed).await,
            StatusCode::NOT_MODIFIED,
            "{method} none-match true"
        );
        let listed = [
            ("if-none-match", tag.as_str()),
            ("if-modified-since", HOUR_AFTER),
        ];
        assert_eq!(
            status(&rig, method, &listed).await,
            StatusCode::NOT_MODIFIED,
            "{method} both true"
        );
        let unlisted = [("if-none-match", other), ("if-modified-since", HOUR_AFTER)];
        assert_eq!(
            status(&rig, method, &unlisted).await,
            StatusCode::OK,
            "{method} none-match false"
        );
        let first = [("if-match", other), ("if-none-match", tag.as_str())];
        assert_eq!(
            status(&rig, method, &first).await,
            failed,
            "{method} match before none-match"
        );
        let unmodified = [
            ("if-unmodified-since", HOUR_BEFORE),
            ("if-modified-since", HOUR_AFTER),
        ];
        assert_eq!(
            status(&rig, method, &unmodified).await,
            failed,
            "{method} unmodified before modified"
        );
    }
}

/// Asserts that a date which is not an HTTP date is ignored, as S3 ignores it, for both
/// headers, a get and a head: the object is served, even when the other date would refuse it.
/// The two obsolete forms of a date (RFC 850 and asctime) are read.
#[tokio::test(start_paused = true)]
async fn a_malformed_date_is_ignored_and_the_obsolete_forms_are_read() {
    let (rig, _) = rig_with_object().await;
    for method in ["GET", "HEAD"] {
        for name in ["if-modified-since", "if-unmodified-since"] {
            for text in [
                "",
                "yesterday",
                "2027-01-15T08:00:00Z",
                "Fri, 31 Feb 2027 08:00:00 GMT",
            ] {
                let reply = read(&rig, method, &[(name, text)]).await;
                assert_eq!(reply.status, StatusCode::OK, "{method} {name} {text:?}");
            }
        }
        let rfc850 = "Friday, 15-Jan-27 08:00:00 GMT";
        let old = read(&rig, method, &[("if-modified-since", rfc850)]).await;
        assert_eq!(old.status, StatusCode::NOT_MODIFIED, "{method}");
        let asctime = "Fri Jan 15 07:00:00 2027";
        let old = read(&rig, method, &[("if-unmodified-since", asctime)]).await;
        assert_eq!(old.status, StatusCode::PRECONDITION_FAILED, "{method}");
    }
}

/// Asserts that a range read with a date condition that holds is served as a range, and that
/// one that does not is answered by the condition, not by the range.
#[tokio::test(start_paused = true)]
async fn a_date_condition_comes_before_the_range() {
    let (rig, _) = rig_with_object().await;
    let range = ("range", "bytes=0-1");
    let served = read(&rig, "GET", &[range, ("if-modified-since", HOUR_BEFORE)]).await;
    assert_eq!(served.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(served.body, "on");
    let cut = read(&rig, "GET", &[range, ("if-modified-since", STAMP)]).await;
    assert_eq!(cut.status, StatusCode::NOT_MODIFIED);
    let refused = read(&rig, "GET", &[range, ("if-unmodified-since", HOUR_BEFORE)]).await;
    assert_eq!(refused.status, StatusCode::PRECONDITION_FAILED);
}
