//! Command-line parsing.
//!
//! Hand-rolled rather than pulled from a crate: the grammar is small and
//! fixed, and owning it keeps the help output exactly the shape we want.
//!
//! Supported forms are `--flag`, `--key value`, `--key=value`, short aliases
//! like `-n 10`, and `--` to stop flag parsing. Combined short flags (`-abc`)
//! are deliberately not supported — nothing here benefits from them and they
//! make typos parse as something valid.

use std::path::PathBuf;

use crate::error::Result;
use crate::model::{Field, Topic, parse_size};
use crate::{bail, err};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, PartialEq)]
pub enum Command {
    Search(Search),
    /// Download directly by MD5, skipping search.
    Get {
        md5: String,
    },
    Mirrors {
        refresh: bool,
    },
    /// Full-screen interface. `query` pre-fills the search box.
    Tui {
        query: Option<String>,
    },
    Config {
        init: bool,
        path: bool,
    },
    Doctor,
    Help,
    Version,
}

#[derive(Debug, Default, PartialEq)]
pub struct Search {
    pub terms: Vec<String>,
    pub fields: Vec<Field>,
    pub topics: Vec<Topic>,
    pub limit: Option<usize>,
    pub extension: Option<String>,
    pub language: Option<String>,
    /// Take the first result without prompting.
    pub first: bool,
    /// Show results and stop.
    pub no_download: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct Global {
    pub mirrors: Vec<String>,
    pub refresh_mirrors: bool,
    pub dest_dir: Option<PathBuf>,
    pub output: Option<String>,
    pub max_size: Option<u64>,
    pub verify: Option<bool>,
    pub force: bool,
    pub no_resume: bool,
    pub allow_http: bool,
    pub allow_private_hosts: bool,
    pub no_color: bool,
    pub quiet: bool,
    pub json: bool,
}

#[derive(Debug, PartialEq)]
pub struct Cli {
    pub command: Command,
    pub global: Global,
}

const SUBCOMMANDS: [&str; 8] = [
    "search", "get", "mirrors", "config", "doctor", "help", "version", "tui",
];

/// Parse arguments, excluding the program name.
pub fn parse(argv: &[String]) -> Result<Cli> {
    let mut global = Global::default();
    let mut search = Search::default();

    // Work out the subcommand. A bare `clibgen rust programming` is a search,
    // so anything that is not a known subcommand starts the query.
    let (command_name, rest) = split_command(argv);

    let mut positionals: Vec<String> = Vec::new();
    let mut mirrors_refresh = false;
    let mut config_init = false;
    let mut config_path = false;
    let mut i = 0usize;
    let mut flags_done = false;

    while i < rest.len() {
        let arg = &rest[i];

        if flags_done || !arg.starts_with('-') || arg == "-" {
            positionals.push(arg.clone());
            i += 1;
            continue;
        }
        if arg == "--" {
            flags_done = true;
            i += 1;
            continue;
        }

        // Split `--key=value` into its parts up front.
        let (flag, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };

        // Fetch the value for a flag that needs one.
        let take_value = |flag: &str, i: &mut usize| -> Result<String> {
            if let Some(v) = inline.clone() {
                *i += 1;
                return Ok(v);
            }
            let value = rest
                .get(*i + 1)
                .cloned()
                .ok_or_else(|| err!("`{flag}` needs a value"))?;
            if value.starts_with('-') && value.len() > 1 {
                bail!("`{flag}` needs a value, but got `{value}`");
            }
            *i += 2;
            Ok(value)
        };

        match flag.as_str() {
            "-h" | "--help" => {
                return Ok(Cli {
                    command: Command::Help,
                    global,
                });
            }
            "-V" | "--version" => {
                return Ok(Cli {
                    command: Command::Version,
                    global,
                });
            }

            // Search options.
            "-a" | "--author" => {
                let v = take_value(&flag, &mut i)?;
                search.fields.push(Field::Author);
                positionals.push(v);
            }
            "-t" | "--title" => {
                let v = take_value(&flag, &mut i)?;
                search.fields.push(Field::Title);
                positionals.push(v);
            }
            "--isbn" => {
                let v = take_value(&flag, &mut i)?;
                search.fields.push(Field::Isbn);
                positionals.push(v);
            }
            "--publisher" => {
                let v = take_value(&flag, &mut i)?;
                search.fields.push(Field::Publisher);
                positionals.push(v);
            }
            "-s" | "--series" => {
                let v = take_value(&flag, &mut i)?;
                search.fields.push(Field::Series);
                positionals.push(v);
            }
            "--year" => {
                let v = take_value(&flag, &mut i)?;
                search.fields.push(Field::Year);
                positionals.push(v);
            }
            "-n" | "--limit" => {
                let v = take_value(&flag, &mut i)?;
                let n: usize = v
                    .parse()
                    .map_err(|_| err!("`--limit` expects a number, got `{v}`"))?;
                if n == 0 {
                    bail!("`--limit` must be at least 1");
                }
                search.limit = Some(n.min(100));
            }
            "-e" | "--ext" | "--extension" => {
                search.extension = Some(
                    take_value(&flag, &mut i)?
                        .trim_start_matches('.')
                        .to_ascii_lowercase(),
                );
            }
            "-l" | "--lang" | "--language" => {
                search.language = Some(take_value(&flag, &mut i)?);
            }
            "--topic" => {
                let v = take_value(&flag, &mut i)?;
                for name in v.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let topic = Topic::parse(name).ok_or_else(|| {
                        err!(
                            "unknown topic `{name}` (expected one of: {})",
                            Topic::ALL_NAMES
                        )
                    })?;
                    search.topics.push(topic);
                }
            }
            "-y" | "--first" | "--yes" => {
                search.first = true;
                i += 1;
            }
            "--no-download" | "--search-only" => {
                search.no_download = true;
                i += 1;
            }

            // Output and destination.
            "-d" | "--dir" => {
                global.dest_dir = Some(crate::config::expand_tilde(&take_value(&flag, &mut i)?));
            }
            "-o" | "--output" => global.output = Some(take_value(&flag, &mut i)?),
            "--json" => {
                global.json = true;
                i += 1;
            }

            // Mirrors.
            "-m" | "--mirror" => {
                let v = take_value(&flag, &mut i)?;
                global.mirrors.extend(
                    v.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from),
                );
            }
            "--refresh" | "--refresh-mirrors" => {
                global.refresh_mirrors = true;
                mirrors_refresh = true;
                i += 1;
            }

            // Safety toggles.
            "--max-size" => {
                let v = take_value(&flag, &mut i)?;
                global.max_size =
                    Some(parse_size(&v).ok_or_else(|| err!("`--max-size` is not a size: `{v}`"))?);
            }
            "--no-verify" => {
                global.verify = Some(false);
                i += 1;
            }
            "-f" | "--force" => {
                global.force = true;
                i += 1;
            }
            "--no-resume" => {
                global.no_resume = true;
                i += 1;
            }
            "--allow-http" => {
                global.allow_http = true;
                i += 1;
            }
            "--allow-private-hosts" => {
                global.allow_private_hosts = true;
                i += 1;
            }

            // Presentation.
            "--no-color" | "--no-colour" | "--plain" => {
                global.no_color = true;
                i += 1;
            }
            "-q" | "--quiet" => {
                global.quiet = true;
                i += 1;
            }

            // `config` options.
            "--init" => {
                config_init = true;
                i += 1;
            }
            "--path" => {
                config_path = true;
                i += 1;
            }

            other => bail!("unknown option `{other}` (try `clibgen --help`)"),
        }
    }

    let command = match command_name {
        Some("help") => Command::Help,
        Some("version") => Command::Version,
        Some("doctor") => Command::Doctor,
        Some("tui") => Command::Tui {
            query: (!positionals.is_empty()).then(|| positionals.join(" ")),
        },
        Some("mirrors") => Command::Mirrors {
            refresh: mirrors_refresh,
        },
        Some("config") => Command::Config {
            init: config_init,
            path: config_path,
        },
        Some("get") => {
            let md5 = positionals
                .first()
                .cloned()
                .ok_or_else(|| err!("`get` needs an MD5, e.g. `clibgen get 1b915999…`"))?;
            let md5 = extract_md5_argument(&md5)?;
            Command::Get { md5 }
        }
        _ => {
            // `search`, or a bare query.
            if positionals.is_empty() {
                // Bare `clibgen` opens the interface. Callers that are not on a
                // terminal get help instead; `main` makes that call.
                return Ok(Cli {
                    command: Command::Tui { query: None },
                    global,
                });
            }
            // A bare MD5 is unambiguous; treat it as a direct download.
            if positionals.len() == 1 && crate::model::is_md5(&positionals[0]) {
                Command::Get {
                    md5: positionals[0].to_ascii_lowercase(),
                }
            } else {
                search.terms = positionals;
                Command::Search(search)
            }
        }
    };

    Ok(Cli { command, global })
}

/// Split off a leading subcommand, if there is one.
fn split_command(argv: &[String]) -> (Option<&str>, &[String]) {
    match argv.first() {
        Some(first) if SUBCOMMANDS.contains(&first.as_str()) => (Some(first.as_str()), &argv[1..]),
        _ => (None, argv),
    }
}

/// Accept either a bare MD5 or a Libgen URL containing one.
fn extract_md5_argument(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if crate::model::is_md5(trimmed) {
        return Ok(trimmed.to_ascii_lowercase());
    }
    if let Some(pos) = trimmed.find("md5=") {
        let candidate: String = trimmed[pos + 4..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if crate::model::is_md5(&candidate) {
            return Ok(candidate.to_ascii_lowercase());
        }
    }
    // A trailing path segment, as in `/main/<md5>`.
    if let Some(last) = trimmed.rsplit('/').next()
        && crate::model::is_md5(last)
    {
        return Ok(last.to_ascii_lowercase());
    }
    bail!("`{input}` is not a 32-character MD5 or a Libgen link containing one")
}

/// The help screen.
pub fn help_text(style: &crate::term::Style) -> String {
    /// Width of the flag column, so descriptions line up.
    const FLAG_WIDTH: usize = 22;

    fn section(out: &mut String, style: &crate::term::Style, title: &str) {
        out.push_str(&format!("\n{}\n", style.bold(title)));
    }

    fn flush(
        out: &mut String,
        style: &crate::term::Style,
        rows: &mut Vec<(&str, &str)>,
        width: usize,
    ) {
        for (flag, description) in rows.drain(..) {
            // Pad before colouring, or the escape codes count towards width.
            out.push_str(&format!(
                "  {}{}\n",
                style.cyan(&crate::term::pad(flag, width)),
                style.dim(description)
            ));
        }
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{} {VERSION} — search and download from Library Genesis\n",
        style.bold("clibgen")
    ));
    let mut rows: Vec<(&str, &str)> = Vec::new();

    section(&mut out, style, "USAGE");
    rows.extend([
        ("clibgen", "open the full-screen interface"),
        ("clibgen <query>...", "search, then pick what to download"),
        ("clibgen get <md5>", "download a file you already know"),
        ("clibgen mirrors", "show which mirrors are usable"),
        ("clibgen config --init", "write a starter config file"),
        ("clibgen doctor", "check the setup end to end"),
    ]);
    flush(&mut out, style, &mut rows, FLAG_WIDTH);

    section(&mut out, style, "SEARCH");
    rows.extend([
        ("-a, --author <q>", "restrict the query to the author"),
        ("-t, --title <q>", "restrict the query to the title"),
        ("-s, --series <q>", "restrict the query to the series"),
        ("--publisher <q>", "restrict the query to the publisher"),
        ("--year <q>", "restrict the query to the year"),
        ("--isbn <q>", "search by ISBN"),
        ("-n, --limit <n>", "how many results to show (25)"),
        ("-e, --ext <x>", "only this format, e.g. epub, pdf"),
        ("-l, --lang <x>", "only this language, e.g. english"),
        ("--topic <t>", "collections to search (libgen, fiction)"),
        ("-y, --first", "take the top result without asking"),
        ("--no-download", "list results and stop"),
    ]);
    flush(&mut out, style, &mut rows, FLAG_WIDTH);

    section(&mut out, style, "DOWNLOAD");
    rows.extend([
        ("-d, --dir <path>", "where to save (~/Downloads)"),
        ("-o, --output <name>", "save under this filename"),
        ("-f, --force", "overwrite a file that already exists"),
        ("--max-size <size>", "refuse anything larger (4 GB)"),
        ("--no-verify", "skip the MD5 check — not advised"),
        ("--no-resume", "always start the transfer from zero"),
    ]);
    flush(&mut out, style, &mut rows, FLAG_WIDTH);

    section(&mut out, style, "MIRRORS");
    rows.extend([
        ("-m, --mirror <url>", "use this mirror instead of choosing"),
        ("--refresh", "re-probe mirrors, ignoring the cache"),
    ]);
    flush(&mut out, style, &mut rows, FLAG_WIDTH);

    section(&mut out, style, "GENERAL");
    rows.extend([
        ("--json", "machine-readable results"),
        ("-q, --quiet", "print only paths and errors"),
        ("--no-color", "disable colour"),
        ("--allow-http", "permit cleartext http mirrors"),
        ("-h, --help", "show this help"),
        ("-V, --version", "show the version"),
    ]);
    flush(&mut out, style, &mut rows, FLAG_WIDTH);

    section(&mut out, style, "EXAMPLES");
    for example in [
        "clibgen the pragmatic programmer",
        "clibgen -a \"Ursula K. Le Guin\" -e epub",
        "clibgen --title dune --lang english -n 50",
        "clibgen get 1b9159991f7fb1b3910c0be9ebf7e595",
    ] {
        out.push_str(&format!("  {}\n", style.dim(example)));
    }

    out.push_str(&format!(
        "\n{}\n",
        style.dim("Every download is checked against the catalogue's MD5 before it is saved.")
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Result<Cli> {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        parse(&owned)
    }

    fn search_of(cli: Cli) -> Search {
        match cli.command {
            Command::Search(s) => s,
            other => panic!("expected a search, got {other:?}"),
        }
    }

    #[test]
    fn bare_words_are_a_search() {
        let s = search_of(parse_args(&["the", "pragmatic", "programmer"]).unwrap());
        assert_eq!(s.terms, ["the", "pragmatic", "programmer"]);
    }

    #[test]
    fn explicit_search_subcommand_works_too() {
        let s = search_of(parse_args(&["search", "dune"]).unwrap());
        assert_eq!(s.terms, ["dune"]);
    }

    #[test]
    fn field_flags_scope_the_query() {
        let s = search_of(parse_args(&["-a", "Le Guin", "-e", "EPUB"]).unwrap());
        assert_eq!(s.terms, ["Le Guin"]);
        assert_eq!(s.fields, [Field::Author]);
        assert_eq!(s.extension.as_deref(), Some("epub"));
    }

    #[test]
    fn leading_dot_on_extension_is_tolerated() {
        let s = search_of(parse_args(&["dune", "--ext", ".pdf"]).unwrap());
        assert_eq!(s.extension.as_deref(), Some("pdf"));
    }

    #[test]
    fn equals_form_is_equivalent() {
        let a = search_of(parse_args(&["dune", "--limit=10"]).unwrap());
        let b = search_of(parse_args(&["dune", "--limit", "10"]).unwrap());
        assert_eq!(a.limit, Some(10));
        assert_eq!(a.limit, b.limit);
    }

    #[test]
    fn limit_is_validated_and_capped() {
        assert!(parse_args(&["x", "-n", "abc"]).is_err());
        assert!(parse_args(&["x", "-n", "0"]).is_err());
        assert_eq!(
            search_of(parse_args(&["x", "-n", "9999"]).unwrap()).limit,
            Some(100)
        );
    }

    #[test]
    fn topics_accept_lists() {
        let s = search_of(parse_args(&["x", "--topic", "fiction,comics"]).unwrap());
        assert_eq!(s.topics, [Topic::Fiction, Topic::Comics]);
        assert!(parse_args(&["x", "--topic", "nonsense"]).is_err());
    }

    #[test]
    fn a_bare_md5_downloads_directly() {
        let cli = parse_args(&["1b9159991f7fb1b3910c0be9ebf7e595"]).unwrap();
        assert_eq!(
            cli.command,
            Command::Get {
                md5: "1b9159991f7fb1b3910c0be9ebf7e595".into()
            }
        );
    }

    #[test]
    fn get_accepts_urls_containing_an_md5() {
        for input in [
            "https://libgen.li/ads.php?md5=1B9159991F7FB1B3910C0BE9EBF7E595",
            "https://libgen.li/main/1b9159991f7fb1b3910c0be9ebf7e595",
            "1b9159991f7fb1b3910c0be9ebf7e595",
        ] {
            let cli = parse_args(&["get", input]).unwrap();
            assert_eq!(
                cli.command,
                Command::Get {
                    md5: "1b9159991f7fb1b3910c0be9ebf7e595".into()
                }
            );
        }
        assert!(parse_args(&["get", "nonsense"]).is_err());
        assert!(parse_args(&["get"]).is_err());
    }

    #[test]
    fn subcommands_are_recognised() {
        assert_eq!(
            parse_args(&["mirrors"]).unwrap().command,
            Command::Mirrors { refresh: false }
        );
        assert_eq!(
            parse_args(&["mirrors", "--refresh"]).unwrap().command,
            Command::Mirrors { refresh: true }
        );
        assert_eq!(parse_args(&["doctor"]).unwrap().command, Command::Doctor);
        assert_eq!(
            parse_args(&["config", "--init"]).unwrap().command,
            Command::Config {
                init: true,
                path: false
            }
        );
    }

    /// Bare `clibgen` opens the interface; `main` swaps in help when there is
    /// no terminal to draw on.
    #[test]
    fn no_arguments_opens_the_interface() {
        assert_eq!(
            parse_args(&[]).unwrap().command,
            Command::Tui { query: None }
        );
        assert_eq!(
            parse_args(&["tui", "dune"]).unwrap().command,
            Command::Tui {
                query: Some("dune".into())
            }
        );
        assert_eq!(parse_args(&["--help"]).unwrap().command, Command::Help);
        assert_eq!(parse_args(&["-V"]).unwrap().command, Command::Version);
    }

    #[test]
    fn global_safety_flags_are_collected() {
        let cli = parse_args(&[
            "dune",
            "--no-verify",
            "--force",
            "--allow-http",
            "--max-size",
            "500 MB",
            "-d",
            "/tmp/books",
        ])
        .unwrap();
        assert_eq!(cli.global.verify, Some(false));
        assert!(cli.global.force);
        assert!(cli.global.allow_http);
        assert_eq!(cli.global.max_size, Some(500 * 1024 * 1024));
        assert_eq!(cli.global.dest_dir, Some(PathBuf::from("/tmp/books")));
    }

    #[test]
    fn mirrors_can_be_repeated_or_listed() {
        let cli =
            parse_args(&["dune", "-m", "https://a.example", "-m", "https://b.example"]).unwrap();
        assert_eq!(
            cli.global.mirrors,
            ["https://a.example", "https://b.example"]
        );

        let cli = parse_args(&["dune", "-m", "https://a.example,https://b.example"]).unwrap();
        assert_eq!(
            cli.global.mirrors,
            ["https://a.example", "https://b.example"]
        );
    }

    #[test]
    fn unknown_flags_are_rejected() {
        let err = parse_args(&["dune", "--nonsense"]).unwrap_err().to_string();
        assert!(err.contains("unknown option"), "{err}");
    }

    #[test]
    fn missing_values_are_rejected() {
        assert!(parse_args(&["dune", "--limit"]).is_err());
        assert!(parse_args(&["--author"]).is_err());
        // A following flag is not a value.
        assert!(parse_args(&["--author", "--json"]).is_err());
    }

    #[test]
    fn double_dash_stops_flag_parsing() {
        let s = search_of(parse_args(&["--", "--not-a-flag"]).unwrap());
        assert_eq!(s.terms, ["--not-a-flag"]);
    }

    #[test]
    fn queries_that_look_like_negative_numbers_still_work() {
        let s = search_of(parse_args(&["catch", "-", "22"]).unwrap());
        assert_eq!(s.terms, ["catch", "-", "22"]);
    }
}
