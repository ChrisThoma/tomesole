//! Rendering: the results table, mirror status, JSON output, and prompts.

use std::io::{BufRead, IsTerminal, Write};

use crate::error::Result;
use crate::mirror::{MirrorStatus, Outcome};
use crate::model::Book;
use crate::term::{Style, display_width, pad, pad_left, terminal_width, truncate};
use crate::{bail, net};

/// Column widths that do not depend on the terminal size.
const W_INDEX: usize = 3;
const W_YEAR: usize = 4;
const W_LANG: usize = 8;
const W_SIZE: usize = 8;
const W_EXT: usize = 5;
const GAP: usize = 2;
const INDENT: usize = 2;

/// Work out how much room the two flexible columns get.
fn flex_widths(total: usize) -> (usize, usize) {
    let fixed = W_INDEX + W_YEAR + W_LANG + W_SIZE + W_EXT;
    let gaps = GAP * 6;
    let flex = total.saturating_sub(INDENT + fixed + gaps);
    let title = ((flex as f64 * 0.62) as usize).max(20);
    let author = flex.saturating_sub(title).max(12);
    (title, author)
}

/// Render the results as an aligned table.
pub fn render_results(books: &[Book], style: &Style) -> String {
    let width = terminal_width();
    let (w_title, w_author) = flex_widths(width);
    let gap = " ".repeat(GAP);
    let indent = " ".repeat(INDENT);
    let mut out = String::new();

    // Header.
    let header = format!(
        "{indent}{}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}",
        pad_left("#", W_INDEX),
        pad("Title", w_title),
        pad("Author", w_author),
        pad("Year", W_YEAR),
        pad("Lang", W_LANG),
        pad_left("Size", W_SIZE),
        pad("Fmt", W_EXT),
    );
    out.push_str(&style.dim(header.trim_end()));
    out.push('\n');

    // Rule.
    let rule = format!(
        "{indent}{}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}",
        "─".repeat(W_INDEX),
        "─".repeat(w_title),
        "─".repeat(w_author),
        "─".repeat(W_YEAR),
        "─".repeat(W_LANG),
        "─".repeat(W_SIZE),
        "─".repeat(W_EXT),
    );
    out.push_str(&style.dim(&rule));
    out.push('\n');

    for (i, book) in books.iter().enumerate() {
        // Only the first of several semicolon-separated authors fits.
        let all = book.authors_or_unknown();
        let author = all.split(';').next().unwrap_or(all).trim().to_string();

        let row = format!(
            "{indent}{}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}{gap}{}",
            style.cyan(&pad_left(&(i + 1).to_string(), W_INDEX)),
            pad(&truncate(&book.title, w_title), w_title),
            style.dim(&pad(&truncate(&author, w_author), w_author)),
            // Every fixed-width cell is truncated as well as padded: records
            // carry values like `2016;2014` in the year column, which would
            // otherwise overflow and shift the rest of the row.
            style.dim(&pad(
                &truncate(book.year.as_deref().unwrap_or(""), W_YEAR),
                W_YEAR
            )),
            style.dim(&pad(
                &truncate(book.language.as_deref().unwrap_or(""), W_LANG),
                W_LANG
            )),
            style.dim(&pad_left(&truncate(&book.size_human(), W_SIZE), W_SIZE)),
            style.green(&pad(&truncate(book.ext(), W_EXT), W_EXT)),
        );
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

/// Render mirror health as a table.
pub fn render_mirror_table(statuses: &[MirrorStatus], style: &Style) -> String {
    let host_width = statuses
        .iter()
        .filter_map(|s| net::host_of(&s.base).ok())
        .map(|h| display_width(&h))
        .max()
        .unwrap_or(20)
        .max(6);

    let mut out = String::new();
    out.push_str(&style.dim(&format!(
        "  {}  {}  {}\n",
        pad("Mirror", host_width),
        pad("Status", 8),
        "Detail"
    )));
    out.push_str(&style.dim(&format!(
        "  {}  {}  {}\n",
        "─".repeat(host_width),
        "─".repeat(8),
        "─".repeat(28)
    )));

    for status in statuses {
        let host = net::host_of(&status.base).unwrap_or_else(|_| status.base.to_string());
        let (badge, detail) = match &status.outcome {
            Outcome::Healthy { latency, results } => (
                style.green(&pad("up", 8)),
                style.dim(&format!("{} ms, {results} results", latency.as_millis())),
            ),
            Outcome::Unhealthy(reason) => (style.red(&pad("down", 8)), style.dim(reason)),
        };
        out.push_str(&format!(
            "  {}  {badge}  {detail}\n",
            pad(&host, host_width)
        ));
    }
    out
}

/// Serialise results as JSON.
///
/// Written by hand because this is the only JSON the program emits, and the
/// shape is fixed.
pub fn json_results(books: &[Book]) -> String {
    let mut out = String::from("[\n");
    for (i, book) in books.iter().enumerate() {
        out.push_str("  {\n");
        let mut fields: Vec<String> = vec![
            format!("    \"md5\": \"{}\"", json_escape(&book.md5)),
            format!("    \"title\": \"{}\"", json_escape(&book.title)),
        ];
        let mut optional = |key: &str, value: Option<&str>| {
            if let Some(v) = value.filter(|v| !v.trim().is_empty()) {
                fields.push(format!("    \"{key}\": \"{}\"", json_escape(v)));
            }
        };
        optional("authors", book.authors.as_deref());
        optional("publisher", book.publisher.as_deref());
        optional("year", book.year.as_deref());
        optional("language", book.language.as_deref());
        optional("pages", book.pages.as_deref());
        optional("extension", book.extension.as_deref());

        if let Some(size) = book.size_bytes {
            fields.push(format!("    \"size_bytes\": {size}"));
        }
        out.push_str(&fields.join(",\n"));
        out.push('\n');
        out.push_str(if i + 1 == books.len() {
            "  }\n"
        } else {
            "  },\n"
        });
    }
    out.push_str("]\n");
    out
}

/// Escape a string for inclusion in a JSON double-quoted value.
pub fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Ask which results to download.
///
/// Returns `None` when the user declines. Errors when there is no terminal to
/// ask, rather than guessing on their behalf.
pub fn prompt_selection(count: usize, style: &Style) -> Result<Option<Vec<usize>>> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "no terminal to prompt on — pass `--first` to take the top result \
             or `--no-download` to just list them"
        );
    }

    let stdin = std::io::stdin();
    for attempt in 0..3 {
        eprint!(
            "\n  {} {} ",
            style.bold(&format!("Select 1-{count}")),
            style.dim("(e.g. 3, or 1-4; Enter to cancel) ›"),
        );
        let _ = std::io::stderr().flush();

        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            // EOF, e.g. the user pressed Ctrl-D.
            eprintln!();
            return Ok(None);
        }
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("q") {
            return Ok(None);
        }

        match crate::term::parse_selection(trimmed, count) {
            Ok(picks) => return Ok(Some(picks)),
            Err(reason) => {
                eprintln!("  {} {reason}", style.red("✗"));
                if attempt == 2 {
                    bail!("could not read a valid selection");
                }
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn books() -> Vec<Book> {
        vec![
            Book {
                md5: "1b9159991f7fb1b3910c0be9ebf7e595".into(),
                title: "The Rust Programming Language".into(),
                authors: Some("Klabnik, Steve;Nichols, Carol".into()),
                year: Some("2019".into()),
                language: Some("English".into()),
                extension: Some("epub".into()),
                size_bytes: Some(19 * 1024),
                ..Default::default()
            },
            Book {
                md5: "0000000000000000000000000000abcd".into(),
                title: "A Very Long Title That Will Certainly Need To Be Truncated Somewhere"
                    .into(),
                authors: None,
                ..Default::default()
            },
        ]
    }

    #[test]
    fn table_has_a_row_per_book_plus_headers() {
        let rendered = render_results(&books(), &Style::plain());
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 4, "header, rule, two rows");
        assert!(lines[2].contains("Rust Programming"));
        assert!(lines[2].contains("epub"));
        assert!(lines[2].contains("Klabnik"));
    }

    #[test]
    fn rows_never_exceed_the_terminal_width() {
        let width = terminal_width();
        let rendered = render_results(&books(), &Style::plain());
        for line in rendered.lines() {
            assert!(
                display_width(line) <= width,
                "line of {} exceeds {width}: {line:?}",
                display_width(line)
            );
        }
    }

    /// Records really do carry values like `2016;2014` in the year column.
    #[test]
    fn overlong_fixed_width_fields_do_not_shift_the_row() {
        let mut list = books();
        list[0].year = Some("2016;2014;2011".into());
        list[0].language = Some("English, French, German".into());
        list[0].extension = Some("verylongformat".into());
        let rendered = render_results(&list, &Style::plain());

        let width = terminal_width();
        let lines: Vec<&str> = rendered.lines().collect();
        for line in &lines {
            assert!(display_width(line) <= width, "{line:?}");
        }
        // The row must keep the same column layout as the header rule.
        assert!(
            !lines[2].contains("2016;2014;2011"),
            "year was not truncated"
        );
    }

    #[test]
    fn missing_metadata_renders_blank_not_none() {
        let rendered = render_results(&books(), &Style::plain());
        assert!(!rendered.contains("None"));
    }

    #[test]
    fn wide_titles_stay_aligned() {
        let mut list = books();
        list[0].title = "日本語のタイトル".repeat(10);
        let rendered = render_results(&list, &Style::plain());
        let width = terminal_width();
        for line in rendered.lines() {
            assert!(display_width(line) <= width, "{line:?}");
        }
    }

    #[test]
    fn json_is_well_formed_and_escaped() {
        let mut list = books();
        list[0].title = "Quote \" backslash \\ newline \n".into();
        let json = json_results(&list);
        assert!(json.starts_with("[\n"));
        assert!(json.trim_end().ends_with(']'));
        assert!(json.contains(r#"\" backslash \\ newline \n"#));
        // Optional fields that are absent are omitted rather than null.
        assert!(!json.contains("\"authors\": \"\""));
    }

    #[test]
    fn json_escapes_control_characters() {
        assert_eq!(json_escape("a\u{1}b"), "a\\u0001b");
        assert_eq!(json_escape("tab\there"), "tab\\there");
    }

    #[test]
    fn empty_results_produce_an_empty_json_array() {
        assert_eq!(json_results(&[]).trim(), "[\n]");
    }

    #[test]
    fn flex_widths_stay_positive_on_narrow_terminals() {
        for width in [40usize, 60, 80, 100, 200] {
            let (title, author) = flex_widths(width);
            assert!(
                title >= 20 && author >= 12,
                "width {width} gave {title}/{author}"
            );
        }
    }
}
