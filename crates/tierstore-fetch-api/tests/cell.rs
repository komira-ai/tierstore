//! The cell byte and its transitions (`cell`, `entry::{EvictStep, Settle}`).
//!
//! Proves the state bits and the byte encoding for every byte value, that `servable` is true for
//! exactly `present` and `present + durable`, and that each transition succeeds only from its
//! predecessor, over every state, durable bit and transition, with the durable bit set, kept or
//! cleared as the table of section 2 says.
//!
//! Catches:
//! - a state whose bits change, or bits 6 and 7 read as a state;
//! - a byte with any of bits 3 to 6 set that is served as present;
//! - a transition that succeeds from a state other than its predecessor;
//! - `Durable` that fails to set the bit, an eviction abort that drops it, or any other
//!   transition that keeps it;
//! - an evictor step or settle that maps to the wrong transition.

use tierstore_fetch_api::cell::{CellByte, CellState, DURABLE_BIT, STATE_MASK, Transition};
use tierstore_fetch_api::entry::{EvictStep, Settle};

const STATES: [CellState; 6] = [
    CellState::Absent,
    CellState::Claimed,
    CellState::Present,
    CellState::Recheck,
    CellState::Evicting,
    CellState::Bad,
];

const TRANSITIONS: [Transition; 11] = [
    Transition::Claim,
    Transition::Abandon,
    Transition::Publish,
    Transition::MarkBad,
    Transition::Durable,
    Transition::BeginEvict,
    Transition::AbortEvict,
    Transition::FinishEvict,
    Transition::RecheckOk,
    Transition::RecheckFailed,
    Transition::ExpireBad,
];

#[test]
fn the_state_bits_are_the_documented_values_and_round_trip() {
    for (i, s) in STATES.into_iter().enumerate() {
        assert_eq!(usize::from(s.bits()), i);
        assert_eq!(CellState::from_bits(s.bits()), Some(s));
    }
}

#[test]
fn bits_six_and_seven_and_anything_wider_are_not_a_state() {
    for bits in [6, 7, 8, 0x80, 0x82, 255] {
        assert_eq!(CellState::from_bits(bits), None, "{bits}");
    }
}

#[test]
fn a_cell_byte_holds_the_state_in_the_low_bits_and_durable_in_bit_seven() {
    assert_eq!(STATE_MASK, 0b111);
    assert_eq!(DURABLE_BIT, 0x80);
    for s in STATES {
        assert_eq!(CellByte::new(s, false).0, s.bits());
        assert_eq!(CellByte::new(s, true).0, s.bits() | 0x80);
        assert_eq!(CellByte::new(s, false).state(), Some(s));
        assert_eq!(CellByte::new(s, true).state(), Some(s));
        assert!(!CellByte::new(s, false).durable());
        assert!(CellByte::new(s, true).durable());
    }
}

#[test]
fn a_byte_whose_state_bits_are_six_or_seven_has_no_state() {
    for b in [6u8, 7, 0x86, 0x87, 0x0E, 0xFF] {
        assert_eq!(CellByte(b).state(), None, "{b:#04x}");
    }
}

#[test]
fn a_byte_with_any_of_bits_three_to_six_set_has_no_state() {
    for bit in 3..=6u8 {
        for s in STATES {
            for durable in [false, true] {
                let b = CellByte(CellByte::new(s, durable).0 | (1 << bit));
                assert_eq!(b.state(), None, "{b:?}");
                assert!(Transition::Claim.apply(b).is_none());
            }
        }
    }
}

#[test]
fn only_present_with_or_without_durable_is_servable() {
    for b in 0..=255u8 {
        let expected = b == 2 || b == 0x82;
        assert_eq!(CellByte(b).servable(), expected, "{b:#04x}");
    }
}

#[test]
fn a_present_byte_with_a_reserved_bit_set_is_not_servable() {
    for bit in 3..=6 {
        let b = CellByte(CellState::Present.bits() | (1 << bit));
        assert_eq!(b.state(), None);
        assert!(!b.servable(), "bit {bit}");
        assert!(!CellByte(b.0 | DURABLE_BIT).servable(), "bit {bit} durable");
    }
}

#[test]
fn present_is_servable_durable_or_not_and_no_other_state_is() {
    for s in STATES {
        for d in [false, true] {
            assert_eq!(
                CellByte::new(s, d).servable(),
                s == CellState::Present,
                "{s:?} {d}"
            );
        }
    }
}

#[test]
fn each_transition_names_its_predecessor_and_successor_as_the_state_table_says() {
    use CellState::*;
    use Transition::*;
    let table = [
        (Claim, Absent, Claimed),
        (Abandon, Claimed, Absent),
        (Publish, Claimed, Present),
        (MarkBad, Claimed, Bad),
        (Durable, Present, Present),
        (BeginEvict, Present, Evicting),
        (AbortEvict, Evicting, Present),
        (FinishEvict, Evicting, Absent),
        (RecheckOk, Recheck, Present),
        (RecheckFailed, Recheck, Absent),
        (ExpireBad, Bad, Absent),
    ];
    assert_eq!(table.len(), TRANSITIONS.len());
    for (t, from, to) in table {
        assert_eq!(t.from(), from, "{t:?}");
        assert_eq!(t.to(), to, "{t:?}");
    }
}

fn expected_durable(t: Transition, before: bool) -> bool {
    match t {
        Transition::Durable => true,
        Transition::BeginEvict | Transition::AbortEvict => before,
        _ => false,
    }
}

#[test]
fn a_transition_succeeds_only_from_its_predecessor_for_every_state_and_durable_bit() {
    for t in TRANSITIONS {
        for s in STATES {
            for d in [false, true] {
                let current = CellByte::new(s, d);
                let got = t.apply(current);
                if s == t.from() {
                    assert_eq!(
                        got,
                        Some(CellByte::new(t.to(), expected_durable(t, d))),
                        "{t:?} on {s:?} durable={d}"
                    );
                } else {
                    assert_eq!(got, None, "{t:?} on {s:?} durable={d}");
                }
            }
        }
    }
}

#[test]
fn no_transition_applies_to_a_byte_with_no_state() {
    for t in TRANSITIONS {
        for b in [6u8, 7, 0x86, 0x87] {
            assert_eq!(t.apply(CellByte(b)), None, "{t:?} on {b:#04x}");
        }
    }
}

#[test]
fn durable_sets_the_bit_and_is_idempotent() {
    let present = CellByte::new(CellState::Present, false);
    let durable = Transition::Durable.apply(present).expect("present");
    assert_eq!(durable, CellByte::new(CellState::Present, true));
    assert_eq!(Transition::Durable.apply(durable), Some(durable));
}

#[test]
fn an_aborted_eviction_returns_a_durable_cell_as_durable_and_a_clean_one_as_clean() {
    for d in [false, true] {
        let present = CellByte::new(CellState::Present, d);
        let evicting = Transition::BeginEvict.apply(present).expect("present");
        assert_eq!(evicting, CellByte::new(CellState::Evicting, d));
        let back = Transition::AbortEvict.apply(evicting).expect("evicting");
        assert_eq!(back, present);
    }
}

#[test]
fn a_finished_eviction_and_every_other_settle_leave_no_durable_bit() {
    let evicting = CellByte::new(CellState::Evicting, true);
    assert_eq!(
        Transition::FinishEvict.apply(evicting),
        Some(CellByte::new(CellState::Absent, false))
    );
    let recheck = CellByte::new(CellState::Recheck, true);
    assert_eq!(
        Transition::RecheckOk.apply(recheck),
        Some(CellByte::new(CellState::Present, false))
    );
    let claimed = CellByte::new(CellState::Claimed, true);
    assert_eq!(
        Transition::Publish.apply(claimed),
        Some(CellByte::new(CellState::Present, false))
    );
}

#[test]
fn a_cell_walks_its_whole_life_one_cas_at_a_time() {
    let mut b = CellByte::new(CellState::Absent, false);
    for t in [
        Transition::Claim,
        Transition::Publish,
        Transition::Durable,
        Transition::BeginEvict,
        Transition::AbortEvict,
        Transition::BeginEvict,
        Transition::FinishEvict,
    ] {
        b = t.apply(b).unwrap_or_else(|| panic!("{t:?} from {b:?}"));
    }
    assert_eq!(b, CellByte::new(CellState::Absent, false));
    // A second claim after the eviction, then the failure paths.
    b = Transition::Claim.apply(b).expect("absent");
    b = Transition::MarkBad.apply(b).expect("claimed");
    b = Transition::ExpireBad.apply(b).expect("bad");
    assert_eq!(b, CellByte::new(CellState::Absent, false));
    // A claim cannot be taken twice.
    b = Transition::Claim.apply(b).expect("absent");
    assert_eq!(Transition::Claim.apply(b), None);
    assert_eq!(
        Transition::Abandon.apply(b),
        Some(CellByte::new(CellState::Absent, false))
    );
}

#[test]
fn evictor_steps_map_to_their_transitions() {
    assert_eq!(EvictStep::Begin.transition(), Transition::BeginEvict);
    assert_eq!(EvictStep::Abort.transition(), Transition::AbortEvict);
    assert_eq!(EvictStep::Finish.transition(), Transition::FinishEvict);
}

#[test]
fn settles_map_to_their_transitions() {
    assert_eq!(Settle::RecheckOk.transition(), Transition::RecheckOk);
    assert_eq!(
        Settle::RecheckFailed.transition(),
        Transition::RecheckFailed
    );
    assert_eq!(Settle::ExpireBad.transition(), Transition::ExpireBad);
}

#[test]
fn no_transition_starts_from_a_byte_with_a_reserved_bit_set() {
    for t in [
        Transition::Claim,
        Transition::Abandon,
        Transition::Publish,
        Transition::MarkBad,
        Transition::Durable,
        Transition::BeginEvict,
        Transition::AbortEvict,
        Transition::FinishEvict,
        Transition::RecheckOk,
        Transition::RecheckFailed,
        Transition::ExpireBad,
    ] {
        let from = CellByte::new(t.from(), false);
        assert!(t.apply(from).is_some(), "{t:?}");
        for bit in 3..7 {
            assert_eq!(
                t.apply(CellByte(from.0 | 1 << bit)),
                None,
                "{t:?} bit {bit}"
            );
        }
    }
}
