//! `rledger` — plain text accounting tooling.
//!
//! Two subcommands: `fmt` (a formatter, drop-in equivalent to `hledger-fmt`)
//! and `add` (interactive data entry, epic 2).

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use color_eyre::eyre::Result;

use rledger::fmt::{format, format_sorted, is_formatted, is_formatted_sorted};

#[derive(Parser)]
#[command(name = "rledger", version, about = "Plain text accounting tooling")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Format hledger journal files.
    Fmt(FmtArgs),
    /// Enter transactions interactively (not implemented yet).
    Add,
}

#[derive(clap::Args)]
struct FmtArgs {
    /// Write nothing; exit non-zero if any file is not already formatted.
    #[arg(long)]
    check: bool,
    /// Also sort transactions by date.
    #[arg(long)]
    sort: bool,
    /// Files to format in place. `-`, or no files at all, means stdin.
    files: Vec<PathBuf>,
}

fn main() -> Result<ExitCode> {
    color_eyre::install()?;
    let cli = Cli::parse();
    match cli.command {
        Command::Fmt(args) => Ok(run_fmt(&args)),
        Command::Add => {
            eprintln!("rledger add: not implemented yet");
            Ok(ExitCode::FAILURE)
        }
    }
}

/// The transform applied to a file's contents, per `--sort`.
fn transform(sort: bool, src: &str) -> String {
    if sort {
        format_sorted(src)
    } else {
        format(src)
    }
}

/// Whether a file's contents are already in final form, per `--sort`.
fn formatted(sort: bool, src: &str) -> bool {
    if sort {
        is_formatted_sorted(src)
    } else {
        is_formatted(src)
    }
}

fn run_fmt(args: &FmtArgs) -> ExitCode {
    let stdin_only = args.files.is_empty() || args.files.iter().all(|p| p == Path::new("-"));
    let ok = if stdin_only {
        run_stdin(args.check, args.sort)
    } else {
        run_files(args)
    };
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run_stdin(check: bool, sort: bool) -> bool {
    let mut src = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut src) {
        eprintln!("rledger fmt: stdin: {e}");
        return false;
    }
    if check {
        return formatted(sort, &src);
    }
    let out = transform(sort, &src);
    if let Err(e) = io::stdout().write_all(out.as_bytes()) {
        eprintln!("rledger fmt: stdout: {e}");
        return false;
    }
    true
}

/// Format or check each file. A file that cannot be read or written is
/// reported and skipped; the run then exits non-zero, but the remaining files
/// are still processed.
fn run_files(args: &FmtArgs) -> bool {
    let mut ok = true;
    for path in &args.files {
        let src = match fs::read_to_string(path) {
            Ok(src) => src,
            Err(e) => {
                eprintln!("rledger fmt: {}: {e}", path.display());
                ok = false;
                continue;
            }
        };
        if args.check {
            if !formatted(args.sort, &src) {
                eprintln!("unformatted: {}", path.display());
                ok = false;
            }
            continue;
        }
        let out = transform(args.sort, &src);
        // Leave an already-formatted file untouched on disk.
        if out != src {
            if let Err(e) = fs::write(path, &out) {
                eprintln!("rledger fmt: {}: {e}", path.display());
                ok = false;
            }
        }
    }
    ok
}
