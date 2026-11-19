//! The Markdown tables of C-7 (see [`crate::placement`]).

use crate::placement::{Spread, Summary};
use crate::table::{Table, bytes, count, num};

/// The nodes down as a table cell (`none`, `n1`, `n0+n2`).
#[must_use]
pub fn down_label(down: &[u8]) -> String {
    if down.is_empty() {
        return "none".to_owned();
    }
    down.iter()
        .map(|n| format!("n{n}"))
        .collect::<Vec<_>>()
        .join("+")
}

/// A share of 1 as a percentage with two decimals.
fn percent(x: f64) -> String {
    num(x * 100.0, 2)
}

/// `min / mean / max` of a spread, as percentages.
fn triple(s: &Spread) -> String {
    format!(
        "{} / {} / {}",
        percent(s.min),
        percent(s.mean),
        percent(s.max)
    )
}

/// The Markdown tables of the measurement document for `summary`.
#[must_use]
pub fn tables(summary: &Summary) -> Vec<Table> {
    let mut out = Vec::new();
    for p in &summary.profiles {
        let name = &p.profile.name;
        let layout = p.profile.layout();
        // With a replica set smaller than the cluster the design's writer is T-S1's rule.
        let writer_note = if p.profile.replicas < usize::from(p.profile.nodes) {
            "; writer: the first live node of the rank, standing in for T-S1's rule"
        } else {
            ""
        };
        let mut keys = Table::new(
            format!(
                "C-7, key placement under node rank, {name} ({} objects, {}{writer_note})",
                count(summary.objects),
                bytes(summary.bytes)
            ),
            &[
                "nodes down",
                "writer keys, % min / mean / max",
                "writer bytes, % min / mean / max",
                "replica keys, % min / mean / max",
                "replica bytes, % min / mean / max",
            ],
        );
        let mut sets = Table::new(
            format!("C-7, replica sets of the key set, {name}"),
            &[
                "nodes down",
                "set keys, % min / mean / max",
                "set bytes, % min / mean / max",
            ],
        );
        for k in &p.keys {
            keys.row(vec![
                down_label(&k.down),
                triple(&k.writer_keys),
                triple(&k.writer_bytes),
                triple(&k.replica_keys),
                triple(&k.replica_bytes),
            ]);
            if let (Some(sk), Some(sb)) = (&k.set_keys, &k.set_bytes) {
                sets.row(vec![down_label(&k.down), triple(sk), triple(sb)]);
            }
        }
        if !keys.rows().is_empty() {
            out.push(keys);
        }
        if !sets.rows().is_empty() {
            out.push(sets);
        }
        for pop in &p.populations {
            let mut t = Table::new(
                format!(
                    "C-7, new stripes {layout}, {name}, {} ({} stripes; other bytes per drive: {})",
                    pop.name,
                    count(pop.stripes),
                    bytes(p.profile.shared_bytes_per_drive)
                ),
                &[
                    "nodes down",
                    "placed",
                    "refused",
                    "why",
                    "shards per node, min / max",
                    "shards per drive, min / mean / max",
                    "shards per drive, cv",
                    "drive fill %, min / max",
                ],
            );
            for s in &pop.scenarios {
                t.row(vec![
                    down_label(&s.down),
                    count(s.placed),
                    count(s.refused),
                    s.first_refusal
                        .as_ref()
                        .map_or_else(|| "-".to_owned(), ToString::to_string),
                    format!("{} / {}", s.node_shards_min, s.node_shards_max),
                    format!(
                        "{} / {} / {}",
                        s.drive_shards_min,
                        num(s.drive_shards_mean, 1),
                        s.drive_shards_max
                    ),
                    num(s.drive_shards_cv, 3),
                    format!("{} / {}", percent(s.fill_min), percent(s.fill_max)),
                ]);
            }
            out.push(t);
        }
        let mut t = Table::new(
            format!("C-7, existing {layout} stripes under failure, {name} (stripes of L_silo)"),
            &[
                "nodes down",
                "stripes",
                "margin (surviving - k): stripes",
                "unreadable",
                "degraded cold reads, %",
            ],
        );
        for f in &p.failures {
            let margins: Vec<String> = f
                .margins
                .iter()
                .map(|(m, n)| format!("{m}: {}", count(*n)))
                .collect();
            t.row(vec![
                down_label(&f.down),
                count(f.stripes),
                margins.join(", "),
                count(f.unreadable),
                percent(f.degraded_share),
            ]);
        }
        out.push(t);
    }
    out
}
