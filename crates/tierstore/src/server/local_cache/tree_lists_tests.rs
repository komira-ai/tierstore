//! What the mounts report of their tree lists, as the cache counts and renders it: the report read
//! back as written and every other form refused; the gauges taking the last report and the
//! counters adding what each process's totals grew by; the families in the order of the mounts;
//! the bound on the mounts.

use super::{MOST_MOUNTS, Totals, TreeLists};
use crate::server::local_cache::testing::Rig;

fn report(mount: &str, process: u64, gauges: [u64; 3], totals: [u64; 3]) -> TreeLists {
    let [mapped, bytes, cap] = gauges;
    let [released, idle, over_cap] = totals;
    TreeLists {
        mount: mount.to_owned(),
        process,
        mapped,
        bytes,
        cap,
        totals: Totals {
            cap: released,
            idle,
            over_cap,
        },
    }
}

/// The lines of the five families in the cache's `/metricz` text.
fn families(rig: &Rig) -> Vec<String> {
    rig.cache
        .metrics()
        .lines()
        .filter(|line| line.contains("tierstore_fs_tree_list"))
        .map(str::to_owned)
        .collect()
}

/// Asserts that a report reads back as written, the largest numbers too, and that a missing or
/// extra word, an empty word, a number with a sign or a non-digit, a number over 64 bits and a bad
/// mount name are refused.
#[test]
fn a_treelists_report_reads_back_as_written_and_any_other_form_is_refused() {
    let one = report("boot-1", 4242, [3, 12288, 536_870_912], [1, 2, 0]);
    assert_eq!(one.line(), "treelists boot-1 4242 3 12288 536870912 1 2 0");
    assert_eq!(
        TreeLists::parse("boot-1 4242 3 12288 536870912 1 2 0"),
        Some(one)
    );
    let most = report("m", u64::MAX, [u64::MAX; 3], [u64::MAX; 3]);
    let rest = most.line().replacen("treelists ", "", 1);
    assert_eq!(TreeLists::parse(&rest), Some(most));
    let big = format!("m 7 1 1 1 1 1 {}0", u64::MAX);
    let too_long = format!("{} 7 1 1 1 1 1 1", "m".repeat(129));
    for refused in [
        "",
        "m",
        "m 7 1 1 1 1 1",
        "m 7 1 1 1 1 1 1 1",
        "m  7 1 1 1 1 1 1",
        " 7 1 1 1 1 1 1",
        "m 7 1 1 1 1 1 ",
        "m 7 1 1 1 1 1 -1",
        "m 7 1 1 +1 1 1 1",
        "m 7 1 x 1 1 1 1",
        "m 7 1 1 1 1.0 1 1",
        big.as_str(),
        too_long.as_str(),
    ] {
        assert_eq!(TreeLists::parse(refused), None, "{refused:?}");
    }
}

/// Asserts the five families exactly: each with its `# TYPE` line before any mount reported, then
/// the gauges of each mount's last report and its counters, the mounts in the order of their ids,
/// a label value escaped.
#[test]
fn the_families_render_each_mount_in_the_order_of_their_ids() {
    let rig = Rig::new("tree-lists-render");
    assert_eq!(
        families(&rig),
        [
            "# TYPE tierstore_fs_tree_lists_mapped gauge",
            "# TYPE tierstore_fs_tree_list_mapped_bytes gauge",
            "# TYPE tierstore_fs_tree_list_ram_bytes gauge",
            "# TYPE tierstore_fs_tree_list_releases_total counter",
            "# TYPE tierstore_fs_tree_list_over_cap_total counter",
        ]
    );
    rig.cache
        .tree_lists_reported(report("z\"", 1, [2, 8192, 4096], [3, 4, 5]));
    rig.cache
        .tree_lists_reported(report("a", 2, [1, 4096, 8192], [0, 1, 0]));
    assert_eq!(
        families(&rig),
        [
            "# TYPE tierstore_fs_tree_lists_mapped gauge",
            "tierstore_fs_tree_lists_mapped{mount=\"a\"} 1",
            "tierstore_fs_tree_lists_mapped{mount=\"z\\\"\"} 2",
            "# TYPE tierstore_fs_tree_list_mapped_bytes gauge",
            "tierstore_fs_tree_list_mapped_bytes{mount=\"a\"} 4096",
            "tierstore_fs_tree_list_mapped_bytes{mount=\"z\\\"\"} 8192",
            "# TYPE tierstore_fs_tree_list_ram_bytes gauge",
            "tierstore_fs_tree_list_ram_bytes{mount=\"a\"} 8192",
            "tierstore_fs_tree_list_ram_bytes{mount=\"z\\\"\"} 4096",
            "# TYPE tierstore_fs_tree_list_releases_total counter",
            "tierstore_fs_tree_list_releases_total{mount=\"a\",why=\"cap\"} 0",
            "tierstore_fs_tree_list_releases_total{mount=\"z\\\"\",why=\"cap\"} 3",
            "tierstore_fs_tree_list_releases_total{mount=\"a\",why=\"idle\"} 1",
            "tierstore_fs_tree_list_releases_total{mount=\"z\\\"\",why=\"idle\"} 4",
            "# TYPE tierstore_fs_tree_list_over_cap_total counter",
            "tierstore_fs_tree_list_over_cap_total{mount=\"a\"} 0",
            "tierstore_fs_tree_list_over_cap_total{mount=\"z\\\"\"} 5",
        ]
    );
}

/// Asserts that each counter adds what the total of a process grew by since its report before;
/// the whole total of a process not seen before (a restart, the next process of a hand-over), and
/// of one whose total went down (a new process given the same id); that the two latest processes
/// are counted apart as they report in turns; and that the gauges are the last report's.
#[test]
fn each_counter_adds_what_each_processs_total_grew_by() {
    let rig = Rig::new("tree-lists-counters");
    let counters = |rig: &Rig| -> Vec<String> {
        families(rig)
            .into_iter()
            .filter(|line| line.contains("_total{"))
            .map(|line| line.rsplit(' ').next().unwrap().to_owned())
            .collect()
    };
    let steps: [(u64, [u64; 3], [&str; 3]); 6] = [
        (1, [2, 1, 0], ["2", "1", "0"]),
        (1, [5, 1, 1], ["5", "1", "1"]),
        (2, [1, 0, 0], ["6", "1", "1"]),
        (1, [6, 2, 1], ["7", "2", "1"]),
        (2, [3, 0, 2], ["9", "2", "3"]),
        (2, [1, 0, 0], ["10", "2", "3"]),
    ];
    for (process, totals, want) in steps {
        rig.cache
            .tree_lists_reported(report("m", process, [process, 0, 0], totals));
        assert_eq!(counters(&rig), want, "{process} {totals:?}");
    }
    // A third process drops the first's last totals: its report again counts whole.
    rig.cache
        .tree_lists_reported(report("m", 3, [9, 0, 0], [0, 0, 0]));
    rig.cache
        .tree_lists_reported(report("m", 1, [1, 0, 0], [1, 0, 0]));
    assert_eq!(counters(&rig), ["11", "2", "3"]);
    assert!(families(&rig).contains(&"tierstore_fs_tree_lists_mapped{mount=\"m\"} 1".to_owned()));
}

/// Asserts that a report of another mount, once the node keeps the most mounts, is counted
/// nowhere, and that the mounts kept go on counting.
#[test]
fn a_mount_beyond_the_bound_is_dropped() {
    let rig = Rig::new("tree-lists-bound");
    for at in 0..MOST_MOUNTS {
        rig.cache
            .tree_lists_reported(report(&format!("m{at:03}"), 1, [1, 0, 0], [0, 0, 0]));
    }
    rig.cache
        .tree_lists_reported(report("new", 1, [1, 0, 0], [0, 0, 0]));
    rig.cache
        .tree_lists_reported(report("m000", 1, [2, 0, 0], [0, 0, 0]));
    let mapped: Vec<String> = families(&rig)
        .into_iter()
        .filter(|line| line.starts_with("tierstore_fs_tree_lists_mapped{"))
        .collect();
    assert_eq!(mapped.len(), MOST_MOUNTS);
    assert!(!mapped.iter().any(|line| line.contains("\"new\"")));
    assert_eq!(
        mapped[0],
        "tierstore_fs_tree_lists_mapped{mount=\"m000\"} 2"
    );
}
