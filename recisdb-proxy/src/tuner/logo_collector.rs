use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use log::{debug, warn};

use crate::ts_analyzer::{PsiSection, SdtTable, SectionCollector, TsPacket, TS_PACKET_SIZE, table_id};

const SDT_PID: u16 = 0x0011;
const CDT_PID: u16 = 0x0029;
const CDT_TABLE_ID: u8 = 0xC8;

/// Lightweight logo collector.
///
/// It listens SDT/CDT sections from live TS and saves discovered PNG logo bytes
/// to logos/{nid}_{sid}.png. This is a best-effort implementation.
pub struct ChannelLogoCollector {
    sdt_collector: SectionCollector,
    cdt_collector: SectionCollector,
    current_nid: Option<u16>,
    current_service_ids: Vec<u16>,
    saved_keys: HashSet<String>,
    output_dir: PathBuf,
}

impl ChannelLogoCollector {
    pub fn new() -> Self {
        let output_dir = PathBuf::from("logos");
        if let Err(e) = fs::create_dir_all(&output_dir) {
            warn!("[LogoCollector] Failed to create logo directory {:?}: {}", output_dir, e);
        }

        Self {
            sdt_collector: SectionCollector::new(),
            cdt_collector: SectionCollector::new(),
            current_nid: None,
            current_service_ids: Vec::new(),
            saved_keys: HashSet::new(),
            output_dir,
        }
    }

    pub fn process_ts_chunk(&mut self, data: &[u8]) {
        let mut offset = 0usize;
        while offset + TS_PACKET_SIZE <= data.len() {
            if data[offset] != 0x47 {
                offset += 1;
                continue;
            }

            if let Ok(packet) = TsPacket::parse(&data[offset..offset + TS_PACKET_SIZE]) {
                self.process_packet(&packet);
            }

            offset += TS_PACKET_SIZE;
        }
    }

    fn process_packet(&mut self, packet: &TsPacket<'_>) {
        if packet.header.transport_error || packet.header.is_scrambled() || !packet.header.has_payload() {
            return;
        }

        match packet.header.pid {
            SDT_PID => {
                let complete = self.sdt_collector.add_data(
                    packet.payload,
                    packet.header.continuity_counter,
                    packet.header.payload_unit_start,
                );
                if complete {
                    if let Some(section_data) = self.sdt_collector.get_section() {
                        let section_data = section_data.to_vec();
                        self.sdt_collector.clear();
                        self.process_sdt_section(&section_data);
                    }
                }
            }
            CDT_PID => {
                let complete = self.cdt_collector.add_data(
                    packet.payload,
                    packet.header.continuity_counter,
                    packet.header.payload_unit_start,
                );
                if complete {
                    if let Some(section_data) = self.cdt_collector.get_section() {
                        let section_data = section_data.to_vec();
                        self.cdt_collector.clear();
                        self.process_cdt_section(&section_data);
                    }
                }
            }
            _ => {}
        }
    }

    fn process_sdt_section(&mut self, section_data: &[u8]) {
        let Ok(section) = PsiSection::parse(section_data) else {
            return;
        };
        if section.header.table_id != table_id::SDT_ACTUAL {
            return;
        }

        let Ok(sdt) = SdtTable::parse(&section) else {
            return;
        };

        self.current_nid = Some(sdt.original_network_id);
        self.current_service_ids = sdt.services.iter().map(|s| s.service_id).collect();
    }

    fn process_cdt_section(&mut self, section_data: &[u8]) {
        let Ok(section) = PsiSection::parse(section_data) else {
            return;
        };

        if section.header.table_id != CDT_TABLE_ID {
            return;
        }

        let Some(png) = extract_png(section_data) else {
            return;
        };

        let Some(nid) = self.current_nid else {
            return;
        };

        if self.current_service_ids.is_empty() {
            return;
        }

        for sid in &self.current_service_ids {
            let key = format!("{}_{}", nid, sid);
            if self.saved_keys.contains(&key) {
                continue;
            }

            let path = self.output_dir.join(format!("{}_{}.png", nid, sid));
            if path.exists() {
                self.saved_keys.insert(key);
                continue;
            }

            match fs::write(&path, &png) {
                Ok(_) => {
                    self.saved_keys.insert(key);
                    debug!("[LogoCollector] Saved logo {:?}", path);
                }
                Err(e) => {
                    warn!("[LogoCollector] Failed to save logo {:?}: {}", path, e);
                }
            }
        }
    }
}

fn extract_png(data: &[u8]) -> Option<Vec<u8>> {
    const SIG: &[u8] = b"\x89PNG\r\n\x1a\n";
    const IEND: &[u8] = b"IEND\xAE\x42\x60\x82";

    let start = data.windows(SIG.len()).position(|w| w == SIG)?;
    let end_rel = data[start..].windows(IEND.len()).position(|w| w == IEND)?;
    let end = start + end_rel + IEND.len();
    Some(data[start..end].to_vec())
}
