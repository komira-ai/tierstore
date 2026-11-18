//! The suite against an object store in memory: a conforming store passes every case in both
//! answers to an identical write, and each way of breaking the contract is found by its case,
//! with the violation's whole text asserted.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod store;

use objstore_conformance::{Case, IdenticalPut, run};
use objstore_s3::KeyPrefix;

use crate::store::{Defect, Model};

fn prefix(p: &str) -> KeyPrefix {
    KeyPrefix::new(p).unwrap()
}

/// The text of the violation `case` finds in a store with `defect` that answers an identical
/// write as `identical` says, the suite expecting that answer.
async fn found(defect: Defect, case: Case, identical: IdenticalPut) -> String {
    let store = Model::new(identical, Some(defect));
    let found = case.check(&store, &prefix("t/"), identical).await;
    found.unwrap_err().to_string()
}

/// Asserts that a conforming store passes every case, whether it accepts or refuses an identical
/// write (and the suite expects that answer), that the report says so case by case in the order of
/// `Case::ALL`, and that the run leaves nothing behind.
#[tokio::test]
async fn a_conforming_store_passes_every_case() {
    for identical in [IdenticalPut::Accepted, IdenticalPut::Refused] {
        let store = Model::new(identical, None);
        let report = run(&store, &prefix("run/1/"), identical).await;
        assert!(report.passed(), "{report}");
        assert_eq!(report.violations().count(), 0);
        assert_eq!(
            report.to_string(),
            "PASS claims\nPASS create-only\nPASS identical-put\nPASS create-race\n\
             PASS streamed\nPASS range\nPASS head\nPASS delete\nPASS list\nPASS pages\n"
        );
        assert_eq!(store.keys(), Vec::<String>::new());
    }
}

/// Asserts that the claims case finds a store that is not create-only, one that claims Object
/// Lock, one that takes a retention date it cannot keep, and one that refuses it for another
/// reason.
#[tokio::test]
async fn the_claims_case_finds_each_wrong_claim() {
    let refused = IdenticalPut::Refused;
    let cases = [
        (
            Defect::NoConditionalPut,
            "claims: the store does not claim conditional writes; the suite is of create-only stores",
        ),
        (
            Defect::ObjectLock,
            "claims: the store claims Object Lock, which the suite does not check",
        ),
        (
            Defect::KeepsRetention,
            "claims: put_new of t/retained succeeded; Unsupported was required",
        ),
        (
            Defect::RetentionAlreadyExists,
            "claims: put_new of t/retained was refused with \"object t/retained already exists\"; \
             Unsupported was required",
        ),
    ];
    for (defect, want) in cases {
        assert_eq!(
            found(defect, Case::Claims, refused).await,
            want,
            "{defect:?}"
        );
    }
}

/// Asserts that the create-only case finds a store that overwrites a used key with `put_new`, one
/// that does with `put_stream` alone, and one whose writes fail.
#[tokio::test]
async fn the_create_only_case_finds_an_overwrite_whole_or_streamed() {
    let refused = IdenticalPut::Refused;
    let cases = [
        (
            Defect::Overwrites,
            "create-only: put_new of t/object succeeded; AlreadyExists was required",
        ),
        (
            Defect::StreamOverwrites,
            "create-only: put_stream of t/object succeeded; AlreadyExists was required",
        ),
        (
            Defect::PutFails,
            "create-only: put_new of t/object failed: store answered 503 SlowDown: the model is down",
        ),
    ];
    for (defect, want) in cases {
        assert_eq!(
            found(defect, Case::CreateOnly, refused).await,
            want,
            "{defect:?}"
        );
    }
}

/// Asserts that the identical-write case and the race case find a store whose answer to an
/// identical write is not the one declared, in both directions.
#[tokio::test]
async fn an_identical_write_answered_otherwise_than_declared_is_found() {
    let prefix = prefix("t/");
    let accepts = Model::new(IdenticalPut::Accepted, None);
    let found = Case::IdenticalPut.check(&accepts, &prefix, IdenticalPut::Refused);
    assert_eq!(
        found.await.unwrap_err().to_string(),
        "identical-put: put_new of t/object succeeded; AlreadyExists was required"
    );
    let refuses = Model::new(IdenticalPut::Refused, None);
    let found = Case::IdenticalPut.check(&refuses, &prefix, IdenticalPut::Accepted);
    assert_eq!(
        found.await.unwrap_err().to_string(),
        "identical-put: put_new of t/object failed: object t/object already exists"
    );
    let refuses = Model::new(IdenticalPut::Refused, None);
    let found = Case::CreateRace.check(&refuses, &prefix, IdenticalPut::Accepted);
    assert_eq!(
        found.await.unwrap_err().to_string(),
        "create-race: 1 of 8 concurrent writers of t/same succeeded, expected 8"
    );
}

/// Asserts that the race case finds a store that checks a key and writes it in two steps (every
/// concurrent writer succeeds), and one that refuses a writer for another reason than the key
/// being used.
#[tokio::test]
async fn the_race_case_finds_a_check_then_write() {
    let refused = IdenticalPut::Refused;
    assert_eq!(
        found(Defect::RacyCheck, Case::CreateRace, refused).await,
        "create-race: 8 of 8 concurrent writers of t/different succeeded, expected 1"
    );
    assert_eq!(
        found(Defect::PutFails, Case::CreateRace, refused).await,
        "create-race: put_new of t/different was refused with \"store answered 503 SlowDown: \
         the model is down\"; AlreadyExists was required"
    );
}

/// Asserts that the streamed case finds a store that keeps what a short body gave, and one that
/// cuts a long body and stores it.
#[tokio::test]
async fn the_streamed_case_finds_a_body_of_the_wrong_length_stored() {
    let refused = IdenticalPut::Refused;
    assert_eq!(
        found(Defect::KeepsShortBody, Case::Streamed, refused).await,
        "streamed: len of t/short is 999, expected absent"
    );
    assert_eq!(
        found(Defect::CutsLongBody, Case::Streamed, refused).await,
        "streamed: put_stream of t/long succeeded; Body was required"
    );
}

/// Asserts that the range case finds a read past the end cut one byte short, a changed byte (and
/// says where), an empty read where the range starts at the end, and a read of an absent key
/// refused for another reason than `NotFound`.
#[tokio::test]
async fn the_range_case_finds_wrong_bytes_and_wrong_refusals() {
    let refused = IdenticalPut::Refused;
    let cases = [
        (
            Defect::CutsOneShort,
            "range: t/object [500, 1500): read 499 bytes, expected 500",
        ),
        (
            Defect::FlipsByte,
            "range: t/object [0, 1000): read 1000 bytes, expected 1000, first difference at 3",
        ),
        (
            Defect::EmptyAtEnd,
            "range: get_range of t/object succeeded; InvalidRange was required",
        ),
        (
            Defect::AbsentForbidden,
            "range: get_range of t/absent was refused with \"store answered 403 AccessDenied: \"; \
             NotFound was required",
        ),
    ];
    for (defect, want) in cases {
        assert_eq!(
            found(defect, Case::Range, refused).await,
            want,
            "{defect:?}"
        );
    }
}

/// Asserts that the HEAD case finds a wrong length and a HEAD that fails, and that the
/// create-only case's re-reads find a store whose reads change a byte.
#[tokio::test]
async fn the_head_case_finds_a_wrong_length() {
    let refused = IdenticalPut::Refused;
    assert_eq!(
        found(Defect::LongHead, Case::Head, refused).await,
        "head: len of t/empty is 1, expected 0"
    );
    assert_eq!(
        found(Defect::HeadFails, Case::Head, refused).await,
        "head: len of t/empty failed: store answered 500 InternalError: "
    );
    assert_eq!(
        found(Defect::FlipsByte, Case::CreateOnly, refused).await,
        "create-only: t/object [0, 65): read 64 bytes, expected 64, first difference at 3"
    );
}

/// Asserts that the delete case finds a delete of an absent key that fails, and a deleted key
/// still listed.
#[tokio::test]
async fn the_delete_case_finds_a_failed_delete_and_a_listed_ghost() {
    let refused = IdenticalPut::Refused;
    assert_eq!(
        found(Defect::DeleteAbsentFails, Case::Delete, refused).await,
        "delete: delete of t/object failed: object t/object not found"
    );
    assert_eq!(
        found(Defect::ListsDeleted, Case::Delete, refused).await,
        "delete: the listing of \"t/\" is [t/object (0)], expected []"
    );
}

/// Asserts that the list case finds a listing that leaves out a key that is not ASCII, one that
/// is not in key order, and one that still lists deleted keys.
#[tokio::test]
async fn the_list_case_finds_a_missing_key_disorder_and_ghosts() {
    let refused = IdenticalPut::Refused;
    let live = "t/a (1), t/a/b (3), t/b (0), t/d (9), t/e (6), t/k1 (11), t/k10 (10), t/k2 (2)";
    assert_eq!(
        found(Defect::DropsNonAscii, Case::List, refused).await,
        format!("list: the listing of \"t/\" is [{live}], expected [{live}, t/x y/caf\u{e9} (7)]")
    );
    assert_eq!(
        found(Defect::Unordered, Case::List, refused).await,
        "list: list of \"t/\" failed: store broke the protocol: listing \"t/\" returned t/k2 \
         after t/x y/caf\u{e9}: keys must ascend"
    );
    assert_eq!(
        found(Defect::ListsDeleted, Case::List, refused).await,
        format!(
            "list: the listing of \"t/\" is [t/a (1), t/a/b (3), t/b (0), t/c (0), t/d (9), \
             t/e (6), t/k1 (11), t/k10 (10), t/k2 (2), t/x y/caf\u{e9} (7)], expected [{live}, \
             t/x y/caf\u{e9} (7)]"
        )
    );
}

/// Asserts that the pages case finds a page longer than its `max_keys`, a listing that never
/// ends (stopped, and what it gave is not the listing), and a listing after a key that gives that
/// key again.
#[tokio::test]
async fn the_pages_case_finds_long_pages_endless_listings_and_a_repeated_key() {
    let refused = IdenticalPut::Refused;
    assert_eq!(
        found(Defect::IgnoresMaxKeys, Case::Pages, refused).await,
        "pages: a page of \"t/\" holds 5 keys, more than its max_keys 1"
    );
    let repeated = ["t/p1 (2)"; 9].join(", ");
    assert_eq!(
        found(Defect::NeverEnds, Case::Pages, refused).await,
        format!(
            "pages: the listing of \"t/\" is [t/p0 (1), {repeated}], expected [t/p0 (1), \
             t/p1 (2), t/p2 (3), t/p3 (4), t/p4 (5)]"
        )
    );
    assert_eq!(
        found(Defect::IncludesAfter, Case::Pages, refused).await,
        "pages: list of \"t/\" failed: store broke the protocol: listing \"t/\" returned t/p1 \
         after t/p1: keys must ascend"
    );
}

/// Asserts that a run reports a case's violation on its `FAIL` line and the other cases' `PASS`,
/// that a violation found only by the clean-up after a passing case (a deleted key still listed,
/// a listing out of order) is that case's, and that the run still removes what it can.
#[tokio::test]
async fn a_run_reports_each_case_and_the_violations_of_its_clean_up() {
    let store = Model::new(IdenticalPut::Refused, Some(Defect::Overwrites));
    let report = run(&store, &prefix("r/"), IdenticalPut::Refused).await;
    assert!(!report.passed());
    let failed: Vec<String> = report.violations().map(ToString::to_string).collect();
    assert_eq!(
        failed,
        [
            "create-only: put_new of r/create-only/object succeeded; AlreadyExists was required",
            "identical-put: put_new of r/identical-put/object succeeded; AlreadyExists was required",
            "create-race: 8 of 8 concurrent writers of r/create-race/different succeeded, expected 1",
        ]
    );
    assert!(report.to_string().starts_with(
        "PASS claims\nFAIL create-only: put_new of r/create-only/object succeeded; \
         AlreadyExists was required\n"
    ));
    assert_eq!(store.keys(), Vec::<String>::new());

    let ghosts = Model::new(IdenticalPut::Refused, Some(Defect::ListsDeleted));
    let report = run(&ghosts, &prefix("g/"), IdenticalPut::Refused).await;
    let first = report.outcomes[1].result.as_ref().unwrap_err().to_string();
    assert_eq!(
        first,
        "create-only: the listing of \"g/create-only/\" is [g/create-only/object (0)], expected []"
    );

    let unordered = Model::new(IdenticalPut::Refused, Some(Defect::Unordered));
    let report = run(&unordered, &prefix("u/"), IdenticalPut::Refused).await;
    let head = &report.outcomes[6];
    assert_eq!(head.case, Case::Head);
    assert_eq!(
        head.result.as_ref().unwrap_err().to_string(),
        "head: list of \"u/head/\" failed: store broke the protocol: listing \"u/head/\" \
         returned u/head/one after u/head/some: keys must ascend"
    );
}

/// Asserts that a root too long for a case's prefix, or for a key under it, is that case's
/// violation and not a panic or a key cut short.
#[tokio::test]
async fn a_root_too_long_for_a_key_is_a_violation() {
    let store = Model::new(IdenticalPut::Refused, None);
    // 1019 bytes: no case's prefix fits but `list/` (1024), and none of its keys does.
    let root = prefix(&"r".repeat(1019));
    let report = run(&store, &root, IdenticalPut::Refused).await;
    let texts: Vec<String> = report.violations().map(ToString::to_string).collect();
    assert_eq!(texts.len(), Case::ALL.len());
    assert_eq!(
        texts[0],
        "claims: a key under the root is refused: object key has 1026 bytes; at most 1024 are \
         allowed"
    );
    assert_eq!(
        texts[1],
        "create-only: a key under the root is refused: object key has 1031 bytes; at most 1024 \
         are allowed"
    );
    assert_eq!(
        texts[8],
        "list: a key under the root is refused: object key has 1026 bytes; at most 1024 are \
         allowed"
    );
    assert_eq!(store.keys(), Vec::<String>::new());
}

/// Asserts that `LiveSet` checks the live keys again after the writes, and that a store which
/// loses a key between the writes and the check (as a restart without its index would) fails it.
#[tokio::test]
async fn a_live_set_finds_a_key_lost_between_seed_and_check() {
    let store = Model::new(IdenticalPut::Refused, None);
    let p = prefix("s/");
    let set = objstore_conformance::LiveSet::seed(&store, &p)
        .await
        .unwrap();
    set.check(&store).await.unwrap();
    let lost = objstore_s3::ObjectKey::new("s/k10").unwrap();
    objstore_s3::ObjectStore::delete(&store, &lost)
        .await
        .unwrap();
    assert_eq!(
        set.check(&store).await.unwrap_err().to_string(),
        "list: the listing of \"s/\" is [s/a (1), s/a/b (3), s/b (0), s/d (9), s/e (6), \
         s/k1 (11), s/k2 (2), s/x y/caf\u{e9} (7)], expected [s/a (1), s/a/b (3), s/b (0), \
         s/d (9), s/e (6), s/k1 (11), s/k10 (10), s/k2 (2), s/x y/caf\u{e9} (7)]"
    );
}
