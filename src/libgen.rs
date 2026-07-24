//! Talking to a Libgen mirror: building search URLs, parsing the results
//! table, and resolving a record's actual download link.
//!
//! The flow a mirror expects is:
//!
//! 1. `GET /index.php?req=…` — an HTML results table.
//! 2. `GET /ads.php?md5=…` — an interstitial page holding a one-shot key.
//! 3. `GET /get.php?md5=…&key=…` — the file itself.
//!
//! Column positions are read from the table header rather than hard-coded, so
//! a mirror adding or reordering a column degrades to missing metadata instead
//! of silently shifting every field by one.

use ureq::http::Uri;

use crate::error::{Context, Result};
use crate::model::{Book, SearchQuery, is_md5, parse_size};
use crate::net::{Http, encode_query_value, join_uri};
use crate::{bail, html};

/// Build the search URL for a mirror.
pub fn search_url(base: &Uri, query: &SearchQuery) -> Result<Uri> {
    let mut qs = format!("index.php?req={}", encode_query_value(&query.terms));

    for field in &query.fields {
        qs.push_str("&columns%5B%5D=");
        qs.push_str(field.code());
    }
    // `f` = search files, which is the only object type that yields downloads.
    qs.push_str("&objects%5B%5D=f");
    for topic in &query.topics {
        qs.push_str("&topics%5B%5D=");
        qs.push_str(topic.code());
    }
    qs.push_str(&format!("&res={}&curtab=f", query.page_size()));

    join_uri(base, &qs)
}

/// The interstitial page that mints a download key for a record.
pub fn ads_url(base: &Uri, md5: &str) -> Result<Uri> {
    if !is_md5(md5) {
        bail!("`{md5}` is not a 32-character hex MD5");
    }
    join_uri(base, &format!("ads.php?md5={}", md5.to_ascii_lowercase()))
}

/// Where each piece of metadata lives in the results table.
#[derive(Debug, Default, Clone, Copy)]
struct Columns {
    title: Option<usize>,
    authors: Option<usize>,
    publisher: Option<usize>,
    year: Option<usize>,
    language: Option<usize>,
    pages: Option<usize>,
    size: Option<usize>,
    extension: Option<usize>,
}

impl Columns {
    /// Derive positions from the header row's text.
    fn from_header(header_html: &str) -> Self {
        let mut cols = Columns::default();
        for (i, cell) in html::children(header_html, "th").iter().enumerate() {
            let label = html::text(cell).to_lowercase();
            // Checked most-specific first; the first column's label is
            // "ID ↕ Time add. ↕ Title ↕ Series ↕", so `title` must win there.
            if label.contains("title") && cols.title.is_none() {
                cols.title = Some(i);
            } else if label.contains("author") && cols.authors.is_none() {
                cols.authors = Some(i);
            } else if label.contains("publisher") && cols.publisher.is_none() {
                cols.publisher = Some(i);
            } else if label.contains("year") && cols.year.is_none() {
                cols.year = Some(i);
            } else if label.contains("lang") && cols.language.is_none() {
                cols.language = Some(i);
            } else if label.contains("page") && cols.pages.is_none() {
                cols.pages = Some(i);
            } else if label.contains("size") && cols.size.is_none() {
                cols.size = Some(i);
            } else if label.contains("ext") && cols.extension.is_none() {
                cols.extension = Some(i);
            }
        }
        cols
    }

    /// Positions used by libgen.li when the header cannot be read.
    fn fallback() -> Self {
        Columns {
            title: Some(0),
            authors: Some(1),
            publisher: Some(2),
            year: Some(3),
            language: Some(4),
            pages: Some(5),
            size: Some(6),
            extension: Some(7),
        }
    }

    fn is_usable(&self) -> bool {
        self.title.is_some() && self.extension.is_some()
    }
}

/// Parse a mirror's search results page into records.
///
/// Rows that carry no MD5 are skipped: without one we can neither download the
/// file nor verify it, so the entry is useless to us.
pub fn parse_search_results(page: &str) -> Vec<Book> {
    let Some(table) = find_results_table(page) else {
        return Vec::new();
    };
    let rows = html::children(table, "tr");
    if rows.is_empty() {
        return Vec::new();
    }

    let mut columns = Columns::from_header(rows[0]);
    if !columns.is_usable() {
        columns = Columns::fallback();
    }
    // A row of `<th>` is the header; anything else is data.
    let data_rows = if html::children(rows[0], "th").is_empty() {
        &rows[..]
    } else {
        &rows[1..]
    };

    data_rows
        .iter()
        .filter_map(|row| parse_row(row, &columns))
        .collect()
}

/// Locate the results table, preferring the known id but falling back to the
/// largest table on the page if a mirror renames it.
fn find_results_table(page: &str) -> Option<&str> {
    if let Some(t) = html::find_by_attr(page, "table", "id", "tablelibgen") {
        return Some(t);
    }
    html::children(page, "table")
        .into_iter()
        .filter(|t| t.contains("md5="))
        .max_by_key(|t| html::children(t, "tr").len())
}

fn parse_row(row: &str, columns: &Columns) -> Option<Book> {
    let cells = html::children(row, "td");
    if cells.is_empty() {
        return None;
    }

    // The MD5 is taken from whichever link carries it rather than from a fixed
    // column, since mirrors shuffle the "Mirrors" cell around.
    let md5 = extract_md5(row)?;

    let cell_text = |idx: Option<usize>| -> Option<String> {
        let value = html::text(cells.get(idx?)?);
        if value.is_empty() { None } else { Some(value) }
    };

    let title = columns
        .title
        .and_then(|i| cells.get(i))
        .and_then(|cell| title_from_cell(cell))
        .or_else(|| cell_text(columns.title))
        .unwrap_or_else(|| "Untitled".to_string());

    let size_bytes = cell_text(columns.size).and_then(|t| parse_size(&t));
    let extension = cell_text(columns.extension).map(|e| e.trim().to_ascii_lowercase());

    Some(Book {
        md5,
        title,
        authors: cell_text(columns.authors),
        publisher: cell_text(columns.publisher),
        year: cell_text(columns.year),
        language: cell_text(columns.language),
        pages: cell_text(columns.pages).map(|p| p.replace(" pages", "")),
        size_bytes,
        extension,
        file_id: extract_file_id(row),
    })
}

/// Pick the title out of a title cell.
///
/// The cell is a small pile of links. When a book belongs to a series the cell
/// leads with a bold `series.php` link, and the title proper is the following
/// `edition.php` link — so anchors are chosen by target, not by position.
/// Trailing links hold ISBNs and badges and are ignored.
fn title_from_cell(cell: &str) -> Option<String> {
    let anchors = html::anchors(cell);

    let text_of = |inner: &str| {
        let text = html::text(inner);
        if text.is_empty() { None } else { Some(text) }
    };

    // An edition link is the title.
    if let Some(title) = anchors
        .iter()
        .find(|(href, _)| href.contains("edition.php"))
        .and_then(|(_, inner)| text_of(inner))
    {
        return Some(title);
    }
    // Otherwise the first link that is not a series or an ISBN blob.
    anchors
        .iter()
        .find(|(href, inner)| !href.contains("series.php") && !inner.contains("<font"))
        .and_then(|(_, inner)| text_of(inner))
}

/// Pull the first 32-hex value following `md5=` from any link in the row.
fn extract_md5(row: &str) -> Option<String> {
    for href in html::hrefs(row) {
        if let Some(found) = md5_in(&href) {
            return Some(found);
        }
    }
    None
}

fn md5_in(text: &str) -> Option<String> {
    let mut from = 0usize;
    while let Some(pos) = text[from..].find("md5=") {
        let start = from + pos + 4;
        let candidate: String = text[start..]
            .chars()
            .take_while(|c| c.is_ascii_hexdigit())
            .collect();
        if candidate.len() >= 32 {
            return Some(candidate[..32].to_ascii_lowercase());
        }
        from = start.max(from + pos + 1);
    }
    None
}

fn extract_file_id(row: &str) -> Option<String> {
    for href in html::hrefs(row) {
        if href.contains("file.php?id=") {
            let id: String = href
                .split("id=")
                .nth(1)?
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !id.is_empty() {
                return Some(id);
            }
        }
    }
    None
}

/// Find the real file URL on an interstitial `ads.php` page.
///
/// The link carries a one-shot key, so this page must be fetched fresh for
/// every download rather than cached.
pub fn find_download_link(base: &Uri, page: &str) -> Result<Uri> {
    let links = html::hrefs(page);

    // Preferred: the keyed link libgen.li mints for each visit.
    let candidate = links
        .iter()
        .find(|h| h.contains("get.php") && h.contains("key="))
        // Older mirrors serve an unkeyed get.php.
        .or_else(|| links.iter().find(|h| h.contains("get.php")))
        // Some mirror families use /main/<md5> download hosts instead.
        .or_else(|| {
            links
                .iter()
                .find(|h| h.contains("/main/") && md5_in(h).is_some())
        });

    let Some(href) = candidate else {
        bail!(
            "no download link on the page — the mirror may be rate-limiting, \
             showing a captcha, or the file may have been removed"
        );
    };

    join_uri(base, href).with_context(|| format!("bad download link `{href}`"))
}

/// A record's download URL together with whatever metadata the interstitial
/// page carried.
pub struct Resolved {
    pub url: Uri,
    pub book: Book,
}

/// Read one `name = {value}` field out of the BibTeX block Libgen embeds in
/// its interstitial pages.
///
/// The BibTeX block is used in preference to scraping the neighbouring
/// "Title: … Author(s): …" text because it is delimited, so a value containing
/// a label word cannot bleed into the next field.
fn bibtex_field(text: &str, name: &str) -> Option<String> {
    let needle = format!("{name} = {{");
    let start = text.find(&needle)? + needle.len();
    let mut depth = 1usize;
    let mut value = String::new();
    for c in text[start..].chars() {
        match c {
            '{' => {
                depth += 1;
                value.push(c);
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                value.push(c);
            }
            c => value.push(c),
        }
    }
    let value = value.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

/// Build a record from an interstitial page, for downloads started from an MD5
/// where we never saw a search row.
pub fn parse_ads_metadata(page: &str, md5: &str) -> Book {
    let text = html::text(page);
    Book {
        md5: md5.to_ascii_lowercase(),
        title: bibtex_field(&text, "title").unwrap_or_else(|| md5.to_string()),
        authors: bibtex_field(&text, "author"),
        publisher: bibtex_field(&text, "publisher"),
        year: bibtex_field(&text, "year"),
        // The interstitial does not state the format; the downloader falls back
        // to the Content-Disposition hint, filtered through its allowlist.
        ..Default::default()
    }
}

/// Fetch and parse search results from one mirror.
pub fn search(http: &Http, base: &Uri, query: &SearchQuery) -> Result<Vec<Book>> {
    let url = search_url(base, query)?;
    let page = http.get_text(&url)?;
    let mut books = parse_search_results(&page);
    books.retain(|b| query.matches(b));
    books.truncate(query.limit);
    Ok(books)
}

/// Resolve a record's MD5 to a live download URL.
///
/// The interstitial is fetched once and yields both the keyed link and the
/// record's metadata, since the key it carries is single-use.
pub fn resolve_download(http: &Http, base: &Uri, md5: &str) -> Result<Resolved> {
    let ads = ads_url(base, md5)?;
    let page = http.get_text(&ads)?;
    let url = find_download_link(&ads, &page)?;
    Ok(Resolved {
        url,
        book: parse_ads_metadata(&page, md5),
    })
}

/// The word used to test that a mirror's search works.
///
/// It has to be a term the catalogue genuinely matches. Libgen returns an
/// empty page for very common short words — `the` yields nothing at all — so a
/// stopword canary makes every healthy mirror look broken.
const PROBE_TERM: &str = "science";

/// A cheap end-to-end check that a mirror is actually usable.
///
/// This deliberately runs a real search rather than pinging the front page.
/// Several Libgen mirrors serve a healthy-looking homepage while their search
/// endpoint returns HTTP 500, and a homepage ping would happily route users to
/// one of those.
pub fn probe(http: &Http, base: &Uri) -> Result<usize> {
    let mut query = SearchQuery::new(PROBE_TERM);
    query.limit = 25;
    let books = search(http, base, &query)?;
    if books.is_empty() {
        bail!("search returned no usable results");
    }
    Ok(books.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Field, Topic};

    const FIXTURE: &str = include_str!("../tests/fixtures/li_search.html");

    fn base() -> Uri {
        "https://libgen.li/".parse().unwrap()
    }

    #[test]
    fn parses_real_search_page() {
        let books = parse_search_results(FIXTURE);
        assert_eq!(books.len(), 4, "fixture holds four data rows");

        let first = &books[0];
        assert_eq!(first.md5, "1b9159991f7fb1b3910c0be9ebf7e595");
        assert!(first.title.to_lowercase().contains("rust"));
        assert_eq!(first.year.as_deref(), Some("2019"));
        assert_eq!(first.language.as_deref(), Some("English"));
        assert_eq!(first.extension.as_deref(), Some("doc"));
        assert_eq!(first.size_bytes, Some(19 * 1024));
        assert_eq!(first.file_id.as_deref(), Some("97505425"));
        assert!(first.authors.as_deref().unwrap().contains("Klabnik"));
    }

    #[test]
    fn every_parsed_row_has_a_usable_md5() {
        for book in parse_search_results(FIXTURE) {
            assert!(is_md5(&book.md5), "bad md5: {}", book.md5);
        }
    }

    /// A book in a series puts a bold series link first; the title is the
    /// following edition link. Taking the first anchor picked the series name.
    #[test]
    fn series_link_does_not_masquerade_as_the_title() {
        let cell = r#"<td><b><a href="series.php?id=409328">The BOOK of… </a></b><br><a data-html="true" title="Add/Edit : 2019-08-16; ID: 93389176<br>Notes" href="edition.php?id=138175198">The Rust Programming Language (Covers Rust 2018) <i></i></a><br><a href="edition.php?id=138175198"><i><font color="green"> 1718500440; 9781718500440</font></a></i></td>"#;
        let title = title_from_cell(cell).unwrap();
        assert_eq!(title, "The Rust Programming Language (Covers Rust 2018)");
    }

    /// A `>` inside an attribute value must not truncate the anchor.
    #[test]
    fn markup_inside_attributes_does_not_break_title_extraction() {
        let cell = r#"<td><a title="a <br> b" href="edition.php?id=1">Real Title</a></td>"#;
        assert_eq!(title_from_cell(cell).as_deref(), Some("Real Title"));
    }

    #[test]
    fn title_excludes_isbn_noise_from_the_cell() {
        let books = parse_search_results(FIXTURE);
        // The title cell also contains ISBNs; they must not leak into the title.
        assert!(!books[0].title.contains("9781718500457"));
    }

    #[test]
    fn column_positions_come_from_the_header() {
        let table = html::find_by_attr(FIXTURE, "table", "id", "tablelibgen").unwrap();
        let header = html::children(table, "tr")[0];
        let cols = Columns::from_header(header);
        assert_eq!(cols.title, Some(0));
        assert_eq!(cols.authors, Some(1));
        assert_eq!(cols.publisher, Some(2));
        assert_eq!(cols.year, Some(3));
        assert_eq!(cols.language, Some(4));
        assert_eq!(cols.pages, Some(5));
        assert_eq!(cols.size, Some(6));
        assert_eq!(cols.extension, Some(7));
    }

    /// An inserted column must shift the mapping, not corrupt every field.
    #[test]
    fn tolerates_an_inserted_column() {
        let shifted = FIXTURE
            .replace(
                "<th scope=\"col\"><nobr>Author(s)",
                "<th>New</th><th scope=\"col\"><nobr>Author(s)",
            )
            .replace(
                "<td>Rust Community;Klabnik, Steve;Nichols, Carol</td>",
                "<td>filler</td><td>Rust Community;Klabnik, Steve;Nichols, Carol</td>",
            );
        let books = parse_search_results(&shifted);
        assert_eq!(books.len(), 4);
        assert!(books[0].authors.as_deref().unwrap().contains("Klabnik"));
    }

    #[test]
    fn builds_search_urls_with_filters() {
        let mut q = SearchQuery::new("rust programming");
        q.fields = vec![Field::Author];
        q.topics = vec![Topic::Libgen, Topic::Fiction];
        q.limit = 25;
        let url = search_url(&base(), &q).unwrap().to_string();
        assert!(url.starts_with("https://libgen.li/index.php?req=rust+programming"));
        assert!(url.contains("columns%5B%5D=a"));
        assert!(url.contains("objects%5B%5D=f"));
        assert!(url.contains("topics%5B%5D=l"));
        assert!(url.contains("topics%5B%5D=f"));
        assert!(url.contains("res=25"));
    }

    #[test]
    fn ads_url_rejects_bad_md5() {
        assert!(ads_url(&base(), "not-a-hash").is_err());
        assert!(ads_url(&base(), "1b9159991f7fb1b3910c0be9ebf7e595").is_ok());
    }

    #[test]
    fn finds_keyed_download_link() {
        let page = r##"<html><a href="#">DOWNLOAD</a>
            <a href="get.php?md5=1b9159991f7fb1b3910c0be9ebf7e595&amp;key=TB2KRNBRTS00FED0">GET</a></html>"##;
        let ads = "https://libgen.li/ads.php?md5=1b9159991f7fb1b3910c0be9ebf7e595"
            .parse()
            .unwrap();
        let link = find_download_link(&ads, page).unwrap().to_string();
        assert_eq!(
            link,
            "https://libgen.li/get.php?md5=1b9159991f7fb1b3910c0be9ebf7e595&key=TB2KRNBRTS00FED0"
        );
    }

    #[test]
    fn missing_download_link_is_an_error() {
        let page = "<html><body>Please solve this captcha</body></html>";
        let ads: Uri = "https://libgen.li/ads.php?md5=x".parse().unwrap();
        assert!(find_download_link(&ads, page).is_err());
    }

    #[test]
    fn extracts_md5_from_assorted_link_shapes() {
        assert_eq!(
            md5_in("/ads.php?md5=1B9159991F7FB1B3910C0BE9EBF7E595"),
            Some("1b9159991f7fb1b3910c0be9ebf7e595".to_string())
        );
        assert_eq!(md5_in("/ads.php?md5=tooshort"), None);
        assert_eq!(md5_in("/no/hash/here"), None);
    }

    #[test]
    fn reads_metadata_from_the_interstitial_bibtex() {
        let page = "<html><body>Title: Ignored Label Text \
            @book{book:{97505425}, title = {The Rust Programming Language}, \
            author = {Rust Community;Klabnik, Steve}, \
            publisher = {No Starch Press}, year = {2019}, \
            url = {libgen.li/file.php?md5=x}}</body></html>";
        let book = parse_ads_metadata(page, "1B9159991F7FB1B3910C0BE9EBF7E595");
        assert_eq!(book.title, "The Rust Programming Language");
        assert_eq!(
            book.authors.as_deref(),
            Some("Rust Community;Klabnik, Steve")
        );
        assert_eq!(book.publisher.as_deref(), Some("No Starch Press"));
        assert_eq!(book.year.as_deref(), Some("2019"));
        assert_eq!(book.md5, "1b9159991f7fb1b3910c0be9ebf7e595");
    }

    #[test]
    fn metadata_falls_back_to_the_md5_when_absent() {
        let book = parse_ads_metadata(
            "<html>no bibtex here</html>",
            "1b9159991f7fb1b3910c0be9ebf7e595",
        );
        assert_eq!(book.title, "1b9159991f7fb1b3910c0be9ebf7e595");
        assert!(book.authors.is_none());
    }

    #[test]
    fn bibtex_values_may_contain_braces() {
        let text = "title = {A {nested} title}, year = {2020}";
        assert_eq!(
            bibtex_field(text, "title").as_deref(),
            Some("A {nested} title")
        );
        assert_eq!(bibtex_field(text, "year").as_deref(), Some("2020"));
        assert_eq!(bibtex_field(text, "missing"), None);
    }

    #[test]
    fn garbage_pages_parse_to_nothing() {
        for junk in ["", "<html></html>", "not html at all", "<table></table>"] {
            assert!(parse_search_results(junk).is_empty());
        }
    }
}
