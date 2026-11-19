//! Proves that every wire struct of `tsfuse::abi` has the layout the farm's C compiler gives the
//! pinned uapi header (`abi/layout.txt`, the output of `abi/probe.c` run on a farm node), and
//! that the opcode, flag and constant values equal the header's.
//!
//! Catches:
//! - a Rust struct whose size, alignment, or any field's offset or size differs from C's;
//! - a struct or field the probe lists that the Rust side lacks, or the reverse;
//! - a probe that leaves out a field of a struct (its fields must tile the struct exactly);
//! - a pinned header changed without the digest recorded here, or without a new probe run
//!   (the probe prints the FNV-1a checksum of the header it read, which must be the pinned
//!   header's, and its version line must equal the header's and the library's);
//! - an opcode, INIT flag, FOPEN or FATTR bit, or constant that differs from the header;
//! - a comparison that cannot see a defect: the same check, run on probe output with one
//!   offset, size, field, struct or version changed, names the difference.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::BTreeMap;
use std::mem::offset_of;

use sha2::{Digest, Sha256};
use tsfuse::Opcode;
use tsfuse::abi::{
    self, Attr, AttrOut, BatchForgetIn, CreateIn, Dirent, DirentPlus, EntryOut, FlushIn, ForgetIn,
    ForgetOne, FsyncIn, GetattrIn, InHeader, InitFlags, InitIn, InitOut, InterruptIn, Kstatfs,
    MkdirIn, OpenIn, OpenOut, OutHeader, ReadIn, ReleaseIn, RenameIn, SetattrIn, StatfsOut,
    WriteIn, WriteOut,
};

/// The pinned header: `include/uapi/linux/fuse.h` of the farm kernel, Linux 7.0.0-42 (the
/// header package of 7.0.0-43 has the same bytes).
const HEADER: &str = include_str!("../abi/fuse.h");
/// Its SHA-256, as read on the farm node the probe ran on.
const HEADER_SHA256: &str = "5f6abd74663a746b8944086ed98a559f54041e6114a305cd980b6dfc3fed9b4b";
/// The probe's output for that header (x86_64, gcc 14.2).
const LAYOUT: &str = include_str!("../abi/layout.txt");

/// The layout of one struct, from the probe or from Rust.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Layout {
    size: usize,
    align: usize,
    /// `(C field name, offset, size)`; size 0 is a flexible array member.
    fields: Vec<(String, usize, usize)>,
}

/// The probe's output: the version and each struct's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Probe {
    header_fnv1a64: String,
    version: (u32, u32),
    structs: BTreeMap<String, Layout>,
}

fn parse_probe(text: &str) -> Probe {
    let mut version = None;
    let mut header_fnv1a64 = None;
    let mut structs: BTreeMap<String, Layout> = BTreeMap::new();
    for line in text.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        let n = |i: usize| -> usize { w[i].parse().unwrap_or_else(|e| panic!("{line}: {e}")) };
        match w.as_slice() {
            ["header-fnv1a64", hash] => header_fnv1a64 = Some((*hash).to_owned()),
            ["version", _, _] => version = Some((n(1) as u32, n(2) as u32)),
            ["struct", name, _, _] => {
                let layout = Layout {
                    size: n(2),
                    align: n(3),
                    fields: Vec::new(),
                };
                assert!(
                    structs.insert((*name).to_owned(), layout).is_none(),
                    "{line}"
                );
            }
            ["field", name, field, _, _] => structs
                .get_mut(*name)
                .unwrap_or_else(|| panic!("field before its struct: {line}"))
                .fields
                .push(((*field).to_owned(), n(3), n(4))),
            _ => panic!("not a probe line: {line:?}"),
        }
    }
    Probe {
        header_fnv1a64: header_fnv1a64.expect("no header-fnv1a64 line"),
        version: version.expect("no version line"),
        structs,
    }
}

/// The layout of a Rust struct, its C name, and its fields by C name. A struct's `read_prefix`
/// and `as_bytes` are run on a pattern so each is shown to round-trip at `SIZE` bytes.
macro_rules! rust {
    ($c:literal, $t:ty, { $($cf:literal: $rf:ident),* $(,)? } $(, flex $flex:literal)?) => {{
        let pattern: Vec<u8> = (0..=255u8).collect();
        let (value, rest) = <$t>::read_prefix(&pattern).unwrap();
        assert_eq!(value.as_bytes(), &pattern[..<$t>::SIZE], $c);
        assert_eq!(rest, &pattern[<$t>::SIZE..], $c);
        assert!(<$t>::read_prefix(&pattern[..<$t>::SIZE - 1]).is_none(), $c);
        let zero = <$t>::default();
        #[allow(unused_mut, reason = "a struct with no flexible member pushes nothing more")]
        let mut fields = vec![$(
            ($cf.to_owned(), offset_of!($t, $rf), size_of_val(&zero.$rf))
        ),*];
        $( fields.push(($flex.to_owned(), size_of::<$t>(), 0)); )?
        (
            $c.to_owned(),
            Layout { size: size_of::<$t>(), align: align_of::<$t>(), fields },
        )
    }};
}

fn rust_layouts() -> BTreeMap<String, Layout> {
    BTreeMap::from([
        rust!("fuse_in_header", InHeader, {
            "len": len, "opcode": opcode, "unique": unique, "nodeid": nodeid, "uid": uid,
            "gid": gid, "pid": pid, "total_extlen": total_extlen, "padding": padding,
        }),
        rust!("fuse_out_header", OutHeader, { "len": len, "error": error, "unique": unique }),
        rust!("fuse_attr", Attr, {
            "ino": ino, "size": size, "blocks": blocks, "atime": atime, "mtime": mtime,
            "ctime": ctime, "atimensec": atimensec, "mtimensec": mtimensec,
            "ctimensec": ctimensec, "mode": mode, "nlink": nlink, "uid": uid, "gid": gid,
            "rdev": rdev, "blksize": blksize, "flags": flags,
        }),
        rust!("fuse_kstatfs", Kstatfs, {
            "blocks": blocks, "bfree": bfree, "bavail": bavail, "files": files, "ffree": ffree,
            "bsize": bsize, "namelen": namelen, "frsize": frsize, "padding": padding,
            "spare": spare,
        }),
        rust!("fuse_entry_out", EntryOut, {
            "nodeid": nodeid, "generation": generation, "entry_valid": entry_valid,
            "attr_valid": attr_valid, "entry_valid_nsec": entry_valid_nsec,
            "attr_valid_nsec": attr_valid_nsec, "attr": attr,
        }),
        rust!("fuse_forget_in", ForgetIn, { "nlookup": nlookup }),
        rust!("fuse_forget_one", ForgetOne, { "nodeid": nodeid, "nlookup": nlookup }),
        rust!("fuse_batch_forget_in", BatchForgetIn, { "count": count, "dummy": dummy }),
        rust!("fuse_getattr_in", GetattrIn, {
            "getattr_flags": getattr_flags, "dummy": dummy, "fh": fh,
        }),
        rust!("fuse_attr_out", AttrOut, {
            "attr_valid": attr_valid, "attr_valid_nsec": attr_valid_nsec, "dummy": dummy,
            "attr": attr,
        }),
        rust!("fuse_mkdir_in", MkdirIn, { "mode": mode, "umask": umask }),
        rust!("fuse_rename_in", RenameIn, { "newdir": newdir }),
        rust!("fuse_setattr_in", SetattrIn, {
            "valid": valid, "padding": padding, "fh": fh, "size": size,
            "lock_owner": lock_owner, "atime": atime, "mtime": mtime, "ctime": ctime,
            "atimensec": atimensec, "mtimensec": mtimensec, "ctimensec": ctimensec,
            "mode": mode, "unused4": unused4, "uid": uid, "gid": gid, "unused5": unused5,
        }),
        rust!("fuse_open_in", OpenIn, { "flags": flags, "open_flags": open_flags }),
        rust!("fuse_create_in", CreateIn, {
            "flags": flags, "mode": mode, "umask": umask, "open_flags": open_flags,
        }),
        rust!("fuse_open_out", OpenOut, {
            "fh": fh, "open_flags": open_flags, "backing_id": backing_id,
        }),
        rust!("fuse_release_in", ReleaseIn, {
            "fh": fh, "flags": flags, "release_flags": release_flags, "lock_owner": lock_owner,
        }),
        rust!("fuse_flush_in", FlushIn, {
            "fh": fh, "unused": unused, "padding": padding, "lock_owner": lock_owner,
        }),
        rust!("fuse_read_in", ReadIn, {
            "fh": fh, "offset": offset, "size": size, "read_flags": read_flags,
            "lock_owner": lock_owner, "flags": flags, "padding": padding,
        }),
        rust!("fuse_write_in", WriteIn, {
            "fh": fh, "offset": offset, "size": size, "write_flags": write_flags,
            "lock_owner": lock_owner, "flags": flags, "padding": padding,
        }),
        rust!("fuse_write_out", WriteOut, { "size": size, "padding": padding }),
        rust!("fuse_statfs_out", StatfsOut, { "st": st }),
        rust!("fuse_fsync_in", FsyncIn, {
            "fh": fh, "fsync_flags": fsync_flags, "padding": padding,
        }),
        rust!("fuse_init_in", InitIn, {
            "major": major, "minor": minor, "max_readahead": max_readahead, "flags": flags,
            "flags2": flags2, "unused": unused,
        }),
        rust!("fuse_init_out", InitOut, {
            "major": major, "minor": minor, "max_readahead": max_readahead, "flags": flags,
            "max_background": max_background, "congestion_threshold": congestion_threshold,
            "max_write": max_write, "time_gran": time_gran, "max_pages": max_pages,
            "map_alignment": map_alignment, "flags2": flags2,
            "max_stack_depth": max_stack_depth, "request_timeout": request_timeout,
            "unused": unused,
        }),
        rust!("fuse_interrupt_in", InterruptIn, { "unique": unique }),
        rust!("fuse_dirent", Dirent, {
            "ino": ino, "off": off, "namelen": namelen, "type": typ,
        }, flex "name"),
        rust!("fuse_direntplus", DirentPlus, { "entry_out": entry_out, "dirent": dirent }),
    ])
}

/// Every difference between the probe's output and the Rust structs, and every struct whose
/// probed fields do not tile it (a field the probe left out).
fn mismatches(probe: &Probe, rust: &BTreeMap<String, Layout>) -> Vec<String> {
    let mut out = Vec::new();
    let pinned = fnv1a64(HEADER.as_bytes());
    if probe.header_fnv1a64 != pinned {
        out.push(format!(
            "header: probe read {}, the pinned fuse.h is {pinned}",
            probe.header_fnv1a64
        ));
    }
    let ours = (abi::KERNEL_VERSION, abi::KERNEL_MINOR_VERSION);
    if probe.version != ours {
        out.push(format!(
            "version: probe {:?}, tsfuse {ours:?}",
            probe.version
        ));
    }
    for (name, c) in &probe.structs {
        let mut sorted = c.fields.clone();
        sorted.sort_by_key(|f| f.1);
        let mut end = 0;
        for (field, offset, size) in &sorted {
            if *offset != end {
                out.push(format!(
                    "{name}: probed fields do not tile it at {field} ({offset} after {end})"
                ));
            }
            end = offset + size;
        }
        if end != c.size {
            out.push(format!(
                "{name}: probed fields end at {end}, the struct at {}",
                c.size
            ));
        }
        match rust.get(name) {
            None => out.push(format!("{name}: probed, but no Rust struct")),
            Some(r) if r != c => out.push(format!("{name}: probe {c:?}, Rust {r:?}")),
            Some(_) => {}
        }
    }
    for name in rust.keys().filter(|n| !probe.structs.contains_key(*n)) {
        out.push(format!("{name}: a Rust struct the probe does not list"));
    }
    out
}

#[test]
fn every_wire_struct_has_the_layout_of_the_farm_probe() {
    let probe = parse_probe(LAYOUT);
    assert_eq!(probe.structs.len(), 28);
    assert_eq!(mismatches(&probe, &rust_layouts()), Vec::<String>::new());
}

#[test]
fn the_pinned_header_is_the_one_the_probe_ran_on() {
    assert_eq!(hex(&Sha256::digest(HEADER.as_bytes())), HEADER_SHA256);
    let probe = parse_probe(LAYOUT);
    let header_version = (
        define(HEADER, "FUSE_KERNEL_VERSION"),
        define(HEADER, "FUSE_KERNEL_MINOR_VERSION"),
    );
    assert_eq!(header_version, (7, 45));
    assert_eq!(
        probe.version,
        (header_version.0 as u32, header_version.1 as u32)
    );
}

/// FNV-1a (64 bits) of `bytes` in 16 lowercase hex digits, as the probe prints it.
fn fnv1a64(bytes: &[u8]) -> String {
    let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("{hash:016x}")
}

#[test]
fn fnv1a64_is_the_published_function() {
    // The test vectors of the FNV reference: "" and "a".
    assert_eq!(fnv1a64(b""), "cbf29ce484222325");
    assert_eq!(fnv1a64(b"a"), "af63dc4c8601ec8c");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_comparison_names_each_kind_of_defect() {
    let rust = rust_layouts();
    let changed = |from: &str, to: &str| {
        assert!(LAYOUT.contains(from), "{from}");
        mismatches(&parse_probe(&LAYOUT.replacen(from, to, 1)), &rust)
    };

    // An offset moved: the struct no longer tiles, and it differs from Rust.
    let m = changed(
        "field fuse_read_in size 16 4",
        "field fuse_read_in size 20 4",
    );
    assert_eq!(m.len(), 3, "{m:?}");
    assert!(m[0].starts_with("fuse_read_in: probed fields do not tile it at size (20 after 16)"));
    assert!(
        m[1].starts_with("fuse_read_in: probed fields do not tile it at read_flags (20 after 24)")
    );
    assert!(m[2].starts_with("fuse_read_in: probe Layout"));

    // A field left out of the probe: a gap.
    let m = changed("field fuse_attr rdev 76 4\n", "");
    assert_eq!(m.len(), 2, "{m:?}");
    assert!(m[0].starts_with("fuse_attr: probed fields do not tile it at blksize (80 after 76)"));

    // A last field left out: the fields end short of the struct.
    let m = changed("field fuse_write_out padding 4 4\n", "");
    assert_eq!(
        m[0],
        "fuse_write_out: probed fields end at 4, the struct at 8"
    );

    // A size or alignment that differs, with fields that still tile.
    let m = changed("struct fuse_open_in 8 4", "struct fuse_open_in 8 8");
    assert_eq!(m.len(), 1, "{m:?}");
    assert!(m[0].starts_with("fuse_open_in: probe Layout { size: 8, align: 8"));

    // A struct only one side has.
    let m = changed(
        "struct fuse_rename_in 8 8\nfield fuse_rename_in newdir 0 8\n",
        "",
    );
    assert_eq!(m, ["fuse_rename_in: a Rust struct the probe does not list"]);
    let m = changed(
        "struct fuse_rename_in 8 8",
        "struct fuse_rename2_in 8 8\nfield fuse_rename2_in newdir 0 8\nstruct fuse_rename_in 8 8",
    );
    assert_eq!(m, ["fuse_rename2_in: probed, but no Rust struct"]);

    // A probe run on another header.
    let pinned = fnv1a64(HEADER.as_bytes());
    let m = changed(
        &format!("header-fnv1a64 {pinned}"),
        "header-fnv1a64 0123456789abcdef",
    );
    assert_eq!(
        m,
        [format!(
            "header: probe read 0123456789abcdef, the pinned fuse.h is {pinned}"
        )]
    );

    // Another protocol version.
    let m = changed("version 7 45", "version 7 46");
    assert_eq!(m, ["version: probe (7, 46), tsfuse (7, 45)"]);
}

/// The value of `#define NAME <n>`, `(1 << n)` or `(1ULL << n)` in `text`.
fn define(text: &str, name: &str) -> u64 {
    let line = text
        .lines()
        .find(|l| {
            let mut w = l.split_whitespace();
            w.next() == Some("#define") && w.next() == Some(name)
        })
        .unwrap_or_else(|| panic!("no #define {name}"));
    let value: String = line.split_whitespace().skip(2).collect::<Vec<_>>().concat();
    if let Some(shift) = value
        .strip_prefix("(1<<")
        .or_else(|| value.strip_prefix("(1ULL<<"))
        .and_then(|v| v.strip_suffix(')'))
    {
        1u64 << shift.parse::<u32>().unwrap()
    } else {
        value
            .parse()
            .unwrap_or_else(|e| panic!("{name} = {value}: {e}"))
    }
}

/// The value of `NAME = <n>,` in an enum of `text`.
fn enumerator(text: &str, name: &str) -> u32 {
    text.lines()
        .find_map(|l| {
            let (n, v) = l.split_once('=')?;
            // `FUSE_FORGET = 2,  /* no reply */`: the value ends at the comma.
            let v = v.split(',').next().unwrap_or_default().trim();
            (n.trim() == name).then(|| v.parse().unwrap())
        })
        .unwrap_or_else(|| panic!("no enumerator {name}"))
}

#[test]
fn the_header_reader_reads_each_form_and_refuses_a_missing_name() {
    let text = "#define A 7\n#define B\t(1 << 3)\n#define C (1ULL << 40)\n\
                enum e {\n\tD\t\t= 9,\n\tF = 2,  /* no reply */\n};\n";
    assert_eq!(define(text, "A"), 7);
    assert_eq!(define(text, "B"), 8);
    assert_eq!(define(text, "C"), 1 << 40);
    assert_eq!(enumerator(text, "D"), 9);
    assert_eq!(enumerator(text, "F"), 2);
    assert!(std::panic::catch_unwind(|| define(text, "E")).is_err());
    assert!(std::panic::catch_unwind(|| enumerator(text, "E")).is_err());
}

#[test]
fn every_opcode_value_is_the_headers() {
    let names = [
        (Opcode::Lookup, "FUSE_LOOKUP"),
        (Opcode::Forget, "FUSE_FORGET"),
        (Opcode::Getattr, "FUSE_GETATTR"),
        (Opcode::Setattr, "FUSE_SETATTR"),
        (Opcode::Readlink, "FUSE_READLINK"),
        (Opcode::Mkdir, "FUSE_MKDIR"),
        (Opcode::Unlink, "FUSE_UNLINK"),
        (Opcode::Rename, "FUSE_RENAME"),
        (Opcode::Open, "FUSE_OPEN"),
        (Opcode::Read, "FUSE_READ"),
        (Opcode::Write, "FUSE_WRITE"),
        (Opcode::Statfs, "FUSE_STATFS"),
        (Opcode::Release, "FUSE_RELEASE"),
        (Opcode::Fsync, "FUSE_FSYNC"),
        (Opcode::Flush, "FUSE_FLUSH"),
        (Opcode::Init, "FUSE_INIT"),
        (Opcode::Opendir, "FUSE_OPENDIR"),
        (Opcode::Readdir, "FUSE_READDIR"),
        (Opcode::Releasedir, "FUSE_RELEASEDIR"),
        (Opcode::Create, "FUSE_CREATE"),
        (Opcode::Interrupt, "FUSE_INTERRUPT"),
        (Opcode::Destroy, "FUSE_DESTROY"),
        (Opcode::BatchForget, "FUSE_BATCH_FORGET"),
        (Opcode::Readdirplus, "FUSE_READDIRPLUS"),
    ];
    for (op, name) in names {
        assert_eq!(op.code(), enumerator(HEADER, name), "{name}");
    }
}

#[test]
fn every_flag_and_constant_is_the_headers() {
    let flags = [
        (InitFlags::ASYNC_READ, "FUSE_ASYNC_READ"),
        (InitFlags::SPLICE_WRITE, "FUSE_SPLICE_WRITE"),
        (InitFlags::SPLICE_READ, "FUSE_SPLICE_READ"),
        (InitFlags::DO_READDIRPLUS, "FUSE_DO_READDIRPLUS"),
        (InitFlags::READDIRPLUS_AUTO, "FUSE_READDIRPLUS_AUTO"),
        (InitFlags::NO_OPEN_SUPPORT, "FUSE_NO_OPEN_SUPPORT"),
        (InitFlags::PARALLEL_DIROPS, "FUSE_PARALLEL_DIROPS"),
        (InitFlags::MAX_PAGES, "FUSE_MAX_PAGES"),
        (InitFlags::CACHE_SYMLINKS, "FUSE_CACHE_SYMLINKS"),
        (InitFlags::NO_OPENDIR_SUPPORT, "FUSE_NO_OPENDIR_SUPPORT"),
        (InitFlags::INIT_EXT, "FUSE_INIT_EXT"),
        (InitFlags::HAS_RESEND, "FUSE_HAS_RESEND"),
        (InitFlags::OVER_IO_URING, "FUSE_OVER_IO_URING"),
    ];
    for (flag, name) in flags {
        assert_eq!(flag.0, define(HEADER, name), "{name}");
    }
    let bits = [
        (u64::from(abi::fopen::KEEP_CACHE), "FOPEN_KEEP_CACHE"),
        (u64::from(abi::fattr::SIZE), "FATTR_SIZE"),
        (u64::from(abi::fattr::ATIME), "FATTR_ATIME"),
        (u64::from(abi::fattr::MTIME), "FATTR_MTIME"),
        (u64::from(abi::fattr::FH), "FATTR_FH"),
        (u64::from(abi::fattr::ATIME_NOW), "FATTR_ATIME_NOW"),
        (u64::from(abi::fattr::MTIME_NOW), "FATTR_MTIME_NOW"),
        (u64::from(abi::fattr::CTIME), "FATTR_CTIME"),
        (abi::ROOT_ID, "FUSE_ROOT_ID"),
        (abi::UNIQUE_RESEND, "FUSE_UNIQUE_RESEND"),
        (abi::MIN_READ_BUFFER as u64, "FUSE_MIN_READ_BUFFER"),
        (u64::from(abi::KERNEL_VERSION), "FUSE_KERNEL_VERSION"),
        (
            u64::from(abi::KERNEL_MINOR_VERSION),
            "FUSE_KERNEL_MINOR_VERSION",
        ),
    ];
    for (value, name) in bits {
        assert_eq!(value, define(HEADER, name), "{name}");
    }
    // The out header the frozen `encode_out_header` writes is `fuse_out_header`, and the in
    // header `RequestHeader::parse` reads is `fuse_in_header`.
    assert_eq!(tsfuse::OUT_HEADER_LEN, OutHeader::SIZE);
    assert_eq!(InHeader::SIZE, 40);
    assert_eq!(abi::INIT_IN_COMPAT_SIZE, offset_of!(InitIn, flags2));
}
