//! Runtime discovery/probing for node transport paths.
//!
//! External networking products are adapters, not hard dependencies. If
//! Tailscale/cloudflared are absent the corresponding discovery simply yields
//! no endpoint; statically configured endpoints keep working.

use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::process::Command;

use super::path::{PathHealth, PathState, TransportPath};
use super::transport::NodeTransportClient;
use super::types::{EndpointKind, NodeEndpoint, TailscalePathKind};

#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub ping_samples: usize,
    pub download_samples: usize,
    pub download_bytes: usize,
    pub command_timeout: Duration,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            ping_samples: 4,
            download_samples: 3,
            download_bytes: 4 * 1024 * 1024,
            command_timeout: Duration::from_secs(3),
        }
    }
}

/// Discover the local Tailscale address advertised by `tailscale status
/// --json`. The generated endpoint uses h2c because the WireGuard/Tailscale
/// overlay already provides encryption and peer authentication; recisdb still
/// applies its own application-level node credential on top.
pub async fn discover_tailscale_endpoint(node_port: u16) -> Option<NodeEndpoint> {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new("tailscale")
            .args(["status", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let json: Value = serde_json::from_slice(&output.stdout).ok()?;
    let ip = json
        .get("Self")?
        .get("TailscaleIPs")?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .find(|ip| ip.contains(':'))
        .or_else(|| {
            json.get("Self")?
                .get("TailscaleIPs")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .next()
        })?;
    let address = if ip.contains(':') {
        format!("http://[{ip}]:{node_port}")
    } else {
        format!("http://{ip}:{node_port}")
    };
    Some(NodeEndpoint {
        kind: EndpointKind::Tailscale,
        address,
        enabled: true,
        record_allowed: true,
        metered: false,
        user_priority: 0,
    })
}

/// Parse `tailscale ping` output without depending on a particular tailscaled
/// JSON schema. Unknown wording remains Unknown and is scored conservatively.
pub fn classify_tailscale_ping(output: &str) -> TailscalePathKind {
    let lower = output.to_ascii_lowercase();
    if lower.contains("derp(") || lower.contains(" via derp") {
        TailscalePathKind::Derp
    } else if lower.contains("peer-relay") || lower.contains("peer relay") {
        TailscalePathKind::PeerRelay
    } else if lower.contains(" via ") && (lower.contains("ms") || lower.contains("pong")) {
        TailscalePathKind::Direct
    } else {
        TailscalePathKind::Unknown
    }
}

pub async fn inspect_tailscale_path(target: &str, timeout: Duration) -> TailscalePathKind {
    let result = tokio::time::timeout(
        timeout,
        Command::new("tailscale")
            .args(["ping", "--c", "1", target])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output(),
    )
    .await;
    let Ok(Ok(output)) = result else {
        return TailscalePathKind::Unknown;
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    classify_tailscale_ping(&text)
}

/// Active probe used only when passive stream measurements are stale. The
/// caller is responsible for suppressing this while RECORD traffic is active.
pub async fn probe_endpoint(
    client: &NodeTransportClient,
    endpoint: NodeEndpoint,
    config: ProbeConfig,
) -> TransportPath {
    let mut rtts = Vec::new();
    let mut successful_pings = 0usize;
    for i in 0..config.ping_samples.max(1) {
        let started = Instant::now();
        if client
            .ping(&endpoint.address, &format!("probe-{i}"))
            .await
            .is_ok()
        {
            successful_pings += 1;
            rtts.push(started.elapsed().as_secs_f64() * 1000.0);
        }
    }
    rtts.sort_by(|a, b| a.total_cmp(b));

    let mut throughputs = Vec::new();
    for _ in 0..config.download_samples {
        if let Ok((bytes, elapsed)) = client
            .probe_download(&endpoint.address, config.download_bytes)
            .await
        {
            let seconds = elapsed.as_secs_f64().max(0.000_001);
            throughputs.push((bytes as f64 * 8.0 / seconds) as u64);
        }
    }
    throughputs.sort_unstable();

    let success_rate = successful_pings as f64 / config.ping_samples.max(1) as f64;
    let percentile = |values: &[f64], p: f64| -> f64 {
        if values.is_empty() {
            return f64::INFINITY;
        }
        let idx = ((values.len() - 1) as f64 * p).round() as usize;
        values[idx.min(values.len() - 1)]
    };
    let p10_bps = if throughputs.is_empty() {
        0
    } else {
        let idx = ((throughputs.len() - 1) as f64 * 0.10).floor() as usize;
        throughputs[idx]
    };
    let ewma_bps = if throughputs.is_empty() {
        0
    } else {
        throughputs.iter().copied().sum::<u64>() / throughputs.len() as u64
    };
    let rtt_p50 = percentile(&rtts, 0.50);
    let rtt_p95 = percentile(&rtts, 0.95);
    let jitter = if rtts.len() < 2 {
        0.0
    } else {
        let mean = rtts.iter().sum::<f64>() / rtts.len() as f64;
        (rtts.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / rtts.len() as f64).sqrt()
    };

    let state = if successful_pings == 0 {
        PathState::Unreachable
    } else if success_rate < 0.75 || p10_bps == 0 {
        PathState::Degraded
    } else {
        PathState::Healthy
    };

    let tailscale_path = if endpoint.kind == EndpointKind::Tailscale {
        // Address may be http://IP:port; tailscale ping accepts the host/IP.
        let host = endpoint
            .address
            .trim_start_matches("http://")
            .trim_start_matches("https://")
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(&endpoint.address)
            .split(':')
            .next()
            .unwrap_or(&endpoint.address);
        Some(inspect_tailscale_path(host, config.command_timeout).await)
    } else {
        None
    };

    TransportPath {
        id: format!("{:?}:{}", endpoint.kind, endpoint.address),
        endpoint,
        health: PathHealth {
            state,
            connect_success_rate: success_rate,
            rtt_p50_ms: rtt_p50,
            rtt_p95_ms: rtt_p95,
            throughput_down_p10_bps: p10_bps,
            throughput_down_ewma_bps: ewma_bps,
            jitter_ms: jitter,
            stall_rate: 0.0,
            reconnect_rate: 0.0,
            confidence: ((successful_pings + throughputs.len()) as f64
                / (config.ping_samples.max(1) + config.download_samples) as f64)
                .clamp(0.0, 1.0),
            tailscale_path,
            measured_at_unix_ms: chrono::Utc::now().timestamp_millis(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_path_output_is_classified() {
        assert_eq!(
            classify_tailscale_ping("pong from gunma via DERP(tok) in 32ms"),
            TailscalePathKind::Derp
        );
        assert_eq!(
            classify_tailscale_ping("pong from gunma via peer-relay(node) in 18ms"),
            TailscalePathKind::PeerRelay
        );
        assert_eq!(
            classify_tailscale_ping("pong from gunma via 192.0.2.1:41641 in 10ms"),
            TailscalePathKind::Direct
        );
    }
}
