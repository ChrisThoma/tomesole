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

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, TableState, Wrap,
};
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

    /// The canvas painted under every frame. Deep navy-black: the darker the
    /// ground, the harder the true-colour accents pop off it.
    pub const BG: Color = Color::Rgb(13, 17, 25);
    /// Every other table row, one step off the canvas: enough for the eye to
    /// track a row across a wide screen, not enough to read as stripes.
    pub const BG_ALT: Color = Color::Rgb(23, 29, 42);
    pub const ACCENT: Color = Color::Rgb(58, 232, 219); // aqua — interactive
    pub const ACCENT_DEEP: Color = Color::Rgb(96, 202, 210);
    pub const AMBER: Color = Color::Rgb(255, 197, 92); // emphasis
    pub const TEXT: Color = Color::Rgb(241, 245, 251);
    pub const MUTED: Color = Color::Rgb(186, 196, 214);
    pub const FAINT: Color = Color::Rgb(139, 150, 172);
    pub const SUCCESS: Color = Color::Rgb(102, 231, 145);
    pub const DANGER: Color = Color::Rgb(255, 110, 122);
    /// Languages get one colour of their own — a bright cornflower blue —
    /// because they are one of the two facets the interface filters by.
    pub const LANG: Color = Color::Rgb(135, 184, 255);
    /// The wash behind the highlighted row: a teal tint, bright enough to be
    /// plainly the cursor, dim enough to sit under white text.
    pub const SELECT_BG: Color = Color::Rgb(28, 62, 72);

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
            "epub" => Color::Rgb(88, 226, 140),
            "pdf" => Color::Rgb(255, 126, 110),
            "mobi" | "azw" | "azw3" => Color::Rgb(250, 190, 80),
            "djvu" => Color::Rgb(196, 150, 255),
            "cbz" | "cbr" => Color::Rgb(250, 142, 206),
            _ => Color::Rgb(154, 166, 188),
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
    Running {
        done: u64,
        total: Option<u64>,
        /// When the transfer began, so the detail pane can show a live speed
        /// and estimate. Averaged over the whole transfer rather than sampled
        /// instantaneously — steadier to read, and it needs no extra state.
        started: Instant,
    },
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

/// How a list is ordered. A Libgen search comes back as a ragged pile — forty
/// editions of one book, scattered years, sizes and formats — so being able to
/// impose an order is as much a part of finding a book as the filters are.
///
/// `Relevance` is the order the mirror returned (its own guess at the best
/// match) on the Search tab, and newest-first on the Library tab; the rest sort
/// by one visible column. The two tabs share the machinery so the key means the
/// same thing on both.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Sort {
    Relevance,
    Title,
    Author,
    Year,
    Size,
}

impl Sort {
    /// The cycle `s` walks through, ending back at the mirror's own order.
    const CYCLE: [Sort; 5] = [
        Sort::Relevance,
        Sort::Title,
        Sort::Author,
        Sort::Year,
        Sort::Size,
    ];

    /// What the control strip calls this order. `Relevance` reads differently
    /// depending on the tab, since a library has no relevance to speak of.
    fn label(self, tab: Tab) -> &'static str {
        match self {
            Sort::Relevance => match tab {
                Tab::Search => "relevance",
                Tab::Library => "recent",
            },
            Sort::Title => "title",
            Sort::Author => "author",
            Sort::Year => "year",
            Sort::Size => "size",
        }
    }

    /// The direction that key is most useful in the first time it is chosen:
    /// names read A→Z, but "sort by size" almost always means "biggest first",
    /// and "by year" means "newest first". `S` overrides this afterwards.
    fn natural_desc(self) -> bool {
        matches!(self, Sort::Year | Sort::Size)
    }
}

/// One of the columns the results can be narrowed by after they arrive.
///
/// A Libgen search brings back a ragged pile — forty editions of one book in a
/// dozen languages, its authors repeating down the list. Format and language
/// are low-cardinality, a handful of values, so they read as a menu; author is
/// high-cardinality, so the same overlay leans on its type-to-narrow line.
/// Treating all three the same way is what lets one gesture — open, type or
/// arrow, Enter — stand in for the blind cycle that came before and could not
/// touch the author column at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Facet {
    Format,
    Language,
    Author,
}

impl Facet {
    const ALL: [Facet; 3] = [Facet::Format, Facet::Language, Facet::Author];

    fn label(self) -> &'static str {
        match self {
            Facet::Format => "format",
            Facet::Language => "language",
            Facet::Author => "author",
        }
    }

    /// The key that opens this facet's picker, echoed in the strip and hints.
    fn key(self) -> char {
        match self {
            Facet::Format => 'e',
            Facet::Language => 'l',
            Facet::Author => 'a',
        }
    }

    fn next(self) -> Self {
        let at = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(at + 1) % Self::ALL.len()]
    }

    fn prev(self) -> Self {
        let at = Self::ALL.iter().position(|f| *f == self).unwrap_or(0);
        Self::ALL[(at + Self::ALL.len() - 1) % Self::ALL.len()]
    }

    /// The value this facet reads off a record, as the menu and the counts see
    /// it. Format is lowercased so `EPUB` and `epub` fold into one row; the
    /// others keep the mirror's own casing.
    fn value_of(self, book: &Book) -> String {
        match self {
            Facet::Format => book.ext().to_ascii_lowercase(),
            Facet::Language => book.language.as_deref().unwrap_or("unknown").to_string(),
            Facet::Author => book.authors_or_unknown().to_string(),
        }
    }

    /// Does `book` satisfy a filter set to `want`? Format and language match a
    /// whole value exactly — they are picked from a fixed menu — while author
    /// matches a substring, so a fragment of a name narrows the list.
    fn admits(self, want: &str, book: &Book) -> bool {
        match self {
            Facet::Format => book.ext().eq_ignore_ascii_case(want),
            Facet::Language => book
                .language
                .as_deref()
                .unwrap_or("unknown")
                .eq_ignore_ascii_case(want),
            Facet::Author => book
                .authors_or_unknown()
                .to_lowercase()
                .contains(&want.to_lowercase()),
        }
    }
}

/// The pop-over that carves the results by one facet.
///
/// It holds the menu of values present — each with the count that choosing it
/// would leave on screen — and a type-to-narrow line, so picking "epub" or
/// "Le Guin" is one gesture whether there are three formats or eighty authors.
/// The list you are aiming at stays on screen the whole time, rather than
/// flashing past one value per keypress the way the old cycle did.
struct Picker {
    facet: Facet,
    /// The type-to-narrow text, matched case-insensitively as a substring
    /// against every row's value.
    query: String,
    /// Every value present for this facet, most common first — the unfiltered
    /// menu. Recomputed when the facet changes, not on every keystroke.
    options: Vec<(String, usize)>,
    /// Where the highlight sits, as an index into the rows on show: row 0 is
    /// always "all", the surviving options follow.
    cursor: usize,
}

impl Picker {
    /// The options that survive the type-to-narrow text, in menu order.
    fn shown(&self) -> Vec<&(String, usize)> {
        if self.query.trim().is_empty() {
            return self.options.iter().collect();
        }
        let needle = self.query.to_lowercase();
        self.options
            .iter()
            .filter(|(v, _)| v.to_lowercase().contains(&needle))
            .collect()
    }

    /// Selectable rows: the surviving options plus the leading "all" row.
    fn rows(&self) -> usize {
        self.shown().len() + 1
    }

    /// Where the highlight belongs after the query changes: the first real
    /// option when the narrowing left any, otherwise the "all" row.
    fn first_match_row(&self) -> usize {
        if self.shown().is_empty() { 0 } else { 1 }
    }
}

/// The little menu behind `s`: which column orders the list, and which way.
///
/// Sort is the other thing that used to step one keypress at a time — `s`
/// walked relevance → title → author → year → size and back. Like the facet
/// picker, this floats the whole set of orders over the results so the one you
/// want is a single choice, with its direction shown and flippable in place.
struct SortPick {
    /// Index into `Sort::CYCLE` of the highlighted key.
    cursor: usize,
    /// The direction Enter would apply for the highlighted key. It follows the
    /// key's natural default as the highlight moves; `←`/`→` overrides it.
    desc: bool,
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
    /// Narrow the results to authors whose name contains this, matched as a
    /// case-insensitive substring so part of a name is enough.
    author_filter: Option<String>,
    /// The open facet pop-over, when one is up. A modal that carves the results
    /// by format, language or author — the interactive answer to "epubs only"
    /// or "just the Le Guin ones" without re-running the search.
    picker: Option<Picker>,
    /// The open sort menu, when one is up. Like the facet picker, a modal that
    /// takes every key while it is showing.
    sort_menu: Option<SortPick>,
    /// How the visible list is ordered, and which way. Shared by both tabs and,
    /// like the filters, a standing preference that survives the next search.
    sort: Sort,
    sort_desc: bool,
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
    /// The MD5s of every book already in the library, so a search result you
    /// have downloaded before can be recognised at a glance and not fetched
    /// twice. Rebuilt whenever the library is reloaded.
    owned: HashSet<String>,
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
    /// A frame counter, bumped once per draw, that drives the small animated
    /// spinner shown while something is in flight. The event loop wakes on a
    /// 250 ms timer even when idle, so it turns at a calm few frames a second.
    spinner: usize,
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
        author_filter: None,
        picker: None,
        sort_menu: None,
        sort: Sort::Relevance,
        sort_desc: false,
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
        owned: HashSet::new(),
        library_table: TableState::default(),
        protocol,
        covers: HashMap::new(),
        cover_due: None,
        painted: None,
        cover_area: None,
        spinner: 0,
    };
    app.pending_search = !app.query.is_empty();
    // Read the library up front rather than when the tab is first opened: it
    // costs one small file read, and it means the tab bar can say how many
    // books are there before anybody goes looking.
    app.reload_library();
    app.spawn_mirror_resolve(app.settings.refresh_mirrors);

    let mut terminal = ratatui::try_init()?;
    // Name the window after us while the interface is up. A full-screen app
    // never overwrites the title the shell last set — usually the launch
    // command — so without this it just hangs there stale. We deliberately do
    // not save and restore the prior title: the shell resets it at the next
    // prompt, and that prior title is the very stale text we are replacing.
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::SetTitle("tomesole"));
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
    // Rather than make people re-search with `lang:` tags, three facets carve
    // the results in place: `e` for format, `l` for language, `a` for author,
    // each opening a pop-over menu of the values actually present with a live
    // count of what choosing one would leave.

    /// The filter value currently set on a facet, if any.
    fn filter_for(&self, facet: Facet) -> Option<&str> {
        match facet {
            Facet::Format => self.fmt_filter.as_deref(),
            Facet::Language => self.lang_filter.as_deref(),
            Facet::Author => self.author_filter.as_deref(),
        }
    }

    /// Set (or, with `None`, clear) a facet's filter.
    fn set_filter(&mut self, facet: Facet, value: Option<String>) {
        match facet {
            Facet::Format => self.fmt_filter = value,
            Facet::Language => self.lang_filter = value,
            Facet::Author => self.author_filter = value,
        }
    }

    /// Whether any facet is narrowing the results at all.
    fn any_filter(&self) -> bool {
        Facet::ALL.iter().any(|&f| self.filter_for(f).is_some())
    }

    /// Does every active filter admit this record?
    fn admits(&self, book: &Book) -> bool {
        Facet::ALL.iter().all(|&f| self.facet_admits(f, book))
    }

    /// Does the filter on one facet — if it has one — admit this record?
    fn facet_admits(&self, facet: Facet, book: &Book) -> bool {
        match self.filter_for(facet) {
            Some(want) => facet.admits(want, book),
            None => true,
        }
    }

    /// Whether `book` survives every filter *except* the one on `facet` — the
    /// set a facet's own menu counts against, so its numbers say what choosing
    /// a value would leave once the other facets have had their say.
    fn admits_except(&self, facet: Facet, book: &Book) -> bool {
        Facet::ALL
            .iter()
            .filter(|&&f| f != facet)
            .all(|&f| self.facet_admits(f, book))
    }

    /// Recompute which rows show, keeping the highlight on the same book when
    /// it survives the cut and on the top row when it does not.
    fn refine(&mut self) {
        let keep = self.selected().map(|b| b.md5.clone());
        self.visible = (0..self.results.len())
            .filter(|&i| self.admits(&self.results[i]))
            .collect();
        self.sort_visible();
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

    /// Order the visible results by the active sort. The vector already carries
    /// the mirror's own order, so `Relevance` only ever reverses it; the other
    /// keys sort stably, so books with an equal key keep that underlying order.
    fn sort_visible(&mut self) {
        use std::cmp::Ordering;
        if self.sort == Sort::Relevance {
            if self.sort_desc {
                self.visible.reverse();
            }
            return;
        }
        // Sort a detached copy so the closure can read the rest of `self`.
        let mut visible = std::mem::take(&mut self.visible);
        let (sort, desc) = (self.sort, self.sort_desc);
        visible.sort_by(|&a, &b| {
            let (x, y) = (&self.results[a], &self.results[b]);
            let ord = match sort {
                Sort::Title => x.title.to_lowercase().cmp(&y.title.to_lowercase()),
                Sort::Author => first_author_of(x.authors.as_deref())
                    .cmp(&first_author_of(y.authors.as_deref())),
                Sort::Year => year_num(x.year.as_deref()).cmp(&year_num(y.year.as_deref())),
                Sort::Size => x.size_bytes.unwrap_or(0).cmp(&y.size_bytes.unwrap_or(0)),
                Sort::Relevance => Ordering::Equal,
            };
            if desc { ord.reverse() } else { ord }
        });
        self.visible = visible;
    }

    /// Raise the sort menu, opened on the order in force so a glance says how
    /// the list is arranged and Enter on it changes nothing. Works from either
    /// tab, since both share the one sort.
    fn open_sort_menu(&mut self) {
        let cursor = Sort::CYCLE.iter().position(|s| *s == self.sort).unwrap_or(0);
        self.sort_menu = Some(SortPick {
            cursor,
            desc: self.sort_desc,
        });
    }

    /// Commit the highlighted order and close the menu. `Relevance` is the
    /// list's own order and carries no direction, so it always applies ascending.
    fn apply_sort_menu(&mut self) {
        let Some(menu) = self.sort_menu.take() else {
            return;
        };
        self.sort = Sort::CYCLE[menu.cursor];
        self.sort_desc = self.sort != Sort::Relevance && menu.desc;
        self.resort();
    }

    /// Flip the current sort's direction, leaving the key alone.
    fn reverse_sort(&mut self) {
        self.sort_desc = !self.sort_desc;
        self.resort();
    }

    /// Re-apply the sort to whichever list the visible tab owns.
    fn resort(&mut self) {
        match self.tab {
            Tab::Search => self.refine(),
            Tab::Library => self.refilter(),
        }
        self.selection_moved();
    }

    /// The values a facet could take, most common first, each counted with the
    /// *other* facets still applied — so the numbers say what picking that value
    /// would actually leave on screen.
    fn facet_options(&self, facet: Facet) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for book in &self.results {
            if !self.admits_except(facet, book) {
                continue;
            }
            let value = facet.value_of(book);
            match counts
                .iter_mut()
                .find(|(name, _)| name.eq_ignore_ascii_case(&value))
            {
                Some((_, n)) => *n += 1,
                None => counts.push((value, 1)),
            }
        }
        counts.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });
        counts
    }

    /// How many records survive once a facet's filter joins the others — the
    /// number shown beside the active value in the strip.
    fn facet_count(&self, facet: Facet) -> Option<usize> {
        self.filter_for(facet)?;
        Some(
            self.results
                .iter()
                .filter(|b| self.admits_except(facet, b) && self.facet_admits(facet, b))
                .count(),
        )
    }

    /// Raise the pop-over that narrows the results by one facet. There is
    /// nothing to pick from until a search returns, so it stays shut on an
    /// empty list. The highlight opens on the value already chosen, if any, so
    /// reopening a facet lands where it was left.
    fn open_picker(&mut self, facet: Facet) {
        if self.results.is_empty() {
            return;
        }
        let options = self.facet_options(facet);
        // Open on the value already chosen; with none set, land on the most
        // common value rather than the "all" row, so "open, Enter" grabs the
        // obvious pick and clearing is a deliberate step up to "all".
        let cursor = self
            .filter_for(facet)
            .and_then(|cur| options.iter().position(|(n, _)| n.eq_ignore_ascii_case(cur)))
            .map(|i| i + 1)
            .unwrap_or(if options.is_empty() { 0 } else { 1 });
        self.picker = Some(Picker {
            facet,
            query: String::new(),
            options,
            cursor,
        });
    }

    /// Commit whatever the picker is pointing at and close it. The "all" row at
    /// the top clears the facet; any other row sets it to that value. Typing a
    /// fragment first is how a value is reached without arrowing — the fragment
    /// narrows the menu, and the highlight lands on the match it commits.
    fn apply_picker(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let facet = picker.facet;
        let value = if picker.cursor == 0 {
            None
        } else {
            picker.shown().get(picker.cursor - 1).map(|(v, _)| v.clone())
        };
        self.set_filter(facet, value);
        self.refine();
    }

    /// Clear every facet at once.
    fn clear_filters(&mut self) {
        for facet in Facet::ALL {
            self.set_filter(facet, None);
        }
        self.refine();
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
        self.owned = self.library.iter().map(|e| e.md5.clone()).collect();
        self.refilter();
    }

    /// Work out which entries the filter admits.
    ///
    /// Matching is the same rule the `tomesole open` selector uses — title,
    /// author or filename, case-insensitively — so the two agree about what
    /// "dune" means.
    fn refilter(&mut self) {
        let keep = self.selected_entry().map(|e| e.md5.clone());
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
        self.sort_shown();

        self.library_table.select(if self.shown.is_empty() {
            None
        } else {
            // Keep the highlight on the same book when it is still shown; else
            // pull it back in rather than leaving it pointing past the end.
            let position = keep.and_then(|md5| {
                self.shown.iter().position(|&i| self.library[i].md5 == md5)
            });
            Some(position.unwrap_or_else(|| {
                self.library_table
                    .selected()
                    .unwrap_or(0)
                    .min(self.shown.len() - 1)
            }))
        });
    }

    /// Order the shown library entries by the active sort. `Relevance` is the
    /// newest-first order the list already arrives in, so it only reverses.
    fn sort_shown(&mut self) {
        use std::cmp::Ordering;
        if self.sort == Sort::Relevance {
            if self.sort_desc {
                self.shown.reverse();
            }
            return;
        }
        let mut shown = std::mem::take(&mut self.shown);
        let (sort, desc) = (self.sort, self.sort_desc);
        shown.sort_by(|&a, &b| {
            let (x, y) = (&self.library[a], &self.library[b]);
            let ord = match sort {
                Sort::Title => x.title.to_lowercase().cmp(&y.title.to_lowercase()),
                Sort::Author => first_author_of(x.authors.as_deref())
                    .cmp(&first_author_of(y.authors.as_deref())),
                // A library entry has no year column, so "year" falls back to
                // when the book was added — the nearest thing it knows.
                Sort::Year => x.at.cmp(&y.at),
                Sort::Size => x.size.cmp(&y.size),
                Sort::Relevance => Ordering::Equal,
            };
            if desc { ord.reverse() } else { ord }
        });
        self.shown = shown;
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

    /// Copy the focused book's MD5 to the system clipboard.
    ///
    /// The MD5 is the one handle that identifies a book everywhere — it is what
    /// `tomesole get` takes, and how a book is quoted to someone else — so it is
    /// the thing worth lifting out of the interface without reaching for a
    /// mouse. It goes out over OSC 52, the clipboard channel a terminal offers
    /// with no platform dependency: honoured by kitty, iTerm2, WezTerm, Ghostty
    /// and tmux, and harmlessly ignored elsewhere, so the status line reports
    /// what was sent rather than claiming a guaranteed paste.
    fn yank_md5(&mut self) {
        let Some(md5) = self.focused_md5() else {
            return;
        };
        copy_to_clipboard(&md5);
        self.error = None;
        self.status = format!("copied md5 {md5} to the clipboard");
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
                // A picker open over the old results is now describing a menu
                // that no longer exists; close it rather than let it commit a
                // stale value against the fresh set.
                self.picker = None;
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
                // Keep the start time across updates so the speed and estimate
                // are averaged over the whole transfer, not the last chunk.
                let started = match self.jobs.get(&md5) {
                    Some(Job::Running { started, .. }) => *started,
                    _ => Instant::now(),
                };
                self.jobs.insert(md5, Job::Running { done, total, started });
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
        // The facet pop-over is modal: while it is up it takes every key, so a
        // letter narrows its list rather than leaking out to the results below.
        if self.picker.is_some() {
            self.on_key_picker(key);
            return;
        }
        // The sort menu is modal in the same way.
        if self.sort_menu.is_some() {
            self.on_key_sort(key);
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
            KeyCode::Char('A') => {
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
            KeyCode::Char('e') => self.open_picker(Facet::Format),
            KeyCode::Char('l') => self.open_picker(Facet::Language),
            KeyCode::Char('a') => self.open_picker(Facet::Author),
            KeyCode::Char('x') if self.any_filter() => self.clear_filters(),
            KeyCode::Char('s') => self.open_sort_menu(),
            KeyCode::Char('S') => self.reverse_sort(),
            KeyCode::Char('y') => self.yank_md5(),
            KeyCode::Enter => self.spawn_downloads(),
            KeyCode::Char('r') => self.spawn_search(),
            KeyCode::Char('m') => self.spawn_mirror_resolve(true),
            KeyCode::Char('o') => self.launch_result(false),
            KeyCode::Char('f') => self.launch_result(true),
            _ => {}
        }
    }

    /// Keys while the facet pop-over is up. Letters type into its narrow line;
    /// the arrows (and Ctrl-N/P, so the home row keeps typing) walk the menu;
    /// Tab rotates to the next facet without leaving the overlay; Enter commits
    /// and Esc backs out leaving the results as they were.
    fn on_key_picker(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // These need `self` whole — a facet switch recomputes the menu, apply
        // re-sorts the results — so they run before the mutable borrow below.
        match key.code {
            KeyCode::Esc => {
                self.picker = None;
                return;
            }
            KeyCode::Enter => {
                self.apply_picker();
                return;
            }
            KeyCode::Tab => {
                if let Some(next) = self.picker.as_ref().map(|p| p.facet.next()) {
                    self.open_picker(next);
                }
                return;
            }
            KeyCode::BackTab => {
                if let Some(prev) = self.picker.as_ref().map(|p| p.facet.prev()) {
                    self.open_picker(prev);
                }
                return;
            }
            _ => {}
        }
        let Some(picker) = self.picker.as_mut() else {
            return;
        };
        let last = picker.rows() - 1;
        match key.code {
            KeyCode::Up => picker.cursor = picker.cursor.saturating_sub(1),
            KeyCode::Char('p') if ctrl => picker.cursor = picker.cursor.saturating_sub(1),
            KeyCode::Down => picker.cursor = (picker.cursor + 1).min(last),
            KeyCode::Char('n') if ctrl => picker.cursor = (picker.cursor + 1).min(last),
            KeyCode::Home => picker.cursor = 0,
            KeyCode::End => picker.cursor = last,
            KeyCode::Char('u') if ctrl => {
                picker.query.clear();
                picker.cursor = 0;
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.cursor = picker.first_match_row();
            }
            KeyCode::Char(c) if !ctrl => {
                picker.query.push(c);
                picker.cursor = picker.first_match_row();
            }
            _ => {}
        }
    }

    /// Keys while the sort menu is up. The list is short and takes no text, so
    /// the arrows and `j`/`k` both walk it; `←`/`→` (and `h`/`l`) flip the
    /// highlighted key's direction; Enter commits and Esc — or `s` again —
    /// backs out.
    fn on_key_sort(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('s') | KeyCode::Char('q') => {
                self.sort_menu = None;
                return;
            }
            KeyCode::Enter => {
                self.apply_sort_menu();
                return;
            }
            _ => {}
        }
        let Some(menu) = self.sort_menu.as_mut() else {
            return;
        };
        let last = Sort::CYCLE.len() - 1;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                menu.cursor = menu.cursor.saturating_sub(1);
                menu.desc = Sort::CYCLE[menu.cursor].natural_desc();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                menu.cursor = (menu.cursor + 1).min(last);
                menu.desc = Sort::CYCLE[menu.cursor].natural_desc();
            }
            // Relevance is the mirror's own order; there is no ascending or
            // descending to choose, so the direction keys leave it alone.
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l')
                if Sort::CYCLE[menu.cursor] != Sort::Relevance =>
            {
                menu.desc = !menu.desc;
            }
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
            KeyCode::Char('s') => self.open_sort_menu(),
            KeyCode::Char('S') => self.reverse_sort(),
            KeyCode::Char('y') => self.yank_md5(),
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

    fn render(&mut self, frame: &mut Frame) {
        // One tick per frame drives every animated glyph on screen. The loop
        // wakes on a 250 ms timer even when idle, so it advances calmly.
        self.spinner = self.spinner.wrapping_add(1);
        let area = frame.area();
        // The canvas goes down first; everything after patches colour onto it.
        frame.render_widget(
            Block::new().style(Style::new().bg(theme::BG).fg(theme::TEXT)),
            area,
        );
        self.cover_area = None;

        // A terminal is a bottom-up place, so the interface is too. With nothing
        // to browse yet the search box floats near the middle — where the eye
        // already rests and a keystroke from the hints below. The moment a
        // search lands something to look at, the box settles to the bottom and
        // the results take the screen.
        let landing = match self.tab {
            Tab::Search => self.results.is_empty(),
            Tab::Library => self.library.is_empty(),
        };
        if landing {
            self.render_landing(frame, area);
        } else {
            // The record for the selected row lies as a horizontal band across
            // the bottom rather than a column down the side, so the results keep
            // the full width of the screen and the table never fights a panel
            // for room. A control strip rides directly on top of the search box:
            // the facets, sort and counts on Search; the sort and a one-line
            // shelf summary on Library. Gathering everything you type with at the
            // foot is what keeps the two tabs reading as one interface.
            let filter_bar = match self.tab {
                Tab::Search => !self.results.is_empty(),
                Tab::Library => !self.library.is_empty(),
            } as u16;
            let empty_tab = match self.tab {
                Tab::Search => self.visible.is_empty(),
                Tab::Library => self.shown.is_empty(),
            };
            let details = if empty_tab && self.error.is_none() {
                0
            } else {
                self.details_height(area)
            };
            let chunks = Layout::vertical([
                Constraint::Min(3),             // results / library body
                Constraint::Length(details),    // details band
                Constraint::Length(filter_bar), // facets / shelf summary + sort
                Constraint::Length(3),          // search box
                Constraint::Length(1),          // key hints
            ])
            .split(area);

            match self.tab {
                Tab::Search => {
                    self.render_results(frame, chunks[0]);
                    self.render_details(frame, chunks[1]);
                }
                Tab::Library => {
                    self.render_library(frame, chunks[0]);
                    self.render_library_details(frame, chunks[1]);
                }
            }
            if filter_bar == 1 {
                match self.tab {
                    Tab::Search => self.render_filters(frame, chunks[2]),
                    Tab::Library => self.render_library_strip(frame, chunks[2]),
                }
            }
            self.render_input(frame, chunks[3]);
            self.render_hints(frame, chunks[4]);
        }

        if self.mode == Mode::Help {
            self.render_help(frame);
        }
        if self.picker.is_some() {
            self.render_picker(frame);
        }
        if self.sort_menu.is_some() {
            self.render_sort_menu(frame);
        }
    }

    /// The opening state: the search box near the middle with a line of guidance
    /// under it, and the hints pinned to the foot of the screen. This is where a
    /// terminal's cursor wants to be — not craned up to the top edge — and it
    /// keeps the box next to the keys that explain it.
    fn render_landing(&self, frame: &mut Frame, area: Rect) {
        let rows = Layout::vertical([
            Constraint::Fill(4),
            Constraint::Length(3), // search box
            Constraint::Length(1), // breathing room
            Constraint::Length(4), // guidance / status / error
            Constraint::Fill(5),
            Constraint::Length(1), // key hints
        ])
        .split(area);

        // A capped width so it reads as a search field rather than a full-bleed
        // bar; centred so it sits under the eye, not off in a corner.
        let box_w = area.width.saturating_sub(6).clamp(24, 76);
        self.render_input(frame, centered(rows[1], box_w, rows[1].height));
        let guide_w = area.width.saturating_sub(4).min(80);
        self.render_landing_guide(frame, centered(rows[3], guide_w, rows[3].height));
        self.render_hints(frame, rows[5]);
    }

    /// What sits under the box before there is a list: an error if a search just
    /// failed, the live status while one runs, or a quiet word on how to begin.
    fn render_landing_guide(&self, frame: &mut Frame, area: Rect) {
        let lines: Vec<Line> = if let Some(err) = &self.error {
            vec![Line::from(vec![
                Span::styled("⚠ ", Style::new().fg(theme::DANGER)),
                Span::styled(err.clone(), Style::new().fg(theme::DANGER)),
            ])]
        } else if self.busy {
            vec![Line::from(vec![
                Span::styled(format!("{} ", spinner_frame(self.spinner)), theme::accent()),
                Span::styled(self.status.clone(), theme::muted()),
            ])]
        } else if !self.status.is_empty() {
            vec![Line::from(Span::styled(self.status.clone(), theme::muted()))]
        } else {
            match self.tab {
                Tab::Search => vec![
                    Line::from(Span::styled(
                        "Search millions of books on Library Genesis.",
                        theme::muted(),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("try ", theme::faint()),
                        Span::styled("author:", theme::accent()),
                        Span::styled("  ", theme::faint()),
                        Span::styled("title:", theme::accent()),
                        Span::styled("  ", theme::faint()),
                        Span::styled("ext:", theme::accent()),
                        Span::styled(" to narrow a search", theme::faint()),
                    ]),
                ],
                Tab::Library => vec![
                    Line::from(Span::styled(
                        "Your library is empty.",
                        theme::text().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("Books you download land here. Press ", theme::faint()),
                        Span::styled("Tab", theme::accent()),
                        Span::styled(" to go find one.", theme::faint()),
                    ]),
                ],
            }
        };
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), area);
    }

    /// The refinement bar: the three facets and what they leave showing.
    ///
    /// It lives on its own row, under the search box, so the counts update in
    /// place as a facet is picked — filters you can watch working, rather than
    /// tags you have to re-type into a new search.
    /// The sort indicator for the control strip: the key that cycles it, the
    /// active order, and a caret for the direction — dropped for `Relevance`,
    /// which is the list's own order and has no up or down.
    fn sort_indicator(&self) -> Vec<Span<'static>> {
        let caret = match (self.sort, self.sort_desc) {
            (Sort::Relevance, _) => "",
            (_, true) => " ▼",
            (_, false) => " ▲",
        };
        vec![
            Span::styled("s", theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(" sort ", theme::faint()),
            Span::styled(self.sort.label(self.tab).to_string(), theme::muted()),
            Span::styled(caret.to_string(), Style::new().fg(theme::AMBER)),
        ]
    }

    /// How many results are marked, and their combined size, when any are — so
    /// the strip can say what a batch download is about to pull.
    fn marked_summary(&self) -> Option<(usize, u64)> {
        let mut count = 0usize;
        let mut bytes = 0u64;
        for (book, marked) in self.results.iter().zip(&self.marked) {
            if *marked {
                count += 1;
                bytes += book.size_bytes.unwrap_or(0);
            }
        }
        (count > 0).then_some((count, bytes))
    }

    fn render_filters(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        // Right side first: the sort, then either what a marked batch would
        // pull, or how much of the catch is on screen. Amber comes out only
        // when something is being narrowed or gathered.
        let mut right = self.sort_indicator();
        right.push(Span::styled("     ", theme::faint()));
        match self.marked_summary() {
            Some((n, bytes)) => {
                let amber = Style::new().fg(theme::AMBER).add_modifier(Modifier::BOLD);
                right.push(Span::styled(format!("{n} marked"), amber));
                if bytes > 0 {
                    right.push(Span::styled(format!("  {} ", human_bytes(bytes)), theme::muted()));
                } else {
                    right.push(Span::raw(" "));
                }
            }
            None if self.visible.len() < self.results.len() => right.push(Span::styled(
                format!("{} of {} shown ", self.visible.len(), self.results.len()),
                Style::new().fg(theme::AMBER).add_modifier(Modifier::BOLD),
            )),
            None => right.push(Span::styled(
                format!("{} results ", self.results.len()),
                theme::faint(),
            )),
        }
        frame.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right),
            area,
        );

        let key = |k: char| {
            Span::styled(k.to_string(), theme::accent().add_modifier(Modifier::BOLD))
        };
        let label = |l: &str| Span::styled(format!(" {l} "), theme::faint());

        // The three facets, each showing its active value with a count of what
        // it leaves — or a muted "all" when it is not narrowing anything.
        let mut spans = vec![Span::raw(" ")];
        for (n, &facet) in Facet::ALL.iter().enumerate() {
            if n > 0 {
                spans.push(Span::raw("   "));
            }
            spans.push(key(facet.key()));
            spans.push(label(facet.label()));
            match self.filter_for(facet) {
                Some(value) => {
                    spans.push(facet_value_span(facet, value));
                    if let Some(c) = self.facet_count(facet) {
                        spans.push(Span::styled(format!(" {c}"), theme::faint()));
                    }
                }
                None => spans.push(Span::styled("all", theme::muted())),
            }
        }
        if self.any_filter() {
            spans.push(Span::raw("   "));
            spans.push(key('x'));
            spans.push(label("clear"));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The facet pop-over: a narrow line you type into and a menu of the values
    /// present, each with the count picking it would leave. It floats over the
    /// results rather than replacing them, so the list you are carving stays in
    /// view — and a search that returned eighty authors is one substring away
    /// from the one you want, not eighty presses of a cycle key.
    fn render_picker(&self, frame: &mut Frame) {
        let Some(picker) = &self.picker else {
            return;
        };
        let facet = picker.facet;
        let shown = picker.shown();

        // The "all" row plus one per surviving option, capped so the overlay
        // never grows past a comfortable slice of the screen.
        let want_rows = shown.len() + 1;
        let screen = frame.area();
        let max_list = (screen.height.saturating_sub(9)).clamp(3, 16) as usize;
        let list_rows = want_rows.min(max_list);
        // input(1) + rule(1) + list + footer(1), plus the border's two rows.
        let height = (list_rows as u16 + 5).min(screen.height.saturating_sub(2));
        let width = 52u16.min(screen.width.saturating_sub(4));
        let area = centered(screen, width, height);

        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::ACCENT))
            .style(Style::new().bg(theme::BG_ALT))
            .title(Span::styled(
                format!(" filter · {} ", facet.label()),
                Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::vertical([
            Constraint::Length(1), // the type-to-narrow line
            Constraint::Length(1), // a faint rule
            Constraint::Min(1),    // the menu
            Constraint::Length(1), // the key legend
        ])
        .split(inner);

        // The narrow line, with a live "n of m" once it is doing anything.
        let mut top = vec![Span::styled(
            "❯ ",
            Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
        )];
        if picker.query.is_empty() {
            top.push(Span::styled("type to narrow", theme::faint()));
        } else {
            top.push(Span::styled(picker.query.clone(), theme::text()));
            top.push(Span::styled("▏", theme::accent()));
        }
        let mut top_line = Paragraph::new(Line::from(top));
        if !picker.query.is_empty() {
            let count = Line::from(Span::styled(
                format!("{} of {} ", shown.len(), picker.options.len()),
                theme::faint(),
            ));
            frame.render_widget(Paragraph::new(count).alignment(Alignment::Right), rows[0]);
        }
        top_line = top_line.style(Style::new().bg(theme::BG_ALT));
        frame.render_widget(top_line, rows[0]);
        frame.render_widget(
            Paragraph::new("─".repeat(rows[1].width as usize)).style(theme::faint()),
            rows[1],
        );

        // Keep the highlight in view by scrolling the menu under it.
        let cap = rows[2].height as usize;
        let total = shown.len() + 1;
        let offset = if picker.cursor >= cap {
            (picker.cursor - cap + 1).min(total.saturating_sub(cap))
        } else {
            0
        };

        let active = self.filter_for(facet);
        let mut lines: Vec<Line> = Vec::new();
        for row in offset..(offset + cap).min(total) {
            let selected = row == picker.cursor;
            let gutter = if selected {
                Span::styled(theme::CURSOR, theme::accent())
            } else {
                Span::raw("  ")
            };
            let mut spans = vec![gutter];
            if row == 0 {
                // The clearing row. It carries a mark when nothing is filtered,
                // so "all" reads as the current state and not just an option.
                let on = active.is_none();
                spans.push(Span::styled(
                    if on { "● " } else { "  " },
                    theme::accent(),
                ));
                spans.push(Span::styled(
                    "all",
                    if on {
                        theme::text().add_modifier(Modifier::BOLD)
                    } else {
                        theme::muted()
                    },
                ));
                spans.push(Span::styled("  clear filter", theme::faint()));
            } else {
                let (value, count) = shown[row - 1];
                let on = active.is_some_and(|a| a.eq_ignore_ascii_case(value));
                spans.push(Span::styled(if on { "● " } else { "  " }, theme::accent()));
                spans.push(facet_value_span(facet, value));
                spans.push(Span::styled(format!("  {count}"), theme::faint()));
            }
            let mut line = Line::from(spans);
            if selected {
                line = line.style(theme::selected_row());
            }
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), rows[2]);

        let legend = |k: &str, l: &str| {
            [
                Span::styled(
                    format!("{k} "),
                    theme::accent().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{l}   "), theme::faint()),
            ]
        };
        let footer: Vec<Span> = [
            legend("⏎", "choose"),
            legend("↑↓", "move"),
            legend("⇥", "facet"),
            legend("esc", "close"),
        ]
        .into_iter()
        .flatten()
        .collect();
        frame.render_widget(Paragraph::new(Line::from(footer)), rows[3]);
    }

    /// The sort menu: the orders the list can take, the active one marked, and
    /// a direction caret on the highlighted row that `←`/`→` flips. Small and
    /// fixed — five keys, no typing — so it needs neither a search line nor
    /// scrolling, just the list and its legend.
    fn render_sort_menu(&self, frame: &mut Frame) {
        let Some(menu) = &self.sort_menu else {
            return;
        };
        let screen = frame.area();
        // Border(2) + the five orders + the legend row.
        let height = (Sort::CYCLE.len() as u16 + 3).min(screen.height.saturating_sub(2));
        let width = 42u16.min(screen.width.saturating_sub(4));
        let area = centered(screen, width, height);

        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme::ACCENT))
            .style(Style::new().bg(theme::BG_ALT))
            .title(Span::styled(
                " sort by ",
                Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

        let amber = Style::new().fg(theme::AMBER);
        let mut lines: Vec<Line> = Vec::new();
        for (i, &order) in Sort::CYCLE.iter().enumerate() {
            let selected = i == menu.cursor;
            let active = order == self.sort;
            let gutter = if selected {
                Span::styled(theme::CURSOR, theme::accent())
            } else {
                Span::raw("  ")
            };
            // A dot on the order the list is arranged by right now.
            let mark = Span::styled(if active { "● " } else { "  " }, theme::accent());
            let label = Span::styled(
                order.label(self.tab).to_string(),
                if selected || active {
                    theme::text().add_modifier(Modifier::BOLD)
                } else {
                    theme::muted()
                },
            );
            // The caret rides the highlighted row (the direction Enter would
            // apply) and the active row (how it is sorted now); Relevance, the
            // mirror's own order, shows none.
            let caret = if order == Sort::Relevance {
                ""
            } else if selected {
                if menu.desc { " ▼" } else { " ▲" }
            } else if active {
                if self.sort_desc { " ▼" } else { " ▲" }
            } else {
                ""
            };
            let mut line = Line::from(vec![gutter, mark, label, Span::styled(caret, amber)]);
            if selected {
                line = line.style(theme::selected_row());
            }
            lines.push(line);
        }
        frame.render_widget(Paragraph::new(lines), rows[0]);

        let legend = |k: &str, l: &str| {
            [
                Span::styled(format!("{k} "), theme::accent().add_modifier(Modifier::BOLD)),
                Span::styled(format!("{l}  "), theme::faint()),
            ]
        };
        let footer: Vec<Span> = [
            legend("⏎", "sort"),
            legend("↑↓", "move"),
            legend("←→", "dir"),
            legend("esc", "close"),
        ]
        .into_iter()
        .flatten()
        .collect();
        frame.render_widget(Paragraph::new(Line::from(footer)), rows[1]);
    }

    /// The Library tab's control strip: the sort on the right, and on the left
    /// a one-line account of the shelf — how many books, how much disk, and the
    /// handful of formats they are in — so the tab answers "what have I got"
    /// before anyone scrolls. Each format word wears its own hue, the same one
    /// the chips and the search results use.
    fn render_library_strip(&self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }

        // Right: the sort, and the filter count when a filter is hiding books.
        let mut right = self.sort_indicator();
        if !self.filter.trim().is_empty() {
            right.push(Span::styled("     ", theme::faint()));
            right.push(Span::styled(
                format!("{} of {} shown ", self.shown.len(), self.library.len()),
                Style::new().fg(theme::AMBER).add_modifier(Modifier::BOLD),
            ));
        } else {
            right.push(Span::raw(" "));
        }
        frame.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right),
            area,
        );

        // Left: the count, the total on disk, then the format breakdown.
        let count = self.library.len();
        let total: u64 = self.library.iter().map(|e| e.size).sum();
        let mut spans = vec![
            Span::raw(" "),
            Span::styled(
                format!("{count} book{}", if count == 1 { "" } else { "s" }),
                theme::muted(),
            ),
        ];
        if total > 0 {
            spans.push(Span::styled(format!("  ·  {}", human_bytes(total)), theme::faint()));
        }
        for (ext, n) in self.format_breakdown().into_iter().take(4) {
            spans.push(Span::styled("    ", theme::faint()));
            spans.push(Span::styled(format!("{n} "), theme::faint()));
            spans.push(Span::styled(ext.clone(), Style::new().fg(theme::format_color(&ext))));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// The library's formats, most common first, for the summary strip.
    fn format_breakdown(&self) -> Vec<(String, usize)> {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for entry in &self.library {
            let ext = entry.ext().to_ascii_lowercase();
            if ext.is_empty() {
                continue;
            }
            match counts.iter_mut().find(|(name, _)| *name == ext) {
                Some((_, n)) => *n += 1,
                None => counts.push((ext, 1)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        counts
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

    /// Reserve a slim scrollbar down the right edge of `area` when the list is
    /// taller than the rows on show, and return the area left for the list. A
    /// list that fits keeps the full width and gets no bar — the indicator only
    /// appears when there is somewhere else to be. The thumb rides in
    /// the accent over a faint track, and skips the header row so it lines up
    /// with the rows it is measuring.
    fn with_scrollbar(&self, frame: &mut Frame, area: Rect, len: usize, pos: usize) -> Rect {
        let rows = area.height.saturating_sub(1) as usize;
        if len <= rows || rows == 0 || area.width < 4 {
            return area;
        }
        let track = Rect {
            x: area.right() - 1,
            y: area.y + 1,
            width: 1,
            height: area.height - 1,
        };
        let mut state = ScrollbarState::new(len)
            .viewport_content_length(rows)
            .position(pos);
        let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("█")
            .track_style(theme::faint())
            .thumb_style(theme::accent());
        frame.render_stateful_widget(bar, track, &mut state);
        Rect {
            width: area.width - 1,
            ..area
        }
    }

    fn render_results(&mut self, frame: &mut Frame, area: Rect) {
        if self.visible.is_empty() {
            self.render_empty(frame, area);
            return;
        }
        let area = self.with_scrollbar(
            frame,
            area,
            self.visible.len(),
            self.table.selected().unwrap_or(0),
        );

        // Titles carry the screen, so they are the last thing to lose room and
        // never the first: the trimmable columns fall away as the list narrows,
        // widest and least essential going soonest.
        let w = area.width;
        let show_year = w >= 56;
        let show_lang = w >= 84;
        let show_status = w >= 98;

        let mut head = vec![Cell::from(""), Cell::from("TITLE"), Cell::from("AUTHOR")];
        if show_year {
            head.push(Cell::from("YEAR"));
        }
        if show_lang {
            head.push(Cell::from("LANGUAGE"));
        }
        head.push(Cell::from("SIZE"));
        head.push(Cell::from("FMT"));
        if show_status {
            head.push(Cell::from("STATUS"));
        }
        let header = Row::new(head).style(theme::header()).height(1);

        // Titles carry the information; authors repeat. Weight the room so the
        // title always wins the slack the fixed columns leave behind.
        let mut widths = vec![Constraint::Length(1), Constraint::Fill(6), Constraint::Fill(2)];
        if show_year {
            widths.push(Constraint::Length(4));
        }
        if show_lang {
            widths.push(Constraint::Length(10));
        }
        widths.push(Constraint::Length(8)); // SIZE
        widths.push(Constraint::Length(6)); // FMT
        if show_status {
            widths.push(Constraint::Length(7)); // STATUS
        }
        // The width the title and author columns will actually get, so a row can
        // decide for itself whether it needs a second line — the highlight
        // gutter eats two cells on the left of every row.
        let gutter = crate::term::display_width(theme::CURSOR) as u16;
        let cols = Layout::horizontal(widths.clone())
            .spacing(1)
            .split(Rect::new(0, 0, area.width.saturating_sub(gutter), 1));
        let title_w = cols[1].width as usize;
        let author_w = cols[2].width as usize;

        let selected = self.table.selected();
        let rows: Vec<Row> = self
            .visible
            .iter()
            .enumerate()
            .filter_map(|(row, &i)| self.results.get(i).map(|b| (row, i, b)))
            .map(|(row, i, book)| {
                let marked = self.marked.get(i).copied().unwrap_or(false);
                let job = self.jobs.get(&book.md5);
                // A book already on the shelf — from any past session, not just
                // this one — is worth knowing before you download it again. It
                // yields to a mark (you are clearly about to act on it) and to
                // this session's own download state.
                let owned = job.is_none() && !marked && self.owned.contains(&book.md5);
                let (marker, marker_style) = match job {
                    Some(Job::Saved(_)) => ("✓", Style::new().fg(theme::SUCCESS)),
                    Some(Job::Failed(_)) => ("✗", Style::new().fg(theme::DANGER)),
                    Some(Job::Running { .. }) | Some(Job::Queued) => {
                        ("↓", Style::new().fg(theme::ACCENT))
                    }
                    None if marked => ("●", Style::new().fg(theme::AMBER)),
                    None if owned => ("•", Style::new().fg(theme::SUCCESS)),
                    None => (" ", Style::new()),
                };
                // The status column carries this session's download state, and
                // otherwise a quiet note that the book is already in the library.
                let (status_text, status_style) = if owned {
                    ("library".to_string(), Style::new().fg(theme::SUCCESS))
                } else {
                    (job_label(job), job_style(job))
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

                // Title and author each spill onto a second line only when they
                // outrun their column; a row that fits stays one line, so the
                // list keeps its density and spends the extra row only where a
                // long title would otherwise be cut.
                let title_lines = wrap_two(&book.title, title_w);
                let author_lines = wrap_two(&author, author_w);
                let row_h = title_lines.len().max(author_lines.len()) as u16;

                let mut cells = vec![
                    Cell::from(marker).style(marker_style),
                    Cell::from(as_text(title_lines)).style(title_style),
                    Cell::from(as_text(author_lines)).style(theme::muted()),
                ];
                if show_year {
                    cells.push(
                        Cell::from(book.year.clone().unwrap_or_default()).style(theme::faint()),
                    );
                }
                if show_lang {
                    cells.push(
                        Cell::from(book.language.clone().unwrap_or_default())
                            .style(Style::new().fg(theme::LANG)),
                    );
                }
                cells.push(Cell::from(book.size_human()).style(theme::muted()));
                cells.push(Cell::from(format!(" {} ", book.ext())).style(chip));
                if show_status {
                    cells.push(Cell::from(status_text).style(status_style));
                }
                // The zebra: alternate rows sit one step off the canvas, so a
                // wide row can be followed without a ruler.
                Row::new(cells).height(row_h).style(Style::new().bg(if row % 2 == 1 {
                    theme::BG_ALT
                } else {
                    theme::BG
                }))
            })
            .collect();

        let table = Table::new(rows, widths)
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
            let carved: Vec<String> = Facet::ALL
                .iter()
                .filter_map(|&f| self.filter_for(f).map(str::to_string))
                .collect();
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
                    Span::styled("e l a", theme::accent().add_modifier(Modifier::BOLD)),
                    Span::styled(" adjust them", theme::faint()),
                ]),
            ];
            let block = area.inner(Margin {
                horizontal: 2,
                vertical: area.height.saturating_sub(lines.len() as u16) / 2,
            });
            frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), block);
            return;
        }

        let mut lines = vec![if self.busy {
            // A turning spinner beside the status says "working", where a
            // trailing ellipsis only ever sat still.
            Line::from(vec![
                Span::styled(format!("{} ", spinner_frame(self.spinner)), theme::accent()),
                Span::styled(self.status.clone(), theme::muted()),
            ])
        } else {
            Line::from(Span::styled(self.status.clone(), theme::muted()))
        }];
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
                Some(Job::Running {
                    done,
                    total,
                    started,
                }) => lines.push(progress_line(*done, *total, *started, 24)),
                Some(Job::Queued) => lines.push(Line::from(vec![
                    Span::styled(spinner_frame(self.spinner), theme::accent()),
                    Span::styled(" queued", theme::faint()),
                ])),
                None => lines.push(Line::from(vec![
                    Span::styled("⏎ download → ", theme::accent()),
                    Span::styled(self.settings.dest_dir.display().to_string(), theme::faint()),
                ])),
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
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
                // The spinner sits on the row that a cover's centre would fall
                // on, so the frame reads as "loading" rather than empty.
                let middle = Rect {
                    x: area.x,
                    y: area.y + area.height / 2,
                    width: area.width,
                    height: 1,
                };
                frame.render_widget(
                    Paragraph::new(spinner_frame(self.spinner))
                        .style(theme::faint())
                        .alignment(Alignment::Center),
                    middle,
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
        let area = self.with_scrollbar(
            frame,
            area,
            self.shown.len(),
            self.library_table.selected().unwrap_or(0),
        );
        let now = history::now();

        // The shelf trims the same way the results do: title and author hold on,
        // the date and size step aside when the column runs short.
        let w = area.width;
        let show_added = w >= 84;
        let show_size = w >= 60;

        let mut head = vec![Cell::from(""), Cell::from("TITLE"), Cell::from("AUTHOR")];
        if show_added {
            head.push(Cell::from("ADDED"));
        }
        if show_size {
            head.push(Cell::from("SIZE"));
        }
        head.push(Cell::from("FMT"));
        let header = Row::new(head).style(theme::header()).height(1);

        let mut widths = vec![Constraint::Length(1), Constraint::Fill(6), Constraint::Fill(2)];
        if show_added {
            widths.push(Constraint::Length(12));
        }
        if show_size {
            widths.push(Constraint::Length(9));
        }
        widths.push(Constraint::Length(6)); // FMT
        // The real title and author widths, so a shelf row wraps to a second
        // line only when its own title or author overruns the column.
        let gutter = crate::term::display_width(theme::CURSOR) as u16;
        let cols = Layout::horizontal(widths.clone())
            .spacing(1)
            .split(Rect::new(0, 0, area.width.saturating_sub(gutter), 1));
        let title_w = cols[1].width as usize;
        let author_w = cols[2].width as usize;

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

                let author = entry.first_author();
                let title_lines = wrap_two(&title, title_w);
                let author_lines = wrap_two(&author, author_w);
                let row_h = title_lines.len().max(author_lines.len()) as u16;

                let mut cells = vec![
                    Cell::from(marker).style(marker_style),
                    Cell::from(as_text(title_lines)).style(title_style),
                    Cell::from(as_text(author_lines)).style(theme::muted()),
                ];
                if show_added {
                    cells.push(Cell::from(history::when(entry.at, now)).style(theme::muted()));
                }
                if show_size {
                    cells.push(Cell::from(human_bytes(entry.size)).style(theme::faint()));
                }
                cells.push(Cell::from(format!(" {} ", entry.ext())).style(chip));
                Row::new(cells).height(row_h).style(Style::new().bg(if row % 2 == 1 {
                    theme::BG_ALT
                } else {
                    theme::BG
                }))
            })
            .collect();

        let table = Table::new(rows, widths)
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
                ("l", "lang"),
                ("a", "author"),
                ("s", "sort"),
                ("/", "search"),
                ("?", "help"),
                ("q", "quit"),
            ],
            (Mode::Browsing, Tab::Library) => &[
                ("tab", "search"),
                ("↑↓", "move"),
                ("⏎", "open"),
                ("f", "reveal"),
                ("s", "sort"),
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
                spans.push(Span::styled(format!(" {label}"), theme::muted()));
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
                    ("s", "open the sort menu; S reverses in place"),
                    ("y", "copy the MD5 to the clipboard"),
                    ("?", "this help"),
                    ("q  esc", "quit"),
                    ("^c", "quit from anywhere"),
                ],
            ),
            (
                "search tab",
                &[
                    ("⏎", "run the search, or download the selection"),
                    ("e  l  a", "filter by format / language / author"),
                    ("", "  a menu: type to narrow, ⏎ to choose"),
                    ("x", "clear every filter"),
                    ("space", "mark a result for batch download"),
                    ("A", "mark or unmark everything showing"),
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
                " tomesole · keys ",
                Style::new().fg(theme::ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(Paragraph::new(lines), inner);
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

/// A facet's value dressed in its own colour, the same one it wears in the
/// results: a format as a solid chip, a language in its calm blue, an author in
/// plain bold text (clipped, since author strings run long). Used by both the
/// strip and the picker so a value looks the same wherever it is shown.
fn facet_value_span(facet: Facet, value: &str) -> Span<'static> {
    match facet {
        Facet::Format => Span::styled(format!(" {value} "), theme::format_chip(value)),
        Facet::Language => Span::styled(
            value.to_string(),
            Style::new().fg(theme::LANG).add_modifier(Modifier::BOLD),
        ),
        Facet::Author => Span::styled(
            clip(value, 30),
            theme::text().add_modifier(Modifier::BOLD),
        ),
    }
}

/// A run of lines as cell text, so a table cell can hold a wrapped title.
fn as_text(lines: Vec<String>) -> Text<'static> {
    Text::from(lines.into_iter().map(Line::from).collect::<Vec<Line>>())
}

/// Wrap `s` into at most two lines of `width` display cells, breaking on
/// spaces. A string that already fits comes back as a single line, so a table
/// row only grows to two lines when its own text would otherwise be cut; a
/// third line's worth spills onto the second and is clipped with an ellipsis.
fn wrap_two(s: &str, width: usize) -> Vec<String> {
    use crate::term::{display_width, truncate};
    if width == 0 || display_width(s) <= width {
        return vec![s.to_string()];
    }
    let mut line = String::new();
    let mut used = 0usize;
    let mut rest = String::new();
    let mut spilling = false;
    for word in s.split(' ') {
        if spilling {
            rest.push(' ');
            rest.push_str(word);
            continue;
        }
        let ww = display_width(word);
        if line.is_empty() && ww > width {
            // A single word wider than the whole line: hard-break it so the
            // overflow still has somewhere to land.
            for c in word.chars() {
                let cw = display_width(&c.to_string());
                if used + cw > width {
                    rest.push(c);
                } else {
                    line.push(c);
                    used += cw;
                }
            }
            spilling = true;
        } else if !line.is_empty() && used + 1 + ww > width {
            rest.push_str(word);
            spilling = true;
        } else {
            if !line.is_empty() {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += ww;
        }
    }
    if rest.is_empty() {
        vec![line]
    } else {
        vec![line, truncate(&rest, width)]
    }
}

/// Shorten `s` to at most `max` characters, ending in an ellipsis when cut, so
/// a long author string cannot push a row off the edge of its box.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// The first of several semicolon-separated authors, lowercased for sorting
/// and comparison. Empty when there is no author at all.
fn first_author_of(authors: Option<&str>) -> String {
    let all = authors.unwrap_or("");
    all.split(';').next().unwrap_or(all).trim().to_lowercase()
}

/// The leading four-ish digits of a year string as a number, zero when there
/// is nothing parseable — so undated books sink to one end rather than
/// scattering. Libgen years are ragged (`1965`, `1965-01`, `c. 1965`).
fn year_num(year: Option<&str>) -> u32 {
    let s = year.unwrap_or("");
    let digits: String = s
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().unwrap_or(0)
}

fn job_label(job: Option<&Job>) -> String {
    match job {
        Some(Job::Queued) => "queued".into(),
        Some(Job::Running { done, total, .. }) => match total {
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

/// The frames of the small in-flight spinner, one braille cell each. Picked to
/// read as a smooth rotation at the few-frames-a-second the loop redraws at.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The spinner glyph for a given frame count.
fn spinner_frame(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

/// A fractional-block meter `width` cells wide. The fill is measured in
/// eighths of a cell and the boundary drawn with a partial block, so the bar
/// moves a step at a time rather than jumping a whole character — the extra
/// resolution is free and reads as far smoother.
fn meter(fraction: f64, width: usize) -> String {
    const PARTIAL: [char; 8] = [' ', '▏', '▎', '▍', '▌', '▋', '▊', '▉'];
    let eighths = (fraction.clamp(0.0, 1.0) * (width * 8) as f64).round() as usize;
    let full = eighths / 8;
    let rem = eighths % 8;
    let mut bar = "█".repeat(full.min(width));
    let mut used = full.min(width);
    if rem > 0 && used < width {
        bar.push(PARTIAL[rem]);
        used += 1;
    }
    bar.push_str(&"░".repeat(width.saturating_sub(used)));
    bar
}

/// The download line for a detail pane: a fractional-block meter, the
/// transferred and total size, and — once enough has come down to judge one —
/// a live speed and estimate of the time left. The meter rides in the accent;
/// the figures beside it stay faint, so the bar is what the eye tracks.
fn progress_line(done: u64, total: Option<u64>, started: Instant, width: usize) -> Line<'static> {
    let secs = started.elapsed().as_secs_f64();
    // A speed judged in the first half-second swings wildly; wait for it.
    let speed = (secs > 0.5).then(|| done as f64 / secs);
    let rate = match speed {
        Some(bytes) if bytes >= 1.0 => format!("{}/s", human_bytes(bytes as u64)),
        _ => String::new(),
    };

    match total {
        Some(total) if total > 0 => {
            let fraction = done as f64 / total as f64;
            // Whole seconds left at the current average, once there is a rate.
            let eta = speed
                .filter(|s| *s > 0.0)
                .map(|s| ((total.saturating_sub(done)) as f64 / s) as u64)
                .map(|left| format!("  ·  {} left", short_duration(left)))
                .unwrap_or_default();
            let tail = format!(
                "  {} / {}{}{}",
                human_bytes(done),
                human_bytes(total),
                if rate.is_empty() {
                    String::new()
                } else {
                    format!("  ·  {rate}")
                },
                eta,
            );
            Line::from(vec![
                Span::styled(meter(fraction, width), theme::accent()),
                Span::styled(tail, theme::faint()),
            ])
        }
        // No total advertised: no bar to draw, just what has arrived so far.
        _ => {
            let tail = if rate.is_empty() {
                String::new()
            } else {
                format!("  ·  {rate}")
            };
            Line::from(vec![
                Span::styled(format!("downloading… {}", human_bytes(done)), theme::accent()),
                Span::styled(tail, theme::faint()),
            ])
        }
    }
}

/// A compact duration for an ETA: `45s`, `3m 20s`, `1h 05m`.
fn short_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Put text on the system clipboard with an OSC 52 escape. Base64 is the
/// encoding the sequence mandates, and the crate already has an encoder for the
/// image protocols, so this borrows it rather than growing a dependency.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    let escape = format!("\x1b]52;c;{}\x07", graphics::base64(text.as_bytes()));
    let mut stdout = std::io::stdout();
    let _ = write!(stdout, "{escape}");
    let _ = stdout.flush();
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
            author_filter: None,
            picker: None,
            sort_menu: None,
            sort: Sort::Relevance,
            sort_desc: false,
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
            owned: HashSet::new(),
            library_table: TableState::default(),
            // Tests draw to an off-screen buffer, so nothing may try to write
            // an escape sequence to a terminal that is not there.
            protocol: Protocol::None,
            covers: HashMap::new(),
            cover_due: None,
            painted: None,
            cover_area: None,
            spinner: 0,
        }
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Drive the facet picker the way a person would: its key opens the menu,
    /// the typed text narrows it, and Enter commits the highlighted row (the
    /// first match once anything is typed).
    fn pick(app: &mut App, facet_key: char, query: &str) {
        press(app, KeyCode::Char(facet_key));
        for c in query.chars() {
            press(app, KeyCode::Char(c));
        }
        press(app, KeyCode::Enter);
    }

    /// Drive the sort menu: open it, walk the highlight to the wanted order, and
    /// commit — taking that order's natural direction, the same default the old
    /// `s` cycle used.
    fn sort_to(app: &mut App, want: Sort) {
        press(app, KeyCode::Char('s'));
        let target = Sort::CYCLE.iter().position(|s| *s == want).unwrap();
        loop {
            let at = app.sort_menu.as_ref().unwrap().cursor;
            if at == target {
                break;
            }
            press(app, if at < target { KeyCode::Down } else { KeyCode::Up });
        }
        press(app, KeyCode::Enter);
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

    /// The titles currently on screen, in display order.
    fn titles(a: &App) -> Vec<String> {
        a.visible
            .iter()
            .map(|&i| a.results[i].title.clone())
            .collect()
    }

    fn book(md5: &str, title: &str, year: &str, size: u64) -> Book {
        Book {
            md5: md5.into(),
            title: title.into(),
            year: Some(year.into()),
            size_bytes: Some(size),
            ..Default::default()
        }
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
    fn capital_a_toggles_every_mark() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, books(3));
        press(&mut a, KeyCode::Char('A'));
        assert_eq!(a.marked, [true, true, true]);
        press(&mut a, KeyCode::Char('A'));
        assert_eq!(a.marked, [false, false, false]);
    }

    #[test]
    fn sorting_reorders_the_list_and_keeps_the_selection() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(
            &mut a,
            vec![
                book("00000000000000000000000000000001", "Charlie", "2001", 300),
                book("00000000000000000000000000000002", "alpha", "1999", 100),
                book("00000000000000000000000000000003", "Bravo", "2010", 200),
            ],
        );
        // Relevance is the order the mirror returned.
        assert_eq!(titles(&a), ["Charlie", "alpha", "Bravo"]);

        // Rest the cursor on a book, then sort by title: it must follow.
        a.table.select(Some(1));
        assert_eq!(a.selected().unwrap().title, "alpha");
        sort_to(&mut a, Sort::Title); // ascending, case-insensitive
        assert_eq!(a.sort, Sort::Title);
        assert_eq!(titles(&a), ["alpha", "Bravo", "Charlie"]);
        assert_eq!(
            a.selected().unwrap().title,
            "alpha",
            "the highlight tracks its book across a re-sort"
        );

        sort_to(&mut a, Sort::Year); // newest first by default
        assert_eq!(a.sort, Sort::Year);
        assert_eq!(titles(&a), ["Bravo", "Charlie", "alpha"]);

        sort_to(&mut a, Sort::Size); // biggest first by default
        assert_eq!(titles(&a), ["Charlie", "Bravo", "alpha"]);
        a.reverse_sort(); // smallest first
        assert_eq!(titles(&a), ["alpha", "Bravo", "Charlie"]);

        sort_to(&mut a, Sort::Relevance); // the mirror's own order again
        assert_eq!(a.sort, Sort::Relevance);
        assert_eq!(titles(&a), ["Charlie", "alpha", "Bravo"]);
    }

    #[test]
    fn a_result_already_in_the_library_is_flagged() {
        let mut a = app();
        a.mode = Mode::Browsing;
        let owned = "1b9159991f7fb1b3910c0be9ebf7e595";
        a.owned.insert(owned.into());
        install(
            &mut a,
            vec![
                Book {
                    md5: owned.into(),
                    title: "Owned Book".into(),
                    extension: Some("epub".into()),
                    ..Default::default()
                },
                Book {
                    md5: "00000000000000000000000000000009".into(),
                    title: "New Book".into(),
                    extension: Some("epub".into()),
                    ..Default::default()
                },
            ],
        );
        let screen = draw(&mut a, 100, 20);
        let owned_row = screen
            .lines()
            .find(|l| l.contains("Owned Book"))
            .unwrap_or("");
        let new_row = screen.lines().find(|l| l.contains("New Book")).unwrap_or("");
        assert!(
            owned_row.contains("library"),
            "an owned result should say so: {screen}"
        );
        assert!(
            !new_row.contains("library"),
            "a fresh result must not: {screen}"
        );
    }

    #[test]
    fn a_marked_batch_reports_its_combined_size() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(
            &mut a,
            vec![
                book("00000000000000000000000000000001", "A", "2000", 1024 * 1024),
                book(
                    "00000000000000000000000000000002",
                    "B",
                    "2000",
                    3 * 1024 * 1024,
                ),
            ],
        );
        assert!(a.marked_summary().is_none());
        a.marked = vec![true, true];
        assert_eq!(a.marked_summary(), Some((2, 4 * 1024 * 1024)));
        let screen = draw(&mut a, 100, 20);
        assert!(screen.contains("2 marked"), "{screen}");
    }

    #[test]
    fn y_copies_the_md5_to_the_status_line() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, books(2));
        a.table.select(Some(1));
        press(&mut a, KeyCode::Char('y'));
        assert!(
            a.status.contains(&a.results[1].md5),
            "the md5 should be named: {}",
            a.status
        );
        assert!(a.status.contains("clipboard"));
    }

    #[test]
    fn the_library_strip_summarises_the_shelf() {
        let mut a = app();
        a.mode = Mode::Browsing;
        a.tab = Tab::Library;
        a.library = entries(3); // three 1 MB epubs
        a.owned = a.library.iter().map(|e| e.md5.clone()).collect();
        a.refilter();
        let screen = draw(&mut a, 100, 20);
        assert!(screen.contains("3 books"), "count missing: {screen}");
        assert!(screen.contains("epub"), "format breakdown missing: {screen}");
    }

    #[test]
    fn a_list_taller_than_the_screen_grows_a_scroll_indicator() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, books(50));
        // A short terminal cannot show fifty rows, so the thumb appears. The
        // full block is drawn nowhere else when no download is in flight.
        let screen = draw(&mut a, 80, 12);
        assert!(screen.contains('█'), "scroll thumb missing: {screen}");
        // A list that fits keeps the whole width — no thumb.
        install(&mut a, books(3));
        let roomy = draw(&mut a, 80, 24);
        assert!(!roomy.contains('█'), "no scrollbar when it all fits: {roomy}");
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
                started: Instant::now(),
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
    fn the_sort_menu_opens_on_the_current_order() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        a.sort = Sort::Year;
        press(&mut a, KeyCode::Char('s'));
        let menu = a.sort_menu.as_ref().expect("menu is up");
        assert_eq!(Sort::CYCLE[menu.cursor], Sort::Year, "highlight starts on the order in force");
        let screen = draw(&mut a, 90, 24);
        assert!(screen.contains("sort by"), "{screen}");
        assert!(screen.contains("relevance") && screen.contains("size"), "{screen}");
    }

    #[test]
    fn the_sort_menu_selects_a_column_directly() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        // From relevance, open and jump straight to Size — one choice, not four
        // steps through the orders in between.
        sort_to(&mut a, Sort::Size);
        assert_eq!(a.sort, Sort::Size);
        assert!(a.sort_desc, "size opens biggest-first by default");
        assert!(a.sort_menu.is_none(), "committing closes the menu");
    }

    #[test]
    fn the_sort_menu_flips_direction_in_place() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        a.sort = Sort::Title;
        press(&mut a, KeyCode::Char('s')); // opens on Title, ascending
        press(&mut a, KeyCode::Left); // flip to descending
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.sort, Sort::Title);
        assert!(a.sort_desc, "the direction key set it descending");
    }

    #[test]
    fn esc_leaves_the_sort_untouched() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        press(&mut a, KeyCode::Char('s'));
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Down);
        press(&mut a, KeyCode::Esc);
        assert!(a.sort_menu.is_none());
        assert_eq!(a.sort, Sort::Relevance, "backing out changes nothing");
    }

    #[test]
    fn the_picker_lists_the_values_present_with_counts() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        press(&mut a, KeyCode::Char('a')); // author picker
        let screen = draw(&mut a, 100, 30);
        assert!(screen.contains("filter · author"), "{screen}");
        assert!(screen.contains("clear filter"), "{screen}");
        // Every author present, most common first, each with its count.
        assert!(screen.contains("Marx, Karl"), "{screen}");
        assert!(screen.contains("Le Guin, Ursula"), "{screen}");
        assert!(screen.contains("choose"), "the key legend is shown: {screen}");

        // Typing narrows the menu in place rather than cycling values past:
        // "guin" leaves one of the three authors on the menu.
        for c in "guin".chars() {
            press(&mut a, KeyCode::Char(c));
        }
        let screen = draw(&mut a, 100, 30);
        assert!(screen.contains("Le Guin, Ursula"), "{screen}");
        assert!(screen.contains("1 of 3"), "the live match count: {screen}");
    }

    #[test]
    fn tab_rotates_the_open_picker_between_facets() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        press(&mut a, KeyCode::Char('e'));
        assert_eq!(a.picker.as_ref().map(|p| p.facet), Some(Facet::Format));
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.picker.as_ref().map(|p| p.facet), Some(Facet::Language));
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.picker.as_ref().map(|p| p.facet), Some(Facet::Author));
        press(&mut a, KeyCode::Tab);
        assert_eq!(a.picker.as_ref().map(|p| p.facet), Some(Facet::Format), "wraps");
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
                started: Instant::now(),
            },
        );
        let screen = draw(&mut a, 100, 20);
        assert!(screen.contains("50%"), "row progress missing: {screen}");
        assert!(screen.contains("█"), "detail bar missing: {screen}");
    }

    #[test]
    fn the_meter_stays_within_its_width() {
        // A half-full ten-cell meter is five full blocks and five track cells.
        let bar = meter(0.5, 10);
        assert_eq!(bar.chars().filter(|c| *c == '█').count(), 5);
        assert_eq!(bar.chars().filter(|c| *c == '░').count(), 5);
        // The fractional boundary never overruns the requested width.
        assert_eq!(meter(0.37, 10).chars().count(), 10);
        assert_eq!(meter(0.0, 8).chars().filter(|c| *c == '░').count(), 8);
        assert_eq!(meter(1.0, 8).chars().filter(|c| *c == '█').count(), 8);
        // Over-full is clamped rather than panicking.
        assert_eq!(meter(2.0, 6).chars().count(), 6);
    }

    #[test]
    fn an_eta_reads_compactly() {
        assert_eq!(short_duration(45), "45s");
        assert_eq!(short_duration(200), "3m 20s");
        assert_eq!(short_duration(3900), "1h 05m");
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

        // The bottom band grows a cover column beside the file facts, wide or
        // narrow — the same horizontal layout at either size.
        for (w, h) in [(130u16, 40u16), (90, 24)] {
            let screen = draw(&mut a, w, h);
            assert!(
                screen.contains(graphics::UPPER_HALF),
                "{w}x{h}: the library cover should draw as half blocks: {screen}"
            );
            assert!(screen.contains("Book 0"), "{w}x{h}: {screen}");
        }
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
    /// languages, formats and authors mixed together.
    fn polyglot() -> Vec<Book> {
        let entry = |i: usize, lang: &str, ext: &str, author: &str| Book {
            md5: format!("{i:032x}"),
            title: format!("Das Kapital {i}"),
            language: Some(lang.into()),
            extension: Some(ext.into()),
            authors: Some(author.into()),
            ..Default::default()
        };
        vec![
            entry(0, "German", "pdf", "Marx, Karl"),
            entry(1, "German", "epub", "Marx, Karl"),
            entry(2, "English", "epub", "Le Guin, Ursula"),
            entry(3, "German", "pdf", "Marx, Karl"),
            entry(4, "English", "pdf", "Engels, Friedrich"),
            entry(5, "Spanish", "djvu", "Le Guin, Ursula"),
        ]
    }

    #[test]
    fn the_language_picker_narrows_the_results() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        assert_eq!(a.visible.len(), 6);

        // One gesture reaches any value: open, type a fragment, Enter — no
        // stepping through the languages in between.
        pick(&mut a, 'l', "english");
        assert_eq!(a.lang_filter.as_deref(), Some("English"));
        assert_eq!(a.visible.len(), 2);
        assert!(a.visible.iter().all(|&i| a.results[i].language.as_deref() == Some("English")));

        // Reopen and take the "all" row at the top to clear it.
        press(&mut a, KeyCode::Char('l'));
        press(&mut a, KeyCode::Home);
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.lang_filter, None);
        assert_eq!(a.visible.len(), 6);
    }

    #[test]
    fn open_then_enter_takes_the_most_common_value() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        // No filter set yet: the highlight opens on the top value, so the
        // quickest gesture — open, Enter — grabs the obvious pick.
        press(&mut a, KeyCode::Char('l'));
        press(&mut a, KeyCode::Enter);
        assert_eq!(a.lang_filter.as_deref(), Some("German"));
        assert_eq!(a.visible.len(), 3);
    }

    #[test]
    fn opening_a_picker_does_not_leak_keys_to_the_list() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        press(&mut a, KeyCode::Char('l')); // picker up
        assert!(a.picker.is_some());
        // A letter narrows the menu; it must not also start a search or move
        // the list underneath.
        press(&mut a, KeyCode::Char('g'));
        assert_eq!(a.picker.as_ref().unwrap().query, "g");
        press(&mut a, KeyCode::Esc);
        assert!(a.picker.is_none());
        assert_eq!(a.lang_filter, None, "esc backs out without applying");
    }

    #[test]
    fn the_author_picker_filters_by_name() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        // Three editions are Marx's; a fragment of the name is enough.
        pick(&mut a, 'a', "marx");
        assert_eq!(a.author_filter.as_deref(), Some("Marx, Karl"));
        assert_eq!(a.visible.len(), 3);
        assert!(a.visible.iter().all(|&i| a.results[i]
            .authors
            .as_deref()
            .unwrap()
            .contains("Marx")));
    }

    #[test]
    fn an_author_fragment_reaches_the_full_name() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        // "guin" is only part of the listed "Le Guin, Ursula", but typing it
        // narrows the menu to that one row and Enter commits the whole value —
        // which then matches both Le Guin editions.
        pick(&mut a, 'a', "guin");
        assert_eq!(a.author_filter.as_deref(), Some("Le Guin, Ursula"));
        assert_eq!(a.visible.len(), 2);
    }

    #[test]
    fn the_three_filters_compose_and_x_clears_them() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());

        pick(&mut a, 'l', "german");
        pick(&mut a, 'e', "pdf");
        pick(&mut a, 'a', "marx");
        assert_eq!(a.fmt_filter.as_deref(), Some("pdf"));
        assert_eq!(a.visible.len(), 2, "German Marx pdfs only");

        press(&mut a, KeyCode::Char('x'));
        assert!(!a.any_filter());
        assert_eq!(a.visible.len(), 6);
    }

    #[test]
    fn facet_counts_respect_the_other_filter() {
        let mut a = app();
        install(&mut a, polyglot());
        a.lang_filter = Some("English".into());
        a.refine();
        // With English active there is 1 epub and 1 pdf to offer, not 2 and 3.
        let formats = a.facet_options(Facet::Format);
        assert!(formats.iter().all(|(_, n)| *n == 1), "{formats:?}");
    }

    #[test]
    fn filters_survive_a_new_search() {
        let mut a = app();
        a.mode = Mode::Browsing;
        install(&mut a, polyglot());
        pick(&mut a, 'l', "english");
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
        pick(&mut a, 'l', "english"); // English: results 2 and 4

        assert_eq!(a.selected().map(|b| b.md5.clone()).as_deref(), Some("00000000000000000000000000000002"));
        press(&mut a, KeyCode::Down);
        assert_eq!(a.selected().map(|b| b.md5.clone()).as_deref(), Some("00000000000000000000000000000004"));
        press(&mut a, KeyCode::Down);
        assert_eq!(a.table.selected(), Some(1), "two rows showing, no further");

        // `A` marks only what is showing.
        press(&mut a, KeyCode::Char('A'));
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
        pick(&mut a, 'e', "pdf");
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
        assert!(screen.contains("author"), "{screen}");

        pick(&mut a, 'l', "english");
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
    fn the_selection_reads_along_the_bottom_band() {
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

        // The record for the selected row lies in the horizontal band across the
        // bottom, so the results keep the whole width — at any terminal size the
        // title, the publisher and the checksum are all there to read.
        for (w, h) in [(130u16, 40u16), (90, 24)] {
            let screen = draw(&mut a, w, h);
            assert!(screen.contains("The Dispossessed"), "{w}x{h}: {screen}");
            assert!(screen.contains("Harper & Row"), "{w}x{h}: {screen}");
            assert!(
                screen.contains("1b9159991f7fb1b3910c0be9ebf7e595"),
                "{w}x{h}: {screen}"
            );
        }
    }

    /// Render the app to styled HTML for out-of-terminal design review.
    /// Dev-only: runs when TOMESOLE_PREVIEW_DIR is set, via `--ignored`.
    #[test]
    #[ignore]
    fn dump_design_previews() {
        use ratatui::backend::TestBackend;
        use std::fmt::Write as _;

        let Some(dir) = std::env::var_os("TOMESOLE_PREVIEW_DIR") else {
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
             </style><h1>tomesole — TUI states</h1>",
        );

        // 1. Wide search, cover art, one download running, one saved.
        let mut a = app();
        a.mode = Mode::Browsing;
        a.mirror_label = "libgen.li".into();
        a.protocol = Protocol::Blocks;
        a.library = entries(7); // so the tab bar carries a believable count
        // The first few library entries share the leading MD5s with the search
        // results, so those editions show as already on the shelf.
        a.owned = a.library.iter().map(|e| e.md5.clone()).collect();
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
                started: Instant::now(),
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

        // 3. Both filters: English epubs, sorted biggest-first to show the
        // sort indicator carrying a direction.
        a.fmt_filter = Some("epub".into());
        a.sort = Sort::Size;
        a.sort_desc = true;
        a.refine();
        page.push_str(&html(&mut a, 132, 42, "Search — English epubs, by size"));

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
                total: Some(100),
                started: Instant::now(),
            })),
            " 25%"
        );
    }
}
