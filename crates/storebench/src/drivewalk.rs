//! The listing of silo read from one of its drives, not through its S3 API.
//!
//! Silo's listing of a bucket reads every object's `xl.meta` from the drives of its erasure set
//! before it answers; on the farm's SMR drives that is about 14 objects a second, a day and more
//! for silo's 1.5 M objects. Every drive of silo's one erasure set holds every object's
//! `xl.meta`, so the same facts are on each drive: this walk reads them straight from a drive's
//! file system, read-only, and writes the same listing lines as [`crate::listing`].
//!
//! **Shares.** Each of `n` walks (one per drive, run at once) reads a share of the objects, so
//! together they read each object once. A walk visits every directory above the objects, which
//! every walk shares; below a directory with at least [`SPLIT_FANOUT`] subdirectories, each
//! subdirectory belongs to one walk (`blake3(path)` modulo `n`), and so does everything under it.
//! An object directory (one holding `xl.meta`) that is still shared belongs to the walk its own
//! path hashes to. So each object is read by exactly one walk, whatever the shape of the tree.
//!
//! The walk does not follow symbolic links, does not leave the drive's file system, and never
//! descends into an object's directory (its data directories hold part files, not objects).
//! Every directory or `xl.meta` it cannot read is counted and named in the result, not fatal.

use std::io::Write;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::xlmeta::{self, Meta};

/// Subdirectories a directory must have for its subdirectories to be shared out between walks.
pub const SPLIT_FANOUT: usize = 32;

/// The flags of `drive-list`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The directory of one bucket on one of silo's drives (`<drive>/kbf-cas`).
    #[arg(long)]
    pub bucket_dir: PathBuf,
    /// The bucket's name, written as the listing's first field (default: the directory's name).
    #[arg(long)]
    pub bucket: Option<String>,
    /// This walk's share, `i/n`: the i-th of n walks run at once (default `0/1`, everything).
    #[arg(long, default_value = "0/1", value_parser = parse_share)]
    pub share: Share,
    /// Where the listing lines are written.
    #[arg(long)]
    pub out: PathBuf,
    /// Where the erasure facts of each object are written (`bucket key data parity parts
    /// inline versions`), for the raw-to-logical gap.
    #[arg(long)]
    pub meta_out: Option<PathBuf>,
}

/// One walk's share of the objects: the `index`-th of `of`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share {
    /// This walk, from 0.
    pub index: u64,
    /// How many walks share the objects.
    pub of: u64,
}

/// Parses `i/n` with `i < n`.
///
/// # Errors
/// The reason, as text, when it is not `i/n` with `0 <= i < n`.
pub fn parse_share(text: &str) -> Result<Share, String> {
    let (i, n) = text
        .split_once('/')
        .ok_or_else(|| format!("{text:?} is not i/n"))?;
    let index: u64 = i.parse().map_err(|_| format!("{i:?} is not a number"))?;
    let of: u64 = n.parse().map_err(|_| format!("{n:?} is not a number"))?;
    if index >= of {
        return Err(format!("share {index}/{of}: i must be below n"));
    }
    Ok(Share { index, of })
}

impl Share {
    /// Whether the directory at `rel` (relative to the bucket directory) belongs to this walk.
    #[must_use]
    pub fn owns(self, rel: &str) -> bool {
        let h = blake3::hash(rel.as_bytes());
        let mut first = [0u8; 8];
        first.copy_from_slice(&h.as_bytes()[..8]);
        u64::from_le_bytes(first) % self.of == self.index
    }
}

/// What a walk found.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Walked {
    /// Directories read.
    pub dirs: u64,
    /// Objects listed (their newest version is an object).
    pub objects: u64,
    /// Bytes of the objects listed.
    pub bytes: u64,
    /// Objects whose newest version is a delete marker (not listed).
    pub deleted: u64,
    /// Objects with more than one version.
    pub versioned: u64,
    /// Directories and files that could not be read, or `xl.meta` files that could not be
    /// parsed: the path relative to the bucket directory and the reason.
    pub errors: Vec<(String, String)>,
}

/// Where a walk writes.
pub struct Sinks<'a> {
    /// The listing lines.
    pub listing: &'a mut dyn Write,
    /// The erasure facts, if asked for.
    pub meta: Option<&'a mut dyn Write>,
}

/// One directory still to read: its path relative to the bucket directory, and whether this
/// walk owns it (else it is shared).
struct Pending {
    rel: String,
    owned: bool,
}

/// Walks the bucket directory `root`, writing a listing line (and an erasure line) for every
/// object of this walk's `share`.
///
/// # Errors
/// [`Error::Io`] when `root` cannot be read or a sink cannot be written; what cannot be read
/// below `root` is counted in [`Walked::errors`] instead.
pub fn walk(
    root: &Path,
    bucket: &str,
    share: Share,
    sinks: &mut Sinks<'_>,
) -> Result<Walked, Error> {
    // The bucket directory itself may be reached through a link; nothing below it is.
    let device = std::fs::metadata(root)
        .map_err(|e| Error::io("stat", root, e))?
        .dev();
    let mut walked = Walked::default();
    // The bucket directory must be readable; below it, what cannot be read is counted.
    let mut first = Some(read_dir(root).map_err(|e| Error::io("read", root, e))?);
    let mut stack = vec![Pending {
        rel: String::new(),
        owned: share.of == 1,
    }];
    while let Some(dir) = stack.pop() {
        let (entries, failed) = match first
            .take()
            .map_or_else(|| read_dir(&root.join(&dir.rel)), Ok)
        {
            Ok(read) => read,
            Err(e) => {
                walked.errors.push((dir.rel, e.to_string()));
                continue;
            }
        };
        // An entry that went away or cannot be looked at is counted; its siblings are walked.
        walked
            .errors
            .extend(failed.into_iter().map(|e| (dir.rel.clone(), e)));
        walked.dirs += 1;
        // The directories on the bucket directory's own file system.
        let subdirs: Vec<String> = entries
            .into_iter()
            .filter(|(_, meta)| meta.is_dir())
            .filter(|(_, meta)| meta.dev() == device)
            .map(|(name, _)| name)
            .collect();
        let split = !dir.owned && subdirs.len() >= SPLIT_FANOUT;
        for name in subdirs {
            let rel = if dir.rel.is_empty() {
                name
            } else {
                format!("{}/{name}", dir.rel)
            };
            let owned = dir.owned || (split && share.owns(&rel));
            if !dir.owned && split && !owned {
                continue;
            }
            // An object's directory holds `xl.meta`: a stat says whether it is one, and only
            // the walk the object belongs to reads the file.
            let xl = root.join(&rel).join("xl.meta");
            let mine = owned || share.owns(&rel);
            match std::fs::symlink_metadata(&xl) {
                Ok(_) if mine => match std::fs::read(&xl) {
                    Ok(bytes) => object(&rel, bucket, &bytes, &xl, sinks, &mut walked)?,
                    Err(e) => walked
                        .errors
                        .push((format!("{rel}/xl.meta"), e.to_string())),
                },
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    stack.push(Pending { rel, owned });
                }
                // Another walk names an object it owns.
                Err(e) => {
                    if mine {
                        walked
                            .errors
                            .push((format!("{rel}/xl.meta"), e.to_string()));
                    }
                }
            }
        }
    }
    walked.errors.sort();
    Ok(walked)
}

/// The directory's entries.
type Entries = Vec<(String, std::fs::Metadata)>;

/// The entries of a directory with their metadata (not followed), sorted by name, and the
/// errors of the entries that could not be read or looked at.
fn read_dir(path: &Path) -> std::io::Result<(Entries, Vec<String>)> {
    let mut out = Vec::new();
    let mut failed = Vec::new();
    for entry in std::fs::read_dir(path)? {
        match entry.and_then(|e| Ok((e.file_name().to_string_lossy().into_owned(), e.metadata()?)))
        {
            Ok(named) => out.push(named),
            Err(e) => failed.push(e.to_string()),
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok((out, failed))
}

/// Writes the lines of the object `key` whose `xl.meta` is `bytes`.
fn object(
    key: &str,
    bucket: &str,
    bytes: &[u8],
    path: &Path,
    sinks: &mut Sinks<'_>,
    walked: &mut Walked,
) -> Result<(), Error> {
    let o = match xlmeta::parse(bytes, path) {
        Ok(Meta::Object(o)) => o,
        Ok(Meta::Deleted { .. }) => {
            walked.deleted += 1;
            return Ok(());
        }
        Err(e) => {
            walked
                .errors
                .push((format!("{key}/xl.meta"), e.to_string()));
            return Ok(());
        }
    };
    walked.objects += 1;
    walked.bytes += o.size;
    walked.versioned += u64::from(o.versions > 1);
    writeln!(
        sinks.listing,
        "{bucket}\t{key}\t{}\t{}\t{}",
        o.size, o.modified_ms, o.etag
    )
    .map_err(|e| Error::io("write", "the listing", e))?;
    if let Some(meta) = sinks.meta.as_mut() {
        writeln!(
            meta,
            "{bucket}\t{key}\t{}\t{}\t{}\t{}\t{}",
            o.data,
            o.parity,
            o.parts,
            u8::from(o.inline),
            o.versions
        )
        .map_err(|e| Error::io("write", "the erasure facts", e))?;
    }
    Ok(())
}

/// The verb `drive-list`: walks the bucket directory and writes the listing (and the erasure
/// facts) to the files the flags name, then one summary line to `stdout`.
///
/// # Errors
/// As [`walk_into`], and [`Error::Io`] when an output cannot be created.
pub fn run(args: &Args, stdout: &mut dyn Write) -> Result<(), Error> {
    let mut listing = crate::listing::create(&args.out)?;
    let mut meta = match &args.meta_out {
        Some(p) => Some(crate::listing::create(p)?),
        None => None,
    };
    walk_into(
        args,
        &mut listing,
        meta.as_mut().map(|m| m as &mut dyn Write),
        stdout,
    )
}

/// The walk of [`run`] into the writers given: the listing, the erasure facts (if asked for),
/// and the summary line and each error on `stdout`.
///
/// # Errors
/// As [`walk`]; [`Error::Usage`] when the bucket is not named and the directory has no name;
/// [`Error::Io`] when a writer fails, flushing included.
pub fn walk_into(
    args: &Args,
    listing: &mut dyn Write,
    mut meta: Option<&mut dyn Write>,
    stdout: &mut dyn Write,
) -> Result<(), Error> {
    let bucket = match &args.bucket {
        Some(b) => b.clone(),
        None => args
            .bucket_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| {
                Error::Usage(format!("{} names no bucket", args.bucket_dir.display()))
            })?,
    };
    let walked = {
        let mut sinks = Sinks {
            listing: &mut *listing,
            meta: meta.as_mut().map(|m| &mut **m as &mut dyn Write),
        };
        walk(&args.bucket_dir, &bucket, args.share, &mut sinks)?
    };
    listing
        .flush()
        .map_err(|e| Error::io("write", &args.out, e))?;
    if let (Some(m), Some(p)) = (meta.as_mut(), &args.meta_out) {
        m.flush().map_err(|e| Error::io("write", p, e))?;
    }
    let out = |e| Error::io("write", "standard output", e);
    writeln!(
        stdout,
        "{} objects, {} bytes, {} deleted, {} with versions, {} directories, {} errors in {}",
        walked.objects,
        walked.bytes,
        walked.deleted,
        walked.versioned,
        walked.dirs,
        walked.errors.len(),
        args.out.display()
    )
    .map_err(out)?;
    for (path, why) in &walked.errors {
        writeln!(stdout, "error {path}: {why}").map_err(out)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "drivewalk_tests.rs"]
mod tests;
