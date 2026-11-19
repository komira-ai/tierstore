//! C-7 placement: the rank, the stripe placement and its refusals, the key shares and the
//! summary, on synthetic keys and small profiles.

use tierstore_testkit::dir::TestDir;

use super::*;
use crate::table::Table;

/// A key of the CAS service's pack layout, `i` of them.
fn key(i: u64) -> String {
    format!("cas/d/n7/b2/{i:016x}")
}

/// A small profile: two nodes of three drives, 2+2, a drive holding eight shards.
fn tiny() -> Profile {
    Profile {
        name: "TINY".to_owned(),
        nodes: 2,
        drives_per_node: 3,
        drive_bytes: 8 * GIB,
        replicas: 2,
        k: 2,
        m: 2,
        max_shards_per_domain: 2,
        fill_limit: 0.85,
        reserve_drives: 1,
        shared_bytes_per_drive: 0,
        block_bytes: GIB,
    }
}

/// A writer that fails every write.
struct Broken;

impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("closed"))
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The score of a node is the first eight bytes of blake3 of the key and the node byte, read
/// little-endian.
#[test]
fn a_score_is_the_first_eight_bytes_of_blake3_little_endian() {
    let digest = blake3::hash(b"cas/x\x02");
    let mut first = [0u8; 8];
    first.copy_from_slice(&digest.as_bytes()[..8]);
    assert_eq!(score(b"cas/x", 2), u64::from_le_bytes(first));
    assert_eq!(hash(&[b"cas/", b"x\x02"]), score(b"cas/x", 2));
}

/// A rank lists every node once, is the same every time, and is the same list for a key
/// whatever the other keys are.
#[test]
fn a_rank_is_a_permutation_and_is_the_same_every_time() {
    let k = key(0x41);
    let first = rank(k.as_bytes(), 3);
    assert_eq!(first, rank(k.as_bytes(), 3));
    assert_eq!(first, RANK_OF_KEY_41);
    let mut sorted = rank(k.as_bytes(), 6);
    sorted.sort_unstable();
    assert_eq!(sorted, [0, 1, 2, 3, 4, 5]);
    let scores: Vec<u64> = first.iter().map(|&n| score(k.as_bytes(), n)).collect();
    assert!(scores.windows(2).all(|w| w[0] > w[1]));
}

/// The rank of the key `cas/d/n7/b2/0000000000000041` over three nodes, pinned.
const RANK_OF_KEY_41: [u8; 3] = [1, 0, 2];

/// The replica set is the first live nodes of the rank, in rank order, and shrinks when fewer
/// nodes are live.
#[test]
fn the_replica_set_is_the_first_live_nodes_of_the_rank() {
    let order = [4, 1, 5, 0, 2, 3];
    assert_eq!(replica_set(&order, &[], 2), [4, 1]);
    assert_eq!(replica_set(&order, &[4], 2), [1, 5]);
    assert_eq!(replica_set(&order, &[1, 5], 2), [4, 0]);
    assert_eq!(replica_set(&order, &[1, 5, 4, 0, 2], 2), [3]);
    assert_eq!(replica_set(&order, &order, 2), Vec::<u8>::new());
}

/// Choices of nodes down: none, each one, each pair, in order.
#[test]
fn the_choices_of_nodes_down_are_none_each_one_and_each_pair() {
    assert_eq!(down_sets(3, 0), vec![Vec::<u8>::new()]);
    assert_eq!(down_sets(3, 1).len(), 4);
    let two = down_sets(3, 2);
    assert_eq!(two.len(), 7);
    assert_eq!(&two[4..], [vec![0, 1], vec![0, 2], vec![1, 2]]);
    assert_eq!(down_sets(6, 2).len(), 22);
}

/// A 10+4 code on three nodes (five shards on a node, four tolerated) is refused for every
/// stripe, however the stripes are offered.
#[test]
fn ten_plus_four_on_three_nodes_is_refused_for_every_stripe() {
    let p = Profile {
        k: 10,
        m: 4,
        max_shards_per_domain: 5,
        ..Profile::later()
    };
    let used = vec![0; p.drives()];
    let want = Refusal::DomainRule {
        max_shards_per_domain: 5,
        m: 4,
    };
    for stripe in 0..50 {
        assert_eq!(place_stripe(&p, &used, &[], stripe), Err(want.clone()));
    }
    let (stats, placements) = place_population(&p, &[], 50);
    assert_eq!((stats.placed, stats.refused), (0, 50));
    assert_eq!(stats.first_refusal, Some(want));
    assert!(placements.is_empty());
    assert_eq!(
        Refusal::DomainRule {
            max_shards_per_domain: 5,
            m: 4
        }
        .to_string(),
        "5 shards per node exceeds the 4 the code tolerates losing"
    );
}

/// With every node live, LATER's 4+4 puts three, three and two shards on the nodes, the
/// lighter node rotating with the stripe.
#[test]
fn later_four_plus_four_with_every_node_live_puts_three_three_and_two() {
    let p = Profile::later();
    let used = vec![0; p.drives()];
    let mut lighter = [0u32; 3];
    for stripe in 0..300 {
        let slots = place_stripe(&p, &used, &[], stripe).unwrap();
        assert_eq!(slots.len(), 8);
        let mut per_node = [0usize; 3];
        for s in &slots {
            per_node[usize::from(s.0)] += 1;
        }
        let mut sorted = per_node;
        sorted.sort_unstable();
        assert_eq!(sorted, [2, 3, 3]);
        let light = per_node.iter().position(|&c| c == 2).unwrap();
        lighter[light] += 1;
    }
    assert!(lighter.iter().all(|&c| c > 50), "{lighter:?}");
}

/// LATER's 4+4 with one node down places four shards on each of the two survivors.
#[test]
fn later_four_plus_four_with_one_node_down_places_four_and_four_on_the_survivors() {
    let p = Profile::later();
    let used = vec![0; p.drives()];
    for stripe in 0..200 {
        let slots = place_stripe(&p, &used, &[1], stripe).unwrap();
        assert_eq!(slots.iter().filter(|s| s.0 == 0).count(), 4);
        assert_eq!(slots.iter().filter(|s| s.0 == 2).count(), 4);
        assert_eq!(slots.len(), 8);
    }
}

/// A 4+2 code on LATER's three nodes with one down needs three shards on a node and tolerates
/// two: refused.
#[test]
fn four_plus_two_on_two_surviving_nodes_is_refused() {
    let p = Profile {
        m: 2,
        max_shards_per_domain: 2,
        ..Profile::later()
    };
    let used = vec![0; p.drives()];
    assert_eq!(
        place_stripe(&p, &used, &[2], 7),
        Err(Refusal::NodeLoss { per_node: 3, m: 2 })
    );
    assert_eq!(
        Refusal::NodeLoss { per_node: 3, m: 2 }.to_string(),
        "a node would hold 3 shards, more than the 2 the code tolerates losing"
    );
}

/// LATER with two nodes down has one node left to hold eight shards: refused. With none live
/// there is nothing to place on.
#[test]
fn later_with_two_nodes_down_or_every_node_down_is_refused() {
    let p = Profile::later();
    let used = vec![0; p.drives()];
    assert_eq!(
        place_stripe(&p, &used, &[0, 2], 3),
        Err(Refusal::NodeLoss { per_node: 8, m: 4 })
    );
    assert_eq!(
        place_stripe(&p, &used, &[0, 1, 2], 3),
        Err(Refusal::NoLiveNodes)
    );
    assert_eq!(Refusal::NoLiveNodes.to_string(), "no node is live");
    let (stats, _) = place_population(&p, &[0, 1, 2], 4);
    assert_eq!((stats.placed, stats.refused), (0, 4));
    assert_eq!((stats.node_shards_min, stats.node_shards_max), (0, 0));
    assert_eq!((stats.fill_min, stats.fill_max), (0.0, 0.0));
}

/// NOW's 4+2 with two of six nodes down places two, two, one and one shards on the four
/// survivors.
#[test]
fn now_four_plus_two_with_two_nodes_down_places_two_two_one_one() {
    let p = Profile::now();
    let used = vec![0; p.drives()];
    for stripe in 0..200 {
        let slots = place_stripe(&p, &used, &[0, 3], stripe).unwrap();
        let mut per_node = [0usize; 6];
        for s in &slots {
            per_node[usize::from(s.0)] += 1;
        }
        assert_eq!(per_node[0] + per_node[3], 0);
        let mut live: Vec<usize> = [1, 2, 4, 5].iter().map(|&n| per_node[n]).collect();
        live.sort_unstable();
        assert_eq!(live, [1, 1, 2, 2]);
    }
}

/// A stripe's shards are always on distinct drives, with every node live and with nodes down.
#[test]
fn the_shards_of_a_stripe_are_on_distinct_drives() {
    for (p, down) in [
        (Profile::later(), vec![]),
        (Profile::later(), vec![1]),
        (Profile::now(), vec![]),
        (Profile::now(), vec![2, 5]),
    ] {
        let used = vec![0; p.drives()];
        for stripe in 0..300 {
            let slots = place_stripe(&p, &used, &down, stripe).unwrap();
            let mut distinct = slots.clone();
            distinct.sort_unstable();
            distinct.dedup();
            assert_eq!(distinct.len(), p.shards());
            assert!(slots.iter().all(|s| !down.contains(&s.0)));
        }
    }
}

/// A drive above the fill limit is never chosen, and a drive exactly at it is not below it.
#[test]
fn a_drive_above_the_fill_limit_is_never_chosen() {
    let p = Profile::later();
    let mut used = vec![0; p.drives()];
    // Node 0: drives 5.. are 90% full, drive 4 exactly at the limit; only drives 0..4 are open.
    for d in 4..15u8 {
        used[p.index((0, d))] = p.drive_bytes / 100 * 90;
    }
    used[p.index((0, 4))] = p.drive_bytes / 100 * 85;
    for stripe in 0..300 {
        let slots = place_stripe(&p, &used, &[], stripe).unwrap();
        assert!(slots.iter().filter(|s| s.0 == 0).all(|s| s.1 < 4));
    }
}

/// A node with one open drive cannot take two shards of a stripe: refused for lack of drives,
/// naming the node.
#[test]
fn a_node_with_too_few_open_drives_refuses_the_stripe() {
    let p = tiny();
    let mut used = vec![0; p.drives()];
    for d in 1..3u8 {
        used[p.index((1, d))] = p.drive_bytes;
    }
    let want = Refusal::DriveShortage {
        node: 1,
        needed: 2,
        eligible: 1,
    };
    assert_eq!(place_stripe(&p, &used, &[], 0), Err(want.clone()));
    assert_eq!(
        want.to_string(),
        "node 1 needs 2 drives below the fill limit and has 1"
    );
}

/// A drive with half its space free is chosen less often than one with all of it free: over
/// 3000 stripes, with the drive's fill held at half, it takes its pinned number of shards.
#[test]
fn a_half_full_drive_gets_fewer_shards() {
    let p = Profile::later();
    let mut used = vec![0; p.drives()];
    used[p.index((0, 0))] = p.drive_bytes / 2;
    let mut shards = vec![0u64; p.drives()];
    for stripe in 0..3000 {
        for s in place_stripe(&p, &used, &[], stripe).unwrap() {
            shards[p.index(s)] += 1;
        }
    }
    let half = shards[p.index((0, 0))];
    let others: Vec<u64> = (1..15).map(|d| shards[p.index((0, d))]).collect();
    let mean_other = others.iter().sum::<u64>() / 14;
    assert_eq!(half, HALF_FULL_DRIVE_SHARDS);
    assert!(half < mean_other, "{half} against {mean_other}");
    assert!(half * 10 > mean_other * 3, "{half} against {mean_other}");
}

/// Shards the half-full drive takes of 3000 stripes, pinned.
const HALF_FULL_DRIVE_SHARDS: u64 = 311;

/// The score of a drive grows with its free bytes, is 0 with none, and a stripe's drive
/// scores differ between drives.
#[test]
fn a_drive_scores_by_its_free_bytes() {
    assert_eq!(drive_score(9, 1, 2, 0), 0.0);
    assert!(drive_score(9, 1, 2, 100) > 0.0);
    let a = drive_score(9, 1, 2, 100);
    let b = drive_score(9, 1, 2, 200);
    assert!((b / a - 2.0).abs() < 1e-12);
    assert_ne!(drive_score(9, 1, 2, 100), drive_score(9, 1, 3, 100));
}

/// The node order of a stripe is a permutation of the live nodes, the same every time.
#[test]
fn the_node_order_of_a_stripe_is_a_permutation_of_the_live_nodes() {
    let order = node_order(12, &[0, 2, 3]);
    assert_eq!(order, node_order(12, &[0, 2, 3]));
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, [0, 2, 3]);
}

/// Placing stripes one after another fills the drives evenly and stops at the fill limit: of
/// twelve stripes on six drives that hold eight shards each, ten place (a drive at 7/8 is above
/// 85%, so the last shards go to the drives at 6/8) and two are refused.
#[test]
fn a_population_fills_the_drives_evenly_and_stops_at_the_fill_limit() {
    let p = tiny();
    let (stats, placements) = place_population(&p, &[], 12);
    assert_eq!(placements.len(), 10);
    assert_eq!((stats.stripes, stats.placed, stats.refused), (12, 10, 2));
    assert!(matches!(
        stats.first_refusal,
        Some(Refusal::DriveShortage {
            needed: 2,
            eligible: 1,
            ..
        })
    ));
    assert_eq!((stats.node_shards_min, stats.node_shards_max), (20, 20));
    assert_eq!((stats.drive_shards_min, stats.drive_shards_max), (6, 7));
    assert!((stats.drive_shards_mean - 40.0 / 6.0).abs() < 1e-12);
    assert!(stats.drive_shards_cv > 0.0 && stats.drive_shards_cv < 0.1);
    assert_eq!((stats.fill_min, stats.fill_max), (0.75, 0.875));
}

/// With a node down, a profile that cannot survive its loss places nothing.
#[test]
fn a_population_with_a_node_down_that_the_code_cannot_survive_places_nothing() {
    let (stats, _) = place_population(&tiny(), &[1], 5);
    assert_eq!((stats.placed, stats.refused), (0, 5));
    assert_eq!(
        stats.first_refusal,
        Some(Refusal::NodeLoss { per_node: 4, m: 2 })
    );
}

/// Stripes of silo's logical bytes: `ceil(L / (k x 1 GiB))`.
#[test]
fn silo_makes_one_stripe_per_k_gibibytes() {
    let p = Profile::later();
    assert_eq!(silo_stripes(&p, 0), 0);
    assert_eq!(silo_stripes(&p, 1), 1);
    assert_eq!(silo_stripes(&p, 4 * GIB), 1);
    assert_eq!(silo_stripes(&p, 4 * GIB + 1), 2);
}

/// The capacity guard of each profile, in stripes, rounded down.
#[test]
fn the_capacity_guard_is_in_whole_stripes() {
    assert_eq!(guard_stripes(&Profile::later()), LATER_GUARD);
    assert_eq!(guard_stripes(&Profile::now()), NOW_GUARD);
    let one_node = Profile {
        nodes: 1,
        ..Profile::later()
    };
    assert_eq!(guard_stripes(&one_node), 0);
}

/// Stripes that fill LATER's guard: 0.85 x (28 drives x 6e12) / (8 GiB), rounded down.
const LATER_GUARD: u64 = 16_624;
/// Stripes that fill NOW's guard: 0.85 x (9 drives x 8e12) / (6 GiB), rounded down.
const NOW_GUARD: u64 = 9_499;

/// A spread of values and of none, and the coefficient of variation.
#[test]
fn a_spread_has_min_max_and_mean_and_a_cv() {
    assert_eq!(spread(&[]), Spread::default());
    let s = spread(&[1.0, 2.0, 6.0]);
    assert_eq!((s.min, s.max, s.mean), (1.0, 6.0, 3.0));
    assert_eq!(cv(&[]), 0.0);
    assert_eq!(cv(&[0.0, 0.0]), 0.0);
    assert_eq!(cv(&[2.0, 2.0, 2.0]), 0.0);
    assert!((cv(&[1.0, 3.0]) - 0.5).abs() < 1e-12);
}

/// Over 30000 synthetic keys of one byte each, the three nodes write their pinned numbers of
/// keys, which add up to every key.
#[test]
fn three_nodes_write_their_pinned_share_of_thirty_thousand_keys() {
    let keys: Vec<String> = (0..30_000).map(key).collect();
    let tallies = tally_keys(
        keys.iter().map(|k| (k.as_str(), 1)),
        3,
        3,
        &[vec![], vec![0]],
    );
    assert_eq!(tallies[0].keys, 30_000);
    assert_eq!(tallies[0].bytes, 30_000);
    assert_eq!(tallies[0].writer_keys, WRITER_SHARES);
    assert_eq!(tallies[0].writer_bytes, tallies[0].writer_keys);
    assert_eq!(tallies[0].replica_keys, [30_000; 3]);
    assert_eq!(tallies[0].sets.len(), 1);
    assert_eq!(tallies[0].sets[&0b111], (30_000, 30_000));
    // With node 0 down, the keys it did not lead keep their writer; its keys move on.
    let after = &tallies[1].writer_keys;
    assert_eq!(after[0], 0);
    assert_eq!(after[1] + after[2], 30_000);
    assert!(after[1] >= WRITER_SHARES[1] && after[2] >= WRITER_SHARES[2]);
    assert_eq!(tallies[1].replica_keys, [0, 30_000, 30_000]);
}

/// Keys of 30000 that each of three nodes writes, pinned.
const WRITER_SHARES: [u64; 3] = [9945, 10037, 10018];

/// With six nodes and two copies, the fifteen replica sets share 6000 keys, pinned, and each key
/// has exactly one set and one writer inside it.
#[test]
fn six_nodes_with_two_copies_share_keys_over_fifteen_pairs() {
    let keys: Vec<String> = (0..6000).map(key).collect();
    let t = &tally_keys(keys.iter().map(|k| (k.as_str(), 10)), 6, 2, &[vec![]])[0];
    assert_eq!(t.sets.len(), 15);
    assert_eq!(t.sets.values().map(|s| s.0).sum::<u64>(), 6000);
    assert_eq!(t.sets.values().map(|s| s.1).sum::<u64>(), 60_000);
    assert!(t.sets.keys().all(|m| m.count_ones() == 2));
    assert_eq!(t.sets.values().map(|s| s.0).min(), Some(PAIR_MIN));
    assert_eq!(t.sets.values().map(|s| s.0).max(), Some(PAIR_MAX));
    assert_eq!(t.replica_keys.iter().sum::<u64>(), 12_000);
    assert_eq!(t.writer_keys.iter().sum::<u64>(), 6000);
}

/// The fewest and most keys of 6000 that one pair of six nodes holds, pinned.
const PAIR_MIN: u64 = 366;
/// See [`PAIR_MIN`].
const PAIR_MAX: u64 = 436;

/// With every node down no key has a writer or a replica, but the keys are still counted.
#[test]
fn keys_with_every_node_down_have_no_writer() {
    let t = &tally_keys([("cas/a", 5), ("cas/b", 7)], 2, 2, &[vec![0, 1]])[0];
    assert_eq!((t.keys, t.bytes), (2, 12));
    assert_eq!(t.writer_keys, [0, 0]);
    assert_eq!(t.replica_keys, [0, 0]);
    assert!(t.sets.is_empty());
    let s = key_scenario(&Profile { nodes: 2, ..tiny() }, t);
    assert_eq!(s.writer_keys, Spread::default());
}

/// The scenario of a tally is shares over the live nodes: two nodes down of three leave one
/// node writing every key; NOW also reports the replica sets, LATER does not.
#[test]
fn a_key_scenario_gives_shares_over_the_live_nodes() {
    let keys: Vec<String> = (0..600).map(key).collect();
    let items = || keys.iter().map(|k| (k.as_str(), 2));
    let later = Profile::later();
    let t = tally_keys(items(), 3, 3, &[vec![0, 1]]);
    let s = key_scenario(&later, &t[0]);
    assert_eq!((s.keys, s.bytes), (600, 1200));
    assert_eq!(
        s.writer_keys,
        Spread {
            min: 1.0,
            max: 1.0,
            mean: 1.0
        }
    );
    assert_eq!(
        s.replica_bytes,
        Spread {
            min: 1.0,
            max: 1.0,
            mean: 1.0
        }
    );
    assert_eq!((s.set_keys, s.set_bytes), (None, None));
    let now = Profile::now();
    let t = tally_keys(items(), 6, 2, &[vec![], vec![1]]);
    let s = key_scenario(&now, &t[0]);
    assert!((s.writer_keys.mean - 1.0 / 6.0).abs() < 1e-12);
    assert!((s.replica_keys.mean - 2.0 / 6.0).abs() < 1e-12);
    let sets = s.set_keys.unwrap();
    assert!((sets.mean - 1.0 / 15.0).abs() < 1e-12);
    assert!(sets.min <= sets.mean && sets.mean <= sets.max);
    // Node 1 down: ten pairs of the other five nodes are possible.
    let s = key_scenario(&now, &t[1]);
    assert!((s.set_bytes.unwrap().mean - 1.0 / 10.0).abs() < 1e-12);
    assert!((s.writer_keys.mean - 1.0 / 5.0).abs() < 1e-12);
}

/// A key set with no keys has zero shares, not a division by zero.
#[test]
fn an_empty_key_set_has_zero_shares() {
    let t = tally_keys([], 3, 3, &[vec![]]);
    let s = key_scenario(&Profile::later(), &t[0]);
    assert_eq!(s.writer_keys, Spread::default());
    assert_eq!(share(1, 0), 0.0);
}

/// By hand: three 4+2 stripes on three nodes, one node down and two down. The margins are
/// the shards left beyond four, the unreadable stripes those below zero, and the degraded
/// share the data shards (indexes 0..4) on down nodes.
#[test]
fn the_margin_histogram_and_degraded_share_are_exact_for_a_small_population() {
    let p = Profile {
        replicas: 3,
        m: 2,
        max_shards_per_domain: 2,
        ..Profile::later()
    };
    let placements: Vec<Vec<Slot>> =
        vec![[0, 0, 1, 1, 2, 2], [0, 1, 2, 0, 1, 2], [0, 0, 0, 1, 1, 2]]
            .into_iter()
            .map(|nodes: [u8; 6]| {
                nodes
                    .iter()
                    .enumerate()
                    .map(|(i, &n)| (n, i as u8))
                    .collect()
            })
            .collect();
    let one = failure(&p, &placements, &[0]);
    assert_eq!(one.stripes, 3);
    assert_eq!(one.margins, [(-1, 1), (0, 2)]);
    assert_eq!(one.unreadable, 1);
    assert_eq!(one.degraded_share, 7.0 / 12.0);
    let two = failure(&p, &placements, &[0, 1]);
    assert_eq!(two.margins, [(-3, 1), (-2, 2)]);
    assert_eq!(two.unreadable, 3);
    assert_eq!(two.degraded_share, 11.0 / 12.0);
    let none = failure(&p, &placements, &[]);
    assert_eq!(none.margins, [(2, 3)]);
    assert_eq!((none.unreadable, none.degraded_share), (0, 0.0));
    let empty = failure(&p, &[], &[0]);
    assert_eq!(
        (empty.stripes, empty.unreadable, empty.degraded_share),
        (0, 0, 0.0)
    );
}

/// LATER's 4+4 stripes lose at most three shards to a node and so stay readable with one node
/// down; with two down they are all unreadable.
#[test]
fn later_stripes_survive_one_node_and_not_two() {
    let p = Profile::later();
    let (stats, placements) = place_population(&p, &[], 300);
    assert_eq!(stats.placed, 300);
    let one = failure(&p, &placements, &[1]);
    assert_eq!(one.unreadable, 0);
    assert!(one.margins.iter().all(|&(m, _)| m == 1 || m == 2));
    assert_eq!(one.margins.iter().map(|m| m.1).sum::<u64>(), 300);
    let two = failure(&p, &placements, &[1, 2]);
    assert_eq!(two.unreadable, 300);
}

/// The nodes down print as a label.
#[test]
fn nodes_down_print_as_a_label() {
    assert_eq!(down_label(&[]), "none");
    assert_eq!(down_label(&[1]), "n1");
    assert_eq!(down_label(&[0, 2]), "n0+n2");
}

fn object(i: u64, size: u64) -> Object {
    Object {
        bucket: "kbf-cas".to_owned(),
        key: key(i),
        size,
        modified_ms: 1,
        etag: "e".to_owned(),
    }
}

/// The summary of a small listing under a small profile has the keys, both populations and
/// the failures, and its tables are titled C-7.
#[test]
fn a_summary_of_a_small_listing_has_keys_populations_and_failures() {
    let objects: Vec<Object> = (0..40).map(|i| object(i, 3 * GIB)).collect();
    let p = Profile {
        nodes: 3,
        replicas: 3,
        ..tiny()
    };
    let p = Profile {
        m: 3,
        max_shards_per_domain: 2,
        k: 3,
        ..p
    };
    let summary = summarize(&objects, std::slice::from_ref(&p));
    assert_eq!((summary.objects, summary.bytes), (40, 120 * GIB));
    let s = &summary.profiles[0];
    assert_eq!(s.keys.len(), 7);
    assert_eq!(s.populations.len(), 2);
    assert_eq!(s.populations[0].name, "L_silo");
    assert_eq!(s.populations[0].stripes, 40);
    assert_eq!(s.populations[1].name, "capacity guard");
    assert_eq!(s.populations[1].scenarios.len(), 7);
    assert_eq!(s.failures.len(), 6);
    let tables = tables(&summary);
    // keys, two populations, failures; no replica-set table when the set is every node.
    assert_eq!(tables.len(), 4);
    assert!(tables.iter().all(|t| t.markdown().starts_with("**C-7, ")));
}

/// A profile whose replica set is smaller than the cluster gets a table of replica sets, and
/// the tables print the rows exactly.
#[test]
fn the_tables_print_the_shares_placements_and_failures() {
    let objects = vec![object(1, GIB), object(2, GIB)];
    let p = Profile {
        nodes: 3,
        replicas: 2,
        ..tiny()
    };
    let summary = summarize(&objects, std::slice::from_ref(&p));
    let tables = tables(&summary);
    assert_eq!(tables.len(), 5);
    let md: Vec<String> = tables.iter().map(Table::markdown).collect();
    assert!(md[0].starts_with("**C-7, key placement under node rank, TINY (2 objects, 2.15 GB)**"));
    assert!(md[1].starts_with("**C-7, replica sets of the key set, TINY**"));
    assert!(md[2].starts_with(
        "**C-7, new stripes 2+2, TINY, L_silo (1 stripes; silo's bytes per drive: 0 B)**"
    ));
    assert!(md[4].starts_with("**C-7, existing 2+2 stripes under failure, TINY"));
    assert_eq!(tables[0].rows().len(), 7);
    assert_eq!(tables[1].rows().len(), 7);
    assert_eq!(tables[0].rows()[0][0], "none");
    assert_eq!(tables[4].rows().len(), 6);
    assert_eq!(tables[2].rows()[0][1], "1");
    assert_eq!(tables[2].rows()[0][3], "-");
    assert_eq!(tables[2].rows()[1][3], "-");
}

/// The verb end to end: a listing file in, the summary JSON and the tables out; the JSON reads
/// back to the summary, which counts the listing's objects and bytes.
#[test]
fn run_reads_a_listing_and_writes_the_summary_and_the_tables() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-placement-run");
    let listing = dir.path().join("listing.tsv");
    let mut text = String::new();
    for i in 0..30u64 {
        text.push_str(&format!("kbf-cas\t{}\t{}\t1\tabc\n", key(i), 1_000_000 + i));
    }
    std::fs::write(&listing, text).unwrap();
    let out = dir.path().join("c7.json");
    let mut stdout = Vec::new();
    run(
        &Args {
            listing,
            out: out.clone(),
            shared_bytes_per_drive: 0,
        },
        &mut stdout,
    )
    .unwrap();
    let summary: Summary = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    assert_eq!(summary.objects, 30);
    assert_eq!(summary.bytes, 30_000_000 + 435);
    let names: Vec<&str> = summary
        .profiles
        .iter()
        .map(|p| p.profile.name.as_str())
        .collect();
    assert_eq!(names, ["LATER", "NOW"]);
    assert_eq!(summary.profiles[0].keys.len(), 7);
    assert_eq!(summary.profiles[1].keys.len(), 22);
    assert_eq!(summary.profiles[0].populations[0].stripes, 1);
    assert_eq!(summary.profiles[0].populations[1].stripes, LATER_GUARD);
    let printed = String::from_utf8(stdout).unwrap();
    let want: Vec<String> = tables(&summary).iter().map(Table::markdown).collect();
    assert_eq!(printed, want.join("\n") + "\n");
}

/// A listing that is not there, a listing line that is not one, an output that cannot be
/// written and a standard output that fails are each an error naming what failed.
#[test]
fn run_fails_with_the_file_or_stream_it_could_not_use() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-placement-errors");
    let out = dir.path().join("c7.json");
    let missing = Args {
        listing: dir.path().join("absent.tsv"),
        out: out.clone(),
        shared_bytes_per_drive: 0,
    };
    assert!(matches!(
        run(&missing, &mut Vec::new()),
        Err(Error::Io { what: "open", .. })
    ));
    let bad = dir.path().join("bad.tsv");
    std::fs::write(&bad, "kbf-cas\tonly\n").unwrap();
    let args = Args {
        listing: bad,
        out: out.clone(),
        shared_bytes_per_drive: 0,
    };
    assert!(matches!(
        run(&args, &mut Vec::new()),
        Err(Error::Parse { line: 1, .. })
    ));
    let good = dir.path().join("good.tsv");
    std::fs::write(&good, format!("kbf-cas\t{}\t5\t1\tabc\n", key(1))).unwrap();
    let unwritable = Args {
        listing: good.clone(),
        out: dir.path().join("no-such-dir").join("c7.json"),
        shared_bytes_per_drive: 0,
    };
    assert!(matches!(
        run(&unwritable, &mut Vec::new()),
        Err(Error::Io { what: "write", .. })
    ));
    let args = Args {
        listing: good,
        out,
        shared_bytes_per_drive: 0,
    };
    assert!(matches!(
        run(&args, &mut Broken),
        Err(Error::Io { what: "write", .. })
    ));
}

/// Silo's bytes on each drive come off the capacity guard of the drives that hold them: NOW's
/// guard of 0.85 x 72 TB = 61.2 TB falls by 10 drives x 1 TB, and LATER's does not move.
#[test]
fn shared_bytes_per_drive_come_off_the_capacity_guard() {
    let shared = Profile {
        shared_bytes_per_drive: 1_000_000_000_000,
        ..Profile::now()
    };
    assert_eq!(guard_stripes(&shared), SHARED_GUARD);
    assert_eq!(guard_stripes(&Profile::now()), NOW_GUARD);
    let all = Profile {
        shared_bytes_per_drive: 8_000_000_000_000,
        ..Profile::now()
    };
    assert_eq!(guard_stripes(&all), 0);
}

/// Stripes that fill NOW's guard with 1 TB of silo on each drive: (61.2e12 - 10e12) / (6 GiB).
const SHARED_GUARD: u64 = 7_947;

/// A population on drives that hold shared bytes starts every drive at them: with 5 of 8 GiB
/// used, a drive takes shards until it is at 7 GiB (above 85%).
#[test]
fn a_population_starts_every_drive_at_the_shared_bytes() {
    let p = Profile {
        shared_bytes_per_drive: 5 * GIB,
        ..tiny()
    };
    let (stats, placements) = place_population(&p, &[], 12);
    assert_eq!(placements.len(), 3);
    assert_eq!((stats.placed, stats.refused), (3, 9));
    assert_eq!((stats.fill_min, stats.fill_max), (0.875, 0.875));
    assert_eq!((stats.drive_shards_min, stats.drive_shards_max), (2, 2));
}

/// Drives that the shared bytes push above the fill limit take no shard: every stripe is
/// refused, naming zero eligible drives.
#[test]
fn drives_that_shared_bytes_fill_past_the_limit_are_never_chosen() {
    let p = Profile {
        shared_bytes_per_drive: 7 * GIB,
        ..tiny()
    };
    let (stats, placements) = place_population(&p, &[], 4);
    assert!(placements.is_empty());
    assert_eq!((stats.placed, stats.refused), (0, 4));
    assert!(matches!(
        stats.first_refusal,
        Some(Refusal::DriveShortage {
            needed: 2,
            eligible: 0,
            ..
        })
    ));
    assert_eq!((stats.fill_min, stats.fill_max), (0.875, 0.875));
}

/// LATER has no shared bytes, and the verb sets them on NOW alone: the summary records the value
/// per profile and the tables of NOW say it.
#[test]
fn the_verb_sets_the_shared_bytes_on_now_only_and_the_tables_say_so() {
    let dir = TestDir::under(std::env::temp_dir(), "storebench-placement-shared");
    let listing = dir.path().join("listing.tsv");
    std::fs::write(&listing, format!("kbf-cas\t{}\t5\t1\tabc\n", key(1))).unwrap();
    let out = dir.path().join("c7.json");
    let mut stdout = Vec::new();
    run(
        &Args {
            listing,
            out: out.clone(),
            shared_bytes_per_drive: 1_000_000_000_000,
        },
        &mut stdout,
    )
    .unwrap();
    let summary: Summary = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
    let shared: Vec<u64> = summary
        .profiles
        .iter()
        .map(|p| p.profile.shared_bytes_per_drive)
        .collect();
    assert_eq!(shared, [0, 1_000_000_000_000]);
    assert_eq!(summary.profiles[0].populations[1].stripes, LATER_GUARD);
    assert_eq!(summary.profiles[1].populations[1].stripes, SHARED_GUARD);
    let printed = String::from_utf8(stdout).unwrap();
    assert!(printed.contains("NOW, L_silo (1 stripes; silo's bytes per drive: 1.00 TB)"));
    assert!(printed.contains("LATER, L_silo (1 stripes; silo's bytes per drive: 0 B)"));
}
