//! Measures the tree list, version 1 (JSON Lines) against version 2 (binary), on a real tree.
//!
//! ```text
//! cargo run --release -p tierstore --example tree_list_bench -- \
//!     [--runs N] [--pack-mib M] [--save LIST] (--from LIST | DIR...)
//! ```
//!
//! Each `DIR` is walked (links not followed, `.git` and anything that is not a file, a directory
//! or a link left out) and put in one tree under its own name; each file's bytes are hashed, and
//! the files are laid out in packs of `M` MiB (64) in the order of the walk, as a client that
//! packs a build's outputs would. `--save` writes that tree as a version 1 list, and `--from`
//! reads one in place of walking, so the same tree can be measured on another machine.
//!
//! Both lists of the tree are then encoded and read `N` times (25), and the median of each step
//! printed:
//!
//! - the size of each list, and the SHA-256 of each, which the reader checks against its key;
//! - version 1: the parse, and the parse and the tree built (the first span is read after both);
//! - version 2: the open; the open and the first range of the ranges table; the first span the
//!   walk hands out, and, for comparison, the same with the list's SHA-256 checked first (the
//!   mount provider does not: it hashes on another thread beside the walk and places the tree
//!   once the hash checks); the open and the walk that checks every entry, after which the list
//!   is a tree as it is; and the open, the walk and the tree store's records built and hashed.
//!
//! The trees each run builds are kept until the runs end: freeing a whole tree makes the next
//! small allocation pay to gather what was freed, a cost of the run before. Last, it says whether
//! the two lists make the same tree.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use tierstore::{
    BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeLimits, TreeList, TreeListV2,
    build_tree, parse_tree_list,
};

/// What the benchmark stops on: a bad flag, a walk that failed, or a list a reader refused.
pub type Failure = Box<dyn std::error::Error>;

/// How the benchmark is run.
const USAGE: &str =
    "usage: tree_list_bench [--runs N] [--pack-mib M] [--save LIST] (--from LIST | DIR...)";

/// The entries of the directories walked so far, and where the next file's bytes go.
struct Walk {
    entries: Vec<ListEntry>,
    pack: u64,
    pack_bytes: u64,
    offset: u64,
}

impl Walk {
    fn dir(&mut self, at: &Path, path: &str) -> std::io::Result<()> {
        let mut children: Vec<_> = std::fs::read_dir(at)?.collect::<Result<_, _>>()?;
        children.sort_by_key(std::fs::DirEntry::file_name);
        if children.is_empty() {
            self.entries.push(ListEntry {
                path: path.to_owned(),
                kind: ListKind::Dir { mode: Mode::DIR },
            });
        }
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            // A checkout's history is not an input of an action.
            if name == ".git" {
                continue;
            }
            let child_path = format!("{path}/{name}");
            let kind = child.file_type()?;
            if kind.is_dir() {
                self.dir(&child.path(), &child_path)?;
            } else if kind.is_symlink() {
                let target = std::fs::read_link(child.path())?;
                self.entries.push(ListEntry {
                    path: child_path,
                    kind: ListKind::Symlink {
                        target: target.to_string_lossy().into_owned(),
                    },
                });
            } else if kind.is_file() {
                self.file(&child.path(), child_path)?;
            }
        }
        Ok(())
    }

    fn file(&mut self, at: &Path, path: String) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let bytes = std::fs::read(at)?;
        let bits = std::fs::metadata(at)?.permissions().mode() & 0o7777;
        let mode = Mode::new(bits as u16).unwrap_or(Mode::FILE);
        let size = bytes.len() as u64;
        let sha256 = TreeDigest::from_bytes(Sha256::digest(&bytes).into());
        let source = (size > 0).then(|| {
            if self.offset + size > self.pack_bytes && self.offset > 0 {
                self.pack += 1;
                self.offset = 0;
            }
            let source = BlobSource {
                key: Arc::from(format!("out/{:016x}/pack", self.pack)),
                offset: self.offset,
            };
            self.offset += size;
            source
        });
        self.entries.push(ListEntry {
            path,
            kind: ListKind::File {
                mode,
                size,
                source,
                sha256: Some(sha256),
            },
        });
        Ok(())
    }
}

/// The tree of `dirs`, each under its own name, its files in packs of `pack_mib` MiB.
fn walk(dirs: &[PathBuf], pack_mib: u64) -> Result<TreeList, Failure> {
    if dirs.is_empty() {
        return Err(USAGE.into());
    }
    let mut walk = Walk {
        entries: Vec::new(),
        pack: 0,
        pack_bytes: pack_mib << 20,
        offset: 0,
    };
    for dir in dirs {
        let name = dir.file_name().ok_or(USAGE)?.to_string_lossy();
        walk.dir(dir, &name)?;
    }
    Ok(TreeList {
        entries: walk.entries,
    })
}

/// The median time of `runs` calls of `f`, and what the last one gave.
#[expect(
    clippy::disallowed_methods,
    reason = "a benchmark measures the machine's own time"
)]
fn time<T, E: Into<Failure>>(
    runs: usize,
    mut f: impl FnMut() -> Result<T, E>,
) -> Result<(Duration, T), Failure> {
    let mut times = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs {
        let start = Instant::now();
        let out = f().map_err(Into::into)?;
        times.push(start.elapsed());
        last = Some(out);
    }
    times.sort();
    let last = last.ok_or(USAGE)?;
    Ok((times[runs / 2], last))
}

/// The median time to the first span the walk hands out, the list's SHA-256 checked first if
/// `hash_first`.
#[expect(
    clippy::disallowed_methods,
    reason = "a benchmark measures the machine's own time"
)]
fn first_span(
    runs: usize,
    bytes: &[u8],
    limits: &TreeLimits,
    hash_first: bool,
) -> Result<Duration, Failure> {
    let mut times = Vec::with_capacity(runs);
    let mut kept = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        let mut first = None;
        std::hint::black_box(hash_first.then(|| TreeDigest::of(bytes)));
        let list = TreeListV2::open(bytes)?;
        let tree = list.build(limits, |_| {
            first.get_or_insert_with(|| start.elapsed());
        })?;
        times.push(first.ok_or("the list has no span to read")?);
        kept.push(tree);
    }
    times.sort();
    Ok(times[runs / 2])
}

/// Runs the benchmark with `args` (the flags, without the program's name), writing to `out`.
pub fn run(args: impl IntoIterator<Item = String>, out: &mut dyn Write) -> Result<(), Failure> {
    let mut runs: usize = 25;
    let mut pack_mib: u64 = 64;
    let (mut save, mut from) = (None::<PathBuf>, None::<PathBuf>);
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => runs = args.next().ok_or("--runs N")?.parse()?,
            "--pack-mib" => pack_mib = args.next().ok_or("--pack-mib M")?.parse()?,
            "--save" => save = Some(args.next().ok_or("--save LIST")?.into()),
            "--from" => from = Some(args.next().ok_or("--from LIST")?.into()),
            _ => dirs.push(arg.into()),
        }
    }
    if runs == 0 {
        return Err(USAGE.into());
    }
    let list = match from {
        Some(from) => {
            let bytes = std::fs::read(from)?;
            parse_tree_list(&bytes, &TreeDigest::of(&bytes))?
        }
        None => walk(&dirs, pack_mib)?,
    };
    let v1 = list.encode();
    if let Some(save) = &save {
        std::fs::write(save, &v1)?;
    }
    let v2 = list.encode_v2()?;
    let files = list
        .entries
        .iter()
        .filter(|e| matches!(e.kind, ListKind::File { .. }))
        .count();
    let limits = TreeLimits::default();
    let digest1 = TreeDigest::of(&v1);
    let v2_entries = TreeListV2::open(&v2)?.len();
    writeln!(
        out,
        "entries listed: {} ({files} files); version 2 entries: {v2_entries}",
        list.entries.len()
    )?;
    writeln!(out, "bytes: v1 {} B, v2 {} B", v1.len(), v2.len())?;
    let (hash1, _) = time(runs, || Ok::<_, Failure>(TreeDigest::of(&v1)))?;
    let (hash2, _) = time(runs, || Ok::<_, Failure>(TreeDigest::of(&v2)))?;
    writeln!(out, "sha256 of the list: v1 {hash1:?}, v2 {hash2:?}")?;
    let (parse1, _) = time(runs, || parse_tree_list(&v1, &digest1))?;
    let (built1, tree1) = time(runs, || -> Result<_, Failure> {
        let entries = parse_tree_list(&v1, &digest1)?.into_entries()?;
        Ok(build_tree(None, entries, &limits)?)
    })?;
    writeln!(
        out,
        "v1: parse (with its sha256) {parse1:?}; parse and tree {built1:?} = first span"
    )?;
    let (open2, _) = time(runs, || TreeListV2::open(&v2).map(|list| list.len()))?;
    let (range2, ranges) = time(runs, || -> Result<_, Failure> {
        let list = TreeListV2::open(&v2)?;
        let first = list.ranges().next().ok_or("the list has no range")?;
        Ok((first, list.ranges().count()))
    })?;
    let (checked, _) = time(runs, || -> Result<_, Failure> {
        TreeListV2::open(&v2)?.check(&limits, drop)?;
        Ok(())
    })?;
    let first2 = first_span(runs, &v2, &limits, false)?;
    let checked2 = first_span(runs, &v2, &limits, true)?;
    let (built2, (tree2, spans)) = time(runs, || -> Result<_, Failure> {
        let list = TreeListV2::open(&v2)?;
        let mut spans = 0usize;
        let tree = list.build(&limits, |_| spans += 1)?;
        Ok((tree, spans))
    })?;
    writeln!(
        out,
        "v2: open {open2:?}; first range {range2:?} ({} ranges); first span {first2:?}, \
         {checked2:?} with the sha256 first; walk (open and check) {checked:?}; walk and tree \
         {built2:?} ({spans} spans)",
        ranges.1
    )?;
    writeln!(
        out,
        "the same tree from both: {} ({})",
        tree1.root == tree2.root,
        tree2.root
    )?;
    Ok(())
}

fn main() -> Result<(), Failure> {
    run(std::env::args().skip(1), &mut std::io::stdout())
}
