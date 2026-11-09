use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::*;
use crate::testdir::TestDir;

const TOOL: &[u8] = b"\x7fELF a pinned tool's bytes";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-fetch-unit"), name)
}

/// A tar of a directory `tool-1.0`, a decoy file, `tool-1.0/tool` holding `TOOL` and a
/// symlink `tool-1.0/link`.
fn tar() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    let mut dir = tar::Header::new_gnu();
    dir.set_entry_type(tar::EntryType::Directory);
    dir.set_mode(0o755);
    dir.set_size(0);
    b.append_data(&mut dir, "tool-1.0/", &[][..]).expect("dir");
    for (path, data) in [("tool-1.0/README", &b"decoy"[..]), ("tool-1.0/tool", TOOL)] {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(data.len() as u64);
        b.append_data(&mut h, path, data).expect("file");
    }
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    b.append_link(&mut link, "tool-1.0/link", "tool")
        .expect("link");
    b.into_inner().expect("tar")
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(bytes).expect("gzip");
    e.finish().expect("gzip")
}

fn xz(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    lzma_rs::xz_compress(&mut &bytes[..], &mut out).expect("xz");
    out
}

fn member(path: &str) -> PathBuf {
    PathBuf::from(path)
}

fn tar_gz(path: &str) -> Unpack {
    Unpack::Tar {
        xz: false,
        member: member(path),
    }
}

/// `install` of `bytes` (pinned at their own digest) into a fresh `out`.
/// The caller holds the directory (the last element) while it reads `out`.
fn install_own(name: &str, bytes: &[u8], unpack: &Unpack) -> (Result<(), Error>, PathBuf, TestDir) {
    let own = scratch(name);
    let out = own.join("bin").join("tool");
    (install(bytes, &Sha256::of(bytes), unpack, &out), out, own)
}

#[test]
fn a_sha256_is_64_lowercase_hex_digits() {
    // Catches: a parser that accepts a short, upper-case or non-hex pin (a typo then
    // pins nothing), or that refuses the real form.
    let hex = "9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f";
    let digest: Sha256 = hex.parse().expect("parse");
    assert_eq!(digest.to_string(), hex);
    assert_eq!(format!("{digest:?}"), hex);
    for bad in [
        &hex[..62],
        &hex.to_uppercase(),
        &format!("{}g", &hex[..63]),
        "",
    ] {
        assert_eq!(
            bad.parse::<Sha256>(),
            Err(NotASha256(bad.to_owned())),
            "{bad}"
        );
    }
    assert_eq!(
        NotASha256("x".to_owned()).to_string(),
        "not a SHA-256: \"x\" (expected 64 lowercase hex digits)"
    );
}

#[test]
fn a_wrong_hash_is_refused_before_anything_is_unpacked_or_written() {
    // Catches: skipping the compare (the archive below unpacks fine, so the tool would
    // be installed), or comparing after unpacking (the second case is no archive at
    // all, so a late compare reports an unpack error instead of the mismatch).
    let dir = scratch("wrong-hash");
    let out = dir.join("tool");
    let archive = gzip(&tar());
    let pinned = Sha256::of(b"the bytes the pin was made from");
    for (bytes, unpack) in [
        (&archive[..], tar_gz("tool-1.0/tool")),
        (b"not gzip", tar_gz("x")),
    ] {
        assert_eq!(
            install(bytes, &pinned, &unpack, &out),
            Err(Error::Mismatch {
                expected: pinned,
                actual: Sha256::of(bytes)
            })
        );
        assert!(!out.exists(), "nothing at --out");
        assert!(!part_path(&out).exists(), "no part file");
    }
    assert_eq!(
        std::fs::read_dir(&dir).expect("ls").count(),
        0,
        "nothing written"
    );
}

#[test]
fn a_mismatch_names_both_digests() {
    // Catches: a message that leaves out the digest a maintainer needs to update the pin.
    let e = Error::Mismatch {
        expected: Sha256([0; 32]),
        actual: Sha256([0xff; 32]),
    };
    let text = e.to_string();
    assert!(
        text.contains(&"ff".repeat(32)) && text.contains(&"00".repeat(32)),
        "{text}"
    );
}

#[test]
fn a_matching_raw_download_is_installed_executable() {
    // Catches: a raw download altered on the way to disk, a file left without the
    // execute bits (the job's next step runs it), or a part file left beside it.
    let (result, out, _own) = install_own("raw", TOOL, &Unpack::Raw);
    result.expect("install");
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
    let mode = std::fs::metadata(&out).expect("stat").permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(!part_path(&out).exists());
}

#[test]
fn each_format_yields_the_tool() {
    // Catches: a format decoded with the wrong codec, or a tar member picked by the wrong
    // name (the decoy README comes first).
    let xz_tar = Unpack::Tar {
        xz: true,
        member: member("tool-1.0/tool"),
    };
    for (name, bytes, unpack) in [
        ("tar-gz", gzip(&tar()), tar_gz("tool-1.0/tool")),
        ("tar-xz", xz(&tar()), xz_tar),
    ] {
        let (result, out, _own) = install_own(name, &bytes, &unpack);
        result.expect(name);
        assert_eq!(std::fs::read(&out).expect("read"), TOOL, "{name}");
    }
}

#[test]
fn an_archive_that_does_not_decode_is_an_unpack_error_with_nothing_written() {
    // Catches: a decode failure that installs the raw bytes (or an empty file) instead
    // of failing; one case per decoder, and a tar whose header is not a tar's.
    let not_tar = [1u8; 1024];
    let xz_garbage = Unpack::Tar {
        xz: true,
        member: member("x"),
    };
    for (name, bytes, unpack) in [
        ("gz", b"not gzip".to_vec(), tar_gz("x")),
        ("xz", b"not xz".to_vec(), xz_garbage),
        ("tar", gzip(&not_tar), tar_gz("x")),
    ] {
        // Not `name` alone: each_format_yields_the_tool installs under other names, in the
        // same process.
        let (result, out, _own) = install_own(&format!("undecodable-{name}"), &bytes, &unpack);
        assert!(
            matches!(result, Err(Error::Unpack(_))),
            "{name}: {result:?}"
        );
        assert!(!out.exists() && !part_path(&out).exists(), "{name}");
    }
}

#[test]
fn a_missing_member_or_one_that_is_not_a_regular_file_is_refused() {
    // Catches: an installer that writes an empty file for a member the archive lacks
    // (a renamed directory upstream), or for a symlink or directory entry.
    let archive = gzip(&tar());
    let cases = [
        (
            "tool-1.0/cargo-deny",
            Error::NoMember(member("tool-1.0/cargo-deny")),
        ),
        ("tool-1.0/link", Error::NotAFile(member("tool-1.0/link"))),
        ("tool-1.0", Error::NotAFile(member("tool-1.0"))),
    ];
    for (path, expected) in cases {
        let (result, out, _own) = install_own(path, &archive, &tar_gz(path));
        assert_eq!(result, Err(expected));
        assert!(!out.exists() && !part_path(&out).exists(), "{path}");
    }
    assert_eq!(
        Error::NoMember(member("a/b")).to_string(),
        "the archive has no \"a/b\""
    );
}

#[test]
fn an_install_that_cannot_rename_leaves_no_part_file() {
    // Catches: a part file left behind when the last step fails (the next run, or a
    // reader of the directory, then sees a half-installed tool): --out is a directory,
    // so the rename over it fails after the part file was written.
    let _out_own = scratch("rename-fails");
    let out = _out_own.join("tool");
    std::fs::create_dir_all(out.join("inside")).expect("dir");
    let result = install(TOOL, &Sha256::of(TOOL), &Unpack::Raw, &out);
    let Err(Error::Install { path, why }) = result else {
        panic!("{result:?}");
    };
    assert_eq!(path, out);
    assert!(!why.is_empty());
    assert!(!part_path(&out).exists(), "part file removed");
    assert!(out.join("inside").is_dir(), "the old --out is untouched");
}

#[test]
fn an_install_replaces_an_old_tool_and_a_stale_part_file() {
    // Catches: refusing to replace an existing --out (a cached older version), or
    // failing on a part file a killed run left.
    let _out_own = scratch("replace");
    let out = _out_own.join("tool");
    std::fs::write(&out, b"old").expect("old");
    std::fs::write(part_path(&out), b"half").expect("stale part");
    install(TOOL, &Sha256::of(TOOL), &Unpack::Raw, &out).expect("install");
    assert_eq!(std::fs::read(&out).expect("read"), TOOL);
    assert!(!part_path(&out).exists());
}

#[test]
fn extract_and_member_must_agree() {
    // Catches: a tar format accepted without the member to take (what would it
    // install?), or a member silently ignored for a format that has none.
    let m = || Some(member("d/tool"));
    assert_eq!(Unpack::new(None, None, false), Ok(Unpack::Raw));
    assert_eq!(
        Unpack::new(Some(Format::TarGz), m(), false),
        Ok(tar_gz("d/tool"))
    );
    assert_eq!(
        Unpack::new(Some(Format::TarXz), m(), false),
        Ok(Unpack::Tar {
            xz: true,
            member: member("d/tool")
        })
    );
    for format in [Format::TarGz, Format::TarXz] {
        let e = Unpack::new(Some(format), None, false).expect_err("no member");
        assert!(e.contains("need --member"), "{e}");
    }
    let e = Unpack::new(None, m(), false).expect_err("member out of place");
    assert!(e.contains("needs --extract tar.gz or tar.xz"), "{e}");
    assert_eq!(
        Unpack::new(Some(Format::TarGz), None, true),
        Ok(Unpack::Tree { xz: false })
    );
    assert_eq!(
        Unpack::new(Some(Format::TarXz), None, true),
        Ok(Unpack::Tree { xz: true })
    );
    let e = Unpack::new(Some(Format::TarXz), m(), true).expect_err("tree and member");
    assert!(e.contains("takes no --member"), "{e}");
    let e = Unpack::new(None, m(), true).expect_err("tree and member, no format");
    assert!(e.contains("takes no --member"), "{e}");
    let e = Unpack::new(None, None, true).expect_err("tree without a tar");
    assert!(e.contains("needs --extract tar.gz or tar.xz"), "{e}");
}

#[test]
fn the_part_file_sits_beside_out() {
    // Catches: a part file in another directory (the rename then crosses file systems,
    // and is no longer atomic).
    assert_eq!(
        part_path(Path::new("/a/b/tool")),
        Path::new("/a/b/tool.part")
    );
}

/// A server on the loopback address that answers its one request with `status` and `body`; the
/// URL to ask it. (Plain HTTP: `download_from` is told not to insist on HTTPS here.)
fn serve_once(status: &'static str, body: &'static [u8]) -> String {
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/tool", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().expect("clone"));
        let mut line = String::new();
        while reader.read_line(&mut line).expect("request") > 2 {
            line.clear();
        }
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).expect("head");
        stream.write_all(body).expect("body");
    });
    url
}

#[test]
fn a_download_returns_the_body_and_a_status_error_is_a_download_error() {
    // Catches: a body cut short or not read, and an HTTP error page taken for the file.
    let url = serve_once("200 OK", TOOL);
    assert_eq!(download_from(&url, &[], false).expect("download"), TOOL);
    let url = serve_once("404 Not Found", b"missing");
    let e = download_from(&url, &[], false).expect_err("404");
    assert!(
        matches!(&e, Error::Download { url: u, .. } if *u == url),
        "{e}"
    );
}

/// A server on the loopback address that answers every request with `200 OK` and records the
/// first line of each. `requests` ends it: it sends one request of its own, which the server
/// recognises and stops at, and returns what the server was asked before it, so a test knows
/// without waiting that a refused download connected to nothing.
struct Watched {
    url: String,
    addr: std::net::SocketAddr,
    thread: std::thread::JoinHandle<Vec<String>>,
}

fn watched() -> Watched {
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let thread = std::thread::spawn(move || {
        let mut asked = Vec::new();
        loop {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut first = String::new();
            reader.read_line(&mut first).expect("request line");
            let mut line = String::new();
            while reader.read_line(&mut line).expect("header") > 2 {
                line.clear();
            }
            if first.starts_with("SENTINEL") {
                return asked;
            }
            asked.push(first.trim_end().to_owned());
            let head = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
            stream.write_all(head.as_bytes()).expect("reply");
        }
    });
    Watched {
        url: format!("http://{addr}/tool"),
        addr,
        thread,
    }
}

impl Watched {
    /// What the server was asked.
    fn requests(self) -> Vec<String> {
        let mut stream = std::net::TcpStream::connect(self.addr).expect("connect");
        stream.write_all(b"SENTINEL\r\n\r\n").expect("sentinel");
        self.thread.join().expect("server")
    }
}

#[test]
fn a_download_over_plain_http_is_refused_before_any_connection() {
    // Catches: `download` fetching a tool over a connection anyone can alter. The URL is a live
    // server's, so a download that went ahead would connect, and the refusal must name the
    // rule (an unreachable address would fail whatever the rule was).
    let server = watched();
    let e = download(&server.url, &[]).expect_err("http");
    assert_eq!(
        e,
        Error::Download {
            url: server.url.clone(),
            why: PLAIN_HTTP.to_owned()
        }
    );
    assert_eq!(server.requests(), Vec::<String>::new());
}

#[test]
fn fetch_downloads_checks_and_installs_and_prints_what_it_did() {
    // Catches: a fetch that installs without the check, or prints nothing a log can show.
    let dir = scratch("fetch-ok");
    let out = dir.join("bin").join("tool");
    let line = fetch_with(
        "https://example.invalid/tool",
        &Sha256::of(TOOL),
        &Unpack::new(None, None, false).expect("unpack"),
        &out,
        |url| {
            assert_eq!(url, "https://example.invalid/tool");
            Ok(TOOL.to_vec())
        },
    )
    .expect("fetch");
    assert!(
        line.starts_with("ok: https://example.invalid/tool has SHA-256 "),
        "{line}"
    );
    assert_eq!(std::fs::read(&out).expect("tool"), TOOL);

    let e = fetch_with(
        "https://example.invalid/tool",
        &Sha256::of(b"another file"),
        &Unpack::new(None, None, false).expect("unpack"),
        &dir.join("never"),
        |_| Ok(TOOL.to_vec()),
    )
    .expect_err("mismatch");
    assert!(matches!(e, Error::Mismatch { .. }), "{e}");
    assert!(!dir.join("never").exists());

    let e = fetch_with(
        "https://example.invalid/tool",
        &Sha256::of(TOOL),
        &Unpack::new(None, None, false).expect("unpack"),
        &dir.join("never"),
        |url| {
            Err(Error::Download {
                url: url.to_owned(),
                why: "down".into(),
            })
        },
    )
    .expect_err("download");
    assert!(matches!(e, Error::Download { .. }), "{e}");
}

#[test]
fn fetch_over_plain_http_is_refused_before_any_connection_and_installs_nothing() {
    // Catches: `fetch` going around `download`'s rule, and the host's roots building no client
    // (the roots may be none on a machine with no store).
    let dir = scratch("fetch-http");
    let roots = host_roots();
    let _ = tls_config(&roots);
    let server = watched();
    let out = dir.join("never");
    let e = fetch(
        &server.url,
        &Sha256::of(TOOL),
        &Unpack::new(None, None, false).expect("unpack"),
        &out,
        &roots,
    )
    .expect_err("http");
    assert!(
        matches!(&e, Error::Download { why, .. } if why == PLAIN_HTTP),
        "{e}"
    );
    assert_eq!(server.requests(), Vec::<String>::new());
    assert!(!out.exists());
}

#[test]
fn an_https_url_is_connected_to_and_a_refused_connection_is_a_download_error() {
    // Catches: the HTTPS rule refusing an `https://` URL, which `download` is for. A port nothing
    // listens on refuses the connection, so the error is the transfer's, not the rule's.
    let addr = tierstore_testkit::port::dead_addr();
    let url = format!("https://{addr}/tool");
    let e = download(&url, &[]).expect_err("closed port");
    assert!(
        matches!(&e, Error::Download { why, .. } if why != PLAIN_HTTP),
        "{e}"
    );
}

/// A tar of the directory `zig-1.0` as a toolchain archive has it: a plain file, an executable,
/// a file in a nested directory and an empty directory.
fn tree_tar() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for dir in ["zig-1.0/", "zig-1.0/lib/", "zig-1.0/lib/empty/"] {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o755);
        h.set_size(0);
        b.append_data(&mut h, dir, &[][..]).expect("dir");
    }
    for (path, mode, data) in [
        ("zig-1.0/LICENSE", 0o644, &b"licence"[..]),
        ("zig-1.0/zig", 0o755, TOOL),
        ("zig-1.0/lib/std.zig", 0o600, &b"std"[..]),
    ] {
        let mut h = tar::Header::new_gnu();
        h.set_mode(mode);
        h.set_size(data.len() as u64);
        b.append_data(&mut h, path, data).expect("file");
    }
    b.into_inner().expect("tar")
}

/// A one-entry tar whose path is written into the header as is (the builder refuses `..`).
fn raw_path_tar(path: &str, kind: tar::EntryType) -> Vec<u8> {
    let mut h = tar::Header::new_gnu();
    h.as_old_mut().name[..path.len()].copy_from_slice(path.as_bytes());
    h.set_entry_type(kind);
    h.set_mode(0o644);
    h.set_size(0);
    h.set_cksum();
    let mut tar = h.as_bytes().to_vec();
    tar.resize(tar.len() + 1024, 0);
    tar
}

fn file_mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn a_tree_is_installed_as_a_directory_without_the_archives_top_directory() {
    // Catches: a tree install that keeps the `zig-1.0/` prefix (the tool is then not at
    // `<out>/zig`), loses the executable bit of the tool, makes the plain files executable,
    // or drops an empty directory or a nested file. One case per decoder.
    for (name, bytes, xz) in [
        ("tree-gz", gzip(&tree_tar()), false),
        ("tree-xz", xz(&tree_tar()), true),
    ] {
        let own = scratch(name);
        let out = own.join("opt").join("zig");
        install(&bytes, &Sha256::of(&bytes), &Unpack::Tree { xz }, &out).expect(name);
        assert_eq!(std::fs::read(out.join("zig")).expect("zig"), TOOL, "{name}");
        assert_eq!(file_mode(&out.join("zig")), 0o755, "{name}");
        assert_eq!(file_mode(&out.join("LICENSE")), 0o644, "{name}");
        assert_eq!(
            file_mode(&out.join("lib/std.zig")),
            0o644,
            "{name}: 0600 becomes 0644"
        );
        assert!(out.join("lib/empty").is_dir(), "{name}");
        assert!(!out.join("zig-1.0").exists(), "{name}");
        assert!(!part_path(&out).exists(), "{name}");
    }
}

#[test]
fn a_tree_replaces_an_old_tree_and_a_stale_part_tree() {
    // Catches: a new tree merged over the old one (a file the new archive lacks survives), or
    // a killed run's part tree that leaks files into the new one.
    let own = scratch("tree-replace");
    let out = own.join("zig");
    std::fs::create_dir_all(&out).expect("old tree");
    std::fs::write(out.join("old-only"), b"old").expect("old file");
    std::fs::create_dir_all(part_path(&out)).expect("stale part");
    std::fs::write(part_path(&out).join("half"), b"half").expect("stale file");
    let bytes = gzip(&tree_tar());
    install(
        &bytes,
        &Sha256::of(&bytes),
        &Unpack::Tree { xz: false },
        &out,
    )
    .expect("install");
    assert!(!out.join("old-only").exists());
    assert!(!out.join("half").exists());
    assert!(out.join("zig").is_file());
    assert!(!part_path(&out).exists());
}

#[test]
fn a_tree_with_a_wrong_hash_writes_nothing() {
    // Catches: a tree unpacked before the digest is compared.
    let own = scratch("tree-mismatch");
    let out = own.join("zig");
    let bytes = gzip(&tree_tar());
    let result = install(
        &bytes,
        &Sha256::of(b"other"),
        &Unpack::Tree { xz: false },
        &out,
    );
    assert!(matches!(result, Err(Error::Mismatch { .. })), "{result:?}");
    assert!(!out.exists() && !part_path(&out).exists());
}

#[test]
fn a_tree_archive_with_an_unsafe_or_unusual_entry_is_refused_and_leaves_no_part_tree() {
    // Catches: a path that climbs out of the tree (`../escape`) or is absolute written outside
    // --out, a symlink or device entry unpacked into a tool tree, and an undecodable archive
    // that installs an empty tree.
    let cases = [
        (
            "dotdot",
            gzip(&raw_path_tar("top/../../escape", tar::EntryType::Regular)),
        ),
        (
            "absolute",
            gzip(&raw_path_tar("/etc/escape", tar::EntryType::Regular)),
        ),
        (
            "symlink",
            gzip(&raw_path_tar("top/link", tar::EntryType::Symlink)),
        ),
        ("gz", b"not gzip".to_vec()),
        ("tar", gzip(&[1u8; 1024])),
    ];
    for (name, bytes) in cases {
        let own = scratch(&format!("tree-refused-{name}"));
        let out = own.join("zig");
        let result = install(
            &bytes,
            &Sha256::of(&bytes),
            &Unpack::Tree { xz: false },
            &out,
        );
        assert!(
            matches!(result, Err(Error::Unpack(_))),
            "{name}: {result:?}"
        );
        assert!(!out.exists() && !part_path(&out).exists(), "{name}");
        assert!(!own.join("escape").exists(), "{name}");
    }
}

#[test]
fn a_tree_that_cannot_be_written_is_an_install_error_and_leaves_no_part_tree() {
    // Catches: an io failure reported as an unpack error or swallowed. Four places fail here:
    // the part tree's parent is a file, a directory entry where a file of the same path was
    // written, a file below a path that is a file, and an --out that is a file.
    let own = scratch("tree-io");
    let bytes = gzip(&tree_tar());
    let unpack = Unpack::Tree { xz: false };
    let blocker = own.join("blocker");
    std::fs::write(&blocker, b"file").expect("blocker");
    let out = blocker.join("zig");
    let result = install(&bytes, &Sha256::of(&bytes), &unpack, &out);
    assert!(matches!(result, Err(Error::Install { .. })), "{result:?}");

    // A directory entry where a regular file of the same path was written before.
    let clash = {
        let mut b = tar::Builder::new(Vec::new());
        for (path, kind) in [
            ("t/lib", tar::EntryType::Regular),
            ("t/lib/", tar::EntryType::Directory),
        ] {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_mode(0o644);
            h.set_size(0);
            b.append_data(&mut h, path, &[][..]).expect("entry");
        }
        gzip(&b.into_inner().expect("tar"))
    };
    let out = own.join("clash");
    let result = install(&clash, &Sha256::of(&clash), &unpack, &out);
    assert!(matches!(result, Err(Error::Install { .. })), "{result:?}");
    assert!(!out.exists() && !part_path(&out).exists());

    // A file below a path that is a file.
    let below = {
        let mut b = tar::Builder::new(Vec::new());
        for path in ["t/a", "t/a/b"] {
            let mut h = tar::Header::new_gnu();
            h.set_mode(0o644);
            h.set_size(0);
            b.append_data(&mut h, path, &[][..]).expect("entry");
        }
        gzip(&b.into_inner().expect("tar"))
    };
    let out = own.join("below");
    let result = install(&below, &Sha256::of(&below), &unpack, &out);
    assert!(matches!(result, Err(Error::Install { .. })), "{result:?}");
    assert!(!part_path(&out).exists());

    // An --out that is a file: it is not a tree that can be replaced.
    let out = own.join("a-file");
    std::fs::write(&out, b"file").expect("file");
    let result = install(&bytes, &Sha256::of(&bytes), &unpack, &out);
    assert!(matches!(result, Err(Error::Install { .. })), "{result:?}");
    assert_eq!(std::fs::read(&out).expect("kept"), b"file");
    assert!(!part_path(&out).exists());
}
