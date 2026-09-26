//! CCSDS TM Space Data Link Protocol framing (CCSDS 132.0-B).
//!
//! Fixed-length transfer frames over a reliable byte stream: 6-octet
//! primary header, packet data field, 2-octet frame error control
//! field (CRC16-CCITT: poly 0x1021, init 0xFFFF, no reflection, no
//! final xor) computed over everything before it. The data field
//! carries space packets with idle-packet fill, so consecutive
//! verified data fields concatenate into a packet-aligned stream for
//! the SPP extractor downstream -- TM is a delimitation stage, SPP
//! stays the packet stage.
//!
//! No sync marker: framing rides the stream's own reliability
//! (TCP delivers bytes in order or not at all), so deframing is
//! strict fixed-size chunking, and a CRC failure costs exactly that
//! frame, never frame alignment. Packet alignment is the consumer's:
//! a packet may straddle frames, so each verified frame carries its
//! first-header pointer and a dropped frame is reported in sequence,
//! letting the packet stage discard the orphaned head and resume at
//! the next packet boundary.
//!
//! Frames are taken as header + data field + trailer: no secondary
//! header and no operational control field. A build that enables
//! either needs those lengths configured here first.

/// Primary header length in octets.
pub const HEADER_SIZE: usize = 6;
/// Frame error control field length in octets.
pub const TRAILER_SIZE: usize = 2;
/// Smallest sane fixed frame: header + one minimal (7-octet idle)
/// packet + trailer.
pub const MIN_FRAME: usize = HEADER_SIZE + 7 + TRAILER_SIZE;

/// CRC16-CCITT as the FECF specifies.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Streaming deframer for one fixed frame length.
pub struct Deframer {
    frame_size: usize,
    buf: Vec<u8>,
}

impl Deframer {
    pub fn new(frame_size: usize) -> Self {
        Self {
            frame_size: frame_size.max(MIN_FRAME),
            buf: Vec::new(),
        }
    }

    /// Feed stream bytes; returns each completed frame in order:
    /// verified ones with their packet data field and first-header
    /// pointer, CRC-failed ones as `Dropped` (whole -- their bytes
    /// never reach the stream).
    pub fn feed(&mut self, data: &[u8]) -> Vec<Frame> {
        self.buf.extend_from_slice(data);
        let mut frames = Vec::new();
        let mut pos = 0usize;
        while self.buf.len() - pos >= self.frame_size {
            let frame = &self.buf[pos..pos + self.frame_size];
            let stated =
                u16::from_be_bytes([frame[self.frame_size - 2], frame[self.frame_size - 1]]);
            if crc16(&frame[..self.frame_size - TRAILER_SIZE]) == stated {
                let field = &frame[HEADER_SIZE..self.frame_size - TRAILER_SIZE];
                let fhp = (u16::from_be_bytes([frame[4], frame[5]]) & 0x07FF) as usize;
                frames.push(Frame::Verified {
                    field: field.to_vec(),
                    // 0x7FF: no packet starts here; 0x7FE: idle only.
                    // Anything past the field is malformed and treated
                    // the same way.
                    first_header: (fhp < field.len()).then_some(fhp),
                });
            } else {
                frames.push(Frame::Dropped);
            }
            pos += self.frame_size;
        }
        if pos > 0 {
            self.buf.drain(..pos);
        }
        frames
    }
}

/// One deframed transfer frame, in stream order.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// CRC verified: the packet data field and the offset within it
    /// where the first packet header starts (None when no packet
    /// starts in this frame).
    Verified {
        field: Vec<u8>,
        first_header: Option<usize>,
    },
    /// CRC failed: the whole frame is gone.
    Dropped,
}

/// Pack one frame for tests and future downlink emitters: header
/// with the given spacecraft id and frame count, the content at
/// offset zero, standard-conformant idle-packet fill, CRC trailer.
/// Content must leave at least 7 octets for the idle packet or
/// exactly fill the field.
// The binary compiles this module directly and has no emitter yet;
// tests are its only caller for now.
#[cfg_attr(not(test), allow(dead_code))]
pub fn pack_frame(frame_size: usize, scid: u16, frame_count: u8, content: &[u8]) -> Vec<u8> {
    let field_len = frame_size - HEADER_SIZE - TRAILER_SIZE;
    assert!(content.len() == field_len || content.len() + 7 <= field_len);
    let mut frame = Vec::with_capacity(frame_size);
    let gvcid: u16 = (scid & 0x3FF) << 4;
    frame.extend_from_slice(&gvcid.to_be_bytes());
    frame.push(frame_count);
    frame.push(frame_count);
    // Data field status: segment length id 0b11, first header
    // pointer 0 (content starts at offset zero).
    frame.extend_from_slice(&0x1800u16.to_be_bytes());
    frame.extend_from_slice(content);
    let fill = field_len - content.len();
    if fill > 0 {
        // Idle packet: APID 0x7FF, unsegmented, length covering the
        // remaining fill.
        let idle_payload = fill - super::ccsds_spp::HEADER_SIZE;
        frame.push(0x07);
        frame.push(0xFF);
        frame.extend_from_slice(&0xC000u16.to_be_bytes());
        frame.extend_from_slice(&((idle_payload - 1) as u16).to_be_bytes());
        frame.resize(frame_size - TRAILER_SIZE, 0);
    }
    let crc = crc16(&frame);
    frame.extend_from_slice(&crc.to_be_bytes());
    frame
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    /// @test CRC16-CCITT against the published check value: "123456789"
    /// -> 0x29B1 (poly 0x1021, init 0xFFFF, no reflect, no xorout).
    #[test]
    fn crc16_matches_published_check_value() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    /// @test Frames split across arbitrary feed boundaries reassemble;
    /// each verified data field comes back whole and idle fill stays
    /// inside it (the downstream packet stage owns idle handling).
    #[test]
    fn deframer_reassembles_across_feeds() {
        let content = crate::protocol::ccsds_spp::pack(0x0D0, 1, &[9, 8, 7]);
        let frame = pack_frame(64, 0x044, 0, &content);
        let mut d = Deframer::new(64);
        let mut fields = Vec::new();
        for byte in &frame {
            for f in d.feed(&[*byte]) {
                match f {
                    Frame::Verified {
                        field,
                        first_header,
                    } => {
                        assert_eq!(first_header, Some(0), "content packed at offset zero");
                        fields.push(field);
                    }
                    Frame::Dropped => panic!("clean frame dropped"),
                }
            }
        }
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].len(), 64 - HEADER_SIZE - TRAILER_SIZE);
        assert_eq!(&fields[0][..content.len()], &content[..]);
    }

    /// @test A corrupted frame is dropped whole -- its packets never
    /// reach the stream -- and the following frame still verifies:
    /// fixed-size chunking never loses alignment to a bad frame.
    #[test]
    fn corrupt_frame_drops_whole_and_stream_realigns() {
        let a = pack_frame(
            64,
            0x044,
            0,
            &crate::protocol::ccsds_spp::pack(0x0D0, 1, &[1]),
        );
        let mut b = pack_frame(
            64,
            0x044,
            1,
            &crate::protocol::ccsds_spp::pack(0x0D0, 2, &[2]),
        );
        let c = pack_frame(
            64,
            0x044,
            2,
            &crate::protocol::ccsds_spp::pack(0x0D0, 3, &[3]),
        );
        b[10] ^= 0xFF; // corrupt inside the data field

        let mut d = Deframer::new(64);
        let mut stream = a.clone();
        stream.extend_from_slice(&b);
        stream.extend_from_slice(&c);
        let frames = d.feed(&stream);
        assert_eq!(frames.len(), 3, "every frame is reported, in order");
        assert_eq!(frames[1], Frame::Dropped);
        let field = |i: usize| match &frames[i] {
            Frame::Verified { field, .. } => field.clone(),
            Frame::Dropped => panic!("frame {i} should verify"),
        };
        assert_eq!(&field(0)[..7], &a[HEADER_SIZE..HEADER_SIZE + 7]);
        assert_eq!(&field(2)[..7], &c[HEADER_SIZE..HEADER_SIZE + 7]);
    }

    /// @test The first-header pointer comes out of the header as the
    /// packet stage needs it: an offset inside the data field, or
    /// None for the two reserved values (no packet starts here; idle
    /// only) and for a pointer past the field.
    #[test]
    fn first_header_pointer_is_reported() {
        let content = crate::protocol::ccsds_spp::pack(0x0D0, 1, &[1]);
        for (fhp, expect) in [
            (0u16, Some(0usize)),
            (5, Some(5)),
            (0x7FF, None),
            (0x7FE, None),
            (64, None),
        ] {
            let mut frame = pack_frame(64, 0x044, 0, &content);
            let status = 0x1800u16 | (fhp & 0x07FF);
            frame[4..6].copy_from_slice(&status.to_be_bytes());
            let crc = crc16(&frame[..64 - TRAILER_SIZE]);
            frame[62..64].copy_from_slice(&crc.to_be_bytes());
            let frames = Deframer::new(64).feed(&frame);
            assert_eq!(
                frames,
                vec![Frame::Verified {
                    field: frame[HEADER_SIZE..64 - TRAILER_SIZE].to_vec(),
                    first_header: expect,
                }],
                "fhp {fhp:#x}"
            );
        }
    }
}
