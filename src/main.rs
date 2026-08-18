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

use hledger_x::add::parser::{parse_journal, FileMap, ParseError};
use hledger_x::add::ui::{plain, term, Session, SessionCtx};
use hledger_x::add::write::{integrate_with, Recovery};
use hledger_x::amount::AmountCtx;
use hledger_x::config::Config;
use hledger_x::errors::{display_path, io_reason};
use hledger_x::fmt::{format_opts_at, format_opts_scanned, Options};
use similar::TextDiff;

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
#[allow(
    clippy::struct_excessive_bools,
    reason = "one field per command-line flag; a state machine would only hide the CLI's shape"
)]
struct FmtArgs {
    /// Write nothing; exit 1 if any file is not already formatted.
    #[arg(long)]
    check: bool,
    /// Print a unified diff of every change instead of naming the files.
    /// Implies --check: nothing is written, and the exit status is 1 if
    /// anything would change.
    #[arg(long)]
    diff: bool,
    /// Write out what the journal leaves implied: fill in each transaction's
    /// inferred amount, and pad amounts to their commodity's declared decimal
    /// places (`1 EUR` becomes `1.00 EUR`). Honoured by `--check` too.
    ///
    /// This rewrites amounts, not just their layout, so it is a flag only —
    /// never a config setting.
    #[arg(short = 'x', long)]
    explicit: bool,
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

fn main() -> ExitCode {
    // color-eyre is installed for its panic hook alone. A panic is a bug, and
    // its report is written for whoever fixes it; an error the user can act on
    // is theirs to read, so none of them travel as a `Report`. Letting one
    // reach `main`'s return type is what produced `Error:` banners with a
    // `Location: src/main.rs:156` frame and backtrace instructions.
    drop(color_eyre::install());
    let cli = Cli::parse();
    match cli.command {
        Command::Fmt(args) => run_fmt(&args),
        Command::Add(args) => run_add(&args),
    }
}

/// A failure worth stopping `add` for: what to tell the user, and how to exit.
struct Failure {
    status: Status,
    message: String,
}

impl Failure {
    /// Something went wrong while doing what was asked.
    fn error(message: impl Into<String>) -> Self {
        Self {
            status: Status::Error,
            message: message.into(),
        }
    }

    /// What was asked did not make sense — a bad config or a bad invocation.
    fn usage(message: impl Into<String>) -> Self {
        Self {
            status: Status::Usage,
            message: message.into(),
        }
    }

    /// An I/O failure against a named file, in the house shape.
    fn io(path: &Path, e: &io::Error) -> Self {
        Self::error(format!("{}: {}", display_path(path), io_reason(e)))
    }
}

fn run_add(args: &AddArgs) -> ExitCode {
    match add(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure { status, message }) => {
            eprintln!("hledger-x add: {message}");
            ExitCode::from(status.code())
        }
    }
}

/// `hledger-x add`: parse the journal, run the entry session, write once.
fn add(args: &AddArgs) -> Result<(), Failure> {
    let cwd = std::env::current_dir().map_err(|e| {
        Failure::error(format!(
            "cannot tell what directory this is running in: {}",
            io_reason(&e)
        ))
    })?;
    let config =
        hledger_x::config::load(&cwd).map_err(|e| Failure::usage(format!("config: {e}")))?;

    // Precedence: the flag, then the config's `ledger_file`, then the
    // environment.
    let main_file = args
        .file
        .clone()
        .or_else(|| config.ledger_file.clone())
        .or_else(|| std::env::var_os("LEDGER_FILE").map(PathBuf::from))
        .ok_or_else(|| {
            Failure::usage(
                "no journal file to add to\n  \
                 (pass -f FILE, set `ledger_file` in .hledger-x.toml, or set $LEDGER_FILE)",
            )
        })?;

    let journal = parse_journal(&main_file).map_err(|e| match &e {
        // The journal is the one thing `add` cannot do without, so a missing
        // one earns a way forward rather than just a diagnosis.
        ParseError::Io(_, io) if io.kind() == io::ErrorKind::NotFound => Failure::error(format!(
            "{e}\n  (create it first, or point -f at an existing journal)"
        )),
        _ => Failure::error(e.to_string()),
    })?;
    for w in &journal.warnings {
        eprintln!("warning: {w}");
    }

    // The write target: the main file, or a --to override that must be
    // reachable through the include graph.
    let target = if let Some(to) = &args.to {
        let Some(f) = journal.file(to) else {
            return Err(Failure::usage(format!(
                "--to {}: {} does not include that file\n  \
                 (only files reachable through the journal's `include` directives can be written to)",
                display_path(to),
                display_path(&main_file)
            )));
        };
        f.path.clone()
    } else {
        main_file
    };
    // The declared styles in effect where the target file begins: the file's
    // own text may not contain the `commodity` directives that govern its
    // amounts, and a style declared after it has not been read yet.
    let styles: Vec<(usize, AmountCtx)> = journal.file_index(&target).map_or_else(
        || vec![(0, journal.amount_ctx())],
        |i| journal.inherited_styles(i),
    );
    let target_src = fs::read_to_string(&target).map_err(|e| Failure::io(&target, &e))?;
    let map = FileMap::build_with(&target_src, &styles);

    let today = chrono::Local::now().date_naive();
    let ctx = SessionCtx::new(journal, config.clone(), today, target.clone(), &map);
    let mut session = Session::new(ctx);
    let recovery = Recovery::for_target(&target);

    let interactive = crossterm_is_tty();
    let session_result = if interactive {
        term::run(&mut session, &recovery)
    } else {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        let mut out = io::stdout();
        plain::run(&mut session, &recovery, &mut input, &mut out)
    };
    let completed = session_result.map_err(|e| {
        Failure::error(format!(
            "the entry session ended early: {}{}",
            io_reason(&e),
            kept_safe(&recovery)
        ))
    })?;

    if completed.is_empty() {
        eprintln!("no transactions entered");
        return Ok(());
    }

    // Re-read the target in case something else wrote to it mid-session.
    let src = fs::read_to_string(&target).map_err(|e| {
        Failure::error(format!(
            "{}: {}{}",
            display_path(&target),
            io_reason(&e),
            kept_safe(&recovery)
        ))
    })?;
    let result = integrate_with(&src, &completed, &config.write_options(), &styles);
    for w in &result.warnings {
        eprintln!("warning: {w}");
    }
    fs::write(&target, &result.contents).map_err(|e| {
        Failure::error(format!(
            "{}: could not save your {}: {}{}",
            display_path(&target),
            plural(completed.len(), "transaction"),
            io_reason(&e),
            kept_safe(&recovery)
        ))
    })?;
    recovery.clear();
    eprintln!(
        "wrote {} to {}",
        plural(completed.len(), "transaction"),
        display_path(&target)
    );
    Ok(())
}

/// The reassurance to append when `add` fails after the user has typed
/// something: the recovery journal still holds it. Empty when there is
/// nothing to reassure them about.
fn kept_safe(recovery: &Recovery) -> String {
    if recovery.path().exists() {
        format!(
            "\n  (nothing you entered is lost — it is in {})",
            recovery.path().display()
        )
    } else {
        String::new()
    }
}

/// `1 transaction`, `2 transactions` — never `1 transaction(s)`.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// Whether stdin is a terminal (raw-mode UI) or a pipe (plain line mode).
fn crossterm_is_tty() -> bool {
    use crossterm::tty::IsTty;
    io::stdin().is_tty()
}

/// The transform applied to a file's contents. With a context, amounts
/// restyle to the include tree's declared styles; without one, to the styles
/// declared in the text itself.
fn transform(opts: Options, src: &str, inherited: Option<&[(usize, AmountCtx)]>) -> String {
    inherited.map_or_else(
        || format_opts_scanned(src, opts),
        |i| format_opts_at(src, i, opts),
    )
}

/// A unified diff of one file's before and after, headed the way `git diff`
/// heads its own.
fn write_diff(out: &mut impl Write, display: &str, before: &str, after: &str) -> io::Result<()> {
    write!(
        out,
        "{}",
        TextDiff::from_lines(before, after)
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{display}"), &format!("b/{display}"))
    )
}

/// The declared commodity styles in effect where `path` *starts* — those its
/// include tree declares ahead of it, the way hledger reads them. `None`
/// (falling back to the file's own directives) when the tree cannot be walked
/// or the file is not in it; `fmt` never refuses to format over a parse
/// problem.
///
/// Only what precedes the file, never the whole tree: hledger parses top to
/// bottom, so a style declared later has not been read yet, and restyling an
/// amount by it can change what the amount means.
fn include_tree_ctx(path: &Path) -> Option<Vec<(usize, AmountCtx)>> {
    let journal = parse_journal(path).ok()?;
    let idx = journal.file_index(path)?;
    Some(journal.inherited_styles(idx))
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
    ctxs: Vec<Option<Vec<(usize, AmountCtx)>>>,
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
        for (idx, file) in journal.files.iter().enumerate() {
            // Each file is styled by what the tree declares ahead of it, in
            // include order — the order hledger reads them in.
            let ctx = self.push_ctx(Some(journal.inherited_styles(idx)));
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

    fn push_ctx(&mut self, ctx: Option<Vec<(usize, AmountCtx)>>) -> usize {
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

impl FmtArgs {
    /// Whether this run writes nothing.
    ///
    /// `--diff` implies `--check`: showing the change and making it are
    /// different requests, and every formatter the user is likely to reach
    /// for next — `black --diff`, `gofmt -d`, `ruff format --diff` — treats
    /// the diff as a dry run. Writing while printing what "would" change is
    /// the surprise, whatever `terraform fmt -diff` does.
    const fn dry_run(&self) -> bool {
        self.check || self.diff
    }
}

fn fmt_status(args: &FmtArgs) -> Status {
    let config = match std::env::current_dir() {
        Ok(cwd) => match hledger_x::config::load(&cwd) {
            Ok(config) => config,
            Err(e) => {
                eprintln!("hledger-x fmt: config: {e}");
                return Status::Usage;
            }
        },
        Err(e) => {
            eprintln!(
                "hledger-x fmt: cannot tell what directory this is running in: {}",
                io_reason(&e)
            );
            return Status::Error;
        }
    };
    // Sorting is part of the canonical form, so it lives in the config: a
    // pre-commit hook and a bare `fmt` cannot disagree about what "formatted"
    // means unless a flag says so explicitly.
    // `--explicit` is deliberately absent from the config: it rewrites
    // amounts rather than layout, so every run that does it says so.
    let opts = Options {
        sort: args.sort.resolve(config.sort),
        explicit: args.explicit,
    };
    match select(args, &config) {
        Err(status) => status,
        Ok(Target::Stdin) => run_stdin(args, opts),
        Ok(Target::Files(plan)) => run_files(args, opts, &plan),
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

fn run_stdin(args: &FmtArgs, opts: Options) -> Status {
    let mut src = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut src) {
        eprintln!("hledger-x fmt: cannot read stdin: {}", io_reason(&e));
        return Status::Error;
    }
    let formatted = transform(opts, &src, None);
    let changed = formatted != src;
    let mut out = io::stdout().lock();
    // There is no file to write here, so a diff takes the place of the
    // formatted text rather than accompanying it.
    let written = if args.diff {
        if changed {
            write_diff(&mut out, "<stdin>", &src, &formatted)
        } else {
            Ok(())
        }
    } else if args.dry_run() {
        Ok(())
    } else {
        out.write_all(formatted.as_bytes())
    };
    if let Err(e) = written {
        eprintln!("hledger-x fmt: cannot write to stdout: {}", io_reason(&e));
        return Status::Error;
    }
    if args.dry_run() && changed {
        Status::Unformatted
    } else {
        Status::Ok
    }
}

/// Format or check each file. One that cannot be read or written is reported
/// and skipped; the run then fails, but the remaining files are still
/// processed.
fn run_files(args: &FmtArgs, opts: Options, plan: &Plan) -> Status {
    let mut status = plan.status;
    let mut out = io::stdout().lock();
    for job in &plan.jobs {
        let src = match fs::read_to_string(&job.path) {
            Ok(src) => src,
            Err(e) => {
                eprintln!("hledger-x fmt: {}: {}", job.display, io_reason(&e));
                status.merge(Status::Error);
                continue;
            }
        };
        let ctx = plan
            .ctxs
            .get(job.ctx)
            .and_then(Option::as_ref)
            .map(Vec::as_slice);
        let formatted = transform(opts, &src, ctx);
        // Leave an already-formatted file untouched on disk.
        if formatted == src {
            continue;
        }
        if args.diff {
            let _ = write_diff(&mut out, &job.display, &src, &formatted);
        }
        if args.dry_run() {
            eprintln!("would reformat: {}", job.display);
            status.merge(Status::Unformatted);
            continue;
        }
        if let Err(e) = fs::write(&job.path, &formatted) {
            eprintln!(
                "hledger-x fmt: {}: could not write the formatted file: {}",
                job.display,
                io_reason(&e)
            );
            status.merge(Status::Error);
            continue;
        }
        // Only a run that actually writes gets here, so a `--diff` run never
        // reaches this line — its own header names the file already.
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
