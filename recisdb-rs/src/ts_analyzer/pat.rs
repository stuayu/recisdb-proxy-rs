//! PAT (Program Association Table) parsing.
//!
//! The PAT is transmitted on PID 0x0000 and contains a list of programs
//! with their PMT PIDs.

use super::psi::PsiSection;
use super::table_id;

/// A single PAT entry (program number and PMT PID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatEntry {
    /// Program number (0 = NIT, others = service).
    pub program_number: u16,
    /// PID of the PMT for this program (or NIT PID if program_number = 0).
    pub pid: u16,
}

/// Parsed PAT (Program Association Table).
#[derive(Debug, Clone, Default)]
pub struct PatTable {
    /// Transport stream ID.
    pub transport_stream_id: u16,
    /// Version number.
    pub version_number: u8,
    /// List of programs.
    pub programs: Vec<PatEntry>,
    /// NIT PID (if present in PAT).
    pub nit_pid: Option<u16>,
}

impl PatTable {
    /// Parse a PAT from a PSI section.
    pub fn parse(section: &PsiSection) -> Result<Self, &'static str> {
        if section.header.table_id != table_id::PAT {
            return Err("Not a PAT section");
        }

        let mut pat = PatTable {
            transport_stream_id: section.header.table_id_extension,
            version_number: section.header.version_number,
            programs: Vec::new(),
            nit_pid: None,
        };

        // Each program entry is 4 bytes
        let data = section.data;
        if data.len() % 4 != 0 {
            return Err("Invalid PAT data length");
        }

        for chunk in data.chunks(4) {
            let program_number = ((chunk[0] as u16) << 8) | chunk[1] as u16;
            let pid = ((chunk[2] as u16 & 0x1F) << 8) | chunk[3] as u16;

            if program_number == 0 {
                // NIT PID
                pat.nit_pid = Some(pid);
            } else {
                pat.programs.push(PatEntry {
                    program_number,
                    pid,
                });
            }
        }

        Ok(pat)
    }

    /// Get PMT PID for a specific program number.
    pub fn get_pmt_pid(&self, program_number: u16) -> Option<u16> {
        self.programs
            .iter()
            .find(|p| p.program_number == program_number)
            .map(|p| p.pid)
    }

    /// Get all PMT PIDs.
    pub fn get_all_pmt_pids(&self) -> Vec<u16> {
        self.programs.iter().map(|p| p.pid).collect()
    }

    /// Get all program numbers (service IDs).
    pub fn get_all_program_numbers(&self) -> Vec<u16> {
        self.programs.iter().map(|p| p.program_number).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_analyzer::psi::PsiHeader;

    #[test]
    fn test_parse_pat() {
        // Create a mock PSI section with PAT data
        let data = [
            // Program 1: number=0x0101, PID=0x0100
            0x01, 0x01, 0xE1, 0x00,
            // Program 2: number=0x0102, PID=0x0200
            0x01, 0x02, 0xE2, 0x00,
        ];

        let header = PsiHeader {
            table_id: table_id::PAT,
            section_syntax_indicator: true,
            section_length: 17, // header(5) + data(8) + crc(4)
            table_id_extension: 0x1234, // TSID
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

        let pat = PatTable::parse(&section).unwrap();

        assert_eq!(pat.transport_stream_id, 0x1234);
        assert_eq!(pat.version_number, 1);
        assert_eq!(pat.programs.len(), 2);
        assert_eq!(pat.programs[0].program_number, 0x0101);
        assert_eq!(pat.programs[0].pid, 0x0100);
        assert_eq!(pat.programs[1].program_number, 0x0102);
        assert_eq!(pat.programs[1].pid, 0x0200);
    }

    #[test]
    fn test_pat_with_nit() {
        // PAT with NIT entry (program_number = 0)
        let data = [
            // NIT: number=0x0000, PID=0x0010
            0x00, 0x00, 0xE0, 0x10,
            // Program 1: number=0x0101, PID=0x0100
            0x01, 0x01, 0xE1, 0x00,
        ];

        let header = PsiHeader {
            table_id: table_id::PAT,
            section_syntax_indicator: true,
            section_length: 17,
            table_id_extension: 0x1234,
            version_number: 0,
            current_next_indicator: true,
            section_number: 0,
            last_section_number: 0,
        };

        let section = PsiSection {
            header,
            data: &data,
            crc32: 0,
        };

        let pat = PatTable::parse(&section).unwrap();

        assert_eq!(pat.nit_pid, Some(0x0010));
        assert_eq!(pat.programs.len(), 1);
    }
}
