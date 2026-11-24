//! The git directory that names the checkout's commit in the input root.

use std::io::Read as _;

use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// Catches HEAD not detached at the commit (git would read it as a ref name), the object not at
/// git's loose path or not as git stores it (zlib of `commit <size>\0<body>`), and `refs/` left
/// out (git takes no directory without it as a repository).
#[test]
fn the_git_directory_is_head_the_commit_and_a_ref() {
    let got = entries(COMMIT, b"tree x\n\nmessage\n").expect("entries");
    let paths: Vec<&str> = got.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        [
            ".git/HEAD",
            ".git/objects/01/23456789abcdef0123456789abcdef01234567",
            ".git/refs/mac-farm/head",
        ]
    );
    let bytes = |e: &Entry| match e {
        Entry::File { bytes, executable } => {
            assert!(!executable);
            bytes.clone()
        }
        Entry::Symlink(_) => panic!("a file"),
    };
    let line = format!("{COMMIT}\n").into_bytes();
    assert_eq!(bytes(&got[0].1), line);
    assert_eq!(bytes(&got[2].1), line);
    let mut object = Vec::new();
    flate2::read::ZlibDecoder::new(bytes(&got[1].1).as_slice())
        .read_to_end(&mut object)
        .expect("zlib");
    assert_eq!(object, b"commit 16\0tree x\n\nmessage\n");
}

/// Catches a HEAD that is not a full SHA-1 (an abbreviation, a ref name, other case) written as
/// one: git would not read it as the commit.
#[test]
fn a_head_that_is_not_a_full_sha_is_refused() {
    for commit in [&COMMIT[..12], "refs/heads/main", &COMMIT.to_uppercase()] {
        assert_eq!(
            entries(commit, b"").expect_err("not a SHA-1").to_string(),
            format!(
                "git names the checkout's HEAD {commit:?}, which is not 40 lowercase hex digits"
            )
        );
    }
}
