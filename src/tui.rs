//! The full-screen interface.
//!
//! Everything slow — resolving a mirror, searching, downloading — happens on a
//! worker thread and reports back over a channel. The UI thread only ever waits
//! on that channel, so the interface stays responsive while a 40 MB file is
//! coming down, and a mirror that takes ten seconds to answer never freezes the
//! display.
//!
//! Downloads run one at a time. That is deliberate: firing off eight parallel
//! requests at a volunteer-run mirror is a good way to get rate-limited, and it
//! makes the progress display much easier to read.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ureq::http::Uri;

use crate::cover::Cover;
use crate::error::Result;
use crate::graphics::{self, Protocol};
use crate::mirror::{Pool, ResolveOptions};
use crate::model::{Book, SearchQuery, human_bytes};
use crate::net::Http;
use crate::{Settings, cover, download, history, launch, libgen, mirror, net, query};

/// The visual language, in one place so the whole interface stays coherent.
///
/// The interface paints its own canvas — a deep blue-grey — rather than
/// inheriting the terminal background, so contrast is something it owns.
/// Colour then carries meaning: teal is interactive (the cursor, the keys, the
/// active tab), amber is emphasis (a narrowed count, a marked row), every file
/// format keeps one hue of its own so a column of results reads at a glance,
/// and languages share one calm blue because they are the other thing people
/// narrow by. Text sits on three levels, primary through faint, so hierarchy
/// comes from weight rather than a rainbow. Colours are true-colour on
/// purpose: the interface commits to a look rather than inheriting whatever
/// sixteen colours the terminal happened to be set to.
mod theme {
    use ratatui::style::{Color, Modifier, Style};

    /// The canvas painted under every frame.
    pub const BG: Color = Color::Rgb(16, 20, 27);
    /// Every other table row, one step off the canvas: enough for the eye to
    /// track a row across a wide screen, not enough to read as stripes.
    pub const BG_ALT: Color = Color::Rgb(22, 27, 36);
    pub const ACCENT: Color = Color::Rgb(88, 216, 216); // teal — interactive
    pub const ACCENT_DEEP: Color = Color::Rgb(56, 148, 152);
    pub const AMBER: Color = Color::Rgb(242, 190, 106); // emphasis
    pub const TEXT: Color = Color::Rgb(238, 241, 246);
    pub const MUTED: Color = Color::Rgb(173, 181, 196);
    pub const FAINT: Color = Color::Rgb(112, 121, 137);
    pub const SUCCESS: Color = Color::Rgb(140, 220, 150);
    pub const DANGER: Color = Color::Rgb(244, 120, 130);
    /// Languages get one colour of their own — a calm blue — because they are
    /// one of the two facets the interface filters by.
    pub const LANG: Color = Color::Rgb(142, 180, 252);
    /// The wash behind the highlighted row: a teal tint, bright enough to be
    /// plainly the cursor, dim enough to sit under white text.
    pub const SELECT_BG: Color = Color::Rgb(31, 54, 61);

    /// The gutter that marks the selected row. A solid bar reads as "here" even
    /// when the list holds a single item, which a mere background tint does not.
    pub const CURSOR: &str = "▌ ";

    pub fn text() -> Style {
        Style::new().fg(TEXT)
    }
    pub fn muted() -> Style {
        Style::new().fg(MUTED)
    }
    pub fn faint() -> Style {
        Style::new().fg(FAINT)
    }
    pub fn accent() -> Style {
        Style::new().fg(ACCENT)
    }

    /// Each format keeps one hue everywhere it appears, so "the green ones are
    /// epubs" becomes something the eye learns in seconds.
    pub fn format_color(ext: &str) -> Color {
        match ext.to_ascii_lowercase().as_str() {
            "epub" => Color::Rgb(129, 201, 141),
            "pdf" => Color::Rgb(240, 138, 126),
            "mobi" | "azw" | "azw3" => Color::Rgb(235, 180, 100),
            "djvu" => Color::Rgb(198, 160, 246),
            "cbz" | "cbr" => Color::Rgb(240, 150, 200),
            _ => Color::Rgb(148, 156, 170),
        }
    }

    /// A format as a small solid chip: dark text on the format's hue.
    pub fn format_chip(ext: &str) -> Style {
        Style::new()
            .fg(BG)
            .bg(format_color(ext))
            .add_modifier(Modifier::BOLD)
    }

    /// The wash behind the selected row — and only the wash. The highlight is
    /// applied over every cell, so anything set here would repaint the row's
    /// own colours; the white title, the blue language and the format's hue
    /// are the information, and selection must not flatten them into teal.
    pub fn selected_row() -> Style {
        Style::new().bg(SELECT_BG)
    }

    /// A column header: quiet, so the data below it does the talking.
    pub fn header() -> Style {
        Style::new().fg(ACCENT_DEEP).add_modifier(Modifier::BOLD)
    }
}

/// Messages the UI thread waits on.
enum Ev {
    Key(KeyEvent),
    Redraw,
    /// Mirror selection finished; carries the ranked pool or a failure.
    Mirrors(std::result::Result<Vec<Uri>, String>),
    /// A search finished; carries the results and the mirror that served them.
    Results(std::result::Result<(Vec<Book>, String), String>),
    Progress {
        md5: String,
        done: u64,
        total: Option<u64>,
    },
    Finished {
        md5: String,
        outcome: std::result::Result<String, String>,
    },
    /// A cover lookup finished. `None` means the book has no cover.
    Cover {
        md5: String,
        result: std::result::Result<Option<Cover>, String>,
    },
}

/// Per-download state, keyed by MD5.
#[derive(Clone)]
enum Job {
    Queued,
    Running { done: u64, total: Option<u64> },
    Saved(String),
    Failed(String),
}

/// What is known about one book's cover.
#[derive(Clone)]
enum Slot {
    Looking,
    /// Looked, and there is nothing to show.
    Nothing,
    Ready(Box<Art>),
}

/// A cover, in whichever forms this terminal can use.
#[derive(Clone)]
struct Art {
    /// The file as served, for iTerm2, which takes it directly.
    encoded: Vec<u8>,
    /// Decoded pixels, for kitty and for the half-block fallback.
    pixels: Option<graphics::Image>,
}

/// The two halves of the program: finding books, and reading the ones you have.
///
/// They are peers rather than a view and a detour off it, which is why `q`
/// quits from either and the tab bar is always visible.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Tab {
    Search,
    Library,
}

impl Tab {
    fn other(self) -> Self {
        match self {
            Tab::Search => Tab::Library,
            Tab::Library => Tab::Search,
        }
    }
}

/// What the keyboard is doing, independently of which tab is showing.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Mode {
    Editing,
    Browsing,
    Help,
}

/// The image id used for the cover. Reusing one id means each new cover
/// replaces the last rather than piling up in the terminal's store.
const COVER_ID: u32 = 0x636C_6267;

/// How long the selection has to sit still before a cover is worth fetching.
/// Long enough that holding a cursor key down does not fire a request per row.
const COVER_DELAY: Duration = Duration::from_millis(250);

pub struct App {
    settings: Settings,
    tab: Tab,
    mode: Mode,
    query: String,
    /// Narrows the library to matching titles, authors and filenames.
    filter: String,
    /// Caret position within whichever of the two boxes is showing, as a
    /// character index. Clamped when the tab changes.
    caret: usize,
    results: Vec<Book>,
    /// Indices into `results` that survive the format and language filters, in
    /// display order. What the table actually shows.
    visible: Vec<usize>,
    /// Narrow the results to one file format, e.g. `epub`. Survives a new
    /// search on purpose: "epubs only" is a standing preference, not a remark
    /// about one result set.
    fmt_filter: Option<String>,
    /// Narrow the results to one language, matched exactly against what the
    /// mirror reported.
    lang_filter: Option<String>,
    table: TableState,
    marked: Vec<bool>,
    jobs: HashMap<String, Job>,
    mirrors: Vec<Uri>,
    mirror_label: String,
    status: String,
    error: Option<String>,
    busy: bool,
    downloading: bool,
    /// A search typed before the mirror pool was ready, run once it is.
    pending_search: bool,
    quit: bool,
    tx: Sender<Ev>,

    /// Every past download, newest first. Read from disk at startup and again
    /// whenever one finishes, so the tab is never stale.
    library: Vec<history::Entry>,
    /// Indices into `library` that survive the filter, in display order.
    shown: Vec<usize>,
    library_table: TableState,

    /// How this terminal can draw a picture, if at all.
    protocol: Protocol,
    covers: HashMap<String, Slot>,
    /// The selection is only worth a cover request once it stops moving.
    cover_due: Option<Instant>,
    /// What is currently painted on the screen by escape code, and where, so
    /// it is only redrawn when it actually changes.
    painted: Option<(String, Rect)>,
    /// Where the last frame left room for the picture. Set during rendering,
    /// read afterwards by the code that writes the escape sequence.
    cover_area: Option<Rect>,
}

/// Run the interface until the user quits.
pub fn run(settings: Settings, initial_query: Option<String>) -> Result<()> {
    let (tx, rx) = mpsc::channel();

    // Keyboard events arrive on the same channel as worker results, so the UI
    // loop has a single thing to wait on.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            loop {
                match event::read() {
                    // `Press` only: Windows also reports key releases, which
                    // would otherwise double every keystroke.
                    Ok(CtEvent::Key(key)) if key.kind == KeyEventKind::Press => {
                        if tx.send(Ev::Key(key)).is_err() {
                            return;
                        }
                    }
                    Ok(CtEvent::Resize(_, _)) => {
                        if tx.send(Ev::Redraw).is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });
    }

    let query = initial_query.unwrap_or_default();
    let protocol = if settings.covers {
        graphics::detect()
    } else {
        Protocol::None
    };
    let mut app = App {
        settings,
        tab: Tab::Search,
        mode: if query.is_empty() {
            Mode::Editing
        } else {
            Mode::Browsing
        },
        caret: query.chars().count(),
        query,
        filter: String::new(),
        results: Vec::new(),
        visible: Vec::new(),
        fmt_filter: None,
        lang_filter: None,
        table: TableState::default(),
        marked: Vec::new(),
        jobs: HashMap::new(),
        mirrors: Vec::new(),
        mirror_label: "connecting…".into(),
        status: "finding a working mirror".into(),
        error: None,
        busy: true,
        downloading: false,
        pending_search: false,
        quit: false,
        tx,
        library: Vec::new(),
        shown: Vec::new(),
        library_table: TableState::default(),
        protocol,
        covers: HashMap::new(),
        cover_due: None,
        painted: None,
        cover_area: None,
    };
    app.pending_search = !app.query.is_empty();
    // Read the library up front rather than when the tab is first opened: it
    // costs one small file read, and it means the tab bar can say how many
    // books are there before anybody goes looking.
    app.reload_library();
    app.spawn_mirror_resolve(app.settings.refresh_mirrors);

    let mut terminal = ratatui::try_init()?;
    let outcome = app.event_loop(&mut terminal, &rx);
    app.erase_cover();
    ratatui::try_restore()?;
    outcome
}

impl App {
    fn event_loop(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        rx: &Receiver<Ev>,
    ) -> Result<()> {
        terminal.draw(|frame| self.render(frame))?;
        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(ev) => self.handle(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if self.quit {
                break;
            }
            // The selection may have stopped moving long enough to be worth a
            // cover; checked here rather than on the keypress so that holding a
            // cursor key down costs nothing.
            self.poll_cover();
            terminal.draw(|frame| self.render(frame))?;
            // Pixel protocols write outside ratatui's buffer, so the picture is
            // placed after the frame it belongs to has been drawn.
            self.paint_cover();
        }
        Ok(())
    }

    // --- background work -------------------------------------------------

    fn spawn_mirror_resolve(&mut self, refresh: bool) {
        self.busy = true;
        self.mirror_label = "connecting…".into();
        self.status = "finding a working mirror".into();

        let tx = self.tx.clone();
        let settings = self.settings.clone();
        std::thread::spawn(move || {
            let result = mirror::resolve(
                &settings.policy,
                ResolveOptions {
                    explicit: &settings.mirrors,
                    refresh,
                    progress: &|_| {},
                },
            );
            let payload = result
                .map(|pool| pool.mirrors().to_vec())
                .map_err(|e| e.to_string());
            let _ = tx.send(Ev::Mirrors(payload));
        });
    }

    fn spawn_search(&mut self) {
        let terms = self.query.trim().to_string();
        if terms.is_empty() {
            self.error = Some("type something to search for".into());
            return;
        }
        if self.mirrors.is_empty() {
            // No mirror yet; run this as soon as one turns up.
            self.pending_search = true;
            self.status = "waiting for a mirror".into();
            return;
        }

        // `author:herbert dune ext:epub` — the box is the only input this
        // interface has, so the tags do the work the CLI's flags do.
        let mut query = SearchQuery::new(String::new());
        query.limit = 100;
        query::apply(&terms, &mut query);
        if query.terms.trim().is_empty() {
            self.error = Some("that is all filters and no search terms".into());
            return;
        }

        self.busy = true;
        self.error = None;
        self.status = format!("searching for “{}”", query.terms);

        let tx = self.tx.clone();
        let settings = self.settings.clone();
        let mirrors = self.mirrors.clone();
        std::thread::spawn(move || {
            let payload = Http::new(settings.policy)
                .and_then(|http| {
                    Pool::new(mirrors)
                        .try_each(
                            |base| net::with_retry(2, |_| libgen::search(&http, base, &query)),
                            |_, _| {},
                        )
                        .map(|(books, used)| (books, net::host_of(&used).unwrap_or_default()))
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(Ev::Results(payload));
        });
    }

    /// Download every marked record, or the highlighted one when none are.
    fn spawn_downloads(&mut self) {
        if self.downloading {
            self.error = Some("a download is already running".into());
            return;
        }
        let chosen: Vec<Book> = if self.marked.iter().any(|m| *m) {
            self.results
                .iter()
                .zip(&self.marked)
                .filter(|(_, m)| **m)
                .map(|(b, _)| b.clone())
                .collect()
        } else {
            match self.selected() {
                Some(book) => vec![book.clone()],
                None => return,
            }
        };
        if chosen.is_empty() {
            return;
        }

        for book in &chosen {
            self.jobs.insert(book.md5.clone(), Job::Queued);
        }
        self.downloading = true;
        self.error = None;
        self.status = format!("downloading {} file(s)", chosen.len());

        let tx = self.tx.clone();
        let settings = self.settings.clone();
        let mirrors = self.mirrors.clone();
        std::thread::spawn(move || {
            let http = match Http::new(settings.policy) {
                Ok(h) => h,
                Err(e) => {
                    for book in &chosen {
                        let _ = tx.send(Ev::Finished {
                            md5: book.md5.clone(),
                            outcome: Err(e.to_string()),
                        });
                    }
                    return;
                }
            };
            let pool = Pool::new(mirrors);

            for book in chosen {
                let opts = download::Options {
                    dest_dir: settings.dest_dir.clone(),
                    filename: None,
                    max_bytes: settings.max_bytes,
                    verify: settings.verify,
                    force: settings.force,
                    resume: settings.resume,
                };

                // Progress arrives per 64 KB chunk; throttle it so a fast
                // transfer cannot flood the UI thread with redraws.
                let mut last = Instant::now() - Duration::from_secs(1);
                let md5 = book.md5.clone();
                let progress_tx = tx.clone();
                let mut report = |done: u64, total: Option<u64>| {
                    if last.elapsed() >= Duration::from_millis(100) {
                        last = Instant::now();
                        let _ = progress_tx.send(Ev::Progress {
                            md5: md5.clone(),
                            done,
                            total,
                        });
                    }
                };

                let result = pool.try_each(
                    |base| {
                        let resolved = libgen::resolve_download(&http, base, &book.md5)?;
                        download::fetch(&http, &resolved.url, &book, &opts, &mut report)
                    },
                    |_, _| {},
                );

                let outcome = match result {
                    Ok((outcome, _)) => {
                        history::record(settings.history, &book, &outcome);
                        Ok(outcome
                            .path
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default())
                    }
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Ev::Finished {
                    md5: book.md5.clone(),
                    outcome,
                });
            }
        });
    }

    // --- cover art --------------------------------------------------------

    /// The record the detail pane is describing.
    fn selected(&self) -> Option<&Book> {
        self.table
            .selected()
            .and_then(|i| self.visible.get(i))
            .and_then(|&i| self.results.get(i))
    }

    // --- narrowing the results --------------------------------------------
    //
    // A search brings back whatever the mirror has — Das Kapital is forty
    // German editions with the English translations scattered among them.
    // Rather than make people re-search with `lang:` tags, two facets carve
    // the results in place: `e` cycles through the formats actually present,
    // `l` through the languages, each shown with a live count of what
    // choosing it would leave.

    /// Does the current pair of filters admit this record?
    fn admits(&self, book: &Book) -> bool {
        self.fmt_admits(book) && self.lang_admits(book)
    }

    fn fmt_admits(&self, book: &Book) -> bool {
        match &self.fmt_filter {
            Some(want) => book.ext().eq_ignore_ascii_case(want),
            None => true,
        }
    }

    fn lang_admits(&self, book: &Book) -> bool {
        match &self.lang_filter {
            Some(want) => book
                .language
                .as_deref()
                .unwrap_or("unknown")
                .eq_ignore_ascii_case(want),
            None => true,
        }
    }

    /// Recompute which rows show, keeping the highlight on the same book when
    /// it survives the cut and on the top row when it does not.
    fn refine(&mut self) {
        let keep = self.selected().map(|b| b.md5.clone());
        self.visible = (0..self.results.len())
            .filter(|&i| self.admits(&self.results[i]))
            .collect();
        let position = keep.and_then(|md5| {
            self.visible
                .iter()
                .position(|&i| self.results[i].md5 == md5)
        });
        self.table.select(if self.visible.is_empty() {
            None
        } else {
            Some(position.unwrap_or(0))
        });
        self.selection_moved();
    }

    /// The values one facet could take, most common first, each counted with
    /// the *other* facet still applied — so the numbers say what picking that
    /// value would actually leave on screen.
    fn facet_options(&self, format: bool) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for book in &self.results {
            let admitted = if format {
                self.lang_admits(book)
            } else {
                self.fmt_admits(book)
            };
            if !admitted {
                continue;
            }
            let value = if format {
                book.ext().to_ascii_lowercase()
            } else {
                book.language.as_deref().unwrap_or("unknown").to_string()
            };
            match counts
                .iter_mut()
                .find(|(name, _)| name.eq_ignore_ascii_case(&value))
            {
                Some((_, n)) => *n += 1,
                None => counts.push((value, 1)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        counts
    }

    /// Step one facet through none → most common → … → back to none.
    fn cycle_filter(&mut self, format: bool, step: isize) {
        let options = self.facet_options(format);
        if options.is_empty() {
            return;
        }
        let current = if format {
            self.fmt_filter.as_deref()
        } else {
            self.lang_filter.as_deref()
        };
        // Position 0 is "no filter"; the options follow in order.
        let at = current
            .and_then(|c| options.iter().position(|(n, _)| n.eq_ignore_ascii_case(c)))
            .map(|i| i as isize + 1)
            .unwrap_or(0);
        let next = (at + step).rem_euclid(options.len() as isize + 1);
        let value = (next > 0).then(|| options[next as usize - 1].0.clone());
        if format {
            self.fmt_filter = value;
        } else {
            self.lang_filter = value;
        }
        self.refine();
    }

    /// The count behind the active choice of one facet, for the filter bar.
    fn facet_count(&self, format: bool) -> Option<usize> {
        let current = if format {
            self.fmt_filter.as_deref()?
        } else {
            self.lang_filter.as_deref()?
        };
        Some(
            self.facet_options(format)
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(current))
                .map(|(_, c)| *c)
                .unwrap_or(0),
        )
    }

    /// Note that the selection moved, so a cover becomes due shortly.
    fn selection_moved(&mut self) {
        if self.protocol != Protocol::None {
            self.cover_due = Some(Instant::now() + COVER_DELAY);
        }
    }

    /// Start a cover lookup once the selection has been still long enough.
    ///
    /// The two tabs source a cover differently: a search result has only its
    /// MD5, so its cover comes off a mirror; a library book is a file on disk
    /// that usually carries its own jacket, so that is read straight out of the
    /// file with no network at all — falling back to the cache from a past
    /// search, and then to a mirror, when the file has none of its own.
    fn poll_cover(&mut self) {
        let Some(due) = self.cover_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        self.cover_due = None;

        let Some(md5) = self.focused_md5() else {
            return;
        };
        if self.covers.contains_key(&md5) {
            return;
        }

        match self.tab {
            Tab::Search => self.spawn_search_cover(md5),
            Tab::Library => self.spawn_library_cover(md5),
        }
    }

    /// Fetch the highlighted search result's cover from a mirror.
    fn spawn_search_cover(&mut self, md5: String) {
        if self.mirrors.is_empty() {
            return;
        }
        let Some(book) = self.selected().cloned() else {
            return;
        };
        self.covers.insert(md5, Slot::Looking);

        let tx = self.tx.clone();
        let settings = self.settings.clone();
        let mirrors = self.mirrors.clone();
        std::thread::spawn(move || {
            let result = Http::new(settings.policy).and_then(|http| {
                // One mirror only. A cover is not worth walking the pool for,
                // and a mirror that cannot serve one is usually about to fail
                // the next search anyway.
                let base = mirrors
                    .first()
                    .cloned()
                    .ok_or_else(|| crate::err!("no mirror available"))?;
                cover::fetch(&http, &base, &book)
            });
            let _ = tx.send(Ev::Cover {
                md5: book.md5,
                result: result.map_err(|e| e.to_string()),
            });
        });
    }

    /// Find the highlighted library book's cover, cheapest source first.
    ///
    /// Three places, in order: the jacket embedded in the file you downloaded
    /// (no network, and it is the book you actually have); a cover cached from a
    /// past search of the same book; and finally the mirror, the same way the
    /// Search tab does it. The mirror fallback matters because most formats
    /// carry no cover this can extract — a PDF has no embedded jacket at all,
    /// and plenty of real EPUBs and MOBIs hide theirs where the heuristics miss
    /// — so without it those books would sit in the library with a blank frame
    /// forever. The fetch caches to disk, so a book is only pulled off a mirror
    /// once.
    fn spawn_library_cover(&mut self, md5: String) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        self.covers.insert(md5.clone(), Slot::Looking);

        let tx = self.tx.clone();
        let settings = self.settings.clone();
        let mirrors = self.mirrors.clone();
        std::thread::spawn(move || {
            // The file's own cover first, then a cover cached from a past search.
            let local = entry
                .present()
                .then(|| crate::embedded::cover(&entry.path))
                .flatten()
                .or_else(|| cover::cached(&md5).flatten());

            // Nothing on disk: ask a mirror, exactly as the Search tab would,
            // building a lookup key out of what the history row remembers.
            let result = match local {
                Some(cover) => Ok(Some(cover)),
                None => Self::fetch_library_cover(&settings, &mirrors, &entry),
            };
            let _ = tx.send(Ev::Cover {
                md5,
                result: result.map_err(|e| e.to_string()),
            });
        });
    }

    /// Pull a library book's cover off a mirror, keyed by its MD5. Returns
    /// `Ok(None)` when there is simply no mirror to ask or the record has no
    /// cover; both leave the frame empty rather than showing an error over a
    /// book someone already has.
    fn fetch_library_cover(
        settings: &Settings,
        mirrors: &[Uri],
        entry: &history::Entry,
    ) -> Result<Option<Cover>> {
        let Some(base) = mirrors.first().cloned() else {
            return Ok(None);
        };
        let book = Book {
            md5: entry.md5.clone(),
            title: entry.title.clone(),
            authors: entry.authors.clone(),
            extension: entry.extension.clone(),
            ..Default::default()
        };
        let http = Http::new(settings.policy)?;
        cover::fetch(&http, &base, &book)
    }

    /// The MD5 of whatever is highlighted on the current tab — a search result
    /// or a library entry. Covers are keyed by it regardless of where the book
    /// is being looked at, so a book downloaded and then found again in the
    /// library reuses the cover already in hand.
    fn focused_md5(&self) -> Option<String> {
        match self.tab {
            Tab::Search => self.selected().map(|b| b.md5.clone()),
            Tab::Library => self.selected_entry().map(|e| e.md5.clone()),
        }
    }

    /// The cover to show right now, if there is one.
    fn current_art(&self) -> Option<&Art> {
        match self.covers.get(&self.focused_md5()?) {
            Some(Slot::Ready(art)) => Some(art),
            _ => None,
        }
    }

    /// Whether the detail pane should make room for a picture.
    fn cover_visible(&self) -> bool {
        self.protocol != Protocol::None
            && match self.covers.get(self.focused_md5().as_deref().unwrap_or("")) {
                // Space is reserved while looking too, so the pane does not
                // jump about when the picture arrives.
                Some(Slot::Looking) => true,
                Some(Slot::Ready(_)) => true,
                _ => false,
            }
    }

    /// Draw the cover with an escape sequence, for terminals that do pixels.
    ///
    /// Half-block art goes through ratatui instead and never reaches here.
    fn paint_cover(&mut self) {
        if let Some(escape) = self.cover_escape() {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            let _ = write!(stdout, "{escape}");
            let _ = stdout.flush();
        }
    }

    /// The escape sequence that brings the screen up to date with what should
    /// be showing, or `None` when it already is.
    ///
    /// Separate from writing it so the decision — what to place, where, and
    /// whether anything needs to change at all — can be tested without a
    /// terminal to write to.
    fn cover_escape(&mut self) -> Option<String> {
        if !self.protocol.is_pixels() {
            return None;
        }
        let want = self
            .cover_rect()
            .zip(self.focused_md5())
            .filter(|_| self.current_art().is_some())
            .map(|(rect, md5)| (md5, rect));

        if want == self.painted {
            return None;
        }

        let mut out = String::new();
        if self.painted.is_some() {
            out.push_str(&graphics::kitty_delete(COVER_ID));
        }
        if let Some((md5, rect)) = &want
            && let Some(art) = self.covers.get(md5).and_then(|slot| match slot {
                Slot::Ready(art) => Some(art),
                _ => None,
            })
        {
            // Terminal coordinates are one-based; ratatui's are not.
            out.push_str(&format!("\x1b[{};{}H", rect.y + 1, rect.x + 1));
            out.push_str(&match self.protocol {
                Protocol::ITerm2 => graphics::iterm_image(&art.encoded, rect.width, rect.height),
                _ => match &art.pixels {
                    Some(image) => graphics::kitty_image(
                        COVER_ID,
                        &image.fit(400, 600),
                        rect.width,
                        rect.height,
                    ),
                    // Kitty decodes PNG itself, so it can have the file.
                    None => graphics::kitty_png(COVER_ID, &art.encoded, rect.width, rect.height),
                },
            });
        }

        self.painted = want;
        if out.is_empty() { None } else { Some(out) }
    }

    /// Take any painted cover off the screen.
    fn erase_cover(&mut self) {
        if self.painted.take().is_some() && self.protocol.is_pixels() {
            use std::io::Write;
            let mut stdout = std::io::stdout();
            let _ = write!(stdout, "{}", graphics::kitty_delete(COVER_ID));
            let _ = stdout.flush();
        }
    }

    // --- the library ------------------------------------------------------

    /// Switch tabs, carrying the caret somewhere valid for the box that is now
    /// showing.
    fn show(&mut self, tab: Tab) {
        if self.tab == tab {
            return;
        }
        self.tab = tab;
        self.error = None;
        self.caret = self.input().chars().count();
        match tab {
            Tab::Library => {
                // Downloads made elsewhere since we started belong here too.
                self.reload_library();
                // Whatever is painted belongs to the tab we are leaving; the
                // library's own cover is then armed like any selection.
                self.erase_cover();
                self.selection_moved();
                self.status = match self.library.len() {
                    0 => "nothing downloaded yet".into(),
                    1 => "1 book in the library".into(),
                    n => format!("{n} books in the library"),
                };
            }
            Tab::Search => {
                // The detail pane is back, so its cover has to be placed again.
                self.selection_moved();
                self.status = match self.results.len() {
                    0 => "press / to search".into(),
                    n => format!("{n} result(s)"),
                };
            }
        }
    }

    /// Re-read the library from disk and reapply the filter, keeping the
    /// highlight on something sensible.
    fn reload_library(&mut self) {
        self.library = history::load();
        self.refilter();
    }

    /// Work out which entries the filter admits.
    ///
    /// Matching is the same rule the `clibgen open` selector uses — title,
    /// author or filename, case-insensitively — so the two agree about what
    /// "dune" means.
    fn refilter(&mut self) {
        let needle = self.filter.trim().to_lowercase();
        self.shown = self
            .library
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                needle.is_empty()
                    || e.title.to_lowercase().contains(&needle)
                    || e.filename().to_lowercase().contains(&needle)
                    || e.authors
                        .as_deref()
                        .is_some_and(|a| a.to_lowercase().contains(&needle))
            })
            .map(|(i, _)| i)
            .collect();

        self.library_table.select(if self.shown.is_empty() {
            None
        } else {
            // Filtering shortens the list under the highlight, so pull it back
            // in rather than leaving it pointing past the end.
            Some(
                self.library_table
                    .selected()
                    .unwrap_or(0)
                    .min(self.shown.len() - 1),
            )
        });
    }

    fn selected_entry(&self) -> Option<&history::Entry> {
        self.library_table
            .selected()
            .and_then(|i| self.shown.get(i))
            .and_then(|&i| self.library.get(i))
    }

    /// Hand a past download to a reader, or to the file manager.
    fn launch_selected(&mut self, reveal: bool) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        let reader = self.settings.reader.clone();
        let result = if reveal {
            launch::reveal(&entry.path)
        } else {
            launch::open(&entry.path, reader.as_deref())
        };
        match result {
            Ok(()) => {
                self.error = None;
                self.status = format!(
                    "{} {}",
                    if reveal { "showing" } else { "opened" },
                    entry.filename()
                );
            }
            Err(e) => self.error = Some(first_line(&e.to_string())),
        }
    }

    /// Open the file for the highlighted search result, if it was downloaded.
    fn launch_result(&mut self, reveal: bool) {
        let Some(md5) = self.selected().map(|b| b.md5.clone()) else {
            return;
        };
        let entry = history::load().into_iter().find(|e| e.md5 == md5);
        let Some(entry) = entry else {
            self.error = Some("that one has not been downloaded yet".into());
            return;
        };
        let reader = self.settings.reader.clone();
        let result = if reveal {
            launch::reveal(&entry.path)
        } else {
            launch::open(&entry.path, reader.as_deref())
        };
        match result {
            Ok(()) => {
                self.error = None;
                self.status = format!("opened {}", entry.filename());
            }
            Err(e) => self.error = Some(first_line(&e.to_string())),
        }
    }

    /// Forget the highlighted entry. The file itself is left alone.
    fn forget_selected(&mut self) {
        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };
        match history::remove(&entry.path) {
            Ok(_) => {
                self.reload_library();
                self.status = format!("forgot {} — the file is still there", entry.filename());
            }
            Err(e) => self.error = Some(first_line(&e.to_string())),
        }
    }

    // --- the text box -----------------------------------------------------
    //
    // One box, two meanings: search terms on one tab, a filter on the other.
    // Sharing the editing code means the caret, word deletion and multi-byte
    // handling only exist once.

    fn input(&self) -> &str {
        match self.tab {
            Tab::Search => &self.query,
            Tab::Library => &self.filter,
        }
    }

    fn input_mut(&mut self) -> &mut String {
        match self.tab {
            Tab::Search => &mut self.query,
            Tab::Library => &mut self.filter,
        }
    }

    /// Called after every edit; the library narrows as you type, whereas a
    /// search waits for ⏎ because it costs a request.
    fn input_changed(&mut self) {
        if self.tab == Tab::Library {
            self.refilter();
        }
    }

    // --- event handling ---------------------------------------------------

    fn handle(&mut self, ev: Ev) {
        match ev {
            // A resize moves everything, so whatever was painted is now in the
            // wrong place and has to be placed again.
            Ev::Redraw => self.erase_cover(),
            Ev::Key(key) => self.on_key(key),
            Ev::Mirrors(Ok(mirrors)) => {
                self.busy = false;
                self.mirror_label = mirrors
                    .first()
                    .and_then(|u| net::host_of(u).ok())
                    .unwrap_or_else(|| "none".into());
                let count = mirrors.len();
                let first_mirror = self.mirrors.is_empty() && count > 0;
                self.mirrors = mirrors;
                self.status = format!("{count} mirror(s) available");
                if self.pending_search {
                    self.pending_search = false;
                    self.spawn_search();
                }
                // A library cover that came up empty while there was no mirror
                // to ask deserves another go now that there is one; drop those
                // blanks and re-arm the selection so it looks again.
                if first_mirror {
                    self.covers.retain(|_, slot| !matches!(slot, Slot::Nothing));
                    self.selection_moved();
                }
            }
            Ev::Mirrors(Err(e)) => {
                self.busy = false;
                self.mirror_label = "unavailable".into();
                self.status = "no mirror available".into();
                self.error = Some(first_line(&e));
            }
            Ev::Results(Ok((books, host))) => {
                self.busy = false;
                self.mirror_label = host;
                self.marked = vec![false; books.len()];
                self.table.select(None);
                self.results = books;
                self.refine();
                self.status = match (self.results.len(), self.visible.len()) {
                    (0, _) => "nothing found — try fewer words".into(),
                    (total, shown) if shown < total => {
                        format!("{shown} of {total} match the filters")
                    }
                    (total, _) => format!("{total} result(s)"),
                };
                self.mode = Mode::Browsing;
            }
            Ev::Results(Err(e)) => {
                self.busy = false;
                self.status = "search failed".into();
                self.error = Some(first_line(&e));
            }
            Ev::Progress { md5, done, total } => {
                self.jobs.insert(md5, Job::Running { done, total });
            }
            Ev::Finished { md5, outcome } => {
                let job = match outcome {
                    Ok(name) => {
                        self.status = format!("saved {name}");
                        // The book is in the library now; the tab should say so
                        // without being asked to reload.
                        self.reload_library();
                        Job::Saved(name)
                    }
                    Err(e) => {
                        let message = first_line(&e);
                        self.error = Some(message.clone());
                        Job::Failed(message)
                    }
                };
                self.jobs.insert(md5, job);
                // The worker finishes the whole batch before going quiet, so
                // the flag clears once nothing is left queued or running.
                self.downloading = self
                    .jobs
                    .values()
                    .any(|j| matches!(j, Job::Queued | Job::Running { .. }));
            }
            Ev::Cover { md5, result } => {
                let slot = match result {
                    // What each protocol needs from a cover differs: iTerm2
                    // takes the encoded file and wants nothing decoded, kitty
                    // takes either, and half blocks can only draw pixels — so
                    // a cover it cannot decode is no cover at all.
                    Ok(Some(cover)) => match self.protocol {
                        Protocol::ITerm2 => Slot::Ready(Box::new(Art {
                            encoded: cover.encoded,
                            pixels: None,
                        })),
                        Protocol::Kitty => {
                            let pixels = cover.pixels();
                            Slot::Ready(Box::new(Art {
                                encoded: cover.encoded,
                                pixels,
                            }))
                        }
                        Protocol::Blocks => match cover.pixels() {
                            Some(pixels) => Slot::Ready(Box::new(Art {
                                encoded: Vec::new(),
                                pixels: Some(pixels),
                            })),
                            None => Slot::Nothing,
                        },
                        Protocol::None => Slot::Nothing,
                    },
                    // A cover that will not come is not an error worth putting
                    // in front of someone who asked for a book.
                    Ok(None) | Err(_) => Slot::Nothing,
                };
                self.covers.insert(md5, slot);
            }
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Ctrl-C always quits, whatever is on screen.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        // Switching tabs works from everywhere, including mid-typing. The
        // library used to be reachable only from the results list, which meant
        // you had to run a search before you could look at books you already
        // had — exactly backwards.
        match key.code {
            KeyCode::Tab | KeyCode::BackTab => {
                if self.mode == Mode::Help {
                    self.mode = Mode::Browsing;
                }
                self.show(self.tab.other());
                return;
            }
            // Direct jumps, for when you know where you are going. Not while
            // typing, where a digit is a digit.
            KeyCode::Char('1') if self.mode != Mode::Editing => return self.show(Tab::Search),
            KeyCode::Char('2') if self.mode != Mode::Editing => return self.show(Tab::Library),
            _ => {}
        }
        match self.mode {
            Mode::Help => self.mode = Mode::Browsing,
            Mode::Editing => self.on_key_editing(key),
            Mode::Browsing => match self.tab {
                Tab::Search => self.on_key_results(key),
                Tab::Library => self.on_key_library(key),
            },
        }
    }

    fn on_key_editing(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.input().chars().count();
        match key.code {
            KeyCode::Enter => {
                self.mode = Mode::Browsing;
                // A filter has already been applied on every keystroke; only a
                // search has anything left to do.
                if self.tab == Tab::Search {
                    self.spawn_search();
                }
            }
            KeyCode::Esc => self.mode = Mode::Browsing,
            KeyCode::Char('u') if ctrl => {
                self.input_mut().clear();
                self.caret = 0;
                self.input_changed();
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Char(c) => {
                let at = self.byte_offset(self.caret);
                self.input_mut().insert(at, c);
                self.caret += 1;
                self.input_changed();
            }
            KeyCode::Backspace if self.caret > 0 => {
                let at = self.byte_offset(self.caret - 1);
                self.input_mut().remove(at);
                self.caret -= 1;
                self.input_changed();
            }
            KeyCode::Delete if self.caret < len => {
                let at = self.byte_offset(self.caret);
                self.input_mut().remove(at);
                self.input_changed();
            }
            KeyCode::Left => self.caret = self.caret.saturating_sub(1),
            KeyCode::Right => self.caret = (self.caret + 1).min(len),
            KeyCode::Home => self.caret = 0,
            KeyCode::End => self.caret = len,
            // The lists are still navigable while the box has focus, so you can
            // narrow and pick without reaching for Esc first.
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            _ => {}
        }
    }

    /// Move whichever list the visible tab owns.
    fn move_selection(&mut self, delta: isize) {
        match self.tab {
            Tab::Search => self.move_by(delta),
            Tab::Library => self.move_library_by(delta),
        }
    }

    fn on_key_results(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('/') | KeyCode::Char('i') => {
                self.mode = Mode::Editing;
                self.caret = self.query.chars().count();
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::Home | KeyCode::Char('g') if !self.visible.is_empty() => {
                self.table.select(Some(0));
                self.selection_moved();
            }
            KeyCode::End | KeyCode::Char('G') if !self.visible.is_empty() => {
                self.table.select(Some(self.visible.len() - 1));
                self.selection_moved();
            }
            KeyCode::Char(' ') => {
                if let Some(&i) = self.table.selected().and_then(|i| self.visible.get(i))
                    && let Some(flag) = self.marked.get_mut(i)
                {
                    *flag = !*flag;
                }
                self.move_by(1);
            }
            KeyCode::Char('a') => {
                // "Everything" means everything showing: filters carve the
                // batch too, which is the whole point of them.
                let all = self
                    .visible
                    .iter()
                    .all(|&i| self.marked.get(i).copied().unwrap_or(false));
                for &i in &self.visible {
                    if let Some(flag) = self.marked.get_mut(i) {
                        *flag = !all;
                    }
                }
            }
            KeyCode::Char('e') => self.cycle_filter(true, 1),
            KeyCode::Char('E') => self.cycle_filter(true, -1),
            KeyCode::Char('l') => self.cycle_filter(false, 1),
            KeyCode::Char('L') => self.cycle_filter(false, -1),
            KeyCode::Char('x') if self.fmt_filter.is_some() || self.lang_filter.is_some() => {
                self.fmt_filter = None;
                self.lang_filter = None;
                self.refine();
            }
            KeyCode::Enter => self.spawn_downloads(),
            KeyCode::Char('r') => self.spawn_search(),
            KeyCode::Char('m') => self.spawn_mirror_resolve(true),
            KeyCode::Char('o') => self.launch_result(false),
            KeyCode::Char('f') => self.launch_result(true),
            _ => {}
        }
    }

    fn on_key_library(&mut self, key: KeyEvent) {
        match key.code {
            // The library is a peer of the search tab, not a detour off it, so
            // `q` means the same thing on both.
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('/') | KeyCode::Char('i') => {
                self.mode = Mode::Editing;
                self.caret = self.filter.chars().count();
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Down | KeyCode::Char('j') => self.move_library_by(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_library_by(-1),
            KeyCode::PageDown => self.move_library_by(10),
            KeyCode::PageUp => self.move_library_by(-10),
            KeyCode::Home | KeyCode::Char('g') if !self.shown.is_empty() => {
                self.library_table.select(Some(0));
                self.selection_moved();
            }
            KeyCode::End | KeyCode::Char('G') if !self.shown.is_empty() => {
                self.library_table.select(Some(self.shown.len() - 1));
                self.selection_moved();
            }
            KeyCode::Enter | KeyCode::Char('o') => self.launch_selected(false),
            KeyCode::Char('f') => self.launch_selected(true),
            KeyCode::Char('d') => self.forget_selected(),
            KeyCode::Char('r') => {
                self.reload_library();
                self.status = format!("{} book(s)", self.library.len());
            }
            _ => {}
        }
    }

    fn move_library_by(&mut self, delta: isize) {
        if self.shown.is_empty() {
            return;
        }
        let last = self.shown.len() - 1;
        let current = self.library_table.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, last as isize) as usize;
        if Some(next) != self.library_table.selected() {
            self.selection_moved();
        }
        self.library_table.select(Some(next));
    }

    fn move_by(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() - 1;
        let current = self.table.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, last as isize) as usize;
        if Some(next) != self.table.selected() {
            self.selection_moved();
        }
        self.table.select(Some(next));
    }

    fn delete_word(&mut self) {
        let text = self.input();
        let head: String = text.chars().take(self.caret).collect();
        let trimmed = head.trim_end();
        let cut = match trimmed.rfind(' ') {
            Some(i) => trimmed[..i + 1].chars().count(),
            None => 0,
        };
        let tail: String = text.chars().skip(self.caret).collect();
        let kept: String = text.chars().take(cut).collect();
        *self.input_mut() = format!("{kept}{tail}");
        self.caret = cut;
        self.input_changed();
    }

    /// Byte offset of the given character index, for editing in place.
    fn byte_offset(&self, chars: usize) -> usize {
        let text = self.input();
        text.char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(text.len())
    }

    // --- rendering --------------------------------------------------------

    /// How tall the detail pane is, borders included.
    ///
    /// A cover needs more room than the six lines of text, but only when there
    /// is one to show, and never so much that the results list is squeezed out.
    fn details_height(&self, area: Rect) -> u16 {
        let plain = 8u16;
        // Leave the list at least four rows plus its header, whatever else we
        // do — the details pane must never crowd out the results.
        let ceiling = area.height.saturating_sub(3 + 1 + 5).max(3);
        let floor = plain.min(ceiling);
        if !self.cover_visible() {
            return floor;
        }
        let wanted = 13;
        wanted.min(area.height.saturating_sub(3 + 1 + 5)).max(floor)
    }

    /// Where the picture goes, in screen coordinates.
    fn cover_rect(&self) -> Option<Rect> {
        self.cover_area
    }

    /// Whether the list earns its tall side panel: enough columns for the rows
    /// to keep breathing next to a poster-sized cover, and something to show.
    fn side_panel(&self, area: Rect) -> bool {
        if area.width < 110 {
            return false;
        }
        match self.tab {
            Tab::Search => !self.results.is_empty(),
            Tab::Library => !self.shown.is_empty(),
        }
    }

    fn render(&mut self, frame: &mut Frame) {
        let area = frame.area();
        // The canvas goes down first; everything after patches colour onto it.
        frame.render_widget(
            Block::new().style(Style::new().bg(theme::BG).fg(theme::TEXT)),
            area,
        );
        self.cover_area = None;

        let side = self.side_panel(area);
        let filter_bar = (self.tab == Tab::Search && !self.results.is_empty()) as u16;
        // No strip when there is nothing to describe — a bordered box holding
        // "no selection" is furniture, not information. Errors still earn it.
        let empty_tab = match self.tab {
            Tab::Search => self.results.is_empty(),
            Tab::Library => self.shown.is_empty(),
        };
        let details = if side || (empty_tab && self.error.is_none()) {
            0
        } else {
            self.details_height(area)
        };
        let chunks = Layout::vertical([
            Constraint::Length(3),          // search box
            Constraint::Length(filter_bar), // the two facets, when there are results
            Constraint::Min(3),             // results
            Constraint::Length(details),    // details strip (narrow layout)
            Constraint::Length(1),          // key hints
        ])
        .split(area);

        self.render_input(frame, chunks[0]);
        if filter_bar == 1 {
            self.render_filters(frame, chunks[1]);
        }
        match self.tab {
            _ if side => {
                let columns =
                    Layout::horizontal([Constraint::Min(40), Constraint::Length(42)])
                        .split(chunks[2]);
                match self.tab {
                    Tab::Search => {
                        self.render_results(frame, columns[0]);
                        self.render_side_details(frame, columns[1]);
                    }
                    Tab::Library => {
                        self.render_library(frame, columns[0]);
                        self.render_library_side_details(frame, columns[1]);
                    }
                }
            }
            Tab::Search => {
                self.render_results(frame, chunks[2]);
                self.render_details(frame, chunks[3]);
            }
            Tab::Library => {
                self.render_library(frame, chunks[2]);
                self.render_library_details(frame, chunks[3]);
            }
        }
        self.render_hints(frame, chunks[4]);

        if self.mode == Mode::Help {
            self.render_help(frame);
        }
    }

    /// The refinement bar: the two facets and what they leave showing.
    ///
    /// It lives on its own row, under the search box, so the counts update in
    /// place as `e` and `l` cycle — filters you can watch working, rather than
    /// tags you have to re-type into a new search.
    fn render_filters(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let filtered = self.fmt_filter.is_some() || self.lang_filter.is_some();

        // Right side first: how much of the catch is on screen. The amber only
        // comes out when a filter is actually hiding something.
        let counter = if self.visible.len() < self.results.len() {
            Span::styled(
                format!("{} of {} shown ", self.visible.len(), self.results.len()),
                Style::new().fg(theme::AMBER).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!("{} results ", self.results.len()), theme::faint())
        };
        frame.render_widget(
            Paragraph::new(Line::from(counter)).alignment(Alignment::Right),
            area,
        );

        let key = |k: &str| {
            Span::styled(k.to_string(), theme::accent().add_modifier(Modifier::BOLD))
        };
        let label = |l: &str| Span::styled(format!(" {l} "), theme::faint());

        let mut spans = vec![Span::raw(" "), key("e"), label("format")];
        match &self.fmt_filter {
            Some(f) => {
                spans.push(Span::styled(format!(" {f} "), theme::format_chip(f)));
                if let Some(n) = self.facet_count(true) {
                    spans.push(Span::styled(format!(" {n}"), theme::faint()));
                }
            }
            None => spans.push(Span::styled("all", theme::muted())),
        }
        spans.push(Span::raw("    "));
        spans.push(key("l"));
        spans.push(label("language"));
        match &self.lang_filter {
            Some(l) => {
                spans.push(Span::styled(
                    l.clone(),
                    Style::new().fg(theme::LANG).add_modifier(Modifier::BOLD),
                ));
                if let Some(n) = self.facet_count(false) {
                    spans.push(Span::styled(format!(" {n}"), theme::faint()));
                }
            }
            None => spans.push(Span::styled("all", theme::muted())),
        }
        if filtered {
            spans.push(Span::raw("    "));
            spans.push(key("x"));
            spans.push(label("clear"));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The tab bar and the text box under it.
    ///
    /// The two tabs are drawn as the box's title so they cost no rows of their
    /// own — on an eighty by twenty-four terminal every line spent on chrome is
    /// a line of results not shown.
    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let editing = self.mode == Mode::Editing;
        // The box border lights up in the accent while you type, and rests at
        // faint otherwise — a quiet cue for where the keyboard is going.
        let border = if editing {
            theme::ACCENT
        } else {
            theme::FAINT
        };

        let tab = |label: &str, count: Option<usize>, active: bool| {
            let text = match count {
                Some(n) => format!("  {label} {n}  "),
                None => format!("  {label}  "),
            };
            Span::styled(
                text,
                if active {
                    // The active tab is a solid accent chip: colour and reverse
                    // together, so it is unmistakable even without truecolour.
                    Style::new()
                        .fg(Color::Rgb(18, 24, 28))
                        .bg(theme::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme::faint()
                },
            )
        };

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(border))
            .title(Line::from(vec![
                tab("1 SEARCH", None, self.tab == Tab::Search),
                Span::raw(" "),
                tab("2 LIBRARY", Some(self.library.len()), self.tab == Tab::Library),
            ]))
            .title_top(
                Line::from(vec![
                    Span::styled("● ", match self.tab {
                        // A green dot when a mirror is answering, amber while we
                        // are still looking — status you can read at a glance.
                        Tab::Search if !self.mirrors.is_empty() => theme::accent(),
                        Tab::Search => Style::new().fg(theme::AMBER),
                        Tab::Library => theme::accent(),
                    }),
                    Span::styled(
                        match self.tab {
                            Tab::Search => format!("{} ", self.mirror_label),
                            // The mirror is irrelevant to books already on disk.
                            Tab::Library => "on this machine ".to_string(),
                        },
                        theme::muted(),
                    ),
                ])
                .right_aligned(),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let prompt = Span::styled(
            if editing { "❯ " } else { "  " },
            Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        );
        let text = if self.input().is_empty() && !editing {
            Span::styled(
                match self.tab {
                    Tab::Search => "press / to search Library Genesis",
                    Tab::Library => "press / to filter your books",
                },
                theme::faint(),
            )
        } else {
            Span::styled(self.input().to_string(), theme::text())
        };
        frame.render_widget(Paragraph::new(Line::from(vec![prompt, text])), inner);

        if editing {
            // Place the real terminal cursor so it blinks in the right column,
            // past the two-cell "❯ " prompt.
            let offset: usize = 2 + self
                .input()
                .chars()
                .take(self.caret)
                .map(|c| crate::term::display_width(&c.to_string()))
                .sum::<usize>();
            frame.set_cursor_position((inner.x + offset as u16, inner.y));
        }
    }

    fn render_results(&mut self, frame: &mut Frame, area: Rect) {
        if self.visible.is_empty() {
            self.render_empty(frame, area);
            return;
        }

        let header = Row::new([
            Cell::from(""),
            Cell::from("TITLE"),
            Cell::from("AUTHOR"),
            Cell::from("YEAR"),
            Cell::from("LANGUAGE"),
            Cell::from("SIZE"),
            Cell::from("FMT"),
            Cell::from("STATUS"),
        ])
        .style(theme::header())
        .height(1);

        let selected = self.table.selected();
        let rows: Vec<Row> = self
            .visible
            .iter()
            .enumerate()
            .filter_map(|(row, &i)| self.results.get(i).map(|b| (row, i, b)))
            .map(|(row, i, book)| {
                let marked = self.marked.get(i).copied().unwrap_or(false);
                let job = self.jobs.get(&book.md5);
                let (marker, marker_style) = match job {
                    Some(Job::Saved(_)) => ("✓", Style::new().fg(theme::SUCCESS)),
                    Some(Job::Failed(_)) => ("✗", Style::new().fg(theme::DANGER)),
                    Some(Job::Running { .. }) | Some(Job::Queued) => {
                        ("↓", Style::new().fg(theme::ACCENT))
                    }
                    None if marked => ("●", Style::new().fg(theme::AMBER)),
                    None => (" ", Style::new()),
                };

                let author = {
                    let all = book.authors_or_unknown();
                    all.split(';').next().unwrap_or(all).trim().to_string()
                };

                // The highlighted title goes bold; every title stays at full
                // strength, because the titles are what the screen is for.
                let title_style = if Some(row) == selected {
                    theme::text().add_modifier(Modifier::BOLD)
                } else {
                    theme::text()
                };
                // The selection wash replaces every background, so the chip
                // trades its solid for the same hue as bold text there.
                let chip = if Some(row) == selected {
                    Style::new()
                        .fg(theme::format_color(book.ext()))
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme::format_chip(book.ext())
                };

                Row::new([
                    Cell::from(marker).style(marker_style),
                    Cell::from(book.title.clone()).style(title_style),
                    Cell::from(author).style(theme::muted()),
                    Cell::from(book.year.clone().unwrap_or_default()).style(theme::faint()),
                    Cell::from(book.language.clone().unwrap_or_default())
                        .style(Style::new().fg(theme::LANG)),
                    Cell::from(book.size_human()).style(theme::faint()),
                    Cell::from(format!(" {} ", book.ext())).style(chip),
                    Cell::from(job_label(job)).style(job_style(job)),
                ])
                // The zebra: alternate rows sit one step off the canvas, so a
                // wide row can be followed without a ruler.
                .style(Style::new().bg(if row % 2 == 1 {
                    theme::BG_ALT
                } else {
                    theme::BG
                }))
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                // Titles carry the information; authors repeat. Weight the
                // room accordingly.
                Constraint::Fill(5),
                Constraint::Fill(2),
                Constraint::Length(4),
                Constraint::Length(10),
                Constraint::Length(8),
                Constraint::Length(6),
                Constraint::Length(7),
            ],
        )
        .header(header)
        .column_spacing(1)
        .row_highlight_style(theme::selected_row())
        .highlight_symbol(Span::styled(
            theme::CURSOR,
            theme::accent().add_modifier(Modifier::BOLD),
        ));

        frame.render_stateful_widget(table, area, &mut self.table);
    }

    /// The results area before or between searches: the status, and once a
    /// mirror is up, a few words on how to begin. Centred rather than tucked in
    /// a corner, so an empty screen still looks composed.
    fn render_empty(&self, frame: &mut Frame, area: Rect) {
        // Results exist but the filters admit none of them — say which lever
        // to pull, not just "nothing here".
        if !self.results.is_empty() {
            let mut carved = Vec::new();
            if let Some(f) = &self.fmt_filter {
                carved.push(f.clone());
            }
            if let Some(l) = &self.lang_filter {
                carved.push(l.clone());
            }
            let lines = vec![
                Line::from(Span::styled(
                    format!(
                        "none of the {} results are {}",
                        self.results.len(),
                        carved.join(" + ")
                    ),
                    theme::muted(),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("x", theme::accent().add_modifier(Modifier::BOLD)),
                    Span::styled(" clears the filters   ", theme::faint()),
                    Span::styled("e l", theme::accent().add_modifier(Modifier::BOLD)),
                    Span::styled(" cycle them", theme::faint()),
                ]),
            ];
            let block = area.inner(Margin {
                horizontal: 2,
                vertical: area.height.saturating_sub(lines.len() as u16) / 2,
            });
            frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), block);
            return;
        }

        let mut lines = vec![Line::from(Span::styled(
            self.status.clone() + if self.busy { "…" } else { "" },
            theme::muted(),
        ))];
        if !self.busy && self.results.is_empty() && self.mode != Mode::Editing {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled("press ", theme::faint()),
                Span::styled("/", theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(" and type a title or author", theme::faint()),
            ]));
        }
        let block = area.inner(Margin {
            horizontal: 2,
            vertical: area.height.saturating_sub(lines.len() as u16) / 2,
        });
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            block,
        );
    }

    fn render_details(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::FAINT));
        let whole = block.inner(area);
        frame.render_widget(block, area);

        // Split off a column for the picture when there is one coming.
        self.cover_area = None;
        let inner = if self.cover_visible() && whole.width > 30 {
            // Roughly two by three, given a character cell about twice as tall
            // as it is wide.
            let width = ((whole.height as u32 * 2 * 2 / 3) as u16).clamp(6, 16);
            let picture = Rect {
                x: whole.x,
                y: whole.y,
                width,
                height: whole.height,
            };
            self.render_cover(frame, picture);
            Rect {
                x: whole.x + width + 2,
                y: whole.y,
                width: whole.width.saturating_sub(width + 2),
                height: whole.height,
            }
        } else {
            whole
        };

        let Some(book) = self.selected().cloned() else {
            let hint = self
                .error
                .clone()
                .unwrap_or_else(|| "no selection".to_string());
            let style = if self.error.is_some() {
                Style::new().fg(theme::DANGER)
            } else {
                theme::faint()
            };
            frame.render_widget(
                Paragraph::new(hint).style(style).wrap(Wrap { trim: true }),
                inner,
            );
            return;
        };

        let mut facts: Vec<String> = Vec::new();
        if let Some(p) = book.publisher.as_deref().filter(|s| !s.is_empty()) {
            facts.push(p.to_string());
        }
        if let Some(y) = book.year.as_deref().filter(|s| !s.is_empty()) {
            facts.push(y.to_string());
        }
        if let Some(l) = book.language.as_deref().filter(|s| !s.is_empty()) {
            facts.push(l.to_string());
        }
        if let Some(p) = book.pages.as_deref().filter(|s| !s.is_empty() && *s != "0") {
            facts.push(format!("{p} pages"));
        }
        facts.push(book.size_human());

        let mut lines = vec![
            Line::from(Span::styled(
                book.title.clone(),
                theme::text().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                book.authors_or_unknown().to_string(),
                theme::muted(),
            )),
            Line::from(vec![
                Span::styled("md5 ", theme::faint()),
                Span::styled(book.md5.clone(), theme::faint()),
            ]),
        ];
        let facts = facts.join("  ·  ");
        if !facts.trim().is_empty() {
            lines.insert(2, Line::from(Span::styled(facts, theme::muted())));
        }

        // A blank line, then the most urgent thing about the selection, where
        // the eye lands: an error, else this file's download state.
        lines.push(Line::from(""));
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::new().fg(theme::DANGER),
            )));
        } else {
            match self.jobs.get(&book.md5) {
                Some(Job::Saved(name)) => lines.push(Line::from(Span::styled(
                    format!("✓ saved as {name} — MD5 verified"),
                    Style::new().fg(theme::SUCCESS),
                ))),
                Some(Job::Failed(e)) => lines.push(Line::from(Span::styled(
                    format!("✗ {e}"),
                    Style::new().fg(theme::DANGER),
                ))),
                Some(Job::Running { done, total }) => lines.push(Line::from(Span::styled(
                    progress_bar(*done, *total, 28),
                    Style::new().fg(theme::ACCENT),
                ))),
                Some(Job::Queued) => {
                    lines.push(Line::from(Span::styled("queued", theme::faint())))
                }
                None => lines.push(Line::from(vec![
                    Span::styled("⏎ download → ", theme::accent()),
                    Span::styled(self.settings.dest_dir.display().to_string(), theme::faint()),
                ])),
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }

    /// The tall right-hand panel of the wide layout: the cover at something
    /// like poster size, then the record beneath it as labelled rows — a
    /// jacket flap rather than a footnote.
    fn render_side_details(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::FAINT));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 10 || inner.height < 6 {
            return;
        }

        let Some(book) = self.selected().cloned() else {
            let hint = self
                .error
                .clone()
                .unwrap_or_else(|| "no selection".to_string());
            let style = if self.error.is_some() {
                Style::new().fg(theme::DANGER)
            } else {
                theme::faint()
            };
            frame.render_widget(
                Paragraph::new(hint)
                    .style(style)
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: true }),
                inner.inner(Margin {
                    horizontal: 1,
                    vertical: inner.height / 2,
                }),
            );
            return;
        };

        let text_top = self.place_cover(frame, inner, &book.md5);

        let body = Rect {
            x: inner.x + 1,
            y: text_top,
            width: inner.width.saturating_sub(2),
            height: inner.bottom().saturating_sub(text_top),
        };
        if body.height == 0 {
            return;
        }

        let fact = |label: &str, value: Span<'static>| {
            Line::from(vec![
                Span::styled(format!("{label:<10}"), theme::faint()),
                value,
            ])
        };
        let mut lines = vec![
            Line::from(Span::styled(
                book.title.clone(),
                theme::text().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                book.authors_or_unknown().to_string(),
                theme::muted(),
            )),
            Line::from(""),
        ];
        if let Some(p) = book.publisher.as_deref().filter(|s| !s.is_empty()) {
            lines.push(fact("publisher", Span::styled(p.to_string(), theme::muted())));
        }
        if let Some(y) = book.year.as_deref().filter(|s| !s.is_empty()) {
            lines.push(fact("year", Span::styled(y.to_string(), theme::muted())));
        }
        if let Some(l) = book.language.as_deref().filter(|s| !s.is_empty()) {
            lines.push(fact(
                "language",
                Span::styled(l.to_string(), Style::new().fg(theme::LANG)),
            ));
        }
        if let Some(p) = book.pages.as_deref().filter(|s| !s.is_empty() && *s != "0") {
            lines.push(fact("pages", Span::styled(p.to_string(), theme::muted())));
        }
        if !book.size_human().is_empty() {
            lines.push(fact("size", Span::styled(book.size_human(), theme::muted())));
        }
        lines.push(fact(
            "format",
            Span::styled(format!(" {} ", book.ext()), theme::format_chip(book.ext())),
        ));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(book.md5.clone(), theme::faint())));
        lines.push(Line::from(""));

        // The most urgent thing about the selection last, where the eye rests:
        // an error, else this file's download state.
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::new().fg(theme::DANGER),
            )));
        } else {
            match self.jobs.get(&book.md5) {
                Some(Job::Saved(name)) => lines.push(Line::from(Span::styled(
                    format!("✓ saved as {name}"),
                    Style::new().fg(theme::SUCCESS),
                ))),
                Some(Job::Failed(e)) => lines.push(Line::from(Span::styled(
                    format!("✗ {e}"),
                    Style::new().fg(theme::DANGER),
                ))),
                Some(Job::Running { done, total }) => lines.push(Line::from(Span::styled(
                    progress_bar(*done, *total, (body.width as usize).saturating_sub(16)),
                    Style::new().fg(theme::ACCENT),
                ))),
                Some(Job::Queued) => {
                    lines.push(Line::from(Span::styled("queued", theme::faint())))
                }
                None => lines.push(Line::from(vec![
                    Span::styled("⏎ download → ", theme::accent()),
                    Span::styled(self.settings.dest_dir.display().to_string(), theme::faint()),
                ])),
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), body);
    }

    /// Place the cover for `md5` across the top of `inner`, and return the row
    /// where the text beneath it should begin. Sized to the picture's own shape
    /// once that is known and to a 2:3 jacket until then, centred, and capped
    /// so the record below always keeps its ten lines. A book with no cover
    /// gets a quiet empty jacket so the panel does not jump when one arrives.
    fn place_cover(&mut self, frame: &mut Frame, inner: Rect, md5: &str) -> u16 {
        if self.protocol == Protocol::None {
            return inner.y;
        }
        let (art_w, art_h) = match self.covers.get(md5) {
            Some(Slot::Ready(art)) => art
                .pixels
                .as_ref()
                .map(|p| (p.width.max(1) as u32, p.height.max(1) as u32))
                .unwrap_or((2, 3)),
            _ => (2, 3),
        };
        let max_height = inner.height.saturating_sub(11).clamp(6, 21);
        let max_width = inner.width.saturating_sub(2);
        let width = max_width.min((max_height as u32 * 2 * art_w / art_h) as u16);
        let height = ((width as u32 * art_h).div_ceil(art_w * 2) as u16).min(max_height);
        let rect = Rect {
            x: inner.x + (inner.width - width) / 2,
            y: inner.y,
            width,
            height,
        };
        match self.covers.get(md5) {
            Some(Slot::Ready(_)) | Some(Slot::Looking) => self.render_cover(frame, rect),
            _ => render_cover_placeholder(frame, rect),
        }
        rect.bottom() + 1
    }

    /// Draw the cover, or reserve the space it is about to occupy.
    ///
    /// For the two pixel protocols this only records where the picture goes;
    /// the escape sequence that actually places it is written after the frame,
    /// because it lives outside ratatui's buffer. Half-block art, being text,
    /// is drawn here like anything else.
    fn render_cover(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.cover_area = Some(area);

        match self.covers.get(self.focused_md5().as_deref().unwrap_or("")) {
            Some(Slot::Looking) => {
                frame.render_widget(
                    Paragraph::new("…")
                        .style(theme::faint())
                        .alignment(Alignment::Center),
                    area,
                );
            }
            Some(Slot::Ready(art)) if !self.protocol.is_pixels() => {
                let Some(image) = &art.pixels else {
                    return;
                };
                let rows = graphics::half_blocks(image, area.width, area.height);
                let lines: Vec<Line> = rows
                    .iter()
                    .map(|row| {
                        Line::from(
                            row.iter()
                                .map(|(upper, lower)| {
                                    Span::styled(
                                        graphics::UPPER_HALF.to_string(),
                                        Style::new()
                                            .fg(Color::Rgb(upper.0, upper.1, upper.2))
                                            .bg(Color::Rgb(lower.0, lower.1, lower.2)),
                                    )
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect();
                frame.render_widget(Paragraph::new(lines), area);
            }
            // Pixels are placed after the draw; leave the cells blank so the
            // image shows through.
            _ => {}
        }
    }

    /// Every book downloaded so far, filtered by whatever is in the box.
    fn render_library(&mut self, frame: &mut Frame, area: Rect) {
        if self.shown.is_empty() {
            self.render_empty_library(frame, area);
            return;
        }
        let now = history::now();

        let header = Row::new([
            Cell::from(""),
            Cell::from("TITLE"),
            Cell::from("AUTHOR"),
            Cell::from("ADDED"),
            Cell::from("SIZE"),
            Cell::from("FMT"),
        ])
        .style(theme::header())
        .height(1);

        let selected = self.library_table.selected();
        let rows: Vec<Row> = self
            .shown
            .iter()
            .enumerate()
            .filter_map(|(row, &i)| self.library.get(i).map(|e| (row, e)))
            .map(|(row, entry)| {
                let present = entry.present();
                let here = Some(row) == selected;
                // A present book leads with a small dot; a missing one with a
                // warning, and everything about its row reads as unavailable.
                let (marker, marker_style) = if present {
                    ("•", Style::new().fg(theme::SUCCESS))
                } else {
                    ("!", Style::new().fg(theme::DANGER))
                };
                let title_style = if !present {
                    theme::faint()
                } else if here {
                    theme::text().add_modifier(Modifier::BOLD)
                } else {
                    theme::text()
                };
                let title = if present {
                    entry.title.clone()
                } else {
                    format!("{} (missing)", entry.title)
                };

                let chip = if !present {
                    theme::faint()
                } else if here {
                    Style::new()
                        .fg(theme::format_color(entry.ext()))
                        .add_modifier(Modifier::BOLD)
                } else {
                    theme::format_chip(entry.ext())
                };

                Row::new([
                    Cell::from(marker).style(marker_style),
                    Cell::from(title).style(title_style),
                    Cell::from(entry.first_author()).style(theme::muted()),
                    Cell::from(history::when(entry.at, now)).style(theme::muted()),
                    Cell::from(human_bytes(entry.size)).style(theme::faint()),
                    Cell::from(format!(" {} ", entry.ext())).style(chip),
                ])
                .style(Style::new().bg(if row % 2 == 1 {
                    theme::BG_ALT
                } else {
                    theme::BG
                }))
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Fill(5),
                Constraint::Fill(2),
                Constraint::Length(12),
                Constraint::Length(9),
                Constraint::Length(6),
            ],
        )
        .header(header)
        .column_spacing(1)
        .row_highlight_style(theme::selected_row())
        .highlight_symbol(Span::styled(
            theme::CURSOR,
            theme::accent().add_modifier(Modifier::BOLD),
        ));

        frame.render_stateful_widget(table, area, &mut self.library_table);
    }

    /// An empty library and an over-narrow filter are different problems and
    /// deserve different words; both are centred so the tab looks intended
    /// rather than broken.
    fn render_empty_library(&self, frame: &mut Frame, area: Rect) {
        let lines: Vec<Line> = if self.library.is_empty() {
            vec![
                Line::from(Span::styled(
                    "Your library is empty",
                    theme::text().add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Books you download land here and stay between sessions.",
                    theme::muted(),
                )),
                Line::from(vec![
                    Span::styled("Press ", theme::faint()),
                    Span::styled("Tab", theme::accent()),
                    Span::styled(" to go and find one.", theme::faint()),
                ]),
            ]
        } else {
            vec![
                Line::from(Span::styled(
                    format!("No book matches “{}”", self.filter.trim()),
                    theme::muted(),
                )),
                Line::from(""),
                Line::from(vec![
                    Span::styled("^u", theme::accent()),
                    Span::styled(" clears the filter", theme::faint()),
                ]),
            ]
        };
        let block = area.inner(Margin {
            horizontal: 2,
            vertical: area.height.saturating_sub(lines.len() as u16) / 2,
        });
        frame.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            block,
        );
    }

    /// Where the highlighted book went, and whether it is still there.
    fn render_library_details(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::FAINT));
        let whole = block.inner(area);
        frame.render_widget(block, area);

        // A cover column on the left when the book has one to show, exactly as
        // the search detail strip does — the library reads its cover out of the
        // file rather than off a mirror, but the layout is the same.
        self.cover_area = None;
        let inner = if self.cover_visible() && whole.width > 30 {
            let width = ((whole.height as u32 * 2 * 2 / 3) as u16).clamp(6, 16);
            let picture = Rect {
                x: whole.x,
                y: whole.y,
                width,
                height: whole.height,
            };
            self.render_cover(frame, picture);
            Rect {
                x: whole.x + width + 2,
                y: whole.y,
                width: whole.width.saturating_sub(width + 2),
                height: whole.height,
            }
        } else {
            whole
        };

        let Some(entry) = self.selected_entry() else {
            let hint = self
                .error
                .clone()
                .unwrap_or_else(|| "⏎ opens a book in your reader".to_string());
            frame.render_widget(
                Paragraph::new(hint)
                    .style(if self.error.is_some() {
                        Style::new().fg(theme::DANGER)
                    } else {
                        theme::faint()
                    })
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        };

        let verified = if entry.verified {
            Span::styled("✓ MD5 verified", Style::new().fg(theme::SUCCESS))
        } else {
            Span::styled("unverified", theme::faint())
        };
        let mut lines = vec![
            Line::from(Span::styled(
                entry.title.clone(),
                theme::text().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                entry.authors.clone().unwrap_or_default(),
                theme::muted(),
            )),
            Line::from(Span::styled(entry.path.display().to_string(), theme::faint())),
            Line::from(vec![
                Span::styled(format!(" {} ", entry.ext()), theme::format_chip(entry.ext())),
                Span::styled(
                    format!(
                        "  {}  ·  {}  ·  ",
                        human_bytes(entry.size),
                        history::timestamp(entry.at),
                    ),
                    theme::muted(),
                ),
                verified,
            ]),
            Line::from(""),
        ];

        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::new().fg(theme::DANGER),
            )));
        } else if !entry.present() {
            lines.push(Line::from(Span::styled(
                "⚠ the file is no longer there — press d to forget it",
                Style::new().fg(theme::DANGER),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled("⏎ open", theme::accent()),
                Span::styled("   f reveal in file manager", theme::faint()),
            ]));
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }

    /// The wide library panel: the book's own cover near poster size, then
    /// where the file is and what is known about it — the same jacket-flap
    /// layout the search tab uses, sourced from the file instead of a mirror.
    fn render_library_side_details(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::FAINT));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 10 || inner.height < 6 {
            return;
        }

        let Some(entry) = self.selected_entry().cloned() else {
            return;
        };

        let text_top = self.place_cover(frame, inner, &entry.md5);
        let body = Rect {
            x: inner.x + 1,
            y: text_top,
            width: inner.width.saturating_sub(2),
            height: inner.bottom().saturating_sub(text_top),
        };
        if body.height == 0 {
            return;
        }

        let fact = |label: &str, value: Span<'static>| {
            Line::from(vec![
                Span::styled(format!("{label:<9}"), theme::faint()),
                value,
            ])
        };
        let mut lines = vec![
            Line::from(Span::styled(
                entry.title.clone(),
                theme::text().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                entry.authors.clone().unwrap_or_default(),
                theme::muted(),
            )),
            Line::from(""),
            fact(
                "format",
                Span::styled(format!(" {} ", entry.ext()), theme::format_chip(entry.ext())),
            ),
            fact("size", Span::styled(human_bytes(entry.size), theme::muted())),
            fact(
                "added",
                Span::styled(history::timestamp(entry.at), theme::muted()),
            ),
            fact(
                "checksum",
                if entry.verified {
                    Span::styled("✓ MD5 verified", Style::new().fg(theme::SUCCESS))
                } else {
                    Span::styled("unverified", theme::faint())
                },
            ),
            Line::from(""),
            Line::from(Span::styled("saved to", theme::faint())),
            Line::from(Span::styled(entry.path.display().to_string(), theme::faint())),
            Line::from(""),
        ];

        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::new().fg(theme::DANGER),
            )));
        } else if !entry.present() {
            lines.push(Line::from(Span::styled(
                "⚠ the file is no longer there — d to forget it",
                Style::new().fg(theme::DANGER),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled("⏎ open", theme::accent()),
                Span::styled("    f reveal", theme::faint()),
            ]));
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), body);
    }

    fn render_hints(&self, frame: &mut Frame, area: Rect) {
        // Each hint is a (key, label) pair; the key rides in the accent, the
        // label sits faint beneath it, so the bar reads as a legend rather than
        // a grey sentence. `tab` leads every line — it is the one key that gets
        // you out of wherever you are.
        let hints: &[(&str, &str)] = match (self.mode, self.tab) {
            (Mode::Help, _) => &[("", "any key to close")],
            (Mode::Editing, Tab::Search) => &[
                ("tab", "library"),
                ("⏎", "search"),
                ("esc", "done"),
                ("^u", "clear"),
                ("", "try author: title: ext:"),
            ],
            (Mode::Editing, Tab::Library) => &[
                ("tab", "search"),
                ("esc", "done"),
                ("^u", "clear"),
                ("", "filtering as you type"),
            ],
            (Mode::Browsing, Tab::Search) => &[
                ("tab", "library"),
                ("↑↓", "move"),
                ("space", "mark"),
                ("⏎", "download"),
                ("e", "format"),
                ("l", "language"),
                ("/", "search"),
                ("?", "help"),
                ("q", "quit"),
            ],
            (Mode::Browsing, Tab::Library) => &[
                ("tab", "search"),
                ("↑↓", "move"),
                ("⏎", "open"),
                ("f", "reveal"),
                ("/", "filter"),
                ("d", "forget"),
                ("?", "help"),
                ("q", "quit"),
            ],
        };

        let mut spans = vec![Span::raw(" ")];
        for (key, label) in hints {
            if !key.is_empty() {
                spans.push(Span::styled(
                    (*key).to_string(),
                    theme::accent().add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(format!(" {label}"), theme::faint()));
            } else {
                spans.push(Span::styled((*label).to_string(), theme::faint()));
            }
            spans.push(Span::styled("   ", theme::faint()));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_help(&self, frame: &mut Frame) {
        // Grouped by tab, because which keys apply depends on where you are.
        let sections: [(&str, &[(&str, &str)]); 3] = [
            (
                "anywhere",
                &[
                    ("tab", "switch between Search and Library"),
                    ("1  2", "go straight to a tab"),
                    ("/  i", "type in the box"),
                    ("↑ ↓  k j", "move the selection"),
                    ("PgUp PgDn", "move ten at a time"),
                    ("g  G", "jump to first / last"),
                    ("?", "this help"),
                    ("q  esc", "quit"),
                    ("^c", "quit from anywhere"),
                ],
            ),
            (
                "search tab",
                &[
                    ("⏎", "run the search, or download the selection"),
                    ("e  E", "cycle the format filter, e.g. epub only"),
                    ("l  L", "cycle the language filter"),
                    ("x", "clear both filters"),
                    ("space", "mark a result for batch download"),
                    ("a", "mark or unmark everything showing"),
                    ("o  f", "open a downloaded result / reveal it"),
                    ("r", "run the search again"),
                    ("m", "re-probe mirrors and pick a new one"),
                ],
            ),
            (
                "library tab",
                &[
                    ("⏎  o", "open the book in your reader"),
                    ("f", "show the file in the file manager"),
                    ("/", "filter by title, author or filename"),
                    ("d", "forget an entry (the file stays)"),
                    ("r", "re-read the library from disk"),
                ],
            ),
        ];

        // A section header sits in amber above its keys, with a blank line
        // before it (except the first) so the groups breathe.
        let section_title = |title: &str, first: bool| {
            let text = if first {
                title.to_uppercase()
            } else {
                format!("\n{}", title.to_uppercase())
            };
            Line::from(Span::styled(
                text,
                Style::new()
                    .fg(theme::AMBER)
                    .add_modifier(Modifier::BOLD),
            ))
        };
        let key_row = |keys: &str, what: &str| {
            Line::from(vec![
                Span::styled(format!("  {keys:<11}"), theme::accent()),
                Span::styled(what.to_string(), theme::muted()),
            ])
        };

        let lines: Vec<Line> = sections
            .iter()
            .enumerate()
            .flat_map(|(s, (title, keys))| {
                std::iter::once(section_title(title, s == 0))
                    .chain(keys.iter().map(|(k, what)| key_row(k, what)))
            })
            // The search box takes tags as well as words, and this is the only
            // place that says so.
            .chain(std::iter::once(section_title("search tags", false)))
            .chain(query::TAGS.iter().map(|(tag, what)| key_row(tag, what)))
            .collect();

        let width = 60u16.min(frame.area().width.saturating_sub(4));
        let height = (lines.len() as u16 + 2).min(frame.area().height.saturating_sub(2));
        let area = centered(frame.area(), width, height);

        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::ACCENT))
            // One step off the canvas, so the overlay reads as a raised
            // surface rather than a hole cut in the screen.
            .style(Style::new().bg(theme::BG_ALT))
            .title(Span::styled(
                " clibgen · keys ",
                Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Where a cover would go when the book has none: a quiet empty jacket, so the
/// panel keeps its shape instead of jumping when a picture does arrive.
fn render_cover_placeholder(frame: &mut Frame, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::FAINT));
    frame.render_widget(&block, area);
    if area.height >= 3 {
        let middle = Rect {
            x: area.x,
            y: area.y + area.height / 2,
            width: area.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new("no cover")
                .style(theme::faint())
                .alignment(Alignment::Center),
            middle,
        );
    }
}

/// A centred rectangle of the given size.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn job_label(job: Option<&Job>) -> String {
    match job {
        Some(Job::Queued) => "queued".into(),
        Some(Job::Running { done, total }) => match total {
            Some(t) if *t > 0 => format!("{:>3}%", (*done as f64 / *t as f64 * 100.0) as u32),
            _ => human_bytes(*done),
        },
        Some(Job::Saved(_)) => "saved".into(),
        Some(Job::Failed(_)) => "failed".into(),
        None => String::new(),
    }
}

fn job_style(job: Option<&Job>) -> Style {
    match job {
        Some(Job::Saved(_)) => Style::new().fg(theme::SUCCESS),
        Some(Job::Failed(_)) => Style::new().fg(theme::DANGER),
        Some(_) => Style::new().fg(theme::ACCENT),
        None => Style::new(),
    }
}

/// A text progress bar for the details pane.
fn progress_bar(done: u64, total: Option<u64>, width: usize) -> String {
    match total {
        Some(total) if total > 0 => {
            let fraction = (done as f64 / total as f64).clamp(0.0, 1.0);
            let filled = (fraction * width as f64).round() as usize;
            format!(
                "{}{}  {}/{}",
                "█".repeat(filled),
                "░".repeat(width - filled),
                human_bytes(done),
                human_bytes(total)
            )
        }
        _ => format!("downloading… {}", human_bytes(done)),
    }
}

/// Error chains get long; the pane has one line for them.
fn first_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or(message).trim();
    if line.len() > 200 {
        format!("{}…", &line[..line.floor_char_boundary(200)])
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let (tx, _rx) = mpsc::channel();
        App {
            settings: Settings {
                policy: Default::default(),
                dest_dir: std::path::PathBuf::from("/tmp"),
                max_bytes: 1 << 30,
                verify: true,
                force: false,
                resume: true,
                mirrors: Vec::new(),
                refresh_mirrors: false,
                quiet: false,
                json: false,
                output: None,
                reader: None,
                history: false,
                covers: false,
            },
            tab: Tab::Search,
            mode: Mode::Editing,
            query: String::new(),
            filter: String::new(),
            caret: 0,
            results: Vec::new(),
            visible: Vec::new(),
            fmt_filter: None,
            lang_filter: None,
            table: TableState::default(),
            marked: Vec::new(),
            jobs: HashMap::new(),
            mirrors: Vec::new(),
            mirror_label: String::new(),
            status: String::new(),
            error: None,
            busy: false,
            downloading: false,
            pending_search: false,
            quit: false,
            tx,
            library: Vec::new(),
            shown: Vec::new(),
            library_table: TableState::default(),
            // Tests draw to an off-screen buffer, so nothing may try to write
            // an escape sequence to a terminal that is not there.
            protocol: Protocol::None,
            covers: HashMap::new(),
            cover_due: None,
            painted: None,
            cover_area: None,
        }
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn books(n: usize) -> Vec<Book> {
        (0..n)
            .map(|i| Book {
                md5: format!("{i:032x}"),
                title: format!("Book {i}"),
                ..Default::default()
            })
            .collect()
    }

    /// Put results in place the way `Ev::Results` would, so the visible-index
    /// mapping is always consistent with what the table believes.
    fn install(a: &mut App, list: Vec<Book>) {
        a.marked = vec![false; list.len()];
        a.results = list;
        a.table.select(None);
        a.refine();
    }

    #[test]
    fn typing_builds_the_query() {
        let mut a = app();
        for c in "dune".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        assert_eq!(a.query, "dune");
        assert_eq!(a.caret, 4);
    }

    #[test]
    fn editing_respects_the_caret() {
        let mut a = app();
        for c in "dune".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        press(&mut a, KeyCode::Home);
        press(&mut a, KeyCode::Char('_'));
        assert_eq!(a.query, "_dune");
        press(&mut a, KeyCode::End);
        press(&mut a, KeyCode::Backspace);
        assert_eq!(a.query, "_dun");
    }

    /// Editing is by character, so a multi-byte query must not panic or split.
    #[test]
    fn editing_handles_multibyte_text() {
        let mut a = app();
        for c in "naïve 日本".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        assert_eq!(a.query, "naïve 日本");
        press(&mut a, KeyCode::Backspace);
        assert_eq!(a.query, "naïve 日");
        press(&mut a, KeyCode::Home);
        press(&mut a, KeyCode::Delete);
        assert_eq!(a.query, "aïve 日");
    }

    #[test]
    fn ctrl_w_deletes_a_word() {
        let mut a = app();
        for c in "the rust book".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        a.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(a.query, "the rust ");
    }

    #[test]
    fn ctrl_u_clears_the_line() {
        let mut a = app();
        for c in "dune".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(a.query, "");
        assert_eq!(a.caret, 0);
    }

    #[test]
    fn navigation_stays_in_bounds() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, books(3));

        press(&mut a, KeyCode::Up); // already at the top
        assert_eq!(a.table.selected(), Some(0));
        for _ in 0..10 {
            press(&mut a, KeyCode::Down);
        }
        assert_eq!(a.table.selected(), Some(2), "must not run off the end");
        press(&mut a, KeyCode::Home);
        assert_eq!(a.table.selected(), Some(0));
        press(&mut a, KeyCode::End);
        assert_eq!(a.table.selected(), Some(2));
    }

    #[test]
    fn navigation_on_an_empty_list_is_harmless() {
        let mut a = app();
        a.mode = Mode::Browsing;
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Up);
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.table.selected(), None);
        assert!(!a.quit);
    }

    #[test]
    fn space_marks_and_advances() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, books(3));

        press(&mut a, KeyCode::Char(' '));
        assert_eq!(a.marked, [true, false, false]);
        assert_eq!(
            a.table.selected(),
            Some(1),
            "space moves on for fast marking"
        );
    }

    #[test]
    fn a_toggles_every_mark() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, books(3));
        press(&mut a, KeyCode::Char('a'));
        assert_eq!(a.marked, [true, true, true]);
        press(&mut a, KeyCode::Char('a'));
        assert_eq!(a.marked, [false, false, false]);
    }

    #[test]
    fn mode_switching_and_quitting() {
        let mut a = app();
        a.mode = Mode::Browsing;
        press(&mut a, KeyCode::Char('/'));
        assert_eq!(a.mode, Mode::Editing);
        press(&mut a, KeyCode::Esc);
        assert_eq!(a.mode, Mode::Browsing);

        press(&mut a, KeyCode::Char('?'));
        assert_eq!(a.mode, Mode::Help);
        press(&mut a, KeyCode::Char('x'));
        assert_eq!(a.mode, Mode::Browsing, "any key closes help");

        press(&mut a, KeyCode::Char('q'));
        assert!(a.quit);
    }

    #[test]
    fn ctrl_c_quits_from_editing_too() {
        let mut a = app();
        a.mode = Mode::Editing;
        a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(a.quit);
    }

    /// Letters must reach the query rather than triggering browse shortcuts.
    #[test]
    fn shortcut_letters_are_literal_while_editing() {
        let mut a = app();
        a.mode = Mode::Editing;
        for c in "qajm".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        assert_eq!(a.query, "qajm");
        assert!(!a.quit);
    }

    #[test]
    fn search_without_a_mirror_is_deferred_not_lost() {
        let mut a = app();
        a.query = "dune".into();
        a.spawn_search();
        assert!(
            a.pending_search,
            "the search should run once a mirror appears"
        );
    }

    #[test]
    fn empty_search_is_reported_not_sent() {
        let mut a = app();
        a.query = "   ".into();
        a.spawn_search();
        assert!(a.error.is_some());
        assert!(!a.pending_search);
    }

    #[test]
    fn results_reset_selection_and_marks() {
        let mut a = app();
        a.marked = vec![true, true];
        a.handle(Ev::Results(Ok((books(2), "libgen.li".into()))));
        assert_eq!(a.marked, [false, false]);
        assert_eq!(a.table.selected(), Some(0));
        assert_eq!(a.mirror_label, "libgen.li");
        assert_eq!(a.mode, Mode::Browsing);
    }

    #[test]
    fn empty_results_leave_nothing_selected() {
        let mut a = app();
        a.handle(Ev::Results(Ok((Vec::new(), "libgen.li".into()))));
        assert_eq!(a.table.selected(), None);
        assert!(a.status.contains("nothing found"));
    }

    #[test]
    fn finishing_the_last_job_clears_the_busy_flag() {
        let mut a = app();
        a.downloading = true;
        a.jobs.insert(
            "abc".into(),
            Job::Running {
                done: 1,
                total: None,
            },
        );
        a.handle(Ev::Finished {
            md5: "abc".into(),
            outcome: Ok("book.epub".into()),
        });
        assert!(!a.downloading);
        assert!(matches!(a.jobs.get("abc"), Some(Job::Saved(_))));
    }

    #[test]
    fn a_failure_is_surfaced_rather_than_swallowed() {
        let mut a = app();
        a.handle(Ev::Finished {
            md5: "abc".into(),
            outcome: Err("integrity check failed\nsecond line".into()),
        });
        assert_eq!(a.error.as_deref(), Some("integrity check failed"));
        assert!(matches!(a.jobs.get("abc"), Some(Job::Failed(_))));
    }

    /// Draw the app to an off-screen buffer and return it as text.
    fn draw(a: &mut App, width: u16, height: u16) -> String {
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| a.render(f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renders_results_and_details() {
        let mut a = app();
        a.mode = Mode::Browsing;
        a.mirror_label = "libgen.li".into();
        install(
            &mut a,
            vec![Book {
                md5: "1b9159991f7fb1b3910c0be9ebf7e595".into(),
                title: "The Rust Programming Language".into(),
                authors: Some("Klabnik, Steve;Nichols, Carol".into()),
                year: Some("2019".into()),
                language: Some("English".into()),
                extension: Some("epub".into()),
                size_bytes: Some(3 * 1024 * 1024),
                ..Default::default()
            }],
        );

        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("SEARCH"), "tab bar missing: {screen}");
        assert!(screen.contains("libgen.li"), "{screen}");
        assert!(screen.contains("The Rust Programming Language"), "{screen}");
        assert!(screen.contains("Klabnik"), "{screen}");
        assert!(screen.contains("epub"), "{screen}");
        // The details pane shows the hash of the highlighted row.
        assert!(
            screen.contains("1b9159991f7fb1b3910c0be9ebf7e595"),
            "{screen}"
        );
        assert!(screen.contains("download"), "hints missing: {screen}");
    }

    #[test]
    fn renders_at_awkward_sizes_without_panicking() {
        // A layout that assumes it has room is a crash waiting for a small
        // terminal, so exercise the extremes.
        for (w, h) in [(20u16, 8u16), (40, 10), (80, 24), (200, 60), (250, 12)] {
            let mut a = app();
            a.mode = Mode::Browsing;
            install(&mut a, books(30));
            a.table.select(Some(29));
            let _ = draw(&mut a, w, h);

            a.mode = Mode::Help;
            let _ = draw(&mut a, w, h);
        }
    }

    #[test]
    fn renders_the_empty_and_error_states() {
        let mut a = app();
        a.status = "finding a working mirror".into();
        a.busy = true;
        let screen = draw(&mut a, 80, 20);
        assert!(screen.contains("finding a working mirror"), "{screen}");

        a.error = Some("every mirror failed".into());
        let screen = draw(&mut a, 80, 20);
        assert!(screen.contains("every mirror failed"), "{screen}");
    }

    #[test]
    fn help_overlay_lists_the_keys() {
        let mut a = app();
        a.mode = Mode::Help;
        let screen = draw(&mut a, 90, 24);
        assert!(screen.contains("keys"), "{screen}");
        assert!(screen.contains("download"), "{screen}");
        assert!(screen.contains("quit"), "{screen}");
    }

    #[test]
    fn download_state_is_visible_in_the_row_and_detail() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, books(1));
        a.jobs.insert(
            a.results[0].md5.clone(),
            Job::Running {
                done: 512 * 1024,
                total: Some(1024 * 1024),
            },
        );
        let screen = draw(&mut a, 100, 20);
        assert!(screen.contains("50%"), "row progress missing: {screen}");
        assert!(screen.contains("█"), "detail bar missing: {screen}");
    }

    #[test]
    fn progress_bar_renders_within_its_width() {
        let bar = progress_bar(50, Some(100), 10);
        assert_eq!(bar.chars().filter(|c| *c == '█').count(), 5);
        assert_eq!(bar.chars().filter(|c| *c == '░').count(), 5);
        // Unknown totals must not divide by zero or panic.
        assert!(progress_bar(50, None, 10).contains("downloading"));
        assert!(progress_bar(50, Some(0), 10).contains("downloading"));
    }

    fn entries(n: usize) -> Vec<history::Entry> {
        (0..n)
            .map(|i| history::Entry {
                at: history::now() - (i as u64 * 3600),
                md5: format!("{i:032x}"),
                path: std::path::PathBuf::from(format!("/books/Book {i}.epub")),
                size: 1024 * 1024,
                verified: true,
                title: format!("Book {i}"),
                authors: Some("Frank Herbert".into()),
                extension: Some("epub".into()),
            })
            .collect()
    }

    /// Put the app on the library tab with a fixed set of books, bypassing the
    /// disk read that `reload_library` would do.
    fn with_library(a: &mut App, entries: Vec<history::Entry>) {
        a.tab = Tab::Library;
        a.mode = Mode::Browsing;
        a.library = entries;
        a.refilter();
    }

    #[test]
    fn moving_in_the_library_arms_a_cover_lookup() {
        let mut a = app();
        a.protocol = Protocol::Blocks;
        with_library(&mut a, entries(3));
        a.cover_due = None;
        press(&mut a, KeyCode::Down);
        assert!(
            a.cover_due.is_some(),
            "a settled library selection is worth a cover, just like search"
        );
    }

    #[test]
    fn a_library_book_shows_its_cover_in_both_layouts() {
        let mut a = app();
        a.protocol = Protocol::Blocks;
        with_library(&mut a, entries(3));
        a.library_table.select(Some(0));

        // Stand in for a cover pulled out of the file on disk.
        let image = crate::jpeg::decode(include_bytes!("../tests/fixtures/cover.jpg")).unwrap();
        let md5 = a.selected_entry().unwrap().md5.clone();
        a.covers.insert(
            md5,
            Slot::Ready(Box::new(Art {
                encoded: Vec::new(),
                pixels: Some(image),
            })),
        );

        // Wide: the poster panel, with the cover and the file facts beside it.
        let wide = draw(&mut a, 130, 40);
        assert!(
            wide.contains(graphics::UPPER_HALF),
            "the library cover should draw as half blocks: {wide}"
        );
        assert!(wide.contains("Book 0"), "{wide}");
        assert!(wide.contains("saved to"), "{wide}");

        // Narrow: the bottom strip grows a cover column too.
        let narrow = draw(&mut a, 90, 24);
        assert!(
            narrow.contains(graphics::UPPER_HALF),
            "the narrow strip should still show the cover: {narrow}"
        );
    }

    #[test]
    fn a_library_cover_with_no_local_source_falls_back_to_a_mirror() {
        // The whole point of the fallback: a book whose file carries no jacket
        // and that was never searched must still get a cover off a mirror, the
        // same way the Search tab does. With no mirror there is nothing to ask,
        // so it comes back empty rather than erroring.
        let a = app();
        let entry = &entries(1)[0];
        assert!(
            App::fetch_library_cover(&a.settings, &[], entry)
                .unwrap()
                .is_none(),
            "no mirror, no cover, but no error either"
        );
    }

    #[test]
    fn a_mirror_arriving_revives_library_covers_that_had_none_to_ask() {
        // Open the library before the mirror pool resolves: a book with no
        // local cover comes up blank because there was nobody to ask. When the
        // mirror lands, that blank must be dropped and the selection re-armed so
        // the cover is looked up for real.
        let mut a = app();
        a.protocol = Protocol::Kitty;
        with_library(&mut a, entries(1));
        a.library_table.select(Some(0));
        let md5 = a.selected_entry().unwrap().md5.clone();

        // Stands in for the empty result of a mirror-less first look.
        a.covers.insert(md5.clone(), Slot::Nothing);
        a.cover_due = None;

        a.handle(Ev::Mirrors(Ok(vec!["https://libgen.li".parse().unwrap()])));

        assert!(
            !a.covers.contains_key(&md5),
            "the blank cover should be dropped so it can be looked up again"
        );
        assert!(
            a.cover_due.is_some(),
            "the selection should be re-armed to fetch now a mirror exists"
        );
    }

    #[test]
    fn the_library_reuses_a_cover_across_tabs_by_md5() {
        // A cover fetched while searching is keyed by MD5; the same book in the
        // library must find it under the same key rather than looking again.
        let mut a = app();
        a.protocol = Protocol::Blocks;
        let md5 = format!("{:032x}", 0);
        a.covers.insert(md5.clone(), Slot::Nothing);
        with_library(&mut a, entries(1));
        a.library_table.select(Some(0));
        assert_eq!(a.selected_entry().unwrap().md5, md5);
        // Already known, so a poll starts no new lookup and leaves it be.
        a.cover_due = Some(Instant::now());
        a.poll_cover();
        assert!(matches!(a.covers.get(&md5), Some(Slot::Nothing)));
    }

    #[test]
    fn the_library_tab_lists_past_downloads() {
        let mut a = app();
        with_library(&mut a, entries(3));

        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("Book 0"), "{screen}");
        assert!(screen.contains("Herbert"), "{screen}");
        assert!(screen.contains("/books/Book 0.epub"), "{screen}");
        // These files do not exist, and the view says so rather than pretending.
        assert!(screen.contains("missing"), "{screen}");
        assert!(screen.contains("forget"), "hints missing: {screen}");
    }

    #[test]
    fn the_library_tab_navigates() {
        let mut a = app();
        with_library(&mut a, entries(3));

        press(&mut a, KeyCode::Up); // already at the top
        assert_eq!(a.library_table.selected(), Some(0));
        for _ in 0..10 {
            press(&mut a, KeyCode::Down);
        }
        assert_eq!(
            a.library_table.selected(),
            Some(2),
            "must not run off the end"
        );
        press(&mut a, KeyCode::Home);
        assert_eq!(a.library_table.selected(), Some(0));
    }

    /// The bug that started all this: the library was reachable only from the
    /// results list, so a fresh session — which opens in the editor with no
    /// results — had no way to reach it at all. Tab must always work.
    #[test]
    fn the_library_is_reachable_before_any_search() {
        let mut a = app();
        assert_eq!(a.tab, Tab::Search);
        assert_eq!(a.mode, Mode::Editing, "a fresh session starts typing");

        // Tab reaches the library even mid-edit, with nothing searched yet.
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.tab, Tab::Library);
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.tab, Tab::Search);

        // And the number keys jump straight there when not typing.
        a.mode = Mode::Browsing;
        press(&mut a, KeyCode::Char('2'));
        assert_eq!(a.tab, Tab::Library);
        press(&mut a, KeyCode::Char('1'));
        assert_eq!(a.tab, Tab::Search);
    }

    #[test]
    fn q_quits_from_the_library_tab_too() {
        let mut a = app();
        with_library(&mut a, entries(3));
        press(&mut a, KeyCode::Char('q'));
        assert!(a.quit, "the library is a peer of search, not a detour");
    }

    #[test]
    fn the_filter_narrows_the_library_as_you_type() {
        let mut a = app();
        with_library(&mut a, entries(3));
        // entries() titles are "Book 0", "Book 1", "Book 2"; filter to one.
        press(&mut a, KeyCode::Char('/'));
        assert_eq!(a.mode, Mode::Editing);
        for c in "Book 1".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        assert_eq!(a.shown.len(), 1, "only one title matches");
        assert_eq!(a.selected_entry().map(|e| e.title.clone()).as_deref(), Some("Book 1"));

        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("Book 1"), "{screen}");
        assert!(!screen.contains("Book 0"), "filtered out: {screen}");

        // Clearing the box brings everyone back.
        a.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(a.shown.len(), 3);
    }

    #[test]
    fn a_filter_that_matches_nothing_says_so() {
        let mut a = app();
        with_library(&mut a, entries(3));
        a.filter = "nonesuch".into();
        a.refilter();
        assert!(a.shown.is_empty());
        assert!(a.selected_entry().is_none());
        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("No book matches"), "{screen}");
    }

    #[test]
    fn an_empty_library_tab_is_harmless_and_explains_itself() {
        let mut a = app();
        a.tab = Tab::Library;
        a.mode = Mode::Browsing;
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Enter);
        press(&mut a, KeyCode::Char('d'));
        assert_eq!(a.library_table.selected(), None);
        let screen = draw(&mut a, 80, 20);
        assert!(screen.contains("library is empty"), "{screen}");
        // It points the way out rather than leaving a dead end.
        assert!(screen.contains("Tab"), "{screen}");
    }

    #[test]
    fn the_tab_bar_shows_both_tabs_and_the_count() {
        let mut a = app();
        a.library = entries(4);
        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("SEARCH"), "{screen}");
        assert!(screen.contains("LIBRARY"), "{screen}");
        // The count rides along in the tab label so it is visible before you go.
        assert!(screen.contains("LIBRARY 4"), "{screen}");
    }

    #[test]
    fn opening_a_result_that_was_never_downloaded_says_so() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, books(1));
        press(&mut a, KeyCode::Char('o'));
        assert_eq!(
            a.error.as_deref(),
            Some("that one has not been downloaded yet")
        );
    }

    #[test]
    fn a_query_of_nothing_but_filters_is_refused() {
        let mut a = app();
        a.query = "ext:epub lang:english".into();
        a.mirrors = vec!["https://libgen.li".parse().unwrap()];
        a.spawn_search();
        assert!(a.error.is_some(), "there is nothing here to search for");
        assert!(!a.busy);
    }

    /// The half-block path is the one that renders through ratatui, so it is
    /// the one that can be checked on an off-screen buffer.
    #[test]
    fn a_cover_is_drawn_as_blocks_when_there_is_no_image_protocol() {
        let mut a = app();
        a.mode = Mode::Browsing;
        a.protocol = Protocol::Blocks;
        install(&mut a, books(1));

        let image = crate::jpeg::decode(include_bytes!("../tests/fixtures/cover.jpg")).unwrap();
        a.covers.insert(
            a.results[0].md5.clone(),
            Slot::Ready(Box::new(Art {
                encoded: Vec::new(),
                pixels: Some(image),
            })),
        );

        let screen = draw(&mut a, 100, 28);
        assert!(
            screen.contains(graphics::UPPER_HALF),
            "the cover should be drawn as half blocks: {screen}"
        );
        // The book's details are still there beside it.
        assert!(screen.contains("Book 0"), "{screen}");
    }

    /// The kitty and iTerm2 paths write outside ratatui's buffer, so what they
    /// would write is checked directly.
    #[test]
    fn a_pixel_cover_is_placed_once_and_not_repainted() {
        let mut a = app();
        a.mode = Mode::Browsing;
        a.protocol = Protocol::Kitty;
        install(&mut a, books(2));

        let image = crate::jpeg::decode(include_bytes!("../tests/fixtures/cover.jpg")).unwrap();
        a.covers.insert(
            a.results[0].md5.clone(),
            Slot::Ready(Box::new(Art {
                encoded: Vec::new(),
                pixels: Some(image),
            })),
        );

        // Nothing is placed until a frame has said where the space is.
        assert_eq!(a.cover_escape(), None);

        let _ = draw(&mut a, 100, 30);
        let escape = a.cover_escape().expect("a cover should be placed");
        assert!(escape.contains("\x1b["), "no cursor placement: {escape:?}");
        assert!(escape.contains("\x1b_Ga=T"), "no kitty image: {escape:?}");
        assert!(
            !escape.contains("a=d"),
            "nothing was on screen to delete yet"
        );

        // An unchanged screen must not be redrawn — the picture would flicker
        // on every keystroke and every 250 ms tick.
        let _ = draw(&mut a, 100, 30);
        assert_eq!(a.cover_escape(), None, "a settled cover is left alone");

        // Moving to a book with no cover takes the old one down.
        press(&mut a, KeyCode::Down);
        let _ = draw(&mut a, 100, 30);
        let escape = a.cover_escape().expect("the old cover must be removed");
        assert!(escape.contains("a=d"), "no delete: {escape:?}");
        assert!(!escape.contains("a=T"), "nothing to draw: {escape:?}");
    }

    #[test]
    fn a_cover_never_squeezes_out_the_results() {
        let mut a = app();
        a.mode = Mode::Browsing;
        a.protocol = Protocol::Blocks;
        install(&mut a, books(20));
        a.covers.insert(a.results[0].md5.clone(), Slot::Looking);

        for (w, h) in [(20u16, 8u16), (40, 10), (80, 24), (200, 60)] {
            let area = Rect {
                x: 0,
                y: 0,
                width: w,
                height: h,
            };
            let details = a.details_height(area);
            assert!(
                details <= h.saturating_sub(4).max(7),
                "{w}×{h} gave a {details}-row detail pane"
            );
            let _ = draw(&mut a, w, h);
        }
    }

    #[test]
    fn covers_are_only_looked_up_after_the_selection_settles() {
        let mut a = app();
        a.protocol = Protocol::Blocks;
        a.mode = Mode::Browsing;
        install(&mut a, books(3));

        press(&mut a, KeyCode::Down);
        assert!(a.cover_due.is_some(), "moving should arm the lookup");
        // Nothing is due yet, so nothing is started.
        a.poll_cover();
        assert!(a.covers.is_empty());

        // With no mirror there is nothing to ask, and it must not spin.
        a.cover_due = Some(Instant::now());
        a.poll_cover();
        assert!(a.covers.is_empty(), "a lookup needs a mirror first");
    }

    #[test]
    fn a_terminal_without_graphics_never_reserves_cover_space() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, books(1));
        a.covers.insert(
            a.results[0].md5.clone(),
            Slot::Ready(Box::new(Art {
                encoded: Vec::new(),
                pixels: None,
            })),
        );
        assert!(!a.cover_visible());
        press(&mut a, KeyCode::Down);
        assert!(a.cover_due.is_none(), "no protocol, no lookups");
    }

    /// A result set shaped like the Das Kapital problem: one work, many
    /// languages and formats mixed together.
    fn polyglot() -> Vec<Book> {
        let entry = |i: usize, lang: &str, ext: &str| Book {
            md5: format!("{i:032x}"),
            title: format!("Das Kapital {i}"),
            language: Some(lang.into()),
            extension: Some(ext.into()),
            ..Default::default()
        };
        vec![
            entry(0, "German", "pdf"),
            entry(1, "German", "epub"),
            entry(2, "English", "epub"),
            entry(3, "German", "pdf"),
            entry(4, "English", "pdf"),
            entry(5, "Spanish", "djvu"),
        ]
    }

    #[test]
    fn cycling_the_language_filter_narrows_the_results() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        assert_eq!(a.visible.len(), 6);

        // Options come most-common-first, so one press lands on German…
        press(&mut a, KeyCode::Char('l'));
        assert_eq!(a.lang_filter.as_deref(), Some("German"));
        assert_eq!(a.visible.len(), 3);

        // …and the next lands on English: the whole point of the feature.
        press(&mut a, KeyCode::Char('l'));
        assert_eq!(a.lang_filter.as_deref(), Some("English"));
        assert_eq!(a.visible.len(), 2);
        assert!(a.visible.iter().all(|&i| a.results[i].language.as_deref() == Some("English")));

        // Cycling past the end returns to everything.
        press(&mut a, KeyCode::Char('l')); // Spanish
        press(&mut a, KeyCode::Char('l')); // back to all
        assert_eq!(a.lang_filter, None);
        assert_eq!(a.visible.len(), 6);
    }

    #[test]
    fn the_two_filters_compose_and_x_clears_them() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());

        press(&mut a, KeyCode::Char('l')); // German (most common)
        press(&mut a, KeyCode::Char('e')); // pdf (most common among German)
        assert_eq!(a.fmt_filter.as_deref(), Some("pdf"));
        assert_eq!(a.visible.len(), 2, "German pdfs only");

        press(&mut a, KeyCode::Char('x'));
        assert_eq!(a.fmt_filter, None);
        assert_eq!(a.lang_filter, None);
        assert_eq!(a.visible.len(), 6);
    }

    #[test]
    fn facet_counts_respect_the_other_filter() {
        let mut a = app();
        install(&mut a, polyglot());
        a.lang_filter = Some("English".into());
        a.refine();
        // With English active there is 1 epub and 1 pdf to offer, not 2 and 3.
        let formats = a.facet_options(true);
        assert!(formats.iter().all(|(_, n)| *n == 1), "{formats:?}");
    }

    #[test]
    fn filters_survive_a_new_search() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        press(&mut a, KeyCode::Char('l'));
        press(&mut a, KeyCode::Char('l'));
        assert_eq!(a.lang_filter.as_deref(), Some("English"));

        a.handle(Ev::Results(Ok((polyglot(), "libgen.li".into()))));
        assert_eq!(a.lang_filter.as_deref(), Some("English"), "a standing preference");
        assert_eq!(a.visible.len(), 2);
        assert!(a.status.contains("2 of 6"), "{}", a.status);
    }

    #[test]
    fn selection_downloads_and_marks_follow_the_visible_rows() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        press(&mut a, KeyCode::Char('l'));
        press(&mut a, KeyCode::Char('l')); // English: results 2 and 4

        assert_eq!(a.selected().map(|b| b.md5.clone()).as_deref(), Some("00000000000000000000000000000002"));
        press(&mut a, KeyCode::Down);
        assert_eq!(a.selected().map(|b| b.md5.clone()).as_deref(), Some("00000000000000000000000000000004"));
        press(&mut a, KeyCode::Down);
        assert_eq!(a.table.selected(), Some(1), "two rows showing, no further");

        // `a` marks only what is showing.
        press(&mut a, KeyCode::Char('a'));
        assert_eq!(a.marked, [false, false, true, false, true, false]);
    }

    #[test]
    fn the_filter_keeps_the_highlighted_book_when_it_survives() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        // Highlight the English pdf (index 4), then filter to pdfs: the book
        // survives the cut, so the highlight must stay on it.
        for _ in 0..4 {
            press(&mut a, KeyCode::Down);
        }
        press(&mut a, KeyCode::Char('e')); // pdf, the most common format
        assert_eq!(a.fmt_filter.as_deref(), Some("pdf"));
        assert_eq!(
            a.selected().map(|b| b.md5.clone()).as_deref(),
            Some("00000000000000000000000000000004"),
            "the highlight should follow the book, not the row number"
        );
        assert_eq!(a.table.selected(), Some(2), "the book moved up the list");
    }

    #[test]
    fn the_filter_bar_reports_the_narrowing() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("6 results"), "{screen}");
        assert!(screen.contains("format"), "{screen}");
        assert!(screen.contains("language"), "{screen}");

        press(&mut a, KeyCode::Char('l'));
        press(&mut a, KeyCode::Char('l'));
        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("English"), "{screen}");
        assert!(screen.contains("2 of 6 shown"), "{screen}");
        assert!(screen.contains("clear"), "{screen}");
    }

    #[test]
    fn an_over_narrow_filter_pair_explains_itself() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        a.lang_filter = Some("Spanish".into());
        a.fmt_filter = Some("epub".into());
        a.refine();
        assert!(a.visible.is_empty());
        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("none of the 6 results"), "{screen}");
        assert!(screen.contains("epub + Spanish"), "{screen}");
        assert!(screen.contains("clears"), "{screen}");
    }

    #[test]
    fn a_wide_terminal_gets_the_side_panel() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(
            &mut a,
            vec![Book {
                md5: "1b9159991f7fb1b3910c0be9ebf7e595".into(),
                title: "The Dispossessed".into(),
                authors: Some("Ursula K. Le Guin".into()),
                publisher: Some("Harper & Row".into()),
                year: Some("1974".into()),
                language: Some("English".into()),
                extension: Some("epub".into()),
                pages: Some("341".into()),
                size_bytes: Some(2 * 1024 * 1024),
                ..Default::default()
            }],
        );

        // Wide: the record reads as labelled rows in the right-hand panel.
        let screen = draw(&mut a, 130, 40);
        assert!(screen.contains("publisher"), "{screen}");
        assert!(screen.contains("Harper & Row"), "{screen}");
        assert!(screen.contains("1b9159991f7fb1b3910c0be9ebf7e595"), "{screen}");

        // Narrow: the old bottom strip, nothing lost.
        let screen = draw(&mut a, 90, 24);
        assert!(screen.contains("The Dispossessed"), "{screen}");
        assert!(screen.contains("1b9159991f7fb1b3910c0be9ebf7e595"), "{screen}");
    }

    /// Render the app to styled HTML for out-of-terminal design review.
    /// Dev-only: runs when CLIBGEN_PREVIEW_DIR is set, via `--ignored`.
    #[test]
    #[ignore]
    fn dump_design_previews() {
        use ratatui::backend::TestBackend;
        use std::fmt::Write as _;

        let Some(dir) = std::env::var_os("CLIBGEN_PREVIEW_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);

        fn html(a: &mut App, width: u16, height: u16, title: &str) -> String {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|f| a.render(f)).unwrap();
            let buffer = terminal.backend().buffer().clone();
            let css = |c: ratatui::style::Color, fallback: &str| match c {
                Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
                _ => fallback.to_string(),
            };
            let mut out = format!(
                "<div class='shot'><h2>{title} <small>{width}×{height}</small></h2><pre>"
            );
            for y in 0..buffer.area.height {
                for x in 0..buffer.area.width {
                    let cell = &buffer[(x, y)];
                    let fg = css(cell.style().fg.unwrap_or(Color::Reset), "rgb(238,241,246)");
                    let bg = css(cell.style().bg.unwrap_or(Color::Reset), "rgb(16,20,27)");
                    let bold = if cell.style().add_modifier.contains(Modifier::BOLD) {
                        "font-weight:bold;"
                    } else {
                        ""
                    };
                    let symbol = cell
                        .symbol()
                        .replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;");
                    let _ = write!(
                        out,
                        "<span style='color:{fg};background:{bg};{bold}'>{symbol}</span>"
                    );
                }
                out.push('\n');
            }
            out.push_str("</pre></div>\n");
            out
        }

        fn kapital() -> Vec<Book> {
            let b = |i: usize,
                     title: &str,
                     authors: &str,
                     publisher: &str,
                     year: &str,
                     lang: &str,
                     pages: &str,
                     size: u64,
                     ext: &str| Book {
                md5: format!("{i:032x}"),
                title: title.into(),
                authors: Some(authors.into()),
                publisher: Some(publisher.into()),
                year: Some(year.into()),
                language: Some(lang.into()),
                pages: Some(pages.into()),
                size_bytes: Some(size),
                extension: Some(ext.into()),
                file_id: None,
            };
            vec![
                b(0, "Das Kapital. Kritik der politischen Ökonomie. Erster Band", "Karl Marx", "Otto Meissner", "1867", "German", "784", 24_600_000, "pdf"),
                b(1, "Capital: A Critique of Political Economy, Volume I", "Karl Marx; Ben Fowkes (transl.)", "Penguin Classics", "1990", "English", "1152", 2_400_000, "epub"),
                b(2, "Das Kapital, Band 1", "Karl Marx", "Dietz Verlag", "1962", "German", "955", 18_100_000, "pdf"),
                b(3, "Capital, Vol. 1: A Critical Analysis of Capitalist Production", "Karl Marx; Samuel Moore", "Progress Publishers", "1887", "English", "802", 5_200_000, "pdf"),
                b(4, "Das Kapital: Kritik der politischen Ökonomie (Gesamtausgabe)", "Karl Marx; Friedrich Engels", "Akademie Verlag", "1991", "German", "1420", 41_000_000, "pdf"),
                b(5, "Capital: A Critique of Political Economy, Volume II", "Karl Marx; David Fernbach (transl.)", "Penguin Classics", "1992", "English", "624", 1_900_000, "epub"),
                b(6, "El Capital: Crítica de la economía política, Tomo I", "Karl Marx", "Siglo XXI Editores", "1975", "Spanish", "381", 9_800_000, "pdf"),
                b(7, "Das Kapital (Volksausgabe)", "Karl Marx; Karl Kautsky (ed.)", "J.H.W. Dietz", "1914", "German", "698", 31_400_000, "djvu"),
                b(8, "Капитал. Критика политической экономии. Том 1", "Карл Маркс", "Политиздат", "1983", "Russian", "905", 14_200_000, "djvu"),
                b(9, "Capital: An Abridged Edition", "Karl Marx; David McLellan (ed.)", "Oxford University Press", "2008", "English", "608", 1_100_000, "epub"),
                b(10, "Das Kapital, Band 2: Der Zirkulationsprozess des Kapitals", "Karl Marx; Friedrich Engels (ed.)", "Dietz Verlag", "1963", "German", "559", 12_700_000, "pdf"),
                b(11, "Le Capital, Livre I", "Karl Marx; Joseph Roy (trad.)", "Éditions sociales", "1976", "French", "716", 7_300_000, "pdf"),
            ]
        }

        // A plausible 2:3 jacket: a deep vertical gradient with a pale title
        // band, so the preview judges layout the way a real cover would.
        let cover = {
            let (w, h) = (120usize, 180usize);
            let mut pixels = Vec::with_capacity(w * h * 3);
            for y in 0..h {
                for x in 0..w {
                    let t = y as f32 / h as f32;
                    let (mut r, mut g, mut b) = (
                        18.0 + 60.0 * t,
                        52.0 + 30.0 * (1.0 - t),
                        84.0 + 90.0 * t,
                    );
                    if (30..48).contains(&y) {
                        (r, g, b) = (226.0, 218.0, 200.0);
                    }
                    if x < 4 {
                        (r, g, b) = (r * 0.6, g * 0.6, b * 0.6);
                    }
                    pixels.extend([r as u8, g as u8, b as u8]);
                }
            }
            graphics::Image::new(w, h, pixels)
        };
        let mut page = String::from(
            "<!doctype html><meta charset='utf-8'><style>\
             body{background:#0a0d12;color:#eef1f6;font-family:system-ui;padding:24px}\
             h2{font-weight:600;margin:32px 0 8px} h2 small{color:#8892a4;font-weight:400}\
             pre{font:13px/1.15 'SF Mono',Menlo,monospace;display:inline-block;\
                 border-radius:8px;overflow:hidden;margin:0;box-shadow:0 8px 40px rgba(0,0,0,.5)}\
             </style><h1>clibgen — TUI states</h1>",
        );

        // 1. Wide search, cover art, one download running, one saved.
        let mut a = app();
        a.mode = Mode::Browsing;
        a.mirror_label = "libgen.li".into();
        a.protocol = Protocol::Blocks;
        a.library = entries(7); // so the tab bar carries a believable count
        install(&mut a, kapital());
        a.covers.insert(
            "00000000000000000000000000000000".into(),
            Slot::Ready(Box::new(Art {
                encoded: Vec::new(),
                pixels: Some(cover.clone()),
            })),
        );
        a.jobs.insert(
            "00000000000000000000000000000001".into(),
            Job::Saved("capital-vol-1.epub".into()),
        );
        a.jobs.insert(
            "00000000000000000000000000000003".into(),
            Job::Running {
                done: 2_400_000,
                total: Some(5_200_000),
            },
        );
        a.marked[9] = true;
        page.push_str(&html(&mut a, 132, 42, "Search — wide, unfiltered"));

        // 2. The Das Kapital case: English only.
        a.lang_filter = Some("English".into());
        a.refine();
        a.covers.insert(
            "00000000000000000000000000000001".into(),
            Slot::Ready(Box::new(Art {
                encoded: Vec::new(),
                pixels: Some(cover.clone()),
            })),
        );
        page.push_str(&html(&mut a, 132, 42, "Search — filtered to English"));

        // 3. Both filters: English epubs.
        a.fmt_filter = Some("epub".into());
        a.refine();
        page.push_str(&html(&mut a, 132, 42, "Search — English epubs only"));

        // 4. Narrow terminal, bottom strip.
        page.push_str(&html(&mut a, 90, 28, "Search — narrow (80–100 col) layout"));

        // 5. Library: real files on disk so `present()` reports the normal
        // case, and exactly one book gone missing.
        let mut a = app();
        with_library(&mut a, {
            let mut es = entries(6);
            for (e, (t, au, ext, mb)) in es.iter_mut().zip([
                ("Capital: A Critique of Political Economy, Volume I", "Karl Marx", "epub", 2.4),
                ("The Dispossessed", "Ursula K. Le Guin", "epub", 1.1),
                ("Dune Messiah", "Frank Herbert", "epub", 0.9),
                ("The Rust Programming Language", "Steve Klabnik; Carol Nichols", "pdf", 11.2),
                ("Gödel, Escher, Bach", "Douglas Hofstadter", "djvu", 24.0),
                ("A Wizard of Earthsea", "Ursula K. Le Guin", "mobi", 0.7),
            ]) {
                e.title = t.into();
                e.authors = Some(au.into());
                e.extension = Some(ext.into());
                e.size = (mb * 1024.0 * 1024.0) as u64;
                e.path = dir.join(format!("{t}.{ext}"));
                std::fs::write(&e.path, "x").unwrap();
            }
            es
        });
        a.library[4].path = std::path::PathBuf::from("/nonexistent/missing.djvu");
        a.refilter();
        page.push_str(&html(&mut a, 100, 26, "Library — narrow"));

        // 5b. Library, wide, with the highlighted book's own cover showing —
        // the thing the file itself carries, read off disk with no network.
        a.protocol = Protocol::Blocks;
        a.library_table.select(Some(0));
        a.covers.insert(
            a.library[0].md5.clone(),
            Slot::Ready(Box::new(Art {
                encoded: Vec::new(),
                pixels: Some(cover.clone()),
            })),
        );
        page.push_str(&html(&mut a, 132, 40, "Library — wide, cover from the file"));

        // 6. Help overlay.
        let mut a = app();
        a.mode = Mode::Help;
        install(&mut a, kapital());
        page.push_str(&html(&mut a, 132, 42, "Help overlay"));

        // 7. First launch, nothing searched yet.
        let mut a = app();
        a.mode = Mode::Browsing;
        a.status = "press / to search".into();
        a.mirror_label = "libgen.li".into();
        page.push_str(&html(&mut a, 100, 26, "Empty — first launch"));

        std::fs::write(dir.join("preview.html"), page).unwrap();
    }

    #[test]
    fn job_labels_cover_every_state() {
        assert_eq!(job_label(None), "");
        assert_eq!(job_label(Some(&Job::Queued)), "queued");
        assert_eq!(job_label(Some(&Job::Saved("x".into()))), "saved");
        assert_eq!(job_label(Some(&Job::Failed("x".into()))), "failed");
        assert_eq!(
            job_label(Some(&Job::Running {
                done: 25,
                total: Some(100)
            })),
            " 25%"
        );
    }
}
