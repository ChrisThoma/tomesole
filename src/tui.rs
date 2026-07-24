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

use crate::error::Result;
use crate::mirror::{Pool, ResolveOptions};
use crate::model::{Book, SearchQuery, human_bytes};
use crate::net::Http;
use crate::{Settings, download, libgen, mirror, net};

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
}

/// Per-download state, keyed by MD5.
#[derive(Clone)]
enum Job {
    Queued,
    Running { done: u64, total: Option<u64> },
    Saved(String),
    Failed(String),
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Mode {
    Editing,
    Browsing,
    Help,
}

pub struct App {
    settings: Settings,
    mode: Mode,
    query: String,
    /// Caret position within `query`, as a character index.
    caret: usize,
    results: Vec<Book>,
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
    let mut app = App {
        settings,
        mode: if query.is_empty() {
            Mode::Editing
        } else {
            Mode::Browsing
        },
        caret: query.chars().count(),
        query,
        results: Vec::new(),
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
    };
    app.pending_search = !app.query.is_empty();
    app.spawn_mirror_resolve(app.settings.refresh_mirrors);

    let mut terminal = ratatui::try_init()?;
    let outcome = app.event_loop(&mut terminal, &rx);
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
            terminal.draw(|frame| self.render(frame))?;
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

        self.busy = true;
        self.error = None;
        self.status = format!("searching for “{terms}”");

        let mut query = SearchQuery::new(terms);
        query.limit = 100;

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
            match self.table.selected().and_then(|i| self.results.get(i)) {
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
                    Ok((outcome, _)) => Ok(outcome
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(Ev::Finished {
                    md5: book.md5.clone(),
                    outcome,
                });
            }
        });
    }

    // --- event handling ---------------------------------------------------

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Redraw => {}
            Ev::Key(key) => self.on_key(key),
            Ev::Mirrors(Ok(mirrors)) => {
                self.busy = false;
                self.mirror_label = mirrors
                    .first()
                    .and_then(|u| net::host_of(u).ok())
                    .unwrap_or_else(|| "none".into());
                let count = mirrors.len();
                self.mirrors = mirrors;
                self.status = format!("{count} mirror(s) available");
                if self.pending_search {
                    self.pending_search = false;
                    self.spawn_search();
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
                self.status = if books.is_empty() {
                    "nothing found — try fewer words".into()
                } else {
                    format!("{} result(s)", books.len())
                };
                self.table
                    .select(if books.is_empty() { None } else { Some(0) });
                self.results = books;
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
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        // Ctrl-C always quits, whatever mode we are in.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match self.mode {
            Mode::Help => self.mode = Mode::Browsing,
            Mode::Editing => self.on_key_editing(key),
            Mode::Browsing => self.on_key_browsing(key),
        }
    }

    fn on_key_editing(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => {
                self.mode = Mode::Browsing;
                self.spawn_search();
            }
            KeyCode::Esc => self.mode = Mode::Browsing,
            KeyCode::Char('u') if ctrl => {
                self.query.clear();
                self.caret = 0;
            }
            KeyCode::Char('w') if ctrl => self.delete_word(),
            KeyCode::Char(c) => {
                let at = self.byte_offset(self.caret);
                self.query.insert(at, c);
                self.caret += 1;
            }
            KeyCode::Backspace if self.caret > 0 => {
                let at = self.byte_offset(self.caret - 1);
                self.query.remove(at);
                self.caret -= 1;
            }
            KeyCode::Delete if self.caret < self.query.chars().count() => {
                let at = self.byte_offset(self.caret);
                self.query.remove(at);
            }
            KeyCode::Left => self.caret = self.caret.saturating_sub(1),
            KeyCode::Right => self.caret = (self.caret + 1).min(self.query.chars().count()),
            KeyCode::Home => self.caret = 0,
            KeyCode::End => self.caret = self.query.chars().count(),
            _ => {}
        }
    }

    fn on_key_browsing(&mut self, key: KeyEvent) {
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
            KeyCode::Home | KeyCode::Char('g') if !self.results.is_empty() => {
                self.table.select(Some(0));
            }
            KeyCode::End | KeyCode::Char('G') if !self.results.is_empty() => {
                self.table.select(Some(self.results.len() - 1));
            }
            KeyCode::Char(' ') => {
                if let Some(i) = self.table.selected()
                    && let Some(flag) = self.marked.get_mut(i)
                {
                    *flag = !*flag;
                }
                self.move_by(1);
            }
            KeyCode::Char('a') => {
                let all = self.marked.iter().all(|m| *m);
                self.marked.iter_mut().for_each(|m| *m = !all);
            }
            KeyCode::Enter => self.spawn_downloads(),
            KeyCode::Char('r') => self.spawn_search(),
            KeyCode::Char('m') => self.spawn_mirror_resolve(true),
            _ => {}
        }
    }

    fn move_by(&mut self, delta: isize) {
        if self.results.is_empty() {
            return;
        }
        let last = self.results.len() - 1;
        let current = self.table.selected().unwrap_or(0) as isize;
        self.table
            .select(Some((current + delta).clamp(0, last as isize) as usize));
    }

    fn delete_word(&mut self) {
        let head: String = self.query.chars().take(self.caret).collect();
        let trimmed = head.trim_end();
        let cut = match trimmed.rfind(' ') {
            Some(i) => trimmed[..i + 1].chars().count(),
            None => 0,
        };
        let tail: String = self.query.chars().skip(self.caret).collect();
        let kept: String = self.query.chars().take(cut).collect();
        self.query = format!("{kept}{tail}");
        self.caret = cut;
    }

    /// Byte offset of the given character index, for editing in place.
    fn byte_offset(&self, chars: usize) -> usize {
        self.query
            .char_indices()
            .nth(chars)
            .map(|(i, _)| i)
            .unwrap_or(self.query.len())
    }

    // --- rendering --------------------------------------------------------

    fn render(&mut self, frame: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(3), // search box
            Constraint::Min(5),    // results
            Constraint::Length(7), // details: 5 lines inside the border
            Constraint::Length(1), // key hints
        ])
        .split(frame.area());

        self.render_search(frame, chunks[0]);
        self.render_results(frame, chunks[1]);
        self.render_details(frame, chunks[2]);
        self.render_hints(frame, chunks[3]);

        if self.mode == Mode::Help {
            self.render_help(frame);
        }
    }

    fn render_search(&self, frame: &mut Frame, area: Rect) {
        let editing = self.mode == Mode::Editing;
        let accent = if editing {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(accent))
            .title(Line::from(vec![Span::styled(
                " clibgen ",
                Style::new().fg(Color::Cyan).bold(),
            )]))
            .title_top(
                Line::from(Span::styled(
                    format!(" {} ", self.mirror_label),
                    Style::new().fg(Color::DarkGray),
                ))
                .right_aligned(),
            );

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let text = if self.query.is_empty() && !editing {
            Span::styled("press / to search", Style::new().fg(Color::DarkGray))
        } else {
            Span::raw(self.query.as_str())
        };
        frame.render_widget(Paragraph::new(Line::from(text)), inner);

        if editing {
            // Place the real terminal cursor so it blinks in the right column.
            let offset: usize = self
                .query
                .chars()
                .take(self.caret)
                .map(|c| crate::term::display_width(&c.to_string()))
                .sum();
            frame.set_cursor_position((inner.x + offset as u16, inner.y));
        }
    }

    fn render_results(&mut self, frame: &mut Frame, area: Rect) {
        if self.results.is_empty() {
            let message = if self.busy {
                format!("  {}…", self.status)
            } else {
                format!("  {}", self.status)
            };
            frame.render_widget(
                Paragraph::new(message).style(Style::new().fg(Color::DarkGray)),
                area,
            );
            return;
        }

        let header = Row::new([
            Cell::from(""),
            Cell::from("Title"),
            Cell::from("Author"),
            Cell::from("Year"),
            Cell::from("Lang"),
            Cell::from("Size"),
            Cell::from("Fmt"),
            Cell::from("Status"),
        ])
        .style(Style::new().fg(Color::DarkGray))
        .height(1);

        let rows: Vec<Row> = self
            .results
            .iter()
            .enumerate()
            .map(|(i, book)| {
                let marked = self.marked.get(i).copied().unwrap_or(false);
                let job = self.jobs.get(&book.md5);
                let (marker, marker_style) = match job {
                    Some(Job::Saved(_)) => ("✓", Style::new().fg(Color::Green)),
                    Some(Job::Failed(_)) => ("✗", Style::new().fg(Color::Red)),
                    Some(Job::Running { .. }) | Some(Job::Queued) => {
                        ("↓", Style::new().fg(Color::Cyan))
                    }
                    None if marked => ("•", Style::new().fg(Color::Cyan)),
                    None => (" ", Style::new()),
                };

                let author = {
                    let all = book.authors_or_unknown();
                    all.split(';').next().unwrap_or(all).trim().to_string()
                };

                Row::new([
                    Cell::from(marker).style(marker_style),
                    Cell::from(book.title.clone()),
                    Cell::from(author).style(Style::new().fg(Color::Gray)),
                    Cell::from(book.year.clone().unwrap_or_default())
                        .style(Style::new().fg(Color::DarkGray)),
                    Cell::from(book.language.clone().unwrap_or_default())
                        .style(Style::new().fg(Color::DarkGray)),
                    Cell::from(book.size_human()).style(Style::new().fg(Color::DarkGray)),
                    Cell::from(book.ext().to_string()).style(Style::new().fg(Color::Green)),
                    Cell::from(job_label(job)).style(job_style(job)),
                ])
            })
            .collect();

        let table = Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Fill(3),
                Constraint::Fill(2),
                Constraint::Length(4),
                Constraint::Length(8),
                Constraint::Length(9),
                Constraint::Length(5),
                Constraint::Length(11),
            ],
        )
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::new().bg(Color::Indexed(236)).bold())
        .highlight_symbol("");

        frame.render_stateful_widget(table, area, &mut self.table);
    }

    fn render_details(&self, frame: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::DarkGray));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(book) = self.table.selected().and_then(|i| self.results.get(i)) else {
            let hint = self
                .error
                .clone()
                .unwrap_or_else(|| "no selection".to_string());
            let style = if self.error.is_some() {
                Style::new().fg(Color::Red)
            } else {
                Style::new().fg(Color::DarkGray)
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
                Style::new().fg(Color::White).bold(),
            )),
            Line::from(Span::styled(
                book.authors_or_unknown().to_string(),
                Style::new().fg(Color::Gray),
            )),
            Line::from(Span::styled(
                format!("md5 {}", book.md5),
                Style::new().fg(Color::DarkGray),
            )),
        ];
        let facts = facts.join("  ·  ");
        if !facts.trim().is_empty() {
            lines.insert(
                2,
                Line::from(Span::styled(facts, Style::new().fg(Color::DarkGray))),
            );
        }

        // The most urgent thing about the selection goes last, where the eye
        // lands: an error, else this file's download state.
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                error.clone(),
                Style::new().fg(Color::Red),
            )));
        } else {
            match self.jobs.get(&book.md5) {
                Some(Job::Saved(name)) => lines.push(Line::from(Span::styled(
                    format!("✓ saved as {name} — MD5 verified"),
                    Style::new().fg(Color::Green),
                ))),
                Some(Job::Failed(e)) => lines.push(Line::from(Span::styled(
                    format!("✗ {e}"),
                    Style::new().fg(Color::Red),
                ))),
                Some(Job::Running { done, total }) => lines.push(Line::from(Span::styled(
                    progress_bar(*done, *total, 28),
                    Style::new().fg(Color::Cyan),
                ))),
                Some(Job::Queued) => lines.push(Line::from(Span::styled(
                    "queued",
                    Style::new().fg(Color::DarkGray),
                ))),
                None => lines.push(Line::from(Span::styled(
                    self.settings.dest_dir.display().to_string(),
                    Style::new().fg(Color::DarkGray),
                ))),
            }
        }

        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    }

    fn render_hints(&self, frame: &mut Frame, area: Rect) {
        let hints = match self.mode {
            Mode::Editing => "⏎ search   esc cancel   ^u clear   ^w delete word",
            Mode::Help => "any key to close",
            Mode::Browsing => {
                "↑↓ move   space mark   ⏎ download   / search   r retry   m mirrors   ? help   q quit"
            }
        };
        frame.render_widget(
            Paragraph::new(format!(" {hints}")).style(Style::new().fg(Color::DarkGray)),
            area,
        );
    }

    fn render_help(&self, frame: &mut Frame) {
        let entries: [(&str, &str); 12] = [
            ("/  i", "edit the search query"),
            ("⏎", "in search: run it; in list: download"),
            ("↑ ↓  k j", "move the selection"),
            ("PgUp PgDn", "move ten at a time"),
            ("g  G", "jump to first / last"),
            ("space", "mark a result for batch download"),
            ("a", "mark or unmark everything"),
            ("r", "run the search again"),
            ("m", "re-probe mirrors and pick a new one"),
            ("?", "this help"),
            ("q  esc", "quit"),
            ("^c", "quit from anywhere"),
        ];

        let width = 56u16.min(frame.area().width.saturating_sub(4));
        let height = (entries.len() as u16 + 4).min(frame.area().height.saturating_sub(2));
        let area = centered(frame.area(), width, height);

        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(Color::Cyan))
            .title(Span::styled(" keys ", Style::new().fg(Color::Cyan).bold()));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let lines: Vec<Line> = entries
            .iter()
            .map(|(keys, what)| {
                Line::from(vec![
                    Span::styled(format!("{keys:<11}"), Style::new().fg(Color::Cyan)),
                    Span::styled((*what).to_string(), Style::new().fg(Color::Gray)),
                ])
            })
            .collect();
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
        Some(Job::Saved(_)) => Style::new().fg(Color::Green),
        Some(Job::Failed(_)) => Style::new().fg(Color::Red),
        Some(_) => Style::new().fg(Color::Cyan),
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
            },
            mode: Mode::Editing,
            query: String::new(),
            caret: 0,
            results: Vec::new(),
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
        a.results = books(3);
        a.marked = vec![false; 3];
        a.table.select(Some(0));

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
        a.results = books(3);
        a.marked = vec![false; 3];
        a.table.select(Some(0));

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
        a.results = books(3);
        a.marked = vec![false; 3];
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
        a.results = vec![Book {
            md5: "1b9159991f7fb1b3910c0be9ebf7e595".into(),
            title: "The Rust Programming Language".into(),
            authors: Some("Klabnik, Steve;Nichols, Carol".into()),
            year: Some("2019".into()),
            language: Some("English".into()),
            extension: Some("epub".into()),
            size_bytes: Some(3 * 1024 * 1024),
            ..Default::default()
        }];
        a.marked = vec![false];
        a.table.select(Some(0));

        let screen = draw(&mut a, 100, 24);
        assert!(screen.contains("clibgen"), "{screen}");
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
            a.results = books(30);
            a.marked = vec![false; 30];
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
        a.results = books(1);
        a.marked = vec![false];
        a.table.select(Some(0));
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
