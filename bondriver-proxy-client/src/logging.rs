//! File-based logging for debugging DLL issues.
//!
//! Creates a log file with the same name as the DLL in the same directory.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::OnceCell;

/// Global log file handle.
static LOG_FILE: OnceCell<Mutex<File>> = OnceCell::new();

/// Get the path to the DLL itself.
#[cfg(windows)]
fn get_dll_path() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    // HMODULE for our DLL
    extern "system" {
        fn GetModuleHandleW(lpModuleName: *const u16) -> *mut std::ffi::c_void;
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
            let _ = LOG_FILE.set(Mutex::new(file));

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

/// Log a message to the file.
pub fn log_message(msg: &str) {
    if let Some(file_mutex) = LOG_FILE.get() {
        if let Ok(mut file) = file_mutex.lock() {
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let _ = writeln!(file, "[{}] {}", timestamp, msg);
            let _ = file.flush();
        }
    }
}

/// Log with level prefix.
#[macro_export]
macro_rules! file_log {
    (trace, $($arg:tt)*) => {
        $crate::logging::log_message(&format!("[TRACE] {}", format!($($arg)*)));
    };
    (debug, $($arg:tt)*) => {
        $crate::logging::log_message(&format!("[DEBUG] {}", format!($($arg)*)));
    };
    (info, $($arg:tt)*) => {
        $crate::logging::log_message(&format!("[INFO ] {}", format!($($arg)*)));
    };
    (warn, $($arg:tt)*) => {
        $crate::logging::log_message(&format!("[WARN ] {}", format!($($arg)*)));
    };
    (error, $($arg:tt)*) => {
        $crate::logging::log_message(&format!("[ERROR] {}", format!($($arg)*)));
    };
}

/// Convenience function for logging errors with context.
pub fn log_error(context: &str, error: &dyn std::fmt::Display) {
    log_message(&format!("[ERROR] {}: {}", context, error));
}

/// Log a panic to the file.
pub fn log_panic(info: &std::panic::PanicInfo) {
    log_message(&format!("[PANIC] {}", info));
    if let Some(location) = info.location() {
        log_message(&format!("[PANIC] at {}:{}:{}",
            location.file(),
            location.line(),
            location.column()));
    }
}
