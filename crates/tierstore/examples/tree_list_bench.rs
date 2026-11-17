//! Measures the tree list, version 1 (JSON Lines) against version 2 (binary), on a real tree.
//!
//! ```text
//! cargo run --release -p tierstore --example tree_list_bench -- [--runs N] [--pack-mib M] DIR...
//! ```
//!
//! Each `DIR` is walked (links not followed) and put in one tree under its own name; each file's
//! bytes are hashed, and the files are laid out in packs of `M` MiB (64) in the order of the walk,
//! as a client that packs a build's outputs would. Both lists of that tree are then encoded and
//! read `N` times (25), and the median of each step printed:
//!
//! - the size of each list;
//! - the SHA-256 of each list, which the reader checks against its key;
//! - version 1: the parse, and the parse and the tree built (the first span is read after both);
//! - version 2: the open; the first span handed out, without and with the list's SHA-256 checked
//!   first (as the mount provider does); and the whole walk with the tree built.
//!
//! The two trees are checked to have the same digest.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};
use tierstore::{
    BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeLimits, TreeList, TreeListV2,
    build_tree, parse_tree_list,
};

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
            // A checkout's history is not an input of an action.
            let Ok(name) = child.file_name().into_string() else {
                continue;
            };
            if name == ".git" {
                continue;
            }
            let child_path = format!("{path}/{name}");
            let meta = std::fs::symlink_metadata(child.path())?;
            if meta.is_dir() {
                self.dir(&child.path(), &child_path)?;
            } else if meta.is_symlink() {
                let target = std::fs::read_link(child.path())?;
                self.entries.push(ListEntry {
                    path: child_path,
                    kind: ListKind::Symlink {
                        target: target.to_string_lossy().into_owned(),
                    },
                });
            } else if meta.is_file() {
                self.file(&child.path(), child_path, &meta)?;
            }
        }
        Ok(())
    }

    fn file(&mut self, at: &Path, path: String, meta: &std::fs::Metadata) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let bytes = std::fs::read(at)?;
        let size = bytes.len() as u64;
        let mode = Mode::new((meta.permissions().mode() & 0o7777) as u16).unwrap_or(Mode::FILE);
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

/// What the benchmark stops on: a bad flag, a walk that failed, or a list a reader refused.
type Failure = Box<dyn std::error::Error>;

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
    let last = last.ok_or("--runs is at least 1")?;
    Ok((times[runs / 2], last))
}

/// The median time to the first span the walk hands out, the list's SHA-256 checked first or not.
#[expect(
    clippy::disallowed_methods,
    reason = "a benchmark measures the machine's own time"
)]
fn first_span(
    runs: usize,
    bytes: &[u8],
    limits: &TreeLimits,
    digest: bool,
) -> Result<Duration, Failure> {
    let mut times = Vec::with_capacity(runs);
    let expected = TreeDigest::of(bytes);
    for _ in 0..runs {
        let start = Instant::now();
        let mut first = None;
        if digest && TreeDigest::of(bytes) != expected {
            return Err("the list changed".into());
        }
        let list = TreeListV2::open(bytes)?;
        list.build(limits, |_| {
            first.get_or_insert_with(|| start.elapsed());
        })?;
        times.push(first.ok_or("the list has no span to read")?);
    }
    times.sort();
    Ok(times[runs / 2])
}

#[expect(
    clippy::disallowed_macros,
    reason = "a benchmark prints its results to the person who ran it"
)]
fn main() -> Result<(), Failure> {
    let mut runs: usize = 25;
    let mut pack_mib: u64 = 64;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => runs = args.next().ok_or("--runs N")?.parse()?,
            "--pack-mib" => pack_mib = args.next().ok_or("--pack-mib M")?.parse()?,
            _ => dirs.push(arg.into()),
        }
    }
    if dirs.is_empty() || runs == 0 {
        return Err("usage: tree_list_bench [--runs N] [--pack-mib M] DIR...".into());
    }
    let mut walk = Walk {
        entries: Vec::new(),
        pack: 0,
        pack_bytes: pack_mib << 20,
        offset: 0,
    };
    for dir in &dirs {
        let name = dir
            .file_name()
            .ok_or("a named directory")?
            .to_string_lossy();
        walk.dir(dir, &name)?;
    }
    let list = TreeList {
        entries: walk.entries,
    };
    let files = list
        .entries
        .iter()
        .filter(|e| matches!(e.kind, ListKind::File { .. }))
        .count();
    let v1 = list.encode();
    let v2 = list.encode_v2()?;
    let limits = TreeLimits::default();
    let digest1 = TreeDigest::of(&v1);
    println!(
        "entries listed: {} ({files} files, {} packs)",
        list.entries.len(),
        walk.pack + 1
    );
    println!("bytes: v1 {} B, v2 {} B", v1.len(), v2.len());
    let (hash1, _) = time(runs, || Ok::<_, Failure>(TreeDigest::of(&v1)))?;
    let (hash2, _) = time(runs, || Ok::<_, Failure>(TreeDigest::of(&v2)))?;
    println!("sha256 of the list: v1 {hash1:?}, v2 {hash2:?}");
    let (parse1, _) = time(runs, || parse_tree_list(&v1, &digest1))?;
    let (built1, tree1) = time(runs, || -> Result<_, Failure> {
        let entries = parse_tree_list(&v1, &digest1)?.into_entries()?;
        Ok(build_tree(None, entries, &limits)?)
    })?;
    println!("v1: parse (with its sha256) {parse1:?}; parse and tree {built1:?} = first span");
    let (open2, _) = time(runs, || TreeListV2::open(&v2).map(|list| list.len()))?;
    let first2 = first_span(runs, &v2, &limits, false)?;
    let checked2 = first_span(runs, &v2, &limits, true)?;
    let (built2, (tree2, spans)) = time(runs, || -> Result<_, Failure> {
        let list = TreeListV2::open(&v2)?;
        let mut spans = 0usize;
        let tree = list.build(&limits, |_| spans += 1)?;
        Ok((tree, spans))
    })?;
    println!(
        "v2: open {open2:?}; first span {first2:?}, with the sha256 first {checked2:?}; walk and tree {built2:?} ({spans} spans)"
    );
    if tree1.root != tree2.root {
        return Err(format!(
            "the lists make two trees: {} and {}",
            tree1.root, tree2.root
        )
        .into());
    }
    println!("tree {}: the same from both", tree1.root);
    Ok(())
}
