//! Measures the L2 tree list on a real tree: its writer, its open and walk, and the cost of cell
//! digests to the writer of packs.
//!
//! ```text
//! cargo run --release -p tierstore-list --example l2_bench -- \
//!     [--runs N] [--pack-mib M] (--from LIST | DIR...)
//! ```
//!
//! Each `DIR` is walked (links not followed, `.git` and anything that is not a file, a directory
//! or a link left out) and put in one tree under its own name, the files laid out in packs of `M`
//! MiB (64) in the order of the walk, as `tierstore`'s `tree_list_bench` does. Each file's bytes
//! are hashed whole (its SHA-256, as a list has today) and, in pack order, as the cells of its pack
//! ([`CellHasher`]): the two rates say what cell digests add to the CPU of a writer that hashes a
//! pack as it uploads it. `--from` reads a version 1 list saved by `tree_list_bench --save` in
//! place of walking; the packs' bytes are then not on the machine, so each pack's size is where its
//! last file ends and its cell digests are stand-ins (the SHA-256 of the pack's number and the
//! cell's): the open and the walk read the digests' places, never their values.
//!
//! The tree's version 2 list is made by `tierstore`'s writer, its L2 sidecar by
//! [`sidecar_from_v2`]; then each step runs `N` times (25) from the bytes in memory and its median
//! is printed:
//!
//! - the sidecar written, and the SHA-256 of the version 2 list (what a node checks today);
//! - the open: the header, sections, packs, cells, ranges and the walk that checks every entry;
//! - the tree id checked: the version 2 projection hashed; and both, as `open_sidecar` does;
//! - a walk of the tree from the root, every entry read through `children` and `node`;
//! - every path looked up from the root, name by name, with `dir_lookup`;
//! - the open and every pack's `pack_root` (computed on first ask).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ring::digest::{SHA256, digest};
use tierstore::{BlobSource, ListEntry, ListKind, Mode, TreeDigest, TreeList, parse_tree_list};
use tierstore_fetch_api::list::{ListL2, NodeIndex};
use tierstore_list::{
    CELL_BYTES, CellHasher, NodeKind, PackCells, PackInfo, TreeListL2, sidecar_from_v2,
};

/// What the benchmark stops on: a bad flag, a walk that failed, or a list a reader refused.
pub type Failure = Box<dyn std::error::Error>;

/// How the benchmark is run.
const USAGE: &str = "usage: l2_bench [--runs N] [--pack-mib M] (--from LIST | DIR...)";

/// The time two hashes of the walked bytes took.
#[derive(Default)]
struct Hashed {
    bytes: u64,
    files: Duration,
    cells: Duration,
}

/// The entries of the directories walked so far, the cells of each pack, and where the next
/// file's bytes go.
struct Walk {
    entries: Vec<ListEntry>,
    packs: BTreeMap<String, CellHasher>,
    pack: u64,
    pack_bytes: u64,
    offset: u64,
    hashed: Hashed,
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

    #[expect(
        clippy::disallowed_methods,
        reason = "a benchmark measures the machine's own time"
    )]
    fn file(&mut self, at: &Path, path: String) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;
        let bytes = std::fs::read(at)?;
        let bits = std::fs::metadata(at)?.permissions().mode() & 0o7777;
        let mode = Mode::new(bits as u16).unwrap_or(Mode::FILE);
        let size = bytes.len() as u64;
        let start = Instant::now();
        let sha256 = sha(&bytes);
        self.hashed.files += start.elapsed();
        let source = (size > 0).then(|| {
            if self.offset + size > self.pack_bytes && self.offset > 0 {
                self.pack += 1;
                self.offset = 0;
            }
            let key = format!("out/{:016x}/pack", self.pack);
            let start = Instant::now();
            self.packs.entry(key.clone()).or_default().update(&bytes);
            self.hashed.cells += start.elapsed();
            self.hashed.bytes += size;
            let source = BlobSource {
                key: Arc::from(key),
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
                sha256: Some(TreeDigest::from_bytes(sha256)),
            },
        });
        Ok(())
    }
}

/// The SHA-256 of `bytes`.
fn sha(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0; 32];
    out.copy_from_slice(digest(&SHA256, bytes).as_ref());
    out
}

/// The tree of `dirs`, each under its own name, its files in packs of `pack_mib` MiB; the cells of
/// each pack; and what hashing the bytes took.
fn walk(
    dirs: &[PathBuf],
    pack_mib: u64,
) -> Result<(TreeList, BTreeMap<String, PackCells>, Hashed), Failure> {
    if dirs.is_empty() {
        return Err(USAGE.into());
    }
    let mut walk = Walk {
        entries: Vec::new(),
        packs: BTreeMap::new(),
        pack: 0,
        pack_bytes: pack_mib << 20,
        offset: 0,
        hashed: Hashed::default(),
    };
    for dir in dirs {
        let name = dir.file_name().ok_or(USAGE)?.to_string_lossy();
        walk.dir(dir, &name)?;
    }
    let packs = walk
        .packs
        .into_iter()
        .map(|(key, hasher)| (key, hasher.finish()))
        .collect();
    let list = TreeList {
        entries: walk.entries,
    };
    Ok((list, packs, walk.hashed))
}

/// Stand-in cells of the packs a list names, whose bytes are not here: each pack as long as its
/// last file's end, each cell's digest the SHA-256 of the pack's and the cell's numbers.
fn stand_in(list: &TreeList) -> BTreeMap<String, PackCells> {
    let mut sizes: BTreeMap<String, u64> = BTreeMap::new();
    for entry in &list.entries {
        if let ListKind::File {
            size,
            source: Some(source),
            ..
        } = &entry.kind
        {
            let end = sizes.entry(source.key.to_string()).or_default();
            *end = (*end).max(source.offset + size);
        }
    }
    sizes
        .into_iter()
        .enumerate()
        .map(|(pack, (key, size))| {
            let digests = (0..size.div_ceil(CELL_BYTES))
                .map(|cell| sha(&[(pack as u64).to_le_bytes(), cell.to_le_bytes()].concat()))
                .collect();
            (key, PackCells { size, digests })
        })
        .collect()
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

/// Every entry of `list` from the root, breadth first, through `children` and `node`: the count of
/// entries and of the bytes of their files.
fn walk_all(list: &TreeListL2<&[u8]>) -> (u64, u64) {
    let mut queue = vec![NodeIndex(0)];
    let (mut entries, mut bytes) = (0u64, 0u64);
    while let Some(at) = queue.pop() {
        entries += 1;
        match list.node(at).map(|node| node.kind) {
            Some(NodeKind::File { size, .. }) => bytes += size,
            Some(NodeKind::Dir { .. }) => {
                queue.extend(list.children(at).into_iter().flatten().map(NodeIndex));
            }
            _ => {}
        }
    }
    (entries, bytes)
}

/// The names of the path of every entry but the root, read once outside the timing.
fn paths(list: &TreeListL2<&[u8]>) -> Vec<Vec<Vec<u8>>> {
    let mut paths: Vec<Vec<Vec<u8>>> = vec![Vec::new(); list.len() as usize];
    let nodes = (1..u64::from(list.len())).filter_map(|at| Some((at, list.node(NodeIndex(at))?)));
    for (at, node) in nodes {
        let mut path = paths[node.parent.0 as usize].clone();
        path.push(node.name.to_vec());
        paths[at as usize] = path;
    }
    paths.remove(0);
    paths
}

/// Looks every path up from the root, name by name: how many were found.
fn look_up(list: &TreeListL2<&[u8]>, paths: &[Vec<Vec<u8>>]) -> usize {
    paths
        .iter()
        .filter(|path| {
            path.iter()
                .try_fold(NodeIndex(0), |at, name| list.dir_lookup(at, name))
                .is_some()
        })
        .count()
}

/// Runs the benchmark with `args` (the flags, without the program's name), writing to `out`.
pub fn run(args: impl IntoIterator<Item = String>, out: &mut dyn Write) -> Result<(), Failure> {
    let mut runs: usize = 25;
    let mut pack_mib: u64 = 64;
    let mut from = None::<PathBuf>;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--runs" => runs = args.next().ok_or("--runs N")?.parse()?,
            "--pack-mib" => pack_mib = args.next().ok_or("--pack-mib M")?.parse()?,
            "--from" => from = Some(args.next().ok_or("--from LIST")?.into()),
            _ => dirs.push(arg.into()),
        }
    }
    if runs == 0 {
        return Err(USAGE.into());
    }
    let (list, packs) = match from {
        Some(from) => {
            let bytes = std::fs::read(from)?;
            let list = parse_tree_list(&bytes, &TreeDigest::of(&bytes))?;
            let packs = stand_in(&list);
            writeln!(out, "cells: stand-ins (the packs' bytes are not here)")?;
            (list, packs)
        }
        None => {
            let (list, packs, hashed) = walk(&dirs, pack_mib)?;
            let rate = |took: Duration| hashed.bytes as f64 / took.as_secs_f64().max(1e-9) / 1e6;
            writeln!(
                out,
                "hashed {} B: whole files {:?} ({:.0} MB/s), cells {:?} ({:.0} MB/s)",
                hashed.bytes,
                hashed.files,
                rate(hashed.files),
                hashed.cells,
                rate(hashed.cells)
            )?;
            (list, packs)
        }
    };
    let v2 = list.encode_v2()?;
    let info = |key: &str| {
        packs.get(key).map(|cells| PackInfo {
            cells: cells.clone(),
            upload_part_size: 0,
            shared: false,
        })
    };
    let (write, l2) = time(runs, || sidecar_from_v2(&v2, info))?;
    let id = sha(&v2);
    let opened = TreeListL2::open_sidecar(Some(&l2[..]), &id)?;
    let cells: u64 = packs.values().map(|p| p.digests.len() as u64).sum();
    writeln!(
        out,
        "entries: {} ({} files); packs: {}; cells: {cells}",
        opened.len(),
        opened.shape().files,
        opened.pack_count()
    )?;
    writeln!(out, "bytes: v2 {} B, L2 {} B", v2.len(), l2.len())?;
    let (hash, _) = time(runs, || Ok::<_, Failure>(sha(&v2)))?;
    writeln!(
        out,
        "write: the sidecar {write:?}; sha256 of the version 2 list {hash:?}"
    )?;
    let (open, _) = time(runs, || TreeListL2::open(&l2[..]).map(|list| list.len()))?;
    let (check, _) = time(runs, || opened.check_tree_id())?;
    let (sidecar, _) = time(runs, || {
        TreeListL2::open_sidecar(Some(&l2[..]), &id).map(|list| list.len())
    })?;
    writeln!(
        out,
        "open {open:?}; tree id checked {check:?}; open_sidecar (both) {sidecar:?}"
    )?;
    let (walked, (entries, bytes)) = time(runs, || Ok::<_, Failure>(walk_all(&opened)))?;
    let paths = paths(&opened);
    let (lookups, found) = time(runs, || Ok::<_, Failure>(look_up(&opened, &paths)))?;
    let (roots, _) = time(runs, || -> Result<_, Failure> {
        let list = TreeListL2::open(&l2[..])?;
        Ok((0..list.pack_count())
            .filter_map(|pack| list.pack_root(pack))
            .count())
    })?;
    writeln!(
        out,
        "walk {walked:?} ({entries} entries, {bytes} B); every path looked up {lookups:?} \
         ({found} of {}); open and every pack_root {roots:?}",
        paths.len()
    )?;
    Ok(())
}

fn main() -> Result<(), Failure> {
    run(std::env::args().skip(1), &mut std::io::stdout())
}
