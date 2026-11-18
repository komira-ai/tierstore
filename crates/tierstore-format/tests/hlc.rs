//! The hybrid logical clock (10.2): stamps strictly increase on one node whatever its wall clock
//! does, and a stamp taken after hearing of an event is above that event's stamp.
//!
//! Catches:
//! - a first tick that is not the wall millisecond with counter 0, or ticks that repeat or go down
//!   when the wall clock stands still or steps back;
//! - a wall clock that jumps ahead and does not reset the counter to 0 at the new millisecond;
//! - an observe whose stamp is not above the remote stamp, a later tick that falls below a remote
//!   stamp heard of, or an observe of an old remote that fails to advance the clock;
//! - a counter that passes 0xffff and does not carry into the millisecond (or wraps to a lower
//!   stamp);
//! - a restart clock (`starting_after`) that gives a stamp at or below the stamp it started after;
//! - a wall clock above the stamp range, or a clock at the largest stamp, that panics, wraps, or
//!   changes the clock when it should be an error;
//! - skewed nodes (an hour behind, ahead) exchanging messages that break causality or give one
//!   node a non-increasing stamp;
//! - two threads on one clock that get the same stamp, or a final `last` that is not the maximum.

use tierstore_format::FormatError;
use tierstore_format::hlc::Hlc;
use tierstore_format::stamp::{STAMP_COUNTER_BITS, Stamp, WALL_MAX};

const WHAT: &str = "cluster stamp";
const HOUR_MS: u64 = 3_600_000;
const W: u64 = 1_700_000_000_000;

fn st(wall: u64, counter: u16) -> Stamp {
    match Stamp::from_parts(wall, counter) {
        Ok(s) => s,
        Err(e) => panic!("{e}"),
    }
}

fn wall_err() -> FormatError {
    FormatError::Invalid {
        what: WHAT,
        reason: "wall clock above 2^48 - 1 ms",
    }
}

fn overflow_err() -> FormatError {
    FormatError::Invalid {
        what: WHAT,
        reason: "the clock reached the largest stamp",
    }
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

#[test]
fn a_new_clock_has_given_nothing_and_default_is_the_same() {
    assert_eq!(Hlc::new().last(), Stamp::ZERO);
    assert_eq!(Hlc::default().last(), Stamp::ZERO);
}

#[test]
fn the_first_tick_is_the_wall_millisecond_with_counter_zero() {
    let clock = Hlc::new();
    let s = clock.tick(W).unwrap();
    assert_eq!(s, st(W, 0));
    assert_eq!(s.raw(), W << STAMP_COUNTER_BITS);
    assert_eq!(clock.last(), s);
}

#[test]
fn a_standing_wall_clock_increments_the_counter() {
    let clock = Hlc::new();
    for i in 0..5u16 {
        assert_eq!(clock.tick(W).unwrap(), st(W, i));
    }
    assert_eq!(clock.last(), st(W, 4));
}

#[test]
fn a_wall_clock_stepping_back_keeps_stamps_increasing() {
    let clock = Hlc::new();
    assert_eq!(clock.tick(W).unwrap(), st(W, 0));
    assert_eq!(clock.tick(W - HOUR_MS).unwrap(), st(W, 1));
    assert_eq!(clock.tick(W - 1).unwrap(), st(W, 2));
    assert_eq!(clock.tick(0).unwrap(), st(W, 3));
}

#[test]
fn a_wall_clock_jumping_ahead_resets_the_counter() {
    let clock = Hlc::new();
    clock.tick(W).unwrap();
    clock.tick(W).unwrap();
    assert_eq!(clock.tick(W + 1).unwrap(), st(W + 1, 0));
    assert_eq!(clock.tick(W + 1).unwrap(), st(W + 1, 1));
    assert_eq!(clock.tick(W + HOUR_MS).unwrap(), st(W + HOUR_MS, 0));
}

#[test]
fn a_tick_at_wall_zero_on_a_new_clock_is_one() {
    // `last + 1` beats the zero floor, so the first stamp is never `Stamp::ZERO`.
    assert_eq!(Hlc::new().tick(0).unwrap(), st(0, 1));
}

#[test]
fn observing_a_remote_ahead_gives_a_stamp_above_it_and_later_ticks_stay_above() {
    let clock = Hlc::new();
    clock.tick(W).unwrap();
    let remote = st(W + HOUR_MS, 7);
    let s = clock.observe(remote, W).unwrap();
    assert_eq!(s, st(W + HOUR_MS, 8));
    assert!(s > remote);
    assert_eq!(clock.tick(W).unwrap(), st(W + HOUR_MS, 9));
    assert_eq!(clock.tick(W + 1).unwrap(), st(W + HOUR_MS, 10));
    assert_eq!(clock.last(), st(W + HOUR_MS, 10));
}

#[test]
fn observing_a_remote_behind_still_advances() {
    let clock = Hlc::new();
    clock.tick(W).unwrap();
    clock.tick(W).unwrap();
    let s = clock.observe(st(W - HOUR_MS, 5), W).unwrap();
    assert_eq!(s, st(W, 2));
    assert_eq!(clock.observe(st(W, 0), W).unwrap(), st(W, 3));
    assert_eq!(clock.observe(Stamp::ZERO, 0).unwrap(), st(W, 4));
}

#[test]
fn observing_with_the_wall_clock_dominating_gives_wall_with_counter_zero() {
    let clock = Hlc::new();
    clock.tick(W).unwrap();
    let s = clock.observe(st(W, 3), W + 10).unwrap();
    assert_eq!(s, st(W + 10, 0));
    let fresh = Hlc::new();
    assert_eq!(fresh.observe(st(W - 1, 9), W).unwrap(), st(W, 0));
}

#[test]
fn a_remote_exactly_one_below_the_wall_floor_gives_the_floor() {
    let clock = Hlc::new();
    let remote = Stamp::from_raw((W << STAMP_COUNTER_BITS) - 1);
    assert_eq!(clock.observe(remote, W).unwrap(), st(W, 0));
    let clock = Hlc::new();
    let remote = Stamp::from_raw(W << STAMP_COUNTER_BITS);
    assert_eq!(clock.observe(remote, W).unwrap(), st(W, 1));
}

#[test]
fn the_counter_carries_into_the_millisecond_after_0xffff() {
    let clock = Hlc::starting_after(st(W, 0xfffe));
    assert_eq!(clock.tick(W).unwrap(), st(W, 0xffff));
    let carried = clock.tick(W).unwrap();
    assert_eq!(carried, st(W + 1, 0));
    assert_eq!(carried.raw(), st(W, 0xffff).raw() + 1);
    assert_eq!(clock.tick(W).unwrap(), st(W + 1, 1));
}

#[test]
fn the_counter_carries_after_a_full_65536_ticks_in_one_millisecond() {
    let clock = Hlc::new();
    let mut prev = clock.tick(W).unwrap();
    for _ in 1..=0xffff_u32 {
        let s = clock.tick(W).unwrap();
        assert!(s > prev);
        prev = s;
    }
    assert_eq!(prev, st(W, 0xffff));
    assert_eq!(clock.tick(W).unwrap(), st(W + 1, 0));
}

#[test]
fn observing_a_remote_with_a_full_counter_carries() {
    let clock = Hlc::new();
    let s = clock.observe(st(W, 0xffff), W).unwrap();
    assert_eq!(s, st(W + 1, 0));
}

#[test]
fn starting_after_gives_stamps_above_the_given_stamp() {
    let last = st(W + HOUR_MS, 12);
    let clock = Hlc::starting_after(last);
    assert_eq!(clock.last(), last);
    // The wall clock was stepped back an hour across the restart.
    assert_eq!(clock.tick(W).unwrap(), st(W + HOUR_MS, 13));
    let ahead = Hlc::starting_after(st(W, 5));
    assert_eq!(ahead.tick(W + 1).unwrap(), st(W + 1, 0));
}

#[test]
fn a_wall_clock_above_the_stamp_range_is_an_error_and_leaves_the_clock() {
    let clock = Hlc::new();
    let before = clock.tick(W).unwrap();
    assert_eq!(clock.tick(WALL_MAX + 1), Err(wall_err()));
    assert_eq!(clock.observe(st(W, 0), WALL_MAX + 1), Err(wall_err()));
    assert_eq!(clock.observe(st(W, 0), u64::MAX), Err(wall_err()));
    assert_eq!(clock.last(), before);
}

#[test]
fn the_largest_wall_clock_is_accepted() {
    let clock = Hlc::new();
    assert_eq!(clock.tick(WALL_MAX).unwrap(), st(WALL_MAX, 0));
    assert_eq!(clock.last().raw(), u64::MAX - 0xffff);
}

#[test]
fn a_clock_at_the_largest_stamp_errors_and_stays() {
    let clock = Hlc::starting_after(Stamp::from_raw(u64::MAX));
    assert_eq!(clock.tick(W), Err(overflow_err()));
    assert_eq!(clock.last().raw(), u64::MAX);
    assert_eq!(clock.observe(st(W, 0), W), Err(overflow_err()));
    assert_eq!(clock.last().raw(), u64::MAX);
}

#[test]
fn the_last_stamp_before_the_largest_is_given_then_the_clock_is_full() {
    let clock = Hlc::starting_after(Stamp::from_raw(u64::MAX - 2));
    assert_eq!(clock.tick(0).unwrap().raw(), u64::MAX - 1);
    assert_eq!(clock.tick(0).unwrap().raw(), u64::MAX);
    assert_eq!(clock.tick(0), Err(overflow_err()));
    assert_eq!(clock.last().raw(), u64::MAX);
}

#[test]
fn observing_the_largest_stamp_is_an_error_and_leaves_the_clock() {
    let clock = Hlc::new();
    let before = clock.tick(W).unwrap();
    assert_eq!(
        clock.observe(Stamp::from_raw(u64::MAX), W),
        Err(overflow_err())
    );
    assert_eq!(clock.last(), before);
    // Observing the second largest stamp is the largest stamp.
    let s = clock.observe(Stamp::from_raw(u64::MAX - 1), W).unwrap();
    assert_eq!(s.raw(), u64::MAX);
}

#[test]
fn an_error_message_names_the_cluster_stamp() {
    let text = Hlc::new().tick(WALL_MAX + 1).unwrap_err().to_string();
    assert!(text.contains("cluster stamp"), "{text}");
}

#[test]
fn three_skewed_nodes_exchanging_messages_keep_causality_and_monotonic_stamps() {
    // Node 0 is right, node 1 an hour behind, node 2 an hour and a half ahead.
    let skew: [i64; 3] = [0, -(HOUR_MS as i64), (HOUR_MS + HOUR_MS / 2) as i64];
    let clocks = [Hlc::new(), Hlc::new(), Hlc::new()];
    let mut last_stamp = [Stamp::ZERO; 3];
    // Each node's latest send stamp, delivered to others in a seeded order.
    let mut in_flight: Vec<(usize, Stamp)> = Vec::new();
    let mut rng = SplitMix64(0x5eed_0001);
    let mut now = W;
    let mut received = 0u32;
    for _ in 0..3000 {
        now += rng.next() % 3; // true time creeps forward, often standing still
        let node = (rng.next() % 3) as usize;
        let wall = now.checked_add_signed(skew[node]).unwrap();
        match rng.next() % 3 {
            0 => {
                let s = clocks[node].tick(wall).unwrap();
                assert!(s > last_stamp[node]);
                last_stamp[node] = s;
                in_flight.push((node, s));
            }
            _ if !in_flight.is_empty() => {
                let pick = (rng.next() % in_flight.len() as u64) as usize;
                let (from, sent) = in_flight[pick];
                if from == node {
                    continue;
                }
                let r = clocks[node].observe(sent, wall).unwrap();
                assert!(r > sent, "receive {r:?} not above send {sent:?}");
                assert!(r > last_stamp[node]);
                last_stamp[node] = r;
                received += 1;
                // Reply: a later tick is above what was heard.
                let reply = clocks[node].tick(wall).unwrap();
                assert!(reply > sent);
                last_stamp[node] = reply;
                in_flight.push((node, reply));
            }
            _ => {}
        }
    }
    assert!(received > 100, "only {received} receives");
    for (i, clock) in clocks.iter().enumerate() {
        assert_eq!(clock.last(), last_stamp[i]);
    }
}

#[test]
fn a_node_an_hour_behind_stamps_above_a_record_it_heard_of() {
    let ahead = Hlc::new();
    let behind = Hlc::new();
    let record = ahead.tick(W).unwrap();
    let heard = behind.observe(record, W - HOUR_MS).unwrap();
    assert!(heard > record);
    let own = behind.tick(W - HOUR_MS + 5).unwrap();
    assert!(own > record);
    assert_eq!(own, st(W, 2));
}

#[test]
fn threads_ticking_one_clock_get_distinct_stamps() {
    const THREADS: usize = 8;
    const PER: usize = 5000;
    let clock = Hlc::new();
    let mut all: Vec<u64> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                scope.spawn(|| {
                    (0..PER)
                        .map(|_| clock.tick(W).unwrap().raw())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut out = Vec::new();
        for h in handles {
            let mine = h.join().unwrap();
            // Each thread sees its own stamps strictly increasing.
            assert!(mine.windows(2).all(|p| p[0] < p[1]));
            out.extend(mine);
        }
        out
    });
    assert_eq!(all.len(), THREADS * PER);
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), THREADS * PER);
    let max = *all.last().unwrap();
    assert_eq!(clock.last().raw(), max);
    assert_eq!(max, (W << STAMP_COUNTER_BITS) + (THREADS * PER) as u64 - 1);
}

#[test]
fn threads_observing_and_ticking_get_distinct_stamps() {
    const THREADS: usize = 4;
    const PER: usize = 3000;
    let clock = Hlc::new();
    let mut all: Vec<u64> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let clock = &clock;
                scope.spawn(move || {
                    let mut rng = SplitMix64(t as u64 + 1);
                    (0..PER)
                        .map(|i| {
                            let wall = W + rng.next() % 4;
                            if i % 2 == 0 {
                                clock.tick(wall).unwrap().raw()
                            } else {
                                let remote = st(W + rng.next() % 4, (rng.next() % 100) as u16);
                                let s = clock.observe(remote, wall).unwrap();
                                assert!(s > remote);
                                s.raw()
                            }
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect()
    });
    all.sort_unstable();
    let n = all.len();
    all.dedup();
    assert_eq!(all.len(), n);
    assert_eq!(n, THREADS * PER);
    assert_eq!(clock.last().raw(), *all.last().unwrap());
}
