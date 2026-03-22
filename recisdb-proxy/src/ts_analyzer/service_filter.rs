//! Single-service TS packet filter.
//!
//! Filters an MPEG-TS stream to pass only packets belonging to a specific
//! service (identified by SID / program_number).  This includes:
//!
//! - PAT (PID 0x0000) — rewritten to contain only the target program entry
//! - PMT — the Program Map Table for the target SID
//! - All Elementary Stream PIDs listed in that PMT (video, audio, etc.)
//! - Essential SI/PSI tables: NIT, SDT, EIT, TOT/TDT, CAT
//!
//! TS packets for other programs are replaced with null packets (PID 0x1FFF)
//! to maintain constant bitrate, or simply dropped from the output.
//!
//! # Architecture
//!
//! The filter is per-session and stateful: it tracks the PAT and PMT of the
//! target service to dynamically update the PID whitelist.  When the PAT or
//! PMT changes (version update), the whitelist is rebuilt automatically.

use std::collections::HashSet;

use log::{debug, trace, warn};

use super::packet::{TsPacket, TS_PACKET_SIZE, SYNC_BYTE};
use super::pat::{PatTable, PatEntry};
use super::pmt::PmtTable;
use super::psi::{PsiSection, SectionCollector, crc32_mpeg2};

/// Well-known PIDs that are always passed through.
const ALWAYS_PASS_PIDS: &[u16] = &[
    0x0000, // PAT (rewritten)
    0x0001, // CAT
    0x0010, // NIT
    0x0011, // SDT
    0x0012, // EIT
    0x0014, // TOT/TDT
];

/// TS service filter that passes only packets for a single SID.
pub struct TsServiceFilter {
    /// Target service ID (program_number in PAT).
    target_sid: u16,
    /// Set of PIDs to pass through.
    allowed_pids: HashSet<u16>,
    /// PMT PID for the target service (from PAT).
    pmt_pid: Option<u16>,
    /// PAT section collector.
    pat_collector: SectionCollector,
    /// PMT section collector.
    pmt_collector: SectionCollector,
    /// Last PAT version seen.
    pat_version: Option<u8>,
    /// Last PMT version seen.
    pmt_version: Option<u8>,
    /// Pre-built rewritten PAT section bytes (full TS packet(s)).
    rewritten_pat_packets: Vec<u8>,
    /// PAT continuity counter for rewritten PAT packets.
    pat_cc: u8,
    /// Whether the filter is ready (PAT and PMT both parsed).
    ready: bool,
}

impl TsServiceFilter {
    /// Create a new service filter for the given SID.
    pub fn new(target_sid: u16) -> Self {
        let mut allowed_pids = HashSet::new();
        for &pid in ALWAYS_PASS_PIDS {
            allowed_pids.insert(pid);
        }

        Self {
            target_sid,
            allowed_pids,
            pmt_pid: None,
            pat_collector: SectionCollector::new(),
            pmt_collector: SectionCollector::new(),
            pat_version: None,
            pmt_version: None,
            rewritten_pat_packets: Vec::new(),
            pat_cc: 0,
            ready: false,
        }
    }

    /// Change the target SID and reset state.
    pub fn set_target_sid(&mut self, sid: u16) {
        self.target_sid = sid;
        self.reset();
    }

    /// Reset all state (call on channel change).
    pub fn reset(&mut self) {
        self.allowed_pids.clear();
        for &pid in ALWAYS_PASS_PIDS {
            self.allowed_pids.insert(pid);
        }
        self.pmt_pid = None;
        self.pat_collector.clear();
        self.pmt_collector.clear();
        self.pat_version = None;
        self.pmt_version = None;
        self.rewritten_pat_packets.clear();
        self.pat_cc = 0;
        self.ready = false;
    }

    /// Filter a TS data chunk.
    ///
    /// The input must be aligned to 188-byte boundaries.
    /// Returns filtered data containing only packets for the target service.
    /// Non-target packets are dropped (not null-stuffed) to reduce bandwidth.
    pub fn filter(&mut self, data: &[u8]) -> Vec<u8> {
        let packet_count = data.len() / TS_PACKET_SIZE;
        // Pre-allocate for worst case (all packets pass)
        let mut output = Vec::with_capacity(data.len());

        for i in 0..packet_count {
            let offset = i * TS_PACKET_SIZE;
            let pkt_data = &data[offset..offset + TS_PACKET_SIZE];

            if pkt_data[0] != SYNC_BYTE {
                continue;
            }

            let pid = ((pkt_data[1] as u16 & 0x1F) << 8) | pkt_data[2] as u16;

            // Process PAT to track PMT PID
            if pid == 0x0000 {
                self.process_pat_packet(pkt_data);
                // Output rewritten PAT instead of original
                if !self.rewritten_pat_packets.is_empty() {
                    output.extend_from_slice(&self.rewritten_pat_packets);
                    // Clear after outputting once per original PAT packet
                    // (rewritten PAT is emitted when we see a new PAT)
                }
                continue;
            }

            // Process PMT to track ES PIDs
            if Some(pid) == self.pmt_pid {
                self.process_pmt_packet(pkt_data);
                // Always pass through PMT packets
                output.extend_from_slice(pkt_data);
                continue;
            }

            // Pass through allowed PIDs
            if self.allowed_pids.contains(&pid) {
                output.extend_from_slice(pkt_data);
            }
            // All other PIDs are dropped
        }

        output
    }

    /// Process a PAT packet and update PMT PID for the target SID.
    fn process_pat_packet(&mut self, pkt_data: &[u8]) {
        let Ok(packet) = TsPacket::parse(pkt_data) else {
            return;
        };

        let complete = self.pat_collector.add_data(
            packet.payload,
            packet.header.continuity_counter,
            packet.header.payload_unit_start,
        );

        if !complete {
            return;
        }

        let Some(section_data) = self.pat_collector.get_section() else {
            return;
        };

        let Ok(pat) = PatTable::parse(&PsiSection::parse(section_data).unwrap()) else {
            return;
        };

        // Check version change
        if self.pat_version == Some(pat.version_number) {
            return;
        }

        debug!(
            "TsServiceFilter: PAT version {} -> {}, {} programs",
            self.pat_version.map(|v| v.to_string()).unwrap_or_else(|| "none".to_string()),
            pat.version_number,
            pat.programs.len()
        );

        self.pat_version = Some(pat.version_number);

        // Find our target SID in the PAT
        let mut found_pmt_pid = None;
        for entry in &pat.programs {
            if entry.program_number == self.target_sid {
                found_pmt_pid = Some(entry.pid);
                break;
            }
        }

        if let Some(pid) = found_pmt_pid {
            let pid_changed = self.pmt_pid != Some(pid);
            self.pmt_pid = Some(pid);
            self.allowed_pids.insert(pid);

            if pid_changed {
                // PMT PID changed, reset PMT tracking
                self.pmt_collector.clear();
                self.pmt_version = None;
                self.ready = false;
                debug!("TsServiceFilter: PMT PID for SID {} = 0x{:04X}", self.target_sid, pid);
            }
        } else {
            warn!(
                "TsServiceFilter: target SID {} not found in PAT ({} programs)",
                self.target_sid,
                pat.programs.len()
            );
        }

        // Build rewritten PAT containing only our target SID (and NIT entry)
        self.build_rewritten_pat(&pat);
    }

    /// Process a PMT packet and update ES PID whitelist.
    fn process_pmt_packet(&mut self, pkt_data: &[u8]) {
        let Ok(packet) = TsPacket::parse(pkt_data) else {
            return;
        };

        let complete = self.pmt_collector.add_data(
            packet.payload,
            packet.header.continuity_counter,
            packet.header.payload_unit_start,
        );

        if !complete {
            return;
        }

        let Some(section_data) = self.pmt_collector.get_section() else {
            return;
        };

        let Ok(section) = PsiSection::parse(section_data) else {
            return;
        };

        let Ok(pmt) = PmtTable::parse(&section) else {
            return;
        };

        // Check version change
        if self.pmt_version == Some(pmt.version_number) {
            return;
        }

        debug!(
            "TsServiceFilter: PMT version {} -> {}, {} streams, PCR PID=0x{:04X}",
            self.pmt_version.map(|v| v.to_string()).unwrap_or_else(|| "none".to_string()),
            pmt.version_number,
            pmt.streams.len(),
            pmt.pcr_pid,
        );

        self.pmt_version = Some(pmt.version_number);

        // Rebuild allowed PIDs: keep base set + PMT PID + all ES PIDs from this PMT
        self.allowed_pids.clear();
        for &pid in ALWAYS_PASS_PIDS {
            self.allowed_pids.insert(pid);
        }
        if let Some(pmt_pid) = self.pmt_pid {
            self.allowed_pids.insert(pmt_pid);
        }

        // PCR PID
        if pmt.pcr_pid != 0x1FFF {
            self.allowed_pids.insert(pmt.pcr_pid);
        }

        // Elementary stream PIDs
        for stream in &pmt.streams {
            self.allowed_pids.insert(stream.elementary_pid);
            trace!(
                "TsServiceFilter: Allow ES PID 0x{:04X} (type=0x{:02X})",
                stream.elementary_pid,
                stream.stream_type
            );
        }

        self.ready = true;
        debug!(
            "TsServiceFilter: Ready, {} PIDs allowed for SID {}",
            self.allowed_pids.len(),
            self.target_sid
        );
    }

    /// Build rewritten PAT packets containing only the target SID entry.
    fn build_rewritten_pat(&mut self, original_pat: &PatTable) {
        // Build PAT section payload:
        // - NIT entry (program_number=0, NIT PID) if present
        // - Target SID entry (program_number=target_sid, PMT PID)
        let mut section_body = Vec::new();

        // NIT entry
        if let Some(nit_pid) = original_pat.nit_pid {
            section_body.push(0x00); // program_number high
            section_body.push(0x00); // program_number low
            section_body.push((0xE0 | ((nit_pid >> 8) & 0x1F)) as u8);
            section_body.push((nit_pid & 0xFF) as u8);
        }

        // Target SID entry
        if let Some(pmt_pid) = self.pmt_pid {
            section_body.push((self.target_sid >> 8) as u8);
            section_body.push((self.target_sid & 0xFF) as u8);
            section_body.push((0xE0 | ((pmt_pid >> 8) & 0x1F)) as u8);
            section_body.push((pmt_pid & 0xFF) as u8);
        }

        // Build full PSI section
        // table_id(1) + flags+length(2) + tsid(2) + version+cni(1) + section_number(1) + last_section_number(1)
        // + body + CRC32(4)
        let section_data_len = section_body.len() + 5 + 4; // 5 bytes after length field (before body) + CRC
        let mut section = Vec::with_capacity(3 + section_data_len);

        // Table ID
        section.push(0x00); // PAT table_id

        // Section syntax indicator + reserved + section length
        let section_length = section_data_len as u16;
        section.push(0xB0 | ((section_length >> 8) & 0x0F) as u8);
        section.push((section_length & 0xFF) as u8);

        // Transport stream ID
        section.push((original_pat.transport_stream_id >> 8) as u8);
        section.push((original_pat.transport_stream_id & 0xFF) as u8);

        // Version number + current_next_indicator
        section.push(0xC1 | (original_pat.version_number << 1));

        // Section number
        section.push(0x00);
        // Last section number
        section.push(0x00);

        // Program entries
        section.extend_from_slice(&section_body);

        // CRC32
        let crc = crc32_mpeg2(&section);
        section.push((crc >> 24) as u8);
        section.push(((crc >> 16) & 0xFF) as u8);
        section.push(((crc >> 8) & 0xFF) as u8);
        section.push((crc & 0xFF) as u8);

        // Pack into TS packet(s)
        self.rewritten_pat_packets.clear();

        // For typical PAT with 1-2 entries, it fits in a single TS packet
        let payload_capacity = TS_PACKET_SIZE - 4 - 1; // 4 byte header + 1 byte pointer field
        if section.len() <= payload_capacity {
            let mut pkt = [0xFFu8; TS_PACKET_SIZE];

            // Header
            pkt[0] = SYNC_BYTE;
            pkt[1] = 0x40; // payload_unit_start=1, PID=0x0000(high)
            pkt[2] = 0x00; // PID=0x0000(low)
            pkt[3] = 0x10 | (self.pat_cc & 0x0F); // adaptation_field_control=01 (payload only) + CC
            self.pat_cc = (self.pat_cc + 1) & 0x0F;

            // Pointer field
            pkt[4] = 0x00;

            // Section data
            pkt[5..5 + section.len()].copy_from_slice(&section);

            self.rewritten_pat_packets.extend_from_slice(&pkt);
        }
    }

    /// Returns true if the filter has parsed both PAT and PMT and is ready.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Returns the current target SID.
    pub fn target_sid(&self) -> u16 {
        self.target_sid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_filter_has_base_pids() {
        let filter = TsServiceFilter::new(0x0400);
        assert!(filter.allowed_pids.contains(&0x0000)); // PAT
        assert!(filter.allowed_pids.contains(&0x0010)); // NIT
        assert!(filter.allowed_pids.contains(&0x0011)); // SDT
        assert!(filter.allowed_pids.contains(&0x0012)); // EIT
        assert!(filter.allowed_pids.contains(&0x0014)); // TOT/TDT
        assert!(!filter.is_ready());
    }

    #[test]
    fn test_reset_clears_state() {
        let mut filter = TsServiceFilter::new(0x0400);
        filter.pmt_pid = Some(0x0100);
        filter.ready = true;
        filter.reset();
        assert!(!filter.is_ready());
        assert!(filter.pmt_pid.is_none());
    }
}
