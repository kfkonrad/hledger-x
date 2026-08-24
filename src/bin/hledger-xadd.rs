//! `hledger-xadd` — ergonomic interactive data entry for hledger journals.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use hledger_x::add::parser::{parse_journal, FileMap, ParseError};
use hledger_x::add::ui::{plain, term, Session, SessionCtx};
use hledger_x::add::write::{integrate_in, Integration, Recovery, TargetScopes};
use hledger_x::amount::AmountCtx;
use hledger_x::config::Config;
use hledger_x::errors::{display_path, io_reason};
use hledger_x::status::Status;

#[derive(Parser)]
#[command(
    name = "hledger-xadd",
    version,
    about = "Enter hledger transactions interactively"
)]
struct Cli {
    /// The journal file. Defaults to the config's `ledger_file`, then
    /// `$LEDGER_FILE`.
    #[arg(short = 'f', long = "file")]
    file: Option<PathBuf>,
    /// Write new transactions into this file instead of the main file. Must
    /// be reachable through the journal's include graph.
    #[arg(long)]
    to: Option<PathBuf>,
}

fn main() -> ExitCode {
    // color-eyre is installed for its panic hook alone. A panic is a bug, and
    // its report is written for whoever fixes it; an error the user can act on
    // is theirs to read, so none of them travel as a `Report`. Letting one
    // reach `main`'s return type is what produced `Error:` banners with a
    // `Location:` frame and backtrace instructions.
    drop(color_eyre::install());
    let cli = Cli::parse();
    match add(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure { status, message }) => {
            eprintln!("hledger-xadd: {message}");
            ExitCode::from(status.code())
        }
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

/// Parse the journal, run the entry session, write once.
fn add(args: &Cli) -> Result<(), Failure> {
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
    // `apply account` / `alias` regions in the write target decide how account
    // names are spelled where they land. Say so up front rather than after the
    // user has typed a batch of transactions.
    let scopes = journal
        .file(&target)
        .map(TargetScopes::of)
        .unwrap_or_default();
    if scopes.any_active() {
        eprintln!(
            "note: {} has `apply account` or `alias` regions; \
             account names are written as those directives require",
            display_path(&target)
        );
    }

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

    save(&completed, &target, &config, &styles, &scopes, &recovery)
}

/// Integrate the session's transactions into the target file and write it.
fn save(
    completed: &[hledger_x::add::write::NewTransaction],
    target: &Path,
    config: &Config,
    styles: &[(usize, AmountCtx)],
    scopes: &TargetScopes,
    recovery: &Recovery,
) -> Result<(), Failure> {
    // Re-read the target in case something else wrote to it mid-session.
    let src = fs::read_to_string(target).map_err(|e| {
        Failure::error(format!(
            "{}: {}{}",
            display_path(target),
            io_reason(&e),
            kept_safe(recovery)
        ))
    })?;
    let result = match integrate_in(&src, completed, &config.write_options(), styles, scopes) {
        Integration::Ready(out) => out,
        // Writing the name as-is would silently enter a different account, so
        // this is one of the few places where blocking beats proceeding. The
        // recovery journal keeps the work.
        Integration::Refused(reasons) => {
            return Err(Failure::error(format!(
                "{}: {}{}",
                display_path(target),
                reasons.join("\n  "),
                kept_safe(recovery)
            )));
        }
    };
    for w in &result.warnings {
        eprintln!("warning: {w}");
    }
    fs::write(target, &result.contents).map_err(|e| {
        Failure::error(format!(
            "{}: could not save your {}: {}{}",
            display_path(target),
            plural(completed.len(), "transaction"),
            io_reason(&e),
            kept_safe(recovery)
        ))
    })?;
    recovery.clear();
    eprintln!(
        "wrote {} to {}",
        plural(completed.len(), "transaction"),
        display_path(target)
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
