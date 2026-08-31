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
            // Tracks the `SharedTuner` a `WarmCommand::Start` has committed
            // to (and already stored a slot permit onto — see `activate`),
            // so the panic handler below can release that permit if a panic
            // unwinds out of `run_bondriver_reader_with_tuner` before its own
            // internal `catch_unwind`/failure paths get a chance to
            // (docs/TUNER_PIPELINE_REDESIGN.md P2a item 2: this was a real
            // leak — the cold-start path's equivalent top-level panic
            // handler already released the slot, this one didn't).
            let mut started_shared: Option<Arc<SharedTuner>> = None;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                info!("[WarmTuner] Opening BonDriver: {}", thread_path);
                let tuner = match BonDriverTuner::new(&thread_path) {
                    Ok(tuner) => {
                        info!("[WarmTuner] BonDriver opened: {}", thread_path);
                        tuner
                    }
                    Err(e) => {
                        error!(
                            "[WarmTuner] Failed to open BonDriver {}: {} (kind: {:?})",
                            thread_path,
                            e,
                            e.kind()
                        );
                        let _ = ready_tx.send(Err(format!("BonDriver error: {}", e)));
                        return;
                    }
                };

                let _ = ready_tx.send(Ok(()));

                let cmd = if timeout_secs > 0 {
                    cmd_rx
                        .recv_timeout(std::time::Duration::from_secs(timeout_secs))
                        .ok()
                } else {
                    cmd_rx.recv().ok()
                };

                match cmd {
                    Some(WarmCommand::Start {
                        shared,
                        tuner_path,
                        space,
                        channel,
                        startup_config,
                        ready_tx,
                    }) => {
                        started_shared = Some(Arc::clone(&shared));
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
                if let Some(shared) = started_shared {
                    shared.stop_and_release_slot();
                }
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
    /// Called only from [`SharedTuner::start_reader`]
    /// (docs/TUNER_PIPELINE_REDESIGN.md P2a item 2), which has already
    /// stored this attempt's [`SlotPermit`] onto `shared` (via
    /// `set_slot_permit`) and transitioned it to `Starting` *before*
    /// dispatching to cold-open or here — both paths go through that one
    /// permit-storage step and one `ready_timeout` computation
    /// (`timing::reader_ready_timeout`) instead of each doing their own.
    ///
    /// Failure branches, and what each leaves behind for the caller:
    ///
    /// - **`ErrorKind::NotConnected`** — the warm thread is gone (it failed
    ///   to open the BonDriver, or timed out waiting for a command and
    ///   exited). It holds no DLL handle, so the caller may retry this exact
    ///   attempt cold **on the same slot**: the permit is deliberately left
    ///   on `shared`. `SharedTuner::start_reader_warm` keys its cold
    ///   fallback off this error kind.
    /// - **`ready`-wait timeout** — the permit is also left in place, but a
    ///   cold retry would be a double open: the warm thread may still be
    ///   mid-`SetChannel` and will release the slot itself once it discovers
    ///   the dropped receiver (the same §2.1-1 mechanism the cold path
    ///   relies on). Not retryable here.
    /// - **anything else** — the reader already ran
    ///   `run_bondriver_reader_with_tuner`, which releases the permit on
    ///   every one of its own failure paths.
    ///
    /// `stop_and_release_slot` is idempotent, so a caller that gives up may
    /// always call it defensively.
    pub(crate) async fn activate(
        &mut self,
        shared: Arc<SharedTuner>,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
        ready_timeout: std::time::Duration,
    ) -> Result<(), std::io::Error> {
        if let Err(err) = self.ensure_ready().await {
            // The warm thread already failed to open the BonDriver (or its
            // ready channel closed for some other reason). It is definitely
            // gone and holds no DLL handle, so this attempt can still be
            // retried cold on the very same slot — the permit is left on
            // `shared` for `SharedTuner::start_reader_warm` to recover, and
            // `NotConnected` is the marker that says recovery is safe (see
            // that function). Whoever gives up releases it.
            return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, err));
        }

        // Mirrors `SharedTuner::start_reader`'s synchronous `Starting`
        // transition (docs/TUNER_PIPELINE_REDESIGN.md §4 P1): the `shared`
        // entry passed in here came from `TunerPool::get_or_create`, which
        // already marks freshly-created entries `Starting`, so this is
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
            // The warm thread's `cmd_rx` is gone (it hit `prewarm_timeout_secs`
            // and exited, closing the BonDriver on its way out) — same
            // reasoning as the `ensure_ready` failure above: retryable cold,
            // permit left in place, `NotConnected` marks it as such. This is
            // the *common* warm failure in practice: a client that sits on
            // the channel list longer than the prewarm timeout before
            // selecting anything.
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Warm tuner command channel closed",
            ));
        }

        if let Some(handle) = self.join_handle.take() {
            shared.set_reader_handle(handle).await;
        }

        match tokio::time::timeout(ready_timeout, start_rx).await {
            Ok(Ok(Ok(()))) => Ok(()),
            // The reader already ran `run_bondriver_reader_with_tuner`,
            // which releases the permit on every one of its own failure
            // paths before sending this `Err` — nothing further to do here.
            Ok(Ok(Err(err))) => Err(std::io::Error::new(std::io::ErrorKind::Other, err)),
            Ok(Err(_)) => {
                // `start_tx` was dropped without sending — the warm thread
                // ended without going through `run_bondriver_reader_with_tuner`'s
                // normal completion (e.g. a panic our own spawn()-level
                // handler already released the slot for). Release
                // defensively; idempotent if already done.
                shared.stop_and_release_slot();
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "Warm tuner start channel closed",
                ))
            }
            Err(_) => {
                // Timed out waiting for the warm thread to finish
                // SetChannel. Do NOT release the slot here — see this
                // function's doc comment.
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "Timeout waiting for warm tuner",
                ))
            }
        }
    }

    pub async fn shutdown(mut self) {
        let _ = self.cmd_tx.send(WarmCommand::Shutdown);
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tuner::channel_key::ChannelKey;
    use crate::tuner::shared::ReaderState;
    use crate::tuner::TunerPool;

    fn startup_config() -> ReaderStartupConfig {
        ReaderStartupConfig {
            set_channel_retry_interval_ms: 5,
            set_channel_retry_timeout_ms: 50,
            signal_poll_interval_ms: 5,
            signal_wait_timeout_ms: 50,
            no_data_timeout_secs: 30,
            b25_enabled: true,
            mmt_converter: None,
        }
    }

    /// A warm handle whose thread is gone must report `NotConnected` and
    /// **leave the slot permit on the target** so
    /// `SharedTuner::start_reader_warm` can fall back to a cold open on the
    /// same slot (docs/TUNER_PIPELINE_REDESIGN.md P2a item 2).
    ///
    /// Losing that fallback would turn the routine case — `prewarm_timeout_secs`
    /// expiring while a client browses the channel list — into a user-visible
    /// tuning failure.
    ///
    /// This environment has no BonDriver DLL, so `WarmTunerHandle::spawn`'s
    /// open fails and the thread exits immediately: exactly the "warm thread
    /// is gone" state this branch exists for.
    #[tokio::test]
    async fn activate_on_a_dead_warm_thread_reports_not_connected_and_keeps_the_permit() {
        let pool = TunerPool::new(4);
        let path = "/nonexistent/BonDriver_Test.dll";

        let warm_permit = pool
            .acquire_slot(path, 1)
            .await
            .expect("first slot is free");
        let mut warm = WarmTunerHandle::spawn(path.to_string(), 1, warm_permit);

        let target_permit = warm
            .take_permit()
            .expect("warm handle holds the permit until activation");
        let shared = SharedTuner::new(ChannelKey::space_channel(path, 0, 13), 2);
        shared.set_slot_permit(target_permit);
        shared.set_state(ReaderState::Starting);

        let err = warm
            .activate(
                Arc::clone(&shared),
                path.to_string(),
                0,
                13,
                startup_config(),
                std::time::Duration::from_millis(500),
            )
            .await
            .expect_err("opening a nonexistent BonDriver must fail");

        assert_eq!(
            err.kind(),
            std::io::ErrorKind::NotConnected,
            "the warm thread is gone, so the caller may retry cold: {err}"
        );
        assert!(
            shared.take_slot_permit().is_some(),
            "the permit must survive for the cold fallback to reuse"
        );

        warm.shutdown().await;
    }
}
