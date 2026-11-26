//! Shared packs first (section 6.1): the needed ranges of packs whose record has `pack_flags`
//! bit 1 (`shared`) set rank before every other needed range, and each group keeps tree order.

#![allow(
    clippy::unwrap_used,
    reason = "an integration test: a failed unwrap is the failure it reports"
)]

mod support;

use support::{Case, cells, list_mib, shape};

#[test]
fn shared_packs_get_lower_ranks_than_tree_order_packs() {
    let mut list = list_mib(&[100, 100]);
    list.packs[1].pack_flags = 2;
    let plan = Case::new(&list, vec![cells(0, 10, 1), cells(1, 20, 1)]).plan();
    let rank_of = |pack: u32| {
        plan.ensures
            .iter()
            .find(|e| e.pack == pack)
            .map(|e| e.rank)
            .unwrap()
    };
    assert!(rank_of(1) < rank_of(0), "{:?}", shape(&plan));
}
