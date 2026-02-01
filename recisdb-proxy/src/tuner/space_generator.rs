//! Automatic tuning space generation from channel information.
//!
//! This module provides functionality to automatically generate virtual tuning space
//! assignments from actual channel records, with intelligent grouping by band type
//! and terrestrial region.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use log::debug;
use recisdb_protocol::BandType;

/// Band information containing channels for a specific band.
#[derive(Debug, Clone)]
pub struct BandInfo {
    pub band_type: BandType,
    /// For terrestrial, this contains per-region NIDs.
    /// For other bands, this contains all channels.
    pub regions: Vec<RegionInfo>,
}

/// Region information (terrestrial region or logical grouping).
#[derive(Debug, Clone)]
pub struct RegionInfo {
    pub region_name: String,
    pub nids: BTreeSet<u16>,
    pub channels: Vec<ChannelSpaceMapping>,
}

/// Channel information for space mapping.
#[derive(Debug, Clone, Copy)]
pub struct ChannelSpaceMapping {
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub bon_space: u32,
    pub bon_channel: u32,
}

/// Automatically generated virtual space mapping.
///
/// Maps virtual space indices (0, 1, 2, ...) to actual BonDriver spaces and
/// the channels that should be available in each space.
#[derive(Debug, Clone)]
pub struct SpaceMapping {
    /// Virtual space index (0, 1, 2, ...)
    pub virtual_space: u32,
    /// Display name for this space (e.g., "福島", "宮城", "BS", "CS")
    pub display_name: String,
    /// Band type
    pub band_type: BandType,
    /// Region name (for terrestrial only)
    pub region_name: Option<String>,
    /// Actual BonDriver spaces that contain channels for this virtual space
    pub actual_spaces: Vec<u32>,
    /// Available NIDs in this space
    pub nids: BTreeSet<u16>,
    /// Channel mappings
    pub channels: Vec<ChannelSpaceMapping>,
}

/// Space generator that produces virtual space assignments from channels.
#[derive(Clone, Debug)]
pub struct SpaceGenerator {
    /// Virtual space mappings
    mappings: Vec<SpaceMapping>,
    /// Reverse lookup: actual_space -> list of virtual_spaces
    actual_to_virtual: HashMap<u32, Vec<u32>>,
}

impl SpaceGenerator {
    /// Generate space mappings from a list of channels.
    ///
    /// Algorithm:
    /// 1. Group channels by NID
    /// 2. Classify each NID by band type
    /// 3. For terrestrial (0x7FXX):
    ///    - Further group by region (福島, 宮城, etc.)
    ///    - Create one virtual space per region
    /// 4. For other bands:
    ///    - Create one virtual space per band (BS, CS, 4K, etc.)
    /// 5. Skip bands with no channels
    /// 6. Number virtual spaces sequentially
    pub fn generate_from_channels(channels: &[ChannelInfo]) -> Self {
        let mut bands: HashMap<BandType, BandInfo> = HashMap::new();

        // Step 1-2: Group channels by band type and region
        for ch in channels {
            let band_type = BandType::from_nid(ch.nid);

            let band = bands.entry(band_type).or_insert(BandInfo {
                band_type,
                regions: Vec::new(),
            });

            // Determine region name
            let region_name = match band_type {
                BandType::Terrestrial => {
                    ch.terrestrial_region
                        .clone()
                        .unwrap_or_else(|| infer_region_from_nid(ch.nid))
                }
                _ => band_type.display_name().to_string(),
            };

            // Find or create region
            let region = band
                .regions
                .iter_mut()
                .find(|r| r.region_name == region_name);

            if let Some(region) = region {
                region.nids.insert(ch.nid);
                region.channels.push(ChannelSpaceMapping {
                    nid: ch.nid,
                    sid: ch.sid,
                    tsid: ch.tsid,
                    bon_space: ch.bon_space,
                    bon_channel: ch.bon_channel,
                });
            } else {
                let mut region = RegionInfo {
                    region_name: region_name.clone(),
                    nids: BTreeSet::new(),
                    channels: Vec::new(),
                };
                region.nids.insert(ch.nid);
                region.channels.push(ChannelSpaceMapping {
                    nid: ch.nid,
                    sid: ch.sid,
                    tsid: ch.tsid,
                    bon_space: ch.bon_space,
                    bon_channel: ch.bon_channel,
                });
                band.regions.push(region);
            }
        }

        // Step 3-5: Build virtual spaces in order
        let mut mappings = Vec::new();
        let mut actual_to_virtual: HashMap<u32, Vec<u32>> = HashMap::new();
        let mut virtual_space_idx = 0u32;

        // Order: Terrestrial regions first, then BS, CS, 4K, Other
        let band_order = [
            BandType::Terrestrial,
            BandType::BS,
            BandType::CS,
            BandType::FourK,
            BandType::Other,
        ];

        for band_type in band_order {
            if let Some(band) = bands.get(&band_type) {
                // For terrestrial, sort regions by inferred order
                let mut regions = band.regions.clone();
                if band_type == BandType::Terrestrial {
                    regions.sort_by_key(|r| region_sort_key(&r.region_name));
                }

                for region in regions {
                    if region.channels.is_empty() {
                        continue; // Skip empty regions
                    }

                    let display_name = match band_type {
                        BandType::Terrestrial => region.region_name.clone(),
                        _ => band_type.display_name().to_string(),
                    };

                    // Collect actual spaces
                    let mut actual_spaces: Vec<u32> = region
                        .channels
                        .iter()
                        .map(|c| c.bon_space)
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    actual_spaces.sort();

                    // Build mapping
                    let mapping = SpaceMapping {
                        virtual_space: virtual_space_idx,
                        display_name,
                        band_type,
                        region_name: match band_type {
                            BandType::Terrestrial => Some(region.region_name.clone()),
                            _ => None,
                        },
                        actual_spaces: actual_spaces.clone(),
                        nids: region.nids.clone(),
                        channels: region.channels.clone(),
                    };

                    // Update reverse lookup
                    for &actual_space in &actual_spaces {
                        actual_to_virtual
                            .entry(actual_space)
                            .or_insert_with(Vec::new)
                            .push(virtual_space_idx);
                    }

                    debug!(
                        "Generated virtual space {}: {} (band={:?}, actual_spaces={:?})",
                        virtual_space_idx, mapping.display_name, band_type, actual_spaces
                    );

                    mappings.push(mapping);
                    virtual_space_idx += 1;
                }
            }
        }

        debug!("Total virtual spaces generated: {}", mappings.len());

        Self {
            mappings,
            actual_to_virtual,
        }
    }

    /// Get mapping for a virtual space index.
    pub fn get_virtual_space(&self, virtual_space: u32) -> Option<&SpaceMapping> {
        self.mappings
            .iter()
            .find(|m| m.virtual_space == virtual_space)
    }

    /// Get list of all virtual spaces.
    pub fn virtual_spaces(&self) -> Vec<u32> {
        self.mappings.iter().map(|m| m.virtual_space).collect()
    }

    /// Get list of all actual spaces.
    pub fn actual_spaces(&self) -> Vec<u32> {
        self.actual_to_virtual
            .keys()
            .copied()
            .collect::<Vec<_>>()
    }

    /// Map virtual space index to actual BonDriver spaces.
    /// Returns the preferred actual space (first one in the list).
    pub fn map_virtual_to_actual(&self, virtual_space: u32) -> Option<u32> {
        self.get_virtual_space(virtual_space)
            .and_then(|m| m.actual_spaces.first().copied())
    }

    /// Get all virtual spaces that map to a given actual space.
    pub fn get_virtual_spaces_for_actual(&self, actual_space: u32) -> Vec<u32> {
        self.actual_to_virtual
            .get(&actual_space)
            .cloned()
            .unwrap_or_default()
    }

    /// Enumerate channel names for a virtual space.
    pub fn enum_channels_in_space(&self, virtual_space: u32) -> Vec<(u32, String)> {
        if let Some(mapping) = self.get_virtual_space(virtual_space) {
            // Group by bon_channel and collect unique channels
            let mut channels: BTreeMap<u32, String> = BTreeMap::new();
            for ch in &mapping.channels {
                channels.insert(ch.bon_channel, format!("CH{}", ch.bon_channel));
            }
            channels.into_iter().collect()
        } else {
            Vec::new()
        }
    }
}

/// Simple channel info for space generation.
pub struct ChannelInfo {
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,
    pub bon_space: u32,
    pub bon_channel: u32,
    pub terrestrial_region: Option<String>,
}

/// Infer terrestrial region from NID.
/// Uses the same logic as recisdb_protocol::broadcast_region::get_prefecture_name
fn infer_region_from_nid(nid: u16) -> String {
    match nid {
        // Hokkaido (multiple NIDs)
        0x7F01 | 0x7FE0 | 0x7FF0 => "北海道".to_string(),

        // Tohoku
        0x7F08 => "青森".to_string(),      // Aomori
        0x7F09 => "岩手".to_string(),      // Iwate
        0x7F0A => "宮城".to_string(),      // Miyagi
        0x7F0B => "秋田".to_string(),      // Akita
        0x7F0C => "山形".to_string(),      // Yamagata
        0x7F0D => "福島".to_string(),      // Fukushima

        // Kanto prefectural
        0x7F0E => "茨城".to_string(),      // Ibaraki
        0x7F0F => "栃木".to_string(),      // Tochigi
        0x7F10 => "群馬".to_string(),      // Gunma
        0x7F11 => "埼玉".to_string(),      // Saitama
        0x7F12 => "千葉".to_string(),      // Chiba
        0x7F13 => "東京".to_string(),      // Tokyo
        0x7F14 => "神奈川".to_string(),    // Kanagawa

        // Koshinetsu
        0x7F15 => "新潟".to_string(),      // Niigata
        0x7F16 => "長野".to_string(),      // Nagano
        0x7F17 => "山梨".to_string(),      // Yamanashi

        // Hokuriku
        0x7F18 => "富山".to_string(),      // Toyama
        0x7F19 => "石川".to_string(),      // Ishikawa
        0x7F1A => "福井".to_string(),      // Fukui

        // Tokai prefectural
        0x7F1B => "静岡".to_string(),      // Shizuoka
        0x7F1C => "愛知".to_string(),      // Aichi
        0x7F1D => "岐阜".to_string(),      // Gifu
        0x7F1E => "三重".to_string(),      // Mie

        // Kinki prefectural
        0x7F1F => "滋賀".to_string(),      // Shiga
        0x7F20 => "京都".to_string(),      // Kyoto
        0x7F21 => "大阪".to_string(),      // Osaka
        0x7F22 => "兵庫".to_string(),      // Hyogo
        0x7F23 => "奈良".to_string(),      // Nara
        0x7F24 => "和歌山".to_string(),    // Wakayama

        // Chugoku
        0x7F25 => "鳥取".to_string(),      // Tottori
        0x7F26 => "島根".to_string(),      // Shimane
        0x7F27 => "岡山".to_string(),      // Okayama
        0x7F28 => "広島".to_string(),      // Hiroshima
        0x7F29 => "山口".to_string(),      // Yamaguchi

        // Shikoku
        0x7F2A => "徳島".to_string(),      // Tokushima
        0x7F2B => "香川".to_string(),      // Kagawa
        0x7F2C => "愛媛".to_string(),      // Ehime
        0x7F2D => "高知".to_string(),      // Kochi

        // Kyushu
        0x7F2E => "福岡".to_string(),      // Fukuoka
        0x7F2F => "佐賀".to_string(),      // Saga
        0x7F30 => "長崎".to_string(),      // Nagasaki
        0x7F31 => "熊本".to_string(),      // Kumamoto
        0x7F32 => "大分".to_string(),      // Oita
        0x7F33 => "宮崎".to_string(),      // Miyazaki
        0x7F34 => "鹿児島".to_string(),    // Kagoshima

        // Okinawa
        0x7F35 => "沖縄".to_string(),      // Okinawa

        // Wide area broadcast NIDs - map to representative prefecture
        0x7FE1..=0x7FE7 => "北海道".to_string(),    // Hokkaido wide
        0x7FE8 => "東京".to_string(),               // Kanto wide -> Tokyo
        0x7FE9 => "大阪".to_string(),               // Kinki wide -> Osaka
        0x7FEA => "愛知".to_string(),               // Tokai wide -> Aichi
        0x7FEB => "岡山".to_string(),               // Okayama-Kagawa -> Okayama
        0x7FEC => "島根".to_string(),               // Shimane-Tottori -> Shimane
        0x7FF1..=0x7FF7 => "北海道".to_string(),    // Additional Hokkaido

        // Unknown terrestrial
        nid if (nid >= 0x7F00 && nid <= 0x7FFF) => "不明".to_string(),

        // Non-terrestrial
        _ => "その他".to_string(),
    }
}

/// Sort key for terrestrial regions.
fn region_sort_key(name: &str) -> u32 {
    match name {
        "北海道" => 0,
        "青森" => 1,
        "岩手" => 2,
        "宮城" => 3,
        "秋田" => 4,
        "山形" => 5,
        "福島" => 6,
        "茨城" => 7,
        "栃木" => 8,
        "群馬" => 9,
        "埼玉" => 10,
        "千葉" => 11,
        "東京" => 12,
        "神奈川" => 13,
        "山梨" => 14,
        "長野" => 15,
        "新潟" => 16,
        "富山" => 17,
        "石川" => 18,
        "福井" => 19,
        "岐阜" => 20,
        "愛知" => 21,
        "三重" => 22,
        "滋賀" => 23,
        "京都" => 24,
        "大阪" => 25,
        "兵庫" => 26,
        "奈良" => 27,
        "和歌山" => 28,
        "鳥取" => 29,
        "島根" => 30,
        "岡山" => 31,
        "広島" => 32,
        "山口" => 33,
        "徳島" => 34,
        "香川" => 35,
        "愛媛" => 36,
        "高知" => 37,
        "福岡" => 38,
        "佐賀" => 39,
        "長崎" => 40,
        "熊本" => 41,
        "大分" => 42,
        "宮崎" => 43,
        "鹿児島" => 44,
        "沖縄" => 45,
        _ => 255,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_space_generator_empty() {
        let generator = SpaceGenerator::generate_from_channels(&[]);
        assert_eq!(generator.virtual_spaces().len(), 0);
    }

    #[test]
    fn test_space_generator_single_terrestrial() {
        let channels = vec![ChannelInfo {
            nid: 0x7FE8,  // Kanto
            sid: 1024,
            tsid: 32736,
            bon_space: 0,
            bon_channel: 13,
            terrestrial_region: Some("東京".to_string()),
        }];

        let generator = SpaceGenerator::generate_from_channels(&channels);
        assert_eq!(generator.virtual_spaces().len(), 1);
        assert_eq!(generator.virtual_spaces()[0], 0);

        let mapping = generator.get_virtual_space(0).unwrap();
        assert_eq!(mapping.band_type, BandType::Terrestrial);
        assert_eq!(mapping.region_name, Some("東京".to_string()));
    }

    #[test]
    fn test_space_generator_mixed_bands() {
        let channels = vec![
            ChannelInfo {
                nid: 0x7FE8,  // Terrestrial
                sid: 1024,
                tsid: 32736,
                bon_space: 0,
                bon_channel: 13,
                terrestrial_region: Some("東京".to_string()),
            },
            ChannelInfo {
                nid: 0x4011,  // BS
                sid: 101,
                tsid: 0x8000,
                bon_space: 1,
                bon_channel: 1,
                terrestrial_region: None,
            },
            ChannelInfo {
                nid: 0x6001,  // CS
                sid: 256,
                tsid: 0x0001,
                bon_space: 2,
                bon_channel: 256,
                terrestrial_region: None,
            },
        ];

        let generator = SpaceGenerator::generate_from_channels(&channels);
        assert_eq!(generator.virtual_spaces().len(), 3);

        // Check order: Terrestrial, BS, CS
        let v0 = generator.get_virtual_space(0).unwrap();
        assert_eq!(v0.band_type, BandType::Terrestrial);

        let v1 = generator.get_virtual_space(1).unwrap();
        assert_eq!(v1.band_type, BandType::BS);

        let v2 = generator.get_virtual_space(2).unwrap();
        assert_eq!(v2.band_type, BandType::CS);
    }
}
