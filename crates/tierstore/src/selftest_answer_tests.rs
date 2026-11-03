//! The range, head and paging checks of the self-test: a backend that answers one wrongly is
//! refused as that check, with the answer.

use crate::Check;
use crate::selftest_tests::{Flaw, refused};

/// Asserts that a ranged read answered with the whole object is refused as the range check, and
/// the message gives what came back.
#[tokio::test]
async fn a_range_answered_with_the_whole_object_is_refused() {
    let failure = refused(Flaw::WholeObjectForRange, true).await;
    assert_eq!(failure.check, Check::Range);
    assert!(failure.observed.contains("0123456789"), "{failure}");
    assert!(failure.observed.contains("2345"), "{failure}");
}

/// Asserts that a head that fails for want of `Last-Modified` is refused as the head check, with
/// the error the backend gave.
#[tokio::test]
async fn a_head_without_last_modified_is_refused() {
    let failure = refused(Flaw::NoLastModified, true).await;
    assert_eq!(failure.check, Check::Head);
    assert!(failure.observed.contains("Last-Modified"), "{failure}");
}

/// Asserts that a head that gives another size than the object written is refused as the head
/// check, and the message gives both sizes.
#[tokio::test]
async fn a_head_with_the_wrong_size_is_refused() {
    let failure = refused(Flaw::HeadLies, true).await;
    assert_eq!(failure.check, Check::Head);
    assert!(failure.observed.contains("size"), "{failure}");
    assert!(failure.observed.contains("5, not 4"), "{failure}");
}

/// Asserts that a listing that ignores `start_after` is refused as the paging check, and the
/// message gives the keys it returned.
#[tokio::test]
async fn a_listing_that_ignores_start_after_is_refused() {
    let failure = refused(Flaw::IgnoresStartAfter, true).await;
    assert_eq!(failure.check, Check::Paging);
    assert!(failure.observed.contains("page/b"), "{failure}");
    assert!(
        failure.observed.contains("the page after the second key"),
        "{failure}"
    );
}
