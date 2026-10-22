//! Streaming base64 decoder for kitty graphics payloads (WIRE-4).
//!
//! kitty decodes each chunk of a chunked upload on its own, so every chunk
//! may carry its own `=` padding (`m=1;AQIDBA==` then `m=0;BQY=`). toastty
//! also keeps two leniencies kitty doesn't have:
//!
//! - whitespace anywhere is skipped (some clients wrap lines);
//! - a chunk that ends mid-quantum without padding carries the leftover
//!   characters into the next chunk, so splitting the encoded text at any
//!   byte works.
//!
//! Like kitty, decoding a chunk stops at its first byte that is neither
//! base64, padding nor whitespace; the bytes before it are kept.
//!
//! Decoding chunk by chunk also means an upload's buffer holds decoded
//! bytes, so the size caps count decoded bytes rather than base64 (WIRE-11).

/// Table value for `=`.
const PAD: u8 = 64;
/// Table value for ASCII whitespace.
const SPACE: u8 = 65;
/// Table value for anything else.
const BAD: u8 = 255;

const TABLE: [u8; 256] = {
    let mut t = [BAD; 256];
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut i = 0;
    while i < 64 {
        t[alphabet[i] as usize] = i as u8;
        i += 1;
    }
    t[b'=' as usize] = PAD;
    t[b' ' as usize] = SPACE;
    t[b'\t' as usize] = SPACE;
    t[b'\n' as usize] = SPACE;
    t[b'\r' as usize] = SPACE;
    t[0x0c] = SPACE;
    t
};

/// Decoder state carried between chunks: up to three sextets of an
/// unfinished quantum.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Decoder {
    acc: u32,
    /// Sextets in `acc` (0..=3).
    n: u8,
}

impl Decoder {
    /// Decode one chunk, appending to `out`. Decoding stops at the first
    /// byte that is neither base64, padding nor whitespace, keeping what
    /// came before it (kitty: `/wAA*w==` gives 3 bytes); the partial
    /// quantum is dropped. Returns `false` if the chunk was cut short.
    pub(super) fn feed(&mut self, chunk: &[u8], out: &mut Vec<u8>) -> bool {
        out.reserve(chunk.len() / 4 * 3 + 3);
        for &b in chunk {
            match TABLE[b as usize] {
                BAD => {
                    self.acc = 0;
                    self.n = 0;
                    return false;
                }
                SPACE => {}
                PAD => self.flush(out),
                v => {
                    self.acc = (self.acc << 6) | u32::from(v);
                    self.n += 1;
                    if self.n == 4 {
                        let a = self.acc;
                        out.extend_from_slice(&[(a >> 16) as u8, (a >> 8) as u8, a as u8]);
                        self.acc = 0;
                        self.n = 0;
                    }
                }
            }
        }
        true
    }

    /// Sextets of an unfinished quantum carried into the next chunk.
    pub(super) fn carried(self) -> u8 {
        self.n
    }

    /// End of data: emit what an unpadded final quantum holds.
    pub(super) fn finish(&mut self, out: &mut Vec<u8>) {
        self.flush(out);
    }

    /// Close the current quantum, as `=` padding does: two sextets give
    /// one byte, three give two; one sextet carries no whole byte.
    fn flush(&mut self, out: &mut Vec<u8>) {
        let a = self.acc;
        match self.n {
            2 => out.push((a >> 4) as u8),
            3 => out.extend_from_slice(&[(a >> 10) as u8, (a >> 2) as u8]),
            _ => {}
        }
        self.acc = 0;
        self.n = 0;
    }
}

/// An upper bound on what [`Decoder::feed`] can append for `chunk`, given
/// the `carried` sextets of a partial quantum from earlier chunks: three
/// bytes per four base64 characters, padding and whitespace not counted.
/// Counting is cheap and allocates nothing, so callers check their cap
/// with it *before* decoding.
pub(super) fn decoded_len_bound(chunk: &[u8], carried: u8) -> u64 {
    let chars = chunk.iter().filter(|&&b| TABLE[b as usize] < PAD).count() as u64;
    (chars + u64::from(carried)) * 3 / 4
}

/// Decode a whole payload in one go (single-chunk transmissions, paths).
pub(super) fn decode_all(input: &[u8]) -> Vec<u8> {
    let mut d = Decoder::default();
    let mut out = Vec::new();
    if !d.feed(input, &mut out) {
        tracing::debug!("kitty: base64 payload cut short at a bad byte");
    }
    d.finish(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn enc(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    fn chunks(parts: &[&str]) -> Vec<u8> {
        let mut d = Decoder::default();
        let mut out = Vec::new();
        for p in parts {
            d.feed(p.as_bytes(), &mut out);
        }
        d.finish(&mut out);
        out
    }

    #[test]
    fn matches_the_standard_decoder() {
        for len in 0..40 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(decode_all(enc(&data).as_bytes()), data, "len {len}");
            let unpadded = enc(&data).trim_end_matches('=').to_string();
            assert_eq!(decode_all(unpadded.as_bytes()), data, "unpadded len {len}");
        }
    }

    /// WIRE-4: each chunk padded on its own, as kitty accepts.
    #[test]
    fn per_chunk_padding() {
        assert_eq!(chunks(&["AQIDBA==", "BQY="]), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(
            chunks(&[&enc(&[0; 5]), &enc(&[1; 3])]),
            vec![0, 0, 0, 0, 0, 1, 1, 1]
        );
        assert_eq!(
            chunks(&["AQIDBA==", "", "BQYHCA=="]),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    /// toastty's leniency: a split anywhere in the encoded text works.
    #[test]
    fn split_mid_quantum_carries_over() {
        let full = enc(&[1, 2, 3, 4, 5, 6, 7, 8]);
        for at in 0..=full.len() {
            let (a, b) = full.split_at(at);
            assert_eq!(
                chunks(&[a, b]),
                vec![1, 2, 3, 4, 5, 6, 7, 8],
                "split at {at}"
            );
        }
    }

    #[test]
    fn whitespace_is_skipped() {
        assert_eq!(decode_all(b"AQ ID\nBA\r\n=="), vec![1, 2, 3, 4]);
    }

    /// Like kitty, decoding stops at a junk byte and keeps the prefix.
    #[test]
    fn junk_byte_ends_the_chunk() {
        assert_eq!(chunks(&["/wAA", "!!!!/w=="]), vec![255, 0, 0]);
        assert!(decode_all(b"!!!!").is_empty());
        assert_eq!(decode_all(b"/wAA*w=="), vec![255, 0, 0]);
        assert_eq!(chunks(&["/wAA", "/w=!"]), vec![255, 0, 0, 255]);
    }

    #[test]
    fn length_bound_is_an_upper_bound() {
        for len in 0..40 {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let e = enc(&data);
            assert_eq!(
                decoded_len_bound(e.as_bytes(), 0),
                data.len() as u64,
                "len {len}"
            );
            let wrapped = format!("{e}\n  \n");
            assert_eq!(decoded_len_bound(wrapped.as_bytes(), 0), data.len() as u64);
            // Split mid-quantum: the carried sextets count.
            if e.len() > 5 {
                let mut d = Decoder::default();
                let mut out = Vec::new();
                d.feed(&e.as_bytes()[..5], &mut out);
                let rest = decoded_len_bound(&e.as_bytes()[5..], d.carried());
                assert!(out.len() as u64 + rest >= data.len() as u64);
            }
        }
    }

    #[test]
    fn stray_padding_is_harmless() {
        assert_eq!(decode_all(b"AQIDBA====BQY="), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(decode_all(b"===="), Vec::<u8>::new());
    }
}
