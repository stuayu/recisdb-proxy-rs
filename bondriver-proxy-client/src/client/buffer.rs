//! Lock-free ring buffer for TS data.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::ptr;
use std::time::Duration;

/// TS packet size.
pub const TS_PACKET_SIZE: usize = 188;

/// Size of the ring buffer (100 MB).
pub const RING_BUFFER_SIZE: usize = TS_PACKET_SIZE * 1024 * 100;

/// When the consumer has fallen so far behind that the buffer is full, it
/// resyncs to near-live by keeping only this many freshest bytes (~3 MB ≈
/// 1.5 s at 16 Mbps) and skipping the rest. Big enough to stay a jitter
/// cushion, small enough that latency does not grow to the full 100 MB.
const RESYNC_KEEP_BYTES: usize = TS_PACKET_SIZE * 1024 * 16;

/// A lock-free ring buffer for TS data.
///
/// This buffer is designed for a single-producer, single-consumer scenario
/// where the network receiver writes data and the BonDriver GetTsStream reads it.
///
/// Data arrival is signaled via a Condvar so that WaitTsStream can block
/// efficiently instead of spinning with sleep() — mirroring the Win32 event
/// used in BonDriverProxy(Ex).
pub struct TsRingBuffer {
    /// The underlying buffer (heap-allocated).
    buffer: Box<[u8]>,
    /// Write position (updated by receiver).
    write_pos: AtomicUsize,
    /// Read position (updated by GetTsStream).
    read_pos: AtomicUsize,
    /// Total number of bytes dropped since creation, either by the producer
    /// (`write`, when the buffer is full) or by the consumer
    /// (`resync_if_overflowing`, skipping stale data to catch up to live).
    /// Both use atomic `fetch_add`, so the two writers do not race. Exposed
    /// via `dropped_bytes()` so overflow is observable.
    dropped_bytes: AtomicUsize,
    /// Condvar for notifying waiting threads when data is available.
    /// Mirrors the manual-reset Win32 event in BonDriverProxy(Ex).
    data_available: Condvar,
    /// Mutex paired with data_available (holds no meaningful state).
    data_mutex: Mutex<()>,
}

#[allow(dead_code)]
impl TsRingBuffer {
    /// Create a new ring buffer.
    pub fn new() -> Self {
        // Allocate directly on heap to avoid stack overflow
        let buffer = vec![0u8; RING_BUFFER_SIZE].into_boxed_slice();
        Self {
            buffer,
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
            dropped_bytes: AtomicUsize::new(0),
            data_available: Condvar::new(),
            data_mutex: Mutex::new(()),
        }
    }

    /// Get the number of bytes available for reading.
    pub fn available(&self) -> usize {
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);
        if write >= read {
            write - read
        } else {
            RING_BUFFER_SIZE - read + write
        }
    }

    /// Get the number of bytes of free space for writing.
    pub fn free_space(&self) -> usize {
        RING_BUFFER_SIZE - self.available() - 1 // -1 to distinguish full from empty
    }

    /// Total number of bytes dropped because the buffer was full (or the write
    /// exceeded the buffer capacity) since creation.  Non-zero indicates the
    /// consumer is not draining fast enough and the TS stream has gaps.
    pub fn dropped_bytes(&self) -> usize {
        self.dropped_bytes.load(Ordering::Relaxed)
    }

    /// Write data to the buffer.
    ///
    /// If the buffer has enough free space, the data is written normally.
    /// If the buffer is full, the newest data that does not fit is dropped by
    /// the producer and counted in `dropped_bytes()`.
    ///
    /// # SPSC correctness and overflow policy
    ///
    /// This is a single-producer / single-consumer ring buffer: the producer
    /// (`write`) owns `write_pos` exclusively and the consumer (`consume`)
    /// owns `read_pos` exclusively.  Earlier versions discarded the *oldest*
    /// data on overflow by advancing `read_pos` from the producer.  That broke
    /// the SPSC contract: `read_pos` is a non-atomic read-modify-write in
    /// `consume`, so a concurrent producer store could be lost or move
    /// `read_pos` backwards, exposing already-consumed or partial TS packets.
    ///
    /// So the producer never touches `read_pos`.  To still keep latency bounded
    /// for live viewing (the point of dropping *oldest*), the drop-oldest
    /// decision is made on the **consumer** side instead: [`read_into`]/[`read`]
    /// call [`resync_if_overflowing`], which — when the buffer is full because
    /// the consumer fell behind — advances `read_pos` to skip stale data and
    /// resume near-live.  Because only the consumer moves `read_pos`, this is
    /// race-free.  The net behavior is drop-oldest latency without the data
    /// race.
    ///
    /// Returns the number of bytes written (0..=data.len()).  The shortfall,
    /// rounded down to a whole number of bytes that fit, is added to
    /// `dropped_bytes()`.  (The round-down to `TS_PACKET_SIZE` keeps the
    /// accepted *count* packet-sized; exact stream-packet alignment depends on
    /// writes arriving packet-aligned, and the demuxer resyncs on 0x47 in any
    /// case.)
    pub fn write(&self, data: &[u8]) -> usize {
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);

        let free = if write >= read {
            RING_BUFFER_SIZE - write + read - 1
        } else {
            read - write - 1
        };

        // Cap to maximum writable size (buffer size - 1).
        let to_write = data.len().min(RING_BUFFER_SIZE - 1);
        if to_write == 0 {
            return 0;
        }

        // If not enough free space, write only what fits and drop the rest.
        // The producer must NOT advance `read_pos` (owned by the consumer), so
        // instead of overwriting the oldest data we drop the excess newest
        // bytes.  Round the accepted amount DOWN to a TS packet boundary so the
        // consumer never sees a partial TS packet.
        let to_write = if to_write > free {
            (free / TS_PACKET_SIZE) * TS_PACKET_SIZE
        } else {
            to_write
        };

        // Account for everything we are not writing (over-length cap + overflow
        // drop) so the loss is observable.  Owned solely by the producer.
        let dropped = data.len() - to_write;
        if dropped > 0 {
            self.dropped_bytes.fetch_add(dropped, Ordering::Relaxed);
        }

        if to_write == 0 {
            return 0;
        }

        let dst = self.buffer.as_ptr() as *mut u8; // 生ポインタ（&mut を作らない）
        let first_chunk = to_write.min(RING_BUFFER_SIZE - write);

        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), dst.add(write), first_chunk);
            if first_chunk < to_write {
                let second = to_write - first_chunk;
                ptr::copy_nonoverlapping(data.as_ptr().add(first_chunk), dst, second);
            }
        }

        let new_write = (write + to_write) % RING_BUFFER_SIZE;
        self.write_pos.store(new_write, Ordering::Release);

        // Notify any thread blocked in wait_data().
        // We briefly acquire the mutex before notify_all() to avoid the
        // lost-wakeup race: the waiter holds the mutex between its condition
        // check and calling wait(), so our notify must happen while the mutex
        // is acquirable (i.e. after the waiter has entered wait()).
        {
            let _guard = self.data_mutex.lock().unwrap_or_else(|e| e.into_inner());
            self.data_available.notify_all();
        }

        to_write
    }

    /// Block until at least one TS packet is available or the timeout expires.
    ///
    /// This replaces the 2 ms sleep-poll loop in WaitTsStream, mirroring
    /// `WaitForMultipleObjects` on the Win32 event used by BonDriverProxy(Ex).
    ///
    /// Returns `true` if data is available, `false` on timeout.
    pub fn wait_data(&self, timeout: Duration) -> bool {
        // Fast path: data already waiting.
        if self.available() >= TS_PACKET_SIZE {
            return true;
        }

        let deadline = std::time::Instant::now() + timeout;
        let mut guard = self.data_mutex.lock().unwrap_or_else(|e| e.into_inner());

        loop {
            if self.available() >= TS_PACKET_SIZE {
                return true;
            }

            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }

            let remaining = deadline - now;
            let result = self
                .data_available
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner());
            guard = result.0;
            if result.1.timed_out() {
                return false;
            }
        }
    }

    /// Consumer-side drop-oldest. When the buffer is full because the consumer
    /// fell behind (so the producer is dropping the newest data in `write`),
    /// skip stale bytes so reading resumes near-live instead of lagging by the
    /// whole 100 MB buffer depth. Only the consumer calls this and only it ever
    /// moves `read_pos`, so it is race-free (unlike a producer-driven
    /// drop-oldest). Skipped bytes are added to `dropped_bytes()`.
    ///
    /// Called at the head of every read path; it is a cheap no-op (one
    /// comparison) unless the buffer is actually overflowing.
    fn resync_if_overflowing(&self) {
        // Not full enough for the producer to be dropping → nothing to do.
        if self.free_space() >= TS_PACKET_SIZE {
            return;
        }
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);
        let available = if write >= read {
            write - read
        } else {
            RING_BUFFER_SIZE - read + write
        };
        if available <= RESYNC_KEEP_BYTES {
            return;
        }
        let skip = available - RESYNC_KEEP_BYTES;
        let new_read = (read + skip) % RING_BUFFER_SIZE;
        self.read_pos.store(new_read, Ordering::Release);
        self.dropped_bytes.fetch_add(skip, Ordering::Relaxed);
    }

    /// Read data from the buffer.
    ///
    /// Returns a slice of the data read and the number of remaining bytes.
    /// The returned slice is valid until the next call to `consume`.
    pub fn read(&self, max_len: usize) -> (&[u8], usize) {
        self.resync_if_overflowing();
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);

        let available = if write >= read {
            write - read
        } else {
            RING_BUFFER_SIZE - read
        };

        let to_read = max_len.min(available);
        let remaining = self.available().saturating_sub(to_read);

        if to_read == 0 {
            return (&[], available);
        }

        let slice = &self.buffer[read..read + to_read];
        (slice, remaining)
    }

    /// Read data into a provided buffer.
    ///
    /// Returns the number of bytes read and the remaining count.
    pub fn read_into(&self, dest: &mut [u8]) -> (usize, usize) {
        self.resync_if_overflowing();
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);

        let available = if write >= read {
            write - read
        } else {
            RING_BUFFER_SIZE - read + write
        };

        let to_read = dest.len().min(available);

        if to_read == 0 {
            return (0, available); // ← ここ重要
        }


        // Copy data, handling wrap-around
        let first_chunk = to_read.min(RING_BUFFER_SIZE - read);
        dest[..first_chunk].copy_from_slice(&self.buffer[read..read + first_chunk]);

        if first_chunk < to_read {
            let second_chunk = to_read - first_chunk;
            dest[first_chunk..to_read].copy_from_slice(&self.buffer[..second_chunk]);
        }

        let remaining = available - to_read;
        (to_read, remaining)
    }

    /// Consume bytes from the read position.
    pub fn consume(&self, count: usize) {
        let read = self.read_pos.load(Ordering::Acquire);
        let new_read = (read + count) % RING_BUFFER_SIZE;
        self.read_pos.store(new_read, Ordering::Release);
    }

    /// Clear the buffer. Must be called only when the producer and consumer
    /// are quiesced (e.g. on channel change / stream restart), since it moves
    /// both cursors. Also resets `dropped_bytes` so it reflects loss for the
    /// new session rather than cumulative lifetime loss.
    pub fn clear(&self) {
        self.read_pos.store(0, Ordering::Release);
        self.write_pos.store(0, Ordering::Release);
        self.dropped_bytes.store(0, Ordering::Relaxed);
    }

    /// Check if the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.available() == 0
    }
}

impl Default for TsRingBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// Safety: The buffer uses atomic operations for write/read positions.
// The Condvar/Mutex fields are already Send+Sync; the raw pointer access
// in write() is guarded by single-producer invariant documented above.
unsafe impl Send for TsRingBuffer {}
unsafe impl Sync for TsRingBuffer {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_write_read() {
        let buffer = TsRingBuffer::new();

        let data = vec![0x47u8; 188 * 10]; // 10 TS packets
        let written = buffer.write(&data);
        assert_eq!(written, data.len());
        assert_eq!(buffer.available(), data.len());

        let (read_data, remaining) = buffer.read(1000);
        assert_eq!(read_data.len(), 1000);
        assert_eq!(remaining, data.len() - 1000);
    }

    #[test]
    fn test_wrap_around() {
        let buffer = TsRingBuffer::new();

        // Fill most of the buffer in chunks to avoid stack issues
        let chunk_size = 64 * 1024; // 64KB chunks
        let total_to_write = RING_BUFFER_SIZE - 100;
        let chunk = vec![0xFFu8; chunk_size];

        let mut written_total = 0;
        while written_total < total_to_write {
            let to_write = (total_to_write - written_total).min(chunk_size);
            buffer.write(&chunk[..to_write]);
            written_total += to_write;
        }

        buffer.consume(RING_BUFFER_SIZE - 200);

        // Write data that wraps around
        let wrap_data = vec![0x47u8; 300];
        let written = buffer.write(&wrap_data);
        assert!(written > 0);
    }

    /// Build a distinct 188-byte TS packet whose whole content encodes `seq`,
    /// so any corruption or reordering after a drain is detectable.
    fn make_packet(seq: u32) -> Vec<u8> {
        let mut p = vec![(seq & 0xFF) as u8; TS_PACKET_SIZE];
        p[0] = 0x47; // TS sync byte
        p[1..5].copy_from_slice(&seq.to_le_bytes());
        p
    }

    #[test]
    fn test_full_buffer_producer_drops_newest_without_touching_read_pos() {
        // Producer-path invariant: when the buffer is full the producer drops
        // the newest data whole and never moves the consumer's read_pos. (The
        // consumer-side resync that reclaims latency is tested separately and
        // only fires on a read, which this test avoids until the very end.)
        let buffer = TsRingBuffer::new();

        let mut accepted: Vec<u32> = Vec::new();
        let mut seq: u32 = 0;
        while buffer.free_space() >= TS_PACKET_SIZE {
            let pkt = make_packet(seq);
            let written = buffer.write(&pkt);
            assert_eq!(written, TS_PACKET_SIZE, "packet {seq} should fit");
            accepted.push(seq);
            seq += 1;
        }

        assert_eq!(buffer.dropped_bytes(), 0, "nothing dropped while filling");
        let accepted_bytes = accepted.len() * TS_PACKET_SIZE;
        assert_eq!(buffer.available(), accepted_bytes);

        // Push more packets into the full buffer: dropped whole and counted,
        // read_pos untouched.
        let read_before = buffer.read_pos.load(Ordering::Acquire);
        let mut extra = 0usize;
        for _ in 0..10 {
            let pkt = make_packet(seq);
            let written = buffer.write(&pkt);
            assert_eq!(written, 0, "no room: packet {seq} must be fully dropped");
            extra += pkt.len();
            seq += 1;
        }

        assert_eq!(
            buffer.read_pos.load(Ordering::Acquire),
            read_before,
            "producer must not move the consumer's read_pos on overflow"
        );
        assert_eq!(buffer.dropped_bytes(), extra, "dropped bytes must be counted");
        assert_eq!(buffer.available(), accepted_bytes);

        // Drain via read(), which triggers the consumer-side resync (drop
        // oldest). What survives must be a contiguous, in-order, uncorrupted
        // *suffix* of the accepted packets (the freshest ones), never garbage.
        let mut survivors: Vec<u32> = Vec::new();
        loop {
            let (slice, _rem) = buffer.read(TS_PACKET_SIZE);
            if slice.is_empty() {
                break;
            }
            assert_eq!(slice.len(), TS_PACKET_SIZE);
            assert_eq!(slice[0], 0x47, "sync byte intact");
            let s = u32::from_le_bytes(slice[1..5].try_into().unwrap());
            assert_eq!(slice, make_packet(s).as_slice(), "packet {s} corrupted");
            survivors.push(s);
            buffer.consume(TS_PACKET_SIZE);
        }
        assert!(buffer.is_empty());
        // Survivors are the tail of `accepted`, strictly increasing, in order.
        assert!(!survivors.is_empty());
        assert!(survivors.windows(2).all(|w| w[1] == w[0] + 1), "in-order, contiguous");
        assert_eq!(*survivors.last().unwrap(), *accepted.last().unwrap(), "keeps freshest");
        let start = survivors[0];
        assert_eq!(&survivors[..], &accepted[(start as usize)..], "suffix of accepted");
    }

    #[test]
    fn test_partial_accept_on_overflow_is_packet_aligned() {
        // Drive the producer into the branch where 188 <= free < data.len():
        // some bytes are accepted (rounded down to a packet multiple) and the
        // remainder is dropped and counted.
        let buffer = TsRingBuffer::new();
        while buffer.free_space() >= 4 * TS_PACKET_SIZE {
            assert_eq!(buffer.write(&vec![0x47u8; TS_PACKET_SIZE]), TS_PACKET_SIZE);
        }
        // Now free space is 1..=3 packets (plus the -1 full sentinel). Push a
        // 5-packet chunk: only whole packets that fit are written.
        let free = buffer.free_space();
        let accept_expected = (free / TS_PACKET_SIZE) * TS_PACKET_SIZE;
        let chunk = vec![0x47u8; 5 * TS_PACKET_SIZE];
        let written = buffer.write(&chunk);
        assert_eq!(written, accept_expected, "accepted amount must be packet-aligned and fit");
        assert_eq!(written % TS_PACKET_SIZE, 0, "never write a partial packet");
        assert_eq!(buffer.dropped_bytes(), chunk.len() - written);
    }

    #[test]
    fn test_consumer_resyncs_to_near_live_when_overflowing() {
        // Simulate a stalled consumer: fill the buffer completely so the
        // producer is dropping newest data. The next read must resync
        // (drop-oldest) so latency collapses to ~RESYNC_KEEP_BYTES instead of
        // the full buffer, and reading still works.
        let buffer = TsRingBuffer::new();
        while buffer.free_space() >= TS_PACKET_SIZE {
            buffer.write(&vec![0x47u8; TS_PACKET_SIZE]);
        }
        assert!(buffer.free_space() < TS_PACKET_SIZE, "buffer should be full");
        let before = buffer.available();

        // A read triggers the consumer-side resync.
        let mut dest = vec![0u8; TS_PACKET_SIZE];
        let (n, _remaining) = buffer.read_into(&mut dest);
        assert_eq!(n, TS_PACKET_SIZE, "read still returns data after resync");

        // Latency (available backlog) dropped to about the keep window, and the
        // skipped bytes were counted as dropped.
        assert!(
            buffer.available() <= RESYNC_KEEP_BYTES,
            "backlog {} should be trimmed to <= {}",
            buffer.available(),
            RESYNC_KEEP_BYTES
        );
        assert!(buffer.dropped_bytes() >= before - RESYNC_KEEP_BYTES - TS_PACKET_SIZE);
        // Buffer is healthy again: the producer can write fresh data.
        assert!(buffer.free_space() >= TS_PACKET_SIZE);
    }

    #[test]
    fn test_clear() {
        let buffer = TsRingBuffer::new();

        buffer.write(&[1, 2, 3, 4, 5]);
        assert!(!buffer.is_empty());

        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.dropped_bytes(), 0, "clear resets the loss counter");
    }
}
