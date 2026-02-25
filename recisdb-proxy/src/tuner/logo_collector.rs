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

    // Convert ARIB PNG (missing PLTE/tRNS) to standard PNG so regular viewers
    // can open it.
    let standard_png = convert_arib_png_to_standard(png_data);

    Some(CdtLogoData {
        network_id: original_network_id,
        logo_id,
        logo_type,
        png: standard_png,
    })
}

// ---------------------------------------------------------------------------
// ARIB STD-B24 default CLUT (128 entries).
// Each entry is [R, G, B, A].  This table is taken from TVTest's
// `DefaultPalette` in Codec_PNG.cpp.
// ---------------------------------------------------------------------------
#[rustfmt::skip]
const ARIB_CLUT: [[u8; 4]; 128] = [
    [  0,   0,   0, 255], [255,   0,   0, 255], [  0, 255,   0, 255], [255, 255,   0, 255],
    [  0,   0, 255, 255], [255,   0, 255, 255], [  0, 255, 255, 255], [255, 255, 255, 255],
    [  0,   0,   0,   0], [170,   0,   0, 255], [  0, 170,   0, 255], [170, 170,   0, 255],
    [  0,   0, 170, 255], [170,   0, 170, 255], [  0, 170, 170, 255], [170, 170, 170, 255],
    [  0,   0,  85, 255], [  0,  85,   0, 255], [  0,  85,  85, 255], [  0,  85, 170, 255],
    [  0,  85, 255, 255], [  0, 170,  85, 255], [  0, 170, 255, 255], [  0, 255,  85, 255],
    [  0, 255, 170, 255], [ 85,   0,   0, 255], [ 85,   0,  85, 255], [ 85,   0, 170, 255],
    [ 85,   0, 255, 255], [ 85,  85,   0, 255], [ 85,  85,  85, 255], [ 85,  85, 170, 255],
    [ 85,  85, 255, 255], [ 85, 170,   0, 255], [ 85, 170,  85, 255], [ 85, 170, 170, 255],
    [ 85, 170, 255, 255], [ 85, 255,   0, 255], [ 85, 255,  85, 255], [ 85, 255, 170, 255],
    [ 85, 255, 255, 255], [170,   0,  85, 255], [170,   0, 255, 255], [170,  85,   0, 255],
    [170,  85,  85, 255], [170,  85, 170, 255], [170,  85, 255, 255], [170, 170,  85, 255],
    [170, 170, 255, 255], [170, 255,   0, 255], [170, 255,  85, 255], [170, 255, 170, 255],
    [170, 255, 255, 255], [255,   0,  85, 255], [255,   0, 170, 255], [255,  85,   0, 255],
    [255,  85,  85, 255], [255,  85, 170, 255], [255,  85, 255, 255], [255, 170,   0, 255],
    [255, 170,  85, 255], [255, 170, 170, 255], [255, 170, 255, 255], [255, 255,  85, 255],
    [255, 255, 170, 255], [  0,   0,   0, 128], [255,   0,   0, 128], [  0, 255,   0, 128],
    [255, 255,   0, 128], [  0,   0, 255, 128], [255,   0, 255, 128], [  0, 255, 255, 128],
    [255, 255, 255, 128], [170,   0,   0, 128], [  0, 170,   0, 128], [170, 170,   0, 128],
    [  0,   0, 170, 128], [170,   0, 170, 128], [  0, 170, 170, 128], [170, 170, 170, 128],
    [  0,   0,  85, 128], [  0,  85,   0, 128], [  0,  85,  85, 128], [  0,  85, 170, 128],
    [  0,  85, 255, 128], [  0, 170,  85, 128], [  0, 170, 255, 128], [  0, 255,  85, 128],
    [  0, 255, 170, 128], [ 85,   0,   0, 128], [ 85,   0,  85, 128], [ 85,   0, 170, 128],
    [ 85,   0, 255, 128], [ 85,  85,   0, 128], [ 85,  85,  85, 128], [ 85,  85, 170, 128],
    [ 85,  85, 255, 128], [ 85, 170,   0, 128], [ 85, 170,  85, 128], [ 85, 170, 170, 128],
    [ 85, 170, 255, 128], [ 85, 255,   0, 128], [ 85, 255,  85, 128], [ 85, 255, 170, 128],
    [ 85, 255, 255, 128], [170,   0,  85, 128], [170,   0, 255, 128], [170,  85,   0, 128],
    [170,  85,  85, 128], [170,  85, 170, 128], [170,  85, 255, 128], [170, 170,  85, 128],
    [170, 170, 255, 128], [170, 255,   0, 128], [170, 255,  85, 128], [170, 255, 170, 128],
    [170, 255, 255, 128], [255,   0,  85, 128], [255,   0, 170, 128], [255,  85,   0, 128],
    [255,  85,  85, 128], [255,  85, 170, 128], [255,  85, 255, 128], [255, 170,   0, 128],
    [255, 170,  85, 128], [255, 170, 170, 128], [255, 170, 255, 128], [255, 255,  85, 128],
];

/// Convert an ARIB PNG (color type 3 without PLTE chunk) to a standard PNG by
/// injecting PLTE and tRNS chunks derived from the ARIB STD-B24 default CLUT.
///
/// If the PNG already contains a PLTE chunk or uses a different color type,
/// the data is returned as-is.
fn convert_arib_png_to_standard(data: &[u8]) -> Vec<u8> {
    // Minimum: 8 (sig) + 25 (IHDR chunk) = 33
    if data.len() < 33 || !data.starts_with(&PNG_SIGNATURE) {
        return data.to_vec();
    }

    // --- Parse IHDR ---
    // chunk at offset 8: length(4) + "IHDR"(4) + 13 bytes data + CRC(4)
    let ihdr_len = u32::from_be_bytes([data[8], data[9], data[10], data[11]]) as usize;
    if ihdr_len != 13 || &data[12..16] != b"IHDR" {
        return data.to_vec();
    }
    let color_type = data[8 + 4 + 4 + 9]; // offset 25: color_type in IHDR data

    // Only inject palette for indexed-color images (color_type == 3)
    if color_type != 3 {
        return data.to_vec();
    }

    // --- Scan chunks to see if PLTE already exists ---
    let ihdr_chunk_end = 8 + 4 + 4 + ihdr_len + 4; // 33
    let mut pos = ihdr_chunk_end;
    let mut has_plte = false;
    while pos + 8 <= data.len() {
        let chunk_len = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let chunk_type = &data[pos + 4..pos + 8];
        if chunk_type == b"PLTE" {
            has_plte = true;
            break;
        }
        if chunk_type == b"IDAT" || chunk_type == b"IEND" {
            break;
        }
        let next = pos + 12 + chunk_len;
        if next > data.len() {
            break;
        }
        pos = next;
    }

    if has_plte {
        return data.to_vec();
    }

    // --- Build PLTE chunk (256 entries × 3 = 768 bytes) ---
    // Indices 0..127 from ARIB_CLUT, indices 128..255 = CLUT[8] (transparent).
    let mut plte_data = Vec::with_capacity(4 + 4 + 768 + 4);
    plte_data.extend_from_slice(&(768u32).to_be_bytes()); // length
    plte_data.extend_from_slice(b"PLTE");
    for i in 0..256u16 {
        let c = if i < 128 { &ARIB_CLUT[i as usize] } else { &ARIB_CLUT[8] };
        plte_data.push(c[0]); // R
        plte_data.push(c[1]); // G
        plte_data.push(c[2]); // B
    }
    let crc = png_crc32(&plte_data[4..]); // CRC over type + data
    plte_data.extend_from_slice(&crc.to_be_bytes());

    // --- Build tRNS chunk (256 alpha values) ---
    let mut trns_data = Vec::with_capacity(4 + 4 + 256 + 4);
    trns_data.extend_from_slice(&(256u32).to_be_bytes()); // length
    trns_data.extend_from_slice(b"tRNS");
    for i in 0..256u16 {
        let c = if i < 128 { &ARIB_CLUT[i as usize] } else { &ARIB_CLUT[8] };
        trns_data.push(c[3]); // Alpha
    }
    let crc = png_crc32(&trns_data[4..]);
    trns_data.extend_from_slice(&crc.to_be_bytes());

    // --- Reassemble: signature + IHDR + PLTE + tRNS + rest ---
    let insert_pos = ihdr_chunk_end; // right after IHDR chunk
    let mut out = Vec::with_capacity(data.len() + plte_data.len() + trns_data.len());
    out.extend_from_slice(&data[..insert_pos]);
    out.extend_from_slice(&plte_data);
    out.extend_from_slice(&trns_data);
    out.extend_from_slice(&data[insert_pos..]);
    out
}

/// Standard CRC-32 (ISO 3309) used by PNG chunks.
fn png_crc32(data: &[u8]) -> u32 {
    static CRC_TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut n = 0usize;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                if c & 1 != 0 {
                    c = 0xEDB88320 ^ (c >> 1);
                } else {
                    c >>= 1;
                }
                k += 1;
            }
            table[n] = c;
            n += 1;
        }
        table
    };

    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}
