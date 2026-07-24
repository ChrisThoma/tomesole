//! tomesole — a small, careful command-line client for Library Genesis.
//!
//! Two things make this more than a `curl` wrapper, and both exist because
//! Libgen is an unreliable, third-party-operated target:
//!
//! * **Mirror selection** ([`mirror`]) treats "is this mirror up?" as "can it
//!   actually run a search?", because mirrors routinely serve a working front
//!   page over a broken search endpoint.
//! * **The download path** ([`download`]) treats every byte, header, and
//!   filename as attacker-controlled, and verifies each file against the MD5
//!   the catalogue published before letting it reach its final name.

mod args;
mod config;
mod cover;
mod download;
mod embedded;
mod error;
mod graphics;
mod history;
mod html;
mod jpeg;
mod launch;
mod libgen;
mod md5;
mod mirror;
mod model;
mod net;
mod query;
mod term;
mod tui;
mod ui;

use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Duration;

use ureq::http::Uri;

use crate::args::{Cli, Command, Global};
use crate::config::Config;
use crate::error::Result;
use crate::mirror::{Pool, ResolveOptions};
use crate::model::{Book, SearchQuery};
use crate::net::{Http, NetPolicy};
use crate::term::{Spinner, Style};

/// Default ceiling on a single download.
const DEFAULT_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();

    // Colour for the error path has to be decided before parsing, since
    // parsing is one of the things that can fail.
    let plain = argv
        .iter()
        .any(|a| a == "--no-color" || a == "--no-colour" || a == "--plain");
    let style = Style::detect(plain);

    let code = match run(&argv, &style) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("\n  {} {e}\n", style.red("error:"));
            1
        }
    };
    std::process::exit(code);
}

fn run(argv: &[String], style: &Style) -> Result<i32> {
    let cli = args::parse(argv)?;
    let config = Config::load()?;

    match &cli.command {
        Command::Help => {
            print!("{}", args::help_text(style));
            Ok(0)
        }
        Command::Version => {
            println!("tomesole {}", args::VERSION);
            Ok(0)
        }
        Command::Config { init, path } => cmd_config(*init, *path, style),
        Command::Doctor => cmd_doctor(&cli, &config, style),
        Command::Mirrors { refresh } => cmd_mirrors(&cli, &config, *refresh, style),
        Command::History { limit, clear } => cmd_history(&cli, &config, *limit, *clear, style),
        Command::Open {
            selector,
            reveal,
            with,
        } => cmd_open(selector, *reveal, with.as_deref(), &cli, &config, style),
        Command::Tui { query } => {
            // Without a terminal there is nothing to draw on, so fall back to
            // the help text rather than failing obscurely.
            if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
                print!("{}", args::help_text(style));
                return Ok(0);
            }
            tui::run(settings(&cli.global, &config), query.clone())?;
            Ok(0)
        }
        Command::Search(_) => cmd_search(&cli, &config, style),
        Command::Get { md5 } => cmd_get(md5, &cli, &config, style),
    }
}

/// Everything the commands need, after merging flags over the config file.
#[derive(Clone)]
pub struct Settings {
    pub policy: NetPolicy,
    pub dest_dir: PathBuf,
    pub max_bytes: u64,
    pub verify: bool,
    pub force: bool,
    pub resume: bool,
    pub mirrors: Vec<String>,
    pub refresh_mirrors: bool,
    pub quiet: bool,
    pub json: bool,
    pub output: Option<String>,
    /// Application to open downloaded books with. None means the system default.
    pub reader: Option<String>,
    /// Record what gets downloaded.
    pub history: bool,
    /// Fetch and draw cover art.
    pub covers: bool,
}

pub fn settings(global: &Global, config: &Config) -> Settings {
    let policy = NetPolicy {
        allow_http: global.allow_http || config.allow_http,
        allow_private_hosts: global.allow_private_hosts,
        connect_timeout: Duration::from_secs(10),
        request_timeout: Duration::from_secs(45),
        max_redirects: 8,
    };

    // Flags beat the config file; the config file beats the built-in default.
    let mirrors = if global.mirrors.is_empty() {
        config.mirrors.clone()
    } else {
        global.mirrors.clone()
    };

    Settings {
        policy,
        dest_dir: global
            .dest_dir
            .clone()
            .or_else(|| config.download_dir.clone())
            .unwrap_or_else(config::default_download_dir),
        max_bytes: global
            .max_size
            .or(config.max_size)
            .unwrap_or(DEFAULT_MAX_BYTES),
        verify: global.verify.or(config.verify).unwrap_or(true),
        force: global.force,
        resume: !global.no_resume,
        mirrors,
        refresh_mirrors: global.refresh_mirrors,
        quiet: global.quiet,
        json: global.json,
        output: global.output.clone(),
        reader: global.reader.clone().or_else(|| config.reader.clone()),
        history: config.history.unwrap_or(true),
        covers: !global.no_covers && config.covers.unwrap_or(true),
    }
}

/// Find a usable mirror pool, narrating progress unless asked to be quiet.
fn resolve_pool(settings: &Settings, style: &Style) -> Result<Pool> {
    let spinner = if settings.quiet || settings.json {
        None
    } else {
        Some(Spinner::start("finding a mirror", *style))
    };

    let note = |message: &str| {
        if let Some(s) = &spinner {
            s.update(message, *style);
        }
    };

    let pool = mirror::resolve(
        &settings.policy,
        ResolveOptions {
            explicit: &settings.mirrors,
            refresh: settings.refresh_mirrors,
            progress: &note,
        },
    );

    if let Some(s) = spinner {
        s.clear();
    }
    pool
}

/// Reorder a pool so the mirror that just worked is tried first.
fn prefer(pool: Pool, used: &Uri) -> Pool {
    let mut ordered = vec![used.clone()];
    for uri in pool.mirrors() {
        if uri.to_string() != used.to_string() {
            ordered.push(uri.clone());
        }
    }
    Pool::new(ordered)
}

fn cmd_search(cli: &Cli, config: &Config, style: &Style) -> Result<i32> {
    let Command::Search(search) = &cli.command else {
        unreachable!("cmd_search called for another command")
    };
    let settings = settings(&cli.global, config);

    let mut query = SearchQuery::new(String::new());
    query.fields = search.fields.clone();
    if !search.topics.is_empty() {
        query.topics = search.topics.clone();
    } else if let Some(topics) = &config.topics {
        query.topics = topics.clone();
    }
    query.limit = search.limit.or(config.limit).unwrap_or(25);
    query.extension = search.extension.clone();
    query.language = search.language.clone();
    // `author:herbert dune` works here as well as in the interface; explicit
    // flags have already been set above and are left alone.
    query::apply(&search.terms.join(" "), &mut query);

    if query.terms.trim().is_empty() {
        return Err(crate::err!("there is nothing to search for"));
    }

    let pool = resolve_pool(&settings, style)?;
    let http = Http::new(settings.policy)?;

    let spinner = if settings.quiet || settings.json {
        None
    } else {
        Some(Spinner::start(
            &format!("searching for “{}”", query.terms),
            *style,
        ))
    };

    let (books, used) = pool.try_each(
        |base| net::with_retry(2, |_| libgen::search(&http, base, &query)),
        |base, _| {
            if let Some(s) = &spinner {
                let host = net::host_of(base).unwrap_or_default();
                s.update(&format!("{host} failed, trying another mirror"), *style);
            }
        },
    )?;

    if let Some(s) = spinner {
        s.clear();
    }

    if settings.json {
        print!("{}", ui::json_results(&books));
        return Ok(if books.is_empty() { 1 } else { 0 });
    }

    if books.is_empty() {
        // When the query carried filters, say so — a search that found plenty
        // and then filtered it all away looks identical to one that found
        // nothing, and the fix is different.
        let advice = if query::parse(&search.terms.join(" ")).is_tagged()
            || query.extension.is_some()
            || query.language.is_some()
        {
            "Try fewer words, or drop the format and language filters."
        } else {
            "Try fewer words."
        };
        eprintln!(
            "\n  {} nothing found for “{}”.\n  {}\n",
            style.yellow("∅"),
            query.terms,
            style.dim(advice)
        );
        return Ok(1);
    }

    if !settings.quiet {
        eprintln!(
            "\n  {} {} {}\n",
            style.bold(&books.len().to_string()),
            if books.len() == 1 {
                "result from"
            } else {
                "results from"
            },
            style.dim(&net::host_of(&used).unwrap_or_default()),
        );
    }
    print!("{}", ui::render_results(&books, style));

    if search.no_download {
        return Ok(0);
    }

    let picks = if search.first {
        vec![0]
    } else {
        match ui::prompt_selection(books.len(), style)? {
            Some(picks) => picks,
            None => {
                eprintln!();
                return Ok(0);
            }
        }
    };

    // Naming an output file only makes sense for a single download.
    if picks.len() > 1 && settings.output.is_some() {
        return Err(crate::err!(
            "--output names a single file, but {} results were selected",
            picks.len()
        ));
    }

    let pool = prefer(pool, &used);
    let mut failures = 0;
    eprintln!();
    for index in picks {
        let book = &books[index];
        if let Err(e) = download_one(&http, &pool, book, &settings, style) {
            failures += 1;
            eprintln!("  {} {e}\n", style.red("✗"));
        }
    }
    Ok(if failures > 0 { 1 } else { 0 })
}

fn cmd_get(md5: &str, cli: &Cli, config: &Config, style: &Style) -> Result<i32> {
    let settings = settings(&cli.global, config);
    let pool = resolve_pool(&settings, style)?;
    let http = Http::new(settings.policy)?;

    let spinner = if settings.quiet {
        None
    } else {
        Some(Spinner::start("looking up the file", *style))
    };

    let (resolved, used) = pool.try_each(
        |base| net::with_retry(2, |_| libgen::resolve_download(&http, base, md5)),
        |base, _| {
            if let Some(s) = &spinner {
                let host = net::host_of(base).unwrap_or_default();
                s.update(&format!("{host} failed, trying another mirror"), *style);
            }
        },
    )?;
    if let Some(s) = spinner {
        s.clear();
    }

    if !settings.quiet {
        eprintln!(
            "\n  {} {}\n",
            style.bold(&resolved.book.title),
            style.dim(&format!("via {}", net::host_of(&used).unwrap_or_default())),
        );
    }

    // The link is already resolved, so stream straight from it rather than
    // asking the pool to resolve again — the key it carries is single-use.
    let opts = download::Options {
        dest_dir: settings.dest_dir.clone(),
        filename: settings.output.clone(),
        max_bytes: settings.max_bytes,
        verify: settings.verify,
        force: settings.force,
        resume: settings.resume,
    };
    let mut bar = term::Progress::new("downloading", resolved.book.size_bytes, *style);
    let result = download::fetch(
        &http,
        &resolved.url,
        &resolved.book,
        &opts,
        &mut |done, total| bar.set(done, total),
    );
    bar.clear();
    match result {
        Ok(outcome) => {
            history::record(settings.history, &resolved.book, &outcome);
            report(&outcome, &resolved.book, &settings, style);
            Ok(0)
        }
        Err(e) => {
            eprintln!("  {} {e}\n", style.red("✗"));
            Ok(1)
        }
    }
}

/// Resolve a fresh download link and stream the file.
fn download_one(
    http: &Http,
    pool: &Pool,
    book: &Book,
    settings: &Settings,
    style: &Style,
) -> Result<()> {
    if !settings.quiet {
        eprintln!("  {} {}", style.cyan("↓"), style.bold(&book.title));
    }

    let opts = download::Options {
        dest_dir: settings.dest_dir.clone(),
        filename: settings.output.clone(),
        max_bytes: settings.max_bytes,
        verify: settings.verify,
        force: settings.force,
        resume: settings.resume,
    };

    // Each attempt re-resolves the link, because the key is bound to the page
    // it came from and cannot be carried across mirrors.
    let (outcome, _used) = pool.try_each(
        |base| {
            let resolved = libgen::resolve_download(http, base, &book.md5)?;
            let mut bar = term::Progress::new("downloading", book.size_bytes, *style);
            let outcome = download::fetch(http, &resolved.url, book, &opts, &mut |done, total| {
                bar.set(done, total)
            });
            bar.clear();
            outcome
        },
        |_, _| {},
    )?;

    history::record(settings.history, book, &outcome);
    report(&outcome, book, settings, style);
    Ok(())
}

fn report(outcome: &download::Outcome, book: &Book, settings: &Settings, style: &Style) {
    if settings.quiet {
        // Quiet mode prints the path and nothing else, so it can be piped.
        println!("{}", outcome.path.display());
        return;
    }
    let name = outcome
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    if outcome.skipped {
        eprintln!(
            "  {} {name}\n    {}\n",
            style.yellow("•"),
            style.dim("already downloaded — pass --force to replace it")
        );
        return;
    }

    let integrity = if outcome.verified {
        style.green("MD5 verified")
    } else if settings.verify {
        style.yellow("no MD5 on record — unverified")
    } else {
        style.yellow("verification skipped")
    };

    let size = if outcome.bytes > 0 {
        outcome.bytes
    } else {
        book.size_bytes.unwrap_or(0)
    };

    eprintln!(
        "  {} {name}\n    {} · {} · {integrity}\n",
        style.green("✓"),
        style.dim(
            &outcome
                .path
                .parent()
                .unwrap_or(&settings.dest_dir)
                .display()
                .to_string()
        ),
        style.dim(&model::human_bytes(size)),
    );
}

fn cmd_mirrors(cli: &Cli, config: &Config, refresh: bool, style: &Style) -> Result<i32> {
    let settings = settings(&cli.global, config);
    if refresh {
        mirror::clear_cache()?;
    }

    // Probe the user's mirrors alongside the built-ins, so a configured mirror
    // shows up in the table too.
    let mut candidates: Vec<Uri> = Vec::new();
    for raw in &settings.mirrors {
        candidates.push(mirror::parse_mirror(raw, &settings.policy)?);
    }
    for seed in mirror::SEED_MIRRORS {
        if let Ok(uri) = mirror::parse_mirror(seed, &settings.policy)
            && !candidates.iter().any(|c| c.to_string() == uri.to_string())
        {
            candidates.push(uri);
        }
    }

    let spinner = if settings.quiet {
        None
    } else {
        Some(Spinner::start(
            &format!("probing {} mirrors", candidates.len()),
            *style,
        ))
    };
    let mut statuses = mirror::probe_all(&candidates, &settings.policy);
    if let Some(s) = spinner {
        s.clear();
    }

    // Healthy and fastest at the top, so the table reads as a ranking.
    statuses.sort_by_key(|s| (!s.is_healthy(), s.latency().unwrap_or(Duration::MAX)));

    let healthy = statuses.iter().filter(|s| s.is_healthy()).count();
    eprintln!();
    print!("{}", ui::render_mirror_table(&statuses, style));
    eprintln!(
        "\n  {}\n",
        style.dim(&format!(
            "{healthy} of {} usable — ranked by search latency",
            statuses.len()
        ))
    );

    Ok(if healthy == 0 { 1 } else { 0 })
}

fn cmd_history(
    cli: &Cli,
    config: &Config,
    limit: Option<usize>,
    clear: bool,
    style: &Style,
) -> Result<i32> {
    let settings = settings(&cli.global, config);

    if clear {
        history::clear()?;
        // Which covers were fetched says as much about what someone has been
        // reading as the download list does, so it goes too.
        cover::clear_cache()?;
        eprintln!(
            "  {} download history and cached covers cleared\n",
            style.green("✓")
        );
        return Ok(0);
    }

    let mut entries = history::load();
    if let Some(limit) = limit {
        entries.truncate(limit);
    }

    if settings.json {
        print!("{}", ui::json_history(&entries));
        return Ok(if entries.is_empty() { 1 } else { 0 });
    }

    if entries.is_empty() {
        eprintln!(
            "\n  {} nothing downloaded yet.\n  {}\n",
            style.dim("•"),
            style.dim(if settings.history {
                "Downloads are recorded here as you make them."
            } else {
                "Recording is off — set `history = true` in the config to keep a list."
            })
        );
        return Ok(0);
    }

    if settings.quiet {
        for entry in &entries {
            println!("{}", entry.path.display());
        }
        return Ok(0);
    }

    eprintln!();
    print!("{}", ui::render_history(&entries, style));

    let gone = entries.iter().filter(|e| !e.present()).count();
    if gone > 0 {
        eprintln!(
            "\n  {} {}",
            style.red("!"),
            style.dim(&format!(
                "{gone} of these {} no longer where {} downloaded",
                if gone == 1 { "is" } else { "are" },
                if gone == 1 { "it was" } else { "they were" }
            ))
        );
    }
    eprintln!(
        "\n  {}\n",
        style.dim(
            "`tomesole open 1` to read the newest, `tomesole reveal 1` to show it in the file manager"
        )
    );
    Ok(0)
}

fn cmd_open(
    selector: &str,
    reveal: bool,
    with: Option<&str>,
    cli: &Cli,
    config: &Config,
    style: &Style,
) -> Result<i32> {
    let settings = settings(&cli.global, config);
    let entries = history::load();
    if entries.is_empty() {
        return Err(crate::err!(
            "nothing has been downloaded yet, so there is nothing to open"
        ));
    }

    let matches = history::select(&entries, selector)?;
    let entry = match matches.as_slice() {
        [only] => *only,
        many => {
            // Several entries answer to that name, so ask rather than guess.
            let list: Vec<history::Entry> = many.iter().map(|e| (*e).clone()).collect();
            eprintln!(
                "\n  {} match “{selector}”\n",
                style.bold(&format!("{} downloads", list.len()))
            );
            print!("{}", ui::render_history(&list, style));
            match ui::prompt_selection(list.len(), style)? {
                Some(picks) => many[picks[0]],
                None => {
                    eprintln!();
                    return Ok(0);
                }
            }
        }
    };

    let reader = with.or(settings.reader.as_deref());
    if reveal {
        launch::reveal(&entry.path)?;
    } else {
        launch::open(&entry.path, reader)?;
    }

    if !settings.quiet {
        let verb = if reveal { "showing" } else { "opening" };
        eprintln!(
            "\n  {} {verb} {}\n    {}\n",
            style.green("✓"),
            style.bold(&entry.filename()),
            style.dim(&entry.path.display().to_string())
        );
    }
    Ok(0)
}

fn cmd_config(init: bool, path_only: bool, style: &Style) -> Result<i32> {
    let path = config::config_path();

    if path_only {
        println!("{}", path.display());
        return Ok(0);
    }

    if init {
        if path.exists() {
            eprintln!(
                "  {} {} already exists — leaving it alone\n",
                style.yellow("•"),
                path.display()
            );
            return Ok(0);
        }
        config::ensure_dir(&config::config_dir())?;
        std::fs::write(&path, config::TEMPLATE)?;
        eprintln!("  {} wrote {}\n", style.green("✓"), path.display());
        return Ok(0);
    }

    match std::fs::read_to_string(&path) {
        Ok(text) => {
            eprintln!("  {}\n", style.dim(&path.display().to_string()));
            print!("{text}");
            Ok(0)
        }
        Err(_) => {
            eprintln!(
                "  {} no config yet at {}\n  {}\n",
                style.dim("•"),
                path.display(),
                style.dim("run `tomesole config --init` to create one")
            );
            Ok(0)
        }
    }
}

fn cmd_doctor(cli: &Cli, config: &Config, style: &Style) -> Result<i32> {
    let settings = settings(&cli.global, config);
    let config_path = config::config_path();

    let row = |label: &str, value: String| {
        eprintln!("  {}  {value}", style.dim(&term::pad(label, 16)));
    };

    eprintln!("\n  {}\n", style.bold("tomesole doctor"));
    row("version", args::VERSION.to_string());
    row(
        "config",
        if config_path.exists() {
            config_path.display().to_string()
        } else {
            format!("{} {}", config_path.display(), style.dim("(none yet)"))
        },
    );
    row("cache", config::mirror_cache_path().display().to_string());
    row("downloads", settings.dest_dir.display().to_string());
    row(
        "writable",
        if is_writable(&settings.dest_dir) {
            style.green("yes")
        } else {
            style.red("no — downloads will fail")
        },
    );
    row(
        "history",
        if settings.history {
            let count = history::load().len();
            format!(
                "{} {}",
                config::history_path().display(),
                style.dim(&format!("({count} entries)"))
            )
        } else {
            style.yellow("off")
        },
    );
    row(
        "reader",
        match settings.reader.as_deref() {
            Some(app) => app.to_string(),
            None => style.dim("system default"),
        },
    );
    row(
        "cover art",
        if !settings.covers {
            style.dim("off").to_string()
        } else {
            match graphics::detect() {
                graphics::Protocol::Kitty => format!("{} (kitty graphics)", style.green("on")),
                graphics::Protocol::ITerm2 => format!("{} (iTerm2 inline)", style.green("on")),
                graphics::Protocol::Blocks => {
                    format!("{} (colour blocks — no image protocol)", style.green("on"))
                }
                graphics::Protocol::None => style.yellow("no colour support in this terminal"),
            }
        },
    );
    let (covers, cover_bytes) = cover::cache_size();
    if covers > 0 {
        row(
            "covers cached",
            style.dim(&format!(
                "{covers} files, {}",
                model::human_bytes(cover_bytes)
            )),
        );
    }
    row("max size", model::human_bytes(settings.max_bytes));
    row(
        "verify md5",
        if settings.verify {
            style.green("on")
        } else {
            style.red("off — not recommended")
        },
    );
    row(
        "cleartext http",
        if settings.policy.allow_http {
            style.yellow("allowed")
        } else {
            style.green("refused")
        },
    );
    row(
        "private hosts",
        if settings.policy.allow_private_hosts {
            style.yellow("allowed")
        } else {
            style.green("refused")
        },
    );

    eprintln!("\n  {}\n", style.bold("mirrors"));
    let candidates: Vec<Uri> = mirror::SEED_MIRRORS
        .iter()
        .filter_map(|s| mirror::parse_mirror(s, &settings.policy).ok())
        .collect();
    let spinner = Spinner::start("probing", *style);
    let mut statuses = mirror::probe_all(&candidates, &settings.policy);
    spinner.clear();
    statuses.sort_by_key(|s| (!s.is_healthy(), s.latency().unwrap_or(Duration::MAX)));
    print!("{}", ui::render_mirror_table(&statuses, style));

    let healthy = statuses.iter().filter(|s| s.is_healthy()).count();
    eprintln!();
    if healthy == 0 {
        eprintln!(
            "  {} no mirror is reachable. Check your connection, or pass \
             `--mirror <url>` if you know a current one.\n",
            style.red("✗")
        );
        return Ok(1);
    }
    eprintln!("  {} {healthy} mirror(s) usable\n", style.green("✓"));
    Ok(0)
}

fn is_writable(dir: &std::path::Path) -> bool {
    if !dir.exists() {
        // A missing directory is fine as long as we could create it.
        return dir
            .parent()
            .map(|p| p.exists() && !is_readonly(p))
            .unwrap_or(false);
    }
    !is_readonly(dir)
}

fn is_readonly(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.permissions().readonly())
        .unwrap_or(true)
}
