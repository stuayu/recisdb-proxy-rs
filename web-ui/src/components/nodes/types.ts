export type EndpointKind =
  'lan' | 'internet_direct' | 'tailscale' | 'cloudflare_private' | 'cloudflare_public' | 'static'

export type NodeEndpoint = {
  kind: EndpointKind
  address: string
  enabled: boolean
  record_allowed: boolean
  metered: boolean
  user_priority: number
}

export type StoredNode = {
  node_id: string
  display_name: string
  site_name: string | null
  enabled: boolean
  allow_transit: boolean
  auto_connect: boolean
  last_seen_unix_ms: number | null
}

export type NodeEntry = {
  node: StoredNode
  endpoints: NodeEndpoint[]
  paired: boolean
  routable_routes: number
  total_routes: number
}
export type RouteGroup = {
  id: number
  name: string
  members?: Array<{ node_id: string; weight: number }>
}
export type SetupStatus = {
  id: string
  label: string
  state: 'done' | 'warn' | 'todo'
  action: string | null
}
export type Topology = {
  local: { node_id: string; display_name: string }
  nodes: StoredNode[]
  paths: Array<{
    from: string
    to: string
    kind: EndpointKind
    status: 'online' | 'unavailable'
    role: string
    record_allowed: boolean
  }>
}
export type NodesResponse = {
  success: boolean
  local: { node_id: string; display_name: string }
  nodes: NodeEntry[]
  route_groups: RouteGroup[]
  setup_status: SetupStatus[]
  topology: Topology
  pending_pairings: Array<{ expires_at_unix_ms: number; label: string | null }>
}
export type IssuedPairing = {
  success: boolean
  code: string
  expires_at_unix_ms: number
  ttl_secs: number
  label: string | null
  node_listen_addr: string | null
}
export type ProbePath = {
  id: string
  endpoint: NodeEndpoint
  health: {
    state: string
    connect_success_rate: number
    rtt_p50_ms: number
    rtt_p95_ms: number
    throughput_down_p10_bps: number
    throughput_down_ewma_bps: number
    jitter_ms: number
    stall_rate: number
    reconnect_rate: number
    confidence: number
    tailscale_path: string | null
  }
}
export type ProbeResponse = {
  success: boolean
  bitrate_bps: number
  paths: ProbePath[]
  selected: { view: string | null; preview: string | null; record: string | null }
}
