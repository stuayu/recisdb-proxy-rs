//! Runtime discovery/probing for node transport paths.
//!
//! External networking products are adapters, not hard dependencies. If
//! Tailscale/cloudflared are absent the corresponding discovery simply yields
//! no endpoint; statically configured endpoints keep working.

use std::net::{IpAddr, SocketAddr};
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
    let ips = discover_tailscale_ips().await?;
    let ip = ips.iter().find(|ip| ip.is_ipv6()).or_else(|| ips.first())?;
    let ip = ip.to_string();
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

/// Discover every local address that Tailscale reports for this node.
/// `tailscale status --json` is optional; an absent CLI simply produces no
/// overlay-specific candidates and interface discovery still runs.
async fn discover_tailscale_ips() -> Option<Vec<IpAddr>> {
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
    Some(
        json.get("Self")?
            .get("TailscaleIPs")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .filter_map(|value| value.parse().ok())
            .collect(),
    )
}

/// Build node URLs from the listener and discovered interface addresses.
/// This pure part is deliberately separate so wildcard handling and ordering
/// can be tested without depending on the host's network interfaces.
pub fn build_advertised_endpoints(
    listen_addr: SocketAddr,
    interface_addresses: impl IntoIterator<Item = IpAddr>,
) -> Vec<NodeEndpoint> {
    let addresses = if listen_addr.ip().is_unspecified() {
        // `0.0.0.0` only accepts IPv4; advertising the host's IPv6 addresses
        // would hand the peer endpoints that can never connect. `::` may be
        // dual-stack, so it keeps both families.
        let v4_only = listen_addr.is_ipv4();
        interface_addresses
            .into_iter()
            .filter(|ip| !v4_only || ip.is_ipv4())
            .collect::<Vec<_>>()
    } else {
        vec![listen_addr.ip()]
    };

    let mut unique = std::collections::BTreeSet::new();
    let mut endpoints = addresses
        .into_iter()
        .filter(|ip| is_advertisable_ip(*ip))
        .filter(|ip| unique.insert(*ip))
        .map(|ip| {
            let kind = endpoint_kind(ip);
            NodeEndpoint {
                kind,
                address: format_node_url(ip, listen_addr.port()),
                enabled: true,
                // A plain HTTP endpoint outside a trusted overlay is useful
                // for bootstrap/probing, but must not become a default RECORD
                // path until the operator explicitly configures it.
                record_allowed: !matches!(kind, EndpointKind::InternetDirect),
                metered: false,
                user_priority: 0,
            }
        })
        .collect::<Vec<_>>();
    endpoints.sort_by_key(|endpoint| endpoint_rank(endpoint.kind));
    endpoints
}

/// Discover interface addresses using commands already present on the host.
/// This avoids adding a platform-specific interface crate to the proxy.
pub async fn discover_advertised_endpoints(listen_addr: SocketAddr) -> Vec<NodeEndpoint> {
    // Called from `/node/v3/hello` and the dashboard's `/api/nodes` poll;
    // spawning ipconfig/tailscale on every call is too heavy. Addresses
    // change rarely, so a short-lived cache is enough.
    const TTL: Duration = Duration::from_secs(60);
    static CACHE: std::sync::Mutex<Option<(Instant, SocketAddr, Vec<NodeEndpoint>)>> =
        std::sync::Mutex::new(None);
    if let Ok(cache) = CACHE.lock() {
        if let Some((at, addr, endpoints)) = cache.as_ref() {
            if *addr == listen_addr && at.elapsed() < TTL {
                return endpoints.clone();
            }
        }
    }
    let endpoints = discover_advertised_endpoints_uncached(listen_addr).await;
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((Instant::now(), listen_addr, endpoints.clone()));
    }
    endpoints
}

async fn discover_advertised_endpoints_uncached(listen_addr: SocketAddr) -> Vec<NodeEndpoint> {
    let mut addresses = if listen_addr.ip().is_unspecified() {
        discover_interface_addresses().await
    } else {
        Vec::new()
    };
    if listen_addr.ip().is_unspecified() {
        if let Some(tailscale_ips) = discover_tailscale_ips().await {
            addresses.extend(tailscale_ips);
        }
    }
    build_advertised_endpoints(listen_addr, addresses)
}

async fn discover_interface_addresses() -> Vec<IpAddr> {
    // Route-based probing first: works on every OS and does not depend on
    // localized command output (Japanese `ipconfig` is CP932, which
    // `from_utf8_lossy` turns into U+FFFD so its address lines are lost).
    let mut addresses = route_source_addresses();
    addresses.extend(command_interface_addresses().await);
    addresses
}

/// Local source address the OS would pick to reach each probe target.
/// `UdpSocket::connect` only selects a route; no packet is sent.
/// 100.100.100.100 is Tailscale's in-tailnet service address, so its route
/// yields the Tailscale IP when Tailscale is up; the public ones yield the
/// default-route LAN address.
fn route_source_addresses() -> Vec<IpAddr> {
    const TARGETS: [&str; 3] = ["100.100.100.100:53", "8.8.8.8:53", "[2001:4860:4860::8888]:53"];
    TARGETS
        .iter()
        .filter_map(|target| {
            let bind = if target.starts_with('[') { "[::]:0" } else { "0.0.0.0:0" };
            let socket = std::net::UdpSocket::bind(bind).ok()?;
            socket.connect(target).ok()?;
            socket.local_addr().ok().map(|addr| addr.ip())
        })
        .collect()
}

async fn command_interface_addresses() -> Vec<IpAddr> {
    let commands: &[(&str, &[&str])] = if cfg!(target_os = "windows") {
        &[("ipconfig", &[])]
    } else if cfg!(target_os = "linux") {
        &[("ip", &["-o", "addr", "show"]), ("ifconfig", &[])]
    } else {
        &[("ifconfig", &[])]
    };

    for (program, args) in commands {
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            Command::new(program)
                .args(*args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output(),
        )
        .await;
        if let Ok(Ok(output)) = result {
            if output.status.success() {
                let addresses = parse_interface_addresses(&String::from_utf8_lossy(&output.stdout));
                if !addresses.is_empty() {
                    return addresses;
                }
            }
        }
    }
    Vec::new()
}

/// Parse the address-bearing portions of common `ifconfig`, `ip -o addr`, and
/// Windows `ipconfig` output. Unknown text is ignored.
pub fn parse_interface_addresses(output: &str) -> Vec<IpAddr> {
    let mut addresses = Vec::new();
    for line in output.lines() {
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        let windows_line = line.contains("Address") || line.contains("アドレス");
        for (index, token) in tokens.iter().enumerate() {
            let candidate = if *token == "inet" || *token == "inet6" {
                tokens.get(index + 1).copied()
            } else if windows_line {
                Some(*token)
            } else {
                None
            };
            let Some(candidate) = candidate else { continue };
            let candidate = candidate
                .trim_matches(|ch: char| matches!(ch, ':' | ','))
                .split('/')
                .next()
                .unwrap_or_default()
                .split('%')
                .next()
                .unwrap_or_default();
            if let Ok(ip) = candidate.parse::<IpAddr>() {
                addresses.push(ip);
            }
        }
    }
    addresses
}

fn is_advertisable_ip(ip: IpAddr) -> bool {
    !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast() && !is_link_local(ip)
}

fn is_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.octets()[0..2] == [169, 254],
        IpAddr::V6(ip) => ip.segments()[0] & 0xffc0 == 0xfe80,
    }
}

fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            octets[0] == 100 && (64..=127).contains(&octets[1])
        }
        IpAddr::V6(ip) => {
            let segments = ip.segments();
            segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
        }
    }
}

fn is_private_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private(),
        IpAddr::V6(ip) => ip.segments()[0] & 0xfe00 == 0xfc00,
    }
}

fn endpoint_kind(ip: IpAddr) -> EndpointKind {
    if is_tailscale(ip) {
        EndpointKind::Tailscale
    } else if is_private_lan(ip) {
        EndpointKind::Lan
    } else {
        EndpointKind::InternetDirect
    }
}

fn endpoint_rank(kind: EndpointKind) -> u8 {
    match kind {
        EndpointKind::Tailscale => 0,
        EndpointKind::Lan => 1,
        _ => 2,
    }
}

fn format_node_url(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(ip) => format!("http://{ip}:{port}"),
        IpAddr::V6(ip) => format!("http://[{ip}]:{port}"),
    }
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
    // No ping got through: the endpoint is unreachable, and downloads would
    // only add more connect timeouts.
    let download_samples = if successful_pings == 0 { 0 } else { config.download_samples };
    for _ in 0..download_samples {
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
    fn wildcard_listen_never_becomes_an_advertised_url() {
        let endpoints = build_advertised_endpoints(
            "0.0.0.0:40071".parse().unwrap(),
            [
                "0.0.0.0".parse().unwrap(),
                "127.0.0.1".parse().unwrap(),
                "169.254.1.2".parse().unwrap(),
                "100.64.0.3".parse().unwrap(),
                "192.168.1.10".parse().unwrap(),
                "8.8.8.8".parse().unwrap(),
            ],
        );
        assert_eq!(
            endpoints
                .iter()
                .map(|endpoint| endpoint.address.as_str())
                .collect::<Vec<_>>(),
            vec![
                "http://100.64.0.3:40071",
                "http://192.168.1.10:40071",
                "http://8.8.8.8:40071",
            ]
        );
        assert!(endpoints
            .iter()
            .all(|endpoint| !endpoint.address.contains("0.0.0.0")));
    }

    #[test]
    fn ipv4_wildcard_listen_does_not_advertise_ipv6() {
        let addresses: Vec<IpAddr> = vec![
            "fd7a:115c:a1e0::7e01:9c30".parse().unwrap(),
            "100.64.0.3".parse().unwrap(),
        ];
        let v4 = build_advertised_endpoints("0.0.0.0:40071".parse().unwrap(), addresses.clone());
        assert_eq!(
            v4.iter().map(|e| e.address.as_str()).collect::<Vec<_>>(),
            vec!["http://100.64.0.3:40071"]
        );
        let dual = build_advertised_endpoints("[::]:40071".parse().unwrap(), addresses);
        assert_eq!(dual.len(), 2);
    }

    #[test]
    fn specific_listen_ip_is_the_only_candidate() {
        let endpoints = build_advertised_endpoints(
            "192.168.1.10:40071".parse().unwrap(),
            [
                "100.64.0.3".parse().unwrap(),
                "192.168.1.11".parse().unwrap(),
            ],
        );
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].address, "http://192.168.1.10:40071");
    }

    #[test]
    fn interface_parser_ignores_broadcast_and_scope_metadata() {
        let addresses = parse_interface_addresses(
            "en0: flags=...\n    inet 192.168.1.10 netmask 0xffffff00 broadcast 192.168.1.255\n    inet6 fe80::1%en0 prefixlen 64\n",
        );
        assert_eq!(
            addresses,
            vec![
                "192.168.1.10".parse::<IpAddr>().unwrap(),
                "fe80::1".parse::<IpAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn tailscale_path_output_is_classified() {
        assert_eq!(
            classify_tailscale_ping("pong from site-a via DERP(relay) in 32ms"),
            TailscalePathKind::Derp
        );
        assert_eq!(
            classify_tailscale_ping("pong from site-a via peer-relay(node) in 18ms"),
            TailscalePathKind::PeerRelay
        );
        assert_eq!(
            classify_tailscale_ping("pong from site-a via 192.0.2.1:41641 in 10ms"),
            TailscalePathKind::Direct
        );
    }
}
