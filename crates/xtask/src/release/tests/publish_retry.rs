//! `release publish` when GitHub fails the create of the draft.

use super::*;
use crate::release::publish::server_error;

/// What `gh` prints when GitHub answers the create of a release with `status`.
fn failed_with(status: &str) -> String {
    format!("gh release create: HTTP {status} (https://api.github.com/repos/{REPO}/releases)")
}

/// The verb of every `gh` run: `create`, `delete`, `edit`.
fn verbs(gh: &FakeGh) -> Vec<String> {
    gh.ran.borrow().iter().map(|a| a[1].clone()).collect()
}

#[test]
fn a_create_github_fails_with_a_server_error_is_tried_again_from_no_draft() {
    // Catches: a release job left red by one HTTP 500 of GitHub's, and a second create on top
    // of the draft the failed one left (two releases of one tag, or a draft with no files).
    let dir = scratch("publish-retry-500");
    assets(&dir);
    let gh = FakeGh::default();
    gh.create_fails
        .borrow_mut()
        .push((failed_with("500"), true));
    let printed = publish_in(&gh, &dir, "v0.3.0").expect("publish");
    assert_eq!(verbs(&gh), ["create", "delete", "create", "edit"]);
    assert_eq!(
        printed[printed.len() - 2],
        format!(
            "creating the draft v0.3.0 failed with a server error ({}): attempt 2 of 3",
            failed_with("500")
        )
    );
    assert_eq!(
        printed[printed.len() - 1],
        format!("published v0.3.0 at {COMMIT} with 33 files")
    );
    let releases = gh.releases.borrow();
    assert_eq!(releases.len(), 1, "{releases:?}");
    assert_eq!(releases[0]["draft"], false);
    assert_eq!(releases[0]["assets"].as_array().map(Vec::len), Some(33));
}

#[test]
fn a_server_error_that_left_no_draft_is_tried_again_without_a_delete() {
    // Catches: a delete asked of GitHub for a draft that is not there (which `gh` refuses).
    let dir = scratch("publish-retry-502");
    assets(&dir);
    let gh = FakeGh::default();
    gh.create_fails
        .borrow_mut()
        .extend([(failed_with("502"), false), (failed_with("503"), false)]);
    let printed = publish_in(&gh, &dir, "v0.3.0").expect("publish");
    assert_eq!(verbs(&gh), ["create", "create", "create", "edit"]);
    assert!(
        printed.iter().any(|l| l.ends_with("attempt 3 of 3")),
        "{printed:?}"
    );
    assert_eq!(gh.releases.borrow()[0]["draft"], false);
}

#[test]
fn three_server_errors_fail_the_publish_with_the_last_and_publish_nothing() {
    // Catches: a create tried without end, and a release published after the last attempt
    // failed.
    let dir = scratch("publish-retry-give-up");
    assets(&dir);
    let gh = FakeGh::default();
    gh.create_fails.borrow_mut().extend([
        (failed_with("500"), true),
        (failed_with("502"), false),
        (failed_with("504"), true),
    ]);
    let e = publish_in(&gh, &dir, "v0.3.0").expect_err("three server errors");
    assert_eq!(
        e.to_string(),
        format!(
            "{} (the last of 3 attempts, each a server error)",
            failed_with("504")
        )
    );
    assert_eq!(verbs(&gh), ["create", "delete", "create", "create"]);
    // The draft the last attempt left is the next run's to replace
    // (`publish_replaces_a_draft_a_failed_run_left`); nothing is published.
    let releases = gh.releases.borrow();
    assert_eq!(releases.len(), 1, "{releases:?}");
    assert_eq!(releases[0]["draft"], true);
}

#[test]
fn a_create_github_refuses_with_a_client_error_is_not_tried_again() {
    // Catches: a request GitHub refuses as such (HTTP 422, a bad tag or target) sent twice
    // more, and an error that is not GitHub's (gh missing, no network) taken for a server error.
    for message in [
        failed_with("422"),
        "gh release create: error connecting to api.github.com".to_owned(),
    ] {
        let dir = scratch("publish-retry-422");
        assets(&dir);
        let gh = FakeGh::default();
        gh.create_fails.borrow_mut().push((message.clone(), false));
        let e = publish_in(&gh, &dir, "v0.3.0").expect_err("client error");
        assert_eq!(e.to_string(), message);
        assert_eq!(verbs(&gh), ["create"], "{message}");
        assert!(gh.releases.borrow().is_empty());
    }
}

#[test]
fn a_client_error_after_a_server_error_ends_the_attempts() {
    // Catches: a 4xx on the second attempt tried a third time, or reported as the server
    // error of the first.
    let dir = scratch("publish-retry-then-422");
    assets(&dir);
    let gh = FakeGh::default();
    gh.create_fails
        .borrow_mut()
        .extend([(failed_with("500"), false), (failed_with("422"), false)]);
    let e = publish_in(&gh, &dir, "v0.3.0").expect_err("client error");
    assert_eq!(e.to_string(), failed_with("422"));
    assert_eq!(verbs(&gh), ["create", "create"]);
    assert!(gh.releases.borrow().is_empty());
}

#[test]
fn a_server_error_is_an_http_status_of_500_to_599_and_nothing_else() {
    // Catches: a status read from the wrong place, a 4xx or a longer number taken for a 5xx,
    // and a message cut short read past its end.
    for (message, server) in [
        ("HTTP 500 (https://api.github.com/x)", true),
        ("gh: Server Error (HTTP 502)", true),
        ("HTTP 599", true),
        ("HTTP 404: Not Found", false),
        ("HTTP 499", false),
        ("HTTP 600", false),
        ("HTTP 5000", false),
        ("HTTP 50", false),
        ("HTTP 5x0", false),
        ("HTTP ", false),
        ("", false),
        (
            "HTTP 404 Not Found, then HTTP 503 Service Unavailable",
            true,
        ),
        ("connection reset by peer", false),
    ] {
        assert_eq!(server_error(message), server, "{message:?}");
    }
}
