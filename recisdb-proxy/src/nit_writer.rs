//! Background task that applies NIT observations from live streams to the
//! `channels` table.
//!
//! `NitCollector` (`tuner/nit_collector.rs`) runs inside the tuner reader
//! thread and has no `Database` handle available to it (see
//! `tuner/epg_collector.rs`'s doc comment for why). It forwards parsed rows
//! through a process-wide mpsc channel, installed here in [`NitWriter::new`]
//! and drained by [`NitWriter::run`].
//!
//! Shaped like `epg_writer::EpgWriter` but without batching: the NIT
//! describes a handful of transport streams per network and each one is
//! forwarded at most once per reader task, so the volume is a few rows per
//! channel switch — nothing to accumulate. What it does keep is a set of
//! network ids already applied, so the repeated reports that arrive whenever
//! a tuner restarts do not turn into repeated UPDATE statements against the
//! shared, mutex-guarded database handle.

use std::collections::HashSet;

use log::{debug, error, info};
use tokio::sync::mpsc;

use crate::server::listener::DatabaseHandle;
use crate::tuner::nit_collector::{self, NitObservation};

/// NIT application task. See module doc comment.
pub struct NitWriter {
    database: DatabaseHandle,
    rx: mpsc::UnboundedReceiver<NitObservation>,
}

impl NitWriter {
    /// Create the writer and install its sender as the process-wide target
    /// for every `NitCollector` (`tuner/nit_collector.rs`). Must be called at
    /// most once per process — there is only one `Database`/writer. A second
    /// call is logged as a warning (not a panic: a programming mistake here
    /// must never bring down the server) and its `NitWriter` will simply
    /// receive no observations.
    pub fn new(database: DatabaseHandle) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        if !nit_collector::set_global_sender(tx) {
            log::warn!(
                "[NitWriter] a global NIT sender was already installed; this NitWriter \
                 instance will receive no observations (NitWriter::new called more than once?)"
            );
        }
        Self { database, rx }
    }

    /// Run the apply loop. Never returns during normal operation (same
    /// convention as `epg_writer::EpgWriter::run`); intended to be
    /// `tokio::spawn`ed once from `main.rs`.
    pub async fn run(mut self) {
        // Network ids whose metadata has already been applied in this
        // process. Only entries that actually reached the database are
        // recorded, so an observation that arrives before the row exists
        // (channel added later) is retried on the next reader start.
        let mut applied: HashSet<u16> = HashSet::new();

        while let Some(observation) = self.rx.recv().await {
            if applied.contains(&observation.nid) {
                continue;
            }

            let db = self.database.lock().await;
            match db.fill_missing_terrestrial_metadata(
                observation.nid,
                observation.remote_control_key,
                observation.physical_ch,
                observation.network_name.as_deref(),
            ) {
                Ok(0) => {
                    // Nothing was missing (the common case once a network has
                    // been scanned) — remember it so the repeated reports for
                    // this network stop taking the database lock.
                    applied.insert(observation.nid);
                }
                Ok(n) => {
                    applied.insert(observation.nid);
                    info!(
                        "[NitWriter] filled channel metadata from NIT: nid={} tsid={} \
                         remote_control_key={:?} physical_ch={:?} ({} row(s))",
                        observation.nid,
                        observation.tsid,
                        observation.remote_control_key,
                        observation.physical_ch,
                        n
                    );
                }
                Err(e) => {
                    // Deliberately not inserted into `applied`: a transient
                    // failure should be retried on the next observation.
                    error!(
                        "[NitWriter] failed to apply NIT metadata for nid={}: {}",
                        observation.nid, e
                    );
                }
            }
        }

        // The static sender lives for the process lifetime, so this should
        // not normally happen.
        debug!("[NitWriter] channel closed, stopping");
    }
}
