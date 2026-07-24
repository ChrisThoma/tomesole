//! Finding, fetching and caching cover art.
//!
//! Search results carry no cover, so a cover costs a request for the record
//! page followed by a request for the image itself. That is two round trips per
//! book, which is far too many to do while someone is holding a cursor key
//! down — so the interface only asks once a selection has settled, and every
//! answer, including "this book has no cover", is cached on disk.
//!
//! The URL of the image comes out of a page served by a mirror, which makes it
//! attacker-chosen. It is fetched through the same validated client as
//! everything else, so the SSRF guard applies, and then:
//!
//! * the response has to be small — a cover is tens of kilobytes;
//! * it has to actually start with an image signature, so an HTML captcha page
//!   or a disguised payload is discarded rather than cached;
//! * nothing is ever executed, opened or written outside the cache directory,
//!   whose filenames come from the MD5 and not from the URL.

use std::path::PathBuf;

use ureq::http::Uri;

use crate::error::Result;
use crate::graphics::Image;
use crate::model::Book;
use crate::net::{Http, join_uri};
use crate::{bail, jpeg, libgen, net};

/// Covers are small. Anything larger is not a thumbnail and is not wanted.
const MAX_COVER_BYTES: u64 = 4 * 1024 * 1024;

/// What a cover lookup produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cover {
    /// The image file exactly as served, for protocols that take it directly.
    pub encoded: Vec<u8>,
    pub kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Jpeg,
    Png,
    /// Some other image format: passed to a terminal that can read it,
    /// otherwise unusable.
    Other,
}

impl Cover {
    /// Decode to pixels, for the kitty protocol and the half-block fallback.
    ///
    /// Only JPEG is decoded here. PNG is handed to the terminal untouched by
    /// both pixel protocols, and writing an inflate implementation so that the
    /// block fallback can draw the occasional PNG cover is not a good trade.
    pub fn pixels(&self) -> Option<Image> {
        match self.kind {
            Kind::Jpeg => jpeg::decode(&self.encoded).ok(),
            _ => None,
        }
    }

    /// Wrap raw image bytes as a cover, identifying the format from its
    /// signature and rejecting anything that is not actually an image. Used by
    /// the network path and by covers pulled out of a book file on disk.
    pub fn from_bytes(bytes: Vec<u8>) -> Option<Cover> {
        Self::classify(bytes)
    }

    fn classify(bytes: Vec<u8>) -> Option<Cover> {
        let kind = if jpeg::is_jpeg(&bytes) {
            Kind::Jpeg
        } else if jpeg::is_png(&bytes) {
            Kind::Png
        } else if bytes.starts_with(b"GIF8")
            || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
        {
            Kind::Other
        } else {
            // Not an image at all: an error page, a captcha, or something
            // pretending. Nothing here is worth keeping.
            return None;
        };
        Some(Cover {
            encoded: bytes,
            kind,
        })
    }

    fn extension(&self) -> &'static str {
        match self.kind {
            Kind::Jpeg => "jpg",
            Kind::Png => "png",
            Kind::Other => "img",
        }
    }
}

/// Fetch a book's cover, from the cache when possible.
///
/// `Ok(None)` means the lookup succeeded and the book simply has no cover;
/// that answer is cached too, so a book without one is only ever looked up
/// once.
pub fn fetch(http: &Http, base: &Uri, book: &Book) -> Result<Option<Cover>> {
    if !crate::model::is_md5(&book.md5) {
        bail!("a cover needs a valid MD5");
    }
    if let Some(cached) = from_cache(&book.md5) {
        return Ok(cached);
    }

    let cover = match find_url(http, base, book)? {
        // A record page has no business pointing its cover at another host,
        // and the one reason it would is to have us fetch something from
        // somewhere else on its behalf.
        Some((url, from)) if same_host(base, &url) => download(http, &url, &from)?,
        Some(_) | None => None,
    };
    store(&book.md5, cover.as_ref());
    Ok(cover)
}

/// Locate the cover image on a record page, and say which page it was on.
fn find_url(http: &Http, base: &Uri, book: &Book) -> Result<Option<(Uri, Uri)>> {
    // `file.php` is the record page proper. `ads.php` also carries the cover
    // but mints a single-use download key as a side effect, so it is only the
    // fallback, for records whose file id the search row did not carry.
    let page_url = match &book.file_id {
        Some(id) if id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty() => {
            join_uri(base, &format!("file.php?id={id}"))?
        }
        _ => libgen::ads_url(base, &book.md5)?,
    };

    let page = http.get_text(&page_url)?;
    let Some(src) = pick_cover_src(&page) else {
        return Ok(None);
    };
    Ok(Some((join_uri(base, &src)?, page_url)))
}

/// Choose the cover out of every image on a record page.
///
/// Libgen pages are full of icons, flags and spacers. The cover is the one
/// under a `covers/` path; failing that, an image whose name says so.
fn pick_cover_src(page: &str) -> Option<String> {
    let images = crate::html::img_srcs(page);

    let looks_like_cover = |src: &&String| {
        let lower = src.to_lowercase();
        lower.contains("/covers/") || lower.contains("cover")
    };
    let is_image_file = |src: &&String| {
        let lower = src.to_lowercase();
        [".jpg", ".jpeg", ".png", ".gif", ".webp"]
            .iter()
            .any(|ext| lower.contains(ext))
    };

    images
        .iter()
        .find(|src| looks_like_cover(src) && is_image_file(src))
        .or_else(|| images.iter().find(looks_like_cover))
        .cloned()
}

/// Fetch the image itself, refusing anything that is not one.
///
/// `from` is the record page the image was named on. Mirrors hotlink-protect
/// covers and answer a request without it with a zero-byte file.
fn download(http: &Http, url: &Uri, from: &Uri) -> Result<Option<Cover>> {
    let response = http.get_referred(url, from)?;
    if !(200..300).contains(&response.status) {
        return Ok(None);
    }
    // The header is the mirror's to choose, so it is a reason to stop early,
    // never a reason to trust what follows.
    if let Some(length) = response.content_length
        && length > MAX_COVER_BYTES
    {
        return Ok(None);
    }
    if response
        .content_type
        .as_deref()
        .is_some_and(|ct| ct.to_ascii_lowercase().contains("text/html"))
    {
        return Ok(None);
    }

    let bytes = response
        .body
        .into_with_config()
        .limit(MAX_COVER_BYTES)
        .read_to_vec()
        .unwrap_or_default();

    Ok(Cover::classify(bytes))
}

// --- the on-disk cache ----------------------------------------------------

/// A zero-byte file recording that a book has no cover, so the record page is
/// not fetched again on every visit.
const MISSING: &str = "none";

fn cache_path(md5: &str, extension: &str) -> PathBuf {
    crate::config::cover_cache_dir().join(format!("{md5}.{extension}"))
}

/// The cover for this MD5 if one was fetched in a past search, without going
/// near the network. `Some(None)` means "looked before, there is none".
///
/// This is what lets the library show a cover for a book whose file carries
/// none of its own: if you saw it while searching, it is already on disk.
pub fn cached(md5: &str) -> Option<Option<Cover>> {
    from_cache(md5)
}

/// `Some(None)` means "cached, and there is no cover"; `None` means "not
/// cached, go and look".
fn from_cache(md5: &str) -> Option<Option<Cover>> {
    if cache_path(md5, MISSING).exists() {
        return Some(None);
    }
    for extension in ["jpg", "png", "img"] {
        if let Ok(bytes) = std::fs::read(cache_path(md5, extension))
            && !bytes.is_empty()
            && let Some(cover) = Cover::classify(bytes)
        {
            return Some(Some(cover));
        }
    }
    None
}

/// Remember a lookup. Best effort: a cover is not worth an error.
fn store(md5: &str, cover: Option<&Cover>) {
    let dir = crate::config::cover_cache_dir();
    if crate::config::ensure_dir(&dir).is_err() {
        return;
    }
    match cover {
        Some(cover) => {
            let _ = std::fs::write(cache_path(md5, cover.extension()), &cover.encoded);
        }
        None => {
            let _ = std::fs::write(cache_path(md5, MISSING), b"");
        }
    }
}

/// Empty the cover cache.
///
/// Cleared alongside the download history: which covers have been fetched is
/// a record of what someone has been looking at, and "forget my downloads"
/// plainly means that too.
pub fn clear_cache() -> Result<()> {
    let dir = crate::config::cover_cache_dir();
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(crate::err!("could not clear {}: {e}", dir.display())),
    }
}

/// How much of the cover cache is on disk, for `doctor`.
pub fn cache_size() -> (usize, u64) {
    let Ok(entries) = std::fs::read_dir(crate::config::cover_cache_dir()) else {
        return (0, 0);
    };
    let mut count = 0;
    let mut bytes = 0;
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata()
            && meta.is_file()
        {
            count += 1;
            bytes += meta.len();
        }
    }
    (count, bytes)
}

/// Reject an image URL that points somewhere other than the mirror it came
/// from, which is the only reason a record page would name another host.
pub fn same_host(base: &Uri, image: &Uri) -> bool {
    match (net::host_of(base), net::host_of(image)) {
        (Ok(a), Ok(b)) => a.eq_ignore_ascii_case(&b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"
        <html><body>
          <img src="/img/logo.png" alt="logo">
          <img src="/templates/flags/en.gif">
          <table><tr><td>
            <img src="/covers/2649000/1b9159991f7fb1b3910c0be9ebf7e595.jpg" alt="cover">
          </td></tr></table>
        </body></html>
    "#;

    #[test]
    fn the_cover_is_picked_out_of_the_page_furniture() {
        assert_eq!(
            pick_cover_src(PAGE).as_deref(),
            Some("/covers/2649000/1b9159991f7fb1b3910c0be9ebf7e595.jpg")
        );
    }

    #[test]
    fn a_page_with_no_cover_says_so() {
        assert_eq!(
            pick_cover_src("<html><img src='/img/logo.png'></html>"),
            None
        );
        assert_eq!(pick_cover_src("<html>nothing at all</html>"), None);
    }

    #[test]
    fn a_cover_named_without_a_covers_directory_is_still_found() {
        let page = "<img src='/thumbs/book-cover-123.jpeg'>";
        assert_eq!(
            pick_cover_src(page).as_deref(),
            Some("/thumbs/book-cover-123.jpeg")
        );
    }

    #[test]
    fn image_signatures_decide_the_kind() {
        assert_eq!(
            Cover::classify(b"\xff\xd8\xff\xe0stuff".to_vec())
                .unwrap()
                .kind,
            Kind::Jpeg
        );
        assert_eq!(
            Cover::classify(b"\x89PNG\r\n\x1a\nstuff".to_vec())
                .unwrap()
                .kind,
            Kind::Png
        );
        assert_eq!(
            Cover::classify(b"GIF89a...".to_vec()).unwrap().kind,
            Kind::Other
        );
    }

    /// The one that matters: a mirror answering with a page instead of a
    /// picture must not have it cached and handed to a terminal.
    #[test]
    fn a_web_page_is_not_an_image() {
        assert!(Cover::classify(b"<html><body>captcha</body></html>".to_vec()).is_none());
        assert!(Cover::classify(b"".to_vec()).is_none());
        assert!(Cover::classify(b"\x7fELF".to_vec()).is_none());
    }

    #[test]
    fn a_real_jpeg_cover_decodes_to_pixels() {
        let cover =
            Cover::classify(include_bytes!("../tests/fixtures/cover.jpg").to_vec()).unwrap();
        assert_eq!(cover.kind, Kind::Jpeg);
        let image = cover.pixels().expect("the fixture decodes");
        assert_eq!((image.width, image.height), (24, 24));
    }

    #[test]
    fn a_png_cover_is_kept_but_not_decoded_here() {
        let cover = Cover::classify(b"\x89PNG\r\n\x1a\nnot really".to_vec()).unwrap();
        assert_eq!(cover.extension(), "png");
        assert!(cover.pixels().is_none());
    }

    /// End to end against a live mirror.
    ///
    /// Ignored, because every other test in this crate runs offline and a
    /// suite that fails on a train is worse than useless. Run it by hand with
    /// `cargo test -- --ignored covers_can_be_fetched` after touching either
    /// the page scraping or the fetch path.
    #[test]
    #[ignore = "needs the network and a working mirror"]
    fn covers_can_be_fetched_from_a_real_mirror() {
        let policy = crate::net::NetPolicy::default();
        let http = Http::new(policy).unwrap();
        let pool = crate::mirror::resolve(
            &policy,
            crate::mirror::ResolveOptions {
                explicit: &[],
                refresh: false,
                progress: &|_| {},
            },
        )
        .expect("a mirror should be reachable");
        let base = pool.mirrors().first().cloned().expect("a mirror");

        let mut query = crate::model::SearchQuery::new("dune");
        query.limit = 5;
        let books = crate::libgen::search(&http, &base, &query).expect("a search");
        assert!(!books.is_empty(), "the search found nothing to look up");

        // Not every record has a cover, so the test is that the lookup runs
        // cleanly and that anything it does return really is an image.
        let mut found = 0;
        for book in books.iter().take(3) {
            match fetch(&http, &base, book).expect("the lookup should not error") {
                Some(cover) => {
                    found += 1;
                    assert!(!cover.encoded.is_empty());
                    if cover.kind == Kind::Jpeg {
                        let image = cover.pixels().expect("a JPEG cover should decode");
                        assert!(image.width > 10 && image.height > 10, "{image:?}");
                    }
                }
                None => continue,
            }
        }
        eprintln!("{found} of 3 records had a cover");
    }

    #[test]
    fn hosts_are_compared_for_the_image_url() {
        let base: Uri = "https://libgen.li/index.php".parse().unwrap();
        assert!(same_host(
            &base,
            &"https://libgen.li/covers/x.jpg".parse().unwrap()
        ));
        assert!(!same_host(
            &base,
            &"https://elsewhere.example/covers/x.jpg".parse().unwrap()
        ));
    }
}
