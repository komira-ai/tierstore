//! The configuration: both deployment profiles validate, and each configuration the budget
//! could not run is refused with the key and value named.
//!
//! Catches:
//! - a configuration accepted that could never grant a part (no client, no stream, zero
//!   permits), or whose ladder has no step;
//! - an error message that does not name the key and the value.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod support;

use std::sync::Arc;

use support::{MIB, later, now_profile};
use tierstore_budget::{BudgetConfig, ClientBudget, ConfigError, Topology};
use tierstore_fetch::timer::ManualTimer;

fn refused(f: impl FnOnce(&mut BudgetConfig)) -> ConfigError {
    let mut c = BudgetConfig::new(later());
    f(&mut c);
    c.validate().expect_err("refused")
}

#[test]
fn both_profiles_and_a_hot_only_install_validate() {
    for t in [
        later(),
        now_profile(),
        Topology {
            cold_drives: 0,
            drive_streams: 0,
            ..later()
        },
    ] {
        assert_eq!(BudgetConfig::new(t).validate(), Ok(()), "{t:?}");
    }
    let c = BudgetConfig::new(now_profile());
    assert_eq!((c.drive_permits, c.hot_permits), (1, 64));
}

#[test]
fn each_unrunnable_configuration_is_refused_by_name() {
    let cases: Vec<(ConfigError, &str)> = vec![
        (
            refused(|c| c.topology.clients = 0),
            "budget: topology.clients is 0; at least one client shares the storage tier",
        ),
        (
            refused(|c| c.topology.drive_streams = 0),
            "budget: topology.drive_streams is 0 with a cold tier; a drive admits at least one stream",
        ),
        (
            refused(|c| c.drive_permits = 0),
            "budget: drive_permits is 0; a device or node admits at least one part",
        ),
        (
            refused(|c| c.hot_permits = 0),
            "budget: hot_permits is 0; a device or node admits at least one part",
        ),
        (
            refused(|c| c.legacy_permits = Some(0)),
            "budget: legacy_permits is 0; a device or node admits at least one part",
        ),
        (
            refused(|c| c.node_permits = Some(0)),
            "budget: node_permits is 0; a device or node admits at least one part",
        ),
        (
            refused(|c| c.ceiling_parts = Some(1)),
            "budget: ceiling_parts is 1, below the floor of 2 (one part plus the demand reserve)",
        ),
        (
            refused(|c| c.part_floor = 0),
            "budget: part size 0 is zero, not a multiple of 1 MiB, or below part_floor",
        ),
        (
            refused(|c| c.part_floor = MIB + 1),
            "budget: part size 1048577 is zero, not a multiple of 1 MiB, or below part_floor",
        ),
        (
            refused(|c| c.part_sizes[1] = 8 * MIB),
            "budget: part size 8388608 is zero, not a multiple of 1 MiB, or below part_floor",
        ),
        (
            refused(|c| c.part_sizes[2] = 64 * MIB + 4096),
            "budget: part size 67112960 is zero, not a multiple of 1 MiB, or below part_floor",
        ),
        (
            refused(|c| c.window_parts = 0),
            "budget: window_parts is 0; a window holds at least one part",
        ),
        (
            refused(|c| c.pace_start_ms = 0),
            "budget: pace_start_ms 0 must be above 0 and at most pace_max_ms 2000",
        ),
        (
            refused(|c| c.pace_start_ms = 2_001),
            "budget: pace_start_ms 2001 must be above 0 and at most pace_max_ms 2000",
        ),
        (
            refused(|c| c.reserve_percent = 101),
            "budget: reserve_percent is 101, above 100",
        ),
    ];
    for (e, text) in cases {
        assert_eq!(e.to_string(), text);
    }
}

#[test]
fn a_budget_is_not_made_from_a_refused_configuration() {
    let mut c = BudgetConfig::new(later());
    c.topology.clients = 0;
    let timer = Arc::new(ManualTimer::default());
    assert_eq!(
        ClientBudget::new(c, timer).expect_err("refused"),
        ConfigError::NoClients
    );
    // The values just inside each bound are accepted.
    let mut c = BudgetConfig::new(later());
    c.ceiling_parts = Some(2);
    c.reserve_percent = 100;
    c.pace_start_ms = c.pace_max_ms;
    c.part_sizes = [16 * MIB; 3];
    assert_eq!(c.validate(), Ok(()));
}
