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
    ///
    /// Every data byte, escaped or not, enters the frame through one
    /// path: only while a frame is open, and never past MAX_FRAME_SIZE,
    /// where the frame is dropped and the decoder waits for the next
    /// END. Bytes outside a frame are discarded, whatever their
    /// encoding.
    pub fn feed(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();

        for &b in data {
            let byte = if self.escape {
                self.escape = false;
                match b {
                    ESC_END => END,
                    ESC_ESC => ESC,
                    other => other, // Protocol violation, accept anyway
                }
            } else {
                match b {
                    END => {
                        if self.in_frame && !self.buf.is_empty() {
                            frames.push(std::mem::take(&mut self.buf));
                        }
                        self.buf.clear();
                        self.in_frame = true;
                        continue;
                    }
                    ESC => {
                        self.escape = true;
                        continue;
                    }
                    other => other,
                }
            };
            if self.in_frame {
                self.buf.push(byte);
                if self.buf.len() > MAX_FRAME_SIZE {
                    tracing::warn!("SLIP frame exceeds {} bytes, dropping", MAX_FRAME_SIZE);
                    self.buf.clear();
                    self.in_frame = false;
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

    /// @test The frame-size bound holds for escaped bytes too: a frame
    /// of MAX_FRAME_SIZE + 1 escaped bytes is dropped as it passes the
    /// bound, the decoder never holds more than the bound, and the next
    /// frame decodes. Escaped bytes outside any frame are dropped like
    /// plain ones, so a peer streaming escapes without an END holds no
    /// memory.
    #[test]
    fn escaped_bytes_respect_the_frame_bound() {
        let mut d = Decoder::new();
        let mut stream = vec![END];
        for _ in 0..=MAX_FRAME_SIZE {
            stream.extend_from_slice(&[ESC, ESC_ESC]);
        }
        assert!(d.feed(&stream).is_empty());
        assert!(d.buf.len() <= MAX_FRAME_SIZE, "{} bytes held", d.buf.len());
        let mut tail = vec![END];
        tail.extend_from_slice(&encode(b"ok"));
        assert_eq!(d.feed(&tail), vec![b"ok".to_vec()]);

        let mut outside = Decoder::new();
        let escapes: Vec<u8> = std::iter::repeat_n([ESC, ESC_END], 1 << 16)
            .flatten()
            .collect();
        assert!(outside.feed(&escapes).is_empty());
        assert!(
            outside.buf.is_empty(),
            "{} bytes held outside a frame",
            outside.buf.len()
        );
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

    /// @test A frame decodes to its original bytes, the escaped END and
    /// ESC included, however the stream is split: fed one byte at a time
    /// it yields the same single frame as one bulk feed, which covers a
    /// frame split across reads and an escape whose ESC ends one read.
    /// An ESC followed by a byte that is neither escape code is a
    /// protocol violation the decoder tolerates: the byte is kept.
    #[test]
    fn byte_by_byte_feed_matches_bulk_feed() {
        let mut data = b"the quick brown fox ".to_vec();
        data.extend_from_slice(&[END, ESC, 0x03, ESC, END]);
        let encoded = encode(&data);
        assert!(encoded.contains(&ESC_END) && encoded.contains(&ESC_ESC));

        let mut bulk = Decoder::new();
        let bulk_frames = bulk.feed(&encoded);

        let mut byte_dec = Decoder::new();
        let mut byte_frames = Vec::new();
        for &b in &encoded {
            byte_frames.extend(byte_dec.feed(&[b]));
        }

        assert_eq!(bulk_frames, vec![data]);
        assert_eq!(byte_frames, bulk_frames);

        let mut tolerant = Decoder::new();
        assert_eq!(
            tolerant.feed(&[END, 0x01, ESC, 0x42, END]),
            vec![vec![0x01, 0x42]]
        );
    }
}
