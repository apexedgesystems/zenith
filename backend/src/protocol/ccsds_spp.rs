//! CCSDS Space Packet Protocol primary header (CCSDS 133.0-B-2).
//!
//! Zenith's second wire protocol and the structural proof that the
//! layers above the transport are protocol-blind. Field layout per
//! the Blue Book, cross-checked against the producing repo's SPP
//! library constants (6-octet header; version/type/secondary-flag/
//! APID-high in octet 0, APID-low in octet 1, sequence flags+count in
//! octets 2-3, packet-data-length-minus-one big-endian in octets
//! 4-5). Golden-vector byte pinning against that library follows the
//! compat/ pattern once the producer relays the vector set.

/// Primary header length in octets.
pub const HEADER_SIZE: usize = 6;
/// Ceiling on a whole packet, mirroring the reference library's bound.
pub const MAX_PACKET: usize = 4096;

/// Parsed primary header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SppHeader {
    pub version: u8,
    /// Packet type bit: true = telecommand, false = telemetry.
    pub is_command: bool,
    pub has_secondary_header: bool,
    pub apid: u16,
    pub seq_flags: u8,
    pub seq_count: u16,
    /// Actual data-field byte count (the wire carries this minus one).
    pub data_len: usize,
}

/// Parse a primary header from the first 6 octets. None when the
/// buffer is short, the version is not 0, or the declared data field
/// would exceed the packet ceiling -- the extractor uses those checks
/// to resynchronize a damaged stream.
pub fn parse_header(b: &[u8]) -> Option<SppHeader> {
    if b.len() < HEADER_SIZE {
        return None;
    }
    let version = (b[0] >> 5) & 0x07;
    if version != 0 {
        return None;
    }
    let data_len = (usize::from(b[4]) << 8 | usize::from(b[5])) + 1;
    if HEADER_SIZE + data_len > MAX_PACKET {
        return None;
    }
    Some(SppHeader {
        version,
        is_command: b[0] & 0x10 != 0,
        has_secondary_header: b[0] & 0x08 != 0,
        apid: (u16::from(b[0] & 0x07) << 8) | u16::from(b[1]),
        seq_flags: (b[2] >> 6) & 0x03,
        seq_count: (u16::from(b[2] & 0x3F) << 8) | u16::from(b[3]),
        data_len,
    })
}

/// Pack a telemetry packet (version 0, unsegmented). The fake-target
/// tests and any future command path share this one packer; compiled
/// for the full surface like parse_v3's verify side.
#[allow(dead_code)]
pub fn pack(apid: u16, seq_count: u16, payload: &[u8]) -> Vec<u8> {
    let apid = apid & 0x07FF;
    let seq = seq_count & 0x3FFF;
    let pdl = payload.len().saturating_sub(1) as u16;
    let mut out = Vec::with_capacity(HEADER_SIZE + payload.len());
    out.push(((apid >> 8) as u8) & 0x07);
    out.push((apid & 0xFF) as u8);
    // Sequence flags 0b11 = unsegmented, per the Blue Book default
    // for standalone packets.
    out.push(0xC0 | ((seq >> 8) as u8 & 0x3F));
    out.push((seq & 0xFF) as u8);
    out.push((pdl >> 8) as u8);
    out.push((pdl & 0xFF) as u8);
    out.extend_from_slice(payload);
    out
}

/// Streaming packet extractor. SPP has no sync marker: framing is
/// header-declared length over a byte stream. A damaged stream
/// resynchronizes by skipping forward one octet at a time until a
/// plausible header parses -- the same recovery discipline the
/// reference library's Processor applies.
#[derive(Default)]
pub struct Extractor {
    buf: Vec<u8>,
}

impl Extractor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed stream bytes; returns every complete packet now available
    /// as (header, data field bytes).
    ///
    /// Consumption is cursor-based with a single drain per feed:
    /// resync skips are O(1) each (a front-removal per garbage octet
    /// would go quadratic across a garbage run -- a corrupted or
    /// hostile stream must cost linear time, same standard the other
    /// parsers are held to). Residue is bounded by one incomplete
    /// packet (MAX_PACKET).
    pub fn feed(&mut self, data: &[u8]) -> Vec<(SppHeader, Vec<u8>)> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut pos = 0usize;
        while self.buf.len() - pos >= HEADER_SIZE {
            match parse_header(&self.buf[pos..]) {
                // Resync: skip one octet until a plausible header leads.
                None => pos += 1,
                Some(hdr) => {
                    let total = HEADER_SIZE + hdr.data_len;
                    if self.buf.len() - pos < total {
                        break;
                    }
                    out.push((hdr, self.buf[pos + HEADER_SIZE..pos + total].to_vec()));
                    pos += total;
                }
            }
        }
        if pos > 0 {
            self.buf.drain(..pos);
        }
        out
    }
}

/* ----------------------------- Tests ----------------------------- */

#[cfg(test)]
mod tests {
    use super::*;

    /// @test Header pack/parse round trips at field extremes: max
    /// APID (11 bits), max sequence count (14 bits), and 1-byte and
    /// max-size data fields.
    #[test]
    fn header_round_trip_at_extremes() {
        for (apid, seq, len) in [
            (0u16, 0u16, 1usize),
            (0x7FF, 0x3FFF, 8),
            (0x123, 0x2ABC, MAX_PACKET - HEADER_SIZE),
        ] {
            let payload = vec![0xA5u8; len];
            let pkt = pack(apid, seq, &payload);
            let hdr = parse_header(&pkt).unwrap();
            assert_eq!(hdr.apid, apid);
            assert_eq!(hdr.seq_count, seq);
            assert_eq!(hdr.data_len, len);
            assert_eq!(hdr.version, 0);
            assert!(!hdr.is_command);
            assert_eq!(hdr.seq_flags, 0b11);
        }
    }

    /// @test A nonzero version octet or an oversize declared length
    /// refuses to parse -- the validity checks resync depends on.
    #[test]
    fn implausible_headers_refuse() {
        let mut pkt = pack(1, 0, &[0; 4]);
        pkt[0] |= 0x20; // version 1
        assert!(parse_header(&pkt).is_none());

        let mut pkt = pack(1, 0, &[0; 4]);
        pkt[4] = 0xFF;
        pkt[5] = 0xFF; // declares 65536-byte data field
        assert!(parse_header(&pkt).is_none());
    }

    /// @test Packets split across arbitrary feed boundaries reassemble
    /// in order, one byte at a time included.
    #[test]
    fn extractor_reassembles_across_feeds() {
        let a = pack(0x100, 1, &[1, 2, 3, 4]);
        let b = pack(0x101, 2, &[5, 6]);
        let stream: Vec<u8> = a.iter().chain(b.iter()).copied().collect();

        let mut ex = Extractor::new();
        let mut got = Vec::new();
        for byte in stream {
            got.extend(ex.feed(&[byte]));
        }
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0.apid, 0x100);
        assert_eq!(got[0].1, vec![1, 2, 3, 4]);
        assert_eq!(got[1].0.apid, 0x101);
        assert_eq!(got[1].1, vec![5, 6]);
    }

    /// @test A garbage gap mid-stream is skipped and the following
    /// packet still extracts -- the resync behavior the producer's
    /// vector set will pin byte-for-byte.
    #[test]
    fn extractor_resyncs_past_garbage() {
        let good = pack(0x42, 7, &[9, 8, 7]);
        let mut stream = vec![0xFFu8; 11]; // implausible headers
        stream.extend_from_slice(&good);

        let mut ex = Extractor::new();
        let got = ex.feed(&stream);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0.apid, 0x42);
        assert_eq!(got[0].1, vec![9, 8, 7]);
    }
}
