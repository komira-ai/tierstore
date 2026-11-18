//! The components of the read path: the fetch engine and its budget (section 4), the node cache
//! and its journal (sections 7, 8, 11), the FUSE front and the read replies of the fronts
//! (section 5), and the Prepare provider (sections 3, 6).

use tierstore_fetch_api::ids::Class;
use tierstore_fetch_api::verify::VerifyPriority;
use tierstore_format::ids::DriveId;

use crate::groups::group;
use crate::labels::{
    AdmissionReason, BudgetReason, FuseOp, GetKind, ListRefusal, PackLostPhase, ReadWaitCause,
    SizeClass, TransportMode,
};
use crate::{CounterVec, GaugeVec, HistogramVec, StateSetVec};

group! {
    /// The fetch engine and its budget (section 4): waits, load, controller state, tail control,
    /// GET counts, efficiency, read waits and lost fills. `{drive}` is any drive
    /// of the cluster the engine reads from.
    Fetch {
        part_permit_wait_seconds: HistogramVec<Class> = "part_permit_wait_seconds",
            "Time a part waited for its budget permits (drive, storage node and client ceiling).",
            SECONDS;
        part_net_seconds: HistogramVec<(Class, SizeClass)> = "part_net_seconds",
            "Network time of a part, its last byte minus its first byte (a controller input).",
            SECONDS;
        part_first_byte_seconds: HistogramVec<(Class, SizeClass)> = "part_first_byte_seconds",
            "Time from a part's GET to its first byte (a controller input).",
            SECONDS;
        part_hash_wait_seconds: HistogramVec<()> = "part_hash_wait_seconds",
            "Time a part's receive paused for a full hash pool queue, kept out of the controller.",
            SECONDS;
        part_write_seconds: HistogramVec<()> = "part_write_seconds",
            "Time to write a part's cells to the cache, kept out of the controller.",
            SECONDS;
        bytes_in_flight: GaugeVec<()> = "bytes_in_flight",
            "Bytes of the parts in flight.";
        parts_in_flight: GaugeVec<()> = "parts_in_flight",
            "Parts in flight; a degraded read counts its k parts.";
        parts_per_drive: GaugeVec<DriveId> = "parts_per_drive",
            "Parts in flight to a drive.";
        budget_ceiling_parts: GaugeVec<()> = "budget_ceiling_parts",
            "The client ceiling: the parts the node may have in flight across the storage nodes.";
        budget_reason: StateSetVec<(), BudgetReason> = "budget_reason",
            "Why the controller last moved a permit count or the ceiling: 1 for the reason, 0 for the others.";
        budget_ladder_step: GaugeVec<()> = "budget_ladder_step",
            "The step of the client ceiling's decrease ladder: 0 none, 1 fewer parts, 2 a smaller part size.";
        drive_permits: GaugeVec<DriveId> = "drive_permits",
            "The permits of a drive (per_drive_max).";
        drive_decrease_state: GaugeVec<DriveId> = "drive_decrease_state",
            "The step of a drive's decrease ladder: 0 none, 1 permits lowered, 2 pacing.";
        win_goodput: GaugeVec<()> = "win_goodput",
            "Goodput of the last controller window, in bytes per second.";
        win_p95: GaugeVec<()> = "win_p95",
            "The 95th percentile of network time per MiB of the last controller window, in seconds.";
        win_first_byte_p95: GaugeVec<()> = "win_first_byte_p95",
            "The 95th percentile of first-byte time of the last controller window, in seconds.";
        win_drive_wait_p95: GaugeVec<DriveId> = "win_drive_wait_p95",
            "The 95th percentile of a drive's reported queue wait of the last controller window, in seconds.";
        cluster_budget_sum: GaugeVec<()> = "cluster_budget_sum",
            "The sum of the node's permits over the storage nodes, in parts.";
        retries_total: CounterVec<()> = "retries_total",
            "Parts retried after a stall, a transport failure or a mismatch.";
        stall_retries_gated_total: CounterVec<()> = "stall_retries_gated_total",
            "Stall retries held back: past the drive's one stall retry per window, or while it is in a decrease state.";
        hedges_total: CounterVec<()> = "hedges_total",
            "Hedged GETs sent to another hot replica or as a degraded read.";
        hedge_wins_total: CounterVec<()> = "hedge_wins_total",
            "Hedged GETs that answered before the GET they hedged.";
        hedge_wasted_bytes_total: CounterVec<()> = "hedge_wasted_bytes_total",
            "Bytes received by the losing side of a hedge and thrown away.";
        stall_cancels_total: CounterVec<()> = "stall_cancels_total",
            "GETs cancelled as stalled.";
        gets_total: CounterVec<GetKind> = "gets_total",
            "GETs sent, by kind.";
        gets_per_pack: HistogramVec<()> = "gets_per_pack",
            "GETs the fill of one pack sent.",
            COUNT;
        bytes_fetched_total: CounterVec<()> = "bytes_fetched_total",
            "Bytes received from the storage tier.";
        bytes_needed_total: CounterVec<()> = "bytes_needed_total",
            "Bytes of the cells reads needed.";
        amplification: GaugeVec<()> = "amplification",
            "Bytes fetched over bytes needed.";
        bytes_prefetched_never_read_total: CounterVec<()> = "bytes_prefetched_never_read_total",
            "Bytes fetched ahead of a read and evicted without being read.";
        read_wait_seconds: HistogramVec<ReadWaitCause> = "read_wait_seconds",
            "Time a read waited for its cells, by cause.",
            SECONDS;
        reads_waiting: GaugeVec<()> = "reads_waiting",
            "Reads waiting for cells.";
        fill_wait_seconds: HistogramVec<()> = "fill_wait_seconds",
            "Time a wait for a fill took, from the mount's wait to its answer.",
            SECONDS;
        launch_before_ready_total: CounterVec<()> = "launch_before_ready_total",
            "Reads of cells not yet present because the job launched before Prepare was done.";
        store_pack_lost_total: CounterVec<PackLostPhase> = "store_pack_lost_total",
            "Fills found lost (no copy readable), by when.";
    }
}

group! {
    /// The node cache (sections 4.7, 7, 8, 11): verification, the journal, holds and eviction,
    /// and the cache device.
    Cache {
        verify_seconds: HistogramVec<VerifyPriority> = "verify_seconds",
            "Time to hash and check one cell.",
            SECONDS;
        verify_queue_depth: GaugeVec<()> = "verify_queue_depth",
            "Cells queued for the hash pool.";
        verify_lag_seconds: GaugeVec<()> = "verify_lag_seconds",
            "Age of the oldest cell queued for the hash pool, in seconds.";
        verify_failures_total: CounterVec<()> = "verify_failures_total",
            "Cells whose hash did not match.";
        poisoned_cells: GaugeVec<()> = "poisoned_cells",
            "Cells marked bad.";
        recheck_cells: GaugeVec<()> = "recheck_cells",
            "Cells to hash again after a restart.";
        journal_commit_seconds: HistogramVec<()> = "journal_commit_seconds",
            "Time to commit one batch of the cache journal.",
            SECONDS;
        journal_records: HistogramVec<()> = "journal_records",
            "Records in one batch of the cache journal.",
            COUNT;
        held_bytes: GaugeVec<()> = "held_bytes",
            "Bytes of the cache held by jobs.";
        holds: GaugeVec<()> = "holds",
            "Holds of jobs on packs.";
        admission_refusals_total: CounterVec<AdmissionReason> = "admission_refusals_total",
            "Admissions to the cache refused, by reason.";
        punched_bytes_total: CounterVec<()> = "punched_bytes_total",
            "Bytes punched out of the cache by eviction.";
        evictions_total: CounterVec<()> = "evictions_total",
            "Cells evicted.";
        evict_timeouts_total: CounterVec<()> = "evict_timeouts_total",
            "Evictions abandoned because readers did not leave in time.";
        holder_lifeline_hangups_total: CounterVec<()> = "holder_lifeline_hangups_total",
            "Holders dropped because their lifeline hung up.";
        dirty_bytes: GaugeVec<()> = "dirty_bytes",
            "Dirty bytes of the cache device not yet written back.";
        writeback_stall_seconds: HistogramVec<()> = "writeback_stall_seconds",
            "Time a cache write waited for writeback.",
            SECONDS;
        hash_pool_busy: GaugeVec<()> = "hash_pool_busy",
            "Hash pool threads busy.";
    }
}

group! {
    /// The FUSE front: requests, transport, parking, blocked threads and the mount protocol's
    /// restarts (section 5).
    Fuse {
        fuse_transport_mode: StateSetVec<(), TransportMode> = "fuse_transport_mode",
            "The FUSE transport the mounts run: 1 for the mode, 0 for the other.";
        fuse_requests_total: CounterVec<FuseOp> = "fuse_requests_total",
            "FUSE requests, by operation.";
        fuse_inline_total: CounterVec<()> = "fuse_inline_total",
            "FUSE requests answered on the thread that read them.";
        fuse_parked: GaugeVec<()> = "fuse_parked",
            "FUSE requests parked until their cells arrive.";
        fuse_request_seconds: HistogramVec<FuseOp> = "fuse_request_seconds",
            "Time from reading a FUSE request to writing its reply, by operation.",
            SECONDS;
        fuse_interrupts_total: CounterVec<()> = "fuse_interrupts_total",
            "FUSE INTERRUPT requests.";
        uring_entries_parked: GaugeVec<()> = "uring_entries_parked",
            "io_uring entries parked with a request.";
        uring_registered_bytes: GaugeVec<()> = "uring_registered_bytes",
            "Bytes of buffers registered with the io_uring queues.";
        fuse_d_state_threads: GaugeVec<()> = "fuse_d_state_threads",
            "Threads of the process blocked in uninterruptible sleep.";
        mount_reconnects_total: CounterVec<()> = "mount_reconnects_total",
            "Reconnections of mounts to tierstore.";
        takeovers_total: CounterVec<()> = "takeovers_total",
            "Takeovers of mounts by a new tierstore process.";
        stale_epoch_replies_dropped_total: CounterVec<()> = "stale_epoch_replies_dropped_total",
            "Replies of an earlier epoch dropped by a mount.";
    }
}

group! {
    /// The Prepare provider: tree readiness, request to start, and the list format's rollout
    /// (sections 3, 6).
    Prepare {
        tree_ready_seconds: HistogramVec<()> = "tree_ready_seconds",
            "Time from Prepare to every planned cell of a tree being present.",
            SECONDS;
        request_to_start_seconds: HistogramVec<()> = "request_to_start_seconds",
            "Time from the job's request to its start, across the scheduler and the mount.",
            SECONDS;
        list_refused_total: CounterVec<ListRefusal> = "list_refused_total",
            "Tree lists refused, by reason.";
        l2_sidecar_missing_total: CounterVec<()> = "l2_sidecar_missing_total",
            "Trees refused because they have no L2 sidecar.";
    }
}

group! {
    /// How the read replies of a front were sent: the FUSE front and the NFS front both register
    /// it (5.2, 5.6).
    ReadReply {
        splice_bytes_total: CounterVec<()> = "splice_bytes_total",
            "Bytes of read replies sent by splice.";
        pread_bytes_total: CounterVec<()> = "pread_bytes_total",
            "Bytes of read replies sent from a pread buffer.";
        fadvise_dontneed_bytes_total: CounterVec<()> = "fadvise_dontneed_bytes_total",
            "Bytes dropped from the page cache with POSIX_FADV_DONTNEED.";
    }
}
