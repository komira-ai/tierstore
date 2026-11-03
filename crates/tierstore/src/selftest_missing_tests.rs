//! The if-match-missing check of the self-test: a replace with a tag of a key that does not exist
//! must be refused on the hot tier, and is not asked of the cold tier.

use crate::selftest_tests::{Flaw, Flawed, PROBE, go, options, refused};
use crate::{Backend, Check};

/// Asserts that a hot tier whose store creates the key on a replace with a tag of a key that does
/// not exist is refused as the if-match-missing check, with the exact operator line, and that the
/// key it created is deleted again.
#[tokio::test]
async fn a_replace_on_a_missing_key_that_creates_it_is_refused_on_the_hot_tier() {
    let failure = refused(Flaw::CreatesOnIfMatchMissing, false).await;
    assert_eq!(failure.check, Check::IfMatchMissing);
    assert_eq!(
        failure.to_string(),
        "refusing to start: the backend failed the if-match-missing check: a replace on a key \
         that does not exist was accepted; it must be refused with 412 or 404, not create the key"
    );
}

/// Asserts that a replace on a missing key refused with 404 (the memory backend) or with 412 passes
/// the if-match-missing check on the hot tier, with no note and no key left.
#[tokio::test]
async fn a_replace_on_a_missing_key_refused_with_404_or_412_passes_on_the_hot_tier() {
    for flaw in [Flaw::None, Flaw::PreconditionOnIfMatchMissing] {
        let backend = Flawed::new(flaw);
        let (outcome, notes) = go(&backend, &options("run-1", false, &[])).await;
        assert_eq!(outcome, Ok(()), "{flaw:?}");
        assert!(notes.is_empty(), "{flaw:?}: {notes:?}");
        assert!(backend.inner.list(PROBE).await.unwrap().is_empty());
    }
}

/// Asserts that an answer to a replace on a missing key that is an error and no refusal (403) is
/// refused as the if-match-missing check, and the operator line gives the answer exactly.
#[tokio::test]
async fn a_replace_on_a_missing_key_answered_with_an_error_is_refused_on_the_hot_tier() {
    let failure = refused(Flaw::ErrorsOnIfMatchMissing, false).await;
    assert_eq!(failure.check, Check::IfMatchMissing);
    assert_eq!(
        failure.to_string(),
        "refusing to start: the backend failed the if-match-missing check: a replace on a key \
         that does not exist failed with: backend error: 403 Forbidden: If-Match"
    );
}

/// Asserts that the if-match-missing check is asked of the hot tier only: a cold tier (the one
/// that takes multipart uploads) whose store creates on a replace of a missing key passes.
#[tokio::test]
async fn the_if_match_missing_check_is_not_asked_of_the_cold_tier() {
    let backend = Flawed::new(Flaw::CreatesOnIfMatchMissing);
    let (outcome, notes) = go(&backend, &options("run-1", true, &[])).await;
    assert_eq!(outcome, Ok(()));
    assert!(notes.is_empty(), "{notes:?}");
    assert!(backend.inner.list(PROBE).await.unwrap().is_empty());
}
