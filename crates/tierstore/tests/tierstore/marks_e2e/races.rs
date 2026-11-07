//! Two proxies on one key: the writes race and the last writer wins, and no write that was
//! acknowledged is lost. Each race is made exact by holding one store call at a gate, and a proxy
//! that waits for a lock is let go on by what the test sees it do, not by time.

use std::sync::Mutex;
use std::time::Duration;

use tierstore::{GateBackend, Phase, PutBody, PutClaim};

use super::*;

/// The key of a test.
const K: &str = "builds/k";

/// A held plain put.
async fn overwrite(proxy: &Proxy, text: &str) -> Result<tierstore::PlainPut, Error> {
    proxy
        .put_plain(K, PutBody::Held(body(text)), &PutClaim::default())
        .await
}

/// Asserts that two plain puts of a key that neither found, both of which looked before either
/// wrote, are both acknowledged, and that the later one's bytes are the key's: the second finds its
/// creation refused, looks again, and overwrites. The first one's bytes were read after its
/// acknowledgement and before the second's.
#[tokio::test]
async fn two_plain_puts_that_both_found_no_key_end_with_the_second_ones_bytes_and_both_are_acknowledged()
 {
    let world = World::new().await;
    let p1 = world.proxy("p1").await;
    let slow = Arc::new(GateBackend::new(world.hot.clone()));
    let p2 = world.proxy_over(slow.clone(), world.config("p2")).await;
    let gate = slow.gate(Op::Head, K, Phase::Post);
    let second = tokio::spawn({
        let p2 = Arc::clone(&p2);
        async move { overwrite(&p2, "from p2").await }
    });
    gate.parked().await;

    overwrite(&p1, "from p1").await.unwrap();
    assert_eq!(served(&p1, K).await.as_deref(), Ok("from p1"));
    assert!(gate.release());
    second.await.unwrap().unwrap();
    assert_eq!(served(&p1, K).await.as_deref(), Ok("from p2"));
    assert_eq!(served(&p2, K).await.as_deref(), Ok("from p2"));
    released(&world, K).await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
    assert_eq!(world.leader("r1").step().await.changed, 1);
    assert_eq!(world.cold_text(K).await.as_deref(), Some("from p2"));
}

/// Asserts that a create-only put that found no key, and a plain put of the key that landed first,
/// end with the plain put's bytes and the create-only put answered `AlreadyExists` (412): a create-only
/// put never replaces an acknowledged object.
#[tokio::test]
async fn a_create_only_put_that_lost_the_creation_to_a_plain_put_is_412_and_the_plain_put_stands() {
    let world = World::new().await;
    let p1 = world.proxy("p1").await;
    let slow = Arc::new(GateBackend::new(world.hot.clone()));
    let p2 = world.proxy_over(slow.clone(), world.config("p2")).await;
    let gate = slow.gate(Op::Head, K, Phase::Post);
    let second = tokio::spawn({
        let p2 = Arc::clone(&p2);
        async move { p2.put(K, body("created")).await }
    });
    gate.parked().await;

    overwrite(&p1, "plain").await.unwrap();
    assert!(gate.release());
    assert_eq!(
        second.await.unwrap(),
        Err(Error::AlreadyExists(K.to_owned()))
    );
    assert_eq!(served(&p2, K).await.as_deref(), Ok("plain"));
}

/// Asserts that an overwrite that meets another overwrite's lock waits for it and then writes, and
/// does not fail or lose: the first is acknowledged, the second takes the lock after it is let go,
/// and the key ends with the second's bytes, `DIRTY` for the copier.
#[tokio::test]
async fn an_overwrite_waits_for_the_lock_of_another_overwrite_and_then_wins() {
    let world = World::new().await;
    let held = Arc::new(GateBackend::new(world.hot.clone()));
    let (config, _) = world.config_counting("p1");
    let p1 = world.proxy_over(held.clone(), config).await;
    let (config, waits) = world.config_counting("p2");
    let p2 = world.proxy_with(config).await;
    p1.put(K, body("v0")).await.unwrap();
    released(&world, K).await;
    let gate = held.gate(Op::PutIfMatch, K, Phase::Pre);
    let first = tokio::spawn({
        let p1 = Arc::clone(&p1);
        async move { overwrite(&p1, "first").await }
    });
    gate.parked().await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Lock, false)));

    let second = tokio::spawn({
        let p2 = Arc::clone(&p2);
        async move { overwrite(&p2, "second").await }
    });
    waiting(&waits).await;
    assert_eq!(served(&p2, K).await.as_deref(), Ok("v0"));
    assert!(gate.release());
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(served(&p1, K).await.as_deref(), Ok("second"));
    released(&world, K).await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, false)));
    // The second writer replaces by the tag of its first look, which the first writer changed: its
    // replace is refused once, it reads the tag again and replaces again.
    assert_eq!(world.hot.calls_on(Op::PutIfMatch, K), 3);
}

/// Asserts that a delete that meets an overwrite's lock waits for it and then deletes: the
/// overwrite is acknowledged first, the delete after, and the key is then absent to both proxies
/// while the cold tier has not yet been told.
#[tokio::test]
async fn a_delete_waits_for_the_lock_of_an_overwrite_and_the_key_ends_absent() {
    let world = World::new().await;
    let held = Arc::new(GateBackend::new(world.hot.clone()));
    let (config, _) = world.config_counting("p1");
    let p1 = world.proxy_over(held.clone(), config).await;
    let (config, waits) = world.config_counting("p2");
    let p2 = world.proxy_with(config).await;
    p1.put(K, body("v0")).await.unwrap();
    assert_eq!(world.leader("r1").step().await.changed, 1);
    let gate = held.gate(Op::PutIfMatch, K, Phase::Pre);
    let first = tokio::spawn({
        let p1 = Arc::clone(&p1);
        async move { overwrite(&p1, "v1").await }
    });
    gate.parked().await;

    let second = tokio::spawn({
        let p2 = Arc::clone(&p2);
        async move { p2.delete(K).await }
    });
    waiting(&waits).await;
    assert!(gate.release());
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    released(&world, K).await;
    for proxy in [&p1, &p2] {
        assert_eq!(served(proxy, K).await, Err(Error::NotFound(K.to_owned())));
    }
    assert_eq!(world.cold_text(K).await.as_deref(), Some("v0"));
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, true)));
}

/// Asserts that an overwrite that meets a delete's lock, after it had found the key, waits for the
/// delete, then writes the key anew where the delete removed it, and is acknowledged: the put
/// happened after the delete, so the key is the put's, and the delete's `gone` is kept for the
/// copier, which ends it once cold holds the new object.
#[tokio::test]
async fn an_overwrite_that_waited_for_a_delete_recreates_the_key_and_the_cold_copy_follows() {
    let world = World::new().await;
    let held = Arc::new(GateBackend::new(world.hot.clone()));
    let (config, _) = world.config_counting("p1");
    let p1 = world.proxy_over(held.clone(), config).await;
    let (config, waits) = world.config_counting("p2");
    let p2 = world.proxy_with(config).await;
    p1.put(K, body("v0")).await.unwrap();
    let mut leader = world.leader("r1");
    assert_eq!(leader.step().await.changed, 1);
    let gate = held.gate(Op::Delete, K, Phase::Pre);
    let deleter = tokio::spawn({
        let p1 = Arc::clone(&p1);
        async move { p1.delete(K).await }
    });
    gate.parked().await;

    let writer = tokio::spawn({
        let p2 = Arc::clone(&p2);
        async move { overwrite(&p2, "after").await }
    });
    waiting(&waits).await;
    assert!(gate.release());
    deleter.await.unwrap().unwrap();
    writer.await.unwrap().unwrap();
    assert_eq!(served(&p1, K).await.as_deref(), Ok("after"));
    assert_eq!(served(&p2, K).await.as_deref(), Ok("after"));
    released(&world, K).await;
    assert_eq!(world.shape(K).await, Some((MarkerState::Dirty, true)));

    world.converge(&mut leader, K).await;
    assert_eq!(world.cold_text(K).await.as_deref(), Some("after"));
    assert_eq!(served(&p1, K).await.as_deref(), Ok("after"));
}

/// What a write of the sequence below did, as the key holds it afterwards.
#[derive(Clone, Copy, Debug)]
enum Write {
    /// A create-only put by proxy 0 or 1.
    Create(usize, &'static str),
    /// A plain put by proxy 0 or 1.
    Plain(usize, &'static str),
    /// A delete by proxy 0 or 1.
    Delete(usize),
    /// A pass of the leader, after the clocks moved on.
    Leader,
}

/// Asserts, for a sequence of writes from two proxies with the leader's passes between them, that
/// after each step both proxies read what the last acknowledged write left: a key that a
/// create-only put found existing keeps its bytes, a delete leaves the key absent whatever cold
/// holds, a put after it brings it back, and the cold tier ends equal to the model.
#[tokio::test]
async fn a_sequence_of_writes_from_two_proxies_with_the_leader_between_reads_the_same_from_both() {
    let world = World::new().await;
    let proxies = [world.proxy("p1").await, world.proxy("p2").await];
    let mut leader = world.leader("r1");
    let steps = [
        Write::Create(0, "a"),
        Write::Plain(1, "b"),
        Write::Leader,
        Write::Create(1, "refused"),
        Write::Delete(0),
        Write::Plain(1, "c"),
        Write::Leader,
        Write::Delete(1),
        Write::Leader,
        Write::Create(0, "d"),
        Write::Delete(0),
        Write::Leader,
        Write::Leader,
        Write::Plain(0, "e"),
        Write::Plain(1, "f"),
        Write::Leader,
    ];
    let mut model: Option<&str> = None;
    for step in steps {
        match step {
            Write::Create(by, text) => {
                let put = proxies[by].put(K, body(text)).await;
                if model.is_some() {
                    assert_eq!(put, Err(Error::AlreadyExists(K.to_owned())), "{step:?}");
                } else {
                    put.unwrap();
                    model = Some(text);
                }
            }
            Write::Plain(by, text) => {
                overwrite(&proxies[by], text).await.unwrap();
                model = Some(text);
            }
            Write::Delete(by) => {
                proxies[by].delete(K).await.unwrap();
                model = None;
            }
            Write::Leader => {
                world.pass(Duration::from_secs(121));
                let report = leader.step().await;
                assert!(report.failed.is_empty(), "{report:?}");
            }
        }
        released(&world, K).await;
        for proxy in &proxies {
            let read = served(proxy, K).await.ok();
            assert_eq!(read.as_deref(), model, "after {step:?}");
        }
    }
    world.converge(&mut leader, K).await;
    assert_eq!(world.cold_text(K).await.as_deref(), model);
    assert_eq!(world.hot_text(K).await.as_deref(), model);
}

/// Asserts, with the stores slow enough that the writes overlap, that eight plain puts of one key
/// from two proxies are all acknowledged and that the key ends with the bytes of the one
/// acknowledged last: no acknowledged write is lost and none is acknowledged and then overtaken by an
/// earlier one.
#[tokio::test(start_paused = true)]
async fn eight_overlapping_plain_puts_from_two_proxies_end_with_the_bytes_of_the_last_acknowledged()
{
    let world = World::new().await;
    // The proxies first: a proxy is live after its first beat, which waits for the stores.
    let proxies = [
        world.proxy_with(world.config_sleeping("p1")).await,
        world.proxy_with(world.config_sleeping("p2")).await,
    ];
    world
        .hot
        .set_delay(Op::Head, Some(Duration::from_millis(3)));
    world
        .hot
        .set_delay(Op::PutIfMatch, Some(Duration::from_millis(7)));
    world.hot.set_delay(Op::Put, Some(Duration::from_millis(5)));
    world
        .marks
        .set_delay(Op::Get, Some(Duration::from_millis(3)));
    world
        .marks
        .set_delay(Op::Put, Some(Duration::from_millis(8)));
    world
        .marks
        .set_delay(Op::PutIfMatch, Some(Duration::from_millis(8)));
    let order = Arc::new(Mutex::new(Vec::new()));
    let mut tasks = Vec::new();
    for n in 0..8 {
        let proxy = Arc::clone(&proxies[n % 2]);
        let order = Arc::clone(&order);
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(u64::try_from(n).unwrap())).await;
            let put = overwrite(&proxy, &format!("writer {n}")).await;
            if put.is_ok() {
                order.lock().unwrap().push(n);
            }
            put
        }));
    }
    let mut refused = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => {}
            // A 503 (`SlowDown`): a writer that lost its races for the lock more often than
            // `entry_rounds`, or waited out `lock_wait`. It was not acknowledged.
            Err(Error::Unavailable(_)) => refused += 1,
            Err(other) => panic!("{other:?}"),
        }
    }
    let order = order.lock().unwrap().clone();
    assert_eq!(order.len() + refused, 8);
    let last = *order.last().expect("a put was acknowledged");
    assert_eq!(
        served(&proxies[0], K).await,
        Ok(format!("writer {last}")),
        "acknowledged in the order {order:?}"
    );
}
