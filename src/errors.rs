//! Human-facing error text.
//!
//! Everything the user reads on stderr is phrased here. The rule: no Rust and
//! no OS internals. No `os error 2`, no serde's `invalid type: string "yes",
//! expected a boolean`, no `Location:` frame from an
//! [`eyre::Report`](color_eyre::eyre::Report). A message names what failed,
//! says why in plain language, and — where knowing why is not enough to act —
//! carries an indented hint saying what to do about it.
//!
//! The house shape, which every call site follows:
//!
//! ```text
//! hledger-xfmt: main.journal: no such file
//!   (create it, or name a different file)
//! ```

use std::io;
use std::path::Path;

/// A path as it is worth showing.
///
/// Relative to the working directory when it sits underneath it, since that is
/// how the user named it and how they will find it again. Anything outside — a
/// config file under `$HOME`, an include reached by absolute path — stays
/// absolute, because there the full path is the information.
#[must_use]
pub fn display_path(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(&cwd).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}

/// Why an I/O operation failed, in words rather than an errno.
///
/// `std::io::Error`'s own `Display` appends the raw OS code — `No such file or
/// directory (os error 2)` — which tells the user nothing they can act on and
/// reads like a stack trace. The kinds worth naming are named; anything else
/// falls back to the OS's own sentence with the errno stripped.
#[must_use]
pub fn io_reason(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => "no such file".to_owned(),
        io::ErrorKind::PermissionDenied => "permission denied".to_owned(),
        io::ErrorKind::IsADirectory => "is a directory".to_owned(),
        io::ErrorKind::NotADirectory => "a parent of this path is not a directory".to_owned(),
        io::ErrorKind::StorageFull => "the disk is full".to_owned(),
        io::ErrorKind::ReadOnlyFilesystem => "the filesystem is read-only".to_owned(),
        io::ErrorKind::InvalidData => "not valid UTF-8 text".to_owned(),
        _ => lower_first(&strip_errno(&e.to_string())),
    }
}

/// Drop the ` (os error 13)` tail the OS layer appends.
fn strip_errno(s: &str) -> String {
    s.split_once(" (os error ")
        .map_or_else(|| s.to_owned(), |(head, _)| head.to_owned())
}

/// Lowercase the first character only: these messages follow a colon, and
/// lowercasing the whole sentence would mangle any proper noun in it.
fn lower_first(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

/// A configuration problem phrased for someone editing a TOML file, not for
/// someone reading serde's internals.
///
/// Replaces the `toml` crate's rustc-style diagnostic — a `|` gutter with a
/// caret run under the offending span — with a line number and a plain echo of
/// the line, and rewrites serde's vocabulary (`unknown field`, `invalid type`,
/// `unknown variant`) into the words the config file itself uses.
#[must_use]
pub fn toml_error(src: &str, origin: &str, e: &toml::de::Error) -> String {
    let (message, hint) = humanize(e.message());
    let located = e
        .span()
        .and_then(|span| locate(src, span.start))
        .map_or_else(
            || format!("{origin}: {message}"),
            |(line_no, line)| format!("{origin}, line {line_no}: {message}\n      {}", line.trim()),
        );
    hint.map_or_else(|| located.clone(), |h| format!("{located}\n  ({h})"))
}

/// The 1-based line number holding `byte`, and that line's text.
fn locate(src: &str, byte: usize) -> Option<(usize, &str)> {
    let mut offset = 0usize;
    for (i, line) in src.lines().enumerate() {
        let end = offset.saturating_add(line.len());
        if byte <= end {
            return Some((i.saturating_add(1), line));
        }
        offset = end.saturating_add(1);
    }
    None
}

/// Rewrite one serde/toml message into a message plus an optional hint.
/// Anything unrecognised — TOML's own syntax complaints, which are already
/// phrased in terms of the file — passes through untouched.
fn humanize(msg: &str) -> (String, Option<String>) {
    if let Some((field, valid)) = parse_backticked(msg, "unknown field `", ", expected one of ") {
        // A near-miss against the settings valid *here* wins: inside `[add]`,
        // `formatfile` is a typo for a setting of that table, not a setting
        // written in the wrong table.
        let hint = suggest(field, valid).map_or_else(
            || {
                misplaced(field).map_or_else(
                    || format!("valid settings are {valid}"),
                    |setting| format!("`{setting}` belongs in the [add] section"),
                )
            },
            |near| format!("did you mean `{near}`?"),
        );
        return (format!("unknown setting `{field}`"), Some(hint));
    }
    if let Some((variant, valid)) = parse_backticked(msg, "unknown variant `", ", expected ") {
        // `expected one of `a`, `b`` for a multi-variant enum, plain
        // ``a`` for a single one; the near-miss reads either.
        let list = valid.strip_prefix("one of ").unwrap_or(valid);
        let hint = suggest(variant, list).map_or_else(
            || format!("expected {valid}"),
            |near| format!("did you mean `{near}`?"),
        );
        return (format!("`{variant}` is not a valid value"), Some(hint));
    }
    if let Some(rest) = msg.strip_prefix("invalid type: ") {
        if let Some((found, want)) = rest.rsplit_once(", expected ") {
            return (
                format!(
                    "expected {}, but found {}",
                    describe_want(want),
                    describe_found(found)
                ),
                None,
            );
        }
    }
    (msg.to_owned(), None)
}

/// Pull `NAME` and the trailing `REST` out of a
/// `<prefix>NAME<backtick><joiner>REST` message.
fn parse_backticked<'a>(msg: &'a str, prefix: &str, joiner: &str) -> Option<(&'a str, &'a str)> {
    let rest = msg.strip_prefix(prefix)?;
    let (name, tail) = rest.split_once('`')?;
    Some((name, tail.strip_prefix(joiner)?))
}

/// The `[add]` setting an unknown top-level key is, written in the wrong
/// place. Matched the same forgiving way as [`suggest`], so `formatFile` at
/// the top level is pointed at the section rather than at itself.
fn misplaced(field: &str) -> Option<&'static str> {
    let target = squash(field);
    crate::config::ADD_SETTINGS
        .iter()
        .copied()
        .find(|s| squash(s) == target)
}

/// The valid setting or value a typo most likely meant. Deliberately narrow:
/// it matches on separators, case, and a missing or spare trailing `s`, which
/// covers the mistakes people actually make (`formatfile`, `formatFile`,
/// `format-file`, `payee` for `payees`) and never guesses wildly.
fn suggest<'a>(field: &str, valid: &'a str) -> Option<&'a str> {
    let target = squash(field);
    // serde joins two alternatives with ` or `, more than two with `, `.
    let mut candidates = valid
        .split(", ")
        .flat_map(|c| c.split(" or "))
        .map(|c| c.trim_matches('`'));
    candidates
        .clone()
        .find(|c| squash(c) == target)
        .or_else(|| candidates.find(|c| plural_of(&squash(c), &target)))
}

/// Whether two squashed names differ only by a trailing `s`.
fn plural_of(a: &str, b: &str) -> bool {
    let longer_is = |long: &str, short: &str| long.strip_suffix('s') == Some(short);
    longer_is(a, b) || longer_is(b, a)
}

fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// serde's name for a type, in the config file's own terms.
fn describe_want(want: &str) -> String {
    match want {
        "a boolean" => "true or false".to_owned(),
        "a string" => "text in quotes".to_owned(),
        "a sequence" => "a list".to_owned(),
        "a map" => "a table".to_owned(),
        "an integer" | "a floating point number" => "a number".to_owned(),
        other => other.to_owned(),
    }
}

/// serde's rendering of the value it found, likewise.
fn describe_found(found: &str) -> String {
    let (kind, value) = found.rsplit_once(' ').unwrap_or(("", found));
    let bare = value.trim_matches('`');
    match kind {
        "string" => format!("the text {value}"),
        "boolean" => format!("the value {bare}"),
        "" => found.to_owned(),
        k if k.contains("integer") || k.contains("number") => format!("the number {bare}"),
        _ => found.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_kinds_read_as_english() {
        let e = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(io_reason(&e), "no such file");
        let e = io::Error::from(io::ErrorKind::PermissionDenied);
        assert_eq!(io_reason(&e), "permission denied");
    }

    #[test]
    fn errno_never_reaches_the_user() {
        // The real OS error, as `fs::read_to_string` would hand it over.
        let e = std::fs::read_to_string("/nonexistent-hledger-x-test").unwrap_err();
        let msg = io_reason(&e);
        assert!(!msg.contains("os error"), "{msg}");
    }

    #[test]
    fn unrecognised_kinds_lose_their_errno() {
        let e = io::Error::other("Broken pipe (os error 32)");
        assert_eq!(io_reason(&e), "broken pipe");
    }

    fn render(src: &str) -> String {
        #[derive(serde::Deserialize, Debug)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct Raw {
            format_file: Option<bool>,
            sort: Option<bool>,
            insertion: Option<Insertion>,
        }
        #[derive(serde::Deserialize, Debug)]
        #[serde(rename_all = "lowercase")]
        enum Insertion {
            Append,
            Chronological,
        }
        let e = toml::from_str::<Raw>(src).unwrap_err();
        toml_error(src, ".hledger-x.toml", &e)
    }

    #[test]
    fn unknown_setting_suggests_the_near_miss() {
        let out = render("formatfile = true\n");
        assert_eq!(
            out,
            ".hledger-x.toml, line 1: unknown setting `formatfile`\n      \
             formatfile = true\n  (did you mean `format_file`?)"
        );
    }

    #[test]
    fn a_wild_typo_gets_the_list_instead_of_a_guess() {
        let out = render("wibble = true\n");
        assert!(out.contains("unknown setting `wibble`"), "{out}");
        assert!(out.contains("valid settings are"), "{out}");
        assert!(!out.contains("did you mean"), "{out}");
    }

    #[test]
    fn wrong_type_says_what_it_wanted_and_what_it_saw() {
        let out = render("sort = \"yes\"\n");
        assert!(
            out.contains("expected true or false, but found the text \"yes\""),
            "{out}"
        );
    }

    #[test]
    fn bad_enum_value_lists_the_alternatives() {
        let out = render("insertion = \"sideways\"\n");
        assert!(out.contains("`sideways` is not a valid value"), "{out}");
        assert!(out.contains("`append`"), "{out}");
    }

    #[test]
    fn a_near_miss_value_is_suggested_singular_for_plural_included() {
        let out = render("insertion = \"Chronological\"\n");
        assert!(out.contains("did you mean `chronological`?"), "{out}");
        // The mistake the plural check names invite: `payee` for `payees`.
        assert_eq!(
            suggest("payee", "`accounts`, `commodities`, `payees`"),
            Some("payees")
        );
        assert_eq!(
            suggest("accounts", "`accounts`, `commodities`"),
            Some("accounts")
        );
        assert_eq!(suggest("wibble", "`accounts`, `commodities`"), None);
    }

    #[test]
    fn no_rustc_diagnostic_art_survives() {
        for src in [
            "formatfile = true\n",
            "sort = \"yes\"\n",
            "sort =\n",
            "insertion = \"sideways\"\n",
            "[oops\n",
        ] {
            let e = toml::from_str::<toml::Table>(src)
                .err()
                .map(|e| toml_error(src, "c.toml", &e));
            let out = e.unwrap_or_default();
            assert!(!out.contains("  |"), "gutter art in: {out}");
            assert!(!out.contains("^^"), "caret art in: {out}");
            assert!(!out.contains("invalid type"), "serde jargon in: {out}");
        }
    }

    #[test]
    fn the_offending_line_is_reported_and_echoed() {
        let out = render("sort = true\nformat_file = true\nwibble = 1\n");
        assert!(out.contains("line 3"), "{out}");
        assert!(out.contains("wibble = 1"), "{out}");
    }
}
