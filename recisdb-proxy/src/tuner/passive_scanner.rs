//! Passive scanner for updating channel information during streaming.
//!
//! This module provides real-time channel information updates during
//! TS streaming. When a tuner is actively streaming, the passive scanner
//! monitors TS packets to extract and update channel metadata in the database.
//!
//! # How It Works
//!
//! 1. When streaming starts, the passive scanner is attached to the tuner
//! 2. It monitors TS packets for PAT/SDT/NIT tables
//! 3. When channel information changes, it updates the database
//! 4. This allows automatic discovery of new channels or metadata updates

use bytes::Bytes;
use log::{debug, trace};
use tokio::sync::broadcast;

use recisdb_protocol::ChannelInfo;

use crate::server::listener::DatabaseHandle;
use crate::tuner::ts_parser::MinimalTsParser;

/// Configuration for passive scanning.
#[derive(Debug, Clone)]
pub struct PassiveScanConfig {
    /// Whether passive scanning is enabled.
    pub enabled: bool,
    /// Interval between scan updates (to avoid too frequent DB writes).
    pub update_interval_secs: u64,
}

impl Default for PassiveScanConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            update_interval_secs: 60,
        }
    }
}

/// Passive scanner instance for a single tuner.
pub struct PassiveScanner {
    /// Database handle.
    database: DatabaseHandle,
    /// BonDriver ID in database.
    bon_driver_id: i64,
    /// Current space number.
    space: u32,
    /// Current channel number.
    channel: u32,
    /// Configuration.
    config: PassiveScanConfig,
    /// Last update timestamp.
    last_update: std::time::Instant,
    /// Accumulated channel info (will be filled by TS analysis).
    pending_info: Option<ChannelInfo>,
    /// TS parser for extracting channel info.
    ts_parser: MinimalTsParser,
    /// Whether the parser has completed (found required tables).
    parser_complete: bool,
}

impl PassiveScanner {
    /// Create a new passive scanner.
    pub fn new(
        database: DatabaseHandle,
        bon_driver_id: i64,
        space: u32,
        channel: u32,
        config: PassiveScanConfig,
    ) -> Self {
        Self {
            database,
            bon_driver_id,
            space,
            channel,
            config,
            last_update: std::time::Instant::now(),
            pending_info: None,
            ts_parser: MinimalTsParser::new(),
            parser_complete: false,
        }
    }

    /// Process a TS data chunk.
    ///
    /// This method should be called for each TS data chunk received.
    /// It will analyze the TS packets and extract channel information.
    #[allow(dead_code)]
    pub fn process_ts_data(&mut self, data: &Bytes) {
        if !self.config.enabled {
            return;
        }

        // Feed data to the TS parser
        if !self.parser_complete {
            self.parser_complete = self.ts_parser.feed(data);

            if self.parser_complete {
                trace!(
                    "PassiveScanner: TS parsing complete for space={}, channel={}",
                    self.space,
                    self.channel
                );

                // Get parsed channel infos
                let channel_infos = self.ts_parser.to_channel_infos();
                if !channel_infos.is_empty() {
                    // Use the first channel info as pending
                    self.pending_info = Some(channel_infos[0].clone());

                    // Update all channels from the TS
                    self.update_database_batch(&channel_infos);
                }
            }
        }

        // Check if enough time has passed since last update
        if self.last_update.elapsed().as_secs() < self.config.update_interval_secs {
            return;
        }

        // If we have pending channel info, update the database
        if let Some(info) = self.pending_info.take() {
            trace!(
                "PassiveScanner: Updating channel info for space={}, channel={}",
                self.space,
                self.channel
            );
            self.update_database(&info);
        }
    }

    /// Update the database with the extracted channel information.
    fn update_database(&mut self, info: &ChannelInfo) {
        let db = self.database.clone();
        let bon_driver_id = self.bon_driver_id;
        let info = info.clone();

        // Spawn a task to update the database asynchronously
        tokio::spawn(async move {
            let db_guard = db.lock().await;
            match db_guard.passive_update_channels(bon_driver_id, &[info]) {
                Ok(updated) => {
                    if updated > 0 {
                        debug!("PassiveScanner: Updated {} channel(s)", updated);
                    }
                }
                Err(e) => {
                    debug!("PassiveScanner: Failed to update database: {}", e);
                }
            }
        });

        self.last_update = std::time::Instant::now();
    }

    /// Update the database with multiple channel infos at once.
    fn update_database_batch(&mut self, infos: &[ChannelInfo]) {
        if infos.is_empty() {
            return;
        }

        let db = self.database.clone();
        let bon_driver_id = self.bon_driver_id;
        let infos = infos.to_vec();

        // Spawn a task to update the database asynchronously
        tokio::spawn(async move {
            let db_guard = db.lock().await;
            match db_guard.passive_update_channels(bon_driver_id, &infos) {
                Ok(updated) => {
                    if updated > 0 {
                        debug!("PassiveScanner: Batch updated {} channel(s)", updated);
                    }
                }
                Err(e) => {
                    debug!("PassiveScanner: Failed to batch update database: {}", e);
                }
            }
        });

        self.last_update = std::time::Instant::now();
    }

    /// Set the pending channel info (for testing or manual updates).
    #[allow(dead_code)]
    pub fn set_pending_info(&mut self, info: ChannelInfo) {
        self.pending_info = Some(info);
    }

    /// Get the current space number.
    #[allow(dead_code)]
    pub fn space(&self) -> u32 {
        self.space
    }

    /// Get the current channel number.
    #[allow(dead_code)]
    pub fn channel(&self) -> u32 {
        self.channel
    }
}

/// Start passive scanning on a tuner's broadcast receiver.
///
/// This function creates a passive scanner and processes incoming TS data.
#[allow(dead_code)]
pub async fn start_passive_scan(
    mut ts_receiver: broadcast::Receiver<Bytes>,
    database: DatabaseHandle,
    bon_driver_id: i64,
    space: u32,
    channel: u32,
    config: PassiveScanConfig,
) {
    let mut scanner = PassiveScanner::new(database, bon_driver_id, space, channel, config);

    loop {
        match ts_receiver.recv().await {
            Ok(data) => {
                scanner.process_ts_data(&data);
            }
            Err(broadcast::error::RecvError::Closed) => {
                debug!("PassiveScanner: Channel closed, stopping");
                break;
            }
            Err(broadcast::error::RecvError::Lagged(count)) => {
                debug!("PassiveScanner: Lagged {} messages", count);
                // Continue processing
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_passive_scan_config_default() {
        let config = PassiveScanConfig::default();
        assert!(config.enabled);
        assert_eq!(config.update_interval_secs, 60);
    }
}
