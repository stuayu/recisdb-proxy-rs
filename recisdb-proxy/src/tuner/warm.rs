//! Warm BonDriver handling for pre-opened tuners.

use std::sync::mpsc;
use std::sync::Arc;

use log::{error, info, warn};
use tokio::sync::oneshot;

use crate::bondriver::BonDriverTuner;
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
}

impl WarmTunerHandle {
    pub fn spawn(path: String, timeout_secs: u64) -> Self {
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
        }
    }

    pub fn path(&self) -> &str {
        &self.path
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

    pub async fn activate(
        &mut self,
        shared: Arc<SharedTuner>,
        tuner_path: String,
        space: u32,
        channel: u32,
        startup_config: ReaderStartupConfig,
    ) -> Result<(), std::io::Error> {
        self.ensure_ready().await.map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::Other, err)
        })?;

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
