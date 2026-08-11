//! `hledger-x` — plain text accounting tooling.
//!
//! Two subcommands: `fmt` (a formatter producing `hledger-fmt`'s output behind
//! a project-aware CLI) and `add` (interactive data entry, epic 2).

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{bail, eyre, Result};

use hledger_x::add::parser::{parse_journal, FileMap, ParseError};
use hledger_x::config::Config;
use hledger_x::add::ui::{plain, term, Session, SessionCtx};
use hledger_x::add::write::{integrate_with, Recovery};
use hledger_x::amount::AmountCtx;
use hledger_x::fmt::{
    format, format_sorted, format_sorted_with, format_with, is_formatted, is_formatted_sorted,
    is_formatted_sorted_with, is_formatted_with,
};

#[derive(Parser)]
#[command(name = "hledger-x", version, about = "Plain text accounting tooling")]
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
    /// The journal file. Defaults to the config's `ledger_file`, then
    /// `$LEDGER_FILE`.
    #[arg(short = 'f', long = "file")]
    file: Option<PathBuf>,
    /// Write new transactions into this file instead of the main file. Must
    /// be reachable through the journal's include graph.
    #[arg(long)]
    to: Option<PathBuf>,
}

#[derive(clap::Args)]
struct FmtArgs {
    /// Write nothing; exit 1 if any file is not already formatted.
    #[arg(long)]
    check: bool,
    /// Format this journal together with every file it includes. Repeatable,
    /// and combinable with FILE operands.
    #[arg(short = 'f', long = "follow", value_name = "ROOT")]
    follow: Vec<PathBuf>,
    #[command(flatten)]
    sort: SortFlags,
    /// Do not list the files that changed.
    #[arg(short, long)]
    quiet: bool,
    /// Files to format in place, without following their includes. `-` means
    /// stdin. With no files and no `--follow`, the configured journal and its
    /// includes are formatted.
    #[arg(value_name = "FILE")]
    files: Vec<PathBuf>,
}

/// The `--sort` / `--no-sort` pair, which overrides the configured canonical
/// form in either direction.
#[derive(clap::Args)]
struct SortFlags {
    /// Also sort transactions by date, overriding the config's `sort`.
    #[arg(long, overrides_with = "no_sort")]
    sort: bool,
    /// Leave transaction order alone, overriding the config's `sort`.
    #[arg(long = "no-sort", overrides_with = "sort")]
    no_sort: bool,
}

impl SortFlags {
    /// Whether to sort, given what the config asked for. `clap`'s
    /// `overrides_with` settles `--sort --no-sort` by last-one-wins, so at
    /// most one of the two is set here.
    const fn resolve(&self, configured: bool) -> bool {
        if self.sort {
            true
        } else if self.no_sort {
            false
        } else {
            configured
        }
    }
}

/// How the run ended. Ordered worst-last: a run that both finds unformatted
/// files and hits an error reports the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
enum Status {
    /// Everything was formatted, or already was.
    #[default]
    Ok,
    /// `--check` found files that need formatting.
    Unformatted,
    /// The invocation itself did not make sense.
    Usage,
    /// A file could not be read, written, or walked.
    Error,
}

impl Status {
    const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Unformatted => 1,
            Self::Usage => 2,
            Self::Error => 3,
        }
    }

    /// Keep the worse of the two.
    fn merge(&mut self, other: Self) {
        if other > *self {
            *self = other;
        }
    }
}

fn main() -> Result<ExitCode> {
    color_eyre::install()?;
    let cli = Cli::parse();
    match cli.command {
        Command::Fmt(args) => Ok(run_fmt(&args)),
        Command::Add(args) => run_add(&args),
    }
}

/// `hledger-x add`: parse the journal, run the entry session, write once.
fn run_add(args: &AddArgs) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let config = hledger_x::config::load(&cwd).map_err(|e| eyre!("config: {e}"))?;

    // Precedence: the flag, then the config's `ledger_file`, then the
    // environment.
    let main_file = args
        .file
        .clone()
        .or_else(|| config.ledger_file.clone())
        .or_else(|| std::env::var_os("LEDGER_FILE").map(PathBuf::from))
        .ok_or_else(|| {
            eyre!("no journal file: pass -f FILE, set ledger_file in the config, or set $LEDGER_FILE")
        })?;

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

/// One file to format, and where its declared styles come from.
struct Job {
    path: PathBuf,
    /// How the file is named in output: as the user wrote it for an operand,
    /// relative to the working directory for one found through an include.
    display: String,
    /// Index into [`Plan::ctxs`]. `None` means the file's own directives.
    ctx: usize,
}

/// The files a `fmt` invocation selects, in the order they were selected.
#[derive(Default)]
struct Plan {
    /// Declared styles, one entry per include tree walked. Held here rather
    /// than in the jobs so a tree's files share a single copy.
    ctxs: Vec<Option<AmountCtx>>,
    jobs: Vec<Job>,
    /// Canonical paths already in `jobs`: a file reachable from two roots, or
    /// named as an operand as well, is formatted once.
    seen: HashSet<PathBuf>,
    /// Roots that could not be walked. Reported, then the run continues.
    status: Status,
}

impl Plan {
    /// Add `root` and every file it includes, all sharing the tree's styles.
    fn push_tree(&mut self, root: &Path) {
        if self.reject_directory(root) {
            return;
        }
        let journal = match parse_journal(root) {
            Ok(journal) => journal,
            // A cycle leaves the root itself perfectly formattable, so fall
            // back to it alone rather than dropping it. An I/O failure means
            // there is nothing to fall back to.
            Err(e @ ParseError::Cycle(_)) => {
                eprintln!("hledger-x fmt: {e}");
                self.status.merge(Status::Error);
                let ctx = self.push_ctx(None);
                self.push_job(root.to_path_buf(), display_path(root), ctx);
                return;
            }
            Err(e) => {
                eprintln!("hledger-x fmt: {e}");
                self.status.merge(Status::Error);
                return;
            }
        };
        for w in &journal.warnings {
            eprintln!("warning: {w}");
        }
        let ctx = self.push_ctx(Some(journal.amount_ctx()));
        for file in &journal.files {
            let display = display_path(&file.path);
            self.push_job(file.path.clone(), display, ctx);
        }
    }

    /// Add a single file, styled by its own include tree — the tree is read
    /// for styles, never formatted.
    fn push_file(&mut self, path: &Path) {
        if self.reject_directory(path) || self.seen.contains(&canonical(path)) {
            return;
        }
        let ctx = self.push_ctx(include_tree_ctx(path));
        self.push_job(path.to_path_buf(), path.display().to_string(), ctx);
    }

    fn push_ctx(&mut self, ctx: Option<AmountCtx>) -> usize {
        self.ctxs.push(ctx);
        // The index of what was just pushed; 0 only if the vector was empty.
        self.ctxs.len().saturating_sub(1)
    }

    fn push_job(&mut self, path: PathBuf, display: String, ctx: usize) {
        if self.seen.insert(canonical(&path)) {
            self.jobs.push(Job { path, display, ctx });
        }
    }

    /// Directory operands are a deliberate error, not a leaked `os error 21`.
    fn reject_directory(&mut self, path: &Path) -> bool {
        if !path.is_dir() {
            return false;
        }
        eprintln!(
            "hledger-x fmt: {}: is a directory\n  \
             (pass journal files, or run `hledger-x fmt` with no arguments to format the configured journal)",
            display_path(path)
        );
        self.status.merge(Status::Error);
        true
    }
}

/// What the operands select.
enum Target {
    Stdin,
    Files(Plan),
}

fn run_fmt(args: &FmtArgs) -> ExitCode {
    ExitCode::from(fmt_status(args).code())
}

fn fmt_status(args: &FmtArgs) -> Status {
    let config = match std::env::current_dir().map_err(|e| e.to_string()) {
        Ok(cwd) => match hledger_x::config::load(&cwd) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("hledger-x fmt: config: {e}");
                return Status::Usage;
            }
        },
        Err(e) => {
            eprintln!("hledger-x fmt: {e}");
            return Status::Error;
        }
    };
    // Sorting is part of the canonical form, so it lives in the config: a
    // pre-commit hook and a bare `fmt` cannot disagree about what "formatted"
    // means unless a flag says so explicitly.
    let sort = args.sort.resolve(config.sort);
    match select(args, &config) {
        Err(status) => status,
        Ok(Target::Stdin) => run_stdin(args.check, sort),
        Ok(Target::Files(plan)) => run_files(args, sort, &plan),
    }
}

/// Resolve the operands into a target, or into the status to exit with.
fn select(args: &FmtArgs, config: &Config) -> Result<Target, Status> {
    let dashes = args.files.iter().filter(|p| *p == Path::new("-")).count();
    if dashes > 0 {
        if dashes != args.files.len() || !args.follow.is_empty() {
            eprintln!(
                "hledger-x fmt: `-` reads stdin and cannot be combined with other files or --follow"
            );
            return Err(Status::Usage);
        }
        return Ok(Target::Stdin);
    }

    let mut plan = Plan::default();
    if args.follow.is_empty() && args.files.is_empty() {
        let root = config
            .ledger_file
            .clone()
            .or_else(|| std::env::var_os("LEDGER_FILE").map(PathBuf::from))
            .ok_or_else(|| {
                eprintln!(
                    "hledger-x fmt: no journal file: pass FILE..., use -f/--follow FILE, \
                     set ledger_file in the config, or set $LEDGER_FILE"
                );
                Status::Usage
            })?;
        plan.push_tree(&root);
    } else {
        // Roots first, so a file in a tree keeps the tree's styles even when
        // it is also named as an operand.
        for root in &args.follow {
            plan.push_tree(root);
        }
        for file in &args.files {
            plan.push_file(file);
        }
    }
    Ok(Target::Files(plan))
}

fn run_stdin(check: bool, sort: bool) -> Status {
    let mut src = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut src) {
        eprintln!("hledger-x fmt: stdin: {e}");
        return Status::Error;
    }
    if check {
        return if formatted(sort, &src, None) {
            Status::Ok
        } else {
            Status::Unformatted
        };
    }
    let out = transform(sort, &src, None);
    if let Err(e) = io::stdout().write_all(out.as_bytes()) {
        eprintln!("hledger-x fmt: stdout: {e}");
        return Status::Error;
    }
    Status::Ok
}

/// Format or check each file. One that cannot be read or written is reported
/// and skipped; the run then fails, but the remaining files are still
/// processed.
fn run_files(args: &FmtArgs, sort: bool, plan: &Plan) -> Status {
    let mut status = plan.status;
    let mut out = io::stdout().lock();
    for job in &plan.jobs {
        let src = match fs::read_to_string(&job.path) {
            Ok(src) => src,
            Err(e) => {
                eprintln!("hledger-x fmt: {}: {e}", job.display);
                status.merge(Status::Error);
                continue;
            }
        };
        let ctx = plan.ctxs.get(job.ctx).and_then(Option::as_ref);
        if args.check {
            if !formatted(sort, &src, ctx) {
                eprintln!("would reformat: {}", job.display);
                status.merge(Status::Unformatted);
            }
            continue;
        }
        let formatted = transform(sort, &src, ctx);
        // Leave an already-formatted file untouched on disk.
        if formatted == src {
            continue;
        }
        if let Err(e) = fs::write(&job.path, &formatted) {
            eprintln!("hledger-x fmt: {}: {e}", job.display);
            status.merge(Status::Error);
            continue;
        }
        if !args.quiet {
            let _ = writeln!(out, "{}", job.display);
        }
    }
    status
}

/// Canonicalize for identity comparisons, falling back to the path as given
/// for anything that cannot be resolved.
fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A path as it is worth showing: relative to the working directory when it
/// sits underneath it, since that is how the user named it.
fn display_path(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(&cwd).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}
