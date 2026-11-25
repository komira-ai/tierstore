//! `fetch --url <https url> --sha256 <hex> --out <path> [--extract <format> [--member <path>]]
//! [--cache <file> [--url <https url>]...]`: installs a pinned tool for a CI job.
//!
//! The order is the point:
//!
//! 1. The whole download is read into memory over HTTPS (redirects followed, each one
//!    HTTPS too; the host's trusted roots; a status other than 2xx is an error).
//! 2. Its SHA-256 is compared with `--sha256`. A mismatch is refused here, before any
//!    byte is unpacked or written: nothing is created on disk.
//! 3. It is unpacked in memory: as is, or one regular file (`--member`, its path inside
//!    the archive) out of a gzip or xz tar (`--extract tar.gz`, `--extract tar.xz`), or the
//!    whole tar as a directory tree (`--tree`: `--out` is the directory, the archive's one top
//!    directory is dropped, and an entry that is not a regular file or a directory, or whose
//!    path leaves the tree, is refused), or one zstd-compressed file decompressed whole
//!    (`--extract zst`, the form buck2's releases take).
//! 4. The file is written to `<out>.part` beside `--out`, made executable (0755) and
//!    renamed over `--out`. When writing or renaming fails, `<out>.part` is removed, so
//!    `--out` holds either the whole new file or what it held before.
//!
//! With `--cache <file>` ([`fetch_cached`]), the file the actions cache restores and saves holds
//! the download as it was fetched, and steps 1 and 2 become:
//!
//! - when the file is there, it is the download: its SHA-256 is compared with `--sha256` and a
//!   mismatch is refused, the file left as it is and nothing installed. No network is used;
//! - when it is not, each `--url` is tried in turn, in [`WAITS`]`.len()` rounds with the pause
//!   of [`WAITS`] before each round, each attempt given up after [`ATTEMPT_TIMEOUT`]; the first
//!   download whose SHA-256 is the pinned one is written to the file (through a `.part` file,
//!   mode 0644) and installed. A download of other bytes is one more failed attempt. When every
//!   attempt failed, the error names each one, in order, and nothing is written.
//!
//! Without `--cache` there is one `--url` and one attempt, given up after [`DOWNLOAD_TIMEOUT`].

use std::fmt;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::CertificateDer;
use sha2::Digest;

/// What a download of a URL that is not `https://` says: no connection is made for it.
pub const PLAIN_HTTP: &str = "refused: only https:// URLs are fetched";

/// The largest download read: well above the biggest pinned tool (about 60 MB), and a
/// bound on the memory a wrong URL can take.
pub const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

/// How long one download without `--cache` may take, redirects included.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// The pause before each round of downloads of [`first_pinned`]: none before the first.
pub const WAITS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// How long one attempt of [`fetch_cached`] may take, redirects included: a stalled host gives
/// way to the next `--url` well before [`DOWNLOAD_TIMEOUT`].
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// The download of one URL: its bytes, or why it failed (one line).
pub type Get<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, String>;

/// A SHA-256 digest, written as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sha256(pub [u8; 32]);

impl Sha256 {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Sha256 {
        Sha256(sha2::Sha256::digest(bytes).into())
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A `--sha256` value that is not 64 lowercase hex digits.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a SHA-256: {0:?} (expected 64 lowercase hex digits)")]
pub struct NotASha256(String);

impl FromStr for Sha256 {
    type Err = NotASha256;

    fn from_str(s: &str) -> Result<Sha256, NotASha256> {
        let mut digest = [0; 32];
        let lowercase = s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        match hex::decode_to_slice(s, &mut digest) {
            Ok(()) if lowercase => Ok(Sha256(digest)),
            _ => Err(NotASha256(s.to_owned())),
        }
    }
}

/// The flags of `cargo xtask fetch`.
#[derive(clap::Args)]
pub struct Args {
    /// Where to download it from (HTTPS; redirects must stay on HTTPS). With `--cache`, given
    /// once per URL to try, in order.
    #[arg(long, required = true)]
    pub url: Vec<String>,
    /// The SHA-256 of the download (before unpacking), 64 lowercase hex digits.
    #[arg(long)]
    pub sha256: Sha256,
    /// The file to install (with `--tree`, the directory); its parent is created when missing.
    #[arg(long)]
    pub out: PathBuf,
    /// How the download is packed (default: it is the file).
    #[arg(long, value_enum)]
    pub extract: Option<Format>,
    /// The file to take out of a tar (`--extract tar.gz` or `tar.xz`): its path inside
    /// the archive.
    #[arg(long)]
    pub member: Option<PathBuf>,
    /// Install the whole tar (`--extract tar.gz` or `tar.xz`) as the directory `--out`,
    /// without the archive's top directory.
    #[arg(long)]
    pub tree: bool,
    /// The file the actions cache restores and saves: the download as fetched, used when it is
    /// there and its SHA-256 is the pinned one, written when it is not.
    #[arg(long)]
    pub cache: Option<PathBuf>,
}

impl Args {
    /// How the file comes out of the download ([`Unpack::new`]); `Err` also when `--url` is
    /// given more than once without `--cache`.
    pub fn unpack(&self) -> Result<Unpack, String> {
        if self.cache.is_none() && self.url.len() > 1 {
            return Err(
                "--url is given once without --cache: only a cached fetch tries several URLs"
                    .to_owned(),
            );
        }
        Unpack::new(self.extract, self.member.clone(), self.tree)
    }
}

/// The verb, with the host's roots: [`fetch_cached`] with `--cache` (each attempt given up after
/// [`ATTEMPT_TIMEOUT`], sleeping between rounds), else [`fetch`] of the one `--url`.
pub fn run(args: &Args, unpack: &Unpack) -> Result<String, Error> {
    let roots = host_roots();
    match &args.cache {
        Some(cache) => fetch_cached(
            &Cached {
                urls: &args.url,
                sha256: &args.sha256,
                unpack,
                out: &args.out,
                cache,
            },
            &https(&roots, ATTEMPT_TIMEOUT),
            &std::thread::sleep,
        ),
        // Without `--cache` there is one `--url` ([`Args::unpack`] refuses more; clap, none).
        None => fetch(&args.url.concat(), &args.sha256, unpack, &args.out, &roots),
    }
}

/// What `--extract` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// A gzip-compressed tar: the file is its `--member`, or with `--tree` the whole archive.
    #[value(name = "tar.gz")]
    TarGz,
    /// An xz-compressed tar: the file is its `--member`, or with `--tree` the whole archive.
    #[value(name = "tar.xz")]
    TarXz,
    /// One zstd-compressed file: the file is the download decompressed (no `--member`, no
    /// `--tree`).
    #[value(name = "zst")]
    Zst,
}

/// How the installed file comes out of the download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unpack {
    /// The download is the file.
    Raw,
    /// The download is a tar archive, gzip-compressed (`xz: false`) or xz-compressed;
    /// the file is the regular file at `member` in it.
    Tar { xz: bool, member: PathBuf },
    /// The download is a tar archive, gzip-compressed (`xz: false`) or xz-compressed; all of it
    /// is installed as the directory `out`, without the archive's top directory.
    Tree { xz: bool },
    /// The download is one zstd-compressed file; the file is the download decompressed.
    Zst,
}

impl Unpack {
    /// `--extract`, `--member` and `--tree` as given; `Err` names the flag that is missing or
    /// out of place.
    pub fn new(
        extract: Option<Format>,
        member: Option<PathBuf>,
        tree: bool,
    ) -> Result<Unpack, String> {
        match (extract, member, tree) {
            (None, None, false) => Ok(Unpack::Raw),
            (Some(Format::Zst), None, false) => Ok(Unpack::Zst),
            (Some(Format::Zst), _, _) => Err("--extract zst is one compressed file: it takes no \
                                              --member and no --tree"
                .to_owned()),
            (Some(Format::TarGz), Some(member), false) => Ok(Unpack::Tar { xz: false, member }),
            (Some(Format::TarXz), Some(member), false) => Ok(Unpack::Tar { xz: true, member }),
            (Some(format), None, true) => Ok(Unpack::Tree {
                xz: format == Format::TarXz,
            }),
            (_, Some(_), true) => Err("--tree installs the whole archive: it takes no \
                                       --member"
                .to_owned()),
            (None, _, true) => {
                Err("--tree is for a tar: it needs --extract tar.gz or tar.xz".to_owned())
            }
            (Some(Format::TarGz | Format::TarXz), None, false) => Err(
                "--extract tar.gz and tar.xz need --member, the file to take, or --tree".to_owned(),
            ),
            (None, Some(_), false) => Err("--member is a file in a tar: it needs --extract \
                                           tar.gz or tar.xz"
                .to_owned()),
        }
    }
}

/// The tar archive inside a gzip (`xz: false`) or xz download.
fn decompress(bytes: &[u8], xz: bool) -> Result<Vec<u8>, Error> {
    let mut tar = Vec::new();
    if xz {
        lzma_rs::xz_decompress(&mut &bytes[..], &mut tar).map_err(unpack_error)?;
    } else {
        flate2::read::MultiGzDecoder::new(bytes)
            .read_to_end(&mut tar)
            .map_err(unpack_error)?;
    }
    Ok(tar)
}

/// The file inside a zstd-compressed download.
fn decompress_zst(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(bytes)
        .map_err(|e| Error::Unpack(format!("not a zstd frame: {e}")))?;
    let mut file = Vec::new();
    decoder.read_to_end(&mut file).map_err(unpack_error)?;
    Ok(file)
}

/// Why a fetch failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("download {url}: {why}")]
    Download { url: String, why: String },
    #[error("the download's SHA-256 is {actual}, not the pinned {expected}; nothing was installed")]
    Mismatch { expected: Sha256, actual: Sha256 },
    #[error("unpack the download: {0}")]
    Unpack(String),
    #[error("the archive has no {0:?}")]
    NoMember(PathBuf),
    #[error("{0:?} in the archive is not a regular file")]
    NotAFile(PathBuf),
    #[error("install {path:?}: {why}")]
    Install { path: PathBuf, why: String },
    /// The cache file could not be read, or the download not written to it.
    #[error("the cache {path:?}: {why}; nothing was installed")]
    Cache { path: PathBuf, why: String },
    /// The cache file is not the pinned download.
    #[error(
        "the cached {path:?} has SHA-256 {actual}, not the pinned {expected}; nothing was installed"
    )]
    CachedMismatch {
        path: PathBuf,
        expected: Sha256,
        actual: Sha256,
    },
    /// The cache missed and no URL gave the pinned download in any round.
    #[error(
        "the cache {cache:?} missed and no download was the pinned file: {}; nothing was installed",
        attempts.join("; ")
    )]
    Unavailable {
        cache: PathBuf,
        /// Each attempt, in order: its URL and why it failed.
        attempts: Vec<String>,
    },
}

/// The error for an archive that could not be decoded.
fn unpack_error(e: impl fmt::Display) -> Error {
    Error::Unpack(e.to_string())
}

/// The roots the host trusts (the system store), for [`fetch`] and [`download`].
pub fn host_roots() -> Vec<CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

/// The TLS settings of a client that trusts `roots` and nothing else.
pub fn tls_config(roots: &[CertificateDer<'static>]) -> ureq::tls::TlsConfig {
    let roots = roots
        .iter()
        .map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned())
        .collect();
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(roots)))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build()
}

/// The body of `url`, over HTTPS only, trusting `roots`.
pub fn download(url: &str, roots: &[CertificateDer<'static>]) -> Result<Vec<u8>, Error> {
    download_from(url, roots, true)
}

/// [`download`], with the HTTPS-only rule as `https_only` says (the tests of the transfer
/// itself serve plain HTTP from the loopback address; `download` never does).
pub fn download_from(
    url: &str,
    roots: &[CertificateDer<'static>],
    https_only: bool,
) -> Result<Vec<u8>, Error> {
    if https_only && !url.starts_with("https://") {
        return Err(Error::Download {
            url: url.to_owned(),
            why: PLAIN_HTTP.to_owned(),
        });
    }
    download_with(&agent(roots, DOWNLOAD_TIMEOUT, https_only), url)
}

/// The client of the downloads: trusting `roots` alone, HTTPS only as `https_only` says, each
/// download (redirects included) given up after `timeout`.
pub fn agent(
    roots: &[CertificateDer<'static>],
    timeout: Duration,
    https_only: bool,
) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(https_only)
        .timeout_global(Some(timeout))
        .tls_config(tls_config(roots))
        .build()
        .into()
}

/// The body of `url` downloaded by `agent`, at most [`MAX_DOWNLOAD`] bytes; a status other than
/// 2xx is an error.
pub fn download_with(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, Error> {
    let failed = |e: ureq::Error| Error::Download {
        url: url.to_owned(),
        why: e.to_string(),
    };
    let mut response = agent.get(url).call().map_err(failed)?;
    let body = response.body_mut().with_config().limit(MAX_DOWNLOAD);
    body.read_to_vec().map_err(failed)
}

/// `bytes` when their SHA-256 is `sha256`.
fn pinned(bytes: &[u8], sha256: &Sha256) -> Result<(), Error> {
    let actual = Sha256::of(bytes);
    if actual != *sha256 {
        return Err(Error::Mismatch {
            expected: *sha256,
            actual,
        });
    }
    Ok(())
}

/// Checks `bytes` against `sha256`, unpacks them and installs the file at `out`.
pub fn install(bytes: &[u8], sha256: &Sha256, unpack: &Unpack, out: &Path) -> Result<(), Error> {
    if let Unpack::Tree { xz } = unpack {
        pinned(bytes, sha256)?;
        return install_tree(&decompress(bytes, *xz)?, out);
    }
    let file = file_of(bytes, sha256, unpack)?;
    write_executable(out, &file).map_err(|e| Error::Install {
        path: out.to_owned(),
        why: e.to_string(),
    })
}

/// Checks `bytes` against `sha256` and takes out of them the one file `unpack` names, as
/// [`install`] would install it (`mac-farm` sends it to a farm Mac instead).
///
/// # Errors
/// [`Error::Mismatch`] for bytes that are not the pin, [`Error::Unpack`] for `Unpack::Tree`
/// (a directory, not one file) or bytes that do not decode, and the errors of a missing member.
pub fn file_of(bytes: &[u8], sha256: &Sha256, unpack: &Unpack) -> Result<Vec<u8>, Error> {
    pinned(bytes, sha256)?;
    match unpack {
        Unpack::Raw => Ok(bytes.to_vec()),
        Unpack::Zst => decompress_zst(bytes),
        Unpack::Tar { xz, member } => tar_member(&decompress(bytes, *xz)?, member),
        Unpack::Tree { .. } => Err(Error::Unpack(
            "a whole archive is a directory, not one file".to_owned(),
        )),
    }
}

/// [`download`]s `url` and [`install`]s it at `out`: the line to print.
pub fn fetch(
    url: &str,
    sha256: &Sha256,
    unpack: &Unpack,
    out: &Path,
    roots: &[CertificateDer<'static>],
) -> Result<String, Error> {
    fetch_with(url, sha256, unpack, out, |url| download(url, roots))
}

/// [`fetch`] with the way the bytes are downloaded (the tests pass a stand-in).
pub fn fetch_with(
    url: &str,
    sha256: &Sha256,
    unpack: &Unpack,
    out: &Path,
    download: impl FnOnce(&str) -> Result<Vec<u8>, Error>,
) -> Result<String, Error> {
    let bytes = download(url)?;
    install(&bytes, sha256, unpack, out)?;
    Ok(format!(
        "ok: {url} has SHA-256 {sha256}; installed {}",
        out.display()
    ))
}

/// Downloads over HTTPS alone with one [`agent`] of `roots` and `timeout` for every attempt;
/// each failure is one line.
pub fn https(
    roots: &[CertificateDer<'static>],
    timeout: Duration,
) -> impl Fn(&str) -> Result<Vec<u8>, String> {
    let agent = agent(roots, timeout, true);
    move |url| {
        if !url.starts_with("https://") {
            return Err(format!("download {url}: {PLAIN_HTTP}"));
        }
        download_with(&agent, url).map_err(|e| e.to_string())
    }
}

/// The bytes of the first of `urls` whose download has SHA-256 `sha256`, round after round,
/// `pause` waiting the [`WAITS`] before each round, and where they came from:
/// `<url> has SHA-256 <hex>`, with the attempts that failed before it. `Err` is every attempt,
/// in order: its URL and why it failed (an error of `get`, or a download of other bytes).
pub fn first_pinned(
    urls: &[String],
    sha256: &Sha256,
    get: Get<'_>,
    pause: &dyn Fn(Duration),
) -> Result<(Vec<u8>, String), Vec<String>> {
    let mut attempts = Vec::new();
    for wait in WAITS {
        pause(wait);
        for url in urls {
            let failed = match get(url) {
                Ok(bytes) if Sha256::of(&bytes) == *sha256 => {
                    let mut line = format!("{url} has SHA-256 {sha256}");
                    if !attempts.is_empty() {
                        line.push_str(&format!(
                            ", after {} failed: {}",
                            attempts.len(),
                            attempts.join("; ")
                        ));
                    }
                    return Ok((bytes, line));
                }
                Ok(bytes) => format!("SHA-256 {}, not the pinned one", Sha256::of(&bytes)),
                Err(why) => why.replace(['\r', '\n'], " "),
            };
            attempts.push(format!("{url}: {failed}"));
        }
    }
    Err(attempts)
}

/// What [`fetch_cached`] installs, and from where.
pub struct Cached<'a> {
    /// The URLs to download from on a miss, in order.
    pub urls: &'a [String],
    /// The SHA-256 of the download (before unpacking).
    pub sha256: &'a Sha256,
    /// How the file comes out of the download.
    pub unpack: &'a Unpack,
    /// The file (with `Unpack::Tree`, the directory) to install.
    pub out: &'a Path,
    /// The file the actions cache restores and saves: the download as fetched.
    pub cache: &'a Path,
}

/// Installs `req.out` from the download in `req.cache` when it is there, else from the first of
/// `req.urls` that gives the pinned bytes ([`first_pinned`]), kept in `req.cache`: the line to
/// print, which says whether the cache hit.
pub fn fetch_cached(
    req: &Cached<'_>,
    get: Get<'_>,
    pause: &dyn Fn(Duration),
) -> Result<String, Error> {
    let cache_error = |e: std::io::Error| Error::Cache {
        path: req.cache.to_owned(),
        why: e.to_string(),
    };
    let (bytes, from) = match std::fs::read(req.cache) {
        Ok(bytes) => {
            let actual = Sha256::of(&bytes);
            if actual != *req.sha256 {
                return Err(Error::CachedMismatch {
                    path: req.cache.to_owned(),
                    expected: *req.sha256,
                    actual,
                });
            }
            let from = format!(
                "the cache hit: {} has SHA-256 {actual}; nothing was downloaded",
                req.cache.display()
            );
            (bytes, from)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let (bytes, line) =
                first_pinned(req.urls, req.sha256, get, pause).map_err(|attempts| {
                    Error::Unavailable {
                        cache: req.cache.to_owned(),
                        attempts,
                    }
                })?;
            write_file(req.cache, &bytes, 0o644).map_err(cache_error)?;
            let from = format!("the cache missed: {line}; kept in {}", req.cache.display());
            (bytes, from)
        }
        Err(e) => return Err(cache_error(e)),
    };
    install(&bytes, req.sha256, req.unpack, req.out)?;
    Ok(format!("ok: {from}; installed {}", req.out.display()))
}

/// The file `out` is written as before it is renamed over `out`.
pub fn part_path(out: &Path) -> PathBuf {
    let mut part = out.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

/// Writes `data` to `out` (mode 0755) through [`part_path`], removing the part file
/// when a step fails.
fn write_executable(out: &Path, data: &[u8]) -> std::io::Result<()> {
    write_file(out, data, 0o755)
}

/// Writes `data` to `out` with `mode` through [`part_path`], removing the part file when a step
/// fails.
fn write_file(out: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    out.parent().map_or(Ok(()), std::fs::create_dir_all)?;
    let part = part_path(out);
    let written = std::fs::write(&part, data)
        .and_then(|()| std::fs::set_permissions(&part, std::fs::Permissions::from_mode(mode)))
        .and_then(|()| std::fs::rename(&part, out));
    if written.is_err() {
        // A part file that cannot be removed is reported by the write error itself.
        let _ = std::fs::remove_file(&part);
    }
    written
}

/// Installs the tar archive `tar` as the directory `out`: unpacked into [`part_path`] and
/// renamed over `out`, so `out` holds the whole new tree or what it held before.
fn install_tree(tar: &[u8], out: &Path) -> Result<(), Error> {
    let part = part_path(out);
    let installed = unpack_tree(tar, &part, out).and_then(|()| {
        let install = |e: std::io::Error| install_error(out, e);
        if out.exists() {
            std::fs::remove_dir_all(out).map_err(install)?;
        }
        std::fs::rename(&part, out).map_err(install)
    });
    if installed.is_err() {
        // A part tree that cannot be removed is reported by the error itself.
        let _ = std::fs::remove_dir_all(&part);
    }
    installed
}

fn install_error(out: &Path, e: impl fmt::Display) -> Error {
    Error::Install {
        path: out.to_owned(),
        why: e.to_string(),
    }
}

/// Writes the files and directories of `tar` below `dest` (made empty first), each without the
/// archive's top directory. A file is 0755 when the archive makes it executable by anyone, 0644
/// otherwise.
fn unpack_tree(tar: &[u8], dest: &Path, out: &Path) -> Result<(), Error> {
    let io = |e: std::io::Error| install_error(out, e);
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(io)?;
    }
    std::fs::create_dir_all(dest).map_err(io)?;
    let mut archive = tar::Archive::new(tar);
    for entry in archive.entries().map_err(unpack_error)? {
        let mut entry = entry.map_err(unpack_error)?;
        let path = entry.path().map_err(unpack_error)?.into_owned();
        if !path.components().all(|c| matches!(c, Component::Normal(_))) {
            return Err(Error::Unpack(format!("{path:?} leaves the tree")));
        }
        let relative: PathBuf = path.components().skip(1).collect();
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(&relative);
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            std::fs::create_dir_all(&target).map_err(io)?;
        } else if kind.is_file() {
            let mode = if entry.header().mode().map_err(unpack_error)? & 0o111 == 0 {
                0o644
            } else {
                0o755
            };
            target
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .map_err(io)?;
            let mut file = std::fs::File::create(&target).map_err(io)?;
            std::io::copy(&mut entry, &mut file).map_err(io)?;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).map_err(io)?;
        } else {
            return Err(Error::Unpack(format!(
                "{path:?} is not a regular file or a directory"
            )));
        }
    }
    Ok(())
}

/// The contents of the regular file at `member` in the tar archive `tar`.
fn tar_member(tar: &[u8], member: &Path) -> Result<Vec<u8>, Error> {
    let mut archive = tar::Archive::new(tar);
    for entry in archive.entries().map_err(unpack_error)? {
        let mut entry = entry.map_err(unpack_error)?;
        if entry.path().map_err(unpack_error)? != member {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(Error::NotAFile(member.to_owned()));
        }
        let mut file = Vec::new();
        entry.read_to_end(&mut file).map_err(unpack_error)?;
        return Ok(file);
    }
    Err(Error::NoMember(member.to_owned()))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cache_tests;
