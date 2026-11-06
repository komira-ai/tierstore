//! A held put or delete whose owner's process has restarted: the registration of the owner names
//! another boot than the entry does (and no peer of that boot), so the reconciler reaps the entry
//! at its next step instead of waiting for `held_ttl`. No test moves the monotonic clock: what is
//! asserted is that the reconciler does not wait for it.

use super::*;
use crate::registration::{PROTO, Registration};

impl Rig {
    /// The calls of the process `boot` of the proxy `id`: its entries name `boot` as their owner's.
    fn process(&self, id: &str, boot: &str) -> Entries {
        Entries::with_boot(self.hot.clone(), id, self.clock.clone(), boot)
    }

    /// The registration of the proxy `id` as the process `boot` writes it for its beat `hb`, naming
    /// the processes `peers` as alive too.
    async fn register_peers(&self, id: &str, boot: &str, hb: u64, peers: &[&str]) {
        let peers = Some(peers.iter().map(|peer| (*peer).to_owned()).collect());
        let body = Registration {
            proto: PROTO,
            boot: boot.to_owned(),
            hb,
            peers,
        }
        .encode();
        self.hot.delete(&proxy_key(id)).await.ok();
        self.hot.put(&proxy_key(id), body).await.unwrap();
    }
}

impl Rig {
    /// As [`Rig::register_peers`], with no peers.
    async fn register_boot(&self, id: &str, boot: &str, hb: u64) {
        self.register_peers(id, boot, hb, &[]).await;
    }
}

fn take_of(op: EntryOp) -> Take {
    if op == EntryOp::Put {
        Take::Put
    } else {
        Take::Del
    }
}

/// Asserts that a held put or delete made by an earlier process of a proxy is reaped by the next
/// step once the proxy's registration names the boot of a later process and not that of the entry,
/// with the monotonic clock where it was: while the registration still names the boot of the entry
/// the owner is waited for.
/// The put is then copied to the cold tier and the delete is finished in both tiers, and the earlier
/// process, if it comes back, finds its entry changed: its landing or hand-over is lost, so that
/// the put is not acknowledged.
#[tokio::test]
async fn a_held_entry_of_a_process_its_owners_registration_no_longer_names_is_reaped_at_the_next_step()
 {
    for op in [EntryOp::Put, EntryOp::Del] {
        let rig = Rig::new();
        rig.activate().await;
        rig.hot.put("k", bytes("v")).await.unwrap();
        rig.cold.put("k", bytes("v")).await.unwrap();
        rig.register_boot("p1", "boot-1", 4).await;
        let created = rig
            .process("p1", "boot-1")
            .create("k", take_of(op))
            .await
            .unwrap();
        assert!(matches!(created, Created::Taken(_)));
        let taken = rig.entry("k").await.unwrap();
        let mut r = rig.reconciler();
        assert_eq!(r.step().await.pinned, 1);

        rig.register_boot("p1", "boot-2", 0).await;
        assert_eq!(r.step().await, report(1));
        let reaped = rig.entry("k").await.unwrap();
        assert_eq!(
            (reaped.entry.op, reaped.entry.st),
            (op, EntryState::Pending)
        );
        assert_eq!(reaped.entry.owner_boot, None);

        let late = if op == EntryOp::Put {
            Step::LandPut
        } else {
            Step::Handover
        };
        let answer = rig
            .process("p1", "boot-1")
            .advance(&taken, late)
            .await
            .unwrap();
        assert!(matches!(answer, Outcome::Lost));
        assert_eq!(rig.entry("k").await.unwrap().tag, reaped.tag);

        assert_eq!(r.step().await, report(1));
        assert!(rig.entry("k").await.is_none());
        assert_eq!(rig.hot_has("k").await, op == EntryOp::Put);
        assert_eq!(rig.cold_has("k").await, op == EntryOp::Put);
    }
}

/// Asserts that the put of a restarted owner has the uploads it left open aborted, as the put of a
/// departed owner has.
#[tokio::test]
async fn reaping_the_put_of_a_restarted_owner_aborts_its_open_uploads() {
    let rig = Rig::new();
    rig.activate().await;
    for tier in [&rig.hot, &rig.cold] {
        tier.create_upload("k").await.unwrap();
    }
    rig.register_boot("p1", "boot-1", 1).await;
    rig.process("p1", "boot-1")
        .create("k", Take::Put)
        .await
        .unwrap();
    rig.register_boot("p1", "boot-2", 0).await;
    assert_eq!(rig.reconciler().step().await, report(1));
    for tier in [&rig.hot, &rig.cold] {
        assert!(tier.list_uploads("k", None, 10).await.unwrap().is_empty());
    }
}

/// Asserts that an owner that is not restarted is waited for: its registration is rewritten at every
/// beat with the boot its entry names and a higher count, and step after step the entry stays
/// held by it, for a put and for a delete, and no store object is changed.
#[tokio::test]
async fn a_held_entry_of_an_owner_that_goes_on_beating_in_the_same_process_is_waited_for() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("p", bytes("v")).await.unwrap();
    rig.hot.put("d", bytes("v")).await.unwrap();
    rig.register_boot("p1", "boot-1", 1).await;
    let process = rig.process("p1", "boot-1");
    process.create("p", Take::Put).await.unwrap();
    process.create("d", Take::Del).await.unwrap();
    let mut r = rig.reconciler();
    for hb in 2..8 {
        assert_eq!((r.step().await.pinned, rig.hot_has("d").await), (2, true));
        rig.register_boot("p1", "boot-1", hb).await;
    }
    let held = |owner: &str| Some((EntryOp::Put, EntryState::Held, Some(owner.to_owned())));
    assert_eq!(rig.shape("p").await, held("p1"));
    assert!(!rig.cold_has("p").await);
}

/// Asserts that a registration which cannot be compared leaves the owner waited for as before: an
/// entry that names no boot (one an earlier release wrote) beside a registration with one, an
/// entry that names a boot beside the empty-object registration of an earlier release, beside a body
/// that is not a registration, and beside a registration that says nothing of its peers (the
/// release before `peers`, whose process may be one that overwrites the registration of a live
/// process of this release).
#[tokio::test]
async fn an_entry_or_a_registration_that_names_no_boot_is_never_taken_for_a_restart() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("old", bytes("v")).await.unwrap();
    rig.hot.put("new", bytes("v")).await.unwrap();
    rig.hot.put("odd", bytes("v")).await.unwrap();
    rig.register_boot("p1", "boot-2", 3).await;
    rig.plant("old", EntryOp::Put, EntryState::Held, Some("p1"))
        .await;
    rig.register("p2").await;
    rig.process("p2", "boot-1")
        .create("new", Take::Put)
        .await
        .unwrap();
    rig.hot
        .put(&proxy_key("p3"), bytes("not json"))
        .await
        .unwrap();
    rig.process("p3", "boot-1")
        .create("odd", Take::Put)
        .await
        .unwrap();
    rig.hot.put("mixed", bytes("v")).await.unwrap();
    let before = bytes(r#"{"proto":2,"boot":"boot-2","hb":3}"#);
    rig.hot.put(&proxy_key("p4"), before).await.unwrap();
    rig.process("p4", "boot-1")
        .create("mixed", Take::Put)
        .await
        .unwrap();
    let mut r = rig.reconciler();
    let step = r.step().await;
    assert_eq!((step.pinned, step.changed, step.failed.len()), (4, 0, 0));
}

/// Asserts that two live processes of one proxy id (a hand-off: the new one has taken the sockets
/// and the old one drains) do not have their entries taken for those of a restart, whichever of
/// them wrote the registration last, because each registration names the other as a peer; and
/// that once the old one is named no more, its entries are reaped at the next step while the
/// new one's are not.
#[tokio::test]
async fn the_entries_of_two_live_processes_of_one_proxy_id_are_waited_for_until_one_is_named_no_more()
 {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("old", bytes("v")).await.unwrap();
    rig.hot.put("new", bytes("v")).await.unwrap();
    rig.register_peers("p1", "boot-1", 4, &[]).await;
    let old = rig.process("p1", "boot-1");
    old.create("old", Take::Put).await.unwrap();
    rig.register_peers("p1", "boot-2", 0, &["boot-1"]).await;
    let new = rig.process("p1", "boot-2");
    new.create("new", Take::Put).await.unwrap();
    let mut r = rig.reconciler();
    assert_eq!(r.step().await.pinned, 2);
    rig.register_peers("p1", "boot-1", 5, &["boot-2"]).await;
    assert_eq!(r.step().await.pinned, 2);
    rig.register_peers("p1", "boot-2", 1, &["boot-1"]).await;
    assert_eq!(r.step().await.pinned, 2);

    rig.register_peers("p1", "boot-2", 7, &[]).await;
    assert_eq!(
        r.step().await,
        StepReport {
            pinned: 1,
            ..report(1)
        }
    );
    assert_eq!(
        rig.shape("old").await,
        Some((EntryOp::Put, EntryState::Pending, None))
    );
    assert_eq!(
        rig.shape("new").await,
        Some((EntryOp::Put, EntryState::Held, Some("p1".to_owned())))
    );
}

/// Asserts that a restarted owner's entry is reaped only if it is still the entry the reconciler
/// read: the earlier process, not gone after all, lands its put while the reap is parked at its
/// write; its landing wins (the put is acknowledged), the reap writes nothing over it and is no
/// failure, the entry is as the landing made it, and the next step copies the object to the cold
/// tier, so no acknowledged put is lost.
#[tokio::test]
async fn the_reap_of_a_restarted_owners_entry_loses_to_a_landing_that_changed_the_entry() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.register_boot("p1", "boot-1", 4).await;
    let old = rig.process("p1", "boot-1");
    let created = old.create("k", Take::Put).await.unwrap();
    assert!(matches!(created, Created::Taken(_)));
    let held = rig.entry("k").await.unwrap();
    rig.register_boot("p1", "boot-2", 0).await;
    let gated = Arc::new(GateBackend::new(rig.hot.clone()));
    let gate = gated.gate(Op::PutIfMatch, ENTRIES_PREFIX, Phase::Pre);
    let mut r = rig.over(gated, rig.cold.clone(), "r1", |_| {});
    let mut step = Box::pin(r.step());
    park(&mut step, &gate).await;

    let won = old.advance(&held, Step::LandPut).await.unwrap();
    assert!(matches!(won, Outcome::Done(_)));
    let landed = rig.entry("k").await.unwrap();
    gate.release();
    let done = step.await;
    assert_eq!((done.changed, done.failed.len()), (1, 0));
    assert_eq!(rig.entry("k").await.unwrap(), landed);
    assert!(!rig.cold_has("k").await);
    assert_eq!(r.step().await, report(1));
    assert!(rig.cold_has("k").await);
    assert!(rig.entry("k").await.is_none());
}

/// Asserts what the check costs: every held entry of one owner is served by one read of the
/// registration, in the step, and a step with no held entry of another owner reads none.
#[tokio::test]
async fn a_step_reads_the_registration_of_an_owner_once_and_none_when_nothing_is_held() {
    let rig = Rig::new();
    rig.activate().await;
    let faults = Arc::new(FaultyBackend::new(rig.hot.clone()));
    rig.register_boot("p1", "boot-1", 1).await;
    land(&rig, "p1", "landed").await;
    let mut r = rig.over(faults.clone(), rig.cold.clone(), "r1", |_| {});
    r.step().await;
    assert_eq!(faults.calls_on(Op::Get, &proxy_key("p1")), 0);

    let process = rig.process("p1", "boot-1");
    for key in ["a", "b", "c"] {
        process.create(key, Take::Del).await.unwrap();
    }
    assert_eq!(r.step().await.pinned, 3);
    assert_eq!(faults.calls_on(Op::Get, &proxy_key("p1")), 1);
}

/// Asserts that a registration that cannot be read fails the step (the error is in the report), and
/// leaves the entry held and waited for; once it can be read again, the restart is seen and the
/// entry is reaped. A registration that is gone between the listing and the read says nothing: the
/// owner is departed, which the step judges from the listing.
#[tokio::test]
async fn a_registration_that_cannot_be_read_fails_the_step_and_leaves_the_entry_waited_for() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.register_boot("p1", "boot-1", 1).await;
    rig.process("p1", "boot-1")
        .create("k", Take::Put)
        .await
        .unwrap();
    rig.register_boot("p1", "boot-2", 0).await;
    let faults = Arc::new(FaultyBackend::new(rig.hot.clone()));
    let down = Error::Unavailable("down".to_owned());
    faults.fail_next_on(Op::Get, &proxy_key("p1"), down.clone());
    let mut r = rig.over(faults.clone(), rig.cold.clone(), "r1", |_| {});
    let failed = r.step().await;
    assert_eq!(
        (failed.pinned, failed.changed, failed.failed),
        (1, 0, vec![down])
    );
    assert_eq!(r.step().await, report(1));
}

/// Asserts that an owner with no registration to read is no failure: the entry is reaped at once as
/// that of a departed owner, and the step reports no error.
#[tokio::test]
async fn an_owner_with_no_registration_to_read_is_departed_and_no_failure() {
    let rig = Rig::new();
    rig.activate().await;
    rig.hot.put("k", bytes("v")).await.unwrap();
    rig.process("p9", "boot-1")
        .create("k", Take::Put)
        .await
        .unwrap();
    let step = rig.reconciler().step().await;
    assert_eq!((step.changed, step.failed.len()), (1, 0));
}
