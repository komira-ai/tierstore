//! The drives of a storage node (10.7, 10.8): scheduling and liveness, per drive.

use tierstore_format::ids::DriveId;
use tierstore_store_api::sched::DriveClass;

use crate::groups::group;
use crate::labels::{Dir, Drive503, DriveState};
use crate::{CounterVec, GaugeVec, HistogramVec, StateSetVec};

group! {
    /// Per drive of the node, scheduling and liveness: the cold store registers its hard drives
    /// and the hot store its device.
    Drives {
        drive_state: StateSetVec<DriveId, DriveState> = "drive_state",
            "A drive's state: 1 for the state, 0 for the others.";
        drive_streams_active: GaugeVec<DriveId> = "drive_streams_active",
            "Streams of a drive that hold a slot.";
        drive_queue_depth: GaugeVec<(DriveId, DriveClass)> = "drive_queue_depth",
            "Requests queued on a drive, by class.";
        drive_wait_seconds: HistogramVec<(DriveId, DriveClass)> = "drive_wait_seconds",
            "Time a request waited for a slot of a drive, by class.",
            SECONDS;
        drive_service_seconds: HistogramVec<DriveId> = "drive_service_seconds",
            "Time a drive took to serve one slice.",
            SECONDS;
        drive_bytes_total: CounterVec<(DriveId, Dir)> = "drive_bytes_total",
            "Bytes read from or written to a drive.";
        drive_slice_yields_total: CounterVec<DriveId> = "drive_slice_yields_total",
            "Streams that yielded their slot at the end of a slice.";
        drive_preemptions_total: CounterVec<DriveId> = "drive_preemptions_total",
            "Streams preempted by a demand read.";
        drive_slot_releases_total: CounterVec<DriveId> = "drive_slot_releases_total",
            "Slots released by a slow consumer.";
        drive_parked_streams: GaugeVec<DriveId> = "drive_parked_streams",
            "Streams parked without a slot.";
        drive_parked_buffer_bytes: GaugeVec<DriveId> = "drive_parked_buffer_bytes",
            "Bytes read ahead and held for parked streams.";
        drive_small_reads_total: CounterVec<DriveId> = "drive_small_reads_total",
            "Reads served by the small lane.";
        drive_503_total: CounterVec<(DriveId, Drive503)> = "drive_503_total",
            "Requests a drive answered 503, by reason.";
        drive_stall_total: CounterVec<DriveId> = "drive_stall_total",
            "Times the watchdog marked a drive stalled.";
        drive_io_threads_stuck: GaugeVec<DriveId> = "drive_io_threads_stuck",
            "I/O threads of a drive in a call outstanding past stall_after.";
        drive_oldest_inflight_seconds: GaugeVec<DriveId> = "drive_oldest_inflight_seconds",
            "Age of a drive's oldest outstanding call, in seconds.";
        drive_eio_total: CounterVec<DriveId> = "drive_eio_total",
            "I/O errors of a drive.";
        drive_index_ready: GaugeVec<DriveId> = "drive_index_ready",
            "1 when a drive's index is built, else 0.";
        drive_util: GaugeVec<DriveId> = "drive_util",
            "Fraction of the last interval a drive's I/O threads were busy.";
    }
}
