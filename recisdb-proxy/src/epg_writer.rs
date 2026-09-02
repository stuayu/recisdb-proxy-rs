//! Background task that batches parsed EIT events into periodic UPSERTs
//! against the `programs` table (Migration 015).
//!
//! `EpgCollector` (`tuner/epg_collector.rs`) runs inside the tuner reader
//! thread and has no `Database` handle available to it (see that module's
//! doc comment for why). Instead it forwards parsed rows through a
//! process-wide mpsc channel, installed here in [`EpgWriter::new`] and
//! drained by [`EpgWriter::run`].
//!
//! Shaped like `alert::AlertManager` (owns the shared `DatabaseHandle`,
//! `run(self)` loop spawned once from `main.rs`), but driven by the mpsc
//! channel instead of a fixed timer alone: events are buffered in memory
//! (deduped by `(nid, sid, tsid, event_id)`, keeping the newest `updated_at`) and
//! flushed either when [`FLUSH_INTERVAL`] elapses or [`FLUSH_BATCH_SIZE`]
//! records have queued up, whichever comes first — "毎チャンク書くのでは
//! なく、一定間隔(例: 10秒)か一定件数でフラッシュ" from the design.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::time::Duration;

use log::{debug, error, info, warn};
use tokio::sync::{broadcast, mpsc};
use tokio::time::MissedTickBehavior;

use crate::database::ProgramUpsert;
use crate::server::listener::DatabaseHandle;
use crate::tuner::epg_collector;

/// Flush the buffered batch after this much time, even if it hasn't reached
/// [`FLUSH_BATCH_SIZE`].
const FLUSH_INTERVAL: Duration = Duration::from_secs(10);

/// Flush early once this many distinct events are buffered, so a busy
/// multiplex (BS carries schedule-other sections for many services at
/// once) doesn't grow the in-memory buffer unbounded between timer ticks.
const FLUSH_BATCH_SIZE: usize = 500;

/// Prune rows older than 24h only once every this-many flushes (a `DELETE
/// ... WHERE` full-table scan does not need to run on every 10s flush).
/// 30 flushes at the default 10s interval is ~5 minutes.
const PRUNE_EVERY_N_FLUSHES: u32 = 30;

/// EPG batching/UPSERT task. See module doc comment.
pub struct EpgWriter {
    database: DatabaseHandle,
    rx: mpsc::UnboundedReceiver<ProgramUpsert>,
    /// Fan-out for `GET /mirakurun/api/events/stream`
    /// (`web/mirakurun_events.rs`) — every record that is successfully
    /// UPSERTed is also broadcast here so EPGStation's incremental EPG
    /// updater sees it without waiting for its next full `/programs` poll.
    /// The `Sender` is created once in `main.rs` and shared with
    /// `web::state::WebState` so both sides hold the same channel; see that
    /// field's doc comment for the capacity rationale.
    events_tx: broadcast::Sender<ProgramUpsert>,
}

impl EpgWriter {
    /// Create the writer and install its sender as the process-wide target
    /// for every `EpgCollector` (`tuner/epg_collector.rs`). Must be called
    /// at most once per process — there is only one `Database`/writer.
    /// A second call is logged as a warning (not a panic: a programming
    /// mistake here must never bring down the server) and its `EpgWriter`
    /// will simply receive no events.
    ///
    /// `events_tx` is the same `broadcast::Sender` handed to
    /// `web::state::WebState` (wired up in `main.rs`) — see
    /// [`Self::flush`] for what gets sent through it and when.
    pub fn new(database: DatabaseHandle, events_tx: broadcast::Sender<ProgramUpsert>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        if !epg_collector::set_global_sender(tx) {
            warn!(
                "[EpgWriter] a global EPG sender was already installed; this EpgWriter \
                 instance will receive no events (EpgWriter::new called more than once?)"
            );
        }
        Self {
            database,
            rx,
            events_tx,
        }
    }

    /// Run the batching/UPSERT loop. Never returns during normal operation
    /// (same convention as `alert::AlertManager::run`); intended to be
    /// `tokio::spawn`ed once from `main.rs`.
    pub async fn run(mut self) {
        // Keyed by (nid, sid, tsid, event_id) — the table's UNIQUE constraint —
        // so a burst of duplicate present/following + schedule sections for
        // the same event within one flush window collapses into a single
        // UPSERT, keeping only the newest `updated_at`.
        let mut batch: HashMap<(u16, u16, u16, u16, u8), ProgramUpsert> = HashMap::new();
        let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut flush_count: u32 = 0;

        loop {
            tokio::select! {
                maybe_event = self.rx.recv() => {
                    match maybe_event {
                        Some(record) => {
                            let key = (record.nid, record.sid, record.tsid, record.event_id, record.source as u8);
                            match batch.entry(key) {
                                Entry::Occupied(mut e) => {
                                    if record.updated_at >= e.get().updated_at {
                                        e.insert(record);
                                    }
                                }
                                Entry::Vacant(e) => {
                                    e.insert(record);
                                }
                            }
                            if batch.len() >= FLUSH_BATCH_SIZE {
                                self.flush(&mut batch, &mut flush_count).await;
                            }
                        }
                        None => {
                            // The static sender lives for the process
                            // lifetime, so this should not normally happen.
                            // Flush what we have and stop rather than spin.
                            self.flush(&mut batch, &mut flush_count).await;
                            info!("[EpgWriter] channel closed, stopping");
                            return;
                        }
                    }
                }
                _ = ticker.tick() => {
                    self.flush(&mut batch, &mut flush_count).await;
                }
            }
        }
    }

    async fn flush(
        &self,
        batch: &mut HashMap<(u16, u16, u16, u16, u8), ProgramUpsert>,
        flush_count: &mut u32,
    ) {
        if batch.is_empty() {
            return;
        }
        let records: Vec<ProgramUpsert> = batch.drain().map(|(_, v)| v).collect();
        let count = records.len();

        let mut db = self.database.lock().await;
        match db.upsert_programs(&records) {
            Ok(_) => {
                debug!("[EpgWriter] flushed {} program row(s)", count);
                if let Err(e) = db.refresh_epg_coverage() {
                    warn!("[EpgWriter] failed to refresh coverage: {}", e);
                }
                // Fan out to `/mirakurun/api/events/stream` subscribers only
                // *after* the UPSERT actually succeeded — a subscriber must
                // never observe an event for a row that failed to persist.
                // `name.is_none()` rows are dropped here (not upstream in
                // `EpgCollector`): EPGStation discards any `program` event
                // whose `data.name` is `undefined`
                // (`EPGUpdateManageModel.ts:554`, see `mirakurun_events.rs`
                // module doc comment), so forwarding them would be pure
                // waste. `type` is always `"update"` — this project's UPSERT
                // does not distinguish "new row" from "existing row changed"
                // (see `mirakurun_events.rs` for the full rationale), and
                // EPGStation treats `create`/`update` identically anyway.
                // `broadcast::Sender::send` errors only when there are zero
                // subscribers (nobody has opened `/events/stream`); that is
                // the common case, not a failure, so the `Result` is
                // deliberately ignored.
                for record in &records {
                    if record.name.is_some() {
                        let _ = self.events_tx.send(record.clone());
                    }
                }
            }
            Err(e) => error!(
                "[EpgWriter] failed to upsert {} program row(s): {}",
                count, e
            ),
        }

        *flush_count += 1;
        if flush_count.is_multiple_of(PRUNE_EVERY_N_FLUSHES) {
            let now = chrono::Utc::now().timestamp();
            match db.prune_old_programs(now) {
                Ok(n) if n > 0 => debug!("[EpgWriter] pruned {} stale program row(s)", n),
                Ok(_) => {}
                Err(e) => error!("[EpgWriter] failed to prune old programs: {}", e),
            }
            if let Err(e) = db.refresh_epg_coverage() {
                warn!("[EpgWriter] failed to refresh coverage after prune: {}", e);
            }
        }
    }
}
