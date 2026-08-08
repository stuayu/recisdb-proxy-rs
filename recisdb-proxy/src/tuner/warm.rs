//! Warm BonDriver handling for pre-opened tuners.

use std::sync::mpsc;
use std::sync::Arc;

use log::{error, info, warn};
use tokio::sync::oneshot;

use crate::bondriver::BonDriverTuner;
use crate::tuner::pool::SlotPermit;
use crate::tuner::shared::{ReaderStartupConfig, SharedTuner};

pub enum WarmCommand {
    Start {
        shared: Arc<SharedTuner>,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        ready_tx: oneshot::Sender<Result<(), String>>,
    },
    Shutdown,
}

pub struct WarmTunerHandle {
    path: String,
    cmd_tx: mpsc::Sender<WarmCommand>,
    ready_rx: Option<oneshot::Receiver<Result<(), String>>>,
    ready_result: Option<Result<(), String>>,
    join_handle: Option<tokio::task::JoinHandle<()>>,
    /// This warm tuner's reservation against `path`'s `max_instances`
    /// capacity (docs/TUNER_PIPELINE_REDESIGN.md P1b §5), acquired by the
    /// caller (`Session::maybe_start_warm_tuner`) *before* `spawn` — prewarm
    /// must not even open the BonDriver if the driver has no spare slot,
    /// since an idle-but-open warm instance occupies one exactly like a live
    /// reader does (§2.1-4: this used to be invisible to capacity
    /// accounting entirely). Held for as long as this handle sits warm;
    /// `take_permit` moves it out for `activate` to transfer onto the target
    /// `SharedTuner`, and simply dropping this handle (`shutdown`, or losing
    /// the race against `cmd_rx`'s timeout) releases it back automatically.
    permit: Option<SlotPermit>,
}

impl WarmTunerHandle {
    /// `permit` must already be held for `path` (see this type's `permit`
    /// field doc) — callers get one via `TunerPool::acquire_slot` and skip
    /// spawning entirely if that returns `None` (docs/TUNER_PIPELINE_REDESIGN.md
    /// P1b §5, first bullet).
    pub fn spawn(path: String, timeout_secs: u64, permit: SlotPermit) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WarmCommand>();
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), String>>();

        let thread_path = path.clone();
        let join_handle = tokio::task::spawn_blocking(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                info!("[WarmTuner] Opening BonDriver: {}", thread_path);
                let tuner = match BonDriverTuner::new(&thread_path) {
                    Ok(tuner) => {
                        info!("[WarmTuner] BonDriver opened: {}", thread_path);
                        tuner
                    }
                    Err(e) => {
                        error!("[WarmTuner] Failed to open BonDriver {}: {} (kind: {:?})", thread_path, e, e.kind());
                        let _ = ready_tx.send(Err(format!("BonDriver error: {}", e)));
                        return;
                    }
                };

                let _ = ready_tx.send(Ok(()));

                let cmd = if timeout_secs > 0 {
                    cmd_rx.recv_timeout(std::time::Duration::from_secs(timeout_secs)).ok()
                } else {
                    cmd_rx.recv().ok()
                };

                match cmd {
                    Some(WarmCommand::Start { shared, tuner_path, space, channel, startup_config, ready_tx }) => {
                        SharedTuner::run_bondriver_reader_with_tuner(
                            shared,
                            tuner,
                            tuner_path,
                            space,
                            channel,
                            startup_config,
                            ready_tx,
                        );
                    }
                    Some(WarmCommand::Shutdown) => {
                        info!("[WarmTuner] Shutdown requested before channel set");
                    }
                    None => {
                        warn!("[WarmTuner] Warm tuner timed out before start");
                    }
                }
            }));

            if let Err(panic_err) = result {
                error!("[WarmTuner] Panic in warm thread: {:?}", panic_err);
            }
        });

        Self {
            path,
            cmd_tx,
            ready_rx: Some(ready_rx),
            ready_result: None,
            join_handle: Some(join_handle),
            permit: Some(permit),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// Take this handle's slot permit, if it still holds one.
    ///
    /// Called by whoever is about to `activate` this warm tuner, to move the
    /// permit onto the target `SharedTuner` and then pass it into `activate`
    /// as an explicit argument — mirroring `SharedTuner::take_slot_permit`'s
    /// own "extract, then hand back in" pattern so both the cold and warm
    /// start paths require a permit at the same call boundary (`activate`/
    /// `start_bondriver_reader`), not one implicitly and the other not.
    pub fn take_permit(&mut self) -> Option<SlotPermit> {
        self.permit.take()
    }

    async fn ensure_ready(&mut self) -> Result<(), String> {
        if let Some(result) = &self.ready_result {
            return result.clone();
        }

        if let Some(ready_rx) = self.ready_rx.take() {
            match ready_rx.await {
                Ok(result) => {
                    self.ready_result = Some(result.clone());
                    result
                }
                Err(_) => Err("Warm tuner readiness channel closed".to_string()),
            }
        } else {
            Ok(())
        }
    }

    /// Activate this warm (already-open) BonDriver against `shared`, tuning
    /// it to `space`/`channel` and starting its reader loop.
    ///
    /// `permit` is required (docs/TUNER_PIPELINE_REDESIGN.md P1b §3/§5) — in
    /// the normal flow it is this same handle's own reservation, retrieved
    /// by the caller via `take_permit()` immediately before this call (see
    /// that method's doc comment for why it is an explicit argument here
    /// rather than read directly off `self`). It is stored onto `shared`
    /// before the warm thread is signaled to start, so a failure inside
    /// `run_bondriver_reader_with_tuner` (wrong-thread SetChannel error,
    /// etc.) releases it the same way a cold `start_bondriver_reader`
    /// failure does — via `SharedTuner::stop_and_release_slot`.
    pub async fn activate(
        &mut self,
        shared: Arc<SharedTuner>,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        permit: SlotPermit,
    ) -> Result<(), std::io::Error> {
        shared.set_slot_permit(permit);

        self.ensure_ready().await.map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::Other, err)
        })?;

        // Mirrors `SharedTuner::start_bondriver_reader`'s synchronous
        // `Starting` transition (docs/TUNER_PIPELINE_REDESIGN.md §4 P1): the
        // `shared` entry passed in here came from `TunerPool::get_or_create`,
        // which already marks freshly-created entries `Starting`, so this is
        // idempotent defensive coverage for any future caller that reuses an
        // `Idle`/`Stopped` `SharedTuner` with a warm handle.
        shared.set_state(crate::tuner::shared::ReaderState::Starting);

        let (start_tx, start_rx) = oneshot::channel::<Result<(), String>>();
        let cmd = WarmCommand::Start {
            shared: Arc::clone(&shared),
            tuner_path,
            space,
            channel,
            startup_config,
            ready_tx: start_tx,
        };

        if self.cmd_tx.send(cmd).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Warm tuner command channel closed",
            ));
        }

        if let Some(handle) = self.join_handle.take() {
            shared.set_reader_handle(handle).await;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(10), start_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(err))) => Err(std::io::Error::new(std::io::ErrorKind::Other, err)),
            Ok(Err(_)) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "Warm tuner start channel closed",
            )),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Timeout waiting for warm tuner",
            )),
        }
    }

    pub async fn shutdown(mut self) {
        let _ = self.cmd_tx.send(WarmCommand::Shutdown);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.await;
        }
    }
}
