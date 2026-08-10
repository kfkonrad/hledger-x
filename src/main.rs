//! `rledger` — plain text accounting tooling.
//!
//! Two subcommands: `fmt` (a formatter, drop-in equivalent to `hledger-fmt`)
//! and `add` (interactive data entry, epic 2).

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{bail, eyre, Result};

use rledger::add::parser::{parse_journal, FileMap};
use rledger::add::ui::{plain, term, Session, SessionCtx};
use rledger::add::write::{integrate_with, Recovery};
use rledger::amount::AmountCtx;
use rledger::fmt::{
    format, format_sorted, format_sorted_with, format_with, is_formatted, is_formatted_sorted,
    is_formatted_sorted_with, is_formatted_with,
};

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
    /// Enter transactions interactively.
    Add(AddArgs),
}

#[derive(clap::Args)]
struct AddArgs {
    /// The journal file. Defaults to `$LEDGER_FILE`.
    #[arg(short = 'f', long = "file")]
    file: Option<PathBuf>,
    /// Write new transactions into this file instead of the main file. Must
    /// be reachable through the journal's include graph.
    #[arg(long)]
    to: Option<PathBuf>,
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
        Command::Add(args) => run_add(&args),
    }
}

/// `rledger add`: parse the journal, run the entry session, write once.
fn run_add(args: &AddArgs) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let config = rledger::config::load(&cwd).map_err(|e| eyre!("config: {e}"))?;

    let main_file = args
        .file
        .clone()
        .or_else(|| std::env::var_os("LEDGER_FILE").map(PathBuf::from))
        .ok_or_else(|| eyre!("no journal file: pass -f FILE or set $LEDGER_FILE"))?;

    let journal = parse_journal(&main_file)?;
    for w in &journal.warnings {
        eprintln!("warning: {w}");
    }

    // The write target: the main file, or a --to override that must be
    // reachable through the include graph.
    let target = if let Some(to) = &args.to {
        let Some(f) = journal.file(to) else {
            bail!(
                "{} is not reachable through the include graph of {}",
                to.display(),
                main_file.display()
            );
        };
        f.path.clone()
    } else {
        main_file
    };
    // The include tree's declared styles: the target's own text may not
    // contain the `commodity` directives that govern its amounts.
    let styles = journal.amount_ctx();
    let target_src = fs::read_to_string(&target)?;
    let map = FileMap::build_with(&target_src, &styles);

    let today = chrono::Local::now().date_naive();
    let ctx = SessionCtx::new(journal, config.clone(), today, target.clone(), &map);
    let mut session = Session::new(ctx);
    let recovery = Recovery::for_target(&target);

    let interactive = crossterm_is_tty();
    let completed = if interactive {
        term::run(&mut session, &recovery)?
    } else {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut out = io::stdout();
        plain::run(&mut session, &recovery, &mut input, &mut out)?
    };

    if completed.is_empty() {
        eprintln!("no transactions entered");
        return Ok(ExitCode::SUCCESS);
    }

    // Re-read the target in case something else wrote to it mid-session.
    let src = fs::read_to_string(&target)?;
    let result = integrate_with(&src, &completed, &config.write_options(), &styles);
    for w in &result.warnings {
        eprintln!("warning: {w}");
    }
    fs::write(&target, &result.contents)?;
    recovery.clear();
    eprintln!(
        "wrote {} transaction(s) to {}",
        completed.len(),
        target.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// Whether stdin is a terminal (raw-mode UI) or a pipe (plain line mode).
fn crossterm_is_tty() -> bool {
    use crossterm::tty::IsTty;
    io::stdin().is_tty()
}

/// The transform applied to a file's contents, per `--sort`. With a context,
/// amounts restyle to the include tree's declared styles; without one, to
/// the styles declared in the text itself.
fn transform(sort: bool, src: &str, ctx: Option<&AmountCtx>) -> String {
    match (sort, ctx) {
        (true, Some(c)) => format_sorted_with(src, c),
        (true, None) => format_sorted(src),
        (false, Some(c)) => format_with(src, c),
        (false, None) => format(src),
    }
}

/// Whether a file's contents are already in final form, per `--sort`.
fn formatted(sort: bool, src: &str, ctx: Option<&AmountCtx>) -> bool {
    match (sort, ctx) {
        (true, Some(c)) => is_formatted_sorted_with(src, c),
        (true, None) => is_formatted_sorted(src),
        (false, Some(c)) => is_formatted_with(src, c),
        (false, None) => is_formatted(src),
    }
}

/// The declared commodity styles visible from `path` — its whole include
/// tree's, the way hledger resolves them. `None` (falling back to the text's
/// own directives) when the tree cannot be walked; `fmt` never refuses to
/// format over a parse problem.
fn include_tree_ctx(path: &Path) -> Option<AmountCtx> {
    parse_journal(path).ok().map(|j| j.amount_ctx())
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
        return formatted(sort, &src, None);
    }
    let out = transform(sort, &src, None);
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
        let ctx = include_tree_ctx(path);
        if args.check {
            if !formatted(args.sort, &src, ctx.as_ref()) {
                eprintln!("unformatted: {}", path.display());
                ok = false;
            }
            continue;
        }
        let out = transform(args.sort, &src, ctx.as_ref());
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
