//! Shared encoder pool (STREAMING_DESIGN.md §5 "tsreplace 複数ストリームの高速化").
//!
//! # Problem
//!
//! Previously, `Session` spawned its own tsreplace/QSVEncC process chain per
//! client. When several sessions watched the same channel with tsreplace
//! enabled, each one started an independent encoder, so N sessions meant N
//! simultaneous hardware-encode requests fighting over a limited number of
//! HW encode sessions (Intel QSV etc.).
//!
//! # Design
//!
//! This mirrors the `TunerPool` / `SharedTuner` sharing pattern used for raw
//! tuner reads (see `crate::tuner::pool` / `crate::tuner::shared` and
//! DESIGN.md §4.3):
//!
//! - [`EncodeKey`] identifies "the same encode": same tuned channel, same SID
//!   set, and the same tsreplace config generation. Sessions that resolve to
//!   an identical key join the existing running encoder instead of starting
//!   a new one.
//! - [`SharedEncoder`] owns one tsreplace process chain. A dedicated task
//!   subscribes to the source [`SharedTuner`]'s broadcast and feeds the
//!   chain's stdin; another dedicated task reads the chain's stdout and
//!   re-broadcasts it to every subscribed session. A watchdog task kills the
//!   chain if input keeps flowing but output stalls for `read_timeout`.
//! - [`EncoderPool`] hands out `Arc<SharedEncoder>` instances, admits new
//!   encoders through a `tokio::sync::Semaphore` sized by
//!   `tsreplace_config.max_concurrent_encoders`, and idle-closes encoders
//!   with zero subscribers after a grace period (same pattern as
//!   `TunerPool::schedule_idle_close`).
//!
//! Sessions that can't get a slot (pool saturated) or whose encoder dies
//! (watchdog / EOF) fall back to raw TS passthrough — see
//! `Session::start_tsreplace_pipeline` in `server/session.rs`.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use log::{debug, info, warn};
use tokio::io::{AsyncReadExt, AsyncWriteExt, AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, oneshot, RwLock, Semaphore};

use crate::tuner::channel_key::ChannelKey;
use crate::tuner::shared::{SharedTuner, UntrackedSubscription, BROADCAST_CAPACITY};

/// Grace period between the last subscriber leaving and the encoder chain
/// actually being killed. Mirrors `TunerPool`'s keep-alive idea, but the
/// value is intentionally small and fixed (config value in
/// STREAMING_DESIGN.md §5.2 talks about `max_concurrent_encoders`, not a
/// per-encoder keep-alive knob).
const DEFAULT_IDLE_GRACE: Duration = Duration::from_secs(5);

/// Chunk size used when reading the chain's final stdout.
const OUTPUT_CHUNK_SIZE: usize = 256 * 1024;

/// Key identifying "the same shared encode".
///
/// Two sessions that resolve to an equal `EncodeKey` share a single running
/// encoder chain. `sids` is kept sorted+deduped by [`EncodeKey::new`] so that
/// SID ordering never causes a spurious cache miss.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EncodeKey {
    /// The tuned channel (tuner path + space/channel) whose TS is being encoded.
    pub channel_key: ChannelKey,
    /// SIDs being encoded, sorted ascending and deduplicated. Empty means
    /// "no `--service` injection" (either full-TS pass-through encode, or the
    /// user's own `arguments` already specify `--service`).
    pub sids: Vec<u16>,
    /// Hash of the tsreplace config fields that affect the spawned chain's
    /// behaviour (command path / arguments / read timeout). Bumped whenever
    /// the config changes so that new sessions spawn a fresh encoder with the
    /// new settings instead of joining a stale one; already-running encoders
    /// are left alone (STREAMING_DESIGN.md §5.2: "設定変更は次回エンコーダ
    /// 生成から反映で良い").
    pub config_generation: u64,
}

impl EncodeKey {
    /// Build a new key, normalizing `sids` (sort + dedup) so SID order never
    /// matters for equality/hash.
    pub fn new(channel_key: ChannelKey, mut sids: Vec<u16>, config_generation: u64) -> Self {
        sids.sort_unstable();
        sids.dedup();
        Self {
            channel_key,
            sids,
            config_generation,
        }
    }
}

/// Placeholder token in `preprocessor_arguments` / `arguments` that is
/// replaced with the target service id before spawning (e.g. tsreadex's
/// `-n {SID}`). When present in either template, the classic tsreplace-style
/// `--service` auto-injection is disabled.
pub const SID_PLACEHOLDER: &str = "{SID}";

/// Runtime external-encoder settings needed to spawn an encoder chain.
#[derive(Debug, Clone)]
pub struct EncoderRuntimeConfig {
    pub command_path: String,
    pub arguments: String,
    pub read_timeout_ms: u64,
    /// Optional stage-1 command placed *before* `command_path` in the OS
    /// pipe chain (`TS -> preprocessor -> encoder -> stdout`), e.g. tsreadex.
    /// Empty string means "no preprocessor" (single-stage, legacy behavior).
    /// TOML-only, same S1 trust boundary as `command_path`.
    pub preprocessor_path: String,
    /// Argument template for the preprocessor. May contain [`SID_PLACEHOLDER`].
    pub preprocessor_arguments: String,
}

impl EncoderRuntimeConfig {
    /// The preprocessor command, if one is configured (non-blank path).
    pub fn preprocessor(&self) -> Option<&str> {
        let trimmed = self.preprocessor_path.trim();
        if trimmed.is_empty() { None } else { Some(trimmed) }
    }
}

/// Compute a `config_generation` value from the external-encoder settings
/// that affect the spawned chain (command paths, argument templates,
/// watchdog timeout). Content-hash based, per STREAMING_DESIGN.md §5.2's
/// "設定ロード時に内容ハッシュか updated_at を使う".
pub fn config_generation(cfg: &EncoderRuntimeConfig) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    cfg.command_path.hash(&mut hasher);
    cfg.arguments.hash(&mut hasher);
    cfg.read_timeout_ms.hash(&mut hasher);
    cfg.preprocessor_path.hash(&mut hasher);
    cfg.preprocessor_arguments.hash(&mut hasher);
    hasher.finish()
}

/// Replace every [`SID_PLACEHOLDER`] occurrence with the service id.
fn substitute_sid(template: &str, sid: u16) -> String {
    template.replace(SID_PLACEHOLDER, &sid.to_string())
}

/// Errors returned by [`EncoderPool::get_or_create`].
#[derive(Debug, thiserror::Error)]
pub enum EncoderPoolError {
    /// `max_concurrent_encoders` has been reached and no existing encoder
    /// matches the requested [`EncodeKey`]. Callers should fall back to raw
    /// TS passthrough (STREAMING_DESIGN.md §5.2 (B)).
    #[error("shared encoder pool saturated (max_concurrent_encoders reached)")]
    Saturated,
    /// The encoder chain failed to spawn (bad command path, permissions,
    /// etc.).
    #[error("failed to spawn shared encoder: {0}")]
    SpawnFailed(String),
}

/// Check whether `--service`/`-s` is already present in the user's argument
/// template. If so, we must not auto-inject `--service <SID>` (that's the
/// pre-existing single-process behavior carried over from `session.rs`).
pub(crate) fn args_contain_service_option(arguments: &str) -> bool {
    arguments
        .split_whitespace()
        .any(|t| t == "--service" || t == "-s")
}

/// Build command arguments with `--service <SID>` auto-injected, inserting it
/// (and `--preserve-other-services`) before `-e`/`--encoder` if present, or
/// at the end otherwise.
fn build_tsreplace_args(base_arguments: &str, sid: u16) -> Vec<String> {
    let tokens: Vec<&str> = base_arguments.split_whitespace().collect();
    let mut args = Vec::with_capacity(tokens.len() + 4);

    let encoder_pos = tokens.iter().position(|t| *t == "-e" || *t == "--encoder");
    let has_preserve = tokens.iter().any(|t| *t == "--preserve-other-services");

    for (i, token) in tokens.iter().enumerate() {
        if encoder_pos == Some(i) {
            args.push("--service".to_string());
            args.push(sid.to_string());
            if !has_preserve {
                args.push("--preserve-other-services".to_string());
            }
        }
        args.push(token.to_string());
    }

    if encoder_pos.is_none() {
        args.push("--service".to_string());
        args.push(sid.to_string());
        if !has_preserve {
            args.push("--preserve-other-services".to_string());
        }
    }

    args
}

/// How [`SharedEncoder::spawn`] will wire up the external process(es) for a
/// given config + SID set. Computed by [`plan_spawn`] as a pure function so
/// the branch selection and `{SID}` substitution are unit-testable without
/// spawning real processes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SpawnPlan {
    /// One process: `command_path` with the given argument vector.
    Single { enc_args: Vec<String> },
    /// Two processes connected by an OS pipe:
    /// `stdin -> preprocessor_path(pre_args) -> command_path(enc_args) -> stdout`.
    TwoStage { pre_args: Vec<String>, enc_args: Vec<String> },
    /// Legacy tsreplace multi-SID chain (`spawn_chain`), one `command_path`
    /// process per SID with `--service` auto-injection. Never used together
    /// with a preprocessor or `{SID}` placeholder.
    Chain,
}

/// Decide process wiring and argument vectors for `cfg` + `sids`.
///
/// Rules (in priority order):
/// 1. If either argument template contains [`SID_PLACEHOLDER`], the token is
///    substituted with the (single) target SID and no `--service` injection
///    happens. Requires exactly one SID.
/// 2. Otherwise, the pre-existing tsreplace behavior: `--service <SID>`
///    auto-injection into `arguments` unless the template already carries
///    `--service`/`-s` or `sids` is empty; 2+ SIDs use the per-SID chain.
/// 3. A preprocessor (when configured) is prepended as stage 1 in the
///    single-process cases; it is not supported with the multi-SID chain.
fn plan_spawn(cfg: &EncoderRuntimeConfig, sids: &[u16]) -> std::io::Result<SpawnPlan> {
    let split = |s: &str| -> Vec<String> { s.split_whitespace().map(String::from).collect() };
    let has_preprocessor = cfg.preprocessor().is_some();
    let has_placeholder = cfg.arguments.contains(SID_PLACEHOLDER)
        || cfg.preprocessor_arguments.contains(SID_PLACEHOLDER);

    if has_placeholder {
        let sid = match sids {
            [sid] => *sid,
            [] => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "argument template uses {} but no target service id was resolved",
                        SID_PLACEHOLDER
                    ),
                ));
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "argument template uses {} but {} SIDs were requested; \
                         the placeholder supports exactly one service per encoder",
                        SID_PLACEHOLDER,
                        sids.len()
                    ),
                ));
            }
        };
        let enc_args = split(&substitute_sid(&cfg.arguments, sid));
        return Ok(if has_preprocessor {
            SpawnPlan::TwoStage {
                pre_args: split(&substitute_sid(&cfg.preprocessor_arguments, sid)),
                enc_args,
            }
        } else {
            SpawnPlan::Single { enc_args }
        });
    }

    let user_specified_service = args_contain_service_option(&cfg.arguments);
    let enc_args = if user_specified_service || sids.is_empty() {
        split(&cfg.arguments)
    } else if sids.len() == 1 {
        build_tsreplace_args(&cfg.arguments, sids[0])
    } else {
        // Multi-SID chain.
        if has_preprocessor {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "preprocessor_path is not supported with a multi-SID encoder chain; \
                 use a {SID}-placeholder template or a single-SID stream",
            ));
        }
        return Ok(SpawnPlan::Chain);
    };

    Ok(if has_preprocessor {
        SpawnPlan::TwoStage {
            pre_args: split(&cfg.preprocessor_arguments),
            enc_args,
        }
    } else {
        SpawnPlan::Single { enc_args }
    })
}

/// Spawn a single process with the given stdin/stdout wiring.
fn spawn_process(
    command_path: &str,
    args: &[String],
    stdin_cfg: Stdio,
    stdout_cfg: Stdio,
) -> std::io::Result<Child> {
    let mut cmd = Command::new(command_path);
    for arg in args {
        cmd.arg(arg);
    }
    cmd.stdin(stdin_cfg)
        .stdout(stdout_cfg)
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn()
}

/// Forward a chain member's stderr to the log, tagged with the encode key
/// and (if applicable) the SID it is responsible for.
fn spawn_stderr_logger(label: String, sid: Option<u16>, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match sid {
                    Some(sid) => debug!("[SharedEncoder {} SID={}] {}", label, sid, line),
                    None => debug!("[SharedEncoder {}] {}", label, line),
                },
                Ok(None) => break,
                Err(e) => {
                    warn!("[SharedEncoder {}] stderr read failed: {}", label, e);
                    break;
                }
            }
        }
    });
}

/// Build a single-process chain (no per-SID `--service` injection): used
/// when the caller's arguments already specify `--service`, or when there
/// are no SIDs to encode (full passthrough through one process — this is
/// also what the `cat`-based tests use).
fn spawn_single(
    command_path: &str,
    args: &[String],
    label: &str,
) -> std::io::Result<(ChildStdin, ChildStdout, Vec<Child>)> {
    let mut child = spawn_process(command_path, args, Stdio::piped(), Stdio::piped()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to spawn '{}': {}", command_path, e),
        )
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "stdin not available"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "stdout not available"))?;

    if let Some(stderr) = child.stderr.take() {
        spawn_stderr_logger(label.to_string(), None, stderr);
    }

    Ok((stdin, stdout, vec![child]))
}

/// Build a two-stage pipeline via an OS-level pipe:
/// `stdin -> preprocessor (stage 1, e.g. tsreadex) -> encoder (stage 2,
/// e.g. QSVEncC) -> stdout`. Both stages get their stderr forwarded to the
/// log, and both children are returned so the watchdog/`finish()` kill path
/// covers the whole chain.
fn spawn_two_stage(
    pre_path: &str,
    pre_args: &[String],
    enc_path: &str,
    enc_args: &[String],
    label: &str,
) -> std::io::Result<(ChildStdin, ChildStdout, Vec<Child>)> {
    let mut pre_child =
        spawn_process(pre_path, pre_args, Stdio::piped(), Stdio::piped()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("failed to spawn preprocessor '{}': {}", pre_path, e),
            )
        })?;

    let stdin = pre_child.stdin.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "preprocessor stdin not available")
    })?;
    let pre_stdout = pre_child.stdout.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "preprocessor stdout not available")
    })?;
    if let Some(stderr) = pre_child.stderr.take() {
        spawn_stderr_logger(format!("{} stage1", label), None, stderr);
    }

    let pre_stdio: Stdio = pre_stdout.try_into().map_err(|e: std::io::Error| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to convert preprocessor stdout to Stdio: {}", e),
        )
    })?;

    let mut enc_child = spawn_process(enc_path, enc_args, pre_stdio, Stdio::piped()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("failed to spawn encoder '{}': {}", enc_path, e),
        )
    })?;

    let stdout = enc_child.stdout.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "encoder stdout not available")
    })?;
    if let Some(stderr) = enc_child.stderr.take() {
        spawn_stderr_logger(format!("{} stage2", label), None, stderr);
    }

    Ok((stdin, stdout, vec![pre_child, enc_child]))
}

/// Build a chained multi-SID process pipeline via OS-level pipes:
/// `stdin -> proc(SID1) -> proc(SID2) -> ... -> proc(SIDn) -> stdout`.
/// Each process encodes its target SID while passing all other packets
/// through immediately, so all SIDs encode in parallel with near-zero
/// inter-process overhead. Ported from the original per-session
/// implementation in `server/session.rs`.
fn spawn_chain(
    command_path: &str,
    base_arguments: &str,
    sids: &[u16],
    label: &str,
) -> std::io::Result<(ChildStdin, ChildStdout, Vec<Child>)> {
    let mut children: Vec<Child> = Vec::with_capacity(sids.len());

    let args_first = build_tsreplace_args(base_arguments, sids[0]);
    let mut first_child =
        spawn_process(command_path, &args_first, Stdio::piped(), Stdio::piped()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("failed to spawn encoder for SID {}: {}", sids[0], e),
            )
        })?;

    let pipeline_stdin = first_child.stdin.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "encoder chain: stdin not available")
    })?;
    if let Some(stderr) = first_child.stderr.take() {
        spawn_stderr_logger(label.to_string(), Some(sids[0]), stderr);
    }
    children.push(first_child);

    for &sid in &sids[1..] {
        let args = build_tsreplace_args(base_arguments, sid);

        let prev_stdout = children.last_mut().unwrap().stdout.take().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("encoder chain: stdout not available for previous stage (SID {})", sid),
            )
        })?;

        let prev_stdio: Stdio = prev_stdout.try_into().map_err(|e: std::io::Error| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("encoder chain: failed to convert stdout to Stdio: {}", e),
            )
        })?;

        let mut child = spawn_process(command_path, &args, prev_stdio, Stdio::piped()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("failed to spawn encoder for SID {}: {}", sid, e),
            )
        })?;

        if let Some(stderr) = child.stderr.take() {
            spawn_stderr_logger(label.to_string(), Some(sid), stderr);
        }
        children.push(child);
    }

    let final_stdout = children.last_mut().unwrap().stdout.take().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "encoder chain: final stdout not available")
    })?;

    Ok((pipeline_stdin, final_stdout, children))
}

/// Background tasks + process handles owned by a running [`SharedEncoder`].
/// Held behind a `tokio::sync::Mutex<Option<..>>` so `finish()` can take it
/// exactly once (idempotent shutdown).
struct EncoderTasks {
    children: Vec<Child>,
    feeder: tokio::task::JoinHandle<()>,
    outputter: tokio::task::JoinHandle<()>,
    watchdog: tokio::task::JoinHandle<()>,
}

/// One running shared encoder chain, broadcasting its output to every
/// subscribed session. Mirrors [`SharedTuner`]'s subscribe/unsubscribe and
/// broadcast pattern.
pub struct SharedEncoder {
    /// The key this encoder was created for.
    pub key: EncodeKey,
    /// Output broadcast sender. `None` once the encoder has stopped — taking
    /// it drops the last `Sender`, closing the channel for every existing
    /// `Receiver` (STREAMING_DESIGN.md §5: "購読者へ通知 (broadcast close で
    /// 伝わる)").
    tx_slot: std::sync::Mutex<Option<broadcast::Sender<Bytes>>>,
    subscriber_count: AtomicU32,
    is_running: AtomicBool,
    last_input_at: std::sync::Mutex<Instant>,
    last_output_at: std::sync::Mutex<Instant>,
    read_timeout: Duration,
    tasks: tokio::sync::Mutex<Option<EncoderTasks>>,
    /// Semaphore permit occupying one of `max_concurrent_encoders` slots.
    /// Auto-released when this `SharedEncoder` (and thus the permit) is
    /// finally dropped, i.e. once evicted from the pool's map and no session
    /// holds a reference to it anymore.
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl SharedEncoder {
    /// Spawn a new encoder chain and its supporting tasks.
    async fn spawn(
        key: EncodeKey,
        tuner: Arc<SharedTuner>,
        cfg: EncoderRuntimeConfig,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> std::io::Result<Arc<Self>> {
        let label = format!("{:?}", key.channel_key);

        let (stdin, stdout, children) = match plan_spawn(&cfg, &key.sids)? {
            SpawnPlan::Single { enc_args } => spawn_single(&cfg.command_path, &enc_args, &label)?,
            SpawnPlan::TwoStage { pre_args, enc_args } => spawn_two_stage(
                cfg.preprocessor().expect("TwoStage plan implies a preprocessor"),
                &pre_args,
                &cfg.command_path,
                &enc_args,
                &label,
            )?,
            SpawnPlan::Chain => spawn_chain(&cfg.command_path, &cfg.arguments, &key.sids, &label)?,
        };

        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let now = Instant::now();
        let read_timeout = Duration::from_millis(cfg.read_timeout_ms.max(1));

        // Subscribe to the source tuner synchronously (before spawning the
        // feeder task) so that by the time this function returns, we are
        // guaranteed to observe every subsequent broadcast chunk — no window
        // where a fast test/caller could send data before the feeder task
        // has had a chance to register its receiver.
        //
        // The subscription is untracked (no subscriber_count increment):
        // sessions that use this encoder keep their own tracked raw
        // subscriptions, so the tuner's keep-alive / idle-close accounting
        // stays entirely session-driven and the encoder can never keep a
        // tuner alive on its own.
        let tuner_rx = tuner.subscribe_untracked();

        let shared = Arc::new(Self {
            key,
            tx_slot: std::sync::Mutex::new(Some(tx)),
            subscriber_count: AtomicU32::new(0),
            is_running: AtomicBool::new(true),
            last_input_at: std::sync::Mutex::new(now),
            last_output_at: std::sync::Mutex::new(now),
            read_timeout,
            tasks: tokio::sync::Mutex::new(None),
            _permit: permit,
        });

        let feeder = tokio::spawn(Self::run_feeder(Arc::clone(&shared), tuner_rx, stdin));
        let outputter = tokio::spawn(Self::run_output(Arc::clone(&shared), stdout));
        let watchdog = tokio::spawn(Self::run_watchdog(Arc::clone(&shared)));

        *shared.tasks.lock().await = Some(EncoderTasks {
            children,
            feeder,
            outputter,
            watchdog,
        });

        info!(
            "[SharedEncoder {:?}] started (sids={:?}, generation={})",
            shared.key.channel_key, shared.key.sids, shared.key.config_generation
        );

        Ok(shared)
    }

    async fn run_feeder(shared: Arc<Self>, mut tuner_rx: UntrackedSubscription, mut stdin: ChildStdin) {
        loop {
            match tuner_rx.recv().await {
                Ok(data) => {
                    *shared.last_input_at.lock().unwrap() = Instant::now();
                    if let Err(e) = stdin.write_all(&data).await {
                        debug!("[SharedEncoder {:?}] stdin write failed: {}", shared.key.channel_key, e);
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("[SharedEncoder {:?}] input lagged, skipped {} chunks", shared.key.channel_key, n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!("[SharedEncoder {:?}] source tuner broadcast closed", shared.key.channel_key);
                    break;
                }
            }
        }
        let _ = stdin.shutdown().await;
    }

    async fn run_output(shared: Arc<Self>, mut stdout: ChildStdout) {
        let mut buf = vec![0u8; OUTPUT_CHUNK_SIZE];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) => {
                    debug!("[SharedEncoder {:?}] output EOF", shared.key.channel_key);
                    break;
                }
                Ok(n) => {
                    *shared.last_output_at.lock().unwrap() = Instant::now();
                    let data = Bytes::copy_from_slice(&buf[..n]);
                    let tx_guard = shared.tx_slot.lock().unwrap();
                    if let Some(tx) = tx_guard.as_ref() {
                        let _ = tx.send(data);
                    }
                }
                Err(e) => {
                    warn!("[SharedEncoder {:?}] output read failed: {}", shared.key.channel_key, e);
                    break;
                }
            }
        }
        shared.finish("output ended").await;
    }

    async fn run_watchdog(shared: Arc<Self>) {
        let check_interval = (shared.read_timeout / 4).max(Duration::from_millis(50));
        let mut ticker = tokio::time::interval(check_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if !shared.is_running.load(Ordering::Acquire) {
                break;
            }
            let last_input = *shared.last_input_at.lock().unwrap();
            let last_output = *shared.last_output_at.lock().unwrap();
            let now = Instant::now();
            // Only treat this as a stall if input is (recently) flowing but
            // output has not kept up — an idle channel with no input at all
            // is not the encoder's fault.
            if now.duration_since(last_input) < shared.read_timeout
                && now.duration_since(last_output) > shared.read_timeout
            {
                warn!(
                    "[SharedEncoder {:?}] output stalled for {:?} (>{:?}) while input was flowing, killing chain",
                    shared.key.channel_key,
                    now.duration_since(last_output),
                    shared.read_timeout
                );
                shared.finish("watchdog stall").await;
                break;
            }
        }
    }

    /// Idempotent shutdown: kill the process chain and close the output
    /// broadcast so every subscribed session observes `RecvError::Closed`.
    /// Safe to call from multiple tasks concurrently — only the first caller
    /// performs the actual work.
    async fn finish(&self, reason: &str) {
        if self
            .is_running
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }

        info!("[SharedEncoder {:?}] stopping ({})", self.key.channel_key, reason);

        // Drop the last Sender: any subscriber's next `recv()` observes
        // `RecvError::Closed` once its buffered backlog is drained.
        let _ = self.tx_slot.lock().unwrap().take();

        if let Some(mut tasks) = self.tasks.lock().await.take() {
            for mut child in tasks.children.drain(..).rev() {
                if let Err(e) = child.start_kill() {
                    debug!("[SharedEncoder {:?}] kill skipped: {}", self.key.channel_key, e);
                }
                let _ = child.wait().await;
            }
            tasks.watchdog.abort();
            tasks.feeder.abort();
            tasks.outputter.abort();
        }
    }

    /// Subscribe to this encoder's output. Always returns a receiver, even
    /// if the encoder has already stopped (in which case the returned
    /// receiver immediately observes `RecvError::Closed`), so callers don't
    /// need to special-case a narrow shutdown race.
    pub fn subscribe(&self) -> broadcast::Receiver<Bytes> {
        self.subscriber_count.fetch_add(1, Ordering::SeqCst);
        let guard = self.tx_slot.lock().unwrap();
        match guard.as_ref() {
            Some(tx) => tx.subscribe(),
            None => {
                // Already stopped: hand back a receiver on a throwaway
                // channel whose sender is dropped immediately below, so the
                // very next `recv()` returns `Closed`.
                let (tx, rx) = broadcast::channel(1);
                drop(tx);
                rx
            }
        }
    }

    /// Unsubscribe from this encoder's output.
    ///
    /// Uses `fetch_update` so the decrement is skipped atomically when the
    /// count is already 0 (mirrors `SharedTuner::unsubscribe`).
    pub fn unsubscribe(&self) {
        match self.subscriber_count.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            if n > 0 {
                Some(n - 1)
            } else {
                None
            }
        }) {
            Ok(_) => {}
            Err(0) => warn!(
                "[SharedEncoder {:?}] unsubscribe() called when subscriber_count is already 0; ignoring",
                self.key.channel_key
            ),
            Err(_) => unreachable!(),
        }
    }

    pub fn subscriber_count(&self) -> u32 {
        self.subscriber_count.load(Ordering::SeqCst)
    }

    pub fn has_subscribers(&self) -> bool {
        self.subscriber_count() > 0
    }

    pub fn is_running(&self) -> bool {
        self.is_running.load(Ordering::Acquire)
    }
}

impl std::fmt::Debug for SharedEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedEncoder")
            .field("key", &self.key)
            .field("subscribers", &self.subscriber_count())
            .field("is_running", &self.is_running())
            .finish_non_exhaustive()
    }
}

/// Pending idle-close task handle (mirrors `TunerPool::IdleHandle`).
struct IdleHandle {
    cancel_tx: oneshot::Sender<()>,
}

/// Pool of shared encoders, admission-controlled by `max_concurrent_encoders`.
///
/// Mirrors `TunerPool`'s structure: a map keyed by [`EncodeKey`], plus
/// idle-close scheduling for encoders that have lost all subscribers.
/// Concurrency admission uses a real `tokio::sync::Semaphore` so that the
/// slot is naturally released (via `OwnedSemaphorePermit`'s `Drop`) whenever
/// a `SharedEncoder` is finally dropped — no manual bookkeeping needed.
pub struct EncoderPool {
    encoders: RwLock<HashMap<EncodeKey, Arc<SharedEncoder>>>,
    idle_tasks: tokio::sync::Mutex<HashMap<EncodeKey, IdleHandle>>,
    semaphore: Arc<Semaphore>,
    configured_max: AtomicUsize,
    idle_grace: Duration,
    /// Count of `get_or_create` calls that returned `Saturated`. Exposed for
    /// logging/diagnostics (STREAMING_DESIGN.md §5.2 (B): "飽和カウンタ").
    saturated_count: AtomicU64,
}

impl EncoderPool {
    /// Create a new pool admitting at most `max_concurrent` simultaneously
    /// running encoders.
    pub fn new(max_concurrent: usize) -> Self {
        Self::new_with_idle_grace(max_concurrent, DEFAULT_IDLE_GRACE)
    }

    /// Same as [`Self::new`] but with an injectable idle-close grace period
    /// (used by tests to avoid multi-second sleeps).
    pub fn new_with_idle_grace(max_concurrent: usize, idle_grace: Duration) -> Self {
        let max_concurrent = max_concurrent.max(1);
        Self {
            encoders: RwLock::new(HashMap::new()),
            idle_tasks: tokio::sync::Mutex::new(HashMap::new()),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            configured_max: AtomicUsize::new(max_concurrent),
            idle_grace,
            saturated_count: AtomicU64::new(0),
        }
    }

    /// Adjust the number of concurrently-admitted encoders. Only affects
    /// *future* `get_or_create` calls — already-running encoders are left
    /// alone (STREAMING_DESIGN.md §5.2).
    pub async fn set_max_concurrent(&self, new_max: usize) {
        let new_max = new_max.max(1);
        let old = self.configured_max.swap(new_max, Ordering::AcqRel);
        match new_max.cmp(&old) {
            std::cmp::Ordering::Greater => self.semaphore.add_permits(new_max - old),
            std::cmp::Ordering::Less => {
                let _ = self.semaphore.forget_permits(old - new_max);
            }
            std::cmp::Ordering::Equal => {}
        }
    }

    /// Count of `get_or_create` calls that hit `Saturated`.
    pub fn saturated_count(&self) -> u64 {
        self.saturated_count.load(Ordering::Relaxed)
    }

    /// Number of currently tracked encoders (running or pending idle-close).
    pub async fn count(&self) -> usize {
        self.encoders.read().await.len()
    }

    /// Cancel a pending idle-close timer for `key`, if any.
    pub async fn cancel_idle_close(&self, key: &EncodeKey) {
        let mut idle_tasks = self.idle_tasks.lock().await;
        if let Some(handle) = idle_tasks.remove(key) {
            let _ = handle.cancel_tx.send(());
        }
    }

    /// Get an existing running encoder for `key`, or spawn a new one.
    ///
    /// Joining an existing encoder never touches the semaphore — that's the
    /// whole point of sharing (STREAMING_DESIGN.md §5.2 (A)). Only spawning
    /// a genuinely new encoder consumes one of `max_concurrent_encoders`
    /// slots; if none are free, returns [`EncoderPoolError::Saturated`].
    pub async fn get_or_create(
        &self,
        key: EncodeKey,
        tuner: Arc<SharedTuner>,
        cfg: EncoderRuntimeConfig,
    ) -> Result<Arc<SharedEncoder>, EncoderPoolError> {
        // Fast path: an existing, still-running encoder for this key.
        {
            let encoders = self.encoders.read().await;
            if let Some(enc) = encoders.get(&key) {
                if enc.is_running() {
                    let enc = Arc::clone(enc);
                    drop(encoders);
                    self.cancel_idle_close(&key).await;
                    debug!("[EncoderPool] joining existing encoder for {:?}", key);
                    return Ok(enc);
                }
            }
        }

        let mut encoders = self.encoders.write().await;

        // Re-check under the write lock (another task may have created it,
        // or the stale entry may have already been evicted).
        if let Some(enc) = encoders.get(&key) {
            if enc.is_running() {
                let enc = Arc::clone(enc);
                drop(encoders);
                self.cancel_idle_close(&key).await;
                return Ok(enc);
            }
            warn!("[EncoderPool] evicting stale (stopped) encoder for {:?}", key);
            encoders.remove(&key);
        }

        let permit = match Arc::clone(&self.semaphore).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                self.saturated_count.fetch_add(1, Ordering::Relaxed);
                warn!(
                    "[EncoderPool] saturated (max_concurrent_encoders={}), rejecting new encoder for {:?}",
                    self.configured_max.load(Ordering::Acquire),
                    key
                );
                return Err(EncoderPoolError::Saturated);
            }
        };

        match SharedEncoder::spawn(key.clone(), tuner, cfg, permit).await {
            Ok(enc) => {
                encoders.insert(key, Arc::clone(&enc));
                Ok(enc)
            }
            Err(e) => Err(EncoderPoolError::SpawnFailed(e.to_string())),
        }
    }

    /// Release a session's subscription to `encoder`. If this was the last
    /// subscriber, schedules an idle-close after the configured grace
    /// period (same pattern as `TunerPool::schedule_idle_close`).
    pub async fn release(self: &Arc<Self>, key: &EncodeKey, encoder: &Arc<SharedEncoder>) {
        encoder.unsubscribe();
        if !encoder.has_subscribers() {
            self.schedule_idle_close(key.clone(), Arc::clone(encoder)).await;
        }
    }

    /// Schedule a delayed stop for `encoder` if it remains subscriber-less
    /// for `idle_grace`.
    pub async fn schedule_idle_close(self: &Arc<Self>, key: EncodeKey, encoder: Arc<SharedEncoder>) {
        {
            let idle_tasks = self.idle_tasks.lock().await;
            if idle_tasks.contains_key(&key) {
                return;
            }
        }

        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        {
            let mut idle_tasks = self.idle_tasks.lock().await;
            idle_tasks.insert(key.clone(), IdleHandle { cancel_tx });
        }

        let pool = Arc::downgrade(self);
        let grace = self.idle_grace;
        tokio::spawn(async move {
            let sleep = tokio::time::sleep(grace);
            tokio::pin!(sleep);

            tokio::select! {
                _ = &mut sleep => {
                    if let Some(pool) = pool.upgrade() {
                        if !encoder.has_subscribers() {
                            info!("[EncoderPool] idle timeout reached, stopping encoder for {:?}", key);
                            encoder.finish("idle timeout").await;
                            let mut encoders = pool.encoders.write().await;
                            if let Some(current) = encoders.get(&key) {
                                if Arc::ptr_eq(current, &encoder) {
                                    encoders.remove(&key);
                                }
                            }
                        }
                        let mut idle_tasks = pool.idle_tasks.lock().await;
                        idle_tasks.remove(&key);
                    }
                }
                _ = cancel_rx => {
                    if let Some(pool) = pool.upgrade() {
                        let mut idle_tasks = pool.idle_tasks.lock().await;
                        idle_tasks.remove(&key);
                    }
                }
            }
        });
    }
}

impl Default for EncoderPool {
    fn default() -> Self {
        Self::new(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_channel_key(ch: u32) -> ChannelKey {
        ChannelKey::space_channel("/dev/test-tuner", 0, ch)
    }

    // ---- EncodeKey ----

    #[test]
    fn encode_key_sid_order_is_normalized() {
        let a = EncodeKey::new(test_channel_key(1), vec![30, 10, 20], 1);
        let b = EncodeKey::new(test_channel_key(1), vec![10, 20, 30], 1);
        assert_eq!(a, b);
        assert_eq!(a.sids, vec![10, 20, 30]);

        let mut map = HashMap::new();
        map.insert(a, "encoder-a");
        assert_eq!(map.get(&b), Some(&"encoder-a"));
    }

    #[test]
    fn encode_key_dedups_sids() {
        let key = EncodeKey::new(test_channel_key(1), vec![10, 10, 20], 1);
        assert_eq!(key.sids, vec![10, 20]);
    }

    #[test]
    fn encode_key_differs_by_generation() {
        let a = EncodeKey::new(test_channel_key(1), vec![10], 1);
        let b = EncodeKey::new(test_channel_key(1), vec![10], 2);
        assert_ne!(a, b);
    }

    #[test]
    fn encode_key_differs_by_channel() {
        let a = EncodeKey::new(test_channel_key(1), vec![10], 1);
        let b = EncodeKey::new(test_channel_key(2), vec![10], 1);
        assert_ne!(a, b);
    }

    fn runtime_config(
        command_path: &str,
        arguments: &str,
        preprocessor_path: &str,
        preprocessor_arguments: &str,
    ) -> EncoderRuntimeConfig {
        EncoderRuntimeConfig {
            command_path: command_path.to_string(),
            arguments: arguments.to_string(),
            read_timeout_ms: 10_000,
            preprocessor_path: preprocessor_path.to_string(),
            preprocessor_arguments: preprocessor_arguments.to_string(),
        }
    }

    #[test]
    fn config_generation_is_stable_and_sensitive() {
        let g1 = config_generation(&runtime_config("tsreplace", "-a -b", "", ""));
        let g2 = config_generation(&runtime_config("tsreplace", "-a -b", "", ""));
        assert_eq!(g1, g2);

        let g3 = config_generation(&runtime_config("tsreplace", "-a -b -c", "", ""));
        assert_ne!(g1, g3);

        // Preprocessor settings must also bump the generation.
        let g4 = config_generation(&runtime_config("tsreplace", "-a -b", "tsreadex", ""));
        assert_ne!(g1, g4);
        let g5 = config_generation(&runtime_config("tsreplace", "-a -b", "tsreadex", "-n {SID} -"));
        assert_ne!(g4, g5);
    }

    // ---- {SID} substitution & spawn planning (pure, no processes) ----

    #[test]
    fn substitute_sid_replaces_every_occurrence() {
        assert_eq!(substitute_sid("-n {SID} --tag {SID} -", 1032), "-n 1032 --tag 1032 -");
        assert_eq!(substitute_sid("no placeholder", 1), "no placeholder");
    }

    #[test]
    fn plan_placeholder_single_sid_substitutes_and_skips_service_injection() {
        let cfg = runtime_config("QSVEncC", "-i - --service-hint {SID} -o -", "", "");
        let plan = plan_spawn(&cfg, &[1032]).unwrap();
        match plan {
            SpawnPlan::Single { enc_args } => {
                assert_eq!(enc_args, vec!["-i", "-", "--service-hint", "1032", "-o", "-"]);
                assert!(!enc_args.iter().any(|a| a == "--service"), "no auto-injection with {{SID}}");
            }
            other => panic!("expected Single, got {:?}", other),
        }
    }

    #[test]
    fn plan_placeholder_with_preprocessor_builds_two_stage() {
        let cfg = runtime_config(
            "QSVEncC",
            "--avhw -i - -o -",
            "tsreadex",
            "-x 18 -n {SID} -",
        );
        let plan = plan_spawn(&cfg, &[101]).unwrap();
        match plan {
            SpawnPlan::TwoStage { pre_args, enc_args } => {
                assert_eq!(pre_args, vec!["-x", "18", "-n", "101", "-"]);
                assert_eq!(enc_args, vec!["--avhw", "-i", "-", "-o", "-"]);
            }
            other => panic!("expected TwoStage, got {:?}", other),
        }
    }

    #[test]
    fn plan_placeholder_requires_exactly_one_sid() {
        let cfg = runtime_config("enc", "-n {SID}", "", "");
        assert!(plan_spawn(&cfg, &[]).is_err(), "no SID resolved -> error");
        assert!(plan_spawn(&cfg, &[1, 2]).is_err(), "multiple SIDs -> error");
    }

    #[test]
    fn plan_without_placeholder_keeps_legacy_service_injection() {
        // Single SID, no --service in template -> auto-injected.
        let cfg = runtime_config("tsreplace", "-i - -o -", "", "");
        match plan_spawn(&cfg, &[500]).unwrap() {
            SpawnPlan::Single { enc_args } => {
                assert!(enc_args.iter().any(|a| a == "--service"));
                assert!(enc_args.iter().any(|a| a == "500"));
            }
            other => panic!("expected Single, got {:?}", other),
        }

        // Template already has --service -> used verbatim.
        let cfg = runtime_config("tsreplace", "--service 42 -i - -o -", "", "");
        match plan_spawn(&cfg, &[500]).unwrap() {
            SpawnPlan::Single { enc_args } => {
                assert_eq!(enc_args, vec!["--service", "42", "-i", "-", "-o", "-"]);
            }
            other => panic!("expected Single, got {:?}", other),
        }

        // 2+ SIDs -> per-SID chain (unchanged legacy behavior).
        let cfg = runtime_config("tsreplace", "-i - -o -", "", "");
        assert_eq!(plan_spawn(&cfg, &[1, 2]).unwrap(), SpawnPlan::Chain);
    }

    #[test]
    fn plan_preprocessor_without_placeholder_wraps_single_stage_cases() {
        // Empty SIDs: verbatim args, two stages.
        let cfg = runtime_config("enc", "-i - -o -", "pre", "-a -b");
        match plan_spawn(&cfg, &[]).unwrap() {
            SpawnPlan::TwoStage { pre_args, enc_args } => {
                assert_eq!(pre_args, vec!["-a", "-b"]);
                assert_eq!(enc_args, vec!["-i", "-", "-o", "-"]);
            }
            other => panic!("expected TwoStage, got {:?}", other),
        }

        // Multi-SID chain + preprocessor is rejected.
        let cfg = runtime_config("enc", "-i - -o -", "pre", "-a");
        assert!(plan_spawn(&cfg, &[1, 2]).is_err());
    }

    #[test]
    fn plan_blank_preprocessor_path_means_single_stage() {
        let cfg = runtime_config("enc", "-i - -o -", "   ", "-ignored");
        assert!(matches!(plan_spawn(&cfg, &[]).unwrap(), SpawnPlan::Single { .. }));
    }

    // ---- EncoderPool (real `cat` subprocess: pure stdin->stdout passthrough) ----
    //
    // Using `cat` with no SIDs and no extra arguments takes the
    // "single-process, no --service injection" branch in `SharedEncoder::spawn`,
    // giving byte-for-byte passthrough — perfect for verifying the plumbing
    // without depending on tsreplace/QSVEncC being installed.

    fn cat_config() -> EncoderRuntimeConfig {
        EncoderRuntimeConfig {
            command_path: "cat".to_string(),
            arguments: String::new(),
            read_timeout_ms: 2_000,
            preprocessor_path: String::new(),
            preprocessor_arguments: String::new(),
        }
    }

    fn test_tuner(ch: u32) -> Arc<SharedTuner> {
        SharedTuner::new(test_channel_key(ch), 2)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn get_or_create_merges_identical_key() {
        let pool = EncoderPool::new(2);
        let tuner = test_tuner(1);
        let key = EncodeKey::new(tuner.key.clone(), vec![], 1);

        let enc_a = pool
            .get_or_create(key.clone(), Arc::clone(&tuner), cat_config())
            .await
            .expect("first get_or_create should succeed");
        let enc_b = pool
            .get_or_create(key.clone(), Arc::clone(&tuner), cat_config())
            .await
            .expect("second get_or_create should join, not fail");

        assert!(Arc::ptr_eq(&enc_a, &enc_b), "expected the same shared encoder instance");
        assert_eq!(pool.count().await, 1);
        assert_eq!(pool.saturated_count(), 0);

        enc_a.finish("test cleanup").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn get_or_create_returns_saturated_when_full() {
        let pool = EncoderPool::new(1);
        let tuner_a = test_tuner(1);
        let tuner_b = test_tuner(2);
        let key_a = EncodeKey::new(tuner_a.key.clone(), vec![], 1);
        // Different SID set => different key, so this must NOT be able to join key_a.
        let key_b = EncodeKey::new(tuner_b.key.clone(), vec![99], 1);

        let enc_a = pool
            .get_or_create(key_a, Arc::clone(&tuner_a), cat_config())
            .await
            .expect("first encoder should be admitted");

        let err = pool
            .get_or_create(key_b, Arc::clone(&tuner_b), cat_config())
            .await
            .expect_err("second, different-key encoder should be rejected when pool is full");
        assert!(matches!(err, EncoderPoolError::Saturated));
        assert_eq!(pool.saturated_count(), 1);

        enc_a.finish("test cleanup").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cat_passthrough_broadcasts_input_unchanged() {
        let pool = EncoderPool::new(2);
        let tuner = test_tuner(1);
        let key = EncodeKey::new(tuner.key.clone(), vec![], 1);

        let encoder = pool
            .get_or_create(key, Arc::clone(&tuner), cat_config())
            .await
            .expect("get_or_create should succeed");
        let mut out_rx = encoder.subscribe();

        let payload = Bytes::from_static(b"hello-shared-encoder-pool");
        tuner.test_broadcast(payload.clone());

        let received = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("timed out waiting for cat to echo input")
            .expect("broadcast recv should succeed");

        assert_eq!(received, payload);

        encoder.finish("test cleanup").await;
    }

    /// Locate a `cat.exe` for Windows plumbing tests (ships with Git for
    /// Windows). `findstr`/`more` are unsuitable: they fully buffer their
    /// output while stdin (a pipe) is still open, so a streaming
    /// passthrough never emits anything.
    #[cfg(windows)]
    fn find_windows_cat() -> Option<String> {
        [
            r"C:\Program Files\Git\usr\bin\cat.exe",
            r"C:\Program Files (x86)\Git\usr\bin\cat.exe",
        ]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|s| s.to_string())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn two_stage_cat_passthrough_broadcasts_input_unchanged_windows() {
        // Windows equivalent of the unix cat|cat test, using Git for
        // Windows' cat.exe (byte-for-byte, unbuffered passthrough). Skipped
        // when no cat.exe is available on the machine.
        let Some(cat) = find_windows_cat() else {
            eprintln!("skipping: no cat.exe found (Git for Windows not installed)");
            return;
        };

        let pool = EncoderPool::new(2);
        let tuner = test_tuner(1);
        let key = EncodeKey::new(tuner.key.clone(), vec![], 1);
        let cfg = EncoderRuntimeConfig {
            command_path: cat.clone(),
            arguments: String::new(),
            read_timeout_ms: 5_000,
            preprocessor_path: cat,
            preprocessor_arguments: String::new(),
        };

        let encoder = pool
            .get_or_create(key, Arc::clone(&tuner), cfg)
            .await
            .expect("get_or_create should succeed");
        let mut out_rx = encoder.subscribe();

        let payload = Bytes::from_static(b"hello-two-stage-pipeline");
        tuner.test_broadcast(payload.clone());

        let received = tokio::time::timeout(Duration::from_secs(10), out_rx.recv())
            .await
            .expect("timed out waiting for cat|cat to echo input")
            .expect("broadcast recv should succeed");

        assert_eq!(received, payload);

        encoder.finish("test cleanup").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn two_stage_cat_passthrough_broadcasts_input_unchanged() {
        // preprocessor=cat, encoder=cat: byte-for-byte passthrough through a
        // real two-process OS pipe chain — verifies the TwoStage plumbing.
        let pool = EncoderPool::new(2);
        let tuner = test_tuner(1);
        let key = EncodeKey::new(tuner.key.clone(), vec![], 1);
        let cfg = EncoderRuntimeConfig {
            command_path: "cat".to_string(),
            arguments: String::new(),
            read_timeout_ms: 2_000,
            preprocessor_path: "cat".to_string(),
            preprocessor_arguments: String::new(),
        };

        let encoder = pool
            .get_or_create(key, Arc::clone(&tuner), cfg)
            .await
            .expect("get_or_create should succeed");
        let mut out_rx = encoder.subscribe();

        let payload = Bytes::from_static(b"hello-two-stage-pipeline");
        tuner.test_broadcast(payload.clone());

        let received = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .expect("timed out waiting for cat|cat to echo input")
            .expect("broadcast recv should succeed");

        assert_eq!(received, payload);

        encoder.finish("test cleanup").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn idle_release_stops_encoder_after_grace_period() {
        let pool = Arc::new(EncoderPool::new_with_idle_grace(2, Duration::from_millis(50)));
        let tuner = test_tuner(1);
        let key = EncodeKey::new(tuner.key.clone(), vec![], 1);

        let encoder = pool
            .get_or_create(key.clone(), Arc::clone(&tuner), cat_config())
            .await
            .expect("get_or_create should succeed");
        let _sub = encoder.subscribe();
        assert!(encoder.has_subscribers());

        pool.release(&key, &encoder).await;
        assert!(!encoder.has_subscribers());
        // Not stopped immediately — idle grace period must elapse first.
        assert!(encoder.is_running());

        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(!encoder.is_running(), "encoder should have been stopped after the idle grace period");
        assert_eq!(pool.count().await, 0, "stale entry should have been evicted from the pool");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn idle_release_cancelled_by_rejoin() {
        let pool = Arc::new(EncoderPool::new_with_idle_grace(2, Duration::from_millis(100)));
        let tuner = test_tuner(1);
        let key = EncodeKey::new(tuner.key.clone(), vec![], 1);

        let encoder = pool
            .get_or_create(key.clone(), Arc::clone(&tuner), cat_config())
            .await
            .expect("get_or_create should succeed");
        let _sub = encoder.subscribe();
        pool.release(&key, &encoder).await; // subscriber_count -> 0, idle-close scheduled

        // Rejoin before the grace period elapses.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let enc2 = pool
            .get_or_create(key.clone(), Arc::clone(&tuner), cat_config())
            .await
            .expect("rejoin should succeed");
        assert!(Arc::ptr_eq(&encoder, &enc2));
        let _sub2 = enc2.subscribe();

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(encoder.is_running(), "encoder should still be running: idle-close was cancelled by the rejoin");

        encoder.finish("test cleanup").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watchdog_kills_stalled_chain() {
        // `sleep` never reads stdin nor writes stdout, so once input starts
        // flowing the watchdog should detect the output stall and kill it
        // well before `sleep`'s own (long) timer would exit it naturally.
        let pool = EncoderPool::new(2);
        let tuner = test_tuner(1);
        let key = EncodeKey::new(tuner.key.clone(), vec![], 1);
        let cfg = EncoderRuntimeConfig {
            command_path: "sleep".to_string(),
            arguments: "30".to_string(),
            read_timeout_ms: 100,
            preprocessor_path: String::new(),
            preprocessor_arguments: String::new(),
        };

        let encoder = pool
            .get_or_create(key, Arc::clone(&tuner), cfg)
            .await
            .expect("get_or_create should succeed");
        let mut out_rx = encoder.subscribe();

        // Keep "input flowing" so the watchdog's flowing-input condition is met.
        for _ in 0..5 {
            tuner.test_broadcast(Bytes::from_static(b"x"));
            tokio::time::sleep(Duration::from_millis(40)).await;
        }

        let closed = tokio::time::timeout(Duration::from_secs(3), out_rx.recv()).await;
        match closed {
            Ok(Err(broadcast::error::RecvError::Closed)) => {}
            other => panic!("expected watchdog to close the broadcast channel, got {:?}", other),
        }
        assert!(!encoder.is_running());
    }
}
