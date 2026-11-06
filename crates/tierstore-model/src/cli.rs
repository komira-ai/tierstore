//! The command line of `tierstore-model`: run named scenarios and print the verdicts.

use crate::scenarios::{Expect, ablation, all, matrix, run, verdict};
use std::io::Write;

/// Run the scenarios named in `args` (`all`, `heavy`, or ids; `-v` prints every trace).
/// Returns false if any verdict is not the required one.
pub fn run_cli(args: &[String], out: &mut dyn Write) -> std::io::Result<bool> {
    let verbose = args.iter().any(|a| a == "-v");
    let names: Vec<&String> = args.iter().filter(|a| *a != "-v").collect();
    let mut ok = true;
    if names.iter().any(|n| *n == "list") {
        for sc in all() {
            let expect = match sc.expect {
                Expect::Pass => "pass".to_string(),
                Expect::Fail(rule) => format!("fail:{rule}"),
            };
            writeln!(
                out,
                "{}\t{}\t{}\t{}\t{}",
                sc.id, sc.heavy, expect, sc.claims, sc.desc
            )?;
        }
        return Ok(true);
    }
    for (word, literal) in [("literal", true), ("corrected", false)] {
        if names.iter().any(|n| *n == word) {
            for r in matrix(literal) {
                let verdict = match &r.broken {
                    None => "PASS".to_string(),
                    Some((id, msg)) => format!("FAIL in {id}: {msg}"),
                };
                writeln!(
                    out,
                    "{:<4} {:<4} scenarios={:<3} states={:<9} {} ({})",
                    r.rule, "", r.scenarios, r.states, verdict, r.what
                )?;
            }
            return Ok(true);
        }
    }
    if names.iter().any(|n| *n == "ablation") {
        for (fix, found) in ablation() {
            match found {
                Some((id, msg)) => writeln!(out, "{fix:<44} needed: without it {id} fails: {msg}")?,
                None => {
                    ok = false;
                    writeln!(out, "{fix:<44} NOT needed by any scenario of the PR set")?;
                }
            }
        }
        return Ok(ok);
    }
    for sc in all() {
        let want = names.is_empty()
            || names
                .iter()
                .any(|n| *n == "all" || **n == sc.id || (*n == "heavy" && sc.heavy));
        if !want {
            continue;
        }
        if sc.heavy
            && !names
                .iter()
                .any(|n| *n == "heavy" || **n == sc.id || *n == "all")
        {
            continue;
        }
        let o = run(&sc);
        let (good, text) = verdict(&sc, &o);
        ok &= good;
        writeln!(
            out,
            "{:<34} {:<9} states={:<9} transitions={:<10} live_starts={:<6} {}",
            sc.id, sc.claims, o.states, o.transitions, o.live_starts, text
        )?;
        if let Some(t) = &o.trace
            && (verbose || !good)
        {
            writeln!(out, "    {}", t.message)?;
            for (i, (a, s)) in t.steps.iter().enumerate() {
                writeln!(out, "    {:>2}. {a}\n        => {s}", i + 1)?;
            }
        }
    }
    Ok(ok)
}
