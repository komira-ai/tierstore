//! Arm C-7, placement: the design's weighted rendezvous hashing over the 12 drives, applied
//! offline to the real key set. It reports how evenly copies (and bytes) land per drive, the
//! probe depth a reader needs when the copies were written while one node was down (counted in
//! the plain ranking and in the ranking after the distinct-node filter), and how many keys move
//! when a drive is removed or a node of two drives is added.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::report::{Table, num};

/// One drive of the cold set.
#[derive(Debug, Clone, PartialEq)]
pub struct Drive {
    /// Its stable id.
    pub id: String,
    /// Its node.
    pub node: String,
    /// Its weight (capacity).
    pub weight: f64,
}

/// The farm's 12 drives: two per node over six nodes, equal weights.
#[must_use]
pub fn farm(nodes: usize, per_node: usize) -> Vec<Drive> {
    (1..=nodes)
        .flat_map(|n| {
            (1..=per_node).map(move |d| Drive {
                id: format!("n{n}-hdd{d}"),
                node: format!("n{n}"),
                weight: 1.0,
            })
        })
        .collect()
}

/// The drive's score for `key`: `-weight / ln(h / 2^64)` with `h` the first 8 bytes of
/// `blake3(key || drive_id)`.
#[must_use]
pub fn score(key: &str, d: &Drive) -> f64 {
    let mut h = blake3::Hasher::new();
    h.update(key.as_bytes());
    h.update(d.id.as_bytes());
    let b = h.finalize();
    let mut x = [0u8; 8];
    x.copy_from_slice(&b.as_bytes()[..8]);
    // A uniform value in (0, 1): never 0, so the logarithm is finite and negative.
    let u = (u64::from_le_bytes(x) as f64 + 1.0) / (u64::MAX as f64 + 2.0);
    -d.weight / u.ln()
}

/// The drives in rank order for `key` (indices into `drives`), best first.
#[must_use]
pub fn rank(key: &str, drives: &[Drive]) -> Vec<usize> {
    let mut v: Vec<(f64, usize)> = drives
        .iter()
        .enumerate()
        .map(|(i, d)| (score(key, d), i))
        .collect();
    v.sort_by(|a, b| b.0.total_cmp(&a.0));
    v.into_iter().map(|(_, i)| i).collect()
}

/// The first `c` drives of `ranked` on distinct nodes, skipping those `usable` refuses.
#[must_use]
pub fn copies(
    ranked: &[usize],
    drives: &[Drive],
    c: usize,
    usable: &dyn Fn(usize) -> bool,
) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for &i in ranked {
        if out.len() == c {
            break;
        }
        if usable(i) && out.iter().all(|&j| drives[j].node != drives[i].node) {
            out.push(i);
        }
    }
    out
}

/// The ranking with every drive whose node already appeared earlier removed: the ranks a
/// reader probes when it counts candidates after the distinct-node filter.
#[must_use]
pub fn filtered(ranked: &[usize], drives: &[Drive]) -> Vec<usize> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for &i in ranked {
        if !seen.contains(&drives[i].node.as_str()) {
            seen.push(&drives[i].node);
            out.push(i);
        }
    }
    out
}

/// What the arm reports.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Report {
    /// Keys.
    pub keys: u64,
    /// Copies per drive.
    pub copies: BTreeMap<String, u64>,
    /// Bytes per drive (copies included).
    pub bytes: BTreeMap<String, u64>,
    /// With one node down while writing: keys whose deepest copy is at each plain rank (1-based).
    pub depth_plain: BTreeMap<usize, u64>,
    /// The same, counted in the filtered ranking (0 = a copy is on a drive the filtered ranking
    /// drops: the reader must probe the sibling drive of a node).
    pub depth_filtered: BTreeMap<usize, u64>,
    /// Keys whose copy set changes when the first drive is removed.
    pub moved_remove: u64,
    /// Keys whose copy set changes when a node of two drives is added.
    pub moved_add: u64,
}

/// Runs the analysis over `(key, size)` with `c` copies, the node `down` (its drives skipped
/// for writes) for the probe-depth part.
#[must_use]
pub fn analyse<'a>(
    objects: impl IntoIterator<Item = (&'a str, u64)>,
    drives: &[Drive],
    c: usize,
    down: &str,
) -> Report {
    let mut r = Report::default();
    let mut grown = drives.to_vec();
    let extra = drives.len() / 2 + 1;
    grown.push(Drive {
        id: format!("n{extra}-hdd1"),
        node: format!("n{extra}"),
        weight: 1.0,
    });
    grown.push(Drive {
        id: format!("n{extra}-hdd2"),
        node: format!("n{extra}"),
        weight: 1.0,
    });
    let shrunk: Vec<Drive> = drives.iter().skip(1).cloned().collect();
    for (key, size) in objects {
        r.keys += 1;
        let ranked = rank(key, drives);
        let all = |_: usize| true;
        let set = copies(&ranked, drives, c, &all);
        for &i in &set {
            *r.copies.entry(drives[i].id.clone()).or_default() += 1;
            *r.bytes.entry(drives[i].id.clone()).or_default() += size;
        }
        let up = |i: usize| drives[i].node != down;
        let written = copies(&ranked, drives, c, &up);
        let deepest = written
            .iter()
            .map(|i| ranked.iter().position(|j| j == i).map_or(0, |p| p + 1))
            .max()
            .unwrap_or(0);
        *r.depth_plain.entry(deepest).or_default() += 1;
        let f = filtered(&ranked, drives);
        let fdeep = written
            .iter()
            .map(|i| f.iter().position(|j| j == i).map(|p| p + 1))
            .try_fold(0, |m, p| p.map(|p| m.max(p)))
            .unwrap_or(0);
        *r.depth_filtered.entry(fdeep).or_default() += 1;
        let ids = |v: &[usize], ds: &[Drive]| {
            let mut s: Vec<String> = v.iter().map(|&i| ds[i].id.clone()).collect();
            s.sort();
            s
        };
        let base = ids(&set, drives);
        let s2 = copies(&rank(key, &shrunk), &shrunk, c, &all);
        if ids(&s2, &shrunk) != base {
            r.moved_remove += 1;
        }
        let g2 = copies(&rank(key, &grown), &grown, c, &all);
        if ids(&g2, &grown) != base {
            r.moved_add += 1;
        }
    }
    r
}

/// The tables: per drive, probe depth, movement.
#[must_use]
pub fn tables(r: &Report, c: usize, m: usize) -> Vec<Table> {
    let mean = if r.copies.is_empty() {
        0.0
    } else {
        r.copies.values().sum::<u64>() as f64 / r.copies.len() as f64
    };
    let mut d = Table::new(
        "C-7, placement: copies per drive",
        &["drive", "copies", "over mean", "GiB"],
    );
    for (id, n) in &r.copies {
        d.row(vec![
            id.clone(),
            n.to_string(),
            num(*n as f64 / mean.max(1e-9), 3),
            num(
                r.bytes.get(id).copied().unwrap_or(0) as f64 / f64::from(1u32 << 30),
                1,
            ),
        ]);
    }
    let pct = |n: u64| {
        if r.keys == 0 {
            0.0
        } else {
            n as f64 * 100.0 / r.keys as f64
        }
    };
    let mut p = Table::new(
        &format!("C-7, probe depth after writes with one node down (c={c}, m={m})"),
        &[
            "deepest copy at rank",
            "% keys (plain ranking)",
            "% keys (filtered ranking)",
        ],
    );
    let ranks: std::collections::BTreeSet<usize> = r
        .depth_plain
        .keys()
        .chain(r.depth_filtered.keys())
        .copied()
        .collect();
    for k in ranks {
        p.row(vec![
            if k == 0 {
                "not in filtered".into()
            } else {
                k.to_string()
            },
            num(pct(r.depth_plain.get(&k).copied().unwrap_or(0)), 2),
            num(pct(r.depth_filtered.get(&k).copied().unwrap_or(0)), 2),
        ]);
    }
    let beyond = |map: &BTreeMap<usize, u64>| {
        map.iter()
            .filter(|(k, _)| **k == 0 || **k > c + m)
            .map(|(_, v)| *v)
            .sum::<u64>()
    };
    let mut mv = Table::new(
        "C-7, keys that move and keys beyond the probe",
        &["what", "% keys"],
    );
    mv.row(vec![
        "copy set changes when one drive is removed".into(),
        num(pct(r.moved_remove), 2),
    ]);
    mv.row(vec![
        "copy set changes when a node of two drives is added".into(),
        num(pct(r.moved_add), 2),
    ]);
    mv.row(vec![
        format!("deepest copy beyond rank c+m={} (plain)", c + m),
        num(pct(beyond(&r.depth_plain)), 2),
    ]);
    mv.row(vec![
        format!("deepest copy beyond rank c+m={} (filtered)", c + m),
        num(pct(beyond(&r.depth_filtered)), 2),
    ]);
    vec![d, p, mv]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_land_evenly_on_distinct_nodes() {
        let drives = farm(6, 2);
        assert_eq!(drives.len(), 12);
        let keys: Vec<String> = (0..12_000).map(|i| format!("cas/{i:08x}")).collect();
        let r = analyse(keys.iter().map(|k| (k.as_str(), 2)), &drives, 2, "n3");
        assert_eq!(r.keys, 12_000);
        assert_eq!(r.copies.values().sum::<u64>(), 24_000);
        // Each drive holds 2000 copies on average; uniform hashing keeps each within 10%.
        assert!(
            r.copies.values().all(|&n| (1800..=2200).contains(&n)),
            "{:?}",
            r.copies
        );
        assert_eq!(r.bytes.values().sum::<u64>(), 48_000);
        // About 2/12 of keys hold the removed drive; adding a node takes about 2/14 of copies.
        let rem = r.moved_remove as f64 / 12_000.0;
        let add = r.moved_add as f64 / 12_000.0;
        assert!((0.13..0.20).contains(&rem), "{rem}");
        assert!((0.20..0.32).contains(&add), "{add}");
        // With a node down, most keys still have their copies within the first 4 ranks.
        let within: u64 = r
            .depth_plain
            .iter()
            .filter(|(k, _)| (1..=4).contains(*k))
            .map(|(_, v)| v)
            .sum();
        assert!(within as f64 / 12_000.0 > 0.8);
        let t = tables(&r, 2, 2);
        assert_eq!(t.len(), 3);
        assert!(t[2].markdown().contains("beyond rank c+m=4 (plain)"));
    }

    #[test]
    fn copies_skip_unusable_drives_and_one_node_twice() {
        let drives = farm(3, 2);
        let ranked = vec![0, 1, 2, 3, 4, 5];
        let all = |_: usize| true;
        assert_eq!(copies(&ranked, &drives, 2, &all), vec![0, 2]);
        let no_n1 = |i: usize| drives[i].node != "n1";
        assert_eq!(copies(&ranked, &drives, 2, &no_n1), vec![2, 4]);
        assert_eq!(copies(&ranked, &drives, 5, &all), vec![0, 2, 4]);
        assert_eq!(filtered(&[1, 0, 3, 2], &drives), vec![1, 3]);
        // Weight raises a drive's share: a drive of weight 100 ranks first for most keys.
        let mut heavy = farm(2, 1);
        heavy[1].weight = 100.0;
        let first = (0..200)
            .filter(|i| rank(&format!("k{i}"), &heavy)[0] == 1)
            .count();
        assert!(first > 180, "{first}");
    }

    #[test]
    fn an_empty_key_set_reports_nothing() {
        let r = analyse(Vec::new(), &farm(6, 2), 2, "n3");
        assert_eq!(r, Report::default());
        let t = tables(&r, 2, 2);
        assert!(t[0].rows().is_empty());
        assert!(t[2].markdown().contains("| 0.00 |"));
        // A one-drive set with a node down has nowhere to write: depth 0 in both rankings.
        let one = farm(1, 1);
        let r = analyse(vec![("k", 1)], &one, 1, "n1");
        assert_eq!(r.depth_plain.get(&0), Some(&1));
        assert!(
            tables(&r, 1, 0)[1]
                .markdown()
                .contains("| not in filtered |")
        );
    }
}
