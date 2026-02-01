//! Database command handlers for scan, show, query operations.

use std::io::Write;
use std::path::PathBuf;

use log::{error, info, warn};

use crate::context::{BroadcastType, OutputFormat};
use crate::database::{
    BonDriverRecord, ChannelRecord, Database, DatabaseError, NewBonDriver,
};
use crate::tuner::Voltage;

use recisdb_protocol::broadcast_region::{classify_nid, BroadcastRegion};

/// Default database path.
fn default_database_path() -> PathBuf {
    // Try to use XDG_DATA_HOME or ~/.local/share on Unix
    // and %APPDATA% on Windows
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("recisdb").join("channels.db");
        }
    }

    #[cfg(unix)]
    {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(data_home).join("recisdb").join("channels.db");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("recisdb")
                .join("channels.db");
        }
    }

    PathBuf::from("channels.db")
}

/// Open database with optional path.
fn open_database(path: Option<String>) -> Result<Database, DatabaseError> {
    let db_path = path.map(PathBuf::from).unwrap_or_else(default_database_path);

    // Create parent directory if needed
    if let Some(parent) = db_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| {
                DatabaseError::PathError(format!("Failed to create directory: {}", e))
            })?;
        }
    }

    info!("Using database: {}", db_path.display());
    Database::open(&db_path)
}

/// Generate channels to scan based on broadcast type.
fn generate_scan_channels(
    broadcast_type: BroadcastType,
    range: Option<String>,
) -> Vec<(String, ChannelType)> {
    let mut channels = Vec::new();

    match broadcast_type {
        BroadcastType::Terrestrial | BroadcastType::All => {
            // Parse range or use default UHF 13-62
            let (start, end) = if let Some(ref r) = range {
                parse_range(r).unwrap_or((13, 62))
            } else {
                (13, 62)
            };

            for ch in start..=end {
                let name = format!("T{}", ch);
                channels.push((name.clone(), ChannelType::Terrestrial(ch as i32, None)));
            }
        }
        _ => {}
    }

    match broadcast_type {
        BroadcastType::Bs | BroadcastType::All => {
            // BS odd channels 1-23
            for ch in (1..=23).step_by(2) {
                let name = format!("BS{:02}", ch);
                channels.push((
                    name.clone(),
                    ChannelType::BS(ch, crate::channels::representation::TsFilter::AsIs),
                ));
            }
        }
        _ => {}
    }

    match broadcast_type {
        BroadcastType::Cs | BroadcastType::All => {
            // CS even channels 2-24
            for ch in (2..=24).step_by(2) {
                let name = format!("CS{}", ch);
                channels.push((
                    name.clone(),
                    ChannelType::CS(ch, crate::channels::representation::TsFilter::AsIs),
                ));
            }
        }
        _ => {}
    }

    channels
}

/// Parse range string like "13-62" to (start, end).
fn parse_range(range: &str) -> Option<(i32, i32)> {
    let parts: Vec<&str> = range.split('-').collect();
    if parts.len() == 2 {
        let start = parts[0].parse().ok()?;
        let end = parts[1].parse().ok()?;
        Some((start, end))
    } else if parts.len() == 1 {
        let ch = parts[0].parse().ok()?;
        Some((ch, ch))
    } else {
        None
    }
}

/// Scan command implementation.
///
/// Note: Full scan implementation requires async TS reading which is complex.
/// For now, this provides basic registration functionality.
/// Use the proxy server's scan scheduler for production scanning.
#[allow(unused_variables)]
pub fn cmd_scan(
    device: String,
    range: Option<String>,
    broadcast_type: BroadcastType,
    database_path: Option<String>,
    timeout_secs: u32,
    lnb: Option<Voltage>,
    continue_on_error: bool,
    verbose: bool,
) -> i32 {
    let mut db = match open_database(database_path) {
        Ok(db) => db,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return 1;
        }
    };

    // Register or get BonDriver
    let bon_driver_id = match db.get_bon_driver_by_path(&device) {
        Ok(driver) => {
            info!("Using existing BonDriver: {} (ID: {})", device, driver.id);
            driver.id
        }
        Err(_) => {
            info!("Registering new BonDriver: {}", device);
            match db.insert_bon_driver(&NewBonDriver::new(&device)) {
                Ok(id) => id,
                Err(e) => {
                    error!("Failed to register BonDriver: {}", e);
                    return 1;
                }
            }
        }
    };

    // Generate channels to scan
    let scan_channels = generate_scan_channels(broadcast_type, range.clone());
    let total = scan_channels.len();

    if total == 0 {
        error!("No channels to scan");
        return 1;
    }

    println!("Scan Configuration:");
    println!("  Device: {}", device);
    println!("  Broadcast Type: {:?}", broadcast_type);
    println!("  Channel Range: {}", range.as_deref().unwrap_or("default"));
    println!("  Channels to scan: {}", total);
    println!("  Timeout per channel: {}s", timeout_secs);
    println!();

    // Note: Full async scan implementation is complex.
    // For now, provide info about what would be scanned.
    println!("Note: Full channel scanning requires async TS stream processing.");
    println!("For production use, consider using the recisdb-proxy scan scheduler.");
    println!();
    println!("Channels that would be scanned:");

    for (name, _ch_type) in scan_channels.iter().take(10) {
        println!("  - {}", name);
    }
    if total > 10 {
        println!("  ... and {} more", total - 10);
    }

    println!();
    println!("BonDriver registered with ID: {}", bon_driver_id);
    println!("Use 'recisdb show' to view stored channels after scanning.");

    0
}

/// Show command implementation.
pub fn cmd_show(
    database_path: Option<String>,
    format: OutputFormat,
    broadcast_type: Option<BroadcastType>,
    nid_filter: Option<u16>,
    tsid_filter: Option<u16>,
    enabled_only: bool,
    sort_field: String,
) -> i32 {
    let db = match open_database(database_path) {
        Ok(db) => db,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return 1;
        }
    };

    // Get all channels
    let channels = match db.get_all_channels() {
        Ok(chs) => chs,
        Err(e) => {
            error!("Failed to get channels: {}", e);
            return 1;
        }
    };

    // Filter channels
    let filtered: Vec<_> = channels
        .into_iter()
        .filter(|ch| {
            if enabled_only && !ch.is_enabled {
                return false;
            }
            if let Some(nid) = nid_filter {
                if ch.nid != nid {
                    return false;
                }
            }
            if let Some(tsid) = tsid_filter {
                if ch.tsid != tsid {
                    return false;
                }
            }
            if let Some(ref bt) = broadcast_type {
                let region = classify_nid(ch.nid);
                match bt {
                    BroadcastType::Terrestrial => {
                        if !matches!(region, BroadcastRegion::Terrestrial(_)) {
                            return false;
                        }
                    }
                    BroadcastType::Bs => {
                        if !matches!(region, BroadcastRegion::BS) {
                            return false;
                        }
                    }
                    BroadcastType::Cs => {
                        if !matches!(region, BroadcastRegion::CS(_)) {
                            return false;
                        }
                    }
                    BroadcastType::All => {}
                }
            }
            true
        })
        .collect();

    // Sort channels
    let mut sorted = filtered;
    match sort_field.as_str() {
        "name" => sorted.sort_by(|a, b| {
            a.channel_name
                .as_ref()
                .unwrap_or(&String::new())
                .cmp(b.channel_name.as_ref().unwrap_or(&String::new()))
        }),
        "nid" => sorted.sort_by_key(|ch| ch.nid),
        "sid" => sorted.sort_by_key(|ch| ch.sid),
        "physical_ch" | _ => sorted.sort_by_key(|ch| ch.physical_ch.unwrap_or(255)),
    }

    // Output
    match format {
        OutputFormat::Table => print_channels_table(&sorted),
        OutputFormat::Json => print_channels_json(&sorted),
        OutputFormat::Csv => print_channels_csv(&sorted),
    }

    0
}

/// Query command implementation.
pub fn cmd_query(
    database_path: Option<String>,
    name: Option<String>,
    sid: Option<u16>,
    nid: Option<u16>,
    tsid: Option<u16>,
    remote_key: Option<u8>,
    format: OutputFormat,
    detail: bool,
) -> i32 {
    let db = match open_database(database_path) {
        Ok(db) => db,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return 1;
        }
    };

    // Get all channels and filter
    let channels = match db.get_all_channels() {
        Ok(chs) => chs,
        Err(e) => {
            error!("Failed to get channels: {}", e);
            return 1;
        }
    };

    let filtered: Vec<_> = channels
        .into_iter()
        .filter(|ch| {
            if let Some(ref n) = name {
                let ch_name = ch.channel_name.as_ref().map(|s| s.to_lowercase());
                let raw_name = ch.raw_name.as_ref().map(|s| s.to_lowercase());
                let n_lower = n.to_lowercase();

                let matches = ch_name.map(|cn| cn.contains(&n_lower)).unwrap_or(false)
                    || raw_name.map(|rn| rn.contains(&n_lower)).unwrap_or(false);

                if !matches {
                    return false;
                }
            }
            if let Some(s) = sid {
                if ch.sid != s {
                    return false;
                }
            }
            if let Some(n) = nid {
                if ch.nid != n {
                    return false;
                }
            }
            if let Some(t) = tsid {
                if ch.tsid != t {
                    return false;
                }
            }
            if let Some(rk) = remote_key {
                if ch.remote_control_key != Some(rk) {
                    return false;
                }
            }
            true
        })
        .collect();

    if filtered.is_empty() {
        println!("No channels found matching the criteria.");
        return 0;
    }

    if detail {
        for ch in &filtered {
            print_channel_detail(ch);
            println!();
        }
    } else {
        match format {
            OutputFormat::Table => print_channels_table(&filtered),
            OutputFormat::Json => print_channels_json(&filtered),
            OutputFormat::Csv => print_channels_csv(&filtered),
        }
    }

    0
}

/// Driver add command.
pub fn cmd_driver_add(
    path: String,
    name: Option<String>,
    database_path: Option<String>,
) -> i32 {
    let mut db = match open_database(database_path) {
        Ok(db) => db,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return 1;
        }
    };

    let mut new_driver = NewBonDriver::new(&path);
    if let Some(n) = name {
        new_driver = new_driver.with_name(n);
    }

    match db.insert_bon_driver(&new_driver) {
        Ok(id) => {
            println!("BonDriver registered with ID: {}", id);
            0
        }
        Err(e) => {
            error!("Failed to register BonDriver: {}", e);
            1
        }
    }
}

/// Driver list command.
pub fn cmd_driver_list(database_path: Option<String>, format: OutputFormat) -> i32 {
    let db = match open_database(database_path) {
        Ok(db) => db,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return 1;
        }
    };

    let drivers = match db.get_all_bon_drivers() {
        Ok(d) => d,
        Err(e) => {
            error!("Failed to get BonDrivers: {}", e);
            return 1;
        }
    };

    match format {
        OutputFormat::Table => print_drivers_table(&drivers),
        OutputFormat::Json => print_drivers_json(&drivers),
        OutputFormat::Csv => print_drivers_csv(&drivers),
    }

    0
}

/// Driver remove command.
pub fn cmd_driver_remove(
    id_or_path: String,
    database_path: Option<String>,
    skip_confirm: bool,
) -> i32 {
    let mut db = match open_database(database_path) {
        Ok(db) => db,
        Err(e) => {
            error!("Failed to open database: {}", e);
            return 1;
        }
    };

    // Find driver by ID or path
    let driver = if let Ok(id) = id_or_path.parse::<i64>() {
        db.get_bon_driver(id).ok()
    } else {
        db.get_bon_driver_by_path(&id_or_path).ok()
    };

    let driver = match driver {
        Some(d) => d,
        None => {
            error!("BonDriver not found: {}", id_or_path);
            return 1;
        }
    };

    if !skip_confirm {
        print!(
            "Remove BonDriver '{}' (ID: {}) and all its channels? [y/N]: ",
            driver.dll_path, driver.id
        );
        std::io::stdout().flush().ok();

        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return 0;
        }
    }

    match db.delete_bon_driver(driver.id) {
        Ok(_) => {
            println!("BonDriver removed.");
            0
        }
        Err(e) => {
            error!("Failed to remove BonDriver: {}", e);
            1
        }
    }
}

// Output formatting functions

fn print_channels_table(channels: &[ChannelRecord]) {
    if channels.is_empty() {
        println!("No channels found.");
        return;
    }

    // Header
    println!(
        "{:<6} {:<6} {:<6} {:<4} {:<20} {:<12} {}",
        "NID", "TSID", "SID", "Ch", "Name", "Network", "Enabled"
    );
    println!("{}", "-".repeat(70));

    for ch in channels {
        println!(
            "{:<6} {:<6} {:<6} {:<4} {:<20} {:<12} {}",
            format!("0x{:04X}", ch.nid),
            format!("0x{:04X}", ch.tsid),
            format!("0x{:04X}", ch.sid),
            ch.physical_ch.map(|c| c.to_string()).unwrap_or_default(),
            ch.channel_name.as_deref().unwrap_or("-").chars().take(20).collect::<String>(),
            ch.network_name.as_deref().unwrap_or("-").chars().take(12).collect::<String>(),
            if ch.is_enabled { "Yes" } else { "No" }
        );
    }

    println!("\nTotal: {} channels", channels.len());
}

fn print_channels_json(channels: &[ChannelRecord]) {
    // Simple JSON output
    println!("[");
    for (i, ch) in channels.iter().enumerate() {
        let comma = if i < channels.len() - 1 { "," } else { "" };
        println!(
            r#"  {{"nid": {}, "tsid": {}, "sid": {}, "physical_ch": {}, "name": {}, "network": {}, "enabled": {}}}{}"#,
            ch.nid,
            ch.tsid,
            ch.sid,
            ch.physical_ch.map(|c| c.to_string()).unwrap_or("null".to_string()),
            ch.channel_name.as_ref().map(|s| format!("\"{}\"", s.replace('"', "\\\""))).unwrap_or("null".to_string()),
            ch.network_name.as_ref().map(|s| format!("\"{}\"", s.replace('"', "\\\""))).unwrap_or("null".to_string()),
            ch.is_enabled,
            comma
        );
    }
    println!("]");
}

fn print_channels_csv(channels: &[ChannelRecord]) {
    println!("nid,tsid,sid,physical_ch,name,network,enabled");
    for ch in channels {
        println!(
            "{},{},{},{},{},{},{}",
            ch.nid,
            ch.tsid,
            ch.sid,
            ch.physical_ch.map(|c| c.to_string()).unwrap_or_default(),
            ch.channel_name.as_deref().unwrap_or(""),
            ch.network_name.as_deref().unwrap_or(""),
            ch.is_enabled
        );
    }
}

fn print_channel_detail(ch: &ChannelRecord) {
    println!("Channel Details:");
    println!("  NID:              0x{:04X} ({})", ch.nid, ch.nid);
    println!("  TSID:             0x{:04X} ({})", ch.tsid, ch.tsid);
    println!("  SID:              0x{:04X} ({})", ch.sid, ch.sid);
    println!(
        "  Physical Ch:      {}",
        ch.physical_ch.map(|c| c.to_string()).unwrap_or("-".to_string())
    );
    println!(
        "  Name:             {}",
        ch.channel_name.as_deref().unwrap_or("-")
    );
    println!(
        "  Raw Name:         {}",
        ch.raw_name.as_deref().unwrap_or("-")
    );
    println!(
        "  Network:          {}",
        ch.network_name.as_deref().unwrap_or("-")
    );
    println!(
        "  Service Type:     {}",
        ch.service_type.map(|t| format!("0x{:02X}", t)).unwrap_or("-".to_string())
    );
    println!(
        "  Remote Key:       {}",
        ch.remote_control_key.map(|k| k.to_string()).unwrap_or("-".to_string())
    );
    println!("  Enabled:          {}", if ch.is_enabled { "Yes" } else { "No" });
    println!("  Failure Count:    {}", ch.failure_count);

    // Broadcast region
    let region = classify_nid(ch.nid);
    println!("  Broadcast Region: {}", region);
}

fn print_drivers_table(drivers: &[BonDriverRecord]) {
    if drivers.is_empty() {
        println!("No BonDrivers registered.");
        return;
    }

    println!(
        "{:<4} {:<40} {:<20} {}",
        "ID", "Path", "Name", "Auto Scan"
    );
    println!("{}", "-".repeat(70));

    for d in drivers {
        println!(
            "{:<4} {:<40} {:<20} {}",
            d.id,
            d.dll_path.chars().take(40).collect::<String>(),
            d.driver_name.as_deref().unwrap_or("-").chars().take(20).collect::<String>(),
            if d.auto_scan_enabled { "Yes" } else { "No" }
        );
    }
}

fn print_drivers_json(drivers: &[BonDriverRecord]) {
    println!("[");
    for (i, d) in drivers.iter().enumerate() {
        let comma = if i < drivers.len() - 1 { "," } else { "" };
        println!(
            r#"  {{"id": {}, "path": "{}", "name": {}, "auto_scan": {}}}{}"#,
            d.id,
            d.dll_path.replace('"', "\\\""),
            d.driver_name.as_ref().map(|s| format!("\"{}\"", s.replace('"', "\\\""))).unwrap_or("null".to_string()),
            d.auto_scan_enabled,
            comma
        );
    }
    println!("]");
}

fn print_drivers_csv(drivers: &[BonDriverRecord]) {
    println!("id,path,name,auto_scan");
    for d in drivers {
        println!(
            "{},{},{},{}",
            d.id,
            d.dll_path,
            d.driver_name.as_deref().unwrap_or(""),
            d.auto_scan_enabled
        );
    }
}
