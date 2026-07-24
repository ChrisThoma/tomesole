//! MD5 (RFC 1321), used purely as an integrity checksum.
//!
//! Libgen indexes every file by its MD5, which gives us a free end-to-end
//! integrity check: if the bytes that arrive do not hash to the digest the
//! catalogue advertised, we got the wrong file, a truncated file, or an
//! injected one, and we throw it away.
//!
//! To be explicit about what this is and is not: MD5 has been broken for
//! collision resistance since 2004, so this detects corruption, truncation and
//! opportunistic substitution — not a determined attacker who can craft a
//! collision. It is the strongest check the upstream catalogue makes available.
//! It is implemented here rather than pulled in as a dependency because the
//! algorithm is fixed, short, and verifiable against the published test
//! vectors (see the tests at the bottom of this file).

/// Per-round left-rotation amounts.
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// `K[i] = floor(2^32 * abs(sin(i + 1)))`, as published in RFC 1321.
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// Streaming MD5 state. Feed it with [`Md5::update`], finish with
/// [`Md5::finalize_hex`].
#[derive(Clone)]
pub struct Md5 {
    state: [u32; 4],
    /// Total message length in bytes, used for the length padding.
    length: u64,
    /// Partial block carried between `update` calls.
    buffer: [u8; 64],
    buffered: usize,
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

impl Md5 {
    pub fn new() -> Self {
        Self {
            state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
            length: 0,
            buffer: [0u8; 64],
            buffered: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);

        // Top up a partial block first.
        if self.buffered > 0 {
            let want = 64 - self.buffered;
            let take = want.min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered < 64 {
                // Input ran out before completing the block. Returning here is
                // load-bearing: the tail logic below would reset `buffered` and
                // silently discard what we have carried so far.
                debug_assert!(data.is_empty());
                return;
            }
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }

        // Then consume whole blocks straight out of the input.
        let mut chunks = data.chunks_exact(64);
        for block in &mut chunks {
            let block: &[u8; 64] = block.try_into().expect("chunks_exact yields 64 bytes");
            self.compress(block);
        }

        // Whatever is left is the new partial block.
        let rest = chunks.remainder();
        self.buffer[..rest.len()].copy_from_slice(rest);
        self.buffered = rest.len();
    }

    /// Consume the hasher and return the digest as 16 raw bytes.
    pub fn finalize(mut self) -> [u8; 16] {
        let bit_length = self.length.wrapping_mul(8);

        // Append 0x80, pad with zeros until 56 mod 64, then the length.
        self.update_padding(&[0x80]);
        while self.buffered != 56 {
            self.update_padding(&[0x00]);
        }
        self.update_padding(&bit_length.to_le_bytes());

        debug_assert_eq!(self.buffered, 0);

        let mut out = [0u8; 16];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        out
    }

    /// Consume the hasher and return the lowercase hex digest, which is the
    /// form Libgen publishes.
    pub fn finalize_hex(self) -> String {
        to_hex(&self.finalize())
    }

    /// Like `update`, but does not count towards the message length. Only used
    /// while emitting the padding.
    fn update_padding(&mut self, data: &[u8]) {
        for &byte in data {
            self.buffer[self.buffered] = byte;
            self.buffered += 1;
            if self.buffered == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut m = [0u32; 16];
        for (i, word) in m.iter_mut().enumerate() {
            *word = u32::from_le_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }

        let [mut a, mut b, mut c, mut d] = self.state;

        for i in 0..64 {
            let (mixed, g) = match i {
                0..=15 => ((b & c) | (!b & d), i),
                16..=31 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                32..=47 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let tmp = d;
            d = c;
            c = b;
            let sum = a.wrapping_add(mixed).wrapping_add(K[i]).wrapping_add(m[g]);
            b = b.wrapping_add(sum.rotate_left(SHIFTS[i]));
            a = tmp;
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
    }
}

/// Lowercase hex encoding.
pub fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0f) as usize] as char);
    }
    out
}

/// Convenience helper for one-shot hashing.
#[cfg(test)]
pub fn hex_digest(data: &[u8]) -> String {
    let mut h = Md5::new();
    h.update(data);
    h.finalize_hex()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seven test vectors published in RFC 1321, appendix A.5.
    #[test]
    fn matches_rfc1321_vectors() {
        let cases: [(&str, &str); 7] = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                // Eight repetitions of "1234567890", 80 bytes.
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(hex_digest(input.as_bytes()), expected, "input {input:?}");
        }
    }

    /// Feeding the same message in awkward chunk sizes must not change the
    /// digest — this is what the streaming download path actually does.
    #[test]
    fn streaming_matches_one_shot() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let one_shot = hex_digest(&data);

        for chunk in [1usize, 7, 63, 64, 65, 1000, 4096] {
            let mut h = Md5::new();
            for part in data.chunks(chunk) {
                h.update(part);
            }
            assert_eq!(h.finalize_hex(), one_shot, "chunk size {chunk}");
        }
    }

    /// Exercises the length-padding edge cases around the 56/64-byte boundary.
    #[test]
    fn handles_block_boundary_lengths() {
        for len in 0..200usize {
            let data = vec![b'x'; len];
            let mut h = Md5::new();
            h.update(&data);
            let digest = h.finalize_hex();
            assert_eq!(digest.len(), 32);
            assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        }
        // A known 64-byte (exactly one block) value.
        assert_eq!(hex_digest(&[b'a'; 64]), "014842d480b571495a4a0363793f7367");
        // A known 56-byte value, the padding boundary.
        assert_eq!(hex_digest(&[b'a'; 56]), "3b0c8ac703f828b04c6c197006d17218");
    }

    #[test]
    fn hex_encoding_is_lowercase_and_padded() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
        assert_eq!(to_hex(&[]), "");
    }
}
