//! What the rig reads of a start line and of `/proc/<pid>/status`, and how it fills a disk, on
//! texts and writers of the tests (the processes and the file systems themselves are in
//! `tests/os_integration.rs`).

use super::*;

/// Asserts that the address of a start line is the word after `listen=`, wherever the word is,
/// for tierstore's line and fake-s3's, and that a line with no such word, or an empty one, names
/// none.
#[test]
fn the_address_of_a_start_line_is_its_listen_word() {
    assert_eq!(
        listen_of("tierstore 0.3.206 listen=127.0.0.1:38797 mode=read-cache commit=70ea714a7a44"),
        Some("127.0.0.1:38797")
    );
    assert_eq!(
        listen_of("fake-s3 listen=127.0.0.1:9 control=/s/up.sock"),
        Some("127.0.0.1:9")
    );
    assert_eq!(listen_of("tierstore: local cache at /c: read-write"), None);
    assert_eq!(listen_of("tierstore listen= mode=x"), None);
    assert_eq!(listen_of(""), None);
}

/// Asserts that the resident memory is the `VmRSS` line's kibibytes in bytes, and that a status
/// with no such line, a number that is not one, or another unit, names none.
#[test]
fn the_resident_memory_is_the_vmrss_line() {
    let status = "Name:\ttierstore\nVmPeak:\t  900 kB\nVmRSS:\t   12345 kB\nThreads:\t9\n";
    assert_eq!(vm_rss(status), Some(12345 * 1024));
    assert_eq!(vm_rss("Name:\tx\n"), None);
    assert_eq!(vm_rss("VmRSS:\tmany kB\n"), None);
    assert_eq!(vm_rss("VmRSS:\t12 MB\n"), None);
    assert_eq!(vm_rss("VmRSS:\n"), None);
}

/// A writer that takes `room` bytes, in writes of at most `most`, then answers as `full` says;
/// interrupted once first when `interrupt` is set.
struct Disk {
    room: usize,
    most: usize,
    interrupt: bool,
    full: fn() -> io::Result<usize>,
    taken: usize,
}

impl Write for Disk {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if std::mem::take(&mut self.interrupt) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let n = buf.len().min(self.most).min(self.room - self.taken);
        if n == 0 {
            return (self.full)();
        }
        self.taken += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn disk(room: usize, most: usize, full: fn() -> io::Result<usize>) -> Disk {
    Disk {
        room,
        most,
        interrupt: false,
        full,
        taken: 0,
    }
}

/// Asserts that a fill writes until the disk is full, counting every byte of a short last
/// write: to `ENOSPC`, to a write of no byte, and through an interrupted write.
#[test]
fn a_fill_writes_until_no_byte_more_fits() {
    let enospc = || Err(io::ErrorKind::StorageFull.into());
    let room = 3 * FILL_CHUNK + 17;
    assert_eq!(
        fill_with(disk(room, usize::MAX, enospc)).unwrap(),
        room as u64
    );
    assert_eq!(fill_with(disk(room, 4096, || Ok(0))).unwrap(), room as u64);
    let mut interrupted = disk(5, 2, enospc);
    interrupted.interrupt = true;
    assert_eq!(fill_with(interrupted).unwrap(), 5);
}

/// Asserts that a write that fails other than for a full disk ends the fill with its error.
#[test]
fn a_fill_stops_at_any_other_failure() {
    let refused = fill_with(disk(10, 4, || Err(io::ErrorKind::PermissionDenied.into())));
    assert_eq!(refused.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    // The disk of these tests keeps nothing back.
    assert!(disk(1, 1, || Ok(0)).flush().is_ok());
}
