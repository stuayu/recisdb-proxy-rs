//! MPEG-TS packet parsing.
//!
//! This module handles parsing of 188-byte MPEG Transport Stream packets.

/// TS packet size in bytes.
pub const TS_PACKET_SIZE: usize = 188;

/// TS sync byte (0x47).
pub const SYNC_BYTE: u8 = 0x47;

/// Parsed TS packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TsHeader {
    /// Transport error indicator.
    pub transport_error: bool,
    /// Payload unit start indicator.
    pub payload_unit_start: bool,
    /// Transport priority.
    pub transport_priority: bool,
    /// Packet Identifier (13 bits).
    pub pid: u16,
    /// Transport scrambling control (2 bits).
    pub scrambling_control: u8,
    /// Adaptation field control (2 bits).
    pub adaptation_field_control: u8,
    /// Continuity counter (4 bits).
    pub continuity_counter: u8,
}

impl TsHeader {
    /// Check if packet has adaptation field.
    pub fn has_adaptation_field(&self) -> bool {
        self.adaptation_field_control & 0x02 != 0
    }

    /// Check if packet has payload.
    pub fn has_payload(&self) -> bool {
        self.adaptation_field_control & 0x01 != 0
    }

    /// Check if packet is scrambled.
    pub fn is_scrambled(&self) -> bool {
        self.scrambling_control != 0
    }
}

/// Adaptation field data.
#[derive(Debug, Clone, Default)]
pub struct AdaptationField {
    /// Adaptation field length.
    pub length: u8,
    /// Discontinuity indicator.
    pub discontinuity: bool,
    /// Random access indicator.
    pub random_access: bool,
    /// Elementary stream priority indicator.
    pub es_priority: bool,
    /// PCR flag.
    pub pcr_flag: bool,
    /// OPCR flag.
    pub opcr_flag: bool,
    /// Splicing point flag.
    pub splicing_point_flag: bool,
    /// Transport private data flag.
    pub transport_private_data_flag: bool,
    /// Adaptation field extension flag.
    pub adaptation_extension_flag: bool,
    /// PCR value (if present).
    pub pcr: Option<u64>,
}

/// A parsed TS packet.
#[derive(Debug, Clone)]
pub struct TsPacket<'a> {
    /// Packet header.
    pub header: TsHeader,
    /// Adaptation field (if present).
    pub adaptation_field: Option<AdaptationField>,
    /// Payload data.
    pub payload: &'a [u8],
}

impl<'a> TsPacket<'a> {
    /// Parse a TS packet from raw bytes.
    ///
    /// # Arguments
    /// * `data` - Slice containing at least 188 bytes
    ///
    /// # Returns
    /// Parsed packet or error message
    pub fn parse(data: &'a [u8]) -> Result<Self, &'static str> {
        if data.len() < TS_PACKET_SIZE {
            return Err("Packet too short");
        }

        if data[0] != SYNC_BYTE {
            return Err("Invalid sync byte");
        }

        let header = TsHeader {
            transport_error: data[1] & 0x80 != 0,
            payload_unit_start: data[1] & 0x40 != 0,
            transport_priority: data[1] & 0x20 != 0,
            pid: ((data[1] as u16 & 0x1F) << 8) | data[2] as u16,
            scrambling_control: (data[3] >> 6) & 0x03,
            adaptation_field_control: (data[3] >> 4) & 0x03,
            continuity_counter: data[3] & 0x0F,
        };

        let mut offset = 4;
        let adaptation_field = if header.has_adaptation_field() {
            let af_length = data[4] as usize;
            offset = 5 + af_length;

            if af_length > 0 && data.len() > 5 {
                let flags = data[5];
                let mut af = AdaptationField {
                    length: data[4],
                    discontinuity: flags & 0x80 != 0,
                    random_access: flags & 0x40 != 0,
                    es_priority: flags & 0x20 != 0,
                    pcr_flag: flags & 0x10 != 0,
                    opcr_flag: flags & 0x08 != 0,
                    splicing_point_flag: flags & 0x04 != 0,
                    transport_private_data_flag: flags & 0x02 != 0,
                    adaptation_extension_flag: flags & 0x01 != 0,
                    pcr: None,
                };

                // Parse PCR if present
                if af.pcr_flag && af_length >= 6 && data.len() >= 12 {
                    let pcr_base = ((data[6] as u64) << 25)
                        | ((data[7] as u64) << 17)
                        | ((data[8] as u64) << 9)
                        | ((data[9] as u64) << 1)
                        | ((data[10] as u64) >> 7);
                    let pcr_ext = ((data[10] as u64 & 0x01) << 8) | data[11] as u64;
                    af.pcr = Some(pcr_base * 300 + pcr_ext);
                }

                Some(af)
            } else {
                Some(AdaptationField {
                    length: data[4],
                    ..Default::default()
                })
            }
        } else {
            None
        };

        let payload = if header.has_payload() && offset < TS_PACKET_SIZE {
            &data[offset..TS_PACKET_SIZE]
        } else {
            &[]
        };

        Ok(TsPacket {
            header,
            adaptation_field,
            payload,
        })
    }

    /// Get the payload start offset for PSI sections.
    /// When payload_unit_start is set, the first byte is the pointer field.
    pub fn get_psi_payload(&self) -> Option<&'a [u8]> {
        if !self.header.has_payload() || self.payload.is_empty() {
            return None;
        }

        if self.header.payload_unit_start {
            // First byte is pointer field
            let pointer = self.payload[0] as usize;
            if pointer + 1 < self.payload.len() {
                Some(&self.payload[pointer + 1..])
            } else {
                None
            }
        } else {
            Some(self.payload)
        }
    }
}

/// Iterator over TS packets in a byte stream.
pub struct TsPacketIterator<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> TsPacketIterator<'a> {
    /// Create a new iterator over TS packets.
    pub fn new(data: &'a [u8]) -> Self {
        // Find first sync byte
        let mut offset = 0;
        while offset < data.len() && data[offset] != SYNC_BYTE {
            offset += 1;
        }
        Self { data, offset }
    }

    /// Resynchronize to next sync byte.
    fn resync(&mut self) {
        self.offset += 1;
        while self.offset < self.data.len() && self.data[self.offset] != SYNC_BYTE {
            self.offset += 1;
        }
    }
}

impl<'a> Iterator for TsPacketIterator<'a> {
    type Item = TsPacket<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.offset + TS_PACKET_SIZE <= self.data.len() {
            if self.data[self.offset] != SYNC_BYTE {
                self.resync();
                continue;
            }

            match TsPacket::parse(&self.data[self.offset..]) {
                Ok(packet) => {
                    self.offset += TS_PACKET_SIZE;
                    return Some(packet);
                }
                Err(_) => {
                    self.resync();
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_null_packet() {
        let mut packet = [0u8; 188];
        packet[0] = SYNC_BYTE;
        packet[1] = 0x1F; // PID high bits
        packet[2] = 0xFF; // PID low bits (NULL = 0x1FFF)
        packet[3] = 0x10; // adaptation_field_control = 01, has payload

        let parsed = TsPacket::parse(&packet).unwrap();
        assert_eq!(parsed.header.pid, 0x1FFF);
        assert!(!parsed.header.transport_error);
        assert!(parsed.header.has_payload());
        assert!(!parsed.header.has_adaptation_field());
    }

    #[test]
    fn test_parse_pat_packet() {
        let mut packet = [0u8; 188];
        packet[0] = SYNC_BYTE;
        packet[1] = 0x40; // payload_unit_start = 1, PID = 0x0000
        packet[2] = 0x00;
        packet[3] = 0x10; // has payload

        let parsed = TsPacket::parse(&packet).unwrap();
        assert_eq!(parsed.header.pid, 0x0000);
        assert!(parsed.header.payload_unit_start);
    }

    #[test]
    fn test_invalid_sync_byte() {
        let mut packet = [0u8; 188];
        packet[0] = 0x00; // Invalid sync byte

        assert!(TsPacket::parse(&packet).is_err());
    }

    #[test]
    fn test_ts_header_methods() {
        let header = TsHeader {
            transport_error: false,
            payload_unit_start: true,
            transport_priority: false,
            pid: 0x0000,
            scrambling_control: 0,
            adaptation_field_control: 0x03, // Has both
            continuity_counter: 0,
        };

        assert!(header.has_adaptation_field());
        assert!(header.has_payload());
        assert!(!header.is_scrambled());
    }
}
