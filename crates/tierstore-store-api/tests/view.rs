//! Proves the membership view and its quorum rules: which nodes count as live, which drives belong
//! to a node, the arithmetic of a definitive miss (section 14 "Definitive miss") and of a majority
//! commit, and the lowercase JSON names of the admin, role and node states.
//!
//! Catches:
//! - a build node, a `down`, `indexing` or `suspect` node, or a node whose hot device is `out`
//!   counted as live;
//! - a drive listed under the wrong node;
//! - an off-by-one in `N - min_acks + 1`, or a definitive answer count for settings that cannot be
//!   met (`min_acks` 0 or above `N`);
//! - a majority of two nodes that is one, or of six that is three;
//! - a state serialised in a case other than lowercase.

use tierstore_fetch_api::upstream::DriveState;
use tierstore_format::descriptor::Code;
use tierstore_format::ids::{DriveId, NodeId};
use tierstore_store_api::view::{
    Admin, ClusterSettings, DriveView, NodeState, NodeView, Role, View, definitive_miss_answers,
    majority,
};

fn node(id: u8, role: Role, hot: Admin, state: NodeState) -> NodeView {
    NodeView {
        id: NodeId(id),
        domain: u16::from(id),
        role,
        hot,
        state,
    }
}

fn drive(id: u8, node: u8) -> DriveView {
    DriveView {
        id: DriveId([id; 16]),
        node: NodeId(node),
        admin: Admin::In,
        state: DriveState::Up,
        capacity: 1000,
        free: 500,
        above_fill_limit: false,
    }
}

fn view() -> View {
    View {
        epoch: 7,
        settings: ClusterSettings {
            hot_replicas: 3,
            hot_min_acks: 2,
            cold_code: Some(Code::Ec { k: 4, m: 2 }),
        },
        nodes: vec![
            node(1, Role::Storage, Admin::In, NodeState::Up),
            node(2, Role::Combined, Admin::Drain, NodeState::Up),
            node(3, Role::Build, Admin::In, NodeState::Up),
            node(4, Role::Storage, Admin::In, NodeState::Down),
            node(5, Role::Storage, Admin::In, NodeState::Indexing),
            node(6, Role::Storage, Admin::In, NodeState::Suspect),
            node(7, Role::Storage, Admin::Out, NodeState::Up),
        ],
        drives: vec![drive(10, 1), drive(11, 1), drive(12, 2)],
    }
}

#[test]
fn only_storage_and_combined_roles_store_data() {
    assert!(Role::Storage.stores());
    assert!(Role::Combined.stores());
    assert!(!Role::Build.stores());
}

#[test]
fn a_node_is_found_by_id_and_an_unknown_id_is_none() {
    let v = view();
    assert_eq!(v.node(NodeId(2)).map(|n| n.role), Some(Role::Combined));
    assert_eq!(v.node(NodeId(99)), None);
}

#[test]
fn live_nodes_are_up_storing_nodes_whose_hot_device_is_not_out() {
    let v = view();
    let live: Vec<u8> = v.live_nodes().map(|n| n.id.0).collect();
    assert_eq!(live, vec![1, 2]);
}

#[test]
fn drives_of_a_node_are_only_its_own() {
    let v = view();
    let ids = |n: u8| -> Vec<u8> { v.drives_of(NodeId(n)).map(|d| d.id.0[0]).collect() };
    assert_eq!(ids(1), vec![10, 11]);
    assert_eq!(ids(2), vec![12]);
    assert!(ids(3).is_empty());
}

#[test]
fn a_miss_is_definitive_after_n_minus_min_acks_plus_one_absent_answers() {
    assert_eq!(definitive_miss_answers(3, 2), Some(2));
    assert_eq!(definitive_miss_answers(3, 3), Some(1));
    assert_eq!(definitive_miss_answers(3, 1), Some(3));
    assert_eq!(definitive_miss_answers(6, 2), Some(5));
    assert_eq!(definitive_miss_answers(1, 1), Some(1));
}

#[test]
fn a_miss_is_never_definitive_for_settings_that_cannot_be_met() {
    assert_eq!(definitive_miss_answers(3, 0), None);
    assert_eq!(definitive_miss_answers(3, 4), None);
    assert_eq!(definitive_miss_answers(0, 1), None);
    assert_eq!(definitive_miss_answers(0, 0), None);
}

#[test]
fn a_majority_is_half_the_nodes_rounded_down_plus_one() {
    assert_eq!(majority(1), 1);
    assert_eq!(majority(2), 2);
    assert_eq!(majority(3), 2);
    assert_eq!(majority(4), 3);
    assert_eq!(majority(6), 4);
}

#[test]
fn admin_role_and_node_state_serialise_in_lowercase() {
    assert_eq!(serde_json::to_string(&Admin::Drain).unwrap(), "\"drain\"");
    assert_eq!(serde_json::to_string(&Admin::In).unwrap(), "\"in\"");
    assert_eq!(serde_json::to_string(&Admin::Out).unwrap(), "\"out\"");
    assert_eq!(
        serde_json::to_string(&Role::Combined).unwrap(),
        "\"combined\""
    );
    assert_eq!(
        serde_json::to_string(&Role::Storage).unwrap(),
        "\"storage\""
    );
    assert_eq!(serde_json::to_string(&Role::Build).unwrap(), "\"build\"");
    assert_eq!(serde_json::to_string(&NodeState::Up).unwrap(), "\"up\"");
    assert_eq!(
        serde_json::to_string(&NodeState::Indexing).unwrap(),
        "\"indexing\""
    );
    assert_eq!(
        serde_json::to_string(&NodeState::Suspect).unwrap(),
        "\"suspect\""
    );
    assert_eq!(serde_json::to_string(&NodeState::Down).unwrap(), "\"down\"");
}

#[test]
fn lowercase_names_deserialise_and_capitalised_ones_are_refused() {
    assert_eq!(
        serde_json::from_str::<Admin>("\"drain\"").unwrap(),
        Admin::Drain
    );
    assert_eq!(
        serde_json::from_str::<Role>("\"build\"").unwrap(),
        Role::Build
    );
    assert_eq!(
        serde_json::from_str::<NodeState>("\"down\"").unwrap(),
        NodeState::Down
    );
    assert!(serde_json::from_str::<NodeState>("\"Down\"").is_err());
}
