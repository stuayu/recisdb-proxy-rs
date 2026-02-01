//! Message type definitions for the recisdb network protocol.

use serde::{Deserialize, Serialize};

/// Protocol version.
pub const PROTOCOL_VERSION: u16 = 1;

/// Magic bytes for frame header: "BNDP" (BonDriver Network Protocol).
pub const MAGIC: [u8; 4] = *b"BNDP";

/// Maximum frame payload size (16 MB).
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Maximum TS data chunk size (188 KB = 1000 TS packets).
pub const MAX_TS_CHUNK_SIZE: usize = 188 * 1000;

/// Broadcast band type classification.
///
/// Based on ARIB STD-B10 and TR-B14/TR-B15 standards, broadcasts are classified into bands:
/// - Terrestrial (地上波): Digital terrestrial television
/// - BS: BS satellite broadcasts
/// - CS: 110度CS satellite broadcasts (CS1, CS2)
/// - CATV: Cable television
/// - SKY: 124/128度CS (スカパー!プレミアムサービス)
/// - BS4K: Advanced BS digital (BS4K/8K)
/// - Other: Undefined bands
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BandType {
    /// Digital terrestrial television (地上波デジタル)
    Terrestrial = 0,
    /// BS satellite (BS衛星)
    BS = 1,
    /// 110度CS satellite (110度CS衛星: CS1, CS2)
    CS = 2,
    /// Advanced BS/CS digital (BS4K/CS4K)
    FourK = 3,
    /// Other/undefined bands
    Other = 4,
    /// Cable television (ケーブルテレビ)
    CATV = 5,
    /// 124/128度CS (スカパー!プレミアムサービス)
    SKY = 6,
}

impl BandType {
    /// Classify band type from NID (Network ID).
    ///
    /// Based on ARIB STD-B10 第2部 付録N and TR-B14 standards:
    /// - Terrestrial: 0x7880-0x7FE8 (region specific, including 県複フラグ=1 range)
    /// - BS: 0x0004
    /// - CS (110度): 0x0006, 0x0007
    /// - BS4K: 0x000B (高度BS), 0x000C (高度110度CS)
    /// - SKY (124/128度CS): 0x000A (SPHD), 0x0001, 0x0003
    /// - CATV: 0xFFFE, 0xFFFA, 0xFFFD, 0xFFF9, 0xFFF7
    ///
    /// ref: https://www.arib.or.jp/english/html/overview/doc/6-STD-B10v5_13-E1.pdf
    pub fn from_nid(nid: u16) -> Self {
        match nid {
            // BS digital (BSデジタル放送)
            0x0004 => BandType::BS,

            // 110度CS digital (110度CSデジタル放送)
            // CS1: 0x0006 (旧プラット・ワン系)
            // CS2: 0x0007 (旧スカイパーフェクTV!2系)
            0x0006 | 0x0007 => BandType::CS,

            // Advanced BS/CS digital (高度BS/110度CSデジタル放送: BS4K/CS4K)
            // BS4K: 0x000B
            // CS4K: 0x000C (運用終了)
            0x000B | 0x000C => BandType::FourK,

            // 124/128度CS digital (スカパー!プレミアムサービス)
            // SPHD: 0x000A
            // SPSD-PerfecTV: 0x0001 (スターデジオ: 運用終了)
            // SPSD-SKY: 0x0003 (運用終了)
            0x000A | 0x0001 | 0x0003 => BandType::SKY,

            // Cable television (ケーブルテレビ)
            // デジタル放送リマックス: 0xFFFE
            // デジタル放送高度リマックス: 0xFFFA
            // JC-HITSトランスモジュレーション: 0xFFFD
            // 高度JC-HITSトランスモジュレーション: 0xFFF9
            // 高度ケーブル自主放送: 0xFFF7
            0xFFFE | 0xFFFA | 0xFFFD | 0xFFF9 | 0xFFF7 => BandType::CATV,

            // Terrestrial digital broadcasting (地上デジタル放送)
            // 県複フラグ=0: 0x7C10 〜 0x7FEF
            // 県複フラグ=1: 0x7810 〜 0x7BEF
            // Actual range: 0x7880 (福岡・北九州) to 0x7FE8 (関東広域)
            0x7800..=0x7FFF => BandType::Terrestrial,

            _ => BandType::Other,
        }
    }

    /// Get display name in Japanese.
    pub fn display_name(&self) -> &'static str {
        match self {
            BandType::Terrestrial => "地上波",
            BandType::BS => "BS",
            BandType::CS => "CS",
            BandType::FourK => "4K",
            BandType::CATV => "CATV",
            BandType::SKY => "スカパー!",
            BandType::Other => "その他",
        }
    }

    /// Get display name in English.
    pub fn name_en(&self) -> &'static str {
        match self {
            BandType::Terrestrial => "Terrestrial",
            BandType::BS => "BS",
            BandType::CS => "CS",
            BandType::FourK => "4K",
            BandType::CATV => "CATV",
            BandType::SKY => "SKY",
            BandType::Other => "Other",
        }
    }
}

/// Message type identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum MessageType {
    // Handshake (0x00xx)
    /// Client hello with protocol version.
    Hello = 0x0001,
    /// Server hello response.
    HelloAck = 0x0002,

    // Tuner control (0x01xx)
    /// Open tuner request.
    OpenTuner = 0x0100,
    /// Open tuner response.
    OpenTunerAck = 0x0101,
    /// Close tuner request.
    CloseTuner = 0x0102,
    /// Close tuner response.
    CloseTunerAck = 0x0103,
    /// Set channel (IBonDriver::SetChannel).
    SetChannel = 0x0104,
    /// Set channel response.
    SetChannelAck = 0x0105,
    /// Set channel by space (IBonDriver2::SetChannel).
    SetChannelSpace = 0x0106,
    /// Set channel by space response.
    SetChannelSpaceAck = 0x0107,

    // Tuner info (0x02xx)
    /// Get signal level request.
    GetSignalLevel = 0x0200,
    /// Get signal level response.
    GetSignalLevelAck = 0x0201,
    /// Enumerate tuning space request.
    EnumTuningSpace = 0x0202,
    /// Enumerate tuning space response.
    EnumTuningSpaceAck = 0x0203,
    /// Enumerate channel name request.
    EnumChannelName = 0x0204,
    /// Enumerate channel name response.
    EnumChannelNameAck = 0x0205,

    // Streaming (0x03xx)
    /// Start TS stream request.
    StartStream = 0x0300,
    /// Start TS stream response.
    StartStreamAck = 0x0301,
    /// Stop TS stream request.
    StopStream = 0x0302,
    /// Stop TS stream response.
    StopStreamAck = 0x0303,
    /// TS data chunk (server to client).
    TsData = 0x0304,
    /// Purge TS stream buffer.
    PurgeStream = 0x0306,
    /// Purge TS stream response.
    PurgeStreamAck = 0x0307,

    // LNB control (0x04xx)
    /// Set LNB power.
    SetLnbPower = 0x0400,
    /// Set LNB power response.
    SetLnbPowerAck = 0x0401,

    // Logical channel selection (0x05xx)
    /// Select logical channel (by NID/TSID/SID).
    SelectLogicalChannel = 0x0500,
    /// Select logical channel response.
    SelectLogicalChannelAck = 0x0501,
    /// Get channel list request.
    GetChannelList = 0x0502,
    /// Get channel list response.
    GetChannelListAck = 0x0503,

    // Misc (0xFFxx)
    /// Error response.
    Error = 0xFF00,
    /// Keep-alive ping.
    Ping = 0xFF01,
    /// Keep-alive pong.
    Pong = 0xFF02,
}

impl TryFrom<u16> for MessageType {
    type Error = u16;

    fn try_from(value: u16) -> Result<Self, u16> {
        match value {
            0x0001 => Ok(MessageType::Hello),
            0x0002 => Ok(MessageType::HelloAck),
            0x0100 => Ok(MessageType::OpenTuner),
            0x0101 => Ok(MessageType::OpenTunerAck),
            0x0102 => Ok(MessageType::CloseTuner),
            0x0103 => Ok(MessageType::CloseTunerAck),
            0x0104 => Ok(MessageType::SetChannel),
            0x0105 => Ok(MessageType::SetChannelAck),
            0x0106 => Ok(MessageType::SetChannelSpace),
            0x0107 => Ok(MessageType::SetChannelSpaceAck),
            0x0200 => Ok(MessageType::GetSignalLevel),
            0x0201 => Ok(MessageType::GetSignalLevelAck),
            0x0202 => Ok(MessageType::EnumTuningSpace),
            0x0203 => Ok(MessageType::EnumTuningSpaceAck),
            0x0204 => Ok(MessageType::EnumChannelName),
            0x0205 => Ok(MessageType::EnumChannelNameAck),
            0x0300 => Ok(MessageType::StartStream),
            0x0301 => Ok(MessageType::StartStreamAck),
            0x0302 => Ok(MessageType::StopStream),
            0x0303 => Ok(MessageType::StopStreamAck),
            0x0304 => Ok(MessageType::TsData),
            0x0306 => Ok(MessageType::PurgeStream),
            0x0307 => Ok(MessageType::PurgeStreamAck),
            0x0400 => Ok(MessageType::SetLnbPower),
            0x0401 => Ok(MessageType::SetLnbPowerAck),
            0x0500 => Ok(MessageType::SelectLogicalChannel),
            0x0501 => Ok(MessageType::SelectLogicalChannelAck),
            0x0502 => Ok(MessageType::GetChannelList),
            0x0503 => Ok(MessageType::GetChannelListAck),
            0xFF00 => Ok(MessageType::Error),
            0xFF01 => Ok(MessageType::Ping),
            0xFF02 => Ok(MessageType::Pong),
            _ => Err(value),
        }
    }
}

impl From<MessageType> for u16 {
    fn from(value: MessageType) -> Self {
        value as u16
    }
}

/// Channel specification for tuning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelSpec {
    /// Simple channel number (IBonDriver v1).
    Channel(u8),
    /// Space and channel number (IBonDriver v2).
    SpaceChannel { space: u32, channel: u32 },
}

/// Messages sent from client to server.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    /// Client hello with protocol version.
    Hello { version: u16 },
    /// Ping for keep-alive.
    Ping,
    /// Open a tuner by path.
    OpenTuner { tuner_path: String },
    /// Open a tuner by group name (auto-select driver from group).
    OpenTunerWithGroup { group_name: String },
    /// Close the current tuner.
    CloseTuner,
    /// Set channel (IBonDriver v1 style).
    SetChannel { channel: u8, priority: i32, exclusive: bool },
    /// Set channel by space (IBonDriver v2 style).
    SetChannelSpace { space: u32, channel: u32, priority: i32, exclusive: bool },
    /// Set channel by space within a group (auto-select driver).
    SetChannelSpaceInGroup { group_name: String, space_idx: u32, channel: u32, priority: i32, exclusive: bool },
    /// Get signal level.
    GetSignalLevel,
    /// Enumerate tuning space.
    EnumTuningSpace { space: u32 },
    /// Enumerate channel name.
    EnumChannelName { space: u32, channel: u32 },
    /// Start TS streaming.
    StartStream,
    /// Stop TS streaming.
    StopStream,
    /// Purge TS stream buffer.
    PurgeStream,
    /// Set LNB power.
    SetLnbPower { enable: bool },
    /// Select logical channel (by NID/TSID/SID from database).
    SelectLogicalChannel {
        nid: u16,
        tsid: u16,
        /// Optional SID filter
        sid: Option<u16>,
    },
    /// Get channel list from server.
    GetChannelList {
        filter: Option<ChannelFilter>,
    },
}

/// Messages sent from server to client.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// Server hello response.
    HelloAck { version: u16, success: bool },
    /// Pong response to ping.
    Pong,
    /// Open tuner response.
    OpenTunerAck {
        success: bool,
        error_code: u16,
        bondriver_version: u8,
    },
    /// Close tuner response.
    CloseTunerAck { success: bool },
    /// Set channel response.
    SetChannelAck { success: bool, error_code: u16 },
    /// Set channel by space response.
    SetChannelSpaceAck { success: bool, error_code: u16 },
    /// Signal level response.
    GetSignalLevelAck { signal_level: f32 },
    /// Enumerate tuning space response.
    EnumTuningSpaceAck { name: Option<String> },
    /// Enumerate channel name response.
    EnumChannelNameAck { name: Option<String> },
    /// Start stream response.
    StartStreamAck { success: bool, error_code: u16 },
    /// Stop stream response.
    StopStreamAck { success: bool },
    /// TS data chunk.
    TsData { data: Vec<u8> },
    /// Purge stream response.
    PurgeStreamAck { success: bool },
    /// Set LNB power response.
    SetLnbPowerAck { success: bool, error_code: u16 },
    /// Select logical channel response.
    SelectLogicalChannelAck {
        success: bool,
        error_code: u16,
        /// The tuner that was selected for tuning.
        tuner_id: Option<String>,
        /// Resolved space/channel.
        space: Option<u32>,
        channel: Option<u32>,
    },
    /// Get channel list response.
    GetChannelListAck {
        channels: Vec<ClientChannelInfo>,
        /// Timestamp for incremental sync.
        timestamp: i64,
    },
    /// Error response.
    Error { error_code: u16, message: String },
}

impl ClientMessage {
    /// Returns the message type for this message.
    pub fn message_type(&self) -> MessageType {
        match self {
            ClientMessage::Hello { .. } => MessageType::Hello,
            ClientMessage::Ping => MessageType::Ping,
            ClientMessage::OpenTuner { .. } => MessageType::OpenTuner,
            ClientMessage::OpenTunerWithGroup { .. } => MessageType::OpenTuner,
            ClientMessage::CloseTuner => MessageType::CloseTuner,
            ClientMessage::SetChannel { .. } => MessageType::SetChannel,
            ClientMessage::SetChannelSpace { .. } => MessageType::SetChannelSpace,
            ClientMessage::SetChannelSpaceInGroup { .. } => MessageType::SetChannelSpace,
            ClientMessage::GetSignalLevel => MessageType::GetSignalLevel,
            ClientMessage::EnumTuningSpace { .. } => MessageType::EnumTuningSpace,
            ClientMessage::EnumChannelName { .. } => MessageType::EnumChannelName,
            ClientMessage::StartStream => MessageType::StartStream,
            ClientMessage::StopStream => MessageType::StopStream,
            ClientMessage::PurgeStream => MessageType::PurgeStream,
            ClientMessage::SetLnbPower { .. } => MessageType::SetLnbPower,
            ClientMessage::SelectLogicalChannel { .. } => MessageType::SelectLogicalChannel,
            ClientMessage::GetChannelList { .. } => MessageType::GetChannelList,
        }
    }
}

impl ServerMessage {
    /// Returns the message type for this message.
    pub fn message_type(&self) -> MessageType {
        match self {
            ServerMessage::HelloAck { .. } => MessageType::HelloAck,
            ServerMessage::Pong => MessageType::Pong,
            ServerMessage::OpenTunerAck { .. } => MessageType::OpenTunerAck,
            ServerMessage::CloseTunerAck { .. } => MessageType::CloseTunerAck,
            ServerMessage::SetChannelAck { .. } => MessageType::SetChannelAck,
            ServerMessage::SetChannelSpaceAck { .. } => MessageType::SetChannelSpaceAck,
            ServerMessage::GetSignalLevelAck { .. } => MessageType::GetSignalLevelAck,
            ServerMessage::EnumTuningSpaceAck { .. } => MessageType::EnumTuningSpaceAck,
            ServerMessage::EnumChannelNameAck { .. } => MessageType::EnumChannelNameAck,
            ServerMessage::StartStreamAck { .. } => MessageType::StartStreamAck,
            ServerMessage::StopStreamAck { .. } => MessageType::StopStreamAck,
            ServerMessage::TsData { .. } => MessageType::TsData,
            ServerMessage::PurgeStreamAck { .. } => MessageType::PurgeStreamAck,
            ServerMessage::SetLnbPowerAck { .. } => MessageType::SetLnbPowerAck,
            ServerMessage::SelectLogicalChannelAck { .. } => MessageType::SelectLogicalChannelAck,
            ServerMessage::GetChannelListAck { .. } => MessageType::GetChannelListAck,
            ServerMessage::Error { .. } => MessageType::Error,
        }
    }
}

// ============================================================================
// Channel Information Types (for DB-backed channel management)
// ============================================================================

/// Channel information stored in database.
/// Uniquely identified by (nid, sid, tsid, manual_sheet).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// Network ID (from SDT original_network_id)
    pub nid: u16,
    /// Service ID
    pub sid: u16,
    /// Transport Stream ID
    pub tsid: u16,
    /// User-defined sheet number for manual grouping (None = default)
    pub manual_sheet: Option<u16>,

    /// Raw service name (ARIB encoded string)
    pub raw_name: Option<String>,
    /// Normalized channel name
    pub channel_name: Option<String>,
    /// Physical channel number (from NIT)
    pub physical_ch: Option<u8>,
    /// Remote control key ID (from NIT)
    pub remote_control_key: Option<u8>,
    /// Service type (0x01=TV, 0x02=Radio, etc.)
    pub service_type: Option<u8>,
    /// Network name (from NIT)
    pub network_name: Option<String>,

    /// BonDriver Space number (recorded during scan)
    pub bon_space: Option<u32>,
    /// BonDriver Channel number (recorded during scan)
    pub bon_channel: Option<u32>,

    /// Band type classification (0=Terrestrial, 1=BS, 2=CS, 3=4K, 4=Other)
    pub band_type: Option<u8>,
    /// Terrestrial region name (e.g., "福島", "宮城") - for Terrestrial only
    pub terrestrial_region: Option<String>,
}

impl ChannelInfo {
    /// Create a new ChannelInfo with minimal required fields.
    pub fn new(nid: u16, sid: u16, tsid: u16) -> Self {
        Self {
            nid,
            sid,
            tsid,
            manual_sheet: None,
            raw_name: None,
            channel_name: None,
            physical_ch: None,
            remote_control_key: None,
            service_type: None,
            network_name: None,
            bon_space: None,
            bon_channel: None,
            band_type: None,
            terrestrial_region: None,
        }
    }

    /// Generate unique key tuple for this channel.
    pub fn unique_key(&self) -> (u16, u16, u16, Option<u16>) {
        (self.nid, self.sid, self.tsid, self.manual_sheet)
    }

    /// Generate service key (ignores manual_sheet).
    pub fn service_key(&self) -> (u16, u16, u16) {
        (self.nid, self.sid, self.tsid)
    }
}

/// Channel selector for tuning requests.
/// Supports both physical (direct) and logical (DB-backed) modes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelSelector {
    /// Physical mode: Direct tuner/space/channel specification.
    /// Bypasses DB is_enabled flag - always attempts to tune.
    Physical {
        tuner_id: String,
        space: u32,
        channel: u32,
    },

    /// Logical mode: NID/TSID-based selection.
    /// Server selects best available tuner from DB based on priority.
    Logical {
        nid: u16,
        tsid: u16,
        /// Optional SID filter
        sid: Option<u16>,
    },
}

impl ChannelSelector {
    /// Create a physical channel selector.
    pub fn physical(tuner_id: impl Into<String>, space: u32, channel: u32) -> Self {
        Self::Physical {
            tuner_id: tuner_id.into(),
            space,
            channel,
        }
    }

    /// Create a logical channel selector.
    pub fn logical(nid: u16, tsid: u16, sid: Option<u16>) -> Self {
        Self::Logical { nid, tsid, sid }
    }

    /// Returns true if this is a physical (direct) selection.
    pub fn is_physical(&self) -> bool {
        matches!(self, Self::Physical { .. })
    }

    /// Returns true if DB is_enabled flag should be checked.
    /// Physical mode bypasses this check.
    pub fn should_check_enabled(&self) -> bool {
        !self.is_physical()
    }
}

// ============================================================================
// Channel List Synchronization Types
// ============================================================================

/// Channel list synchronization messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChannelListMessage {
    /// Client to Server: Request channel list.
    Request {
        filter: Option<ChannelFilter>,
    },

    /// Server to Client: Full channel list response.
    Response {
        channels: Vec<ClientChannelInfo>,
        /// Timestamp for incremental sync
        timestamp: i64,
    },

    /// Server to Client: Incremental update notification.
    Update {
        added: Vec<ClientChannelInfo>,
        updated: Vec<ClientChannelInfo>,
        removed: Vec<ChannelKey>,
        timestamp: i64,
    },
}

/// Filter for channel list requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelFilter {
    pub nid: Option<u16>,
    pub tsid: Option<u16>,
    pub broadcast_type: Option<BroadcastType>,
    pub enabled_only: bool,
}

/// Broadcast type classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BroadcastType {
    /// Terrestrial digital (地上波)
    Terrestrial,
    /// BS digital
    BS,
    /// CS digital (CS1, CS2)
    CS,
}

/// Channel key for identifying removed channels in updates.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelKey {
    pub tuner_id: String,
    pub space: u32,
    pub channel: u32,
}

impl ChannelKey {
    pub fn new(tuner_id: impl Into<String>, space: u32, channel: u32) -> Self {
        Self {
            tuner_id: tuner_id.into(),
            space,
            channel,
        }
    }
}

/// Channel information sent to clients (optimized for display).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientChannelInfo {
    // Identifiers
    pub nid: u16,
    pub sid: u16,
    pub tsid: u16,

    // Display information
    pub channel_name: String,
    pub network_name: Option<String>,
    pub service_type: u8,
    pub remote_control_key: Option<u8>,

    // BonDriver compatibility (for TVTest display)
    pub space_name: String,
    pub channel_display_name: String,

    // Selection priority
    pub priority: i32,
}

impl ClientChannelInfo {
    /// Create from ChannelInfo with additional display fields.
    pub fn from_channel_info(
        info: &ChannelInfo,
        space_name: String,
        priority: i32,
    ) -> Self {
        Self {
            nid: info.nid,
            sid: info.sid,
            tsid: info.tsid,
            channel_name: info.channel_name.clone().unwrap_or_default(),
            network_name: info.network_name.clone(),
            service_type: info.service_type.unwrap_or(0x01),
            remote_control_key: info.remote_control_key,
            space_name,
            channel_display_name: info.channel_name.clone().unwrap_or_default(),
            priority,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_type_roundtrip() {
        let types = [
            MessageType::Hello,
            MessageType::HelloAck,
            MessageType::OpenTuner,
            MessageType::TsData,
            MessageType::Error,
        ];

        for msg_type in types {
            let value: u16 = msg_type.into();
            let recovered = MessageType::try_from(value).unwrap();
            assert_eq!(msg_type, recovered);
        }
    }

    #[test]
    fn test_channel_info_keys() {
        let ch = ChannelInfo::new(0x7FE8, 1024, 32736);
        assert_eq!(ch.unique_key(), (0x7FE8, 1024, 32736, None));
        assert_eq!(ch.service_key(), (0x7FE8, 1024, 32736));
    }

    #[test]
    fn test_channel_selector() {
        let physical = ChannelSelector::physical("tuner0", 0, 13);
        assert!(physical.is_physical());
        assert!(!physical.should_check_enabled());

        let logical = ChannelSelector::logical(0x7FE8, 32736, Some(1024));
        assert!(!logical.is_physical());
        assert!(logical.should_check_enabled());
    }
}
