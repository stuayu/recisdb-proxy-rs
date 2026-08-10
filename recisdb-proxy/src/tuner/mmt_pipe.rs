//! MMT/TLV → MPEG-2 TS conversion stage for 4K tuners.
//!
//! # Why the converter runs here and not as a BonDriver wrapper
//!
//! dantto4k ships `BonDriver_dantto4k.dll`, which loads the real tuner's
//! BonDriver and converts inline — on paper that needs no code here at all.
//! In practice that arrangement was observed to deliver nothing while MMT/TLV
//! was demonstrably flowing, so the raw stream is read by this process (the
//! tuner's own BonDriver is opened normally) and only the *conversion* is
//! delegated, to the `dantto4k` CLI over a pipe:
//!
//! ```text
//! 4K tuner ─BonDriver─▶ SharedTuner reader ─stdin─▶ dantto4k ─stdout─▶ TS ─▶ broadcast
//!                       (raw MMT/TLV bytes)                            (everything
//!                                                                       downstream)
//! ```
//!
//! The conversion sits *before* the broadcast on purpose: one conversion is
//! shared by every subscriber, and the TS analyzer, EPG/logo collectors and
//! all sessions get a stream in the only format they understand.
//!
//! # Descrambling is normally the converter's job
//!
//! We read straight off the tuner's BonDriver, which does not descramble, so
//! the stream reaches `dantto4k` still scrambled and it has to do the ACAS
//! work itself — via a smart card reader (`--smartCardReaderName`) or a
//! CasProxyServer (`--casProxyServer`).
//!
//! `--frontend-descrambled` covers the other case, where something upstream
//! already descrambled and only the remux is wanted. It is exposed as
//! [`MmtConverterConfig::frontend_descrambled`] but off by default, because it
//! does not apply to the plain "raw tuner" path this module exists for.
//!
//! **A failed descramble is silent**: the converter still exits successfully
//! and still writes a full-size TS, just an undecipherable one. That is
//! exactly the "converts fine, plays nothing" symptom, so this module watches
//! stderr and surfaces it instead of quietly publishing ciphertext.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use log::{error, info, warn};

/// Bounded backlog between the tuner read loop and the converter's stdin.
///
/// The read loop must never block on the converter: a stalled child (waiting
/// on a card, say) would otherwise stall the tuner thread and overflow the
/// driver's own buffer. Chunks that do not fit are dropped and counted — a
/// visible gap is better than a wedged tuner.
const WRITE_BACKLOG_CHUNKS: usize = 64;

/// Size of each read from the converter's stdout.
const STDOUT_CHUNK: usize = 256 * 1024;

/// Configuration for the external converter.
///
/// `command_path` is TOML-only for the same reason as `[tsreplace]`: the
/// server executes it directly, so letting the dashboard set it would be a
/// remote code execution vector.
#[derive(Debug, Clone, Default)]
pub struct MmtConverterConfig {
    /// Path to the `dantto4k` executable.
    pub command_path: String,
    /// `--casProxyServer <addr>`: descramble through a CasProxyServer.
    pub cas_proxy_server: Option<String>,
    /// `--smartCardReaderName <name>`: descramble with a local PC/SC reader.
    pub smart_card_reader_name: Option<String>,
    /// `--frontend-descrambled`: the input is already descrambled, so only
    /// remux MMT/TLV to TS and do not touch a card at all.
    ///
    /// Off by default: reading raw off a tuner's BonDriver means nothing has
    /// descrambled yet. Turning this on when the stream *is* still scrambled
    /// produces a full-size, unplayable TS with no warning at all — worse than
    /// the missing-reader case, which at least says so on stderr.
    pub frontend_descrambled: bool,
    /// Extra arguments appended verbatim, for options this struct does not
    /// model yet (`--disableADTSConversion`, `--customWinscardDLL`, …).
    pub extra_args: Vec<String>,
}

/// Build the converter's argument list.
///
/// `-` `-` puts it in pipe mode (stdin → stdout). Progress and statistics are
/// suppressed: both go to the console and would otherwise be written for every
/// reader start, drowning the messages that matter.
pub fn build_args(config: &MmtConverterConfig) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    if let Some(addr) = config
        .cas_proxy_server
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        args.push("--casProxyServer".to_string());
        args.push(addr.to_string());
    }
    if let Some(name) = config
        .smart_card_reader_name
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        args.push("--smartCardReaderName".to_string());
        args.push(name.to_string());
    }

    if config.frontend_descrambled {
        args.push("--frontend-descrambled".to_string());
    }

    args.push("--no-progress".to_string());
    args.push("--no-stats".to_string());

    args.extend(config.extra_args.iter().cloned());

    // Input and output, in that order, must come last.
    args.push("-".to_string());
    args.push("-".to_string());
    args
}

/// A problem recognised on the converter's stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConverterProblem {
    /// No way to descramble: no reader, or the CasProxyServer is unreachable
    /// and it fell back to local PC/SC.
    ///
    /// The converter keeps going and emits a full-size but still-encrypted TS,
    /// so this has to be reported explicitly or it looks like a tuner fault.
    NoDescrambler,
}

/// Classify one line of converter stderr.
///
/// Matching is substring-based and case-insensitive: the exact wording is not
/// part of any contract, so an unrecognised line is simply passed through to
/// the log rather than treated as fatal.
pub fn classify_stderr_line(line: &str) -> Option<ConverterProblem> {
    let lower = line.to_ascii_lowercase();
    if lower.contains("no smart card readers are available")
        || lower.contains("smart card reader not found")
    {
        return Some(ConverterProblem::NoDescrambler);
    }
    None
}

/// Live status of one converter process.
#[derive(Debug, Default)]
pub struct ConverterStatus {
    /// Number of stderr lines that indicated descrambling is not working.
    descramble_errors: AtomicU64,
    /// Chunks dropped because the converter could not keep up.
    dropped_chunks: AtomicU64,
    /// The most recent unrecognised stderr line, for diagnostics.
    last_message: Mutex<Option<String>>,
}

impl ConverterStatus {
    pub fn descramble_failing(&self) -> bool {
        self.descramble_errors.load(Ordering::Relaxed) > 0
    }

    pub fn descramble_error_count(&self) -> u64 {
        self.descramble_errors.load(Ordering::Relaxed)
    }

    pub fn dropped_chunks(&self) -> u64 {
        self.dropped_chunks.load(Ordering::Relaxed)
    }

    pub fn last_message(&self) -> Option<String> {
        self.last_message
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn record_line(&self, line: &str) {
        match classify_stderr_line(line) {
            Some(ConverterProblem::NoDescrambler) => {
                let n = self.descramble_errors.fetch_add(1, Ordering::Relaxed) + 1;
                // Loud once, then rate-limited: the converter repeats this per
                // ECM and would otherwise bury everything else.
                if n == 1 || n % 100 == 0 {
                    error!(
                        "[MmtPipe] 4K descrambling is not working ({}回目): {}. \
                         出力TSは復号されていない。CasProxyServerの起動と \
                         --smartCardReaderName / --casProxyServer の設定を確認",
                        n, line
                    );
                }
            }
            None => {
                *self
                    .last_message
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = Some(line.to_string());
            }
        }
    }
}

/// A running `dantto4k` process wired up as a byte-stream transform.
///
/// Shaped like [`crate::tuner::b25_pipe::B25Pipe`] on purpose — `push` takes
/// whatever the driver handed over and returns whatever conversion has
/// produced so far — so the reader loop treats both stages the same way.
pub struct MmtPipe {
    child: Child,
    /// Raw MMT/TLV to the converter. Bounded; see [`WRITE_BACKLOG_CHUNKS`].
    to_child: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    /// Converted TS from the converter.
    from_child: std::sync::mpsc::Receiver<Vec<u8>>,
    status: Arc<ConverterStatus>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl MmtPipe {
    /// Spawn the converter.
    pub fn new(config: &MmtConverterConfig) -> std::io::Result<Self> {
        if config.command_path.trim().is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "MMT/TLV converter path is not configured ([mmttlv] command_path)",
            ));
        }

        Self::spawn(&config.command_path, &build_args(config))
    }

    /// Spawn `command_path` with an explicit argument list.
    ///
    /// Split out from [`Self::new`] so the process plumbing can be exercised
    /// against a stand-in command whose options differ from the converter's.
    fn spawn(command_path: &str, args: &[String]) -> std::io::Result<Self> {
        info!("[MmtPipe] Starting converter: {} {}", command_path, args.join(" "));

        let mut child = Command::new(command_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "converter stdin unavailable")
        })?;
        let mut stdout = child.stdout.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "converter stdout unavailable")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "converter stderr unavailable")
        })?;

        let status = Arc::new(ConverterStatus::default());
        let (to_child, write_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(WRITE_BACKLOG_CHUNKS);
        let (read_tx, from_child) = std::sync::mpsc::channel::<Vec<u8>>();

        let mut workers = Vec::with_capacity(3);

        // stdin feeder.
        workers.push(std::thread::spawn(move || {
            while let Ok(chunk) = write_rx.recv() {
                if let Err(e) = stdin.write_all(&chunk) {
                    warn!("[MmtPipe] Converter stdin write failed: {}", e);
                    break;
                }
            }
            // Closing stdin lets the converter flush and exit.
            drop(stdin);
        }));

        // stdout collector.
        workers.push(std::thread::spawn(move || {
            let mut buf = vec![0u8; STDOUT_CHUNK];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if read_tx.send(buf[..n].to_vec()).is_err() {
                            break; // pipe dropped
                        }
                    }
                    Err(e) => {
                        warn!("[MmtPipe] Converter stdout read failed: {}", e);
                        break;
                    }
                }
            }
        }));

        // stderr watcher — this is what turns a silent descramble failure into
        // something an operator can see.
        let status_for_stderr = Arc::clone(&status);
        workers.push(std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        let line = line.trim();
                        if !line.is_empty() {
                            status_for_stderr.record_line(line);
                        }
                    }
                    Err(_) => break,
                }
            }
        }));

        Ok(Self {
            child,
            to_child: Some(to_child),
            from_child,
            status,
            workers,
        })
    }

    pub fn status(&self) -> &Arc<ConverterStatus> {
        &self.status
    }

    /// Feed raw MMT/TLV and collect whatever TS is ready.
    ///
    /// Never blocks on the converter. The returned buffer is empty whenever
    /// conversion has not produced anything yet, which is normal at start-up.
    pub fn push(&mut self, input: &[u8]) -> std::io::Result<Vec<u8>> {
        if !input.is_empty() {
            let Some(tx) = self.to_child.as_ref() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "converter input already closed",
                ));
            };
            match tx.try_send(input.to_vec()) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(_)) => {
                    let n = self
                        .status
                        .dropped_chunks
                        .fetch_add(1, Ordering::Relaxed)
                        + 1;
                    if n == 1 || n % 100 == 0 {
                        warn!(
                            "[MmtPipe] Converter cannot keep up; dropped {} chunk(s) of MMT/TLV",
                            n
                        );
                    }
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "converter input thread ended",
                    ));
                }
            }
        }

        let mut out = Vec::new();
        while let Ok(chunk) = self.from_child.try_recv() {
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}

impl Drop for MmtPipe {
    fn drop(&mut self) {
        // Dropping the sender closes stdin, which is how the converter is
        // asked to finish; kill covers the case where it does not.
        self.to_child = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_put_the_pipe_placeholders_last() {
        let args = build_args(&MmtConverterConfig {
            command_path: "dantto4k".to_string(),
            ..Default::default()
        });
        // `input output` are positional and must stay at the end.
        assert_eq!(&args[args.len() - 2..], &["-".to_string(), "-".to_string()]);
        assert!(args.contains(&"--no-progress".to_string()));
        assert!(args.contains(&"--no-stats".to_string()));
    }

    #[test]
    fn cas_options_are_passed_through() {
        let args = build_args(&MmtConverterConfig {
            command_path: "dantto4k".to_string(),
            cas_proxy_server: Some("127.0.0.1:24000".to_string()),
            smart_card_reader_name: Some("My Reader 0".to_string()),
            extra_args: vec!["--disableADTSConversion".to_string()],
            ..Default::default()
        });

        let pos = |flag: &str| args.iter().position(|a| a == flag).expect(flag);
        assert_eq!(args[pos("--casProxyServer") + 1], "127.0.0.1:24000");
        assert_eq!(args[pos("--smartCardReaderName") + 1], "My Reader 0");
        assert!(args.contains(&"--disableADTSConversion".to_string()));
        assert_eq!(&args[args.len() - 2..], &["-".to_string(), "-".to_string()]);
    }

    #[test]
    fn frontend_descrambled_is_opt_in() {
        let default_args = build_args(&MmtConverterConfig {
            command_path: "dantto4k".to_string(),
            ..Default::default()
        });
        assert!(
            !default_args.contains(&"--frontend-descrambled".to_string()),
            "reading raw off a tuner means nothing has descrambled yet"
        );

        let args = build_args(&MmtConverterConfig {
            command_path: "dantto4k".to_string(),
            frontend_descrambled: true,
            ..Default::default()
        });
        assert!(args.contains(&"--frontend-descrambled".to_string()));
        assert_eq!(&args[args.len() - 2..], &["-".to_string(), "-".to_string()]);
    }

    #[test]
    fn blank_cas_settings_are_omitted_rather_than_passed_empty() {
        // An empty string in the config file must not become `--casProxyServer ""`,
        // which the converter would reject.
        let args = build_args(&MmtConverterConfig {
            command_path: "dantto4k".to_string(),
            cas_proxy_server: Some("   ".to_string()),
            smart_card_reader_name: Some(String::new()),
            ..Default::default()
        });
        assert!(!args.contains(&"--casProxyServer".to_string()));
        assert!(!args.contains(&"--smartCardReaderName".to_string()));
    }

    /// The converter exits successfully and writes a full-size TS even when it
    /// could not descramble a single packet, so this line is the only warning
    /// anyone gets that the output is ciphertext.
    #[test]
    fn stderr_reports_a_missing_descrambler() {
        assert_eq!(
            classify_stderr_line("No smart card readers are available"),
            Some(ConverterProblem::NoDescrambler)
        );
        // Case and surrounding text must not matter.
        assert_eq!(
            classify_stderr_line("  ERROR: no smart card readers are available  "),
            Some(ConverterProblem::NoDescrambler)
        );
        // Anything else is just logged.
        assert_eq!(classify_stderr_line("[50.0%] 100/200 MiB"), None);
        assert_eq!(classify_stderr_line(""), None);
    }

    #[test]
    fn status_counts_and_latches_descramble_failures() {
        let status = ConverterStatus::default();
        assert!(!status.descramble_failing());

        status.record_line("No smart card readers are available");
        status.record_line("No smart card readers are available");
        assert!(status.descramble_failing());
        assert_eq!(status.descramble_error_count(), 2);

        // Unrecognised lines are kept for diagnostics but are not failures.
        status.record_line("TLV: NullPacket: 578067");
        assert_eq!(status.descramble_error_count(), 2);
        assert_eq!(
            status.last_message().as_deref(),
            Some("TLV: NullPacket: 578067")
        );
    }

    #[test]
    fn a_missing_converter_path_is_rejected_before_spawning() {
        let err = match MmtPipe::new(&MmtConverterConfig::default()) {
            Err(e) => e,
            Ok(_) => panic!("an unconfigured converter path must not spawn anything"),
        };
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// End-to-end through a real child process: `cat` stands in for the
    /// converter, so this exercises the spawn / feed / collect plumbing
    /// without needing dantto4k or a 4K tuner.
    #[cfg(unix)]
    #[test]
    fn bytes_survive_a_round_trip_through_the_child() {
        // `cat -` reads stdin and writes stdout, which is the same contract
        // the converter is driven under. Its own option set differs, so this
        // goes through `spawn` rather than `build_args`.
        let mut pipe =
            MmtPipe::spawn("/bin/cat", &["-".to_string()]).expect("spawn cat");

        let payload = vec![0x7Fu8; 4096];
        pipe.push(&payload).expect("push");

        // The child echoes asynchronously; give it a moment and drain.
        let mut received = Vec::new();
        for _ in 0..50 {
            if received.len() >= payload.len() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            received.extend(pipe.push(&[]).expect("drain"));
        }

        assert_eq!(received.len(), payload.len(), "all bytes must come back");
        assert_eq!(received, payload);
        assert!(!pipe.status().descramble_failing());
    }
}
