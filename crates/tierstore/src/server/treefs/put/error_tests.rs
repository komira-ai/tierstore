//! The errno of each refusal of the put directory (the kbf design's mapping, section 3).

use std::io;
use std::path::PathBuf;

use rustix::io::Errno;

use super::error::{OpenError, PutError};
use super::testing::sha;

/// Asserts the errno a frontend answers for every refusal, and that an I/O error keeps its own
/// errno, or is `EIO` when it has none.
#[test]
fn each_refusal_has_its_errno() {
    let cases = [
        (PutError::InvalidName("x"), Errno::INVAL),
        (PutError::NameTooLong, Errno::NAMETOOLONG),
        (PutError::NotUnderPrefix, Errno::PERM),
        (PutError::NotPermitted("x"), Errno::PERM),
        (PutError::NotFound, Errno::NOENT),
        (PutError::Exists, Errno::EXIST),
        (PutError::ExistsDifferent, Errno::EXIST),
        (PutError::BadHandle, Errno::BADF),
        (PutError::NoSpace { free: 1, min: 2 }, Errno::NOSPC),
        (PutError::Quota("files"), Errno::DQUOT),
        (PutError::TooLarge, Errno::FBIG),
        (PutError::Busy("x"), Errno::BUSY),
        (
            PutError::DigestMismatch {
                expected: sha(b"a"),
                actual: sha(b"b"),
            },
            Errno::BADMSG,
        ),
        (PutError::Checksum, Errno::BADMSG),
        (PutError::Upstream, Errno::IO),
        (PutError::Io(io::Error::from_raw_os_error(28)), Errno::NOSPC),
        (PutError::Io(io::Error::other("no errno")), Errno::IO),
    ];
    for (error, errno) in cases {
        assert_eq!(error.errno(), errno, "{error}");
    }
}

/// Asserts the full text of the refusals that carry values, which an operator reads in a log.
#[test]
fn the_refusals_that_carry_values_say_them() {
    assert_eq!(
        PutError::NoSpace { free: 1, min: 2 }.to_string(),
        "the stage has 1 bytes free, under the 2 it keeps"
    );
    let mismatch = PutError::DigestMismatch {
        expected: sha(b"a"),
        actual: sha(b"b"),
    };
    assert_eq!(
        mismatch.to_string(),
        format!(
            "the bytes hash to {}, not to the {} the name expects",
            sha(b"b"),
            sha(b"a")
        )
    );
    let other = OpenError::OtherVolume {
        stage: PathBuf::from("/s"),
        cache: PathBuf::from("/c"),
    };
    assert_eq!(
        other.to_string(),
        "the stage directory /s is not on the volume of the cache /c"
    );
    assert_eq!(
        OpenError::Prefix("cas".to_owned()).to_string(),
        "fs.put.prefixes: \"cas\" is not a key prefix ending in `/`"
    );
}
