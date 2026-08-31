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

mod analyzer;
mod descriptors;
mod eit;
mod nit;
mod packet;
mod pat;
mod pmt;
mod psi;
mod sdt;
pub mod service_filter;

pub use analyzer::{AnalyzerConfig, AnalyzerResult, TsAnalyzer};
pub use descriptors::{parse_descriptor_loop, ServiceDescriptor, TerrestrialDeliveryDescriptor};
pub use eit::{EitEvent, EitTable};
pub use nit::{uhf_channel_from_frequency, NitTable, NitTransportStream};
pub use packet::{AdaptationField, TsHeader, TsPacket, SYNC_BYTE, TS_PACKET_SIZE};
pub use pat::{PatEntry, PatTable};
pub use pmt::{PmtStream, PmtTable};
pub use psi::{PsiHeader, PsiSection, SectionCollector};
pub use sdt::{SdtService, SdtTable};

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
    /// M-EIT PID (mobile-receiver EPG, ARIB TR-B14 Vol. 4 Table 13-7,
    /// printed p. 83). This is not a terrestrial/satellite distinction.
    pub const EIT_MOBILE: u16 = 0x0026;
    /// L-EIT PID (partial-reception/portable-receiver EPG, ARIB TR-B14
    /// Vol. 4 Table 13-7, printed p. 83).
    pub const EIT_PARTIAL_RECEPTION: u16 = 0x0027;
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
    /// Event Information Section - actual TS, present/following.
    pub const EIT_PF_ACTUAL: u8 = 0x4E;
    /// Event Information Section - other TS, present/following.
    pub const EIT_PF_OTHER: u8 = 0x4F;
    /// Event Information Section - actual TS, schedule (first of 0x50..=0x5F).
    pub const EIT_SCHEDULE_ACTUAL_START: u8 = 0x50;
    /// Event Information Section - other TS, schedule (last of 0x60..=0x6F).
    pub const EIT_SCHEDULE_OTHER_END: u8 = 0x6F;

    /// Whether `id` is any EIT table (present/following or schedule,
    /// actual or other TS): 0x4E/0x4F plus the contiguous 0x50..=0x6F
    /// schedule range. BS multiplexes carry schedule-other sections for
    /// services on *other* transport streams, so tuning to a single BS
    /// channel can populate the EPG for the whole BS multiplex group.
    pub fn is_eit_table_id(id: u8) -> bool {
        (EIT_PF_ACTUAL..=EIT_SCHEDULE_OTHER_END).contains(&id)
    }

    /// EIT table IDs used by TR-B14's terrestrial transmission.
    ///
    /// Terrestrial broadcasting carries actual-TS EIT only (Vol. 4 §13.1,
    /// printed p. 64): H-EIT[p/f] is 0x4E, H-EIT[schedule basic] is
    /// 0x50..=0x57, and H-EIT[schedule extended] is 0x58..=0x5F. M-EIT and
    /// L-EIT use 0x4E (Table 13-8, printed p. 83).
    pub fn is_terrestrial_eit_table_id(id: u8) -> bool {
        id == EIT_PF_ACTUAL || (EIT_SCHEDULE_ACTUAL_START..=0x5F).contains(&id)
    }
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
    /// Short event descriptor (0x4D).
    pub const SHORT_EVENT: u8 = 0x4D;
    /// Extended event descriptor (0x4E).
    pub const EXTENDED_EVENT: u8 = 0x4E;
    /// Content descriptor (0x54).
    pub const CONTENT: u8 = 0x54;
}

#[cfg(test)]
mod tests {
    use super::table_id;

    #[test]
    fn terrestrial_eit_table_ids_exclude_other_ts() {
        assert!(table_id::is_terrestrial_eit_table_id(0x4E));
        assert!(table_id::is_terrestrial_eit_table_id(0x50));
        assert!(table_id::is_terrestrial_eit_table_id(0x5F));
        assert!(!table_id::is_terrestrial_eit_table_id(0x4F));
        assert!(!table_id::is_terrestrial_eit_table_id(0x60));
        assert!(!table_id::is_terrestrial_eit_table_id(0x6F));
    }
}
