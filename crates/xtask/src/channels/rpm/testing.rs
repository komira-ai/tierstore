//! Stand-ins for the tests of the rpm verbs: the S3 and CloudFront of the apt tests, a `gpg` that
//! makes a signature packet of the shape GnuPG writes (and refuses a header it did not sign), and
//! the files of a release.

use std::cell::RefCell;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::Error;
use crate::channels::apt::sign::Gpg;
pub use crate::channels::apt::testing::{BUCKET, DENIED, FakeAws, etag, flag, refused};
use crate::channels::aws::Answer;
use crate::release::rpm;
use crate::sums;
use crate::testdir::TestDir;

/// The signing subkey of the tests.
pub const FPR: &str = "F767235D83B0410CFAF4779E8738AD1BE061EEDD";
/// Its key id.
pub const KEY_ID: &str = "8738AD1BE061EEDD";

pub fn scratch(name: &str) -> TestDir {
    let beside = std::env::current_exe().expect("test binary");
    TestDir::under(
        beside
            .parent()
            .expect("deps")
            .join("xtask-rpm-channel-unit"),
        name,
    )
}

/// A signature packet of version 4 as GnuPG writes it: an old-format header, the issuer
/// fingerprint among the hashed subpackets, the key id among the unhashed ones, and `mpi`.
pub fn packet(algorithm: u8, fingerprint: &str, mpi: &[u8]) -> Vec<u8> {
    let fpr = hex::decode(fingerprint).expect("a fingerprint");
    let mut hashed = vec![22, 33, 4];
    hashed.extend(&fpr);
    let mut unhashed = vec![9, 16];
    unhashed.extend(&fpr[12..]);
    let mut body = vec![4, 0, algorithm, 8];
    for area in [&hashed, &unhashed] {
        body.extend((area.len() as u16).to_be_bytes());
        body.extend(area.iter());
    }
    body.extend([0, 0]);
    body.extend(mpi);
    let mut out = vec![0x89];
    out.extend((body.len() as u16).to_be_bytes());
    out.extend(body);
    out
}

/// What the fake `gpg` signs `data` as.
fn signed_as(fingerprint: &str, data: &[u8]) -> Vec<u8> {
    packet(1, fingerprint, &Sha256::digest(data))
}

/// A `gpg` that signs and verifies in its own way: a detached binary signature is a packet naming
/// the key and holding the digest of the input; an armored one is a line of text.
#[derive(Default)]
pub struct FakeGpg {
    pub calls: RefCell<Vec<Vec<String>>>,
    /// An answer for every call whose arguments, joined by spaces, contain the text.
    pub refusals: RefCell<Vec<(String, String)>>,
    /// What `--detach-sign` writes in place of a signature, when set.
    pub garbage: RefCell<Option<Vec<u8>>>,
    /// The fingerprint a binary signature is made by, when it is not the key asked for.
    pub signs_as: RefCell<Option<String>>,
}

impl FakeGpg {
    pub fn refuse_when(&self, text: &str, stderr: &str) {
        self.refusals
            .borrow_mut()
            .push((text.to_owned(), stderr.to_owned()));
    }

    pub fn calls_with(&self, word: &str) -> Vec<Vec<String>> {
        self.calls
            .borrow()
            .iter()
            .filter(|c| c.iter().any(|a| a == word))
            .cloned()
            .collect()
    }
}

impl Gpg for FakeGpg {
    fn run(&self, args: &[String]) -> Result<Answer, Error> {
        self.calls.borrow_mut().push(args.to_vec());
        let joined = args.join(" ");
        if let Some((_, stderr)) = self
            .refusals
            .borrow()
            .iter()
            .find(|(t, _)| joined.contains(t))
        {
            return Ok(refused(stderr));
        }
        let has = |w: &str| args.iter().any(|a| a == w);
        let ok = Answer {
            ok: true,
            stdout: "ok".to_owned(),
            stderr: String::new(),
        };
        if has("--detach-sign") {
            let user = flag(args, "--local-user").expect("a signing key");
            let data = std::fs::read(args.last().expect("an input")).expect("input");
            let out = flag(args, "--output").expect("an output");
            let bytes = match (self.garbage.borrow().clone(), has("--armor")) {
                (Some(garbage), _) => garbage,
                (None, true) => format!(
                    "-----BEGIN PGP SIGNATURE-----\n{user} {}\n",
                    hex::encode(Sha256::digest(&data))
                )
                .into_bytes(),
                (None, false) => {
                    let by = self.signs_as.borrow().clone();
                    signed_as(
                        by.as_deref().unwrap_or_else(|| user.trim_end_matches('!')),
                        &data,
                    )
                }
            };
            std::fs::write(out, bytes).expect("write");
        } else if has("--verify") {
            let signature = std::fs::read(&args[args.len() - 2]).expect("signature");
            let data = std::fs::read(&args[args.len() - 1]).expect("data");
            let fits = if signature.starts_with(b"-----") {
                String::from_utf8_lossy(&signature).contains(&hex::encode(Sha256::digest(&data)))
            } else {
                signature.get(signature.len().saturating_sub(32)..)
                    == Some(&Sha256::digest(&data)[..])
            };
            if !fits {
                return Ok(refused("gpg: BAD signature"));
            }
        }
        Ok(ok)
    }
}

/// The files of release `version`: its two RPM packages (different bytes for each architecture),
/// and the `SHA256SUMS` that lists them.
pub fn release_files(dir: &Path, version: &str) {
    let mut lines = std::fs::read_to_string(dir.join(sums::SUMS))
        .ok()
        .and_then(|t| sums::parse(&t).ok())
        .unwrap_or_default();
    for arch in ["x86_64", "aarch64"] {
        let files = rpm::files(arch.as_bytes(), b"notice");
        let bytes = rpm::build(version, arch, &files).expect("build");
        let name = rpm::file_name(version, arch);
        std::fs::write(dir.join(&name), &bytes).expect("write");
        lines.push(sums::Line {
            sha256: crate::fetch::Sha256::of(&bytes),
            name,
        });
    }
    std::fs::write(dir.join(sums::SUMS), sums::render(&lines)).expect("sums");
}

/// A curl that serves `bytes` for every url, or refuses.
#[derive(Default)]
pub struct FakeCurl {
    pub calls: RefCell<Vec<Vec<String>>>,
    pub serves: RefCell<Option<Vec<u8>>>,
}

impl crate::channels::check::Host for FakeCurl {
    fn run(&self, _program: &Path, args: &[String]) -> Result<Answer, Error> {
        self.calls.borrow_mut().push(args.to_vec());
        match self.serves.borrow().as_ref() {
            Some(bytes) => {
                std::fs::write(flag(args, "--output").expect("an output"), bytes).expect("write");
                Ok(Answer {
                    ok: true,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            }
            None => Ok(refused("curl: (22) The requested URL returned error: 404")),
        }
    }
}

/// The request `rpm sign` is asked for `version` in `root`: the release's files in `root/dist`,
/// the signed packages to `root/signed`.
pub struct Signing {
    pub root: TestDir,
}

impl Signing {
    pub fn new(name: &str, version: &str) -> Signing {
        let root = scratch(name);
        std::fs::create_dir_all(root.join("dist")).expect("dist");
        release_files(&root.join("dist"), version);
        std::fs::write(root.join("key.asc"), "PRIVATE KEY").expect("key");
        std::fs::write(root.join("pass"), "secret").expect("pass");
        Signing { root }
    }

    pub fn request<'a>(
        &'a self,
        version: &'a str,
        fingerprints: &'a str,
    ) -> crate::channels::rpm::sign::Request<'a> {
        let leak = |p: std::path::PathBuf| -> &'a Path { Box::leak(p.into_boxed_path()) };
        crate::channels::rpm::sign::Request {
            rpms: leak(self.root.join("dist")),
            out: leak(self.root.join("signed")),
            version,
            sums: leak(self.root.join("dist").join(sums::SUMS)),
            work: leak(self.root.join("work")),
            fingerprints,
            key_file: leak(self.root.join("key.asc")),
            passphrase_file: leak(self.root.join("pass")),
            signed_at: 1_794_200_000,
        }
    }

    /// Signs the release with the fake gpg.
    pub fn sign(&self, version: &str) {
        crate::channels::rpm::sign::sign(&FakeGpg::default(), &self.request(version, FPR), None)
            .expect("sign");
    }
}

/// A repository in memory, and the releases that are published into it.
pub struct World {
    pub root: TestDir,
    pub aws: FakeAws,
    pub curl: FakeCurl,
    steps: std::cell::Cell<u32>,
}

impl World {
    pub fn new(name: &str) -> World {
        World {
            root: scratch(name),
            aws: FakeAws::new(),
            curl: FakeCurl::default(),
            steps: std::cell::Cell::new(0),
        }
    }

    /// A directory of its own for one step.
    pub fn dir(&self, what: &str) -> std::path::PathBuf {
        self.steps.set(self.steps.get() + 1);
        self.root.join(format!("{}-{what}", self.steps.get()))
    }

    /// The release `version`: its files in `dist-<version>`, signed into `signed-<version>`.
    pub fn release(&self, version: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let (dist, signed) = (
            self.root.join(format!("dist-{version}")),
            self.root.join(format!("signed-{version}")),
        );
        if !dist.exists() {
            std::fs::create_dir_all(&dist).expect("dist");
            release_files(&dist, version);
            let key = self.root.join(format!("key-{version}"));
            let pass = self.root.join(format!("pass-{version}"));
            std::fs::write(&key, "KEY").expect("key");
            std::fs::write(&pass, "secret").expect("pass");
            crate::channels::rpm::sign::sign(
                &FakeGpg::default(),
                &crate::channels::rpm::sign::Request {
                    rpms: &dist,
                    out: &signed,
                    version,
                    sums: &dist.join(sums::SUMS),
                    work: &self.root.join(format!("work-{version}")),
                    fingerprints: FPR,
                    key_file: &key,
                    passphrase_file: &pass,
                    signed_at: 1_794_200_000,
                },
                None,
            )
            .expect("sign");
        }
        (dist, signed)
    }

    pub fn read(&self, suite: crate::channels::apt::state::Suite) -> std::path::PathBuf {
        let out = self.dir("read");
        crate::channels::rpm::read::read(&self.aws, suite, BUCKET, &out).expect("read");
        out
    }

    pub fn index(
        &self,
        suite: crate::channels::apt::state::Suite,
        current: &Path,
        version: &str,
    ) -> std::path::PathBuf {
        let out = self.dir("next");
        let (_, signed) = self.release(version);
        crate::channels::rpm::index::build(&crate::channels::rpm::index::Request {
            suite,
            current,
            out: &out,
            rpms: &signed,
            version,
            date: 1_794_300_000,
            keep: 25,
        })
        .expect("index");
        out
    }

    /// Signs the `repomd.xml` of each architecture below `next/<suite>`.
    pub fn sign_repo(&self, suite: crate::channels::apt::state::Suite, next: &Path) {
        let key = self.root.join("repo-key");
        let pass = self.root.join("repo-pass");
        std::fs::write(&key, "KEY").expect("key");
        std::fs::write(&pass, "secret").expect("pass");
        crate::channels::rpm::repo_sign::sign(
            &FakeGpg::default(),
            &crate::channels::rpm::repo_sign::Request {
                dir: &next.join(suite.name()),
                work: &self.root.join("repo-work"),
                fingerprints: FPR,
                key_file: &key,
                passphrase_file: &pass,
            },
            None,
        )
        .expect("sign");
    }

    pub fn write(
        &self,
        suite: crate::channels::apt::state::Suite,
        next: &Path,
        version: Option<&str>,
        last: bool,
    ) -> Result<Vec<String>, Error> {
        let signed = version.map(|v| self.release(v).1);
        crate::channels::rpm::write::write(
            &self.aws,
            &self.curl,
            &crate::channels::rpm::write::Request {
                suite,
                bucket: BUCKET,
                distribution_id: "E1ABC",
                hostname: "packages-test.example",
                next,
                pool: signed
                    .as_deref()
                    .zip(version)
                    .map(|(rpms, version)| crate::channels::rpm::write::Pool { rpms, version }),
                last,
                curl: Path::new("curl"),
            },
            None,
        )
    }

    /// Publishes `version` into `suite` the way the three jobs do: read, merge, sign, write.
    pub fn publish(&self, suite: crate::channels::apt::state::Suite, version: &str) {
        let current = self.read(suite);
        let next = self.index(suite, &current, version);
        self.sign_repo(suite, &next);
        self.write(suite, &next, Some(version), false)
            .expect("write");
    }
}
