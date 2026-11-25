//! [`fetch_cached`] and [`first_pinned`] against a fake transport: what a cache hit, a cache miss
//! and every way a miss can fail install, write, download and wait for. Pauses are recorded, not
//! slept, and nothing is timed.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::tests::{TOOL, file_mode, tree_tar, xz};
use super::*;
use crate::testdir::TestDir;

/// The first URL tried: a community mirror.
const ONE: &str = "https://mirror.example/zig/zig-x86_64-linux-1.0.tar.xz";
/// The second: the project's own download host.
const TWO: &str = "https://origin.example/download/1.0/zig-x86_64-linux-1.0.tar.xz";

/// An empty directory of its own for one test, removed when it drops.
fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    let deps = beside.parent().expect("deps");
    TestDir::under(deps.join("xtask-fetch-cache-unit"), name)
}

/// A fake transport: answers each URL from `answers` (the first whose URL is that, a refused
/// connection for any other), and records every URL asked for and every pause.
struct Transport {
    answers: Vec<(String, Result<Vec<u8>, String>)>,
    asked: RefCell<Vec<String>>,
    paused: RefCell<Vec<Duration>>,
}

impl Transport {
    fn new(answers: &[(&str, Result<&[u8], &str>)]) -> Transport {
        Transport {
            answers: answers
                .iter()
                .map(|(url, answer)| {
                    let answer = answer.map(<[u8]>::to_vec).map_err(str::to_owned);
                    ((*url).to_owned(), answer)
                })
                .collect(),
            asked: RefCell::new(Vec::new()),
            paused: RefCell::new(Vec::new()),
        }
    }

    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        self.asked.borrow_mut().push(url.to_owned());
        self.answers
            .iter()
            .find(|(u, _)| u == url)
            .map_or(Err(format!("connection refused by {url}")), |(_, a)| {
                a.clone()
            })
    }

    /// [`fetch_cached`] of `ONE` then `TWO`, pinned to `sha256`, unpacked as `unpack`, into
    /// `out`, through `cache`.
    fn fetch_pinned(
        &self,
        sha256: &Sha256,
        out: &Path,
        cache: &Path,
        unpack: &Unpack,
    ) -> Result<String, Error> {
        let urls = [ONE.to_owned(), TWO.to_owned()];
        let req = Cached {
            urls: &urls,
            sha256,
            unpack,
            out,
            cache,
        };
        fetch_cached(&req, &|url| self.get(url), &|d| {
            self.paused.borrow_mut().push(d)
        })
    }

    /// [`Transport::fetch_pinned`] of `TOOL`, as is.
    fn fetch(&self, out: &Path, cache: &Path) -> Result<String, Error> {
        self.fetch_pinned(&Sha256::of(TOOL), out, cache, &Unpack::Raw)
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }

    fn paused(&self) -> Vec<Duration> {
        self.paused.borrow().clone()
    }
}

/// `out` and `cache` paths in `dir`.
fn paths(dir: &TestDir) -> (PathBuf, PathBuf) {
    (
        dir.join("bin").join("zig"),
        dir.join("cache").join("zig.tar.xz"),
    )
}

#[test]
fn a_cache_hit_installs_the_cached_download_without_the_network() {
    // Asserts: a cache file holding the pinned bytes is installed (0755) with no download and
    // no pause, the line says the cache hit, and the cache file is left as it was.
    let dir = scratch("hit");
    let (out, cache) = paths(&dir);
    std::fs::create_dir_all(cache.parent().expect("parent")).expect("mkdir");
    std::fs::write(&cache, TOOL).expect("seed");
    let net = Transport::new(&[(ONE, Ok(b"never asked"))]);
    let line = net.fetch(&out, &cache).expect("hit");
    assert_eq!(
        line,
        format!(
            "ok: the cache hit: {} has SHA-256 {}; nothing was downloaded; installed {}",
            cache.display(),
            Sha256::of(TOOL),
            out.display()
        )
    );
    assert_eq!(net.asked(), Vec::<String>::new());
    assert_eq!(net.paused(), Vec::<Duration>::new());
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
    assert_eq!(file_mode(&out), 0o755);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
}

#[test]
fn a_cache_miss_downloads_once_keeps_the_download_and_installs_it() {
    // Asserts: on a miss the first URL's pinned bytes are kept in the cache file (0644, no part
    // file left) and installed after one round with no pause; the line says the cache missed;
    // a second fetch with that file makes no download.
    let dir = scratch("miss");
    let (out, cache) = paths(&dir);
    let net = Transport::new(&[(ONE, Ok(TOOL)), (TWO, Ok(b"never asked"))]);
    let line = net.fetch(&out, &cache).expect("miss");
    assert_eq!(
        line,
        format!(
            "ok: the cache missed: {ONE} has SHA-256 {}; kept in {}; installed {}",
            Sha256::of(TOOL),
            cache.display(),
            out.display()
        )
    );
    assert_eq!(net.asked(), [ONE]);
    assert_eq!(net.paused(), [Duration::ZERO]);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
    assert_eq!(file_mode(&cache), 0o644);
    assert!(!part_path(&cache).exists());
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
    let again = Transport::new(&[]);
    let line = again.fetch(&out, &cache).expect("hit");
    assert!(line.starts_with("ok: the cache hit: "), "{line}");
    assert_eq!(again.asked(), Vec::<String>::new());
}

#[test]
fn a_first_url_that_times_out_gives_way_to_the_next_in_the_same_round() {
    // Asserts: the failure of the CI runs this exists for. The first URL's transfer gives up
    // (`timeout: global`, as ureq says it); the second URL is asked next, in the same round with
    // no pause, and its pinned bytes are kept and installed; the line names the timed-out
    // attempt.
    let dir = scratch("timeout");
    let (out, cache) = paths(&dir);
    let net = Transport::new(&[
        (ONE, Err(&format!("download {ONE}: timeout: global"))),
        (TWO, Ok(TOOL)),
    ]);
    let line = net.fetch(&out, &cache).expect("the second URL");
    assert_eq!(
        line,
        format!(
            "ok: the cache missed: {TWO} has SHA-256 {}, after 1 failed: {ONE}: download {ONE}: \
             timeout: global; kept in {}; installed {}",
            Sha256::of(TOOL),
            cache.display(),
            out.display()
        )
    );
    assert_eq!(net.asked(), [ONE, TWO]);
    assert_eq!(net.paused(), [Duration::ZERO]);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
}

#[test]
fn a_miss_tries_each_url_in_turn_and_waits_between_rounds() {
    // Asserts: a URL that errors (its multi-line error flattened to one line) and one serving
    // other bytes are passed over round after round, with exactly the pauses of `WAITS`, until
    // the pinned bytes arrive in the last round; the line lists the 5 failed attempts in order,
    // and the bytes of other SHA-256 are never kept.
    let dir = scratch("retry");
    let (out, cache) = paths(&dir);
    let wrong: &[u8] = b"a truncated tarball";
    let rounds = RefCell::new(0);
    let urls = [ONE.to_owned(), TWO.to_owned()];
    let asked = RefCell::new(Vec::new());
    let paused = RefCell::new(Vec::new());
    let get = |url: &str| {
        asked.borrow_mut().push(url.to_owned());
        if url == ONE {
            return Err("timeout: global\r\nafter 300 s".to_owned());
        }
        *rounds.borrow_mut() += 1;
        Ok(if *rounds.borrow() < 3 { wrong } else { TOOL }.to_vec())
    };
    let req = Cached {
        urls: &urls,
        sha256: &Sha256::of(TOOL),
        unpack: &Unpack::Raw,
        out: &out,
        cache: &cache,
    };
    let line = fetch_cached(&req, &get, &|d| paused.borrow_mut().push(d)).expect("third round");
    let refused = format!("{ONE}: timeout: global  after 300 s");
    let other = format!("{TWO}: SHA-256 {}, not the pinned one", Sha256::of(wrong));
    let failed = [&refused, &other, &refused, &other, &refused].map(String::as_str);
    assert_eq!(
        line,
        format!(
            "ok: the cache missed: {TWO} has SHA-256 {}, after 5 failed: {}; kept in {}; \
             installed {}",
            Sha256::of(TOOL),
            failed.join("; "),
            cache.display(),
            out.display()
        )
    );
    assert_eq!(*asked.borrow(), [ONE, TWO, ONE, TWO, ONE, TWO]);
    assert_eq!(*paused.borrow(), WAITS);
    assert_eq!(std::fs::read(&cache).expect("cache"), TOOL);
}

#[test]
fn a_miss_no_url_serves_is_refused_with_each_attempt_and_writes_nothing() {
    // Asserts: when no URL gives the pinned bytes in any round (a refused connection, and a
    // mirror serving bytes of another SHA-256 every time), the error lists every attempt in
    // order (its full message by exact equality), after exactly the pauses of `WAITS` (0 s,
    // 10 s, 30 s); neither the cache file nor `out` is written. Pins `ATTEMPT_TIMEOUT` (300 s).
    let dir = scratch("unavailable");
    let (out, cache) = paths(&dir);
    let net = Transport::new(&[(TWO, Ok(b"not the pin"))]);
    let refused = net.fetch(&out, &cache).expect_err("refused");
    let mut attempts = Vec::new();
    for _ in WAITS {
        attempts.push(format!("{ONE}: connection refused by {ONE}"));
        attempts.push(format!(
            "{TWO}: SHA-256 {}, not the pinned one",
            Sha256::of(b"not the pin")
        ));
    }
    assert_eq!(
        refused.to_string(),
        format!(
            "the cache {cache:?} missed and no download was the pinned file: {}; nothing was \
             installed",
            attempts.join("; ")
        )
    );
    assert_eq!(
        refused,
        Error::Unavailable {
            cache: cache.clone(),
            attempts
        }
    );
    assert_eq!(net.paused(), WAITS);
    assert_eq!(
        WAITS,
        [
            Duration::ZERO,
            Duration::from_secs(10),
            Duration::from_secs(30)
        ]
    );
    assert_eq!(ATTEMPT_TIMEOUT, Duration::from_secs(300));
    assert!(!cache.exists() && !part_path(&cache).exists());
    assert!(!out.exists());
}

#[test]
fn a_cached_file_of_other_bytes_is_refused_without_a_download() {
    // Asserts: a restored cache file whose SHA-256 is not the pin is refused, its full message
    // naming both digests; no download is made, nothing is installed, and the file is left as
    // it is for a person to look at.
    let dir = scratch("cached-mismatch");
    let (out, cache) = paths(&dir);
    std::fs::create_dir_all(cache.parent().expect("parent")).expect("mkdir");
    let other: &[u8] = b"a tarball of another version";
    std::fs::write(&cache, other).expect("seed");
    let net = Transport::new(&[(ONE, Ok(TOOL))]);
    let refused = net.fetch(&out, &cache).expect_err("refused");
    assert_eq!(
        refused.to_string(),
        format!(
            "the cached {cache:?} has SHA-256 {}, not the pinned {}; nothing was installed",
            Sha256::of(other),
            Sha256::of(TOOL)
        )
    );
    assert_eq!(net.asked(), Vec::<String>::new());
    assert!(!out.exists());
    assert_eq!(std::fs::read(&cache).expect("cache"), other);
}

#[test]
fn a_cache_that_cannot_be_read_is_refused_before_a_download() {
    // Asserts: a cache path that cannot be read (a directory stands there) is `Error::Cache`,
    // naming the path, before any download; nothing is installed.
    let dir = scratch("cache-dir");
    let (out, cache) = paths(&dir);
    std::fs::create_dir_all(&cache).expect("a directory where the file goes");
    let net = Transport::new(&[(ONE, Ok(TOOL))]);
    let refused = net.fetch(&out, &cache).expect_err("refused");
    let Error::Cache { path, why } = &refused else {
        panic!("{refused:?}");
    };
    assert_eq!(path, &cache);
    assert_eq!(
        refused.to_string(),
        format!("the cache {cache:?}: {why}; nothing was installed")
    );
    assert_eq!(net.asked(), Vec::<String>::new());
    assert!(!out.exists());
}

#[test]
fn a_download_that_cannot_be_kept_installs_nothing() {
    // Asserts: a pinned download that cannot be written to the cache (a directory stands where
    // its part file goes) is `Error::Cache` naming the cache, after the one download, with
    // nothing installed.
    let dir = scratch("cache-unwritable");
    let (out, cache) = paths(&dir);
    std::fs::create_dir_all(part_path(&cache)).expect("a directory where the part file goes");
    let net = Transport::new(&[(ONE, Ok(TOOL))]);
    let refused = net.fetch(&out, &cache).expect_err("refused");
    assert!(
        matches!(&refused, Error::Cache { path, .. } if *path == cache),
        "{refused:?}"
    );
    assert_eq!(net.asked(), [ONE]);
    assert!(!cache.exists());
    assert!(!out.exists());
}

#[test]
fn a_cached_toolchain_is_installed_as_a_tree_on_a_miss_and_on_a_hit() {
    // Asserts: the form the Linux build jobs fetch Zig in (`--extract tar.xz --tree`): on a miss
    // the xz tarball is kept as downloaded and unpacked whole into `out` without its top
    // directory; on the next run's hit the same tree is installed from the cache alone.
    let dir = scratch("tree");
    let (out, cache) = paths(&dir);
    let archive = xz(&tree_tar());
    let unpack = Unpack::Tree { xz: true };
    let net = Transport::new(&[(ONE, Ok(&archive))]);
    let line = net
        .fetch_pinned(&Sha256::of(&archive), &out, &cache, &unpack)
        .expect("miss");
    assert!(line.starts_with("ok: the cache missed: "), "{line}");
    assert_eq!(std::fs::read(&cache).expect("kept"), archive);
    assert_eq!(std::fs::read(out.join("zig")).expect("zig"), TOOL);
    assert_eq!(file_mode(&out.join("zig")), 0o755);
    std::fs::remove_dir_all(&out).expect("remove the tree");
    let again = Transport::new(&[]);
    let line = again
        .fetch_pinned(&Sha256::of(&archive), &out, &cache, &unpack)
        .expect("hit");
    assert!(line.starts_with("ok: the cache hit: "), "{line}");
    assert_eq!(again.asked(), Vec::<String>::new());
    assert_eq!(std::fs::read(out.join("lib/std.zig")).expect("std"), b"std");
}

#[test]
fn a_kept_download_that_does_not_unpack_installs_nothing_and_the_next_run_hits() {
    // Asserts: an unpack error after the verified download was kept (a member the archive lacks)
    // is that error with nothing installed; the next call hits and makes no download.
    let dir = scratch("unpack");
    let (out, cache) = paths(&dir);
    let archive = xz(&tree_tar());
    let net = Transport::new(&[(ONE, Ok(&archive))]);
    let fetch = |member: &str| {
        let unpack = Unpack::Tar {
            xz: true,
            member: PathBuf::from(member),
        };
        net.fetch_pinned(&Sha256::of(&archive), &out, &cache, &unpack)
    };
    assert_eq!(
        fetch("zig-1.0/missing"),
        Err(Error::NoMember(PathBuf::from("zig-1.0/missing")))
    );
    assert_eq!(std::fs::read(&cache).expect("kept"), archive);
    assert!(!out.exists());
    let line = fetch("zig-1.0/zig").expect("hit");
    assert!(line.starts_with("ok: the cache hit: "), "{line}");
    assert_eq!(net.asked(), [ONE]);
    assert_eq!(std::fs::read(&out).expect("out"), TOOL);
}

/// The verb's flags with `urls` and `cache`, the download as is.
fn args(urls: &[&str], cache: Option<&str>) -> Args {
    Args {
        url: urls.iter().map(|u| (*u).to_owned()).collect(),
        sha256: Sha256::of(TOOL),
        out: PathBuf::from("out"),
        extract: None,
        member: None,
        tree: false,
        cache: cache.map(PathBuf::from),
    }
}

#[test]
fn several_urls_need_a_cache() {
    // Asserts: the verb's flags take `--url` more than once only with `--cache`, and the other
    // flags still reach `Unpack::new`.
    assert_eq!(args(&[ONE], None).unpack(), Ok(Unpack::Raw));
    assert_eq!(args(&[ONE, TWO], Some("c")).unpack(), Ok(Unpack::Raw));
    assert_eq!(
        args(&[ONE, TWO], None).unpack(),
        Err(
            "--url is given once without --cache: only a cached fetch tries several URLs"
                .to_owned()
        )
    );
    let tree = Args {
        extract: Some(Format::TarXz),
        tree: true,
        ..args(&[ONE, TWO], Some("c"))
    };
    assert_eq!(tree.unpack(), Ok(Unpack::Tree { xz: true }));
}

#[test]
fn the_cached_fetch_client_gives_up_on_an_attempt_sooner_than_a_plain_fetch() {
    // Asserts: each attempt of a cached fetch is bounded by `ATTEMPT_TIMEOUT`, below the
    // `DOWNLOAD_TIMEOUT` of a plain fetch, and both clients are HTTPS only. Read from the
    // clients' own configuration: no request, no clock.
    let budget = |agent: &ureq::Agent| agent.config().timeouts().global;
    let attempt = agent(&[], ATTEMPT_TIMEOUT, true);
    assert_eq!(budget(&attempt), Some(ATTEMPT_TIMEOUT));
    assert!(attempt.config().https_only());
    assert!(ATTEMPT_TIMEOUT < DOWNLOAD_TIMEOUT);
    assert_eq!(DOWNLOAD_TIMEOUT, Duration::from_secs(600));
}

#[test]
fn the_cached_fetch_transport_refuses_plain_http_and_reports_a_failure_in_one_line() {
    // Asserts: `https` refuses a plain-http URL with the rule's words before any connection (the
    // URL is a live loopback server's, which records what it is asked), and an `https://` URL
    // whose port refuses the connection is a one-line error naming the URL.
    let get = https(&[], ATTEMPT_TIMEOUT);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let url = format!("http://{}/zig.tar.xz", listener.local_addr().expect("addr"));
    assert_eq!(get(&url), Err(format!("download {url}: {PLAIN_HTTP}")));
    assert_eq!(
        listener.accept().map_err(|e| e.kind()).err(),
        Some(std::io::ErrorKind::WouldBlock),
        "the refused download connected"
    );
    let dead = format!(
        "https://{}/zig.tar.xz",
        tierstore_testkit::port::dead_addr()
    );
    let why = get(&dead).expect_err("a refused connection");
    assert!(why.starts_with(&format!("download {dead}: ")), "{why}");
    assert!(!why.contains('\n'), "{why}");
}
