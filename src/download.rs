//! Fetching a file safely.
//!
//! Everything arriving here came from an untrusted mirror, so the download path
//! assumes the bytes, the filename, and the content type are all attacker-
//! influenced:
//!
//! * **Filenames are built from catalogue metadata, never from the server's
//!   `Content-Disposition`.** That header is the mirror's to choose, and it is
//!   the obvious way to try for a path traversal or a dotfile.
//! * **The extension is checked against an allowlist.** A "book" that wants to
//!   be `.exe`, `.dmg` or `.sh` is refused outright; anything unrecognised is
//!   saved as `.bin` so it cannot be launched by double-clicking.
//! * **Size is capped twice**: against the advertised `Content-Length`, and
//!   again against the bytes actually streamed, since the header can lie.
//! * **An HTML response is rejected**, because that means a captcha or error
//!   page rather than a file.
//! * **The MD5 is verified as the bytes stream past**, then the file is only
//!   moved into place if it matches. A failed file never lands under its final
//!   name.
//! * On macOS the result is flagged with the same quarantine attribute a
//!   browser would set, so Gatekeeper still gets a say.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ureq::http::Uri;

use crate::error::{Context, Result};
use crate::md5::Md5;
use crate::model::Book;
use crate::net::Http;
use crate::{bail, err};

/// Formats we are willing to write with their own extension.
const ALLOWED_EXTENSIONS: &[&str] = &[
    "pdf", "epub", "mobi", "azw", "azw3", "djvu", "djv", "fb2", "txt", "rtf", "doc", "docx", "odt",
    "chm", "lit", "prc", "pdb", "cbz", "cbr", "zip", "rar", "7z", "gz", "tar", "md", "ps", "dvi",
    "htm", "html", "xps", "epub3", "mp3", "m4b", "csv", "json", "xml",
];

/// Extensions that are executable or installable on some platform. A Libgen
/// record claiming one of these is not a book, so we refuse rather than guess.
const DANGEROUS_EXTENSIONS: &[&str] = &[
    "exe", "dll", "com", "bat", "cmd", "scr", "pif", "msi", "msp", "jar", "app", "dmg", "pkg",
    "sh", "bash", "zsh", "fish", "ps1", "psm1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "scpt",
    "command", "reg", "lnk", "elf", "so", "dylib", "deb", "rpm", "apk", "run", "bin_exec",
    "action", "workflow", "term",
];

/// Names Windows refuses to create, reserved regardless of extension.
const RESERVED_STEMS: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Longest filename we will produce, in bytes, leaving room for `.part` and
/// staying under the usual 255-byte limit.
const MAX_FILENAME_BYTES: usize = 180;

#[derive(Debug, Clone)]
pub struct Options {
    pub dest_dir: PathBuf,
    /// Overrides the generated filename. Still sanitised.
    pub filename: Option<String>,
    pub max_bytes: u64,
    /// Verify the MD5. Off is supported but strongly discouraged.
    pub verify: bool,
    /// Overwrite an existing file instead of skipping it.
    pub force: bool,
    pub resume: bool,
}

#[derive(Debug)]
pub struct Outcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub verified: bool,
    /// True when the file was already present and left alone.
    pub skipped: bool,
}

/// Decide what to call the file, using only catalogue metadata.
pub fn build_filename(book: &Book, override_name: Option<&str>) -> Result<String> {
    let extension = resolve_extension(book.ext())?;

    if let Some(name) = override_name {
        let cleaned = sanitize_component(name);
        if let Some((stem, ext)) = cleaned.rsplit_once('.')
            && looks_like_extension(ext)
        {
            // Honour the user's extension, or refuse it if it is dangerous.
            // Falling through instead would produce `mybook.exe.epub`, and
            // a double extension is the exact shape we are guarding against.
            let resolved = resolve_extension(ext)?;
            let stem = sanitize_component(stem);
            let stem = if stem.is_empty() {
                book.md5.clone()
            } else {
                stem
            };
            return Ok(cap_length(&stem, &resolved));
        }
        let stem = if cleaned.is_empty() {
            book.md5.clone()
        } else {
            cleaned
        };
        return Ok(cap_length(&stem, &extension));
    }

    let mut stem = String::new();
    if let Some(authors) = book.authors.as_deref().filter(|a| !a.trim().is_empty()) {
        // Multiple authors are semicolon-separated; one is plenty for a name.
        let first = authors.split(';').next().unwrap_or(authors).trim();
        if !first.is_empty() {
            stem.push_str(first);
            stem.push_str(" - ");
        }
    }
    stem.push_str(&book.title);
    if let Some(year) = book.year.as_deref().filter(|y| !y.trim().is_empty()) {
        stem.push_str(&format!(" ({year})"));
    }

    let stem = sanitize_component(&stem);
    let stem = if stem.is_empty() {
        book.md5.clone()
    } else {
        stem
    };
    Ok(cap_length(&stem, &extension))
}

/// Whether a trailing `.xyz` should be read as a file extension.
///
/// Requires at least one letter, so a version suffix like `Book v1.2` keeps its
/// `.2` instead of being reinterpreted as an extension and silently dropped.
fn looks_like_extension(ext: &str) -> bool {
    (1..=5).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric())
        && ext.chars().any(|c| c.is_ascii_alphabetic())
}

/// Neutralise a dangerous extension embedded at the end of a stem, so a title
/// of `payload.exe` becomes `payload-exe.epub` rather than `payload.exe.epub`.
fn defuse_double_extension(stem: &str) -> String {
    match stem.rsplit_once('.') {
        Some((head, tail))
            if DANGEROUS_EXTENSIONS.contains(&tail.to_ascii_lowercase().as_str())
                && !head.is_empty() =>
        {
            format!("{head}-{tail}")
        }
        _ => stem.to_string(),
    }
}

/// Map a claimed extension to one we are willing to write.
fn resolve_extension(ext: &str) -> Result<String> {
    let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    let ext: String = ext.chars().filter(|c| c.is_ascii_alphanumeric()).collect();

    if ext.is_empty() {
        return Ok("bin".to_string());
    }
    if DANGEROUS_EXTENSIONS.contains(&ext.as_str()) {
        bail!(
            "refusing to download a `.{ext}` file — that is an executable, not a book. \
             This record is either mislabelled or malicious."
        );
    }
    if ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        Ok(ext)
    } else {
        // Unknown but not obviously dangerous: keep it inert.
        Ok("bin".to_string())
    }
}

/// Strip anything that could escape the destination directory or confuse a
/// shell, then collapse the result into a tidy single line.
fn sanitize_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        match c {
            // Path separators and the Windows-illegal set.
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('-'),
            // Control characters, including newlines that could forge output.
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }

    // Collapse whitespace runs.
    let mut collapsed = String::with_capacity(out.len());
    let mut last_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !last_space {
                collapsed.push(' ');
            }
            last_space = true;
        } else {
            collapsed.push(c);
            last_space = false;
        }
    }

    // Leading dots hide the file; leading dashes look like flags to other tools.
    let trimmed = collapsed
        .trim()
        .trim_start_matches(['.', '-', ' '])
        .trim_end_matches(['.', ' '])
        .to_string();

    if trimmed.is_empty() {
        return String::new();
    }
    // `.` / `..` can never survive the trim above, but be explicit.
    if trimmed == "." || trimmed == ".." {
        return String::new();
    }
    if RESERVED_STEMS.contains(&trimmed.to_ascii_lowercase().as_str()) {
        return format!("_{trimmed}");
    }
    trimmed
}

/// Join stem and extension, truncating the stem so the whole name fits.
fn cap_length(stem: &str, extension: &str) -> String {
    let stem = defuse_double_extension(stem);
    let suffix_len = extension.len() + 1;
    let budget = MAX_FILENAME_BYTES.saturating_sub(suffix_len);

    let mut truncated = String::new();
    for c in stem.chars() {
        if truncated.len() + c.len_utf8() > budget {
            break;
        }
        truncated.push(c);
    }
    let truncated = truncated.trim_end_matches([' ', '.']).to_string();
    let stem = if truncated.is_empty() {
        "download".to_string()
    } else {
        truncated
    };
    format!("{stem}.{extension}")
}

/// Stream a file to disk, verifying it as it arrives.
pub type Report<'a> = &'a mut dyn FnMut(u64, Option<u64>);

/// Stream a file to disk, verifying it as it arrives.
///
/// Progress is reported to the caller rather than drawn here, so the same code
/// serves the CLI's inline bar and the TUI's per-row indicator.
pub fn fetch(
    http: &Http,
    url: &Uri,
    book: &Book,
    opts: &Options,
    report: Report<'_>,
) -> Result<Outcome> {
    // When the catalogue told us the format we can decide the filename, and so
    // whether the file already exists, before spending a request.
    if book.extension.is_some() {
        let name = build_filename(book, opts.filename.as_deref())?;
        let path = opts.dest_dir.join(&name);
        if path.exists() && !opts.force {
            return Ok(Outcome {
                path,
                bytes: 0,
                verified: false,
                skipped: true,
            });
        }
    }

    crate::config::ensure_dir(&opts.dest_dir)?;

    // Resume needs the filename up front, so it only applies when the format
    // was already known. Starting from zero is the safe fallback.
    let resume_from = match (&book.extension, opts.resume) {
        (Some(_), true) => {
            let name = build_filename(book, opts.filename.as_deref())?;
            std::fs::metadata(opts.dest_dir.join(format!("{name}.part")))
                .map(|m| m.len())
                .unwrap_or(0)
        }
        _ => 0,
    };

    let response = http.get_download(
        url,
        if resume_from > 0 {
            Some(resume_from)
        } else {
            None
        },
    )?;
    if !(200..300).contains(&response.status) {
        bail!("mirror returned HTTP {} for the file", response.status);
    }

    // A 200 in reply to a ranged request means the server ignored the range.
    // A 206 is only safe to append when it begins exactly where our partial
    // file ends; accepting any other offset would splice unrelated bytes.
    let resuming = if resume_from > 0 && response.status == 206 {
        let start = response
            .content_range
            .as_deref()
            .and_then(content_range_start)
            .ok_or_else(|| err!("mirror returned an invalid Content-Range for a resumed file"))?;
        if start != resume_from {
            bail!(
                "mirror resumed at byte {start}, but the partial file ends at byte {resume_from}"
            );
        }
        true
    } else {
        false
    };

    // For an MD5-only download the catalogue never told us the format, so fall
    // back to the server's filename hint — but only to read an extension off
    // it, and only one the allowlist accepts. The name itself is still ours.
    let record = match book.extension {
        Some(_) => book.clone(),
        None => Book {
            extension: extension_hint(response.content_disposition.as_deref()),
            ..book.clone()
        },
    };

    guard_content_type(response.content_type.as_deref(), record.ext())?;

    let filename = build_filename(&record, opts.filename.as_deref())?;
    let final_path = opts.dest_dir.join(&filename);

    // Belt and braces: the sanitiser should make this impossible, but confirm
    // the resolved name really is a direct child of the destination.
    if final_path.parent() != Some(opts.dest_dir.as_path()) {
        bail!("refusing to write outside {}", opts.dest_dir.display());
    }
    if final_path.exists() && !opts.force {
        return Ok(Outcome {
            path: final_path,
            bytes: 0,
            verified: false,
            skipped: true,
        });
    }
    let part_path = opts.dest_dir.join(format!("{filename}.part"));

    let already = if resuming { resume_from } else { 0 };
    if let Some(len) = response.content_length {
        let total = len.saturating_add(already);
        if total > opts.max_bytes {
            bail!(
                "file is {} which exceeds the {} limit (raise it with --max-size)",
                crate::model::human_bytes(total),
                crate::model::human_bytes(opts.max_bytes)
            );
        }
    }

    let mut hasher = Md5::new();
    let mut file = if resuming {
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&part_path)
            .with_context(|| format!("could not reopen {}", part_path.display()))?;
        // Fold the bytes we already have into the hash before appending.
        hash_existing(&mut f, &mut hasher, already)?;
        f.seek(SeekFrom::Start(already))
            .with_context(|| format!("could not seek in {}", part_path.display()))?;
        f
    } else {
        crate::config::create_private_file(&part_path)?
    };

    let total_expected = response.content_length.map(|l| l.saturating_add(already));
    let total_expected = total_expected.or(book.size_bytes);
    report(already, total_expected);

    let mut reader = response.body.into_reader();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut written = already;

    loop {
        let n = match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                let _ = file.flush();
                return Err(err!(
                    "transfer interrupted after {}: {e}",
                    crate::model::human_bytes(written)
                ));
            }
        };

        written += n as u64;
        // The advertised length can lie, so enforce the cap on real bytes too.
        if written > opts.max_bytes {
            drop(file);
            let _ = std::fs::remove_file(&part_path);
            bail!(
                "download exceeded the {} limit",
                crate::model::human_bytes(opts.max_bytes)
            );
        }

        hasher.update(&buffer[..n]);
        file.write_all(&buffer[..n])
            .with_context(|| format!("could not write to {}", part_path.display()))?;
        report(written, total_expected);
    }

    file.flush()
        .with_context(|| format!("could not flush {}", part_path.display()))?;
    drop(file);

    if written == 0 {
        let _ = std::fs::remove_file(&part_path);
        bail!("mirror sent an empty file");
    }

    // Verify before the file is allowed to take its real name.
    let digest = hasher.finalize_hex();
    let verified = digest.eq_ignore_ascii_case(&book.md5);
    if opts.verify && !book.md5.is_empty() && !verified {
        let _ = std::fs::remove_file(&part_path);
        bail!(
            "integrity check failed — expected MD5 {} but got {digest}.\n\
             The file was discarded. The mirror served something other than \
             the catalogued file.",
            book.md5
        );
    }

    if !install(&part_path, &final_path, opts.force)? {
        return Ok(Outcome {
            path: final_path,
            bytes: 0,
            verified: false,
            skipped: true,
        });
    }

    mark_quarantined(&final_path);

    Ok(Outcome {
        path: final_path,
        bytes: written,
        verified: verified && opts.verify,
        skipped: false,
    })
}

/// Parse the first byte offset from `Content-Range: bytes START-END/TOTAL`.
fn content_range_start(value: &str) -> Option<u64> {
    let (unit, range) = value.trim().split_once(' ')?;
    if !unit.eq_ignore_ascii_case("bytes") {
        return None;
    }
    let (bounds, _) = range.split_once('/')?;
    let (start, end) = bounds.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?;
    (start <= end).then_some(start)
}

/// Move a verified partial into place with the requested replacement policy.
fn install(part: &Path, final_path: &Path, force: bool) -> Result<bool> {
    if !force {
        // Creating the second name is atomic and fails if it already exists.
        // Both paths are siblings, so the hard link is on the same filesystem.
        match std::fs::hard_link(part, final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(part);
                return Ok(false);
            }
            // Not every filesystem has hard links — exFAT and FAT32 have none,
            // and several network mounts refuse them — and an external drive is
            // a perfectly ordinary place to keep a library. Fall back to an
            // exclusive create, which is atomic in the same way; it costs a copy
            // of the bytes, which only happens where the link was impossible.
            Err(error) if !links_supported(&error) => return install_by_copy(part, final_path),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "could not install {} at {} without replacing an existing file",
                        part.display(),
                        final_path.display()
                    )
                });
            }
        }
        std::fs::remove_file(part)
            .with_context(|| format!("could not remove {}", part.display()))?;
        return Ok(true);
    }

    replace(part, final_path).with_context(|| {
        format!(
            "could not replace {} with {}",
            final_path.display(),
            part.display()
        )
    })?;
    Ok(true)
}

/// Whether a failed `hard_link` means "this file cannot be linked" rather than
/// "this filesystem does not do links at all".
///
/// The two are told apart by errno: a filesystem without link support answers
/// `Unsupported` or `PermissionDenied` depending on the platform, where a real
/// problem — a full disk, a vanished source — reports itself specifically.
fn links_supported(error: &std::io::Error) -> bool {
    !matches!(
        error.kind(),
        std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied
    )
}

/// Install by copying into an exclusively-created file.
///
/// `create_new` fails if the name is taken, which is the same guarantee the
/// hard link gave: an existing file is never replaced without `--force`.
fn install_by_copy(part: &Path, final_path: &Path) -> Result<bool> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        // The hard-link path gives the final name the partial file's own mode,
        // so match it rather than letting a copied book be more visible than a
        // linked one.
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut destination = match options.open(final_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(part);
            return Ok(false);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("could not create {}", final_path.display()));
        }
    };

    let mut source = std::fs::File::open(part)
        .with_context(|| format!("could not reopen {}", part.display()))?;
    if let Err(error) = std::io::copy(&mut source, &mut destination)
        .and_then(|_| destination.flush())
        .with_context(|| format!("could not write {}", final_path.display()))
    {
        // A half-written file under the real name is worse than no file.
        drop(destination);
        let _ = std::fs::remove_file(final_path);
        return Err(error);
    }
    drop(destination);
    drop(source);

    std::fs::remove_file(part).with_context(|| format!("could not remove {}", part.display()))?;
    Ok(true)
}

#[cfg(not(windows))]
fn replace(part: &Path, final_path: &Path) -> std::io::Result<()> {
    std::fs::rename(part, final_path)
}

#[cfg(windows)]
fn replace(part: &Path, final_path: &Path) -> std::io::Result<()> {
    match std::fs::rename(part, final_path) {
        Ok(()) => return Ok(()),
        Err(rename_error) if !final_path.exists() => return Err(rename_error),
        Err(_) => {}
    }

    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let replaced: Vec<u16> = final_path.as_os_str().encode_wide().chain([0]).collect();
    let replacement: Vec<u16> = part.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: both strings are NUL-terminated and live for the duration of the
    // call. No backup or metadata-exclusion buffers are supplied.
    let ok = unsafe {
        ReplaceFileW(
            replaced.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Read a usable extension out of a `Content-Disposition` header.
///
/// This header is chosen by the mirror, so nothing but the extension is taken
/// from it, and even that has to survive the allowlist. A header offering
/// `.exe` yields `None`, which lands the download on the inert `.bin`.
fn extension_hint(disposition: Option<&str>) -> Option<String> {
    let raw = disposition?;
    // `filename*=UTF-8''name.epub` and `filename="name.epub"` both appear.
    let after = raw
        .split("filename*=")
        .nth(1)
        .or_else(|| raw.split("filename=").nth(1))?;
    let value = after
        .trim()
        .trim_start_matches("UTF-8''")
        .trim_matches(['"', '\'', ';', ' ']);
    let ext = value.rsplit('.').next()?;
    // Trim any trailing header cruft before judging it.
    let ext: String = ext
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if !looks_like_extension(&ext) {
        return None;
    }
    match resolve_extension(&ext) {
        Ok(resolved) if resolved != "bin" => Some(resolved),
        _ => None,
    }
}

/// Reject responses that are obviously not the file we asked for.
fn guard_content_type(content_type: Option<&str>, expected_ext: &str) -> Result<()> {
    let Some(ct) = content_type else {
        return Ok(());
    };
    let ct = ct.to_ascii_lowercase();
    let is_html = ct.contains("text/html") || ct.contains("application/xhtml");
    let wants_html = matches!(expected_ext, "htm" | "html");

    if is_html && !wants_html {
        bail!(
            "mirror returned a web page instead of a file — it is probably \
             rate-limiting us or showing a captcha. Try again, or use \
             `--mirror` to pick a different one."
        );
    }
    Ok(())
}

/// Hash the bytes already on disk so a resumed download still produces the
/// digest of the whole file.
fn hash_existing(file: &mut std::fs::File, hasher: &mut Md5, len: u64) -> Result<()> {
    file.seek(SeekFrom::Start(0))
        .context("could not rewind the partial file")?;
    let mut remaining = len;
    let mut buffer = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = buffer.len().min(remaining as usize);
        let n = file
            .read(&mut buffer[..want])
            .context("could not read the partial file")?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        remaining -= n as u64;
    }
    Ok(())
}

/// Tag the file the way a browser would, so Gatekeeper still evaluates it.
///
/// Best-effort: a filesystem without extended attribute support is not a
/// reason to fail a download that already passed its integrity check.
#[cfg(target_os = "macos")]
fn mark_quarantined(path: &Path) {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // flags;hex timestamp;agent name;event uuid
    let value = format!("0081;{stamp:x};tomesole;");
    let _ = xattr::set(path, "com.apple.quarantine", value.as_bytes());
}

#[cfg(not(target_os = "macos"))]
fn mark_quarantined(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn book() -> Book {
        Book {
            md5: "1b9159991f7fb1b3910c0be9ebf7e595".into(),
            title: "The Rust Programming Language".into(),
            authors: Some("Klabnik, Steve;Nichols, Carol".into()),
            year: Some("2019".into()),
            extension: Some("epub".into()),
            ..Default::default()
        }
    }

    #[test]
    fn builds_a_readable_filename() {
        let name = build_filename(&book(), None).unwrap();
        assert_eq!(
            name,
            "Klabnik, Steve - The Rust Programming Language (2019).epub"
        );
    }

    #[test]
    fn path_separators_never_survive_sanitising() {
        let mut b = book();
        b.title = "../../etc/passwd".into();
        b.authors = None;
        b.year = None;
        let name = build_filename(&b, None).unwrap();
        assert!(!name.contains('/'), "got {name}");
        assert!(!name.contains(".."), "got {name}");
        assert!(!name.starts_with('.'), "got {name}");
    }

    #[test]
    fn absolute_paths_and_backslashes_are_neutralised() {
        for hostile in [
            "/etc/shadow",
            "C:\\Windows\\System32\\evil",
            "..\\..\\secret",
            "~/.ssh/authorized_keys",
        ] {
            let mut b = book();
            b.title = hostile.into();
            b.authors = None;
            b.year = None;
            let name = build_filename(&b, None).unwrap();
            assert!(!name.contains('/') && !name.contains('\\'), "got {name}");
            assert!(Path::new(&name).components().count() == 1, "got {name}");
        }
    }

    #[test]
    fn control_characters_and_newlines_are_stripped() {
        let mut b = book();
        b.title = "evil\n\rTitle\u{0}with\tcontrol".into();
        b.authors = None;
        let name = build_filename(&b, None).unwrap();
        assert!(!name.contains('\n') && !name.contains('\r') && !name.contains('\0'));
    }

    #[test]
    fn executable_extensions_are_refused() {
        for ext in ["exe", "dmg", "sh", "app", "jar", "ps1", "EXE"] {
            let mut b = book();
            b.extension = Some(ext.into());
            let result = build_filename(&b, None);
            assert!(result.is_err(), "`.{ext}` should be refused");
        }
    }

    #[test]
    fn unknown_extensions_become_inert_bin() {
        let mut b = book();
        b.extension = Some("weirdformat".into());
        assert!(build_filename(&b, None).unwrap().ends_with(".bin"));

        b.extension = None;
        assert!(build_filename(&b, None).unwrap().ends_with(".bin"));
    }

    #[test]
    fn extension_tricks_do_not_slip_through() {
        let mut b = book();
        // A crafted extension must not smuggle in a path. Non-alphanumerics are
        // stripped, so this lands on the inert `.bin` rather than a path.
        b.extension = Some("../../x.exe".into());
        let name = build_filename(&b, None).unwrap();
        assert!(!name.contains('/') && !name.contains(".."), "got {name}");
        assert!(name.ends_with(".bin"), "got {name}");

        // Punctuation and spaces are stripped before the allowlist is consulted,
        // which normalises sloppy records and, more importantly, means an
        // obfuscated `e.x.e` still gets caught as an executable.
        b.extension = Some("pd f".into());
        assert!(build_filename(&b, None).unwrap().ends_with(".pdf"));

        b.extension = Some("e.x.e".into());
        assert!(
            build_filename(&b, None).is_err(),
            "obfuscated exe must be refused"
        );
    }

    #[test]
    fn dangerous_double_extensions_are_defused() {
        let mut b = book();
        b.title = "invoice.exe".into();
        b.authors = None;
        b.year = None;
        let name = build_filename(&b, None).unwrap();
        assert_eq!(name, "invoice-exe.epub");
        assert!(!name.contains(".exe"), "got {name}");
    }

    #[test]
    fn version_suffixes_are_not_mistaken_for_extensions() {
        // `.2` has no letter, so it is part of the name, not an extension.
        let name = build_filename(&book(), Some("Handbook v1.2")).unwrap();
        assert_eq!(name, "Handbook v1.2.epub");
    }

    #[test]
    fn known_good_extensions_are_kept() {
        for ext in ["pdf", "epub", "djvu", "cbz", "PDF"] {
            let mut b = book();
            b.extension = Some(ext.into());
            let name = build_filename(&b, None).unwrap();
            assert!(
                name.to_lowercase()
                    .ends_with(&format!(".{}", ext.to_lowercase())),
                "got {name}"
            );
        }
    }

    #[test]
    fn user_supplied_names_are_sanitised_too() {
        let name = build_filename(&book(), Some("../../evil")).unwrap();
        assert!(!name.contains('/') && !name.contains(".."), "got {name}");

        // A user extension is honoured when acceptable.
        let name = build_filename(&book(), Some("mybook.pdf")).unwrap();
        assert_eq!(name, "mybook.pdf");

        // ...and refused when not.
        assert!(build_filename(&book(), Some("mybook.exe")).is_err());
    }

    #[test]
    fn windows_reserved_names_are_escaped() {
        let mut b = book();
        b.title = "CON".into();
        b.authors = None;
        b.year = None;
        assert_eq!(build_filename(&b, None).unwrap(), "_CON.epub");
    }

    #[test]
    fn very_long_titles_are_capped() {
        let mut b = book();
        b.title = "x".repeat(500);
        b.authors = None;
        b.year = None;
        let name = build_filename(&b, None).unwrap();
        assert!(name.len() <= MAX_FILENAME_BYTES, "got {} bytes", name.len());
        assert!(name.ends_with(".epub"));
    }

    #[test]
    fn multibyte_titles_are_capped_on_char_boundaries() {
        let mut b = book();
        b.title = "日本語".repeat(200);
        b.authors = None;
        b.year = None;
        let name = build_filename(&b, None).unwrap();
        assert!(name.len() <= MAX_FILENAME_BYTES);
        // Would have panicked during construction if a boundary was split.
        assert!(name.ends_with(".epub"));
    }

    #[test]
    fn empty_metadata_falls_back_to_the_md5() {
        let mut b = book();
        b.title = "   ".into();
        b.authors = None;
        b.year = None;
        let name = build_filename(&b, None).unwrap();
        assert!(name.starts_with("1b9159991f"), "got {name}");
    }

    // --- Streaming path, exercised against a throwaway local server. ---
    //
    // The integrity check is the central safety claim of this program, so it is
    // tested end to end rather than by unit-testing the hasher alone.

    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::time::Duration;

    /// Serve one canned response on a loopback port, then exit.
    fn serve_once(content_type: &str, body: Vec<u8>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let content_type = content_type.to_string();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            // Consume the request headers so the client is not left blocked.
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&body);
            let _ = stream.flush();
        });
        port
    }

    /// Loopback is normally blocked, so these tests opt in explicitly.
    fn loopback_http() -> Http {
        Http::new(crate::net::NetPolicy {
            allow_http: true,
            allow_private_hosts: true,
            ..Default::default()
        })
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tomesole-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn options(dir: &Path) -> Options {
        Options {
            dest_dir: dir.to_path_buf(),
            filename: None,
            max_bytes: 10 * 1024 * 1024,
            verify: true,
            force: false,
            resume: false,
        }
    }

    #[test]
    fn a_matching_file_is_saved_and_marked_verified() {
        let body = b"pretend this is an epub".to_vec();
        let digest = crate::md5::hex_digest(&body);
        let port = serve_once("application/octet-stream", body.clone());

        let dir = temp_dir("ok");
        let mut b = book();
        b.md5 = digest;
        let url = format!("http://127.0.0.1:{port}/f").parse().unwrap();

        let outcome = fetch(&loopback_http(), &url, &b, &options(&dir), &mut |_, _| {}).unwrap();
        assert!(outcome.verified);
        assert_eq!(outcome.bytes, body.len() as u64);
        assert_eq!(std::fs::read(&outcome.path).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tampered_file_is_rejected_and_leaves_nothing_behind() {
        // The server returns content that does not match the catalogued MD5.
        let port = serve_once("application/octet-stream", b"substituted payload".to_vec());
        let dir = temp_dir("tamper");
        let b = book(); // md5 refers to entirely different content
        let url = format!("http://127.0.0.1:{port}/f").parse().unwrap();

        let err = fetch(&loopback_http(), &url, &b, &options(&dir), &mut |_, _| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("integrity check failed"), "got: {err}");

        // Neither the final file nor the partial may survive a failed check.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_captcha_page_is_not_saved_as_a_book() {
        let port = serve_once("text/html; charset=utf-8", b"<html>captcha</html>".to_vec());
        let dir = temp_dir("html");
        let url = format!("http://127.0.0.1:{port}/f").parse().unwrap();

        let err = fetch(
            &loopback_http(),
            &url,
            &book(),
            &options(&dir),
            &mut |_, _| {},
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("web page instead of a file"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_downloads_are_refused() {
        let port = serve_once("application/octet-stream", vec![b'x'; 4096]);
        let dir = temp_dir("toobig");
        let url = format!("http://127.0.0.1:{port}/f").parse().unwrap();
        let mut opts = options(&dir);
        opts.max_bytes = 1024;

        let err = fetch(&loopback_http(), &url, &book(), &opts, &mut |_, _| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds"), "got: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verification_can_be_disabled_but_is_reported_as_unverified() {
        let port = serve_once("application/octet-stream", b"whatever".to_vec());
        let dir = temp_dir("noverify");
        let url = format!("http://127.0.0.1:{port}/f").parse().unwrap();
        let mut opts = options(&dir);
        opts.verify = false;

        let outcome = fetch(&loopback_http(), &url, &book(), &opts, &mut |_, _| {}).unwrap();
        assert!(!outcome.verified, "mismatch must never report as verified");
        assert!(outcome.path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn html_responses_are_rejected_unless_expected() {
        assert!(guard_content_type(Some("text/html; charset=UTF-8"), "epub").is_err());
        assert!(guard_content_type(Some("application/octet-stream"), "epub").is_ok());
        assert!(guard_content_type(None, "epub").is_ok());
        // An actual HTML record is allowed to be HTML.
        assert!(guard_content_type(Some("text/html"), "html").is_ok());
    }

    #[test]
    fn content_range_must_name_a_valid_start() {
        assert_eq!(content_range_start("bytes 42-99/100"), Some(42));
        assert_eq!(content_range_start("BYTES 0-9/*"), Some(0));
        assert_eq!(content_range_start("bytes 99-42/100"), None);
        assert_eq!(content_range_start("items 42-99/100"), None);
        assert_eq!(content_range_start("bytes */100"), None);
    }

    #[test]
    fn a_resume_with_the_wrong_range_is_rejected_before_append() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\n\
                      Content-Type: application/octet-stream\r\n\
                      Content-Range: bytes 2-4/5\r\nContent-Length: 3\r\n\
                      Connection: close\r\n\r\nnew",
                )
                .unwrap();
        });

        let dir = temp_dir("wrong-range");
        let b = book();
        let name = build_filename(&b, None).unwrap();
        let part = dir.join(format!("{name}.part"));
        std::fs::write(&part, b"old").unwrap();
        let mut opts = options(&dir);
        opts.resume = true;
        opts.verify = false;
        let url = format!("http://127.0.0.1:{port}/f").parse().unwrap();

        let error = fetch(&loopback_http(), &url, &b, &opts, &mut |_, _| {})
            .unwrap_err()
            .to_string();
        assert!(error.contains("resumed at byte 2"), "{error}");
        assert_eq!(std::fs::read(&part).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_replace_install_never_clobbers_an_existing_file() {
        let dir = temp_dir("install-no-replace");
        let part = dir.join("book.epub.part");
        let final_path = dir.join("book.epub");
        std::fs::write(&part, b"new").unwrap();
        std::fs::write(&final_path, b"existing").unwrap();

        assert!(!install(&part, &final_path, false).unwrap());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing");
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// exFAT, FAT32 and several network mounts have no hard links, and an
    /// external drive is an ordinary place to keep a library. The copy fallback
    /// has to make the same promise the link did: the bytes land, the partial
    /// goes, and an existing file is never replaced.
    #[test]
    fn the_copy_fallback_installs_without_a_hard_link() {
        let dir = temp_dir("install-copy");
        let part = dir.join("book.epub.part");
        let final_path = dir.join("book.epub");
        std::fs::write(&part, b"the whole book").unwrap();

        assert!(install_by_copy(&part, &final_path).unwrap());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"the whole book");
        assert!(!part.exists(), "the partial should be gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_copy_fallback_never_clobbers_an_existing_file() {
        let dir = temp_dir("install-copy-exists");
        let part = dir.join("book.epub.part");
        let final_path = dir.join("book.epub");
        std::fs::write(&part, b"new").unwrap();
        std::fs::write(&final_path, b"existing").unwrap();

        assert!(!install_by_copy(&part, &final_path).unwrap());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"existing");
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A copied book must be no more visible than a linked one, which inherits
    /// the partial file's owner-only mode.
    #[cfg(unix)]
    #[test]
    fn the_copy_fallback_keeps_the_file_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("install-copy-mode");
        let part = dir.join("book.epub.part");
        let final_path = dir.join("book.epub");
        std::fs::write(&part, b"private").unwrap();

        install_by_copy(&part, &final_path).unwrap();
        let mode = std::fs::metadata(&final_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Only "this filesystem cannot do links" may fall through to the copy. A
    /// full disk or a vanished partial has to stay an error.
    #[test]
    fn only_a_missing_link_facility_falls_back_to_copying() {
        use std::io::{Error, ErrorKind};
        for kind in [ErrorKind::Unsupported, ErrorKind::PermissionDenied] {
            assert!(!links_supported(&Error::from(kind)), "{kind:?}");
        }
        for kind in [
            ErrorKind::NotFound,
            ErrorKind::StorageFull,
            ErrorKind::AlreadyExists,
        ] {
            assert!(links_supported(&Error::from(kind)), "{kind:?}");
        }
    }

    #[test]
    fn forced_install_replaces_an_existing_file() {
        let dir = temp_dir("install-force");
        let part = dir.join("book.epub.part");
        let final_path = dir.join("book.epub");
        std::fs::write(&part, b"new").unwrap();
        std::fs::write(&final_path, b"existing").unwrap();

        assert!(install(&part, &final_path, true).unwrap());
        assert_eq!(std::fs::read(&final_path).unwrap(), b"new");
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_slow_stream_is_not_bound_by_the_page_deadline() {
        let body = b"slow but healthy".to_vec();
        let digest = crate::md5::hex_digest(&body);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = body.clone();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                line.clear();
            }
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                served.len()
            )
            .unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(100));
            stream.write_all(&served).unwrap();
        });

        let http = Http::new(crate::net::NetPolicy {
            allow_http: true,
            allow_private_hosts: true,
            request_timeout: Duration::from_millis(30),
            ..Default::default()
        });
        let dir = temp_dir("slow-stream");
        let mut b = book();
        b.md5 = digest;
        let url = format!("http://127.0.0.1:{port}/f").parse().unwrap();
        let outcome = fetch(&http, &url, &b, &options(&dir), &mut |_, _| {}).unwrap();
        assert_eq!(std::fs::read(outcome.path).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
