//! Abstraction over "a thing that behaves like an opened `BonDriverTuner`"
//! (docs/TUNER_PIPELINE_REDESIGN.md §4 P1, item 3: "reader のテスト可能化").
//!
//! `SharedTuner::run_bondriver_reader_with_tuner` used to be hard-wired to
//! `crate::bondriver::BonDriverTuner`, so `ReaderState` could only ever be
//! driven by a real BonDriver DLL — meaning the pool's competitive/racy
//! scenarios (an evict racing a still-starting reader, `stop_reader()`
//! racing a fresh `subscribe()`, ...) were untestable in this environment
//! (see `tuner/pool.rs`'s pre-P1 test comments). Making the reader generic
//! over this trait lets `#[cfg(test)]` code (see [`FakeTsSource`] below)
//! drive the exact same state machine deterministically.
//!
//! The method set mirrors `BonDriverTuner`'s existing inherent methods
//! exactly (see `bondriver/unix.rs` / `bondriver/windows.rs` / `bondriver/mod.rs`'s
//! non-Windows/Unix stub) — this trait adds a vtable indirection with no
//! behavior change for the real implementation.

use std::io;

/// A TS source that can be tuned, drained, and read from a background
/// (`spawn_blocking`) thread.
///
/// `BonDriverTuner` is not `Send` (it wraps a DLL-provided vtable/COM-ish
/// pointer), so `SharedTuner::run_bondriver_reader_with_tuner` — generic over
/// this trait — must keep running entirely on the single `spawn_blocking`
/// thread it was constructed on, exactly as before. This trait itself does
/// not require `Send`; only whatever construction closure hands a `T` to
/// `spawn_blocking` needs `Send`, same requirement `BonDriverTuner::new(..)`
/// already satisfies today by being constructed *inside* the blocking
/// closure rather than moved in from outside.
pub(crate) trait TsSource {
    /// Tune to `(space, channel)`. Mirrors `IBonDriver2::SetChannel`.
    fn set_channel(&self, space: u32, channel: u32) -> io::Result<()>;
    /// Discard any buffered TS data. Mirrors `IBonDriver::PurgeTsStream`.
    fn purge_ts_stream(&self);
    /// Block up to `timeout_ms` for TS data to become available. Mirrors
    /// `IBonDriver::WaitTsStream`.
    fn wait_ts_stream(&self, timeout_ms: u32) -> bool;
    /// Read available TS data into `buf`. Returns `(bytes_written,
    /// bytes_remaining)`. Mirrors `IBonDriver2::GetTsStream2`.
    fn get_ts_stream(&self, buf: &mut [u8]) -> io::Result<(usize, usize)>;
    /// Current signal level in dB. Mirrors `IBonDriver::GetSignalLevel`.
    fn get_signal_level(&self) -> f32;
}

impl TsSource for crate::bondriver::BonDriverTuner {
    fn set_channel(&self, space: u32, channel: u32) -> io::Result<()> {
        // Inherent method of the same name/signature takes priority over the
        // trait method in method resolution, so this delegates rather than
        // recursing.
        self.set_channel(space, channel)
    }

    fn purge_ts_stream(&self) {
        self.purge_ts_stream()
    }

    fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
        self.wait_ts_stream(timeout_ms)
    }

    fn get_ts_stream(&self, buf: &mut [u8]) -> io::Result<(usize, usize)> {
        self.get_ts_stream(buf)
    }

    fn get_signal_level(&self) -> f32 {
        self.get_signal_level()
    }
}

/// Deterministic, in-process [`TsSource`] fake for tests.
///
/// All configuration is fixed at construction time via the `with_*` builder
/// methods; interior mutability (`Mutex`) is used only for the parts that
/// genuinely change while the reader loop runs (the chunk queue, and a
/// one-shot signal so a test can observe "the fake actually reached
/// `set_channel`").
#[cfg(test)]
pub(crate) struct FakeTsSource {
    /// Sleep injected inside `set_channel` before it returns, to widen the
    /// `ReaderState::Starting` window for races that need to observe it.
    startup_delay: std::time::Duration,
    /// If set, `set_channel` fails with this error kind instead of
    /// succeeding.
    set_channel_error: Option<io::ErrorKind>,
    /// Chunks handed out one-per-`get_ts_stream` call, in order. Once
    /// drained, further reads report zero bytes (matching a real driver with
    /// no more data buffered).
    chunks: std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>,
    signal_level: f32,
    /// If set, `set_channel` panics instead of returning — exercises the
    /// same `catch_unwind` path a corrupted BonDriver DLL would hit.
    panic_on_set_channel: bool,
}

#[cfg(test)]
impl FakeTsSource {
    pub(crate) fn new() -> Self {
        Self {
            startup_delay: std::time::Duration::ZERO,
            set_channel_error: None,
            chunks: std::sync::Mutex::new(std::collections::VecDeque::new()),
            signal_level: 0.0,
            panic_on_set_channel: false,
        }
    }

    /// Make `set_channel` panic instead of returning.
    pub(crate) fn with_panic_on_set_channel(mut self) -> Self {
        self.panic_on_set_channel = true;
        self
    }

    /// Make `set_channel` block for `delay` before returning — widens the
    /// window during which the reader sits in `ReaderState::Starting`.
    pub(crate) fn with_startup_delay(mut self, delay: std::time::Duration) -> Self {
        self.startup_delay = delay;
        self
    }

    /// Make `set_channel` fail with `kind` (e.g. `AddrNotAvailable` to
    /// exercise the same retry-then-give-up path a real tuner-unavailable
    /// error takes).
    pub(crate) fn with_set_channel_error(mut self, kind: io::ErrorKind) -> Self {
        self.set_channel_error = Some(kind);
        self
    }

    /// Queue one chunk of TS data to be returned by a future
    /// `get_ts_stream` call.
    pub(crate) fn with_chunk(self, data: Vec<u8>) -> Self {
        self.chunks.lock().unwrap().push_back(data);
        self
    }
}

#[cfg(test)]
impl TsSource for FakeTsSource {
    fn set_channel(&self, _space: u32, _channel: u32) -> io::Result<()> {
        if !self.startup_delay.is_zero() {
            std::thread::sleep(self.startup_delay);
        }
        if self.panic_on_set_channel {
            panic!("FakeTsSource: configured to panic on set_channel");
        }
        match self.set_channel_error {
            Some(kind) => Err(io::Error::new(kind, "FakeTsSource: configured set_channel error")),
            None => Ok(()),
        }
    }

    fn purge_ts_stream(&self) {}

    fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
        // Short, bounded sleep so the reader loop's poll cadence stays fast
        // in tests regardless of the real `timeout_ms` value passed in.
        std::thread::sleep(std::time::Duration::from_millis(timeout_ms.min(5) as u64));
        !self.chunks.lock().unwrap().is_empty()
    }

    fn get_ts_stream(&self, buf: &mut [u8]) -> io::Result<(usize, usize)> {
        let mut chunks = self.chunks.lock().unwrap();
        match chunks.pop_front() {
            Some(chunk) => {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                Ok((n, chunks.len()))
            }
            None => Ok((0, 0)),
        }
    }

    fn get_signal_level(&self) -> f32 {
        self.signal_level
    }
}
