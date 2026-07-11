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
    writer: BufWriter<File>,
    last_flush: Instant,
}

/// Global log file handle.
static LOG_FILE: OnceCell<Mutex<LoggerState>> = OnceCell::new();

/// Global file log level filter.
/// Encoded as: Off=0, Error=1, Warn=2, Info=3, Debug=4, Trace=5.
static FILE_LOG_LEVEL: AtomicU8 = AtomicU8::new(2); // default: Warn

const FLUSH_INTERVAL_SECS: u64 = 2;

/// Set the file log level.
pub fn set_file_log_level(level: log::LevelFilter) {
    let n = match level {
        log::LevelFilter::Off   => 0,
        log::LevelFilter::Error => 1,
        log::LevelFilter::Warn  => 2,
        log::LevelFilter::Info  => 3,
        log::LevelFilter::Debug => 4,
        log::LevelFilter::Trace => 5,
    };
    FILE_LOG_LEVEL.store(n, Ordering::Relaxed);
}

/// Returns true if the given level should be written to the log file.
pub fn file_level_enabled(level: log::Level) -> bool {
    let filter = FILE_LOG_LEVEL.load(Ordering::Relaxed);
    if filter == 0 { return false; }
    (level as u8) <= filter
}

/// Get the path to the DLL itself.
#[cfg(windows)]
fn get_dll_path() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    // HMODULE for our DLL
    extern "system" {
        fn GetModuleFileNameW(hModule: *mut std::ffi::c_void, lpFilename: *mut u16, nSize: u32) -> u32;
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

    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            let state = LoggerState {
                writer: BufWriter::new(file),
                last_flush: Instant::now(),
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
            let _ = writeln!(state.writer, "[{}] {}", timestamp, msg);

            // Flush immediately for Warn/Error, or on 2-second interval for others
            let should_flush = match level {
                log::Level::Warn | log::Level::Error => true,
                _ => state.last_flush.elapsed().as_secs() >= FLUSH_INTERVAL_SECS,
            };

            if should_flush {
                let _ = state.writer.flush();
                state.last_flush = Instant::now();
            }
        }
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
    log_with_level(&format!("[ERROR] {}: {}", context, error), log::Level::Error);
}

/// Log a panic to the file.
#[allow(deprecated)]
pub fn log_panic(info: &std::panic::PanicInfo) {
    log_with_level(&format!("[PANIC] {}", info), log::Level::Error);
    if let Some(location) = info.location() {
        log_with_level(&format!("[PANIC] at {}:{}:{}",
            location.file(),
            location.line(),
            location.column()), log::Level::Error);
    }
}
