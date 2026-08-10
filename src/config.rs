//! Configuration: `~/.config/rledger/config.toml`, overridden by a local
//! `.rledger.toml` discovered by walking up from the current directory (the
//! same way hledger discovers its own config).

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::add::write::{Insertion, WriteOptions};

/// What to do when an entered account (or payee) is new to the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NewAccountPolicy {
    /// Prompt, surfacing near-misses ("did you mean …?").
    Confirm,
    /// Warn but accept.
    Warn,
    /// Accept silently.
    Allow,
    /// Refuse the field until it matches something known.
    Error,
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
    /// Reformat the whole file on write.
    pub format_file: bool,
    /// Also sort transactions by date on write.
    pub sort: bool,
    /// Where new transactions are inserted.
    pub insertion: Insertion,
    /// New-account guard. `None` means: decide from the journal — `confirm`
    /// when it declares accounts, `warn` otherwise.
    pub new_account: Option<NewAccountPolicy>,
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            format_file: true,
            sort: false,
            insertion: Insertion::Append,
            new_account: None,
            half_life_days: crate::add::index::DEFAULT_HALF_LIFE_DAYS,
            account_matching: Matching::Substring,
            description_matching: Matching::Substring,
            default_commodity: None,
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
    format_file: Option<bool>,
    sort: Option<bool>,
    insertion: Option<RawInsertion>,
    new_account: Option<NewAccountPolicy>,
    half_life_days: Option<f64>,
    account_matching: Option<Matching>,
    description_matching: Option<Matching>,
    default_commodity: Option<String>,
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

/// Load from explicit file contents (for tests and `--config`).
///
/// # Errors
///
/// Same conditions as [`load`].
pub fn load_str(user: Option<&str>, local: Option<&str>) -> Result<Config, ConfigError> {
    let mut cfg = Config::default();
    if let Some(src) = user {
        apply_str(&mut cfg, src, "config.toml")?;
    }
    if let Some(src) = local {
        apply_str(&mut cfg, src, ".rledger.toml")?;
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
    if !cfg.half_life_days.is_finite() || cfg.half_life_days <= 0.0 {
        return Err(ConfigError(format!(
            "half_life_days must be a positive number, got {}",
            cfg.half_life_days
        )));
    }
    Ok(())
}

fn apply_file(cfg: &mut Config, path: &Path) -> Result<(), ConfigError> {
    let src = fs::read_to_string(path)
        .map_err(|e| ConfigError(format!("{}: {e}", path.display())))?;
    apply_str(cfg, &src, &path.display().to_string())
}

fn apply_str(cfg: &mut Config, src: &str, origin: &str) -> Result<(), ConfigError> {
    let raw: Raw =
        toml::from_str(src).map_err(|e| ConfigError(format!("{origin}: {e}")))?;
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
    if let Some(v) = raw.new_account {
        cfg.new_account = Some(v);
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
    Ok(())
}

/// `~/.config/rledger/config.toml` (respecting `$XDG_CONFIG_HOME`), if it
/// exists.
fn user_config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME").map_or_else(
        || Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")),
        |x| Some(PathBuf::from(x)),
    )?;
    let path = base.join("rledger").join("config.toml");
    path.exists().then_some(path)
}

/// The nearest `.rledger.toml` walking up from `cwd`.
fn local_config_path(cwd: &Path) -> Option<PathBuf> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let candidate = d.join(".rledger.toml");
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
        assert_eq!(cfg.new_account, None);
        assert!((cfg.half_life_days - 90.0).abs() < f64::EPSILON);
        assert_eq!(cfg.account_matching, Matching::Substring);
        assert_eq!(cfg.description_matching, Matching::Substring);
        assert_eq!(cfg.default_commodity, None);
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
            Some("new_account = \"error\"\ninsertion = \"chronological\"\naccount_matching = \"fuzzy\"\n"),
            None,
        )
        .unwrap();
        assert_eq!(cfg.new_account, Some(NewAccountPolicy::Error));
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
