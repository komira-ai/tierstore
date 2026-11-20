//! What the storm's file system answers to each request through a fake device, and when it
//! answers a parked READ: by an injected clock, never by the machine's.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tsfuse::abi::{AttrOut, EntryOut, GetattrIn, OpenIn, OpenOut, ReadIn, ReleaseIn};
use tsfuse::{Fs as _, Handled, ReplyDevice, Request, Tokens};

use super::{
    COLD_INO, COLD_SIZE, Clock, ColdRead, FOPEN_DIRECT_IO, PROBE_INO, PROBE_SIZE, Parked, ROOT_INO,
    StormCounts, StormFs, cold_index,
};

const LOOKUP: u32 = 1;
const GETATTR: u32 = 3;
const OPEN: u32 = 14;
const READ: u32 = 15;
const RELEASE: u32 = 18;
const FLUSH: u32 = 25;
const ENOENT: i32 = -2;
const EIO: i32 = -5;
const ENOTDIR: i32 = -20;
const EISDIR: i32 = -21;
const ENOSYS: i32 = -38;

/// A clock the test sets.
#[derive(Clone, Default)]
struct Manual(Arc<AtomicU64>);

impl Manual {
    fn set(&self, now: u64) {
        self.0.store(now, Ordering::SeqCst);
    }
}

impl Clock for Manual {
    fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A device that keeps every reply written to it.
#[derive(Default)]
struct FakeDevice {
    sent: Mutex<Vec<Vec<u8>>>,
}

impl ReplyDevice for FakeDevice {
    fn send(&self, parts: &[&[u8]]) -> std::io::Result<()> {
        self.sent.lock().unwrap().push(parts.concat());
        Ok(())
    }
}

/// A reply as the kernel reads it: its unique, its error and its payload.
#[derive(Debug, PartialEq, Eq)]
struct Answer {
    unique: u64,
    error: i32,
    payload: Vec<u8>,
}

fn decode(bytes: &[u8]) -> Answer {
    let len = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    assert_eq!(len, bytes.len(), "the header's length is the reply's");
    Answer {
        unique: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        error: i32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        payload: bytes[16..].to_vec(),
    }
}

/// A request: the 40-byte header for `opcode`, `unique` and `nodeid`, then `body`.
fn request(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
    let len = u32::try_from(40 + body.len()).unwrap();
    let mut r = Vec::new();
    r.extend(len.to_le_bytes());
    r.extend(opcode.to_le_bytes());
    r.extend(unique.to_le_bytes());
    r.extend(nodeid.to_le_bytes());
    r.extend([0u8; 16]);
    r.extend(body);
    r
}

fn read_in(offset: u64, size: u32) -> Vec<u8> {
    ReadIn {
        offset,
        size,
        ..ReadIn::default()
    }
    .as_bytes()
    .to_vec()
}

/// A file system of three cold files, parking for 1000 ns by `clock`, replies of at most 64 KiB.
fn fs(read: ColdRead, park_ns: u64, clock: &Manual) -> StormFs<Manual> {
    StormFs::new(
        3,
        read,
        Duration::from_nanos(park_ns),
        clock.clone(),
        64 * 1024,
    )
}

/// Hands one request to `fs`: how it was handled, and the replies written so far (taken).
fn send(
    fs: &StormFs<Manual>,
    device: &Arc<FakeDevice>,
    tokens: &Tokens,
    (opcode, unique, nodeid): (u32, u64, u64),
    body: &[u8],
) -> (Handled, Vec<Answer>) {
    let bytes = request(opcode, unique, nodeid, body);
    let replier = tokens.register(unique, device.clone());
    let handled = fs.handle(Request::parse(&bytes).unwrap(), replier);
    (handled, taken(device))
}

fn taken(device: &FakeDevice) -> Vec<Answer> {
    std::mem::take(&mut *device.sent.lock().unwrap())
        .iter()
        .map(|b| decode(b))
        .collect()
}

/// Asks one request that is answered inline: its one reply.
fn ask(fs: &StormFs<Manual>, opcode: u32, nodeid: u64, body: &[u8]) -> Answer {
    let (device, tokens) = (Arc::new(FakeDevice::default()), Tokens::new());
    let (handled, mut sent) = send(fs, &device, &tokens, (opcode, 9, nodeid), body);
    assert_eq!(handled, Handled::Done);
    assert_eq!(sent.len(), 1, "one reply");
    sent.remove(0)
}

/// A parked queue gives its items back by the time they are due, the earliest first and those
/// due at the same time in the order they came, and all of them at once when asked.
#[test]
fn the_parked_queue_gives_back_what_is_due_in_order() {
    let mut parked = Parked::default();
    assert!(parked.is_empty());
    assert_eq!(parked.next_due(), None);
    parked.push(30, "c");
    parked.push(10, "a");
    parked.push(10, "b");
    parked.push(u64::MAX, "z");
    assert_eq!((parked.len(), parked.next_due()), (4, Some(10)));
    assert!(parked.take_due(9).is_empty());
    assert_eq!(parked.take_due(10), ["a", "b"]);
    assert_eq!(parked.take_due(29), Vec::<&str>::new());
    assert_eq!(
        parked.take_due(u64::MAX),
        ["c", "z"],
        "due at the end of time"
    );
    parked.push(5, "d");
    parked.push(1, "e");
    assert_eq!(parked.take_all(), ["e", "d"]);
    assert!(parked.is_empty());
}

/// The probe, the cold files `c0` to `c2` and nothing else are in the root, each with its size
/// and with no time in the kernel's caches; a lookup in a file is ENOTDIR, in an unknown node or
/// of an unknown name ENOENT, and each counts as metadata.
#[test]
fn the_tree_is_the_probe_and_the_cold_files_and_nothing_is_cached() {
    let clock = Manual::default();
    let fs = fs(ColdRead::Buffered, 1000, &clock);
    let probe = ask(&fs, LOOKUP, ROOT_INO, b"probe\0");
    assert_eq!(probe.error, 0);
    let (out, _) = EntryOut::read_prefix(&probe.payload).unwrap();
    assert_eq!(
        (out.nodeid, out.attr.size, out.attr.mode, out.attr.nlink),
        (PROBE_INO, PROBE_SIZE, 0o100_444, 1)
    );
    assert_eq!(
        (
            out.entry_valid,
            out.entry_valid_nsec,
            out.attr_valid,
            out.attr_valid_nsec
        ),
        (0, 0, 0, 0)
    );
    let cold = ask(&fs, LOOKUP, ROOT_INO, b"c2\0");
    let (out, _) = EntryOut::read_prefix(&cold.payload).unwrap();
    assert_eq!((out.nodeid, out.attr.size), (COLD_INO + 2, COLD_SIZE));
    for name in [&b"c3\0"[..], b"c01\0", b"c\0", b"c+1\0", b"x\0", b"c-1\0"] {
        assert_eq!(ask(&fs, LOOKUP, ROOT_INO, name).error, ENOENT, "{name:?}");
    }
    assert_eq!(ask(&fs, LOOKUP, PROBE_INO, b"x\0").error, ENOTDIR);
    assert_eq!(ask(&fs, LOOKUP, COLD_INO + 1, b"x\0").error, ENOTDIR);
    assert_eq!(ask(&fs, LOOKUP, 77, b"x\0").error, ENOENT);
    let body = GetattrIn::default();
    let root = ask(&fs, GETATTR, ROOT_INO, body.as_bytes());
    let (out, _) = AttrOut::read_prefix(&root.payload).unwrap();
    assert_eq!(
        (out.attr.ino, out.attr.mode, out.attr.nlink, out.attr_valid),
        (ROOT_INO, 0o40_555, 2, 0)
    );
    assert_eq!(
        ask(&fs, GETATTR, COLD_INO + 3, body.as_bytes()).error,
        ENOENT
    );
    assert_eq!(
        fs.counts(),
        StormCounts {
            metadata: 13,
            ..StormCounts::default()
        }
    );
}

/// A cold file's name is `c` and its number in decimal, with no sign and no leading zero.
#[test]
fn a_cold_name_is_c_and_a_plain_decimal() {
    assert_eq!(cold_index(b"c0"), Some(0));
    assert_eq!(cold_index(b"c17"), Some(17));
    for name in [
        &b"c"[..],
        b"c00",
        b"c07",
        b"c+7",
        b"c-1",
        b"d1",
        b"c1x",
        b"c\xff",
        b"",
    ] {
        assert_eq!(cold_index(name), None, "{name:?}");
    }
}

/// OPEN of a cold file is answered with `FOPEN_DIRECT_IO` when the storm reads direct and with
/// no flag when it reads through the page cache; the probe never; RELEASE is answered empty; an
/// opcode it does not serve is ENOSYS and a body too short for its opcode EIO.
#[test]
fn opens_say_direct_io_for_a_cold_file_of_a_direct_storm_alone() {
    let clock = Manual::default();
    let open = OpenIn::default();
    let flags = |fs: &StormFs<Manual>, ino| {
        let reply = ask(fs, OPEN, ino, open.as_bytes());
        assert_eq!(reply.error, 0);
        OpenOut::read_prefix(&reply.payload).unwrap().0.open_flags
    };
    let direct = fs(ColdRead::Direct, 1000, &clock);
    assert_eq!(flags(&direct, COLD_INO), FOPEN_DIRECT_IO);
    assert_eq!(flags(&direct, PROBE_INO), 0);
    let buffered = fs(ColdRead::Buffered, 1000, &clock);
    assert_eq!(flags(&buffered, COLD_INO), 0);
    let release = ask(
        &buffered,
        RELEASE,
        COLD_INO,
        ReleaseIn::default().as_bytes(),
    );
    assert_eq!((release.error, release.payload.len()), (0, 0));
    assert_eq!(ask(&buffered, FLUSH, PROBE_INO, &[0u8; 24]).error, ENOSYS);
    assert_eq!(ask(&buffered, GETATTR, PROBE_INO, &[]).error, EIO);
    assert_eq!(
        (ColdRead::Buffered.label(), ColdRead::Direct.label()),
        ("buffered", "direct")
    );
}

/// A READ of the probe is answered inline with its zeros, up to its end; of the root EISDIR; of
/// an unknown node ENOENT; none of them is a cold read.
#[test]
fn reads_of_the_probe_are_answered_at_once() {
    let clock = Manual::default();
    let fs = fs(ColdRead::Buffered, 1000, &clock);
    let reply = ask(&fs, READ, PROBE_INO, &read_in(4000, 4096));
    assert_eq!((reply.error, reply.payload), (0, vec![0u8; 96]));
    assert_eq!(
        ask(&fs, READ, PROBE_INO, &read_in(5000, 10)).payload.len(),
        0
    );
    assert_eq!(ask(&fs, READ, ROOT_INO, &read_in(0, 10)).error, EISDIR);
    assert_eq!(ask(&fs, READ, COLD_INO + 3, &read_in(0, 10)).error, ENOENT);
    assert_eq!(fs.counts(), StormCounts::default());
}

/// A READ of a cold file is parked: nothing is written until the clock reaches the time it is
/// due, `park` after it came; then it is answered with zeros of the size asked (at most the
/// largest reply), the earliest first, and the counts follow. With a park of zero it is
/// answered inline.
#[test]
fn a_cold_read_is_answered_when_its_park_is_over_by_the_clock() {
    let clock = Manual::default();
    let fs = fs(ColdRead::Buffered, 1000, &clock);
    let (device, tokens) = (Arc::new(FakeDevice::default()), Tokens::new());
    clock.set(50);
    let (handled, sent) = send(
        &fs,
        &device,
        &tokens,
        (READ, 1, COLD_INO),
        &read_in(0, 4096),
    );
    assert_eq!((handled, sent.len()), (Handled::Parked, 0));
    clock.set(500);
    let (handled, _) = send(
        &fs,
        &device,
        &tokens,
        (READ, 2, COLD_INO + 1),
        &read_in(COLD_SIZE - 10, 4096),
    );
    assert_eq!(handled, Handled::Parked);
    let (_, _) = send(
        &fs,
        &device,
        &tokens,
        (READ, 3, COLD_INO + 2),
        &read_in(0, 1 << 20),
    );
    assert_eq!(
        fs.counts(),
        StormCounts {
            cold_reads: 3,
            answered: 0,
            parked: 3,
            metadata: 0
        }
    );
    clock.set(1049);
    assert_eq!(fs.answer_due(), 0);
    assert!(taken(&device).is_empty());
    clock.set(1050);
    assert_eq!(fs.answer_due(), 1);
    assert_eq!(
        taken(&device),
        [Answer {
            unique: 1,
            error: 0,
            payload: vec![0; 4096]
        }]
    );
    clock.set(1500);
    assert_eq!(fs.answer_due(), 2);
    let sent = taken(&device);
    assert_eq!(
        sent.iter()
            .map(|a| (a.unique, a.error, a.payload.len()))
            .collect::<Vec<_>>(),
        [(2, 0, 10), (3, 0, 64 * 1024)],
        "the end of the file, and no more than the largest reply"
    );
    assert_eq!(
        fs.counts(),
        StormCounts {
            cold_reads: 3,
            answered: 3,
            parked: 0,
            metadata: 0
        }
    );
    assert_eq!(tokens.registered(), 0, "each answered once");
    let inline = self::fs(ColdRead::Direct, 0, &clock);
    let reply = ask(&inline, READ, COLD_INO, &read_in(0, 100));
    assert_eq!((reply.error, reply.payload.len()), (0, 100));
    assert_eq!(inline.counts().answered, 1);
}

/// The parker answers each READ once the clock says it is due (it looks at the clock again at
/// least every 10 ms, so a clock that jumps is followed); once stopped it answers every READ
/// still parked, due or not, and a READ that comes after the stop is answered inline.
#[test]
fn the_parker_answers_what_is_due_and_everything_at_its_stop() {
    let clock = Manual::default();
    let fs = Arc::new(fs(ColdRead::Buffered, 1_000_000_000_000, &clock));
    let (device, tokens) = (Arc::new(FakeDevice::default()), Tokens::new());
    let parker = {
        let fs = fs.clone();
        std::thread::spawn(move || fs.park_loop())
    };
    for unique in 1..=3 {
        let (handled, _) = send(
            &fs,
            &device,
            &tokens,
            (READ, unique, COLD_INO),
            &read_in(0, 8),
        );
        assert_eq!(handled, Handled::Parked);
    }
    assert!(taken(&device).is_empty(), "none due yet");
    clock.set(1_000_000_000_000);
    while fs.counts().answered < 3 {
        std::thread::yield_now();
    }
    assert_eq!(taken(&device).len(), 3);
    let (handled, _) = send(&fs, &device, &tokens, (READ, 4, COLD_INO), &read_in(0, 8));
    assert_eq!(handled, Handled::Parked);
    fs.stop();
    parker.join().unwrap();
    assert_eq!(
        taken(&device)
            .iter()
            .map(|a| (a.unique, a.error))
            .collect::<Vec<_>>(),
        [(4, 0)],
        "answered at the stop, not when due"
    );
    let (handled, sent) = send(&fs, &device, &tokens, (READ, 5, COLD_INO), &read_in(0, 8));
    assert_eq!((handled, sent.len()), (Handled::Done, 1));
    assert_eq!(fs.counts().answered, 5);
    assert!(format!("{fs:?}").starts_with("StormFs { cold_files: 3, read: Buffered"));
}
