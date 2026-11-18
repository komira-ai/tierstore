//! The families every node renders, whatever its role: the build, durability cost shared by the
//! cache journal and the hot tier, runtime saturation and the per-disk queues.

use crate::groups::group;
use crate::labels::{Disk, Version};
use crate::{CounterVec, GaugeVec, HistogramVec};

group! {
    /// What every node renders.
    Node (Node) {
        tierstore_build_info: GaugeVec<Version> = "tierstore_build_info" as "tierstore_build_info",
            "Always 1, labelled with the release the process runs.";
        fdatasync_seconds: HistogramVec<()> = "fdatasync_seconds",
            "Time of one fdatasync, of the cache journal or the hot tier.",
            SECONDS;
        blocking_pool_queue_len: GaugeVec<()> = "blocking_pool_queue_len",
            "Tasks queued for the blocking thread pool.";
        r_await: GaugeVec<Disk> = "r_await" as "tierstore_disk_r_await_seconds",
            "Average time of a disk's reads over the last interval, in seconds.";
        aqu_sz: GaugeVec<Disk> = "aqu_sz" as "tierstore_disk_aqu_sz",
            "Average queue length of a disk over the last interval.";
        util: GaugeVec<Disk> = "util" as "tierstore_disk_util",
            "Fraction of the last interval a disk was busy.";
        store_copy_generation_mismatch_total: CounterVec<()> = "store_copy_generation_mismatch_total",
            "Copies whose generation differed from the one expected.";
    }
}
