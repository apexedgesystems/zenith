//! SLIP framing (RFC 1055).
//!
//! Encodes/decodes byte frames with END/ESC delimiters for reliable
//! packet boundary detection over a byte stream (TCP).

/* ----------------------------- Constants ----------------------------- */

const END: u8 = 0xC0;
const ESC: u8 = 0xDB;
const ESC_END: u8 = 0xDC;
const ESC_ESC: u8 = 0xDD;

/// Maximum allowed frame size (64 KB). Frames exceeding this are dropped
/// to prevent unbounded memory growth from malformed streams.
const MAX_FRAME_SIZE: usize = 65536;

/* ----------------------------- Encode ----------------------------- */

/// SLIP-encode a frame with END delimiters.
pub fn encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 8);
    out.push(END);
    for &b in data {
        match b {
            END => {
                out.push(ESC);
                out.push(ESC_END);
            }
            ESC => {
                out.push(ESC);
                out.push(ESC_ESC);
            }
            _ => out.push(b),
        }
    }
    out.push(END);
    out
}

/* ----------------------------- Decode ----------------------------- */

/// Streaming SLIP decoder.
///
/// Feeds bytes into the decoder and extracts complete frames.
/// Maintains internal state across calls for partial frame handling.
pub struct Decoder {
    buf: Vec<u8>,
    in_frame: bool,
    escape: bool,
}

impl Decoder {
    /// Create a fresh decoder with no in-flight frame state. Pre-allocates
    /// 8 KB for the assembly buffer; grows as needed up to MAX_FRAME_SIZE.
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(8192),
            in_frame: false,
            escape: false,
        }
    }

    /// Feed raw bytes from the socket. Returns decoded frames.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();

        for &b in data {
            if self.escape {
                self.escape = false;
                match b {
                    ESC_END => self.buf.push(END),
                    ESC_ESC => self.buf.push(ESC),
                    _ => self.buf.push(b), // Protocol violation, accept anyway
                }
                continue;
            }

            match b {
                END => {
                    if self.in_frame && !self.buf.is_empty() {
                        frames.push(std::mem::take(&mut self.buf));
                    }
                    self.buf.clear();
                    self.in_frame = true;
                }
                ESC => {
                    self.escape = true;
                }
                _ => {
                    if self.in_frame {
                        self.buf.push(b);
                        if self.buf.len() > MAX_FRAME_SIZE {
                            tracing::warn!("SLIP frame exceeds {} bytes, dropping", MAX_FRAME_SIZE);
                            self.buf.clear();
                            self.in_frame = false;
                        }
                    }
                }
            }
        }

        frames
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// @test SLIP encode followed by decode produces the original bytes,
    /// including the special END and ESC bytes that need escaping.
    #[test]
    fn round_trip() {
        let data = vec![0x01, 0x02, END, ESC, 0x03];
        let encoded = encode(&data);
        let mut decoder = Decoder::new();
        let frames = decoder.feed(&encoded);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], data);
    }

    /// @test Two encoded frames concatenated into a single stream
    /// decode as two separate frames.
    #[test]
    fn multiple_frames() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&encode(b"hello"));
        stream.extend_from_slice(&encode(b"world"));

        let mut decoder = Decoder::new();
        let frames = decoder.feed(&stream);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], b"hello");
        assert_eq!(frames[1], b"world");
    }

    /// @test Decoder handles a frame split across two feed() calls
    /// (the realistic case for TCP socket reads).
    #[test]
    fn partial_feed() {
        let encoded = encode(b"test");
        let (a, b) = encoded.split_at(3);

        let mut decoder = Decoder::new();
        let frames = decoder.feed(a);
        assert!(frames.is_empty());

        let frames = decoder.feed(b);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], b"test");
    }

    /// @test ESC byte at end of one feed and ESC_END at start of the
    /// next decodes correctly. The decoder must remember escape state
    /// across feed() calls.
    #[test]
    fn escape_across_feed_boundary() {
        // ESC byte at end of first feed, ESC_END at start of second feed.
        // Decoder must remember the escape state across calls.
        let data = vec![0x42, END, 0x99]; // contains an END that needs escaping
        let encoded = encode(&data);
        // Find the ESC byte and split there
        let esc_idx = encoded.iter().position(|&b| b == ESC).unwrap();
        let (a, b) = encoded.split_at(esc_idx + 1); // split AFTER the ESC

        let mut decoder = Decoder::new();
        assert!(decoder.feed(a).is_empty());
        let frames = decoder.feed(b);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], data);
    }

    /// @test Frames larger than MAX_FRAME_SIZE are dropped and the
    /// decoder recovers state to process the next valid frame.
    #[test]
    fn oversized_frame_dropped() {
        // Build a frame larger than MAX_FRAME_SIZE; verify it's dropped
        // and the decoder recovers for the next valid frame.
        let mut stream = vec![END];
        stream.extend(std::iter::repeat_n(0xAA, MAX_FRAME_SIZE + 100));
        stream.push(END);
        // Append a valid frame after
        stream.extend_from_slice(&encode(b"after"));

        let mut decoder = Decoder::new();
        let frames = decoder.feed(&stream);
        // The oversized frame must be dropped and decoder recovers
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], b"after");
    }

    /// @test Two consecutive END bytes (an empty frame) are not
    /// emitted as a frame.
    #[test]
    fn empty_frame_skipped() {
        // Two consecutive END bytes form an empty frame -- decoder should skip it
        let stream = vec![END, END, END];
        let mut decoder = Decoder::new();
        assert!(decoder.feed(&stream).is_empty());
    }

    /// @test Feeding the same encoded payload one byte at a time
    /// produces the same frames as a single bulk feed -- proves the
    /// streaming decoder maintains correct state at every boundary.
    #[test]
    fn byte_by_byte_feed_matches_bulk_feed() {
        // Feeding the same encoded payload in 1-byte chunks must produce
        // the same output as a single bulk feed.
        let data = b"the quick brown fox jumps over the lazy dog";
        let encoded = encode(data);

        let mut bulk = Decoder::new();
        let bulk_frames = bulk.feed(&encoded);

        let mut byte_dec = Decoder::new();
        let mut byte_frames = Vec::new();
        for &b in &encoded {
            byte_frames.extend(byte_dec.feed(&[b]));
        }

        assert_eq!(bulk_frames, byte_frames);
        assert_eq!(bulk_frames.len(), 1);
        assert_eq!(bulk_frames[0], data);
    }

    /// @test The decoder never panics on arbitrary byte streams fed in
    /// irregular chunk sizes, and every emitted frame survives the
    /// APROTO parser. Both consume untrusted network input, so this is
    /// a deterministic pseudo-random adversarial sweep (fixed seed:
    /// reproducible in CI).
    #[test]
    fn decoder_survives_arbitrary_byte_streams() {
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next_byte = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u8
        };

        for round in 0..200usize {
            let mut decoder = Decoder::new();
            let len = (round * 37) % 4096 + 1;
            let bytes: Vec<u8> = (0..len).map(|_| next_byte()).collect();
            for chunk in bytes.chunks(1 + round % 61) {
                for frame in decoder.feed(chunk) {
                    let _ = crate::protocol::aproto::parse_packet(&frame);
                }
            }
        }
    }
}
