//! The LIST merge over several index nodes: equal to a model, continuation, the delimiter
//! roll-up and its one seek per source, nodes down, and the sources' errors.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "an integration test: a failed unwrap or expect is the failure it reports"
)]

use std::collections::{BTreeSet, VecDeque};

use tierstore_format::block::BlockGeometry;
use tierstore_format::ids::{BlockId, Generation, NodeId};
use tierstore_format::record::RecordFlags;
use tierstore_format::stamp::Stamp;
use tierstore_format::tomb::Tombstone;
use tierstore_index::backend::MemBackend;
use tierstore_index::backend::mem::Call;
use tierstore_index::list::{
    IndexSource, ListPage, ListRequest, ListSource, MAX_KEY_LEN, MAX_LIST_KEYS, list, seek_past,
};
use tierstore_index::{IndexConfig, KvIndex};
use tierstore_store_api::index::{EntryLocation, Index, IndexEntry, IndexError, PutOutcome};

type Node = KvIndex<MemBackend>;

fn node(id: u8) -> Node {
    let index = KvIndex::open(
        MemBackend::new(),
        IndexConfig {
            node: NodeId(id),
            geometry: BlockGeometry::DEFAULT,
        },
    )
    .expect("open");
    index.set_ready(true);
    index
}

fn entry(stamp: u64) -> IndexEntry {
    IndexEntry {
        generation: Generation([stamp as u8; 16]),
        location: EntryLocation::Block {
            block: BlockId::from_raw(1),
            offset: stamp,
        },
        length: 10,
        stamp: Stamp::from_raw(stamp),
        flags: RecordFlags(0),
        prior_blocks: Vec::new(),
        promoted: None,
    }
}

fn put(n: &Node, key: &[u8], stamp: u64) {
    assert_eq!(
        n.put_if_absent(key, entry(stamp)).expect("put"),
        PutOutcome::Created
    );
}

fn del(n: &Node, key: &[u8], stamp: u64) {
    n.delete(&Tombstone {
        key: key.to_vec(),
        generation: Generation([0; 16]),
        stamp: Stamp::from_raw(stamp),
        names: Vec::new(),
    })
    .expect("delete");
}

fn req(prefix: &str, delim: Option<&str>, max: usize, after: Option<&[u8]>) -> ListRequest {
    ListRequest {
        prefix: prefix.as_bytes().to_vec(),
        delimiter: delim.map(|d| d.as_bytes().to_vec()),
        max_keys: max,
        after: after.map(<[u8]>::to_vec),
    }
}

fn merge(nodes: &[&Node], r: &ListRequest) -> Result<ListPage, IndexError> {
    let mut sources: Vec<IndexSource<'_>> = nodes.iter().map(|n| IndexSource(*n)).collect();
    let mut refs: Vec<&mut dyn ListSource> = sources
        .iter_mut()
        .map(|s| s as &mut dyn ListSource)
        .collect();
    list(&mut refs, r)
}

fn b(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn bs(v: &[&str]) -> Vec<Vec<u8>> {
    v.iter().map(|s| b(s)).collect()
}

/// A fixed-seed xorshift generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A key of one to four segments joined by `/`, `::` or `:`.
fn gen_key(rng: &mut Rng) -> Vec<u8> {
    const SEGS: [&str; 6] = ["a", "b", "zz", "a1", "m", "q9"];
    const SEPS: [&str; 3] = ["/", "::", ":"];
    let mut key = String::from(SEGS[rng.below(SEGS.len())]);
    for _ in 0..rng.below(4) {
        key.push_str(SEPS[rng.below(SEPS.len())]);
        key.push_str(SEGS[rng.below(SEGS.len())]);
    }
    key.into_bytes()
}

/// Three nodes where each key of a universe lives on two or three of them, and the model.
struct Cluster {
    nodes: [Node; 3],
    model: BTreeSet<Vec<u8>>,
}

fn cluster(seed: u64) -> Cluster {
    let nodes = [node(0), node(1), node(2)];
    let mut rng = Rng(seed);
    let universe: Vec<Vec<u8>> = (0..160).map(|_| gen_key(&mut rng)).collect();
    let mut model = BTreeSet::new();
    let mut stamp = 100;
    for _ in 0..900 {
        let key = &universe[rng.below(universe.len())];
        // The replica set: two or three nodes, chosen by the key's bytes.
        let sum = key.iter().map(|&x| usize::from(x)).sum::<usize>();
        let on: Vec<&Node> = match sum % 4 {
            0 => vec![&nodes[0], &nodes[1]],
            1 => vec![&nodes[1], &nodes[2]],
            2 => vec![&nodes[0], &nodes[2]],
            _ => vec![&nodes[0], &nodes[1], &nodes[2]],
        };
        stamp += 1;
        if model.contains(key) {
            for n in on {
                del(n, key, stamp);
            }
            model.remove(key);
        } else {
            for n in on {
                put(n, key, stamp);
            }
            model.insert(key.clone());
        }
    }
    Cluster { nodes, model }
}

/// Every entry (key or common prefix) of the full listing of the model, in order.
fn model_listing(model: &BTreeSet<Vec<u8>>, prefix: &str, delim: Option<&str>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for key in model.iter().filter(|k| k.starts_with(prefix.as_bytes())) {
        let rest = &key[prefix.len()..];
        let cut = delim.and_then(|d| {
            rest.windows(d.len())
                .position(|w| w == d.as_bytes())
                .map(|at| prefix.len() + at + d.len())
        });
        match cut {
            Some(end) => {
                if out.last() != Some(&key[..end].to_vec()) {
                    out.push(key[..end].to_vec());
                }
            }
            None => out.push(key.clone()),
        }
    }
    out
}

/// Pages through the whole listing; checks every page's size and `next`, and returns the entries.
fn walk(nodes: &[&Node], prefix: &str, delim: Option<&str>, max: usize) -> Vec<Vec<u8>> {
    let cap = max.min(MAX_LIST_KEYS);
    let mut out = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    loop {
        let page = merge(nodes, &req(prefix, delim, max, after.as_deref())).expect("list");
        let mut entries: Vec<Vec<u8>> = page.keys.clone();
        entries.extend(page.common_prefixes.clone());
        entries.sort();
        assert!(entries.len() <= cap);
        if page.next.is_some() {
            assert_eq!(entries.len(), cap, "only a full page has a next");
        }
        out.extend(entries);
        match page.next {
            Some(next) => after = Some(next),
            None => return out,
        }
    }
}

#[test]
fn merged_listing_equals_the_model_for_prefixes_delimiters_and_page_sizes() {
    for seed in [7_u64, 1234, 99_991] {
        let c = cluster(seed);
        let all: Vec<&Node> = c.nodes.iter().collect();
        for prefix in ["", "a", "a/", "a::", "zz", "m:", "nothing"] {
            for delim in [None, Some("/"), Some("::")] {
                let want = model_listing(&c.model, prefix, delim);
                for max in [1, 2, 7, 1000, 5000] {
                    assert_eq!(
                        walk(&all, prefix, delim, max),
                        want,
                        "seed {seed} prefix {prefix:?} delim {delim:?} max {max}"
                    );
                }
            }
        }
    }
}

#[test]
fn listing_with_one_node_down_is_complete() {
    let c = cluster(42);
    for down in 0..3 {
        let up: Vec<&Node> = (0..3).filter(|i| *i != down).map(|i| &c.nodes[i]).collect();
        for delim in [None, Some("/")] {
            assert_eq!(
                walk(&up, "", delim, 7),
                model_listing(&c.model, "", delim),
                "node {down} down, delimiter {delim:?}"
            );
        }
    }
}

#[test]
fn a_key_held_by_two_nodes_is_listed_once() {
    let (n0, n1) = (node(0), node(1));
    put(&n0, b"k1", 10);
    put(&n1, b"k1", 10);
    put(&n1, b"k2", 11);
    let page = merge(&[&n0, &n1], &req("", None, 10, None)).expect("list");
    assert_eq!(page.keys, bs(&["k1", "k2"]));
    assert_eq!(page.next, None);
}

#[test]
fn tombstone_entries_are_never_listed() {
    let (n0, n1) = (node(0), node(1));
    for n in [&n0, &n1] {
        put(n, b"gone", 10);
        put(n, b"kept", 11);
        del(n, b"gone", 20);
    }
    del(&n0, b"never-put", 30);
    let page = merge(&[&n0, &n1], &req("", None, 10, None)).expect("list");
    assert_eq!(page.keys, bs(&["kept"]));
}

#[test]
fn a_key_deleted_before_the_call_is_absent_and_one_put_before_is_present() {
    let (n0, n1) = (node(0), node(1));
    put(&n0, b"x", 10);
    put(&n1, b"x", 10);
    let before = merge(&[&n0, &n1], &req("", None, 10, None)).expect("list");
    assert_eq!(before.keys, bs(&["x"]));
    del(&n0, b"x", 20);
    del(&n1, b"x", 20);
    put(&n0, b"y", 21);
    let after = merge(&[&n0, &n1], &req("", None, 10, None)).expect("list");
    assert_eq!(after.keys, bs(&["y"]));
}

#[test]
fn max_keys_zero_gives_an_empty_page_and_above_the_cap_is_capped() {
    let n = node(0);
    for i in 0..1005_u32 {
        put(&n, format!("k{i:05}").as_bytes(), 10);
    }
    let empty = merge(&[&n], &req("", None, 0, None)).expect("list");
    assert_eq!(empty, ListPage::default());
    let capped = merge(&[&n], &req("", None, 5000, None)).expect("list");
    assert_eq!(capped.keys.len(), MAX_LIST_KEYS);
    assert_eq!(capped.keys[999], b("k00999"));
    assert_eq!(capped.next, Some(b("k00999")));
    let rest = merge(&[&n], &req("", None, 5000, capped.next.as_deref())).expect("list");
    assert_eq!(rest.keys.len(), 5);
    assert_eq!(rest.next, None);
}

#[test]
fn a_page_ending_exactly_at_the_last_key_has_no_next() {
    let n = node(0);
    put(&n, b"a", 10);
    put(&n, b"b", 11);
    let page = merge(&[&n], &req("", None, 2, None)).expect("list");
    assert_eq!(page.keys, bs(&["a", "b"]));
    assert_eq!(page.next, None);
    let page = merge(&[&n], &req("", None, 1, None)).expect("list");
    assert_eq!(page.keys, bs(&["a"]));
    assert_eq!(page.next, Some(b("a")));
}

/// A source that counts its calls and records their `after`.
struct Spy<'a> {
    inner: IndexSource<'a>,
    afters: Vec<Option<Vec<u8>>>,
}

impl ListSource for Spy<'_> {
    fn list(
        &mut self,
        prefix: &[u8],
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<Vec<u8>>, IndexError> {
        self.afters.push(after.map(<[u8]>::to_vec));
        self.inner.list(prefix, after, limit)
    }
}

#[test]
fn a_rolled_up_prefix_of_many_keys_costs_one_extra_call_per_source() {
    let nodes = [node(0), node(1), node(2)];
    for n in &nodes {
        for i in 0..200_u32 {
            put(n, format!("a/{i:04}").as_bytes(), 10);
        }
        put(n, b"b", 11);
    }
    let mut spies: Vec<Spy<'_>> = nodes
        .iter()
        .map(|n| Spy {
            inner: IndexSource(n),
            afters: Vec::new(),
        })
        .collect();
    let mut refs: Vec<&mut dyn ListSource> =
        spies.iter_mut().map(|s| s as &mut dyn ListSource).collect();
    let page = list(&mut refs, &req("", Some("/"), 10, None)).expect("list");
    assert_eq!(page.common_prefixes, bs(&["a/"]));
    assert_eq!(page.keys, bs(&["b"]));
    assert_eq!(page.next, None);
    for spy in &spies {
        assert_eq!(spy.afters, vec![None, Some(seek_past(b"a/"))]);
    }
}

#[test]
fn a_rolled_up_prefix_ends_the_page_with_the_seek_past_cursor() {
    let n = node(0);
    for i in 0..30_u32 {
        put(&n, format!("a/{i:02}").as_bytes(), 10);
    }
    put(&n, b"b/1", 11);
    let page = merge(&[&n], &req("", Some("/"), 1, None)).expect("list");
    assert_eq!(page.common_prefixes, bs(&["a/"]));
    assert_eq!(page.next, Some(seek_past(b"a/")));
    let page = merge(&[&n], &req("", Some("/"), 1, page.next.as_deref())).expect("list");
    assert_eq!(page.common_prefixes, bs(&["b/"]));
    assert_eq!(page.next, None);
}

#[test]
fn an_empty_delimiter_is_no_delimiter() {
    let n = node(0);
    put(&n, b"a/1", 10);
    let r = ListRequest {
        delimiter: Some(Vec::new()),
        ..req("", None, 10, None)
    };
    let page = merge(&[&n], &r).expect("list");
    assert_eq!(page.keys, bs(&["a/1"]));
    assert!(page.common_prefixes.is_empty());
}

#[test]
fn the_after_cursor_before_inside_and_past_the_prefix() {
    let n = node(0);
    for key in ["a/1", "a/2", "a/3", "b/1"] {
        put(&n, key.as_bytes(), 10);
    }
    let before = merge(&[&n], &req("a/", None, 10, Some(b"A"))).expect("list");
    assert_eq!(before.keys, bs(&["a/1", "a/2", "a/3"]));
    let inside = merge(&[&n], &req("a/", None, 10, Some(b"a/1"))).expect("list");
    assert_eq!(inside.keys, bs(&["a/2", "a/3"]));
    let past = merge(&[&n], &req("a/", None, 10, Some(b"b"))).expect("list");
    assert!(past.keys.is_empty());
    assert_eq!(past.next, None);
}

#[test]
fn an_indexing_source_fails_the_list() {
    let (ready, building) = (node(0), node(1));
    building.set_ready(false);
    put(&ready, b"k", 10);
    assert!(matches!(
        merge(&[&ready, &building], &req("", None, 10, None)),
        Err(IndexError::Indexing)
    ));
}

#[test]
fn a_failing_store_error_is_returned_as_it_is() {
    let n = node(0);
    put(&n, b"k", 10);
    n.backend().fail_next(Call::Scan, 0, 1);
    match merge(&[&n], &req("", None, 10, None)) {
        Err(IndexError::Store { op, .. }) => assert_eq!(op, "scan"),
        other => panic!("expected a store error, got {other:?}"),
    }
}

#[test]
fn a_failure_on_a_later_page_is_returned() {
    let n = node(0);
    for i in 0..4_u32 {
        put(&n, format!("k{i}").as_bytes(), 10);
    }
    // The first page of two keys is read, then the peek for more fails.
    n.backend().fail_next(Call::Scan, 1, 1);
    assert!(matches!(
        merge(&[&n], &req("", None, 2, None)),
        Err(IndexError::Store { .. })
    ));
}

/// A source that answers from a script, whatever it is asked.
struct Script(VecDeque<Vec<Vec<u8>>>);

impl ListSource for Script {
    fn list(&mut self, _: &[u8], _: Option<&[u8]>, _: usize) -> Result<Vec<Vec<u8>>, IndexError> {
        Ok(self.0.pop_front().unwrap_or_default())
    }
}

#[test]
fn a_key_longer_than_the_key_limit_or_outside_the_prefix_is_dropped() {
    let mut long = b("a/");
    long.resize(MAX_KEY_LEN + 6, 0xff);
    let mut script = Script(VecDeque::from([vec![
        b("0-before"),
        b("a/1"),
        long,
        b("b"),
    ]]));
    let mut refs: Vec<&mut dyn ListSource> = vec![&mut script];
    let page = list(&mut refs, &req("a", None, 10, None)).expect("list");
    assert_eq!(page.keys, bs(&["a/1"]));
    let mut script = Script(VecDeque::from([vec![b("0-before"), b("a/1"), b("b")]]));
    let mut refs: Vec<&mut dyn ListSource> = vec![&mut script];
    let page = list(&mut refs, &req("", Some("/"), 10, None)).expect("list");
    assert_eq!(page.common_prefixes, bs(&["a/"]));
    assert_eq!(page.keys, bs(&["0-before", "b"]));
}

#[test]
fn keys_a_source_returns_under_an_already_rolled_up_prefix_are_dropped() {
    let mut honest = Script(VecDeque::from([bs(&["a/1", "b"])]));
    let mut lying = Script(VecDeque::from([bs(&["a/0", "a/9"]), bs(&["a/5", "c"])]));
    let mut refs: Vec<&mut dyn ListSource> = vec![&mut honest, &mut lying];
    let page = list(&mut refs, &req("", Some("/"), 2, None)).expect("list");
    assert_eq!(page.common_prefixes, bs(&["a/"]));
    assert_eq!(page.keys, bs(&["b"]));
    assert_eq!(page.next, Some(b("b")));
}

#[test]
fn seek_past_pads_the_prefix_with_ff_to_the_key_limit() {
    let p = seek_past(b"ab");
    assert_eq!(p.len(), MAX_KEY_LEN);
    assert!(p.starts_with(b"ab"));
    assert!(p[2..].iter().all(|&x| x == 0xff));
    let full = vec![b'x'; MAX_KEY_LEN];
    assert_eq!(seek_past(&full), full);
    let longer = vec![b'x'; MAX_KEY_LEN + 3];
    assert_eq!(seek_past(&longer), longer);
}
