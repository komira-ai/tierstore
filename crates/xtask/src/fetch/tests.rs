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
    assert_eq!(Unpack::new(None, None), Ok(Unpack::Raw));
    assert_eq!(Unpack::new(Some(Format::TarGz), m()), Ok(tar_gz("d/tool")));
    assert_eq!(
        Unpack::new(Some(Format::TarXz), m()),
        Ok(Unpack::Tar {
            xz: true,
            member: member("d/tool")
        })
    );
    for format in [Format::TarGz, Format::TarXz] {
        let e = Unpack::new(Some(format), None).expect_err("no member");
        assert!(e.contains("need --member"), "{e}");
    }
    let e = Unpack::new(None, m()).expect_err("member out of place");
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

/// A listener on the loopback address that nobody serves, and the plain-HTTP URL of it. A
/// connection made to it waits in the kernel's queue until `accept`, so `nothing_connected`
/// can tell, without waiting, whether anything connected.
fn watched() -> (std::net::TcpListener, String) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}/tool", listener.local_addr().expect("addr"));
    (listener, url)
}

fn nothing_connected(listener: &std::net::TcpListener) -> bool {
    listener.set_nonblocking(true).expect("nonblocking");
    matches!(
        listener.accept(),
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
    )
}

#[test]
fn a_download_over_plain_http_is_refused_before_any_connection() {
    // Catches: `download` fetching a tool over a connection anyone can alter. The URL is a live
    // server's, so a download that went ahead would connect, and the refusal must name the
    // rule (an unreachable address would fail whatever the rule was).
    let (listener, url) = watched();
    let e = download(&url, &[]).expect_err("http");
    assert_eq!(
        e,
        Error::Download {
            url: url.clone(),
            why: PLAIN_HTTP.to_owned()
        }
    );
    assert!(
        nothing_connected(&listener),
        "the refused download connected"
    );
}

#[test]
fn fetch_downloads_checks_and_installs_and_prints_what_it_did() {
    // Catches: a fetch that installs without the check, or prints nothing a log can show.
    let dir = scratch("fetch-ok");
    let out = dir.join("bin").join("tool");
    let line = fetch_with(
        "https://example.invalid/tool",
        &Sha256::of(TOOL),
        &Unpack::new(None, None).expect("unpack"),
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
        &Unpack::new(None, None).expect("unpack"),
        &dir.join("never"),
        |_| Ok(TOOL.to_vec()),
    )
    .expect_err("mismatch");
    assert!(matches!(e, Error::Mismatch { .. }), "{e}");
    assert!(!dir.join("never").exists());

    let e = fetch_with(
        "https://example.invalid/tool",
        &Sha256::of(TOOL),
        &Unpack::new(None, None).expect("unpack"),
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
    let (listener, url) = watched();
    let out = dir.join("never");
    let e = fetch(
        &url,
        &Sha256::of(TOOL),
        &Unpack::new(None, None).expect("unpack"),
        &out,
        &roots,
    )
    .expect_err("http");
    assert!(
        matches!(&e, Error::Download { why, .. } if why == PLAIN_HTTP),
        "{e}"
    );
    assert!(nothing_connected(&listener), "the refused fetch connected");
    assert!(!out.exists());
}
