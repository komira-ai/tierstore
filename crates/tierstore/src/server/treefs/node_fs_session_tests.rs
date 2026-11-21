//! `NodeFs` over a `tsfuse` session and in the middle of a request: the `INIT` terms of a mount
//! (no passthrough asked for), the kernel's FORGET and BATCH_FORGET reaching the core, the routing
//! of the put directory's requests and forgets, and what the frontend gives back to the core when
//! the kernel does not take its reply (an INTERRUPT answered the request `EINTR` first, or the
//! device is gone): the lookup of a found LOOKUP, the handle of an OPEN, the lookups a READDIRPLUS
//! counted.

use std::cell::Cell;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::time::Duration;

use tsfuse::abi::{BatchForgetIn, CreateIn, ForgetIn, ForgetOne, InitFlags, InitIn, MkdirIn};
use tsfuse::{DevFd, Fs, Session};

use super::NodeFs;
use super::{
    BIN, CC, Delivery, RUN, Reply, TOOL, Wire, assert_not_taken, attr_out, cstr, entry_out, node,
    op, open_in, parse_reply, read_in, reads, request,
};
use crate::server::treefs::backing::{BackingId, Guard};
use crate::server::treefs::init_record::Outcome;
use crate::server::treefs::serving::init_config;
use crate::server::treefs::{fakedev, ino};

/// Asserts, with a stand-in for the kernel that offers passthrough (and resend), that the answer to
/// `INIT` asks for resend and not for passthrough, and declares a stack depth of 0: whatever the
/// kernel the test runs on offers, and whatever capability the test process holds.
#[test]
fn init_asks_for_no_passthrough_where_the_kernel_offers_it() {
    let (device, kernel) = fakedev::pair();
    kernel.init(1, InitFlags::PASSTHROUGH.union(InitFlags::HAS_RESEND));
    let session = Session::init(DevFd::from_fd(device), &init_config()).unwrap();
    let reply = kernel.reply();
    assert_eq!((reply.unique, reply.error), (1, 0));
    let outcome = Outcome::of(session.negotiated());
    assert!(outcome.capabilities.contains(InitFlags::PASSTHROUGH));
    assert!(!outcome.flags.contains(InitFlags::PASSTHROUGH));
    assert!(outcome.flags.contains(InitFlags::HAS_RESEND));
    assert_eq!(outcome.max_stack_depth, 0);
    // The reply itself: `flags2` (bytes 32 to 36) without bit 37 of the flags, a depth of 0.
    let word = |at: usize| u32::from_ne_bytes(reply.body[at..at + 4].try_into().unwrap());
    let replied = u64::from(word(12)) | (u64::from(word(32)) << 32);
    assert_eq!(replied & InitFlags::PASSTHROUGH.0, 0);
    assert_ne!(replied & InitFlags::HAS_RESEND.0, 0);
    assert_eq!(word(36), 0);
}

/// The kernel's end of a socket pair that stands in for `/dev/fuse` under a [`Session`]: one
/// request per write, one reply per read.
struct Socket {
    kernel: UnixDatagram,
    next: Cell<u64>,
}

impl Socket {
    /// The device end for a session, and the kernel's end for the test.
    fn pair() -> (OwnedFd, Self) {
        let (device, kernel) = UnixDatagram::pair().unwrap();
        // A reply that never comes fails the test instead of hanging it (no time is asserted).
        kernel
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let next = Cell::new(0);
        (OwnedFd::from(device), Self { kernel, next })
    }

    /// Sends a request that takes no reply.
    fn tell(&self, opcode: u32, nodeid: u64, body: &[u8]) {
        let unique = self.next.get() + 1;
        self.next.set(unique);
        let bytes = request(opcode, unique, nodeid, body);
        assert_eq!(self.kernel.send(&bytes).unwrap(), bytes.len());
    }

    /// The next reply.
    fn reply(&self) -> Reply {
        let mut bytes = vec![0u8; 1 << 20];
        let got = self.kernel.recv(&mut bytes).unwrap();
        parse_reply(&bytes[..got])
    }

    /// Sends a request and reads its reply.
    fn ask(&self, opcode: u32, nodeid: u64, body: &[u8]) -> Reply {
        self.tell(opcode, nodeid, body);
        self.reply()
    }
}

/// Asserts, with a kernel that sends FORGET and BATCH_FORGET over a `tsfuse` session, that each
/// forgotten lookup reaches the core (the tree stays in memory while a lookup of it is held and
/// leaves when the last is forgotten), and that the core still answers a LOOKUP and a GETATTR of
/// the inodes afterwards. The test sends the FORGET that a kernel sends when it drops an inode.
#[test]
fn the_kernel_forgetting_an_inode_reaches_the_core() {
    let wire = Wire::new("nodefs-forget");
    let (device, kernel) = Socket::pair();
    let init = InitIn {
        major: 7,
        minor: 40,
        max_readahead: 131_072,
        flags: u32::try_from(InitFlags::INIT_EXT.0).unwrap(),
        ..InitIn::default()
    };
    kernel.tell(op::INIT, 0, init.as_bytes());
    let session = Session::init(DevFd::from_fd(device), &init_config()).unwrap();
    assert_eq!(kernel.reply().error, 0);
    let fs = NodeFs::new(Arc::clone(&wire.rig.core));
    let serving = session.serve(Arc::new(fs)).unwrap();

    // Three lookups held: two of `cc` and one of `tool`.
    let cc = kernel.ask(op::LOOKUP, node(BIN), &cstr(b"cc"));
    assert_eq!(entry_out(&cc).nodeid, node(CC));
    assert_eq!(kernel.ask(op::LOOKUP, node(BIN), &cstr(b"cc")).error, 0);
    assert_eq!(kernel.ask(op::LOOKUP, node(BIN), &cstr(b"tool")).error, 0);
    let _ = wire.rig.core.detach(RUN);
    assert_eq!(wire.rig.core.trees_in_memory(), 1);
    assert_eq!(wire.served("forget"), 0);

    // A FORGET, then a request that is answered after it was handled (the one reader handles the
    // requests in order).
    kernel.tell(op::FORGET, node(CC), ForgetIn { nlookup: 1 }.as_bytes());
    assert_eq!(kernel.ask(op::STATFS, ino::ROOT, &[]).error, 0);
    assert_eq!(wire.served("forget"), 1);
    assert_eq!(wire.rig.core.trees_in_memory(), 1);

    let batch = [
        BatchForgetIn { count: 2, dummy: 0 }.as_bytes(),
        ForgetOne {
            nodeid: node(CC),
            nlookup: 1,
        }
        .as_bytes(),
        ForgetOne {
            nodeid: node(TOOL),
            nlookup: 1,
        }
        .as_bytes(),
    ]
    .concat();
    kernel.tell(op::BATCH_FORGET, 0, &batch);
    assert_eq!(kernel.ask(op::STATFS, ino::ROOT, &[]).error, 0);
    assert_eq!(wire.served("forget"), 3);
    assert_eq!(wire.rig.core.trees_in_memory(), 0);

    let got = kernel.ask(op::GETATTR, node(CC), &[0; 16]);
    assert_eq!((got.error, attr_out(&got).attr.size), (0, 3000));
    let again = kernel.ask(op::LOOKUP, node(BIN), &cstr(b"cc"));
    assert_eq!(entry_out(&again).nodeid, node(CC));
    assert_eq!(kernel.ask(op::DESTROY, 0, &[]).error, 0);
    let _ = serving.join().unwrap();
}

/// Asserts the routing of the put directory where it is served: the lookup of `put` in the root,
/// and every request on an inode of `put/`, go to it (an inode it does not know is its `ESTALE`),
/// every other request goes to the core, and where it is not served the same requests go to the
/// core (`put` is not a name there, and its inode is none).
#[test]
fn requests_on_the_put_directory_go_to_it_where_it_is_served_and_to_the_core_where_not() {
    let wire = Wire::with_put("nodefs-put");
    let put = entry_out(&wire.ask(op::LOOKUP, ino::ROOT, &cstr(b"put")));
    assert_eq!(put.nodeid, ino::PUT);
    let getattr = wire.ask(op::GETATTR, ino::PUT, &[0; 16]);
    assert_eq!((getattr.error, attr_out(&getattr).attr.ino), (0, ino::PUT));
    let unknown = wire.error(op::GETATTR, ino::put_dir(5), &[0; 16]);
    assert_eq!(unknown, reads(rustix::io::Errno::STALE));
    assert_eq!(wire.served("getattr"), 0);
    assert_eq!(wire.served("namespace"), 0);

    let trees = entry_out(&wire.ask(op::LOOKUP, ino::ROOT, &cstr(b"trees")));
    assert_eq!(trees.nodeid, ino::TREES);
    assert_eq!(wire.served("namespace"), 1);
    assert_eq!(wire.error(op::GETATTR, node(CC), &[0; 16]), 0);
    assert_eq!(wire.served("getattr"), 1);

    let without = Wire::new("nodefs-no-put");
    let lookup = without.error(op::LOOKUP, ino::ROOT, &cstr(b"put"));
    assert_eq!(lookup, reads(rustix::io::Errno::NOENT));
    let getattr = without.error(op::GETATTR, ino::PUT, &[0; 16]);
    assert_eq!(getattr, reads(rustix::io::Errno::NOENT));
    assert_eq!(
        (without.served("namespace"), without.served("getattr")),
        (1, 1)
    );
}

/// Asserts that a FORGET goes to the put directory for an inode of `put/` where it is served (a
/// directory a MKDIR made, which it keeps while the kernel holds a lookup of it, is let go
/// when they are forgotten, and the core is not told), and to the core for any other inode, and
/// for every inode where the put directory is not served.
#[test]
fn a_forget_goes_to_the_put_directory_for_its_inodes_and_to_the_core_for_the_rest() {
    let wire = Wire::with_put("nodefs-put-forget");
    let mut nonce = ino::PUT;
    for child in ["cas", "b", "build-1", "nonce-1"] {
        let looked = wire.ask(op::LOOKUP, nonce, &cstr(child.as_bytes()));
        nonce = entry_out(&looked).nodeid;
    }
    let create = CreateIn {
        flags: 0o1,
        mode: 0o100_644,
        umask: 0o022,
        open_flags: 0,
    };
    let body = [create.as_bytes(), &cstr(b"a.part")].concat();
    let _ = wire.ask(op::CREATE, nonce, &body);
    let mkdir = MkdirIn {
        mode: 0o755,
        umask: 0o022,
    };
    let made = wire.ask(
        op::MKDIR,
        nonce,
        &[mkdir.as_bytes(), &cstr(b"sub")].concat(),
    );
    let sub = entry_out(&made).nodeid;
    let found = wire.ask(op::LOOKUP, nonce, &cstr(b"sub"));
    assert_eq!(entry_out(&found).nodeid, sub);
    // The MKDIR and the LOOKUP: two lookups the kernel holds.
    wire.fs.forget(sub, 2);
    let gone = wire.ask(op::LOOKUP, nonce, &cstr(b"sub"));
    assert_eq!(entry_out(&gone).nodeid, 0);
    assert_eq!(wire.served("forget"), 0);

    wire.fs.forget(node(CC), 1);
    assert_eq!(wire.served("forget"), 1);

    let without = Wire::new("nodefs-no-put-forget");
    without.fs.forget(ino::put_dir(5), 1);
    without.fs.forget(node(CC), 1);
    assert_eq!(without.served("forget"), 2);
}

/// Asserts that a found LOOKUP whose reply the kernel does not take (an INTERRUPT answered the
/// request `EINTR` first, or the device is gone) is forgotten again, so that no tree is held in
/// memory by a lookup the kernel never forgets; and that a reply it takes is not.
#[test]
fn a_lookup_the_kernel_did_not_take_is_forgotten_again() {
    let taken = Wire::new("nodefs-lookup-taken");
    let _ = taken.ask(op::LOOKUP, node(BIN), &cstr(b"cc"));
    assert_eq!(taken.served("forget"), 0);
    assert_eq!(taken.kept_after_detach(), 1);
    for how in [Delivery::Interrupted, Delivery::Gone] {
        let wire = Wire::new(&format!("nodefs-lookup-lost-{how:?}"));
        let replies = wire.deliver(how, op::LOOKUP, node(BIN), &cstr(b"cc"));
        assert_not_taken(how, &replies);
        assert_eq!(wire.served("forget"), 1, "{how:?}");
        assert_eq!(wire.kept_after_detach(), 0, "{how:?}");
    }
}

/// Asserts that an OPEN whose reply the kernel does not take (an INTERRUPT answered the request
/// `EINTR` first, or the device is gone) is released again, so that no handle stays open; and that
/// the same holds for the open answered with an adopted id.
#[test]
fn an_open_the_kernel_did_not_take_is_released_again() {
    let taken = Wire::new("nodefs-open-taken");
    let _ = taken.ask(op::OPEN, node(CC), &open_in(0));
    assert_eq!(taken.rig.core.open_files(), 1);
    for how in [Delivery::Interrupted, Delivery::Gone] {
        let wire = Wire::new(&format!("nodefs-open-lost-{how:?}"));
        let replies = wire.deliver(how, op::OPEN, node(CC), &open_in(0));
        assert_not_taken(how, &replies);
        assert_eq!(wire.rig.core.open_files(), 0, "{how:?}");
        let adopting = Wire::new(&format!("nodefs-open-adopted-lost-{how:?}"));
        adopting.adopt(9, &|id| -> Guard { Arc::new(BackingId(id)) });
        let replies = adopting.deliver(how, op::OPEN, node(CC), &open_in(0));
        assert_not_taken(how, &replies);
        assert_eq!(adopting.rig.core.open_files(), 0, "{how:?}");
    }
}

/// Asserts that the entries a READDIRPLUS counted a lookup of, when the kernel does not take its
/// reply (an INTERRUPT answered the request `EINTR` first, or the device is gone), are forgotten
/// again, and that a reply it takes keeps them.
#[test]
fn a_listing_the_kernel_did_not_take_forgets_the_entries_it_counted() {
    let taken = Wire::new("nodefs-readdirplus-taken");
    let _ = taken.ask(op::READDIRPLUS, node(BIN), &read_in(0, 0, 4096));
    assert_eq!((taken.served("count"), taken.served("forget")), (2, 0));
    assert_eq!(taken.kept_after_detach(), 1);
    for how in [Delivery::Interrupted, Delivery::Gone] {
        let wire = Wire::new(&format!("nodefs-readdirplus-lost-{how:?}"));
        let replies = wire.deliver(how, op::READDIRPLUS, node(BIN), &read_in(0, 0, 4096));
        assert_not_taken(how, &replies);
        assert_eq!(
            (wire.served("count"), wire.served("forget")),
            (2, 2),
            "{how:?}"
        );
        assert_eq!(wire.kept_after_detach(), 0, "{how:?}");
    }
}
