//! The names under the put root: where a path is (above the jobs' directories, a job's directory
//! `cas/b/<build>/<nonce>`, or a file of one), and what a file's name says (a `.part` name, a `.part`
//! name that declares the SHA-256 its committed name must have, or a committed name).

use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::TreeDigest;

/// The components above a job's directory that are not the build's: `cas/b`.
pub(super) const PREFIX: [&str; 2] = ["cas", "b"];

/// The most bytes of one component (`NAME_MAX`).
const MAX_NAME_BYTES: usize = 255;

/// The ending of a name that is not committed.
const PART: &str = ".part";

/// What comes before the hex of a declared digest in a `.part` name.
const DIGEST_MARK: &str = ".sha256-";

/// Whether `name` may be one component of a key: 1 to [`MAX_NAME_BYTES`] bytes, not `.` or `..`,
/// and no `/`, NUL, white space or control character (the rules of the put directory's names).
pub(super) fn is_component(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= MAX_NAME_BYTES
        && !name
            .chars()
            .any(|c| c == '/' || c.is_whitespace() || c.is_control())
}

/// The directory of one job: `cas/b/<build>/<nonce>` under the root, each component checked.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct JobDir {
    build: String,
    nonce: String,
}

impl JobDir {
    /// The directory of `build` and `nonce`, or none when either is not a component.
    pub(super) fn new(build: &str, nonce: &str) -> Option<Self> {
        (is_component(build) && is_component(nonce)).then(|| Self {
            build: build.to_owned(),
            nonce: nonce.to_owned(),
        })
    }

    /// The build's directory under the root: `cas/b/<build>`.
    pub(super) fn build_dir(&self) -> PathBuf {
        let [cas, b] = PREFIX;
        [cas, b, self.build.as_str()].iter().collect()
    }

    /// The directory under the root: `cas/b/<build>/<nonce>`.
    pub(super) fn rel(&self) -> PathBuf {
        self.build_dir().join(&self.nonce)
    }

    /// The key of the file `name` of the directory, `cas/b/<build>/<nonce>/<name>`: at most 773
    /// bytes, three components and the prefix, so within the 1024 bytes of an S3 key.
    pub(super) fn key(&self, name: &str) -> String {
        format!("{self}/{name}")
    }
}

impl fmt::Display for JobDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [cas, b] = PREFIX;
        write!(f, "{cas}/{b}/{}/{}", self.build, self.nonce)
    }
}

/// Where a path under the root is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// The root, `cas`, `cas/b` or a build's directory: the number of components (0 to 3).
    Above(usize),
    /// A job's directory.
    Job(JobDir),
    /// The file of that name in a job's directory.
    File(JobDir, String),
    /// Anything else: a name of the wrong shape, or a path deeper than a job's files.
    Elsewhere,
}

/// Where the path `rel`, relative to the root, is.
pub(super) fn place(rel: &Path) -> Place {
    let mut parts = Vec::new();
    for component in rel.components() {
        let Component::Normal(name) = component else {
            return Place::Elsewhere;
        };
        let Some(name) = name.to_str().filter(|name| is_component(name)) else {
            return Place::Elsewhere;
        };
        parts.push(name);
    }
    if parts.iter().zip(PREFIX).any(|(part, want)| *part != want) {
        return Place::Elsewhere;
    }
    // Every part is a component already.
    let job = || JobDir {
        build: parts[2].to_owned(),
        nonce: parts[3].to_owned(),
    };
    match parts.len() {
        depth @ 0..=3 => Place::Above(depth),
        4 => Place::Job(job()),
        5 => Place::File(job(), parts[4].to_owned()),
        _ => Place::Elsewhere,
    }
}

/// What a file's name says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Name {
    /// A `.part` name that declares no digest: never stored.
    Part,
    /// `<name>.sha256-<64 lower-case hex>.part`: the committed `name` must hash to `sha256`.
    Declares {
        /// The committed name the declaration is for.
        name: String,
        /// The SHA-256 its bytes must have.
        sha256: TreeDigest,
    },
    /// A name without `.part`: committed, and stored once its digest is declared.
    Committed,
}

/// What the file name `name` says.
pub(super) fn name_of(name: &str) -> Name {
    let Some(stem) = name.strip_suffix(PART) else {
        return Name::Committed;
    };
    let declared = stem.rsplit_once(DIGEST_MARK).and_then(|(base, hex)| {
        let sha256 = TreeDigest::from_hex(hex)?;
        is_component(base).then(|| (base.to_owned(), sha256))
    });
    match declared {
        Some((name, sha256)) if !name.ends_with(PART) => Name::Declares { name, sha256 },
        _ => Name::Part,
    }
}
