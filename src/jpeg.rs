//! A baseline JPEG decoder.
//!
//! Cover art arrives as JPEG, and two of the three ways of drawing it need
//! actual pixels: the half-block fallback obviously, and kitty's protocol
//! because sending raw RGB avoids re-encoding a PNG only for the terminal to
//! decode it again. iTerm2 is the exception — it takes the file as-is — so
//! nothing here runs when that is the protocol in use.
//!
//! Scope is deliberately narrow: baseline sequential DCT, Huffman coded, one
//! or three components. That is what every cover on Libgen is. Progressive and
//! arithmetic-coded files are refused by name rather than half-decoded, and the
//! caller falls back to showing no picture, which is a much better outcome than
//! a mangled one.
//!
//! Every length, index and table id read from the file is checked before use.
//! This is an image parser fed by an untrusted mirror — the one place in this
//! program where a malformed input has a whole state machine to play with — so
//! it is written to fail rather than to be clever, and it never indexes without
//! having bounded the index first.

use crate::error::Result;
use crate::graphics::Image;
use crate::{bail, err};

/// The largest picture worth decoding for a thumbnail. Anything bigger is a
/// waste of time at best and a memory bomb at worst.
const MAX_DIMENSION: usize = 4096;
const MAX_PIXELS: usize = 16_000_000;

/// True when the bytes start with a JPEG signature.
pub fn is_jpeg(data: &[u8]) -> bool {
    data.starts_with(&[0xFF, 0xD8, 0xFF])
}

/// True when the bytes start with a PNG signature.
pub fn is_png(data: &[u8]) -> bool {
    data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
}

/// Decode a baseline JPEG into RGB.
pub fn decode(data: &[u8]) -> Result<Image> {
    Decoder::new(data).decode()
}

/// Coefficient order within a block. The file stores them along a diagonal
/// zigzag; this maps position in that sequence to position in the 8×8 block.
#[rustfmt::skip]
const ZIGZAG: [usize; 64] = [
     0,  1,  8, 16,  9,  2,  3, 10,
    17, 24, 32, 25, 18, 11,  4,  5,
    12, 19, 26, 33, 40, 48, 41, 34,
    27, 20, 13,  6,  7, 14, 21, 28,
    35, 42, 49, 56, 57, 50, 43, 36,
    29, 22, 15, 23, 30, 37, 44, 51,
    58, 59, 52, 45, 38, 31, 39, 46,
    53, 60, 61, 54, 47, 55, 62, 63,
];

/// A canonical Huffman table, in the form the spec's decoding procedure wants.
#[derive(Clone, Default)]
struct HuffTable {
    /// Smallest code of each length, indexed by length - 1.
    min_code: [i32; 16],
    /// Largest code of each length, or -1 when no code has that length.
    max_code: [i32; 16],
    /// Index into `values` of the first code of each length.
    val_index: [usize; 16],
    values: Vec<u8>,
}

impl HuffTable {
    /// Build from the counts-and-values form stored in a DHT segment.
    fn build(counts: &[u8; 16], values: Vec<u8>) -> HuffTable {
        let mut table = HuffTable {
            values,
            ..Default::default()
        };
        let mut code = 0i32;
        let mut index = 0usize;
        for (length, count) in counts.iter().enumerate() {
            let count = *count as usize;
            table.val_index[length] = index;
            table.min_code[length] = code;
            if count == 0 {
                // No code of this length; mark it unmatchable.
                table.max_code[length] = -1;
            } else {
                table.max_code[length] = code + count as i32 - 1;
                code += count as i32;
                index += count;
            }
            code <<= 1;
        }
        table
    }
}

/// One colour channel.
#[derive(Clone)]
struct Component {
    id: u8,
    /// Horizontal and vertical sampling factors.
    h: usize,
    v: usize,
    /// Quantisation table index.
    tq: usize,
    /// Huffman table indices, set when the scan header is read.
    td: usize,
    ta: usize,
    /// Running DC predictor.
    pred: i32,
    /// Decoded samples, at this component's own resolution.
    plane: Vec<u8>,
    stride: usize,
    rows: usize,
}

struct Decoder<'a> {
    data: &'a [u8],
    pos: usize,
    width: usize,
    height: usize,
    components: Vec<Component>,
    quant: [[u16; 64]; 4],
    dc_tables: Vec<HuffTable>,
    ac_tables: Vec<HuffTable>,
    restart_interval: usize,
    /// Adobe's colour transform flag, when the file carries an APP14 segment.
    adobe_transform: Option<u8>,
}

impl<'a> Decoder<'a> {
    fn new(data: &'a [u8]) -> Self {
        Decoder {
            data,
            pos: 0,
            width: 0,
            height: 0,
            components: Vec::new(),
            quant: [[0; 64]; 4],
            dc_tables: vec![HuffTable::default(); 4],
            ac_tables: vec![HuffTable::default(); 4],
            restart_interval: 0,
            adobe_transform: None,
        }
    }

    fn byte(&mut self) -> Result<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| err!("the image ends in the middle of a segment"))?;
        self.pos += 1;
        Ok(b)
    }

    fn u16be(&mut self) -> Result<usize> {
        let hi = self.byte()? as usize;
        let lo = self.byte()? as usize;
        Ok((hi << 8) | lo)
    }

    /// Take the body of a segment, given the length field that precedes it.
    fn segment(&mut self) -> Result<&'a [u8]> {
        let length = self.u16be()?;
        if length < 2 {
            bail!("a segment claims an impossible length of {length}");
        }
        let body = length - 2;
        let end = self
            .pos
            .checked_add(body)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| err!("a segment runs past the end of the image"))?;
        let slice = &self.data[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn decode(mut self) -> Result<Image> {
        if !is_jpeg(self.data) {
            bail!("not a JPEG");
        }
        self.pos = 2;

        loop {
            // Markers are 0xFF followed by a type; padding 0xFF bytes are legal
            // between segments.
            let mut marker = self.byte()?;
            if marker != 0xFF {
                bail!("expected a marker, found {marker:#04x}");
            }
            while marker == 0xFF {
                marker = self.byte()?;
            }

            match marker {
                // SOF0/SOF1: baseline and extended sequential, both decodable.
                0xC0 | 0xC1 => self.read_frame_header()?,
                0xC2 => bail!("progressive JPEG is not supported"),
                0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF => {
                    bail!("this JPEG uses a coding mode we do not read")
                }
                0xC4 => self.read_huffman_tables()?,
                0xDB => self.read_quant_tables()?,
                0xDD => {
                    let body = self.segment()?;
                    self.restart_interval = match body {
                        [hi, lo, ..] => ((*hi as usize) << 8) | *lo as usize,
                        _ => 0,
                    };
                }
                0xDA => return self.read_scan(),
                0xD9 => bail!("the image ended before any pixels were coded"),
                0xEE => {
                    // APP14. Adobe uses it to say whether three channels are
                    // YCbCr or plain RGB, and getting that backwards turns a
                    // cover into a psychedelic mess.
                    let body = self.segment()?;
                    if body.starts_with(b"Adobe") {
                        self.adobe_transform = body.last().copied();
                    }
                }
                // Everything else is metadata: thumbnails, colour profiles,
                // comments. Skipped wholesale.
                0xD8 => {}
                0xD0..=0xD7 | 0x01 => {}
                _ => {
                    self.segment()?;
                }
            }
        }
    }

    fn read_quant_tables(&mut self) -> Result<()> {
        let body = self.segment()?;
        let mut i = 0;
        while i < body.len() {
            let spec = body[i];
            i += 1;
            let precision = spec >> 4;
            let id = (spec & 15) as usize;
            if id >= 4 {
                bail!("quantisation table id {id} is out of range");
            }
            for k in 0..64 {
                let value = match precision {
                    0 => {
                        let v = *body
                            .get(i)
                            .ok_or_else(|| err!("a quantisation table is truncated"))?;
                        i += 1;
                        v as u16
                    }
                    1 => {
                        let hi = *body
                            .get(i)
                            .ok_or_else(|| err!("a quantisation table is truncated"))?;
                        let lo = *body
                            .get(i + 1)
                            .ok_or_else(|| err!("a quantisation table is truncated"))?;
                        i += 2;
                        ((hi as u16) << 8) | lo as u16
                    }
                    other => bail!("unknown quantisation precision {other}"),
                };
                self.quant[id][k] = value;
            }
        }
        Ok(())
    }

    fn read_huffman_tables(&mut self) -> Result<()> {
        let body = self.segment()?;
        let mut i = 0;
        while i < body.len() {
            let spec = body[i];
            i += 1;
            let class = spec >> 4;
            let id = (spec & 15) as usize;
            if id >= 4 || class > 1 {
                bail!("Huffman table {class}/{id} is out of range");
            }

            let counts: [u8; 16] = body
                .get(i..i + 16)
                .ok_or_else(|| err!("a Huffman table is truncated"))?
                .try_into()
                .expect("a 16-byte slice");
            i += 16;

            let total: usize = counts.iter().map(|c| *c as usize).sum();
            let values = body
                .get(i..i + total)
                .ok_or_else(|| err!("a Huffman table is truncated"))?
                .to_vec();
            i += total;

            let table = HuffTable::build(&counts, values);
            if class == 0 {
                self.dc_tables[id] = table;
            } else {
                self.ac_tables[id] = table;
            }
        }
        Ok(())
    }

    fn read_frame_header(&mut self) -> Result<()> {
        let body = self.segment()?;
        if body.len() < 6 {
            bail!("the frame header is too short to read");
        }
        let precision = body[0];
        if precision != 8 {
            bail!("only 8-bit JPEG is supported, this one is {precision}-bit");
        }
        self.height = ((body[1] as usize) << 8) | body[2] as usize;
        self.width = ((body[3] as usize) << 8) | body[4] as usize;
        let count = body[5] as usize;

        if self.width == 0 || self.height == 0 {
            bail!("the image claims to be {}×{}", self.width, self.height);
        }
        if self.width > MAX_DIMENSION || self.height > MAX_DIMENSION {
            bail!(
                "the image is {}×{}, larger than we will decode",
                self.width,
                self.height
            );
        }
        if self.width * self.height > MAX_PIXELS {
            bail!("the image has more pixels than we will decode");
        }
        if count != 1 && count != 3 {
            bail!("{count}-channel JPEG is not supported");
        }
        if body.len() < 6 + count * 3 {
            bail!("the frame header is missing channel descriptions");
        }

        self.components.clear();
        for c in 0..count {
            let base = 6 + c * 3;
            let h = (body[base + 1] >> 4) as usize;
            let v = (body[base + 1] & 15) as usize;
            let tq = body[base + 2] as usize;
            // Sampling factors above 4 are outside the spec, and a zero would
            // divide by zero later.
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) {
                bail!("channel {c} claims sampling factors {h}×{v}");
            }
            if tq >= 4 {
                bail!("channel {c} refers to quantisation table {tq}");
            }
            self.components.push(Component {
                id: body[base],
                h,
                v,
                tq,
                td: 0,
                ta: 0,
                pred: 0,
                plane: Vec::new(),
                stride: 0,
                rows: 0,
            });
        }
        Ok(())
    }

    fn read_scan(&mut self) -> Result<Image> {
        if self.components.is_empty() {
            bail!("the scan arrives before the frame header");
        }
        let body = self.segment()?;
        if body.is_empty() {
            bail!("the scan header is empty");
        }
        let count = body[0] as usize;
        if count != self.components.len() {
            bail!(
                "the scan covers {count} of {} channels",
                self.components.len()
            );
        }
        if body.len() < 1 + count * 2 {
            bail!("the scan header is truncated");
        }

        for s in 0..count {
            let id = body[1 + s * 2];
            let tables = body[2 + s * 2];
            let td = (tables >> 4) as usize;
            let ta = (tables & 15) as usize;
            if td >= 4 || ta >= 4 {
                bail!("the scan refers to Huffman tables {td}/{ta}");
            }
            let component = self
                .components
                .iter_mut()
                .find(|c| c.id == id)
                .ok_or_else(|| err!("the scan names a channel the frame never declared"))?;
            component.td = td;
            component.ta = ta;
        }

        self.decode_scan()
    }

    fn decode_scan(&mut self) -> Result<Image> {
        let hmax = self.components.iter().map(|c| c.h).max().unwrap_or(1);
        let vmax = self.components.iter().map(|c| c.v).max().unwrap_or(1);
        let mcus_x = self.width.div_ceil(8 * hmax);
        let mcus_y = self.height.div_ceil(8 * vmax);

        for component in &mut self.components {
            component.stride = mcus_x * component.h * 8;
            component.rows = mcus_y * component.v * 8;
            component.plane = vec![0u8; component.stride * component.rows];
            component.pred = 0;
        }

        let mut reader = BitReader::new(&self.data[self.pos..]);
        let mut samples = [0u8; 64];
        let mut since_restart = 0usize;

        for my in 0..mcus_y {
            for mx in 0..mcus_x {
                if self.restart_interval > 0 && since_restart == self.restart_interval {
                    reader.restart();
                    for component in &mut self.components {
                        component.pred = 0;
                    }
                    since_restart = 0;
                }
                since_restart += 1;

                for index in 0..self.components.len() {
                    let (h, v, tq, td, ta) = {
                        let c = &self.components[index];
                        (c.h, c.v, c.tq, c.td, c.ta)
                    };
                    for by in 0..v {
                        for bx in 0..h {
                            let mut block = [0i32; 64];
                            let pred = self.components[index].pred;
                            let next = decode_block(
                                &mut reader,
                                &self.dc_tables[td],
                                &self.ac_tables[ta],
                                &self.quant[tq],
                                pred,
                                &mut block,
                            )?;
                            self.components[index].pred = next;
                            idct(&block, &mut samples);

                            let component = &mut self.components[index];
                            let x0 = (mx * h + bx) * 8;
                            let y0 = (my * v + by) * 8;
                            for row in 0..8 {
                                let start = (y0 + row) * component.stride + x0;
                                // The plane was sized to whole MCUs, so a full
                                // block always fits; the guard is for a
                                // malformed header that got this far.
                                if let Some(target) = component.plane.get_mut(start..start + 8) {
                                    target.copy_from_slice(&samples[row * 8..row * 8 + 8]);
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(self.to_rgb(hmax, vmax))
    }

    /// Upsample the channels and convert to RGB.
    fn to_rgb(&self, hmax: usize, vmax: usize) -> Image {
        let mut pixels = Vec::with_capacity(self.width * self.height * 3);

        // Three channels are YCbCr unless Adobe says otherwise.
        let ycbcr = self.components.len() == 3 && self.adobe_transform != Some(0);

        for y in 0..self.height {
            for x in 0..self.width {
                let sample = |c: &Component| -> u8 {
                    let sx = x * c.h / hmax;
                    let sy = y * c.v / vmax;
                    c.plane
                        .get(
                            sy.min(c.rows.saturating_sub(1)) * c.stride
                                + sx.min(c.stride.saturating_sub(1)),
                        )
                        .copied()
                        .unwrap_or(0)
                };

                match self.components.as_slice() {
                    [grey] => {
                        let v = sample(grey);
                        pixels.extend_from_slice(&[v, v, v]);
                    }
                    [a, b, c] if ycbcr => {
                        let (r, g, bl) = ycbcr_to_rgb(sample(a), sample(b), sample(c));
                        pixels.extend_from_slice(&[r, g, bl]);
                    }
                    [a, b, c] => {
                        pixels.extend_from_slice(&[sample(a), sample(b), sample(c)]);
                    }
                    _ => pixels.extend_from_slice(&[0, 0, 0]),
                }
            }
        }
        Image::new(self.width, self.height, pixels)
    }
}

fn ycbcr_to_rgb(y: u8, cb: u8, cr: u8) -> (u8, u8, u8) {
    let y = y as f32;
    let cb = cb as f32 - 128.0;
    let cr = cr as f32 - 128.0;
    let clamp = |v: f32| v.clamp(0.0, 255.0) as u8;
    (
        clamp(y + 1.402 * cr),
        clamp(y - 0.344_136 * cb - 0.714_136 * cr),
        clamp(y + 1.772 * cb),
    )
}

/// Decode one 8×8 block, returning the new DC predictor.
fn decode_block(
    reader: &mut BitReader,
    dc: &HuffTable,
    ac: &HuffTable,
    quant: &[u16; 64],
    pred: i32,
    block: &mut [i32; 64],
) -> Result<i32> {
    let t = reader.huffman(dc)? as usize;
    if t > 15 {
        bail!("a DC coefficient claims {t} bits");
    }
    let diff = if t == 0 {
        0
    } else {
        extend(reader.receive(t)?, t)
    };
    let pred = pred.wrapping_add(diff);
    block[0] = pred * quant[0] as i32;

    let mut k = 1usize;
    while k < 64 {
        let rs = reader.huffman(ac)? as usize;
        let size = rs & 15;
        let run = rs >> 4;

        if size == 0 {
            // 0xF0 is a run of sixteen zeroes; anything else ends the block.
            if run == 15 {
                k += 16;
                continue;
            }
            break;
        }
        k += run;
        if k > 63 {
            break;
        }
        block[ZIGZAG[k]] = extend(reader.receive(size)?, size) * quant[k] as i32;
        k += 1;
    }
    Ok(pred)
}

/// Turn a `size`-bit magnitude into a signed coefficient.
fn extend(value: i32, size: usize) -> i32 {
    if size == 0 {
        return 0;
    }
    if value < (1 << (size - 1)) {
        value - (1 << size) + 1
    } else {
        value
    }
}

/// `cos((2x + 1) u π / 16)`, scaled by the normalising constant, for all
/// `u`, `x`. Computed once and shared.
fn cosines() -> &'static [[f32; 8]; 8] {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[[f32; 8]; 8]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [[0f32; 8]; 8];
        for (u, row) in table.iter_mut().enumerate() {
            let scale = if u == 0 { (0.5f32).sqrt() } else { 1.0 };
            for (x, cell) in row.iter_mut().enumerate() {
                *cell =
                    scale * ((2.0 * x as f32 + 1.0) * u as f32 * std::f32::consts::PI / 16.0).cos();
            }
        }
        table
    })
}

/// Inverse DCT, separably: rows first, then columns.
fn idct(block: &[i32; 64], out: &mut [u8; 64]) {
    let cos = cosines();
    let mut rows = [0f32; 64];

    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0f32;
            for u in 0..8 {
                let coefficient = block[y * 8 + u];
                if coefficient != 0 {
                    sum += coefficient as f32 * cos[u][x];
                }
            }
            rows[y * 8 + x] = sum * 0.5;
        }
    }

    for x in 0..8 {
        for y in 0..8 {
            let mut sum = 0f32;
            for v in 0..8 {
                sum += rows[v * 8 + x] * cos[v][y];
            }
            // The coder subtracted 128 before transforming; put it back.
            out[y * 8 + x] = (sum * 0.5 + 128.5).clamp(0.0, 255.0) as u8;
        }
    }
}

/// Reads single bits out of an entropy-coded segment.
struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    current: u32,
    remaining: u32,
    /// Set once the stream runs into a marker or off the end. From then on it
    /// feeds zero bits, so a truncated image decodes to a grey tail rather
    /// than an error — half a cover is still a cover.
    finished: bool,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader {
            data,
            pos: 0,
            current: 0,
            remaining: 0,
            finished: false,
        }
    }

    fn bit(&mut self) -> u32 {
        if self.remaining == 0 {
            let Some(&byte) = self.data.get(self.pos) else {
                self.finished = true;
                return 0;
            };
            self.pos += 1;

            if byte == 0xFF {
                match self.data.get(self.pos) {
                    // A stuffed zero means a literal 0xFF byte.
                    Some(0x00) => self.pos += 1,
                    // Any other marker ends the entropy-coded data.
                    _ => {
                        self.pos -= 1;
                        self.finished = true;
                        return 0;
                    }
                }
            }
            self.current = byte as u32;
            self.remaining = 8;
        }
        self.remaining -= 1;
        (self.current >> self.remaining) & 1
    }

    /// Read `count` bits as an unsigned value.
    fn receive(&mut self, count: usize) -> Result<i32> {
        let mut value = 0i32;
        for _ in 0..count.min(16) {
            value = (value << 1) | self.bit() as i32;
        }
        Ok(value)
    }

    /// Decode one Huffman-coded value.
    fn huffman(&mut self, table: &HuffTable) -> Result<u8> {
        let mut code = 0i32;
        for length in 0..16 {
            code = (code << 1) | self.bit() as i32;
            if table.max_code[length] >= code && code >= table.min_code[length] {
                let offset = table.val_index[length] + (code - table.min_code[length]) as usize;
                return table
                    .values
                    .get(offset)
                    .copied()
                    .ok_or_else(|| err!("a Huffman code points outside its table"));
            }
        }
        // A stream that has run out feeds zeroes forever, which would otherwise
        // spin here on every remaining block.
        if self.finished {
            return Ok(0);
        }
        bail!("no Huffman code matches the bits in the stream")
    }

    /// Step over a restart marker and start reading on a byte boundary.
    fn restart(&mut self) {
        self.remaining = 0;
        // The marker may be immediately here, or a byte or two along if the
        // last entropy byte was padded.
        while self.pos + 1 < self.data.len() {
            if self.data[self.pos] == 0xFF && (0xD0..=0xD7).contains(&self.data[self.pos + 1]) {
                self.pos += 2;
                self.finished = false;
                return;
            }
            self.pos += 1;
        }
        self.finished = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 24×24 JPEG of four coloured quadrants, produced with `sips`.
    const COLOUR: &[u8] = include_bytes!("../tests/fixtures/cover.jpg");
    /// The same picture in greyscale, so the one-channel path is covered.
    const GREY: &[u8] = include_bytes!("../tests/fixtures/cover_grey.jpg");

    #[test]
    fn signatures_are_recognised() {
        assert!(is_jpeg(COLOUR));
        assert!(!is_png(COLOUR));
        assert!(is_png(b"\x89PNG\r\n\x1a\n and so on"));
        assert!(!is_jpeg(b"GIF89a"));
        assert!(!is_jpeg(b""));
    }

    #[test]
    fn a_real_jpeg_decodes_to_the_right_shape() {
        let image = decode(COLOUR).expect("the fixture should decode");
        assert_eq!((image.width, image.height), (24, 24));
        assert_eq!(image.pixels.len(), 24 * 24 * 3);
    }

    /// The fixture is red, green, blue and white quadrants. Exact values move
    /// around with the quantisation tables, so each corner is checked for the
    /// channel that should dominate rather than for a literal colour.
    #[test]
    fn colours_land_in_the_right_quadrants() {
        let image = decode(COLOUR).unwrap();
        let (r, g, b) = image.pixel(5, 5);
        assert!(
            r > 180 && g < 90 && b < 90,
            "top left should be red: {r},{g},{b}"
        );

        let (r, g, b) = image.pixel(18, 5);
        assert!(
            g > 130 && r < 110 && b < 110,
            "top right should be green: {r},{g},{b}"
        );

        let (r, g, b) = image.pixel(5, 18);
        assert!(
            b > 180 && r < 90 && g < 110,
            "bottom left should be blue: {r},{g},{b}"
        );

        let (r, g, b) = image.pixel(18, 18);
        assert!(
            r > 200 && g > 200 && b > 200,
            "bottom right should be white: {r},{g},{b}"
        );
    }

    #[test]
    fn a_greyscale_jpeg_decodes_to_neutral_pixels() {
        let image = decode(GREY).expect("the greyscale fixture should decode");
        assert_eq!((image.width, image.height), (24, 24));
        let (r, g, b) = image.pixel(5, 5);
        assert_eq!(
            (r, g),
            (g, b),
            "one channel must expand to grey: {r},{g},{b}"
        );
    }

    #[test]
    fn rubbish_is_refused_rather_than_guessed_at() {
        assert!(decode(b"").is_err());
        assert!(decode(b"not a jpeg at all").is_err());
        // A valid signature and nothing else.
        assert!(decode(&[0xFF, 0xD8, 0xFF]).is_err());
    }

    /// The decoder is fed by an untrusted mirror, so truncation at every offset
    /// has to end in an error or a picture — never a panic.
    #[test]
    fn truncation_at_any_point_is_survivable() {
        for cut in 0..COLOUR.len() {
            let _ = decode(&COLOUR[..cut]);
        }
    }

    /// Likewise for corruption: flip bytes throughout and require the same.
    #[test]
    fn corrupt_bytes_do_not_panic() {
        for i in (0..COLOUR.len()).step_by(3) {
            let mut broken = COLOUR.to_vec();
            broken[i] ^= 0xFF;
            let _ = decode(&broken);
        }
    }

    #[test]
    fn a_progressive_file_is_refused_by_name() {
        // Rewrite the SOF0 marker as SOF2 to make the fixture progressive.
        let mut progressive = COLOUR.to_vec();
        let sof = progressive
            .windows(2)
            .position(|w| w == [0xFF, 0xC0])
            .expect("the fixture is baseline");
        progressive[sof + 1] = 0xC2;

        let err = decode(&progressive).unwrap_err().to_string();
        assert!(err.contains("progressive"), "got: {err}");
    }

    #[test]
    fn absurd_dimensions_are_refused_before_allocating() {
        let mut huge = COLOUR.to_vec();
        let sof = huge
            .windows(2)
            .position(|w| w == [0xFF, 0xC0])
            .expect("the fixture is baseline");
        // Height and width live at offsets 5 and 7 past the marker.
        huge[sof + 5] = 0xFF;
        huge[sof + 6] = 0xFF;
        huge[sof + 7] = 0xFF;
        huge[sof + 8] = 0xFF;
        assert!(decode(&huge).is_err());
    }

    #[test]
    fn extend_recovers_signed_coefficients() {
        // The spec's own example: two bits, value 0 means -3.
        assert_eq!(extend(0, 2), -3);
        assert_eq!(extend(1, 2), -2);
        assert_eq!(extend(2, 2), 2);
        assert_eq!(extend(3, 2), 3);
        assert_eq!(extend(0, 0), 0);
    }

    #[test]
    fn a_flat_block_transforms_to_a_flat_field() {
        // Only the DC coefficient set: every output sample is the same.
        let mut block = [0i32; 64];
        block[0] = 8 * 16; // DC 16 after the 1/8 normalisation
        let mut out = [0u8; 64];
        idct(&block, &mut out);
        assert!(
            out.iter().all(|v| *v == out[0]),
            "a DC-only block is uniform, got {out:?}"
        );
        assert!(out[0] > 128, "a positive DC raises the level: {}", out[0]);
    }

    #[test]
    fn the_bit_reader_unstuffs_ff_bytes() {
        // 0xFF 0x00 encodes a literal 0xFF byte of coded data.
        let mut reader = BitReader::new(&[0xFF, 0x00, 0x0F]);
        let bits: Vec<u32> = (0..8).map(|_| reader.bit()).collect();
        assert_eq!(bits, [1, 1, 1, 1, 1, 1, 1, 1]);
        let next: Vec<u32> = (0..4).map(|_| reader.bit()).collect();
        assert_eq!(next, [0, 0, 0, 0]);
    }

    #[test]
    fn the_bit_reader_stops_at_a_marker() {
        let mut reader = BitReader::new(&[0xFF, 0xD9]);
        assert_eq!(reader.bit(), 0);
        assert!(reader.finished, "an end-of-image marker ends the stream");
    }

    #[test]
    fn running_off_the_end_yields_zeroes_not_a_panic() {
        let mut reader = BitReader::new(&[]);
        for _ in 0..100 {
            assert_eq!(reader.bit(), 0);
        }
        assert!(reader.finished);
    }
}
