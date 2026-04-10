//! APROTO packet encoding and decoding.
//!
//! Wire format: 14-byte little-endian header + variable payload.
//!
//! Header layout:
//!   Offset  Size  Field
//!   0       2     magic          (0x5041 "PA")
//!   2       1     version        (1)
//!   3       1     flags          (bitfield)
//!   4       4     fullUid        (component address)
//!   8       2     opcode
//!   10      2     sequence
//!   12      2     payloadLength
//!
//! This module is the canonical Rust mirror of apex's `AprotoTypes.hpp`.
//! Many constants below are part of the apex protocol surface but not
//! currently consumed by zenith -- the module-level `allow(dead_code)`
//! keeps the full protocol definition compiled for documentation and
//! future use.

#![allow(dead_code)]

use serde::Serialize;

/* ----------------------------- Constants ----------------------------- */

/// APROTO magic bytes ("PA" little-endian) at the start of every header.
pub const MAGIC: u16 = 0x5041;
/// Wire-protocol version supported by this client.
pub const VERSION: u8 = 1;
/// Fixed header length in bytes.
pub const HEADER_SIZE: usize = 14;

// Flag bits

/// Flag bit: internal/system-generated packet.
pub const FLAG_INTERNAL: u8 = 0x01;
/// Flag bit: this packet is a response (ACK / NAK / push telemetry).
pub const FLAG_RESPONSE: u8 = 0x02;
/// Flag bit: command requests an ACK from the receiver.
pub const FLAG_ACK_REQ: u8 = 0x04;
/// Flag bit: payload includes a CRC trailer (not currently used by zenith).
pub const FLAG_CRC: u8 = 0x08;
/// Flag bit: payload is encrypted (not currently used by zenith).
pub const FLAG_ENCRYPTED: u8 = 0x10;

// System opcodes

/// System opcode: no-op, used as a connectivity check.
pub const SYS_NOOP: u16 = 0x0000;
/// System opcode: ping, returns a pong.
pub const SYS_PING: u16 = 0x0001;
/// System opcode: positive acknowledgement of a command.
pub const SYS_ACK: u16 = 0x00FE;
/// System opcode: negative acknowledgement of a command.
pub const SYS_NAK: u16 = 0x00FF;

// File transfer opcodes

/// File transfer opcode: begin a file upload (zenith -> apex).
pub const FILE_BEGIN: u16 = 0x0020;
/// File transfer opcode: a single data chunk of an in-flight upload.
pub const FILE_CHUNK: u16 = 0x0021;
/// File transfer opcode: finalize the upload, apex verifies CRC.
pub const FILE_END: u16 = 0x0022;
/// File transfer opcode: abort an in-flight upload.
pub const FILE_ABORT: u16 = 0x0023;
/// File transfer opcode: query current transfer state.
pub const FILE_STATUS: u16 = 0x0024;

// Executive opcodes

/// Executive opcode: GET_HEALTH, returns the executive health struct.
pub const EXEC_GET_HEALTH: u16 = 0x0100;
/// Executive opcode: GET_CLOCK_CYCLES, returns the elapsed clock count.
pub const EXEC_GET_CLOCK_CYCLES: u16 = 0x0104;
/// Executive opcode: CMD_PAUSE, halts the scheduler.
pub const EXEC_CMD_PAUSE: u16 = 0x0110;
/// Executive opcode: CMD_RESUME, resumes a paused scheduler.
pub const EXEC_CMD_RESUME: u16 = 0x0111;
/// Executive opcode: CMD_SHUTDOWN, gracefully exits the apex process.
pub const EXEC_CMD_SHUTDOWN: u16 = 0x0112;
/// Executive opcode: CMD_SLEEP, puts the system into idle mode.
pub const EXEC_CMD_SLEEP: u16 = 0x0116;
/// Executive opcode: CMD_WAKE, returns the system to active mode.
pub const EXEC_CMD_WAKE: u16 = 0x0117;
/// Executive opcode: INSPECT, reads a registered data block.
pub const EXEC_INSPECT: u16 = 0x0130;

// NAK status codes

/// NAK status code: SUCCESS (also returned in ACK).
pub const NAK_SUCCESS: u8 = 0;
/// NAK status code: opcode not recognized by the receiving component.
pub const NAK_UNKNOWN_OPCODE: u8 = 1;
/// NAK status code: payload format or length is invalid.
pub const NAK_INVALID_PAYLOAD: u8 = 2;
/// NAK status code: target component (by fullUid) is not registered.
pub const NAK_COMPONENT_NOT_FOUND: u8 = 4;

/* ----------------------------- Header ----------------------------- */

/// Parsed APROTO header.
#[derive(Debug, Clone, Serialize)]
pub struct Header {
    pub magic: u16,
    pub version: u8,
    pub flags: u8,
    pub full_uid: u32,
    pub opcode: u16,
    pub sequence: u16,
    pub payload_length: u16,
}

impl Header {
    /// Check if this is a response packet (ACK/NAK or telemetry).
    pub fn is_response(&self) -> bool {
        self.flags & FLAG_RESPONSE != 0
    }

    /// Check if ACK was requested.
    pub fn ack_requested(&self) -> bool {
        self.flags & FLAG_ACK_REQ != 0
    }
}

/* ----------------------------- Parsed Packet ----------------------------- */

/// Fully parsed APROTO packet (header + payload).
#[derive(Debug, Clone)]
pub struct Packet {
    pub header: Header,
    pub payload: Vec<u8>,
}

/* ----------------------------- ACK/NAK Response ----------------------------- */

/// Parsed ACK/NAK response payload.
#[derive(Debug, Clone, Serialize)]
pub struct AckResponse {
    pub cmd_opcode: u16,
    pub cmd_sequence: u16,
    pub status: u8,
    pub status_name: String,
    /// Extra data after the 8-byte ACK header (response payload from handleCommand).
    pub extra: Vec<u8>,
}

/* ----------------------------- Encoding ----------------------------- */

/// Build an APROTO command packet (header + payload).
pub fn build_command(full_uid: u32, opcode: u16, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let flags = FLAG_ACK_REQ;
    let mut buf = Vec::with_capacity(HEADER_SIZE + payload.len());

    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.push(VERSION);
    buf.push(flags);
    buf.extend_from_slice(&full_uid.to_le_bytes());
    buf.extend_from_slice(&opcode.to_le_bytes());
    buf.extend_from_slice(&sequence.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    buf.extend_from_slice(payload);

    buf
}

/* ----------------------------- Decoding ----------------------------- */

/// Parse an APROTO header from raw bytes. Returns None if invalid.
pub fn parse_header(data: &[u8]) -> Option<Header> {
    if data.len() < HEADER_SIZE {
        return None;
    }

    let magic = u16::from_le_bytes([data[0], data[1]]);
    if magic != MAGIC {
        return None;
    }

    Some(Header {
        magic,
        version: data[2],
        flags: data[3],
        full_uid: u32::from_le_bytes([data[4], data[5], data[6], data[7]]),
        opcode: u16::from_le_bytes([data[8], data[9]]),
        sequence: u16::from_le_bytes([data[10], data[11]]),
        payload_length: u16::from_le_bytes([data[12], data[13]]),
    })
}

/// Parse a complete APROTO packet (header + payload).
pub fn parse_packet(data: &[u8]) -> Option<Packet> {
    let header = parse_header(data)?;
    let payload_start = HEADER_SIZE;
    let payload_end = payload_start + header.payload_length as usize;

    if data.len() < payload_end {
        return None;
    }

    Some(Packet {
        header,
        payload: data[payload_start..payload_end].to_vec(),
    })
}

/// Parse an ACK/NAK payload (8+ bytes).
pub fn parse_ack(payload: &[u8]) -> Option<AckResponse> {
    if payload.len() < 8 {
        return None;
    }

    let cmd_opcode = u16::from_le_bytes([payload[0], payload[1]]);
    let cmd_sequence = u16::from_le_bytes([payload[2], payload[3]]);
    let status = payload[4];
    let extra = if payload.len() > 8 {
        payload[8..].to_vec()
    } else {
        Vec::new()
    };

    let status_name = match status {
        0 => "SUCCESS",
        1 => "UNKNOWN_OPCODE",
        2 => "INVALID_PAYLOAD",
        3 => "NO_RESOLVER",
        4 => "COMPONENT_NOT_FOUND",
        5 => "LOAD_FAILED",
        _ => "UNKNOWN",
    }
    .to_string();

    Some(AckResponse {
        cmd_opcode,
        cmd_sequence,
        status,
        status_name,
        extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// @test Building a command packet then parsing it back recovers
    /// every header field exactly.
    #[test]
    fn build_and_parse_round_trip() {
        let packet = build_command(0x00D000, SYS_NOOP, 42, &[]);
        let parsed = parse_packet(&packet).unwrap();
        assert_eq!(parsed.header.magic, MAGIC);
        assert_eq!(parsed.header.full_uid, 0x00D000);
        assert_eq!(parsed.header.opcode, SYS_NOOP);
        assert_eq!(parsed.header.sequence, 42);
        assert!(parsed.payload.is_empty());
        assert!(parsed.header.ack_requested());
    }

    /// @test A command built with a non-empty payload reports the
    /// correct payload_length and round-trips the bytes.
    #[test]
    fn build_with_payload() {
        let payload = vec![0x01, 0x02, 0x03];
        let packet = build_command(0x000000, EXEC_GET_HEALTH, 1, &payload);
        let parsed = parse_packet(&packet).unwrap();
        assert_eq!(parsed.header.payload_length, 3);
        assert_eq!(parsed.payload, payload);
    }

    /// @test ACK payload with extra data parses cmd_opcode, sequence,
    /// status, status_name, and the trailing extra bytes.
    #[test]
    fn parse_ack_response() {
        // Simulated ACK: cmdOpcode=0x0000, cmdSeq=42, status=0, reserved=0, extra=[0xFF]
        let ack_data = vec![0x00, 0x00, 42, 0, 0, 0, 0, 0, 0xFF];
        let ack = parse_ack(&ack_data).unwrap();
        assert_eq!(ack.cmd_opcode, 0x0000);
        assert_eq!(ack.cmd_sequence, 42);
        assert_eq!(ack.status, 0);
        assert_eq!(ack.status_name, "SUCCESS");
        assert_eq!(ack.extra, vec![0xFF]);
    }

    /// @test parse_header returns None for buffers shorter than the
    /// 14-byte header length.
    #[test]
    fn parse_header_rejects_short_buffer() {
        assert!(parse_header(&[]).is_none());
        assert!(parse_header(&[0x41, 0x50, 0x01, 0x00]).is_none()); // 4 bytes < 14
    }

    /// @test parse_header returns None when the magic bytes don't match
    /// the APROTO marker.
    #[test]
    fn parse_header_rejects_bad_magic() {
        let mut packet = build_command(0x00D000, SYS_NOOP, 42, &[]);
        packet[0] = 0x00; // corrupt magic
        assert!(parse_header(&packet).is_none());
    }

    /// @test parse_packet returns None when the buffer is shorter than
    /// header.payload_length declares.
    #[test]
    fn parse_packet_rejects_truncated_payload() {
        // Build a packet with payload_length=4 but only 2 bytes of payload
        let mut packet = build_command(0x000000, EXEC_GET_HEALTH, 1, &[0x01, 0x02, 0x03, 0x04]);
        packet.truncate(HEADER_SIZE + 2); // chop payload in half
        assert!(parse_packet(&packet).is_none());
    }

    /// @test parse_ack returns None for payloads shorter than the
    /// minimum 8-byte ACK header.
    #[test]
    fn parse_ack_rejects_short_payload() {
        assert!(parse_ack(&[]).is_none());
        assert!(parse_ack(&[0x00, 0x01, 0x02]).is_none());
    }

    /// @test Every documented status code maps to its named string,
    /// and unknown codes return "UNKNOWN".
    #[test]
    fn parse_ack_status_names_cover_known_codes() {
        for (status, expected) in [
            (0u8, "SUCCESS"),
            (1, "UNKNOWN_OPCODE"),
            (2, "INVALID_PAYLOAD"),
            (3, "NO_RESOLVER"),
            (4, "COMPONENT_NOT_FOUND"),
            (5, "LOAD_FAILED"),
            (99, "UNKNOWN"),
        ] {
            let payload = vec![0x00, 0x01, 0x02, 0x03, status, 0, 0, 0];
            let ack = parse_ack(&payload).unwrap();
            assert_eq!(ack.status_name, expected, "status code {}", status);
        }
    }

    /// @test The payload_length field in a built command equals the
    /// actual payload byte count, and the total packet size is
    /// header + payload.
    #[test]
    fn build_command_payload_length_field_matches() {
        let payload = vec![0u8; 100];
        let packet = build_command(0x00D000, EXEC_INSPECT, 0, &payload);
        let header = parse_header(&packet).unwrap();
        assert_eq!(header.payload_length as usize, payload.len());
        assert_eq!(packet.len(), HEADER_SIZE + payload.len());
    }
}
