//! A record of what has been downloaded, and where it landed.
//!
//! The file is a tab-separated table under the user's data directory, written
//! with owner-only permissions: a list of what somebody has been reading is
//! exactly the sort of thing that should not be world-readable.
//!
//! Two decisions worth stating. Entries are keyed by *path*, not by MD5, so
//! re-downloading a file updates its entry rather than adding a second one —
//! the list reads as "what is in my library", not as an audit log. And parsing
//! is deliberately forgiving: a line we cannot read is skipped, never fatal,
//! because a corrupt history is not a reason to refuse to download a book.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::download::Outcome;
use crate::error::{Context, Result};
use crate::model::Book;

/// How many downloads to remember. Old entries fall off the top.
const MAX_ENTRIES: usize = 1000;

/// The number of fields written per line. Readers accept fewer.
const FIELDS: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    /// Unix seconds. Zero when the clock was unreadable.
    pub at: u64,
    pub md5: String,
    pub path: PathBuf,
    pub size: u64,
    /// Whether the download matched the catalogue's MD5.
    pub verified: bool,
    pub title: String,
    pub authors: Option<String>,
    pub extension: Option<String>,
}

impl Entry {
    /// Build an entry from a download that has just landed.
    pub fn of(book: &Book, outcome: &Outcome) -> Self {
        Entry {
            at: now(),
            md5: book.md5.clone(),
            path: outcome.path.clone(),
            // A skipped download transferred nothing, so the file on disk is
            // the authority on its own size; the catalogue's figure is the last
            // resort.
            size: match outcome.bytes {
                0 => std::fs::metadata(&outcome.path)
                    .map(|m| m.len())
                    .unwrap_or_else(|_| book.size_bytes.unwrap_or(0)),
                bytes => bytes,
            },
            verified: outcome.verified,
            title: book.title.clone(),
            authors: book.authors.clone(),
            extension: book.extension.clone().or_else(|| {
                outcome
                    .path
                    .extension()
                    .map(|e| e.to_string_lossy().to_ascii_lowercase())
            }),
        }
    }

    /// Whether the file is still where we left it.
    pub fn present(&self) -> bool {
        self.path.is_file()
    }

    pub fn filename(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| self.path.display().to_string())
    }

    pub fn ext(&self) -> &str {
        match self.extension.as_deref() {
            Some(e) if !e.is_empty() => e,
            _ => "",
        }
    }

    /// The first of several authors, which is all a narrow column can show.
    pub fn first_author(&self) -> String {
        let all = self.authors.as_deref().unwrap_or("");
        all.split(';').next().unwrap_or(all).trim().to_string()
    }

    fn to_line(&self) -> String {
        let fields = [
            self.at.to_string(),
            self.md5.clone(),
            self.path.to_string_lossy().to_string(),
            self.size.to_string(),
            if self.verified { "1" } else { "0" }.to_string(),
            self.title.clone(),
            self.authors.clone().unwrap_or_default(),
            self.extension.clone().unwrap_or_default(),
        ];
        debug_assert_eq!(fields.len(), FIELDS);
        fields
            .iter()
            .map(|f| escape(f))
            .collect::<Vec<_>>()
            .join("\t")
    }

    fn from_line(line: &str) -> Option<Self> {
        let fields: Vec<String> = line.split('\t').map(unescape).collect();
        // Anything past the eighth field belongs to a newer version than this
        // one; ignore it rather than rejecting the line.
        let get = |i: usize| fields.get(i).cloned().unwrap_or_default();

        let path = get(2);
        if path.is_empty() {
            return None;
        }
        let some = |s: String| if s.is_empty() { None } else { Some(s) };

        Some(Entry {
            at: get(0).parse().unwrap_or(0),
            md5: get(1),
            path: PathBuf::from(path),
            size: get(3).parse().unwrap_or(0),
            verified: get(4) == "1",
            title: get(5),
            authors: some(get(6)),
            extension: some(get(7)),
        })
    }
}

/// Seconds since the epoch, or zero if the clock is unusable.
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Note a completed download. Never fails the caller: losing a history line is
/// not worth turning a good download into an error.
pub fn record(enabled: bool, book: &Book, outcome: &Outcome) {
    if enabled {
        record_to(&crate::config::history_path(), book, outcome);
    }
}

/// The recording decision, against a nominated file so it can be tested.
fn record_to(path: &Path, book: &Book, outcome: &Outcome) {
    // A skip means the file was already on disk. Usually we put it there and
    // the entry that says so is better than anything we could write now — it
    // knows the real timestamp and whether the MD5 checked out. But a file
    // downloaded before any of this existed has no entry at all, and asking for
    // it again is a reasonable way to say "this one is mine": record it, so the
    // library can hold books that predate the library.
    let _ = with_lock(path, || {
        if outcome.skipped && load_from(path).iter().any(|e| e.path == outcome.path) {
            return Ok(());
        }
        append_unlocked(path, &Entry::of(book, outcome))
    });
}

/// Every remembered download, newest first.
pub fn load() -> Vec<Entry> {
    load_from(&crate::config::history_path())
}

pub fn load_from(path: &Path) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(Entry::from_line)
        .collect();
    // The file is chronological; the list reads newest first.
    entries.reverse();
    entries
}

/// Add an entry, replacing any earlier one for the same file.
#[cfg(test)]
pub fn append_to(path: &Path, entry: &Entry) -> Result<()> {
    with_lock(path, || append_unlocked(path, entry))
}

fn append_unlocked(path: &Path, entry: &Entry) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::config::ensure_dir(parent)?;
    }

    // Oldest first for writing, which is the order the file is stored in.
    let mut entries: Vec<Entry> = load_from(path).into_iter().rev().collect();
    entries.retain(|e| e.path != entry.path);
    entries.push(entry.clone());
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    write_all(path, &entries)
}

/// Forget a single entry.
pub fn remove_from(path: &Path, target: &Path) -> Result<bool> {
    with_lock(path, || {
        let mut entries: Vec<Entry> = load_from(path).into_iter().rev().collect();
        let before = entries.len();
        entries.retain(|e| e.path != target);
        if entries.len() == before {
            return Ok(false);
        }
        write_all(path, &entries)?;
        Ok(true)
    })
}

pub fn remove(target: &Path) -> Result<bool> {
    remove_from(&crate::config::history_path(), target)
}

/// Forget everything.
pub fn clear() -> Result<()> {
    let path = crate::config::history_path();
    with_lock(&path, || match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("could not remove {}", path.display())),
    })
}

/// Hold a sibling advisory lock across every read/merge/write mutation.
fn with_lock<T>(path: &Path, op: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Some(parent) = path.parent() {
        crate::config::ensure_dir(parent)?;
    }
    let lock_path = path.with_extension("tsv.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("could not open history lock {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("could not lock {}", lock_path.display()))?;
    op()
}

/// Replace the file, oldest entry first.
///
/// Written to a sibling and renamed so an interrupted write cannot leave a
/// half-truncated history behind.
fn write_all(path: &Path, entries: &[Entry]) -> Result<()> {
    use std::io::Write;

    let temp = path.with_extension("tsv.tmp");
    let mut file = crate::config::create_private_file(&temp)?;
    for entry in entries {
        writeln!(file, "{}", entry.to_line())
            .with_context(|| format!("could not write {}", temp.display()))?;
    }
    file.flush()
        .with_context(|| format!("could not flush {}", temp.display()))?;
    drop(file);

    std::fs::rename(&temp, path).with_context(|| {
        format!(
            "could not move {} into place at {}",
            temp.display(),
            path.display()
        )
    })
}

/// Find the entries a selector refers to.
///
/// A number is a position in the listing, counting from one and from the most
/// recent. An MD5 matches the record it was downloaded from. Anything else is
/// matched, case-insensitively, against the title, author and filename — which
/// can legitimately hit several entries, so the caller decides what to do with
/// more than one.
pub fn select<'a>(entries: &'a [Entry], selector: &str) -> Result<Vec<&'a Entry>> {
    let selector = selector.trim();
    if selector.is_empty() {
        return match entries.first() {
            Some(entry) => Ok(vec![entry]),
            None => Err(crate::err!("nothing has been downloaded yet")),
        };
    }

    if let Ok(index) = selector.parse::<usize>() {
        if index == 0 || index > entries.len() {
            crate::bail!(
                "there is no entry {index} — the history holds {}",
                match entries.len() {
                    0 => "nothing".to_string(),
                    1 => "one entry".to_string(),
                    n => format!("{n} entries"),
                }
            );
        }
        return Ok(vec![&entries[index - 1]]);
    }

    if crate::model::is_md5(selector) {
        let matches: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.md5.eq_ignore_ascii_case(selector))
            .collect();
        if matches.is_empty() {
            crate::bail!("no download in the history has the MD5 {selector}");
        }
        return Ok(matches);
    }

    let needle = selector.to_lowercase();
    let matches: Vec<&Entry> = entries
        .iter()
        .filter(|e| {
            e.title.to_lowercase().contains(&needle)
                || e.filename().to_lowercase().contains(&needle)
                || e.authors
                    .as_deref()
                    .is_some_and(|a| a.to_lowercase().contains(&needle))
        })
        .collect();
    if matches.is_empty() {
        crate::bail!("nothing in the history matches “{selector}”");
    }
    Ok(matches)
}

// --- dates ---------------------------------------------------------------
//
// Formatting a timestamp without a date library means doing the calendar
// arithmetic here. It is UTC: the standard library exposes no timezone, and
// shelling out to `date` for a display string is not a trade worth making.
// Recent downloads are shown relative, which is what "when did I get this"
// usually means anyway, and sidesteps the offset entirely.

/// A human answer to "when": relative for the last week, else a date.
pub fn when(at: u64, now: u64) -> String {
    if at == 0 {
        return String::new();
    }
    let elapsed = now.saturating_sub(at);
    match elapsed {
        0..=90 => "just now".to_string(),
        91..=5399 => format!("{} min ago", elapsed / 60),
        5400..=86_399 => format!("{}h ago", elapsed / 3600),
        86_400..=604_799 => match elapsed / 86_400 {
            1 => "yesterday".to_string(),
            days => format!("{days}d ago"),
        },
        _ => date(at),
    }
}

/// `YYYY-MM-DD`, in UTC.
pub fn date(at: u64) -> String {
    let (y, m, d) = civil_from_days((at / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MM`, in UTC.
pub fn timestamp(at: u64) -> String {
    let seconds = at % 86_400;
    format!(
        "{} {:02}:{:02}",
        date(at),
        seconds / 3600,
        (seconds % 3600) / 60
    )
}

/// Days since 1970-01-01 to a civil date, after Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// --- escaping ------------------------------------------------------------

/// Make a field safe to store between tabs.
fn escape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    for c in field.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            // An escape we do not recognise keeps both characters, so nothing
            // is silently eaten.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tomesole-history-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(name: &str) -> Entry {
        Entry {
            at: 1_700_000_000,
            md5: "1b9159991f7fb1b3910c0be9ebf7e595".into(),
            path: PathBuf::from(format!("/books/{name}.epub")),
            size: 1024,
            verified: true,
            title: name.to_string(),
            authors: Some("Frank Herbert".into()),
            extension: Some("epub".into()),
        }
    }

    #[test]
    fn an_entry_survives_a_round_trip() {
        let original = entry("Dune");
        let parsed = Entry::from_line(&original.to_line()).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn tabs_and_newlines_in_metadata_cannot_break_the_format() {
        let mut hostile = entry("Dune");
        hostile.title = "col\tumn\nrow\\end".into();
        hostile.authors = Some("a\tb".into());
        let line = hostile.to_line();
        assert_eq!(line.matches('\t').count(), FIELDS - 1, "field count moved");
        assert!(!line.contains('\n'));
        assert_eq!(Entry::from_line(&line).unwrap(), hostile);
    }

    #[test]
    fn a_corrupt_line_is_skipped_not_fatal() {
        let dir = temp_dir("corrupt");
        let path = dir.join("history.tsv");
        std::fs::write(
            &path,
            format!("{}\n\nnot a real line\n\t\t\t\n", entry("Dune").to_line()),
        )
        .unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 1, "only the good line survives: {loaded:?}");
        assert_eq!(loaded[0].title, "Dune");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn newest_comes_first() {
        let dir = temp_dir("order");
        let path = dir.join("history.tsv");
        for name in ["First", "Second", "Third"] {
            append_to(&path, &entry(name)).unwrap();
        }
        let loaded = load_from(&path);
        assert_eq!(
            loaded.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(),
            ["Third", "Second", "First"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn re_downloading_updates_rather_than_duplicates() {
        let dir = temp_dir("dedupe");
        let path = dir.join("history.tsv");
        append_to(&path, &entry("Dune")).unwrap();
        let mut again = entry("Dune");
        again.at += 10;
        again.size = 4096;
        append_to(&path, &again).unwrap();

        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].size, 4096);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_updates_are_merged_without_loss() {
        let dir = temp_dir("concurrent");
        let path = dir.join("history.tsv");
        let start = std::sync::Arc::new(std::sync::Barrier::new(17));
        let mut workers = Vec::new();
        for i in 0..16 {
            let path = path.clone();
            let start = start.clone();
            workers.push(std::thread::spawn(move || {
                let mut item = entry(&format!("Book {i}"));
                item.path = PathBuf::from(format!("/books/{i}.epub"));
                start.wait();
                append_to(&path, &item).unwrap();
            }));
        }
        start.wait();
        for worker in workers {
            worker.join().unwrap();
        }

        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 16);
        let paths: std::collections::HashSet<_> = loaded.iter().map(|e| &e.path).collect();
        assert_eq!(paths.len(), 16);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_is_capped() {
        let dir = temp_dir("cap");
        let path = dir.join("history.tsv");
        let entries: Vec<Entry> = (0..MAX_ENTRIES + 25)
            .map(|i| {
                let mut e = entry(&format!("Book {i}"));
                e.path = PathBuf::from(format!("/books/{i}.epub"));
                e
            })
            .collect();
        write_all(&path, &entries).unwrap();
        append_to(&path, &entry("One More")).unwrap();

        let loaded = load_from(&path);
        assert_eq!(loaded.len(), MAX_ENTRIES);
        assert_eq!(loaded[0].title, "One More", "the newest must survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_an_entry_leaves_the_rest() {
        let dir = temp_dir("remove");
        let path = dir.join("history.tsv");
        append_to(&path, &entry("Dune")).unwrap();
        append_to(&path, &entry("Emma")).unwrap();

        assert!(remove_from(&path, Path::new("/books/Dune.epub")).unwrap());
        assert!(!remove_from(&path, Path::new("/books/Nothing.epub")).unwrap());
        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "Emma");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn selecting_by_position_counts_from_the_newest() {
        let entries = vec![entry("Third"), entry("Second"), entry("First")];
        assert_eq!(select(&entries, "1").unwrap()[0].title, "Third");
        assert_eq!(select(&entries, "3").unwrap()[0].title, "First");
        // An empty selector means "the last thing I downloaded".
        assert_eq!(select(&entries, "").unwrap()[0].title, "Third");
        assert!(select(&entries, "0").is_err());
        assert!(select(&entries, "4").is_err());
    }

    #[test]
    fn selecting_by_text_matches_title_author_and_filename() {
        let entries = vec![entry("Dune"), entry("Emma")];
        assert_eq!(select(&entries, "dune").unwrap().len(), 1);
        assert_eq!(select(&entries, "DUNE.epub").unwrap()[0].title, "Dune");
        // The shared author matches both, and the caller disambiguates.
        assert_eq!(select(&entries, "herbert").unwrap().len(), 2);
        assert!(select(&entries, "nothing here").is_err());
    }

    #[test]
    fn selecting_by_md5_works_and_reports_a_miss() {
        let entries = vec![entry("Dune")];
        assert_eq!(
            select(&entries, "1B9159991F7FB1B3910C0BE9EBF7E595")
                .unwrap()
                .len(),
            1
        );
        assert!(select(&entries, "0".repeat(32).as_str()).is_err());
    }

    #[test]
    fn a_skip_of_a_known_file_changes_nothing() {
        // Re-downloading a file already in the history is the common skip: the
        // existing entry is better than a fresh one, so the file is left alone.
        let dir = temp_dir("skip-known");
        let path = dir.join("history.tsv");
        let first = entry("Dune");
        append_to(&path, &first).unwrap();

        let outcome = Outcome {
            path: first.path.clone(),
            bytes: 0,
            verified: false,
            skipped: true,
        };
        record_to(&path, &Book::default(), &outcome);

        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].at, first.at, "the original entry is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_skip_of_an_unknown_file_is_recorded() {
        // A book downloaded before the history existed has no entry; asking for
        // it again is how it joins the library, even though nothing transfers.
        let dir = temp_dir("skip-unknown");
        let path = dir.join("history.tsv");
        std::fs::write(&path, b"").unwrap();

        let book = Book {
            md5: "35f70305fc4592eaafa2bc803676a51f".into(),
            title: "House of Leaves".into(),
            extension: Some("mobi".into()),
            size_bytes: Some(2_515_644),
            ..Default::default()
        };
        let outcome = Outcome {
            path: PathBuf::from("/books/House of Leaves.mobi"),
            bytes: 0,
            verified: false,
            skipped: true,
        };
        record_to(&path, &book, &outcome);

        let loaded = load_from(&path);
        assert_eq!(loaded.len(), 1, "a pre-existing book should enter the library");
        assert_eq!(loaded[0].title, "House of Leaves");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dates_are_converted_correctly() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_700_000_000), "2023-11-14");
        assert_eq!(timestamp(1_700_000_000), "2023-11-14 22:13");
        // A leap day, which is where naive arithmetic goes wrong.
        assert_eq!(date(1_709_164_800), "2024-02-29");
    }

    #[test]
    fn relative_times_read_naturally() {
        let now = 1_700_000_000;
        assert_eq!(when(now, now), "just now");
        assert_eq!(when(now - 600, now), "10 min ago");
        assert_eq!(when(now - 7200, now), "2h ago");
        assert_eq!(when(now - 86_400, now), "yesterday");
        assert_eq!(when(now - 3 * 86_400, now), "3d ago");
        // Beyond a week, an actual date is more use than "23d ago".
        assert_eq!(when(now - 30 * 86_400, now), date(now - 30 * 86_400));
        assert_eq!(when(0, now), "");
    }
}
