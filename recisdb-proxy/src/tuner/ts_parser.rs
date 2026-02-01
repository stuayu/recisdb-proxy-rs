//! Minimal TS parser for passive scanning.
//!
//! This is a lightweight TS parser that extracts only the essential
//! information needed for passive scanning (NID, TSID, SID, service names).

use std::collections::HashMap;

use recisdb_protocol::ChannelInfo;

/// TS packet size.
pub const TS_PACKET_SIZE: usize = 188;
/// TS sync byte.
pub const SYNC_BYTE: u8 = 0x47;

/// Well-known PIDs.
mod pid {
    pub const PAT: u16 = 0x0000;
    pub const NIT: u16 = 0x0010;
    pub const SDT: u16 = 0x0011;
    pub const NULL: u16 = 0x1FFF;
}

/// Table IDs.
mod table_id {
    pub const PAT: u8 = 0x00;
    pub const NIT_ACTUAL: u8 = 0x40;
    pub const SDT_ACTUAL: u8 = 0x42;
}

/// Descriptor tags.
mod descriptor_tag {
    pub const SERVICE: u8 = 0x48;
    pub const NETWORK_NAME: u8 = 0x40;
}

/// Minimal TS parser for passive scanning.
#[derive(Debug, Default)]
pub struct MinimalTsParser {
    /// Section buffers by PID.
    section_buffers: HashMap<u16, SectionBuffer>,
    /// Parsed result.
    result: ParseResult,
}

/// Section buffer for collecting PSI data across packets.
#[derive(Debug, Default)]
struct SectionBuffer {
    data: Vec<u8>,
    expected_length: Option<usize>,
    continuity_counter: Option<u8>,
}

/// Parsed result from TS stream.
#[derive(Debug, Default, Clone)]
pub struct ParseResult {
    /// Network ID (from NIT).
    pub network_id: Option<u16>,
    /// Transport stream ID (from PAT).
    pub transport_stream_id: Option<u16>,
    /// Network name (from NIT).
    pub network_name: Option<String>,
    /// Services (SID -> service info).
    pub services: HashMap<u16, ServiceInfo>,
    /// Has received PAT.
    pub has_pat: bool,
    /// Has received NIT.
    pub has_nit: bool,
    /// Has received SDT.
    pub has_sdt: bool,
}

/// Minimal service information.
#[derive(Debug, Default, Clone)]
pub struct ServiceInfo {
    /// Service ID.
    pub service_id: u16,
    /// Service name.
    pub service_name: Option<String>,
    /// Service type.
    pub service_type: Option<u8>,
    /// Provider name.
    pub provider_name: Option<String>,
}

impl MinimalTsParser {
    /// Create a new minimal TS parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed TS data to the parser.
    ///
    /// Returns true if minimum required data has been collected.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        let mut offset = 0;

        // Find first sync byte
        while offset < data.len() && data[offset] != SYNC_BYTE {
            offset += 1;
        }

        // Process packets
        while offset + TS_PACKET_SIZE <= data.len() {
            if data[offset] != SYNC_BYTE {
                offset += 1;
                continue;
            }

            self.process_packet(&data[offset..offset + TS_PACKET_SIZE]);
            offset += TS_PACKET_SIZE;
        }

        self.is_complete()
    }

    /// Process a single TS packet.
    fn process_packet(&mut self, packet: &[u8]) {
        if packet.len() < 4 || packet[0] != SYNC_BYTE {
            return;
        }

        // Parse header
        let transport_error = (packet[1] & 0x80) != 0;
        let payload_start = (packet[1] & 0x40) != 0;
        let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
        let scrambling = (packet[3] >> 6) & 0x03;
        let adaptation_field = (packet[3] >> 4) & 0x03;
        let continuity_counter = packet[3] & 0x0F;

        // Skip packets with errors, scrambled, or null
        if transport_error || scrambling != 0 || pid == pid::NULL {
            return;
        }

        // We only care about PAT, NIT, SDT
        if pid != pid::PAT && pid != pid::NIT && pid != pid::SDT {
            return;
        }

        // Calculate payload offset
        let payload_offset = match adaptation_field {
            0 => return, // No payload
            1 => 4,      // Payload only
            2 => return, // Adaptation only
            3 => {
                // Adaptation + payload
                if packet.len() < 5 {
                    return;
                }
                let adaptation_length = packet[4] as usize;
                5 + adaptation_length
            }
            _ => return,
        };

        if payload_offset >= packet.len() {
            return;
        }

        let payload = &packet[payload_offset..];
        self.process_payload(pid, payload, payload_start, continuity_counter);
    }

    /// Process packet payload.
    fn process_payload(&mut self, pid: u16, payload: &[u8], start: bool, cc: u8) {
        let buffer = self.section_buffers.entry(pid).or_default();

        if start {
            // Pointer field
            if payload.is_empty() {
                return;
            }
            let pointer = payload[0] as usize;
            if pointer + 1 >= payload.len() {
                return;
            }

            // New section starts
            buffer.data.clear();
            buffer.expected_length = None;
            buffer.continuity_counter = Some(cc);

            let section_start = &payload[pointer + 1..];
            buffer.data.extend_from_slice(section_start);
        } else {
            // Continuation
            if let Some(expected_cc) = buffer.continuity_counter {
                let next_cc = (expected_cc + 1) & 0x0F;
                if cc != next_cc {
                    // Discontinuity, reset
                    buffer.data.clear();
                    buffer.expected_length = None;
                    return;
                }
            }
            buffer.continuity_counter = Some(cc);
            buffer.data.extend_from_slice(payload);
        }

        // Try to parse section
        self.try_parse_section(pid);
    }

    /// Try to parse a complete section.
    fn try_parse_section(&mut self, pid: u16) {
        let buffer = match self.section_buffers.get(&pid) {
            Some(b) => b,
            None => return,
        };

        if buffer.data.len() < 3 {
            return;
        }

        // Get section length
        let section_length = ((buffer.data[1] as usize & 0x0F) << 8) | buffer.data[2] as usize;
        let total_length = 3 + section_length;

        if buffer.data.len() < total_length {
            return; // Need more data
        }

        // Extract section data
        let section_data: Vec<u8> = buffer.data[..total_length].to_vec();

        // Clear buffer
        if let Some(b) = self.section_buffers.get_mut(&pid) {
            b.data = b.data[total_length..].to_vec();
        }

        // Parse based on PID
        match pid {
            pid::PAT => self.parse_pat(&section_data),
            pid::NIT => self.parse_nit(&section_data),
            pid::SDT => self.parse_sdt(&section_data),
            _ => {}
        }
    }

    /// Parse PAT (Program Association Table).
    fn parse_pat(&mut self, data: &[u8]) {
        if data.len() < 8 || data[0] != table_id::PAT {
            return;
        }

        // Get transport stream ID
        let tsid = ((data[3] as u16) << 8) | data[4] as u16;
        self.result.transport_stream_id = Some(tsid);
        self.result.has_pat = true;

        // Parse program entries
        let section_length = ((data[1] as usize & 0x0F) << 8) | data[2] as usize;
        let mut offset = 8;
        let end = std::cmp::min(3 + section_length - 4, data.len()); // -4 for CRC

        while offset + 4 <= end {
            let program_number = ((data[offset] as u16) << 8) | data[offset + 1] as u16;
            // let pid = ((data[offset + 2] as u16 & 0x1F) << 8) | data[offset + 3] as u16;

            if program_number != 0 {
                // Not NIT
                self.result
                    .services
                    .entry(program_number)
                    .or_insert_with(|| ServiceInfo {
                        service_id: program_number,
                        ..Default::default()
                    });
            }

            offset += 4;
        }
    }

    /// Parse NIT (Network Information Table).
    fn parse_nit(&mut self, data: &[u8]) {
        if data.len() < 10 || data[0] != table_id::NIT_ACTUAL {
            return;
        }

        // Get network ID
        let nid = ((data[3] as u16) << 8) | data[4] as u16;
        self.result.network_id = Some(nid);
        self.result.has_nit = true;

        // Parse network descriptors
        let network_desc_length = ((data[8] as usize & 0x0F) << 8) | data[9] as usize;
        let desc_start = 10;
        let desc_end = std::cmp::min(desc_start + network_desc_length, data.len());

        if desc_end > desc_start {
            self.parse_network_descriptors(&data[desc_start..desc_end]);
        }
    }

    /// Parse network descriptors from NIT.
    fn parse_network_descriptors(&mut self, data: &[u8]) {
        let mut offset = 0;

        while offset + 2 <= data.len() {
            let tag = data[offset];
            let length = data[offset + 1] as usize;

            if offset + 2 + length > data.len() {
                break;
            }

            if tag == descriptor_tag::NETWORK_NAME && length > 0 {
                let name_data = &data[offset + 2..offset + 2 + length];
                if let Some(name) = decode_arib_string(name_data) {
                    self.result.network_name = Some(name);
                }
            }

            offset += 2 + length;
        }
    }

    /// Parse SDT (Service Description Table).
    fn parse_sdt(&mut self, data: &[u8]) {
        if data.len() < 11 || data[0] != table_id::SDT_ACTUAL {
            return;
        }

        let original_network_id = ((data[8] as u16) << 8) | data[9] as u16;
        if self.result.network_id.is_none() {
            self.result.network_id = Some(original_network_id);
        }
        self.result.has_sdt = true;

        // Parse services
        let section_length = ((data[1] as usize & 0x0F) << 8) | data[2] as usize;
        let mut offset = 11;
        let end = std::cmp::min(3 + section_length - 4, data.len()); // -4 for CRC

        while offset + 5 <= end {
            let service_id = ((data[offset] as u16) << 8) | data[offset + 1] as u16;
            let desc_loop_length =
                ((data[offset + 3] as usize & 0x0F) << 8) | data[offset + 4] as usize;

            if offset + 5 + desc_loop_length > end {
                break;
            }

            // Parse service descriptors
            let desc_start = offset + 5;
            let desc_end = desc_start + desc_loop_length;
            if desc_end <= data.len() {
                self.parse_service_descriptors(service_id, &data[desc_start..desc_end]);
            }

            offset = desc_end;
        }
    }

    /// Parse service descriptors from SDT.
    fn parse_service_descriptors(&mut self, service_id: u16, data: &[u8]) {
        let mut offset = 0;

        while offset + 2 <= data.len() {
            let tag = data[offset];
            let length = data[offset + 1] as usize;

            if offset + 2 + length > data.len() {
                break;
            }

            if tag == descriptor_tag::SERVICE && length >= 3 {
                let desc_data = &data[offset + 2..offset + 2 + length];
                let service_type = desc_data[0];
                let provider_name_length = desc_data[1] as usize;

                let mut name_offset = 2 + provider_name_length;
                let provider_name = if provider_name_length > 0 && name_offset <= desc_data.len() {
                    decode_arib_string(&desc_data[2..name_offset])
                } else {
                    None
                };

                let service_name = if name_offset + 1 <= desc_data.len() {
                    let service_name_length = desc_data[name_offset] as usize;
                    name_offset += 1;
                    if name_offset + service_name_length <= desc_data.len() {
                        decode_arib_string(&desc_data[name_offset..name_offset + service_name_length])
                    } else {
                        None
                    }
                } else {
                    None
                };

                let entry = self
                    .result
                    .services
                    .entry(service_id)
                    .or_insert_with(|| ServiceInfo {
                        service_id,
                        ..Default::default()
                    });

                entry.service_type = Some(service_type);
                if service_name.is_some() {
                    entry.service_name = service_name;
                }
                if provider_name.is_some() {
                    entry.provider_name = provider_name;
                }
            }

            offset += 2 + length;
        }
    }

    /// Check if parsing is complete (has minimum required info).
    pub fn is_complete(&self) -> bool {
        self.result.has_pat && (self.result.has_nit || self.result.has_sdt)
    }

    /// Get the parsing result.
    pub fn result(&self) -> &ParseResult {
        &self.result
    }

    /// Convert result to ChannelInfo list.
    pub fn to_channel_infos(&self) -> Vec<ChannelInfo> {
        let nid = self.result.network_id.unwrap_or(0);
        let tsid = self.result.transport_stream_id.unwrap_or(0);

        self.result
            .services
            .values()
            .map(|s| ChannelInfo {
                nid,
                tsid,
                sid: s.service_id,
                manual_sheet: None,
                raw_name: s.service_name.clone(),
                channel_name: s.service_name.clone(),
                physical_ch: None,
                remote_control_key: None,
                service_type: s.service_type,
                network_name: self.result.network_name.clone(),
                bon_space: None,
                bon_channel: None,
                band_type: None,
                terrestrial_region: None,
            })
            .collect()
    }

    /// Reset the parser state.
    pub fn reset(&mut self) {
        self.section_buffers.clear();
        self.result = ParseResult::default();
    }
}

/// Decode ARIB string (simplified - handles basic ASCII and UTF-8).
fn decode_arib_string(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }

    // Skip ARIB character code designation if present
    let start = if data[0] == 0x1B {
        // ESC sequence - skip it
        let mut i = 1;
        while i < data.len() && data[i] != 0x1B && i < 4 {
            i += 1;
        }
        i
    } else {
        0
    };

    if start >= data.len() {
        return None;
    }

    // Try to decode as UTF-8 first, then as Shift-JIS-like
    let text_data = &data[start..];

    // Simple approach: try UTF-8, fallback to lossy conversion
    match String::from_utf8(text_data.to_vec()) {
        Ok(s) => Some(s.trim_end_matches('\0').to_string()),
        Err(_) => {
            // Lossy conversion
            let s = String::from_utf8_lossy(text_data);
            Some(s.trim_end_matches('\0').to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parser_new() {
        let parser = MinimalTsParser::new();
        assert!(!parser.is_complete());
    }

    #[test]
    fn test_parse_result_default() {
        let result = ParseResult::default();
        assert!(result.network_id.is_none());
        assert!(result.transport_stream_id.is_none());
        assert!(!result.has_pat);
        assert!(!result.has_nit);
        assert!(!result.has_sdt);
    }

    #[test]
    fn test_decode_arib_string() {
        // Simple ASCII
        let data = b"Test Channel";
        assert_eq!(decode_arib_string(data), Some("Test Channel".to_string()));

        // Empty
        assert_eq!(decode_arib_string(&[]), None);

        // With null terminator
        let data_null = b"Test\0";
        assert_eq!(decode_arib_string(data_null), Some("Test".to_string()));
    }
}
