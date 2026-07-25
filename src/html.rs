//! A deliberately small HTML scanner.
//!
//! We only ever need to do four things to a Libgen page: find one table by its
//! `id`, split it into rows and cells, pull `href` attributes out of a cell,
//! and flatten a cell to plain text. A full CSS-selector engine and HTML5
//! tree-builder would be a large dependency for that.
//!
//! This is a scanner, not a parser: it never builds a DOM, and it makes no
//! attempt to implement HTML5 error recovery. It is written to tolerate the
//! specific sloppiness Libgen actually emits — unclosed rows, void elements,
//! and stray quotes inside attribute lists (`<span class="badge""> `) — and to
//! degrade to "no match" rather than misbehave on anything else.

/// Elements that never have a closing tag, so depth tracking must not wait for
/// one.
const VOID_ELEMENTS: [&str; 14] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

/// Elements whose content is raw text and may contain `<`, so we skip them
/// wholesale rather than trying to tokenise inside.
const RAW_TEXT_ELEMENTS: [&str; 2] = ["script", "style"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tag<'a> {
    name: String,
    /// The attribute section, i.e. everything between the name and the `>`.
    attrs: &'a str,
    closing: bool,
    self_closing: bool,
    /// Byte offset of the opening `<`.
    start: usize,
    /// Byte offset just past the closing `>`.
    end: usize,
}

impl Tag<'_> {
    /// True when this tag opens an element that will need a matching close.
    fn opens_scope(&self) -> bool {
        !self.closing && !self.self_closing && !VOID_ELEMENTS.contains(&self.name.as_str())
    }
}

/// Find the next tag at or after `from`, skipping comments, doctypes, and the
/// contents of raw-text elements.
fn next_tag(html: &str, from: usize) -> Option<Tag<'_>> {
    let bytes = html.as_bytes();
    let mut i = from;

    while i < bytes.len() {
        // Find the next '<' on a char boundary.
        let lt = html[i..].find('<')? + i;
        let rest = &html[lt..];

        if rest.starts_with("<!--") {
            i = html[lt..].find("-->")? + lt + 3;
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = html[lt..].find('>')? + lt + 1;
            continue;
        }

        let after_lt = lt + 1;
        let closing = html[after_lt..].starts_with('/');
        let name_start = if closing { after_lt + 1 } else { after_lt };

        // A tag name must start with an ASCII letter; otherwise this '<' is
        // just text (e.g. "a < b").
        let first = html[name_start..].chars().next();
        if !matches!(first, Some(c) if c.is_ascii_alphabetic()) {
            i = lt + 1;
            continue;
        }

        let name_end = html[name_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == ':'))
            .map(|off| name_start + off)
            .unwrap_or(html.len());
        let name = html[name_start..name_end].to_ascii_lowercase();

        // Walk to the '>' that ends this tag, honouring quoted attribute
        // values so a '>' inside one does not end the tag early. Libgen puts
        // markup like `title="... <br> ..."` in attributes, so this matters.
        //
        // A quote only *opens* a value when it directly follows `=`. Libgen
        // also emits doubled quotes (`class="badge""`); treating that stray
        // quote as opening a new value would swallow the rest of the document.
        let mut j = name_end;
        let mut quote: Option<u8> = None;
        let mut prev_significant = b' ';
        let mut gt = None;
        while j < bytes.len() {
            let b = bytes[j];
            if let Some(q) = quote {
                if b == q {
                    quote = None;
                    prev_significant = b;
                }
                j += 1;
                continue;
            }
            match b {
                b'"' | b'\'' if prev_significant == b'=' => {
                    quote = Some(b);
                    prev_significant = b;
                }
                b'>' => {
                    gt = Some(j);
                    break;
                }
                _ => {
                    if !b.is_ascii_whitespace() {
                        prev_significant = b;
                    }
                }
            }
            j += 1;
        }
        let gt = gt?;

        let attrs = &html[name_end..gt];
        let self_closing = attrs.trim_end().ends_with('/');

        return Some(Tag {
            name,
            attrs,
            closing,
            self_closing,
            start: lt,
            end: gt + 1,
        });
    }
    None
}

/// Advance past a raw-text element's content, returning the offset of its
/// closing tag.
fn skip_raw_text(html: &str, name: &str, content_start: usize) -> usize {
    let needle = format!("</{name}");
    match html[content_start..].to_ascii_lowercase().find(&needle) {
        Some(off) => content_start + off,
        None => html.len(),
    }
}

/// Given the offset just past an element's opening tag, return the byte range
/// of its inner HTML by walking to the matching close tag.
///
/// When no matching close exists (Libgen omits `</tr>` in places), the element
/// is treated as running until the next sibling of the same name, or to the end
/// of the slice.
fn inner_range(html: &str, name: &str, content_start: usize) -> (usize, usize) {
    let mut depth = 1usize;
    let mut cursor = content_start;
    let mut first_sibling_open: Option<usize> = None;

    while let Some(tag) = next_tag(html, cursor) {
        if RAW_TEXT_ELEMENTS.contains(&tag.name.as_str()) && tag.opens_scope() {
            cursor = skip_raw_text(html, &tag.name, tag.end);
            continue;
        }

        if tag.name == name {
            if tag.closing {
                depth -= 1;
                if depth == 0 {
                    return (content_start, tag.start);
                }
            } else if tag.opens_scope() {
                if depth == 1 && first_sibling_open.is_none() {
                    first_sibling_open = Some(tag.start);
                }
                depth += 1;
            }
        }
        cursor = tag.end;
    }

    // Unclosed. Prefer stopping at the next sibling so a run of unclosed
    // `<tr>`s still splits into separate rows.
    (content_start, first_sibling_open.unwrap_or(html.len()))
}

/// Return the inner HTML of the first `<tag>` whose `attr` equals `value`.
pub fn find_by_attr<'a>(html: &'a str, tag: &str, attr: &str, value: &str) -> Option<&'a str> {
    let mut cursor = 0usize;
    while let Some(t) = next_tag(html, cursor) {
        if !t.closing
            && t.name == tag
            && let Some(found) = get_attr(t.attrs, attr)
            && found.eq_ignore_ascii_case(value)
        {
            let (s, e) = inner_range(html, tag, t.end);
            return Some(&html[s..e]);
        }
        cursor = t.end;
    }
    None
}

/// Return the inner HTML of each top-level `<tag>` in `html`.
///
/// Nested elements of the same name are part of their parent's slice and are
/// not returned separately.
pub fn children<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(t) = next_tag(html, cursor) {
        if RAW_TEXT_ELEMENTS.contains(&t.name.as_str()) && t.opens_scope() {
            cursor = skip_raw_text(html, &t.name, t.end);
            continue;
        }
        if !t.closing && t.name == tag {
            let (s, e) = inner_range(html, tag, t.end);
            out.push(&html[s..e]);
            // Continue after the element we just consumed.
            cursor = e.max(t.end);
            continue;
        }
        cursor = t.end;
    }
    out
}

/// Read one attribute's value out of a tag's attribute section.
///
/// Tolerates unquoted values and the stray doubled quotes Libgen emits.
pub fn get_attr(attrs: &str, want: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let want_lower = want.to_ascii_lowercase();
    let mut search_from = 0usize;

    while let Some(pos) = lower[search_from..].find(&want_lower) {
        let at = search_from + pos;
        search_from = at + want_lower.len();

        // Must be preceded by whitespace or start-of-attrs, so `href` does not
        // match inside `data-href`.
        let preceded_ok = at == 0
            || attrs[..at]
                .chars()
                .next_back()
                .map(|c| c.is_whitespace())
                .unwrap_or(false);
        if !preceded_ok {
            continue;
        }

        let after = &attrs[at + want_lower.len()..];
        let after_trimmed = after.trim_start();
        if !after_trimmed.starts_with('=') {
            continue;
        }
        let value_part = after_trimmed[1..].trim_start();
        let value = if let Some(rest) = value_part.strip_prefix('"') {
            rest.split('"').next().unwrap_or("")
        } else if let Some(rest) = value_part.strip_prefix('\'') {
            rest.split('\'').next().unwrap_or("")
        } else {
            value_part
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or("")
        };
        return Some(decode_entities(value));
    }
    None
}

/// Collect the `href` of every `<a>` in `html`, in document order.
pub fn hrefs(html: &str) -> Vec<String> {
    anchors(html).into_iter().map(|(href, _)| href).collect()
}

/// Collect the `src` of every `<img>` in `html`, in document order.
///
/// `<img>` has no closing tag, so it cannot go through [`anchors`].
pub fn img_srcs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(t) = next_tag(html, cursor) {
        if !t.closing
            && t.name == "img"
            && let Some(src) = get_attr(t.attrs, "src")
            && !src.trim().is_empty()
        {
            out.push(src);
        }
        cursor = t.end;
    }
    out
}

/// Collect `(href, inner_html)` for every `<a>` in `html`, in document order.
///
/// Needed because which link a cell means is decided by where it points, not
/// by its position: a Libgen title cell leads with a series link when the book
/// belongs to a series, and the title itself comes second.
pub fn anchors(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(t) = next_tag(html, cursor) {
        if !t.closing
            && t.name == "a"
            && let Some(href) = get_attr(t.attrs, "href")
        {
            let (s, e) = inner_range(html, "a", t.end);
            out.push((href, html[s..e].to_string()));
            cursor = e.max(t.end);
            continue;
        }
        cursor = t.end;
    }
    out
}

/// Flatten markup to readable plain text: tags dropped, entities decoded,
/// whitespace collapsed.
pub fn text(html: &str) -> String {
    let mut buf = String::with_capacity(html.len() / 2);
    let mut cursor = 0usize;

    loop {
        match next_tag(html, cursor) {
            Some(t) => {
                buf.push_str(&html[cursor..t.start]);
                // Block-ish separators should not glue words together.
                if matches!(t.name.as_str(), "br" | "td" | "tr" | "p" | "div" | "li") {
                    buf.push(' ');
                }
                if RAW_TEXT_ELEMENTS.contains(&t.name.as_str()) && t.opens_scope() {
                    cursor = skip_raw_text(html, &t.name, t.end);
                } else {
                    cursor = t.end;
                }
            }
            None => {
                buf.push_str(&html[cursor..]);
                break;
            }
        }
    }

    collapse_whitespace(&decode_entities(&buf))
}

fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_space = false;
    for c in s.chars() {
        // Non-breaking space counts as whitespace for display purposes.
        if c.is_whitespace() || c == '\u{a0}' {
            if !last_was_space {
                out.push(' ');
            }
            last_was_space = true;
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

/// Decode the HTML entities that actually turn up in Libgen pages.
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;

    while i < s.len() {
        if bytes[i] != b'&' {
            // Copy one whole char.
            let ch = s[i..].chars().next().expect("valid boundary");
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }

        // Entities are short; a missing ';' is common in the wild so we also
        // accept a bare run of alphanumerics.
        let tail = &s[i + 1..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .unwrap_or(tail.len());
        let (body, consumed) = if tail[end..].starts_with(';') {
            (&tail[..end], end + 2) // include '&' and ';'
        } else {
            (&tail[..end], end + 1)
        };

        match decode_one(body) {
            Some(decoded) => {
                out.push_str(&decoded);
                i += consumed;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

fn decode_one(body: &str) -> Option<String> {
    if body.is_empty() {
        return None;
    }
    if let Some(num) = body.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        return char::from_u32(code).map(|c| c.to_string());
    }
    let named = match body.to_ascii_lowercase().as_str() {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{a0}",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "laquo" => "«",
        "raquo" => "»",
        "rsquo" => "’",
        "lsquo" => "‘",
        "ldquo" => "“",
        "rdquo" => "”",
        _ => return None,
    };
    Some(named.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/li_search.html");

    #[test]
    fn finds_the_right_table_among_several() {
        let table = find_by_attr(FIXTURE, "table", "id", "tablelibgen")
            .expect("fixture contains the results table");
        assert!(!table.contains("decoy table"));
        let rows = children(table, "tr");
        // One header row plus four data rows.
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn splits_rows_into_nine_cells() {
        let table = find_by_attr(FIXTURE, "table", "id", "tablelibgen").unwrap();
        let rows = children(table, "tr");
        for row in &rows[1..] {
            let cells = children(row, "td");
            assert_eq!(cells.len(), 9, "each data row has nine columns");
        }
    }

    #[test]
    fn extracts_md5_from_the_mirrors_cell() {
        let table = find_by_attr(FIXTURE, "table", "id", "tablelibgen").unwrap();
        let row = children(table, "tr")[1];
        let cells = children(row, "td");
        let links = hrefs(cells[8]);
        assert!(
            links.iter().any(|h| h.contains("ads.php?md5=")),
            "mirrors cell links to ads.php, got {links:?}"
        );
    }

    #[test]
    fn cell_text_is_flattened_and_decoded() {
        let table = find_by_attr(FIXTURE, "table", "id", "tablelibgen").unwrap();
        let row = children(table, "tr")[1];
        let cells = children(row, "td");
        // Year column.
        assert_eq!(text(cells[3]), "2019");
        // Extension column.
        assert_eq!(text(cells[7]), "doc");
        // Language column.
        assert_eq!(text(cells[4]), "English");
    }

    #[test]
    fn tolerates_stray_quotes_in_attributes() {
        // Lifted from a real Libgen row: note the doubled quote.
        let bad = r#"<span class="badge badge-secondary"">l 3359259</span>"#;
        assert_eq!(text(bad), "l 3359259");
    }

    #[test]
    fn attribute_lookup_is_not_fooled_by_prefixes() {
        let attrs = r#" data-href="/decoy" href="/real" "#;
        assert_eq!(get_attr(attrs, "href").as_deref(), Some("/real"));
    }

    #[test]
    fn ignores_greater_than_inside_attribute_values() {
        let html = r#"<div title="a > b"><td>inner</td></div>"#;
        let cells = children(html, "td");
        assert_eq!(cells.len(), 1);
        assert_eq!(text(cells[0]), "inner");
    }

    #[test]
    fn skips_script_contents() {
        let html = r#"<div><script>var x = "<td>fake</td>";</script><td>real</td></div>"#;
        let inner = children(html, "div");
        let cells = children(inner[0], "td");
        assert_eq!(cells.len(), 1);
        assert_eq!(text(cells[0]), "real");
    }

    #[test]
    fn handles_unclosed_rows() {
        let html = "<table><tr><td>one</td><tr><td>two</td></table>";
        let table = children(html, "table")[0];
        let rows = children(table, "tr");
        assert_eq!(rows.len(), 2);
        assert_eq!(text(rows[0]), "one");
        assert_eq!(text(rows[1]), "two");
    }

    #[test]
    fn nested_tables_do_not_split_into_extra_rows() {
        let html =
            "<table id=\"x\"><tr><td><table><tr><td>inner</td></tr></table></td></tr></table>";
        let outer = find_by_attr(html, "table", "id", "x").unwrap();
        assert_eq!(children(outer, "tr").len(), 1);
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(decode_entities("a &amp; b"), "a & b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_entities("&#8597;"), "↕");
        assert_eq!(decode_entities("&#x41;"), "A");
        assert_eq!(decode_entities("caf&eacute;"), "caf&eacute;"); // unknown, left alone
        assert_eq!(decode_entities("100% &"), "100% &");
    }

    #[test]
    fn text_does_not_glue_words_across_tags() {
        assert_eq!(text("<td>a</td><td>b</td>"), "a b");
        assert_eq!(text("one<br>two"), "one two");
    }

    #[test]
    fn missing_table_returns_none() {
        assert!(find_by_attr("<html><body>nope</body></html>", "table", "id", "x").is_none());
    }

    #[test]
    fn malformed_input_does_not_panic() {
        for junk in [
            "<",
            "<<<>>>",
            "<table id=",
            "<table id=\"x\">unclosed",
            "a < b > c",
            "<!-- unterminated",
            "<td><td><td>",
            "",
        ] {
            let _ = find_by_attr(junk, "table", "id", "x");
            let _ = children(junk, "tr");
            let _ = text(junk);
            let _ = hrefs(junk);
        }
    }
}
