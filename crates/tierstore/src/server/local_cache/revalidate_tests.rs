//! The check of an old hit against the upstream (`revalidate_after`): when it is made, what it
//! sends, what each answer does to the entry and to the read, and that without the setting
//! nothing is checked. Time is the rig's clock, moved by hand; no test waits for a real time.

use std::time::Duration;

use super::index::Index;
use super::layout::{self, Sidecar};
use super::metrics::Revalidated;
use super::testing::{Rig, digest, pattern, tree, until};
use super::{Backend as _, Mode};
use crate::{ByteRange, Error, Op};

/// The setting of the rigs below, in milliseconds.
const AFTER_MS: u64 = 60_000;

/// What the fill of one object costs the upstream: one GET, and two HEADs (one before the first
/// chunk and one after the last, for a key that is not named by its content).
const FILL: (u64, u64) = (1, 2);

/// A cache that checks a hit older than [`AFTER_MS`].
fn checking(name: &str) -> Rig {
    Rig::with(name, |config| {
        config.revalidate_after = Some(Duration::from_millis(AFTER_MS));
    })
}

/// The GETs and HEADs the upstream has had.
fn upstream_calls(rig: &Rig) -> (u64, u64) {
    (rig.calls(Op::Get), rig.calls(Op::Head))
}

/// How many checks found `found`.
fn found(rig: &Rig, found: Revalidated) -> u64 {
    rig.cache.counters().revalidations(found)
}

/// The sum of all the checks.
fn checks(rig: &Rig) -> u64 {
    [
        Revalidated::Same,
        Revalidated::Changed,
        Revalidated::Gone,
        Revalidated::Failed,
    ]
    .into_iter()
    .map(|result| found(rig, result))
    .sum()
}

/// A rig whose upstream holds `key` with `body` and whose cache has read it once.
async fn filled(name: &str, key: &str, body: &[u8]) -> Rig {
    let rig = checking(name);
    rig.put(key, body).await;
    assert_eq!(rig.get(key).await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), FILL);
    rig
}

/// Asserts that a hit is not checked until it is older than the setting: at exactly the setting
/// a GET, a ranged GET and a HEAD are answered from the disk with no call to the upstream and
/// nothing is counted as a check.
#[tokio::test]
async fn a_hit_not_older_than_revalidate_after_makes_no_upstream_call() {
    let body = pattern(500);
    let rig = filled("tscache-rv-young", "cas/b/obj", &body).await;
    rig.clock.advance(AFTER_MS);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    let range = Some(ByteRange::new(10, 20));
    let part = rig.cache.get("cas/b/obj", range).await.unwrap();
    assert_eq!(part, &body[10..30]);
    assert_eq!(rig.cache.head("cas/b/obj").await.unwrap().size, 500);
    assert_eq!(upstream_calls(&rig), FILL);
    assert_eq!(checks(&rig), 0);
}

/// Asserts that `revalidate_after = "0s"` checks every hit: the hit in the very millisecond of the
/// fill and the ones after it, with the clock not moved, each send one HEAD and no GET, and are
/// served from the disk.
#[tokio::test]
async fn with_revalidate_after_zero_every_hit_is_checked_also_in_the_millisecond_of_the_last_check()
{
    let body = pattern(500);
    let rig = Rig::with("tscache-rv-zero", |config| {
        config.revalidate_after = Some(Duration::ZERO);
    });
    rig.put("cas/b/obj", &body).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), FILL);
    for hit in 1..=3 {
        assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
        assert_eq!(upstream_calls(&rig), (1, 2 + hit));
        assert_eq!(found(&rig, Revalidated::Same), hit);
    }
}

/// Asserts that a hit older than the setting is checked by one HEAD and served from the disk when
/// the upstream's answer is the entry's version: the body is not read again, the entry's time is
/// now, so the next hits send nothing until it is older again, and a ranged GET is checked as a
/// whole one is.
#[tokio::test]
async fn a_hit_older_than_revalidate_after_is_checked_by_a_head_and_served_without_a_body_download_on_a_match()
 {
    let body = pattern(500);
    let rig = filled("tscache-rv-match", "cas/b/obj", &body).await;
    rig.clock.advance(AFTER_MS + 1);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 3));
    assert_eq!(found(&rig, Revalidated::Same), 1);

    // The time of the entry is now: nothing more is sent for as long as the setting.
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    rig.clock.advance(AFTER_MS);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 3));

    rig.clock.advance(1);
    let range = Some(ByteRange::new(10, 20));
    let part = rig.cache.get("cas/b/obj", range).await.unwrap();
    assert_eq!(part, &body[10..30]);
    assert_eq!(upstream_calls(&rig), (1, 4));
    assert_eq!(found(&rig, Revalidated::Same), 2);
    assert_eq!(rig.cache.held(), (500, 1));
}

/// Asserts that a HEAD of an old hit is the check: one HEAD to the upstream, and the answer is
/// the entry's.
#[tokio::test]
async fn a_head_older_than_revalidate_after_is_checked_by_one_head_and_answered_from_the_entry() {
    let body = pattern(500);
    let rig = filled("tscache-rv-head", "cas/b/obj", &body).await;
    let kept = rig.cache.head("cas/b/obj").await.unwrap();
    rig.clock.advance(AFTER_MS + 1);
    assert_eq!(rig.cache.head("cas/b/obj").await.unwrap(), kept);
    assert_eq!(upstream_calls(&rig), (1, 3));
    assert_eq!(found(&rig, Revalidated::Same), 1);
    assert_eq!(rig.cache.head("cas/b/obj").await.unwrap(), kept);
    assert_eq!(upstream_calls(&rig), (1, 3));
}

/// Asserts that a check which finds another version drops the entry and the read goes upstream as
/// a miss: for the same size with other bytes (another tag) and for another size, the new bytes
/// are read, kept and served, and the next hit is a hit of the new object.
#[tokio::test]
async fn a_revalidation_that_finds_another_etag_drops_the_entry_and_goes_upstream() {
    let (old, same_size, longer) = (pattern(500), vec![9_u8; 500], vec![8_u8; 700]);
    let rig = filled("tscache-rv-changed", "cas/b/obj", &old).await;
    rig.replace("cas/b/obj", &same_size).await;
    rig.clock.advance(AFTER_MS + 1);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), same_size);
    assert_eq!(found(&rig, Revalidated::Changed), 1);
    // The check's HEAD, then the fill: one GET and its two HEADs.
    assert_eq!(upstream_calls(&rig), (2, 5));
    assert_eq!(rig.cache.held(), (500, 1));

    assert_eq!(rig.get("cas/b/obj").await.unwrap(), same_size);
    assert_eq!(upstream_calls(&rig), (2, 5));

    rig.replace("cas/b/obj", &longer).await;
    rig.clock.advance(AFTER_MS + 1);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), longer);
    assert_eq!(found(&rig, Revalidated::Changed), 2);
    assert_eq!(rig.cache.held(), (700, 1));
}

/// Asserts that a check which finds no object drops the entry and its files and the read is the
/// upstream's `NotFound`, and that the key put again later is read from the upstream.
#[tokio::test]
async fn a_revalidation_that_finds_a_missing_key_drops_the_entry_and_answers_not_found() {
    let rig = filled("tscache-rv-gone", "cas/b/obj", &pattern(500)).await;
    rig.tamper.delete("cas/b/obj").await.unwrap();
    rig.clock.advance(AFTER_MS + 1);
    let absent = Error::NotFound("cas/b/obj".to_owned());
    assert_eq!(rig.get("cas/b/obj").await, Err(absent));
    assert_eq!(found(&rig, Revalidated::Gone), 1);
    assert_eq!(rig.cache.held(), (0, 0));
    assert!(!rig.has_file("cas/b/obj"));

    rig.put("cas/b/obj", &[5_u8; 40]).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), vec![5_u8; 40]);
    assert_eq!(rig.cache.held(), (40, 1));
}

/// Asserts that a HEAD of an old hit answers with the upstream's HEAD when the object changed, in
/// that one call, and with `NotFound` when it is gone, and that both drop the entry.
#[tokio::test]
async fn a_head_that_finds_another_version_or_none_is_answered_by_the_check_itself() {
    let rig = filled("tscache-rv-head-changed", "cas/b/obj", &pattern(500)).await;
    rig.replace("cas/b/obj", &[3_u8; 500]).await;
    let (gets, heads) = upstream_calls(&rig);
    rig.clock.advance(AFTER_MS + 1);
    let answered = rig.cache.head("cas/b/obj").await.unwrap();
    assert_eq!(answered, rig.tamper.memory.head("cas/b/obj").await.unwrap());
    assert_eq!(upstream_calls(&rig), (gets, heads + 1));
    assert_eq!(rig.cache.held(), (0, 0));

    // Read again, kept again, old again, and then deleted.
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), vec![3_u8; 500]);
    rig.tamper.delete("cas/b/obj").await.unwrap();
    rig.clock.advance(AFTER_MS + 1);
    let absent = Error::NotFound("cas/b/obj".to_owned());
    assert_eq!(rig.cache.head("cas/b/obj").await, Err(absent));
    assert_eq!(rig.cache.held(), (0, 0));
    assert_eq!(found(&rig, Revalidated::Changed), 1);
    assert_eq!(found(&rig, Revalidated::Gone), 1);
}

/// Asserts that a HEAD that fails, for one call and then for an upstream that is down, serves the
/// cached copy and counts the failure; that the entry's time is not refreshed, so each next hit
/// asks again; and that the first check that gets an answer settles it.
#[tokio::test]
async fn a_revalidation_whose_head_fails_serves_the_cached_copy_counts_it_and_asks_again() {
    let body = pattern(500);
    let rig = filled("tscache-rv-fails", "cas/b/obj", &body).await;
    rig.clock.advance(AFTER_MS + 1);
    rig.upstream.fail_next(Op::Head, 1);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 3));
    assert_eq!(found(&rig, Revalidated::Failed), 1);

    rig.upstream.set_down(true);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 4));
    assert_eq!(found(&rig, Revalidated::Failed), 2);
    rig.upstream.set_down(false);

    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 5));
    assert_eq!(found(&rig, Revalidated::Same), 1);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 5));
    assert_eq!(rig.cache.held(), (500, 1));
}

/// Asserts that with the setting left out a hit never calls the upstream, however old, and
/// nothing is counted: the behaviour before the key.
#[tokio::test]
async fn with_revalidate_after_unset_a_hit_never_calls_the_upstream() {
    let rig = Rig::new("tscache-rv-unset");
    let body = pattern(500);
    rig.put("cas/b/obj", &body).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    rig.clock.advance(u64::MAX / 2);
    rig.tamper.delete("cas/b/obj").await.unwrap();
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(rig.cache.head("cas/b/obj").await.unwrap().size, 500);
    assert_eq!(upstream_calls(&rig), FILL);
    assert_eq!(checks(&rig), 0);
}

/// Asserts that the hits that come while a check is out are served from the disk and send no
/// check of their own: one HEAD an interval for an object however many readers it has.
#[tokio::test]
async fn hits_that_arrive_while_a_check_is_out_are_served_and_send_none_of_their_own() {
    let body = pattern(500);
    let rig = filled("tscache-rv-herd", "cas/b/obj", &body).await;
    rig.clock.advance(AFTER_MS + 1);
    let gate = rig.tamper.hold_heads();
    let cache = rig.cache.clone();
    let first = tokio::spawn(async move { cache.get("cas/b/obj", None).await });
    until("the check is out", || rig.tamper.heading() == 1).await;

    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(rig.cache.head("cas/b/obj").await.unwrap().size, 500);
    assert_eq!(upstream_calls(&rig), (1, 3));

    gate.add_permits(1);
    assert_eq!(first.await.unwrap().unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 3));
    assert_eq!(found(&rig, Revalidated::Same), 1);
}

/// Asserts that a check whose request is dropped before the upstream answers (a client that went
/// away) leaves the entry as old as it was, so that the next hit checks it.
#[tokio::test]
async fn a_check_whose_request_is_dropped_is_given_back_and_the_next_hit_asks_again() {
    let body = pattern(500);
    let rig = filled("tscache-rv-dropped", "cas/b/obj", &body).await;
    rig.clock.advance(AFTER_MS + 1);
    let gate = rig.tamper.hold_heads();
    let cache = rig.cache.clone();
    let first = tokio::spawn(async move { cache.get("cas/b/obj", None).await });
    until("the check is out", || rig.tamper.heading() == 1).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(found(&rig, Revalidated::Same), 0);

    gate.add_permits(1);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 4));
    assert_eq!(found(&rig, Revalidated::Same), 1);
}

/// Asserts that an entry found on the disk at start, whose age is not known, is checked by its
/// first hit whatever the clock says, and then by the interval.
#[tokio::test]
async fn an_entry_found_on_the_disk_at_start_is_checked_by_its_first_hit() {
    let body = pattern(500);
    let rig = filled("tscache-rv-start", "cas/b/obj", &body).await;
    let reopened = rig.reopen();
    assert_eq!(reopened.held(), (500, 1));
    assert_eq!(reopened.get("cas/b/obj", None).await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 3));
    assert_eq!(reopened.get("cas/b/obj", None).await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 3));
    assert_eq!(reopened.counters().revalidations(Revalidated::Same), 1);
}

/// Asserts that an entry read back from the disk at start is compared by the tag its sidecar
/// holds, not by its size: the object written over with other bytes of the same length while the
/// process was down is read again by the first hit, and the next hit is served from the disk.
#[tokio::test]
async fn an_entry_found_on_the_disk_at_start_is_compared_by_the_tag_of_its_sidecar() {
    let rig = filled("tscache-rv-start-tag", "cas/b/obj", &pattern(500)).await;
    rig.replace("cas/b/obj", &[6_u8; 500]).await;
    let reopened = rig.reopen();
    assert_eq!(reopened.held(), (500, 1));
    assert_eq!(
        reopened.get("cas/b/obj", None).await.unwrap(),
        vec![6_u8; 500]
    );
    assert_eq!(reopened.counters().revalidations(Revalidated::Changed), 1);
    assert_eq!(upstream_calls(&rig), (2, 5));

    assert_eq!(
        reopened.get("cas/b/obj", None).await.unwrap(),
        vec![6_u8; 500]
    );
    assert_eq!(upstream_calls(&rig), (2, 5));
    assert_eq!(reopened.held(), (500, 1));
}

/// Asserts that an entry whose sidecar holds no tag (nothing the cache writes, but a sidecar is a
/// file) is never taken for the upstream's version: its first hit reads the object again and the
/// entry then has the upstream's tag.
#[tokio::test]
async fn an_entry_whose_sidecar_has_an_empty_tag_is_read_again_by_its_first_hit() {
    let body = pattern(500);
    let rig = filled("tscache-rv-no-tag", "cas/b/obj", &body).await;
    let path = layout::meta_path(&rig.config.path, &layout::name_of("cas/b/obj").hex);
    let mut sidecar: Sidecar = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    sidecar.tag.clear();
    std::fs::write(&path, serde_json::to_vec(&sidecar).unwrap()).unwrap();

    let reopened = rig.reopen();
    assert_eq!(reopened.get("cas/b/obj", None).await.unwrap(), body);
    assert_eq!(reopened.counters().revalidations(Revalidated::Changed), 1);
    assert_eq!(upstream_calls(&rig), (2, 5));
    let healed: Sidecar = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        healed.tag,
        rig.tamper.memory.head("cas/b/obj").await.unwrap().tag
    );
}

/// Asserts that an entry whose time is after the clock's (the clock was set back) is checked
/// rather than trusted for the time the clock takes to catch up.
#[tokio::test]
async fn an_entry_stamped_after_the_clock_now_is_checked() {
    let rig = checking("tscache-rv-clock-back");
    rig.clock.set(10 * AFTER_MS);
    let body = pattern(500);
    rig.put("cas/b/obj", &body).await;
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    rig.clock.set(10);
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 3));
    assert_eq!(rig.get("cas/b/obj").await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 3));
}

/// Asserts that a key named by its content is checked like any other: an object that was deleted
/// at the upstream is not served once its entry is old.
#[tokio::test]
async fn a_key_named_by_its_content_that_was_deleted_is_not_served_past_the_interval() {
    let body = pattern(500);
    let key = format!("cas/l/{}", digest(&body));
    let rig = filled_content("tscache-rv-content", &key, &body).await;
    rig.tamper.delete(&key).await.unwrap();
    rig.clock.advance(AFTER_MS);
    assert_eq!(rig.get(&key).await.unwrap(), body);
    rig.clock.advance(1);
    assert_eq!(rig.get(&key).await, Err(Error::NotFound(key.clone())));
    assert_eq!(rig.cache.held(), (0, 0));
}

/// As `filled`, for a key named by its content, whose fill makes one HEAD (for the size) and one
/// GET.
async fn filled_content(name: &str, key: &str, body: &[u8]) -> Rig {
    let rig = checking(name);
    rig.put(key, body).await;
    assert_eq!(rig.get(key).await.unwrap(), body);
    assert_eq!(upstream_calls(&rig), (1, 1));
    rig
}

/// Asserts that a read-only cache (another process holds the directory) checks too: it finds the
/// object changed, forgets it without removing the holder's files, and serves the new bytes from
/// the upstream.
#[tokio::test]
async fn a_read_only_cache_that_finds_a_changed_object_forgets_it_and_leaves_the_files() {
    let rig = filled("tscache-rv-read-only", "cas/b/obj", &pattern(500)).await;
    let holder = rig.locked();
    let reader = rig.locked();
    assert_eq!(
        (holder.mode(), reader.mode()),
        (Mode::ReadWrite, Mode::ReadOnly)
    );
    rig.replace("cas/b/obj", &[4_u8; 500]).await;
    let files = tree(&rig.config.path);

    assert_eq!(
        reader.get("cas/b/obj", None).await.unwrap(),
        vec![4_u8; 500]
    );
    assert_eq!(reader.counters().revalidations(Revalidated::Changed), 1);
    assert_eq!(reader.held(), (0, 0));
    assert_eq!(tree(&rig.config.path), files);
}

/// Asserts that the counter of the checks is on `/metricz`, one series for each result.
#[tokio::test]
async fn the_metrics_count_the_checks_by_result() {
    let rig = filled("tscache-rv-metrics", "cas/b/obj", &pattern(500)).await;
    rig.clock.advance(AFTER_MS + 1);
    let _ = rig.get("cas/b/obj").await.unwrap();
    let text = rig.cache.metrics();
    for (result, count) in [("same", 1), ("changed", 0), ("gone", 0), ("failed", 0)] {
        let line = format!("tierstore_cache_revalidations_total{{result=\"{result}\"}} {count}\n");
        assert!(text.contains(&line), "{line}: {text}");
    }
}

/// Asserts when the index grants a check: never for an absent entry or one not older than the
/// interval; for one older, for any entry when the interval is 0, for one never checked, and for one stamped after `now`; that a grant
/// sets the entry's time so that a second finds nothing due; and that giving a grant back puts
/// the time back only while the entry still has the time the grant set.
#[test]
fn the_index_grants_one_check_to_an_entry_that_is_due_and_takes_back_only_its_own() {
    let mut index = Index::default();
    let tagged = |size| (size, "tag".to_owned(), 1);
    assert!(index.claim("a", 10, 5).is_none());

    index.insert("a", tagged(4), Some(100));
    assert!(index.claim("a", 105, 5).is_none());
    let (meta, before) = index.claim("a", 106, 5).expect("older than the interval");
    assert_eq!(
        (meta.size, meta.tag.as_str(), before),
        (4, "tag", Some(100))
    );
    assert!(index.claim("a", 106, 5).is_none());

    // Not the time the grant set: another check has settled the entry since.
    index.unclaim("a", 999, None);
    assert!(index.claim("a", 106, 5).is_none());
    index.unclaim("absent", 106, None);
    index.unclaim("a", 106, before);
    assert!(index.claim("a", 106, 5).is_some());

    // An interval of 0 is a check for every hit, also one at the time of the entry.
    assert!(index.claim("a", 106, 0).is_some());
    assert!(index.claim("a", 106, 0).is_some());

    index.insert("never", tagged(1), None);
    assert!(index.claim("never", 0, u64::MAX).is_some());
    index.insert("future", tagged(1), Some(500));
    assert!(index.claim("future", 10, u64::MAX).is_some());
}
