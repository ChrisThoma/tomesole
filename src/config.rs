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
    /// Active entries this version does not understand. Keeping their original
    /// lines lets an older binary edit known settings without erasing options
    /// written by a newer one.
    unknown: Vec<String>,
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
        cfg.unknown = text
            .lines()
            .filter_map(|raw| {
                let active = raw.split('#').next().unwrap_or("").trim();
                let (key, _) = active.split_once('=')?;
                (!is_known_key(key.trim())).then(|| raw.to_string())
            })
            .collect();

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

    /// Render the config back to its file form.
    ///
    /// The output mirrors [`TEMPLATE`]: a set value becomes an active
    /// `key = value` line, an unset one stays a commented example, so the file
    /// keeps its own documentation whether or not a field is in use. This
    /// regenerates the file rather than editing it in place, so a hand-written
    /// comment or a custom ordering is not preserved — the guidance comments are.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# tomesole configuration.\n");
        out.push_str("# Command-line flags override everything here.\n\n");

        line(
            &mut out,
            "# Mirrors to try first, in order. Repeat the key or use a comma-separated list.\n\
             # Leave unset to use the built-in list plus auto-discovery.",
            "mirror",
            (!self.mirrors.is_empty()).then(|| self.mirrors.join(", ")),
            "https://libgen.li",
        );
        line(
            &mut out,
            "# Where downloads are saved.",
            "download_dir",
            self.download_dir
                .as_ref()
                .map(|p| p.display().to_string()),
            "~/Downloads",
        );
        line(
            &mut out,
            "# Default number of results to show.",
            "limit",
            self.limit.map(|n| n.to_string()),
            "25",
        );
        line(
            &mut out,
            "# Collections to search: libgen, fiction, comics, magazines, standards, russian",
            "topics",
            self.topics
                .as_ref()
                .map(|t| t.iter().map(|topic| topic.name()).collect::<Vec<_>>().join(", ")),
            "libgen, fiction",
        );
        line(
            &mut out,
            "# Refuse to save a file larger than this.",
            "max_size",
            self.max_size.map(|n| n.to_string()),
            "4 GB",
        );
        line(
            &mut out,
            "# Verify each download against the MD5 the catalogue advertises.\n\
             # Turning this off removes the only integrity check available. Don't.",
            "verify",
            self.verify.map(|b| b.to_string()),
            "true",
        );
        line(
            &mut out,
            "# What `tomesole open` hands a book to. On macOS this is an application name,\n\
             # passed to `open -a`; elsewhere it is a command run with the file as its\n\
             # argument. Unset means the system default for the file type.",
            "reader",
            self.reader.clone(),
            "Books",
        );
        line(
            &mut out,
            "# Keep a record of what has been downloaded, for `tomesole history`.",
            "history",
            self.history.map(|b| b.to_string()),
            "true",
        );
        line(
            &mut out,
            "# Fetch and draw cover art in the full-screen interface.",
            "covers",
            self.covers.map(|b| b.to_string()),
            "true",
        );
        line(
            &mut out,
            "# Permit cleartext http mirrors. Off unless you have a specific reason.",
            "allow_http",
            self.allow_http.then(|| "true".to_string()),
            "false",
        );
        if !self.unknown.is_empty() {
            out.push_str("\n# Options preserved for compatibility with newer versions.\n");
            for entry in &self.unknown {
                out.push_str(entry);
                out.push('\n');
            }
        }
        out
    }

    /// Write the config to its standard path, creating the directory if needed.
    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    /// Write the config to a specific path, creating its directory. The thin
    /// [`save`](Self::save) wrapper targets the standard location; taking the
    /// path explicitly keeps the writing logic testable without touching it.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            ensure_dir(parent)?;
        }
        std::fs::write(path, self.render())
            .with_context(|| format!("could not write {}", path.display()))
    }
}

fn is_known_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "mirror"
            | "mirrors"
            | "download_dir"
            | "limit"
            | "topics"
            | "allow_http"
            | "verify"
            | "reader"
            | "viewer"
            | "history"
            | "covers"
            | "cover_art"
            | "max_size"
    )
}

/// Emit one config entry: its comment block, then either the active
/// `key = value` when `value` is set or a commented `# key = example` when not.
fn line(out: &mut String, comment: &str, key: &str, value: Option<String>, example: &str) {
    out.push('\n');
    out.push_str(comment);
    out.push('\n');
    match value {
        Some(v) => out.push_str(&format!("{key} = {v}\n")),
        None => out.push_str(&format!("# {key} = {example}\n")),
    }
}

pub(crate) fn parse_bool(value: &str) -> Result<bool> {
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
    xdg_dir("XDG_CONFIG_HOME", ".config").join("tomesole")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.conf")
}

pub fn cache_dir() -> PathBuf {
    xdg_dir("XDG_CACHE_HOME", ".cache").join("tomesole")
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
    xdg_dir("XDG_DATA_HOME", ".local/share").join("tomesole")
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

/// The commented template written by `tomesole config --init`.
pub const TEMPLATE: &str = "\
# tomesole configuration.
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

# What `tomesole open` hands a book to. On macOS this is an application name,
# passed to `open -a`; elsewhere it is a command run with the file as its
# argument. Unset means the system default for the file type.
# reader = Books
# reader = zathura

# Keep a record of what has been downloaded, for `tomesole history`.
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
    fn unknown_keys_survive_rendering_and_saving() {
        let mut cfg =
            Config::parse("future_option = 3 # keep this\nlimit = 10\nfuture_list = a, b\n")
                .unwrap();
        cfg.limit = Some(20);

        let rendered = cfg.render();
        assert!(rendered.contains("future_option = 3 # keep this"));
        assert!(rendered.contains("future_list = a, b"));

        let path = std::env::temp_dir().join(format!(
            "tomesole-config-preserve-{}-{:?}.conf",
            std::process::id(),
            std::thread::current().id()
        ));
        cfg.save_to(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("future_option = 3 # keep this"));
        assert!(saved.contains("limit = 20"));
        let _ = std::fs::remove_file(path);
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
    fn a_populated_config_survives_a_render_round_trip() {
        let original = Config::parse(
            r#"
            mirror = https://libgen.li, https://libgen.gl
            download_dir = /tmp/books
            limit = 50
            topics = libgen, fiction
            max_size = 2 GB
            verify = false
            allow_http = yes
            reader = zathura
            history = no
            covers = off
            "#,
        )
        .unwrap();

        let reparsed = Config::parse(&original.render()).unwrap();
        assert_eq!(reparsed.mirrors, original.mirrors);
        assert_eq!(reparsed.download_dir, original.download_dir);
        assert_eq!(reparsed.limit, original.limit);
        assert_eq!(reparsed.topics, original.topics);
        assert_eq!(reparsed.max_size, original.max_size);
        assert_eq!(reparsed.verify, original.verify);
        assert_eq!(reparsed.allow_http, original.allow_http);
        assert_eq!(reparsed.reader, original.reader);
        assert_eq!(reparsed.history, original.history);
        assert_eq!(reparsed.covers, original.covers);
    }

    #[test]
    fn a_default_config_renders_to_all_defaults() {
        // Every field is unset, so every line is a commented example: rendering
        // and reparsing it yields the same empty defaults.
        let rendered = Config::default().render();
        let reparsed = Config::parse(&rendered).unwrap();
        assert!(reparsed.mirrors.is_empty());
        assert_eq!(reparsed.limit, None);
        assert_eq!(reparsed.download_dir, None);
        assert_eq!(reparsed.topics, None);
        assert_eq!(reparsed.max_size, None);
        assert_eq!(reparsed.verify, None);
        assert!(!reparsed.allow_http);
        assert_eq!(reparsed.reader, None);
    }

    #[test]
    fn tilde_expands_to_home() {
        let expanded = expand_tilde("~/x");
        assert!(expanded.is_absolute() || expanded.starts_with("."));
        assert!(expanded.ends_with("x"));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
