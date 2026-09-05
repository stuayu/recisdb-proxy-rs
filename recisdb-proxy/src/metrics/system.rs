//! Host resource metrics kept in a short-lived in-memory history.

use serde::Serialize;
use std::collections::VecDeque;
use std::future::Future;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Networks, System};
use tokio::process::Command;
use tokio::sync::RwLock;

pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
pub const HISTORY_CAPACITY: usize = 180;
const GPU_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Apple,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
pub struct GpuMetrics {
    pub index: usize,
    pub vendor: GpuVendor,
    pub name: String,
    pub usage_percent: Option<f32>,
    pub memory_used_bytes: Option<u64>,
    pub memory_total_bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemMetricSample {
    pub timestamp: i64,
    pub cpu_usage_percent: f32,
    pub cpu_cores: usize,
    pub load_average_1: Option<f64>,
    pub load_average_5: Option<f64>,
    pub load_average_15: Option<f64>,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub network_received_bytes: u64,
    pub network_transmitted_bytes: u64,
    pub network_receive_bps: f64,
    pub network_transmit_bps: f64,
    pub gpus: Vec<GpuMetrics>,
}

pub type ProbeFuture<'a> = Pin<Box<dyn Future<Output = Vec<GpuMetrics>> + Send + 'a>>;
pub trait GpuProbe: Send + Sync {
    fn sample<'a>(&'a self) -> ProbeFuture<'a>;
}

#[derive(Debug, Clone)]
struct NvidiaProbe;
impl GpuProbe for NvidiaProbe {
    fn sample<'a>(&'a self) -> ProbeFuture<'a> {
        Box::pin(async move {
            command_output(
                "nvidia-smi",
                &[
                    "--query-gpu=index,name,utilization.gpu,memory.used,memory.total",
                    "--format=csv,noheader,nounits",
                ],
            )
            .await
            .map(|s| parse_nvidia_smi(&s))
            .unwrap_or_default()
        })
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct AmdProbe;
#[cfg(target_os = "linux")]
impl GpuProbe for AmdProbe {
    fn sample<'a>(&'a self) -> ProbeFuture<'a> {
        Box::pin(async move {
            if let Some(text) = command_output(
                "rocm-smi",
                &["--showuse", "--showmeminfo", "vram", "--json"],
            )
            .await
            {
                let values = parse_rocm_smi(&text);
                if !values.is_empty() {
                    return values;
                }
            }
            parse_sysfs_gpus("0x1002")
        })
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct IntelProbe;
#[cfg(target_os = "linux")]
impl GpuProbe for IntelProbe {
    fn sample<'a>(&'a self) -> ProbeFuture<'a> {
        Box::pin(async { parse_sysfs_gpus("0x8086") })
    }
}

#[cfg(windows)]
#[derive(Debug, Clone)]
struct WindowsGpuProbe;
#[cfg(windows)]
impl GpuProbe for WindowsGpuProbe {
    fn sample<'a>(&'a self) -> ProbeFuture<'a> {
        Box::pin(async move {
            let Some(text) = command_output(
                "powershell",
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    r#"$mem = @{}; (Get-Counter '\GPU Adapter Memory(*)\Dedicated Usage','\GPU Adapter Memory(*)\Shared Usage' -ErrorAction SilentlyContinue).CounterSamples | ForEach-Object { if ($_.Path -match '(luid_[^)]+)\)') { $key=$Matches[1]; if (!$mem.ContainsKey($key)) { $mem[$key]=@{dedicated=0;shared=0} }; if ($_.Path -match 'Dedicated Usage') { $mem[$key].dedicated += $_.CookedValue } else { $mem[$key].shared += $_.CookedValue } } }; $usage = @{}; (Get-Counter '\GPU Engine(*)\Utilization Percentage' -ErrorAction SilentlyContinue).CounterSamples | ForEach-Object { if ($_.Path -match '(luid_[^)]+)\)') { $key=$Matches[1]; $usage[$key] = ($usage[$key] + $_.CookedValue) } }; Get-CimInstance Win32_VideoController | Where-Object { $_.Name -notmatch 'Remote Display|Microsoft Basic' } | ForEach-Object { $vendor=$_.AdapterCompatibility; $key=$null; if ($vendor -match 'NVIDIA' -or $_.Name -match 'NVIDIA') { $key=$mem.GetEnumerator() | Sort-Object { $_.Value.dedicated } -Descending | Select-Object -First 1 -ExpandProperty Key } elseif ($vendor -match 'Intel' -or $_.Name -match 'Intel') { $key=$mem.GetEnumerator() | Where-Object { $_.Key -ne ($mem.GetEnumerator() | Sort-Object { $_.Value.dedicated } -Descending | Select-Object -First 1 -ExpandProperty Key) } | Sort-Object { $_.Value.shared } -Descending | Select-Object -First 1 -ExpandProperty Key }; $d=0; $s=0; $u=$null; if ($key -and $mem.ContainsKey($key)) { $d=$mem[$key].dedicated; $s=$mem[$key].shared; $u=$usage[$key] }; \"$($_.Name)|$vendor|$u|$d|$s\" }"#,
                ],
            )
            .await
            else {
                return Vec::new();
            };
            parse_windows_gpu_inventory(&text)
        })
    }
}

async fn command_output(command: &str, args: &[&str]) -> Option<String> {
    let output = tokio::time::timeout(
        GPU_COMMAND_TIMEOUT,
        Command::new(command).args(args).output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_nvidia_smi(text: &str) -> Vec<GpuMetrics> {
    text.lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split(',').map(str::trim).collect();
            if fields.len() < 5 {
                return None;
            }
            Some(GpuMetrics {
                index: fields.first()?.parse().ok()?,
                vendor: GpuVendor::Nvidia,
                name: fields.get(1)?.to_string(),
                usage_percent: fields.get(2)?.parse().ok(),
                memory_used_bytes: fields
                    .get(3)?
                    .parse::<u64>()
                    .ok()
                    .map(|v| v.saturating_mul(1024 * 1024)),
                memory_total_bytes: fields
                    .get(4)?
                    .parse::<u64>()
                    .ok()
                    .map(|v| v.saturating_mul(1024 * 1024)),
            })
        })
        .collect()
}

#[allow(dead_code)]
fn parse_rocm_smi(text: &str) -> Vec<GpuMetrics> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(objects) = value.as_object() else {
        return Vec::new();
    };
    objects
        .iter()
        .filter_map(|(key, value)| {
            let index = key
                .trim_start_matches("card")
                .trim_start_matches("GPU")
                .parse()
                .ok()?;
            let obj = value.as_object()?;
            Some(GpuMetrics {
                index,
                vendor: GpuVendor::Amd,
                name: format!("AMD GPU {index}"),
                usage_percent: find_number(
                    obj,
                    &["GPU use (%)", "gpu_busy_percent", "GPU Use (%)"],
                )
                .map(|v| v as f32),
                memory_used_bytes: find_number(
                    obj,
                    &["VRAM Used (B)", "vram_used", "VRAM Total Used (B)"],
                )
                .map(|v| v as u64),
                memory_total_bytes: find_number(
                    obj,
                    &["VRAM Total Memory (B)", "vram_total", "VRAM Total (B)"],
                )
                .map(|v| v as u64),
            })
        })
        .collect()
}
#[allow(dead_code)]
fn find_number(object: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| find_number_value(object.get(*key)))
        .or_else(|| {
            object
                .values()
                .filter_map(serde_json::Value::as_object)
                .find_map(|child| find_number(child, keys))
        })
}

#[allow(dead_code)]
fn find_number_value(value: Option<&serde_json::Value>) -> Option<f64> {
    let value = value?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.split_whitespace().next()?.parse().ok())
        .or_else(|| {
            value
                .as_object()?
                .values()
                .find_map(|child| find_number_value(Some(child)))
        })
}

#[cfg(target_os = "linux")]
fn parse_sysfs_gpus(vendor_id: &str) -> Vec<GpuMetrics> {
    let mut cards: Vec<_> = std::fs::read_dir("/sys/class/drm")
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name().to_string_lossy().starts_with("card")
                && e.path().join("device/vendor").is_file()
        })
        .collect();
    cards.sort_by_key(|e| e.file_name());
    cards
        .into_iter()
        .filter(|entry| {
            std::fs::read_to_string(entry.path().join("device/vendor"))
                .map(|vendor| vendor.trim() == vendor_id)
                .unwrap_or(false)
        })
        .enumerate()
        .filter_map(|(index, entry)| {
            let device = entry.path().join("device");
            let read = |name: &str| read_sysfs_number(&device.join(name));
            Some(GpuMetrics {
                index,
                vendor: if vendor_id == "0x1002" {
                    GpuVendor::Amd
                } else {
                    GpuVendor::Intel
                },
                name: format!(
                    "{} GPU {index}",
                    if vendor_id == "0x1002" {
                        "AMD"
                    } else {
                        "Intel"
                    }
                ),
                usage_percent: read("gpu_busy_percent").map(|v| v as f32),
                memory_used_bytes: read("mem_info_vram_used"),
                memory_total_bytes: read("mem_info_vram_total"),
            })
        })
        .collect()
}
#[cfg(target_os = "linux")]
fn read_sysfs_number(path: &Path) -> Option<u64> {
    parse_sysfs_number(&std::fs::read_to_string(path).ok()?)
}
#[cfg(target_os = "linux")]
fn parse_sysfs_number(value: &str) -> Option<u64> {
    value.trim().parse().ok()
}

fn parse_windows_gpu_inventory(text: &str) -> Vec<GpuMetrics> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('|');
            let name = fields.next()?;
            let compatibility = fields.next()?;
            let usage_percent = fields.next().and_then(|v| v.trim().parse::<f32>().ok());
            let dedicated = fields.next().and_then(|v| v.trim().parse::<f64>().ok());
            let shared = fields.next().and_then(|v| v.trim().parse::<f64>().ok());
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let vendor_text = compatibility.trim().to_ascii_lowercase();
            let vendor = if vendor_text.contains("nvidia")
                || name.to_ascii_lowercase().contains("nvidia")
            {
                GpuVendor::Nvidia
            } else if vendor_text.contains("amd") || vendor_text.contains("advanced micro") {
                GpuVendor::Amd
            } else if vendor_text.contains("intel") || name.to_ascii_lowercase().contains("intel") {
                GpuVendor::Intel
            } else {
                GpuVendor::Unknown
            };
            Some((name.to_string(), vendor, usage_percent, dedicated, shared))
        })
        .enumerate()
        .map(
            |(index, (name, vendor, usage_percent, dedicated, shared))| GpuMetrics {
                index,
                vendor,
                name,
                usage_percent,
                memory_used_bytes: dedicated
                    .zip(shared)
                    .map(|(d, s)| (d.max(0.0) + s.max(0.0)) as u64),
                // Windows exposes shared usage but not a stable per-adapter VRAM
                // capacity through these counters. Do not present shared usage as
                // the total capacity.
                memory_total_bytes: None,
            },
        )
        .collect()
}

async fn discover_probes() -> Vec<Box<dyn GpuProbe>> {
    let mut probes: Vec<Box<dyn GpuProbe>> = Vec::new();
    let nvidia = Box::new(NvidiaProbe) as Box<dyn GpuProbe>;
    if !nvidia.sample().await.is_empty() {
        probes.push(nvidia);
    }
    #[cfg(target_os = "linux")]
    for probe in [
        Box::new(AmdProbe) as Box<dyn GpuProbe>,
        Box::new(IntelProbe) as Box<dyn GpuProbe>,
    ] {
        if !probe.sample().await.is_empty() {
            probes.push(probe);
        }
    }
    #[cfg(windows)]
    {
        let probe = Box::new(WindowsGpuProbe) as Box<dyn GpuProbe>;
        if !probe.sample().await.is_empty() {
            probes.push(probe);
        }
    }
    probes
}

#[derive(Debug, Clone)]
pub struct SystemMetricsSnapshot {
    pub latest: Option<SystemMetricSample>,
    pub history: Vec<SystemMetricSample>,
}
#[derive(Debug)]
pub struct SystemMetricsCollector {
    samples: RwLock<VecDeque<SystemMetricSample>>,
}
impl SystemMetricsCollector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            samples: RwLock::new(VecDeque::with_capacity(HISTORY_CAPACITY)),
        })
    }
    pub fn spawn(self: &Arc<Self>) {
        let collector = Arc::clone(self);
        tokio::spawn(async move {
            collector.collect_loop().await;
        });
    }
    pub async fn snapshot(&self) -> SystemMetricsSnapshot {
        let samples = self.samples.read().await;
        SystemMetricsSnapshot {
            latest: samples.back().cloned(),
            history: samples.iter().cloned().collect(),
        }
    }
    async fn collect_loop(&self) {
        let mut system = System::new_all();
        let mut networks = Networks::new_with_refreshed_list();
        let probes = discover_probes().await;
        let mut previous_network: Option<(i64, u64, u64)> = None;
        let mut interval = tokio::time::interval(SAMPLE_INTERVAL);
        loop {
            interval.tick().await;
            system.refresh_cpu_usage();
            system.refresh_memory();
            networks.refresh(true);
            let received: u64 = networks.values().map(|data| data.total_received()).sum();
            let transmitted: u64 = networks.values().map(|data| data.total_transmitted()).sum();
            let now = unix_timestamp_millis();
            let (receive_bps, transmit_bps) = previous_network
                .map(|(at, old_received, old_transmitted)| {
                    let seconds = (now - at).max(1) as f64 / 1000.0;
                    (
                        (received.saturating_sub(old_received) as f64 * 8.0) / seconds,
                        (transmitted.saturating_sub(old_transmitted) as f64 * 8.0) / seconds,
                    )
                })
                .unwrap_or((0.0, 0.0));
            previous_network = Some((now, received, transmitted));
            let (load_average_1, load_average_5, load_average_15) = load_average();
            let mut gpus = Vec::new();
            for probe in &probes {
                gpus.extend(probe.sample().await);
            }
            let sample = SystemMetricSample {
                timestamp: now,
                cpu_usage_percent: system.global_cpu_usage(),
                cpu_cores: system.cpus().len(),
                load_average_1,
                load_average_5,
                load_average_15,
                memory_used_bytes: system.used_memory(),
                memory_total_bytes: system.total_memory(),
                network_received_bytes: received,
                network_transmitted_bytes: transmitted,
                network_receive_bps: receive_bps,
                network_transmit_bps: transmit_bps,
                gpus,
            };
            let mut samples = self.samples.write().await;
            samples.push_back(sample);
            while samples.len() > HISTORY_CAPACITY {
                samples.pop_front();
            }
        }
    }
}
#[cfg(unix)]
fn load_average() -> (Option<f64>, Option<f64>, Option<f64>) {
    let load = System::load_average();
    (Some(load.one), Some(load.five), Some(load.fifteen))
}
#[cfg(not(unix))]
fn load_average() -> (Option<f64>, Option<f64>, Option<f64>) {
    (None, None, None)
}
fn unix_timestamp_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_all_nvidia_rows_and_skips_bad_rows() {
        let result = parse_nvidia_smi("0, RTX 3060, 12, 100, 200\nbad\n1, RX, 0, 1, 2");
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].index, 1);
        assert_eq!(result[0].memory_total_bytes, Some(200 * 1024 * 1024));
    }
    #[test]
    fn parses_rocm_json_values() {
        let result = parse_rocm_smi(
            r#"{"card0":{"GPU use (%)":"42 %","VRAM Used (B)":123,"VRAM Total Memory (B)":456}}"#,
        );
        assert_eq!(result[0].usage_percent, Some(42.0));
        assert_eq!(result[0].memory_total_bytes, Some(456));
    }
    #[test]
    fn parses_windows_gpu_inventory_with_usage_and_memory() {
        let result = parse_windows_gpu_inventory(
            "Intel(R) UHD Graphics 730|Intel Corporation|12.5|0|78303232\nNVIDIA GeForce GTX 1660 Ti|NVIDIA|0|466128896|25124864",
        );
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].vendor, GpuVendor::Intel);
        assert_eq!(result[0].usage_percent, Some(12.5));
        assert_eq!(result[0].memory_used_bytes, Some(78303232));
        assert_eq!(result[0].memory_total_bytes, None);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn parses_sysfs_numbers_without_filling_invalid_values() {
        assert_eq!(parse_sysfs_number("123\n"), Some(123));
        assert_eq!(parse_sysfs_number("N/A"), None);
    }
}
