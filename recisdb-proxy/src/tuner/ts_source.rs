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
    /// Optional native read diagnostics (calls, bytes, max chunk, pending peak).
    fn get_ts_stream_stats(&self) -> Option<(u64, u64, u64, u64)> {
        None
    }
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

    fn get_ts_stream_stats(&self) -> Option<(u64, u64, u64, u64)> {
        self.get_ts_stream_stats()
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
    /// If set, `get_ts_stream` blocks (polling every 5 ms) until the gate is
    /// released — simulates a hung DLL call that never respects the reader
    /// loop's stop-flag poll cadence, so tests can force `stop_reader()`'s
    /// own join timeout to actually fire (docs/TUNER_PIPELINE_REDESIGN.md
    /// P2a item 5).
    get_ts_stream_gate: Option<BlockingGate>,
    get_ts_stream_error: Option<io::ErrorKind>,
    wait_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// Two-way rendezvous a test uses to know a fake `get_ts_stream` call has
/// actually started blocking before the test acts on that assumption (e.g.
/// calling `stop_reader()` and asserting it times out).
///
/// A test-controlled gate rather than a fixed sleep duration on either side:
/// without `wait_until_entered`, a test that simply "sleeps a bit, then
/// calls `stop_reader()`" races the reader loop's own setup work (buffer
/// allocation, collector construction, an `Initial signal level` log call)
/// between transitioning to `Running` and reaching its first `get_ts_stream`
/// call — if `stop_reader()` sets `Stopping` before the loop's first
/// stop-flag check, the reader exits immediately without ever calling
/// `get_ts_stream` at all, which is *correct* fast-stop behavior but defeats
/// a test that specifically wants to exercise "the reader is stuck inside a
/// blocking call and ignores the stop flag". This was caught as a genuinely
/// flaky (not just occasionally-slow) test during review — occurring on
/// roughly 1 in 4-5 runs even single-threaded.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct BlockingGate {
    entered: std::sync::Arc<std::sync::atomic::AtomicBool>,
    release: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(test)]
impl BlockingGate {
    pub(crate) fn new() -> Self {
        Self {
            entered: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            release: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Block (polling every 5 ms) until [`Self::release`] is called. Called
    /// from the fake reader's blocking thread; marks `entered` on the way
    /// in so a test's `wait_until_entered` can observe it.
    fn block_until_released(&self) {
        self.entered
            .store(true, std::sync::atomic::Ordering::SeqCst);
        while !self.release.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// Await (polling every 5 ms via `tokio::time::sleep`, so this does not
    /// block the calling task's runtime thread) until the fake reader has
    /// actually entered [`Self::block_until_released`]. Call this before
    /// assuming the reader is stuck and acting on that (e.g. calling
    /// `stop_reader()`).
    pub(crate) async fn wait_until_entered(&self) {
        while !self.entered.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// Let a blocked `get_ts_stream` call return.
    pub(crate) fn release(&self) {
        self.release
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
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
            get_ts_stream_gate: None,
            get_ts_stream_error: None,
            wait_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Make `get_ts_stream` block on `gate` until released, simulating a DLL
    /// call that never returns in time for `stop_reader()`'s own timeout —
    /// so the "existing reader must actually stop before a restart
    /// proceeds" path (docs/TUNER_PIPELINE_REDESIGN.md §2.1-3) can be
    /// exercised deterministically. See [`BlockingGate`]'s doc comment for
    /// why a test must call `gate.wait_until_entered()` before assuming the
    /// reader is actually stuck.
    pub(crate) fn with_get_ts_stream_gate(mut self, gate: BlockingGate) -> Self {
        self.get_ts_stream_gate = Some(gate);
        self
    }

    pub(crate) fn with_get_ts_stream_error(mut self, kind: io::ErrorKind) -> Self {
        self.get_ts_stream_error = Some(kind);
        self
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

    pub(crate) fn wait_call_counter(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        self.wait_calls.clone()
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
            Some(kind) => Err(io::Error::new(
                kind,
                "FakeTsSource: configured set_channel error",
            )),
            None => Ok(()),
        }
    }

    fn purge_ts_stream(&self) {}

    fn wait_ts_stream(&self, timeout_ms: u32) -> bool {
        self.wait_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Short, bounded sleep so the reader loop's poll cadence stays fast
        // in tests regardless of the real `timeout_ms` value passed in.
        std::thread::sleep(std::time::Duration::from_millis(timeout_ms.min(5) as u64));
        !self.chunks.lock().unwrap().is_empty()
    }

    fn get_ts_stream(&self, buf: &mut [u8]) -> io::Result<(usize, usize)> {
        if let Some(kind) = self.get_ts_stream_error {
            return Err(io::Error::new(kind, "FakeTsSource: configured read error"));
        }
        if let Some(gate) = &self.get_ts_stream_gate {
            gate.block_until_released();
        }
        let mut chunks = self.chunks.lock().unwrap();
        match chunks.front_mut() {
            Some(chunk) => {
                let n = chunk.len().min(buf.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                chunk.drain(..n);
                if chunk.is_empty() {
                    chunks.pop_front();
                }
                let remaining = chunks.iter().map(Vec::len).sum();
                Ok((n, remaining))
            }
            None => Ok((0, 0)),
        }
    }

    fn get_signal_level(&self) -> f32 {
        self.signal_level
    }
}
