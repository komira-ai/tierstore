//! Crash property: whatever batches a crash keeps after the last persist, a restart that replays
//! the facts after the replay mark gives the index the run that never crashed has.
//!
//! A workload is a list of facts from a fixed-seed generator, applied to the live index in small
//! groups with `flush_mark` between some of them. The mark records how many facts the flushed store
//! reflects. After every group, for every prefix of the batches not yet persisted, the store a
//! crash leaves is reopened and the facts after its mark are replayed; its dump must equal the
//! live index's final dump.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

#[allow(dead_code, reason = "each test binary uses some of the shared helpers")]
mod index_support;

use index_support::*;
use tierstore_format::ids::BlockId;
use tierstore_format::record::RecordFlags;
use tierstore_format::tomb::{LogEntry, Named};
use tierstore_index::KvIndex;
use tierstore_index::backend::MemBackend;
use tierstore_store_api::index::{Fact, Index, ReplayMark};

/// A fixed-seed xorshift generator: the workloads are the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const KEYS: [&[u8]; 5] = [b"a", b"b", b"c", b"d/1", b"d/2"];

/// The facts of one workload: records (some relocate-flagged), relocates, and log entries of three
/// origins with consecutive sequence numbers. A tombstone names every block a record or relocate of
/// its key put the key in earlier in the workload, as the writer that issued it would have found.
fn workload(seed: u64, len: usize) -> Vec<Fact> {
    let mut rng = Rng(seed);
    let mut seqs = [0u64; 3];
    let mut facts = Vec::new();
    let mut blocks_of: Vec<std::collections::BTreeSet<u64>> = vec![Default::default(); KEYS.len()];
    while facts.len() < len {
        let ki = rng.below(KEYS.len() as u64) as usize;
        let key = KEYS[ki];
        let g = 1 + rng.below(3) as u8;
        let b = 5 + rng.below(4);
        let s = 1 + rng.below(30);
        let fact = match rng.below(10) {
            0..=3 => {
                let mut e = entry(g, b, rng.below(4) * 64, s, 10 + rng.below(5));
                if rng.below(4) == 0 {
                    e.flags = RecordFlags::RELOCATE;
                }
                blocks_of[ki].insert(b);
                record(key, e)
            }
            4 | 5 => {
                let to = 5 + rng.below(4);
                blocks_of[ki].insert(to);
                relocate(key, g, s, at(to, rng.below(4) * 64))
            }
            6 | 7 => {
                let o = rng.below(3) as usize;
                seqs[o] += 1;
                log_tomb(
                    2 + o as u8,
                    seqs[o],
                    tomb(
                        key,
                        g,
                        s,
                        blocks_of[ki]
                            .iter()
                            .map(|n| Named::Block(block(*n)))
                            .collect(),
                    ),
                )
            }
            8 => {
                let o = rng.below(3) as usize;
                seqs[o] += 1;
                if rng.below(2) == 0 {
                    log_drop(2 + o as u8, seqs[o], b)
                } else {
                    log_report(
                        2 + o as u8,
                        seqs[o],
                        2 + rng.below(3) as u8,
                        1 + rng.below(3),
                    )
                }
            }
            _ => {
                let o = rng.below(3) as usize;
                seqs[o] += 1;
                log(
                    2 + o as u8,
                    seqs[o],
                    LogEntry::Unknown {
                        kind: 200,
                        body: vec![1],
                    },
                )
            }
        };
        facts.push(fact);
    }
    facts
}

/// The mark that says the flushed store reflects the first `n` facts.
fn mark_of(n: usize) -> ReplayMark {
    ReplayMark {
        open_segments: vec![(BlockId::from_raw(0), n as u64)],
        tomb_cursors: vec![],
    }
}

fn applied_by(mark: &ReplayMark) -> usize {
    mark.open_segments.first().map_or(0, |(_, n)| *n as usize)
}

/// What a reader sees: every key's lookup and the whole listing.
fn view(index: &KvIndex<MemBackend>) -> Vec<String> {
    let mut out: Vec<String> = KEYS
        .iter()
        .map(|k| format!("{:?}", index.get(k).unwrap()))
        .collect();
    out.push(format!("{:?}", index.list(b"", None, 100).unwrap()));
    out
}

fn run(seed: u64, len: usize) {
    let facts = workload(seed, len);
    let mut rng = Rng(seed ^ 0x9e37_79b9);
    let live = ready();
    let mut snapshots: Vec<(MemBackend, ReplayMark)> = Vec::new();
    let mut flushed = None;
    let mut at_fact = 0;
    while at_fact < facts.len() {
        let group = (1 + rng.below(3) as usize).min(facts.len() - at_fact);
        let slice = facts[at_fact..at_fact + group].to_vec();
        live.replay(&mut slice.into_iter()).unwrap();
        at_fact += group;
        if rng.below(4) == 0 {
            live.flush_mark(&mark_of(at_fact)).unwrap();
            flushed = Some(mark_of(at_fact));
        }
        for keep in 0..=live.backend().unpersisted() {
            let crashed = live.backend().crashed(keep);
            let mark = KvIndex::open(crashed.crashed(usize::MAX), config())
                .unwrap()
                .replay_mark()
                .unwrap()
                .unwrap_or_default();
            snapshots.push((crashed, mark));
        }
    }
    let final_dump = live.backend().dump();
    let final_view = view(&live);
    assert!(
        snapshots.len() > len / 4,
        "every group has at least the persisted state"
    );
    for (n, (backend, mark)) in snapshots.into_iter().enumerate() {
        let restarted = KvIndex::open(backend, config()).unwrap();
        let rest = facts[applied_by(&mark)..].to_vec();
        restarted.replay(&mut rest.into_iter()).unwrap();
        if let Some(m) = &flushed {
            restarted.flush_mark(m).unwrap();
        }
        restarted.set_ready(true);
        let got = restarted.backend().dump();
        let differing: Vec<_> = got
            .iter()
            .filter(|row| !final_dump.contains(row))
            .chain(final_dump.iter().filter(|row| !got.contains(row)))
            .collect();
        assert!(
            differing.is_empty(),
            "seed {seed}: snapshot {n} (mark at fact {}) differs in {differing:?}",
            applied_by(&mark)
        );
        assert_eq!(view(&restarted), final_view, "seed {seed}: snapshot {n}");
    }
}

#[test]
fn a_crash_at_any_prefix_replayed_from_the_mark_gives_the_never_crashed_index() {
    for seed in [1, 2, 3, 4, 5, 6, 7, 8] {
        run(seed, 40);
    }
}

#[test]
fn a_crash_of_a_workload_with_many_flushes_replays_from_the_latest_mark() {
    for seed in [101, 102, 103] {
        run(seed, 90);
    }
}

#[test]
fn crashed_stores_keep_the_whole_batches_before_the_cut_and_nothing_after() {
    let index = ready();
    index.flush_mark(&mark_of(0)).unwrap();
    for i in 0..4u64 {
        replay(
            &index,
            vec![record(format!("k{i}").as_bytes(), entry(1, 5, i, 5, 1))],
        );
    }
    for keep in 0..=4 {
        let crashed = KvIndex::open(index.backend().crashed(keep as usize), config()).unwrap();
        crashed.set_ready(true);
        assert_eq!(count(&crashed, 5), lc(keep, keep), "keep {keep}");
        assert_eq!(crashed.replay_mark().unwrap(), Some(mark_of(0)));
    }
}
