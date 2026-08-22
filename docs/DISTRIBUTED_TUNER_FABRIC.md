# Distributed Tuner Fabric

Status: implementation design for `feat/distributed-tuner-fabric`.

The goal is not merely remote BonDriver access. The fabric must keep viewing
responsive and recordings reliable when RF reception, BonDriver behaviour,
remote hosts, WAN paths, VPNs or tunnels are degraded independently.

## 1. Separate identities from routes

A broadcast service and the way it reaches a tuner are different things.

```text
LogicalService (NID/TSID/SID)
        |
        v
LogicalMux (NID/TSID)
        |
        +-- ReceptionRoute: Gunma UHF xx, direct ISDB-T
        +-- ReceptionRoute: Gunma UHF yy, weak repeater
        +-- ReceptionRoute: Tokyo node, direct ISDB-T
        +-- ReceptionRoute: CATV TSMF carrying logical BS
```

`NID/TSID/SID` is never replaced by a service name or RF channel number as an
identity key. The same mux may have multiple reception routes.

`LogicalBroadcastType` and `DeliveryType` are independent. For example, a BS
mux received through CATV transmodulation is represented as:

```text
logical_broadcast = BS
ultimate_delivery = CATV_TRANSMODULATION / CATV_TSMF
```

This lets channel/EPG identity remain BS while route selection can prefer a
direct satellite reception route and keep CATV as a later fallback.

## 2. Reception qualification

A one-time NID/TSID decode is discovery, not proof that a route is usable.
Physical routes transition through:

```text
Discovered -> Validated -> Usable -> Preferred
                     \-> Degraded -> Quarantined
```

A quarantined route remains in the database and is re-probed at low priority;
it is not deleted. This matters for antenna changes, maintenance, fading and
seasonal changes.

Qualification considers at least:

- PAT/SDT presence and expected NID/TSID
- sustained sample byte count and bitrate
- TEI / continuity-counter / sync error rates
- scramble rate where applicable
- tune latency and first-TS latency
- locally normalized signal quality

Raw `IBonDriver::GetSignalLevel()` values are diagnostic only. They are not
compared between different driver families or sites because BonDriver does not
standardize one common physical meaning for that float.

Promotion/quarantine thresholds and route-switch hysteresis are recisdb policy,
not claimed ARIB thresholds. Defaults must be operator-tunable and learned
against real hardware.

## 3. Site and route groups

A deployment can define groups such as `Kanto` containing Gunma, Tochigi,
Ibaraki, Tokyo, Saitama and Kanagawa nodes. A group limits the search domain;
it does not merge unrelated services.

For a requested NID/TSID, only nodes advertising that logical mux are
candidates. Source selection hard-gates unusable/quarantined routes first, then
considers:

1. already-running identical mux (cheap sharing)
2. reception state / confidence
3. ultimate delivery preference
4. local tuner capacity and load
5. predicted tuner-ready latency
6. normalized source/TS quality
7. network path viability

RF/source quality and network path health are independent failure domains.

## 4. Node transport

The dashboard/Mirakurun HTTP server and the inter-node transport do not share a
listener or connection pool. Node transport has its own listener and is disabled
until configured.

Baseline transport is HTTP/2. It is used as framing/multiplex infrastructure,
not as a REST design constraint and not through gRPC.

- `https://`: TLS (HTTP/2 through ALPN), suitable for direct Internet exposure.
- `http://`: HTTP/2 prior knowledge (h2c), permitted only on an already encrypted
  trusted overlay such as Tailscale or a Cloudflare private network.
- Cloudflare published/public hostname transport is a fallback and bootstrap
  path, not the preferred sustained RECORD path.

Future HTTP/3/QUIC can be negotiated as an optional capability without changing
lease or TS-frame semantics.

Connection policy should eventually maintain separate pools for:

- control/topology/lease renewal
- VIEW
- PREVIEW
- RECORD (prefer dedicated connection per recording)
- active probes

A lossy or congested live-view connection must not consume the flow-control
window used by an unrelated recording.

### 4.1 Pairing (`POST /node/v3/pair`)

Every other node endpoint requires a `NodeCredential` (32 random bytes, hex) in
`Authorization: Bearer` plus `x-recisdb-node-id`. Pairing is the one endpoint
that establishes the first credential, and it is therefore the only one not
authenticated that way.

Being reachable is deliberately **not** sufficient. Sitting on the same
Tailnet, LAN or Cloudflare private network gets a caller to the listener and no
further: without a live one-time code the request is `401`.

Flow:

1. On node A the operator issues a code from the dashboard
   (`POST /api/nodes/pairing`). The plaintext code (64 bits of entropy,
   formatted `XXXX-XXXX-XXXX-XXXX`) is returned **once, in that response**.
   Only its SHA-256 is written to `node_pending_pairings`, so a database dump or
   backup cannot be replayed into a trusted peer, and the dashboard can only
   ever report *that* a code is outstanding and until when.
2. On node B the operator enters A's node-transport URL and the code
   (`POST /api/nodes/pairing/redeem`). B calls A's `POST /node/v3/pair` with its
   own `NodeIdentity` and the endpoints A should use to reach it back.
3. A redeems the code with a single `DELETE ... WHERE code_hash = ? AND
   expires_at_unix_ms > ?`. Because that is one statement, two concurrent
   redemptions cannot both succeed — exactly one sees a non-zero row count.
   A then generates the shared `NodeCredential`, stores B as a `remote_nodes`
   row, trusts it in memory immediately (no restart), and returns
   `PairingAcceptance { identity, credential }`.
4. B stores the same credential against A. Both sides now authenticate with it.

Rules that must not be relaxed:

- TTL is 10 minutes (`PAIRING_CODE_TTL`) and redemption is single-use.
- The credential is never logged, never rendered in `Debug`
  (`NodeCredential`'s impl prints `**redacted**`), and never serialized back to
  the browser. `GET /api/nodes` reports `paired: true/false` only.
- `/node/v3/pair` is rate limited (10 failures/minute) and answers `401`
  identically for a malformed code, an unknown code and an expired one.
- On startup `NodeTransportState::reload_peers()` repopulates the in-memory
  credential map from `remote_nodes`, so pairings survive restarts.

Configuration lives in `[node]` (`recisdb-proxy.toml.example`): `enabled`,
`listen` (h2c — bind to a trusted overlay or loopback), `display_name`.

### 4.2 Leases (`POST /node/v3/lease`)

`node/serve.rs` (supply) and `node/consume.rs` (demand) are the two halves of
using a peer's tuner.

Serving side (`LocalMuxServer::open_lease`):

1. `RequestContext::enter_node` runs first — loop detection, hop cap and the
   shared end-to-end budget. The caller subtracts what it already spent
   (`spent_ms`); a hop never restarts a full timeout.
2. The logical mux is resolved to **every** local physical route and handed to
   `tuner::acquire::acquire` as one request, carrying the peer's
   `EffectiveClaim` verbatim. A remote recording therefore contends with local
   viewers under exactly the same policy — no per-hop reinterpretation.
3. A `RemoteMuxLease` is created and a pump task takes a `TunerSubscription`
   *before* returning, so the reader never looks idle in the window between
   acquire and the task being scheduled.
4. The pump re-aligns broadcast chunks to 188-byte boundaries and publishes
   `NodeTsFrame`s into the lease's replay buffer and live fanout.

The pump follows the RECORD rule: a `broadcast` `Lagged` on a RECORD lease
closes the lease loudly (`record_broadcast_lag`) rather than emitting a stream
with an unannounced hole. VIEW/PREVIEW clear the carry buffer, set
`DISCONTINUITY` on the next frame and continue.

Consuming side (`RemoteMuxStream`):

- Republishes into a plain `broadcast::Sender<Bytes>` — the same shape
  `SharedTuner` hands local consumers, so downstream needs no remote branch.
- Renews at half the lease TTL, and releases explicitly on drop (the TTL is the
  backstop if that request never lands).
- On connection loss it reconnects with `from_seq = last_sequence + 1`. If the
  peer's replay buffer no longer covers that point it answers `410 Gone`:
  **RECORD ends with an error**, VIEW/PREVIEW restart from live.

Status: both halves are implemented and reachable over the transport. What is
still missing is the *arbitration* integration — see §12.

### 4.3 Route advertisement sync (`GET /node/v3/routes`)

`node/advertise.rs` builds what this node offers; `node/sync.rs` exchanges it
on a 60-second tick.

Outbound, one advertisement per (mux, driver, tuning) triple from the enabled
`channels` rows. Three fields that are easy to get wrong:

- `logical_broadcast` comes from the **NID** first (`classify_nid`), so a 4K
  mux is 4K even on a row scanned before band classification existed — nothing
  downstream may run B25 over it (`docs/FOURK_SETUP.md`).
- `ingress_delivery` / `ultimate_delivery` are separate from the logical
  family. A BS mux arriving over CATV is `logical_broadcast: bs` with a CATV
  delivery, never "a CATV channel".
- `available_slots = 0` is still advertised. "Busy" and "cannot receive this"
  are different answers, and a peer needs to tell them apart.

Inbound, each peer's list replaces that peer's rows in `reception_routes`
wholesale — a route it stopped advertising must stop being a candidate. Route
ids are namespaced per node (`<node>::<route_id>`) because two nodes can
legitimately use the same DLL path and tuning. Advertisements that would route
back through this node are dropped by `validate_for` (loop detection).

Non-routable states (`Discovered`, `Quarantined`, `Disabled`) are stored but
never returned as candidates, so a weak or duplicated relay can be re-probed
later instead of being deleted and rediscovered forever (§2).

The stored picture is a **cache, not an authority**: a peer can still refuse
the lease, and `available_slots` may already be stale when it is read.

### 4.4 Using a peer from the HTTP/Mirakurun paths

`channel_resolve::start_source_for_service_with_claim` returns a
`StreamSource`: either a local `SharedTuner` or a `RemoteMuxStream`.

**The remote path is a fallback, never a preference.** It is only tried when
no local tuner could serve the request. A locally receivable channel is never
sent over the network just because a peer also has it — the local path has no
transport failure domain at all.

Peer selection walks the stored advertisements (§4.3) node by node, and within
a node walks endpoints in a static order: LAN, Tailscale, Cloudflare private,
static, direct Internet, Cloudflare public. Two of those rules are not mere
preference:

- RECORD only uses endpoints the operator marked `record_allowed`.
- RECORD refuses `CloudflarePublic` outright — a general-purpose HTTP proxy is
  a bootstrap and fallback path, not a sustained recording path (§4).

The whole search shares one `REMOTE_SEARCH_BUDGET_MS` budget across every peer
and endpoint tried; it is never reset per attempt.

The ordering is static rather than measured because probing costs a round trip
per path and this runs on the request path. `node::path::score_path` is
currently applied only by the dashboard's explicit probe, and moves here once
`node_path_health` is populated continuously.

Downstream is unchanged: `BodyReceiver::Remote` is just a
`broadcast::Receiver<Bytes>`, so service filtering, the EIT gate and the
RECORD `LossPolicy::Fatal` rule all behave identically. The dashboard shows
`node:<peer> (<url>)` in the tuner column so a busy tuner still explains
itself.

## 5. End-to-end request context

Every inter-node acquisition carries one immutable arbitration context:

```text
request_id
trace_id
stream_class
EffectiveClaim { priority, exclusive }
remaining end-to-end deadline
origin_node
visited_nodes
hop_count / max_hops
```

Each hop subtracts time already spent. A remote hop never starts a fresh full
retry budget. Re-visiting a NodeId or exceeding `max_hops` is an immediate route
error.

`EffectiveClaim` is computed once. `exclusive` is a second ranking component;
it is never encoded by replacing priority with `i32::MAX`.

## 6. Transport path selection

A reception route identifies *which source*. A `TransportPath` identifies *how
to reach the same remote source*.

Candidates include:

- LAN
- direct Internet TLS
- Tailscale Direct
- Tailscale Peer Relay
- Tailscale DERP
- Cloudflare private route / mesh-style private path
- Cloudflare published/public tunnel
- operator-specified static endpoint

Path health stores conservative measurements such as RTT p50/p95, p10 and EWMA
throughput, jitter, stall/reconnect rates and confidence. RECORD gives much
more weight to stalls/reconnects and requires bandwidth headroom; VIEW gives
more weight to startup latency and RTT.

Active bandwidth probes are lower priority than useful traffic. Passive
measurements are preferred; full active probes are suppressed while RECORD
traffic is active.

Tailscale/cloudflared are adapters, not hard runtime dependencies. If an adapter
is unavailable, statically configured endpoints keep working.

## 7. Connection-independent RemoteMuxLease

A network connection is not tuner ownership.

```text
Physical tuner -> RemoteMuxLease -> ReplayBuffer -> TransportPath
```

If the active Tailscale path dies, the source node keeps the tuner lease alive
for a bounded grace period while continuing to append RECORD data to its replay
window. The consumer may reconnect to the same lease through Cloudflare/direct
transport.

Node TS frames carry:

```text
generation
sequence
source monotonic timestamp
flags
188-byte-aligned TS payload
```

The reconnecting node requests `from_seq=N`. If the requested generation and
sequence remain in the replay window, the same source can resume without a
silent gap.

Replay limits are both time- and byte-bounded; message count is not a memory
budget.

A generation change means a different source epoch. RECORD must not silently
stitch a different reception route merely because NID/TSID matches. Seamless
cross-source failover would require an explicit TS stitcher handling PCR,
continuity counters, PSI versions and timestamps and is a separate feature.

## 8. Stream classes

### VIEW / PREVIEW

Loss may be tolerated to stay near live edge. A detected node-frame gap marks a
discontinuity and the receiver may continue.

### RECORD

Silent loss is forbidden. A broadcast receiver lag, node-frame sequence gap,
expired replay request or unrecoverable source change terminates the stream
with an explicit error. Upstream EPGStation/Mirakurun can then retry instead of
producing a file that looks successful but contains an unknown hole.

The existing Mirakurun program-recording endpoint follows this rule as well.

## 9. BonDriver health

Packet integrity alone is insufficient. A driver that eventually succeeds but
needs seconds for every open/tune/first-TS must be demoted before users suffer
that latency repeatedly.

Runtime health includes:

- OpenTuner latency/failures
- SetChannel latency/failures
- first-TS latency/timeouts
- no-data stalls
- worker restarts/crashes when worker isolation is enabled

The combined driver score multiplies stream-integrity quality by runtime-health
quality so either kind of failure can demote the route.

### 9.1 Where the numbers come from

- **Startup latency** is measured across `SharedTuner::start_reader` in
  `tuner::acquire` (open + SetChannel + ready, as one number — the DLL does not
  separate them for us).
- **First-TS latency** is recorded by the reader loop on its first chunk. A
  reader that ran its whole life without ever producing TS is recorded as a
  `first_ts_timeout`: "said yes, then delivered nothing" is invisible to packet
  statistics, because there are no packets to be wrong about.
- **Soft stalls** and **hard no-data timeouts** are counted separately by the
  reader watchdog. The soft threshold is half the configured hard timeout,
  clamped to 3–10 s, and each gap counts once rather than once per second — a
  single 30 s outage must not bury a driver's score.
- **Open/tune failures** are written immediately when `start_reader` fails, so
  a driver that cannot start is demoted without waiting for anything.
  `AddrNotAvailable` is classified as a *tuning* failure ("no such channel on
  this driver"), not an unhealthy driver.
- Everything else is written once when the reader stops, because stalls only
  accumulate while it runs.

A driver with no samples scores a neutral `1.0`. Treating "unknown" as "bad"
would make selection avoid every newly added tuner.

Long term, untrusted/poor native BonDriver code should run in a supervisor-owned
worker process. Tokio blocking-task timeouts can abandon a waiter but cannot
force a hung native DLL callback to return; a process boundary is required for
hard containment.

## 10. GUI principles

Normal setup should require only:

1. Add/pair node (code/QR)
2. Give it a display/site name
3. Optionally add it to a route group such as `Kanto`
4. Leave transport selection on `Auto`

The detail screen may show discovered paths, current transport, RTT, conservative
bandwidth, stability, Tailscale Direct/PeerRelay/DERP state, and source/tuner
health. Operator controls are overrides (`disable path`, `record not allowed`,
`metered`, `priority`) rather than overwriting automatic measurements.

## 11. Failure-domain rule

Never punish a reception route for an unrelated network failure.

```text
SourceHealth    RF / TS / BonDriver
NodeHealth      process / CPU / memory / workers
TransportHealth direct / Tailscale / Cloudflare path
```

Example: a Cloudflare path to Gunma can be Degraded while the Gunma UHF route
remains Healthy. Route learning must preserve that distinction.

## 12. Implementation status

The feature branch introduces domain types, route/path selection, qualification,
HTTP/2 node transport, application-level node credentials, Tailscale probing,
connection-independent remote leases, sequenced framing, RECORD replay and
SQLite fabric tables. Existing tuner/session/Mirakurun acquisition paths are
being migrated incrementally so the critical local tuner path remains usable at
each commit.

Done:

- Node domain types, store schema, path scoring, qualification, replay/framing.
- Node transport listener (`[node]` in the config file), wired into `main.rs`
  and served on its own port.
- One-time pairing (§4.1), dashboard API and the 分散ノード dashboard tab,
  including issuing/redeeming codes and per-class path probes.
- Central arbitration correctness the fabric depends on: a single canonical
  `EffectiveClaim` per request, all-candidates acquire, no caller-side
  preselection, `Stopping` never reused, RAII slot-permit transfer.

- Lease supply and demand (§4.2): `POST /node/v3/lease`, the local pump, and
  `RemoteMuxStream` with renew / reconnect / `from_seq` resume and the RECORD
  no-silent-gap rule.
- Route advertisement build/exchange/persistence (§4.3).
- Driver runtime health is now actually measured and fed into selection
  (§9): startup latency, first-TS latency, soft stalls, hard no-data timeouts
  and open/tune failures. Before this the table existed and `tuner::policy`
  already multiplied by `runtime_score`, but nothing ever wrote a sample, so
  the score was permanently `1.0`.
- Remote fallback for `GET /mirakurun/api/services/:id/stream` and
  `/programs/:id/stream` (§4.4), including per-class endpoint admission.

Not done yet:

- **Local and remote are not ranked together.** Remote is a fallback tried
  after local arbitration fails, not a candidate `tuner::policy::decide` weighs
  against local drivers. Ranking them in one place needs a single lease type
  covering both a local `SharedTuner` and a remote stream (the TunerManager
  step below). Until then a healthy peer cannot beat a marginal local tuner.
- **BNDP sessions (TVTest/EDCB) have no remote fallback.** Only the
  HTTP/Mirakurun paths do; `server/session.rs` still resolves to a
  `SharedTuner` directly.
- **Path selection on the request path is static, not measured** (§4.4).
- Route-group weights are stored but not consulted during candidate
  generation.
- **No circuit breaker.** `open_backoff` cools a driver down after repeated
  open failures, but there is no Healthy → Degraded → Open → HalfOpen state
  machine, and no separate soft/hard deadline for `Open`/`SetChannel`/`Stop`.
- **No BonDriver process isolation** (`recisdb-driver-worker`,
  `DriverSupervisor`). `spawn_blocking` + timeout cannot kill a thread stuck
  inside a native DLL call, and a DLL crash still takes the proxy with it.
