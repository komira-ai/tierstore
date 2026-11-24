//! `cargo xtask mac-farm`: runs one Cargo command of this workspace on a Mac of the build farm,
//! from a Linux runner, as one REAPI action, and brings back the files it declares
//! (docs/mac-farm.md).
//!
//! A farm Mac runs an action as an ordinary user under a sandbox with no network, and has the
//! Command Line Tools and no Rust. So everything the command needs is an input of the action,
//! gathered here:
//!
//! 1. the files git tracks in the checkout, at their paths, as they are on disk (a step before
//!    this one may have changed one, such as the version a release build writes);
//! 2. the crates of `Cargo.lock`, which `cargo fetch` downloads into a Cargo home of this run's
//!    own (in `--scratch`), below [`CARGO_HOME`];
//! 3. the pinned Rust toolchain for `aarch64-apple-darwin` ([`toolchain`]), below [`TOOLCHAIN`];
//! 4. each `--tool-url`: a Cargo subcommand's release tarball, held to its `--tool-sha256`,
//!    whose `--tool-member` is put in the Cargo home's `bin/`, where Cargo finds a subcommand.
//!
//! The Command is the toolchain's `cargo` with the arguments after `--`, from the checkout's
//! root, offline, with the toolchain's `rustc` and `rustdoc` and the Mac's `/usr/bin` (its C
//! compiler and linker, and its tools such as `pkgbuild`). Its `output_paths` are the
//! `--output` paths. The action's platform is `--properties`, the farm's Mac platform.
//!
//! The blobs the farm's CAS lacks are uploaded, the action is executed and followed until it is
//! done (a stage change is printed: queued while no Mac is free, then running), and the
//! command's stdout and stderr are printed. When the command exited 0, every `--output` is
//! written below the checkout at its path, each file held to its digest ([`outputs`]). The verb
//! ends 0 when the command exited 0 and its outputs are written, and 1 for every other end: the
//! command's own non-zero exit, an output that is missing or not what was declared, or the
//! farm's refusal or failure, whose status is printed (with the job's output tail when the farm
//! attached one).
//!
//! The verb's first lines mask the address in a workflow's log (`::add-mask::`), so nothing it
//! prints after them shows it. (A workflow step's own header still shows the values of its
//! `env:`; that is the workflow's to keep out.)
//!
//! With an empty `--address` the verb prints a notice and exits 0: a repository with no farm runs
//! nothing and fails nothing. A workflow that needs the outputs fails at their upload.

pub mod client;
pub mod outputs;
pub mod proto;
pub mod toolchain;
pub mod tree;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as Process;

use clap::Args as ClapArgs;
use prost::Message as _;

use self::client::{Farm, Progress};
use self::outputs::Item;
use self::proto::google::rpc;
use self::proto::reapi::{
    Action, ActionResult, Command, Digest, ExecuteResponse, Platform, command::EnvironmentVariable,
    platform::Property,
};
use self::tree::{Entry, Tree};
use crate::Error;
use crate::fetch::{Sha256, Unpack};

/// The directory the run's own inputs are in, which the checkout must not have.
pub const OURS: &str = ".mac-farm";

/// Where the toolchain is in the input root.
pub const TOOLCHAIN: &str = ".mac-farm/toolchain";

/// Where the Cargo home is in the input root.
pub const CARGO_HOME: &str = ".mac-farm/cargo-home";

/// The Mac's target.
pub const TARGET: &str = "aarch64-apple-darwin";

/// The action's timeout when `--timeout-secs` is not given: under the farm's default limit of
/// 3000 s.
pub const DEFAULT_TIMEOUT_SECS: u64 = 2400;

/// The Mac's programs a build uses: its C compiler and linker, `git`, `xcrun`, `pkgbuild`.
const MAC_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The entries of a Cargo home that are not sent, by their path in it: Cargo's locks and its
/// cache tracker, which change on every run; the installed programs (a `--tool-url` is the way a
/// program is sent); and the crates unpacked, which Cargo unpacks again from their `.crate` files
/// in `registry/cache` (four times smaller), offline.
const HOME_SKIPPED: [&str; 5] = [
    ".global-cache",
    ".package-cache",
    ".package-cache-mutate",
    "bin",
    "registry/src",
];

/// The reason of the `ErrorInfo` the farm attaches to a failed job: its output tail.
const OUTPUT_TAIL: &str = "JOB_OUTPUT_TAIL";

/// The flags of the verb.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The farm's REAPI address, `grpc://<host>:<port>`. Empty: print a notice and run
    /// nothing.
    #[arg(long)]
    pub address: String,
    /// The instance name sent on every request. Empty: the empty instance.
    #[arg(long, default_value = "")]
    pub instance: String,
    /// Whether the address is served over TLS: `true` or `false`.
    #[arg(long, default_value = "")]
    pub tls: String,
    /// The farm's Mac platform properties, `key=value` separated by commas.
    #[arg(long, default_value = "")]
    pub properties: String,
    /// A directory of this run's own: the Cargo home of `cargo fetch` is made in it.
    #[arg(long)]
    pub scratch: PathBuf,
    /// The action's timeout, in seconds.
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout_secs: u64,
    /// The git program that lists the checkout's files.
    #[arg(long, default_value = "git")]
    pub git: PathBuf,
    /// The Cargo program that fetches the crates on this machine.
    #[arg(long = "cargo", default_value = "cargo")]
    pub cargo_program: PathBuf,
    /// A file or directory the command makes, relative to the checkout's root: declared in the
    /// Command and written below the checkout after a run that exited 0. Repeatable.
    #[arg(long = "output")]
    pub outputs: Vec<String>,
    /// The `.tar.xz` release of a Cargo subcommand to send (HTTPS). Repeatable; the n-th goes
    /// with the n-th `--tool-sha256` and `--tool-member`.
    #[arg(long = "tool-url")]
    pub tool_urls: Vec<String>,
    /// The SHA-256 of that tarball, 64 lowercase hex digits.
    #[arg(long = "tool-sha256")]
    pub tool_sha256s: Vec<String>,
    /// The program's path inside that tarball; it is sent as the Cargo home's `bin/<its name>`.
    #[arg(long = "tool-member")]
    pub tool_members: Vec<String>,
    /// The Cargo command to run on the Mac, after `--`: `test -p tierstore --locked`.
    #[arg(last = true, required = true)]
    pub cargo: Vec<String>,
}

/// Why the flags do not make a run.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Problem {
    #[error("the address {0:?} is not grpc://<host>:<port>")]
    Address(String),
    #[error("--tls is {0:?}: it must be `true` or `false`")]
    Tls(String),
    #[error("the properties are empty: the platform needs at least one key=value")]
    NoProperties,
    #[error("the property {0:?} is not key=value with a key and a value")]
    Property(String),
    #[error("the property {0:?} is given twice")]
    DuplicateProperty(String),
    #[error("{what} holds whitespace or a control character: {value:?}")]
    Unsafe { what: &'static str, value: String },
    #[error("--timeout-secs is 0")]
    NoTimeout,
    #[error(
        "the Cargo command is empty, or starts with an option ({0:?}): it starts with its subcommand"
    )]
    Cargo(String),
    #[error(
        "--output {0:?} is not a relative path of plain components outside {OURS}/, or is given twice"
    )]
    Output(String),
    #[error(
        "--tool-url, --tool-sha256 and --tool-member are given {0}, {1} and {2} times: one of each per tool"
    )]
    ToolCount(usize, usize, usize),
    #[error("--tool-url {0:?} is not an https:// URL of a .tar.xz")]
    ToolUrl(String),
    #[error("--tool-sha256 {0:?} is not 64 lowercase hex digits")]
    ToolSha256(String),
    #[error("--tool-member {0:?} is not a relative path of plain components")]
    ToolMember(String),
    #[error("two tools are both named {0:?} in the Cargo home's bin/")]
    ToolTwice(String),
}

/// A Cargo subcommand sent with the action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    pub url: String,
    pub sha256: Sha256,
    /// Its path inside the tarball.
    pub member: String,
    /// Its file name, which it has in the Cargo home's `bin/`.
    pub name: String,
}

/// The checked settings of a run.
#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
    /// The URL of the farm, `http://` or `https://`.
    pub endpoint: String,
    pub tls: bool,
    pub instance: String,
    /// Sorted by key.
    pub properties: Vec<(String, String)>,
    pub timeout_secs: u64,
    pub cargo: Vec<String>,
    /// Sorted, as REAPI requires of `output_paths`.
    pub outputs: Vec<String>,
    pub tools: Vec<Tool>,
}

/// `value` when it holds no whitespace and no control character.
fn one_token(what: &'static str, value: &str) -> Result<(), Problem> {
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(Problem::Unsafe {
            what,
            value: value.to_owned(),
        });
    }
    Ok(())
}

/// The properties in `text`, in order: `key=value` entries separated by commas, each
/// trimmed. No entry may be empty, lack the `=`, have an empty key or value, or repeat a
/// key; the list may not be empty.
///
/// # Errors
/// The [`Problem`] naming the first entry that is wrong.
pub fn parse_properties(text: &str) -> Result<Vec<(String, String)>, Problem> {
    if text.trim().is_empty() {
        return Err(Problem::NoProperties);
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for entry in text.split(',') {
        let entry = entry.trim();
        let (key, value) = match entry.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() && !v.trim().is_empty() => (k.trim(), v.trim()),
            _ => return Err(Problem::Property(entry.to_owned())),
        };
        one_token("a property", key)?;
        one_token("a property", value)?;
        if out.iter().any(|(k, _)| k == key) {
            return Err(Problem::DuplicateProperty(key.to_owned()));
        }
        out.push((key.to_owned(), value.to_owned()));
    }
    Ok(out)
}

/// The tools the `--tool-*` flags of `args` name.
fn tools(args: &Args) -> Result<Vec<Tool>, Problem> {
    let (urls, sums, members) = (&args.tool_urls, &args.tool_sha256s, &args.tool_members);
    if urls.len() != sums.len() || urls.len() != members.len() {
        return Err(Problem::ToolCount(urls.len(), sums.len(), members.len()));
    }
    let mut out: Vec<Tool> = Vec::new();
    for ((url, sha256), member) in urls.iter().zip(sums).zip(members) {
        if !url.starts_with("https://") || !url.ends_with(".tar.xz") {
            return Err(Problem::ToolUrl(url.clone()));
        }
        let sha256 = sha256
            .parse::<Sha256>()
            .map_err(|_| Problem::ToolSha256(sha256.clone()))?;
        let parts = tree::components(member).map_err(|_| Problem::ToolMember(member.clone()))?;
        let name = parts.last().copied().unwrap_or_default().to_owned();
        if out.iter().any(|t| t.name == name) {
            return Err(Problem::ToolTwice(name));
        }
        out.push(Tool {
            url: url.clone(),
            sha256,
            member: member.clone(),
            name,
        });
    }
    Ok(out)
}

/// The `--output` paths of `args`, checked and sorted.
fn output_paths(args: &Args) -> Result<Vec<String>, Problem> {
    let mut out: Vec<String> = Vec::new();
    for path in &args.outputs {
        let plain = tree::components(path).is_ok();
        let ours = path == OURS || path.starts_with(&format!("{OURS}/"));
        if !plain || ours || out.contains(path) {
            return Err(Problem::Output(path.clone()));
        }
        out.push(path.clone());
    }
    out.sort();
    Ok(out)
}

impl Settings {
    /// The settings `args` name.
    ///
    /// # Errors
    /// The [`Problem`] of the first flag that is wrong.
    pub fn of(args: &Args) -> Result<Settings, Problem> {
        let address = args.address.trim();
        let host = address.strip_prefix("grpc://").unwrap_or_default();
        let port = host.rsplit_once(':').map(|(_, p)| p).unwrap_or_default();
        let plain = !host.contains('/') && !host.chars().any(char::is_whitespace);
        if !plain || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Problem::Address(address.to_owned()));
        }
        let tls = match args.tls.trim() {
            "true" => true,
            "false" => false,
            other => return Err(Problem::Tls(other.to_owned())),
        };
        let mut properties = parse_properties(&args.properties)?;
        properties.sort();
        if args.timeout_secs == 0 {
            return Err(Problem::NoTimeout);
        }
        match args.cargo.first() {
            Some(sub) if !sub.starts_with('-') && !sub.starts_with('+') => {}
            first => return Err(Problem::Cargo(first.cloned().unwrap_or_default())),
        }
        let outputs = output_paths(args)?;
        let tools = tools(args)?;
        let scheme = if tls { "https" } else { "http" };
        Ok(Settings {
            endpoint: format!("{scheme}://{host}"),
            tls,
            instance: args.instance.trim().to_owned(),
            properties,
            timeout_secs: args.timeout_secs,
            cargo: args.cargo.clone(),
            outputs,
            tools,
        })
    }
}

/// The action's platform.
fn platform(s: &Settings) -> Platform {
    Platform {
        properties: s
            .properties
            .iter()
            .map(|(name, value)| Property {
                name: name.clone(),
                value: value.clone(),
            })
            .collect(),
    }
}

/// The Command: the toolchain's `cargo` with the run's arguments, from the input root, its
/// environment sorted by name, its outputs the run's `--output` paths. Its platform is the
/// Action's ([`action`]).
#[must_use]
pub fn command(s: &Settings) -> Command {
    let env = [
        ("CARGO_HOME", CARGO_HOME.to_owned()),
        ("CARGO_INCREMENTAL", "0".to_owned()),
        ("CARGO_NET_OFFLINE", "true".to_owned()),
        ("CARGO_TERM_COLOR", "always".to_owned()),
        ("PATH", MAC_PATH.to_owned()),
        ("RUSTC", format!("{TOOLCHAIN}/bin/rustc")),
        ("RUSTDOC", format!("{TOOLCHAIN}/bin/rustdoc")),
    ];
    Command {
        arguments: std::iter::once(format!("{TOOLCHAIN}/bin/cargo"))
            .chain(s.cargo.iter().cloned())
            .collect(),
        environment_variables: env
            .into_iter()
            .map(|(name, value)| EnvironmentVariable {
                name: name.to_owned(),
                value,
            })
            .collect(),
        output_paths: s.outputs.clone(),
        ..Command::default()
    }
}

/// The Action of `command` over the input root `root`; its blobs go into `tree`.
pub fn action(s: &Settings, root: &Digest, tree: &mut Tree) -> Digest {
    let command = tree::add(&mut tree.blobs, command(s).encode_to_vec());
    let action = Action {
        command_digest: Some(command),
        input_root_digest: Some(root.clone()),
        timeout: Some(prost_types::Duration {
            seconds: i64::try_from(s.timeout_secs).unwrap_or(i64::MAX),
            nanos: 0,
        }),
        platform: Some(platform(s)),
    };
    tree::add(&mut tree.blobs, action.encode_to_vec())
}

/// The entry of the file or symlink at `path`.
fn entry_at(path: &Path) -> Result<Entry, Error> {
    let what = || format!("read {}", path.display());
    let meta = std::fs::symlink_metadata(path).map_err(Error::spawn(what()))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).map_err(Error::spawn(what()))?;
        return Ok(Entry::Symlink(target.to_string_lossy().into_owned()));
    }
    if !meta.is_file() {
        return Err(Error::Failed(format!(
            "{} is neither a file nor a symlink",
            path.display()
        )));
    }
    use std::os::unix::fs::PermissionsExt as _;
    let executable = meta.permissions().mode() & 0o111 != 0;
    let bytes = std::fs::read(path).map_err(Error::spawn(what()))?;
    Ok(Entry::File { bytes, executable })
}

/// The entries of the checkout `root`: each of `tracked` (paths relative to it).
///
/// # Errors
/// [`Error`] for a path that cannot be read, or one in the directory the run's own inputs use.
pub fn checkout(root: &Path, tracked: &[String]) -> Result<Vec<(String, Entry)>, Error> {
    let mut out = Vec::new();
    for path in tracked {
        if path == OURS || path.starts_with(&format!("{OURS}/")) {
            return Err(Error::Failed(format!(
                "the checkout tracks {path}, where the run puts its toolchain and crates"
            )));
        }
        out.push((path.clone(), entry_at(&root.join(path))?));
    }
    Ok(out)
}

/// The entries of the Cargo home `home`, below [`CARGO_HOME`], but those of [`HOME_SKIPPED`].
///
/// # Errors
/// [`Error`] for a directory or file that cannot be read.
pub fn cargo_home(home: &Path) -> Result<Vec<(String, Entry)>, Error> {
    let mut out = Vec::new();
    walk(home, "", &mut out)?;
    Ok(out)
}

/// The entries below `dir`, which is `inside` (`/`-separated, empty for the top) in the Cargo
/// home, into `out`.
fn walk(dir: &Path, inside: &str, out: &mut Vec<(String, Entry)>) -> Result<(), Error> {
    let what = format!("list {}", dir.display());
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .and_then(|d| d.map(|e| e.map(|e| e.file_name())).collect())
        .map_err(Error::spawn(what))?;
    names.sort();
    for name in names {
        let name = name.to_string_lossy().into_owned();
        let here = if inside.is_empty() {
            name.clone()
        } else {
            format!("{inside}/{name}")
        };
        if HOME_SKIPPED.contains(&here.as_str()) {
            continue;
        }
        let path = dir.join(&name);
        let meta =
            std::fs::symlink_metadata(&path).map_err(Error::spawn(path.display().to_string()))?;
        if meta.is_dir() {
            walk(&path, &here, out)?;
        } else {
            out.push((format!("{CARGO_HOME}/{here}"), entry_at(&path)?));
        }
    }
    Ok(())
}

/// The entries of `tools`, each downloaded by `download`, held to its SHA-256 and taken out of
/// its tarball, as an executable of the Cargo home's `bin/`.
///
/// # Errors
/// [`Error::Failed`] for a download that fails, is not its pin, or lacks its member.
pub fn tool_entries(
    tools: &[Tool],
    download: &mut toolchain::Download<'_>,
) -> Result<Vec<(String, Entry)>, Error> {
    let mut out = Vec::new();
    for tool in tools {
        let bytes = download(&tool.url)
            .map_err(|why| Error::Failed(format!("download {}: {why}", tool.url)))?;
        let unpack = Unpack::Tar {
            xz: true,
            member: PathBuf::from(&tool.member),
        };
        let file = crate::fetch::file_of(&bytes, &tool.sha256, &unpack)
            .map_err(|e| Error::Failed(format!("{}: {e}", tool.url)))?;
        out.push((
            format!("{CARGO_HOME}/bin/{}", tool.name),
            Entry::File {
                bytes: file,
                executable: true,
            },
        ));
    }
    Ok(out)
}

/// What a run takes from the machine it runs on.
pub struct Host<'a> {
    /// The paths git tracks in the checkout.
    pub tracked: &'a mut dyn FnMut() -> Result<Vec<String>, Error>,
    /// Fetches the crates of `Cargo.lock` into the Cargo home it is given.
    pub fetch: &'a mut dyn FnMut(&Path) -> Result<(), Error>,
    /// The toolchain's pins ([`toolchain::PINS`]).
    pub pins: toolchain::Pins,
    /// Downloads a toolchain or tool tarball.
    pub download: &'a mut toolchain::Download<'a>,
}

/// The input root of the run and its action: every blob of both, and the action's digest.
///
/// # Errors
/// [`Error`] for an input that cannot be gathered.
pub fn inputs(
    s: &Settings,
    root: &Path,
    scratch: &Path,
    host: &mut Host<'_>,
) -> Result<(Tree, Digest), Error> {
    let mut entries = checkout(root, &(host.tracked)()?)?;
    let home = scratch.join("cargo-home");
    std::fs::create_dir_all(&home).map_err(Error::spawn(format!("create {}", home.display())))?;
    (host.fetch)(&home)?;
    entries.extend(cargo_home(&home)?);
    entries.extend(
        toolchain::entries(&host.pins, TOOLCHAIN, host.download)
            .map_err(|e| Error::Failed(e.to_string()))?,
    );
    entries.extend(tool_entries(&s.tools, host.download)?);
    let mut tree = tree::build(entries).map_err(|e| Error::Failed(e.to_string()))?;
    let root = tree.root.clone();
    let action = action(s, &root, &mut tree);
    Ok((tree, action))
}

/// What the farm answered, printed into `out` and `err`: the command's result when it exited 0
/// and the line that says so, or the run's failure.
///
/// # Errors
/// [`Error::Failed`] for a status that is not OK, no result, or a non-zero exit.
pub async fn report(
    farm: &Farm,
    s: &Settings,
    response: ExecuteResponse,
    out: &mut (dyn Write + Send),
    err: &mut (dyn Write + Send),
) -> Result<(ActionResult, String), Error> {
    let shown = s.cargo.join(" ");
    if let Some(result) = &response.result {
        print_stream(farm, &result.stdout_raw, result.stdout_digest.as_ref(), out).await?;
        print_stream(farm, &result.stderr_raw, result.stderr_digest.as_ref(), err).await?;
    }
    let status = response.status.unwrap_or_default();
    if status.code != 0 {
        let _ = err.write_all(tail(&status).as_bytes());
        return Err(Error::Failed(format!(
            "the farm did not run `cargo {shown}` to its end: {:?}: {}",
            tonic::Code::from_i32(status.code),
            status.message
        )));
    }
    let Some(result) = response.result else {
        return Err(Error::Failed(format!(
            "the farm answered `cargo {shown}` with no result"
        )));
    };
    let cached = if response.cached_result {
        ", a cached result"
    } else {
        ""
    };
    if result.exit_code != 0 {
        return Err(Error::Failed(format!(
            "`cargo {shown}` exited {} on a farm Mac{cached}",
            result.exit_code
        )));
    }
    Ok((
        result,
        format!("ok: `cargo {shown}` passed on a farm Mac{cached}"),
    ))
}

/// One of the command's streams into `to`. A farm may send a small one inline (`raw`), a copy
/// of the blob its digest names: the inline bytes when there are any, else the blob.
async fn print_stream(
    farm: &Farm,
    raw: &[u8],
    digest: Option<&Digest>,
    to: &mut (dyn Write + Send),
) -> Result<(), Error> {
    if !raw.is_empty() {
        // A log line that cannot be written must not hide the verdict.
        let _ = to.write_all(raw);
    } else if let Some(digest) = digest {
        let bytes = farm
            .read(digest)
            .await
            .map_err(|e| Error::Failed(e.to_string()))?;
        let _ = to.write_all(&bytes);
    }
    Ok(())
}

/// The stdout and stderr tails of a failed job the farm attached to `status`, as text.
fn tail(status: &rpc::Status) -> String {
    let mut text = String::new();
    for detail in &status.details {
        let Ok(info) = rpc::ErrorInfo::decode(detail.value.as_slice()) else {
            continue;
        };
        if info.reason != OUTPUT_TAIL {
            continue;
        }
        for key in ["stdout", "stderr"] {
            if let Some(value) = info.metadata.get(key) {
                text.push_str(&format!("--- the job's {key} (its end) ---\n{value}\n"));
            }
        }
    }
    text
}

/// The line a stage change prints.
#[must_use]
pub fn progress_line(progress: Progress) -> &'static str {
    match progress {
        Progress::Queued => "mac-farm: queued: waiting for a free Mac",
        Progress::Executing => "mac-farm: running on a farm Mac",
    }
}

/// Makes `item` below `root`: a directory, or a file whose bytes are held to its digest. Nothing
/// that is there already is replaced, and a file that is not its digest is removed.
async fn write_item(farm: &Farm, root: &Path, item: &Item) -> Result<(), Error> {
    let path = root.join(item.path());
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(Error::Failed(format!(
            "the output {} is there already; a run writes its outputs where nothing is",
            item.path()
        )));
    }
    let Item::File {
        digest,
        inline,
        executable,
        ..
    } = item
    else {
        return std::fs::create_dir_all(&path)
            .map_err(Error::spawn(format!("create {}", path.display())));
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(Error::spawn(format!("create {}", parent.display())))?;
    }
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(if *executable { 0o755 } else { 0o644 })
        .open(&path)
        .map_err(Error::spawn(format!("create {}", path.display())))?;
    let written = match inline {
        Some(bytes) if tree::digest_of(bytes) == *digest => file
            .write_all(bytes)
            .map_err(Error::spawn(format!("write {}", path.display()))),
        Some(_) => Err(Error::Failed(format!(
            "the farm sent {} inline with bytes that are not its digest {}/{}",
            item.path(),
            digest.hash,
            digest.size_bytes
        ))),
        None => farm
            .read_to(digest, &mut file)
            .await
            .map_err(|e| Error::Failed(format!("{}: {e}", item.path()))),
    };
    if written.is_err() {
        // Half a file, or one that is not its digest, is not left to be published.
        drop(file);
        let _ = std::fs::remove_file(&path);
    }
    written
}

/// Writes the outputs of `result` that `s` declares below `root`: the lines to print.
///
/// # Errors
/// [`Error::Failed`] for outputs that are not what was declared, or that cannot be read or
/// written.
pub async fn fetch_outputs(
    farm: &Farm,
    s: &Settings,
    result: &ActionResult,
    root: &Path,
) -> Result<Vec<String>, Error> {
    let failed = |e: outputs::OutputError| Error::Failed(e.to_string());
    let mut trees = BTreeMap::new();
    for digest in outputs::tree_digests(result).map_err(failed)? {
        let bytes = farm
            .read(&digest)
            .await
            .map_err(|e| Error::Failed(e.to_string()))?;
        trees.insert(digest.hash, bytes);
    }
    let items = outputs::plan(result, &s.outputs, &trees).map_err(failed)?;
    for item in &items {
        write_item(farm, root, item).await?;
    }
    Ok(s.outputs
        .iter()
        .map(|declared| {
            let files = items
                .iter()
                .filter(|i| matches!(i, Item::File { .. }) && outputs::within(i.path(), declared))
                .count();
            let files = if files == 1 {
                "1 file".to_owned()
            } else {
                format!("{files} files")
            };
            format!("ok: {declared} written from the farm Mac ({files})")
        })
        .collect())
}

/// Where a run's output goes: the command's stdout and stderr, and the stage changes.
pub struct Sink<'a> {
    pub out: &'a mut (dyn Write + Send),
    pub err: &'a mut (dyn Write + Send),
    pub progress: &'a mut (dyn FnMut(Progress) + Send),
}

/// The run against the farm: upload what it lacks, execute, report, and write the outputs below
/// `root`.
///
/// # Errors
/// [`Error::Failed`], for every end but a command that exited 0 whose outputs were written.
pub async fn remote(
    s: &Settings,
    tree: &Tree,
    action: &Digest,
    root: &Path,
    sink: &mut Sink<'_>,
) -> Result<Vec<String>, Error> {
    let failed = |e: client::FarmError| Error::Failed(e.to_string());
    let farm = Farm::connect(&s.endpoint, s.tls, &s.instance)
        .await
        .map_err(failed)?;
    let digests: Vec<Digest> = tree
        .blobs
        .iter()
        .map(|(hash, bytes)| Digest {
            hash: hash.clone(),
            size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        })
        .collect();
    let missing = farm.missing(&digests).await.map_err(failed)?;
    let bytes: i64 = missing.iter().map(|d| d.size_bytes).sum();
    let _ = writeln!(
        sink.err,
        "mac-farm: {} blobs, {} missing from the farm ({bytes} bytes to upload)",
        digests.len(),
        missing.len()
    );
    farm.upload(&tree.blobs, &missing).await.map_err(failed)?;
    let response = farm
        .execute(action, &mut *sink.progress)
        .await
        .map_err(failed)?;
    let (result, passed) = report(&farm, s, response, &mut *sink.out, &mut *sink.err).await?;
    let mut lines = vec![passed];
    lines.extend(fetch_outputs(&farm, s, &result, root).await?);
    Ok(lines)
}

/// The verb with the machine's inputs given, the checkout at `root`.
///
/// # Errors
/// [`Error`], for every end but a command that exited 0 on a farm Mac whose outputs were
/// written.
pub fn run_with(
    args: &Args,
    root: &Path,
    host: &mut Host<'_>,
    sink: &mut Sink<'_>,
) -> Result<Vec<String>, Error> {
    // The logs of a public repository's runs are public: keep the farm's address out of what
    // follows, the refusal of a wrong address included.
    let address = args.address.trim();
    let _ = writeln!(sink.out, "::add-mask::{address}");
    let _ = writeln!(
        sink.out,
        "::add-mask::{}",
        address.trim_start_matches("grpc://")
    );
    let settings = Settings::of(args).map_err(|p| Error::Failed(p.to_string()))?;
    let scratch = std::path::absolute(&args.scratch)
        .map_err(Error::spawn(format!("resolve {}", args.scratch.display())))?;
    let (tree, action) = inputs(&settings, root, &scratch, host)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Error::spawn("start the runtime".to_owned()))?;
    runtime.block_on(remote(&settings, &tree, &action, root, sink))
}

/// The verb, on the checkout at `root`: the lines to print.
///
/// # Errors
/// [`Error`], for every end but a command that exited 0 on a farm Mac whose outputs were
/// written.
pub fn run(args: &Args, root: &Path) -> Result<Vec<String>, Error> {
    if args.address.trim().is_empty() {
        return Ok(vec![
            "::notice::mac-farm runs nothing: the variable FARM_ADDRESS is not set".to_owned(),
        ]);
    }
    let roots = crate::fetch::host_roots();
    let mut tracked = || git_files(&args.git, root);
    let mut fetch = |home: &Path| fetch_crates(&args.cargo_program, root, home);
    let mut download = |url: &str| crate::fetch::download(url, &roots).map_err(|e| e.to_string());
    let pins = toolchain::parse(toolchain::PINS).map_err(|e| Error::Failed(e.to_string()))?;
    let mut host = Host {
        tracked: &mut tracked,
        fetch: &mut fetch,
        pins,
        download: &mut download,
    };
    let mut progress = |p: Progress| {
        let _ = writeln!(std::io::stderr(), "{}", progress_line(p));
    };
    run_with(
        args,
        root,
        &mut host,
        &mut Sink {
            out: &mut std::io::stdout(),
            err: &mut std::io::stderr(),
            progress: &mut progress,
        },
    )
}

/// What a program wrote, and whether it exited 0.
pub struct Out {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `cmd` to its end, its standard input closed.
///
/// # Errors
/// [`Error::Spawn`] when the program cannot be started.
pub fn spawn(cmd: &mut Process) -> Result<Out, Error> {
    let what = format!("run {:?}", cmd.get_program());
    let out = cmd
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(Error::spawn(what))?;
    Ok(Out {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// The output of a program `what` ran, when it succeeded.
///
/// # Errors
/// [`Error::Failed`] naming `what` and the program's stderr.
pub fn succeeded(what: &str, ran: Out) -> Result<String, Error> {
    if ran.ok {
        Ok(ran.stdout)
    } else {
        Err(Error::Failed(format!(
            "{what} failed: {}",
            ran.stderr.trim_end()
        )))
    }
}

/// The paths `<git> ls-files -z` lists in `root`.
///
/// # Errors
/// [`Error`] when git cannot be started or fails.
pub fn git_files(git: &Path, root: &Path) -> Result<Vec<String>, Error> {
    let listed = spawn(Process::new(git).args(["ls-files", "-z"]).current_dir(root))?;
    Ok(split_nul(&succeeded("git ls-files", listed)?))
}

/// The entries of a NUL-separated list.
#[must_use]
pub fn split_nul(text: &str) -> Vec<String> {
    text.split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `<cargo> fetch` of the crates of `root`'s lock file, for every platform, into the Cargo home
/// `home`: a Mac build's `cargo metadata` (an SBOM, a licence notice) reads the manifests of
/// every package of the lock, not the Mac's alone.
///
/// # Errors
/// [`Error`] when Cargo cannot be started or fails.
pub fn fetch_crates(cargo: &Path, root: &Path, home: &Path) -> Result<(), Error> {
    let fetched = spawn(
        Process::new(cargo)
            .args(["fetch", "--locked"])
            .env("CARGO_HOME", home)
            .current_dir(root),
    )?;
    succeeded("cargo fetch", fetched).map(drop)
}
