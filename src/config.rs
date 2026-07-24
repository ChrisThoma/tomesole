//! Config file and standard directory locations.
//!
//! The config format is intentionally trivial — `key = value` lines with `#`
//! comments — because the whole configurable surface is a handful of scalars
//! and a mirror list. A TOML parser would be a large dependency for that.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{Context, Result};
use crate::model::Topic;

/// Everything the user can set in the config file. All fields are optional;
/// command-line flags win over any of them.
#[derive(Debug, Default, Clone)]
pub struct Config {
    /// Mirrors to try first, in order, before the built-in list.
    pub mirrors: Vec<String>,
    pub download_dir: Option<PathBuf>,
    pub limit: Option<usize>,
    pub topics: Option<Vec<Topic>>,
    pub allow_http: bool,
    pub max_size: Option<u64>,
    /// Set to false to skip MD5 verification. Strongly discouraged.
    pub verify: Option<bool>,
    /// What to hand a downloaded book to. On macOS this names an application
    /// for `open -a`; elsewhere it is a command run with the file as argument.
    pub reader: Option<String>,
    /// Set to false to stop recording what was downloaded.
    pub history: Option<bool>,
    /// Set to false to never fetch or draw cover art.
    pub covers: Option<bool>,
}

impl Config {
    /// Load the config file, returning defaults when it does not exist.
    pub fn load() -> Result<Self> {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                Self::parse(&text).with_context(|| format!("invalid config at {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut cfg = Config::default();
        let entries = parse_pairs(text)?;

        for (key, values) in &entries {
            let last = values.last().map(String::as_str).unwrap_or_default();
            match key.as_str() {
                "mirror" | "mirrors" => {
                    for v in values {
                        cfg.mirrors.extend(
                            v.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(String::from),
                        );
                    }
                }
                "download_dir" => cfg.download_dir = Some(expand_tilde(last)),
                "limit" => {
                    cfg.limit = Some(
                        last.parse()
                            .with_context(|| format!("`limit` must be a number, got `{last}`"))?,
                    )
                }
                "topics" => {
                    let mut topics = Vec::new();
                    for name in last.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        topics.push(Topic::parse(name).with_context(|| {
                            format!(
                                "unknown topic `{name}` (expected one of: {})",
                                Topic::ALL_NAMES
                            )
                        })?);
                    }
                    cfg.topics = Some(topics);
                }
                "allow_http" => cfg.allow_http = parse_bool(last)?,
                "verify" => cfg.verify = Some(parse_bool(last)?),
                "reader" | "viewer" => {
                    if !last.trim().is_empty() {
                        cfg.reader = Some(last.trim().to_string());
                    }
                }
                "history" => cfg.history = Some(parse_bool(last)?),
                "covers" | "cover_art" => cfg.covers = Some(parse_bool(last)?),
                "max_size" => {
                    cfg.max_size = Some(
                        crate::model::parse_size(last)
                            .with_context(|| format!("`max_size` is not a size, got `{last}`"))?,
                    )
                }
                other => {
                    // Unknown keys are ignored rather than fatal so a config
                    // written for a newer version still works.
                    let _ = other;
                }
            }
        }
        Ok(cfg)
    }
}

fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(crate::err!("expected true or false, got `{other}`")),
    }
}

/// Parse `key = value` lines, preserving repeats so `mirror` can be listed
/// more than once.
fn parse_pairs(text: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(crate::err!(
                "line {}: expected `key = value`, got `{}`",
                lineno + 1,
                raw.trim()
            ));
        };
        let value = value.trim().trim_matches('"').trim_matches('\'');
        out.entry(key.trim().to_ascii_lowercase())
            .or_default()
            .push(value.to_string());
    }
    Ok(out)
}

/// Replace a leading `~` with the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    let path = path.trim();
    if path == "~" {
        return home_dir();
    }
    match path.strip_prefix("~/") {
        Some(rest) => home_dir().join(rest),
        None => PathBuf::from(path),
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home_dir().join(fallback),
    }
}

pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("clibgen")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.conf")
}

pub fn cache_dir() -> PathBuf {
    xdg_dir("XDG_CACHE_HOME", ".cache").join("clibgen")
}

pub fn mirror_cache_path() -> PathBuf {
    cache_dir().join("mirrors.tsv")
}

/// Downloaded cover thumbnails. Cache, not data: losing it costs a refetch.
pub fn cover_cache_dir() -> PathBuf {
    cache_dir().join("covers")
}

/// Where state the user would miss lives, as opposed to state we can rebuild.
pub fn data_dir() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share").join("clibgen")
}

pub fn history_path() -> PathBuf {
    data_dir().join("history.tsv")
}

/// The default place downloads land: `~/Downloads` when it exists, else cwd.
pub fn default_download_dir() -> PathBuf {
    let downloads = home_dir().join("Downloads");
    if downloads.is_dir() {
        downloads
    } else {
        PathBuf::from(".")
    }
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("could not create directory {}", path.display()))
}

/// Create (or truncate) a file readable only by the current user.
///
/// Used for partial downloads and for the history file: both record what
/// someone has been reading, which is nobody else's business.
pub fn create_private_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))
}

/// The commented template written by `clibgen config --init`.
pub const TEMPLATE: &str = "\
# clibgen configuration.
# Command-line flags override everything here.

# Mirrors to try first, in order. Repeat the key or use a comma-separated list.
# Leave unset to use the built-in list plus auto-discovery.
# mirror = https://libgen.li

# Where downloads are saved.
# download_dir = ~/Downloads

# Default number of results to show.
# limit = 25

# Collections to search: libgen, fiction, comics, magazines, standards, russian
# topics = libgen, fiction

# Refuse to save a file larger than this.
# max_size = 4 GB

# Verify each download against the MD5 the catalogue advertises.
# Turning this off removes the only integrity check available. Don't.
# verify = true

# What `clibgen open` hands a book to. On macOS this is an application name,
# passed to `open -a`; elsewhere it is a command run with the file as its
# argument. Unset means the system default for the file type.
# reader = Books
# reader = zathura

# Keep a record of what has been downloaded, for `clibgen history`.
# history = true

# Fetch and draw cover art in the full-screen interface.
# covers = true

# Permit cleartext http mirrors. Off unless you have a specific reason.
# allow_http = false
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let cfg = Config::parse(
            r#"
            # a comment
            mirror = https://libgen.li
            mirror = https://libgen.gl
            download_dir = ~/Books
            limit = 50
            topics = libgen, fiction
            max_size = 2 GB
            verify = false
            allow_http = yes
            reader = Books
            history = no
            covers = off
            "#,
        )
        .unwrap();

        assert_eq!(cfg.mirrors, ["https://libgen.li", "https://libgen.gl"]);
        assert_eq!(cfg.limit, Some(50));
        assert_eq!(
            cfg.topics.as_deref(),
            Some(&[Topic::Libgen, Topic::Fiction][..])
        );
        assert_eq!(cfg.max_size, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(cfg.verify, Some(false));
        assert!(cfg.allow_http);
        assert_eq!(cfg.reader.as_deref(), Some("Books"));
        assert_eq!(cfg.history, Some(false));
        assert_eq!(cfg.covers, Some(false));
        assert!(cfg.download_dir.unwrap().ends_with("Books"));
    }

    #[test]
    fn comma_separated_mirrors_split() {
        let cfg = Config::parse("mirrors = https://a.example, https://b.example").unwrap();
        assert_eq!(cfg.mirrors, ["https://a.example", "https://b.example"]);
    }

    #[test]
    fn empty_config_is_defaults() {
        let cfg = Config::parse("\n# only comments\n\n").unwrap();
        assert!(cfg.mirrors.is_empty());
        assert_eq!(cfg.limit, None);
    }

    #[test]
    fn unknown_keys_are_ignored_not_fatal() {
        let cfg = Config::parse("future_option = 3\nlimit = 10").unwrap();
        assert_eq!(cfg.limit, Some(10));
    }

    #[test]
    fn malformed_lines_are_reported_with_line_numbers() {
        let err = Config::parse("limit = 10\nthis is not valid\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 2"), "got: {err}");
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Config::parse("limit = many").is_err());
        assert!(Config::parse("topics = nonsense").is_err());
        assert!(Config::parse("verify = maybe").is_err());
    }

    #[test]
    fn quotes_are_stripped() {
        let cfg = Config::parse("download_dir = \"/tmp/books\"").unwrap();
        assert_eq!(cfg.download_dir, Some(PathBuf::from("/tmp/books")));
    }

    #[test]
    fn tilde_expands_to_home() {
        let expanded = expand_tilde("~/x");
        assert!(expanded.is_absolute() || expanded.starts_with("."));
        assert!(expanded.ends_with("x"));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
