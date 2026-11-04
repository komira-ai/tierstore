//! The two ways the explorer chooses schedules. Both replay a run from the start for every
//! schedule (the actors, the stores and the runtime are deterministic, so the same choices give
//! the same run), and both stop at the first run that breaks a rule, naming the schedule.

use super::world::{Decision, Scenario, World};

/// The most schedules one exhaustive search visits before it says its bound is too large for the
/// test budget.
const SCHEDULE_CAP: usize = 6000;

/// Whether choosing `pick` after `last` is a preemption: `last` could have gone on and did not.
fn cost(last: Option<usize>, pick: usize, enabled: &[usize]) -> u32 {
    u32::from(last.is_some_and(|held| held != pick && enabled.contains(&held)))
}

fn describe(world: &World, trace: &[Decision]) -> String {
    let names: Vec<&str> = trace.iter().map(|d| world.name(d.chosen)).collect();
    names.join(" ")
}

/// Finishes the run and fails with the schedule if it broke a rule.
async fn judge_run(mut world: World, trace: &[Decision], label: &str) {
    let broken = world.verdict().await;
    let schedule = describe(&world, trace);
    assert!(
        broken.is_empty(),
        "{label}\nrules broken: {broken:#?}\nschedule, one name per store call: {schedule}"
    );
}

/// Runs one schedule: the choices of `prefix`, then the actor that went last for as long as it can
/// go on, else the first that can. Gives what was chosen and what could have been.
async fn replay(scenario: &Scenario, prefix: &[usize]) -> Vec<Decision> {
    let mut world = World::start(scenario).await;
    let mut last: Option<usize> = None;
    let mut choose = |step: usize, enabled: &[usize]| {
        let pick = match prefix.get(step) {
            Some(&forced) => {
                assert!(
                    enabled.contains(&forced),
                    "step {step} cannot be replayed: {forced} is not among {enabled:?}"
                );
                forced
            }
            None => last
                .filter(|held| enabled.contains(held))
                .unwrap_or(enabled[0]),
        };
        last = Some(pick);
        pick
    };
    let trace = world.run(&mut choose).await;
    let label = format!("the schedule that begins {prefix:?}");
    judge_run(world, &trace, &label).await;
    trace
}

/// Every schedule of the scenario with at most `bound` preemptions: a switch from an actor that
/// could have gone on to another counts one, a switch from one that ended or is waiting for the
/// others counts none. Gives how many schedules it ran.
pub async fn exhaust(scenario: &Scenario, bound: u32) -> usize {
    let mut pending: Vec<Vec<usize>> = vec![Vec::new()];
    let mut schedules = 0;
    while let Some(prefix) = pending.pop() {
        let trace = replay(scenario, &prefix).await;
        schedules += 1;
        assert!(
            schedules <= SCHEDULE_CAP,
            "more than {SCHEDULE_CAP} schedules with at most {bound} preemptions"
        );
        let mut used = 0;
        let mut last = None;
        for (step, decision) in trace.iter().enumerate() {
            if step >= prefix.len() {
                let others = decision
                    .enabled
                    .iter()
                    .filter(|&&other| other != decision.chosen);
                for &other in others {
                    if used + cost(last, other, &decision.enabled) <= bound {
                        let mut next: Vec<usize> =
                            trace[..step].iter().map(|earlier| earlier.chosen).collect();
                        next.push(other);
                        pending.push(next);
                    }
                }
            }
            used += cost(last, decision.chosen, &decision.enabled);
            last = Some(decision.chosen);
        }
    }
    schedules
}

/// `walks` runs of the scenario in which every choice is made by a generator seeded with `seed`
/// (xorshift, so the same seed is the same walks). Gives how many it ran.
pub async fn sample(scenario: &Scenario, seed: u64, walks: usize) -> usize {
    let mut state = seed.max(1);
    for walk in 0..walks {
        let mut world = World::start(scenario).await;
        let mut choose = |_step: usize, enabled: &[usize]| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            enabled[usize::try_from(state % enabled.len() as u64).unwrap_or(0)]
        };
        let trace = world.run(&mut choose).await;
        judge_run(world, &trace, &format!("walk {walk} of seed {seed}")).await;
    }
    walks
}
