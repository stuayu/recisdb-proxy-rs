//! PSI (Program Specific Information) section parsing.
//!
//! This module handles common PSI section header parsing and CRC validation.

use log::trace;

/// Maximum legal `section_length` per MPEG-2 PSI (12-bit field, but the
/// standard further caps it at 4093 so that `3 + section_length` never
/// exceeds the maximum private-section size of 4096 bytes).
const MAX_SECTION_LENGTH: usize = 4093;

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
///
/// PSI/SI sections rarely align with TS packet boundaries — this is
/// especially true for densely-packed tables like EIT (EPG), where several
/// sections can start and end within a single packet, or a section header
/// can straddle a packet boundary. `add_data` therefore treats the buffer as
/// a continuous byte stream and repeatedly slices complete, CRC-valid
/// sections out of it (see `drain_sections`), rather than assuming "one
/// packet in, at most one section out".
#[derive(Debug, Default)]
pub struct SectionCollector {
    /// Buffer for collecting section data (may hold more than one pending
    /// section's worth of bytes after a `drain_sections` pass leaves a
    /// partial trailing section).
    buffer: Vec<u8>,
    /// Expected total length (header + data + CRC) of the section currently
    /// at the front of `buffer`, once its 3-byte header has been parsed.
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
    /// Returns zero or more complete, CRC-validated sections. A single call
    /// can return multiple sections (back-to-back sections in one packet,
    /// or one packet finishing a pending section and starting/finishing
    /// another).
    pub fn add_data(&mut self, payload: &[u8], cc: u8, payload_unit_start: bool) -> Vec<Vec<u8>> {
        let mut sections = Vec::new();

        // Check continuity
        let mut continuity_broken = false;
        if let Some(last) = self.last_cc {
            let expected_cc = (last + 1) & 0x0F;
            if cc != expected_cc {
                // Discontinuity - clear and start over
                self.clear();
                continuity_broken = true;
            }
        }
        self.last_cc = Some(cc);

        if payload_unit_start {
            if payload.is_empty() {
                return sections;
            }

            // Pointer field: number of bytes *before* the start of the next
            // section, i.e. bytes that finish the section already in
            // progress (if any).
            let pointer = payload[0] as usize;
            let rest = &payload[1..];

            if pointer > rest.len() {
                // Malformed pointer_field. Best effort: treat everything as
                // a continuation of the in-progress section and don't start
                // a new one this packet.
                self.buffer.extend_from_slice(rest);
                self.drain_sections(&mut sections);
                return sections;
            }

            let (before, after) = rest.split_at(pointer);

            // Finish the section that was in progress before this packet.
            if !continuity_broken && !before.is_empty() {
                self.buffer.extend_from_slice(before);
            }
            self.drain_sections(&mut sections);

            // Start a fresh section stream with whatever follows the
            // pointer, discarding any incomplete leftovers from the
            // previous stream (a PUSI here means the encoder is starting a
            // new section regardless of what we had buffered).
            self.buffer.clear();
            self.expected_length = None;
            self.buffer.extend_from_slice(after);
            self.drain_sections(&mut sections);
        } else if !self.buffer.is_empty() {
            self.buffer.extend_from_slice(payload);
            self.drain_sections(&mut sections);
        }

        sections
    }

    /// Repeatedly slice complete sections off the front of `buffer`,
    /// pushing each CRC-valid one onto `out`. Leaves any trailing partial
    /// section (and its still-unknown or known `expected_length`) in
    /// `buffer` for the next call. Stops (and resets state) on stuffing
    /// bytes (`table_id == 0xFF`) or an invalid `section_length`.
    fn drain_sections(&mut self, out: &mut Vec<Vec<u8>>) {
        loop {
            if self.buffer.is_empty() {
                self.expected_length = None;
                return;
            }

            // 0xFF marks stuffing bytes filling the remainder of the TS
            // packets up to the next section start; nothing meaningful
            // follows in this stream.
            if self.buffer[0] == 0xFF {
                self.clear();
                return;
            }

            if self.buffer.len() < 3 {
                // Header itself straddles a packet boundary - wait for more
                // data before we can even read section_length.
                self.expected_length = None;
                return;
            }

            let section_length = ((self.buffer[1] as usize & 0x0F) << 8) | self.buffer[2] as usize;
            if section_length > MAX_SECTION_LENGTH {
                trace!(
                    "[SectionCollector] invalid section_length={} (table_id={:#04x}), discarding",
                    section_length,
                    self.buffer[0]
                );
                self.clear();
                return;
            }

            let total = 3 + section_length;
            self.expected_length = Some(total);

            if self.buffer.len() < total {
                // Not enough data yet for this section; wait for more.
                return;
            }

            let candidate = &self.buffer[..total];
            // CRC-32/MPEG-2 over the whole section (header + data + CRC)
            // is 0 for a valid section, given this implementation's
            // init=0xFFFFFFFF / no final XOR.
            if crc32_mpeg2(candidate) == 0 {
                out.push(candidate.to_vec());
            } else {
                trace!(
                    "[SectionCollector] CRC mismatch for section (table_id={:#04x}, len={}), discarding",
                    self.buffer[0],
                    total
                );
            }

            self.buffer.drain(0..total);
            self.expected_length = None;
            // Loop again: there may be another section (or stuffing)
            // immediately following in the buffer.
        }
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

    /// Build a complete, CRC-valid PSI section: `table_id` + header byte
    /// (section_syntax_indicator + high nibble of length) + length low byte
    /// + `data_after_header` (extended header fields + payload, if any) + a
    /// correct trailing CRC32.
    fn make_section(
        table_id: u8,
        section_syntax_indicator: bool,
        data_after_header: &[u8],
    ) -> Vec<u8> {
        let section_length = data_after_header.len() + 4; // + CRC
        assert!(section_length <= MAX_SECTION_LENGTH);

        let mut out = Vec::with_capacity(3 + section_length);
        out.push(table_id);
        let b1 = (if section_syntax_indicator { 0x80 } else { 0x00 })
            | 0x30 // reserved bits, arbitrary but conventional (11)
            | ((section_length >> 8) as u8 & 0x0F);
        out.push(b1);
        out.push((section_length & 0xFF) as u8);
        out.extend_from_slice(data_after_header);

        let crc = crc32_mpeg2(&out);
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }

    /// Wrap `pointer` + `body` into a TS-payload-shaped byte vector for a
    /// PUSI packet (pointer_field followed by the actual byte stream).
    /// `body[..pointer]` is the "before" segment (tail of a prior section)
    /// and `body[pointer..]` is the "after" segment (start of the next
    /// section stream) — same layout `SectionCollector::add_data` expects.
    fn pusi_payload(pointer: u8, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + body.len());
        out.push(pointer);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn test_crc32_empty() {
        // CRC32 of empty data with initial value 0xFFFFFFFF
        let crc = crc32_mpeg2(&[]);
        assert_eq!(crc, 0xFFFFFFFF);
    }

    #[test]
    fn test_section_is_crc_valid_property() {
        // Sanity-check the "CRC of a full valid section, CRC included, is
        // zero" property that `SectionCollector::drain_sections` relies on.
        let section = make_section(0x00, true, &[0x00, 0x01, 0xC1, 0x00, 0x00]);
        assert_eq!(crc32_mpeg2(&section), 0);
    }

    #[test]
    fn test_section_collector_single_section() {
        let mut collector = SectionCollector::new();
        assert!(collector.is_empty());

        let section = make_section(0x00, true, &[0x00, 0x01, 0xC1, 0x00, 0x00]);
        let payload = pusi_payload(0, &section);

        let sections = collector.add_data(&payload, 0, true);
        assert_eq!(sections, vec![section]);
        assert!(collector.is_empty());
    }

    /// Defect 1: a section's tail fragment and the next section's head are
    /// both present, spanning a single PUSI packet boundary (pointer_field
    /// > 0). Both sections must be recovered.
    #[test]
    fn test_tail_and_head_share_pusi_packet() {
        let mut collector = SectionCollector::new();

        let section1 = make_section(
            0x4E,
            true,
            &[0x00, 0x01, 0xE1, 0x00, 0x00, 0x00, 0x00, 0x00],
        );
        let section2 = make_section(
            0x4E,
            true,
            &[0x00, 0x02, 0xE1, 0x00, 0x00, 0x11, 0x22, 0x33],
        );

        // Packet A: PUSI, pointer=0, only the first part of section1 (rest
        // arrives in packet B).
        let split = section1.len() - 3;
        let payload_a = pusi_payload(0, &section1[..split]);
        let sections_a = collector.add_data(&payload_a, 0, true);
        assert!(
            sections_a.is_empty(),
            "section1 is still incomplete after packet A"
        );

        // Packet B: PUSI, pointer = remaining bytes of section1, followed by
        // section2 in full.
        let remaining = section1.len() - split;
        let mut body = section1[split..].to_vec();
        body.extend_from_slice(&section2);
        let payload_b = pusi_payload(remaining as u8, &body);

        let sections_b = collector.add_data(&payload_b, 1, true);
        assert_eq!(sections_b, vec![section1, section2]);
    }

    /// Defect 2: two complete sections back-to-back in the same PUSI
    /// packet (pointer_field == 0). Both must be recovered.
    #[test]
    fn test_two_sections_back_to_back() {
        let mut collector = SectionCollector::new();

        let section1 = make_section(0x4E, true, &[0x00, 0x01, 0xE1, 0x00, 0x00]);
        let section2 = make_section(0x4E, true, &[0x00, 0x02, 0xE1, 0x00, 0x00]);

        let mut body = section1.clone();
        body.extend_from_slice(&section2);
        let payload = pusi_payload(0, &body);

        let sections = collector.add_data(&payload, 0, true);
        assert_eq!(sections, vec![section1, section2]);
    }

    #[test]
    fn test_accepts_maximum_4096_byte_section() {
        // section_length=4093 means 3-byte header + 4093 bytes = 4096,
        // the EIT/private-section maximum from B10 §5.2.7.
        let section = make_section(0x50, true, &vec![0x00; 4089]);
        assert_eq!(section.len(), 4096);
        let mut collector = SectionCollector::new();
        let first = pusi_payload(0, &section[..100]);
        let mut sections = collector.add_data(&first, 0, true);
        sections.extend(collector.add_data(&section[100..], 1, false));
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].len(), 4096);
    }

    /// Defect 3: the 3-byte section header itself straddles a packet
    /// boundary, so `section_length` can't be computed on the PUSI packet
    /// and must be recomputed once the continuation packet arrives.
    #[test]
    fn test_header_straddles_packet_boundary() {
        let mut collector = SectionCollector::new();

        let section = make_section(0x4E, true, &[0x00, 0x01, 0xE1, 0x00, 0x00, 0xAA, 0xBB]);

        // Packet A: PUSI, pointer=0, only the first 2 header bytes.
        let payload_a = pusi_payload(0, &section[..2]);
        let sections_a = collector.add_data(&payload_a, 0, true);
        assert!(sections_a.is_empty());

        // Packet B: continuation carrying the rest of the section.
        let payload_b = &section[2..];
        let sections_b = collector.add_data(payload_b, 1, false);
        assert_eq!(sections_b, vec![section]);
    }

    /// Defect fix regression: a corrupted (CRC-invalid) section must never
    /// be emitted.
    #[test]
    fn test_crc_invalid_section_discarded() {
        let mut collector = SectionCollector::new();

        let mut section = make_section(0x4E, true, &[0x00, 0x01, 0xE1, 0x00, 0x00]);
        // Corrupt a data byte so the CRC no longer matches.
        let last = section.len() - 5;
        section[last] ^= 0xFF;

        let payload = pusi_payload(0, &section);
        let sections = collector.add_data(&payload, 0, true);
        assert!(sections.is_empty());
    }

    /// table_id == 0xFF marks stuffing; the collector must stop there and
    /// reset instead of trying to parse stuffing bytes as a section header.
    #[test]
    fn test_stuffing_byte_stops_collection() {
        let mut collector = SectionCollector::new();

        let section = make_section(0x4E, true, &[0x00, 0x01, 0xE1, 0x00, 0x00]);
        let mut body = section.clone();
        body.extend_from_slice(&[0xFF; 8]); // stuffing to end of packet

        let payload = pusi_payload(0, &body);
        let sections = collector.add_data(&payload, 0, true);
        assert_eq!(sections, vec![section]);
        assert!(collector.is_empty());
    }

    #[test]
    fn test_section_collector_clear() {
        let mut collector = SectionCollector::new();
        let section = make_section(0x00, true, &[0x00, 0x01, 0xC1, 0x00, 0x00]);
        let payload_a = pusi_payload(0, &section[..section.len() - 2]);
        collector.add_data(&payload_a, 0, true);
        assert!(!collector.is_empty());

        collector.clear();
        assert!(collector.is_empty());
    }
}
