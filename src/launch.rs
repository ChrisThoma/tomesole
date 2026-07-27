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
/// takes: `Books`, `Preview`, `/Applications/Skim.app` — one argument, never
/// split. Elsewhere it is a command, run with the file as its last argument:
/// `zathura`, `mupdf -r 120`. See [`split_reader`] for how that is taken apart.
pub fn open(path: &Path, reader: Option<&str>) -> Result<()> {
    let path = resolve(path)?;

    match reader.map(str::trim).filter(|r| !r.is_empty()) {
        #[cfg(target_os = "macos")]
        Some(app) => run(Command::new("open").arg("-a").arg(app).arg(&path)),
        #[cfg(not(target_os = "macos"))]
        Some(command) => {
            // A reader like `zathura` stays in the foreground for as long as
            // the book is open, so this one is started and let go of.
            let (program, args) = split_reader(command);
            detach(Command::new(&program).args(args).arg(&path)).with_context(|| {
                format!("could not start `{program}` — is it installed and on your PATH?")
            })
        }
        None => open_with_default(&path),
    }
}

/// Take a configured reader apart into a program and its arguments.
///
/// The config calls this a command, so `mupdf -r 120` has to work, and the
/// obvious whitespace split is what does it. But a program can also live at a
/// path with a space in it — `C:\Program Files\SumatraPDF\SumatraPDF.exe` is
/// the ordinary case on Windows, not a corner one — and no split survives that.
/// So two things rescue it:
///
/// * A double-quoted word is kept whole, wherever it appears. That is the way
///   to say `"/opt/Foxit Reader/FoxitReader" --page 3`.
/// * Failing that, a string that names a file which is actually there is taken
///   whole, unsplit. That covers the config someone wrote when a reader could
///   only ever be one program, before there was anything to quote.
///
/// There is still no shell in the chain: these become argv entries directly,
/// and the file being opened is always appended last by the caller.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn split_reader(command: &str) -> (String, Vec<String>) {
    if !command.contains('"') && looks_like_path(command) && Path::new(command).is_file() {
        return (command.to_string(), Vec::new());
    }

    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in command.chars() {
        match c {
            // An unterminated quote simply runs to the end of the line. There
            // is nothing useful to say about it that starting the program and
            // letting it fail does not say better.
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }

    let mut words = words.into_iter();
    // A command of nothing but quotes leaves no first word. Hand the string
    // back as it was written so the failure names what the config says.
    let program = words.next().unwrap_or_else(|| command.to_string());
    (program, words.collect())
}

/// Whether a string is shaped like a path rather than a bare program name.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn looks_like_path(s: &str) -> bool {
    s.contains('/') || (cfg!(windows) && s.contains('\\'))
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
        open_with_default(folder).map_err(|_| err!("{}", cannot_show_message(&path, is_wsl())))
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
        run(Command::new("gio").arg("open").arg(path))
            .map_err(|_| err!("{}", no_handler_message(path, is_wsl())))
    }
}

/// What to say when nothing on this system will open a book.
///
/// Two things matter here. It has to fit on one line in the full-screen
/// interface, so it names the file rather than its path and holds that name to
/// a length a book title cannot blow past. And it has to end with something to
/// do, since knowing that nothing is installed does not get a book open.
#[allow(dead_code)]
fn no_handler_message(path: &Path, wsl: bool) -> String {
    let name = short_name(path);
    if wsl {
        // WSL has no desktop of its own; opening a book means handing it back
        // to Windows, which is exactly what `wslview` is for.
        format!(
            "could not open {name} — WSL has no desktop to hand it to. \
             Run `sudo apt install wslu`, then put `reader = wslview` in the config"
        )
    } else {
        format!(
            "could not open {name} — neither `xdg-open` nor `gio` worked. \
             Install `xdg-utils`, or name a reader in the config, e.g. `reader = zathura`"
        )
    }
}

/// The same, for showing a file rather than opening one.
///
/// `reader` has no bearing on this path, so repeating that advice here would
/// send someone to edit a setting that cannot help.
#[allow(dead_code)]
fn cannot_show_message(path: &Path, wsl: bool) -> String {
    let name = short_name(path);
    if wsl {
        format!(
            "could not show {name} — WSL has no file manager. \
             Run `explorer.exe .` in that folder to see it from Windows"
        )
    } else {
        format!(
            "could not show {name} — neither the freedesktop call nor `xdg-open` worked. \
             Install `xdg-utils`, or open the folder yourself"
        )
    }
}

/// A file's name, short enough to leave room for the rest of a message.
#[allow(dead_code)]
fn short_name(path: &Path) -> String {
    let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
    crate::term::truncate(&name, 40)
}

/// Whether this is the Linux side of WSL.
///
/// `WSL_DISTRO_NAME` is set by the interop layer; the kernel release string is
/// the fallback for a shell that started without it.
#[allow(dead_code)]
fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
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

    #[test]
    fn a_reader_splits_into_a_program_and_its_options() {
        let (program, args) = split_reader("mupdf -r 120");
        assert_eq!(program, "mupdf");
        assert_eq!(args, ["-r", "120"]);

        let (program, args) = split_reader("zathura");
        assert_eq!(program, "zathura");
        assert!(args.is_empty());
    }

    /// The whole point of the quoting: a program whose path contains a space,
    /// which is the ordinary shape of one on Windows.
    #[test]
    fn a_quoted_program_keeps_the_spaces_in_its_path() {
        let (program, args) = split_reader(r#""/opt/Foxit Reader/FoxitReader" --page 3"#);
        assert_eq!(program, "/opt/Foxit Reader/FoxitReader");
        assert_eq!(args, ["--page", "3"]);

        // Quoting works on an argument too, rather than being a rule about the
        // first word only.
        let (program, args) = split_reader(r#"mupdf --title "A Book""#);
        assert_eq!(program, "mupdf");
        assert_eq!(args, ["--title", "A Book"]);
    }

    /// A config written before a reader could carry arguments named a program
    /// and nothing else. Splitting one of those on whitespace would break a
    /// setup that had been working, so a path that is really there is left
    /// alone.
    #[test]
    fn an_existing_path_with_a_space_is_not_split() {
        let dir = std::env::temp_dir().join(format!("tomesole launch {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("my reader");
        std::fs::write(&program, b"#!/bin/sh\n").unwrap();

        let text = program.to_string_lossy().to_string();
        let (parsed, args) = split_reader(&text);
        assert_eq!(parsed, text);
        assert!(args.is_empty(), "an existing path is the whole command");

        // The same path with an option after it is no longer a file that
        // exists, so it splits — and quoting is how to say what was meant.
        let (parsed, _) = split_reader(&format!("{text} --page 3"));
        assert_ne!(parsed, text);
        let (parsed, args) = split_reader(&format!("\"{text}\" --page 3"));
        assert_eq!(parsed, text);
        assert_eq!(args, ["--page", "3"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bare word is never treated as a path, so a file in the working
    /// directory cannot claim a command that was meant for `PATH`.
    #[test]
    fn a_bare_program_name_is_not_a_path() {
        assert!(!looks_like_path("mupdf"));
        assert!(!looks_like_path("mupdf -r 120"));
        assert!(looks_like_path("/usr/bin/mupdf"));
    }

    /// Knowing that nothing can open the file does not get it open, so both
    /// forms of the message have to end with something to do.
    #[test]
    fn the_no_handler_message_says_what_to_do_next() {
        let path = Path::new("/books/A Book.pdf");

        let generic = no_handler_message(path, false);
        assert!(generic.contains("A Book.pdf"), "got: {generic}");
        assert!(generic.contains("xdg-utils"), "got: {generic}");
        assert!(generic.contains("reader = zathura"), "got: {generic}");

        // WSL has no desktop at all, so pointing at xdg-utils would be a
        // wasted install.
        let wsl = no_handler_message(path, true);
        assert!(wsl.contains("wslu") && wsl.contains("reader = wslview"), "got: {wsl}");
        assert!(!wsl.contains("xdg-utils"), "got: {wsl}");
    }

    /// `reader` is not consulted when showing a file, so the advice for that
    /// failure has to be different advice, not the same sentence reused.
    #[test]
    fn showing_a_file_is_not_told_to_set_a_reader() {
        for wsl in [true, false] {
            let message = cannot_show_message(Path::new("/books/A Book.pdf"), wsl);
            assert!(message.contains("A Book.pdf"), "got: {message}");
            assert!(!message.contains("reader"), "got: {message}");
        }
        assert!(cannot_show_message(Path::new("/books/A Book.pdf"), true).contains("explorer.exe"));
    }

    /// A book title is as long as its publisher felt like making it, and the
    /// advice sits at the end of the message. One must not push out the other.
    #[test]
    fn a_long_title_cannot_crowd_out_the_advice() {
        let path = PathBuf::from(format!("/books/{}.pdf", "Dungeons and Dragons ".repeat(20)));
        for wsl in [true, false] {
            let message = no_handler_message(&path, wsl);
            assert!(
                message.len() < 200,
                "{} bytes: {message}",
                message.len()
            );
            assert!(message.ends_with("in the config") || message.contains("reader = zathura"));
        }
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
