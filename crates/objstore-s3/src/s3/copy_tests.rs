//! The reading of a 200 answer to a copy, whose body says whether the copy was made.

use super::copy_answer;
use crate::{ObjectKey, ObjectStoreError};

fn read(body: &str, root: &str) -> Result<super::CopyAnswer, ObjectStoreError> {
    let key = ObjectKey::new("k").unwrap();
    copy_answer(body.as_bytes(), root, &key)
}

/// Asserts that the document of a copy gives its tag as sent and its time, and that a document
/// with no `LastModified`, or one that is not a date, gives no time.
#[test]
fn a_copy_result_gives_its_tag_and_its_time() {
    let full = "<CopyObjectResult xmlns=\"urn:s3\">\
                <LastModified>2026-11-03T08:49:37.000Z</LastModified>\
                <ETag> &quot;abc&quot; </ETag></CopyObjectResult>";
    let bare = "<CopyPartResult><ETag>\"def\"</ETag><LastModified>soon</LastModified>\
                </CopyPartResult>";

    let full = read(full, "CopyObjectResult").unwrap();
    let bare = read(bare, "CopyPartResult").unwrap();

    assert_eq!(full.etag, "\"abc\"");
    assert_eq!(full.modified_millis, Some(1_793_695_777_000));
    assert_eq!(bare.etag, "\"def\"");
    assert_eq!(bare.modified_millis, None);
}

/// Asserts that a 200 whose body is not the document of the copy that names a tag (another
/// document, one with no or an empty `ETag`, no XML) is a protocol error, and that an `<Error>`
/// is the failure it names, with status 200.
#[test]
fn a_copy_answer_that_is_not_a_result_is_an_error() {
    let others = [
        "<CopyPartResult><ETag>\"abc\"</ETag></CopyPartResult>",
        "<CopyObjectResult><LastModified>2026-11-03T08:49:37.000Z</LastModified>\
         </CopyObjectResult>",
        "<CopyObjectResult><ETag> </ETag></CopyObjectResult>",
        "not xml",
    ];
    for body in others {
        let refused = read(body, "CopyObjectResult").unwrap_err();
        assert!(
            matches!(refused, ObjectStoreError::Protocol(_)),
            "{body}: {refused:?}"
        );
    }
    let late = "<Error><Code>SlowDown</Code><Message>wait</Message></Error>";
    match read(late, "CopyObjectResult").unwrap_err() {
        ObjectStoreError::Service {
            status,
            code,
            message,
        } => assert_eq!(
            (status, code.as_str(), message.as_str()),
            (200, "SlowDown", "wait")
        ),
        other => panic!("expected Service, got {other:?}"),
    }
}

/// Asserts that a document laid out over lines, as AWS sends it, with text and a comment between
/// its elements, gives its tag: only elements are read as its fields.
#[test]
fn a_copy_result_laid_out_over_lines_gives_its_tag() {
    let laid_out = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<CopyObjectResult>\n  \
                    <!-- written by the store -->\n  \
                    <LastModified>2026-11-03T08:49:37.000Z</LastModified>\n  \
                    <ETag>\"abc\"</ETag>\n</CopyObjectResult>\n";
    let answer = read(laid_out, "CopyObjectResult").unwrap();
    assert_eq!(answer.etag, "\"abc\"");
    assert_eq!(answer.modified_millis, Some(1_793_695_777_000));
}
