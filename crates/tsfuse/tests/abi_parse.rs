//! Proves the parse of a request body for every implemented opcode: each argument struct read
//! field for field, names split at their NUL, WRITE data of exactly `size` bytes, the nodes of a
//! BATCH_FORGET, and INIT from an old kernel's 16 bytes; and the error, with its errno and
//! message, for an unimplemented opcode, a short body and a name with no NUL.
//!
//! Ports the tests of `src/ll/request.rs` of fuser 0.18.0 (MIT): a short header, a short body,
//! and INIT (its MKNOD test is dropped: MKNOD is not implemented).
//!
//! Catches:
//! - an argument struct read at the wrong offset or with fields swapped;
//! - a body one byte short of an opcode's arguments accepted, or its error naming the wrong
//!   sizes;
//! - a name read past its NUL, a second name not split, or a missing NUL accepted;
//! - WRITE data longer or shorter than `size`, or a BATCH_FORGET count larger than its nodes;
//! - an old kernel's 16-byte INIT refused, or its absent `flags2` read from other bytes;
//! - an unimplemented opcode parsed, or answered other than `ENOSYS`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use tsfuse::abi::{
    BatchForgetIn, CreateIn, FlushIn, ForgetIn, ForgetOne, FsyncIn, GetattrIn, InHeader, InitIn,
    InterruptIn, MkdirIn, OpenIn, Operation, ParseError, ReadIn, ReleaseIn, RenameIn, SetattrIn,
    UNIQUE_RESEND, WriteIn,
};
use tsfuse::{Errno, Opcode, Request, RequestHeader};

/// A request of `opcode` with `body`, as the kernel writes it.
fn request(opcode: u32, body: &[u8]) -> Vec<u8> {
    let header = InHeader {
        len: u32::try_from(InHeader::SIZE + body.len()).unwrap(),
        opcode,
        unique: 0x0102_0304_0506_0708,
        nodeid: 0x11,
        uid: 1000,
        gid: 1001,
        pid: 4242,
        total_extlen: 0,
        padding: 0,
    };
    [header.as_bytes(), body].concat()
}

fn parse(buf: &[u8]) -> Result<Operation<'_>, ParseError> {
    Operation::parse(&Request::parse(buf).unwrap())
}

/// The struct `T` filled with distinct bytes, so a field read from the wrong offset differs.
/// A wire struct's `read_prefix`.
type ReadPrefix<T> = fn(&[u8]) -> Option<(T, &[u8])>;

fn distinct<T: Copy>(read: ReadPrefix<T>) -> T {
    let bytes: Vec<u8> = (1..=200u8).collect();
    read(&bytes).unwrap().0
}

fn short(opcode: Opcode, need: usize, have: usize) -> ParseError {
    ParseError::Short { opcode, need, have }
}

/// Parses an opcode whose body is one fixed struct: whole, with trailing bytes, and one byte
/// short.
macro_rules! fixed {
    ($opcode:expr, $t:ty, $variant:path) => {{
        let arg: $t = distinct(<$t>::read_prefix);
        let code = $opcode.code();
        assert_eq!(parse(&request(code, arg.as_bytes())), Ok($variant(arg)));
        let padded = [arg.as_bytes(), &[0xEE; 8]].concat();
        assert_eq!(parse(&request(code, &padded)), Ok($variant(arg)));
        let cut = &arg.as_bytes()[..<$t>::SIZE - 1];
        assert_eq!(
            parse(&request(code, cut)),
            Err(short($opcode, <$t>::SIZE, <$t>::SIZE - 1))
        );
    }};
}

#[test]
fn every_fixed_struct_opcode_reads_its_argument_and_refuses_one_byte_less() {
    fixed!(Opcode::Forget, ForgetIn, Operation::Forget);
    fixed!(Opcode::Getattr, GetattrIn, Operation::Getattr);
    fixed!(Opcode::Setattr, SetattrIn, Operation::Setattr);
    fixed!(Opcode::Open, OpenIn, Operation::Open);
    fixed!(Opcode::Read, ReadIn, Operation::Read);
    fixed!(Opcode::Release, ReleaseIn, Operation::Release);
    fixed!(Opcode::Fsync, FsyncIn, Operation::Fsync);
    fixed!(Opcode::Flush, FlushIn, Operation::Flush);
    fixed!(Opcode::Opendir, OpenIn, Operation::Opendir);
    fixed!(Opcode::Readdir, ReadIn, Operation::Readdir);
    fixed!(Opcode::Releasedir, ReleaseIn, Operation::Releasedir);
    fixed!(Opcode::Interrupt, InterruptIn, Operation::Interrupt);
    fixed!(Opcode::Readdirplus, ReadIn, Operation::Readdirplus);
}

#[test]
fn opcodes_without_arguments_ignore_their_body() {
    for (op, want) in [
        (Opcode::Readlink, Operation::Readlink),
        (Opcode::Statfs, Operation::Statfs),
        (Opcode::Destroy, Operation::Destroy),
    ] {
        assert_eq!(parse(&request(op.code(), &[])), Ok(want));
        assert_eq!(parse(&request(op.code(), &[1, 2, 3])), Ok(want));
    }
}

#[test]
fn a_name_ends_at_its_nul_and_one_without_is_refused() {
    for op in [Opcode::Lookup, Opcode::Unlink] {
        let want = |name| match op {
            Opcode::Lookup => Operation::Lookup { name },
            _ => Operation::Unlink { name },
        };
        assert_eq!(parse(&request(op.code(), b"foo\0")), Ok(want(b"foo")));
        // Padding after the NUL is not part of the name.
        assert_eq!(
            parse(&request(op.code(), b"a.c\0\0\0\0\0")),
            Ok(want(b"a.c"))
        );
        assert_eq!(
            parse(&request(op.code(), b"foo")),
            Err(ParseError::MissingNul { opcode: op })
        );
        assert_eq!(
            parse(&request(op.code(), b"")),
            Err(ParseError::MissingNul { opcode: op })
        );
    }
}

#[test]
fn mkdir_and_create_read_their_argument_then_the_name() {
    let mkdir: MkdirIn = distinct(MkdirIn::read_prefix);
    let body = [mkdir.as_bytes(), b"dir\0"].concat();
    assert_eq!(
        parse(&request(9, &body)),
        Ok(Operation::Mkdir {
            arg: mkdir,
            name: b"dir"
        })
    );
    assert_eq!(
        parse(&request(9, mkdir.as_bytes())),
        Err(ParseError::MissingNul {
            opcode: Opcode::Mkdir
        })
    );
    assert_eq!(
        parse(&request(9, &body[..7])),
        Err(short(Opcode::Mkdir, 8, 7))
    );

    let create: CreateIn = distinct(CreateIn::read_prefix);
    let body = [create.as_bytes(), b"new.o\0"].concat();
    assert_eq!(
        parse(&request(35, &body)),
        Ok(Operation::Create {
            arg: create,
            name: b"new.o"
        })
    );
    assert_eq!(
        parse(&request(35, &body[..15])),
        Err(short(Opcode::Create, 16, 15))
    );
    assert_eq!(
        parse(&request(35, &body[..21])),
        Err(ParseError::MissingNul {
            opcode: Opcode::Create
        })
    );
}

#[test]
fn rename_reads_both_names() {
    let arg = RenameIn { newdir: 0x77 };
    let body = [arg.as_bytes(), b"old\0new\0"].concat();
    assert_eq!(
        parse(&request(12, &body)),
        Ok(Operation::Rename {
            arg,
            name: b"old",
            newname: b"new"
        })
    );
    let no_second_nul = [arg.as_bytes(), b"old\0new"].concat();
    assert_eq!(
        parse(&request(12, &no_second_nul)),
        Err(ParseError::MissingNul {
            opcode: Opcode::Rename
        })
    );
    assert_eq!(
        parse(&request(12, &body[..4])),
        Err(short(Opcode::Rename, 8, 4))
    );
}

#[test]
fn write_data_is_exactly_size_bytes() {
    let arg = WriteIn {
        fh: 3,
        offset: 4096,
        size: 5,
        ..WriteIn::default()
    };
    let body = [arg.as_bytes(), b"hello"].concat();
    assert_eq!(
        parse(&request(16, &body)),
        Ok(Operation::Write {
            arg,
            data: b"hello"
        })
    );
    // Bytes after `size` are not data.
    let longer = [arg.as_bytes(), b"hello, world"].concat();
    assert_eq!(
        parse(&request(16, &longer)),
        Ok(Operation::Write {
            arg,
            data: b"hello"
        })
    );
    let fewer = [arg.as_bytes(), b"hell"].concat();
    assert_eq!(
        parse(&request(16, &fewer)),
        Err(short(Opcode::Write, 45, 44))
    );
    let empty = WriteIn { size: 0, ..arg };
    assert_eq!(
        parse(&request(16, empty.as_bytes())),
        Ok(Operation::Write {
            arg: empty,
            data: b""
        })
    );
    assert_eq!(
        parse(&request(16, &body[..39])),
        Err(short(Opcode::Write, 40, 39))
    );
}

#[test]
fn batch_forget_yields_count_nodes_and_refuses_fewer() {
    let one = ForgetOne {
        nodeid: 5,
        nlookup: 2,
    };
    let two = ForgetOne {
        nodeid: 9,
        nlookup: 1,
    };
    let arg = BatchForgetIn { count: 2, dummy: 0 };
    let body = [arg.as_bytes(), one.as_bytes(), two.as_bytes()].concat();
    let buf = request(42, &body);
    let Ok(Operation::BatchForget { arg: got, nodes }) = parse(&buf) else {
        panic!("not a BATCH_FORGET");
    };
    assert_eq!(got, arg);
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes.collect::<Vec<_>>(), [one, two]);
    assert_eq!(
        format!("{nodes:?}"),
        "[ForgetOne { nodeid: 5, nlookup: 2 }, ForgetOne { nodeid: 9, nlookup: 1 }]"
    );

    let three = BatchForgetIn { count: 3, dummy: 0 };
    let body = [three.as_bytes(), one.as_bytes(), two.as_bytes()].concat();
    assert_eq!(
        parse(&request(42, &body)),
        Err(short(Opcode::BatchForget, 8 + 48, 8 + 32))
    );
    let huge = BatchForgetIn {
        count: u32::MAX,
        dummy: 0,
    };
    assert_eq!(
        parse(&request(42, huge.as_bytes())),
        Err(short(Opcode::BatchForget, 8 + u32::MAX as usize * 16, 8))
    );
    let none = BatchForgetIn { count: 0, dummy: 0 };
    let buf = request(42, none.as_bytes());
    let Ok(Operation::BatchForget { nodes, .. }) = parse(&buf) else {
        panic!("not a BATCH_FORGET");
    };
    assert_eq!(nodes.len(), 0);
    assert_eq!(
        parse(&request(42, &[0; 7])),
        Err(short(Opcode::BatchForget, 8, 7))
    );
}

#[test]
fn init_reads_the_full_struct_or_an_old_kernels_sixteen_bytes() {
    let full = InitIn {
        major: 7,
        minor: 45,
        max_readahead: 131_072,
        flags: 0x4000_0001,
        flags2: 0x80,
        unused: [0xAB; 11],
    };
    assert_eq!(
        parse(&request(26, full.as_bytes())),
        Ok(Operation::Init(full))
    );
    // A kernel before 7.36 sends major, minor, max_readahead and flags only.
    let old = &full.as_bytes()[..16];
    assert_eq!(
        parse(&request(26, old)),
        Ok(Operation::Init(InitIn {
            flags2: 0,
            unused: [0; 11],
            ..full
        }))
    );
    // Some bytes of flags2, but not all: the rest read as zero.
    let partial = &full.as_bytes()[..17];
    let Ok(Operation::Init(got)) = parse(&request(26, partial)) else {
        panic!("not an INIT");
    };
    assert_eq!(got.flags2, 0x80);
    assert_eq!(
        parse(&request(26, &old[..15])),
        Err(short(Opcode::Init, 16, 15))
    );
}

#[test]
fn an_unimplemented_opcode_is_refused_with_enosys() {
    for code in [6, 8, 11, 21, 45, 52, 4096] {
        let err = parse(&request(code, &[0; 64])).unwrap_err();
        assert_eq!(err, ParseError::Unimplemented(code));
        assert_eq!(err.errno(), Errno::ENOSYS);
    }
    assert_eq!(
        ParseError::Unimplemented(6).to_string(),
        "FUSE opcode 6 is not implemented"
    );
}

#[test]
fn a_malformed_body_is_answered_eio_with_a_message_naming_it() {
    let e = short(Opcode::Read, 40, 39);
    assert_eq!(e.errno(), Errno::EIO);
    assert_eq!(
        e.to_string(),
        "FUSE Read request body has 39 bytes; its arguments need 40"
    );
    let e = ParseError::MissingNul {
        opcode: Opcode::Lookup,
    };
    assert_eq!(e.errno(), Errno::EIO);
    assert_eq!(
        e.to_string(),
        "FUSE Lookup request has a name with no NUL terminator"
    );
}

#[test]
fn a_resent_request_keeps_its_unique_with_the_resend_bit() {
    let mut buf = request(3, GetattrIn::default().as_bytes());
    buf[8..16].copy_from_slice(&(UNIQUE_RESEND | 77).to_ne_bytes());
    let req = Request::parse(&buf).unwrap();
    assert_eq!(req.header.unique, UNIQUE_RESEND | 77);
    assert_eq!(
        Operation::parse(&req),
        Ok(Operation::Getattr(GetattrIn::default()))
    );
}

/// INIT of protocol 7.8 from fuser's request tests (`INIT_REQUEST`, little-endian), 104 bytes.
const INIT_REQUEST: [u8; 104] = {
    let mut r = [0u8; 104];
    let head: [u8; 64] = [
        0x68, 0x00, 0x00, 0x00, 0x1a, 0x00, 0x00, 0x00, // len, opcode
        0x0d, 0xf0, 0xad, 0xba, 0xef, 0xbe, 0xad, 0xde, // unique
        0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, // nodeid
        0x0d, 0xd0, 0x01, 0xc0, 0xfe, 0xca, 0x01, 0xc0, // uid, gid
        0x5e, 0xba, 0xde, 0xc0, 0x00, 0x00, 0x00, 0x00, // pid, total_extlen, padding
        0x07, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, // major, minor
        0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // max_readahead, flags
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // flags2, unused
    ];
    let mut i = 0;
    while i < head.len() {
        r[i] = head[i];
        i += 1;
    }
    r
};

#[test]
fn short_read_header() {
    assert!(RequestHeader::parse(&INIT_REQUEST[..20]).is_none());
    assert!(Request::parse(&INIT_REQUEST[..20]).is_none());
}

#[test]
fn short_read() {
    // The header says 104 bytes; 48 arrived.
    assert_eq!(RequestHeader::parse(&INIT_REQUEST).unwrap().len, 104);
    assert!(Request::parse(&INIT_REQUEST[..48]).is_none());
}

#[test]
fn init() {
    let req = Request::parse(&INIT_REQUEST).unwrap();
    assert_eq!(req.header.len, 104);
    assert_eq!(req.header.opcode, 26);
    assert_eq!(req.header.unique, 0xdead_beef_baad_f00d);
    assert_eq!(req.header.nodeid, 0x1122_3344_5566_7788);
    assert_eq!(req.header.uid, 0xc001_d00d);
    assert_eq!(req.header.gid, 0xc001_cafe);
    assert_eq!(req.header.pid, 0xc0de_ba5e);
    let Ok(Operation::Init(init)) = Operation::parse(&req) else {
        panic!("not an INIT");
    };
    assert_eq!((init.major, init.minor), (7, 8));
    assert_eq!(init.max_readahead, 4096);
    // The wire header struct reads the same fields as the frozen `RequestHeader`.
    let (wire, _) = InHeader::read_prefix(&INIT_REQUEST).unwrap();
    assert_eq!(
        (wire.len, wire.opcode, wire.unique, wire.nodeid),
        (104, 26, req.header.unique, req.header.nodeid)
    );
    assert_eq!(
        (wire.uid, wire.gid, wire.pid),
        (0xc001_d00d, 0xc001_cafe, 0xc0de_ba5e)
    );
}
