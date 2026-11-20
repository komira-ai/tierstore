use super::*;

fn entry_json(size: u64, chunks: &str, extra: &str) -> Vec<u8> {
    format!(r#"{{"FullPath":"/x","FileSize":{size},"Md5":null{extra},"chunks":{chunks}}}"#)
        .into_bytes()
}

fn why(e: &Error) -> String {
    e.to_string()
}

#[test]
fn entry_with_chunks_given_out_of_order_is_sorted() {
    let chunks = r#"[{"file_id":"7,0b","offset":10,"size":5},{"file_id":"38,1627","offset":0,"size":10,"e_tag":"x"}]"#;
    let e = parse_entry("cas/b/p", &entry_json(15, chunks, "")).unwrap();
    assert_eq!(e.size, 15);
    assert_eq!(e.etag, None);
    assert_eq!(
        e.chunks,
        vec![
            Chunk {
                fid: "38,1627".into(),
                volume: 38,
                offset: 0,
                size: 10
            },
            Chunk {
                fid: "7,0b".into(),
                volume: 7,
                offset: 10,
                size: 5
            },
        ]
    );
}

#[test]
fn entry_etag_is_the_answers_or_the_only_chunks() {
    let one = r#"[{"file_id":"1,aa","size":4,"e_tag":"0123"}]"#;
    let e = parse_entry("k", &entry_json(4, one, "")).unwrap();
    assert_eq!(e.etag.as_deref(), Some("0123"));
    let named = parse_entry("k", &entry_json(4, one, r#","ETag":"beef""#)).unwrap();
    assert_eq!(named.etag.as_deref(), Some("beef"));
    let empty_tag = r#"[{"file_id":"1,aa","size":4,"e_tag":""}]"#;
    assert_eq!(
        parse_entry("k", &entry_json(4, empty_tag, ""))
            .unwrap()
            .etag,
        None
    );
    let two = r#"[{"file_id":"1,aa","size":2,"e_tag":"a"},{"file_id":"1,ab","offset":2,"size":2,"e_tag":"b"}]"#;
    assert_eq!(
        parse_entry("k", &entry_json(4, two, "")).unwrap().etag,
        None
    );
}

#[test]
fn entry_without_chunks_is_empty_only_at_size_zero() {
    let e = parse_entry("k", br#"{"FileSize":0}"#).unwrap();
    assert!(e.chunks.is_empty());
    assert_eq!(e.size, 0);
    let err = parse_entry("cas/b/q", br#"{"FileSize":9,"chunks":null}"#).unwrap_err();
    assert_eq!(why(&err), "cas/b/q: chunks cover 0 bytes of 9");
}

#[test]
fn entry_with_a_gap_overlap_or_short_cover_is_an_error() {
    let gap = r#"[{"file_id":"1,aa","size":4},{"file_id":"1,ab","offset":6,"size":4}]"#;
    assert_eq!(
        why(&parse_entry("k", &entry_json(10, gap, "")).unwrap_err()),
        "k: chunk 1,ab at offset 6 leaves a gap before offset 4"
    );
    let overlap = r#"[{"file_id":"1,aa","size":6},{"file_id":"1,ab","offset":4,"size":6}]"#;
    assert_eq!(
        why(&parse_entry("k", &entry_json(10, overlap, "")).unwrap_err()),
        "k: chunk 1,ab at offset 4 overlaps offset 6"
    );
    let short = r#"[{"file_id":"1,aa","size":4}]"#;
    assert_eq!(
        why(&parse_entry("k", &entry_json(10, short, "")).unwrap_err()),
        "k: chunks cover 4 bytes of 10"
    );
}

#[test]
fn entry_whose_chunks_run_past_u64_is_an_error() {
    let big = format!(
        r#"[{{"file_id":"1,aa","size":{m}}},{{"file_id":"1,ab","offset":{m},"size":2}}]"#,
        m = u64::MAX
    );
    let err = parse_entry("k", &entry_json(1, &big, "")).unwrap_err();
    assert_eq!(why(&err), "k: chunk 1,ab runs past 2^64");
}

#[test]
fn entry_with_a_bad_file_id_or_json_is_an_error() {
    for fid in ["noComma", "1,", "x,aa", "-1,aa"] {
        let chunks = format!(r#"[{{"file_id":"{fid}","size":1}}]"#);
        let err = parse_entry("k", &entry_json(1, &chunks, "")).unwrap_err();
        assert_eq!(
            why(&err),
            format!("k: file id {fid:?} is not <volume id>,<needle>")
        );
    }
    let err = parse_entry("cas/b/z", b"{").unwrap_err();
    assert!(why(&err).starts_with("cas/b/z: "), "{err}");
}

#[test]
fn server_argument_names_a_node_and_an_address() {
    assert_eq!(
        parse_server("2=10.77.0.2:8080").unwrap(),
        VolumeServer {
            node: 2,
            addr: "10.77.0.2:8080".into()
        }
    );
    for (arg, part) in [
        ("10.0.0.1:8080", "expected <node id>=<host:port>"),
        ("256=h:1", "the node id is not a number from 0 to 255"),
        ("x=h:1", "the node id is not a number from 0 to 255"),
        ("1=h", "the address is not <host>:<port>"),
        ("1=:80", "the address is not <host>:<port>"),
        ("1=h:http", "the address is not <host>:<port>"),
    ] {
        let err = parse_server(arg).unwrap_err();
        assert_eq!(why(&err), format!("volume server {arg:?}: {part}"));
    }
}

fn status(volumes: &str) -> Vec<u8> {
    format!(
        r#"{{"Version":"30GB 4.54","DiskStatuses":[{{"dir":"/mnt/hdd1/swfs"}}],"Volumes":{volumes}}}"#
    )
    .into_bytes()
}

fn server(node: u8) -> VolumeServer {
    VolumeServer {
        node,
        addr: format!("10.0.0.{node}:8080"),
    }
}

#[test]
fn map_orders_copies_by_node_and_counts_them() {
    let mut map = VolumeMap::default();
    map.add_status(&server(3), &status(r#"[{"Id":13,"DiskId":2},{"Id":14}]"#))
        .unwrap();
    map.add_status(&server(1), &status(r#"[{"Id":13,"DiskId":1,"Size":8}]"#))
        .unwrap();
    assert_eq!(map.volumes(), 2);
    assert_eq!(
        map.replicas(13),
        [
            Replica {
                node: 1,
                addr: "10.0.0.1:8080".into(),
                disk: 1
            },
            Replica {
                node: 3,
                addr: "10.0.0.3:8080".into(),
                disk: 2
            },
        ]
    );
    assert_eq!(map.replicas(14).len(), 1);
    assert_eq!(map.replicas(14)[0].disk, 0);
    assert!(map.replicas(99).is_empty());
    assert_eq!(map.histogram(), BTreeMap::from([(1, 1), (2, 1)]));
}

#[test]
fn map_server_added_twice_replaces_its_copies() {
    let mut map = VolumeMap::default();
    map.add_status(&server(2), &status(r#"[{"Id":5,"DiskId":0}]"#))
        .unwrap();
    map.add_status(&server(2), &status(r#"[{"Id":5,"DiskId":1}]"#))
        .unwrap();
    assert_eq!(map.replicas(5).len(), 1);
    assert_eq!(map.replicas(5)[0].disk, 1);
    assert_eq!(map.histogram(), BTreeMap::from([(1, 1)]));
}

#[test]
fn map_status_without_volumes_adds_none_and_bad_status_names_the_server() {
    let mut map = VolumeMap::default();
    map.add_status(&server(2), br#"{"Version":"x","Volumes":null}"#)
        .unwrap();
    map.add_status(&server(2), b"{}").unwrap();
    assert_eq!(map.volumes(), 0);
    assert!(map.histogram().is_empty());
    let err = map.add_status(&server(2), b"[]").unwrap_err();
    assert!(why(&err).starts_with("10.0.0.2:8080 /status: "), "{err}");
}

#[test]
fn drive_ids_and_labels_name_node_and_disk() {
    let id = drive_id(2, 0x0102_0304);
    assert_eq!(id.0, [2, 1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let r = Replica {
        node: 2,
        addr: "h:1".into(),
        disk: 1,
    };
    let d = device(&r);
    assert_eq!(d, DeviceRef::Drive(NodeId(2), drive_id(2, 1)));
    assert_eq!(device_label(d), "n2d1");
    assert_eq!(device_label(DeviceRef::Hot(NodeId(7))), "n7hot");
    let other = Replica {
        disk: 2,
        ..r.clone()
    };
    assert_ne!(device(&r), device(&other));
}
