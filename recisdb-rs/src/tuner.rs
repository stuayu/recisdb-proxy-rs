use crate::channels::Channel;

#[cfg(target_os = "linux")]
pub use self::linux::{Tuner, UnTunedTuner};
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub use self::unsupported::{Tuner, UnTunedTuner};
#[cfg(target_os = "windows")]
pub use self::windows::{Tuner, UnTunedTuner};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

mod error;

/// Bytes already consumed from a BonDriver-owned chunk but not yet returned
/// to the caller.  This is platform-neutral so the lossless behavior can be
/// regression-tested without loading a Windows DLL.
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct TsCarryOver {
    pending: Vec<u8>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl TsCarryOver {
    const MAX_BYTES: usize = 8 * 1024 * 1024;

    pub(crate) fn read_pending(&mut self, output: &mut [u8]) -> Option<(usize, usize)> {
        if self.pending.is_empty() {
            return None;
        }
        let copied = self.pending.len().min(output.len());
        output[..copied].copy_from_slice(&self.pending[..copied]);
        self.pending.drain(..copied);
        Some((copied, self.pending.len()))
    }

    pub(crate) fn copy_chunk(
        &mut self,
        chunk: &[u8],
        output: &mut [u8],
    ) -> Result<(usize, usize), std::io::Error> {
        let copied = chunk.len().min(output.len());
        let tail = &chunk[copied..];
        if tail.len() > Self::MAX_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "BonDriver returned an implausibly large TS chunk ({} bytes)",
                    chunk.len()
                ),
            ));
        }
        output[..copied].copy_from_slice(&chunk[..copied]);
        self.pending.extend_from_slice(tail);
        Ok((copied, self.pending.len()))
    }

    pub(crate) fn clear(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod carry_over_tests {
    use super::TsCarryOver;

    #[test]
    fn preserves_every_byte_across_smaller_caller_buffers() {
        let input: Vec<u8> = (0..(188 * 5)).map(|n| (n % 251) as u8).collect();
        let mut carry = TsCarryOver::default();
        let mut reconstructed = Vec::new();

        let mut first = [0_u8; 317];
        let (copied, remaining) = carry.copy_chunk(&input, &mut first).unwrap();
        reconstructed.extend_from_slice(&first[..copied]);
        assert_eq!(remaining, input.len() - copied);

        for size in [101, 188, 400] {
            let mut output = vec![0_u8; size];
            if let Some((copied, _)) = carry.read_pending(&mut output) {
                reconstructed.extend_from_slice(&output[..copied]);
            }
        }

        assert_eq!(reconstructed, input);
        assert!(carry.read_pending(&mut [0_u8; 1]).is_none());
    }
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum Voltage {
    _11v,
    _15v,
    Low,
}

pub trait Tunable {
    fn tune(self, ch: Channel, lnb: Option<Voltage>) -> Result<Tuner, std::io::Error>;
}
