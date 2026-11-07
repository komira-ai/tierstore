//! One test per scenario of the check: the property each scenario is evidence for holds in every
//! reachable state (`holds_in_*`), or the checker rejects the wrong variant of the protocol with a
//! counterexample of the rule named (`rejects_*`). The `heavy/` scenarios (three operations per
//! proxy, two crashes, up to a million states) run here too: the whole file takes seconds,
//! because the crate is built with optimisations in test builds (root `Cargo.toml`).

use tierstore_model::scenarios::verify;

fn check(id: &str) {
    if let Err(line) = verify(id) {
        panic!("{line}");
    }
}

/// A put whose marker landed before a delete's tombstone cleanup (A7).
#[test]
fn rejects_a7_in_doc_fast_put_delete_get() {
    check("doc/fast-put-delete-get");
}

/// A put's marker bump after a delete resurrects the deleted object (A1).
#[test]
fn rejects_a1_in_doc_create_only_delete() {
    check("doc/create-only-delete");
}

/// A read that missed hot before a recreate serves the deleted cold copy (A2).
#[test]
fn rejects_a2_in_doc_recreate_read() {
    check("doc/recreate-read");
}

/// A create-only put lands between a delete's gone lock and its hot DELETE (A1).
#[test]
fn rejects_a1_in_doc_cold_only_delete_create() {
    check("doc/cold-only-delete-create");
}

/// fixed/fast-put-delete-get (A1 A2 A3 A6 A7).
#[test]
fn holds_in_fixed_fast_put_delete_get() {
    check("fixed/fast-put-delete-get");
}

/// fixed/create-only-delete (A1 A5 A21).
#[test]
fn holds_in_fixed_create_only_delete() {
    check("fixed/create-only-delete");
}

/// fixed/recreate-read (A2 A12).
#[test]
fn holds_in_fixed_recreate_read() {
    check("fixed/recreate-read");
}

/// fixed/cold-only-delete-create (A1 A17 A21).
#[test]
fn holds_in_fixed_cold_only_delete_create() {
    check("fixed/cold-only-delete-create");
}

/// Two overwrites of an uncopied key, a get, a list and an eviction (A1 A6 A8).
#[test]
fn holds_in_overwrite_race() {
    check("overwrite-race");
}

/// Two If-Match puts with one tag, a get and a delete (A5).
#[test]
fn holds_in_if_match_race() {
    check("if-match-race");
}

/// If-Match of an evicted key (resolved from cold) (A5).
#[test]
fn holds_in_if_match_cold_only() {
    check("if-match-cold-only");
}

/// An overwrite and a delete of an uncopied key against gets and lists (A1 A2 A8).
#[test]
fn holds_in_overwrite_vs_delete() {
    check("overwrite-vs-delete");
}

/// Two deletes and puts of a copied key (A1 A23).
#[test]
fn holds_in_delete_race() {
    check("delete-race");
}

/// Gets racing a delete, the copier's cold delete and tombstone hold (A13).
#[test]
fn holds_in_read_pair_after_delete() {
    check("read-pair-after-delete");
}

/// Two puts at hot Full, a get and a delete (A15).
#[test]
fn holds_in_cold_land() {
    check("cold-land");
}

/// A put at hot Full against a create-only put: both can win, and the cold landing's hot DELETE then removes the other (A15).
#[test]
fn rejects_a5_in_residual_cold_land_vs_create_only() {
    check("residual/cold-land-vs-create-only");
}

/// A list whose hot page missed before a copy-back reads the cold object a cold landing wrote while hot still holds the older one (A15 A2).
#[test]
fn rejects_a2_in_residual_cold_land_vs_stale_list() {
    check("residual/cold-land-vs-stale-list");
}

/// A put at hot Full against a plain put: reads see 3, 5, 3 (A15).
#[test]
fn rejects_a1_in_residual_cold_land_vs_plain_put() {
    check("residual/cold-land-vs-plain-put");
}

/// A put at hot Full over a deleted key whose cold copy remains (A12 A15).
#[test]
fn holds_in_cold_land_over_deleted() {
    check("cold-land-over-deleted");
}

/// The same with the lock taking over the gone marker as 5.5 says (A12 A15).
#[test]
fn rejects_a12_in_lit_cold_land_over_deleted() {
    check("lit/cold-land-over-deleted");
}

/// A plain put against a delete of an evicted key (A1 A2).
#[test]
fn holds_in_cold_only_plain_put() {
    check("cold-only-plain-put");
}

/// A copy-back against a put and a delete (A17).
#[test]
fn holds_in_copy_back() {
    check("copy-back");
}

/// An eviction between the checks and the landing of a create-only put (A16 A21).
#[test]
fn holds_in_eviction_vs_create_only() {
    check("eviction-vs-create-only");
}

/// A one-shot stream that loses the creation race (A6 A24).
#[test]
fn holds_in_stream_loses_race() {
    check("stream-loses-race");
}

/// A delete while the copier is stalled mid-copy (A23).
#[test]
fn holds_in_stalled_copier() {
    check("stalled-copier");
}

/// Lock writes whose answers are lost (A20).
#[test]
fn holds_in_ambiguous_locks() {
    check("ambiguous-locks");
}

/// An old-release proxy under active-marks (A22).
#[test]
fn holds_in_old_release_fails_closed() {
    check("old-release-fails-closed");
}

/// An old-release proxy running the entry protocol beside markers (A22).
#[test]
fn rejects_a2_in_old_release_beside_markers() {
    check("old-release-beside-markers");
}

/// Crash of a proxy or the leader during puts, a delete and gets (A4 A7 A18).
#[test]
fn holds_in_crash_fast_put() {
    check("crash/fast-put");
}

/// Crash holding an OVERWRITE lock (A4 A18).
#[test]
fn holds_in_crash_overwrite_lock() {
    check("crash/overwrite-lock");
}

/// Crash holding a DELETE lock, reaped keeping the `gone` it found (5.13, pull request 7).
#[test]
fn holds_in_crash_delete_lock_rolled_back() {
    check("crash/delete-lock-rolled-back");
}

/// Crash holding a DELETE lock (A4 A18).
#[test]
fn holds_in_crash_delete_lock() {
    check("crash/delete-lock");
}

/// Crash holding a COLDLAND lock (A4 A18).
#[test]
fn holds_in_crash_cold_land_lock() {
    check("crash/cold-land-lock");
}

/// The leader dies holding an EVICT lock (A4 A18).
#[test]
fn holds_in_crash_evict_lock() {
    check("crash/evict-lock");
}

/// The leader dies holding a COPYBACK lock (A4 A18 A17).
#[test]
fn holds_in_crash_copy_back_lock() {
    check("crash/copy-back-lock");
}

/// A stalled delete's hot DELETE lands after a newer put (A19 (R1)).
#[test]
fn rejects_a8_in_ghost_hot_delete() {
    check("ghost/hot-delete");
}

/// The same with the exclusion assertion off: the acknowledged newer put is deleted (A19 (R1)).
#[test]
fn rejects_a1_in_ghost_hot_delete_damage() {
    check("ghost/hot-delete-damage");
}

/// A stalled put's object lands after its orphan marker was cleared (A19 (R3')).
#[test]
fn rejects_a7_in_ghost_hot_create() {
    check("ghost/hot-create");
}

/// A stalled leader's cold write lands after the tombstone pass (A10 A19 (R2')).
#[test]
fn rejects_a1_in_ghost_cold_write() {
    check("ghost/cold-write");
}

/// A stalled leader's marker DELETE erases a marker written after its CLEARING (A19 (R4')).
#[test]
fn rejects_a9_in_ghost_marker_delete() {
    check("ghost/marker-delete");
}

/// A stalled leader's cold write over a newer cold object fails on its If-Match (A10 A19).
#[test]
fn holds_in_ghost_cold_write_overwrite() {
    check("ghost/cold-write-overwrite");
}

/// A stalled overwrite's If-Match replace lands late (A8 A19).
#[test]
fn rejects_a7_in_ghost_hot_replace() {
    check("ghost/hot-replace");
}

/// A stalled marker create or CAS lands late (A19).
#[test]
fn holds_in_ghost_marker_write() {
    check("ghost/marker-write");
}

/// The fast path writes no marker (A7).
#[test]
fn rejects_a7_in_negative_no_marker_on_put() {
    check("negative/no-marker-on-put");
}

/// The copier deletes the marker without the CAS (A9 A10).
#[test]
fn rejects_a9_in_negative_clear_without_cas() {
    check("negative/clear-without-cas");
}

/// The evictor deletes hot without the lock (A8 off, to show the loss) (A16).
#[test]
fn rejects_a1_in_negative_evict_without_lock() {
    check("negative/evict-without-lock");
}

/// The same, with the exclusion assertion on (A8 A16).
#[test]
fn rejects_a8_in_negative_evict_without_lock_a8() {
    check("negative/evict-without-lock-a8");
}

/// The parallel read pair with no tombstone hold (A13).
#[test]
fn rejects_a2_in_negative_no_hold() {
    check("negative/no-hold");
}

/// The gone flag written after the hot DELETE (A14).
#[test]
fn rejects_a1_in_negative_delete_gone_late() {
    check("negative/delete-gone-late");
}

/// A cold landing without the lock (A15).
#[test]
fn rejects_a8_in_negative_cold_land_without_lock() {
    check("negative/cold-land-without-lock");
}

/// The copy-back lands with a plain PUT (A17).
#[test]
fn rejects_a1_in_negative_copy_back_plain_put() {
    check("negative/copy-back-plain-put");
}

/// A delete that leaves no gone marker behind (A23).
#[test]
fn rejects_a1_in_negative_delete_leaves_no_marker() {
    check("negative/delete-leaves-no-marker");
}

/// A hot-miss read that ignores the marker (A2 A9).
#[test]
fn rejects_a2_in_negative_read_ignores_marker() {
    check("negative/read-ignores-marker");
}

/// A writer that meets a live lock answers 503 at once (A6).
#[test]
fn rejects_a6_in_negative_immediate_503() {
    check("negative/immediate-503");
}

/// A lost-race stream goes on as an overwrite of bytes it no longer has (A24).
#[test]
fn rejects_a24_in_negative_stream_continues() {
    check("negative/stream-continues");
}

/// An ambiguous lock write assumed to have landed (A20).
#[test]
fn rejects_a8_in_negative_assume_lock_on_unknown() {
    check("negative/assume-lock-on-unknown");
}

/// A delete that waits while the copier is mid-copy (A23).
#[test]
fn rejects_a23_in_negative_delete_waits_for_copier() {
    check("negative/delete-waits-for-copier");
}

/// No evict_min_age: an eviction between the checks and the landing (A21).
#[test]
fn rejects_a5_in_negative_no_evict_min_age() {
    check("negative/no-evict-min-age");
}

/// Evict_min_age counted from the object's creation, not from the copy (A21).
#[test]
fn rejects_a5_in_lit_eviction_age_from_landing() {
    check("lit/eviction-age-from-landing");
}

/// An OVERWRITE lock reaped to DIRTY(gone=false) over a deleted key whose cold copy remains (A12 A18).
#[test]
fn rejects_a12_in_lit_reap_overwrite_gone() {
    check("lit/reap-overwrite-gone");
}

/// If-Match compares once, before the lock (A5).
#[test]
fn rejects_a5_in_negative_if_match_no_recheck() {
    check("negative/if-match-no-recheck");
}

/// An overwrite that takes no lock (A8).
#[test]
fn rejects_a8_in_negative_overwrite_without_lock() {
    check("negative/overwrite-without-lock");
}

/// A stalled holder's plain replace (A8).
#[test]
fn rejects_a8_in_negative_plain_replace_ghost() {
    check("negative/plain-replace-ghost");
}

/// A stalled holder's plain replace puts its bytes over a newer acknowledged version (A8).
#[test]
fn rejects_a7_in_negative_plain_replace_ghost_damage() {
    check("negative/plain-replace-ghost-damage");
}

/// The copier's cold write is a plain PUT (A10).
#[test]
fn rejects_a7_in_negative_copier_plain_cold_put() {
    check("negative/copier-plain-cold-put");
}

/// An orphan marker cleared while the put is still in flight (A7).
#[test]
fn rejects_a7_in_negative_no_orphan_guard() {
    check("negative/no-orphan-guard");
}

/// Reads finish while a dead owner's lock is held and no leader reaps it (A11).
#[test]
fn holds_in_reads_never_wait() {
    check("reads-never-wait");
}

/// A read that waits for a lock (A11).
#[test]
fn rejects_a11_in_negative_read_waits_on_lock() {
    check("negative/read-waits-on-lock");
}

/// A writer meets a dead owner's lock: 503 after lock_wait (A11).
#[test]
fn holds_in_lock_wait_ends() {
    check("lock-wait-ends");
}

/// A writer with no lock_wait behind a dead owner (A11).
#[test]
fn rejects_a11_in_negative_wait_forever() {
    check("negative/wait-forever");
}

/// The leader never reaps a dead owner's lock (A4 A18).
#[test]
fn rejects_a4_in_negative_no_reap() {
    check("negative/no-reap");
}

/// The fast path clears gone before its object has landed (A12).
#[test]
fn rejects_a12_in_negative_gone_cleared_first() {
    check("negative/gone-cleared-first");
}

/// A copy-back that takes no lock lands over a delete (A17).
#[test]
fn rejects_a1_in_negative_copy_back_without_lock() {
    check("negative/copy-back-without-lock");
}

/// A create-only put that does not ask cold (A21).
#[test]
fn rejects_a5_in_negative_create_only_ignores_cold() {
    check("negative/create-only-ignores-cold");
}

/// A create-only put that answers 412 from what wave 1 read while a delete completes (A5).
#[test]
fn rejects_a5_in_negative_create_only_no_recheck() {
    check("negative/create-only-no-recheck");
}

/// The same for a key only cold has (A5).
#[test]
fn rejects_a5_in_negative_create_only_no_recheck_cold_only() {
    check("negative/create-only-no-recheck-cold-only");
}

/// A create-only put of a copied key against a delete that completes first is 409 or 200, never
/// 412 for an absent key (A5).
#[test]
fn holds_in_create_only_recheck_hot_key() {
    check("create-only-recheck/hot-key");
}

/// The same for an evicted key (A5 A21).
#[test]
fn holds_in_create_only_recheck_cold_only_key() {
    check("create-only-recheck/cold-only-key");
}

/// A gone marker with hot present is a recreate: the re-check answers 412 for it, not 409 (A5).
#[test]
fn holds_in_create_only_recheck_recreated_key() {
    check("create-only-recheck/recreated-key");
}

/// Four writes and two reads from an empty key, with an eviction (A1 A2 A3 A5 A6 A7).
#[test]
fn holds_in_heavy_puts_delete_reads() {
    check("heavy/puts-delete-reads");
}

/// Overwrites, If-Match puts, a delete and lists of an uncopied key (A1 A5 A6 A8).
#[test]
fn holds_in_heavy_overwrite_if_match() {
    check("heavy/overwrite-if-match");
}

/// A crash of any actor during puts, a delete and reads (A4 A7 A18).
#[test]
fn holds_in_heavy_crash_puts_delete() {
    check("heavy/crash-puts-delete");
}

/// An evicted key: create-only, delete, put, reads and copy-backs (A17 A21).
#[test]
fn holds_in_heavy_cold_only_copy_back() {
    check("heavy/cold-only-copy-back");
}

/// Two crashes of any actors during overwrites, a delete and reads (A4 A7 A18).
#[test]
fn holds_in_heavy_two_crashes() {
    check("heavy/two-crashes");
}

/// Lost answers of lock writes during puts, a delete and reads (A20).
#[test]
fn holds_in_heavy_ambiguity() {
    check("heavy/ambiguity");
}

/// Puts at Full among overwrites, a delete and reads of a deleted key (A12 A15 A16).
#[test]
fn holds_in_heavy_cold_land_mix() {
    check("heavy/cold-land-mix");
}

/// Any stalled call landing late among puts and a delete (A19).
#[test]
fn rejects_a8_in_heavy_ghost_any() {
    check("heavy/ghost-any");
}

/// Write-once puts, deletes and gets against the copier, a copy-back and an eviction (A1 A2 A3 A5).
#[test]
fn holds_in_baseline_put_delete_get() {
    check("baseline/put-delete-get");
}

/// A delete against a put and the copy of it (A1 A2 A23).
#[test]
fn holds_in_baseline_delete_waits_for_copy() {
    check("baseline/delete-waits-for-copy");
}

/// Puts, deletes and gets of an evicted key (A5).
#[test]
fn holds_in_baseline_cold_only() {
    check("baseline/cold-only");
}

/// A crash of any actor between any two steps (A4 A7 A18).
#[test]
fn holds_in_baseline_crash() {
    check("baseline/crash");
}

/// A delete that does not wait for the copy it amended (A2).
#[test]
fn rejects_a1_in_baseline_negative_delete_no_wait() {
    check("baseline-negative/delete-no-wait");
}

/// A put that does not check the tiers under its entry (A5).
#[test]
fn rejects_a5_in_baseline_negative_put_no_check() {
    check("baseline-negative/put-no-check");
}

/// A put acknowledged before its object landed (A1).
#[test]
fn rejects_a1_in_baseline_negative_ack_before_landing() {
    check("baseline-negative/ack-before-landing");
}

/// The leader copies without taking the entry first (A1 A2).
#[test]
fn rejects_a1_in_baseline_negative_copy_without_held() {
    check("baseline-negative/copy-without-held");
}

/// The first draft: a slow put's marker is cleared as an orphan before its object lands (A7).
#[test]
fn rejects_a7_in_doc_slow_put_orphan() {
    check("doc/slow-put-orphan");
}

/// A slow put re-asserts its marker after the copier cleared it as an orphan (A7 A4).
#[test]
fn holds_in_slow_put_orphan() {
    check("slow-put/orphan");
}

/// A slow put whose marker was created before a delete lands its object during the tombstone hold (A7 A2).
#[test]
fn holds_in_slow_put_gone_branch() {
    check("slow-put/gone-branch");
}

/// A delete whose hot DELETE times out keeps its lock until the queued call is gone (A8 A1).
#[test]
fn holds_in_timeout_delete_keeps_lock() {
    check("timeout/delete-keeps-lock");
}

/// A delete whose hot DELETE times out and releases its lock at once: the queued call deletes a newer put (A8).
#[test]
fn rejects_a8_in_timeout_release_at_once() {
    check("timeout/release-at-once");
}

/// An old-release put and delete beside markers delete a copied key the copier then rewrites (A22).
#[test]
fn rejects_a1_in_rollout_old_release_put_delete_beside_markers() {
    check("rollout/old-release-put-delete-beside-markers");
}

/// An old-release put beside markers writes an object no marker covers (A22).
#[test]
fn rejects_a7_in_rollout_old_release_put_beside_markers() {
    check("rollout/old-release-put-beside-markers");
}

/// An overwrite without the lock, with the lock assertion off, still breaks no acknowledged
/// operation when its replace finds the object moved (A8 A1).
#[test]
fn holds_in_negative_overwrite_without_lock_damage() {
    check("negative/overwrite-without-lock-damage");
}

/// A put that HEADs hot before it bumps a DIRTY marker keeps the deleted cold copy hidden (A12).
#[test]
fn holds_in_head_before_bump() {
    check("head-before-bump");
}

/// Both proxies of the entry protocol crash one after the other (A4 A7 A18).
#[test]
fn holds_in_baseline_two_client_crashes() {
    check("baseline/two-client-crashes");
}

/// The leader of the entry protocol dies and a successor takes over (A4 A18).
#[test]
fn holds_in_baseline_leader_crash() {
    check("baseline/leader-crash");
}

/// Only the leader dies, at every step, during puts, a delete and gets (A4 A18).
#[test]
fn holds_in_crash_leader_only() {
    check("crash/leader-only");
}

/// An If-Match put of an evicted key whose cold copy is replaced and evicted while it waits
/// answers 409 or 200, never an unexplained 412 (A5).
#[test]
fn holds_in_if_match_cold_resolve() {
    check("if-match-cold-resolve");
}
