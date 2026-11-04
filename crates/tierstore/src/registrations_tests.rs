//! What the leader has seen of the registrations.

use std::time::Duration;

use crate::Listed;
use crate::journal::proxy_key;
use crate::registrations::Registrations;

const WAIT: Duration = Duration::from_secs(600);

fn listed(id: &str, size: u64, modified: u64) -> Listed {
    Listed {
        key: proxy_key(id),
        size,
        modified,
    }
}

/// Asserts that an owner is departed when its registration is not listed, and when it has stood
/// unchanged for the wait, not a millisecond earlier; that a changed registration starts the count
/// again, and that one first seen late (a new leader) counts from then.
#[test]
fn an_owner_has_departed_when_absent_or_unchanged_for_the_wait() {
    let mut seen = Registrations::default();
    seen.observe(1_000, &[listed("a", 10, 5), listed("b", 10, 5)]);
    assert!(seen.departed("absent", 1_000, WAIT));
    assert!(!seen.departed("a", 1_000 + 600_000 - 1, WAIT));
    assert!(seen.departed("a", 1_000 + 600_000, WAIT));

    seen.observe(500_000, &[listed("a", 11, 5), listed("b", 10, 5)]);
    assert!(!seen.departed("a", 500_000 + 600_000 - 1, WAIT));
    assert!(seen.departed("a", 500_000 + 600_000, WAIT));
    assert!(seen.departed("b", 1_000 + 600_000, WAIT));

    seen.observe(700_000, &[listed("b", 10, 5)]);
    assert!(seen.departed("a", 700_000, WAIT));
}

/// Asserts that a registration seen again as it was keeps the time it was first seen, and a changed
/// one (another time of the last write, or another size) is first seen now: an owner has departed
/// exactly when its registration has stood unchanged for the wait, at the wait itself and not a
/// millisecond before.
#[test]
fn an_unchanged_registration_keeps_its_first_seen_time_and_a_changed_one_starts_again() {
    let mut seen = Registrations::default();
    seen.observe(1_000, &[listed("a", 10, 5), listed("b", 10, 5)]);
    seen.observe(300_000, &[listed("a", 10, 5), listed("b", 10, 6)]);
    let at_wait = 1_000 + 600_000;
    assert!(!seen.departed("a", at_wait - 1, WAIT));
    assert!(seen.departed("a", at_wait, WAIT));
    assert!(!seen.departed("b", at_wait, WAIT));
    assert!(seen.departed("b", 300_000 + 600_000, WAIT));

    seen.observe(at_wait, &[listed("a", 11, 5), listed("b", 10, 6)]);
    assert!(!seen.departed("a", at_wait, WAIT));
    assert!(!seen.departed("a", 900_000, WAIT));
    assert!(seen.departed("b", 900_000, WAIT));
}

/// Asserts that a registration that is no longer listed is forgotten, so that one that comes
/// back is first seen when it does, and that an object outside the prefix is not a registration.
#[test]
fn a_registration_that_leaves_the_listing_is_forgotten_and_one_outside_the_prefix_is_ignored() {
    let mut seen = Registrations::default();
    let stray = Listed {
        key: "_tiering/usage".into(),
        size: 1,
        modified: 1,
    };
    seen.observe(0, &[listed("a", 1, 1), stray]);
    seen.observe(100, &[]);
    seen.observe(900_000, &[listed("a", 1, 1)]);
    assert!(!seen.departed("a", 1_499_999, WAIT));
    assert!(seen.departed("a", 1_500_000, WAIT));
}
