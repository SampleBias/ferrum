//! Authenticated JSON frames shared by the guest bridge and the host controller.
//!
//! A frame is a big-endian payload length, the exact UTF-8 JSON bytes, and an
//! HMAC-SHA256 over the direction marker, the length, and those bytes. JSON is
//! not reserialized before the tag is checked.

#![forbid(unsafe_code)]

mod json;
mod messages;

pub use json::validate_json;
pub use messages::{
    decode_message, encode_abstain, encode_applied, encode_hello, encode_hello_ack,
    encode_proposal, encode_reject, encode_snapshot, AppliedReport, Decoded, RejectReport,
};

use hmac::{Hmac, Mac};
use policy_types::MAX_PAYLOAD_BYTES;
use sha2::Sha256;

pub const GUEST_TO_CONTROLLER: &[u8] = b"aik1-guest-to-controller";
pub const CONTROLLER_TO_GUEST: &[u8] = b"aik1-controller-to-guest";

type Tag = Hmac<Sha256>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    GuestToController,
    ControllerToGuest,
}

impl Direction {
    pub const fn marker(self) -> &'static [u8] {
        match self {
            Self::GuestToController => GUEST_TO_CONTROLLER,
            Self::ControllerToGuest => CONTROLLER_TO_GUEST,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    TooLong,
    Truncated,
    Trailing,
    BadMac,
    Utf8,
    Structure,
    DuplicateKey,
    TooDeep,
    Float,
    Schema,
    OutOfRange,
    UnsupportedVersion,
    SnapshotHash,
    Buffer,
}

pub fn seal(direction: Direction, key: &[u8; 32], payload: &[u8]) -> Result<Vec<u8>, WireError> {
    if payload.len() > MAX_PAYLOAD_BYTES {
        return Err(WireError::TooLong);
    }
    let mut frame = Vec::with_capacity(4 + payload.len() + 32);
    let len = u32::try_from(payload.len()).map_err(|_| WireError::TooLong)?;
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(payload);
    let mac = tag(direction, key, &len.to_be_bytes(), payload)?;
    frame.extend_from_slice(&mac);
    Ok(frame)
}

pub fn open(direction: Direction, key: &[u8; 32], frame: &[u8]) -> Result<Vec<u8>, WireError> {
    if frame.len() < 4 + 32 {
        return Err(WireError::Truncated);
    }
    let len = u32::from_be_bytes(frame[0..4].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD_BYTES {
        return Err(WireError::TooLong);
    }
    let total = 4 + len + 32;
    if frame.len() < total {
        return Err(WireError::Truncated);
    }
    if frame.len() != total {
        return Err(WireError::Trailing);
    }
    let payload = &frame[4..4 + len];
    let expected = tag(direction, key, &frame[0..4], payload)?;
    let found = &frame[4 + len..];
    if !constant_eq(&expected, found) {
        return Err(WireError::BadMac);
    }
    Ok(payload.to_vec())
}

fn tag(direction: Direction, key: &[u8; 32], length: &[u8], payload: &[u8]) -> Result<[u8; 32], WireError> {
    let mut mac = Tag::new_from_slice(key).map_err(|_| WireError::BadMac)?;
    mac.update(direction.marker());
    mac.update(length);
    mac.update(payload);
    let mut out = [0u8; 32];
    out.copy_from_slice(&mac.finalize().into_bytes());
    Ok(out)
}

fn constant_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn push(&mut self, data: &[u8]) -> Result<Option<Vec<u8>>, WireError> {
        if self.buf.len().saturating_add(data.len()) > 4 + MAX_PAYLOAD_BYTES + 32 {
            self.buf.clear();
            return Err(WireError::TooLong);
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes(self.buf[0..4].try_into().unwrap()) as usize;
        if len > MAX_PAYLOAD_BYTES {
            // Drop only the illegal length. A later frame already in the buffer
            // stays available, and the body is never stored.
            self.buf.drain(..4);
            return Err(WireError::TooLong);
        }
        let total = 4 + len + 32;
        if self.buf.len() < total {
            return Ok(None);
        }
        let frame: Vec<u8> = self.buf.drain(..total).collect();
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> &'static [u8] {
        let raw = include_bytes!("../../../tests/protocol-vectors/proposal.json");
        let end = raw.iter().rposition(|byte| *byte != b'\n').unwrap() + 1;
        &raw[..end]
    }

    #[test]
    fn proposal_mac_matches_the_python_vector() {
        let key = [0x11u8; 32];
        let frame = seal(Direction::ControllerToGuest, &key, payload()).unwrap();
        let mac = &frame[frame.len() - 32..];
        assert_eq!(
            hex(mac),
            "2c99f48a01e3f53afd70acae38f3efd7e12dea1063fe837473905c9d078c1219"
        );
        assert_eq!(
            open(Direction::ControllerToGuest, &key, &frame).unwrap(),
            payload()
        );
        assert_eq!(
            open(Direction::GuestToController, &key, &frame),
            Err(WireError::BadMac)
        );
    }

    #[test]
    fn partial_reads_assemble_one_frame() {
        let key = [0x11u8; 32];
        let frame = seal(Direction::GuestToController, &key, payload()).unwrap();
        let mut decoder = FrameDecoder::new();
        assert_eq!(decoder.push(&frame[..3]).unwrap(), None);
        assert_eq!(decoder.push(&frame[3..10]).unwrap(), None);
        let rest = decoder.push(&frame[10..]).unwrap().unwrap();
        assert_eq!(rest, frame);
    }

    #[test]
    fn oversized_length_is_rejected_before_the_body() {
        let mut decoder = FrameDecoder::new();
        let header = (MAX_PAYLOAD_BYTES as u32 + 1).to_be_bytes();
        assert_eq!(decoder.push(&header), Err(WireError::TooLong));
        assert!(decoder.buf.is_empty());
    }

    #[test]
    fn oversized_header_does_not_discard_the_next_frame() {
        let key = [0x11u8; 32];
        let frame = seal(Direction::ControllerToGuest, &key, payload()).unwrap();
        let mut bytes = frame.clone();
        bytes.extend_from_slice(&(MAX_PAYLOAD_BYTES as u32 + 1).to_be_bytes());
        bytes.extend_from_slice(&frame);
        let mut decoder = FrameDecoder::new();
        assert_eq!(decoder.push(&bytes).unwrap().unwrap(), frame);
        assert_eq!(decoder.push(&[]), Err(WireError::TooLong));
        assert_eq!(decoder.push(&[]).unwrap().unwrap(), frame);
    }

    fn hex(bytes: &[u8]) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::new();
        for byte in bytes {
            out.push(DIGITS[(byte >> 4) as usize] as char);
            out.push(DIGITS[(byte & 0xf) as usize] as char);
        }
        out
    }
}
