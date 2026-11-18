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

use log::{debug, trace, warn};

use super::packet::{TsPacket, SYNC_BYTE, TS_PACKET_SIZE};
use super::pat::{PatEntry, PatTable};
use super::pmt::PmtTable;
use super::psi::{crc32_mpeg2, PsiSection, SectionCollector};

/// Well-known PIDs that are always passed through.
///
/// This set matches what the reference Mirakurun emits on its own
/// per-service stream for the same multiplex (compared packet-by-packet
/// 2026-08-12): filtering to one service must not cost a client any SI it
/// would have had from Mirakurun.
const ALWAYS_PASS_PIDS: &[u16] = &[
    0x0000, // PAT (rewritten)
    0x0001, // CAT
    0x0010, // NIT
    0x0011, // SDT
    0x0012, // EIT
    0x0014, // TOT/TDT
    // BIT — carries the `affiliation_id` identifying a station's network
    // (日テレ系/TBS系/…), which no API exposes: EPGStation's stuayu fork
    // harvests it passively from the streams it receives
    // (`src/model/channel/BitParser.ts`), so dropping this PID would silently
    // stop network classification from ever learning a new station. Costs
    // about six packets per eight seconds of stream.
    0x0024,
];

/// Largest PID value (13 bits).
const MAX_PID: u16 = 0x1FFF;

/// Fixed PID membership set: one bit per possible 13-bit PID.
///
/// The filter probes this for every 188-byte packet, so it is a plain bit
/// test instead of a hashed lookup.
struct PidSet([u64; 128]);

impl PidSet {
    fn new() -> Self {
        Self([0; 128])
    }

    /// PIDs above 13 bits can never occur in a packet header, so they are
    /// ignored rather than stored.
    fn insert(&mut self, pid: u16) {
        if pid <= MAX_PID {
            let pid = usize::from(pid);
            self.0[pid >> 6] |= 1 << (pid & 63);
        }
    }

    fn contains(&self, pid: u16) -> bool {
        if pid > MAX_PID {
            return false;
        }
        let pid = usize::from(pid);
        self.0[pid >> 6] & (1 << (pid & 63)) != 0
    }

    fn clear(&mut self) {
        self.0 = [0; 128];
    }

    fn count(&self) -> usize {
        self.0.iter().map(|word| word.count_ones() as usize).sum()
    }
}

fn is_newer_version(current: u8, new: u8) -> bool {
    let delta = (new & 0x1F).wrapping_sub(current & 0x1F) & 0x1F;
    delta != 0 && delta < 16
}

/// TS service filter that passes only packets for a single SID.
pub struct TsServiceFilter {
    /// Target service ID (program_number in PAT).
    target_sid: u16,
    /// Set of PIDs to pass through.
    allowed_pids: PidSet,
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
        let mut allowed_pids = PidSet::new();
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
        // Pre-allocate for worst case (all packets pass)
        let mut output = Vec::with_capacity(data.len());
        self.filter_into(data, &mut output);
        output
    }

    /// Same as [`Self::filter`], but appends the surviving packets to
    /// `output` so the caller can reuse one buffer across chunks.
    pub fn filter_into(&mut self, data: &[u8], output: &mut Vec<u8>) {
        let packet_count = data.len() / TS_PACKET_SIZE;

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
            if self.allowed_pids.contains(pid) {
                output.extend_from_slice(pkt_data);
            }
            // All other PIDs are dropped
        }
    }

    /// Process a PAT packet and update PMT PID for the target SID.
    fn process_pat_packet(&mut self, pkt_data: &[u8]) {
        let Ok(packet) = TsPacket::parse(pkt_data) else {
            return;
        };

        let sections = self.pat_collector.add_data(
            packet.payload,
            packet.header.continuity_counter,
            packet.header.payload_unit_start,
        );

        for section_data in &sections {
            self.process_pat_section(section_data);
        }
    }

    /// Handle a single complete PAT section (there may be several per
    /// packet if they're densely packed).
    fn process_pat_section(&mut self, section_data: &[u8]) {
        let Ok(section) = PsiSection::parse(section_data) else {
            return;
        };
        if !section.header.current_next_indicator {
            return;
        }
        let Ok(pat) = PatTable::parse(&section) else {
            return;
        };

        // Accept every section of the current version, but reject an older
        // table. PAT is allowed to span multiple sections; dropping all
        // same-version sections can hide the target SID when it is not in
        // section 0.
        if let Some(current) = self.pat_version {
            if current != pat.version_number && !is_newer_version(current, pat.version_number) {
                return;
            }
        }

        debug!(
            "TsServiceFilter: PAT version {} -> {}, {} programs",
            self.pat_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".to_string()),
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
                debug!(
                    "TsServiceFilter: PMT PID for SID {} = 0x{:04X}",
                    self.target_sid, pid
                );
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

        let sections = self.pmt_collector.add_data(
            packet.payload,
            packet.header.continuity_counter,
            packet.header.payload_unit_start,
        );

        for section_data in &sections {
            self.process_pmt_section(section_data);
        }
    }

    /// Handle a single complete PMT section (there may be several per
    /// packet if they're densely packed).
    fn process_pmt_section(&mut self, section_data: &[u8]) {
        let Ok(section) = PsiSection::parse(section_data) else {
            return;
        };
        if !section.header.current_next_indicator {
            return;
        }

        let Ok(pmt) = PmtTable::parse(&section) else {
            return;
        };

        if let Some(current) = self.pmt_version {
            if current != pmt.version_number && !is_newer_version(current, pmt.version_number) {
                return;
            }
        }

        debug!(
            "TsServiceFilter: PMT version {} -> {}, {} streams, PCR PID=0x{:04X}",
            self.pmt_version
                .map(|v| v.to_string())
                .unwrap_or_else(|| "none".to_string()),
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
            self.allowed_pids.count(),
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
        assert!(filter.allowed_pids.contains(0x0000)); // PAT
        assert!(filter.allowed_pids.contains(0x0010)); // NIT
        assert!(filter.allowed_pids.contains(0x0011)); // SDT
        assert!(filter.allowed_pids.contains(0x0012)); // EIT
        assert!(filter.allowed_pids.contains(0x0014)); // TOT/TDT
        assert!(!filter.is_ready());
    }

    /// BIT is the only source of a station's network affiliation, and
    /// EPGStation collects it from the stream — see [`ALWAYS_PASS_PIDS`].
    #[test]
    fn bit_is_passed_through_so_affiliations_can_be_harvested() {
        let filter = TsServiceFilter::new(0x0400);
        assert!(filter.allowed_pids.contains(0x0024));

        let mut filter = filter;
        filter.reset();
        assert!(
            filter.allowed_pids.contains(0x0024),
            "reset must restore it too"
        );
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

    /// PAT carrying one program `sid` -> `pmt_pid`, in a single TS packet.
    fn pat_packet(sid: u16, pmt_pid: u16) -> Vec<u8> {
        let mut section = vec![0x00, 0xB0, 0x00, 0x00, 0x01, 0xC1, 0x00, 0x00];
        section.extend_from_slice(&sid.to_be_bytes());
        section.extend_from_slice(&(0xE000 | pmt_pid).to_be_bytes());
        finish_section(&mut section);
        ts_packet(0x0000, 0, &section)
    }

    /// PMT for `sid` on `pmt_pid` with one H.264 video ES on `es_pid`.
    fn pmt_packet(sid: u16, pmt_pid: u16, es_pid: u16) -> Vec<u8> {
        let mut section = vec![0x02, 0xB0, 0x00];
        section.extend_from_slice(&sid.to_be_bytes());
        section.extend_from_slice(&[0xC1, 0x00, 0x00]);
        section.extend_from_slice(&(0xE000 | es_pid).to_be_bytes()); // PCR PID
        section.extend_from_slice(&[0xF0, 0x00]); // program_info_length = 0
        section.push(0x1B); // H.264 video
        section.extend_from_slice(&(0xE000 | es_pid).to_be_bytes());
        section.extend_from_slice(&[0xF0, 0x00]); // ES_info_length = 0
        finish_section(&mut section);
        ts_packet(pmt_pid, 0, &section)
    }

    /// Fill in section_length and append the CRC32 of a PSI section whose
    /// first three bytes are a placeholder header.
    fn finish_section(section: &mut Vec<u8>) {
        let length = (section.len() - 3 + 4) as u16;
        section[1] = 0xB0 | (length >> 8) as u8;
        section[2] = length as u8;
        let crc = crc32_mpeg2(section);
        section.extend_from_slice(&crc.to_be_bytes());
    }

    /// One TS packet with PUSI set, a pointer field of 0, and `section` as
    /// payload, padded with 0xFF.
    fn ts_packet(pid: u16, cc: u8, section: &[u8]) -> Vec<u8> {
        let mut packet = vec![
            SYNC_BYTE,
            0x40 | ((pid >> 8) as u8 & 0x1F),
            (pid & 0xFF) as u8,
            0x10 | (cc & 0x0F),
            0x00,
        ];
        packet.extend_from_slice(section);
        packet.resize(TS_PACKET_SIZE, 0xFF);
        packet
    }

    /// Plain payload-only packet on `pid`, used as the data under test.
    fn data_packet(pid: u16) -> Vec<u8> {
        let mut packet = vec![SYNC_BYTE, (pid >> 8) as u8 & 0x1F, (pid & 0xFF) as u8, 0x10];
        packet.resize(TS_PACKET_SIZE, 0x55);
        packet
    }

    #[test]
    fn pid_inserted_from_pmt_is_passed_and_unrelated_pid_is_dropped() {
        let mut filter = TsServiceFilter::new(0x0400);
        let mut input = Vec::new();
        input.extend(pat_packet(0x0400, 0x0100));
        input.extend(pmt_packet(0x0400, 0x0100, 0x0101));
        input.extend(data_packet(0x0101)); // ES listed in the PMT
        input.extend(data_packet(0x0200)); // not in any table for this service

        let out = filter.filter(&input);

        assert!(filter.is_ready());
        assert!(filter.allowed_pids.contains(0x0101));
        assert!(!filter.allowed_pids.contains(0x0200));
        let pids: Vec<u16> = out
            .chunks(TS_PACKET_SIZE)
            .map(|p| ((p[1] as u16 & 0x1F) << 8) | p[2] as u16)
            .collect();
        assert!(pids.contains(&0x0101), "ES packet must pass: {pids:?}");
        assert!(
            !pids.contains(&0x0200),
            "unrelated packet must drop: {pids:?}"
        );
    }

    #[test]
    fn pid_set_covers_the_full_13_bit_range() {
        let mut set = PidSet::new();
        assert_eq!(set.count(), 0);
        for pid in [0x0000, 0x003F, 0x0040, 0x0101, 0x1FFE, MAX_PID] {
            assert!(!set.contains(pid));
            set.insert(pid);
            assert!(set.contains(pid));
        }
        assert!(!set.contains(0x0102));
        assert_eq!(set.count(), 6);
        // Out-of-range PIDs are never stored and never match.
        set.insert(0x2000);
        assert!(!set.contains(0x2000));
        set.clear();
        assert_eq!(set.count(), 0);
        assert!(!set.contains(0x0101));
    }

    #[test]
    fn filter_into_appends_to_the_existing_buffer() {
        let mut filter = TsServiceFilter::new(0x0400);
        let input = data_packet(0x0014); // always-pass PID
        let mut out = vec![0xAB; 3];
        filter.filter_into(&input, &mut out);
        assert_eq!(&out[..3], &[0xAB; 3]);
        assert_eq!(&out[3..], &input[..]);
    }
}
