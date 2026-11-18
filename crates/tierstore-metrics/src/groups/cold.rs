//! The cold tier (10.4, 10.12) and the movement between the tiers (10.5, 10.9).

use tierstore_format::ids::DriveId;

use crate::groups::group;
use crate::labels::{
    DegradedCause, DestageOutcome, DestagePriority, FreeOutcome, Margin, PromoteOutcome,
    RebuildKind, StripeCode, StripeState,
};
use crate::{CounterVec, GaugeVec, HistogramVec, StateSetVec};

group! {
    /// The cold tier: redundancy margin, degraded service, rebuild and scrub. `{drive}` is a
    /// drive of the node.
    Cold (Storage) {
        cold_stripes: GaugeVec<(StripeCode, StripeState)> = "cold_stripes",
            "Stripes, by code and state.";
        cold_stripe_margin: GaugeVec<Margin> = "cold_stripe_margin",
            "Stripes, by how many more shards each can lose and stay readable.";
        cold_stripes_below_tolerance: GaugeVec<()> = "cold_stripes_below_tolerance",
            "Stripes that cannot lose a failure domain and stay readable.";
        cold_partial_stripes_total: CounterVec<()> = "cold_partial_stripes_total",
            "Partial stripes written.";
        cold_orphan_stripes_total: CounterVec<()> = "cold_orphan_stripes_total",
            "Orphan shard files the sweep removed.";
        cold_placement_violations: GaugeVec<()> = "cold_placement_violations",
            "Stripes whose placement breaks the placement rule.";
        cold_degraded_reads_total: CounterVec<DegradedCause> = "cold_degraded_reads_total",
            "Cold reads served by decoding, by cause.";
        cold_degraded_read_seconds: HistogramVec<()> = "cold_degraded_read_seconds",
            "Time of a degraded read.",
            SECONDS;
        cold_degraded_slot_charges_total: CounterVec<()> = "cold_degraded_slot_charges_total",
            "Drive slots charged to degraded reads.";
        cold_fill_efficiency: GaugeVec<()> = "cold_fill_efficiency",
            "Live data bytes over the raw bytes the cold tier uses.";
        cold_rebuild_over_fill_limit_bytes: GaugeVec<()> = "cold_rebuild_over_fill_limit_bytes",
            "Bytes rebuild wrote above fill_limit.";
        cold_hedged_degraded_total: CounterVec<()> = "cold_hedged_degraded_total",
            "Degraded reads sent as a hedge.";
        cold_rebuild_bytes_total: CounterVec<RebuildKind> = "cold_rebuild_bytes_total",
            "Bytes rebuild wrote, by kind.";
        cold_rebuild_queue_depth: GaugeVec<Margin> = "cold_rebuild_queue_depth",
            "Stripes queued for rebuild, by margin.";
        cold_scrub_bytes_total: CounterVec<()> = "cold_scrub_bytes_total",
            "Bytes the scrub read.";
        cold_scrub_bad_total: CounterVec<()> = "cold_scrub_bad_total",
            "Shards the scrub found bad.";
        cold_deep_scrub_mismatch_total: CounterVec<()> = "cold_deep_scrub_mismatch_total",
            "Stripes whose parity the deep scrub found inconsistent.";
        cold_fill_ratio: GaugeVec<DriveId> = "cold_fill_ratio",
            "Fraction of a drive's filesystem in use.";
    }
}

group! {
    /// Destage from the hot tier to the cold tier, promotion back, and garbage collection.
    Movement (Storage) {
        destage_bytes_total: CounterVec<()> = "destage_bytes_total",
            "Bytes destaged.";
        destage_stripes_total: CounterVec<DestageOutcome> = "destage_stripes_total",
            "Stripes destaged, by outcome.";
        destage_queue_bytes: GaugeVec<()> = "destage_queue_bytes",
            "Bytes sealed and waiting for destage.";
        destage_priority: StateSetVec<(), DestagePriority> = "destage_priority",
            "The priority destage runs at: 1 for the priority, 0 for the other.";
        destage_stalled_seconds: GaugeVec<()> = "destage_stalled_seconds",
            "Time destage has made no progress while it had work, in seconds.";
        promote_total: CounterVec<PromoteOutcome> = "promote_total",
            "Promotions, by outcome.";
        promote_bytes_total: CounterVec<()> = "promote_bytes_total",
            "Bytes promoted.";
        gc_dead_ratio: GaugeVec<()> = "gc_dead_ratio",
            "Dead bytes over all bytes of the cold tier.";
        gc_stripe_free_total: CounterVec<FreeOutcome> = "gc_stripe_free_total",
            "Stripe-free rounds, by outcome.";
        gc_stripes_freed_total: CounterVec<()> = "gc_stripes_freed_total",
            "Stripes freed.";
        gc_bytes_freed_total: CounterVec<()> = "gc_bytes_freed_total",
            "Bytes freed.";
        gc_rewrite_bytes_total: CounterVec<()> = "gc_rewrite_bytes_total",
            "Live bytes the GC rewrite copied.";
        gc_classes_in_use: GaugeVec<()> = "gc_classes_in_use",
            "Expiry classes with data.";
    }
}
