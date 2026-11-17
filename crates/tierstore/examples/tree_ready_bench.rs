//! Measures how long a node takes to make a tree ready from its version 2 tree list, in the tree
//! store of a real cache: before (the list's directories built into records, each hashed, and
//! written) and after (the list kept as the tree, read in place).
//!
//! ```text
//! cargo run --release -p tierstore --example tree_ready_bench -- \
//!     [--runs N] --cache DIR --from LIST
//! ```
//!
//! `LIST` is a tree list, version 1 (as `tree_list_bench --save` writes it) or version 2; a
//! version 1 list is encoded as version 2 first. `DIR` is an empty directory the benchmark makes
//! its caches in, one for each run and each way, and removes again. Each step is run `N` times
//! (25) and the median printed:
//!
//! - the SHA-256 of the list, which the mount provider checks against the list's key on a thread
//!   of its own beside the walk;
//! - before: the walk and the records built and hashed (`TreeListV2::build`); the tree store's
//!   `create` of them; the two as the mount provider ran them, the hash beside the walk and then
//!   the create (the tree is ready when the create returns); and the same with no hash;
//! - after: the walk alone, the list kept as the tree (`TreeImage::open`); the tree store's
//!   `create_image` of it; the two as the provider runs them, the hash and the write of the
//!   image (`TreeStore::stage_image`) beside the walk, then the create; and the same with no
//!   hash;
//! - the walk again, as the mount and a start check a tree's image.
//!
//! Last, it says whether every file has the same digest and size in both trees.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tierstore::server::local_cache::trees::{
    CreateRequest, ImageRequest, Location, Resolved, StagedImage, TreeStore, TreesConfig,
};
use tierstore::server::local_cache::{CacheConfig, LocalCache, SystemDisk};
use tierstore::{
    DirBackend, SystemClock, TreeDigest, TreeImage, TreeLimits, TreeListV2, is_tree_list_v2,
    parse_tree_list,
};

/// What the benchmark stops on: a bad flag, a list a reader refused, or a cache that failed.
pub type Failure = Box<dyn std::error::Error>;

/// How the benchmark is run.
const USAGE: &str = "usage: tree_ready_bench [--runs N] --cache DIR --from LIST";

/// A tree store over a new cache in `dir`, whose upstream is an empty directory beside it.
fn store_in(dir: &Path) -> Result<TreeStore, Failure> {
    let upstream = dir.join("upstream");
    std::fs::create_dir_all(&upstream)?;
    let config = CacheConfig::new(dir.join("cache"), 1 << 30, Vec::new());
    let cache = LocalCache::open(
        config,
        Arc::new(DirBackend::open(upstream)?),
        Arc::new(SystemDisk),
        Arc::new(SystemClock),
    )?;
    Ok(TreeStore::open(cache, TreesConfig::default())?)
}

/// The fs location of the benchmark's trees.
fn place() -> Result<Location, Failure> {
    Ok(Location::fs("bench/tree")?)
}

/// The median of `times`, which is not empty.
fn median(mut times: Vec<Duration>) -> Duration {
    times.sort();
    times[times.len() / 2]
}

/// The time of `f`, and what it gave.
#[expect(
    clippy::disallowed_methods,
    reason = "a benchmark measures the machine's own time"
)]
fn timed<T>(f: impl FnOnce() -> Result<T, Failure>) -> Result<(Duration, T), Failure> {
    let start = Instant::now();
    let out = f()?;
    Ok((start.elapsed(), out))
}

/// `walk` beside the SHA-256 of `bytes` on a thread of its own, as the mount provider runs them,
/// if `hash`; what the walk gave, once the hash is done.
fn beside<T>(
    bytes: &[u8],
    hash: bool,
    walk: impl FnOnce() -> Result<T, Failure>,
) -> Result<T, Failure> {
    std::thread::scope(|scope| {
        let hashing = scope.spawn(|| hash.then(|| TreeDigest::of(bytes)));
        let walked = walk();
        let _ = hashing.join().map_err(|_| "the hash panicked")?;
        walked
    })
}

/// The times of one way of making the tree ready: the walk, the create, the two as the provider
/// runs them, and the same with no hash.
struct Way {
    walk: Vec<Duration>,
    create: Vec<Duration>,
    ready: Vec<Duration>,
    made: Vec<Duration>,
}

impl Way {
    fn new() -> Self {
        Self {
            walk: Vec::new(),
            create: Vec::new(),
            ready: Vec::new(),
            made: Vec::new(),
        }
    }

    fn line(self, label: &str, walk: &str, create: &str) -> String {
        format!(
            "{label}: {walk} {:?}; {create} {:?}; ready as the provider makes it {:?}, \
             {:?} with no sha256",
            median(self.walk),
            median(self.create),
            median(self.ready),
            median(self.made)
        )
    }
}

/// A request to place `image`, named by `digest`, at the benchmark's place, `staged` if it was.
fn image_request(
    image: TreeImage,
    staged: Option<StagedImage>,
    digest: TreeDigest,
) -> Result<ImageRequest, Failure> {
    Ok(ImageRequest {
        image: Arc::new(image),
        digest,
        at: vec![place()?],
        ttl: None,
        ready: false,
        staged,
    })
}

/// A request to place the records `built` at `at`.
fn request(built: tierstore::BuiltTree, at: Location) -> CreateRequest {
    CreateRequest {
        built,
        at: vec![at],
        ttl: None,
        ready: false,
    }
}

/// The version 2 bytes of the list in `bytes`, of either version.
fn v2_of(bytes: &[u8]) -> Result<Vec<u8>, Failure> {
    if is_tree_list_v2(bytes) {
        return Ok(bytes.to_vec());
    }
    let list = parse_tree_list(bytes, &TreeDigest::of(bytes))?;
    Ok(list.encode_v2()?)
}

/// Runs the benchmark with `args` (the flags, without the program's name), writing to `out`.
pub fn run(args: impl IntoIterator<Item = String>, out: &mut dyn Write) -> Result<(), Failure> {
    let mut runs: usize = 25;
    let (mut cache, mut from) = (None::<PathBuf>, None::<PathBuf>);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => runs = args.next().ok_or("--runs N")?.parse()?,
            "--cache" => cache = Some(args.next().ok_or("--cache DIR")?.into()),
            "--from" => from = Some(args.next().ok_or("--from LIST")?.into()),
            _ => return Err(USAGE.into()),
        }
    }
    let (Some(cache), Some(from), true) = (cache, from, runs > 0) else {
        return Err(USAGE.into());
    };
    let v2 = v2_of(&std::fs::read(from)?)?;
    let digest = TreeDigest::of(&v2);
    let limits = TreeLimits::default();
    let runtime = tokio::runtime::Runtime::new()?;
    let _entered = runtime.enter();
    let list = TreeListV2::open(&v2)?;
    let image = Arc::new(TreeImage::open(v2.clone().into(), &limits, drop)?);
    writeln!(
        out,
        "tree: {} entries, {} files, {} packs; list {} B",
        list.len(),
        image.files(),
        list.keys().len(),
        v2.len()
    )?;
    let (mut hashes, mut checks) = (Vec::new(), Vec::new());
    let (mut before, mut after) = (Way::new(), Way::new());
    let mut kept = Vec::new();
    let bytes = bytes::Bytes::from(v2.clone());
    let store = |way: &str, run: usize| store_in(&cache.join(format!("{way}-{run}")));
    for run in 0..runs {
        hashes.push(timed(|| Ok(TreeDigest::of(&v2)))?.0);
        checks.push(timed(|| Ok(TreeImage::open(bytes.clone(), &limits, drop)?))?.0);
        // Before: the records built and hashed, then created.
        let (walk, built) = timed(|| Ok(list.build(&limits, drop)?))?;
        let made = store("before", run)?;
        let (create, _) = timed(|| Ok(made.create(request(built, place()?))?))?;
        before.walk.push(walk);
        before.create.push(create);
        kept.push(made);
        for (hash, times) in [(true, &mut before.ready), (false, &mut before.made)] {
            let made = store(if hash { "before-ready" } else { "before-made" }, run)?;
            let (time, _) = timed(|| {
                let built = beside(&v2, hash, || Ok(list.build(&limits, drop)?))?;
                Ok(made.create(request(built, place()?))?)
            })?;
            times.push(time);
            kept.push(made);
        }
        // After: the list kept as the tree, written beside the walk, then created.
        let (walk, image) = timed(|| Ok(TreeImage::open(bytes.clone(), &limits, drop)?))?;
        let made = store("after", run)?;
        let (create, _) = timed(|| Ok(made.create_image(image_request(image, None, digest)?)?))?;
        after.walk.push(walk);
        after.create.push(create);
        kept.push(made);
        for (hash, times) in [(true, &mut after.ready), (false, &mut after.made)] {
            let made = store(if hash { "after-ready" } else { "after-made" }, run)?;
            let (time, _) = timed(|| {
                let (image, staged) = beside(&v2, hash, || {
                    std::thread::scope(|scope| {
                        let staging = scope.spawn(|| made.stage_image(bytes.clone()).ok());
                        let image = TreeImage::open(bytes.clone(), &limits, drop)?;
                        let staged = staging.join().map_err(|_| "the write panicked")?;
                        Ok((image, staged))
                    })
                })?;
                Ok(made.create_image(image_request(image, staged, digest)?)?)
            })?;
            times.push(time);
            kept.push(made);
        }
    }
    writeln!(out, "sha256 of the list: {:?}", median(hashes))?;
    writeln!(
        out,
        "{}",
        before.line("before (records)", "walk and records", "create")
    )?;
    writeln!(
        out,
        "{}",
        after.line("after (image)", "walk", "create_image")
    )?;
    writeln!(
        out,
        "image walked again (as the mount and a start do, with no span): {:?}",
        median(checks)
    )?;
    // The first store of the image, made fourth in the first run.
    let same = same_files(&list, kept.get(3))?;
    writeln!(out, "the same files in both: {same}")?;
    drop(kept);
    for entry in std::fs::read_dir(&cache)? {
        std::fs::remove_dir_all(entry?.path())?;
    }
    Ok(())
}

/// Whether every file the records of `list` have resolves, in the image tree placed in `store`,
/// to the same digest and size.
fn same_files(list: &TreeListV2<'_>, store: Option<&TreeStore>) -> Result<bool, Failure> {
    let store = store.ok_or(USAGE)?;
    let located = store
        .locate_fs("bench/tree")
        .ok_or("the tree is not placed")?;
    let built = list.build(&TreeLimits::default(), drop)?;
    for file in &built.files {
        let resolved = store.resolve(&located.placement, &file.path)?;
        let same = matches!(resolved, Resolved::File { size, sha256, .. }
            if (size, sha256) == (file.size, file.sha256));
        if !same {
            return Ok(false);
        }
    }
    Ok(true)
}

fn main() -> Result<(), Failure> {
    run(std::env::args().skip(1), &mut std::io::stdout())
}
