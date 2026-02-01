//! MPEG-TS (Transport Stream) Analyzer Module.
//!
//! This module provides parsing functionality for MPEG-TS packets and
//! PSI/SI tables used in digital broadcasting.
//!
//! # Supported Tables
//! - PAT (Program Association Table) - PID 0x0000
//! - PMT (Program Map Table) - Variable PIDs from PAT
//! - NIT (Network Information Table) - PID 0x0010
//! - SDT (Service Description Table) - PID 0x0011
//!
//! # Usage
//! ```ignore
//! use recisdb::ts_analyzer::{TsAnalyzer, AnalyzerConfig};
//!
//! let mut analyzer = TsAnalyzer::new(AnalyzerConfig::default());
//! analyzer.feed(&ts_data);
//!
//! if let Some(info) = analyzer.get_channel_info() {
//!     println!("NID: {}, TSID: {}", info.nid, info.tsid);
//! }
//! ```

mod packet;
mod psi;
mod pat;
mod pmt;
mod nit;
mod sdt;
mod analyzer;
mod descriptors;

pub use packet::{TsPacket, TsHeader, AdaptationField, TS_PACKET_SIZE, SYNC_BYTE};
pub use psi::{PsiSection, PsiHeader};
pub use pat::{PatTable, PatEntry};
pub use pmt::{PmtTable, PmtStream};
pub use nit::{NitTable, NitTransportStream};
pub use sdt::{SdtTable, SdtService};
pub use analyzer::{TsAnalyzer, AnalyzerConfig, AnalyzerResult};
pub use descriptors::{ServiceDescriptor, TerrestrialDeliveryDescriptor};

/// Well-known PIDs in MPEG-TS.
pub mod pid {
    /// Program Association Table PID.
    pub const PAT: u16 = 0x0000;
    /// Conditional Access Table PID.
    pub const CAT: u16 = 0x0001;
    /// Transport Stream Description Table PID.
    pub const TSDT: u16 = 0x0002;
    /// Network Information Table (actual) PID.
    pub const NIT: u16 = 0x0010;
    /// Service Description Table (actual) PID.
    pub const SDT: u16 = 0x0011;
    /// Event Information Table PID.
    pub const EIT: u16 = 0x0012;
    /// Time and Date Table PID.
    pub const TDT: u16 = 0x0014;
    /// Null packet PID (stuffing).
    pub const NULL: u16 = 0x1FFF;
}

/// Table IDs for PSI/SI tables.
pub mod table_id {
    /// Program Association Section.
    pub const PAT: u8 = 0x00;
    /// Conditional Access Section.
    pub const CAT: u8 = 0x01;
    /// Program Map Section.
    pub const PMT: u8 = 0x02;
    /// Network Information Section - actual.
    pub const NIT_ACTUAL: u8 = 0x40;
    /// Network Information Section - other.
    pub const NIT_OTHER: u8 = 0x41;
    /// Service Description Section - actual.
    pub const SDT_ACTUAL: u8 = 0x42;
    /// Service Description Section - other.
    pub const SDT_OTHER: u8 = 0x46;
}

/// Descriptor tags used in PSI/SI tables.
pub mod descriptor_tag {
    /// Service descriptor (0x48).
    pub const SERVICE: u8 = 0x48;
    /// Network name descriptor (0x40).
    pub const NETWORK_NAME: u8 = 0x40;
    /// Service list descriptor (0x41).
    pub const SERVICE_LIST: u8 = 0x41;
    /// Terrestrial delivery system descriptor (0xFA for ISDB-T).
    pub const TERRESTRIAL_DELIVERY: u8 = 0xFA;
    /// Satellite delivery system descriptor.
    pub const SATELLITE_DELIVERY: u8 = 0x43;
    /// Partial reception descriptor (0xFB for ISDB-T 1seg).
    pub const PARTIAL_RECEPTION: u8 = 0xFB;
    /// TS information descriptor (0xCD).
    pub const TS_INFORMATION: u8 = 0xCD;
    /// Extended broadcaster descriptor (0xCE).
    pub const EXTENDED_BROADCASTER: u8 = 0xCE;
    /// Logo transmission descriptor (0xCF).
    pub const LOGO_TRANSMISSION: u8 = 0xCF;
    /// Remote control key descriptor (0xDE for ISDB).
    pub const REMOTE_CONTROL_KEY: u8 = 0xDE;
}
