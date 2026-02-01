//! TS Stream Analyzer - Main analysis engine.
//!
//! This module provides the main `TsAnalyzer` struct that processes
//! TS packets and extracts channel information (PAT, PMT, NIT, SDT).

use std::collections::HashMap;

use super::nit::NitTable;
use super::packet::{TsPacket, TS_PACKET_SIZE};
use super::pat::PatTable;
use super::pmt::PmtTable;
use super::psi::{PsiSection, SectionCollector};
use super::sdt::SdtTable;
use super::{pid, table_id};

/// Configuration for the TS analyzer.
#[derive(Debug, Clone)]
pub struct AnalyzerConfig {
    /// Whether to parse NIT.
    pub parse_nit: bool,
    /// Whether to parse SDT.
    pub parse_sdt: bool,
    /// Whether to parse PMT for all programs.
    pub parse_all_pmts: bool,
    /// Maximum number of packets to process (0 = unlimited).
    pub max_packets: usize,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            parse_nit: true,
            parse_sdt: true,
            parse_all_pmts: true,
            max_packets: 0,
        }
    }
}

/// Result of TS analysis.
#[derive(Debug, Clone, Default)]
pub struct AnalyzerResult {
    /// Network ID (from NIT).
    pub network_id: Option<u16>,
    /// Transport stream ID (from PAT).
    pub transport_stream_id: Option<u16>,
    /// Network name (from NIT).
    pub network_name: Option<String>,
    /// PAT table.
    pub pat: Option<PatTable>,
    /// NIT table.
    pub nit: Option<NitTable>,
    /// SDT table.
    pub sdt: Option<SdtTable>,
    /// PMT tables by program number.
    pub pmts: HashMap<u16, PmtTable>,
    /// Total packets processed.
    pub packets_processed: usize,
    /// Analysis complete flag.
    pub complete: bool,
}

impl AnalyzerResult {
    /// Get channel info for a specific service ID.
    pub fn get_channel_info(&self, service_id: u16) -> Option<ChannelInfo> {
        let pat = self.pat.as_ref()?;

        // Find PMT PID
        let _pmt_pid = pat.get_pmt_pid(service_id)?;

        // Get PMT
        let pmt = self.pmts.get(&service_id);

        // Get service name from SDT
        let service_name = self
            .sdt
            .as_ref()
            .and_then(|sdt| sdt.get_service_name(service_id))
            .map(|s| s.to_string());

        // Get service type from SDT
        let service_type = self
            .sdt
            .as_ref()
            .and_then(|sdt| sdt.find_service(service_id))
            .and_then(|s| s.get_service_type());

        Some(ChannelInfo {
            network_id: self.network_id,
            transport_stream_id: self.transport_stream_id,
            service_id,
            service_name,
            service_type,
            video_pid: pmt.and_then(|p| p.get_video_pids().first().copied()),
            audio_pids: pmt.map(|p| p.get_audio_pids()).unwrap_or_default(),
        })
    }

    /// Get all channel info.
    pub fn get_all_channels(&self) -> Vec<ChannelInfo> {
        let Some(pat) = &self.pat else {
            return Vec::new();
        };

        pat.get_all_program_numbers()
            .into_iter()
            .filter_map(|sid| self.get_channel_info(sid))
            .collect()
    }

    /// Check if analysis has gathered minimum required info.
    pub fn has_minimum_info(&self) -> bool {
        self.pat.is_some()
    }

    /// Check if analysis is complete (all tables received).
    pub fn is_complete(&self, config: &AnalyzerConfig) -> bool {
        if self.pat.is_none() {
            return false;
        }

        if config.parse_nit && self.nit.is_none() {
            return false;
        }

        if config.parse_sdt && self.sdt.is_none() {
            return false;
        }

        if config.parse_all_pmts {
            let pat = self.pat.as_ref().unwrap();
            for program_number in pat.get_all_program_numbers() {
                if !self.pmts.contains_key(&program_number) {
                    return false;
                }
            }
        }

        true
    }
}

/// Channel information extracted from TS.
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    /// Network ID.
    pub network_id: Option<u16>,
    /// Transport stream ID.
    pub transport_stream_id: Option<u16>,
    /// Service ID.
    pub service_id: u16,
    /// Service name.
    pub service_name: Option<String>,
    /// Service type.
    pub service_type: Option<u8>,
    /// Primary video PID.
    pub video_pid: Option<u16>,
    /// Audio PIDs.
    pub audio_pids: Vec<u16>,
}

/// TS Stream Analyzer.
#[derive(Debug)]
pub struct TsAnalyzer {
    /// Configuration.
    config: AnalyzerConfig,
    /// Analysis result.
    result: AnalyzerResult,
    /// Section collectors by PID.
    collectors: HashMap<u16, SectionCollector>,
    /// PMT PIDs to watch (from PAT).
    pmt_pids: HashMap<u16, u16>, // PID -> program_number
}

impl TsAnalyzer {
    /// Create a new analyzer with the given configuration.
    pub fn new(config: AnalyzerConfig) -> Self {
        Self {
            config,
            result: AnalyzerResult::default(),
            collectors: HashMap::new(),
            pmt_pids: HashMap::new(),
        }
    }

    /// Create a new analyzer with default configuration.
    pub fn new_default() -> Self {
        Self::new(AnalyzerConfig::default())
    }

    /// Feed TS data to the analyzer.
    ///
    /// Returns true if analysis is complete.
    pub fn feed(&mut self, data: &[u8]) -> bool {
        let mut offset = 0;

        // Find first sync byte
        while offset < data.len() && data[offset] != 0x47 {
            offset += 1;
        }

        // Process packets
        while offset + TS_PACKET_SIZE <= data.len() {
            if data[offset] != 0x47 {
                // Lost sync, resync
                offset += 1;
                while offset < data.len() && data[offset] != 0x47 {
                    offset += 1;
                }
                continue;
            }

            if let Ok(packet) = TsPacket::parse(&data[offset..]) {
                self.process_packet(&packet);
                self.result.packets_processed += 1;

                // Check max packets limit
                if self.config.max_packets > 0
                    && self.result.packets_processed >= self.config.max_packets
                {
                    self.result.complete = true;
                    return true;
                }

                // Check if complete
                if self.result.is_complete(&self.config) {
                    self.result.complete = true;
                    return true;
                }
            }

            offset += TS_PACKET_SIZE;
        }

        self.result.complete
    }

    /// Process a single TS packet.
    fn process_packet(&mut self, packet: &TsPacket) {
        let pid_val = packet.header.pid;

        // Skip null packets and packets with errors
        if pid_val == pid::NULL || packet.header.transport_error {
            return;
        }

        // Skip scrambled packets
        if packet.header.is_scrambled() {
            return;
        }

        // Check if we're interested in this PID
        let should_process = pid_val == pid::PAT
            || (self.config.parse_nit && pid_val == pid::NIT)
            || (self.config.parse_sdt && pid_val == pid::SDT)
            || self.pmt_pids.contains_key(&pid_val);

        if !should_process || !packet.header.has_payload() {
            return;
        }

        // Get or create section collector
        let collector = self.collectors.entry(pid_val).or_default();

        // Add payload to collector
        let complete = collector.add_data(
            packet.payload,
            packet.header.continuity_counter,
            packet.header.payload_unit_start,
        );

        if complete {
            // Clone section data to avoid borrow conflicts
            if let Some(section_data) = collector.get_section().map(|s| s.to_vec()) {
                // Clear collector first to end mutable borrow
                collector.clear();
                // Process the section
                self.process_section(pid_val, &section_data);
            }
        }
    }

    /// Process a complete PSI section.
    fn process_section(&mut self, pid_val: u16, data: &[u8]) {
        let section = match PsiSection::parse(data) {
            Ok(s) => s,
            Err(_) => return,
        };

        match pid_val {
            pid::PAT => self.process_pat(&section),
            pid::NIT => self.process_nit(&section),
            pid::SDT => self.process_sdt(&section),
            _ => {
                // Check if this is a PMT PID
                if let Some(&program_number) = self.pmt_pids.get(&pid_val) {
                    self.process_pmt(&section, program_number);
                }
            }
        }
    }

    /// Process PAT section.
    fn process_pat(&mut self, section: &PsiSection) {
        if section.header.table_id != table_id::PAT {
            return;
        }

        // Skip if we already have PAT with same or newer version
        if let Some(ref existing) = self.result.pat {
            if existing.version_number >= section.header.version_number {
                return;
            }
        }

        if let Ok(pat) = PatTable::parse(section) {
            // Update PMT PIDs to watch
            self.pmt_pids.clear();
            for entry in &pat.programs {
                if self.config.parse_all_pmts {
                    self.pmt_pids.insert(entry.pid, entry.program_number);
                }
            }

            self.result.transport_stream_id = Some(pat.transport_stream_id);
            self.result.pat = Some(pat);
        }
    }

    /// Process NIT section.
    fn process_nit(&mut self, section: &PsiSection) {
        if section.header.table_id != table_id::NIT_ACTUAL {
            return;
        }

        // Skip if we already have NIT with same or newer version
        if let Some(ref existing) = self.result.nit {
            if existing.version_number >= section.header.version_number {
                return;
            }
        }

        if let Ok(nit) = NitTable::parse(section) {
            self.result.network_id = Some(nit.network_id);
            self.result.network_name = nit.network_name.clone();
            self.result.nit = Some(nit);
        }
    }

    /// Process SDT section.
    fn process_sdt(&mut self, section: &PsiSection) {
        if section.header.table_id != table_id::SDT_ACTUAL {
            return;
        }

        // Skip if we already have SDT with same or newer version
        if let Some(ref existing) = self.result.sdt {
            if existing.version_number >= section.header.version_number {
                return;
            }
        }

        if let Ok(sdt) = SdtTable::parse(section) {
            self.result.sdt = Some(sdt);
        }
    }

    /// Process PMT section.
    fn process_pmt(&mut self, section: &PsiSection, expected_program: u16) {
        if section.header.table_id != table_id::PMT {
            return;
        }

        // Verify program number matches
        if section.header.table_id_extension != expected_program {
            return;
        }

        // Skip if we already have PMT with same or newer version
        if let Some(existing) = self.result.pmts.get(&expected_program) {
            if existing.version_number >= section.header.version_number {
                return;
            }
        }

        if let Ok(pmt) = PmtTable::parse(section) {
            self.result.pmts.insert(expected_program, pmt);
        }
    }

    /// Get the current analysis result.
    pub fn result(&self) -> &AnalyzerResult {
        &self.result
    }

    /// Take the analysis result, consuming the analyzer.
    pub fn into_result(self) -> AnalyzerResult {
        self.result
    }

    /// Reset the analyzer state.
    pub fn reset(&mut self) {
        self.result = AnalyzerResult::default();
        self.collectors.clear();
        self.pmt_pids.clear();
    }

    /// Check if analysis is complete.
    pub fn is_complete(&self) -> bool {
        self.result.complete
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_analyzer_config_default() {
        let config = AnalyzerConfig::default();
        assert!(config.parse_nit);
        assert!(config.parse_sdt);
        assert!(config.parse_all_pmts);
        assert_eq!(config.max_packets, 0);
    }

    #[test]
    fn test_analyzer_new() {
        let analyzer = TsAnalyzer::new_default();
        assert!(!analyzer.is_complete());
        assert_eq!(analyzer.result().packets_processed, 0);
    }

    #[test]
    fn test_analyzer_result_has_minimum_info() {
        let mut result = AnalyzerResult::default();
        assert!(!result.has_minimum_info());

        result.pat = Some(PatTable::default());
        assert!(result.has_minimum_info());
    }

    #[test]
    fn test_analyzer_result_is_complete() {
        let config = AnalyzerConfig {
            parse_nit: true,
            parse_sdt: true,
            parse_all_pmts: false,
            max_packets: 0,
        };

        let mut result = AnalyzerResult::default();
        assert!(!result.is_complete(&config));

        result.pat = Some(PatTable::default());
        assert!(!result.is_complete(&config)); // Missing NIT and SDT

        result.nit = Some(NitTable::default());
        assert!(!result.is_complete(&config)); // Missing SDT

        result.sdt = Some(SdtTable::default());
        assert!(result.is_complete(&config)); // All required tables present
    }

    #[test]
    fn test_analyzer_result_get_channel_info() {
        use crate::ts_analyzer::descriptors::ServiceDescriptor;
        use crate::ts_analyzer::pat::PatEntry;
        use crate::ts_analyzer::pmt::stream_type;
        use crate::ts_analyzer::pmt::PmtStream;
        use crate::ts_analyzer::sdt::SdtService;

        let mut result = AnalyzerResult::default();

        // Setup PAT
        result.pat = Some(PatTable {
            transport_stream_id: 0x7FE1,
            version_number: 0,
            programs: vec![PatEntry {
                program_number: 0x0101,
                pid: 0x0100,
            }],
            nit_pid: Some(0x0010),
        });
        result.transport_stream_id = Some(0x7FE1);

        // Setup PMT
        let mut pmts = HashMap::new();
        pmts.insert(
            0x0101,
            PmtTable {
                program_number: 0x0101,
                version_number: 0,
                pcr_pid: 0x0100,
                program_info: vec![],
                streams: vec![
                    PmtStream {
                        stream_type: stream_type::H264_VIDEO,
                        elementary_pid: 0x0100,
                        descriptors: vec![],
                    },
                    PmtStream {
                        stream_type: stream_type::AAC_AUDIO,
                        elementary_pid: 0x0110,
                        descriptors: vec![],
                    },
                ],
            },
        );
        result.pmts = pmts;

        // Setup SDT
        result.sdt = Some(SdtTable {
            transport_stream_id: 0x7FE1,
            original_network_id: 0x7FE0,
            version_number: 0,
            services: vec![SdtService {
                service_id: 0x0101,
                eit_schedule_flag: false,
                eit_present_following_flag: true,
                running_status: 4,
                free_ca_mode: false,
                descriptors: vec![],
                service_descriptor: Some(ServiceDescriptor {
                    service_type: 0x01,
                    provider_name: "Provider".to_string(),
                    service_name: "Test Channel".to_string(),
                }),
            }],
        });

        // Setup NIT
        result.network_id = Some(0x7FE0);

        // Get channel info
        let info = result.get_channel_info(0x0101).unwrap();
        assert_eq!(info.network_id, Some(0x7FE0));
        assert_eq!(info.transport_stream_id, Some(0x7FE1));
        assert_eq!(info.service_id, 0x0101);
        assert_eq!(info.service_name, Some("Test Channel".to_string()));
        assert_eq!(info.service_type, Some(0x01));
        assert_eq!(info.video_pid, Some(0x0100));
        assert_eq!(info.audio_pids, vec![0x0110]);
    }
}
