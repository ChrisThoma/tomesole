//! Handing a downloaded file to the rest of the desktop.
//!
//! Two things happen here: opening a book in a reader, and showing it in a file
//! manager. Both mean starting another program, so:
//!
//! * **No shell is involved.** The path is passed as a single argument to
//!   `Command`, never interpolated into a command line, so a filename cannot
//!   turn into an argument or a command of its own. On Windows that means
//!   calling `ShellExecuteW` directly rather than going through `cmd /C start`:
//!   `cmd` is a shell, and it splits an unquoted argument on `&`, which is a
//!   character titles are allowed to contain.
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
            //
            // Split on whitespace so `mupdf -r 120` works, since the config
            // calls this a command. Still no shell: the parts become argv
            // entries directly, and the path is always the last one.
            //
            // The match arm already trimmed this and rejected the empty string,
            // so there is always a first word to take.
            let mut parts = command.split_whitespace();
            let program = parts.next().unwrap_or(command);
            detach(Command::new(program).args(parts).arg(&path)).with_context(|| {
                format!("could not start `{program}` — is it installed and on your PATH?")
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
        // not worth consulting. It also does not understand the `\\?\` prefix
        // that canonicalising adds.
        detach(Command::new("explorer").arg(format!("/select,{}", display_path(&path).display())))
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
        shell_execute(path)
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

/// Hand a path to the shell's default handler for its type.
///
/// This is what a double-click does, and it is the reason nothing here goes
/// near `cmd /C start`: `ShellExecuteW` takes the path as one opaque argument,
/// so a book called `Notes & Queries.epub` opens instead of being split at the
/// ampersand and run as two commands.
#[cfg(target_os = "windows")]
fn shell_execute(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let file: Vec<u16> = display_path(path)
        .as_os_str()
        .encode_wide()
        .chain([0])
        .collect();
    let operation: Vec<u16> = "open".encode_utf16().chain([0]).collect();

    // SAFETY: both strings are NUL-terminated and live for the duration of the
    // call. A null window handle means "no parent", and the remaining pointers
    // are the documented nulls for "no arguments" and "no working directory".
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };

    // The return is typed as a handle for historical reasons but is really a
    // status: anything above 32 is success, and anything at or below it is one
    // of the legacy error codes, which overlap the ordinary system ones.
    let status = result as isize;
    if status > 32 {
        return Ok(());
    }
    Err(std::io::Error::from_raw_os_error(status as i32)).with_context(|| {
        format!(
            "could not open {} — no application is associated with this file type",
            path.display()
        )
    })
}

/// Strip the `\\?\` prefix that canonicalising adds on Windows.
///
/// It is the extended-length form, which the file APIs understand but the shell
/// and Explorer do not: handed one, Explorer silently opens the user's
/// documents folder instead of selecting the file.
#[cfg(target_os = "windows")]
fn display_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        // The UNC form has to keep a leading separator to stay a UNC path.
        Some(rest) => match rest.strip_prefix("UNC\\") {
            Some(share) => PathBuf::from(format!(r"\\{share}")),
            None => PathBuf::from(rest),
        },
        None => path.to_path_buf(),
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
        let file = dir.join(format!("tomesole-launch-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();

        let resolved = resolve(&file).unwrap();
        assert!(resolved.is_absolute());
        // An absolute path can never be mistaken for a flag by the program we
        // hand it to, which is the point of canonicalising.
        assert!(resolved.to_string_lossy().starts_with('/') || cfg!(windows));
        let _ = std::fs::remove_file(&file);
    }

    /// The config calls `reader` a command, so one with arguments has to work.
    /// Splitting happens here, into argv entries — there is still no shell, so
    /// the filename can never become one of them.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_reader_may_carry_arguments() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("tomesole-launch-a-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();

        // `true` ignores its arguments and exits zero, so this proves the
        // program name was taken from the first word rather than the whole
        // string being looked up as one.
        assert!(open(&file, Some("true --page 3")).is_ok());
        // The whole string as a program name is what used to happen, and there
        // is no such executable.
        assert!(open(&file, Some("tomesole-nonesuch --page 3")).is_err());
        let _ = std::fs::remove_file(&file);
    }

    /// A reader that does not exist must fail with a sentence, not a panic.
    #[test]
    fn an_unknown_reader_fails_gracefully() {
        let dir = std::env::temp_dir();
        let file = dir.join(format!("tomesole-launch-r-{}.txt", std::process::id()));
        std::fs::write(&file, b"x").unwrap();

        let result = open(&file, Some("tomesole-no-such-application-9c1f"));
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
