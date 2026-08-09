//! Configuration loading for BonDriver_NetworkProxy.
//!
//! This module handles loading configuration from INI files.
//! The INI file should be located in the same directory as the DLL
//! with the same name but .ini extension.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use log::{debug, error, info, warn};

use recisdb_protocol::StreamClass;

use crate::client::ConnectionConfig;

/// Parse the `StreamClass` INI/env value.
///
/// Accepts both the numeric protocol value (0/1/2) and the human-readable
/// name (`view`/`record`/`preview`, case-insensitive). Unrecognized values
/// fall back to `View` with a warning rather than failing config load
/// (STREAMING_DESIGN.md §2).
fn parse_stream_class(raw: &str) -> StreamClass {
    let trimmed = raw.trim();
    match trimmed.to_lowercase().as_str() {
        "view" => return StreamClass::View,
        "record" => return StreamClass::Record,
        "preview" => return StreamClass::Preview,
        _ => {}
    }
    match trimmed.parse::<u8>().ok().and_then(|v| StreamClass::try_from(v).ok()) {
        Some(class) => class,
        None => {
            warn!("Unknown StreamClass value '{}', defaulting to View", raw);
            StreamClass::View
        }
    }
}

/// Load log level from INI file or environment.
///
/// Reads `LogLevel` from the `[Logging]` section of the INI file.
/// Accepted values (case-insensitive): `off`, `error`, `warn`, `info`, `debug`, `trace`.
/// Default: `warn`.
pub fn load_log_level() -> log::LevelFilter {
    let level_str = if let Some(ini_path) = find_ini_file() {
        if let Ok(content) = std::fs::read_to_string(&ini_path) {
            let sections = parse_ini(&content);
            sections
                .get("Logging")
                .and_then(|s| s.get("LogLevel").cloned())
        } else {
            None
        }
    } else {
        None
    };

    // Fall back to environment variable
    let level_str = level_str
        .or_else(|| std::env::var("BONDRIVER_LOG_LEVEL").ok())
        .unwrap_or_else(|| "warn".to_string());

    match level_str.to_lowercase().as_str() {
        "off"   => log::LevelFilter::Off,
        "error" => log::LevelFilter::Error,
        "warn"  => log::LevelFilter::Warn,
        "info"  => log::LevelFilter::Info,
        "debug" => log::LevelFilter::Debug,
        "trace" => log::LevelFilter::Trace,
        _ => log::LevelFilter::Warn,
    }
}

/// Load configuration from INI file.
///
/// Searches for configuration in the following order:
/// 1. BonDriver_NetworkProxy.ini next to the DLL
/// 2. Environment variables (BONDRIVER_PROXY_*)
/// 3. Default values
pub fn load_config() -> ConnectionConfig {
    // Try to find and load INI file
    if let Some(ini_path) = find_ini_file() {
        info!("Loading configuration from {:?}", ini_path);
        if let Some(config) = load_from_ini(&ini_path) {
            return config;
        }
    }

    // Fall back to environment variables
    load_from_env()
}

/// Full path of this DLL.
///
/// `GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT` is mandatory: without it
/// `GetModuleHandleExW` increments the module's reference count, and the DLL can
/// never be unloaded again. This runs once per `CreateBonDriver`, so the leak
/// would also scale with the number of instances a host opens.
#[cfg(windows)]
fn module_path() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use winapi::um::libloaderapi::{GetModuleFileNameW, GetModuleHandleExW};
    use winapi::um::libloaderapi::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };

    unsafe {
        let mut module = std::ptr::null_mut();
        let addr = module_path as *const () as *mut std::ffi::c_void;

        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            addr as *const u16,
            &mut module,
        ) == 0
        {
            warn!("Failed to get module handle");
            return None;
        }

        // MAX_PATH is not a real limit — long paths and \\?\ prefixes exceed it,
        // and GetModuleFileNameW then silently truncates while returning nSize.
        // A truncated path yields a wrong .ini name, so detect it instead.
        let mut path = vec![0u16; 32768];
        let len = GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32);
        if len == 0 {
            warn!("Failed to get module file name");
            return None;
        }
        if len as usize >= path.len() {
            warn!("Module path exceeds {} chars; ignoring it", path.len());
            return None;
        }

        Some(PathBuf::from(OsString::from_wide(&path[..len as usize])))
    }
}

#[cfg(not(windows))]
fn module_path() -> Option<PathBuf> {
    None
}

/// Find the INI file path.
///
/// The INI is named after the DLL, so a deployment that runs several copies
/// (`BonDriver_NetworkProxy_T0.dll`, `_T1`, … — the usual way to give EDCB one
/// driver per tuner) gets one INI per copy. Falling back to a hard-coded
/// `BonDriver_NetworkProxy.ini` would make every copy read the *first* tuner's
/// settings and connect to the wrong place.
fn find_ini_file() -> Option<PathBuf> {
    if let Some(dll_path) = module_path() {
        let ini_path = dll_path.with_extension("ini");
        if ini_path.exists() {
            return Some(ini_path);
        }
        // Same base name, but next to the process instead of the DLL.
        if let Some(name) = ini_path.file_name() {
            if let Some(found) = try_current_dir(&name.to_string_lossy()) {
                return Some(found);
            }
        }
        return None;
    }

    try_current_dir("BonDriver_NetworkProxy.ini")
}

/// Try to find the named INI file in the current directory.
fn try_current_dir(ini_name: &str) -> Option<PathBuf> {
    let current_dir = std::env::current_dir().ok()?;
    let ini_path = current_dir.join(ini_name);

    if ini_path.exists() {
        return Some(ini_path);
    }

    None
}

/// Simple INI section parser.
fn parse_ini(content: &str) -> HashMap<String, HashMap<String, String>> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current_section = String::new();

    for line in content.lines() {
        let line = line.trim();

        // Skip empty lines and comments
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }

        // Section header
        if line.starts_with('[') && line.ends_with(']') {
            current_section = line[1..line.len()-1].to_string();
            sections.entry(current_section.clone()).or_insert_with(HashMap::new);
            continue;
        }

        // Key=Value
        if let Some(eq_pos) = line.find('=') {
            let key = line[..eq_pos].trim().to_string();
            let value = line[eq_pos+1..].trim().to_string();

            sections
                .entry(current_section.clone())
                .or_insert_with(HashMap::new)
                .insert(key, value);
        }
    }

    sections
}

/// Load configuration from INI file.
fn load_from_ini(path: &PathBuf) -> Option<ConnectionConfig> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to read INI file: {}", e);
            return None;
        }
    };

    let sections = parse_ini(&content);
    let Some(section) = sections.get("Server") else {
        // Silently falling through to the defaults here hides a real
        // misconfiguration: the operator wrote a file, we read it, and then
        // connected somewhere else entirely.
        error!(
            "INI {:?} has no [Server] section; ignoring it and using defaults",
            path
        );
        crate::file_log!(
            error,
            "INI {:?} has no [Server] section; ignoring it and using defaults",
            path
        );
        return None;
    };

    let server_addr = section
        .get("Address")
        .or_else(|| section.get("Server"))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:40070".to_string());

    let tuner_path = section
        .get("Tuner")
        .or_else(|| section.get("TunerPath"))
        .cloned()
        .unwrap_or_default();

    let defaults = ConnectionConfig::default();

    let connect_timeout = section
        .get("ConnectTimeout")
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(defaults.connect_timeout);

    let read_timeout = section
        .get("ReadTimeout")
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(defaults.read_timeout);

    let client_priority = section
        .get("Priority")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let client_exclusive = section
        .get("Exclusive")
        .map(|s| {
            let lower = s.to_lowercase();
            lower == "1" || lower == "true" || lower == "yes" || lower == "on"
        })
        .unwrap_or(false);

    // TLS settings
    #[cfg(feature = "tls")]
    let tls_enabled = section
        .get("TLS")
        .or_else(|| section.get("UseTLS"))
        .map(|s| s == "1" || s.to_lowercase() == "true")
        .unwrap_or(false);

    #[cfg(feature = "tls")]
    let tls_ca_cert = section
        .get("TLSCACert")
        .or_else(|| section.get("CACertPath"))
        .cloned();

    let single_service = section
        .get("ServiceFilter")
        .map(|s| s.to_lowercase() == "single")
        .unwrap_or(false);

    let stream_class = section
        .get("StreamClass")
        .map(|s| parse_stream_class(s))
        .unwrap_or(StreamClass::View);

    debug!("Configuration loaded: server={}, tuner={}", server_addr, tuner_path);

    Some(ConnectionConfig {
        server_addr,
        tuner_path,
        connect_timeout,
        read_timeout,
        client_priority,
        client_exclusive,
        #[cfg(feature = "tls")]
        tls_enabled,
        #[cfg(feature = "tls")]
        tls_ca_cert,
        single_service,
        stream_class,
    })
}

/// Load configuration from environment variables.
fn load_from_env() -> ConnectionConfig {
    let server_addr = std::env::var("BONDRIVER_PROXY_SERVER")
        .unwrap_or_else(|_| "127.0.0.1:40070".to_string());

    let tuner_path = std::env::var("BONDRIVER_PROXY_TUNER")
        .unwrap_or_default();

    let defaults = ConnectionConfig::default();

    let connect_timeout = std::env::var("BONDRIVER_PROXY_CONNECT_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(defaults.connect_timeout);

    let read_timeout = std::env::var("BONDRIVER_PROXY_READ_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(defaults.read_timeout);

    let client_priority = std::env::var("BONDRIVER_PROXY_PRIORITY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let client_exclusive = std::env::var("BONDRIVER_PROXY_EXCLUSIVE")
        .map(|s| {
            let lower = s.to_lowercase();
            lower == "1" || lower == "true" || lower == "yes" || lower == "on"
        })
        .unwrap_or(false);

    debug!("Using environment/default config: server={}, tuner={}", server_addr, tuner_path);

    ConnectionConfig {
        server_addr,
        tuner_path,
        connect_timeout,
        read_timeout,
        client_priority,
        client_exclusive,
        #[cfg(feature = "tls")]
        tls_enabled: std::env::var("BONDRIVER_PROXY_TLS")
            .map(|s| s == "1" || s.to_lowercase() == "true")
            .unwrap_or(false),
        #[cfg(feature = "tls")]
        tls_ca_cert: std::env::var("BONDRIVER_PROXY_CA_CERT").ok(),
        single_service: std::env::var("BONDRIVER_PROXY_SERVICE_FILTER")
            .map(|s| s.to_lowercase() == "single")
            .unwrap_or(false),
        stream_class: std::env::var("BONDRIVER_PROXY_STREAM_CLASS")
            .ok()
            .map(|s| parse_stream_class(&s))
            .unwrap_or(StreamClass::View),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_load_from_env() {
        let config = load_from_env();
        assert!(!config.server_addr.is_empty());
    }

    #[test]
    fn test_parse_ini() {
        let content = r#"
; Comment
[Server]
Address = 192.168.1.1:12345
Tuner = /dev/pt3video0

[Other]
Key = Value
"#;
        let sections = parse_ini(content);

        assert!(sections.contains_key("Server"));
        let server = sections.get("Server").unwrap();
        assert_eq!(server.get("Address").unwrap(), "192.168.1.1:12345");
        assert_eq!(server.get("Tuner").unwrap(), "/dev/pt3video0");
    }
}
