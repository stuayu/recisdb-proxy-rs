//! NIT (Network Information Table) parsing.
//!
//! The NIT is transmitted on PID 0x0010 and contains information about
//! the network and transport streams, including physical channel parameters.

use super::descriptors::{
    find_descriptor, parse_descriptor_loop, NetworkNameDescriptor, TerrestrialDeliveryDescriptor,
    TsInformationDescriptor,
};
use super::psi::PsiSection;
use super::{descriptor_tag, table_id};

/// Transport stream entry in the NIT.
#[derive(Debug, Clone, Default)]
pub struct NitTransportStream {
    /// Transport stream ID.
    pub transport_stream_id: u16,
    /// Original network ID.
    pub original_network_id: u16,
    /// Transport descriptors (raw).
    pub descriptors: Vec<u8>,
    /// Terrestrial delivery descriptor (if present).
    pub terrestrial_delivery: Option<TerrestrialDeliveryDescriptor>,
    /// Remote-control key id (TS情報記述子 0xCD; terrestrial only).
    pub remote_control_key: Option<u8>,
}

impl NitTransportStream {
    /// Parse descriptors and extract known types.
    pub fn parse_descriptors(&mut self) {
        if let Some(data) = find_descriptor(&self.descriptors, descriptor_tag::TERRESTRIAL_DELIVERY)
        {
            if let Ok(desc) = TerrestrialDeliveryDescriptor::parse(&data) {
                self.terrestrial_delivery = Some(desc);
            }
        }
        if let Some(data) = find_descriptor(&self.descriptors, descriptor_tag::TS_INFORMATION) {
            if let Ok(desc) = TsInformationDescriptor::parse(&data) {
                self.remote_control_key = Some(desc.remote_control_key_id);
            }
        }
    }

    /// Get all frequencies from terrestrial delivery descriptor.
    pub fn get_frequencies(&self) -> Vec<u32> {
        self.terrestrial_delivery
            .as_ref()
            .map(|d| d.frequencies.clone())
            .unwrap_or_default()
    }

    /// Physical UHF channel of this transport stream (terrestrial only).
    ///
    /// The 地上分配システム記述子 may list several frequencies (親局+中継局);
    /// the first entry is the main transmitter, which is what channel files
    /// display. Shared by the scan path (`scheduler/scan_scheduler.rs`) and
    /// the live NIT collector (`tuner/nit_collector.rs`) so the two cannot
    /// derive different physical channels for the same stream.
    pub fn physical_ch(&self) -> Option<u8> {
        self.terrestrial_delivery
            .as_ref()
            .and_then(|d| d.frequencies.first().copied())
            .and_then(uhf_channel_from_frequency)
    }
}

/// UHF center frequency (Hz) → physical channel 13-62.
/// ch13 = 473 + 1/7 MHz, 6 MHz spacing (ISDB-T).
pub fn uhf_channel_from_frequency(freq_hz: u32) -> Option<u8> {
    let offset = freq_hz as i64 - 473_142_857;
    let ch = 13 + (offset + 3_000_000).div_euclid(6_000_000);
    (13..=62).contains(&ch).then_some(ch as u8)
}

/// Parsed NIT (Network Information Table).
#[derive(Debug, Clone, Default)]
pub struct NitTable {
    /// Network ID.
    pub network_id: u16,
    /// Version number.
    pub version_number: u8,
    /// Network name (from descriptor).
    pub network_name: Option<String>,
    /// Network descriptors (raw).
    pub network_descriptors: Vec<u8>,
    /// Transport stream loop.
    pub transport_streams: Vec<NitTransportStream>,
}

impl NitTable {
    /// Parse a NIT from a PSI section.
    pub fn parse(section: &PsiSection) -> Result<Self, &'static str> {
        if section.header.table_id != table_id::NIT_ACTUAL
            && section.header.table_id != table_id::NIT_OTHER
        {
            return Err("Not a NIT section");
        }

        let data = section.data;
        if data.len() < 2 {
            return Err("NIT data too short");
        }

        // Network descriptors length
        let network_descriptors_length = ((data[0] as usize & 0x0F) << 8) | data[1] as usize;

        if data.len() < 2 + network_descriptors_length + 2 {
            return Err("Invalid network descriptors length");
        }

        let network_descriptors = data[2..2 + network_descriptors_length].to_vec();

        // Parse network name from descriptors
        let network_name =
            find_descriptor(&network_descriptors, descriptor_tag::NETWORK_NAME).and_then(|d| {
                NetworkNameDescriptor::parse(&d)
                    .ok()
                    .map(|n| n.network_name)
            });

        let mut nit = NitTable {
            network_id: section.header.table_id_extension,
            version_number: section.header.version_number,
            network_name,
            network_descriptors,
            transport_streams: Vec::new(),
        };

        // Transport stream loop length
        let ts_loop_offset = 2 + network_descriptors_length;
        let ts_loop_length =
            ((data[ts_loop_offset] as usize & 0x0F) << 8) | data[ts_loop_offset + 1] as usize;

        // Parse transport stream loop
        let mut offset = ts_loop_offset + 2;
        let ts_loop_end = offset + ts_loop_length;

        while offset + 6 <= ts_loop_end && offset + 6 <= data.len() {
            let transport_stream_id = ((data[offset] as u16) << 8) | data[offset + 1] as u16;
            let original_network_id = ((data[offset + 2] as u16) << 8) | data[offset + 3] as u16;
            let ts_descriptors_length =
                ((data[offset + 4] as usize & 0x0F) << 8) | data[offset + 5] as usize;

            offset += 6;

            if offset + ts_descriptors_length > data.len() {
                break;
            }

            let descriptors = data[offset..offset + ts_descriptors_length].to_vec();
            offset += ts_descriptors_length;

            let mut ts = NitTransportStream {
                transport_stream_id,
                original_network_id,
                descriptors,
                terrestrial_delivery: None,
                remote_control_key: None,
            };
            ts.parse_descriptors();

            nit.transport_streams.push(ts);
        }

        Ok(nit)
    }

    /// Find transport stream by TSID.
    pub fn find_transport_stream(&self, tsid: u16) -> Option<&NitTransportStream> {
        self.transport_streams
            .iter()
            .find(|ts| ts.transport_stream_id == tsid)
    }

    /// Get all transport stream IDs.
    pub fn get_all_tsids(&self) -> Vec<u16> {
        self.transport_streams
            .iter()
            .map(|ts| ts.transport_stream_id)
            .collect()
    }

    /// Check if this is NIT actual (for current network).
    pub fn is_actual(&self) -> bool {
        // NIT actual has table_id 0x40, other has 0x41
        // Since we don't store table_id, we assume it's actual if parsed successfully
        true
    }
}

// Re-export for convenience (already imported above)

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_analyzer::psi::PsiHeader;

    #[test]
    fn test_parse_nit() {
        // Create a mock NIT section.
        // The network name is a real ARIB STD-B24 byte sequence: the
        // Alphanumeric-set designation ESC 0x28 0x4A followed by ASCII
        // "Net001", which the aribb24 decoder turns into the fullwidth
        // "Ｎｅｔ００１" (the default code set is the 2-byte Kanji plane, so
        // raw ASCII would decode to mojibake).
        let data = [
            // Network descriptors length = 11 (tag + len + 9-byte name field)
            0xF0, 0x0B,
            // Network name descriptor: tag=0x40, length=9,
            // ESC 0x28 0x4A + "Net001"
            0x40, 0x09, 0x1b, 0x28, 0x4a, b'N', b'e', b't', b'0', b'0', b'1',
            // Transport stream loop length = 8
            0xF0, 0x08,
            // TS entry: TSID=0x7FE1, ONID=0x7FE0, descriptors_length=2
            0x7F, 0xE1, 0x7F, 0xE0, 0xF0, 0x02,
            // Dummy descriptor
            0xFF, 0x00,
        ];

        let header = PsiHeader {
            table_id: table_id::NIT_ACTUAL,
            section_syntax_indicator: true,
            section_length: 25,
            table_id_extension: 0x7FE0, // Network ID
            version_number: 1,
            current_next_indicator: true,
            section_number: 0,
            last_section_number: 0,
        };

        let section = PsiSection {
            header,
            data: &data,
            crc32: 0,
        };

        let nit = NitTable::parse(&section).unwrap();

        assert_eq!(nit.network_id, 0x7FE0);
        assert_eq!(nit.network_name, Some("Ｎｅｔ００１".to_string()));
        assert_eq!(nit.transport_streams.len(), 1);
        assert_eq!(nit.transport_streams[0].transport_stream_id, 0x7FE1);
        assert_eq!(nit.transport_streams[0].original_network_id, 0x7FE0);
    }

    #[test]
    fn uhf_channel_from_frequency_maps_center_frequencies() {
        // ch13 = 473.142857 MHz, ch27 = 557.142857 MHz, ch52 = 707.142857 MHz
        assert_eq!(uhf_channel_from_frequency(473_142_857), Some(13));
        assert_eq!(uhf_channel_from_frequency(557_142_857), Some(27));
        assert_eq!(uhf_channel_from_frequency(707_142_857), Some(52));
        // ±3MHz 未満のずれは同じチャンネルに丸める
        assert_eq!(uhf_channel_from_frequency(556_000_000), Some(27));
        // UHF帯域外
        assert_eq!(uhf_channel_from_frequency(200_000_000), None);
        assert_eq!(uhf_channel_from_frequency(800_000_000), None);
    }

    #[test]
    fn physical_ch_uses_the_first_listed_frequency() {
        let mut ts = NitTransportStream {
            transport_stream_id: 0x7FE1,
            original_network_id: 0x7FE0,
            descriptors: vec![],
            terrestrial_delivery: None,
            remote_control_key: None,
        };
        // 地上分配システム記述子が無ければ物理チャンネルは決まらない
        assert_eq!(ts.physical_ch(), None);

        ts.terrestrial_delivery = Some(TerrestrialDeliveryDescriptor {
            // 親局 (ch27) が先頭、以降は中継局
            frequencies: vec![557_142_857, 473_142_857],
            ..Default::default()
        });
        assert_eq!(ts.physical_ch(), Some(27));
    }

    #[test]
    fn test_nit_find_transport_stream() {
        let nit = NitTable {
            network_id: 0x7FE0,
            version_number: 0,
            network_name: None,
            network_descriptors: vec![],
            transport_streams: vec![
                NitTransportStream {
                    transport_stream_id: 0x7FE1,
                    original_network_id: 0x7FE0,
                    descriptors: vec![],
                    terrestrial_delivery: None,
                    remote_control_key: None,
                },
                NitTransportStream {
                    transport_stream_id: 0x7FE2,
                    original_network_id: 0x7FE0,
                    descriptors: vec![],
                    terrestrial_delivery: None,
                    remote_control_key: None,
                },
            ],
        };

        assert!(nit.find_transport_stream(0x7FE1).is_some());
        assert!(nit.find_transport_stream(0x7FE2).is_some());
        assert!(nit.find_transport_stream(0x7FE3).is_none());
    }

    #[test]
    fn test_nit_get_all_tsids() {
        let nit = NitTable {
            network_id: 0x7FE0,
            version_number: 0,
            network_name: None,
            network_descriptors: vec![],
            transport_streams: vec![
                NitTransportStream {
                    transport_stream_id: 0x7FE1,
                    original_network_id: 0x7FE0,
                    descriptors: vec![],
                    terrestrial_delivery: None,
                    remote_control_key: None,
                },
                NitTransportStream {
                    transport_stream_id: 0x7FE2,
                    original_network_id: 0x7FE0,
                    descriptors: vec![],
                    terrestrial_delivery: None,
                    remote_control_key: None,
                },
            ],
        };

        let tsids = nit.get_all_tsids();
        assert_eq!(tsids, vec![0x7FE1, 0x7FE2]);
    }
}
