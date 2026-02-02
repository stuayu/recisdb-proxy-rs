//! TS packet quality analyzer.

use std::collections::HashMap;

use crate::tuner::ts_parser::{SYNC_BYTE, TS_PACKET_SIZE};

/// Quality counters for TS stream.
#[derive(Debug, Clone, Copy, Default)]
pub struct TsStreamQuality {
    pub packets_total: u64,
    pub packets_dropped: u64,
    pub packets_scrambled: u64,
    pub packets_error: u64,
}

/// Delta counters for a single analyze call.
#[derive(Debug, Clone, Copy, Default)]
pub struct TsStreamQualityDelta {
    pub packets_total: u64,
    pub packets_dropped: u64,
    pub packets_scrambled: u64,
    pub packets_error: u64,
}

/// TS packet analyzer for continuity and error tracking.
#[derive(Debug, Default)]
pub struct TsPacketAnalyzer {
    last_cc: HashMap<u16, u8>,
    quality: TsStreamQuality,
}

impl TsPacketAnalyzer {
    /// Create a new analyzer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Analyze a chunk of TS data and return the delta counters.
    pub fn analyze(&mut self, data: &[u8]) -> TsStreamQualityDelta {
        let mut delta = TsStreamQualityDelta::default();

        let mut offset = 0;
        while offset + TS_PACKET_SIZE <= data.len() {
            let packet = &data[offset..offset + TS_PACKET_SIZE];
            offset += TS_PACKET_SIZE;

            if packet[0] != SYNC_BYTE {
                continue;
            }

            let transport_error = (packet[1] & 0x80) != 0;
            let pid = ((packet[1] as u16 & 0x1F) << 8) | packet[2] as u16;
            let scrambling = (packet[3] >> 6) & 0x03;
            let adaptation_field = (packet[3] >> 4) & 0x03;
            let continuity_counter = packet[3] & 0x0F;

            delta.packets_total += 1;
            self.quality.packets_total += 1;

            if transport_error {
                delta.packets_error += 1;
                self.quality.packets_error += 1;
            }

            if scrambling != 0 {
                delta.packets_scrambled += 1;
                self.quality.packets_scrambled += 1;
            }

            if pid == 0x1FFF {
                continue;
            }

            if adaptation_field == 0 || adaptation_field == 2 {
                continue;
            }

            let expected = self.last_cc.get(&pid).map(|v| (v + 1) & 0x0F);
            if let Some(expected_cc) = expected {
                if continuity_counter != expected_cc {
                    delta.packets_dropped += 1;
                    self.quality.packets_dropped += 1;
                }
            }
            self.last_cc.insert(pid, continuity_counter);
        }

        delta
    }

    /// Get a snapshot of current quality counters.
    pub fn snapshot(&self) -> TsStreamQuality {
        self.quality
    }

    /// Reset counters.
    pub fn reset(&mut self) {
        self.quality = TsStreamQuality::default();
        self.last_cc.clear();
    }
}
