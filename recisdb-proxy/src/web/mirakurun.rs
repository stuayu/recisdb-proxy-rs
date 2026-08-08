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

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::json;

use crate::database::{ChannelRecord, Database, ProgramRecord};
use crate::server::channel_resolve;
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
    pub channel: MirakurunChannelRef,
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

fn program_record_to_mirakurun(r: ProgramRecord) -> MirakurunProgram {
    let genres = r
        .genre
        .map(|g| {
            let g = g as u8;
            vec![MirakurunGenre { lv1: (g >> 4) & 0x0F, lv2: g & 0x0F }]
        })
        .unwrap_or_default();

    MirakurunProgram {
        id: mirakurun_program_id(r.nid, r.sid, r.event_id),
        event_id: r.event_id,
        service_id: r.sid,
        network_id: r.nid,
        transport_stream_id: r.tsid,
        start_at: r.start_at * 1000,
        duration: r.duration_secs * 1000,
        is_free: true,
        name: r.name,
        description: r.description,
        genres,
    }
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
                channel: MirakurunChannelRef {
                    channel_type: band_type_to_mirakurun(bt).to_string(),
                    channel: ch_str,
                },
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
pub async fn stream_service_by_mirakurun_id(
    State(web_state): State<Arc<WebState>>,
    Path(id): Path<u64>,
) -> Response {
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

    let tuner = match channel_resolve::start_tuner_for_service(&web_state.tuner_pool, &resolved).await {
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

    let tuner = match channel_resolve::start_tuner_for_service(&web_state.tuner_pool, &resolved).await {
        Ok(t) => t,
        Err(e) => return channel_resolve_error_response(id, &e),
    };

    let tuner_rx = tuner.subscribe();
    let cleanup = StreamCleanup::tuner_only(Arc::clone(&tuner), Arc::clone(&web_state.tuner_pool));
    respond_with_stream(broadcast_to_body_stream(BodyReceiver::Tuner(tuner_rx), cleanup))
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
        assert!(mirakurun_type_to_band_candidates("BS4K").is_none());
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
}
