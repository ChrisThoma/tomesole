//! Getting pixels onto a terminal.
//!
//! Three ways, in descending order of how good they look:
//!
//! * **The kitty graphics protocol** — kitty, Ghostty and WezTerm. Real pixels,
//!   placed at the cursor and left to the terminal to scale.
//! * **The iTerm2 inline image protocol** — iTerm2, and WezTerm again. Also
//!   real pixels, and it takes the encoded file as-is, so a JPEG never has to
//!   be decoded at all.
//! * **Half-block characters** — anywhere else with colour. Each cell is a `▀`
//!   with one colour on top and another behind, so a character cell carries two
//!   pixels. Chunky, but it is text, which means ratatui can draw it as part of
//!   the ordinary buffer instead of us writing escape codes behind its back.
//!
//! Detection is by environment variable, because the alternative — writing a
//! query escape to the terminal and waiting for an answer — races with the
//! keyboard reader for the reply, and a terminal that does not answer costs a
//! timeout on every start.

use std::sync::OnceLock;

pub type Rgb = (u8, u8, u8);

/// How this terminal can be made to show an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Kitty,
    ITerm2,
    /// No image protocol, but enough colour for half-block art.
    Blocks,
    /// Nothing worth drawing.
    None,
}

impl Protocol {
    /// True when the terminal draws actual pixels rather than characters.
    pub fn is_pixels(self) -> bool {
        matches!(self, Protocol::Kitty | Protocol::ITerm2)
    }
}

/// What this terminal supports. Decided once; nothing here changes at runtime.
pub fn detect() -> Protocol {
    static CACHED: OnceLock<Protocol> = OnceLock::new();
    *CACHED.get_or_init(|| from_env(&|name: &str| std::env::var(name)))
}

/// The detection itself, taking its environment as an argument so it can be
/// tested without touching the real one.
fn from_env(var: &dyn Fn(&str) -> std::result::Result<String, std::env::VarError>) -> Protocol {
    let get = |name: &str| var(name).unwrap_or_default().to_ascii_lowercase();
    let set = |name: &str| !var(name).unwrap_or_default().is_empty();

    let term = get("TERM");
    if term == "dumb" || set("NO_COLOR") {
        return Protocol::None;
    }

    // Kitty's own terminal, and the two that reimplemented its protocol.
    if term.contains("kitty") || set("KITTY_WINDOW_ID") || set("GHOSTTY_RESOURCES_DIR") {
        return Protocol::Kitty;
    }
    match get("TERM_PROGRAM").as_str() {
        "ghostty" => return Protocol::Kitty,
        // WezTerm speaks both; kitty's protocol scales images for us.
        "wezterm" => return Protocol::Kitty,
        "iterm.app" | "iterm2" => return Protocol::ITerm2,
        _ => {}
    }
    if set("WEZTERM_PANE") {
        return Protocol::Kitty;
    }
    if set("ITERM_SESSION_ID") {
        return Protocol::ITerm2;
    }

    // No pixels, but colour is enough to be worth drawing something.
    let colorterm = get("COLORTERM");
    if colorterm.contains("truecolor")
        || colorterm.contains("24bit")
        || term.contains("256color")
        || term.contains("color")
    {
        return Protocol::Blocks;
    }
    Protocol::None
}

/// An 8-bit RGB image.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Image {
    pub width: usize,
    pub height: usize,
    /// `width * height * 3` bytes, row-major.
    pub pixels: Vec<u8>,
}

impl Image {
    pub fn new(width: usize, height: usize, pixels: Vec<u8>) -> Self {
        debug_assert_eq!(pixels.len(), width * height * 3);
        Image {
            width,
            height,
            pixels,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    pub fn pixel(&self, x: usize, y: usize) -> Rgb {
        let i = (y.min(self.height.saturating_sub(1)) * self.width
            + x.min(self.width.saturating_sub(1)))
            * 3;
        match self.pixels.get(i..i + 3) {
            Some(p) => (p[0], p[1], p[2]),
            None => (0, 0, 0),
        }
    }

    /// Scale to fit inside a box, keeping the aspect ratio.
    ///
    /// Nearest-neighbour: a cover is being squeezed into something like eighty
    /// pixels across, where the difference between this and a proper filter is
    /// not visible, and a resampler is a lot of code to not see.
    pub fn fit(&self, max_width: usize, max_height: usize) -> Image {
        if self.is_empty() || max_width == 0 || max_height == 0 {
            return Image::default();
        }
        // Never enlarge: blowing a 40-pixel thumbnail up to fill a pane makes
        // it worse, not bigger.
        let scale = (max_width as f64 / self.width as f64)
            .min(max_height as f64 / self.height as f64)
            .min(1.0);
        let width = ((self.width as f64 * scale).round() as usize).clamp(1, max_width);
        let height = ((self.height as f64 * scale).round() as usize).clamp(1, max_height);

        let mut pixels = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            let sy = y * self.height / height;
            for x in 0..width {
                let sx = x * self.width / width;
                let (r, g, b) = self.pixel(sx, sy);
                pixels.extend_from_slice(&[r, g, b]);
            }
        }
        Image::new(width, height, pixels)
    }
}

// --- kitty ---------------------------------------------------------------

/// Kitty accepts at most 4096 base64 bytes per escape, so a picture arrives in
/// instalments.
const CHUNK: usize = 4096;

/// Transmit and place an image at the cursor, scaled into `cols` × `rows`.
///
/// `id` names the image so it can be deleted again; reusing an id replaces
/// whatever was there.
pub fn kitty_image(id: u32, image: &Image, cols: u16, rows: u16) -> String {
    if image.is_empty() {
        return String::new();
    }
    // `f=24` is raw RGB, which spares us encoding a PNG just to have it
    // decoded again at the other end. `C=1` leaves the cursor where it was:
    // without it the terminal scrolls when an image is placed near the bottom
    // of the screen, which is exactly where the detail pane is.
    let header = format!(
        "a=T,q=2,C=1,f=24,s={},v={},c={cols},r={rows}",
        image.width, image.height
    );
    transmit(id, &header, &base64(&image.pixels))
}

/// Transmit an already-encoded PNG, which kitty decodes itself.
pub fn kitty_png(id: u32, png: &[u8], cols: u16, rows: u16) -> String {
    if png.is_empty() {
        return String::new();
    }
    transmit(
        id,
        &format!("a=T,q=2,C=1,f=100,c={cols},r={rows}"),
        &base64(png),
    )
}

fn transmit(id: u32, header: &str, payload: &str) -> String {
    let mut out = String::with_capacity(payload.len() + 64);
    let chunks: Vec<&str> = payload
        .as_bytes()
        .chunks(CHUNK)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect();

    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        if i == 0 {
            out.push_str(&format!("\x1b_G{header},i={id},m={more};{chunk}\x1b\\"));
        } else {
            // Continuations carry only the "there is more coming" flag.
            out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }
    out
}

/// Remove an image and free the terminal's copy of it.
pub fn kitty_delete(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

// --- iTerm2 --------------------------------------------------------------

/// Place an encoded image (JPEG, PNG, GIF — whatever the OS can read) at the
/// cursor, scaled into `cols` × `rows`.
pub fn iterm_image(encoded: &[u8], cols: u16, rows: u16) -> String {
    if encoded.is_empty() {
        return String::new();
    }
    format!(
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=1:{}\x07",
        encoded.len(),
        base64(encoded)
    )
}

// --- half blocks ---------------------------------------------------------

/// One character cell: the colour of its upper half and of its lower half.
pub type Cell = (Rgb, Rgb);

/// Render an image as rows of `▀`, two pixels to a cell.
///
/// The image is fitted to `cols` × `rows * 2` pixels first, so the result is
/// exactly as wide and tall as the space it was given, or less.
pub fn half_blocks(image: &Image, cols: u16, rows: u16) -> Vec<Vec<Cell>> {
    let fitted = image.fit(cols as usize, rows as usize * 2);
    if fitted.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(fitted.height.div_ceil(2));
    for y in (0..fitted.height).step_by(2) {
        let mut row = Vec::with_capacity(fitted.width);
        for x in 0..fitted.width {
            let upper = fitted.pixel(x, y);
            // An odd-height image ends on a half-filled row; repeating the
            // last line beats a stripe of black across the bottom.
            let lower = fitted.pixel(x, (y + 1).min(fitted.height - 1));
            row.push((upper, lower));
        }
        out.push(row);
    }
    out
}

/// The half-block glyph the cells above are meant to be drawn with.
pub const UPPER_HALF: char = '▀';

// --- base64 ---------------------------------------------------------------

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, which is what both protocols expect.
pub fn base64(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::env::VarError;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> std::result::Result<String, VarError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned().ok_or(VarError::NotPresent)
    }

    #[test]
    fn kitty_terminals_are_recognised() {
        for pairs in [
            vec![("TERM", "xterm-kitty")],
            vec![("KITTY_WINDOW_ID", "1")],
            vec![("TERM_PROGRAM", "ghostty")],
            vec![("GHOSTTY_RESOURCES_DIR", "/x")],
            vec![("TERM_PROGRAM", "WezTerm")],
        ] {
            assert_eq!(from_env(&env(&pairs)), Protocol::Kitty, "{pairs:?}");
        }
    }

    #[test]
    fn iterm_is_recognised() {
        assert_eq!(
            from_env(&env(&[("TERM_PROGRAM", "iTerm.app")])),
            Protocol::ITerm2
        );
        assert_eq!(
            from_env(&env(&[("ITERM_SESSION_ID", "w0t0p0")])),
            Protocol::ITerm2
        );
    }

    #[test]
    fn terminals_without_images_still_get_blocks() {
        assert_eq!(
            from_env(&env(&[
                ("TERM_PROGRAM", "Apple_Terminal"),
                ("TERM", "xterm-256color")
            ])),
            Protocol::Blocks
        );
        assert_eq!(
            from_env(&env(&[
                ("TERM_PROGRAM", "vscode"),
                ("COLORTERM", "truecolor")
            ])),
            Protocol::Blocks
        );
    }

    #[test]
    fn a_terminal_that_cannot_do_anything_is_left_alone() {
        assert_eq!(from_env(&env(&[("TERM", "dumb")])), Protocol::None);
        assert_eq!(from_env(&env(&[])), Protocol::None);
        // Respecting NO_COLOR matters more than showing off.
        assert_eq!(
            from_env(&env(&[("TERM", "xterm-kitty"), ("NO_COLOR", "1")])),
            Protocol::None
        );
    }

    fn solid(width: usize, height: usize, colour: Rgb) -> Image {
        let mut pixels = Vec::with_capacity(width * height * 3);
        for _ in 0..width * height {
            pixels.extend_from_slice(&[colour.0, colour.1, colour.2]);
        }
        Image::new(width, height, pixels)
    }

    #[test]
    fn fitting_preserves_the_aspect_ratio() {
        let tall = solid(100, 200, (1, 2, 3));
        let fitted = tall.fit(50, 50);
        assert_eq!((fitted.width, fitted.height), (25, 50));
        assert_eq!(fitted.pixel(0, 0), (1, 2, 3));

        let wide = solid(200, 100, (0, 0, 0));
        let fitted = wide.fit(50, 50);
        assert_eq!((fitted.width, fitted.height), (50, 25));
    }

    #[test]
    fn fitting_a_degenerate_image_does_not_panic() {
        assert!(Image::default().fit(10, 10).is_empty());
        assert!(solid(4, 4, (0, 0, 0)).fit(0, 10).is_empty());
        // One pixel is still a picture, and it is not stretched into sixteen.
        assert_eq!(solid(1, 1, (9, 9, 9)).fit(10, 10).width, 1);
    }

    #[test]
    fn fitting_never_enlarges() {
        let small = solid(10, 10, (5, 5, 5));
        let fitted = small.fit(100, 100);
        assert_eq!((fitted.width, fitted.height), (10, 10));
    }

    #[test]
    fn half_blocks_fill_the_space_they_are_given() {
        let image = solid(80, 160, (10, 20, 30));
        let cells = half_blocks(&image, 20, 20);
        assert_eq!(cells.len(), 20, "20 rows of cells is 40 pixel rows");
        assert_eq!(cells[0].len(), 20);
        assert_eq!(cells[0][0], ((10, 20, 30), (10, 20, 30)));
    }

    #[test]
    fn an_odd_number_of_pixel_rows_does_not_run_off_the_end() {
        // 3 pixels tall: the last cell has no lower half to read.
        let image = solid(3, 3, (7, 7, 7));
        let cells = half_blocks(&image, 3, 8);
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[1][0], ((7, 7, 7), (7, 7, 7)));
    }

    #[test]
    fn base64_matches_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0x00, 0xff]), "/wD/");
    }

    #[test]
    fn a_kitty_escape_is_well_formed_and_chunked() {
        // Big enough to need several instalments.
        let image = solid(60, 60, (1, 2, 3));
        let escape = kitty_image(7, &image, 10, 5);

        assert!(
            escape.starts_with("\x1b_Ga=T,q=2,C=1,f=24,s=60,v=60,c=10,r=5,i=7,m=1;"),
            "got: {:?}",
            &escape[..60.min(escape.len())]
        );
        assert!(escape.ends_with("\x1b\\"));
        let chunks = escape.matches("\x1b_G").count();
        assert!(chunks > 1, "a 10 KB image should be chunked, got {chunks}");
        // Exactly one chunk announces itself as the last.
        assert_eq!(escape.matches("m=0;").count(), 1);
    }

    #[test]
    fn a_single_chunk_image_says_so_immediately() {
        let escape = kitty_image(1, &solid(2, 2, (0, 0, 0)), 1, 1);
        assert_eq!(escape.matches("\x1b_G").count(), 1);
        assert!(escape.contains("m=0;"));
    }

    #[test]
    fn empty_images_produce_no_escape_at_all() {
        assert!(kitty_image(1, &Image::default(), 4, 4).is_empty());
        assert!(kitty_png(1, &[], 4, 4).is_empty());
        assert!(iterm_image(&[], 4, 4).is_empty());
    }

    #[test]
    fn the_delete_escape_frees_the_image() {
        assert_eq!(kitty_delete(3), "\x1b_Ga=d,d=I,i=3,q=2\x1b\\");
    }

    #[test]
    fn an_iterm_escape_carries_the_file_verbatim() {
        let escape = iterm_image(b"\xff\xd8\xffnot really a jpeg", 8, 4);
        assert!(
            escape.starts_with("\x1b]1337;File=inline=1;size=20;width=8;height=4;"),
            "got: {escape:?}"
        );
        assert!(escape.ends_with('\x07'));
        assert!(escape.contains(&base64(b"\xff\xd8\xffnot really a jpeg")));
    }
}
