//! Error types for the recisdb network protocol.

use thiserror::Error;

/// Protocol-level errors that can occur during communication.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// Invalid magic bytes in frame header.
    #[error("Invalid magic bytes: expected 'BNDP', got {0:?}")]
    InvalidMagic([u8; 4]),

    /// Message type is unknown or unsupported.
    #[error("Unknown message type: 0x{0:04X}")]
    UnknownMessageType(u16),

    /// Frame payload is too large.
    #[error("Frame too large: {0} bytes (max: {1})")]
    FrameTooLarge(u32, u32),

    /// Frame payload is incomplete.
    #[error("Incomplete frame: expected {expected} bytes, got {actual}")]
    IncompleteFrame { expected: usize, actual: usize },

    /// Failed to decode message payload.
    #[error("Failed to decode message: {0}")]
    DecodeError(String),

    /// Failed to encode message payload.
    #[error("Failed to encode message: {0}")]
    EncodeError(String),

    /// Protocol version mismatch.
    #[error("Protocol version mismatch: client={client}, server={server}")]
    VersionMismatch { client: u16, server: u16 },
}

/// Server-side errors that can occur during operation.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
    /// Tuner could not be opened.
    #[error("Failed to open tuner: {0}")]
    TunerOpenFailed(String),

    /// Channel could not be set.
    #[error("Failed to set channel: {0}")]
    ChannelSetFailed(String),

    /// Tuner is busy (already in use by another exclusive session).
    #[error("Tuner is busy")]
    TunerBusy,

    /// Session is not authenticated.
    #[error("Not authenticated")]
    NotAuthenticated,

    /// Invalid session state for the requested operation.
    #[error("Invalid session state: {0}")]
    InvalidState(String),

    /// Internal server error.
    #[error("Internal error: {0}")]
    Internal(String),
}

/// Client-side errors that can occur during operation.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// Connection failed.
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    /// Connection was closed unexpectedly.
    #[error("Connection closed")]
    ConnectionClosed,

    /// Request timed out.
    #[error("Request timed out")]
    Timeout,

    /// Server returned an error.
    #[error("Server error: {0}")]
    ServerError(String),
}

/// Error code sent in response messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrorCode {
    /// Operation succeeded.
    Success = 0x0000,
    /// Unknown error occurred.
    Unknown = 0x0001,
    /// Tuner could not be opened.
    TunerOpenFailed = 0x0002,
    /// Channel could not be set.
    ChannelSetFailed = 0x0003,
    /// Tuner is busy.
    TunerBusy = 0x0004,
    /// Not authenticated.
    NotAuthenticated = 0x0005,
    /// Invalid session state.
    InvalidState = 0x0006,
    /// Invalid parameter.
    InvalidParameter = 0x0007,
    /// Protocol error.
    ProtocolError = 0x0008,
}

impl From<u16> for ErrorCode {
    fn from(value: u16) -> Self {
        match value {
            0x0000 => ErrorCode::Success,
            0x0002 => ErrorCode::TunerOpenFailed,
            0x0003 => ErrorCode::ChannelSetFailed,
            0x0004 => ErrorCode::TunerBusy,
            0x0005 => ErrorCode::NotAuthenticated,
            0x0006 => ErrorCode::InvalidState,
            0x0007 => ErrorCode::InvalidParameter,
            0x0008 => ErrorCode::ProtocolError,
            _ => ErrorCode::Unknown,
        }
    }
}

impl From<ErrorCode> for u16 {
    fn from(value: ErrorCode) -> Self {
        value as u16
    }
}

impl ErrorCode {
    /// Returns true if this error code indicates success.
    pub fn is_success(self) -> bool {
        self == ErrorCode::Success
    }
}
