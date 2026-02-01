//! PMT (Program Map Table) parsing.
//!
//! The PMT contains information about a specific program/service,
//! including the PIDs of its elementary streams (video, audio, etc.).

use super::psi::PsiSection;
use super::table_id;

/// Stream type constants.
pub mod stream_type {
    /// MPEG-1 Video.
    pub const MPEG1_VIDEO: u8 = 0x01;
    /// MPEG-2 Video.
    pub const MPEG2_VIDEO: u8 = 0x02;
    /// MPEG-1 Audio.
    pub const MPEG1_AUDIO: u8 = 0x03;
    /// MPEG-2 Audio.
    pub const MPEG2_AUDIO: u8 = 0x04;
    /// MPEG-2 Private Sections.
    pub const PRIVATE_SECTIONS: u8 = 0x05;
    /// MPEG-2 PES Private Data.
    pub const PES_PRIVATE_DATA: u8 = 0x06;
    /// MPEG-4 Video (H.264/AVC).
    pub const H264_VIDEO: u8 = 0x1B;
    /// HEVC Video (H.265).
    pub const H265_VIDEO: u8 = 0x24;
    /// AAC Audio (ADTS).
    pub const AAC_AUDIO: u8 = 0x0F;
    /// AAC Audio (LATM).
    pub const AAC_LATM: u8 = 0x11;
}

/// A single elementary stream entry in the PMT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmtStream {
    /// Stream type.
    pub stream_type: u8,
    /// Elementary PID.
    pub elementary_pid: u16,
    /// ES info descriptors.
    pub descriptors: Vec<u8>,
}

impl PmtStream {
    /// Check if this is a video stream.
    pub fn is_video(&self) -> bool {
        matches!(
            self.stream_type,
            stream_type::MPEG1_VIDEO
                | stream_type::MPEG2_VIDEO
                | stream_type::H264_VIDEO
                | stream_type::H265_VIDEO
        )
    }

    /// Check if this is an audio stream.
    pub fn is_audio(&self) -> bool {
        matches!(
            self.stream_type,
            stream_type::MPEG1_AUDIO
                | stream_type::MPEG2_AUDIO
                | stream_type::AAC_AUDIO
                | stream_type::AAC_LATM
        )
    }

    /// Get a human-readable stream type name.
    pub fn stream_type_name(&self) -> &'static str {
        match self.stream_type {
            stream_type::MPEG1_VIDEO => "MPEG-1 Video",
            stream_type::MPEG2_VIDEO => "MPEG-2 Video",
            stream_type::MPEG1_AUDIO => "MPEG-1 Audio",
            stream_type::MPEG2_AUDIO => "MPEG-2 Audio",
            stream_type::PRIVATE_SECTIONS => "Private Sections",
            stream_type::PES_PRIVATE_DATA => "PES Private Data",
            stream_type::H264_VIDEO => "H.264/AVC Video",
            stream_type::H265_VIDEO => "H.265/HEVC Video",
            stream_type::AAC_AUDIO => "AAC Audio (ADTS)",
            stream_type::AAC_LATM => "AAC Audio (LATM)",
            _ => "Unknown",
        }
    }
}

/// Parsed PMT (Program Map Table).
#[derive(Debug, Clone, Default)]
pub struct PmtTable {
    /// Program number (service ID).
    pub program_number: u16,
    /// Version number.
    pub version_number: u8,
    /// PCR PID.
    pub pcr_pid: u16,
    /// Program info descriptors.
    pub program_info: Vec<u8>,
    /// Elementary streams.
    pub streams: Vec<PmtStream>,
}

impl PmtTable {
    /// Parse a PMT from a PSI section.
    pub fn parse(section: &PsiSection) -> Result<Self, &'static str> {
        if section.header.table_id != table_id::PMT {
            return Err("Not a PMT section");
        }

        let data = section.data;
        if data.len() < 4 {
            return Err("PMT data too short");
        }

        let pcr_pid = ((data[0] as u16 & 0x1F) << 8) | data[1] as u16;
        let program_info_length = ((data[2] as usize & 0x0F) << 8) | data[3] as usize;

        if data.len() < 4 + program_info_length {
            return Err("Invalid program info length");
        }

        let program_info = data[4..4 + program_info_length].to_vec();

        let mut pmt = PmtTable {
            program_number: section.header.table_id_extension,
            version_number: section.header.version_number,
            pcr_pid,
            program_info,
            streams: Vec::new(),
        };

        // Parse elementary stream loop
        let mut offset = 4 + program_info_length;
        while offset + 5 <= data.len() {
            let stream_type = data[offset];
            let elementary_pid = ((data[offset + 1] as u16 & 0x1F) << 8) | data[offset + 2] as u16;
            let es_info_length = ((data[offset + 3] as usize & 0x0F) << 8) | data[offset + 4] as usize;

            offset += 5;

            if offset + es_info_length > data.len() {
                break;
            }

            let descriptors = data[offset..offset + es_info_length].to_vec();
            offset += es_info_length;

            pmt.streams.push(PmtStream {
                stream_type,
                elementary_pid,
                descriptors,
            });
        }

        Ok(pmt)
    }

    /// Get video stream PIDs.
    pub fn get_video_pids(&self) -> Vec<u16> {
        self.streams
            .iter()
            .filter(|s| s.is_video())
            .map(|s| s.elementary_pid)
            .collect()
    }

    /// Get audio stream PIDs.
    pub fn get_audio_pids(&self) -> Vec<u16> {
        self.streams
            .iter()
            .filter(|s| s.is_audio())
            .map(|s| s.elementary_pid)
            .collect()
    }

    /// Get all elementary PIDs.
    pub fn get_all_pids(&self) -> Vec<u16> {
        self.streams.iter().map(|s| s.elementary_pid).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_analyzer::psi::PsiHeader;

    #[test]
    fn test_parse_pmt() {
        // Create a mock PSI section with PMT data
        let data = [
            // PCR PID = 0x0100 (with reserved bits)
            0xE1, 0x00,
            // Program info length = 0
            0xF0, 0x00,
            // Stream 1: Video H.264, PID=0x0100, ES info length=0
            0x1B, 0xE1, 0x00, 0xF0, 0x00,
            // Stream 2: AAC Audio, PID=0x0110, ES info length=0
            0x0F, 0xE1, 0x10, 0xF0, 0x00,
        ];

        let header = PsiHeader {
            table_id: table_id::PMT,
            section_syntax_indicator: true,
            section_length: 22,
            table_id_extension: 0x0101, // Program number
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

        let pmt = PmtTable::parse(&section).unwrap();

        assert_eq!(pmt.program_number, 0x0101);
        assert_eq!(pmt.pcr_pid, 0x0100);
        assert_eq!(pmt.streams.len(), 2);

        assert_eq!(pmt.streams[0].stream_type, stream_type::H264_VIDEO);
        assert_eq!(pmt.streams[0].elementary_pid, 0x0100);
        assert!(pmt.streams[0].is_video());

        assert_eq!(pmt.streams[1].stream_type, stream_type::AAC_AUDIO);
        assert_eq!(pmt.streams[1].elementary_pid, 0x0110);
        assert!(pmt.streams[1].is_audio());
    }

    #[test]
    fn test_pmt_get_pids() {
        let pmt = PmtTable {
            program_number: 1,
            version_number: 0,
            pcr_pid: 0x100,
            program_info: vec![],
            streams: vec![
                PmtStream {
                    stream_type: stream_type::H264_VIDEO,
                    elementary_pid: 0x100,
                    descriptors: vec![],
                },
                PmtStream {
                    stream_type: stream_type::AAC_AUDIO,
                    elementary_pid: 0x110,
                    descriptors: vec![],
                },
                PmtStream {
                    stream_type: stream_type::AAC_AUDIO,
                    elementary_pid: 0x111,
                    descriptors: vec![],
                },
            ],
        };

        assert_eq!(pmt.get_video_pids(), vec![0x100]);
        assert_eq!(pmt.get_audio_pids(), vec![0x110, 0x111]);
        assert_eq!(pmt.get_all_pids(), vec![0x100, 0x110, 0x111]);
    }
}
