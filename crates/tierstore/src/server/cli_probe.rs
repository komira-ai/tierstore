//! The start-up self-test of the tiers of a storage node (see [`self_test`]): the probe key of a
//! start and the run of the checks over the hot tier and the cold tier.

use std::time::{Duration, Instant};

use crate::{
    Backend, Clock, MonotonicClock, SelfTestOptions, SweepBudget, SystemClock, self_test,
    settle_conditional_delete, sweep_probes,
};

use super::cli::{Cli, Error};

/// The id of a start for the probe keys of its self-test: the proxy id (with any character a
/// key cannot carry replaced by `_`), the time of the start and the process id, so that two
/// starts never share a probe key.
pub(super) fn probe_run_id(proxy_id: &str, now: u64, process: u32) -> String {
    let safe = |c: char| {
        if c.is_ascii_alphanumeric() || c == '-' {
            c
        } else {
            '_'
        }
    };
    let proxy: String = proxy_id.chars().map(safe).collect();
    format!("{proxy}-{now}-{process}")
}

/// The pause before the first repeat of a check of the self-test that could not reach its tier;
/// each repeat waits twice as long as the one before.
const SELF_TEST_FIRST_PAUSE: Duration = Duration::from_secs(1);

/// The longest pause between two attempts at a tier the self-test could not reach.
const SELF_TEST_MAX_PAUSE: Duration = Duration::from_secs(30);

/// Asks each tier the questions of the self-test, the hot tier first, and says on stderr which
/// checks the flags leave out, a line before and after each tier with how long it took, and
/// what the test notes (a check skipped because the tier is full, an attempt repeated, a probe key
/// not deleted). The cold tier is also asked about multipart uploads, an S3 tier how its conditional
/// deletes go (never a failure). A tier that did not answer is asked again, after pauses from one
/// second doubling to thirty, for `wait` (each tier has the whole of it).
///
/// # Errors
/// The [`SelfTestError`](crate::SelfTestError) of the first tier to answer a check wrongly (a violation: exit status 3)
/// or not to answer within `wait` (unreachable: exit status 1).
pub(super) async fn probe_tiers(
    cli: &Cli,
    hot: &dyn Backend,
    cold: &dyn Backend,
    wait: Duration,
) -> Result<(), Error> {
    let mut skip = Vec::new();
    for skipped in &cli.skip_backend_check {
        eprintln!(
            "tierstore: skipping the {} check of the tiers: {}",
            skipped.check, skipped.reason
        );
        skip.push(skipped.check);
    }
    let run_id = probe_run_id(&cli.proxy_id, SystemClock.now(), std::process::id());
    let hot_name = format!("hot tier {}", cli.hot);
    let cold_name = format!("cold tier {}", cli.cold);
    let tiers = [(&hot_name, hot, false), (&cold_name, cold, true)];
    for (name, backend, multipart) in tiers {
        let options = SelfTestOptions {
            name,
            run_id: &run_id,
            multipart,
            skip: &skip,
            wait,
            pause: SELF_TEST_FIRST_PAUSE,
            max_pause: SELF_TEST_MAX_PAUSE,
        };
        let note = |line: String| eprintln!("tierstore: {line}");
        eprintln!("tierstore: testing the {name}");
        let started = Instant::now();
        let outcome = self_test(backend, &options, &note).await;
        let took = started.elapsed().as_millis();
        eprintln!("tierstore: tested the {name} in {took} ms");
        outcome?;
        settle_conditional_delete(backend, &options, &note).await;
        // Bounded in keys and in time (no delete starts after it), on a clock the machine's clock does not move.
        let budget = SweepBudget::default();
        sweep_probes(backend, &options, budget, &MonotonicClock::new(), &note).await;
    }
    Ok(())
}
