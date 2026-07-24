//! Handing a downloaded file to the rest of the desktop.
//!
//! Two things happen here: opening a book in a reader, and showing it in a file
//! manager. Both mean starting another program, so:
//!
//! * **No shell is involved.** The path is passed as a single argument to
//!   `Command`, never interpolated into a command line, so a filename cannot
//!   turn into an argument or a command of its own.
//! * **The path is canonicalised first**, which both proves the file is still
//!   there and guarantees it is absolute — so it cannot start with `-` and be
//!   read as a flag by whatever we hand it to.
//! * **The child gets no terminal.** Output is discarded rather than inherited,
//!   because a reader that logs to stderr would otherwise scribble over the
//!   full-screen interface.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Context, Result};
use crate::{bail, err};

/// Open a file with `reader`, or with the system default when none is set.
///
/// On macOS `reader` names an application, because that is what `open -a`
/// takes: `Books`, `Preview`, `/Applications/Skim.app`. Elsewhere it is a
/// command, run with the file as its only argument: `zathura`, `mupdf`.
pub fn open(path: &Path, reader: Option<&str>) -> Result<()> {
    let path = resolve(path)?;

    match reader.map(str::trim).filter(|r| !r.is_empty()) {
        #[cfg(target_os = "macos")]
        Some(app) => run(Command::new("open").arg("-a").arg(app).arg(&path)),
        #[cfg(not(target_os = "macos"))]
        Some(command) => {
            // A reader like `zathura` stays in the foreground for as long as
            // the book is open, so this one is started and let go of.
            detach(Command::new(command).arg(&path)).with_context(|| {
                format!("could not start `{command}` — is it installed and on your PATH?")
            })
        }
        None => open_with_default(&path),
    }
}

/// Show a file in the system file manager, selected where that is possible.
pub fn reveal(path: &Path) -> Result<()> {
    let path = resolve(path)?;

    #[cfg(target_os = "macos")]
    {
        run(Command::new("open").arg("-R").arg(&path))
    }

    #[cfg(target_os = "windows")]
    {
        // Explorer reports failure even when it worked, so its exit status is
        // not worth consulting.
        detach(Command::new("explorer").arg(format!("/select,{}", path.display())))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // The freedesktop interface is the only one that can highlight the
        // file rather than merely opening the folder it sits in, so it is
        // worth trying before falling back.
        let shown = run(Command::new("dbus-send")
            .arg("--session")
            .arg("--dest=org.freedesktop.FileManager1")
            .arg("--type=method_call")
            .arg("/org/freedesktop/FileManager1")
            .arg("org.freedesktop.FileManager1.ShowItems")
            .arg(format!("array:string:{}", file_uri(&path)))
            .arg("string:"))
        .is_ok();
        if shown {
            return Ok(());
        }
        let folder = path.parent().unwrap_or(Path::new("."));
        open_with_default(folder)
    }
}

/// Hand something to whatever the desktop thinks should handle it.
fn open_with_default(path: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        run(Command::new("open").arg(path))
    }

    #[cfg(target_os = "windows")]
    {
        // The empty string is `start`'s window-title argument; without it a
        // quoted path would be taken for the title.
        detach(Command::new("cmd").arg("/C").arg("start").arg("").arg(path))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if run(Command::new("xdg-open").arg(path)).is_ok() {
            return Ok(());
        }
        run(Command::new("gio").arg("open").arg(path)).map_err(|_| {
            err!(
                "could not open {} — neither `xdg-open` nor `gio` worked. \
                 Set `reader` in the config to name a program directly.",
                path.display()
            )
        })
    }
}

/// Prove the file is still there, and make the path absolute while we are at it.
fn resolve(path: &Path) -> Result<PathBuf> {
    let resolved = std::fs::canonicalize(path).map_err(|_| {
        err!(
            "{} is no longer there — it was moved or deleted since it was downloaded",
            path.display()
        )
    })?;
    if !resolved.is_file() && !resolved.is_dir() {
        bail!("{} is not a file", resolved.display());
    }
    Ok(resolved)
}

/// Run a launcher to completion and report what it said if it failed.
///
/// Launchers (`open`, `xdg-open`, `dbus-send`) return as soon as they have
/// handed the file over, so waiting costs nothing and buys a real error
/// message when the named application does not exist.
#[allow(dead_code)]
fn run(command: &mut Command) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| {
            format!(
                "could not run `{}`",
                command.get_program().to_string_lossy()
            )
        })?;

    if output.status.success() {
        return Ok(());
    }
    let message = String::from_utf8_lossy(&output.stderr);
    let message = message.lines().next().unwrap_or("").trim();
    if message.is_empty() {
        bail!("`{}` failed", command.get_program().to_string_lossy());
    }
    bail!("{message}")
}

/// Start a program and stop caring about it.
#[allow(dead_code)]
fn detach(command: &mut Command) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .with_context(|| {
            format!(
                "could not start `{}`",
                command.get_program().to_string_lossy()
            )
        })
}

/// Percent-encode a path as a `file://` URI, for the freedesktop call.
#[cfg(all(unix, not(target_os = "macos")))]
fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.to_string_lossy().bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_is_reported_clearly() {
        let err = open(Path::new("/definitely/not/here.epub"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no longer there"), "got: {err}");

        let err = reveal(Path::new("/definitely/not/here.epub"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no longer there"), "got: {err}");
    }

    #[test]
    fn resolving_makes_the_path_absolute() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("clibgen-launch-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();

        let resolved = resolve(&file).unwrap();
        assert!(resolved.is_absolute());
        // An absolute path can never be mistaken for a flag by the program we
        // hand it to, which is the point of canonicalising.
        assert!(resolved.to_string_lossy().starts_with('/') || cfg!(windows));
        let _ = std::fs::remove_file(&file);
    }

    /// A reader that does not exist must fail with a sentence, not a panic.
    #[test]
    fn an_unknown_reader_fails_gracefully() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("clibgen-launch-r-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();

        let result = open(&file, Some("clibgen-no-such-application-9c1f"));
        assert!(result.is_err(), "a bogus reader should not report success");
        let _ = std::fs::remove_file(&file);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn file_uris_escape_what_they_must() {
        assert_eq!(
            file_uri(Path::new("/books/A Book & Co.epub")),
            "file:///books/A%20Book%20%26%20Co.epub"
        );
    }
}
