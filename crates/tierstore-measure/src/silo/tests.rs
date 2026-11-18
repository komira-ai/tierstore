use super::*;
use crate::clock::StepClock;
use crate::testutil::Fake;
use objstore_s3::ObjectStore as _;
use objstore_s3::s3::fake::FakeS3;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn with_packs(rt: &tokio::runtime::Runtime, fake: &FakeS3, packs: &[(&str, usize)]) -> S3Store {
    let s = store(
        fake.endpoint(),
        "tierstore-fake",
        Credentials::new("k", "s"),
    )
    .unwrap();
    for (k, n) in packs {
        let key = ObjectKey::new(*k).unwrap();
        rt.block_on(s.put_new(&key, bytes::Bytes::from(vec![7u8; *n]), None))
            .unwrap();
    }
    s
}

#[test]
fn credentials_come_from_silo_environment_file() {
    let f = Fake::new("silo", &[]);
    let p = f.path().join("env");
    std::fs::write(
        &p,
        "# x\nMINIO_ROOT_USER=admin\nMINIO_ROOT_PASSWORD=\"s3cr3t\"\n",
    )
    .unwrap();
    let c = credentials(&p).unwrap();
    assert_eq!(
        format!("{c:?}"),
        "Credentials { access_key_id: \"admin\", secret_access_key: \"<redacted>\" }"
    );
    std::fs::write(&p, "MINIO_ROOT_USER=admin\n").unwrap();
    assert!(
        credentials(&p)
            .unwrap_err()
            .to_string()
            .ends_with("has no MINIO_ROOT_USER and MINIO_ROOT_PASSWORD")
    );
    assert!(credentials(&f.path().join("none")).is_err());
    assert!(store("not a url", "b", Credentials::new("a", "b")).is_err());
}

#[test]
fn the_cursor_reads_each_part_once_then_wraps() {
    let packs = vec![
        ("a".to_string(), 10),
        ("b".to_string(), 4),
        ("c".to_string(), 25),
    ];
    let mut c = Cursor::new(&packs, 0, 1);
    let got: Vec<(String, u64)> = (0..6).map(|_| c.next(10).unwrap()).collect();
    assert_eq!(
        got,
        [("a", 0), ("c", 0), ("c", 10), ("a", 0), ("c", 0), ("c", 10)]
            .map(|(k, o)| (k.to_string(), o))
    );
    assert!(c.wrapped());
    let mut odd = Cursor::new(&packs, 1, 2);
    assert_eq!(odd.next(4).unwrap(), ("b".to_string(), 0));
    assert_eq!(Cursor::new(&packs, 0, 1).next(100), None);
    assert_eq!(Cursor::new(&[], 0, 0).next(1), None);
}

#[test]
fn lists_a_bucket_and_runs_cells_against_it() {
    let rt = runtime();
    let fake = rt.block_on(FakeS3::start());
    let s = with_packs(
        &rt,
        &fake,
        &[("cas/p1", 1 << 21), ("cas/p2", 1 << 21), ("x/small", 10)],
    );
    let f = Fake::new("silo", &["sdb"]);
    let out = f.path().join("list.tsv");
    assert_eq!(rt.block_on(list(&s, "", &out)).unwrap(), 3);
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        "cas/p1\t2097152\ncas/p2\t2097152\nx/small\t10\n"
    );
    assert!(rt.block_on(list(&s, "", &f.path().join("none/x"))).is_err());
    let cfg = Config {
        parts_mib: vec![1],
        conc: vec![1, 2],
        warmup: Duration::from_millis(1),
        measure: Duration::from_millis(40),
        nodes: 1,
        devs: vec!["sdb".into()],
        diskstats: f.diskstats(),
    };
    let packs = vec![
        ("cas/p1".to_string(), 1 << 21),
        ("cas/p2".to_string(), 1 << 21),
    ];
    let clock = StepClock::new(Duration::from_millis(1), 1000);
    let rows = run(&cfg, &s, Cursor::new(&packs, 0, 1), &clock, &rt).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|r| r.mb_s > 0.0 && r.first_byte.is_some()));
    assert!(rows.iter().all(|r| r.disk_per_byte.abs() < 1e-12));
    assert_eq!(rows[0].from_unix_ms, 1_000_001);
    let t = table("n3", &rows).markdown();
    assert!(t.contains("### C-1, silo ranged GETs: n3"));
    // A part no pack holds, and a key the store does not have, fail the cell.
    let big = Config {
        parts_mib: vec![4],
        ..cfg.clone()
    };
    assert!(run(&big, &s, Cursor::new(&packs, 0, 1), &clock, &rt).is_err());
    let gone = vec![("cas/none".to_string(), 1 << 21)];
    assert!(run(&cfg, &s, Cursor::new(&gone, 0, 1), &clock, &rt).is_err());
    let idle = Config {
        measure: Duration::ZERO,
        conc: vec![1],
        ..cfg.clone()
    };
    let z = run(&idle, &s, Cursor::new(&packs, 0, 1), &clock, &rt).unwrap();
    assert_eq!((z[0].mb_s, z[0].disk_per_byte), (0.0, 0.0));
    let none = Row {
        first_byte: None,
        total: None,
        io: Default::default(),
        ..rows[0].clone()
    };
    let md = table("n", &[none]).markdown();
    assert!(md.contains("| - | - | - | - | 0 | 0.0 |"), "{md}");
}
