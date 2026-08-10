//! Mirakurun-compatible API subset (STREAMING_DESIGN.md §7.1, Phase P6).
//!
//! Mounted at `/mirakurun/api/*` — deliberately **not** under `/api/*` — for
//! two independent reasons (see `web/mod.rs::build_app`):
//!
//! 1. **Namespace collision**: Mirakurun's real paths (`/api/services`,
//!    `/api/channels`, `/api/version`, ...) collide with this project's own
//!    dashboard API of the same names but a different JSON shape.
//! 2. **Authentication mismatch**: real Mirakurun clients (EPGStation/mirakc/
//!    KonomiTV) never send an `Authorization` header — the reference
//!    implementation has no auth at all. Mounting this subset behind
//!    `require_auth` (like every other `/api/*` route, STREAMING_DESIGN.md
//!    §6.5) would make it unusable out of the box. So this router is built
//!    and nested *without* the auth middleware, and is **opt-in** —
//!    `[mirakurun] enabled = false` by default (`main.rs`). `web_listen`
//!    still defaults to `127.0.0.1`, so turning this on does not, by itself,
//!    expose an unauthenticated endpoint beyond localhost; exposing it
//!    further is an explicit operator choice (`--web-listen 0.0.0.0:...`)
//!    already covered by the existing startup WARN for that case. Enabling
//!    `[mirakurun] enabled = true` additionally logs its own startup WARN
//!    (see `main.rs`) since this endpoint is unauthenticated regardless of
//!    `web_listen`.
//!
//! # Scope (what this subset intentionally does not implement)
//! - `GET /programs` (EPG) IS implemented, reading from the `programs`
//!   table (Migration 015) populated by `crate::epg_writer::EpgWriter` from
//!   live EIT collection (`tuner/epg_collector.rs`) — this used to be fully
//!   out of scope per STREAMING_DESIGN.md §7.1's original P6 note ("まずは
//!   視聴系のみのサブセットで良い"), but EPG storage/collection was added
//!   later. `/schedules` and other EPG-adjacent endpoints (recording rules,
//!   etc.) remain unimplemented. `isFree` is always reported `true` — the
//!   `programs` table does not store `free_CA_mode` (see `web/mirakurun.rs`
//!   `get_programs` doc comment for why).
//! - `GET /docs` ([`crate::web::mirakurun_docs`]) IS implemented: the
//!   `mirakurun` npm client used by EPGStation resolves every API call
//!   through this OpenAPI (Swagger 2.0) document (operationId → method/path/
//!   parameters), so without it none of the other endpoints below are ever
//!   reachable from that client, even though their HTTP routes exist. See
//!   `docs/EPGSTATION_COMPAT.md` §1.
//! - `GET /tuners` and `GET /config/server` ARE implemented ([`get_tuners`],
//!   [`get_server_config`]), but with several fields hardcoded to
//!   type-appropriate defaults where this project has no equivalent concept
//!   (per-tuner `pid`/`command`, `ConfigServer`'s job-scheduler/log-history
//!   knobs, ...) — see each function's doc comment for exactly which fields.
//! - `GET /programs/:id/stream` ([`stream_program_by_mirakurun_id`]) IS
//!   implemented — EPG-reservation recording (`docs/EPGSTATION_COMPAT.md`
//!   §3/§5) depends on it. It waits to emit TS data until the target event
//!   is observed as EIT[p/f] "present" on its service
//!   ([`crate::web::mirakurun_program_stream::ProgramGate`]), then streams
//!   raw passthrough (same convention as every other stream endpoint here)
//!   until a different event becomes present. `GET /events/stream`
//!   ([`crate::web::mirakurun_events`]) IS implemented: incremental
//!   `program` UPSERT notifications only (no `service`/`tuner` events — see
//!   that module's doc comment for why), sourced from
//!   `crate::epg_writer::EpgWriter` via a `broadcast` channel wired up in
//!   `main.rs`.
//! - No tuner/recording-process introspection beyond `/status`'s coarse
//!   tuner counts (real Mirakurun's `/status` reports process RSS, EPG gather
//!   progress, RPC/stream/error counters, etc. — none of that exists here).
//! - `/services` and `/channels` only list channels that are `is_enabled`
//!   **and** have a scanned physical assignment (`bon_channel` present) — an
//!   unscanned row has no meaningful `channel` string to report and nothing
//!   to stream.
//! - Service logos (`GET /api/services/:id/logo`, `logoId`/`hasLogoData`) are
//!   not implemented; `hasLogoData` is always reported `false`. This
//!   project's own `/logos/:file` convention (`<nid>_<sid>.png`) is not
//!   wired up to the Mirakurun logo endpoint.
//! - `X-Mirakurun-Priority` (sent on both stream endpoints, real and EPG-
//!   Station-specific: `recPriority`/`conflictPriority`/`streamingPriority`)
//!   is accepted and parsed but **not** fed into tuner-contention decisions
//!   (`tuner/policy.rs::decide()`) — that requires a design decision outside
//!   this pass's scope. See [`stream_service_by_mirakurun_id`].
//!
//! # Data mapping
//!
//! - **Service id**: Mirakurun's convention is
//!   `id = networkId * 100000 + serviceId` — EPGStation relies on being able
//!   to invert this to look a service back up, so both directions
//!   ([`mirakurun_service_id`] / [`split_mirakurun_service_id`]) are unit
//!   tested for round-tripping.
//! - **`BandType` → Mirakurun channel `type`** ([`band_type_to_mirakurun`]):
//!   `Terrestrial → "GR"`, `BS → "BS"`, `CS → "CS"`, `SKY → "SKY"`. This
//!   crate's `BandType` (`recisdb_protocol::types`) has two more variants
//!   than Mirakurun's `type` enum, bucketed as a documented simplification:
//!   - `FourK → "BS"`: advanced-BS/CS4K has no distinct Mirakurun type in
//!     this subset (real Mirakurun forks disagree on whether a `"BS4K"` type
//!     even exists); 4K services are overwhelmingly BS-delivered in practice
//!     so `"BS"` is the closer bucket than `"CS"`.
//!   - `CATV → "GR"`, `Other → "SKY"`: neither has a faithful Mirakurun
//!     equivalent; these are best-effort buckets purely so the row is not
//!     silently dropped.
//!   The reverse mapping used by `GET /channels/:type/:channel/stream`
//!   ([`mirakurun_type_to_band_candidates`]) is therefore one-to-many (e.g.
//!   `type=BS` matches both `BandType::BS` and `BandType::FourK` rows).
//! - **`channel` string**: `physical_ch` for terrestrial rows when present,
//!   else `bon_channel`, rendered as a decimal string. Real Mirakurun encodes
//!   satellite channels as e.g. `"BS15_0"` (transponder + slot); reproducing
//!   that convention exactly is out of scope here — EPGStation/KonomiTV
//!   primarily key off `id`/`serviceId` for streaming and only display the
//!   `channel` string, so this simplification does not block the "視聴が通
//!   る" goal.
//!
//! # Not runtime-verified
//! No BonDriver hardware and no real Mirakurun client (EPGStation/mirakc/
//! KonomiTV) is available in this environment (b25-sys does not link here —
//! see task constraints), so none of this has been exercised against an
//! actual client. Coverage is limited to unit/integration tests against an
//! in-memory database and `tower::ServiceExt::oneshot`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use log::debug;
use serde::Serialize;
use serde_json::json;

use crate::database::{ChannelRecord, Database, ProgramRecord, ProgramUpsert};
use crate::server::channel_resolve;
use crate::web::mirakurun_program_stream;
use crate::web::state::WebState;
use crate::web::stream::{
    broadcast_to_body_stream, channel_resolve_error_response, error_response, respond_with_stream,
    BodyReceiver, StreamCleanup,
};
use recisdb_protocol::BandType;

// ============================================================================
// Service id <-> (nid, sid) conversion
// ============================================================================

/// `id = networkId * 100000 + serviceId` — Mirakurun's service-id
/// convention. `nid`/`sid` are both `u16` (max 65535), so the product always
/// fits comfortably in `u64` (max representable id ~6.5 billion, versus
/// `u64::MAX`).
pub fn mirakurun_service_id(nid: u16, sid: u16) -> u64 {
    nid as u64 * 100_000 + sid as u64
}

/// Inverse of [`mirakurun_service_id`]. Returns `None` if `id` does not
/// decode back into two values that fit in `u16` — i.e. it wasn't produced
/// by this scheme (or is corrupt/attacker-supplied garbage), so callers can
/// reject it as a 400 rather than querying the database with truncated
/// values.
pub fn split_mirakurun_service_id(id: u64) -> Option<(u16, u16)> {
    let nid = id / 100_000;
    let sid = id % 100_000;
    if nid > u16::MAX as u64 || sid > u16::MAX as u64 {
        return None;
    }
    Some((nid as u16, sid as u16))
}

// ============================================================================
// BandType <-> Mirakurun channel `type` mapping
// ============================================================================

/// Reconstruct `BandType` from the raw `channels.band_type` column (stored
/// as the enum's `u8` discriminant — see `database/channel.rs::insert_channel`).
/// `recisdb_protocol::BandType` has no `TryFrom<u8>`/`From<u8>` impl today,
/// so this mirrors its discriminants by hand; unknown/`NULL` values fall
/// back to `Other`, matching how the rest of the dashboard treats an
/// unclassified band.
fn band_type_from_db(v: Option<u8>) -> BandType {
    match v {
        Some(0) => BandType::Terrestrial,
        Some(1) => BandType::BS,
        Some(2) => BandType::CS,
        Some(3) => BandType::FourK,
        Some(5) => BandType::CATV,
        Some(6) => BandType::SKY,
        _ => BandType::Other,
    }
}

/// `BandType` → Mirakurun `type` string. See the module doc comment for the
/// documented, lossy `FourK`/`CATV`/`Other` bucketing.
fn band_type_to_mirakurun(bt: BandType) -> &'static str {
    match bt {
        BandType::Terrestrial => "GR",
        BandType::BS | BandType::FourK => "BS",
        BandType::CS => "CS",
        BandType::CATV => "GR",
        BandType::SKY | BandType::Other => "SKY",
    }
}

/// Mirakurun `type` string → candidate `BandType`s to search when resolving
/// `GET /channels/:type/:channel/stream` — necessarily one-to-many, the
/// reverse of [`band_type_to_mirakurun`]. `None` for an unrecognized type
/// string.
fn mirakurun_type_to_band_candidates(t: &str) -> Option<&'static [BandType]> {
    match t {
        "GR" => Some(&[BandType::Terrestrial, BandType::CATV]),
        "BS" => Some(&[BandType::BS, BandType::FourK]),
        "CS" => Some(&[BandType::CS]),
        "SKY" => Some(&[BandType::SKY, BandType::Other]),
        // Accepted on input only, never produced by `band_type_to_mirakurun`.
        //
        // MMirakurun (otya128), the 4K-capable fork, does define `BS4K` as a
        // real ChannelType, so a client written against it would otherwise get
        // an empty list from us. Answering it costs nothing and is purely
        // additive.
        //
        // We still advertise 4K as `BS`: the `api.d.ts` that EPGStation
        // actually compiles against declares
        // `"GR" | "BS" | "CS" | "SKY" | "NW1".."NW40"` with no `BS4K`
        // (`node_modules/mirakurun/api.d.ts:48-52`), and its
        // `ChannelDB.getChannelTypeId` funnels anything unrecognised into a
        // single catch-all bucket (`src/model/db/ChannelDB.ts:169`, `default:
        // return 44`). Emitting a type outside the union would be off-contract
        // for the client we care about.
        "BS4K" => Some(&[BandType::FourK]),
        _ => None,
    }
}

/// The Mirakurun `channel` string reported for a row. See module doc comment.
fn channel_string(record: &ChannelRecord) -> Option<String> {
    match band_type_from_db(record.band_type) {
        BandType::Terrestrial => record
            .physical_ch
            .map(|c| c.to_string())
            .or_else(|| record.bon_channel.map(|c| c.to_string())),
        _ => record.bon_channel.map(|c| c.to_string()),
    }
}

// ============================================================================
// JSON response types (Mirakurun-shaped subset — see module doc comment for
// which real Mirakurun fields are omitted)
// ============================================================================

/// `GET /channels` element. Mirakurun's real `Channel` type carries several
/// more fields (`satelite`, `space`, etc. depending on fork); only what
/// EPGStation/KonomiTV need to discover and identify a multiplex is
/// implemented here.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunChannel {
    #[serde(rename = "type")]
    pub channel_type: String,
    pub channel: String,
    pub name: String,
    pub services: Vec<MirakurunServiceSummary>,
}

/// A service as embedded in a [`MirakurunChannel`]'s `services` array —
/// intentionally a smaller subset of fields than the flat [`MirakurunService`]
/// returned by `GET /services` (matches real Mirakurun, whose embedded
/// service summaries are also a subset of the full service object).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunServiceSummary {
    pub id: u64,
    pub service_id: u16,
    pub network_id: u16,
    pub name: String,
}

/// `GET /services` element.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunService {
    pub id: u64,
    pub service_id: u16,
    pub network_id: u16,
    pub name: String,
    /// ARIB service type (`channels.service_type`); Mirakurun's own field is
    /// also named `type` and is this same numeric ARIB value.
    #[serde(rename = "type")]
    pub service_type: i32,
    /// **Array**, not a single object — matches real Mirakurun (both the
    /// stuayu fork this project targets and upstream): `ServiceItem.export()`
    /// puts `this._channel` (a `ChannelItem[]`) straight into this field, and
    /// `api.d.ts` declares `channel?: Channel[]`. This project only ever
    /// resolves one physical channel per service, so the array always has
    /// exactly one element — but the *shape* must be an array for
    /// EPGStation's `ChannelDB` to parse it without relying on its
    /// (unreleased, as of 2026-08-09) single-object fallback. See
    /// `docs/EPGSTATION_COMPAT.md` §1/§4.
    pub channel: Vec<MirakurunChannelRef>,
    pub has_logo_data: bool,
}

/// The `channel` field embedded in a [`MirakurunService`] (not the same
/// shape as the top-level [`MirakurunChannel`] — real Mirakurun does this
/// too: the embedded reference has no `services` array of its own).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunChannelRef {
    #[serde(rename = "type")]
    pub channel_type: String,
    pub channel: String,
}

/// `GET /programs` element (Mirakurun `Program` shape, subset).
///
/// `id` follows Mirakurun's own convention for program ids:
/// `(networkId * 100000 + serviceId) * 100000 + eventId` — i.e.
/// [`mirakurun_service_id`] further multiplied and offset by `eventId`, the
/// same pattern real Mirakurun uses so a program id can be inverted back to
/// its service id by integer division.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunProgram {
    pub id: u64,
    pub event_id: u16,
    pub service_id: u16,
    pub network_id: u16,
    pub transport_stream_id: u16,
    /// Milliseconds since epoch (Mirakurun convention; the `programs` table
    /// stores seconds).
    pub start_at: i64,
    /// Milliseconds.
    pub duration: i64,
    /// Always `true` — see [`get_programs`] doc comment: the `programs`
    /// table does not carry `free_CA_mode`.
    pub is_free: bool,
    pub name: Option<String>,
    pub description: Option<String>,
    pub genres: Vec<MirakurunGenre>,
}

/// A single genre entry in [`MirakurunProgram::genres`]. Real Mirakurun's
/// `Genre` type has more fields (`un1`/`un2`/`un3`, user-nibble level);
/// only the ARIB content nibble levels are stored/reported here.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MirakurunGenre {
    pub lv1: u8,
    pub lv2: u8,
}

/// `(networkId * 100000 + serviceId) * 100000 + eventId` — see
/// [`MirakurunProgram::id`] doc comment.
fn mirakurun_program_id(nid: u16, sid: u16, event_id: u16) -> u64 {
    mirakurun_service_id(nid, sid) * 100_000 + event_id as u64
}

/// Inverse of [`mirakurun_program_id`], same shape as
/// [`split_mirakurun_service_id`]: returns `None` if `id` does not decode
/// back into three `u16`s, so callers can reject it as a 400 rather than
/// querying the database with truncated/garbage values.
fn split_mirakurun_program_id(id: u64) -> Option<(u16, u16, u16)> {
    let event_id = id % 100_000;
    let service_id_part = id / 100_000;
    let (nid, sid) = split_mirakurun_service_id(service_id_part)?;
    if event_id > u16::MAX as u64 {
        return None;
    }
    Some((nid, sid, event_id as u16))
}

/// Shared field mapping into [`MirakurunProgram`], used by both
/// [`program_record_to_mirakurun`] (`GET /programs`, a durably-stored
/// [`ProgramRecord`]) and [`program_upsert_to_mirakurun`]
/// (`GET /events/stream`, a [`ProgramUpsert`] broadcast the moment it is
/// written — see `web/mirakurun_events.rs`) so the two representations are
/// never mapped to the wire shape via two independently-maintained field
/// lists that could drift apart.
#[allow(clippy::too_many_arguments)]
fn build_mirakurun_program(
    nid: u16,
    sid: u16,
    tsid: u16,
    event_id: u16,
    start_at: i64,
    duration_secs: i64,
    name: Option<String>,
    description: Option<String>,
    genre: Option<i64>,
) -> MirakurunProgram {
    let genres = genre
        .map(|g| {
            let g = g as u8;
            vec![MirakurunGenre { lv1: (g >> 4) & 0x0F, lv2: g & 0x0F }]
        })
        .unwrap_or_default();

    MirakurunProgram {
        id: mirakurun_program_id(nid, sid, event_id),
        event_id,
        service_id: sid,
        network_id: nid,
        transport_stream_id: tsid,
        start_at: start_at * 1000,
        duration: duration_secs * 1000,
        is_free: true,
        name,
        description,
        genres,
    }
}

fn program_record_to_mirakurun(r: ProgramRecord) -> MirakurunProgram {
    build_mirakurun_program(
        r.nid,
        r.sid,
        r.tsid,
        r.event_id,
        r.start_at,
        r.duration_secs,
        r.name,
        r.description,
        r.genre,
    )
}

/// Same mapping as [`program_record_to_mirakurun`], but off a not-yet-stored
/// [`ProgramUpsert`] (`GET /events/stream`, `web/mirakurun_events.rs`) rather
/// than a `programs` table row. `pub(crate)` so `mirakurun_events.rs` can
/// reuse it instead of re-deriving `MirakurunProgram`'s field list.
pub(crate) fn program_upsert_to_mirakurun(u: &ProgramUpsert) -> MirakurunProgram {
    build_mirakurun_program(
        u.nid,
        u.sid,
        u.tsid,
        u.event_id,
        u.start_at,
        u.duration_secs,
        u.name.clone(),
        u.description.clone(),
        u.genre,
    )
}

// ============================================================================
// Handlers
// ============================================================================

/// `GET /mirakurun/api/version`.
///
/// Intentionally `CARGO_PKG_VERSION` (not `crate::VERSION`/git describe):
/// Mirakurun-compatible clients (EPGStation etc.) may expect a strict
/// semver-ish string here, not a `-N-g<hash>` dev-build suffix.
pub async fn get_version() -> impl IntoResponse {
    let v = env!("CARGO_PKG_VERSION");
    Json(json!({ "current": v, "latest": v }))
}

/// `GET /mirakurun/api/status`.
///
/// Real Mirakurun's `/status` response is much larger (process RSS, EPG
/// gathering state, RPC/stream/error counters, storage usage, ...). This
/// reports only tuner counts — the one piece EPGStation's capacity checks
/// actually read in practice — and the server version; everything else is
/// omitted (see module doc comment).
pub async fn get_status(State(web_state): State<Arc<WebState>>) -> impl IntoResponse {
    let tuner_keys = web_state.tuner_pool.keys().await;
    let mut running_tuner_count = 0usize;
    for key in &tuner_keys {
        if let Some(tuner) = web_state.tuner_pool.get(key).await {
            if tuner.is_running() {
                running_tuner_count += 1;
            }
        }
    }

    Json(json!({
        // Intentionally CARGO_PKG_VERSION, see get_version() above.
        "version": env!("CARGO_PKG_VERSION"),
        "tunerCount": tuner_keys.len(),
        "runningTunerCount": running_tuner_count,
    }))
}

/// Channels usable by this API: enabled and with a scanned physical
/// assignment (`bon_channel`). See module doc comment.
fn usable_channels(db: &Database) -> Result<Vec<ChannelRecord>, crate::database::DatabaseError> {
    Ok(db
        .get_all_channels_for_export()?
        .into_iter()
        .map(|(channel, _dll_path)| channel)
        .filter(|c| c.is_enabled && c.bon_channel.is_some())
        .collect())
}

/// `GET /mirakurun/api/channels`.
///
/// Real Mirakurun returns a bare JSON array (not wrapped in this project's
/// usual `{"success": ..., ...}` envelope) — matching that shape is what
/// makes this "Mirakurun-compatible" rather than just another dashboard
/// endpoint, so every handler in this module returns the client-facing shape
/// directly.
pub async fn get_channels(State(web_state): State<Arc<WebState>>) -> Response {
    let channels = {
        let db = web_state.database.lock().await;
        match usable_channels(&db) {
            Ok(c) => c,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    };

    // Group by (Mirakurun type, channel string): one multiplex can (and
    // usually does) carry several services.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), MirakurunChannel> = HashMap::new();

    for c in &channels {
        let bt = band_type_from_db(c.band_type);
        let ty = band_type_to_mirakurun(bt).to_string();
        let Some(ch_str) = channel_string(c) else { continue };
        let key = (ty.clone(), ch_str.clone());

        groups
            .entry(key.clone())
            .or_insert_with(|| {
                order.push(key.clone());
                let name = c
                    .network_name
                    .clone()
                    .or_else(|| c.channel_name.clone())
                    .unwrap_or_else(|| ch_str.clone());
                MirakurunChannel {
                    channel_type: ty.clone(),
                    channel: ch_str.clone(),
                    name,
                    services: Vec::new(),
                }
            })
            .services
            .push(MirakurunServiceSummary {
                id: mirakurun_service_id(c.nid, c.sid),
                service_id: c.sid,
                network_id: c.nid,
                name: c.channel_name.clone().unwrap_or_default(),
            });
    }

    let result: Vec<MirakurunChannel> = order
        .into_iter()
        .filter_map(|key| groups.remove(&key))
        .collect();

    Json(result).into_response()
}

/// `GET /mirakurun/api/services`. See [`get_channels`] on response shape
/// (bare array, not this project's usual envelope).
pub async fn get_services(State(web_state): State<Arc<WebState>>) -> Response {
    let channels = {
        let db = web_state.database.lock().await;
        match usable_channels(&db) {
            Ok(c) => c,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    };

    let services: Vec<MirakurunService> = channels
        .iter()
        .filter_map(|c| {
            let bt = band_type_from_db(c.band_type);
            let ch_str = channel_string(c)?;
            Some(MirakurunService {
                id: mirakurun_service_id(c.nid, c.sid),
                service_id: c.sid,
                network_id: c.nid,
                name: c.channel_name.clone().unwrap_or_default(),
                service_type: c.service_type.map(|v| v as i32).unwrap_or(0),
                // Single-element array — see `MirakurunService::channel` doc
                // comment.
                channel: vec![MirakurunChannelRef {
                    channel_type: band_type_to_mirakurun(bt).to_string(),
                    channel: ch_str,
                }],
                has_logo_data: false,
            })
        })
        .collect();

    Json(services).into_response()
}

/// `GET /mirakurun/api/programs`. See [`get_channels`] on response shape
/// (bare array, not this project's usual envelope).
///
/// Real Mirakurun accepts `?networkId=&serviceId=` filters; this reads the
/// full `programs` table unfiltered (EPGStation/KonomiTV typically fetch
/// everything and filter client-side for this subset's scale). `isFree` is
/// always reported `true`: the `programs` table (Migration 015) does not
/// store `free_CA_mode` — the collector (`tuner/epg_collector.rs`) parses it
/// per-event but the design's schema (see `database/program.rs`) omits the
/// column, so it is not persisted. This is a known simplification, not a
/// bug: EPGStation/KonomiTV use `isFree` only to badge scrambled programs in
/// the UI, which does not block program-guide population.
pub async fn get_programs(State(web_state): State<Arc<WebState>>) -> Response {
    let programs = {
        let db = web_state.database.lock().await;
        match db.get_programs(i64::MIN, i64::MAX, None, None) {
            Ok(p) => p,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    };

    let result: Vec<MirakurunProgram> = programs.into_iter().map(program_record_to_mirakurun).collect();
    Json(result).into_response()
}

/// Parse the `X-Mirakurun-Priority` header EPGStation (and real Mirakurun
/// clients generally) attach to both stream endpoints — `recPriority` /
/// `conflictPriority` / `streamingPriority` depending on which reservation
/// path sent the request (`docs/EPGSTATION_COMPAT.md` §5). Real Mirakurun
/// treats a missing/unparseable value as `0` (lowest), so this does the
/// same rather than rejecting the request — a malformed header should not
/// break streaming.
///
/// The parsed value is presently only logged, **not** wired into
/// `tuner/policy.rs::decide()`: feeding stream priority into tuner
/// contention is a policy decision (which stream loses when tuners are
/// full) that belongs in a dedicated design pass, not folded into this
/// EPGStation-compat task. Until then, a recording request and a live-view
/// request compete for tuners exactly as any two ordinary clients do.
fn parse_mirakurun_priority(headers: &HeaderMap) -> i32 {
    headers
        .get("X-Mirakurun-Priority")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0)
}

/// `GET /mirakurun/api/services/:id/stream`.
///
/// `:id` is the Mirakurun service id (`networkId * 100000 + serviceId`, see
/// [`split_mirakurun_service_id`]), resolved to a channel via
/// `channel_resolve::resolve_service_by_nid_sid`, then streamed with the
/// exact same passthrough machinery `web/stream.rs`'s
/// `GET /api/stream/service/:sid` uses (`StreamCleanup`,
/// `broadcast_to_body_stream`, `respond_with_stream`) — no `?profile=`
/// transcoding here: Mirakurun's convention is raw TS passthrough
/// (STREAMING_DESIGN.md §7.1), and query params other than none are ignored
/// by design (a `?decode=1`-style param some clients send is simply not
/// read).
///
/// `X-Mirakurun-Priority` is parsed and logged (see
/// [`parse_mirakurun_priority`]) but not otherwise acted on.
pub async fn stream_service_by_mirakurun_id(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    let priority = parse_mirakurun_priority(&headers);
    debug!("mirakurun: GET /services/{}/stream (X-Mirakurun-Priority={})", id, priority);

    let Some((nid, sid)) = split_mirakurun_service_id(id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("'{}' is not a valid Mirakurun service id", id),
        );
    };

    let resolved = {
        let db = web_state.database.lock().await;
        channel_resolve::resolve_service_by_nid_sid(&db, nid, sid)
    };
    let resolved = match resolved {
        Ok(r) => r,
        Err(e) => return channel_resolve_error_response(id, &e),
    };

    let tuner = match channel_resolve::start_tuner_for_service(&web_state.tuner_pool, &web_state.database, &resolved).await {
        Ok(t) => t,
        Err(e) => return channel_resolve_error_response(id, &e),
    };

    let tuner_rx = tuner.subscribe();
    let cleanup = StreamCleanup::tuner_only(Arc::clone(&tuner), Arc::clone(&web_state.tuner_pool));
    respond_with_stream(broadcast_to_body_stream(BodyReceiver::Tuner(tuner_rx), cleanup))
}

/// `GET /mirakurun/api/channels/:type/:channel/stream`.
///
/// Finds the first usable channel whose (type, channel-string) matches, then
/// streams it exactly like [`stream_service_by_mirakurun_id`] (raw TS
/// passthrough — the full multiplex, not filtered to one service's PIDs;
/// same simplification `web/stream.rs`'s passthrough path already makes, see
/// its module doc comment).
pub async fn stream_channel_by_type(
    State(web_state): State<Arc<WebState>>,
    Path((channel_type, channel_str)): Path<(String, String)>,
) -> Response {
    let Some(candidates) = mirakurun_type_to_band_candidates(&channel_type) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("unknown channel type '{}' (expected GR/BS/CS/SKY)", channel_type),
        );
    };

    let target_id = {
        let db = web_state.database.lock().await;
        let channels = match usable_channels(&db) {
            Ok(c) => c,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        channels.iter().find_map(|c| {
            let bt = band_type_from_db(c.band_type);
            if !candidates.contains(&bt) {
                return None;
            }
            let cs = channel_string(c)?;
            (cs == channel_str).then_some(c.id)
        })
    };

    let Some(id) = target_id else {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("channel {}/{} not found", channel_type, channel_str),
        );
    };

    let resolved = {
        let db = web_state.database.lock().await;
        channel_resolve::resolve_service(&db, id)
    };
    let resolved = match resolved {
        Ok(r) => r,
        Err(e) => return channel_resolve_error_response(id, &e),
    };

    let tuner = match channel_resolve::start_tuner_for_service(&web_state.tuner_pool, &web_state.database, &resolved).await {
        Ok(t) => t,
        Err(e) => return channel_resolve_error_response(id, &e),
    };

    let tuner_rx = tuner.subscribe();
    let cleanup = StreamCleanup::tuner_only(Arc::clone(&tuner), Arc::clone(&web_state.tuner_pool));
    respond_with_stream(broadcast_to_body_stream(BodyReceiver::Tuner(tuner_rx), cleanup))
}

/// `GET /mirakurun/api/tuners`.
///
/// EPGStation only reads this for its `/api/status` display (`docs/
/// EPGSTATION_COMPAT.md` §3), so it does not need to be exact — but the
/// shape must match `TunerDevice` (`api.d.ts`). One element per
/// `bon_drivers` row (this project's closest equivalent of a "tuner
/// device"), `index` assigned by enumeration order (this project has no
/// separate stable tuner index — real Mirakurun's comes from `tuners.yml`
/// position, which has no analogue here).
///
/// Fields with no equivalent concept in this project, hardcoded to a
/// type-appropriate default:
/// - `types`: always `[]`. Real Mirakurun reports which `ChannelType`s a
///   tuner can receive (from `tuners.yml`); this project's BonDriver rows
///   are not statically typed to a band until channels are scanned onto
///   them, and a single BonDriver can carry multiple bands, so there is no
///   single honest static answer here without querying scanned channels
///   per driver — left empty rather than guessed.
/// - `pid`: always `0`. BonDriver DLLs run in-process (loaded by
///   `tuner/shared.rs`, not spawned as a subprocess), so there is no
///   separate OS process to report.
/// - `users`: always `[]`. Real Mirakurun lists active `TunerUser`s
///   (id/priority/agent/url); mapping this project's session/subscriber
///   model onto that shape is not needed for EPGStation's `/status` display
///   and is left for a future pass if a client actually needs it.
/// - `isRemote`: always `false` (no remote-tuner concept here).
/// - `isFault`: always `false` (this project has no persisted "this tuner
///   is broken" flag distinct from "currently failing to stream").
///
/// Fields derived from real state:
/// - `isUsing`/`isFree`: `true`/`false` if any [`crate::tuner::pool::TunerPool`]
///   key whose `tuner_path` equals this driver's `dll_path` currently has a
///   running [`crate::tuner::shared::SharedTuner`] — same signal
///   [`get_status`] already aggregates into `runningTunerCount`.
/// - `isAvailable`: always `true` — this project does not track a
///   driver-level "administratively disabled" state distinct from
///   `is_enabled` on individual channels, and a driver with no channels
///   enabled is still a usable tuner slot.
pub async fn get_tuners(State(web_state): State<Arc<WebState>>) -> Response {
    let drivers = {
        let db = web_state.database.lock().await;
        match db.get_all_bon_drivers() {
            Ok(d) => d,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    };

    // Which dll_paths currently have a running reader, per the same signal
    // `get_status` uses for `runningTunerCount`.
    let running_dll_paths: HashSet<String> = {
        let mut set = HashSet::new();
        for key in web_state.tuner_pool.keys().await {
            if let Some(tuner) = web_state.tuner_pool.get(&key).await {
                if tuner.is_running() {
                    set.insert(key.tuner_path.clone());
                }
            }
        }
        set
    };

    let tuners: Vec<serde_json::Value> = drivers
        .iter()
        .enumerate()
        .map(|(index, d)| {
            let is_using = running_dll_paths.contains(&d.dll_path);
            json!({
                "index": index,
                "name": d.driver_name.clone().unwrap_or_else(|| d.dll_path.clone()),
                "types": [],
                "command": d.dll_path,
                "pid": 0,
                "users": [],
                "isAvailable": true,
                "isRemote": false,
                "isFree": !is_using,
                "isUsing": is_using,
                "isFault": false,
            })
        })
        .collect();

    Json(tuners).into_response()
}

/// `GET /mirakurun/api/config/server`.
///
/// Per `docs/EPGSTATION_COMPAT.md` §3, EPGStation's own `tunerServerType:
/// mirakurun` config setting (if the operator sets it) lets it skip calling
/// this endpoint entirely — so this handler's main job is simply to exist
/// and return `200` for the `auto`-detection path, not to be a faithful
/// `ConfigServer`. Only the two fields `api.d.ts`'s `ConfigServer` marks
/// non-optional (`allowOrigins`, `allowPNA`) are populated with real
/// (empty/permissive-off) values; every other field on that type is
/// optional and intentionally omitted rather than guessed.
pub async fn get_server_config() -> Response {
    Json(json!({
        "allowOrigins": [],
        "allowPNA": false,
    }))
    .into_response()
}

/// `GET /mirakurun/api/services/:id/logo` — **placeholder, not
/// implemented**.
///
/// Declared in `/docs` (`getLogoImage`) for completeness, but every service
/// this API reports has `hasLogoData: false` ([`get_services`]) — real
/// Mirakurun clients, including EPGStation's `ChannelApiModel.ts:101`, only
/// call this when a service's `hasLogoData` is `true`, so in practice this
/// route is never hit. Answers `404` rather than `501`: unlike the
/// programs/events endpoints, this one has a well-defined "correct" empty
/// answer ("this service has no logo") rather than "not built yet".
pub async fn get_logo_stub(Path(id): Path<u64>) -> Response {
    error_response(StatusCode::NOT_FOUND, format!("service {} has no logo", id))
}

/// `GET /mirakurun/api/programs/:id/stream`.
///
/// This is the endpoint EPGStation's EPG-reservation recording depends on
/// (`docs/EPGSTATION_COMPAT.md` §3/§5): it must wait to emit data until the
/// target event becomes EIT[p/f] "present", and recording ends when the
/// stream itself ends (not `reserve.endAt`, which real Mirakurun also
/// ignores for this same reason — a program can run long). The
/// present/following gating itself is
/// [`crate::web::mirakurun_program_stream::ProgramGate`]; this handler's job
/// is: decode `:id` -> look up the program row -> resolve/start the tuner
/// (identical path to [`stream_service_by_mirakurun_id`]) -> hand the tuner
/// subscription to the gate.
///
/// `:id` is looked up against the `programs` table (populated by
/// `tuner/epg_collector.rs` via `crate::epg_writer::EpgWriter`) purely to
/// learn the target `(sid, event_id)` and the program's scheduled end (for
/// the give-up deadline, see
/// `mirakurun_program_stream::PRESENT_WAIT_GRACE`) — the row is not
/// otherwise consulted (in particular, the *content* of the recording comes
/// live off the tuner, not from anything stored about the program).
///
/// `X-Mirakurun-Priority` is parsed and logged (see
/// [`parse_mirakurun_priority`]) but not otherwise acted on, same as
/// [`stream_service_by_mirakurun_id`].
pub async fn stream_program_by_mirakurun_id(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
    headers: HeaderMap,
) -> Response {
    let priority = parse_mirakurun_priority(&headers);
    debug!("mirakurun: GET /programs/{}/stream (X-Mirakurun-Priority={})", id, priority);

    let Some((nid, sid, event_id)) = split_mirakurun_program_id(id) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("'{}' is not a valid Mirakurun program id", id),
        );
    };

    let program = {
        let db = web_state.database.lock().await;
        let programs = match db.get_programs(i64::MIN, i64::MAX, Some(nid), Some(sid)) {
            Ok(p) => p,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        programs.into_iter().find(|p| p.event_id == event_id)
    };
    let Some(program) = program else {
        return error_response(StatusCode::NOT_FOUND, format!("program {} not found", id));
    };

    // See `mirakurun_program_stream::PRESENT_WAIT_GRACE` doc comment: give up
    // waiting for "present" this long past the program's own scheduled end.
    let deadline = chrono::DateTime::from_timestamp(program.start_at + program.duration_secs, 0)
        .unwrap_or_else(chrono::Utc::now)
        + mirakurun_program_stream::PRESENT_WAIT_GRACE;

    let resolved = {
        let db = web_state.database.lock().await;
        channel_resolve::resolve_service_by_nid_sid(&db, nid, sid)
    };
    let resolved = match resolved {
        Ok(r) => r,
        Err(e) => return channel_resolve_error_response(id, &e),
    };

    let tuner = match channel_resolve::start_tuner_for_service(&web_state.tuner_pool, &web_state.database, &resolved).await {
        Ok(t) => t,
        Err(e) => return channel_resolve_error_response(id, &e),
    };

    let tuner_rx = tuner.subscribe();
    let cleanup = StreamCleanup::tuner_only(Arc::clone(&tuner), Arc::clone(&web_state.tuner_pool));
    respond_with_stream(mirakurun_program_stream::gated_program_stream(
        tuner_rx, cleanup, sid, event_id, deadline,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // id <-> (nid, sid) round trip
    // ------------------------------------------------------------------

    #[test]
    fn service_id_round_trips() {
        for (nid, sid) in [(1u16, 100u16), (0x7fe8, 1024), (0, 0), (65535, 65535)] {
            let id = mirakurun_service_id(nid, sid);
            assert_eq!(split_mirakurun_service_id(id), Some((nid, sid)));
        }
    }

    #[test]
    fn service_id_matches_documented_formula() {
        assert_eq!(mirakurun_service_id(1, 100), 100_100);
        assert_eq!(mirakurun_service_id(0x7fe8, 1024), 0x7fe8u64 * 100_000 + 1024);
    }

    #[test]
    fn split_rejects_ids_that_decode_out_of_u16_range() {
        // nid would be 65536 (out of u16 range) for this id.
        let bogus = 65536u64 * 100_000;
        assert_eq!(split_mirakurun_service_id(bogus), None);
    }

    // ------------------------------------------------------------------
    // program id <-> (nid, sid, event_id) round trip
    // ------------------------------------------------------------------

    #[test]
    fn program_id_round_trips() {
        for (nid, sid, event_id) in [
            (1u16, 100u16, 1u16),
            (0x7fe8, 1024, 0xffff),
            (0, 0, 0),
            (65535, 65535, 65535),
        ] {
            let id = mirakurun_program_id(nid, sid, event_id);
            assert_eq!(split_mirakurun_program_id(id), Some((nid, sid, event_id)));
        }
    }

    #[test]
    fn program_id_matches_documented_formula() {
        assert_eq!(mirakurun_program_id(1, 100, 5), mirakurun_service_id(1, 100) * 100_000 + 5);
    }

    #[test]
    fn split_program_id_rejects_ids_that_decode_out_of_u16_range() {
        // nid would be 65536 (out of u16 range) for the embedded service id.
        let bogus_nid = (65536u64 * 100_000) * 100_000;
        assert_eq!(split_mirakurun_program_id(bogus_nid), None);

        // event_id would be 70000 (out of u16 range, but still < 100_000 so
        // it doesn't wrap into the service-id portion).
        let bogus_event = mirakurun_service_id(1, 100) * 100_000 + 70_000;
        assert_eq!(split_mirakurun_program_id(bogus_event), None);
    }

    // ------------------------------------------------------------------
    // BandType <-> Mirakurun `type` mapping
    // ------------------------------------------------------------------

    #[test]
    fn band_type_maps_to_expected_mirakurun_type() {
        assert_eq!(band_type_to_mirakurun(BandType::Terrestrial), "GR");
        assert_eq!(band_type_to_mirakurun(BandType::BS), "BS");
        assert_eq!(band_type_to_mirakurun(BandType::CS), "CS");
        assert_eq!(band_type_to_mirakurun(BandType::FourK), "BS");
        assert_eq!(band_type_to_mirakurun(BandType::CATV), "GR");
        assert_eq!(band_type_to_mirakurun(BandType::SKY), "SKY");
        assert_eq!(band_type_to_mirakurun(BandType::Other), "SKY");
    }

    #[test]
    fn mirakurun_type_reverse_lookup_covers_every_forward_mapping() {
        // Every BandType must appear in the candidate list for the type its
        // forward mapping produces, or `/channels/:type/:channel/stream`
        // could never find a row of that band via its own advertised type.
        for bt in [
            BandType::Terrestrial,
            BandType::BS,
            BandType::CS,
            BandType::FourK,
            BandType::CATV,
            BandType::SKY,
            BandType::Other,
        ] {
            let ty = band_type_to_mirakurun(bt);
            let candidates = mirakurun_type_to_band_candidates(ty).unwrap();
            assert!(candidates.contains(&bt), "{:?} -> {} not in reverse candidates", bt, ty);
        }
    }

    #[test]
    fn unknown_type_string_has_no_candidates() {
        assert!(mirakurun_type_to_band_candidates("BS8K").is_none());
        assert!(mirakurun_type_to_band_candidates("").is_none());
        assert!(mirakurun_type_to_band_candidates("bs4k").is_none(), "types are case-sensitive");
    }

    /// `BS4K` is accepted on input because MMirakurun defines it, but must not
    /// be produced: the `api.d.ts` EPGStation compiles against has no such
    /// member, and its ChannelDB drops unknown types into a catch-all bucket.
    #[test]
    fn bs4k_is_accepted_on_input_but_never_advertised() {
        assert_eq!(
            mirakurun_type_to_band_candidates("BS4K"),
            Some(&[BandType::FourK][..])
        );
        assert_eq!(band_type_to_mirakurun(BandType::FourK), "BS");

        // Nothing may advertise itself as BS4K.
        for bt in [
            BandType::Terrestrial,
            BandType::BS,
            BandType::CS,
            BandType::FourK,
            BandType::CATV,
            BandType::SKY,
            BandType::Other,
        ] {
            assert_ne!(band_type_to_mirakurun(bt), "BS4K");
        }
    }

    // ------------------------------------------------------------------
    // channel_string
    // ------------------------------------------------------------------

    fn make_channel_record(band_type: Option<u8>, physical_ch: Option<u8>, bon_channel: Option<u32>) -> ChannelRecord {
        ChannelRecord {
            id: 1,
            bon_driver_id: 1,
            nid: 1,
            sid: 100,
            tsid: 200,
            manual_sheet: None,
            raw_name: None,
            channel_name: Some("Test".to_string()),
            physical_ch,
            remote_control_key: None,
            service_type: None,
            network_name: None,
            bon_space: Some(0),
            bon_channel,
            band_type,
            region_id: None,
            terrestrial_region: None,
            is_enabled: true,
            scan_time: None,
            last_seen: None,
            failure_count: 0,
            priority: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn terrestrial_channel_string_prefers_physical_ch() {
        let record = make_channel_record(Some(0), Some(27), Some(101));
        assert_eq!(channel_string(&record), Some("27".to_string()));
    }

    #[test]
    fn terrestrial_channel_string_falls_back_to_bon_channel() {
        let record = make_channel_record(Some(0), None, Some(101));
        assert_eq!(channel_string(&record), Some("101".to_string()));
    }

    #[test]
    fn non_terrestrial_channel_string_uses_bon_channel() {
        let record = make_channel_record(Some(1), Some(27), Some(15));
        assert_eq!(channel_string(&record), Some("15".to_string()));
    }

    #[test]
    fn missing_physical_assignment_has_no_channel_string() {
        let record = make_channel_record(Some(0), None, None);
        assert_eq!(channel_string(&record), None);
    }

    // ------------------------------------------------------------------
    // Service.channel serializes as an array (docs/EPGSTATION_COMPAT.md §1/§4)
    // ------------------------------------------------------------------

    #[test]
    fn service_channel_serializes_as_a_single_element_array() {
        let service = MirakurunService {
            id: mirakurun_service_id(1, 100),
            service_id: 100,
            network_id: 1,
            name: "Test".to_string(),
            service_type: 1,
            channel: vec![MirakurunChannelRef { channel_type: "GR".to_string(), channel: "27".to_string() }],
            has_logo_data: false,
        };
        let value = serde_json::to_value(&service).unwrap();
        let channel = value["channel"].as_array().expect("channel must serialize as a JSON array");
        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0]["type"], "GR");
        assert_eq!(channel[0]["channel"], "27");
    }

    // ------------------------------------------------------------------
    // X-Mirakurun-Priority parsing
    // ------------------------------------------------------------------

    #[test]
    fn priority_header_parses_a_valid_integer() {
        let mut headers = HeaderMap::new();
        headers.insert("X-Mirakurun-Priority", "3".parse().unwrap());
        assert_eq!(parse_mirakurun_priority(&headers), 3);
    }

    #[test]
    fn priority_header_defaults_to_zero_when_missing_or_unparseable() {
        assert_eq!(parse_mirakurun_priority(&HeaderMap::new()), 0);

        let mut headers = HeaderMap::new();
        headers.insert("X-Mirakurun-Priority", "not-a-number".parse().unwrap());
        assert_eq!(parse_mirakurun_priority(&headers), 0);
    }
}
