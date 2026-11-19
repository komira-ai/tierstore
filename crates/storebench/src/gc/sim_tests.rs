//! Tests of the model on tiny blocks: a segment holds two records of 50 bytes (`seal_bytes`
//! 100), a stripe is two data blocks and one parity shard, and nothing is aligned. The tiny
//! policy destages a block an hour after its oldest byte, has no record header and compacts
//! nothing, so each test sees one rule; the tests of `max_age`, the header and compaction set
//! their own.

use super::*;

/// The policy of the tests; the alignment is 1 and the header 0 so that sizes are what they say.
fn tiny() -> Policy {
    Policy {
        name: "tiny".to_owned(),
        nodes: 1,
        hot_replicas: 1,
        data_blocks: 2,
        parity_blocks: 1,
        seal_bytes: 100,
        seal_min_bytes: 40,
        seal_max_age_h: 24,
        large_min: 50,
        pack_block_bytes: 100,
        pack_window: 64,
        align: 1,
        classes_max: 8,
        destage_min_age_h: 1,
        destage_max_age_h: 1,
        gc_live_below: 0.0,
        record_header: 0,
        partial_after_h: 72,
        rewrite_below: vec![0.5],
        tail_days: 20,
    }
}

/// An object of `size` bytes that arrives at hour `arrival` and dies at hour `death`.
fn item(arrival: u64, size: u64, death: u64, class: u64) -> Item {
    Item {
        arrival_ms: arrival * HOUR_MS,
        size,
        death_ms: death * HOUR_MS,
        writer_hash: 0,
        class,
    }
}

/// The sample of day `day` of a replay.
fn day(r: &Replay, day: usize) -> &Sample {
    &r.samples[day]
}

/// Two blocks of one class whose records all die make one stripe, and that stripe is freed
/// whole at the hour the last record dies; before that its raw bytes are two data blocks and
/// one parity shard as long as the longest.
#[test]
fn two_blocks_of_one_class_that_all_die_free_one_whole_stripe() {
    let items = (0..4).map(|_| item(0, 50, 100, 0)).collect();
    let r = replay(&tiny(), items);
    let held = day(&r, 4);
    assert_eq!(held.cold_raw_bytes, 300);
    assert_eq!(held.live_bytes, 200);
    assert_eq!(held.stripes_live, 1);
    assert_eq!(held.stripes_freed, 0);
    assert_eq!(held.partial_stripes, 0);
    let freed = day(&r, 5);
    assert_eq!(freed.cold_raw_bytes, 0);
    assert_eq!(freed.live_bytes, 0);
    assert_eq!(freed.stripes_live, 0);
    assert_eq!(freed.stripes_freed, 1);
    assert_eq!(freed.cold_fill_efficiency, 0.0);
    assert_eq!(r.end_of_replay.sample.stripes_freed, 1);
}

/// Four blocks each holding one record that dies in week one and one that dies in week two:
/// arrival grouping frees nothing whole until week two, while expiry grouping, which keeps the
/// two weeks apart, frees the week-one stripe at its death.
#[test]
fn arrival_grouping_that_mixes_two_death_weeks_frees_nothing_whole_where_expiry_grouping_frees_both()
 {
    let mixed = |class_of: fn(u64) -> u64| -> Replay {
        let items = (0..8)
            .map(|i| {
                let death = if i % 2 == 0 { 100 } else { 300 };
                item(0, 50, death, class_of(death))
            })
            .collect();
        replay(&tiny(), items)
    };
    let arrival = mixed(|_| 0);
    let expiry = mixed(|death| u64::from(death > 200));

    let a = day(&arrival, 5);
    assert_eq!(a.stripes_freed, 0);
    assert_eq!(a.stripes_live, 2);
    assert_eq!(a.cold_raw_bytes, 600);
    assert_eq!(a.live_bytes, 200);
    assert_eq!(a.cold_fill_efficiency, 200.0 / 600.0);
    let e = day(&expiry, 5);
    assert_eq!(e.stripes_freed, 1);
    assert_eq!(e.stripes_live, 1);
    assert_eq!(e.cold_raw_bytes, 300);
    assert_eq!(e.live_bytes, 200);
    assert_eq!(e.cold_fill_efficiency, 200.0 / 300.0);

    assert_eq!(arrival.end_of_replay.sample.stripes_freed, 2);
    assert_eq!(expiry.end_of_replay.sample.stripes_freed, 2);
    assert_eq!(arrival.end_of_replay.sample.cold_raw_bytes, 0);
}

/// A block with no partner makes a partial stripe only once `partial_after` has passed since it
/// sealed (sealed at hour 24 by age, so at hour 96).
#[test]
fn a_lone_block_forms_a_partial_stripe_only_after_partial_after() {
    let items = vec![item(0, 50, 200, 0), item(0, 50, 200, 0)];
    let r = replay(&tiny(), items);
    let before = day(&r, 3);
    assert_eq!(before.stripes_live, 0);
    assert_eq!(before.partial_stripes, 0);
    let after = day(&r, 4);
    assert_eq!(after.stripes_live, 1);
    assert_eq!(after.partial_stripes, 1);
    assert_eq!(after.cold_raw_bytes, 200);
    assert_eq!(after.live_bytes, 100);
    assert_eq!(after.cold_fill_efficiency, 0.5);
    assert_eq!(r.end_of_replay.sample.stripes_freed, 1);
    assert_eq!(r.end_of_replay.sample.partial_stripes, 1);
}

/// A segment past `seal_max_age` seals at once if it holds `seal_min_bytes`; a shorter one
/// waits for `destage.max_age`.
#[test]
fn a_slow_class_seals_by_age_only_above_seal_min_bytes_else_at_max_age() {
    let p = Policy {
        destage_max_age_h: 48,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    sim.arrive(&item(0, 30, 500, 0), 0);
    sim.arrive(&item(0, 40, 500, 1), 0);
    sim.seal_by_age(23);
    assert_eq!(sim.sealed, BlockSizes::default());
    sim.seal_by_age(24);
    assert_eq!(
        sim.sealed,
        BlockSizes {
            under_seal_min: 0,
            partly_full: 1,
            full: 0
        }
    );
    sim.seal_by_age(47);
    assert_eq!(sim.sealed.under_seal_min, 0);
    sim.seal_by_age(48);
    assert_eq!(
        sim.sealed,
        BlockSizes {
            under_seal_min: 1,
            partly_full: 1,
            full: 0
        }
    );
    assert!(sim.open.is_empty());
}

/// A record that would pass `seal_bytes` seals the segment first and opens a new one.
#[test]
fn a_record_that_would_pass_seal_bytes_seals_the_segment_and_opens_another() {
    let p = tiny();
    let mut sim = Sim::new(&p);
    for _ in 0..3 {
        sim.arrive(&item(0, 50, 500, 0), 0);
    }
    assert_eq!(
        sim.sealed,
        BlockSizes {
            under_seal_min: 0,
            partly_full: 0,
            full: 1
        }
    );
    assert_eq!(sim.open.len(), 1);
    assert_eq!(sim.blocks.len(), 2);
    assert_eq!(sim.blocks[0].bytes, 100);
    assert_eq!(sim.blocks[1].bytes, 50);
}

/// A sealed block whose records are all dead before it goes cold is unlinked from the hot tier
/// and never forms a stripe.
#[test]
fn a_block_wholly_dead_before_destage_never_reaches_cold() {
    let items = vec![item(0, 50, 10, 0), item(0, 50, 10, 0), item(0, 50, 200, 0)];
    let r = replay(&tiny(), items);
    let early = day(&r, 1);
    assert_eq!(early.blocks_dropped_hot, 1);
    assert_eq!(early.stripes_live, 0);
    assert_eq!(early.cold_raw_bytes, 0);
    assert_eq!(r.end_of_replay.sample.blocks_dropped_hot, 1);
    assert_eq!(r.end_of_replay.sample.partial_stripes, 1);
    assert_eq!(r.end_of_replay.sample.stripes_freed, 1);
}

/// A block that is dead when it seals is unlinked in the step it seals in, and the hot
/// death of a block does not touch any stripe.
#[test]
fn a_block_dead_on_arrival_is_unlinked_at_once() {
    let items = vec![item(5, 50, 5, 0), item(5, 50, 5, 0), item(5, 50, 5, 0)];
    let r = replay(&tiny(), items);
    assert_eq!(r.end_of_data.sample.blocks_dropped_hot, 1);
    assert_eq!(r.end_of_data.sample.stripes_live, 0);
    assert_eq!(r.end_of_replay.sample.blocks_dropped_hot, 2);
    assert_eq!(r.end_of_replay.sample.stripes_live, 0);
    assert_eq!(r.end_of_replay.sample.partial_stripes, 0);
}

/// First-fit decreasing puts 60 and 40 in one pack block of 100 and the 50 in another.
#[test]
fn first_fit_decreasing_puts_600_and_400_scaled_in_one_block() {
    let p = Policy {
        large_min: 10,
        pack_window: 3,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    sim.arrive(&item(0, 50, 500, 0), 0);
    sim.arrive(&item(0, 40, 500, 0), 0);
    assert!(sim.blocks.is_empty());
    sim.arrive(&item(0, 60, 500, 0), 0);
    let sizes: Vec<u64> = sim.blocks.iter().map(|b| b.bytes).collect();
    assert_eq!(sizes, [100, 50]);
    assert_eq!(
        sim.sealed,
        BlockSizes {
            under_seal_min: 0,
            partly_full: 1,
            full: 1
        }
    );
    assert!(sim.windows[&0].items.is_empty());
}

/// An object that fills whole pack blocks exactly leaves nothing in the window.
#[test]
fn an_object_of_whole_pack_blocks_leaves_nothing_in_the_window() {
    let p = tiny();
    let mut sim = Sim::new(&p);
    sim.arrive(&item(0, 200, 500, 0), 0);
    let sizes: Vec<u64> = sim.blocks.iter().map(|b| b.bytes).collect();
    assert_eq!(sizes, [100, 100]);
    assert!(sim.windows.is_empty());
    assert_eq!(sim.sealed.full, 2);
}

/// Items are rounded up to the alignment inside a pack block.
#[test]
fn pack_items_are_aligned() {
    let p = Policy {
        large_min: 10,
        pack_window: 2,
        align: 16,
        pack_block_bytes: 64,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    sim.arrive(&item(0, 17, 500, 0), 0);
    sim.arrive(&item(0, 20, 500, 0), 0);
    assert_eq!(sim.blocks.len(), 1);
    assert_eq!(sim.blocks[0].bytes, 64);
}

/// An object larger than a pack block is a chain: whole pack blocks of its own at once, and the
/// rest in the window, packed when its oldest object is `destage.max_age` old. All of it dies
/// together, so its stripes are freed whole.
#[test]
fn a_chain_takes_whole_pack_blocks_and_its_rest_waits_in_the_window() {
    let p = Policy {
        destage_max_age_h: 24,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    sim.arrive(&item(0, 250, 100, 0), 0);
    assert_eq!(sim.blocks.len(), 2);
    assert_eq!(sim.windows[&0].items, [(50, 50, 100)]);
    sim.seal_by_age(23);
    assert_eq!(sim.blocks.len(), 2);
    sim.seal_by_age(24);
    let sizes: Vec<u64> = sim.blocks.iter().map(|b| b.bytes).collect();
    assert_eq!(sizes, [100, 100, 50]);

    let r = replay(&p, vec![item(0, 250, 100, 0)]);
    let held = day(&r, 4);
    assert_eq!(held.stripes_live, 2);
    assert_eq!(held.partial_stripes, 1);
    assert_eq!(held.cold_raw_bytes, 300 + 100);
    assert_eq!(held.live_bytes, 250);
    assert_eq!(r.end_of_replay.sample.stripes_freed, 2);
    assert_eq!(r.end_of_replay.sample.cold_raw_bytes, 0);
}

/// A data shard's raw bytes are its own aligned length; each parity shard is as long as the
/// longest aligned data shard.
#[test]
fn raw_bytes_are_the_aligned_data_lengths_plus_parity_as_long_as_the_longest() {
    let p = Policy {
        align: 16,
        parity_blocks: 2,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    let a = sim.new_block(0, 0, Some(0));
    sim.put(a, 100, 100, 500, 0);
    let b = sim.new_block(0, 0, Some(0));
    sim.put(b, 40, 40, 500, 0);
    sim.stripe(&[a, b], false);
    assert_eq!(sim.cold_raw, 112 + 48 + 2 * 112);
    assert_eq!(sim.stripes[0].view.data, 140);
    assert_eq!(sim.cold_live, 140);
}

/// Eligible blocks of a class are sorted by length and each `k` consecutive ones make a stripe;
/// a block that is not yet `min_age` old waits.
#[test]
fn stripes_group_blocks_of_similar_length_and_young_blocks_wait() {
    let p = tiny();
    let mut sim = Sim::new(&p);
    for bytes in [10, 100, 20, 90] {
        let b = sim.new_block(0, 0, Some(0));
        sim.put(b, bytes, bytes, 500, 0);
        sim.seal(b, 0);
    }
    sim.destage(0);
    assert!(sim.stripes.is_empty());
    sim.destage(1);
    let data: Vec<u64> = sim.stripes.iter().map(|s| s.view.data).collect();
    assert_eq!(data, [30, 190]);
    assert_eq!(sim.cold_raw, (10 + 20 + 20) + (90 + 100 + 100));
    assert_eq!(sim.partial_stripes, 0);
    assert!(sim.pool[&0].is_empty());
}

/// A block goes cold once its oldest byte is `destage.max_age` old and it has been sealed
/// `destage.min_age`, not before: a block opened at hour 0 and sealed at once waits for hour 48,
/// and one opened at 0 but sealed at 47 for hour 48 as well (47 + `min_age`); one sealed at 48
/// waits for 49.
#[test]
fn a_block_goes_cold_at_max_age_from_its_oldest_byte_and_min_age_from_its_seal() {
    let p = Policy {
        data_blocks: 1,
        destage_max_age_h: 48,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    for sealed in [0, 47, 48] {
        let b = sim.new_block(0, 0, Some(0));
        sim.put(b, 10, 10, 500, 0);
        sim.seal(b, sealed);
    }
    sim.destage(47);
    assert!(sim.stripes.is_empty());
    sim.destage(48);
    assert_eq!(sim.stripes.len(), 2);
    sim.destage(49);
    assert_eq!(sim.stripes.len(), 3);
    assert_eq!(sim.partial_stripes, 0);
}

/// A sealed segment less than `gc_live_below` live is compacted on the hot tier: its live
/// records go to the open segment of their writer and class again, the block is unlinked and
/// never goes cold, and a later death of one of its records counts only in the new segment. A
/// segment at exactly `gc_live_below`, and a pack block, are not compacted.
#[test]
fn a_mostly_dead_segment_is_compacted_on_the_hot_tier() {
    let p = Policy {
        gc_live_below: 0.5,
        destage_max_age_h: 48,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    // Block 0: three records of 30 (two die at hour 5); the fourth seals it and opens block 1.
    for death in [5, 5, 500, 500] {
        sim.arrive(&item(0, 30, death, 0), 0);
    }
    assert_eq!(sim.sealed.full, 1);
    sim.die(5);
    sim.destage(6);
    assert_eq!((sim.compacted_hot, sim.compacted_bytes), (1, 30));
    assert!(sim.blocks[0].gone);
    let open = sim.open[&(0, 0)].block;
    assert_eq!((sim.blocks[open].bytes, sim.blocks[open].live), (60, 60));
    sim.die(500);
    assert_eq!(sim.blocks[open].live, 0);
    assert_eq!(
        sim.blocks[0].live, 30,
        "a death in a compacted block is not counted again"
    );
    assert!(sim.stripes.is_empty());

    // Exactly half live is not below the mark; a pack block is never compacted.
    let mut sim = Sim::new(&p);
    let half = sim.new_block(0, 0, Some(0));
    sim.put(half, 50, 50, 5, 0);
    sim.put(half, 50, 50, 500, 0);
    let pack = sim.new_block(0, 0, None);
    sim.put(pack, 90, 90, 5, 0);
    sim.put(pack, 10, 10, 500, 0);
    sim.seal(half, 0);
    sim.seal(pack, 0);
    sim.die(5);
    sim.destage(6);
    assert_eq!(sim.compacted_hot, 0);
    assert_eq!(sim.pool[&0].len(), 2);
}

/// A record is stored with its header: two objects of 40 with a header of 10 fill a segment of
/// 100, the stripe's raw bytes count the headers, and its data and live bytes count only the
/// objects.
#[test]
fn a_record_is_stored_with_its_header_and_only_the_object_is_data() {
    let p = Policy {
        record_header: 10,
        data_blocks: 1,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    for _ in 0..3 {
        sim.arrive(&item(0, 40, 500, 0), 0);
    }
    assert_eq!((sim.blocks[0].bytes, sim.blocks[0].data), (100, 80));
    assert_eq!(sim.blocks[1].bytes, 50);
    sim.destage(1);
    assert_eq!(sim.stripes[0].view.data, 80);
    assert_eq!(sim.stripes[0].view.live, 80);
    assert_eq!(sim.stripes[0].view.raw, 100 + 100);
}

/// Large objects of one class share one window whatever their writer; another class has its own.
#[test]
fn large_objects_of_a_class_share_one_window_whatever_the_writer() {
    let p = Policy {
        nodes: 3,
        hot_replicas: 3,
        large_min: 10,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    for (writer, class) in [(0, 0), (1, 0), (2, 0), (1, 3)] {
        let mut it = item(0, 20, 500, class);
        it.writer_hash = writer;
        sim.arrive(&it, 0);
    }
    assert_eq!(sim.windows.len(), 2);
    assert_eq!(sim.windows[&0].items.len(), 3);
    assert_eq!(sim.windows[&3].items.len(), 1);
}

/// With `cold.replicas = 2` (a stripe of one block and one parity copy) every block goes cold
/// alone at twice its length, no stripe is partial, and a block is freed when it dies.
#[test]
fn two_cold_copies_destage_each_block_alone_at_twice_its_length() {
    let p = Policy {
        data_blocks: 1,
        parity_blocks: 1,
        ..tiny()
    };
    let items = vec![
        item(0, 50, 100, 0),
        item(0, 50, 100, 0),
        item(0, 30, 300, 0),
    ];
    let r = replay(&p, items);
    let held = day(&r, 2);
    assert_eq!(held.stripes_live, 2);
    assert_eq!(held.partial_stripes, 0);
    assert_eq!(held.cold_raw_bytes, 2 * 100 + 2 * 30);
    let later = day(&r, 5);
    assert_eq!((later.stripes_live, later.stripes_freed), (1, 1));
    assert_eq!(later.cold_raw_bytes, 2 * 30);
}

/// Blocks of different classes never share a stripe.
#[test]
fn blocks_of_different_classes_do_not_share_a_stripe() {
    let p = tiny();
    let mut sim = Sim::new(&p);
    for class in [0, 1] {
        let b = sim.new_block(class, 0, Some(0));
        sim.put(b, 10, 10, 500, 0);
        sim.seal(b, 0);
    }
    sim.destage(1);
    assert!(sim.stripes.is_empty());
    assert_eq!(sim.pool[&0].len(), 1);
    assert_eq!(sim.pool[&1].len(), 1);
}

/// A class not below `classes_max` falls into class 0, and the open segment is the hash modulo
/// the open segments per class.
#[test]
fn a_class_beyond_the_maximum_falls_into_class_0_and_the_hash_picks_the_open_segment() {
    let p = Policy {
        nodes: 3,
        hot_replicas: 3,
        ..tiny()
    };
    let mut sim = Sim::new(&p);
    let mut it = item(0, 10, 500, 9);
    it.writer_hash = 7;
    sim.arrive(&it, 0);
    it.class = 7;
    sim.arrive(&it, 0);
    let slots: Vec<Slot> = sim.open.keys().copied().collect();
    assert_eq!(slots, [(1, 0), (1, 7)]);
}

/// The expiry class is the week of the death, wrapped over the classes.
#[test]
fn the_expiry_class_is_the_week_of_death_wrapped_over_the_classes() {
    assert_eq!(expiry_class(0, 8), 0);
    assert_eq!(expiry_class(7 * DAY_MS - 1, 8), 0);
    assert_eq!(expiry_class(7 * DAY_MS, 8), 1);
    assert_eq!(expiry_class(7 * 7 * DAY_MS, 8), 7);
    assert_eq!(expiry_class(8 * 7 * DAY_MS, 8), 0);
    assert_eq!(expiry_class(9 * 7 * DAY_MS, 8), 1);
    assert_eq!(expiry_class(9 * 7 * DAY_MS, 4), 1);
}

/// A stripe 25% live is counted in the third decile and is below `rewrite_below` 0.5 with the
/// bytes it would cost; the reading at the end of the data is the one the stripe had then.
#[test]
fn a_partly_dead_stripe_is_in_its_decile_and_costs_its_live_bytes_to_rewrite() {
    let items = vec![
        item(0, 50, 100, 0),
        item(0, 50, 100, 0),
        item(0, 50, 100, 0),
        item(0, 50, 400, 0),
        item(150, 10, 900, 5),
    ];
    let r = replay(&tiny(), items);
    let snap = &r.end_of_data;
    assert_eq!(snap.sample.day, 6);
    assert_eq!(snap.sample.stripes_live, 1);
    assert_eq!(snap.sample.live_bytes, 50);
    assert_eq!(snap.sample.cold_raw_bytes, 300);
    assert_eq!(snap.live_fraction_deciles, [0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(snap.rewrite.len(), 1);
    let rw = &snap.rewrite[0];
    assert_eq!(rw.below, 0.5);
    assert_eq!(rw.stripes, 1);
    assert_eq!(rw.rewrite_bytes, 50);
    assert_eq!(rw.raw_freed, 300);
    assert_eq!(rw.logical_freed, 150);
    assert_eq!(rw.per_raw_byte_freed, 50.0 / 300.0);
    assert_eq!(rw.per_logical_byte_freed, 50.0 / 150.0);
}

/// Rewriting a stripe 30% live writes 0.43 bytes per byte freed; a stripe not below the
/// threshold costs nothing and an empty set has no ratio.
#[test]
fn rewriting_a_stripe_at_30_percent_live_writes_0_43_bytes_per_byte_freed() {
    let stripes = [
        StripeView {
            data: 100,
            live: 30,
            raw: 150,
        },
        StripeView {
            data: 100,
            live: 60,
            raw: 150,
        },
    ];
    let costs = rewrite_costs(&stripes, &[0.5, 0.3, 0.2]);
    let at = &costs[0];
    assert_eq!(
        (at.stripes, at.rewrite_bytes, at.logical_freed),
        (1, 30, 70)
    );
    assert_eq!(format!("{:.2}", at.per_logical_byte_freed), "0.43");
    assert_eq!(at.per_raw_byte_freed, 30.0 / 150.0);
    assert_eq!(costs[1].stripes, 0);
    assert_eq!(costs[1].per_logical_byte_freed, 0.0);
    assert_eq!(costs[2].per_raw_byte_freed, 0.0);
    let more = rewrite_costs(&stripes, &[0.7]);
    assert_eq!(more[0].stripes, 2);
    assert_eq!(more[0].rewrite_bytes, 90);
}

/// Stripes fall in the bin of their live tenth; a whole stripe is in the last bin.
#[test]
fn stripes_fall_in_the_decile_of_their_live_fraction() {
    let view = |live| StripeView {
        data: 100,
        live,
        raw: 100,
    };
    let bins = deciles(&[view(0), view(9), view(10), view(95), view(100), view(100)]);
    assert_eq!(bins, [2, 1, 0, 0, 0, 0, 0, 0, 0, 3]);
    assert_eq!(ratio(1, 0), 0.0);
    assert_eq!(ratio(1, 4), 0.25);
}

/// The two profiles hold the design's values of section 21.
#[test]
fn the_profiles_hold_the_designs_values() {
    let now = Policy::now();
    assert_eq!(
        (
            now.nodes,
            now.hot_replicas,
            now.data_blocks,
            now.parity_blocks
        ),
        (6, 2, 4, 2)
    );
    assert_eq!((now.destage_max_age_h, now.partial_after_h), (24, 72));
    assert_eq!(now.seal_bytes, 1023 << 20);
    assert_eq!(now.seal_min_bytes, 256 << 20);
    assert_eq!(now.large_min, 4 << 20);
    assert_eq!(now.pack_block_bytes, (1 << 30) - (1 << 20));
    assert_eq!((now.pack_window, now.align, now.classes_max), (64, 4096, 8));
    assert_eq!(now.rewrite_below, [0.2, 0.3, 0.5]);
    let later = Policy::later();
    assert_eq!(later.name, "later");
    assert_eq!(
        (
            later.nodes,
            later.hot_replicas,
            later.data_blocks,
            later.parity_blocks
        ),
        (3, 3, 4, 4)
    );
    assert_eq!((later.destage_max_age_h, later.partial_after_h), (72, 168));
    assert_eq!((now.gc_live_below, now.record_header), (0.5, 64));
    let copies = Policy::now_replicas();
    assert_eq!(copies.name, "now-replicas");
    assert_eq!((copies.data_blocks, copies.parity_blocks), (1, 1));
    assert_eq!(
        Policy {
            name: copies.name.clone(),
            data_blocks: 4,
            parity_blocks: 2,
            ..copies
        },
        Policy {
            name: "now-replicas".to_owned(),
            ..now.clone()
        }
    );
    assert_eq!(now.aligned(1), 4096);
    assert_eq!(now.aligned(4096), 4096);
    assert_eq!(now.aligned(4097), 8192);
}

/// NOW has C(6,2) = 15 open segments per class (unordered pairs, one writer each), LATER has 3
/// (one replica set, three writers); a choice that is not every node counts the unordered sets.
#[test]
fn open_segments_per_class_follow_the_replica_sets() {
    assert_eq!(Policy::now().open_segments(), 15);
    assert_eq!(Policy::later().open_segments(), 3);
    let p = |nodes, hot_replicas| Policy {
        nodes,
        hot_replicas,
        ..tiny()
    };
    assert_eq!(p(6, 1).open_segments(), 6);
    assert_eq!(p(6, 5).open_segments(), 6);
    assert_eq!(p(6, 3).open_segments(), 20);
    assert_eq!(p(1, 1).open_segments(), 1);
}

/// Keys spread over NOW's slots fill 15 distinct open segments of one class and no more.
#[test]
fn objects_of_one_class_open_15_segments_under_now_and_3_under_later() {
    for (p, want) in [(Policy::now(), 15), (Policy::later(), 3)] {
        let mut sim = Sim::new(&p);
        for h in 0..want * 4 {
            let mut it = item(0, 10, 500, 0);
            it.writer_hash = h;
            sim.arrive(&it, 0);
        }
        assert_eq!(sim.open.len() as u64, want);
    }
}

/// No objects give an empty replay.
#[test]
fn no_objects_give_an_empty_replay() {
    assert_eq!(replay(&tiny(), Vec::new()), Replay::default());
}

/// Objects given out of order replay as if in order, and one sample is taken per day.
#[test]
fn objects_out_of_order_replay_in_order_with_a_sample_per_day() {
    let items = vec![item(30, 10, 40, 0), item(0, 10, 10, 0)];
    let r = replay(&tiny(), items);
    assert_eq!(r.samples.len(), 22);
    assert_eq!(r.samples[0].day, 0);
    assert_eq!(r.samples[2].day, 2);
    assert_eq!(r.end_of_data.sample.day, 1);
    assert_eq!(r.end_of_replay.sample.day, (30 + 20 * 24) / 24);
}
