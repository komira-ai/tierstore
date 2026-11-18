//! The hot store's settings (the `[hot]` section of 10.14, with the `destage.max_age` and block
//! geometry the seal triggers read), their defaults and the checks of 10.14 that are the hot
//! tier's own.
//!
//! T-S1 parses the configuration file and the flags; this is what the hot store is handed. The
//! defaults are those of 10.14 (`later-storage.toml`); the NOW profile of section 21.1 differs
//! only in values (`replicas = 2`, `min_acks = 2`, a SATA SSD partition), which is why nothing
//! here names a profile.

use tierstore_format::block::BlockGeometry;

const MIB: u64 = 1 << 20;
const SECOND_MS: u64 = 1000;
const HOUR_MS: u64 = 3600 * SECOND_MS;
const DAY_MS: u64 = 24 * HOUR_MS;

/// The dirty marks of 10.5, each a fraction of the hot device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Marks {
    /// `hot.dirty_destage_stop`.
    pub destage_stop: f64,
    /// `hot.dirty_destage_start` (also: no promotion above it).
    pub destage_start: f64,
    /// `hot.dirty_urgent`.
    pub urgent: f64,
    /// `hot.dirty_slow`: PUTs answer 503 `Retry-After` above it.
    pub slow: f64,
    /// `hot.dirty_reject`: PUTs answer 507 above it (after reclaim; also the total fill).
    pub reject: f64,
}

impl Default for Marks {
    fn default() -> Self {
        Self {
            destage_stop: 0.35,
            destage_start: 0.50,
            urgent: 0.75,
            slow: 0.85,
            reject: 0.90,
        }
    }
}

/// The hot store's settings.
#[derive(Debug, Clone, PartialEq)]
pub struct HotConfig {
    /// `hot.replicas`: copies of every record and large file.
    pub replicas: u8,
    /// `hot.min_acks`: durable copies (the writer's own included) before a PUT is acknowledged.
    pub min_acks: u8,
    /// `hot.large_min`: above it an object is a whole file, at or below a record.
    pub large_min: u64,
    /// `hot.seal_bytes`: a segment seals when the next record does not fit below it.
    pub seal_bytes: u64,
    /// `hot.seal_max_age` in milliseconds: the age seal, only at or above `seal_min_bytes`.
    pub seal_max_age_ms: u64,
    /// `hot.seal_min_bytes`: below it the age seal waits until `destage_max_age_ms`.
    pub seal_min_bytes: u64,
    /// `destage.max_age` in milliseconds: a segment older than it seals whatever its size.
    pub destage_max_age_ms: u64,
    /// `hot.batch_max`: the bytes of one group commit at most.
    pub batch_max: u64,
    /// `hot.replica_stall` in milliseconds: a replica silent this long makes the writer seal early.
    pub replica_stall_ms: u64,
    /// `hot.put_copy_stall` in milliseconds: a replica of a large PUT lagging this long is
    /// abandoned and replaced by a re-send of the finished file.
    pub put_copy_stall_ms: u64,
    /// `hot.queue_depth`: requests `HotSched` runs on the device at once (its I/O threads).
    pub queue_depth: usize,
    /// The stall watchdog of `HotSched` (the drive scheduler's `stall_after`, 10.7).
    pub stall_after_ms: u64,
    /// The dirty marks.
    pub marks: Marks,
    /// `hot.keep_clean`: keep destaged copies as a read cache until space is needed.
    pub keep_clean: bool,
    /// `hot.scrub_period` in milliseconds.
    pub scrub_period_ms: u64,
    /// Hot-only mode (no cold store, `hot.on_full = "reject"`): the marks count every hot byte,
    /// since nothing is ever clean.
    pub hot_only: bool,
    /// The bytes of the hot device (its filesystem's size), the denominator of the marks.
    pub capacity: u64,
    /// The block geometry (`cold.block_bytes`, `cold.block_reserve`, or their defaults without a
    /// cold store).
    pub geometry: BlockGeometry,
}

impl HotConfig {
    /// The defaults of 10.14 for a device of `capacity` bytes, with the default block geometry.
    #[must_use]
    pub fn defaults(capacity: u64, geometry: BlockGeometry) -> Self {
        Self {
            replicas: 3,
            min_acks: 2,
            large_min: 4 * MIB,
            seal_bytes: geometry.max_object(),
            seal_max_age_ms: DAY_MS,
            seal_min_bytes: 256 * MIB,
            destage_max_age_ms: 3 * DAY_MS,
            batch_max: 4 * MIB,
            replica_stall_ms: 5 * SECOND_MS,
            put_copy_stall_ms: 10 * SECOND_MS,
            queue_depth: 64,
            stall_after_ms: 10 * SECOND_MS,
            marks: Marks::default(),
            keep_clean: true,
            scrub_period_ms: 7 * DAY_MS,
            hot_only: false,
            capacity,
            geometry,
        }
    }

    /// The checks of 10.14 that are the hot tier's own (1, 3 and 4 for the hot keys), given the
    /// number of hot failure domains. A configuration that cannot survive its declared failure
    /// is refused, not warned about.
    pub fn validate(&self, domains: usize) -> Result<(), ConfigError> {
        let replicas = usize::from(self.replicas);
        if replicas == 0 || replicas > domains {
            return Err(ConfigError::Replicas {
                replicas: self.replicas,
                domains,
            });
        }
        if self.min_acks == 0 || self.min_acks > self.replicas {
            return Err(ConfigError::MinAcks {
                min_acks: self.min_acks,
                replicas: self.replicas,
            });
        }
        if self.seal_bytes > self.geometry.max_object() {
            return Err(ConfigError::SealBytes {
                seal_bytes: self.seal_bytes,
                max: self.geometry.max_object(),
            });
        }
        if self.large_min > self.geometry.block_bytes() / 4 {
            return Err(ConfigError::LargeMin {
                large_min: self.large_min,
            });
        }
        if self.seal_min_bytes > self.seal_bytes {
            return Err(ConfigError::SealMinBytes);
        }
        if self.batch_max == 0 || self.queue_depth == 0 || self.capacity == 0 {
            return Err(ConfigError::Zero);
        }
        let m = self.marks;
        let ordered = 0.0 < m.destage_stop
            && m.destage_stop < m.destage_start
            && m.destage_start < m.urgent
            && m.urgent < m.slow
            && m.slow < m.reject
            && m.reject <= 1.0;
        if !ordered {
            return Err(ConfigError::Marks);
        }
        Ok(())
    }
}

/// Why a configuration is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    /// `hot.replicas` is 0 or above the number of hot failure domains.
    #[error("hot.replicas = {replicas} with {domains} failure domains")]
    Replicas {
        /// `hot.replicas`.
        replicas: u8,
        /// The hot failure domains.
        domains: usize,
    },
    /// `hot.min_acks` is not between 1 and `hot.replicas`.
    #[error("hot.min_acks = {min_acks} is not between 1 and hot.replicas = {replicas}")]
    MinAcks {
        /// `hot.min_acks`.
        min_acks: u8,
        /// `hot.replicas`.
        replicas: u8,
    },
    /// `hot.seal_bytes` above `block_bytes - block_reserve`.
    #[error("hot.seal_bytes = {seal_bytes} is above block_bytes - block_reserve = {max}")]
    SealBytes {
        /// `hot.seal_bytes`.
        seal_bytes: u64,
        /// `block_bytes - block_reserve`.
        max: u64,
    },
    /// `hot.large_min` above a quarter of a block.
    #[error("hot.large_min = {large_min} is above block_bytes / 4")]
    LargeMin {
        /// `hot.large_min`.
        large_min: u64,
    },
    /// `hot.seal_min_bytes` above `hot.seal_bytes`.
    #[error("hot.seal_min_bytes is above hot.seal_bytes")]
    SealMinBytes,
    /// `hot.batch_max`, `hot.queue_depth` or the device capacity is zero.
    #[error("hot.batch_max, hot.queue_depth and the capacity must be above zero")]
    Zero,
    /// The marks are not `0 < stop < start < urgent < slow < reject <= 1`.
    #[error("the dirty marks are not ordered 0 < stop < start < urgent < slow < reject <= 1")]
    Marks,
}
