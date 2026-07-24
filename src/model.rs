//! Core data types shared across the search backend, the UI, and the downloader.

/// A single downloadable file record as advertised by a Libgen mirror.
///
/// Every field except `md5` is best-effort: mirrors return ragged HTML and
/// missing columns are common. `md5` is the one value we insist on, because it
/// is both the download key and the integrity check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Book {
    /// Lowercase 32-character hex MD5 of the file contents.
    pub md5: String,
    pub title: String,
    pub authors: Option<String>,
    pub publisher: Option<String>,
    pub year: Option<String>,
    pub language: Option<String>,
    pub pages: Option<String>,
    /// File size in bytes, when the mirror reported something parseable.
    pub size_bytes: Option<u64>,
    /// Lowercase extension without a leading dot, e.g. `epub`.
    pub extension: Option<String>,
    /// Mirror-internal file id.
    pub file_id: Option<String>,
}

impl Book {
    /// Extension normalised to lowercase, defaulting to `bin` when unknown.
    pub fn ext(&self) -> &str {
        match self.extension.as_deref() {
            Some(e) if !e.is_empty() => e,
            _ => "bin",
        }
    }

    pub fn authors_or_unknown(&self) -> &str {
        match self.authors.as_deref() {
            Some(a) if !a.trim().is_empty() => a,
            _ => "Unknown author",
        }
    }

    /// Human-facing size, e.g. `1.4 MB`. Empty string when unknown.
    pub fn size_human(&self) -> String {
        match self.size_bytes {
            Some(b) => human_bytes(b),
            None => String::new(),
        }
    }
}

/// Which Libgen topic (collection) to search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Topic {
    /// Sci-tech / non-fiction, the main Libgen collection.
    Libgen,
    Fiction,
    Comics,
    Magazines,
    Standards,
    /// Russian fiction collection.
    RussianFiction,
}

impl Topic {
    /// The single-letter code the `libgen.li` search form uses for `topics[]`.
    pub fn code(self) -> &'static str {
        match self {
            Topic::Libgen => "l",
            Topic::Fiction => "f",
            Topic::Comics => "c",
            Topic::Magazines => "m",
            Topic::Standards => "s",
            Topic::RussianFiction => "r",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "libgen" | "scitech" | "sci-tech" | "nonfiction" | "l" => Topic::Libgen,
            "fiction" | "f" => Topic::Fiction,
            "comics" | "c" => Topic::Comics,
            "magazines" | "magazine" | "m" => Topic::Magazines,
            "standards" | "s" => Topic::Standards,
            "russian" | "russian-fiction" | "r" => Topic::RussianFiction,
            _ => return None,
        })
    }

    /// The canonical lowercase name, the spelling used in the config file and
    /// in `ALL_NAMES`. The inverse of the primary `parse` spelling.
    pub fn name(self) -> &'static str {
        match self {
            Topic::Libgen => "libgen",
            Topic::Fiction => "fiction",
            Topic::Comics => "comics",
            Topic::Magazines => "magazines",
            Topic::Standards => "standards",
            Topic::RussianFiction => "russian",
        }
    }

    pub const ALL_NAMES: &'static str = "libgen, fiction, comics, magazines, standards, russian";

    /// Every topic, in the order they read in `ALL_NAMES`.
    pub const ALL: [Topic; 6] = [
        Topic::Libgen,
        Topic::Fiction,
        Topic::Comics,
        Topic::Magazines,
        Topic::Standards,
        Topic::RussianFiction,
    ];
}

/// Which metadata column a query term should be matched against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Author,
    Series,
    Publisher,
    Year,
    Isbn,
}

impl Field {
    /// The code the `libgen.li` search form uses for `columns[]`.
    pub fn code(self) -> &'static str {
        match self {
            Field::Title => "t",
            Field::Author => "a",
            Field::Series => "s",
            Field::Publisher => "p",
            Field::Year => "y",
            Field::Isbn => "i",
        }
    }
}

/// A fully-specified search request, independent of any particular mirror.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub terms: String,
    /// Restrict matching to these columns. Empty means "search everything".
    pub fields: Vec<Field>,
    pub topics: Vec<Topic>,
    /// Maximum rows to show the user.
    pub limit: usize,
    /// Client-side filter on file extension (lowercase, no dot).
    pub extension: Option<String>,
    /// Client-side filter on language, case-insensitive substring.
    pub language: Option<String>,
}

impl SearchQuery {
    pub fn new(terms: impl Into<String>) -> Self {
        Self {
            terms: terms.into(),
            fields: Vec::new(),
            topics: vec![Topic::Libgen, Topic::Fiction],
            limit: 25,
            extension: None,
            language: None,
        }
    }

    /// Mirrors only honour a fixed set of page sizes; round up to the nearest.
    ///
    /// When client-side filters are active we deliberately over-fetch, since
    /// filtering happens after the mirror has already truncated the page.
    pub fn page_size(&self) -> usize {
        let wanted = if self.extension.is_some() || self.language.is_some() {
            self.limit.saturating_mul(4).max(50)
        } else {
            self.limit
        };
        match wanted {
            0..=25 => 25,
            26..=50 => 50,
            _ => 100,
        }
    }

    /// Apply the filters that mirrors cannot express in the query string.
    pub fn matches(&self, book: &Book) -> bool {
        if let Some(want) = &self.extension {
            let got = book.extension.as_deref().unwrap_or_default();
            if !got.eq_ignore_ascii_case(want) {
                return false;
            }
        }
        if let Some(want) = &self.language {
            let got = book.language.as_deref().unwrap_or_default();
            if !got.to_lowercase().contains(&want.to_lowercase()) {
                return false;
            }
        }
        true
    }
}

/// Format a byte count for display next to a download.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Parse the size strings Libgen prints, e.g. `19 kB`, `1.4 MB`, `700 B`.
pub fn parse_size(text: &str) -> Option<u64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let digits_end = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
        .unwrap_or(text.len());
    let (number, suffix) = text.split_at(digits_end);
    let number: f64 = number.replace(',', ".").parse().ok()?;
    let multiplier = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let bytes = number * multiplier;
    if bytes.is_finite() && bytes >= 0.0 {
        Some(bytes as u64)
    } else {
        None
    }
}

/// True when `s` is a syntactically valid Libgen MD5 key.
pub fn is_md5(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_libgen_size_strings() {
        assert_eq!(parse_size("19 kB"), Some(19 * 1024));
        assert_eq!(parse_size("700 B"), Some(700));
        assert_eq!(parse_size("2 GB"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size(""), None);
        assert_eq!(parse_size("unknown"), None);
        // Comma decimal separators show up on some locales.
        assert_eq!(parse_size("1,5 MB"), Some(1_572_864));
    }

    #[test]
    fn formats_bytes_readably() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(19 * 1024), "19.0 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MB");
    }

    #[test]
    fn validates_md5_keys() {
        assert!(is_md5("1b9159991f7fb1b3910c0be9ebf7e595"));
        assert!(!is_md5("1b9159991f7fb1b3910c0be9ebf7e59"));
        assert!(!is_md5("zzzz59991f7fb1b3910c0be9ebf7e595"));
        assert!(!is_md5(""));
    }

    #[test]
    fn client_side_filters_apply() {
        let book = Book {
            md5: "1b9159991f7fb1b3910c0be9ebf7e595".into(),
            title: "T".into(),
            language: Some("English".into()),
            extension: Some("epub".into()),
            ..Default::default()
        };
        let mut q = SearchQuery::new("x");
        q.extension = Some("epub".into());
        assert!(q.matches(&book));
        q.extension = Some("pdf".into());
        assert!(!q.matches(&book));
        q.extension = None;
        q.language = Some("eng".into());
        assert!(q.matches(&book));
        q.language = Some("french".into());
        assert!(!q.matches(&book));
    }

    #[test]
    fn over_fetches_when_filtering_client_side() {
        let mut q = SearchQuery::new("x");
        q.limit = 25;
        assert_eq!(q.page_size(), 25);
        q.extension = Some("epub".into());
        assert_eq!(q.page_size(), 100);
    }

    #[test]
    fn topic_names_round_trip() {
        assert_eq!(Topic::parse("fiction"), Some(Topic::Fiction));
        assert_eq!(Topic::parse("SCI-TECH"), Some(Topic::Libgen));
        assert_eq!(Topic::parse("nope"), None);
    }
}
