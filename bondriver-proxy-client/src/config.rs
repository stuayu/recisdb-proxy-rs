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

use crate::client::ConnectionConfig;

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

/// Find the INI file path.
fn find_ini_file() -> Option<PathBuf> {
    // Get the DLL path
    #[cfg(windows)]
    {
        use winapi::um::libloaderapi::{GetModuleFileNameW, GetModuleHandleExW};
        use winapi::um::libloaderapi::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS;

        unsafe {
            let mut module = std::ptr::null_mut();
            let addr = load_config as *const () as *mut std::ffi::c_void;

            if GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                addr as *const u16,
                &mut module,
            ) == 0
            {
                warn!("Failed to get module handle");
                return try_current_dir();
            }

            let mut path = vec![0u16; 260];
            let len = GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32);
            if len == 0 {
                warn!("Failed to get module file name");
                return try_current_dir();
            }

            let path = String::from_utf16_lossy(&path[..len as usize]);
            let mut dll_path = PathBuf::from(path);

            // Change extension to .ini
            dll_path.set_extension("ini");

            if dll_path.exists() {
                return Some(dll_path);
            }
        }

        try_current_dir()
    }

    #[cfg(not(windows))]
    try_current_dir()
}

/// Try to find INI file in current directory.
fn try_current_dir() -> Option<PathBuf> {
    let ini_name = "BonDriver_NetworkProxy.ini";
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
    let section = sections.get("Server")?;

    let server_addr = section
        .get("Address")
        .or_else(|| section.get("Server"))
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:12345".to_string());

    let tuner_path = section
        .get("Tuner")
        .or_else(|| section.get("TunerPath"))
        .cloned()
        .unwrap_or_default();

    let connect_timeout = section
        .get("ConnectTimeout")
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(5));

    let read_timeout = section
        .get("ReadTimeout")
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(30));

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
    })
}

/// Load configuration from environment variables.
fn load_from_env() -> ConnectionConfig {
    let server_addr = std::env::var("BONDRIVER_PROXY_SERVER")
        .unwrap_or_else(|_| "127.0.0.1:12345".to_string());

    let tuner_path = std::env::var("BONDRIVER_PROXY_TUNER")
        .unwrap_or_default();

    let connect_timeout = std::env::var("BONDRIVER_PROXY_CONNECT_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(5));

    let read_timeout = std::env::var("BONDRIVER_PROXY_READ_TIMEOUT")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(30));

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
