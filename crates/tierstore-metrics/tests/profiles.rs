//! A process registers the families of the components it enables, each with its own
//! configuration; the deployment profiles of section 21 are only example sets of components,
//! whose output these tests check.
//!
//! Examples: a LATER storage node (hot store on an NVMe device, cold store on 15 hard drives,
//! peer service, S3 front; two peers, `hot.replicas = 3`, `4+4`); a LATER build node (node
//! cache, fetch engine over the cluster's 48 drives, FUSE front, Prepare provider); a NOW
//! combined node (every component but the NFS front: two SMR drives and a hot SSD partition,
//! five peers, `hot.replicas = 2`, `replicas = 2` before the conversion and `4+2` after); a macOS
//! build node (node cache, fetch engine, NFS front, Prepare provider).
//!
//! Catches: a family registered by a component that was not enabled, or missing when its
//! component is; a family shared by two components registered twice; a drive family seeded with
//! the cluster's drives by the stores or with a store's drives by the fetch engine; the cache disk
//! missing with a node cache or present without one; copies or margins not following
//! `hot.replicas` and the codes; a second registration that drops or duplicates series.

use std::collections::BTreeSet;

use tierstore_format::descriptor::Code;
use tierstore_format::ids::{DriveId, NodeId};
use tierstore_metrics::{ColdStoreConfig, FetchEngineConfig, HotStoreConfig, Metrics};

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

/// A storage node's components: hot store on `device`, cold store on `hdds`.
fn storage(
    m: &Metrics,
    device: DriveId,
    hdds: Vec<DriveId>,
    hot: (u8, Vec<NodeId>),
    codes: Vec<Code>,
) {
    let _ = m.hot_store(&HotStoreConfig {
        device,
        replicas: hot.0,
        peers: hot.1,
    });
    let _ = m.cold_store(&ColdStoreConfig {
        drives: hdds,
        codes,
    });
    let _ = m.peer_service();
    let _ = m.s3_front();
}

/// A build node's read path, with the FUSE front.
fn read_path(m: &Metrics, cluster: Vec<DriveId>) {
    let _ = m.node_cache();
    let _ = m.fetch_engine(&FetchEngineConfig { drives: cluster });
    let _ = m.fuse_front();
    let _ = m.prepare_provider();
}

fn later_storage() -> Metrics {
    let m = Metrics::new("v");
    let local = drives(1, 16);
    let hot = (3, vec![NodeId(2), NodeId(3)]);
    storage(
        &m,
        local[15],
        local[..15].to_vec(),
        hot,
        vec![Code::Ec { k: 4, m: 4 }],
    );
    m
}

fn later_build() -> Metrics {
    let m = Metrics::new("v");
    read_path(&m, cluster(3, 16));
    m
}

fn now_combined() -> Metrics {
    let m = Metrics::new("v");
    let local = drives(1, 3);
    let hot = (2, (2..=6).map(NodeId).collect());
    let codes = vec![Code::Replicas(2), Code::Ec { k: 4, m: 2 }];
    storage(&m, local[2], local[..2].to_vec(), hot, codes);
    read_path(&m, cluster(6, 3));
    m
}

fn macos_build() -> Metrics {
    let m = Metrics::new("v");
    let _ = m.node_cache();
    let _ = m.fetch_engine(&FetchEngineConfig {
        drives: cluster(3, 16),
    });
    let _ = m.nfs_front();
    let _ = m.prepare_provider();
    m
}

/// The sample lines (not `#` lines) of the family exported as `name`, with labels.
fn series(text: &str, name: &str) -> Vec<String> {
    let start = format!("{name}{{");
    text.lines()
        .filter(|l| l.starts_with(&start))
        .map(str::to_owned)
        .collect()
}

fn names(m: &Metrics) -> BTreeSet<&'static str> {
    m.families().iter().map(|f| f.name).collect()
}

/// Asserts that a process with no component has only the process families; that the storage and
/// build examples share only the process families and the shared groups both use; that the
/// combined example has the families of both; that each family is registered once; and that the
/// text's `# TYPE` lines are exactly the registered families.
#[test]
fn each_process_has_the_families_of_the_components_it_enabled() {
    let bare = Metrics::new("v");
    assert_eq!(
        names(&bare),
        ["tierstore_build_info", "tierstore_blocking_pool_queue_len"]
            .into_iter()
            .collect()
    );
    let (storage, build, combined) = (later_storage(), later_build(), now_combined());
    let (s, b) = (names(&storage), names(&build));
    let both: BTreeSet<&str> = s.intersection(&b).copied().collect();
    assert_eq!(
        both,
        [
            "tierstore_build_info",
            "tierstore_blocking_pool_queue_len",
            "tierstore_fdatasync_seconds",
            "tierstore_disk_r_await_seconds",
            "tierstore_disk_aqu_sz",
            "tierstore_disk_util",
            "tierstore_store_copy_generation_mismatch_total",
        ]
        .into_iter()
        .collect()
    );
    let union: BTreeSet<&str> = s.union(&b).copied().collect();
    assert_eq!(names(&combined), union);
    assert!(s.contains("tierstore_hot_dirty_bytes") && s.contains("tierstore_drive_state"));
    assert!(
        !s.contains("tierstore_fuse_requests_total") && !s.contains("tierstore_part_net_seconds")
    );
    assert!(b.contains("tierstore_fuse_requests_total") && b.contains("tierstore_verify_seconds"));
    assert!(!b.contains("tierstore_cold_stripes") && !b.contains("tierstore_store_puts_total"));
    for m in [&bare, &storage, &build, &combined] {
        let families = m.families();
        assert_eq!(names(m).len(), families.len());
        let text = m.render();
        let types: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .filter_map(|l| l.split(' ').next())
            .collect();
        let expected: Vec<&str> = families.iter().map(|f| f.name).collect();
        assert_eq!(types, expected);
    }
}

/// Asserts that the NFS front registers the read-reply families and none of the FUSE front's.
#[test]
fn the_nfs_front_has_the_read_replies_and_no_fuse_family() {
    let mac = names(&macos_build());
    assert!(
        mac.contains("tierstore_pread_bytes_total") && mac.contains("tierstore_splice_bytes_total")
    );
    assert!(
        !mac.contains("tierstore_fuse_requests_total") && !mac.contains("tierstore_fuse_parked")
    );
    assert!(
        mac.contains("tierstore_part_net_seconds") && mac.contains("tierstore_tree_ready_seconds")
    );
}

/// Asserts that the fetch engine's `{drive}` is every drive of the cluster it was given, and the
/// per-drive families hold the hot store's device and the cold store's drives.
#[test]
fn drive_families_hold_the_drives_each_component_was_given() {
    let storage = later_storage().render();
    let build = later_build().render();
    let combined = now_combined().render();
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
    assert_eq!(series(&combined, "tierstore_cold_fill_ratio").len(), 2);
    let local: BTreeSet<String> = drives(1, 16).iter().map(ToString::to_string).collect();
    for line in series(&storage, "tierstore_drive_state") {
        let id = line
            .split("drive=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert!(local.contains(id), "{line}");
    }
}

/// Asserts that the per-disk gauges hold the cache device when a node cache is registered and
/// the stores' drives when they are, in label order (the cache first).
#[test]
fn disk_gauges_hold_the_cache_device_and_the_stores_drives() {
    let disks = |m: Metrics| -> Vec<String> {
        series(&m.render(), "tierstore_disk_util")
            .iter()
            .map(|l| l.split('"').nth(1).unwrap().to_owned())
            .collect()
    };
    assert_eq!(disks(later_build()), ["cache"]);
    let local: Vec<String> = drives(1, 16).iter().map(ToString::to_string).collect();
    assert_eq!(disks(later_storage()), local);
    let mut combined = vec!["cache".to_owned()];
    combined.extend(drives(1, 3).iter().map(ToString::to_string));
    assert_eq!(disks(now_combined()), combined);
}

/// Asserts that copies run from 0 to `hot.replicas`, the codes and margins follow the cold
/// store's codes (the widest code's parity, plus `unreadable`), and the peers label the replica
/// families.
#[test]
fn copies_codes_margins_and_peers_follow_each_components_configuration() {
    let storage = later_storage().render();
    let combined = now_combined().render();
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
    assert_eq!(
        values(&storage, "tierstore_hot_replica_lag_bytes", "node"),
        ["2", "3"]
    );
    assert_eq!(
        values(&combined, "tierstore_hot_replica_lag_bytes", "node").len(),
        5
    );
}

/// Asserts that a cold store with no code has only the `unreadable` and `0` margins; that
/// registering it again with more drives and a code adds their series to the same families,
/// keeps what was counted, and registers no family twice; and that the handles of both
/// registrations are one group.
#[test]
fn a_second_registration_adds_its_series_to_the_same_families() {
    let m = Metrics::new("v");
    let first = m.cold_store(&ColdStoreConfig {
        drives: drives(1, 2),
        codes: Vec::new(),
    });
    let text = m.render();
    assert_eq!(series(&text, "tierstore_cold_stripe_margin").len(), 2);
    assert!(series(&text, "tierstore_cold_stripes").is_empty());
    first.cold.cold_scrub_bad_total.inc(());
    let count = m.families().len();
    let second = m.cold_store(&ColdStoreConfig {
        drives: drives(1, 3),
        codes: vec![Code::Ec { k: 4, m: 2 }],
    });
    assert_eq!(m.families().len(), count);
    assert_eq!(second.cold.cold_scrub_bad_total.with(()).get(), 1);
    let text = m.render();
    assert_eq!(series(&text, "tierstore_drive_state").len(), 3 * 8);
    assert_eq!(series(&text, "tierstore_cold_stripe_margin").len(), 4);
    assert_eq!(series(&text, "tierstore_cold_stripes").len(), 2);
    assert!(std::sync::Arc::ptr_eq(&first.drives, &second.drives));
    assert!(std::sync::Arc::ptr_eq(&first.cold, &second.cold));
    assert!(std::sync::Arc::ptr_eq(&m.process(), &m.process()));
}
