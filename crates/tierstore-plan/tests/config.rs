//! The planner's configuration (`config`): the gap rule of section 6 step 3 for both deployment
//! profiles of section 21, its boundaries, and the medium of each device.
//!
//! Catches:
//! - a gap rule that rounds down, so a cold drive pays a positioning to save less than one;
//! - a gap rule that merges on a hot device, where seeks cost nothing;
//! - a profile whose drives the schema cannot describe;
//! - a degraded source planned as if it were a hot copy.

use tierstore_fetch_api::upstream::DeviceRef;
use tierstore_format::ids::{DriveId, NodeId};
use tierstore_plan::config::{DriveModel, Medium, PlanConfig};

const MIB: u64 = 1 << 20;

#[test]
fn the_default_is_the_starting_constants_of_section_6() {
    assert_eq!(
        PlanConfig::default(),
        PlanConfig {
            hot: DriveModel {
                positioning_us: 0,
                media_mb_per_s: 3_000,
            },
            cold: DriveModel {
                positioning_us: 12_000,
                media_mb_per_s: 190,
            },
            whole_cov_percent: 50,
            small_pack_bytes: 64 * MIB,
            small_whole_cov_percent: 20,
            backlog_ms: 2_000,
        }
    );
}

#[test]
fn later_profile_a_sas_drive_reads_through_three_cells_and_the_nvme_through_none() {
    // 12 ms at 190 MB/s is 2,280,000 bytes: 2.17 cells of 1 MiB, rounded up.
    let config = PlanConfig::default();
    assert_eq!(config.gap_cells(Medium::Cold, 20), 3);
    assert_eq!(config.gap_cells(Medium::Hot, 20), 0);
}

#[test]
fn now_profile_an_smr_drive_reads_through_two_cells_and_the_sata_ssd_through_none() {
    // The NOW drives as T0 measured them: 119 MB/s sequential and 43 MB/s at 1 MiB scattered, so
    // one positioning is 1 MiB / 43 MB/s - 1 MiB / 119 MB/s = 15.574 ms; 15,574 us at 119 MB/s is
    // 1,853,306 bytes, 1.77 cells.
    let now = PlanConfig {
        hot: DriveModel {
            positioning_us: 0,
            media_mb_per_s: 500,
        },
        cold: DriveModel {
            positioning_us: 15_574,
            media_mb_per_s: 119,
        },
        ..PlanConfig::default()
    };
    assert_eq!(now.gap_cells(Medium::Cold, 20), 2);
    assert_eq!(now.gap_cells(Medium::Hot, 20), 0);
}

#[test]
fn a_positioning_worth_exactly_one_cell_is_one_cell_and_one_byte_more_is_two() {
    let one = DriveModel {
        positioning_us: 1_048_576,
        media_mb_per_s: 1,
    };
    assert_eq!(one.gap_cells(20), 1);
    let over = DriveModel {
        positioning_us: 1_048_577,
        media_mb_per_s: 1,
    };
    assert_eq!(over.gap_cells(20), 2);
}

#[test]
fn no_positioning_cost_or_no_media_rate_is_no_gap() {
    for model in [
        DriveModel {
            positioning_us: 0,
            media_mb_per_s: 190,
        },
        DriveModel {
            positioning_us: 12_000,
            media_mb_per_s: 0,
        },
    ] {
        assert_eq!(model.gap_cells(20), 0, "{model:?}");
    }
}

#[test]
fn the_gap_is_counted_in_the_packs_own_cells() {
    let cold = PlanConfig::default().cold;
    // 2,280,000 bytes in 4 KiB cells: 556.6, rounded up.
    assert_eq!(cold.gap_cells(12), 557);
    // In 2 MiB cells: 1.09.
    assert_eq!(cold.gap_cells(21), 2);
}

#[test]
fn a_cell_wider_than_the_gap_bytes_is_one_cell_whatever_the_shift() {
    let cold = PlanConfig::default().cold;
    assert_eq!(cold.gap_cells(64), 1);
    assert_eq!(cold.gap_cells(127), 1);
    assert_eq!(cold.gap_cells(128), 1);
    assert_eq!(cold.gap_cells(255), 1);
    let none = DriveModel {
        positioning_us: 0,
        media_mb_per_s: 0,
    };
    assert_eq!(none.gap_cells(255), 0);
}

#[test]
fn a_gap_wider_than_u32_cells_saturates() {
    let huge = DriveModel {
        positioning_us: u32::MAX,
        media_mb_per_s: u32::MAX,
    };
    assert_eq!(huge.gap_cells(0), u32::MAX);
}

#[test]
fn a_hot_copy_is_hot_and_a_drive_is_cold() {
    let node = NodeId(2);
    assert_eq!(Medium::of(DeviceRef::Hot(node)), Medium::Hot);
    assert_eq!(
        Medium::of(DeviceRef::Drive(node, DriveId([7; 16]))),
        Medium::Cold
    );
}
