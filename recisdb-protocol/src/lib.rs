//! Network protocol definitions for recisdb BonDriver proxy.
//!
//! This crate defines the binary protocol used for communication between
//! the recisdb-proxy server and BonDriver clients.
//!
//! # Frame Format
//!
//! ```text
//! +--------+--------+--------+------------------+
//! | Magic  | Length | Type   |     Payload      |
//! | "BNDP" | u32 LE | u16 LE |    (variable)    |
//! +--------+--------+--------+------------------+
//! | 4 bytes| 4 bytes| 2 bytes|  Length bytes    |
//! ```
//!
//! # Example
//!
//! ```rust
//! use recisdb_protocol::{ClientMessage, encode_client_message, decode_header, decode_client_message};
//! use bytes::Bytes;
//!
//! // Encode a message
//! let msg = ClientMessage::Hello { version: 1 };
//! let encoded = encode_client_message(&msg).unwrap();
//!
//! // Decode the header
//! let header = decode_header(&encoded).unwrap().unwrap();
//!
//! // Decode the payload
//! let payload = Bytes::copy_from_slice(&encoded[10..]);
//! let decoded = decode_client_message(header.message_type, payload).unwrap();
//! ```
//!
//! # Channel Management Types
//!
//! The crate also provides types for DB-backed channel management:
//!
//! - [`ChannelInfo`]: Full channel information stored in database
//! - [`ChannelSelector`]: Physical or logical channel selection mode
//! - [`BroadcastType`]: Terrestrial/BS/CS classification
//! - [`broadcast_region`]: NID-based region classification
//!
//! ```rust
//! use recisdb_protocol::{ChannelInfo, ChannelSelector, BroadcastType};
//! use recisdb_protocol::broadcast_region::{classify_nid, TerrestrialRegion};
//!
//! // Create channel info
//! let mut ch = ChannelInfo::new(0x7FE8, 1024, 32736);
//! ch.channel_name = Some("NHK総合".to_string());
//!
//! // Classify NID to broadcast type and region
//! let (btype, region) = classify_nid(0x7FE8);
//! assert_eq!(btype, BroadcastType::Terrestrial);
//! assert_eq!(region, Some(TerrestrialRegion::Kanto));
//!
//! // Channel selection modes
//! let physical = ChannelSelector::physical("tuner0", 0, 13);
//! let logical = ChannelSelector::logical(0x7FE8, 32736, Some(1024));
//! ```

pub mod broadcast_region;
pub mod codec;
pub mod error;
pub mod types;

pub use codec::{
    decode_client_message, decode_header, decode_server_message, encode_client_message,
    encode_server_message, FrameHeader, HEADER_SIZE,
};
pub use error::{ClientError, ErrorCode, ProtocolError, ServerError};
pub use types::{
    // Existing types
    ChannelSpec, ClientMessage, MessageType, ServerMessage, MAGIC, MAX_FRAME_SIZE, MAX_TS_CHUNK_SIZE,
    PROTOCOL_VERSION, BandType,
    // New channel management types
    BroadcastType, ChannelFilter, ChannelInfo, ChannelKey, ChannelListMessage, ChannelSelector,
    ClientChannelInfo,
};
