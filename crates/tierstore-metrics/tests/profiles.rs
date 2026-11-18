//! One registry serves both deployment profiles of section 21: the groups a node renders follow
//! its role, and the drive, node, disk, copies, code and margin series are seeded from its
//! configuration.
//!
//! Profiles: LATER storage node (15 hard drives and an NVMe device, two peers, `hot.replicas =
//! 3`, `4+4`); LATER build node (48 drives of the cluster, no drive of its own); NOW combined node
//! (two SMR drives and a hot SSD partition, five peers, `hot.replicas = 2`, `replicas = 2` before
//! the conversion and `4+2` after).
//!
//! Catches: a build node rendering the storage tier's families or the reverse, a combined node
//! missing either side, a drive family seeded with the cluster's drives on a storage node or its
//! own drives on a build node, the cache disk missing from a build node or present on a storage
//! node, copies or margins not following `hot.replicas` and the codes.

use std::collections::BTreeSet;

use tierstore_format::descriptor::Code;
use tierstore_format::ids::{DriveId, NodeId};
use tierstore_metrics::{Metrics, Topology};
use tierstore_store_api::view::Role;

fn drives(node: u8, count: u8) -> Vec<DriveId> {
    (0..count)
        .map(|i| {
            let mut id = [0; 16];
            id[0] = node;
            id[1] = i;
            DriveId(id)
        })
        .collect()
}

fn cluster(nodes: u8, per_node: u8) -> Vec<DriveId> {
    (1..=nodes).flat_map(|n| drives(n, per_node)).collect()
}

fn later_storage() -> Topology {
    Topology {
        role: Role::Storage,
        version: "v",
        cluster_drives: cluster(3, 16),
        local_drives: drives(1, 16),
        peers: vec![NodeId(2), NodeId(3)],
        hot_replicas: 3,
        codes: vec![Code::Ec { k: 4, m: 4 }],
    }
}

fn later_build() -> Topology {
    Topology {
        role: Role::Build,
        version: "v",
        cluster_drives: cluster(3, 16),
        local_drives: Vec::new(),
        peers: vec![NodeId(1), NodeId(2), NodeId(3)],
        hot_replicas: 3,
        codes: vec![Code::Ec { k: 4, m: 4 }],
    }
}

fn now_combined() -> Topology {
    Topology {
        role: Role::Combined,
        version: "v",
        cluster_drives: cluster(6, 3),
        local_drives: drives(1, 3),
        peers: (2..=6).map(NodeId).collect(),
        hot_replicas: 2,
        codes: vec![Code::Replicas(2), Code::Ec { k: 4, m: 2 }],
    }
}

/// The sample lines (not `#` lines) of the family exported as `name`, with labels.
fn series(text: &str, name: &str) -> Vec<String> {
    let start = format!("{name}{{");
    text.lines()
        .filter(|l| l.starts_with(&start))
        .map(str::to_owned)
        .collect()
}

fn rendered(m: &Metrics) -> BTreeSet<&'static str> {
    m.rendered_families().iter().map(|f| f.name).collect()
}

/// Asserts that a storage node renders every family but the build side's, a build node every
/// family but the storage side's, and a combined node all of them.
#[test]
fn each_role_renders_its_sides_and_every_role_the_node_families() {
    let storage = Metrics::new(&later_storage());
    let build = Metrics::new(&later_build());
    let combined = Metrics::new(&now_combined());
    let all: BTreeSet<&str> = combined.families().iter().map(|f| f.name).collect();
    assert_eq!(rendered(&combined), all);
    let (s, b) = (rendered(&storage), rendered(&build));
    let both: BTreeSet<&str> = s.intersection(&b).copied().collect();
    assert_eq!(
        both,
        [
            "tierstore_build_info",
            "tierstore_fdatasync_seconds",
            "tierstore_blocking_pool_queue_len",
            "tierstore_disk_r_await_seconds",
            "tierstore_disk_aqu_sz",
            "tierstore_disk_util",
            "tierstore_store_copy_generation_mismatch_total",
        ]
        .into_iter()
        .collect()
    );
    assert_eq!(s.union(&b).count(), all.len());
    assert!(s.contains("tierstore_hot_dirty_bytes") && s.contains("tierstore_drive_state"));
    assert!(
        !s.contains("tierstore_fuse_requests_total") && !s.contains("tierstore_part_net_seconds")
    );
    assert!(b.contains("tierstore_fuse_requests_total") && b.contains("tierstore_verify_seconds"));
    assert!(!b.contains("tierstore_cold_stripes") && !b.contains("tierstore_store_puts_total"));
    for (m, role) in [
        (&storage, Role::Storage),
        (&build, Role::Build),
        (&combined, Role::Combined),
    ] {
        assert_eq!(m.role(), role);
        let text = m.render();
        let types: BTreeSet<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .filter_map(|l| l.split(' ').next())
            .collect();
        assert_eq!(types, rendered(m));
    }
}

/// Asserts that the build side's `{drive}` is every drive of the cluster and the storage side's
/// the node's own, on each profile.
#[test]
fn drive_families_are_seeded_with_the_drives_of_their_side() {
    let storage = Metrics::new(&later_storage()).render();
    let build = Metrics::new(&later_build()).render();
    let combined = Metrics::new(&now_combined()).render();
    assert_eq!(series(&build, "tierstore_parts_per_drive").len(), 48);
    assert_eq!(series(&combined, "tierstore_parts_per_drive").len(), 18);
    assert!(series(&storage, "tierstore_parts_per_drive").is_empty());
    assert_eq!(
        series(&storage, "tierstore_drive_queue_depth").len(),
        16 * 4
    );
    assert_eq!(
        series(&combined, "tierstore_drive_queue_depth").len(),
        3 * 4
    );
    assert!(series(&build, "tierstore_drive_queue_depth").is_empty());
    assert_eq!(series(&storage, "tierstore_drive_503_total").len(), 16 * 9);
    assert_eq!(series(&combined, "tierstore_cold_fill_ratio").len(), 3);
    for (topology, text) in [(later_storage(), &storage), (now_combined(), &combined)] {
        let ids: BTreeSet<String> = topology
            .local_drives
            .iter()
            .map(ToString::to_string)
            .collect();
        for line in series(text, "tierstore_drive_state") {
            let id = line
                .split("drive=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            assert!(ids.contains(id), "{line}");
        }
    }
}

/// Asserts that the per-disk gauges cover the cache device on a node with a build side and the
/// node's own drives on a node with a storage side.
#[test]
fn disk_gauges_cover_the_cache_device_and_the_nodes_own_drives() {
    let disks = |t: &Topology| -> Vec<String> {
        series(&Metrics::new(t).render(), "tierstore_disk_util")
            .iter()
            .map(|l| l.split('"').nth(1).unwrap().to_owned())
            .collect()
    };
    assert_eq!(disks(&later_build()), ["cache"]);
    let local: Vec<String> = drives(1, 16).iter().map(ToString::to_string).collect();
    assert_eq!(disks(&later_storage()), local);
    let mut combined = vec!["cache".to_owned()];
    combined.extend(drives(1, 3).iter().map(ToString::to_string));
    assert_eq!(disks(&now_combined()), combined);
}

/// Asserts that copies run from 0 to `hot.replicas`, the codes and margins follow the configured
/// codes (the widest code's parity, plus `unreadable`), and the peers label the replica families.
#[test]
fn copies_codes_margins_and_peers_follow_the_configuration() {
    let storage = Metrics::new(&later_storage()).render();
    let combined = Metrics::new(&now_combined()).render();
    let values = |text: &str, name: &str, label: &str| -> Vec<String> {
        let key = format!("{label}=\"");
        series(text, name)
            .iter()
            .map(|l| {
                l.split(&key)
                    .nth(1)
                    .unwrap()
                    .split('"')
                    .next()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    };
    assert_eq!(
        values(&storage, "tierstore_hot_blocks_by_copies", "copies"),
        ["0", "1", "2", "3"]
    );
    assert_eq!(
        values(&combined, "tierstore_hot_blocks_by_copies", "copies"),
        ["0", "1", "2"]
    );
    assert_eq!(
        values(&storage, "tierstore_cold_stripes", "code"),
        ["4+4", "4+4"]
    );
    assert_eq!(
        values(&combined, "tierstore_cold_stripes", "code"),
        ["4+2", "4+2", "replicas-2", "replicas-2"]
    );
    assert_eq!(
        values(&storage, "tierstore_cold_stripe_margin", "margin"),
        ["unreadable", "0", "1", "2", "3", "4"]
    );
    assert_eq!(
        values(&combined, "tierstore_cold_rebuild_queue_depth", "margin"),
        ["unreadable", "0", "1", "2"]
    );
    let peers = values(&storage, "tierstore_hot_replica_lag_bytes", "node");
    assert_eq!(peers, ["2", "3"]);
    assert_eq!(
        values(&combined, "tierstore_hot_replica_lag_bytes", "node").len(),
        5
    );
}

/// Asserts that a margin with no configured code is only `unreadable` and `0`, and that a family
/// of a side the role does not render still counts but is not written.
#[test]
fn an_unconfigured_cold_tier_and_an_unrendered_side_still_count() {
    let mut topology = later_storage();
    topology.codes.clear();
    let m = Metrics::new(&topology);
    let text = m.render();
    assert_eq!(series(&text, "tierstore_cold_stripe_margin").len(), 2);
    assert!(series(&text, "tierstore_cold_stripes").is_empty());
    m.fetch.retries_total.inc(());
    assert_eq!(m.fetch.retries_total.with(()).get(), 1);
    assert!(!m.render().contains("tierstore_retries_total"));
}
