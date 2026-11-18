use super::*;
use crate::clock::StepClock;
use crate::cold::http::{read_head, request};
use crate::io::{fill, write_data_file};
use crate::testutil::Fake;
use std::io::{BufReader, Read as _};

fn params(queue_max: usize) -> Params {
    Params {
        streams: 1,
        write_streams: 1,
        slice_bytes: 16 << 10,
        preempt: true,
        preempt_max_per_s: 4,
        queue_max,
    }
}

fn config(dir: &std::path::Path, path: ReadPath, queue_max: usize) -> Config {
    Config {
        drives: [("hdd1".to_string(), dir.to_path_buf())].into(),
        params: params(queue_max),
        chunk: 8192,
        path,
        release_after: None,
    }
}

/// The bytes of the data file `write_data_file` makes with chunk 4096 and seed 0.
fn expected(size: usize) -> Vec<u8> {
    let mut v = vec![0u8; size];
    for (i, c) in v.chunks_mut(4096).enumerate() {
        fill(c, (i * 4096) as u64);
    }
    v
}

/// Serves `cfg` on loopback while `f` runs with the address.
fn serving(cfg: Config, f: impl FnOnce(std::net::SocketAddr, &Server<'_>)) {
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let server = Server::new(cfg, &clock);
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let h = scope.spawn(|| server.serve(&l, &stop));
        let _stop = crate::testutil::StopOnDrop(&stop);
        f(addr, &server);
        drop(_stop);
        h.join().unwrap().unwrap();
    });
}

fn fetch(
    addr: std::net::SocketAddr,
    drive: &str,
    file: &str,
    off: u64,
    len: u64,
) -> (u16, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(request(drive, file, off, len, Class::Demand).as_bytes())
        .unwrap();
    let mut r = BufReader::new(s);
    let h = read_head(&mut r).unwrap();
    let mut body = vec![0u8; usize::try_from(h.len).unwrap()];
    r.read_exact(&mut body).unwrap();
    (h.status, body)
}

#[test]
fn serves_exact_ranges_on_both_read_paths() {
    let f = Fake::new("server", &[]);
    let d = f.drive("packs");
    write_data_file(&d.join("p0.pack"), 64 << 10, 4096, 0).unwrap();
    let want = expected(64 << 10);
    for path in [ReadPath::Direct(2), ReadPath::Direct(1), ReadPath::Sendfile] {
        serving(config(&d, path, 8), |addr, server| {
            for (off, len) in [(0u64, 65_536u64), (100, 5000), (8192, 8192), (60_000, 5536)] {
                let (status, body) = fetch(addr, "hdd1", "p0.pack", off, len);
                assert_eq!(status, 206, "{path:?}");
                let (a, z) = (
                    usize::try_from(off).unwrap(),
                    usize::try_from(off + len).unwrap(),
                );
                assert_eq!(body, want[a..z], "{path:?} {off}+{len}");
            }
            // The server logs a request after its last byte is written, which the client may
            // read first: wait for the fourth entry.
            let mut served = Vec::new();
            while served.len() < 4 {
                served.extend(server.take_served());
                std::thread::yield_now();
            }
            assert_eq!(served.len(), 4);
            assert!(served.iter().all(|s| s.class == Class::Demand));
            assert!(server.sched("hdd1").unwrap().counts().admitted >= 4);
            assert!(server.sched("hdd9").is_none());
            assert!(server.counts().admitted >= 4);
            // An idle connection is still open when the server stops: it ends with it.
            let idle = TcpStream::connect(addr).unwrap();
            std::mem::forget(idle);
        });
    }
}

#[test]
fn unknown_drives_missing_files_and_full_queues_are_refused() {
    let f = Fake::new("server", &[]);
    let d = f.drive("packs");
    write_data_file(&d.join("p0.pack"), 8192, 4096, 0).unwrap();
    serving(config(&d, ReadPath::Direct(2), 8), |addr, _| {
        assert_eq!(fetch(addr, "hdd9", "p0.pack", 0, 10).0, 404);
        assert_eq!(fetch(addr, "hdd1", "none.pack", 0, 10).0, 404);
        // A request this server cannot parse ends the connection.
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"PUT / HTTP/1.1\r\n\r\n").unwrap();
        let mut rest = Vec::new();
        let _ = s.read_to_end(&mut rest);
        assert!(rest.is_empty());
    });
    serving(config(&d, ReadPath::Direct(2), 0), |addr, _| {
        assert_eq!(fetch(addr, "hdd1", "p0.pack", 0, 10).0, 503);
    });
}

#[test]
fn a_range_past_the_end_ends_the_answer_short() {
    let f = Fake::new("server", &[]);
    let d = f.drive("packs");
    write_data_file(&d.join("p0.pack"), 8192, 4096, 0).unwrap();
    for path in [ReadPath::Direct(2), ReadPath::Sendfile] {
        serving(config(&d, path, 8), |addr, _| {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(request("hdd1", "p0.pack", 4096, 65_536, Class::Prepare).as_bytes())
                .unwrap();
            let mut r = BufReader::new(s);
            let h = read_head(&mut r).unwrap();
            assert_eq!(h.len, 65_536);
            let mut rest = Vec::new();
            let _ = r.read_to_end(&mut rest);
            assert!(rest.len() < 65_536, "{path:?}");
        });
    }
}

#[test]
fn a_stalled_consumer_gives_its_slot_back_and_takes_it_again() {
    let f = Fake::new("server", &[]);
    let d = f.drive("packs");
    let mut cfg = config(&d, ReadPath::Direct(2), 8);
    cfg.release_after = Some(Duration::from_millis(5));
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let server = Server::new(cfg, &clock);
    let sched = server.sched("hdd1").unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (mut sock, _) = l.accept().unwrap();
    let (t, _) = sched.admit(Class::Prepare, false).unwrap();
    // 64 MiB is more than the socket buffers hold: the write blocks until the client reads.
    let big = vec![5u8; 64 << 20];
    std::thread::scope(|scope| {
        let h = scope.spawn(|| server.write_out(&mut sock, &big, sched, &t));
        // Read only once the server has parked: the release is an event, not a time.
        while sched.counts().parks == 0 {
            std::thread::yield_now();
        }
        let mut got = vec![0u8; 64 << 20];
        client.read_exact(&mut got).unwrap();
        h.join().unwrap().unwrap();
    });
    assert_eq!(sched.counts().parks, 1);
    // Without a release time the write is plain.
    let plain = Server::new(config(&d, ReadPath::Direct(2), 8), &clock);
    let s2 = plain.sched("hdd1").unwrap();
    let (t2, _) = s2.admit(Class::Prepare, false).unwrap();
    assert_eq!(
        plain.write_out(&mut sock, b"abc", s2, &t2).unwrap(),
        Duration::ZERO
    );
    let mut three = [0u8; 3];
    client.read_exact(&mut three).unwrap();
    drop(client);
    // A write to a closed peer fails (after the kernel learns the peer is gone).
    let fail = (0..64).any(|_| {
        server
            .write_out(&mut sock, &big[..1 << 20], sched, &t)
            .is_err()
    });
    assert!(fail);
    sched.release(t);
    s2.release(t2);
}

#[test]
fn writes_a_pack_as_a_write_stream() {
    let f = Fake::new("server", &[]);
    let d = f.drive("packs");
    let clock = StepClock::new(Duration::from_millis(1), 0);
    let server = Server::new(config(&d, ReadPath::Direct(2), 8), &clock);
    assert!(
        server
            .write_pack("hdd1", "w0.pack", 3 * 8192 + 4096)
            .unwrap()
            > 0.0
    );
    assert_eq!(
        std::fs::metadata(d.join("w0.pack")).unwrap().len(),
        3 * 8192 + 4096
    );
    assert!(server.write_pack("hdd9", "w0.pack", 4096).is_err());
    assert!(
        server.write_pack("hdd1", "w1.pack", 100).is_err(),
        "unaligned O_DIRECT write"
    );
    let full = Server::new(config(&d, ReadPath::Direct(2), 0), &clock);
    assert!(full.write_pack("hdd1", "w2.pack", 4096).is_err());
    let gone = Server::new(
        config(&f.path().join("none"), ReadPath::Direct(2), 8),
        &clock,
    );
    assert!(gone.write_pack("hdd1", "w3.pack", 4096).is_err());
}

#[test]
fn a_stopped_reader_is_a_network_error() {
    assert_eq!(stopped(()).to_string(), "a reader stopped");
    assert!(errno(rustix::io::Errno::PIPE).to_string().contains("pipe"));
}
