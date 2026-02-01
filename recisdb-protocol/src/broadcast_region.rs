//! Broadcast region classification based on Network ID (NID).
//!
//! This module provides functionality to classify Japanese broadcast networks
//! based on their NID values according to ARIB TR-B14/TR-B15 standards.
//!
//! # NID Calculation Formula (ARIB TR-B14 第五分冊 第七編 9.1)
//!
//! ```text
//! network_id = 0x7FF0 - 0x0010 × 地域識別 + 地域事業者識別 - 0x0400 × 県複フラグ
//! ```
//!
//! Where:
//! - 地域識別 (region_id): 1-62
//! - 地域事業者識別 (broadcaster_id): 0-15
//! - 県複フラグ: 0 (normal) or 1 (prefecture-specific)

use serde::{Deserialize, Serialize};

use crate::types::BroadcastType;

/// Calculate region ID from terrestrial network ID.
///
/// Based on ARIB TR-B14 第五分冊 第七編 9.1:
/// `network_id = 0x7FF0 - 0x0010 × 地域識別 + 地域事業者識別 - 0x0400 × 県複フラグ`
///
/// # Arguments
/// * `nid` - Network ID
///
/// # Returns
/// Region ID (1-62) if the NID is within terrestrial range, None otherwise.
///
/// # Example
/// ```
/// use recisdb_protocol::broadcast_region::get_region_id_from_nid;
///
/// // 宮城 (region_id = 17): NID = 0x7EE0
/// assert_eq!(get_region_id_from_nid(0x7EE0), Some(17));
/// // 関東広域 (region_id = 1): NID = 0x7FE0
/// assert_eq!(get_region_id_from_nid(0x7FE0), Some(1));
/// // BS (NID = 4): Not terrestrial
/// assert_eq!(get_region_id_from_nid(4), None);
/// ```
pub fn get_region_id_from_nid(nid: u16) -> Option<u8> {
    // Check if NID is within terrestrial range
    // 県複フラグ=0: 0x7C10 〜 0x7FEF
    // 県複フラグ=1: 0x7810 〜 0x7BEF
    if !(0x7800..=0x7FF0).contains(&nid) {
        return None;
    }

    // Normalize NID by adding 0x0400 if 県複フラグ=1
    let normalized_nid = if nid < 0x7C00 {
        nid + 0x0400
    } else {
        nid
    };

    // Calculate region ID
    // network_id = 0x7FF0 - 0x0010 × 地域識別 + 地域事業者識別
    // 地域事業者識別 is 0〜15, so we need to round up when dividing
    let region_id = ((0x7FF0 - normalized_nid + 0x000F) / 0x0010) as u8;

    if (1..=62).contains(&region_id) {
        Some(region_id)
    } else {
        None
    }
}

/// Get prefecture name from region ID.
///
/// # Arguments
/// * `region_id` - Region ID (1-62)
///
/// # Returns
/// Prefecture name in Japanese, or None if region_id is invalid.
pub fn get_prefecture_name_from_region_id(region_id: u8) -> Option<&'static str> {
    match region_id {
        // 広域放送
        1 => Some("東京"),       // 関東広域 → 東京
        2 => Some("大阪"),       // 近畿広域 → 大阪
        3 => Some("愛知"),       // 中京広域 → 愛知
        4 => Some("北海道"),     // 北海道域
        5 => Some("岡山"),       // 岡山香川 → 岡山
        6 => Some("島根"),       // 島根鳥取 → 島根

        // 北海道地域
        10 => Some("北海道"),    // 札幌
        11 => Some("北海道"),    // 函館
        12 => Some("北海道"),    // 旭川
        13 => Some("北海道"),    // 帯広
        14 => Some("北海道"),    // 釧路
        15 => Some("北海道"),    // 北見
        16 => Some("北海道"),    // 室蘭

        // 東北
        17 => Some("宮城"),
        18 => Some("秋田"),
        19 => Some("山形"),
        20 => Some("岩手"),
        21 => Some("福島"),
        22 => Some("青森"),

        // 関東
        23 => Some("東京"),
        24 => Some("神奈川"),
        25 => Some("群馬"),
        26 => Some("茨城"),
        27 => Some("千葉"),
        28 => Some("栃木"),
        29 => Some("埼玉"),

        // 甲信越
        30 => Some("長野"),
        31 => Some("新潟"),
        32 => Some("山梨"),

        // 中部・東海
        33 => Some("愛知"),
        34 => Some("石川"),
        35 => Some("静岡"),
        36 => Some("福井"),
        37 => Some("富山"),
        38 => Some("三重"),
        39 => Some("岐阜"),

        // 近畿
        40 => Some("大阪"),
        41 => Some("京都"),
        42 => Some("兵庫"),
        43 => Some("和歌山"),
        44 => Some("奈良"),
        45 => Some("滋賀"),

        // 中国
        46 => Some("広島"),
        47 => Some("岡山"),
        48 => Some("島根"),
        49 => Some("鳥取"),
        50 => Some("山口"),

        // 四国
        51 => Some("愛媛"),
        52 => Some("香川"),
        53 => Some("徳島"),
        54 => Some("高知"),

        // 九州
        55 => Some("福岡"),
        56 => Some("熊本"),
        57 => Some("長崎"),
        58 => Some("鹿児島"),
        59 => Some("宮崎"),
        60 => Some("大分"),
        61 => Some("佐賀"),

        // 沖縄
        62 => Some("沖縄"),

        _ => None,
    }
}

/// Get region classification from region ID.
fn get_terrestrial_region_from_id(region_id: u8) -> TerrestrialRegion {
    match region_id {
        // 北海道
        4 | 10..=16 => TerrestrialRegion::Hokkaido,
        // 東北
        17..=22 => TerrestrialRegion::Tohoku,
        // 関東
        1 | 23..=29 => TerrestrialRegion::Kanto,
        // 甲信越
        30..=32 => TerrestrialRegion::Koshinetsu,
        // 北陸
        34 | 36 | 37 => TerrestrialRegion::Hokuriku,
        // 東海
        3 | 33 | 35 | 38 | 39 => TerrestrialRegion::Tokai,
        // 近畿
        2 | 40..=45 => TerrestrialRegion::Kinki,
        // 中国
        5 | 6 | 46..=50 => TerrestrialRegion::Chugoku,
        // 四国
        51..=54 => TerrestrialRegion::Shikoku,
        // 九州
        55..=61 => TerrestrialRegion::Kyushu,
        // 沖縄
        62 => TerrestrialRegion::Okinawa,
        // Unknown
        _ => TerrestrialRegion::Unknown(region_id as u16),
    }
}

/// Terrestrial broadcast region classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TerrestrialRegion {
    /// Hokkaido (北海道)
    Hokkaido,
    /// Tohoku (東北)
    Tohoku,
    /// Kanto wide area (関東広域圏)
    Kanto,
    /// Koshinetsu (甲信越)
    Koshinetsu,
    /// Hokuriku (北陸)
    Hokuriku,
    /// Tokai/Chukyo wide area (東海・中京広域圏)
    Tokai,
    /// Kinki wide area (近畿広域圏)
    Kinki,
    /// Chugoku (中国)
    Chugoku,
    /// Shikoku (四国)
    Shikoku,
    /// Kyushu (九州)
    Kyushu,
    /// Okinawa (沖縄)
    Okinawa,
    /// Unknown region with raw NID value
    Unknown(u16),
}

impl TerrestrialRegion {
    /// Returns the Japanese display name for this region.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Hokkaido => "北海道",
            Self::Tohoku => "東北",
            Self::Kanto => "関東",
            Self::Koshinetsu => "甲信越",
            Self::Hokuriku => "北陸",
            Self::Tokai => "東海",
            Self::Kinki => "近畿",
            Self::Chugoku => "中国",
            Self::Shikoku => "四国",
            Self::Kyushu => "九州",
            Self::Okinawa => "沖縄",
            Self::Unknown(_) => "その他",
        }
    }

    /// Returns the English name for this region.
    pub fn name_en(&self) -> &'static str {
        match self {
            Self::Hokkaido => "Hokkaido",
            Self::Tohoku => "Tohoku",
            Self::Kanto => "Kanto",
            Self::Koshinetsu => "Koshinetsu",
            Self::Hokuriku => "Hokuriku",
            Self::Tokai => "Tokai",
            Self::Kinki => "Kinki",
            Self::Chugoku => "Chugoku",
            Self::Shikoku => "Shikoku",
            Self::Kyushu => "Kyushu",
            Self::Okinawa => "Okinawa",
            Self::Unknown(_) => "Unknown",
        }
    }
}

/// Classify broadcast type and region from Network ID.
///
/// # Arguments
/// * `nid` - Network ID from SDT (original_network_id)
///
/// # Returns
/// A tuple of (BroadcastType, Option<TerrestrialRegion>).
/// TerrestrialRegion is Some only for terrestrial broadcasts.
///
/// # NID Allocation (ARIB TR-B14/TR-B15)
/// - BS: NID = 4
/// - CS: NID = 6, 7, 10
/// - Terrestrial: NID = 0x7F00-0x7FFF (varies by region)
///
/// # Example
/// ```
/// use recisdb_protocol::broadcast_region::{classify_nid, TerrestrialRegion};
/// use recisdb_protocol::types::BroadcastType;
///
/// // BS broadcast
/// let (btype, region) = classify_nid(4);
/// assert_eq!(btype, BroadcastType::BS);
/// assert!(region.is_none());
///
/// // Kanto terrestrial
/// let (btype, region) = classify_nid(0x7FE8);
/// assert_eq!(btype, BroadcastType::Terrestrial);
/// assert_eq!(region, Some(TerrestrialRegion::Kanto));
/// ```
pub fn classify_nid(nid: u16) -> (BroadcastType, Option<TerrestrialRegion>) {
    match nid {
        // BS (NID = 4)
        4 => (BroadcastType::BS, None),

        // CS (NID = 6, 7, 10)
        // 6: SKY PerfecTV! (CS1)
        // 7: SKY PerfecTV! (CS2)
        // 10: SKY PerfecTV! Premium Service
        6 | 7 | 10 => (BroadcastType::CS, None),

        // Terrestrial digital broadcasting
        // NID ranges based on ARIB TR-B14
        nid => classify_terrestrial_nid(nid),
    }
}

/// Classify terrestrial NID to region using calculation-based approach.
///
/// Uses `get_region_id_from_nid` and `get_terrestrial_region_from_id` for classification.
fn classify_terrestrial_nid(nid: u16) -> (BroadcastType, Option<TerrestrialRegion>) {
    if let Some(region_id) = get_region_id_from_nid(nid) {
        let region = get_terrestrial_region_from_id(region_id);
        (BroadcastType::Terrestrial, Some(region))
    } else {
        // Not a valid terrestrial NID
        (BroadcastType::Terrestrial, Some(TerrestrialRegion::Unknown(nid)))
    }
}

/// Get a human-readable name for the broadcast type.
pub fn broadcast_type_name(btype: BroadcastType) -> &'static str {
    match btype {
        BroadcastType::Terrestrial => "地デジ",
        BroadcastType::BS => "BS",
        BroadcastType::CS => "CS",
    }
}

/// Get English name for the broadcast type.
pub fn broadcast_type_name_en(btype: BroadcastType) -> &'static str {
    match btype {
        BroadcastType::Terrestrial => "Terrestrial",
        BroadcastType::BS => "BS",
        BroadcastType::CS => "CS",
    }
}

/// Generate a display name for tuning space based on broadcast type and region.
///
/// # Example
/// ```
/// use recisdb_protocol::broadcast_region::{generate_space_name, TerrestrialRegion};
/// use recisdb_protocol::types::BroadcastType;
///
/// assert_eq!(generate_space_name(BroadcastType::BS, None), "BS");
/// assert_eq!(
///     generate_space_name(BroadcastType::Terrestrial, Some(TerrestrialRegion::Kanto)),
///     "地デジ (関東)"
/// );
/// ```
pub fn generate_space_name(btype: BroadcastType, region: Option<TerrestrialRegion>) -> String {
    match btype {
        BroadcastType::BS => "BS".to_string(),
        BroadcastType::CS => "CS".to_string(),
        BroadcastType::Terrestrial => {
            if let Some(r) = region {
                format!("地デジ ({})", r.display_name())
            } else {
                "地デジ".to_string()
            }
        }
    }
}

/// Get TVTest-compatible prefecture name from NID/TSID.
///
/// This function returns the prefecture name in Japanese, compatible with TVTest.
/// Uses `get_region_id_from_nid` and `get_prefecture_name_from_region_id` internally.
/// For non-terrestrial broadcasts, returns None.
///
/// # Example
/// ```
/// use recisdb_protocol::broadcast_region::get_prefecture_name;
///
/// // Miyagi (宮城) region: 0x7EE0-0x7EEF
/// assert_eq!(get_prefecture_name(0x7EE0), Some("宮城"));
/// // Kanto wide area: 0x7FE0-0x7FE8
/// assert_eq!(get_prefecture_name(0x7FE0), Some("東京"));
/// ```
pub fn get_prefecture_name(nid: u16) -> Option<&'static str> {
    get_region_id_from_nid(nid).and_then(get_prefecture_name_from_region_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bs_classification() {
        let (btype, region) = classify_nid(4);
        assert_eq!(btype, BroadcastType::BS);
        assert!(region.is_none());
    }

    #[test]
    fn test_cs_classification() {
        for nid in [6, 7, 10] {
            let (btype, region) = classify_nid(nid);
            assert_eq!(btype, BroadcastType::CS);
            assert!(region.is_none());
        }
    }

    #[test]
    fn test_terrestrial_kanto_wide() {
        // 関東広域: 0x7FE0-0x7FEF
        let (btype, region) = classify_nid(0x7FE0);
        assert_eq!(btype, BroadcastType::Terrestrial);
        assert_eq!(region, Some(TerrestrialRegion::Kanto));
    }

    #[test]
    fn test_terrestrial_kinki_wide() {
        // 近畿広域: 0x7FD0-0x7FDF
        let (btype, region) = classify_nid(0x7FD1);
        assert_eq!(btype, BroadcastType::Terrestrial);
        assert_eq!(region, Some(TerrestrialRegion::Kinki));
    }

    #[test]
    fn test_terrestrial_tokai_wide() {
        // 中京広域: 0x7FC0-0x7FCF
        let (btype, region) = classify_nid(0x7FC1);
        assert_eq!(btype, BroadcastType::Terrestrial);
        assert_eq!(region, Some(TerrestrialRegion::Tokai));
    }

    #[test]
    fn test_terrestrial_hokkaido() {
        // 北海道域: 0x7FB0-0x7FBF
        let (btype, region) = classify_nid(0x7FB2);
        assert_eq!(btype, BroadcastType::Terrestrial);
        assert_eq!(region, Some(TerrestrialRegion::Hokkaido));

        // 北海道（札幌）: 0x7F50-0x7F5F
        let (btype, region) = classify_nid(0x7F50);
        assert_eq!(btype, BroadcastType::Terrestrial);
        assert_eq!(region, Some(TerrestrialRegion::Hokkaido));
    }

    #[test]
    fn test_terrestrial_tohoku() {
        // 宮城: 0x7EE0-0x7EEF
        let (btype, region) = classify_nid(0x7EE0);
        assert_eq!(btype, BroadcastType::Terrestrial);
        assert_eq!(region, Some(TerrestrialRegion::Tohoku));
    }

    #[test]
    fn test_terrestrial_okinawa() {
        // 沖縄: 0x7C10-0x7C1F
        let (btype, region) = classify_nid(0x7C10);
        assert_eq!(btype, BroadcastType::Terrestrial);
        assert_eq!(region, Some(TerrestrialRegion::Okinawa));
    }

    #[test]
    fn test_unknown_nid() {
        // Unknown NID returns Terrestrial with Unknown region
        let (btype, region) = classify_nid(0x1000);
        assert_eq!(btype, BroadcastType::Terrestrial);
        assert!(matches!(region, Some(TerrestrialRegion::Unknown(0x1000))));
    }

    #[test]
    fn test_space_name_generation() {
        assert_eq!(generate_space_name(BroadcastType::BS, None), "BS");
        assert_eq!(generate_space_name(BroadcastType::CS, None), "CS");
        assert_eq!(
            generate_space_name(BroadcastType::Terrestrial, Some(TerrestrialRegion::Kanto)),
            "地デジ (関東)"
        );
        assert_eq!(
            generate_space_name(BroadcastType::Terrestrial, None),
            "地デジ"
        );
    }

    #[test]
    fn test_region_display_names() {
        assert_eq!(TerrestrialRegion::Kanto.display_name(), "関東");
        assert_eq!(TerrestrialRegion::Kinki.display_name(), "近畿");
        assert_eq!(TerrestrialRegion::Unknown(0x7FFF).display_name(), "その他");
    }

    #[test]
    fn test_prefecture_names() {
        // Test prefectural NIDs (new ranges based on ARIB TR-B14)
        // 宮城: 0x7EE0-0x7EEF
        assert_eq!(get_prefecture_name(0x7EE0), Some("宮城"));
        // 茨城: 0x7E50-0x7E5F
        assert_eq!(get_prefecture_name(0x7E50), Some("茨城"));
        // 栃木: 0x7E30-0x7E3F
        assert_eq!(get_prefecture_name(0x7E30), Some("栃木"));
        // 東京: 0x7E80-0x7E8F
        assert_eq!(get_prefecture_name(0x7E87), Some("東京"));
        // 大阪: 0x7D70-0x7D7F
        assert_eq!(get_prefecture_name(0x7D70), Some("大阪"));
        // 沖縄: 0x7C10-0x7C1F
        assert_eq!(get_prefecture_name(0x7C10), Some("沖縄"));

        // Test wide area broadcast NIDs
        // 関東広域: 0x7FE0-0x7FEF -> 東京
        assert_eq!(get_prefecture_name(0x7FE0), Some("東京"));
        // 近畿広域: 0x7FD0-0x7FDF -> 大阪
        assert_eq!(get_prefecture_name(0x7FD1), Some("大阪"));
        // 中京広域: 0x7FC0-0x7FCF -> 愛知
        assert_eq!(get_prefecture_name(0x7FC1), Some("愛知"));

        // Non-terrestrial should return None
        assert_eq!(get_prefecture_name(4), None);   // BS
        assert_eq!(get_prefecture_name(6), None);   // CS
    }
}
