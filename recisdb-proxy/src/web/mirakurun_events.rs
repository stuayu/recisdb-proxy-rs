//! `GET /mirakurun/api/events/stream` (`docs/EPGSTATION_COMPAT.md` §3/§6).
//!
//! EPGStation's EPG updater uses this to learn about program changes
//! (extension/推し戻し/新規) without waiting for its next full `GET
//! /programs` poll (`epgUpdateIntervalTime`). The source of truth is
//! `crate::epg_writer::EpgWriter`: every time it successfully UPSERTs a
//! batch into the `programs` table, it also broadcasts each row (as a
//! [`crate::database::ProgramUpsert`]) on `WebState::epg_events_tx`. This
//! handler does nothing but `.subscribe()` to that channel and re-encode
//! each record into Mirakurun's wire format.
//!
//! # Wire format — do not "clean this up", it is exactly what the client parses
//!
//! This is **not** a standard newline-delimited-JSON or SSE stream. It is
//! reverse-engineered from the actual client
//! (`EPGUpdateManageModel.ts::startAnalayzingMirakurunEvents`, 354-431, and
//! the `\n,\n` sentinel used at line 391/431 of the same file — read via the
//! project's copy at `/Users/ayumu/prog/EPGStation`), which:
//!
//! 1. Appends every received chunk to an in-memory buffer `tmp`.
//! 2. The moment `tmp`'s **last 4 bytes** equal `}\n,\n` (0x7d 0x0a 0x2c
//!    0x0a), it does `JSON.parse("[" + tmp.slice(0, -3) + "]")` — i.e. it
//!    drops the trailing `\n,\n` (3 chars: `\n`, `,`, `\n`), leaving `...}`,
//!    wraps it in `[` `]`, and parses. `tmp` is then reset to empty.
//! 3. A chunk that is *exactly* `[\n` is recognized and ignored outright
//!    (never appended to `tmp`) — this lets the stream announce itself as
//!    "a JSON array is starting" the way a real array literal would, without
//!    that opening bracket ever becoming part of a parsed chunk.
//!
//! Two things follow directly from that parser, and both are load-bearing:
//! - **One `write()`/chunk == one event.** If two events' bytes ever land in
//!   the same chunk, or if `[\n` is not its own standalone chunk, the
//!   client's `tmp.slice(0, -3)` trick produces invalid JSON (or, worse,
//!   *not-obviously-invalid* JSON that silently mis-parses) and that data is
//!   never recovered — `tmp` is only reset on a successful `}\n,\n` match, so
//!   one bad frame corrupts every event after it until the connection is
//!   torn down and reopened. This is why [`encode_event`] returns one
//!   already-complete `Bytes` per event, and why the preamble is `.await`ed
//!   as its own `unfold` step before the receive loop starts, not
//!   concatenated onto the first event's bytes.
//! - **The JSON payload must not itself contain a raw `\n`.**
//!   `serde_json::to_string` never emits an unescaped newline (control
//!   characters inside JSON strings are always escaped as `\n`), so this
//!   holds automatically as long as nothing here hand-builds the JSON text
//!   — do not switch this to writing formatted/pretty JSON.
//!
//! The stream is also expected to **never close** in normal operation:
//! EPGStation's `res.on('close'/'end', ...)` handlers (`EPGUpdateManageModel.ts`
//! 377-386) treat a closed connection as an *error* and throw. This module
//! therefore never voluntarily ends the body — see [`stream_events`]'s
//! handling of [`broadcast::error::RecvError`].
//!
//! # Event shape and filtering
//!
//! `resource: "program"` only — `"service"`/`"tuner"`/`"job"`/`"job_schedule"`
//! events are out of scope for this pass (this project has no service-level
//! change detection to report, and EPGStation only consumes `"program"` and
//! `"service"` anyway, `EPGUpdateManageModel.ts:413-417`). `type` is always
//! `"update"`: this project's `programs` UPSERT (`database/program.rs`) does
//! not distinguish "this event id is new" from "this event id already
//! existed and changed" — both come through `EpgWriter::flush` identically —
//! and EPGStation itself treats `"create"`/`"update"` the same way
//! (`EPGUpdateManageModel.ts:552`), so there is no client-visible difference
//! and no `"remove"` is ever sent (this project has no mechanism to detect a
//! program disappearing from the EIT, only ones appearing/changing).
//!
//! `?resource=` / `?type=` are accepted and honoured the way upstream
//! Mirakurun does (`src/Mirakurun/api/events/stream.ts`: an event whose
//! `resource`/`type` does not equal the query value is simply not written).
//! Since everything emitted here is `program`/`update`, any *other* value
//! yields a stream that stays open and sends nothing — which is exactly what
//! upstream does too, and is why the filter lives in [`encode_event`] rather
//! than short-circuiting the connection. See [`EventsQuery`].
//!
//! Records with `data.name == None` are filtered out **in `EpgWriter::flush`**
//! (not here) before they are even sent on the broadcast channel —
//! EPGStation discards `program` events with an `undefined` name
//! (`EPGUpdateManageModel.ts:554`) so forwarding them at all would be pure
//! waste; see `epg_writer.rs` for that filter.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Query, State},
    http::{header, StatusCode},
    response::Response,
};
use bytes::Bytes;
use futures::stream::{self, Stream};
use log::warn;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast;

use crate::database::ProgramUpsert;
use crate::web::mirakurun::program_upsert_to_mirakurun;
use crate::web::state::WebState;

/// The standalone first chunk the client recognizes and discards outright
/// (see module doc comment, point 3). Must be sent as its own `unfold` step
/// — concatenating it onto the first event's bytes would make the client
/// treat `[\n{...}` as one chunk and fail to strip the `[\n` prefix.
const PREAMBLE: &[u8] = b"[\n";

/// The only `resource`/`type` this project ever emits — see the module doc
/// comment's "Event shape and filtering" section. Kept as constants so
/// [`EventsQuery::matches`] and [`encode_event`] cannot drift apart.
const EVENT_RESOURCE: &str = "program";
const EVENT_TYPE: &str = "update";

/// `?resource=&type=` — the two filters real Mirakurun's own
/// `/events/stream` accepts (upstream `src/Mirakurun/api/events/stream.ts`
/// drops any event whose `resource`/`type` does not match the query).
///
/// EPGStation calls `getEventsStream()` with no arguments, so in the
/// EPGStation path both are always `None`; they exist so another Mirakurun
/// client that *does* filter gets the same behaviour here as upstream,
/// rather than a full firehose it did not ask for. An unrecognized value is
/// not an error upstream either — it simply matches nothing, which is what
/// [`Self::matches`] does.
#[derive(Debug, Default, Deserialize)]
pub struct EventsQuery {
    resource: Option<String>,
    #[serde(rename = "type")]
    event_type: Option<String>,
}

impl EventsQuery {
    /// Whether an event of `(resource, event_type)` passes this filter. An
    /// absent query field matches everything (upstream: `if (req.query.x &&
    /// req.query.x !== message.x) return;`).
    fn matches(&self, resource: &str, event_type: &str) -> bool {
        self.resource.as_deref().is_none_or(|r| r == resource)
            && self.event_type.as_deref().is_none_or(|t| t == event_type)
    }
}

/// `unfold` state: whether the standalone `PREAMBLE` chunk has been emitted
/// yet, plus the live receiver once it has.
enum StreamState {
    Preamble(broadcast::Receiver<ProgramUpsert>),
    Streaming(broadcast::Receiver<ProgramUpsert>),
}

/// Encode one [`ProgramUpsert`] as a single wire chunk: a one-line JSON
/// `Event<MirakurunProgram>` object followed by the `\n,\n` sentinel the
/// client's framing depends on (module doc comment). Returns `None` for a
/// record with no `name` — those are already filtered out by
/// `EpgWriter::flush` before reaching the broadcast channel, but the check
/// is repeated here defensively (a future direct sender must not have to
/// remember that invariant to stay correct).
fn encode_event(record: &ProgramUpsert, filter: &EventsQuery) -> Option<Bytes> {
    record.name.as_ref()?;
    if !filter.matches(EVENT_RESOURCE, EVENT_TYPE) {
        return None;
    }

    let program = program_upsert_to_mirakurun(record);
    let event = json!({
        "resource": EVENT_RESOURCE,
        "type": EVENT_TYPE,
        "data": program,
        "time": chrono::Utc::now().timestamp_millis(),
    });

    // `serde_json::to_string` (not `to_string_pretty`) is required: the
    // client's `\n,\n`-terminated framing (module doc comment) breaks if the
    // JSON body itself contains raw newlines.
    let mut text = serde_json::to_string(&event).ok()?;
    text.push_str("\n,\n");
    Some(Bytes::from(text))
}

/// `GET /mirakurun/api/events/stream`. See module doc comment for the wire
/// format and event-shape rules — both are reverse-engineered from
/// EPGStation's actual parser, not a specification, so changes here must be
/// checked against that parser, not "what looks like reasonable JSON
/// streaming".
pub async fn stream_events(
    State(web_state): State<Arc<WebState>>,
    Query(filter): Query<EventsQuery>,
) -> Response {
    let rx = web_state.epg_events_tx.subscribe();
    let body_stream = event_body_stream(rx, filter);

    Response::builder()
        .status(StatusCode::OK)
        // `; charset=utf-8` matches upstream Mirakurun byte for byte
        // (`src/Mirakurun/api/events/stream.ts`). The npm client reads this
        // endpoint through `_requestStream`, which never inspects the
        // content type, so this is for parity/other clients, not EPGStation.
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(body_stream))
        .expect("static headers/streaming body are always a valid response")
}

fn event_body_stream(
    rx: broadcast::Receiver<ProgramUpsert>,
    filter: EventsQuery,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static {
    // The filter rides along in the `unfold` state rather than being
    // captured by the closure: the closure is called once per chunk, so an
    // `async move` body cannot own it.
    stream::unfold((StreamState::Preamble(rx), filter), |(state, filter)| async move {
        match state {
            StreamState::Preamble(rx) => {
                // Sent as its own chunk (module doc comment, point 3) — the
                // next `unfold` call moves straight to `Streaming` so this
                // branch runs exactly once per connection.
                Some((Ok(Bytes::from_static(PREAMBLE)), (StreamState::Streaming(rx), filter)))
            }
            StreamState::Streaming(mut rx) => loop {
                match rx.recv().await {
                    Ok(record) => {
                        let Some(chunk) = encode_event(&record, &filter) else { continue };
                        return Some((Ok(chunk), (StreamState::Streaming(rx), filter)));
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // CLAUDE.md: `RecvError::Lagged` must be handled
                        // explicitly, never silently ignored. Missing a few
                        // program updates is harmless here — EPGStation
                        // falls back to its periodic full `GET /programs`
                        // poll regardless, so it will pick up whatever this
                        // subscriber missed. Closing the stream over a lag
                        // would be strictly worse: EPGStation treats a
                        // closed connection as an error and throws
                        // (module doc comment), so the correct response to
                        // "we fell behind" is to log and keep going, not to
                        // end the stream.
                        warn!("[mirakurun events] receiver lagged, skipped {} event(s)", n);
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Only happens if every `Sender` (the one
                        // `EpgWriter` holds, shared via `WebState`) is
                        // dropped — i.e. process shutdown. Ending the body
                        // here is fine at that point; there is nothing left
                        // to send it.
                        return None;
                    }
                }
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_upsert(name: Option<&str>) -> ProgramUpsert {
        ProgramUpsert {
            nid: 1,
            sid: 100,
            tsid: 200,
            event_id: 5,
            start_at: 1_700_000_000,
            duration_secs: 1800,
            name: name.map(|s| s.to_string()),
            description: Some("desc".to_string()),
            extended: None,
            genre: Some(0x71),
            free_ca_mode: false,
            updated_at: 1_700_000_000,
        }
    }

    // ------------------------------------------------------------------
    // Framing: reproduce EPGStation's own parser
    // (`EPGUpdateManageModel.ts::startAnalayzingMirakurunEvents`,
    // 391-431) directly, rather than asserting on byte contents, so this
    // test fails the same way EPGStation's client would if the framing
    // regresses.
    // ------------------------------------------------------------------

    #[test]
    fn preamble_chunk_is_exactly_the_bytes_the_client_recognizes_and_discards() {
        assert_eq!(PREAMBLE, b"[\n");
    }

    #[test]
    fn event_chunk_ends_with_the_client_sentinel_and_reparses_via_the_client_algorithm() {
        let record = sample_upsert(Some("Test Program"));
        let chunk = encode_event(&record, &EventsQuery::default()).expect("named record must encode");
        let text = std::str::from_utf8(&chunk).unwrap();

        // Point 2 of the module doc comment: client recognizes completion by
        // the chunk/buffer ending in this exact 4-byte sequence.
        assert!(text.ends_with("}\n,\n"), "chunk must end with '}}\\n,\\n', got: {:?}", text);

        // Reproduce `tmp.slice(0, -3)` (drop the trailing 3 chars: \n , \n),
        // then the client's `"[" + ... + "]"` wrap, then JSON.parse.
        let sliced = &text[..text.len() - 3];
        let wrapped = format!("[{}]", sliced);
        let parsed: serde_json::Value =
            serde_json::from_str(&wrapped).expect("client's slice(0,-3)+[]-wrap reconstruction must be valid JSON");

        let arr = parsed.as_array().expect("must parse to a JSON array");
        assert_eq!(arr.len(), 1);
        let event = &arr[0];
        assert_eq!(event["resource"], "program");
        assert_eq!(event["type"], "update");
        assert_eq!(event["data"]["name"], "Test Program");
        assert_eq!(event["data"]["serviceId"], 100);
        assert_eq!(event["data"]["networkId"], 1);
        assert_eq!(event["data"]["eventId"], 5);
        assert!(event["time"].is_i64() || event["time"].is_u64());
    }

    #[test]
    fn full_two_chunk_connection_opening_reparses_correctly_when_concatenated_and_split_at_the_client_sentinel() {
        // Simulates the client's actual receive loop: PREAMBLE arrives and
        // is recognized/discarded as its own chunk (never touching `tmp`),
        // then the event chunk arrives, gets appended to (empty) `tmp`, and
        // is parsed the moment `tmp` ends in the sentinel.
        let preamble_chunk = PREAMBLE;
        assert_eq!(preamble_chunk, b"[\n"); // "chunk === '[\\n'" check in the client -> discarded, tmp stays "".

        let record = sample_upsert(Some("Another Program"));
        let event_chunk = encode_event(&record, &EventsQuery::default()).unwrap();
        let tmp = String::from_utf8(event_chunk.to_vec()).unwrap();
        assert!(tmp.ends_with("}\n,\n"));

        let wrapped = format!("[{}]", &tmp[..tmp.len() - 3]);
        let parsed: serde_json::Value = serde_json::from_str(&wrapped).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 1);
    }

    // ------------------------------------------------------------------
    // name == None must never be encoded (EPGStation drops nameless
    // program events, `EPGUpdateManageModel.ts:554`).
    // ------------------------------------------------------------------

    #[test]
    fn record_without_a_name_is_not_encoded() {
        let record = sample_upsert(None);
        assert!(encode_event(&record, &EventsQuery::default()).is_none());
    }

    // ------------------------------------------------------------------
    // ?resource=&type= filtering, matching upstream Mirakurun's own
    // `src/Mirakurun/api/events/stream.ts` ("if the query names a value and
    // the event does not match it, drop the event").
    // ------------------------------------------------------------------

    fn query(resource: Option<&str>, event_type: Option<&str>) -> EventsQuery {
        EventsQuery {
            resource: resource.map(|s| s.to_string()),
            event_type: event_type.map(|s| s.to_string()),
        }
    }

    #[test]
    fn absent_query_fields_match_every_event() {
        let record = sample_upsert(Some("Test Program"));
        assert!(encode_event(&record, &query(None, None)).is_some());
    }

    #[test]
    fn matching_resource_and_type_still_encode() {
        let record = sample_upsert(Some("Test Program"));
        assert!(encode_event(&record, &query(Some("program"), Some("update"))).is_some());
    }

    #[test]
    fn non_matching_resource_or_type_filters_the_event_out() {
        let record = sample_upsert(Some("Test Program"));
        // This project only ever emits resource=program / type=update, so a
        // client asking for anything else must receive nothing rather than
        // the wrong events.
        assert!(encode_event(&record, &query(Some("service"), None)).is_none());
        assert!(encode_event(&record, &query(None, Some("remove"))).is_none());
        assert!(encode_event(&record, &query(Some("tuner"), Some("create"))).is_none());
    }

    // ------------------------------------------------------------------
    // JSON body never contains a raw newline (would break the client's
    // \n,\n-terminated framing if it ever did).
    // ------------------------------------------------------------------

    #[test]
    fn encoded_event_json_body_has_no_embedded_raw_newline() {
        // A description containing a literal newline must come out
        // escaped (\n) by serde_json, not as a raw byte, or the client's
        // framing would see it as extra "\n,\n"-eligible content.
        let mut record = sample_upsert(Some("Title\nWith Newline"));
        record.description = Some("Line one\nLine two".to_string());
        let chunk = encode_event(&record, &EventsQuery::default()).unwrap();
        let text = std::str::from_utf8(&chunk).unwrap();

        // Exactly one trailing raw newline as part of the "\n,\n" sentinel
        // is expected; strip it and confirm nothing else remains.
        let json_part = text.strip_suffix("\n,\n").expect("must end with the sentinel");
        assert!(!json_part.contains('\n'), "JSON body must not contain a raw newline: {:?}", json_part);

        // And it must still parse back with the escaped newline intact.
        let value: serde_json::Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(value["data"]["name"], "Title\nWith Newline");
    }

    // ------------------------------------------------------------------
    // Stream end-to-end via `event_body_stream` + a real broadcast channel.
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn stream_emits_preamble_then_named_events_and_skips_unnamed_ones() {
        use futures::StreamExt;

        let (tx, rx) = broadcast::channel(16);
        let mut body = Box::pin(event_body_stream(rx, EventsQuery::default()));

        // First chunk is always the standalone preamble.
        let first = body.next().await.unwrap().unwrap();
        assert_eq!(&first[..], PREAMBLE);

        // Send one nameless (must be skipped) and one named record.
        tx.send(sample_upsert(None)).unwrap();
        tx.send(sample_upsert(Some("Real Program"))).unwrap();

        let second = body.next().await.unwrap().unwrap();
        let text = std::str::from_utf8(&second).unwrap();
        assert!(text.ends_with("}\n,\n"));
        let wrapped = format!("[{}]", &text[..text.len() - 3]);
        let parsed: serde_json::Value = serde_json::from_str(&wrapped).unwrap();
        assert_eq!(parsed[0]["data"]["name"], "Real Program");
    }

    #[tokio::test]
    async fn stream_does_not_end_on_lag_and_keeps_delivering_subsequent_events() {
        use futures::StreamExt;

        // Small capacity so a burst of sends before any recv() forces a lag.
        let (tx, rx) = broadcast::channel(2);
        let mut body = Box::pin(event_body_stream(rx, EventsQuery::default()));

        let preamble = body.next().await.unwrap().unwrap();
        assert_eq!(&preamble[..], PREAMBLE);

        // Overflow the small buffer.
        for i in 0..5u16 {
            let mut r = sample_upsert(Some("Lag Test"));
            r.event_id = i;
            let _ = tx.send(r);
        }
        // One more, sent after the receiver is known to have lagged, so
        // there is something deterministic left to observe.
        let mut last = sample_upsert(Some("After Lag"));
        last.event_id = 99;
        tx.send(last).unwrap();

        // The stream must still be alive and eventually yield an event
        // rather than ending because of the lag.
        let chunk = body.next().await.unwrap().unwrap();
        let text = std::str::from_utf8(&chunk).unwrap();
        assert!(text.contains("\"eventId\":"));
    }

    #[tokio::test]
    async fn stream_ends_when_sender_is_dropped() {
        use futures::StreamExt;

        let (tx, rx) = broadcast::channel::<ProgramUpsert>(4);
        let mut body = Box::pin(event_body_stream(rx, EventsQuery::default()));

        let preamble = body.next().await.unwrap().unwrap();
        assert_eq!(&preamble[..], PREAMBLE);

        drop(tx);
        assert!(body.next().await.is_none());
    }
}
