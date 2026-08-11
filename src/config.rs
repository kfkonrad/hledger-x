//! Configuration: `~/.config/hledger-x/config.toml`, overridden by a local
//! `.hledger-x.toml` discovered by walking up from the current directory (the
//! same way hledger discovers its own config).

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::add::write::{Insertion, WriteOptions};

/// The account equity conversion postings default to, as hledger's
/// `--infer-equity` does.
pub const DEFAULT_EQUITY_CONVERSION_ACCOUNT: &str = "equity:conversion";

/// Whether a finished transaction gets equity conversion postings. A
/// two-variant enum rather than a `bool` so the account beside it reads as
/// what it is: only meaningful when conversions are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EquityConversion {
    /// Leave the transaction exactly as entered (the default).
    #[default]
    Off,
    /// Append the postings that cancel the face-value imbalance.
    On,
}

impl EquityConversion {
    /// Whether conversions are on.
    #[must_use]
    pub const fn is_on(self) -> bool {
        matches!(self, Self::On)
    }
}

/// A completion matching strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Matching {
    /// Classic prefix match.
    Prefix,
    /// Middle-of-string match.
    Substring,
    /// Split the query on `:` and prefix-match each account component
    /// (`ex:gro` → `expenses:groceries`).
    Segment,
    /// Subsequence match, ranked by quality.
    Fuzzy,
}

/// The resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// The journal file `add` reads and writes when `-f` is not given. Sits
    /// between the flag and `$LEDGER_FILE` in precedence. A relative path is
    /// resolved against the directory of the config file that set it, and a
    /// leading `~/` against `$HOME`.
    pub ledger_file: Option<PathBuf>,
    /// Reformat the whole file on write.
    pub format_file: bool,
    /// Also sort transactions by date on write.
    pub sort: bool,
    /// Where new transactions are inserted.
    pub insertion: Insertion,
    /// Strict mode: entering an account or commodity that is not declared
    /// (via `account` / `commodity` directives visible at the insertion
    /// point) asks for confirmation first. Off by default — then undeclared
    /// names are accepted, with a passing note when they are new to the
    /// journal.
    pub strict: bool,
    /// Frecency half-life in days.
    pub half_life_days: f64,
    /// Matching for the account field. Substring by default, switching to
    /// segment as soon as the query contains a colon.
    pub account_matching: Matching,
    /// Matching for the description field.
    pub description_matching: Matching,
    /// Default commodity offered as editable pre-filled text (the journal's
    /// `D` directive also supplies one; that takes precedence, being
    /// file-local truth).
    pub default_commodity: Option<String>,
    /// Append equity conversion postings to a finished transaction whose
    /// postings balance at cost but not at face value (a `@`/`@@` conversion).
    /// Off by default.
    pub equity_conversion: EquityConversion,
    /// The account those postings are written to.
    pub equity_conversion_account: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ledger_file: None,
            format_file: true,
            sort: false,
            insertion: Insertion::Append,
            strict: false,
            half_life_days: crate::add::index::DEFAULT_HALF_LIFE_DAYS,
            account_matching: Matching::Substring,
            description_matching: Matching::Substring,
            default_commodity: None,
            equity_conversion: EquityConversion::Off,
            equity_conversion_account: DEFAULT_EQUITY_CONVERSION_ACCOUNT.to_owned(),
        }
    }
}

impl Config {
    /// The write options this config selects.
    #[must_use]
    pub const fn write_options(&self) -> WriteOptions {
        WriteOptions {
            format_file: self.format_file,
            sort: self.sort,
            insertion: self.insertion,
        }
    }
}

/// The raw TOML shape: everything optional, unknown keys rejected so a typo
/// does not silently disable a setting.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    ledger_file: Option<PathBuf>,
    format_file: Option<bool>,
    sort: Option<bool>,
    insertion: Option<RawInsertion>,
    strict: Option<bool>,
    half_life_days: Option<f64>,
    account_matching: Option<Matching>,
    description_matching: Option<Matching>,
    default_commodity: Option<String>,
    equity_conversion: Option<bool>,
    equity_conversion_account: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum RawInsertion {
    Append,
    Chronological,
}

/// A configuration problem worth stopping for.
#[derive(Debug)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Load configuration: the user file, overridden key-by-key by the local
/// file walked up from `cwd`.
///
/// # Errors
///
/// Malformed TOML, unknown keys, or the incoherent
/// `format_file = false` + `sort = true` combination.
pub fn load(cwd: &Path) -> Result<Config, ConfigError> {
    let mut cfg = Config::default();
    if let Some(path) = user_config_path() {
        apply_file(&mut cfg, &path)?;
    }
    if let Some(path) = local_config_path(cwd) {
        apply_file(&mut cfg, &path)?;
    }
    validate(&cfg)?;
    Ok(cfg)
}

/// Load from explicit file contents (for tests).
///
/// # Errors
///
/// Same conditions as [`load`].
pub fn load_str(user: Option<&str>, local: Option<&str>) -> Result<Config, ConfigError> {
    let mut cfg = Config::default();
    if let Some(src) = user {
        apply_str(&mut cfg, src, "config.toml", None)?;
    }
    if let Some(src) = local {
        apply_str(&mut cfg, src, ".hledger-x.toml", None)?;
    }
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &Config) -> Result<(), ConfigError> {
    if !cfg.format_file && cfg.sort {
        return Err(ConfigError(
            "format_file = false and sort = true are incompatible: sorting rewrites the whole file"
                .to_owned(),
        ));
    }
    if cfg.equity_conversion_account.trim().is_empty() {
        return Err(ConfigError(
            "equity_conversion_account must not be empty".to_owned(),
        ));
    }
    if !cfg.half_life_days.is_finite() || cfg.half_life_days <= 0.0 {
        return Err(ConfigError(format!(
            "half_life_days must be a positive number, got {}",
            cfg.half_life_days
        )));
    }
    Ok(())
}

fn apply_file(cfg: &mut Config, path: &Path) -> Result<(), ConfigError> {
    let src = fs::read_to_string(path).map_err(|e| {
        ConfigError(format!(
            "{}: {}",
            crate::errors::display_path(path),
            crate::errors::io_reason(&e)
        ))
    })?;
    apply_str(cfg, &src, &crate::errors::display_path(path), path.parent())
}

/// Resolve a configured path: `~/` against `$HOME`, a relative path against
/// the directory holding the config file that set it (nothing to resolve
/// against — `load_str` in tests — leaves it as written).
fn resolve_path(value: PathBuf, base: Option<&Path>) -> PathBuf {
    if let Ok(rest) = value.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    match base {
        Some(dir) if value.is_relative() => dir.join(value),
        _ => value,
    }
}

fn apply_str(
    cfg: &mut Config,
    src: &str,
    origin: &str,
    base: Option<&Path>,
) -> Result<(), ConfigError> {
    let raw: Raw = toml::from_str(src)
        .map_err(|e| ConfigError(crate::errors::toml_error(src, origin, &e)))?;
    if let Some(v) = raw.ledger_file {
        cfg.ledger_file = Some(resolve_path(v, base));
    }
    if let Some(v) = raw.format_file {
        cfg.format_file = v;
    }
    if let Some(v) = raw.sort {
        cfg.sort = v;
    }
    if let Some(v) = raw.insertion {
        cfg.insertion = match v {
            RawInsertion::Append => Insertion::Append,
            RawInsertion::Chronological => Insertion::Chronological,
        };
    }
    if let Some(v) = raw.strict {
        cfg.strict = v;
    }
    if let Some(v) = raw.half_life_days {
        cfg.half_life_days = v;
    }
    if let Some(v) = raw.account_matching {
        cfg.account_matching = v;
    }
    if let Some(v) = raw.description_matching {
        cfg.description_matching = v;
    }
    if let Some(v) = raw.default_commodity {
        cfg.default_commodity = Some(v);
    }
    if let Some(v) = raw.equity_conversion {
        cfg.equity_conversion = if v {
            EquityConversion::On
        } else {
            EquityConversion::Off
        };
    }
    if let Some(v) = raw.equity_conversion_account {
        cfg.equity_conversion_account = v;
    }
    Ok(())
}

/// `~/.config/hledger-x/config.toml` (respecting `$XDG_CONFIG_HOME`), if it
/// exists.
fn user_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map_or_else(
        || Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")),
        |x| Some(PathBuf::from(x)),
    )?;
    let path = base.join("hledger-x").join("config.toml");
    path.exists().then_some(path)
}

/// The nearest `.hledger-x.toml` walking up from `cwd`.
fn local_config_path(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let candidate = d.join(".hledger-x.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_design() {
        let cfg = load_str(None, None).unwrap();
        assert!(cfg.format_file);
        assert!(!cfg.sort);
        assert_eq!(cfg.insertion, Insertion::Append);
        assert!(!cfg.strict);
        assert!((cfg.half_life_days - 90.0).abs() < f64::EPSILON);
        assert_eq!(cfg.account_matching, Matching::Substring);
        assert_eq!(cfg.description_matching, Matching::Substring);
        assert_eq!(cfg.default_commodity, None);
        assert!(!cfg.equity_conversion.is_on());
        assert_eq!(cfg.equity_conversion_account, "equity:conversion");
        assert_eq!(cfg.ledger_file, None);
    }

    #[test]
    fn ledger_file_parses_and_the_local_file_wins() {
        let cfg = load_str(Some("ledger_file = \"/a/main.journal\"\n"), None).unwrap();
        assert_eq!(cfg.ledger_file, Some(PathBuf::from("/a/main.journal")));

        let cfg = load_str(
            Some("ledger_file = \"/a/main.journal\"\n"),
            Some("ledger_file = \"/b/other.journal\"\n"),
        )
        .unwrap();
        assert_eq!(cfg.ledger_file, Some(PathBuf::from("/b/other.journal")));
    }

    #[test]
    fn a_relative_ledger_file_resolves_against_the_config_directory() {
        assert_eq!(
            resolve_path(PathBuf::from("main.journal"), Some(Path::new("/acc"))),
            PathBuf::from("/acc/main.journal")
        );
        assert_eq!(
            resolve_path(PathBuf::from("/abs/main.journal"), Some(Path::new("/acc"))),
            PathBuf::from("/abs/main.journal")
        );
    }

    #[test]
    fn equity_conversion_keys_parse() {
        let cfg = load_str(
            Some("equity_conversion = true\nequity_conversion_account = \"equity:trading\"\n"),
            None,
        )
        .unwrap();
        assert!(cfg.equity_conversion.is_on());
        assert_eq!(cfg.equity_conversion_account, "equity:trading");
    }

    #[test]
    fn an_empty_equity_conversion_account_is_rejected() {
        assert!(load_str(Some("equity_conversion_account = \"\"\n"), None).is_err());
    }

    #[test]
    fn local_config_overrides_user_config_key_by_key() {
        let user = "sort = true\nhalf_life_days = 30\n";
        let local = "half_life_days = 10\n";
        let cfg = load_str(Some(user), Some(local)).unwrap();
        assert!(cfg.sort); // untouched by local
        assert!((cfg.half_life_days - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn enums_parse_from_lowercase() {
        let cfg = load_str(
            Some("strict = true\ninsertion = \"chronological\"\naccount_matching = \"fuzzy\"\n"),
            None,
        )
        .unwrap();
        assert!(cfg.strict);
        assert_eq!(cfg.insertion, Insertion::Chronological);
        assert_eq!(cfg.account_matching, Matching::Fuzzy);
    }

    #[test]
    fn no_format_plus_sort_is_rejected_at_load() {
        let err = load_str(Some("format_file = false\nsort = true\n"), None).unwrap_err();
        assert!(err.0.contains("incompatible"));
        // Even split across the two files.
        let err = load_str(Some("format_file = false\n"), Some("sort = true\n")).unwrap_err();
        assert!(err.0.contains("incompatible"));
    }

    #[test]
    fn unknown_keys_are_rejected_not_ignored() {
        let err = load_str(Some("formatfile = true\n"), None).unwrap_err();
        assert!(err.0.contains("formatfile"));
    }

    #[test]
    fn nonsense_half_life_is_rejected() {
        assert!(load_str(Some("half_life_days = 0\n"), None).is_err());
        assert!(load_str(Some("half_life_days = -3\n"), None).is_err());
    }
}
