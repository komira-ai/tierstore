//! Choose (10.3 step 4): the proposal a sealer makes from the promises of a majority.
//!
//! If any promise carries an accepted proposal, the one with the highest ballot is re-proposed (a
//! committed seal is never changed). Otherwise, among the `q` copies that answered with their
//! whole committed length crc-verified (a copy with damage inside its acknowledged bytes counts
//! as not answering), the proposal is the `r`-th longest, `r = max(1, q - replicas + min_acks)`,
//! with that copy's running crc. At the default `min_acks = replicas` that is the shortest copy:
//! every copy holds every acknowledged record, so none is cut, and every answering copy already
//! holds the proposed bytes. The proposal is never below `floor`, the sealer's own last
//! acknowledged offset; when the rule would put it there, or no copy answered, there is no
//! proposal and the segment waits.

use tierstore_format::seal::SealProposal;
use tierstore_store_api::hot::Promise;

/// The proposal for `promises`, or `None` when the sealer must not propose.
#[must_use]
pub fn choose(promises: &[Promise], replicas: u8, min_acks: u8, floor: u64) -> Option<SealProposal> {
    let accepted = promises
        .iter()
        .filter_map(|p| p.accepted)
        .max_by_key(|(ballot, _)| *ballot);
    if let Some((_, proposal)) = accepted {
        return Some(proposal);
    }
    let mut copies: Vec<SealProposal> = promises
        .iter()
        .filter_map(|p| p.copy)
        .filter(|c| c.verified == c.committed)
        .map(|c| SealProposal {
            length: c.committed,
            running_crc: c.running_crc,
        })
        .collect();
    copies.sort_unstable_by_key(|c| std::cmp::Reverse(c.length));
    let r = (copies.len() + usize::from(min_acks))
        .saturating_sub(usize::from(replicas))
        .max(1);
    copies
        .get(r - 1)
        .copied()
        .filter(|p| p.length >= floor)
}

#[cfg(test)]
mod tests {
    use tierstore_format::ids::NodeId;
    use tierstore_format::seal::Ballot;
    use tierstore_store_api::hot::CopyState;

    use super::*;

    fn copy(committed: u64, verified: u64, crc: u32) -> Promise {
        Promise {
            ballot: Ballot::opening(NodeId(1)),
            accepted: None,
            copy: Some(CopyState {
                committed,
                verified,
                running_crc: crc,
            }),
        }
    }

    fn voter() -> Promise {
        Promise {
            ballot: Ballot::opening(NodeId(1)),
            accepted: None,
            copy: None,
        }
    }

    fn prop(length: u64, running_crc: u32) -> SealProposal {
        SealProposal {
            length,
            running_crc,
        }
    }

    #[test]
    fn the_highest_accepted_proposal_wins_over_every_copy() {
        let mut a = copy(90, 90, 9);
        a.accepted = Some((Ballot { round: 2, node: NodeId(2) }, prop(40, 4)));
        let mut b = voter();
        b.accepted = Some((Ballot { round: 3, node: NodeId(1) }, prop(70, 7)));
        let c = copy(100, 100, 10);
        assert_eq!(choose(&[a, b, c], 3, 3, 0), Some(prop(70, 7)));
    }

    #[test]
    fn at_the_default_ack_rule_the_shortest_copy_is_chosen() {
        let ps = [copy(100, 100, 1), copy(60, 60, 2), copy(80, 80, 3)];
        assert_eq!(choose(&ps, 3, 3, 0), Some(prop(60, 2)));
    }

    #[test]
    fn below_the_default_the_rth_longest_is_chosen() {
        // replicas 3, min_acks 2, q 3: r = 2.
        let ps = [copy(100, 100, 1), copy(60, 60, 2), copy(80, 80, 3)];
        assert_eq!(choose(&ps, 3, 2, 0), Some(prop(80, 3)));
        // q 2: r = max(1, 1) = 1, the longer of the two.
        assert_eq!(choose(&ps[..2], 3, 2, 0), Some(prop(100, 1)));
    }

    #[test]
    fn a_damaged_copy_counts_as_not_answering() {
        let ps = [copy(100, 30, 1), copy(60, 60, 2), voter()];
        assert_eq!(choose(&ps, 2, 2, 0), Some(prop(60, 2)));
    }

    #[test]
    fn no_copy_or_a_choice_below_the_floor_proposes_nothing() {
        assert_eq!(choose(&[voter(), voter()], 3, 2, 0), None);
        let ps = [copy(60, 60, 2), copy(80, 80, 3)];
        assert_eq!(choose(&ps, 2, 2, 61), None);
        assert_eq!(choose(&ps, 2, 2, 60), Some(prop(60, 2)));
    }
}
