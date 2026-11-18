//! The storage side of a node: the hot tier (10.3), and the S3 front, index, audit and migration
//! (10.2, 10.6, 10.9 to 10.11).

use tierstore_format::ids::NodeId;

use crate::groups::group;
use crate::labels::{
    AbandonReason, Copies, LocateResult, PutOutcome, ReclaimKind, RecoveryPhase, SealReason,
    SealRound,
};
use crate::{CounterVec, GaugeVec, HistogramVec};

group! {
    /// The hot tier (10.3): load, replication health and backpressure. `{node}` is a peer of
    /// the node's replica sets.
    Hot (Storage) {
        hot_dirty_bytes: GaugeVec<()> = "hot_dirty_bytes",
            "Bytes of the hot tier not yet destaged.";
        hot_fill_ratio: GaugeVec<()> = "hot_fill_ratio",
            "Fraction of the hot device in use.";
        hot_clean_bytes: GaugeVec<()> = "hot_clean_bytes",
            "Bytes of the hot tier already destaged and kept as clean copies.";
        hot_promo_bytes: GaugeVec<()> = "hot_promo_bytes",
            "Bytes of the hot tier in cache segments of promoted copies.";
        hot_batch_seconds: HistogramVec<()> = "hot_batch_seconds",
            "Time of one group commit, from its first record to its acknowledgement.",
            SECONDS;
        hot_batch_bytes: HistogramVec<()> = "hot_batch_bytes",
            "Bytes of one group commit.",
            BYTES;
        hot_replica_ack_seconds: HistogramVec<NodeId> = "hot_replica_ack_seconds",
            "Time from sending a batch to a replica to its acknowledgement, by replica.",
            SECONDS;
        hot_replica_lag_bytes: GaugeVec<NodeId> = "hot_replica_lag_bytes",
            "Bytes sent to a replica and not yet acknowledged, by replica.";
        hot_seal_total: CounterVec<SealReason> = "hot_seal_total",
            "Segments sealed, by reason.";
        hot_early_seals_total: CounterVec<()> = "hot_early_seals_total",
            "Segments the writer sealed early.";
        hot_seal_rounds_total: CounterVec<SealRound> = "hot_seal_rounds_total",
            "Steps of the majority seal: rounds prepared, proposals adopted, ballots rejected.";
        hot_unacked_records_adopted_total: CounterVec<()> = "hot_unacked_records_adopted_total",
            "Records a seal kept that their writer had not acknowledged.";
        hot_open_segments: GaugeVec<()> = "hot_open_segments",
            "Open segments.";
        hot_seal_bytes: HistogramVec<()> = "hot_seal_bytes",
            "Bytes of a sealed block.",
            BYTES;
        hot_under_replicated_blocks: GaugeVec<()> = "hot_under_replicated_blocks",
            "Hot blocks with fewer live copies than hot.replicas.";
        hot_blocks_by_copies: GaugeVec<Copies> = "hot_blocks_by_copies",
            "Hot blocks, by their number of live copies.";
        hot_put_throttled_total: CounterVec<()> = "hot_put_throttled_total",
            "PUTs answered 503 by the dirty marks.";
        hot_reclaim_bytes_total: CounterVec<ReclaimKind> = "hot_reclaim_bytes_total",
            "Bytes the hot tier reclaimed, by kind.";
        hot_scrub_bad_total: CounterVec<()> = "hot_scrub_bad_total",
            "Hot records a scrub found bad.";
    }
}

group! {
    /// The S3 front, the index and its recovery, the live-key audit, and the migration from silo
    /// (10.2, 10.6, 10.9, 10.10).
    Front (Storage) {
        store_puts_total: CounterVec<PutOutcome> = "store_puts_total",
            "PUTs, by outcome.";
        store_put_copies_committed: HistogramVec<()> = "store_put_copies_committed",
            "Copies committed by one PUT.",
            COUNT;
        store_put_copies_abandoned_total: CounterVec<AbandonReason> = "store_put_copies_abandoned_total",
            "Copies of PUTs abandoned, by reason.";
        store_put_conflicts_total: CounterVec<()> = "store_put_conflicts_total",
            "PUTs that raced another PUT of the same key.";
        store_key_conflict_total: CounterVec<()> = "store_key_conflict_total",
            "PUTs refused because the key holds other bytes.";
        store_locate_total: CounterVec<LocateResult> = "store_locate_total",
            "LOCATE requests answered, by result.";
        store_definitive_miss_total: CounterVec<()> = "store_definitive_miss_total",
            "Definitive misses.";
        store_negative_cache_hits_total: CounterVec<()> = "store_negative_cache_hits_total",
            "Misses answered from the negative cache.";
        store_list_total: CounterVec<()> = "store_list_total",
            "LIST requests.";
        store_list_keys_total: CounterVec<()> = "store_list_keys_total",
            "Keys returned by LIST requests.";
        index_keys: GaugeVec<()> = "index_keys",
            "Keys in the index.";
        index_bytes: GaugeVec<()> = "index_bytes",
            "Bytes of the index on disk.";
        index_flush_seconds: HistogramVec<()> = "index_flush_seconds",
            "Time of one flush of the index.",
            SECONDS;
        index_recovery_seconds: GaugeVec<RecoveryPhase> = "index_recovery_seconds",
            "Time the last recovery of the index took, by phase, in seconds.";
        tombstones_live: GaugeVec<()> = "tombstones_live",
            "Tombstones not yet dropped.";
        cluster_stamp_wall_skew_seconds: GaugeVec<()> = "cluster_stamp_wall_skew_seconds",
            "The node's cluster stamp minus its wall clock, in seconds.";
        store_evicted_bytes_total: CounterVec<()> = "store_evicted_bytes_total",
            "Bytes evicted in the hot-only evict mode.";
        store_audit_live_unreadable_ratio: GaugeVec<()> = "store_audit_live_unreadable_ratio",
            "Fraction of the last audit's sample of live keys that could not be read.";
        store_audit_keys_total: CounterVec<()> = "store_audit_keys_total",
            "Live keys the audit read.";
        import_bytes_total: CounterVec<()> = "import_bytes_total",
            "Bytes imported from silo on a read.";
        legacy_mover_bytes_total: CounterVec<()> = "legacy_mover_bytes_total",
            "Bytes the bulk mover copied from silo.";
    }
}
