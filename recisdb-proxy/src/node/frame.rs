//! Framing used only between recisdb-proxy nodes.
//!
//! The payload remains 188-byte-aligned MPEG-TS, but every chunk carries a
//! generation and monotonic sequence number.  This lets a downstream node
//! distinguish path reconnects from source changes and request lossless
//! replay for RECORD sessions.

use bytes::{Buf, BufMut, Bytes, BytesMut};

pub const NODE_TS_MAGIC: [u8; 4] = *b"RCTS";
pub const NODE_TS_VERSION: u8 = 1;
pub const NODE_TS_HEADER_LEN: usize = 32;
pub const MAX_NODE_TS_PAYLOAD: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameFlags(u8);

impl FrameFlags {
    pub const DISCONTINUITY: u8 = 1 << 0;
    pub const REPLAY: u8 = 1 << 1;
    pub const END: u8 = 1 << 2;

    pub const fn new(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeTsFrame {
    pub generation: u32,
    pub sequence: u64,
    pub source_monotonic_ms: u64,
    pub flags: FrameFlags,
    pub payload: Bytes,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("node TS frame is too short")]
    TooShort,
    #[error("invalid node TS magic")]
    BadMagic,
    #[error("unsupported node TS frame version {0}")]
    UnsupportedVersion(u8),
    #[error("node TS frame payload too large: {0}")]
    PayloadTooLarge(usize),
    #[error("node TS payload is not 188-byte aligned: {0} bytes")]
    UnalignedPayload(usize),
    #[error("incomplete node TS payload: declared {declared}, available {available}")]
    Incomplete { declared: usize, available: usize },
}

impl NodeTsFrame {
    pub fn encode(&self) -> Result<Bytes, FrameError> {
        validate_payload(&self.payload)?;
        let mut out = BytesMut::with_capacity(NODE_TS_HEADER_LEN + self.payload.len());
        out.put_slice(&NODE_TS_MAGIC);
        out.put_u8(NODE_TS_VERSION);
        out.put_u8(self.flags.bits());
        out.put_u16(0); // reserved
        out.put_u32(self.generation);
        out.put_u64(self.sequence);
        out.put_u64(self.source_monotonic_ms);
        out.put_u32(self.payload.len() as u32);
        debug_assert_eq!(out.len(), NODE_TS_HEADER_LEN);
        out.put_slice(&self.payload);
        Ok(out.freeze())
    }

    /// Decode exactly one frame from `input`, returning frame and consumed
    /// byte count. HTTP/2 DATA boundaries are not protocol boundaries, so a
    /// caller may keep unread bytes and invoke this again when more arrive.
    pub fn decode(input: &[u8]) -> Result<(Self, usize), FrameError> {
        if input.len() < NODE_TS_HEADER_LEN {
            return Err(FrameError::TooShort);
        }
        if input[..4] != NODE_TS_MAGIC {
            return Err(FrameError::BadMagic);
        }

        let mut header = &input[4..NODE_TS_HEADER_LEN];
        let version = header.get_u8();
        if version != NODE_TS_VERSION {
            return Err(FrameError::UnsupportedVersion(version));
        }
        let flags = FrameFlags::new(header.get_u8());
        let _reserved = header.get_u16();
        let generation = header.get_u32();
        let sequence = header.get_u64();
        let source_monotonic_ms = header.get_u64();
        let payload_len = header.get_u32() as usize;
        if payload_len > MAX_NODE_TS_PAYLOAD {
            return Err(FrameError::PayloadTooLarge(payload_len));
        }
        if payload_len % 188 != 0 {
            return Err(FrameError::UnalignedPayload(payload_len));
        }
        let total = NODE_TS_HEADER_LEN + payload_len;
        if input.len() < total {
            return Err(FrameError::Incomplete {
                declared: payload_len,
                available: input.len() - NODE_TS_HEADER_LEN,
            });
        }

        Ok((
            Self {
                generation,
                sequence,
                source_monotonic_ms,
                flags,
                payload: Bytes::copy_from_slice(&input[NODE_TS_HEADER_LEN..total]),
            },
            total,
        ))
    }
}

fn validate_payload(payload: &[u8]) -> Result<(), FrameError> {
    if payload.len() > MAX_NODE_TS_PAYLOAD {
        return Err(FrameError::PayloadTooLarge(payload.len()));
    }
    if payload.len() % 188 != 0 {
        return Err(FrameError::UnalignedPayload(payload.len()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_sequence_and_payload() {
        let frame = NodeTsFrame {
            generation: 7,
            sequence: 12345,
            source_monotonic_ms: 999,
            flags: FrameFlags::new(FrameFlags::REPLAY),
            payload: Bytes::from(vec![0x47; 188 * 4]),
        };
        let encoded = frame.encode().unwrap();
        let (decoded, consumed) = NodeTsFrame::decode(&encoded).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, frame);
    }

    #[test]
    fn rejects_unaligned_payload() {
        let frame = NodeTsFrame {
            generation: 0,
            sequence: 0,
            source_monotonic_ms: 0,
            flags: FrameFlags::default(),
            payload: Bytes::from_static(b"not-ts"),
        };
        assert_eq!(frame.encode().unwrap_err(), FrameError::UnalignedPayload(6));
    }
}
