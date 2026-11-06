//! A proxy that restarts under its proxy id, and the entry its earlier process left held: the
//! entry names the boot of the process that made it, the registration the next process writes at
//! open names its own, and the reconciler reaps the entry at once.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use super::entry_put_tests::{Rig, body, held_by, pending};
use super::heartbeat_tests::Stepped;
use crate::faulty::Op;
use crate::journal::proxy_key;
use crate::registration::Registration;
use crate::{
    Backend, EntryReconciler, EntryReconcilerConfig, Error, Proxy, ProxyConfig, ReconcilerConfig,
    Sleeper,
};

/// A wait that never ends: no test here sees a renewal of the lease.
struct Pending;

#[async_trait]
impl Sleeper for Pending {
    async fn sleep(&self, _duration: Duration) {
        std::future::pending::<()>().await;
    }
}

/// The boot that the registration of `p1` names in the hot tier.
async fn registered_boot(rig: &Rig) -> String {
    let stored = rig.hot_mem.get(&proxy_key("p1"), None).await.unwrap();
    Registration::decode(&stored).unwrap().boot
}

/// Asserts, with the last step tried once (`step_attempts = 1`), from the proxy's own writes to
/// the reconciler's step, that a put that cannot hand its
/// entry over leaves it held with the boot of its proxy's registration; that a process of the same
/// proxy id opened after writes a registration with another boot, which names the earlier boot as
/// a peer (it may be alive), so that the reconciler waits; that the second process drops the
/// peer after six acknowledged beats (the departure time of 60 seconds in beats of 10), the sixth
/// beat's body being the last to name it; and that the reconciler then reaps the entry at its next
/// step, with no clock moved (an entry of the process that is running is never reaped), and copies
/// the object to the cold tier.
#[tokio::test]
async fn a_put_left_held_by_a_proxy_that_restarts_is_reaped_at_the_step_after_the_peer_is_dropped()
{
    let rig = Rig::single_try().await;
    let first_boot = registered_boot(&rig).await;
    let down = Error::Unavailable("down".to_owned());
    rig.hot_faults
        .fail_next_on(Op::PutIfMatch, "_entries/", down);
    assert!(rig.proxy.put("k", body("v")).await.is_err());
    let held = rig.entry("k").await.unwrap();
    assert_eq!(held.entry.owner_boot.as_deref(), Some(first_boot.as_str()));
    assert_eq!(rig.shape("k").await, held_by("p1"));

    let mut base = ReconcilerConfig::new("r1", 1 << 20, rig.clock.clone());
    base.mono = rig.mono.clone();
    base.sleeper = Arc::new(Pending);
    let mut reconciler = EntryReconciler::new(
        rig.hot_mem.clone(),
        rig.cold_mem.clone(),
        EntryReconcilerConfig::new(base),
    );
    assert_eq!(reconciler.step().await.pinned, 1);

    let config = ProxyConfig::new("p1", super::entry_put_tests::BUDGET, rig.clock.clone());
    let second = Proxy::open(rig.hot_mem.clone(), rig.cold_mem.clone(), config)
        .await
        .unwrap();
    assert_ne!(registered_boot(&rig).await, first_boot);
    assert_eq!(reconciler.step().await.pinned, 1);

    let sleeper = Stepped::new();
    assert!(second.start_heartbeat(sleeper.clone()));
    sleeper.beat_done().await;
    for _ in 2..=6 {
        sleeper.next();
        sleeper.beat_done().await;
        assert_eq!(reconciler.step().await.pinned, 1);
    }
    sleeper.next();
    sleeper.beat_done().await;
    assert_eq!(reconciler.step().await.changed, 1);
    assert_eq!(rig.shape("k").await, pending());
    assert_eq!(reconciler.step().await.changed, 1);
    assert!(rig.cold_has("k").await);
    assert!(rig.entry("k").await.is_none());
    second.stop_heartbeat().await;
}
