//! Proves INIT negotiation (docs/design/fetch-cache-serve.md, 5.2): the flags of 5.2 are asked
//! for and granted only where the kernel offered them, the required flags the kernel lacks are
//! reported, both sides use the smaller minor version, the limits are answered as asked or
//! lowered to the kernel's offer, and a kernel too old or too new is answered per the header's
//! version rules.
//!
//! Catches:
//! - a flag granted that the kernel did not offer, or one asked for beyond the list of 5.2
//!   (passthrough, writeback cache, io_uring);
//! - `flags2` read without `FUSE_INIT_EXT`, or `HAS_RESEND` lost because `INIT_EXT` was not
//!   answered;
//! - `SPLICE_READ` asked for by a tree mount, or not by the put mount;
//! - a reply minor above the kernel's, `max_readahead` above the kernel's offer, `max_pages`
//!   sent without `MAX_PAGES`, or a congestion threshold above `max_background`;
//! - a kernel below 7.23 accepted, or one of a newer major refused instead of asked again;
//! - an INIT request, parsed and answered through a `Replier`, written with the wrong length.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

mod common;

use common::FakeDevice;
use tsfuse::abi::{
    InHeader, InitConfig, InitError, InitFlags, InitIn, InitOut, InitOutcome, MountKind,
    Negotiated, Operation,
};
use tsfuse::{Errno, Request, Tokens};

/// Every bit `FUSE_*` of the pinned header defines, as a 7.45 kernel such as the farm's offers
/// them (passthrough, writeback cache and io_uring among them).
const EVERYTHING: u64 = 0x07ff_ffff_ffff;

fn init_in(minor: u32, flags: u64, max_readahead: u32) -> InitIn {
    InitIn {
        major: 7,
        minor,
        max_readahead,
        flags: (flags & 0xffff_ffff) as u32,
        flags2: (flags >> 32) as u32,
        unused: [0; 11],
    }
}

fn accepted(config: &InitConfig, init: &InitIn) -> Negotiated {
    match config.negotiate(init).unwrap() {
        InitOutcome::Accepted(n) => n,
        other => panic!("not accepted: {other:?}"),
    }
}

#[test]
fn a_tree_mount_on_the_farm_kernel_gets_every_flag_of_5_2_and_nothing_else() {
    let n = accepted(&InitConfig::tree(), &init_in(45, EVERYTHING, 131_072));
    let want = MountKind::Tree.required().union(InitFlags::INIT_EXT);
    assert_eq!(n.granted, want);
    assert!(n.missing.is_empty());
    assert_eq!(n.offered, InitFlags(EVERYTHING));
    assert_eq!(n.minor, 45);
    assert_eq!(
        n.reply,
        InitOut {
            major: 7,
            minor: 45,
            max_readahead: 131_072,
            flags: (want.0 & 0xffff_ffff) as u32,
            max_background: 1024,
            congestion_threshold: 768,
            max_write: 4096,
            time_gran: 1,
            max_pages: 256,
            map_alignment: 0,
            flags2: 0x80, // HAS_RESEND, bit 39
            max_stack_depth: 0,
            request_timeout: 0,
            unused: [0; 11],
        }
    );
    // No passthrough (37), writeback cache (16), io_uring (41) or other bit beyond 5.2's.
    for bit in [16, 20, 29, 37, 41, 42] {
        assert!(!n.granted.contains(InitFlags(1 << bit)), "bit {bit}");
    }
}

#[test]
fn the_required_flags_are_the_list_of_5_2() {
    assert_eq!(
        format!("{:?}", MountKind::Tree.required()),
        "InitFlags(ASYNC_READ | SPLICE_WRITE | DO_READDIRPLUS | READDIRPLUS_AUTO | \
         NO_OPEN_SUPPORT | PARALLEL_DIROPS | MAX_PAGES | CACHE_SYMLINKS | NO_OPENDIR_SUPPORT | \
         HAS_RESEND)"
    );
    assert_eq!(
        MountKind::Put
            .required()
            .difference(MountKind::Tree.required()),
        InitFlags::SPLICE_READ
    );
    assert!(!MountKind::Tree.required().contains(InitFlags::SPLICE_READ));
}

#[test]
fn the_put_mount_also_gets_splice_read_and_a_larger_max_write() {
    let n = accepted(&InitConfig::put(), &init_in(45, EVERYTHING, 131_072));
    assert!(n.granted.contains(InitFlags::SPLICE_READ));
    assert_eq!(
        n.granted,
        MountKind::Put.required().union(InitFlags::INIT_EXT)
    );
    assert_eq!(n.reply.max_write, 1024 * 1024);
    assert_eq!(n.reply.max_pages, 256);
    assert_eq!(InitConfig::put().kind, MountKind::Put);
}

#[test]
fn a_flag_the_kernel_did_not_offer_is_never_granted_and_is_reported_missing() {
    let n = accepted(
        &InitConfig::tree(),
        &init_in(45, InitFlags::ASYNC_READ.0, 131_072),
    );
    assert_eq!(n.granted, InitFlags::ASYNC_READ);
    assert_eq!(
        n.missing,
        MountKind::Tree.required().difference(InitFlags::ASYNC_READ)
    );
    assert_eq!((n.reply.flags, n.reply.flags2), (1, 0));
    // Without MAX_PAGES no max_pages is sent.
    assert_eq!(n.reply.max_pages, 0);
}

#[test]
fn flags2_counts_only_with_init_ext() {
    // HAS_RESEND offered in flags2, but INIT_EXT absent: the kernel did not send flags2.
    let mut init = init_in(45, InitFlags::ASYNC_READ.0, 131_072);
    init.flags2 = (InitFlags::HAS_RESEND.0 >> 32) as u32;
    assert_eq!(InitFlags::offered(&init), InitFlags::ASYNC_READ);
    let n = accepted(&InitConfig::tree(), &init);
    assert!(n.missing.contains(InitFlags::HAS_RESEND));
    assert_eq!(n.reply.flags2, 0);
    assert!(!n.granted.contains(InitFlags::INIT_EXT));

    // With INIT_EXT, HAS_RESEND is granted, and INIT_EXT is answered so the kernel reads it.
    let offer = InitFlags::ASYNC_READ
        .union(InitFlags::INIT_EXT)
        .union(InitFlags::HAS_RESEND);
    let n = accepted(&InitConfig::tree(), &init_in(45, offer.0, 131_072));
    assert_eq!(n.granted, offer);
    assert_eq!(n.reply.flags, 1 | 1 << 30);
    assert_eq!(n.reply.flags2, 0x80);
}

#[test]
fn both_sides_use_the_smaller_minor_version() {
    for (kernel, both) in [(23, 23), (31, 31), (39, 39), (45, 45), (46, 45), (99, 45)] {
        let n = accepted(&InitConfig::tree(), &init_in(kernel, EVERYTHING, 131_072));
        assert_eq!((n.minor, n.reply.minor), (both, both), "kernel 7.{kernel}");
    }
}

#[test]
fn readahead_is_the_smaller_of_ours_and_the_kernels() {
    let tree = InitConfig::tree();
    assert_eq!(
        accepted(&tree, &init_in(45, 0, 65_536)).reply.max_readahead,
        65_536
    );
    assert_eq!(
        accepted(&tree, &init_in(45, 0, 1 << 20))
            .reply
            .max_readahead,
        131_072
    );
    assert_eq!(accepted(&tree, &init_in(45, 0, 0)).reply.max_readahead, 0);
}

#[test]
fn the_congestion_threshold_never_exceeds_max_background() {
    let config = InitConfig {
        max_background: 10,
        congestion_threshold: 50,
        ..InitConfig::tree()
    };
    let n = accepted(&config, &init_in(45, 0, 0));
    assert_eq!(
        (n.reply.max_background, n.reply.congestion_threshold),
        (10, 10)
    );
}

#[test]
fn a_kernel_older_than_7_23_is_refused_with_eproto() {
    for (major, minor) in [(7, 22), (7, 0), (6, 45), (0, 0)] {
        let init = InitIn {
            major,
            ..init_in(minor, EVERYTHING, 131_072)
        };
        let err = InitConfig::tree().negotiate(&init).unwrap_err();
        assert_eq!(err, InitError::Unsupported { major, minor });
        assert_eq!(err.errno(), Errno::EPROTO);
    }
    assert_eq!(
        InitError::Unsupported { major: 7, minor: 8 }.to_string(),
        "FUSE kernel protocol 7.8 is older than the 7.23 tsfuse needs"
    );
}

#[test]
fn a_kernel_of_a_newer_major_is_answered_with_our_version_only() {
    let init = InitIn {
        major: 8,
        ..init_in(0, EVERYTHING, 131_072)
    };
    let outcome = InitConfig::put().negotiate(&init).unwrap();
    assert_eq!(
        outcome,
        InitOutcome::MajorAhead(InitOut {
            major: 7,
            minor: 45,
            ..InitOut::default()
        })
    );
}

#[test]
fn init_flags_set_operations_and_debug() {
    let a = InitFlags::ASYNC_READ;
    let r = InitFlags::HAS_RESEND;
    assert!(a.union(r).contains(a) && a.union(r).contains(r));
    assert!(!a.contains(r));
    assert_eq!(a.union(r).intersection(r), r);
    assert_eq!(a.union(r).difference(a), r);
    assert!(InitFlags::EMPTY.is_empty() && !a.is_empty());
    assert_eq!(InitFlags::default(), InitFlags::EMPTY);
    assert_eq!(format!("{:?}", InitFlags::EMPTY), "InitFlags(0x0)");
    assert_eq!(
        format!("{:?}", a.union(r)),
        "InitFlags(ASYNC_READ | HAS_RESEND)"
    );
    assert_eq!(
        format!("{:?}", a.union(InitFlags(1 << 2 | 1 << 37))),
        "InitFlags(ASYNC_READ | 0x2000000004)"
    );
    assert_eq!(format!("{:?}", InitFlags(1 << 16)), "InitFlags(0x10000)");
}

#[test]
fn an_init_request_is_parsed_negotiated_and_answered_on_its_device() {
    let offer = init_in(45, EVERYTHING, 131_072);
    let header = InHeader {
        len: (InHeader::SIZE + InitIn::SIZE) as u32,
        opcode: 26,
        unique: 1,
        ..InHeader::default()
    };
    let buf = [header.as_bytes(), offer.as_bytes()].concat();
    let req = Request::parse(&buf).unwrap();
    let Ok(Operation::Init(init)) = Operation::parse(&req) else {
        panic!("not an INIT");
    };
    let InitOutcome::Accepted(n) = InitConfig::tree().negotiate(&init).unwrap() else {
        panic!("not accepted");
    };
    let tokens = Tokens::new();
    let device = FakeDevice::new();
    let _ = tokens
        .register(req.header.unique, device.clone())
        .ok(&[n.reply.as_bytes()]);
    let msgs = device.msgs();
    assert_eq!(msgs.len(), 1);
    assert_eq!((msgs[0].len, msgs[0].error, msgs[0].unique), (80, 0, 1));
    let (reply, rest) = InitOut::read_prefix(&msgs[0].payload).unwrap();
    assert!(rest.is_empty());
    assert_eq!(reply, n.reply);
}
