//! The HEAD a create-only put sends to the upstream before its body (`head_before_put`): the
//! requests the upstream gets with it and without it, and what the put answers either way.

use bytes::Bytes;
use futures::StreamExt as _;
use futures::stream;

use super::metrics::read;
use super::testing::Rig;
use crate::{Backend, Error, Op, PartBody};

fn whole(text: &'static str) -> PartBody<'static> {
    let items: Vec<Result<Bytes, Error>> = vec![Ok(Bytes::from_static(text.as_bytes()))];
    stream::iter(items).boxed()
}

/// Asserts that a put with `head_before_put` on asks the upstream for the key before it sends the
/// body (the default, and the requests of a cache before the setting), and that with it off the
/// upstream is asked nothing before the put: its only HEAD is the keep's, after the put.
#[tokio::test]
async fn a_put_asks_the_upstream_for_the_key_first_only_when_head_before_put_is_on() {
    for (on, heads) in [(true, 2), (false, 1)] {
        let rig = Rig::with("tscache-keep-head", |config| {
            config.write_through = true;
            config.head_before_put = on;
        });
        let made = rig.cache.put_stream("cas/b/new", Some(3), whole("abc"));
        assert_eq!(made.await, Ok(()), "on {on}");
        assert_eq!(rig.calls(Op::Head), heads, "on {on}");
        assert_eq!(rig.calls(Op::Put), 1, "on {on}");
        assert_eq!(rig.get("cas/b/new").await.unwrap(), "abc", "on {on}");
    }
}

/// Asserts that a put of a key the upstream holds is refused as `AlreadyExists` and counted, either
/// way: with the setting on by the HEAD, with no put sent; with it off by the upstream's own
/// answer to the put, which the cache passes on. Nothing is kept and the object is as it was.
#[tokio::test]
async fn a_put_of_a_key_the_upstream_holds_is_refused_with_and_without_the_head() {
    for (on, puts) in [(true, 0), (false, 1)] {
        let rig = Rig::with("tscache-keep-head-exists", |config| {
            config.write_through = true;
            config.head_before_put = on;
        });
        rig.tamper
            .memory
            .put("cas/b/held", Bytes::from_static(b"old"))
            .await
            .unwrap();
        let made = rig.cache.put_stream("cas/b/held", Some(3), whole("new"));
        let refused = Error::AlreadyExists("cas/b/held".to_owned());
        assert_eq!(made.await, Err(refused), "on {on}");
        assert_eq!(rig.calls(Op::Put), puts, "on {on}");
        assert_eq!(read(&rig.cache.counters().write_exists), 1, "on {on}");
        assert_eq!(rig.cache.held(), (0, 0), "on {on}");
        let kept = rig.tamper.memory.get("cas/b/held", None).await.unwrap();
        assert_eq!(kept, Bytes::from_static(b"old"), "on {on}");
    }
}
