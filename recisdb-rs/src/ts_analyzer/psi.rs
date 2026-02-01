//! PSI (Program Specific Information) section parsing.
//!
//! This module handles common PSI section header parsing and CRC validation.

/// PSI section header (common to all PSI tables).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PsiHeader {
    /// Table ID.
    pub table_id: u8,
    /// Section syntax indicator.
    pub section_syntax_indicator: bool,
    /// Section length (12 bits).
    pub section_length: u16,
    /// Table ID extension (for long sections).
    pub table_id_extension: u16,
    /// Version number (5 bits).
    pub version_number: u8,
    /// Current/next indicator.
    pub current_next_indicator: bool,
    /// Section number.
    pub section_number: u8,
    /// Last section number.
    pub last_section_number: u8,
}

/// A parsed PSI section.
#[derive(Debug, Clone)]
pub struct PsiSection<'a> {
    /// Section header.
    pub header: PsiHeader,
    /// Section data (after header, before CRC).
    pub data: &'a [u8],
    /// CRC32 value.
    pub crc32: u32,
}

impl<'a> PsiSection<'a> {
    /// Parse a PSI section from raw bytes.
    ///
    /// # Arguments
    /// * `data` - Slice containing the section data starting from table_id
    ///
    /// # Returns
    /// Parsed section or error message
    pub fn parse(data: &'a [u8]) -> Result<Self, &'static str> {
        if data.len() < 3 {
            return Err("Section too short for header");
        }

        let table_id = data[0];
        let section_syntax_indicator = data[1] & 0x80 != 0;
        let section_length = ((data[1] as u16 & 0x0F) << 8) | data[2] as u16;

        if section_length < 5 {
            return Err("Section length too small");
        }

        let total_length = 3 + section_length as usize;
        if data.len() < total_length {
            return Err("Incomplete section data");
        }

        let header = if section_syntax_indicator {
            // Long section (with extended header)
            if data.len() < 8 {
                return Err("Section too short for extended header");
            }

            PsiHeader {
                table_id,
                section_syntax_indicator,
                section_length,
                table_id_extension: ((data[3] as u16) << 8) | data[4] as u16,
                version_number: (data[5] >> 1) & 0x1F,
                current_next_indicator: data[5] & 0x01 != 0,
                section_number: data[6],
                last_section_number: data[7],
            }
        } else {
            // Short section (no extended header)
            PsiHeader {
                table_id,
                section_syntax_indicator,
                section_length,
                table_id_extension: 0,
                version_number: 0,
                current_next_indicator: true,
                section_number: 0,
                last_section_number: 0,
            }
        };

        // Calculate data range (after header, before CRC)
        let data_start = if section_syntax_indicator { 8 } else { 3 };
        let data_end = total_length - 4; // 4 bytes for CRC

        if data_end <= data_start {
            return Err("No data in section");
        }

        let section_data = &data[data_start..data_end];

        // Extract CRC32
        let crc_offset = total_length - 4;
        let crc32 = ((data[crc_offset] as u32) << 24)
            | ((data[crc_offset + 1] as u32) << 16)
            | ((data[crc_offset + 2] as u32) << 8)
            | (data[crc_offset + 3] as u32);

        Ok(PsiSection {
            header,
            data: section_data,
            crc32,
        })
    }

    /// Verify CRC32 of the section.
    pub fn verify_crc(&self, full_data: &[u8]) -> bool {
        let total_length = 3 + self.header.section_length as usize;
        if full_data.len() < total_length {
            return false;
        }

        let calculated = crc32_mpeg2(&full_data[..total_length - 4]);
        calculated == self.crc32
    }

    /// Get the total section length including header and CRC.
    pub fn total_length(&self) -> usize {
        3 + self.header.section_length as usize
    }
}

/// Section collector for multi-packet sections.
#[derive(Debug, Default)]
pub struct SectionCollector {
    /// Buffer for collecting section data.
    buffer: Vec<u8>,
    /// Expected section length.
    expected_length: Option<usize>,
    /// Last continuity counter.
    last_cc: Option<u8>,
}

impl SectionCollector {
    /// Create a new section collector.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear the collector.
    pub fn clear(&mut self) {
        self.buffer.clear();
        self.expected_length = None;
        self.last_cc = None;
    }

    /// Add data from a TS packet.
    ///
    /// Returns true if a complete section is available.
    pub fn add_data(&mut self, payload: &[u8], cc: u8, payload_unit_start: bool) -> bool {
        // Check continuity
        if let Some(last) = self.last_cc {
            let expected_cc = (last + 1) & 0x0F;
            if cc != expected_cc && !payload_unit_start {
                // Discontinuity - clear and start over
                self.clear();
            }
        }
        self.last_cc = Some(cc);

        if payload_unit_start {
            // New section starts
            if payload.is_empty() {
                return false;
            }

            // Pointer field
            let pointer = payload[0] as usize;
            let section_start = pointer + 1;

            if section_start >= payload.len() {
                return false;
            }

            // Start new section
            self.buffer.clear();
            self.buffer.extend_from_slice(&payload[section_start..]);

            // Try to get section length
            if self.buffer.len() >= 3 {
                let section_length =
                    ((self.buffer[1] as usize & 0x0F) << 8) | self.buffer[2] as usize;
                self.expected_length = Some(3 + section_length);
            }
        } else if !self.buffer.is_empty() {
            // Continue existing section
            self.buffer.extend_from_slice(payload);
        }

        // Check if section is complete
        if let Some(expected) = self.expected_length {
            self.buffer.len() >= expected
        } else {
            false
        }
    }

    /// Get the collected section data.
    pub fn get_section(&self) -> Option<&[u8]> {
        self.expected_length
            .filter(|&len| self.buffer.len() >= len)
            .map(|len| &self.buffer[..len])
    }

    /// Check if collector has data.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}

/// Calculate CRC32 for MPEG-2 (polynomial 0x04C11DB7).
pub fn crc32_mpeg2(data: &[u8]) -> u32 {
    // CRC32 lookup table for MPEG-2 polynomial
    static CRC_TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = (i as u32) << 24;
            let mut j = 0;
            while j < 8 {
                if crc & 0x80000000 != 0 {
                    crc = (crc << 1) ^ 0x04C11DB7;
                } else {
                    crc <<= 1;
                }
                j += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    };

    let mut crc = 0xFFFFFFFFu32;
    for &byte in data {
        let index = ((crc >> 24) ^ byte as u32) as usize;
        crc = (crc << 8) ^ CRC_TABLE[index];
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc32_empty() {
        // CRC32 of empty data with initial value 0xFFFFFFFF
        let crc = crc32_mpeg2(&[]);
        assert_eq!(crc, 0xFFFFFFFF);
    }

    #[test]
    fn test_section_collector() {
        let mut collector = SectionCollector::new();
        assert!(collector.is_empty());

        // Simulate PAT section data
        let mut payload = vec![0u8; 184]; // Pointer field + section
        payload[0] = 0; // Pointer field
        payload[1] = 0x00; // table_id = PAT
        payload[2] = 0x80; // section_syntax_indicator = 1
        payload[3] = 0x0D; // section_length = 13

        let complete = collector.add_data(&payload, 0, true);
        assert!(complete);
    }
}
