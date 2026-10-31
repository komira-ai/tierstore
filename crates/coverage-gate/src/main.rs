//! `coverage-gate`: prints per-crate line and branch coverage from an lcov tracefile and
//! fails when a crate is under 99%. See the library docs.
//!
//! Exit status: 0 when every judged crate passes; 1 when the gate fails; 2 when an input
//! cannot be read or parsed (and for usage errors, as clap reports them).

use std::collections::BTreeSet;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use coverage_gate::{gate, lcov};

/// Per-crate coverage table and gate.
#[derive(Debug, Parser)]
struct Cli {
    /// The lcov tracefile `cargo llvm-cov --lcov` wrote.
    #[arg(long)]
    lcov: PathBuf,
    /// The workspace root: absolute, spelled as the tracefile's source paths begin, or
    /// relative to the working directory.
    #[arg(long)]
    root: PathBuf,
    /// A crate that is listed but not judged (repeat for more): tooling and test helpers.
    #[arg(long)]
    exclude: Vec<String>,
    /// Also append the table to this file (created if absent), as a CI job summary:
    /// `--summary-file "$GITHUB_STEP_SUMMARY"`.
    #[arg(long)]
    summary_file: Option<PathBuf>,
}

fn main() -> ExitCode {
    match run(&Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("coverage-gate: {e}");
            ExitCode::from(2)
        }
    }
}

fn read(path: &Path) -> Result<String, Box<dyn Error>> {
    std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()).into())
}

fn append(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?
        .write_all(text.as_bytes())
}

/// `root` as the tracefile spells its source paths. `cargo llvm-cov` writes them absolute,
/// under the directory cargo found the workspace in (the process's working directory, symbolic
/// links resolved), so a relative `--root` (`.`, `..`) is resolved the same way. An absolute
/// one is taken as written, as CI passes it.
fn source_root(root: &Path) -> Result<PathBuf, Box<dyn Error>> {
    if root.is_absolute() {
        return Ok(root.to_path_buf());
    }
    std::fs::canonicalize(root).map_err(|e| format!("--root {}: {e}", root.display()).into())
}

/// Prints the table (and appends it to `--summary-file`); returns whether the gate passed.
fn run(cli: &Cli) -> Result<bool, Box<dyn Error>> {
    let files =
        lcov::parse(&read(&cli.lcov)?).map_err(|e| format!("{}: {e}", cli.lcov.display()))?;
    let crates = gate::workspace_crates(&cli.root)
        .map_err(|e| format!("list crates under {}: {e}", cli.root.display()))?;
    let measured = gate::by_crate(&source_root(&cli.root)?, &crates, &files)?;
    let excluded: BTreeSet<String> = cli.exclude.iter().cloned().collect();
    let rows = gate::rows(&measured, &excluded);
    let report = gate::render(&rows);
    print!("{report}");
    if let Some(path) = &cli.summary_file {
        append(path, &report).map_err(|e| format!("append to {}: {e}", path.display()))?;
    }
    Ok(!rows.iter().any(|r| r.status().fails()))
}
