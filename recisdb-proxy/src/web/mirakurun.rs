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
//! - Service logos are served from this project's own logo store
//!   (`logos/<nid>_<sid>.png`, filled by `tuner/logo_collector.rs` from CDT on
//!   live streams): `hasLogoData` reports whether that file exists and
//!   `GET /api/services/:id/logo` returns it. `logoId` is not reported — it is
//!   the broadcast's logo identifier, which nothing in EPGStation reads (it
//!   keys off `hasLogoData` alone). A service that has never been tuned has no
//!   logo file and so reports `hasLogoData: false`.
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
//!   else `bon_channel`, rendered as a decimal string — then **disambiguated
//!   so that `(type, channel)` identifies exactly one multiplex**
//!   ([`assign_channel_strings`]), which is what Mirakurun's contract
//!   requires and what neither of those raw numbers provides on a
//!   multi-area, multi-BonDriver install. Real Mirakurun encodes satellite
//!   channels as e.g. `"BS15_0"` (transponder + slot); reproducing that
//!   convention exactly is out of scope here — EPGStation/KonomiTV primarily
//!   key off `id`/`serviceId` for streaming and only display the `channel`
//!   string, so this simplification does not block the "視聴が通る" goal.
//! - **One row per service**: the `channels` table stores one row per
//!   *(BonDriver, service)* pair, so a service receivable by four tuners has
//!   four rows. `/services` and `/channels` collapse those to one entry per
//!   `(networkId, serviceId)` ([`unique_services`]) — real Mirakurun lists
//!   each service exactly once, and emitting duplicates let stale rows (with
//!   a placeholder name, or a `bon_channel` belonging to another driver's
//!   channel table) overwrite the good one in EPGStation's channel table.
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
use crate::tuner::logo_collector::{collected_logo_keys, logo_path};
use crate::web::mirakurun_program_stream;
use crate::web::state::WebState;
use crate::web::stream::{
    broadcast_to_body_stream, channel_resolve_error_response, error_response, respond_with_stream,
    service_filtered_body_stream, BodyReceiver, StreamCleanup,
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

/// The highest `NWn` EPGStation's fork defines (`api.d.ts`'s `ChannelType`
/// is `"GR" | "BS" | "CS" | "SKY" | "NW1".."NW40"`). Regions past this fall
/// back to `GR`: an over-full guide tab is recoverable, a channel type the
/// client's type union does not contain is not (its `ChannelDB` funnels
/// unknown types into one catch-all bucket, `src/model/db/ChannelDB.ts:169`).
const MAX_NW_INDEX: usize = 40;

/// Assigns each terrestrial `region_id` present in `services` a Mirakurun
/// channel type: `GR` for the local region(s), `NW1`..`NW40` for the rest,
/// in ascending `region_id` order.
///
/// This is what lets a multi-area install (several BonDriverProxy endpoints
/// in different prefectures — the setup this project exists for) show up in
/// EPGStation the way the reference Mirakurun does on the same hardware:
/// there, only the 21 local services are `GR` and the other ~550 are spread
/// over `NW1`..`NW27`, one group per reception area. Reporting all of them as
/// `GR` instead puts several hundred stations in one guide tab.
///
/// `home_regions` empty (no `[mirakurun] home_region` configured) returns an
/// empty map, which callers read as "everything terrestrial is `GR`" — the
/// behaviour from before this existed, and the right answer for a
/// single-area install.
///
/// The assignment is derived from the scan results, so **receiving a new
/// area can shift the `NWn` numbering of areas sorted after it**. That only
/// moves stations between guide tabs in EPGStation: its channel ids
/// (`networkId * 100000 + serviceId`) do not change, so recordings, reserves
/// and rules keep pointing at the same stations.
fn terrestrial_type_map(services: &[ChannelRecord], home_regions: &[u8]) -> HashMap<u8, String> {
    if home_regions.is_empty() {
        return HashMap::new();
    }

    let mut regions: Vec<u8> = services
        .iter()
        .filter(|c| matches!(band_type_from_db(c.band_type), BandType::Terrestrial | BandType::CATV))
        .filter_map(region_id_of)
        .filter(|id| !home_regions.contains(id))
        .collect();
    regions.sort_unstable();
    regions.dedup();

    let mut map: HashMap<u8, String> = home_regions.iter().map(|id| (*id, "GR".to_string())).collect();
    for (index, region) in regions.into_iter().enumerate() {
        let ty = if index < MAX_NW_INDEX {
            format!("NW{}", index + 1)
        } else {
            "GR".to_string()
        };
        map.insert(region, ty);
    }
    map
}

/// The region a row belongs to: the scanned `channels.region_id` when
/// present, else derived from the network id. The DB column is only filled in
/// by newer scans, and the derivation is exact for terrestrial network ids
/// (ARIB assigns them as `0x7FF0 - 0x10 × region + operator`), so falling
/// back costs nothing.
fn region_id_of(c: &ChannelRecord) -> Option<u8> {
    c.region_id
        .or_else(|| recisdb_protocol::broadcast_region::get_region_id_from_nid(c.nid))
}

/// The Mirakurun channel type for one row, honouring the `GR`/`NWn` split
/// from [`terrestrial_type_map`]. Non-terrestrial rows are unaffected.
fn mirakurun_type_of(c: &ChannelRecord, terrestrial_types: &HashMap<u8, String>) -> String {
    let bt = band_type_from_db(c.band_type);
    if matches!(bt, BandType::Terrestrial | BandType::CATV) {
        if let Some(ty) = region_id_of(c).and_then(|id| terrestrial_types.get(&id)) {
            return ty.clone();
        }
    }
    band_type_to_mirakurun(bt).to_string()
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
        // Out-of-area terrestrial ([`terrestrial_type_map`]). Which region a
        // given `NWn` refers to depends on what has been scanned, so the
        // candidate bands are simply the terrestrial ones and the caller
        // matches on the assigned `(type, channel)` pair.
        t if is_nw_type(t) => Some(&[BandType::Terrestrial, BandType::CATV]),
        _ => None,
    }
}

/// `NW1`..`NW40` exactly — not `NW0`, `NW41`, or `NW01`.
fn is_nw_type(t: &str) -> bool {
    let Some(index) = t.strip_prefix("NW") else { return false };
    if index.starts_with('0') {
        return false;
    }
    matches!(index.parse::<usize>(), Ok(n) if (1..=MAX_NW_INDEX).contains(&n))
}

/// Mirakurun `types` per `bon_drivers.id`, derived from the bands that
/// driver's scanned channels fall into. Ordered `GR`, `BS`, `CS`, `SKY` (the
/// order real Mirakurun's `tuners.yml` conventionally lists them in) rather
/// than by discovery, so the response is stable across restarts.
fn channel_types_by_driver(channels: &[ChannelRecord]) -> HashMap<i64, Vec<&'static str>> {
    const TYPE_ORDER: [&str; 4] = ["GR", "BS", "CS", "SKY"];

    let mut seen: HashMap<i64, HashSet<&'static str>> = HashMap::new();
    for c in channels {
        let ty = band_type_to_mirakurun(band_type_from_db(c.band_type));
        seen.entry(c.bon_driver_id).or_default().insert(ty);
    }

    seen.into_iter()
        .map(|(driver_id, types)| {
            let ordered = TYPE_ORDER.iter().filter(|t| types.contains(*t)).copied().collect();
            (driver_id, ordered)
        })
        .collect()
}

/// The value reported as `Service.remoteControlKeyId`, **terrestrial rows
/// only**.
///
/// `channels.remote_control_key` is also populated for CS110, where the
/// scanner stores the 3-digit channel number in it (see `ChannelRecord`'s
/// field comment) — that is not a remote-control key id and reporting it
/// would put "161" where EPGStation draws a remote button number. Real
/// Mirakurun agrees: on the reference server for this same reception setup,
/// every terrestrial service carries `remoteControlKeyId` and no BS/CS
/// service does.
fn remote_control_key_id(c: &ChannelRecord) -> Option<u16> {
    match band_type_from_db(c.band_type) {
        BandType::Terrestrial | BandType::CATV => c.remote_control_key,
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
    /// ARIB remote-control key id (the 1–12 button number printed on a
    /// Japanese TV remote), from the TS information descriptor. Omitted
    /// entirely when unknown rather than sent as `null`: EPGStation stores
    /// `undefined` as SQL NULL either way
    /// (`src/model/db/ChannelDB.ts:152`), but omitting matches real
    /// Mirakurun, whose `remoteControlKeyId` is an optional field.
    ///
    /// Only terrestrial rows are reported — see [`remote_control_key_id`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_control_key_id: Option<u16>,
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

    // **Configured** tuners, not pool entries. `TunerPool::keys()` only has
    // entries for drivers that have been opened at least once since start-up,
    // so using its length reported "tunerCount: 1" on a 14-driver server —
    // and it was then smaller than `/tuners`'s own array length, which is
    // built from `bon_drivers`. Mirakurun's `tunerCount` is the number of
    // configured tuner devices, so read the same table `get_tuners` does and
    // fall back to the pool size only if the query fails.
    let tuner_count = {
        let db = web_state.database.lock().await;
        db.get_all_bon_drivers().map(|d| d.len()).unwrap_or_else(|e| {
            debug!("mirakurun: /status could not count bon_drivers ({e}); falling back to pool size");
            tuner_keys.len()
        })
    };

    Json(json!({
        // Intentionally CARGO_PKG_VERSION, see get_version() above.
        "version": env!("CARGO_PKG_VERSION"),
        "tunerCount": tuner_count,
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

// ============================================================================
// Deduplication: `channels` rows are per-BonDriver, Mirakurun services are not
// ============================================================================

/// How "trustworthy" a `channels` row is as *the* description of its service,
/// higher is better. Used by [`unique_services`] to pick one row per
/// `(nid, sid)`.
///
/// This project's `channels` table holds **one row per (BonDriver, service)**:
/// a service receivable by four tuners has four rows, and re-scans leave rows
/// behind from drivers that saw the multiplex at a different `bon_channel` or
/// before SDT was parsed (`channel_name` still the synthetic `"BS09/TS1"`
/// placeholder, `service_type`/`network_name` NULL). Mirakurun's `/services`
/// is keyed by `id = networkId * 100000 + serviceId` and must list each
/// service exactly once — EPGStation inserts them keyed by that id
/// (`src/model/db/ChannelDB.ts:88-118`), so duplicates do not error out but
/// the *last* row silently wins, which used to hand EPGStation a placeholder
/// name and/or a `channel` string belonging to a different multiplex.
///
/// Ordering rationale (most significant first):
/// 1. Rows whose `channel_name` is a real SDT service name rather than the
///    synthetic `<band><ch>/TS<n>` placeholder.
/// 2. Rows that carry `service_type` — only set once the SDT service
///    descriptor was parsed.
/// 3. Rows that carry `network_name` (NIT parsed).
/// 4. Rows that carry `physical_ch` (the scan resolved a real RF channel).
/// 5. Rows that carry `remote_control_key`.
/// 6. Most recently seen, then highest `priority`, then fewest failures,
///    then lowest row id — the last purely so the choice is deterministic.
fn representative_rank(c: &ChannelRecord) -> (bool, bool, bool, bool, bool, i64, i32, i32, i64) {
    (
        !is_placeholder_name(c),
        c.service_type.is_some(),
        c.network_name.is_some(),
        c.physical_ch.is_some(),
        c.remote_control_key.is_some(),
        c.last_seen.unwrap_or(0),
        c.priority,
        -c.failure_count,
        -c.id,
    )
}

/// Whether `channel_name` is the synthetic placeholder the scanner writes
/// before the SDT service name is known (`"BS09/TS1"`, `"15Ch(NHK-G)"`-style
/// rows keep their real name and are not matched here). Missing/empty names
/// count as placeholders too — anything is better than an empty service name.
fn is_placeholder_name(c: &ChannelRecord) -> bool {
    let Some(name) = c.channel_name.as_deref() else { return true };
    let name = name.trim();
    if name.is_empty() {
        return true;
    }
    // `<band><2-digit ch>/TS<n>` — e.g. "BS09/TS1", "CS02/TS0".
    let Some((head, tail)) = name.split_once("/TS") else { return false };
    tail.chars().all(|ch| ch.is_ascii_digit())
        && !tail.is_empty()
        && head.len() > 2
        && head[..2].chars().all(|ch| ch.is_ascii_uppercase())
        && head[2..].chars().all(|ch| ch.is_ascii_digit())
}

/// One row per `(nid, sid)` — the highest-[`representative_rank`] row of each
/// service, in a deterministic order (by `nid`, then `sid`).
fn unique_services(channels: Vec<ChannelRecord>) -> Vec<ChannelRecord> {
    let mut best: HashMap<(u16, u16), ChannelRecord> = HashMap::new();
    for c in channels {
        match best.entry((c.nid, c.sid)) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                if representative_rank(&c) > representative_rank(e.get()) {
                    e.insert(c);
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(c);
            }
        }
    }

    let mut services: Vec<ChannelRecord> = best.into_values().collect();
    services.sort_by_key(|c| (c.nid, c.sid));
    services
}

/// The Mirakurun `(type, channel)` pair assigned to each multiplex, keyed by
/// `(nid, tsid)`.
///
/// Mirakurun's contract is that `(type, channel)` identifies one multiplex —
/// `GET /channels/:type/:channel/stream` has nothing else to go on. That does
/// not hold for a raw [`channel_string`] here for two independent reasons:
///
/// - `physical_ch` is only unique within one reception area. This project is
///   built for multi-area reception (a BonDriverProxy fan-out over tuners in
///   several prefectures), where RF 15 is a different station per area — the
///   production database has seven distinct `networkId`s on terrestrial 15.
/// - The `bon_channel` fallback is a *per-BonDriver* index, so two drivers
///   with different channel tables collide on the same number.
///
/// So: assign the natural string first, then disambiguate every group that
/// collided by appending `_<nid>` (and `_<nid>_<tsid>` in the — impossible in
/// practice, but cheap to be exhaustive about — case that two multiplexes on
/// one network still collide). Non-colliding channels keep the plain number,
/// which is what a single-area install sees.
fn assign_channel_strings(
    services: &[ChannelRecord],
    terrestrial_types: &HashMap<u8, String>,
) -> HashMap<(u16, u16), (String, String)> {
    // (nid, tsid) -> (mirakurun type, natural channel string), first row wins
    // (rows are already deduplicated and sorted by `unique_services`).
    let mut natural: Vec<((u16, u16), String, String)> = Vec::new();
    let mut seen: HashSet<(u16, u16)> = HashSet::new();
    for c in services {
        let key = (c.nid, c.tsid);
        if !seen.insert(key) {
            continue;
        }
        let ty = mirakurun_type_of(c, terrestrial_types);
        let Some(ch) = channel_string(c) else {
            seen.remove(&key);
            continue;
        };
        natural.push((key, ty, ch));
    }

    // How many multiplexes want each (type, channel)?
    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for (_, ty, ch) in &natural {
        *counts.entry((ty.clone(), ch.clone())).or_insert(0) += 1;
    }

    let mut assigned: HashMap<(u16, u16), (String, String)> = HashMap::new();
    let mut taken: HashSet<(String, String)> = HashSet::new();
    for ((nid, tsid), ty, ch) in natural {
        let mut candidate = ch.clone();
        if counts.get(&(ty.clone(), ch.clone())).copied().unwrap_or(0) > 1 {
            candidate = format!("{}_{}", ch, nid);
            if taken.contains(&(ty.clone(), candidate.clone())) {
                candidate = format!("{}_{}_{}", ch, nid, tsid);
            }
        }
        taken.insert((ty.clone(), candidate.clone()));
        assigned.insert((nid, tsid), (ty, candidate));
    }

    assigned
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

    // One row per service ([`unique_services`]), then grouped by the
    // multiplex each belongs to — **keyed by `(nid, tsid)`, not by the
    // channel string**: two different networks can land on the same RF
    // channel number in a multi-area install, and merging them into one
    // `Channel` entry would claim services from several stations belong to
    // the same multiplex. See [`assign_channel_strings`].
    let services = unique_services(channels);
    let terrestrial_types = terrestrial_type_map(&services, &web_state.mirakurun_home_regions);
    let channel_strings = assign_channel_strings(&services, &terrestrial_types);

    let mut order: Vec<(u16, u16)> = Vec::new();
    let mut groups: HashMap<(u16, u16), MirakurunChannel> = HashMap::new();

    for c in &services {
        let mux = (c.nid, c.tsid);
        let Some((ty, ch_str)) = channel_strings.get(&mux) else { continue };

        groups
            .entry(mux)
            .or_insert_with(|| {
                order.push(mux);
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

    // Exactly one entry per `(networkId, serviceId)` — see
    // [`unique_services`] for why the raw table has several rows per service
    // and what breaks in EPGStation when they are all emitted.
    let unique = unique_services(channels);
    let terrestrial_types = terrestrial_type_map(&unique, &web_state.mirakurun_home_regions);
    let channel_strings = assign_channel_strings(&unique, &terrestrial_types);

    // Read the logo directory once instead of stat-ing each of the several
    // hundred services — see [`collected_logo_keys`].
    let logo_keys = collected_logo_keys();

    let services: Vec<MirakurunService> = unique
        .iter()
        .filter_map(|c| {
            let (ty, ch_str) = channel_strings.get(&(c.nid, c.tsid))?;
            Some(MirakurunService {
                id: mirakurun_service_id(c.nid, c.sid),
                service_id: c.sid,
                network_id: c.nid,
                name: c.channel_name.clone().unwrap_or_default(),
                service_type: c.service_type.map(|v| v as i32).unwrap_or(0),
                remote_control_key_id: remote_control_key_id(c),
                // Single-element array — see `MirakurunService::channel` doc
                // comment.
                channel: vec![MirakurunChannelRef {
                    channel_type: ty.clone(),
                    channel: ch_str.clone(),
                }],
                has_logo_data: logo_keys.contains(&(c.nid, c.sid)),
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
/// `channel_resolve::resolve_service_by_nid_sid`, then streamed with the same
/// machinery `web/stream.rs`'s `GET /api/stream/service/:sid` uses
/// (`StreamCleanup`, `respond_with_stream`) — no `?profile=` transcoding
/// here: Mirakurun's convention is un-transcoded TS (STREAMING_DESIGN.md
/// §7.1), and query params other than none are ignored by design (a
/// `?decode=1`-style param some clients send is simply not read).
///
/// Unlike this project's own `/api/stream/service/:sid`, the body is filtered
/// down to the requested service
/// ([`crate::web::stream::service_filtered_body_stream`]) rather than being
/// the whole multiplex: Mirakurun's per-service stream carries one service,
/// and EPGStation records straight off this endpoint.
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
    respond_with_stream(service_filtered_body_stream(BodyReceiver::Tuner(tuner_rx), cleanup, sid))
}

/// `GET /mirakurun/api/channels/:type/:channel/stream`.
///
/// Finds the multiplex that `GET /channels` advertises under this
/// `(type, channel)` pair (see [`assign_channel_strings`] — the pair is
/// assigned so exactly one multiplex answers to it), then streams it exactly
/// like [`stream_service_by_mirakurun_id`] (raw TS
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

    // Resolve through the same `(type, channel)` assignment `GET /channels`
    // advertises ([`assign_channel_strings`]) — a bare [`channel_string`]
    // comparison would miss every multiplex that had to be disambiguated, and
    // would happily match a *different* network that shares the RF number.
    let target_id = {
        let db = web_state.database.lock().await;
        let channels = match usable_channels(&db) {
            Ok(c) => c,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        let services = unique_services(channels);
        let terrestrial_types = terrestrial_type_map(&services, &web_state.mirakurun_home_regions);
        let channel_strings = assign_channel_strings(&services, &terrestrial_types);
        services.iter().find_map(|c| {
            let bt = band_type_from_db(c.band_type);
            if !candidates.contains(&bt) {
                return None;
            }
            // Match the *assigned* type, not just the band: `GR` and `NW3`
            // are both terrestrial, and after the `GR`/`NWn` split they can
            // legitimately carry the same channel string (each `NWn` is its
            // own namespace, so RF 15 needs no disambiguation once the areas
            // are separated).
            let (ty, cs) = channel_strings.get(&(c.nid, c.tsid))?;
            (*ty == channel_type && *cs == channel_str).then_some(c.id)
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
/// - `types`: the Mirakurun channel types this driver actually has enabled,
///   scanned channels for ([`channel_types_by_driver`]). Real Mirakurun takes
///   this from `tuners.yml`; a BonDriver row here is not statically typed to a
///   band, but its scan results say exactly which bands it reached. A driver
///   with no scanned channels still reports `[]`.
/// - `isUsing`/`isFree`: `true`/`false` if any [`crate::tuner::pool::TunerPool`]
///   key whose `tuner_path` equals this driver's `dll_path` currently has a
///   running [`crate::tuner::shared::SharedTuner`] — same signal
///   [`get_status`] already aggregates into `runningTunerCount`.
/// - `isAvailable`: always `true` — this project does not track a
///   driver-level "administratively disabled" state distinct from
///   `is_enabled` on individual channels, and a driver with no channels
///   enabled is still a usable tuner slot.
pub async fn get_tuners(State(web_state): State<Arc<WebState>>) -> Response {
    let (drivers, types_by_driver) = {
        let db = web_state.database.lock().await;
        let drivers = match db.get_all_bon_drivers() {
            Ok(d) => d,
            Err(e) => return error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        // Which Mirakurun channel types each driver has scanned channels for.
        // A failed query only costs the `types` field, so degrade to empty
        // rather than failing the whole request.
        let types = match usable_channels(&db) {
            Ok(channels) => channel_types_by_driver(&channels),
            Err(e) => {
                debug!("mirakurun: /tuners could not read channels for `types` ({e}); reporting []");
                HashMap::new()
            }
        };
        (drivers, types)
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
                "types": types_by_driver.get(&d.id).cloned().unwrap_or_default(),
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

/// `GET /mirakurun/api/services/:id/logo`.
///
/// Serves the PNG the logo collector extracted from CDT on a live stream
/// (`tuner/logo_collector.rs`, stored as `logos/<nid>_<sid>.png`). Real
/// Mirakurun answers this from its own logo store and EPGStation proxies it
/// straight through as `GET /api/channels/:id/logo`, so the response has to
/// be the raw image — no envelope.
///
/// `404` when the file is not there: logos only exist for networks that have
/// been tuned at least once, so "no logo yet" is a normal answer, and it
/// matches what [`get_services`] reports through `hasLogoData` (clients only
/// call this endpoint when that flag is true).
pub async fn get_logo(Path(id): Path<u64>) -> Response {
    let Some((nid, sid)) = split_mirakurun_service_id(id) else {
        return error_response(StatusCode::BAD_REQUEST, format!("invalid service id {}", id));
    };

    match tokio::fs::read(logo_path(nid, sid)).await {
        Ok(bytes) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "image/png")],
            bytes,
        )
            .into_response(),
        Err(_) => error_response(StatusCode::NOT_FOUND, format!("service {} has no logo", id)),
    }
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
    // Deduplication of the per-BonDriver `channels` rows
    // ------------------------------------------------------------------

    /// A `channels` row for one service on one BonDriver, with only the
    /// fields the dedup/channel-string logic reads spelled out.
    #[allow(clippy::too_many_arguments)]
    fn service_row(
        id: i64,
        bon_driver_id: i64,
        nid: u16,
        sid: u16,
        tsid: u16,
        band_type: Option<u8>,
        physical_ch: Option<u8>,
        bon_channel: Option<u32>,
        channel_name: Option<&str>,
    ) -> ChannelRecord {
        ChannelRecord {
            id,
            bon_driver_id,
            nid,
            sid,
            tsid,
            channel_name: channel_name.map(|s| s.to_string()),
            physical_ch,
            bon_channel,
            band_type,
            ..make_channel_record(band_type, physical_ch, bon_channel)
        }
    }

    #[test]
    fn placeholder_names_are_recognized() {
        let placeholder = |name: Option<&str>| {
            is_placeholder_name(&service_row(1, 1, 4, 211, 16528, Some(1), None, Some(9), name))
        };
        assert!(placeholder(Some("BS09/TS1")));
        assert!(placeholder(Some("CS02/TS0")));
        assert!(placeholder(None));
        assert!(placeholder(Some("   ")));
        assert!(!placeholder(Some("ＢＳ１１イレブン")));
        // A real service name that merely contains digits and a slash.
        assert!(!placeholder(Some("15Ch(NHK-G)")));
    }

    /// The production case this dedup exists for: one service present on four
    /// BonDrivers, one of those rows still carrying the pre-SDT placeholder
    /// name and a `bon_channel` from a different driver's channel table.
    #[test]
    fn unique_services_keeps_one_row_per_service_and_prefers_the_scanned_one() {
        let scanned = ChannelRecord {
            service_type: Some(1),
            network_name: Some("ＢＳ　Ｄｉｇｉｔａｌ".to_string()),
            remote_control_key: Some(11),
            last_seen: Some(1_783_849_518),
            ..service_row(41, 1, 4, 211, 16528, Some(1), Some(9), Some(8), Some("ＢＳ１１イレブン"))
        };
        let placeholder = ChannelRecord {
            last_seen: Some(1_771_730_985),
            ..service_row(193, 4, 4, 211, 16528, Some(1), None, Some(9), Some("BS09/TS1"))
        };
        let bare = ChannelRecord {
            service_type: Some(1),
            last_seen: Some(1_771_824_058),
            ..service_row(877, 14, 4, 211, 16528, Some(1), None, Some(8), Some("ＢＳ１１イレブン"))
        };

        let unique = unique_services(vec![placeholder, bare, scanned]);
        assert_eq!(unique.len(), 1);
        assert_eq!(unique[0].id, 41, "the fully-scanned row must win");
    }

    #[test]
    fn unique_services_is_sorted_and_keeps_distinct_services() {
        let a = service_row(1, 1, 4, 152, 16400, Some(1), None, Some(0), Some("ＢＳ朝日２"));
        let b = service_row(2, 1, 4, 151, 16400, Some(1), None, Some(0), Some("ＢＳ朝日１"));
        let c = service_row(3, 1, 32416, 21504, 32416, Some(0), Some(15), Some(2), Some("ＮＨＫ総合"));

        let unique = unique_services(vec![a, b, c]);
        assert_eq!(
            unique.iter().map(|c| (c.nid, c.sid)).collect::<Vec<_>>(),
            vec![(4, 151), (4, 152), (32416, 21504)]
        );
    }

    // ------------------------------------------------------------------
    // GR / NW1..NW40 split
    // ------------------------------------------------------------------

    /// Terrestrial network ids of a few real areas, so the region derivation
    /// (`0x7FF0 - 0x10 × region + operator`) is exercised rather than mocked.
    const NID_FUKUSHIMA: u16 = 32416; // region 21
    const NID_AKITA: u16 = 32466; // region 18
    const NID_NIIGATA: u16 = 32256; // region 31
    const NID_TOKYO_WIDE: u16 = 32736; // region 1 (関東広域)

    fn gr_row(id: i64, nid: u16, sid: u16, physical_ch: u8) -> ChannelRecord {
        service_row(id, 1, nid, sid, nid, Some(0), Some(physical_ch), Some(2), Some("局"))
    }

    #[test]
    fn region_is_derived_from_the_network_id_when_the_column_is_empty() {
        let row = gr_row(1, NID_FUKUSHIMA, 21504, 15);
        assert_eq!(row.region_id, None, "this row has no scanned region_id");
        assert_eq!(region_id_of(&row), Some(21));

        // An explicitly scanned value wins over the derivation.
        let scanned = ChannelRecord { region_id: Some(9), ..gr_row(2, NID_FUKUSHIMA, 21504, 15) };
        assert_eq!(region_id_of(&scanned), Some(9));
    }

    #[test]
    fn without_a_home_region_everything_terrestrial_stays_gr() {
        let services = unique_services(vec![
            gr_row(1, NID_FUKUSHIMA, 21504, 15),
            gr_row(2, NID_AKITA, 18448, 15),
        ]);
        let types = terrestrial_type_map(&services, &[]);
        assert!(types.is_empty());
        for c in &services {
            assert_eq!(mirakurun_type_of(c, &types), "GR");
        }
    }

    /// The production shape: local prefecture on `GR`, every other area on
    /// its own `NWn`, numbered by ascending region id.
    #[test]
    fn out_of_area_regions_become_nw_types_in_region_order() {
        let services = unique_services(vec![
            gr_row(1, NID_FUKUSHIMA, 21504, 15), // region 21 (home)
            gr_row(2, NID_AKITA, 18448, 15),     // region 18
            gr_row(3, NID_NIIGATA, 12345, 15),   // region 31
        ]);
        let types = terrestrial_type_map(&services, &[21]);

        assert_eq!(types[&21], "GR");
        assert_eq!(types[&18], "NW1", "lowest non-home region id comes first");
        assert_eq!(types[&31], "NW2");
    }

    /// `home_region = "東京"` covers both the wide-area Kanto id and the
    /// Tokyo prefecture id, so both must land on `GR`.
    #[test]
    fn every_region_id_of_the_home_prefecture_is_gr() {
        let home = recisdb_protocol::broadcast_region::region_ids_from_prefecture_name("東京");
        assert_eq!(home, vec![1, 23], "sanity: 東京 is two region ids");

        let services = unique_services(vec![
            gr_row(1, NID_TOKYO_WIDE, 1024, 21), // region 1
            gr_row(2, NID_FUKUSHIMA, 21504, 15), // region 21
        ]);
        let types = terrestrial_type_map(&services, &home);
        let type_of = |nid: u16| {
            let row = services.iter().find(|c| c.nid == nid).unwrap();
            mirakurun_type_of(row, &types)
        };

        assert_eq!(type_of(NID_TOKYO_WIDE), "GR");
        assert_eq!(type_of(NID_FUKUSHIMA), "NW1");
    }

    #[test]
    fn satellite_rows_are_unaffected_by_the_split() {
        let bs = service_row(1, 1, 4, 211, 16528, Some(1), None, Some(8), Some("ＢＳ１１"));
        let types = terrestrial_type_map(std::slice::from_ref(&bs), &[21]);
        assert_eq!(mirakurun_type_of(&bs, &types), "BS");
    }

    /// EPGStation's `ChannelType` union stops at `NW40`; anything past that
    /// must fall back to a type it knows rather than inventing `NW41`.
    #[test]
    fn regions_past_nw40_fall_back_to_gr() {
        // 41 non-home regions: region ids 1..=41 minus the home one.
        let services: Vec<ChannelRecord> = (1..=42u8)
            .filter(|id| *id != 21)
            .enumerate()
            .map(|(i, region)| {
                let nid = 0x7FF0 - 0x10 * region as u16;
                gr_row(i as i64, nid, 100 + i as u16, 20)
            })
            .collect();
        let services = unique_services(services);
        let types = terrestrial_type_map(&services, &[21]);

        let assigned: Vec<&String> = types.values().collect();
        assert!(!assigned.iter().any(|t| t.as_str() == "NW41"));
        assert_eq!(assigned.iter().filter(|t| t.as_str() == "NW40").count(), 1);
        assert!(assigned.iter().any(|t| t.as_str() == "GR"), "the overflow falls back to GR");
    }

    #[test]
    fn nw_types_are_recognized_on_input_within_range_only() {
        assert!(is_nw_type("NW1"));
        assert!(is_nw_type("NW40"));
        assert!(!is_nw_type("NW0"));
        assert!(!is_nw_type("NW41"));
        assert!(!is_nw_type("NW01"), "no zero padding");
        assert!(!is_nw_type("NW"));
        assert!(!is_nw_type("nw1"), "types are case-sensitive");

        assert_eq!(
            mirakurun_type_to_band_candidates("NW3"),
            Some(&[BandType::Terrestrial, BandType::CATV][..])
        );
    }

    /// Separating the areas removes the reason to disambiguate: each `NWn`
    /// is its own namespace, so RF 15 can stay plain `"15"` in both.
    #[test]
    fn splitting_areas_removes_channel_string_collisions() {
        let services = unique_services(vec![
            gr_row(1, NID_FUKUSHIMA, 21504, 15),
            gr_row(2, NID_AKITA, 18448, 15),
        ]);
        let types = terrestrial_type_map(&services, &[21]);
        let assigned = assign_channel_strings(&services, &types);

        assert_eq!(assigned[&(NID_FUKUSHIMA, NID_FUKUSHIMA)], ("GR".to_string(), "15".to_string()));
        assert_eq!(assigned[&(NID_AKITA, NID_AKITA)], ("NW1".to_string(), "15".to_string()));
    }

    // ------------------------------------------------------------------
    // (type, channel) assignment
    // ------------------------------------------------------------------

    #[test]
    fn channel_strings_stay_plain_when_nothing_collides() {
        let services = unique_services(vec![
            service_row(1, 1, 32416, 21504, 32416, Some(0), Some(15), Some(2), Some("ＮＨＫ総合")),
            service_row(2, 1, 32417, 21512, 32417, Some(0), Some(13), Some(1), Some("ＮＨＫEテレ")),
        ]);
        let assigned = assign_channel_strings(&services, &HashMap::new());
        assert_eq!(assigned[&(32416, 32416)], ("GR".to_string(), "15".to_string()));
        assert_eq!(assigned[&(32417, 32417)], ("GR".to_string(), "13".to_string()));
    }

    /// Multi-area reception: two networks on RF 15. Both must remain
    /// addressable, so neither may keep the bare `"15"`.
    #[test]
    fn colliding_channel_strings_are_disambiguated_by_network_id() {
        let services = unique_services(vec![
            service_row(1, 1, 32416, 21504, 32416, Some(0), Some(15), Some(2), Some("ＮＨＫ総合・福島")),
            service_row(2, 1, 32466, 18448, 32466, Some(0), Some(15), Some(3), Some("ＡＢＳ秋田放送")),
        ]);
        let assigned = assign_channel_strings(&services, &HashMap::new());
        assert_eq!(assigned[&(32416, 32416)], ("GR".to_string(), "15_32416".to_string()));
        assert_eq!(assigned[&(32466, 32466)], ("GR".to_string(), "15_32466".to_string()));

        let all: HashSet<_> = assigned.values().collect();
        assert_eq!(all.len(), 2, "every multiplex must get its own (type, channel)");
    }

    /// Same number on two different bands is not a collision — Mirakurun keys
    /// on the pair, so `GR 15` and `BS 15` coexist.
    #[test]
    fn same_channel_number_on_different_bands_is_not_a_collision() {
        let services = unique_services(vec![
            service_row(1, 1, 32416, 21504, 32416, Some(0), Some(15), Some(2), Some("ＮＨＫ総合")),
            service_row(2, 1, 4, 211, 16528, Some(1), None, Some(15), Some("ＢＳ１１")),
        ]);
        let assigned = assign_channel_strings(&services, &HashMap::new());
        assert_eq!(assigned[&(32416, 32416)], ("GR".to_string(), "15".to_string()));
        assert_eq!(assigned[&(4, 16528)], ("BS".to_string(), "15".to_string()));
    }

    /// Two multiplexes of the *same* network colliding (different TSIDs on
    /// one nid) must still separate, via the `_<nid>_<tsid>` form.
    #[test]
    fn colliding_multiplexes_on_one_network_fall_back_to_tsid() {
        let services = unique_services(vec![
            service_row(1, 1, 4, 101, 16528, Some(1), None, Some(9), Some("A")),
            service_row(2, 2, 4, 102, 16530, Some(1), None, Some(9), Some("B")),
        ]);
        let assigned = assign_channel_strings(&services, &HashMap::new());
        let a = &assigned[&(4, 16528)];
        let b = &assigned[&(4, 16530)];
        assert_ne!(a, b, "distinct multiplexes must not share (type, channel)");
        assert!(b.1.starts_with("9_4"), "unexpected disambiguated form: {}", b.1);
    }

    // ------------------------------------------------------------------
    // remoteControlKeyId
    // ------------------------------------------------------------------

    #[test]
    fn remote_control_key_is_reported_for_terrestrial_only() {
        let terrestrial = ChannelRecord {
            remote_control_key: Some(1),
            ..service_row(1, 1, 32416, 21504, 32416, Some(0), Some(15), Some(2), Some("ＮＨＫ総合"))
        };
        assert_eq!(remote_control_key_id(&terrestrial), Some(1));

        // CS110 stores the 3-digit channel number in this column; it is not a
        // remote-control key and must not be reported as one.
        let cs = ChannelRecord {
            remote_control_key: Some(161),
            ..service_row(2, 1, 7, 301, 18224, Some(2), None, Some(1), Some("エンタメ〜テレ"))
        };
        assert_eq!(remote_control_key_id(&cs), None);

        let bs = ChannelRecord {
            remote_control_key: Some(11),
            ..service_row(3, 1, 4, 211, 16528, Some(1), None, Some(8), Some("ＢＳ１１"))
        };
        assert_eq!(remote_control_key_id(&bs), None);
    }

    #[test]
    fn service_omits_remote_control_key_id_when_unknown() {
        let service = MirakurunService {
            id: mirakurun_service_id(4, 211),
            service_id: 211,
            network_id: 4,
            name: "ＢＳ１１".to_string(),
            service_type: 1,
            remote_control_key_id: None,
            channel: vec![MirakurunChannelRef { channel_type: "BS".to_string(), channel: "8".to_string() }],
            has_logo_data: false,
        };
        let value = serde_json::to_value(&service).unwrap();
        assert!(
            value.get("remoteControlKeyId").is_none(),
            "unknown remoteControlKeyId must be omitted, not null"
        );
    }

    // ------------------------------------------------------------------
    // /tuners `types`
    // ------------------------------------------------------------------

    #[test]
    fn tuner_types_come_from_scanned_channels_in_band_order() {
        let channels = vec![
            service_row(1, 1, 4, 211, 16528, Some(1), None, Some(8), Some("ＢＳ１１")),
            service_row(2, 1, 32416, 21504, 32416, Some(0), Some(15), Some(2), Some("ＮＨＫ総合")),
            service_row(3, 1, 7, 301, 18224, Some(2), None, Some(1), Some("エンタメ〜テレ")),
            service_row(4, 2, 32416, 21504, 32416, Some(0), Some(15), Some(2), Some("ＮＨＫ総合")),
        ];
        let types = channel_types_by_driver(&channels);
        assert_eq!(types[&1], vec!["GR", "BS", "CS"]);
        assert_eq!(types[&2], vec!["GR"]);
        assert!(types.get(&3).is_none(), "a driver with no channels reports nothing");
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
            remote_control_key_id: Some(1),
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
    // hasLogoData / getLogoImage
    // ------------------------------------------------------------------

    #[test]
    fn has_logo_data_follows_whether_a_logo_file_was_collected() {
        let dir = std::env::temp_dir().join("recisdb-proxy-mirakurun-logo-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("32416_21504.png"), b"png").unwrap();

        // Same lookup `get_services` does per service.
        let keys = crate::tuner::logo_collector::collected_logo_keys_in(&dir);
        assert!(keys.contains(&(32416, 21504)), "a collected logo is reported");
        assert!(
            !keys.contains(&(32416, 21505)),
            "a service whose network has been tuned but that has no logo file of its own is not"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn logo_endpoint_rejects_an_id_that_is_not_a_mirakurun_service_id() {
        // `get_logo` answers 400 (not 404) for these — the id could never have
        // named a service, so it is a malformed request rather than a miss.
        assert!(split_mirakurun_service_id(u64::MAX).is_none());
        assert_eq!(split_mirakurun_service_id(mirakurun_service_id(32416, 21504)), Some((32416, 21504)));
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
