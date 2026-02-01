//! SDT (Service Description Table) parsing.
//!
//! The SDT is transmitted on PID 0x0011 and contains information about
//! services (channels) in a transport stream.

use super::descriptors::{find_descriptor, ServiceDescriptor};
use super::psi::PsiSection;
use super::{descriptor_tag, table_id};

/// Service entry in the SDT.
#[derive(Debug, Clone, Default)]
pub struct SdtService {
    /// Service ID (program number).
    pub service_id: u16,
    /// EIT schedule flag.
    pub eit_schedule_flag: bool,
    /// EIT present/following flag.
    pub eit_present_following_flag: bool,
    /// Running status.
    pub running_status: u8,
    /// Free CA mode.
    pub free_ca_mode: bool,
    /// Service descriptors (raw).
    pub descriptors: Vec<u8>,
    /// Parsed service descriptor.
    pub service_descriptor: Option<ServiceDescriptor>,
}

impl SdtService {
    /// Parse descriptors and extract known types.
    pub fn parse_descriptors(&mut self) {
        if let Some(data) = find_descriptor(&self.descriptors, descriptor_tag::SERVICE) {
            if let Ok(desc) = ServiceDescriptor::parse(&data) {
                self.service_descriptor = Some(desc);
            }
        }
    }

    /// Get service name (from service descriptor).
    pub fn get_service_name(&self) -> Option<&str> {
        self.service_descriptor
            .as_ref()
            .map(|d| d.service_name.as_str())
    }

    /// Get provider name (from service descriptor).
    pub fn get_provider_name(&self) -> Option<&str> {
        self.service_descriptor
            .as_ref()
            .map(|d| d.provider_name.as_str())
    }

    /// Get service type (from service descriptor).
    pub fn get_service_type(&self) -> Option<u8> {
        self.service_descriptor.as_ref().map(|d| d.service_type)
    }

    /// Get running status name.
    pub fn running_status_name(&self) -> &'static str {
        match self.running_status {
            0 => "Undefined",
            1 => "Not running",
            2 => "Starts in a few seconds",
            3 => "Pausing",
            4 => "Running",
            5..=7 => "Reserved",
            _ => "Unknown",
        }
    }
}

/// Parsed SDT (Service Description Table).
#[derive(Debug, Clone, Default)]
pub struct SdtTable {
    /// Transport stream ID.
    pub transport_stream_id: u16,
    /// Original network ID.
    pub original_network_id: u16,
    /// Version number.
    pub version_number: u8,
    /// Services.
    pub services: Vec<SdtService>,
}

impl SdtTable {
    /// Parse a SDT from a PSI section.
    pub fn parse(section: &PsiSection) -> Result<Self, &'static str> {
        if section.header.table_id != table_id::SDT_ACTUAL
            && section.header.table_id != table_id::SDT_OTHER
        {
            return Err("Not a SDT section");
        }

        let data = section.data;
        if data.len() < 3 {
            return Err("SDT data too short");
        }

        let original_network_id = ((data[0] as u16) << 8) | data[1] as u16;
        // data[2] is reserved

        let mut sdt = SdtTable {
            transport_stream_id: section.header.table_id_extension,
            original_network_id,
            version_number: section.header.version_number,
            services: Vec::new(),
        };

        // Parse service loop
        let mut offset = 3;
        while offset + 5 <= data.len() {
            let service_id = ((data[offset] as u16) << 8) | data[offset + 1] as u16;
            let eit_schedule_flag = data[offset + 2] & 0x02 != 0;
            let eit_present_following_flag = data[offset + 2] & 0x01 != 0;
            let running_status = (data[offset + 3] >> 5) & 0x07;
            let free_ca_mode = data[offset + 3] & 0x10 != 0;
            let descriptors_length =
                ((data[offset + 3] as usize & 0x0F) << 8) | data[offset + 4] as usize;

            offset += 5;

            if offset + descriptors_length > data.len() {
                break;
            }

            let descriptors = data[offset..offset + descriptors_length].to_vec();
            offset += descriptors_length;

            let mut service = SdtService {
                service_id,
                eit_schedule_flag,
                eit_present_following_flag,
                running_status,
                free_ca_mode,
                descriptors,
                service_descriptor: None,
            };
            service.parse_descriptors();

            sdt.services.push(service);
        }

        Ok(sdt)
    }

    /// Find service by service ID.
    pub fn find_service(&self, service_id: u16) -> Option<&SdtService> {
        self.services.iter().find(|s| s.service_id == service_id)
    }

    /// Get all service IDs.
    pub fn get_all_service_ids(&self) -> Vec<u16> {
        self.services.iter().map(|s| s.service_id).collect()
    }

    /// Get service name by service ID.
    pub fn get_service_name(&self, service_id: u16) -> Option<&str> {
        self.find_service(service_id)
            .and_then(|s| s.get_service_name())
    }

    /// Check if this is SDT actual (for current TS).
    pub fn is_actual(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_analyzer::psi::PsiHeader;

    #[test]
    fn test_parse_sdt() {
        // Create a mock SDT section
        let data = [
            // Original network ID = 0x7FE0
            0x7F, 0xE0,
            // Reserved byte
            0xFF,
            // Service entry: service_id=0x0101
            0x01, 0x01,
            // flags (EIT schedule=0, EIT p/f=1)
            0x01,
            // running_status=4 (running), free_ca=0, descriptors_length=12
            0x80, 0x0C,
            // Service descriptor: tag=0x48, length=10
            0x48, 0x0A,
            // service_type=0x01
            0x01,
            // provider_name_length=3, "ABC"
            0x03, b'A', b'B', b'C',
            // service_name_length=4, "CH01"
            0x04, b'C', b'H', b'0', b'1',
        ];

        let header = PsiHeader {
            table_id: table_id::SDT_ACTUAL,
            section_syntax_indicator: true,
            section_length: 25,
            table_id_extension: 0x7FE1, // TSID
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

        let sdt = SdtTable::parse(&section).unwrap();

        assert_eq!(sdt.transport_stream_id, 0x7FE1);
        assert_eq!(sdt.original_network_id, 0x7FE0);
        assert_eq!(sdt.services.len(), 1);

        let service = &sdt.services[0];
        assert_eq!(service.service_id, 0x0101);
        assert!(!service.eit_schedule_flag);
        assert!(service.eit_present_following_flag);
        assert_eq!(service.running_status, 4);
        assert!(!service.free_ca_mode);

        // Check parsed service descriptor
        assert!(service.service_descriptor.is_some());
        let desc = service.service_descriptor.as_ref().unwrap();
        assert_eq!(desc.service_type, 0x01);
        assert_eq!(desc.provider_name, "ABC");
        assert_eq!(desc.service_name, "CH01");
    }

    #[test]
    fn test_sdt_find_service() {
        let sdt = SdtTable {
            transport_stream_id: 0x7FE1,
            original_network_id: 0x7FE0,
            version_number: 0,
            services: vec![
                SdtService {
                    service_id: 0x0101,
                    eit_schedule_flag: false,
                    eit_present_following_flag: true,
                    running_status: 4,
                    free_ca_mode: false,
                    descriptors: vec![],
                    service_descriptor: Some(ServiceDescriptor {
                        service_type: 0x01,
                        provider_name: "Test".to_string(),
                        service_name: "Channel 1".to_string(),
                    }),
                },
                SdtService {
                    service_id: 0x0102,
                    eit_schedule_flag: false,
                    eit_present_following_flag: true,
                    running_status: 4,
                    free_ca_mode: false,
                    descriptors: vec![],
                    service_descriptor: Some(ServiceDescriptor {
                        service_type: 0x01,
                        provider_name: "Test".to_string(),
                        service_name: "Channel 2".to_string(),
                    }),
                },
            ],
        };

        assert!(sdt.find_service(0x0101).is_some());
        assert!(sdt.find_service(0x0102).is_some());
        assert!(sdt.find_service(0x0103).is_none());

        assert_eq!(sdt.get_service_name(0x0101), Some("Channel 1"));
        assert_eq!(sdt.get_service_name(0x0102), Some("Channel 2"));
    }

    #[test]
    fn test_sdt_get_all_service_ids() {
        let sdt = SdtTable {
            transport_stream_id: 0x7FE1,
            original_network_id: 0x7FE0,
            version_number: 0,
            services: vec![
                SdtService {
                    service_id: 0x0101,
                    ..Default::default()
                },
                SdtService {
                    service_id: 0x0102,
                    ..Default::default()
                },
                SdtService {
                    service_id: 0x0103,
                    ..Default::default()
                },
            ],
        };

        let ids = sdt.get_all_service_ids();
        assert_eq!(ids, vec![0x0101, 0x0102, 0x0103]);
    }
}
