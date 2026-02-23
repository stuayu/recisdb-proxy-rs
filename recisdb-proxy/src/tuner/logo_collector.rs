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

/// Lightweight logo collector.
///
/// It listens SDT/CDT sections from live TS and saves discovered PNG logo bytes
/// to logos/{nid}_{sid}.png. This is a best-effort implementation.
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

        let Some((logo_id, png)) = extract_logo_png_from_cdt_section(&section) else {
            return;
        };

        let Some(nid) = self.current_nid else {
            return;
        };

        if self.current_service_ids.is_empty() {
            return;
        }

        let target_sids: Vec<u16> = if let Some(logo_id) = logo_id {
            let matched: Vec<u16> = self
                .current_service_logo_ids
                .iter()
                .filter_map(|(sid, lid)| if *lid == logo_id { Some(*sid) } else { None })
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

fn extract_logo_png_from_cdt_section(section: &PsiSection<'_>) -> Option<(Option<u16>, Vec<u8>)> {
    // ARIB CDT(0xC8): data_type + descriptor_loop + data_module_byte
    // Mirakurun同様、CDTのデータモジュールからロゴを取り出す方針。
    let d = section.data;
    if d.len() < 3 {
        return None;
    }

    let data_type = d[0];
    if data_type != 0x01 {
        return None;
    }

    let desc_len = (((d[1] & 0x0F) as usize) << 8) | d[2] as usize;
    if d.len() < 3 + desc_len {
        return None;
    }

    let module = &d[3 + desc_len..];

    // best-effort logo_id parse (logo data module先頭)
    let logo_id = if module.len() >= 3 {
        let lid = (((module[1] & 0x01) as u16) << 8) | module[2] as u16;
        if lid > 0 { Some(lid) } else { None }
    } else {
        None
    };

    // Prefer module scan, then whole data scan as fallback.
    let png = extract_png(module).or_else(|| extract_png(d))?;
    Some((logo_id, png))
}

fn extract_png(data: &[u8]) -> Option<Vec<u8>> {
    // Mirakurun では CDT のロゴデータをデコードして PNG 化している。
    // 本実装では外部デコーダ非依存のため、少なくとも PNG チャンク境界を厳密に検証し、
    // 途中切れや誤検出（IDAT内の偶然の IEND パターン）を回避する。
    const SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    let mut sig_pos = 0usize;
    while let Some(rel) = data[sig_pos..].windows(SIG.len()).position(|w| w == SIG) {
        let start = sig_pos + rel;
        let mut p = start + SIG.len();
        let mut seen_ihdr = false;

        loop {
            // chunk: length(4) + type(4) + data(length) + crc(4)
            if p + 12 > data.len() {
                break;
            }

            let len = u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
            let ctype = &data[p + 4..p + 8];

            // PNG chunk type must be alphabetic ASCII letters
            if !ctype.iter().all(|b| b.is_ascii_alphabetic()) {
                break;
            }

            let chunk_end = p + 12 + len;
            if chunk_end > data.len() {
                break;
            }

            if ctype == b"IHDR" {
                // IHDR must be the first chunk and length must be 13
                if seen_ihdr || len != 13 {
                    break;
                }
                seen_ihdr = true;
            }

            if ctype == b"IEND" {
                // IEND must have zero-length payload
                if !seen_ihdr || len != 0 {
                    break;
                }
                return Some(data[start..chunk_end].to_vec());
            }

            p = chunk_end;
        }

        // Continue scanning after this signature position
        sig_pos = start + 1;
        if sig_pos >= data.len() {
            break;
        }
    }

    None
}
