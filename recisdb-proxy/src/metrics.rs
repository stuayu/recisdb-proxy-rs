//! Performance metrics collection and reporting.
//!
//! This module provides session-level and system-level metrics tracking:
//! - TS data reception rates (bytes/sec)
//! - Error occurrence and recovery
//! - Tuner switching and allocation statistics
//! - Signal quality metrics

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::collections::VecDeque;

use log::info;

/// Session metrics for a single client connection.
pub struct SessionMetrics {
    /// Session start time.
    start_time: Instant,
    /// TS bytes received.
    ts_bytes_received: AtomicU64,
    /// TS messages sent.
    ts_messages_sent: AtomicU64,
    /// Number of tuner switches (channel changes).
    tuner_switches: AtomicU64,
    /// Number of errors encountered.
    error_count: AtomicU64,
    /// Last TS data timestamp.
    last_ts_update: std::sync::Mutex<Instant>,
    /// Average signal level (last 10 measurements).
    signal_level_samples: std::sync::Mutex<Vec<f32>>,
}

/// Stream quality samples (last 60 seconds).
pub struct StreamQualityWindow {
    pub bitrate_samples: VecDeque<(Instant, f64)>,
    pub packet_loss_samples: VecDeque<(Instant, f64)>,
}

impl StreamQualityWindow {
    pub fn new() -> Self {
        Self {
            bitrate_samples: VecDeque::new(),
            packet_loss_samples: VecDeque::new(),
        }
    }

    pub fn push_sample(&mut self, bitrate_mbps: f64, packet_loss_rate: f64) {
        let now = Instant::now();
        self.bitrate_samples.push_back((now, bitrate_mbps));
        self.packet_loss_samples.push_back((now, packet_loss_rate));
        self.trim();
    }

    fn trim(&mut self) {
        let cutoff = Instant::now() - Duration::from_secs(60);
        while self.bitrate_samples.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            self.bitrate_samples.pop_front();
        }
        while self.packet_loss_samples.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
            self.packet_loss_samples.pop_front();
        }
    }
}

impl SessionMetrics {
    /// Create a new session metrics instance.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            start_time: Instant::now(),
            ts_bytes_received: AtomicU64::new(0),
            ts_messages_sent: AtomicU64::new(0),
            tuner_switches: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            last_ts_update: std::sync::Mutex::new(Instant::now()),
            signal_level_samples: std::sync::Mutex::new(Vec::with_capacity(10)),
        })
    }

    /// Record TS data reception.
    pub fn record_ts_data(&self, bytes: u64) {
        self.ts_bytes_received.fetch_add(bytes, Ordering::Relaxed);
        let _ = self.last_ts_update.lock().map(|mut t| *t = Instant::now());
    }

    /// Record a TS message sent.
    pub fn record_ts_message(&self) {
        self.ts_messages_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a tuner switch.
    pub fn record_tuner_switch(&self) {
        self.tuner_switches.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an error occurrence.
    pub fn record_error(&self) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Update signal level sample (keeps last 10 samples).
    pub fn add_signal_sample(&self, level: f32) {
        if let Ok(mut samples) = self.signal_level_samples.lock() {
            if samples.len() >= 10 {
                samples.remove(0);
            }
            samples.push(level);
        }
    }

    /// Get average signal level from last 10 samples.
    pub fn average_signal_level(&self) -> f32 {
        if let Ok(samples) = self.signal_level_samples.lock() {
            if samples.is_empty() {
                return 0.0;
            }
            samples.iter().sum::<f32>() / samples.len() as f32
        } else {
            0.0
        }
    }

    /// Get TS data reception rate (bytes/second).
    pub fn ts_rate_bytes_per_sec(&self) -> f64 {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        if elapsed < 0.01 {
            return 0.0;
        }
        self.ts_bytes_received.load(Ordering::Relaxed) as f64 / elapsed
    }

    /// Get session duration.
    pub fn session_duration(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Get total TS bytes received.
    pub fn total_bytes_received(&self) -> u64 {
        self.ts_bytes_received.load(Ordering::Relaxed)
    }

    /// Get total TS messages sent.
    pub fn total_messages_sent(&self) -> u64 {
        self.ts_messages_sent.load(Ordering::Relaxed)
    }

    /// Get total error count.
    pub fn error_count(&self) -> u64 {
        self.error_count.load(Ordering::Relaxed)
    }

    /// Get total tuner switches.
    pub fn tuner_switches(&self) -> u64 {
        self.tuner_switches.load(Ordering::Relaxed)
    }

    /// Get error rate (errors per minute).
    pub fn error_rate_per_minute(&self) -> f64 {
        let elapsed = self.start_time.elapsed().as_secs_f64();
        if elapsed < 1.0 {
            return 0.0;
        }
        self.error_count.load(Ordering::Relaxed) as f64 / (elapsed / 60.0)
    }

    /// Print a human-readable metrics report.
    pub fn print_report(&self, session_id: u64) {
        let duration = self.session_duration();
        let total_bytes = self.total_bytes_received();
        let rate = self.ts_rate_bytes_per_sec();
        let avg_signal = self.average_signal_level();
        let errors = self.error_count();
        let switches = self.tuner_switches();
        let messages = self.total_messages_sent();

        info!(
            "[Session {}] Metrics Report: duration={:.1}s, bytes={}, rate={:.2} MB/s, \
             messages={}, signal={:.1}dB, errors={}, switches={}",
            session_id,
            duration.as_secs_f64(),
            total_bytes,
            rate / 1_000_000.0,
            messages,
            avg_signal,
            errors,
            switches
        );
    }
}

impl Default for SessionMetrics {
    fn default() -> Self {
        // Note: This returns a non-Arc instance, for use in scenarios where Arc is not needed.
        // For the typical use case, use SessionMetrics::new() directly.
        SessionMetrics {
            start_time: Instant::now(),
            ts_bytes_received: AtomicU64::new(0),
            ts_messages_sent: AtomicU64::new(0),
            tuner_switches: AtomicU64::new(0),
            error_count: AtomicU64::new(0),
            last_ts_update: std::sync::Mutex::new(Instant::now()),
            signal_level_samples: std::sync::Mutex::new(Vec::with_capacity(10)),
        }
    }
}

/// System-level metrics aggregator.
pub struct SystemMetrics {
    /// Total sessions created.
    total_sessions: AtomicU64,
    /// Currently active sessions.
    active_sessions: AtomicU64,
    /// Total errors across all sessions.
    total_errors: AtomicU64,
    /// Total TS bytes transferred.
    total_bytes_transferred: AtomicU64,
}

impl SystemMetrics {
    /// Create a new system metrics instance.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            total_sessions: AtomicU64::new(0),
            active_sessions: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            total_bytes_transferred: AtomicU64::new(0),
        })
    }

    /// Increment active session count.
    pub fn session_started(&self) {
        self.total_sessions.fetch_add(1, Ordering::Relaxed);
        self.active_sessions.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement active session count.
    pub fn session_ended(&self) {
        self.active_sessions.fetch_sub(1, Ordering::Relaxed);
    }

    /// Record total bytes transferred.
    pub fn add_bytes_transferred(&self, bytes: u64) {
        self.total_bytes_transferred
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record an error occurrence.
    pub fn record_error(&self) {
        self.total_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Get active session count.
    pub fn active_session_count(&self) -> u64 {
        self.active_sessions.load(Ordering::Relaxed)
    }

    /// Get total sessions created.
    pub fn total_sessions(&self) -> u64 {
        self.total_sessions.load(Ordering::Relaxed)
    }

    /// Get total errors.
    pub fn total_errors(&self) -> u64 {
        self.total_errors.load(Ordering::Relaxed)
    }

    /// Get total bytes transferred.
    pub fn total_bytes_transferred(&self) -> u64 {
        self.total_bytes_transferred.load(Ordering::Relaxed)
    }

    /// Print a system metrics report.
    pub fn print_report(&self) {
        info!(
            "[System] Metrics: sessions={} (active={}), \
             total_bytes={}, total_errors={}",
            self.total_sessions(),
            self.active_session_count(),
            self.total_bytes_transferred(),
            self.total_errors()
        );
    }
}

impl Default for SystemMetrics {
    fn default() -> Self {
        // Note: This returns a non-Arc instance, for use in scenarios where Arc is not needed.
        // For the typical use case, use SystemMetrics::new() directly.
        SystemMetrics {
            total_sessions: AtomicU64::new(0),
            active_sessions: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            total_bytes_transferred: AtomicU64::new(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_metrics() {
        let metrics = SessionMetrics::new();
        metrics.record_ts_data(1000);
        metrics.record_ts_message();
        metrics.record_tuner_switch();

        assert_eq!(metrics.total_bytes_received(), 1000);
        assert_eq!(metrics.total_messages_sent(), 1);
        assert_eq!(metrics.tuner_switches(), 1);

        std::thread::sleep(Duration::from_millis(10));
        let rate = metrics.ts_rate_bytes_per_sec();
        assert!(rate > 0.0);
    }

    #[test]
    fn test_signal_level_samples() {
        let metrics = SessionMetrics::new();
        metrics.add_signal_sample(10.0);
        metrics.add_signal_sample(12.0);
        metrics.add_signal_sample(8.0);

        let avg = metrics.average_signal_level();
        assert!((avg - 10.0).abs() < 0.1);
    }

    #[test]
    fn test_system_metrics() {
        let metrics = SystemMetrics::new();
        metrics.session_started();
        metrics.session_started();
        metrics.session_ended();

        assert_eq!(metrics.total_sessions(), 2);
        assert_eq!(metrics.active_session_count(), 1);

        metrics.add_bytes_transferred(5000);
        assert_eq!(metrics.total_bytes_transferred(), 5000);
    }
}

