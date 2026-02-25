use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use log::{debug, warn};

use crate::ts_analyzer::{
    descriptor_tag, parse_descriptor_loop, PsiSection, SdtTable, SectionCollector, TsPacket,
    TS_PACKET_SIZE, table_id,
};

const SDT_PID: u16 = 0x0011;
const CDT_PID: u16 = 0x0029;
const CDT_TABLE_ID: u8 = 0xC8;

/// PNG file signature (8 bytes).
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Minimum PNG data size below which the logo is considered transparent/empty.
/// This threshold comes from TVTest/LibISDB which skips logos with DataSize <= 93.
const MIN_LOGO_DATA_SIZE: usize = 94;

/// Logo data extracted from a CDT section.
struct CdtLogoData {
    network_id: u16,
    logo_id: u16,
    logo_type: u8,
    png: Vec<u8>,
}

/// Lightweight logo collector.
///
/// It listens SDT/CDT sections from live TS and saves discovered PNG logo bytes
/// to logos/{nid}_{sid}.png.  The CDT data-module header is parsed according to
/// ARIB STD-B21 (same logic as TVTest / LibISDB) so that the PNG payload is
/// extracted using the explicit `data_size` field rather than heuristic scanning.
pub struct ChannelLogoCollector {
    sdt_collector: SectionCollector,
    cdt_collector: SectionCollector,
    current_nid: Option<u16>,
    current_service_ids: Vec<u16>,
    current_service_logo_ids: HashMap<u16, u16>,
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
            current_service_logo_ids: HashMap::new(),
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
        self.current_service_logo_ids.clear();

        for svc in &sdt.services {
            if let Some(logo_id) = extract_logo_id_from_sdt_descriptors(&svc.descriptors) {
                self.current_service_logo_ids.insert(svc.service_id, logo_id);
            }
        }
    }

    fn process_cdt_section(&mut self, section_data: &[u8]) {
        let Ok(section) = PsiSection::parse(section_data) else {
            return;
        };

        if section.header.table_id != CDT_TABLE_ID {
            return;
        }

        // Verify section CRC32 to reject corrupted data (TVTest/LibISDB does
        // this inside PSIStreamTable before calling OnTableUpdate).
        if !section.verify_crc(section_data) {
            debug!("[LogoCollector] CDT section CRC error, skipping");
            return;
        }

        let Some(logo) = extract_logo_from_cdt_section(&section) else {
            return;
        };

        // Prefer original_network_id from CDT; fall back to SDT NID.
        let nid = if logo.network_id != 0 {
            logo.network_id
        } else if let Some(sdt_nid) = self.current_nid {
            sdt_nid
        } else {
            return;
        };

        if self.current_service_ids.is_empty() {
            return;
        }

        let target_sids: Vec<u16> = if logo.logo_id > 0 {
            let matched: Vec<u16> = self
                .current_service_logo_ids
                .iter()
                .filter_map(|(sid, lid)| if *lid == logo.logo_id { Some(*sid) } else { None })
                .collect();

            if matched.is_empty() {
                self.current_service_ids.clone()
            } else {
                matched
            }
        } else {
            self.current_service_ids.clone()
        };

        for sid in &target_sids {
            let key = format!("{}_{}", nid, sid);
            if self.saved_keys.contains(&key) {
                continue;
            }

            let path = self.output_dir.join(format!("{}_{}.png", nid, sid));
            if path.exists() {
                self.saved_keys.insert(key);
                continue;
            }

            match fs::write(&path, &logo.png) {
                Ok(_) => {
                    self.saved_keys.insert(key);
                    debug!(
                        "[LogoCollector] Saved logo type={} id={} nid={} as {:?}",
                        logo.logo_type, logo.logo_id, nid, path
                    );
                }
                Err(e) => {
                    warn!("[LogoCollector] Failed to save logo {:?}: {}", path, e);
                }
            }
        }
    }
}

fn extract_logo_id_from_sdt_descriptors(descriptors: &[u8]) -> Option<u16> {
    for (tag, data) in parse_descriptor_loop(descriptors) {
        if tag != descriptor_tag::LOGO_TRANSMISSION {
            continue;
        }

        // logo_transmission_descriptor
        // transmission_type==0x01 の場合、logo_id を含む
        // [0]=type, [1]=....|logo_id[8], [2]=logo_id[7:0]
        if data.len() < 3 {
            continue;
        }

        let transmission_type = data[0];
        if transmission_type != 0x01 {
            continue;
        }

        let logo_id = (((data[1] & 0x01) as u16) << 8) | data[2] as u16;
        if logo_id > 0 {
            return Some(logo_id);
        }
    }

    None
}

/// Extract logo PNG data from a CDT section following the ARIB STD-B21 data
/// module format.  This mirrors the logic in TVTest / LibISDB
/// (`LogoDownloaderFilter::OnCDTSection`).
///
/// CDT data-module for logo (`data_type == 0x01`):
/// ```text
///   logo_type:             8 bits  [0]
///   reserved_future_use:   7 bits  [1] upper 7
///   logo_id:               9 bits  [1] bit0 + [2]
///   reserved_future_use:   4 bits  [3] upper 4
///   logo_version:         12 bits  [3] lower 4 + [4]
///   data_size:            16 bits  [5..7]  (big-endian)
///   data_byte:     data_size bytes [7..]   <- PNG payload
/// ```
fn extract_logo_from_cdt_section(section: &PsiSection<'_>) -> Option<CdtLogoData> {
    let d = section.data;
    if d.len() < 5 {
        return None;
    }

    // section.data layout (after 8-byte PSI extended header):
    //   [0..2] original_network_id
    //   [2]    data_type
    //   [3..5] reserved(4) + descriptor_loop_length(12)
    //   descriptors …
    //   data_module_byte …
    let original_network_id = ((d[0] as u16) << 8) | d[1] as u16;

    let data_type = d[2];
    if data_type != 0x01 {
        // Not logo data
        return None;
    }

    let desc_len = (((d[3] & 0x0F) as usize) << 8) | d[4] as usize;
    let module_start = 5 + desc_len;

    // Need at least 7 bytes for data-module header
    if d.len() < module_start + 7 {
        return None;
    }

    let module = &d[module_start..];
    let module_len = module.len();

    let logo_type = module[0];
    if logo_type > 0x05 {
        return None;
    }

    let logo_id = (((module[1] as u16) & 0x01) << 8) | module[2] as u16;
    // logo_version (unused but parsed for correctness):
    // let _logo_version = (((module[3] as u16) & 0x0F) << 8) | module[4] as u16;
    let data_size = ((module[5] as usize) << 8) | module[6] as usize;

    // Validate: data fits within the module, and within the CDT section
    if data_size == 0 || 7 + data_size > module_len {
        return None;
    }

    // Skip transparent / very-small logos (TVTest: DataSize <= 93)
    if data_size < MIN_LOGO_DATA_SIZE {
        return None;
    }

    let png_data = &module[7..7 + data_size];

    // Verify PNG signature
    if data_size < PNG_SIGNATURE.len() || !png_data.starts_with(&PNG_SIGNATURE) {
        debug!("[LogoCollector] CDT data-module does not start with PNG signature");
        return None;
    }

    Some(CdtLogoData {
        network_id: original_network_id,
        logo_id,
        logo_type,
        png: png_data.to_vec(),
    })
}
