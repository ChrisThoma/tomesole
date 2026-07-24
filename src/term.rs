//! Terminal presentation: colour, width-aware layout, tables, prompts, and a
//! progress bar.
//!
//! All hand-rolled, because the alternative is pulling a styling crate, a
//! width crate, a progress crate and a prompt crate to draw one table and one
//! bar. The only non-obvious piece is [`terminal_width`], which asks the OS
//! directly rather than adding a dependency for a single ioctl.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

/// ANSI SGR codes, emitted only when the stream is a terminal and the user has
/// not asked for plain output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    enabled: bool,
}

impl Style {
    /// Honours `NO_COLOR` (any value) and `CLICOLOR_FORCE`, then falls back to
    /// "is stdout a terminal".
    pub fn detect(force_plain: bool) -> Self {
        if force_plain || std::env::var_os("NO_COLOR").is_some() {
            return Self { enabled: false };
        }
        if std::env::var_os("CLICOLOR_FORCE").is_some() {
            return Self { enabled: true };
        }
        Self {
            enabled: std::io::stdout().is_terminal(),
        }
    }

    #[cfg(test)]
    pub fn plain() -> Self {
        Self { enabled: false }
    }

    fn wrap(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, t: &str) -> String {
        self.wrap("1", t)
    }
    pub fn dim(&self, t: &str) -> String {
        self.wrap("2", t)
    }
    pub fn red(&self, t: &str) -> String {
        self.wrap("31", t)
    }
    pub fn green(&self, t: &str) -> String {
        self.wrap("32", t)
    }
    pub fn yellow(&self, t: &str) -> String {
        self.wrap("33", t)
    }
    pub fn cyan(&self, t: &str) -> String {
        self.wrap("36", t)
    }
}

/// Terminal width in columns.
///
/// Tries the real window size first, then `COLUMNS`, then a conservative
/// default. Clamped so a very narrow or absurdly wide terminal still produces
/// a sane layout.
pub fn terminal_width() -> usize {
    terminal_size().0
}

/// Terminal size as `(columns, rows)`.
pub fn terminal_size() -> (usize, usize) {
    if let Some((cols, rows)) = os_terminal_size() {
        return (cols.clamp(40, 200), rows.clamp(10, 200));
    }
    let cols = std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(90)
        .clamp(40, 200);
    let rows = std::env::var("LINES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(30)
        .clamp(10, 200);
    (cols, rows)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn os_terminal_size() -> Option<(usize, usize)> {
    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    // Declared directly rather than depending on `libc`: this is the only
    // platform call the program makes, and libc is linked regardless.
    unsafe extern "C" {
        fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
    }

    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;
    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;

    if !std::io::stdout().is_terminal() {
        return None;
    }
    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `ws` is a valid, correctly-sized Winsize for the duration of the
    // call, and fd 1 is stdout. The return value is checked before use.
    let rc = unsafe { ioctl(1, TIOCGWINSZ, &raw mut ws) };
    if rc == 0 && ws.ws_col > 0 {
        Some((ws.ws_col as usize, ws.ws_row as usize))
    } else {
        None
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn os_terminal_size() -> Option<(usize, usize)> {
    None
}

/// Approximate display width of a string in terminal cells.
///
/// Handles the cases that matter for a Libgen catalogue: CJK and full-width
/// forms take two cells, combining marks take none. This is an approximation
/// of East Asian Width, not a full Unicode implementation — being one column
/// off on an exotic script misaligns a table, which is a cosmetic problem.
pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    let cp = c as u32;
    if c == '\0' || (c.is_control()) {
        return 0;
    }
    // Combining marks.
    if (0x0300..=0x036F).contains(&cp)
        || (0x1AB0..=0x1AFF).contains(&cp)
        || (0x20D0..=0x20FF).contains(&cp)
        || (0xFE20..=0xFE2F).contains(&cp)
    {
        return 0;
    }
    // Wide / full-width ranges.
    if (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0x303E).contains(&cp)
        || (0x3041..=0x33FF).contains(&cp)
        || (0x3400..=0x4DBF).contains(&cp)
        || (0x4E00..=0x9FFF).contains(&cp)
        || (0xA000..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE6F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x1F300..=0x1F64F).contains(&cp)
        || (0x1F900..=0x1F9FF).contains(&cp)
        || (0x20000..=0x3FFFD).contains(&cp)
    {
        return 2;
    }
    1
}

/// Truncate to `width` display cells, appending `…` when anything was cut.
pub fn truncate(s: &str, width: usize) -> String {
    if display_width(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0usize;
    // Reserve one cell for the ellipsis.
    for c in s.chars() {
        let w = char_width(c);
        if used + w > width.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Pad on the right to `width` display cells.
pub fn pad(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

/// Pad on the left to `width` display cells.
pub fn pad_left(s: &str, width: usize) -> String {
    let w = display_width(s);
    if w >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - w))
    }
}

/// Parse a selection like `3`, `1,4,7`, `2-5`, or `1-3, 8` into indices.
///
/// Values are 1-based on the way in and 0-based on the way out. Out-of-range
/// entries are an error rather than silently dropped, so a typo does not
/// quietly download the wrong thing.
pub fn parse_selection(input: &str, max: usize) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    let cleaned = input.trim();
    if cleaned.is_empty() {
        return Err("nothing entered".to_string());
    }

    for part in cleaned.split([',', ' ']).filter(|p| !p.trim().is_empty()) {
        let part = part.trim();
        if let Some((lo, hi)) = part.split_once('-') {
            let lo: usize = lo
                .trim()
                .parse()
                .map_err(|_| format!("`{}` is not a number", lo.trim()))?;
            let hi: usize = hi
                .trim()
                .parse()
                .map_err(|_| format!("`{}` is not a number", hi.trim()))?;
            if lo == 0 || hi == 0 || lo > max || hi > max {
                return Err(format!("`{part}` is outside 1-{max}"));
            }
            if lo > hi {
                return Err(format!("`{part}` counts backwards"));
            }
            out.extend(lo..=hi);
        } else {
            let n: usize = part
                .parse()
                .map_err(|_| format!("`{part}` is not a number"))?;
            if n == 0 || n > max {
                return Err(format!("`{n}` is outside 1-{max}"));
            }
            out.push(n);
        }
    }

    out.sort_unstable();
    out.dedup();
    Ok(out.into_iter().map(|n| n - 1).collect())
}

/// A single-line progress bar that redraws in place.
pub struct Progress {
    label: String,
    total: Option<u64>,
    current: u64,
    started: Instant,
    last_draw: Instant,
    style: Style,
    active: bool,
    width: usize,
}

impl Progress {
    pub fn new(label: impl Into<String>, total: Option<u64>, style: Style) -> Self {
        let now = Instant::now();
        Self {
            label: label.into(),
            total,
            current: 0,
            started: now,
            // Force the first draw.
            last_draw: now - Duration::from_secs(1),
            style,
            active: std::io::stderr().is_terminal(),
            width: terminal_width(),
        }
    }

    /// Set the absolute byte count, for callers that track their own total.
    pub fn set(&mut self, done: u64, total: Option<u64>) {
        self.current = done;
        if total.is_some() {
            self.total = total;
        }
        self.draw(false);
    }

    fn draw(&mut self, force: bool) {
        if !self.active {
            return;
        }
        // ~20 fps is smooth enough and keeps the syscall count down.
        if !force && self.last_draw.elapsed() < Duration::from_millis(50) {
            return;
        }
        self.last_draw = Instant::now();

        let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
        let rate = self.current as f64 / elapsed;
        let done = crate::model::human_bytes(self.current);
        let speed = format!("{}/s", crate::model::human_bytes(rate as u64));

        let line = match self.total {
            Some(total) if total > 0 => {
                let frac = (self.current as f64 / total as f64).clamp(0.0, 1.0);
                let remaining = if rate > 0.0 {
                    format_eta(((total - self.current.min(total)) as f64 / rate) as u64)
                } else {
                    "--".to_string()
                };
                let stats = format!(
                    "{:>3}%  {} / {}  {}  eta {}",
                    (frac * 100.0) as u32,
                    done,
                    crate::model::human_bytes(total),
                    speed,
                    remaining
                );
                // Give the bar whatever space is left over.
                let fixed = display_width(&self.label) + display_width(&stats) + 6;
                let bar_width = self.width.saturating_sub(fixed).clamp(8, 40);
                let filled = (frac * bar_width as f64).round() as usize;
                let bar = format!(
                    "{}{}",
                    "█".repeat(filled),
                    self.style.dim(&"░".repeat(bar_width - filled))
                );
                format!(
                    "  {} {}  {}",
                    self.style.cyan(&self.label),
                    bar,
                    self.style.dim(&stats)
                )
            }
            _ => format!(
                "  {} {}",
                self.style.cyan(&self.label),
                self.style.dim(&format!("{done}  {speed}"))
            ),
        };

        let mut err = std::io::stderr();
        // \r then clear-to-end-of-line, so a shorter line never leaves debris.
        let _ = write!(err, "\r\x1b[2K{line}");
        let _ = err.flush();
    }

    /// Erase the bar, leaving the cursor at the start of a clean line.
    pub fn clear(&mut self) {
        if self.active {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        }
        self.active = false;
    }
}

fn format_eta(seconds: u64) -> String {
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, (seconds % 3600) / 60),
    }
}

/// A transient status line, used while probing mirrors or searching.
pub struct Spinner {
    active: bool,
}

impl Spinner {
    pub fn start(message: &str, style: Style) -> Self {
        let active = std::io::stderr().is_terminal();
        if active {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r\x1b[2K  {} {}", style.cyan("·"), style.dim(message));
            let _ = err.flush();
        }
        Self { active }
    }

    pub fn update(&self, message: &str, style: Style) {
        if self.active {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r\x1b[2K  {} {}", style.cyan("·"), style.dim(message));
            let _ = err.flush();
        }
    }

    pub fn clear(self) {
        if self.active {
            let mut err = std::io::stderr();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_are_inert_when_disabled() {
        let s = Style::plain();
        assert_eq!(s.bold("x"), "x");
        assert_eq!(s.red("x"), "x");
    }

    #[test]
    fn styles_wrap_when_enabled() {
        let s = Style { enabled: true };
        assert_eq!(s.bold("x"), "\x1b[1mx\x1b[0m");
    }

    #[test]
    fn measures_wide_characters() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(display_width(""), 0);
        // Combining acute accent adds no width.
        assert_eq!(display_width("e\u{0301}"), 1);
    }

    #[test]
    fn truncation_respects_display_width() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 8), "hello w…");
        assert_eq!(display_width(&truncate("hello world", 8)), 8);
        // Never splits a wide char across the boundary.
        let t = truncate("日本語テスト", 5);
        assert!(display_width(&t) <= 5);
    }

    #[test]
    fn padding_uses_display_width() {
        assert_eq!(pad("ab", 5), "ab   ");
        assert_eq!(display_width(&pad("日本", 6)), 6);
        assert_eq!(pad_left("7", 3), "  7");
        // Over-long input is returned unchanged rather than truncated.
        assert_eq!(pad("abcdef", 3), "abcdef");
    }

    #[test]
    fn parses_selection_forms() {
        assert_eq!(parse_selection("3", 10).unwrap(), [2]);
        assert_eq!(parse_selection("1,3", 10).unwrap(), [0, 2]);
        assert_eq!(parse_selection("2-4", 10).unwrap(), [1, 2, 3]);
        assert_eq!(parse_selection("1-2, 5", 10).unwrap(), [0, 1, 4]);
        assert_eq!(parse_selection(" 4 ", 10).unwrap(), [3]);
        // Duplicates collapse.
        assert_eq!(parse_selection("2,2,2", 10).unwrap(), [1]);
    }

    #[test]
    fn rejects_out_of_range_and_malformed_selections() {
        assert!(parse_selection("0", 10).is_err());
        assert!(parse_selection("11", 10).is_err());
        assert!(parse_selection("abc", 10).is_err());
        assert!(parse_selection("", 10).is_err());
        assert!(parse_selection("5-2", 10).is_err());
        assert!(parse_selection("1-99", 10).is_err());
    }

    #[test]
    fn formats_eta_readably() {
        assert_eq!(format_eta(5), "5s");
        assert_eq!(format_eta(65), "1m05s");
        assert_eq!(format_eta(3700), "1h01m");
    }

    #[test]
    fn terminal_width_is_always_sane() {
        let w = terminal_width();
        assert!((40..=200).contains(&w), "got {w}");
    }
}
