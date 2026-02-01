//! Lock-free ring buffer for TS data.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::ptr;

/// TS packet size.
pub const TS_PACKET_SIZE: usize = 188;

/// Size of the ring buffer (100 MB).
pub const RING_BUFFER_SIZE: usize = TS_PACKET_SIZE * 1024 * 100;

/// A lock-free ring buffer for TS data.
///
/// This buffer is designed for a single-producer, single-consumer scenario
/// where the network receiver writes data and the BonDriver GetTsStream reads it.
pub struct TsRingBuffer {
    /// The underlying buffer (heap-allocated).
    buffer: Box<[u8]>,
    /// Write position (updated by receiver).
    write_pos: AtomicUsize,
    /// Read position (updated by GetTsStream).
    read_pos: AtomicUsize,
}

impl TsRingBuffer {
    /// Create a new ring buffer.
    pub fn new() -> Self {
        // Allocate directly on heap to avoid stack overflow
        let buffer = vec![0u8; RING_BUFFER_SIZE].into_boxed_slice();
        Self {
            buffer,
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
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

    /// Write data to the buffer.
    ///
    /// Returns the number of bytes written (may be less than data.len() if buffer is full).
    pub fn write(&self, data: &[u8]) -> usize {
        let mut write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);

        let free = if write >= read {
            RING_BUFFER_SIZE - write + read - 1
        } else {
            read - write - 1
        };

        let to_write = data.len().min(free);
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

        write = (write + to_write) % RING_BUFFER_SIZE;
        self.write_pos.store(write, Ordering::Release);
        to_write
    }

    /// Read data from the buffer.
    ///
    /// Returns a slice of the data read and the number of remaining bytes.
    /// The returned slice is valid until the next call to `consume`.
    pub fn read(&self, max_len: usize) -> (&[u8], usize) {
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

    /// Clear the buffer.
    pub fn clear(&self) {
        self.read_pos.store(0, Ordering::Release);
        self.write_pos.store(0, Ordering::Release);
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

// Safety: The buffer uses atomic operations for synchronization
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

    #[test]
    fn test_clear() {
        let buffer = TsRingBuffer::new();

        buffer.write(&[1, 2, 3, 4, 5]);
        assert!(!buffer.is_empty());

        buffer.clear();
        assert!(buffer.is_empty());
    }
}
