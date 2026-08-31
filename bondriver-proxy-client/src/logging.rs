//! File-based logging for debugging DLL issues.
//!
//! Creates a log file with the same name as the DLL in the same directory.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use once_cell::sync::OnceCell;

/// Logger state: buffered file writer and last flush time.
struct LoggerState {
    /// `None` only while rotating, or if reopening the file failed.
    ///
    /// Rotation has to close this handle before renaming: Windows refuses to
    /// rename a file that is still open, so a rotation that kept the handle
    /// would silently never happen.
    writer: Option<BufWriter<File>>,
    last_flush: Instant,
    /// Bytes written to the current file, tracked so rotation does not need a
    /// `metadata()` syscall per line.
    written: u64,
    /// Where the log lives, needed to rotate.
    path: PathBuf,
}

/// Rotate once the active log reaches this size.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// How many rotated generations to keep (`.log.1` … `.log.N`).
///
/// The client writes next to the DLL on a machine nobody monitors, and at
/// `LogLevel=debug` it emits a line per `WaitTsStream` call — unbounded growth
/// would eventually fill the volume. The server has `retention_days` for the
/// same reason; this is the client's equivalent.
const MAX_LOG_GENERATIONS: u32 = 3;

/// Global log file handle.
static LOG_FILE: OnceCell<Mutex<LoggerState>> = OnceCell::new();

/// Global file log level filter.
/// Encoded as: Off=0, Error=1, Warn=2, Info=3, Debug=4, Trace=5.
static FILE_LOG_LEVEL: AtomicU8 = AtomicU8::new(2); // default: Warn

const FLUSH_INTERVAL_SECS: u64 = 2;

/// Set the file log level.
pub fn set_file_log_level(level: log::LevelFilter) {
    let n = match level {
        log::LevelFilter::Off => 0,
        log::LevelFilter::Error => 1,
        log::LevelFilter::Warn => 2,
        log::LevelFilter::Info => 3,
        log::LevelFilter::Debug => 4,
        log::LevelFilter::Trace => 5,
    };
    FILE_LOG_LEVEL.store(n, Ordering::Relaxed);
}

/// Returns true if the given level should be written to the log file.
pub fn file_level_enabled(level: log::Level) -> bool {
    let filter = FILE_LOG_LEVEL.load(Ordering::Relaxed);
    if filter == 0 {
        return false;
    }
    (level as u8) <= filter
}

/// Get the path to the DLL itself.
#[cfg(windows)]
fn get_dll_path() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    // HMODULE for our DLL
    extern "system" {
        fn GetModuleFileNameW(
            hModule: *mut std::ffi::c_void,
            lpFilename: *mut u16,
            nSize: u32,
        ) -> u32;
    }

    // Get handle to our DLL by using a known symbol address
    let mut path_buf = vec![0u16; 32768];

    // Use null to get the executable path, then try to find our DLL
    // We'll use a different approach - get the path from a known function pointer
    let func_ptr = get_dll_path as *const ();

    // GetModuleHandleEx with GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
    extern "system" {
        fn GetModuleHandleExW(
            dwFlags: u32,
            lpModuleName: *const std::ffi::c_void,
            phModule: *mut *mut std::ffi::c_void,
        ) -> i32;
    }

    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x00000004;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x00000002;

    let mut h_module: *mut std::ffi::c_void = std::ptr::null_mut();

    unsafe {
        let result = GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            func_ptr as *const std::ffi::c_void,
            &mut h_module,
        );

        if result == 0 || h_module.is_null() {
            return None;
        }

        let len = GetModuleFileNameW(h_module, path_buf.as_mut_ptr(), path_buf.len() as u32);
        if len == 0 {
            return None;
        }

        let path_str = OsString::from_wide(&path_buf[..len as usize]);
        Some(PathBuf::from(path_str))
    }
}

#[cfg(not(windows))]
fn get_dll_path() -> Option<PathBuf> {
    None
}

/// Initialize the file logger.
pub fn init_file_logger() -> bool {
    if LOG_FILE.get().is_some() {
        return true; // Already initialized
    }

    let dll_path = match get_dll_path() {
        Some(p) => p,
        None => {
            // Fallback to current directory
            PathBuf::from("BonDriver_NetworkProxy.dll")
        }
    };

    // Change extension to .log
    let log_path = dll_path.with_extension("log");

    match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(file) => {
            let written = file.metadata().map(|m| m.len()).unwrap_or(0);
            let state = LoggerState {
                writer: Some(BufWriter::new(file)),
                last_flush: Instant::now(),
                written,
                path: log_path.clone(),
            };
            let _ = LOG_FILE.set(Mutex::new(state));

            // Write header
            log_message("========================================");
            log_message(&format!("BonDriver_NetworkProxy Log Started"));
            log_message(&format!("Log file: {:?}", log_path));
            log_message(&format!("DLL path: {:?}", dll_path));
            log_message("========================================");
            true
        }
        Err(_) => false,
    }
}

/// Log a message to the file with a specific level (internal).
pub(crate) fn log_with_level(msg: &str, level: log::Level) {
    if let Some(file_mutex) = LOG_FILE.get() {
        if let Ok(mut state) = file_mutex.lock() {
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let line = format!("[{}] {}\n", timestamp, msg);

            // Flush immediately for Warn/Error, or on 2-second interval for others
            let should_flush = match level {
                log::Level::Warn | log::Level::Error => true,
                _ => state.last_flush.elapsed().as_secs() >= FLUSH_INTERVAL_SECS,
            };

            match state.writer.as_mut() {
                Some(writer) => {
                    let _ = writer.write_all(line.as_bytes());
                    if should_flush {
                        let _ = writer.flush();
                    }
                }
                // Only reachable if reopening after a rotation failed.
                None => return,
            }

            state.written += line.len() as u64;
            if should_flush {
                state.last_flush = Instant::now();
            }

            if state.written >= MAX_LOG_BYTES {
                rotate(&mut state);
            }
        }
    }
}

/// Move the active log aside and start a fresh one.
///
/// Best effort: if any step fails the logger keeps writing to whatever handle
/// it still has. Losing log rotation is not worth failing a driver call over.
fn rotate(state: &mut LoggerState) {
    // Close the active handle before touching the file names.
    if let Some(mut writer) = state.writer.take() {
        let _ = writer.flush();
    }

    let path = state.path.clone();
    let generation_path = |n: u32| -> PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".{}", n));
        PathBuf::from(name)
    };

    // Drop the oldest, then shift the rest down: .log.2 -> .log.3, etc.
    let _ = std::fs::remove_file(generation_path(MAX_LOG_GENERATIONS));
    for n in (1..MAX_LOG_GENERATIONS).rev() {
        let _ = std::fs::rename(generation_path(n), generation_path(n + 1));
    }
    let rotated = std::fs::rename(&path, generation_path(1)).is_ok();

    // Reopen unconditionally: the handle is gone either way, so failing to
    // reopen after a failed rename would leave the logger silent.
    state.writer = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
        .map(BufWriter::new);
    state.last_flush = Instant::now();
    state.written = if rotated {
        0
    } else {
        // Rename failed, so the file still holds everything. Keep the count so
        // we retry on the next line rather than spinning on every write.
        std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bondriver-log-{}-{}-{:?}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_state(path: &PathBuf) -> LoggerState {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        LoggerState {
            writer: Some(BufWriter::new(file)),
            last_flush: Instant::now(),
            written: 0,
            path: path.clone(),
        }
    }

    /// The log sits next to the DLL on an unmonitored machine and, at debug
    /// level, grows by a line per WaitTsStream call. Rotation must actually
    /// happen (it renames a file the process itself has open) and must cap the
    /// number of generations kept.
    #[test]
    fn rotation_caps_the_number_of_generations() {
        let dir = scratch_dir("rotate");
        let path = dir.join("BonDriver_NetworkProxy.log");
        let mut state = open_state(&path);

        for i in 0..(MAX_LOG_GENERATIONS + 2) {
            state
                .writer
                .as_mut()
                .unwrap()
                .write_all(format!("generation {i}\n").as_bytes())
                .unwrap();
            rotate(&mut state);
        }

        assert!(path.exists(), "an active log must exist after rotating");
        assert!(state.writer.is_some(), "the sink must be usable again");
        assert_eq!(state.written, 0, "the fresh file starts empty");

        for n in 1..=MAX_LOG_GENERATIONS {
            assert!(
                dir.join(format!("BonDriver_NetworkProxy.log.{n}")).exists(),
                "generation {n} should be kept"
            );
        }
        assert!(
            !dir.join(format!(
                "BonDriver_NetworkProxy.log.{}",
                MAX_LOG_GENERATIONS + 1
            ))
            .exists(),
            "generations beyond the cap must be discarded"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The newest rotated generation must hold what was most recently written,
    /// i.e. the shuffle goes in the right direction.
    #[test]
    fn the_newest_generation_holds_the_most_recent_lines() {
        let dir = scratch_dir("order");
        let path = dir.join("app.log");
        let mut state = open_state(&path);

        state
            .writer
            .as_mut()
            .unwrap()
            .write_all(b"older\n")
            .unwrap();
        rotate(&mut state);
        state
            .writer
            .as_mut()
            .unwrap()
            .write_all(b"newer\n")
            .unwrap();
        rotate(&mut state);

        let gen1 = std::fs::read_to_string(dir.join("app.log.1")).unwrap();
        let gen2 = std::fs::read_to_string(dir.join("app.log.2")).unwrap();
        assert!(gen1.contains("newer"), ".1 must be the most recent");
        assert!(gen2.contains("older"), ".2 must be the previous one");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Log a message to the file (uses Info level by default).
pub fn log_message(msg: &str) {
    log_with_level(msg, log::Level::Info);
}

/// Log with level prefix (respects the configured file log level).
#[macro_export]
macro_rules! file_log {
    (trace, $($arg:tt)*) => {
        if $crate::logging::file_level_enabled(log::Level::Trace) {
            $crate::logging::log_with_level(&format!("[TRACE] {}", format!($($arg)*)), log::Level::Trace);
        }
    };
    (debug, $($arg:tt)*) => {
        if $crate::logging::file_level_enabled(log::Level::Debug) {
            $crate::logging::log_with_level(&format!("[DEBUG] {}", format!($($arg)*)), log::Level::Debug);
        }
    };
    (info, $($arg:tt)*) => {
        if $crate::logging::file_level_enabled(log::Level::Info) {
            $crate::logging::log_with_level(&format!("[INFO ] {}", format!($($arg)*)), log::Level::Info);
        }
    };
    (warn, $($arg:tt)*) => {
        if $crate::logging::file_level_enabled(log::Level::Warn) {
            $crate::logging::log_with_level(&format!("[WARN ] {}", format!($($arg)*)), log::Level::Warn);
        }
    };
    (error, $($arg:tt)*) => {
        if $crate::logging::file_level_enabled(log::Level::Error) {
            $crate::logging::log_with_level(&format!("[ERROR] {}", format!($($arg)*)), log::Level::Error);
        }
    };
}

/// Convenience function for logging errors with context.
pub fn log_error(context: &str, error: &dyn std::fmt::Display) {
    log_with_level(
        &format!("[ERROR] {}: {}", context, error),
        log::Level::Error,
    );
}

/// Log a panic to the file.
#[allow(deprecated)]
pub fn log_panic(info: &std::panic::PanicInfo) {
    log_with_level(&format!("[PANIC] {}", info), log::Level::Error);
    if let Some(location) = info.location() {
        log_with_level(
            &format!(
                "[PANIC] at {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            ),
            log::Level::Error,
        );
    }
}
