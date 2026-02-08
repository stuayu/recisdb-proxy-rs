//! Descriptor parsing for PSI/SI tables.
//!
//! This module handles parsing of various descriptors found in
//! NIT, SDT, and other tables.


/// Service descriptor (0x48).
#[derive(Debug, Clone, Default)]
pub struct ServiceDescriptor {
    /// Service type.
    pub service_type: u8,
    /// Service provider name.
    pub provider_name: String,
    /// Service name.
    pub service_name: String,
}

impl ServiceDescriptor {
    /// Parse a service descriptor from raw bytes.
    pub fn parse(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() < 3 {
            return Err("Service descriptor too short");
        }

        let service_type = data[0];
        let provider_name_length = data[1] as usize;

        if data.len() < 2 + provider_name_length + 1 {
            return Err("Invalid provider name length");
        }

        let provider_name = decode_arib_string(&data[2..2 + provider_name_length]);

        let service_name_offset = 2 + provider_name_length;
        let service_name_length = data[service_name_offset] as usize;

        if data.len() < service_name_offset + 1 + service_name_length {
            return Err("Invalid service name length");
        }

        let service_name = decode_arib_string(
            &data[service_name_offset + 1..service_name_offset + 1 + service_name_length],
        );

        Ok(ServiceDescriptor {
            service_type,
            provider_name,
            service_name,
        })
    }

    /// Get human-readable service type name.
    pub fn service_type_name(&self) -> &'static str {
        match self.service_type {
            0x01 => "Digital TV",
            0x02 => "Digital Audio",
            0x0C => "Data Service",
            0xA1 => "Special Video (ISDB)",
            0xA2 => "Special Audio (ISDB)",
            0xA3 => "Special Data (ISDB)",
            0xA4 => "Engineering (ISDB)",
            0xA5 => "Promotional Video (ISDB)",
            0xA6 => "Promotional Audio (ISDB)",
            0xA7 => "Promotional Data (ISDB)",
            0xA8 => "For Advance Storage (ISDB)",
            0xA9 => "For Exclusive Storage (ISDB)",
            0xAA => "Bookmark List (ISDB)",
            0xAB => "Server Type Simultaneous (ISDB)",
            0xAC => "Independent File (ISDB)",
            0xC0 => "1seg (ISDB)",
            _ => "Unknown",
        }
    }
}

/// Network name descriptor (0x40).
#[derive(Debug, Clone, Default)]
pub struct NetworkNameDescriptor {
    /// Network name.
    pub network_name: String,
}

impl NetworkNameDescriptor {
    /// Parse a network name descriptor.
    pub fn parse(data: &[u8]) -> Result<Self, &'static str> {
        Ok(NetworkNameDescriptor {
            network_name: decode_arib_string(data),
        })
    }
}

/// Terrestrial delivery system descriptor (0xFA for ISDB-T).
#[derive(Debug, Clone, Default)]
pub struct TerrestrialDeliveryDescriptor {
    /// Area code.
    pub area_code: u16,
    /// Guard interval.
    pub guard_interval: u8,
    /// Transmission mode.
    pub transmission_mode: u8,
    /// Frequencies in Hz.
    pub frequencies: Vec<u32>,
}

impl TerrestrialDeliveryDescriptor {
    /// Parse a terrestrial delivery system descriptor.
    pub fn parse(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() < 2 {
            return Err("Terrestrial delivery descriptor too short");
        }

        let area_code = ((data[0] as u16) << 4) | ((data[1] as u16) >> 4);
        let guard_interval = (data[1] >> 2) & 0x03;
        let transmission_mode = data[1] & 0x03;

        let mut frequencies = Vec::new();
        let mut offset = 2;

        while offset + 2 <= data.len() {
            let freq_value = ((data[offset] as u32) << 8) | data[offset + 1] as u32;
            // ISDB-T frequency calculation: freq_value * 1/7 MHz
            let frequency_hz = (freq_value as u64 * 1_000_000 / 7) as u32;
            frequencies.push(frequency_hz);
            offset += 2;
        }

        Ok(TerrestrialDeliveryDescriptor {
            area_code,
            guard_interval,
            transmission_mode,
            frequencies,
        })
    }
}

/// Satellite delivery system descriptor (0x43).
#[derive(Debug, Clone, Default)]
pub struct SatelliteDeliveryDescriptor {
    /// Frequency in kHz.
    pub frequency: u32,
    /// Orbital position (degrees * 10).
    pub orbital_position: u16,
    /// West/East flag (false = East).
    pub west_east_flag: bool,
    /// Polarization.
    pub polarization: u8,
    /// Modulation system.
    pub modulation_system: u8,
    /// Modulation type.
    pub modulation_type: u8,
    /// Symbol rate in symbols/sec.
    pub symbol_rate: u32,
    /// FEC inner.
    pub fec_inner: u8,
}

impl SatelliteDeliveryDescriptor {
    /// Parse a satellite delivery system descriptor.
    pub fn parse(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() < 11 {
            return Err("Satellite delivery descriptor too short");
        }

        // Frequency: 8 BCD digits (4 bytes)
        let frequency = bcd_to_u32(&data[0..4]) * 10; // Convert to kHz

        // Orbital position: 4 BCD digits (2 bytes)
        let orbital_position = (bcd_to_u32(&data[4..6]) & 0xFFFF) as u16;

        let west_east_flag = data[6] & 0x80 != 0;
        let polarization = (data[6] >> 5) & 0x03;
        let modulation_system = (data[6] >> 2) & 0x01;
        let modulation_type = data[6] & 0x03;

        // Symbol rate: 7 BCD digits (4 bytes, top 28 bits)
        let symbol_rate = bcd_to_u32(&data[7..11]) / 10;

        let fec_inner = data[10] & 0x0F;

        Ok(SatelliteDeliveryDescriptor {
            frequency,
            orbital_position,
            west_east_flag,
            polarization,
            modulation_system,
            modulation_type,
            symbol_rate,
            fec_inner,
        })
    }
}

/// TS information descriptor (0xCD for ISDB).
#[derive(Debug, Clone, Default)]
pub struct TsInformationDescriptor {
    /// Remote control key ID.
    pub remote_control_key_id: u8,
    /// TS name.
    pub ts_name: String,
}

impl TsInformationDescriptor {
    /// Parse a TS information descriptor.
    pub fn parse(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() < 2 {
            return Err("TS information descriptor too short");
        }

        let remote_control_key_id = data[0];
        let ts_name_length = (data[1] >> 2) as usize;

        if data.len() < 2 + ts_name_length {
            return Err("Invalid TS name length");
        }

        let ts_name = decode_arib_string(&data[2..2 + ts_name_length]);

        Ok(TsInformationDescriptor {
            remote_control_key_id,
            ts_name,
        })
    }
}

/// Parse descriptors from a descriptor loop.
pub fn parse_descriptor_loop(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut descriptors = Vec::new();
    let mut offset = 0;

    while offset + 2 <= data.len() {
        let tag = data[offset];
        let length = data[offset + 1] as usize;
        offset += 2;

        if offset + length > data.len() {
            break;
        }

        descriptors.push((tag, data[offset..offset + length].to_vec()));
        offset += length;
    }

    descriptors
}

/// Find a specific descriptor in a descriptor loop.
pub fn find_descriptor(data: &[u8], tag: u8) -> Option<Vec<u8>> {
    parse_descriptor_loop(data)
        .into_iter()
        .find(|(t, _)| *t == tag)
        .map(|(_, d)| d)
}

/// Decode ARIB string to UTF-8.
///
/// This is a simplified implementation that handles common cases.
/// Full ARIB character decoding is complex and includes multiple
/// character sets and escape sequences.
fn decode_arib_string(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }

    // Check for character set indicator
    let (start, _charset) = if !data.is_empty() && data[0] < 0x20 {
        // Character set indicator byte
        (1, data[0])
    } else {
        (0, 0)
    };

    // Simple decoding: try UTF-8 first, then fall back to Shift-JIS-like
    let slice = &data[start..];

    // Try to decode as UTF-8
    if let Ok(s) = std::str::from_utf8(slice) {
        return s.to_string();
    }

    // Fall back to treating bytes as raw with replacement
    // This is a simplified approach - proper ARIB decoding would use
    // the ARIB STD-B24 character set tables
    slice
        .iter()
        .filter(|&&b| b >= 0x20 || b == 0x0A || b == 0x0D)
        .map(|&b| {
            if b.is_ascii() {
                b as char
            } else {
                '?'
            }
        })
        .collect()
}

/// Convert BCD bytes to u32.
fn bcd_to_u32(data: &[u8]) -> u32 {
    let mut result = 0u32;
    for &byte in data {
        let high = (byte >> 4) as u32;
        let low = (byte & 0x0F) as u32;
        result = result * 100 + high * 10 + low;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_service_descriptor() {
        // Service descriptor with ASCII names
        let data = [
            0x01, // service_type = Digital TV
            0x04, // provider_name_length = 4
            b'T', b'E', b'S', b'T', // provider_name = "TEST"
            0x07, // service_name_length = 7
            b'C', b'H', b' ', b'N', b'A', b'M', b'E', // service_name = "CH NAME"
        ];

        let desc = ServiceDescriptor::parse(&data).unwrap();
        assert_eq!(desc.service_type, 0x01);
        assert_eq!(desc.provider_name, "TEST");
        assert_eq!(desc.service_name, "CH NAME");
    }

    #[test]
    fn test_parse_network_name_descriptor() {
        let data = b"Network1";
        let desc = NetworkNameDescriptor::parse(data).unwrap();
        assert_eq!(desc.network_name, "Network1");
    }

    #[test]
    fn test_parse_descriptor_loop() {
        let data = [
            0x48, 0x02, 0xAA, 0xBB, // Service descriptor, length 2
            0x40, 0x03, 0xCC, 0xDD, 0xEE, // Network name, length 3
        ];

        let descriptors = parse_descriptor_loop(&data);
        assert_eq!(descriptors.len(), 2);
        assert_eq!(descriptors[0].0, 0x48);
        assert_eq!(descriptors[0].1, vec![0xAA, 0xBB]);
        assert_eq!(descriptors[1].0, 0x40);
        assert_eq!(descriptors[1].1, vec![0xCC, 0xDD, 0xEE]);
    }

    #[test]
    fn test_find_descriptor() {
        let data = [
            0x48, 0x02, 0xAA, 0xBB,
            0x40, 0x03, 0xCC, 0xDD, 0xEE,
        ];

        let found = find_descriptor(&data, 0x40);
        assert_eq!(found, Some(vec![0xCC, 0xDD, 0xEE]));

        let not_found = find_descriptor(&data, 0x99);
        assert!(not_found.is_none());
    }

    #[test]
    fn test_bcd_to_u32() {
        assert_eq!(bcd_to_u32(&[0x12, 0x34]), 1234);
        assert_eq!(bcd_to_u32(&[0x00, 0x01]), 1);
        assert_eq!(bcd_to_u32(&[0x99, 0x99]), 9999);
    }
}
