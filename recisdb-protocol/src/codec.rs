//! Codec for encoding and decoding protocol messages.
//!
//! Frame format:
//! ```text
//! +--------+--------+--------+------------------+
//! | Magic  | Length | Type   |     Payload      |
//! | "BNDP" | u32 LE | u16 LE |    (variable)    |
//! +--------+--------+--------+------------------+
//! | 4 bytes| 4 bytes| 2 bytes|  Length bytes    |
//! ```

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::error::ProtocolError;
use crate::types::*;

/// Frame header size: 4 (magic) + 4 (length) + 2 (type) = 10 bytes.
pub const HEADER_SIZE: usize = 10;

/// Encode a client message into bytes.
pub fn encode_client_message(msg: &ClientMessage) -> Result<Bytes, ProtocolError> {
    let mut payload = BytesMut::new();

    match msg {
        ClientMessage::Hello { version } => {
            payload.put_u16_le(*version);
        }
        ClientMessage::Ping => {
            // Empty payload
        }
        ClientMessage::OpenTuner { tuner_path } => {
            let path_bytes = tuner_path.as_bytes();
            payload.put_u16_le(path_bytes.len() as u16);
            payload.put_slice(path_bytes);
        }
        ClientMessage::OpenTunerWithGroup { group_name } => {
            let name_bytes = group_name.as_bytes();
            payload.put_u16_le(name_bytes.len() as u16);
            payload.put_slice(name_bytes);
        }
        ClientMessage::CloseTuner => {
            // Empty payload
        }
        ClientMessage::SetChannel { channel, priority, exclusive } => {
            payload.put_u8(*channel);
            payload.put_i32_le(*priority);
            payload.put_u8(if *exclusive { 1 } else { 0 });
        }
        ClientMessage::SetChannelSpace { space, channel, priority, exclusive } => {
            payload.put_u32_le(*space);
            payload.put_u32_le(*channel);
            payload.put_i32_le(*priority);
            payload.put_u8(if *exclusive { 1 } else { 0 });
        }
        ClientMessage::SetChannelSpaceInGroup { group_name, space_idx, channel, priority, exclusive } => {
            let name_bytes = group_name.as_bytes();
            payload.put_u16_le(name_bytes.len() as u16);
            payload.put_slice(name_bytes);
            payload.put_u32_le(*space_idx);
            payload.put_u32_le(*channel);
            payload.put_i32_le(*priority);
            payload.put_u8(if *exclusive { 1 } else { 0 });
        }
        ClientMessage::GetSignalLevel => {
            // Empty payload
        }
        ClientMessage::EnumTuningSpace { space } => {
            payload.put_u32_le(*space);
        }
        ClientMessage::EnumChannelName { space, channel } => {
            payload.put_u32_le(*space);
            payload.put_u32_le(*channel);
        }
        ClientMessage::StartStream => {
            // Empty payload
        }
        ClientMessage::StopStream => {
            // Empty payload
        }
        ClientMessage::PurgeStream => {
            // Empty payload
        }
        ClientMessage::SetLnbPower { enable } => {
            payload.put_u8(if *enable { 1 } else { 0 });
        }
        ClientMessage::SelectLogicalChannel { nid, tsid, sid } => {
            payload.put_u16_le(*nid);
            payload.put_u16_le(*tsid);
            match sid {
                Some(s) => {
                    payload.put_u8(1); // has sid
                    payload.put_u16_le(*s);
                }
                None => {
                    payload.put_u8(0); // no sid
                }
            }
        }
        ClientMessage::GetChannelList { filter } => {
            match filter {
                Some(f) => {
                    payload.put_u8(1); // has filter
                    encode_channel_filter(&mut payload, f);
                }
                None => {
                    payload.put_u8(0); // no filter
                }
            }
        }
    }

    encode_frame(msg.message_type(), payload.freeze())
}

/// Encode a server message into bytes.
pub fn encode_server_message(msg: &ServerMessage) -> Result<Bytes, ProtocolError> {
    let mut payload = BytesMut::new();

    match msg {
        ServerMessage::HelloAck { version, success } => {
            payload.put_u16_le(*version);
            payload.put_u8(if *success { 1 } else { 0 });
        }
        ServerMessage::Pong => {
            // Empty payload
        }
        ServerMessage::OpenTunerAck {
            success,
            error_code,
            bondriver_version,
        } => {
            payload.put_u8(if *success { 1 } else { 0 });
            payload.put_u16_le(*error_code);
            payload.put_u8(*bondriver_version);
        }
        ServerMessage::CloseTunerAck { success } => {
            payload.put_u8(if *success { 1 } else { 0 });
        }
        ServerMessage::SetChannelAck { success, error_code } => {
            payload.put_u8(if *success { 1 } else { 0 });
            payload.put_u16_le(*error_code);
        }
        ServerMessage::SetChannelSpaceAck { success, error_code } => {
            payload.put_u8(if *success { 1 } else { 0 });
            payload.put_u16_le(*error_code);
        }
        ServerMessage::GetSignalLevelAck { signal_level } => {
            payload.put_f32_le(*signal_level);
        }
        ServerMessage::EnumTuningSpaceAck { name } => {
            encode_optional_string(&mut payload, name);
        }
        ServerMessage::EnumChannelNameAck { name } => {
            encode_optional_string(&mut payload, name);
        }
        ServerMessage::StartStreamAck { success, error_code } => {
            payload.put_u8(if *success { 1 } else { 0 });
            payload.put_u16_le(*error_code);
        }
        ServerMessage::StopStreamAck { success } => {
            payload.put_u8(if *success { 1 } else { 0 });
        }
        ServerMessage::TsData { data } => {
            payload.put_slice(data);
        }
        ServerMessage::PurgeStreamAck { success } => {
            payload.put_u8(if *success { 1 } else { 0 });
        }
        ServerMessage::SetLnbPowerAck { success, error_code } => {
            payload.put_u8(if *success { 1 } else { 0 });
            payload.put_u16_le(*error_code);
        }
        ServerMessage::Error { error_code, message } => {
            payload.put_u16_le(*error_code);
            let msg_bytes = message.as_bytes();
            payload.put_u16_le(msg_bytes.len() as u16);
            payload.put_slice(msg_bytes);
        }
        ServerMessage::SelectLogicalChannelAck {
            success,
            error_code,
            tuner_id,
            space,
            channel,
        } => {
            payload.put_u8(if *success { 1 } else { 0 });
            payload.put_u16_le(*error_code);
            encode_optional_string(&mut payload, tuner_id);
            encode_optional_u32(&mut payload, space);
            encode_optional_u32(&mut payload, channel);
        }
        ServerMessage::GetChannelListAck { channels, timestamp } => {
            payload.put_i64_le(*timestamp);
            payload.put_u32_le(channels.len() as u32);
            for ch in channels {
                encode_client_channel_info(&mut payload, ch);
            }
        }
    }

    encode_frame(msg.message_type(), payload.freeze())
}

/// Encode a frame with magic, length, type, and payload.
fn encode_frame(msg_type: MessageType, payload: Bytes) -> Result<Bytes, ProtocolError> {
    let payload_len = payload.len() as u32;
    if payload_len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge(payload_len, MAX_FRAME_SIZE));
    }

    let mut frame = BytesMut::with_capacity(HEADER_SIZE + payload.len());
    frame.put_slice(&MAGIC);
    frame.put_u32_le(payload_len);
    frame.put_u16_le(msg_type.into());
    frame.put_slice(&payload);

    Ok(frame.freeze())
}

fn encode_optional_string(buf: &mut BytesMut, s: &Option<String>) {
    match s {
        Some(s) => {
            let bytes = s.as_bytes();
            buf.put_u16_le(bytes.len() as u16);
            buf.put_slice(bytes);
        }
        None => {
            buf.put_u16_le(0xFFFF); // Marker for None
        }
    }
}

fn decode_optional_string(buf: &mut Bytes) -> Result<Option<String>, ProtocolError> {
    if buf.remaining() < 2 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 2,
            actual: buf.remaining(),
        });
    }
    let len = buf.get_u16_le();
    if len == 0xFFFF {
        return Ok(None);
    }
    if buf.remaining() < len as usize {
        return Err(ProtocolError::IncompleteFrame {
            expected: len as usize,
            actual: buf.remaining(),
        });
    }
    let bytes = buf.copy_to_bytes(len as usize);
    String::from_utf8(bytes.to_vec())
        .map(Some)
        .map_err(|e| ProtocolError::DecodeError(e.to_string()))
}

fn encode_optional_u32(buf: &mut BytesMut, val: &Option<u32>) {
    match val {
        Some(v) => {
            buf.put_u8(1);
            buf.put_u32_le(*v);
        }
        None => {
            buf.put_u8(0);
        }
    }
}

fn decode_optional_u32(buf: &mut Bytes) -> Result<Option<u32>, ProtocolError> {
    if buf.remaining() < 1 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 1,
            actual: buf.remaining(),
        });
    }
    let has_value = buf.get_u8() != 0;
    if has_value {
        if buf.remaining() < 4 {
            return Err(ProtocolError::IncompleteFrame {
                expected: 4,
                actual: buf.remaining(),
            });
        }
        Ok(Some(buf.get_u32_le()))
    } else {
        Ok(None)
    }
}

fn encode_optional_u16(buf: &mut BytesMut, val: &Option<u16>) {
    match val {
        Some(v) => {
            buf.put_u8(1);
            buf.put_u16_le(*v);
        }
        None => {
            buf.put_u8(0);
        }
    }
}

fn decode_optional_u16(buf: &mut Bytes) -> Result<Option<u16>, ProtocolError> {
    if buf.remaining() < 1 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 1,
            actual: buf.remaining(),
        });
    }
    let has_value = buf.get_u8() != 0;
    if has_value {
        if buf.remaining() < 2 {
            return Err(ProtocolError::IncompleteFrame {
                expected: 2,
                actual: buf.remaining(),
            });
        }
        Ok(Some(buf.get_u16_le()))
    } else {
        Ok(None)
    }
}

fn encode_optional_u8(buf: &mut BytesMut, val: &Option<u8>) {
    match val {
        Some(v) => {
            buf.put_u8(1);
            buf.put_u8(*v);
        }
        None => {
            buf.put_u8(0);
        }
    }
}

fn decode_optional_u8(buf: &mut Bytes) -> Result<Option<u8>, ProtocolError> {
    if buf.remaining() < 1 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 1,
            actual: buf.remaining(),
        });
    }
    let has_value = buf.get_u8() != 0;
    if has_value {
        if buf.remaining() < 1 {
            return Err(ProtocolError::IncompleteFrame {
                expected: 1,
                actual: buf.remaining(),
            });
        }
        Ok(Some(buf.get_u8()))
    } else {
        Ok(None)
    }
}

fn encode_channel_filter(buf: &mut BytesMut, filter: &ChannelFilter) {
    encode_optional_u16(buf, &filter.nid);
    encode_optional_u16(buf, &filter.tsid);
    match &filter.broadcast_type {
        Some(bt) => {
            buf.put_u8(1);
            buf.put_u8(match bt {
                BroadcastType::Terrestrial => 0,
                BroadcastType::BS => 1,
                BroadcastType::CS => 2,
            });
        }
        None => {
            buf.put_u8(0);
        }
    }
    buf.put_u8(if filter.enabled_only { 1 } else { 0 });
}

fn decode_channel_filter(buf: &mut Bytes) -> Result<ChannelFilter, ProtocolError> {
    let nid = decode_optional_u16(buf)?;
    let tsid = decode_optional_u16(buf)?;
    let broadcast_type = if buf.remaining() < 1 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 1,
            actual: buf.remaining(),
        });
    } else if buf.get_u8() != 0 {
        if buf.remaining() < 1 {
            return Err(ProtocolError::IncompleteFrame {
                expected: 1,
                actual: buf.remaining(),
            });
        }
        Some(match buf.get_u8() {
            0 => BroadcastType::Terrestrial,
            1 => BroadcastType::BS,
            _ => BroadcastType::CS,
        })
    } else {
        None
    };
    if buf.remaining() < 1 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 1,
            actual: buf.remaining(),
        });
    }
    let enabled_only = buf.get_u8() != 0;
    Ok(ChannelFilter {
        nid,
        tsid,
        broadcast_type,
        enabled_only,
    })
}

fn encode_client_channel_info(buf: &mut BytesMut, ch: &ClientChannelInfo) {
    buf.put_u16_le(ch.nid);
    buf.put_u16_le(ch.sid);
    buf.put_u16_le(ch.tsid);
    encode_string(buf, &ch.channel_name);
    encode_optional_string(buf, &ch.network_name);
    buf.put_u8(ch.service_type);
    encode_optional_u8(buf, &ch.remote_control_key);
    encode_string(buf, &ch.space_name);
    encode_string(buf, &ch.channel_display_name);
    buf.put_i32_le(ch.priority);
}

fn decode_client_channel_info(buf: &mut Bytes) -> Result<ClientChannelInfo, ProtocolError> {
    if buf.remaining() < 6 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 6,
            actual: buf.remaining(),
        });
    }
    let nid = buf.get_u16_le();
    let sid = buf.get_u16_le();
    let tsid = buf.get_u16_le();
    let channel_name = decode_string(buf)?;
    let network_name = decode_optional_string(buf)?;
    if buf.remaining() < 1 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 1,
            actual: buf.remaining(),
        });
    }
    let service_type = buf.get_u8();
    let remote_control_key = decode_optional_u8(buf)?;
    let space_name = decode_string(buf)?;
    let channel_display_name = decode_string(buf)?;
    if buf.remaining() < 4 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 4,
            actual: buf.remaining(),
        });
    }
    let priority = buf.get_i32_le();

    Ok(ClientChannelInfo {
        nid,
        sid,
        tsid,
        channel_name,
        network_name,
        service_type,
        remote_control_key,
        space_name,
        channel_display_name,
        priority,
    })
}

fn encode_string(buf: &mut BytesMut, s: &str) {
    let bytes = s.as_bytes();
    buf.put_u16_le(bytes.len() as u16);
    buf.put_slice(bytes);
}

fn decode_string(buf: &mut Bytes) -> Result<String, ProtocolError> {
    if buf.remaining() < 2 {
        return Err(ProtocolError::IncompleteFrame {
            expected: 2,
            actual: buf.remaining(),
        });
    }
    let len = buf.get_u16_le() as usize;
    if buf.remaining() < len {
        return Err(ProtocolError::IncompleteFrame {
            expected: len,
            actual: buf.remaining(),
        });
    }
    let bytes = buf.copy_to_bytes(len);
    String::from_utf8(bytes.to_vec())
        .map_err(|e| ProtocolError::DecodeError(e.to_string()))
}

/// Frame header information.
#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub payload_len: u32,
    pub message_type: MessageType,
}

/// Try to decode a frame header from the buffer.
/// Returns None if there's not enough data yet.
pub fn decode_header(buf: &[u8]) -> Result<Option<FrameHeader>, ProtocolError> {
    if buf.len() < HEADER_SIZE {
        return Ok(None);
    }

    // Check magic
    let magic: [u8; 4] = buf[0..4].try_into().unwrap();
    if magic != MAGIC {
        return Err(ProtocolError::InvalidMagic(magic));
    }

    // Read length
    let payload_len = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    if payload_len > MAX_FRAME_SIZE {
        return Err(ProtocolError::FrameTooLarge(payload_len, MAX_FRAME_SIZE));
    }

    // Read message type
    let type_val = u16::from_le_bytes(buf[8..10].try_into().unwrap());
    let message_type = MessageType::try_from(type_val)
        .map_err(|v| ProtocolError::UnknownMessageType(v))?;

    Ok(Some(FrameHeader {
        payload_len,
        message_type,
    }))
}

/// Decode a client message from a complete frame buffer.
/// The buffer should start at the payload (after the header).
pub fn decode_client_message(
    msg_type: MessageType,
    mut payload: Bytes,
) -> Result<ClientMessage, ProtocolError> {
    match msg_type {
        MessageType::Hello => {
            if payload.remaining() < 2 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 2,
                    actual: payload.remaining(),
                });
            }
            let version = payload.get_u16_le();
            Ok(ClientMessage::Hello { version })
        }
        MessageType::Ping => Ok(ClientMessage::Ping),
        MessageType::OpenTuner => {
            if payload.remaining() < 2 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 2,
                    actual: payload.remaining(),
                });
            }
            let path_len = payload.get_u16_le() as usize;
            if payload.remaining() < path_len {
                return Err(ProtocolError::IncompleteFrame {
                    expected: path_len,
                    actual: payload.remaining(),
                });
            }
            let path_bytes = payload.copy_to_bytes(path_len);
            let tuner_path = String::from_utf8(path_bytes.to_vec())
                .map_err(|e| ProtocolError::DecodeError(e.to_string()))?;
            
            // Check if this is actually an OpenTunerWithGroup message
            // (client should use proper message type, but handle both)
            if tuner_path.is_empty() {
                // Empty path likely means group message, but we need the group name
                return Err(ProtocolError::DecodeError("Empty tuner path".to_string()));
            }
            Ok(ClientMessage::OpenTuner { tuner_path })
        }
        MessageType::CloseTuner => Ok(ClientMessage::CloseTuner),
        MessageType::SetChannel => {
            if payload.remaining() < 6 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 6,
                    actual: payload.remaining(),
                });
            }
            let channel = payload.get_u8();
            let priority = payload.get_i32_le();
            let exclusive = payload.get_u8() != 0;
            Ok(ClientMessage::SetChannel { channel, priority, exclusive })
        }
        MessageType::SetChannelSpace => {
            if payload.remaining() < 13 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 13,
                    actual: payload.remaining(),
                });
            }
            let space = payload.get_u32_le();
            let channel = payload.get_u32_le();
            let priority = payload.get_i32_le();
            let exclusive = payload.get_u8() != 0;
            Ok(ClientMessage::SetChannelSpace { space, channel, priority, exclusive })
        }
        MessageType::GetSignalLevel => Ok(ClientMessage::GetSignalLevel),
        MessageType::EnumTuningSpace => {
            if payload.remaining() < 4 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 4,
                    actual: payload.remaining(),
                });
            }
            let space = payload.get_u32_le();
            Ok(ClientMessage::EnumTuningSpace { space })
        }
        MessageType::EnumChannelName => {
            if payload.remaining() < 8 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 8,
                    actual: payload.remaining(),
                });
            }
            let space = payload.get_u32_le();
            let channel = payload.get_u32_le();
            Ok(ClientMessage::EnumChannelName { space, channel })
        }
        MessageType::StartStream => Ok(ClientMessage::StartStream),
        MessageType::StopStream => Ok(ClientMessage::StopStream),
        MessageType::PurgeStream => Ok(ClientMessage::PurgeStream),
        MessageType::SetLnbPower => {
            if payload.remaining() < 1 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 1,
                    actual: payload.remaining(),
                });
            }
            let enable = payload.get_u8() != 0;
            Ok(ClientMessage::SetLnbPower { enable })
        }
        MessageType::SelectLogicalChannel => {
            if payload.remaining() < 5 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 5,
                    actual: payload.remaining(),
                });
            }
            let nid = payload.get_u16_le();
            let tsid = payload.get_u16_le();
            let has_sid = payload.get_u8() != 0;
            let sid = if has_sid {
                if payload.remaining() < 2 {
                    return Err(ProtocolError::IncompleteFrame {
                        expected: 2,
                        actual: payload.remaining(),
                    });
                }
                Some(payload.get_u16_le())
            } else {
                None
            };
            Ok(ClientMessage::SelectLogicalChannel { nid, tsid, sid })
        }
        MessageType::GetChannelList => {
            if payload.remaining() < 1 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 1,
                    actual: payload.remaining(),
                });
            }
            let has_filter = payload.get_u8() != 0;
            let filter = if has_filter {
                Some(decode_channel_filter(&mut payload)?)
            } else {
                None
            };
            Ok(ClientMessage::GetChannelList { filter })
        }
        // Treat both OpenTuner and SetChannelSpace message types as group variants
        // when they come with group names (determined by context/implementation)
        // For now, return unknown to force explicit group-based routing
        msg_type @ (MessageType::OpenTuner | MessageType::SetChannelSpace) => {
            // Group messages should be routed differently
            // This is a placeholder - actual implementation should use separate message types
            Err(ProtocolError::UnknownMessageType(msg_type as u16))
        }
        _ => Err(ProtocolError::UnknownMessageType(msg_type as u16)),
    }
}

/// Decode a server message from a complete frame buffer.
/// The buffer should start at the payload (after the header).
pub fn decode_server_message(
    msg_type: MessageType,
    mut payload: Bytes,
) -> Result<ServerMessage, ProtocolError> {
    match msg_type {
        MessageType::HelloAck => {
            if payload.remaining() < 3 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 3,
                    actual: payload.remaining(),
                });
            }
            let version = payload.get_u16_le();
            let success = payload.get_u8() != 0;
            Ok(ServerMessage::HelloAck { version, success })
        }
        MessageType::Pong => Ok(ServerMessage::Pong),
        MessageType::OpenTunerAck => {
            if payload.remaining() < 4 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 4,
                    actual: payload.remaining(),
                });
            }
            let success = payload.get_u8() != 0;
            let error_code = payload.get_u16_le();
            let bondriver_version = payload.get_u8();
            Ok(ServerMessage::OpenTunerAck {
                success,
                error_code,
                bondriver_version,
            })
        }
        MessageType::CloseTunerAck => {
            if payload.remaining() < 1 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 1,
                    actual: payload.remaining(),
                });
            }
            let success = payload.get_u8() != 0;
            Ok(ServerMessage::CloseTunerAck { success })
        }
        MessageType::SetChannelAck => {
            if payload.remaining() < 3 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 3,
                    actual: payload.remaining(),
                });
            }
            let success = payload.get_u8() != 0;
            let error_code = payload.get_u16_le();
            Ok(ServerMessage::SetChannelAck { success, error_code })
        }
        MessageType::SetChannelSpaceAck => {
            if payload.remaining() < 3 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 3,
                    actual: payload.remaining(),
                });
            }
            let success = payload.get_u8() != 0;
            let error_code = payload.get_u16_le();
            Ok(ServerMessage::SetChannelSpaceAck { success, error_code })
        }
        MessageType::GetSignalLevelAck => {
            if payload.remaining() < 4 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 4,
                    actual: payload.remaining(),
                });
            }
            let signal_level = payload.get_f32_le();
            Ok(ServerMessage::GetSignalLevelAck { signal_level })
        }
        MessageType::EnumTuningSpaceAck => {
            let name = decode_optional_string(&mut payload)?;
            Ok(ServerMessage::EnumTuningSpaceAck { name })
        }
        MessageType::EnumChannelNameAck => {
            let name = decode_optional_string(&mut payload)?;
            Ok(ServerMessage::EnumChannelNameAck { name })
        }
        MessageType::StartStreamAck => {
            if payload.remaining() < 3 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 3,
                    actual: payload.remaining(),
                });
            }
            let success = payload.get_u8() != 0;
            let error_code = payload.get_u16_le();
            Ok(ServerMessage::StartStreamAck { success, error_code })
        }
        MessageType::StopStreamAck => {
            if payload.remaining() < 1 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 1,
                    actual: payload.remaining(),
                });
            }
            let success = payload.get_u8() != 0;
            Ok(ServerMessage::StopStreamAck { success })
        }
        MessageType::TsData => {
            let data = payload.to_vec();
            Ok(ServerMessage::TsData { data })
        }
        MessageType::PurgeStreamAck => {
            if payload.remaining() < 1 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 1,
                    actual: payload.remaining(),
                });
            }
            let success = payload.get_u8() != 0;
            Ok(ServerMessage::PurgeStreamAck { success })
        }
        MessageType::SetLnbPowerAck => {
            if payload.remaining() < 3 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 3,
                    actual: payload.remaining(),
                });
            }
            let success = payload.get_u8() != 0;
            let error_code = payload.get_u16_le();
            Ok(ServerMessage::SetLnbPowerAck { success, error_code })
        }
        MessageType::SelectLogicalChannelAck => {
            if payload.remaining() < 3 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 3,
                    actual: payload.remaining(),
                });
            }
            let success = payload.get_u8() != 0;
            let error_code = payload.get_u16_le();
            let tuner_id = decode_optional_string(&mut payload)?;
            let space = decode_optional_u32(&mut payload)?;
            let channel = decode_optional_u32(&mut payload)?;
            Ok(ServerMessage::SelectLogicalChannelAck {
                success,
                error_code,
                tuner_id,
                space,
                channel,
            })
        }
        MessageType::GetChannelListAck => {
            if payload.remaining() < 12 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 12,
                    actual: payload.remaining(),
                });
            }
            let timestamp = payload.get_i64_le();
            let count = payload.get_u32_le() as usize;
            let mut channels = Vec::with_capacity(count);
            for _ in 0..count {
                channels.push(decode_client_channel_info(&mut payload)?);
            }
            Ok(ServerMessage::GetChannelListAck { channels, timestamp })
        }
        MessageType::Error => {
            if payload.remaining() < 4 {
                return Err(ProtocolError::IncompleteFrame {
                    expected: 4,
                    actual: payload.remaining(),
                });
            }
            let error_code = payload.get_u16_le();
            let msg_len = payload.get_u16_le() as usize;
            if payload.remaining() < msg_len {
                return Err(ProtocolError::IncompleteFrame {
                    expected: msg_len,
                    actual: payload.remaining(),
                });
            }
            let msg_bytes = payload.copy_to_bytes(msg_len);
            let message = String::from_utf8(msg_bytes.to_vec())
                .map_err(|e| ProtocolError::DecodeError(e.to_string()))?;
            Ok(ServerMessage::Error { error_code, message })
        }
        _ => Err(ProtocolError::UnknownMessageType(msg_type as u16)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_hello() {
        let msg = ClientMessage::Hello { version: 1 };
        let encoded = encode_client_message(&msg).unwrap();

        // Verify header
        assert_eq!(&encoded[0..4], &MAGIC);

        let header = decode_header(&encoded).unwrap().unwrap();
        assert_eq!(header.message_type, MessageType::Hello);

        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_client_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_encode_decode_open_tuner() {
        let msg = ClientMessage::OpenTuner {
            tuner_path: "/dev/pt3video0".to_string(),
        };
        let encoded = encode_client_message(&msg).unwrap();

        let header = decode_header(&encoded).unwrap().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_client_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_encode_decode_ts_data() {
        let data = vec![0x47; 188 * 10]; // 10 TS packets
        let msg = ServerMessage::TsData { data: data.clone() };
        let encoded = encode_server_message(&msg).unwrap();

        let header = decode_header(&encoded).unwrap().unwrap();
        assert_eq!(header.message_type, MessageType::TsData);
        assert_eq!(header.payload_len as usize, data.len());

        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_server_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_encode_decode_signal_level() {
        let msg = ServerMessage::GetSignalLevelAck { signal_level: 23.5 };
        let encoded = encode_server_message(&msg).unwrap();

        let header = decode_header(&encoded).unwrap().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_server_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_invalid_magic() {
        let bad_frame = b"BADPxxxx\x00\x00";
        let result = decode_header(bad_frame);
        assert!(matches!(result, Err(ProtocolError::InvalidMagic(_))));
    }

    #[test]
    fn test_incomplete_header() {
        let partial = b"BNDP\x00";
        let result = decode_header(partial).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_encode_decode_select_logical_channel() {
        // With SID
        let msg = ClientMessage::SelectLogicalChannel {
            nid: 0x7FE8,
            tsid: 32736,
            sid: Some(1024),
        };
        let encoded = encode_client_message(&msg).unwrap();
        let header = decode_header(&encoded).unwrap().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_client_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);

        // Without SID
        let msg = ClientMessage::SelectLogicalChannel {
            nid: 0x7FE8,
            tsid: 32736,
            sid: None,
        };
        let encoded = encode_client_message(&msg).unwrap();
        let header = decode_header(&encoded).unwrap().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_client_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_encode_decode_select_logical_channel_ack() {
        let msg = ServerMessage::SelectLogicalChannelAck {
            success: true,
            error_code: 0,
            tuner_id: Some("tuner0".to_string()),
            space: Some(0),
            channel: Some(13),
        };
        let encoded = encode_server_message(&msg).unwrap();
        let header = decode_header(&encoded).unwrap().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_server_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);

        // Failure case
        let msg = ServerMessage::SelectLogicalChannelAck {
            success: false,
            error_code: 1001,
            tuner_id: None,
            space: None,
            channel: None,
        };
        let encoded = encode_server_message(&msg).unwrap();
        let header = decode_header(&encoded).unwrap().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_server_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_encode_decode_get_channel_list() {
        // Without filter
        let msg = ClientMessage::GetChannelList { filter: None };
        let encoded = encode_client_message(&msg).unwrap();
        let header = decode_header(&encoded).unwrap().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_client_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);

        // With filter
        let msg = ClientMessage::GetChannelList {
            filter: Some(ChannelFilter {
                nid: Some(0x7FE8),
                tsid: None,
                broadcast_type: Some(BroadcastType::Terrestrial),
                enabled_only: true,
            }),
        };
        let encoded = encode_client_message(&msg).unwrap();
        let header = decode_header(&encoded).unwrap().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_client_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn test_encode_decode_get_channel_list_ack() {
        let channels = vec![
            ClientChannelInfo {
                nid: 0x7FE8,
                sid: 1024,
                tsid: 32736,
                channel_name: "NHK総合".to_string(),
                network_name: Some("関東広域圏".to_string()),
                service_type: 0x01,
                remote_control_key: Some(1),
                space_name: "地上D".to_string(),
                channel_display_name: "NHK総合1・東京".to_string(),
                priority: 100,
            },
            ClientChannelInfo {
                nid: 0x7FE8,
                sid: 1025,
                tsid: 32736,
                channel_name: "NHK Eテレ".to_string(),
                network_name: None,
                service_type: 0x01,
                remote_control_key: Some(2),
                space_name: "地上D".to_string(),
                channel_display_name: "NHK Eテレ1・東京".to_string(),
                priority: 99,
            },
        ];
        let msg = ServerMessage::GetChannelListAck {
            channels,
            timestamp: 1704067200,
        };
        let encoded = encode_server_message(&msg).unwrap();
        let header = decode_header(&encoded).unwrap().unwrap();
        let payload = Bytes::copy_from_slice(&encoded[HEADER_SIZE..]);
        let decoded = decode_server_message(header.message_type, payload).unwrap();
        assert_eq!(decoded, msg);
    }
}
