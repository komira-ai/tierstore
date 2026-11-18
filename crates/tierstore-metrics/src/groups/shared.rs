//! The families every process has, and the families more than one component registers: each
//! component that uses one registers its group, and the group is made once.

use crate::groups::group;
use crate::labels::{Disk, Version};
use crate::{CounterVec, GaugeVec, HistogramVec};

group! {
    /// What every process has, whatever its components.
    Process {
        tierstore_build_info: GaugeVec<Version> = "tierstore_build_info" as "tierstore_build_info",
            "Always 1, labelled with the release the process runs.";
        blocking_pool_queue_len: GaugeVec<()> = "blocking_pool_queue_len",
            "Tasks queued for the blocking thread pool.";
    }
}

group! {
    /// The cost of durability: the node cache's journal and the hot store both register it.
    Durability {
        fdatasync_seconds: HistogramVec<()> = "fdatasync_seconds",
            "Time of one fdatasync, of the cache journal or the hot tier.",
            SECONDS;
    }
}

group! {
    /// The per-disk queues: the node cache registers its device, and the hot and cold stores
    /// their drives.
    Disks {
        r_await: GaugeVec<Disk> = "r_await" as "tierstore_disk_r_await_seconds",
            "Average time of a disk's reads over the last interval, in seconds.";
        aqu_sz: GaugeVec<Disk> = "aqu_sz" as "tierstore_disk_aqu_sz",
            "Average queue length of a disk over the last interval.";
        util: GaugeVec<Disk> = "util" as "tierstore_disk_util",
            "Fraction of the last interval a disk was busy.";
    }
}

group! {
    /// Copies whose generation was not the one expected: the fetch engine (a GET's `If-Match`)
    /// and the S3 front both register it.
    CopyGeneration {
        store_copy_generation_mismatch_total: CounterVec<()> = "store_copy_generation_mismatch_total",
            "Copies whose generation differed from the one expected.";
    }
}
