//! Pulling a book's own cover out of the file you already downloaded.
//!
//! A search result carries no cover, so on the Search tab one is fetched from a
//! mirror. But a book on disk usually ships its jacket inside it — an EPUB is a
//! zip with the image as an entry, a MOBI embeds it as a record pointed at by
//! the metadata — so the Library tab needs no network at all: it reads the file
//! it already has.
//!
//! Everything here treats the file as ragged input. Book files come from Libgen
//! and are not to be trusted any more than the HTML was: every offset and
//! length is bounds-checked with `get`, nothing is decoded in place, and a
//! malformed or truncated file yields `None` rather than a panic. The bytes
//! that come back are still run through [`Cover::from_bytes`], so anything that
//! is not actually a JPEG, PNG or the like is discarded before it is shown.

use std::path::Path;

use crate::cover::Cover;

/// A book file is not a thumbnail store; a cover past this is not a cover.
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Read no more than this off disk. Comfortably larger than any ebook we mean
/// to open, and a hard ceiling against a file that claims to be enormous.
const MAX_FILE_BYTES: u64 = 96 * 1024 * 1024;

/// The cover carried inside a book file, if it has one this can reach.
///
/// Dispatches on the extension because the container is what decides how to
/// look, not the contents. An unknown or coverless format is simply `None`.
pub fn cover(path: &Path) -> Option<Cover> {
    // Refuse to slurp something the size of a disk image into memory.
    if std::fs::metadata(path).ok()?.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let raw = match ext.as_str() {
        "epub" | "cbz" => zip_cover(&bytes, &ext),
        "mobi" | "azw" | "azw3" | "azw4" | "prc" => mobi_cover(&bytes),
        _ => None,
    }?;
    if raw.len() > MAX_IMAGE_BYTES {
        return None;
    }
    Cover::from_bytes(raw)
}

// --- little-endian / big-endian readers, all bounds-checked -----------------

fn be_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn le_u16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn le_u32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// True when the bytes open with an image signature we would keep.
fn is_image(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xD8, 0xFF])                 // JPEG
        || b.starts_with(b"\x89PNG\r\n\x1a\n")         // PNG
        || b.starts_with(b"GIF8")                      // GIF
        || (b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP"))
}

// --- MOBI / AZW -------------------------------------------------------------
//
// A MOBI is a Palm database (PDB): a header, a table of record offsets, then
// the records themselves. Record 0 holds the PalmDOC and MOBI headers and, in
// most files, an EXTH block of typed metadata. EXTH record 201 gives the cover
// as an index into the run of image records at the end of the file.

/// Offsets into record 0's data, past the 16-byte PalmDOC header.
const MOBI_MAGIC_AT: usize = 16;
/// EXTH tag whose value is the cover's index among the image records.
const EXTH_COVER: u32 = 201;

fn mobi_cover(data: &[u8]) -> Option<Vec<u8>> {
    // PDB record table: a count at 76, then one u32 offset per record at 78.
    let count = be_u16(data, 76)? as usize;
    // A wild count would have us read megabytes of "offsets" out of the header;
    // no real book has anywhere near this many records.
    if count == 0 || count > 65_000 {
        return None;
    }
    let record = |i: usize| -> Option<&[u8]> {
        let start = be_u32(data, 78 + i * 8)? as usize;
        let end = if i + 1 < count {
            be_u32(data, 78 + (i + 1) * 8)? as usize
        } else {
            data.len()
        };
        data.get(start..end.min(data.len()).max(start))
    };

    let rec0 = record(0)?;
    if rec0.get(MOBI_MAGIC_AT..MOBI_MAGIC_AT + 4)? != b"MOBI" {
        return None;
    }

    // The image records sit at the end as a contiguous run; find where it
    // starts rather than trusting the header field, which is often unset.
    let first_image = (0..count).find(|&i| record(i).is_some_and(is_image))?;
    let image_at = |idx: usize| record(first_image + idx).filter(|r| is_image(r));

    // Prefer the cover the metadata names; fall back to the largest image,
    // which is what a cover almost always is among a book's figures.
    if let Some(idx) = exth_cover_index(rec0)
        && let Some(img) = image_at(idx as usize)
    {
        return Some(img.to_vec());
    }
    (first_image..count)
        .filter_map(record)
        .filter(|r| is_image(r))
        .max_by_key(|r| r.len())
        .map(<[u8]>::to_vec)
}

/// The cover index from record 0's EXTH block, if it carries one.
fn exth_cover_index(rec0: &[u8]) -> Option<u32> {
    // EXTH follows the MOBI header; locating it by signature avoids depending
    // on the header-length field being right.
    let exth = find(rec0, b"EXTH")?;
    let entries = be_u32(rec0, exth + 8)? as usize;
    let mut p = exth + 12;
    for _ in 0..entries.min(4096) {
        let kind = be_u32(rec0, p)?;
        let len = be_u32(rec0, p + 4)? as usize;
        // A record shorter than its own header, or one that would not advance
        // the cursor, means the block is corrupt — stop rather than spin.
        if len < 8 {
            return None;
        }
        if kind == EXTH_COVER {
            return be_u32(rec0, p + 8);
        }
        p = p.checked_add(len)?;
    }
    None
}

/// First offset of `needle` in `haystack`, searched over a bounded window so a
/// pathological file cannot make this quadratic across megabytes.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let window = haystack.len().min(8192);
    haystack
        .get(..window)?
        .windows(needle.len())
        .position(|w| w == needle)
}

// --- EPUB / CBZ (zip) -------------------------------------------------------
//
// A zip is read from the back: the End Of Central Directory record points at
// the central directory, whose entries name every file and where each begins.
// We never inflate anything — the cover image inside an EPUB is itself already
// compressed (JPEG/PNG) and so is almost always *stored*, and a stored entry is
// just a slice of the file. An entry that turns out to be deflated is skipped
// rather than reached for with a decompressor this crate deliberately omits.

const EOCD_SIG: u32 = 0x0605_4b50;
const CENTRAL_SIG: u32 = 0x0201_4b50;
const LOCAL_SIG: u32 = 0x0403_4b50;
const STORED: u16 = 0;

/// One stored file inside the archive: its name and its raw bytes' location.
struct ZipEntry {
    name: String,
    method: u16,
    local_header: usize,
    size: usize,
}

fn zip_cover(data: &[u8], ext: &str) -> Option<Vec<u8>> {
    let entries = zip_entries(data)?;
    let pick = if ext == "cbz" {
        // A comic archive is pages in order; the first image is the cover.
        comic_first_image(&entries)
    } else {
        epub_cover(&entries)
    }?;
    read_stored(data, pick)
}

/// Walk the central directory into a list of entries.
fn zip_entries(data: &[u8]) -> Option<Vec<ZipEntry>> {
    let eocd = find_eocd(data)?;
    let count = le_u16(data, eocd + 10)? as usize;
    let mut p = le_u32(data, eocd + 16)? as usize;

    let mut out = Vec::new();
    for _ in 0..count.min(16_384) {
        if le_u32(data, p)? != CENTRAL_SIG {
            break;
        }
        let method = le_u16(data, p + 10)?;
        let size = le_u32(data, p + 20)? as usize;
        let name_len = le_u16(data, p + 28)? as usize;
        let extra_len = le_u16(data, p + 30)? as usize;
        let comment_len = le_u16(data, p + 32)? as usize;
        let local_header = le_u32(data, p + 42)? as usize;
        let name = data
            .get(p + 46..p + 46 + name_len)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .unwrap_or_default();
        out.push(ZipEntry {
            name,
            method,
            local_header,
            size,
        });
        p = p.checked_add(46 + name_len + extra_len + comment_len)?;
    }
    (!out.is_empty()).then_some(out)
}

/// Locate the End Of Central Directory record near the file's tail.
fn find_eocd(data: &[u8]) -> Option<usize> {
    // The EOCD is 22 bytes plus a comment of up to 64 KB; scan back over that
    // window for its signature, taking the last match.
    let window = data.len().min(22 + 0xFFFF);
    let start = data.len() - window;
    let tail = data.get(start..)?;
    tail.windows(4)
        .rposition(|w| u32::from_le_bytes(w.try_into().unwrap()) == EOCD_SIG)
        .map(|i| start + i)
}

/// The bytes of a stored entry, sliced straight out of the file.
fn read_stored(data: &[u8], entry: &ZipEntry) -> Option<Vec<u8>> {
    if entry.method != STORED {
        return None;
    }
    let h = entry.local_header;
    if le_u32(data, h)? != LOCAL_SIG {
        return None;
    }
    // The local header repeats the name and extra fields with its own lengths.
    let name_len = le_u16(data, h + 26)? as usize;
    let extra_len = le_u16(data, h + 28)? as usize;
    let start = h + 30 + name_len + extra_len;
    let bytes = data.get(start..start + entry.size)?;
    is_image(bytes).then(|| bytes.to_vec())
}

fn is_image_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
}

/// Choose the cover among an EPUB's entries.
///
/// Reading the OPF manifest to find the declared cover would mean inflating the
/// package document, so instead we go by name: an image whose path says
/// "cover" wins, and failing that the largest stored image, which is
/// overwhelmingly the jacket rather than an inline figure.
fn epub_cover(entries: &[ZipEntry]) -> Option<&ZipEntry> {
    let images: Vec<&ZipEntry> = entries
        .iter()
        .filter(|e| e.method == STORED && is_image_name(&e.name))
        .collect();
    images
        .iter()
        .find(|e| e.name.to_ascii_lowercase().contains("cover"))
        .copied()
        .or_else(|| images.iter().max_by_key(|e| e.size).copied())
}

/// The first page of a comic archive, in the order a reader would show them.
fn comic_first_image(entries: &[ZipEntry]) -> Option<&ZipEntry> {
    let mut images: Vec<&ZipEntry> = entries
        .iter()
        .filter(|e| e.method == STORED && is_image_name(&e.name))
        .collect();
    images.sort_by_key(|e| e.name.to_ascii_lowercase());
    images.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A one-record JPEG "image", small but signature-valid.
    fn jpeg(size: usize) -> Vec<u8> {
        let mut v = vec![0xFF, 0xD8, 0xFF, 0xE0];
        v.resize(size.max(4), 0x42);
        v
    }

    /// Assemble a minimal but structurally real MOBI: a PDB record table, a
    /// record 0 carrying a MOBI+EXTH header with a cover tag, and a run of
    /// image records at the end.
    fn build_mobi(cover_index: Option<u32>, images: &[Vec<u8>], text_records: usize) -> Vec<u8> {
        // Record 0.
        let mut rec0 = vec![0u8; 16]; // PalmDOC header, unused here
        rec0.extend_from_slice(b"MOBI");
        rec0.extend_from_slice(&[0u8; 20]); // rest of MOBI header, unused
        // EXTH block.
        let mut exth = Vec::new();
        exth.extend_from_slice(b"EXTH");
        let mut records = Vec::new();
        let mut n = 0u32;
        if let Some(idx) = cover_index {
            records.extend_from_slice(&EXTH_COVER.to_be_bytes());
            records.extend_from_slice(&12u32.to_be_bytes()); // 8 header + 4 value
            records.extend_from_slice(&idx.to_be_bytes());
            n += 1;
        }
        let exth_len = 12 + records.len();
        exth.extend_from_slice(&(exth_len as u32).to_be_bytes());
        exth.extend_from_slice(&n.to_be_bytes());
        exth.extend_from_slice(&records);
        rec0.extend_from_slice(&exth);

        // The full record list: record 0, some text records, then images.
        let mut recs: Vec<Vec<u8>> = vec![rec0];
        for _ in 0..text_records {
            recs.push(b"text".to_vec());
        }
        for img in images {
            recs.push(img.clone());
        }

        // PDB header (78 bytes) + 8 bytes per record entry, then the data.
        let count = recs.len();
        let header_len = 78 + count * 8;
        let mut out = vec![0u8; 78];
        out[76..78].copy_from_slice(&(count as u16).to_be_bytes());
        let mut cursor = header_len;
        let mut table = Vec::new();
        for r in &recs {
            table.extend_from_slice(&(cursor as u32).to_be_bytes());
            table.extend_from_slice(&[0u8; 4]); // attributes + unique id
            cursor += r.len();
        }
        out.extend_from_slice(&table);
        for r in &recs {
            out.extend_from_slice(r);
        }
        out
    }

    #[test]
    fn a_mobi_cover_is_found_through_its_exth_index() {
        // Three images; the metadata points at the middle one.
        let images = vec![jpeg(50), jpeg(900), jpeg(80)];
        let mobi = build_mobi(Some(1), &images, 4);
        let cover = cover_from_bytes(&mobi, "mobi").expect("a cover");
        assert_eq!(cover.encoded.len(), 900);
    }

    #[test]
    fn a_mobi_without_a_cover_tag_falls_back_to_the_largest_image() {
        let images = vec![jpeg(50), jpeg(1200), jpeg(80)];
        let mobi = build_mobi(None, &images, 2);
        let cover = cover_from_bytes(&mobi, "mobi").expect("a cover");
        assert_eq!(cover.encoded.len(), 1200, "the jacket is the big one");
    }

    #[test]
    fn a_truncated_mobi_yields_none_not_a_panic() {
        let mobi = build_mobi(Some(0), &[jpeg(300)], 3);
        for cut in (0..mobi.len()).step_by(7) {
            let _ = cover_from_bytes(&mobi[..cut], "mobi");
        }
    }

    /// A minimal zip with the given stored files, in order.
    fn build_zip(files: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        let mut locals = Vec::new();
        for (name, body) in files {
            let offset = locals.len();
            let name_b = name.as_bytes();
            // Local file header.
            locals.extend_from_slice(&LOCAL_SIG.to_le_bytes());
            locals.extend_from_slice(&[0u8; 4]); // version + flags
            locals.extend_from_slice(&STORED.to_le_bytes());
            locals.extend_from_slice(&[0u8; 8]); // time/date + crc
            locals.extend_from_slice(&(body.len() as u32).to_le_bytes()); // comp size
            locals.extend_from_slice(&(body.len() as u32).to_le_bytes()); // uncomp size
            locals.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            locals.extend_from_slice(&0u16.to_le_bytes()); // extra len
            locals.extend_from_slice(name_b);
            locals.extend_from_slice(body);
            // Central directory entry.
            central.extend_from_slice(&CENTRAL_SIG.to_le_bytes());
            central.extend_from_slice(&[0u8; 6]); // versions + flags
            central.extend_from_slice(&STORED.to_le_bytes());
            central.extend_from_slice(&[0u8; 8]); // time/date + crc
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name_b.len() as u16).to_le_bytes());
            // bytes 30..42: extra len, comment len, disk start, internal and
            // external attrs — all zero here — then the local header offset.
            central.extend_from_slice(&[0u8; 12]);
            central.extend_from_slice(&(offset as u32).to_le_bytes());
            central.extend_from_slice(name_b);
        }
        let central_offset = locals.len();
        out.extend_from_slice(&locals);
        out.extend_from_slice(&central);
        // EOCD: sig, disk numbers (4), entries this disk (2), total entries
        // (2), central-directory size (4), central-directory offset (4),
        // comment length (2).
        out.extend_from_slice(&EOCD_SIG.to_le_bytes());
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&(central_offset as u32).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn an_epub_prefers_the_image_named_cover() {
        let zip = build_zip(&[
            ("mimetype", b"application/epub+zip".to_vec()),
            ("OEBPS/images/figure1.jpg", jpeg(2000)),
            ("OEBPS/images/cover.jpg", jpeg(300)),
        ]);
        let cover = cover_from_bytes(&zip, "epub").expect("a cover");
        assert_eq!(
            cover.encoded.len(),
            300,
            "the one named cover wins over a bigger figure"
        );
    }

    #[test]
    fn an_epub_without_a_named_cover_takes_the_largest_image() {
        let zip = build_zip(&[
            ("OEBPS/a.jpg", jpeg(400)),
            ("OEBPS/b.jpg", jpeg(1500)),
        ]);
        let cover = cover_from_bytes(&zip, "epub").expect("a cover");
        assert_eq!(cover.encoded.len(), 1500);
    }

    #[test]
    fn a_cbz_takes_the_first_page() {
        let zip = build_zip(&[
            ("002.jpg", jpeg(400)),
            ("001.jpg", jpeg(500)),
            ("003.jpg", jpeg(600)),
        ]);
        let cover = cover_from_bytes(&zip, "cbz").expect("a cover");
        assert_eq!(cover.encoded.len(), 500, "001 comes first, size aside");
    }

    #[test]
    fn a_non_image_entry_is_not_mistaken_for_a_cover() {
        let zip = build_zip(&[("cover.jpg", b"not really a jpeg".to_vec())]);
        assert!(cover_from_bytes(&zip, "epub").is_none());
    }

    #[test]
    fn garbage_is_rejected_without_panicking() {
        assert!(cover_from_bytes(b"", "epub").is_none());
        assert!(cover_from_bytes(b"PK\x03\x04 nonsense", "epub").is_none());
        assert!(cover_from_bytes(&[0xFF; 500], "mobi").is_none());
    }

    /// Extract a cover from a real book file named in `CLIBGEN_COVER_FILE`,
    /// writing it beside `CLIBGEN_PREVIEW_DIR` — a hand check against files the
    /// repo cannot carry. `cargo test -- --ignored extracts_from_a_real_file`.
    #[test]
    #[ignore]
    fn extracts_from_a_real_file() {
        let Some(path) = std::env::var_os("CLIBGEN_COVER_FILE") else {
            return;
        };
        let path = std::path::PathBuf::from(path);
        let cover = super::cover(&path).expect("the file should carry a cover");
        assert!(
            cover.encoded.len() > 1000,
            "a real jacket is more than a thumbnail: {} bytes",
            cover.encoded.len()
        );
        if let Some(dir) = std::env::var_os("CLIBGEN_PREVIEW_DIR") {
            let out = std::path::PathBuf::from(dir).join("rust_extracted_cover.jpg");
            std::fs::write(&out, &cover.encoded).unwrap();
            eprintln!("wrote {} ({} bytes)", out.display(), cover.encoded.len());
        }
    }

    /// The extension dispatch without going through the filesystem.
    fn cover_from_bytes(bytes: &[u8], ext: &str) -> Option<Cover> {
        let raw = match ext {
            "epub" | "cbz" => zip_cover(bytes, ext),
            "mobi" => mobi_cover(bytes),
            _ => None,
        }?;
        Cover::from_bytes(raw)
    }
}
