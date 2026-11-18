//! The label values: each closed set's values are distinct and indexed by position, and each
//! mirror of a frozen type maps every one of its values to the label of the same name.
//!
//! Catches: two values of a label rendering the same text, a state set storing the wrong state,
//! a frozen value mapped to another value's label, a FUSE opcode labelled as another operation,
//! a pack size placed in the wrong size class at a band's edge.

use std::collections::BTreeSet;

use tierstore_fetch_api::ids::Class;
use tierstore_fetch_api::list::Refusal;
use tierstore_fetch_api::upstream;
use tierstore_fetch_api::verify::VerifyPriority;
use tierstore_format::descriptor::Code;
use tierstore_format::ids::{DriveId, Generation, NodeId};
use tierstore_metrics::labels::{
    AbandonReason, AdmissionReason, BudgetReason, Copies, DegradedCause, DestageOutcome,
    DestagePriority, Dir, Disk, Drive503, DriveState, FreeOutcome, FuseOp, GetKind, ListRefusal,
    LocateResult, Margin, PackLostPhase, PromoteOutcome, PutOutcome, ReadWaitCause, RebuildKind,
    ReclaimKind, RecoveryPhase, SealReason, SealRound, SizeClass, StripeCode, StripeState,
    TransportMode,
};
use tierstore_metrics::{LabelValue, State};
use tierstore_store_api::hot::{self, Unavailable};
use tierstore_store_api::proto::LocateAnswer;
use tierstore_store_api::sched::{DriveClass, Refused};
use tsfuse::Opcode;

/// `CamelCase` as `snake_case`: the label text a mirrored variant's name implies.
fn snake(camel: &str) -> String {
    let mut out = String::new();
    for (i, c) in camel.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Asserts, for a closed label, that each value's index is its position, that its text is its
/// `as_str`, and that the texts are distinct snake-case words; returns the texts.
fn check<T: State + std::fmt::Debug>(as_str: fn(T) -> &'static str) -> Vec<&'static str> {
    let mut texts = Vec::new();
    for (i, value) in T::ALL.iter().copied().enumerate() {
        assert_eq!(value.index(), i, "{value:?}");
        assert_eq!(value.text(), as_str(value), "{value:?}");
        let text = as_str(value);
        assert!(
            text.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{text}"
        );
        texts.push(text);
    }
    let distinct: BTreeSet<&str> = texts.iter().copied().collect();
    assert_eq!(distinct.len(), texts.len(), "{}", T::NAME);
    texts
}

/// Asserts the label name, the indexes and the distinct texts of every closed label.
#[test]
fn every_closed_label_has_distinct_texts_indexed_by_position() {
    assert_eq!(
        check::<SizeClass>(SizeClass::as_str),
        ["small", "medium", "large"]
    );
    assert_eq!(check::<BudgetReason>(BudgetReason::as_str).len(), 11);
    assert_eq!(
        check::<GetKind>(GetKind::as_str),
        ["grid_part", "demand_sub", "retry", "hedge"]
    );
    assert_eq!(
        check::<ReadWaitCause>(ReadWaitCause::as_str)[0],
        "launch_before_prepare"
    );
    assert_eq!(
        check::<AdmissionReason>(AdmissionReason::as_str),
        ["budget", "enospc", "oversize"]
    );
    assert_eq!(
        check::<TransportMode>(TransportMode::as_str),
        ["dev_fuse", "io_uring"]
    );
    assert_eq!(check::<FuseOp>(FuseOp::as_str).len(), 25);
    assert_eq!(check::<ListRefusal>(ListRefusal::as_str).len(), 4);
    assert_eq!(
        check::<PackLostPhase>(PackLostPhase::as_str),
        ["before_job", "mid_job"]
    );
    assert_eq!(check::<SealReason>(SealReason::as_str).len(), 4);
    assert_eq!(
        check::<SealRound>(SealRound::as_str),
        ["prepare", "adopted", "rejected"]
    );
    assert_eq!(check::<ReclaimKind>(ReclaimKind::as_str).len(), 3);
    assert_eq!(check::<PutOutcome>(PutOutcome::as_str).len(), 8);
    assert_eq!(check::<AbandonReason>(AbandonReason::as_str).len(), 2);
    assert_eq!(check::<LocateResult>(LocateResult::as_str).len(), 4);
    assert_eq!(check::<RecoveryPhase>(RecoveryPhase::as_str).len(), 4);
    assert_eq!(
        check::<StripeState>(StripeState::as_str),
        ["full", "partial"]
    );
    assert_eq!(check::<DegradedCause>(DegradedCause::as_str).len(), 6);
    assert_eq!(check::<RebuildKind>(RebuildKind::as_str).len(), 5);
    assert_eq!(
        check::<DestageOutcome>(DestageOutcome::as_str),
        ["committed", "abandoned"]
    );
    assert_eq!(
        check::<DestagePriority>(DestagePriority::as_str),
        ["normal", "urgent"]
    );
    assert_eq!(check::<PromoteOutcome>(PromoteOutcome::as_str).len(), 4);
    assert_eq!(
        check::<FreeOutcome>(FreeOutcome::as_str),
        ["committed", "waited", "found_live"]
    );
    assert_eq!(check::<DriveState>(DriveState::as_str).len(), 8);
    assert_eq!(check::<Dir>(Dir::as_str), ["read", "write"]);
}

/// Asserts that every opcode `tsfuse` knows maps to the operation of its name, and every other
/// code to `other`.
#[test]
fn every_fuse_opcode_maps_to_the_operation_of_its_name() {
    let mut known = 0;
    for code in 0..=64 {
        match Opcode::from_code(code) {
            Some(op) => {
                known += 1;
                let label = FuseOp::from_code(code);
                assert_eq!(label, FuseOp::from(op));
                assert_eq!(label.as_str(), snake(&format!("{op:?}")));
            }
            None => assert_eq!(FuseOp::from_code(code), FuseOp::Other),
        }
    }
    assert_eq!(known, 24);
    assert_eq!(FuseOp::from_code(u32::MAX), FuseOp::Other);
}

/// Asserts that each mirror of a frozen enum maps every frozen value to the label of its name.
#[test]
fn every_frozen_value_maps_to_the_label_of_its_name() {
    for refusal in [
        Refusal::V1List,
        Refusal::MissingSidecar,
        Refusal::UnknownCellDigest(9),
        Refusal::UnknownVersion(3),
    ] {
        assert_eq!(ListRefusal::from(refusal).as_str(), refusal.label());
    }
    for reason in [
        hot::SealReason::Size,
        hot::SealReason::Age,
        hot::SealReason::Early,
        hot::SealReason::TakeOver,
    ] {
        assert_eq!(
            SealReason::from(reason).as_str(),
            snake(&format!("{reason:?}"))
        );
    }
    let states = [
        upstream::DriveState::Up,
        upstream::DriveState::Indexing,
        upstream::DriveState::Stalled,
        upstream::DriveState::Suspect,
        upstream::DriveState::Down,
        upstream::DriveState::Failed,
        upstream::DriveState::Drain,
        upstream::DriveState::Out,
    ];
    for state in states {
        let label = DriveState::from(state);
        assert_eq!(label.as_str(), snake(&format!("{state:?}")));
        let refused = Drive503::from(&Refused::State(state));
        assert_eq!(refused, Drive503::State(label));
        assert_eq!(refused.text(), label.as_str());
    }
    let full = Drive503::from(&Refused::QueueFull { retry_after_ms: 5 });
    assert_eq!(
        (full, full.text().into_owned()),
        (Drive503::QueueFull, "queue_full".to_owned())
    );
    for why in [
        Unavailable::TooFewCopies,
        Unavailable::NotDefinitive,
        Unavailable::Throttled,
        Unavailable::Minority,
    ] {
        assert_eq!(PutOutcome::from(why).as_str(), snake(&format!("{why:?}")));
    }
    let generation = Generation([7; 16]);
    let answers = [
        (
            LocateAnswer::Hot {
                generation,
                size: 1,
                copies: Vec::new(),
            },
            "hot",
        ),
        (
            LocateAnswer::Cold {
                generation,
                size: 1,
                extents: Vec::new(),
            },
            "cold",
        ),
        (LocateAnswer::Absent, "absent"),
        (LocateAnswer::Indexing, "indexing"),
    ];
    for (answer, text) in answers {
        assert_eq!(LocateResult::from(&answer).as_str(), text);
    }
}

/// Asserts the text of the id and numeric labels: drive and node ids as their `Display`, the cache
/// disk, copies, margins and both kinds of stripe code.
#[test]
fn id_and_numeric_labels_render_as_their_values() {
    let id = DriveId([0xab; 16]);
    assert_eq!(id.text(), "ab".repeat(16));
    assert_eq!(NodeId(12).text(), "12");
    assert_eq!(Disk::Cache.text(), "cache");
    assert_eq!(Disk::Drive(id).text(), "ab".repeat(16));
    assert_eq!(Copies(3).text(), "3");
    assert_eq!(Margin::Unreadable.text(), "unreadable");
    assert_eq!(Margin::Spare(2).text(), "2");
    assert_eq!(StripeCode::from(Code::Ec { k: 4, m: 4 }).text(), "4+4");
    assert_eq!(StripeCode::from(Code::Replicas(2)).text(), "replicas-2");
    assert_eq!(
        [
            Disk::NAME,
            Copies::NAME,
            Margin::NAME,
            StripeCode::NAME,
            Drive503::NAME
        ],
        ["disk", "copies", "margin", "code", "reason"]
    );
    assert_eq!([DriveId::NAME, NodeId::NAME], ["drive", "node"]);
}

/// Asserts the text of the frozen enums used as labels directly: the class of a fetch, the drive
/// scheduler's class and the verify priority.
#[test]
fn frozen_enums_used_directly_render_their_names() {
    let classes = [Class::Demand, Class::Prepare, Class::Background].map(|c| c.text());
    assert_eq!(classes, ["demand", "prepare", "background"]);
    let drive_classes = [
        DriveClass::Demand,
        DriveClass::Write,
        DriveClass::Prepare,
        DriveClass::Background,
    ]
    .map(|c| c.text());
    assert_eq!(drive_classes, ["demand", "write", "prepare", "background"]);
    let priorities = [VerifyPriority::Fill, VerifyPriority::Recheck].map(|p| p.text());
    assert_eq!(priorities, ["fill", "recheck"]);
    assert_eq!(
        [Class::NAME, DriveClass::NAME, VerifyPriority::NAME],
        ["class", "class", "priority"]
    );
}

/// Asserts the size class at each edge of the bands of 4.4 and its index round trip.
#[test]
fn a_pack_size_falls_in_its_band_at_each_edge() {
    const MIB: u64 = 1 << 20;
    let cases = [
        (0, SizeClass::Small),
        (32 * MIB, SizeClass::Small),
        (32 * MIB + 1, SizeClass::Medium),
        (512 * MIB, SizeClass::Medium),
        (512 * MIB + 1, SizeClass::Large),
        (u64::MAX, SizeClass::Large),
    ];
    for (bytes, class) in cases {
        assert_eq!(SizeClass::of_pack(bytes), class, "{bytes}");
    }
    for class in SizeClass::ALL {
        let index = u8::try_from(class.index()).unwrap();
        assert_eq!(SizeClass::from_index(index), Some(*class));
    }
    assert_eq!(SizeClass::from_index(3), None);
}
