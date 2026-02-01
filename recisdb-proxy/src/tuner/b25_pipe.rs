// src/tuner/b25_pipe.rs
use std::io::{self, Read, Write};

use b25_sys::{DecoderOptions, StreamDecoder};
use log::{debug, error, info, warn};

const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;

/// How many consecutive packets to check when re-synchronizing TS.
/// Larger value reduces false positives but requires more buffered data.
const RESYNC_CHECK_PACKETS: usize = 5;

/// Find an offset (0..TS_PACKET_SIZE-1) such that packets appear aligned:
/// buf[offset + k*188] == 0x47 for k in 0..RESYNC_CHECK_PACKETS.
fn find_ts_sync_offset(buf: &[u8]) -> Option<usize> {
    let need = TS_PACKET_SIZE * RESYNC_CHECK_PACKETS;
    if buf.len() < need {
        return None;
    }
    // We only need to search within one packet length for phase alignment.
    for start in 0..TS_PACKET_SIZE {
        if start + need > buf.len() {
            break;
        }
        let mut ok = true;
        for k in 0..RESYNC_CHECK_PACKETS {
            if buf[start + k * TS_PACKET_SIZE] != TS_SYNC_BYTE {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(start);
        }
    }
    None
}

pub struct B25Pipe {
    dec: StreamDecoder,
    stash: Vec<u8>,
    tmp: Vec<u8>,
}

impl B25Pipe {
    pub fn new(opt: DecoderOptions) -> io::Result<Self> {
        Ok(Self {
            dec: StreamDecoder::new(opt)?,
            stash: Vec::with_capacity(TS_PACKET_SIZE * 32),
            // Output buffer for decoder drain. Larger buffer to handle bursts.
            // B25 decoder can output more than 262KB in one session, so use 1MB.
            tmp: vec![0u8; 1024 * 1024],  // 1MB buffer
        })
    }

    pub fn reset(&mut self, opt: DecoderOptions) -> io::Result<()> {
        *self = Self::new(opt)?;
        Ok(())
    }

    /// 任意長の入力を入れて、復号済みTSを返す（なければ空Vec）
    pub fn push(&mut self, input: &[u8]) -> io::Result<Vec<u8>> {
        self.stash.extend_from_slice(input);

        // --- TS re-synchronization ---
        // TS reads are not guaranteed to be aligned to 188 bytes boundaries.
        // If the first byte isn't 0x47, try to find a sync phase and realign.
        if self.stash.first().copied() != Some(TS_SYNC_BYTE) {
            if let Some(off) = find_ts_sync_offset(&self.stash) {
                warn!("[B25Pipe] Resync TS: dropping {} bytes", off);
                self.stash.drain(..off);
            } else {
                // Keep at most 187 bytes to allow sync across chunk boundary.
                if self.stash.len() > TS_PACKET_SIZE - 1 {
                    let keep = TS_PACKET_SIZE - 1;
                    let tail = self.stash.split_off(self.stash.len() - keep);
                    self.stash = tail;
                }
                return Ok(Vec::new());
            }
        }

        // 188の倍数分だけ処理
        let full_len = (self.stash.len() / TS_PACKET_SIZE) * TS_PACKET_SIZE;
        if full_len == 0 {
            return Ok(Vec::new());
        }

        // Optional lightweight sanity check (avoid full scan + full clear).
        // If misaligned slips through, try resync again once.
        if self.stash.get(0).copied() != Some(TS_SYNC_BYTE) {
            if let Some(off) = find_ts_sync_offset(&self.stash) {
                warn!("[B25Pipe] Resync TS (2nd): dropping {} bytes", off);
                self.stash.drain(..off);
            } else {
                self.stash.clear();
                return Ok(Vec::new());
            }
        }

        // デコーダーに書き込み
        match self.dec.write_all(&self.stash[..full_len]) {
            Ok(_) => {
                self.stash.drain(..full_len);
            }
            Err(e) => {
                // 書き込みエラーの場合、stashをクリアしてエラーを返す
                // これによりデコーダーの状態をリセットする
                error!("[B25Pipe] Decoder write error, clearing stash: {}", e);
                self.stash.clear();
                return Err(e);
            }
        }

        // 出力回収
        let mut out = Vec::with_capacity(full_len);
        // Guard against pathological decoder behavior.
        let mut loops = 0usize;
        const MAX_DRAIN_LOOPS: usize = 256;
        const MAX_SINGLE_READ: usize = 1024 * 1024;  // 1MB max per read (matches tmp buffer)
        
        loop {
            loops += 1;
            if loops > MAX_DRAIN_LOOPS {
                warn!(
                    "[B25Pipe] Decoder drain loop exceeded {}; breaking to avoid stall",
                    MAX_DRAIN_LOOPS
                );
                break;
            }
            match self.dec.read(&mut self.tmp[..]) {
                Ok(0) => break,
                Ok(n) => {
                    if n > self.tmp.len() {
                        // Some StreamDecoder implementations may incorrectly return
                        // "available bytes" rather than "bytes written into buf".
                        // This indicates the decoder's internal buffer is overflowing.
                        // Cap at a reasonable size to prevent memory explosion.
                        warn!(
                            "[B25Pipe] Decoder returned {} bytes > buffer {} bytes; capping to {} bytes",
                            n,
                            self.tmp.len(),
                            MAX_SINGLE_READ
                        );
                        let capped = std::cmp::min(n, MAX_SINGLE_READ);
                        out.extend_from_slice(&self.tmp[..capped]);
                        // Continue draining to empty the decoder
                        continue;
                    }
                    out.extend_from_slice(&self.tmp[..n]);
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        break;
                    }
                    // 読み込みエラーの場合もエラーを返す
                    error!("[B25Pipe] Decoder read error: {}", e);
                    return Err(e);
                }
            }
        }

        Ok(out)
    }

    /// Completely drain all remaining decoded data from the decoder.
    /// Use this before resetting the decoder to ensure all buffered data is flushed.
    pub fn drain_all(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut loops = 0usize;
        const MAX_DRAIN_LOOPS: usize = 512;  // Allow more loops for complete drain
        
        loop {
            loops += 1;
            if loops > MAX_DRAIN_LOOPS {
                warn!(
                    "[B25Pipe] drain_all exceeded {}; giving up to prevent infinite loop",
                    MAX_DRAIN_LOOPS
                );
                break;
            }
            match self.dec.read(&mut self.tmp[..]) {
                Ok(0) => {
                    debug!("[B25Pipe] drain_all completed after {} loops", loops);
                    break;
                }
                Ok(n) => {
                    if n > self.tmp.len() {
                        // Decoder is reporting more data than buffer - cap it
                        let capped = std::cmp::min(n, self.tmp.len());
                        warn!("[B25Pipe] drain_all: decoder reported {} but buffer is {}, using {}", 
                              n, self.tmp.len(), capped);
                        out.extend_from_slice(&self.tmp[..capped]);
                    } else {
                        out.extend_from_slice(&self.tmp[..n]);
                    }
                }
                Err(e) => {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        debug!("[B25Pipe] drain_all: WouldBlock after {} loops", loops);
                        break;
                    }
                    error!("[B25Pipe] drain_all error: {}", e);
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    /// デコーダーをリセットする（チャンネル変更時に使用）
    pub fn reset_decoder(&mut self, opt: DecoderOptions) -> io::Result<()> {
        info!("[B25Pipe] Resetting decoder");
        // First, try to drain any remaining data
        match self.drain_all() {
            Ok(remaining) => {
                if !remaining.is_empty() {
                    info!("[B25Pipe] Drained {} bytes before reset", remaining.len());
                }
            }
            Err(e) => {
                warn!("[B25Pipe] Error draining decoder: {}", e);
                // Continue with reset anyway
            }
        }
        *self = Self::new(opt)?;
        Ok(())
    }
}
