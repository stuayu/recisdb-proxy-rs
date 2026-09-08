//! Browser-facing EPG updates: `GET /api/epg/events`.

use std::{convert::Infallible, sync::Arc, time::Duration};

use axum::{
    extract::State,
    http::header,
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
};
use futures::stream::{self, Stream};
use log::warn;
use serde::Serialize;
use serde_json::json;
use tokio::sync::broadcast;

use crate::database::ProgramUpsert;
use crate::web::state::WebState;

use super::epg::get_epg_status_value;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EpgEventType {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Serialize)]
struct ProgramEventApi {
    nid: u16,
    sid: u16,
    tsid: u16,
    event_id: u16,
    start_at: i64,
    duration_secs: i64,
    free_ca_mode: bool,
    name: Option<String>,
    description: Option<String>,
    extended: Option<String>,
    genre: Option<i64>,
}

impl From<&ProgramUpsert> for ProgramEventApi {
    fn from(p: &ProgramUpsert) -> Self {
        Self {
            nid: p.nid,
            sid: p.sid,
            tsid: p.tsid,
            event_id: p.event_id,
            start_at: p.start_at,
            duration_secs: p.duration_secs,
            free_ca_mode: p.free_ca_mode,
            name: p.name.clone(),
            description: p.description.clone(),
            extended: p.extended.clone(),
            genre: p.genre,
        }
    }
}

fn program_event_frame(program: &ProgramUpsert) -> String {
    serde_json::to_string(&json!({
        "type": EpgEventType::Update,
        "program": ProgramEventApi::from(program),
    }))
    .expect("EPG event payload is serializable")
}

#[cfg(test)]
fn program_event_sse(program: &ProgramUpsert) -> String {
    format!("event: program\ndata: {}\n\n", program_event_frame(program))
}

#[cfg(test)]
fn lagged_event_sse(skipped: u64) -> String {
    format!("event: lagged\ndata: {{\"skipped\":{skipped}}}\n\n")
}

fn ping_event() -> Event {
    Event::default().event("ping").data("")
}

fn status_event(value: serde_json::Value) -> Event {
    Event::default()
        .event("epg_status")
        .json_data(json!({"states": value["states"]}))
        .expect("EPG status payload is serializable")
}

struct StreamState {
    rx: broadcast::Receiver<ProgramUpsert>,
    ping: tokio::time::Interval,
    status: tokio::time::Interval,
    web_state: Arc<WebState>,
}

/// Browser clients can reconnect, so process shutdown is the only reason to
/// end this stream. A lag is reported and the receiver keeps running; the UI
/// then performs a full program fetch.
fn event_stream(
    rx: broadcast::Receiver<ProgramUpsert>,
    web_state: Arc<WebState>,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    let mut status = tokio::time::interval(Duration::from_secs(30));
    ping.reset();
    status.reset();
    stream::unfold(
        StreamState {
            rx,
            ping,
            status,
            web_state,
        },
        |mut state| async move {
            tokio::select! {
                result = state.rx.recv() => {
                    let event = match result {
                        Ok(program) => Event::default()
                            .event("program")
                            .data(program_event_frame(&program)),
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            warn!("[epg events] receiver lagged, skipped {} event(s)", skipped);
                            Event::default().event("lagged").data(format!("{{\"skipped\":{skipped}}}"))
                        }
                        Err(broadcast::error::RecvError::Closed) => return None,
                    };
                    Some((Ok(event), state))
                }
                _ = state.ping.tick() => Some((Ok(ping_event()), state)),
                _ = state.status.tick() => {
                    match get_epg_status_value(&state.web_state).await {
                        Ok(value) => Some((Ok(status_event(value)), state)),
                        Err(error) => {
                            warn!("[epg events] failed to read EPG status: {:?}", error);
                            Some((Ok(ping_event()), state))
                        }
                    }
                }
            }
        },
    )
}

pub async fn get_epg_events(State(web_state): State<Arc<WebState>>) -> Response {
    let response = Sse::new(event_stream(web_state.epg_events_tx.subscribe(), web_state))
        .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response();
    let (mut parts, body) = response.into_parts();
    parts
        .headers
        .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    parts
        .headers
        .insert("x-accel-buffering", "no".parse().unwrap());
    Response::from_parts(parts, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ProgramUpsert {
        ProgramUpsert {
            nid: 1,
            sid: 2,
            tsid: 3,
            event_id: 4,
            start_at: 5,
            duration_secs: 6,
            free_ca_mode: false,
            name: Some("name".into()),
            description: Some("description".into()),
            extended: None,
            genre: Some(7),
            updated_at: 8,
            source: crate::database::EpgSource::Schedule,
            basic_updated_at: Some(8),
            extended_updated_at: None,
        }
    }

    #[test]
    fn epg_event_type_serializes_as_snake_case() {
        assert_eq!(
            serde_json::to_value(EpgEventType::Update).unwrap(),
            "update"
        );
        assert_eq!(
            serde_json::to_value(EpgEventType::Delete).unwrap(),
            "delete"
        );
    }

    #[test]
    fn epg_event_payload_matches_program_api_field_names() {
        let value: serde_json::Value =
            serde_json::from_str(&program_event_frame(&sample())).unwrap();
        let keys = value["program"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let expected = [
            "nid",
            "sid",
            "tsid",
            "event_id",
            "start_at",
            "duration_secs",
            "free_ca_mode",
            "name",
            "description",
            "extended",
            "genre",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(keys, expected);
        assert!(!keys.contains("id"));
    }

    #[test]
    fn lagged_event_encodes_skipped_count() {
        assert_eq!(
            lagged_event_sse(12),
            "event: lagged\ndata: {\"skipped\":12}\n\n"
        );
    }

    #[test]
    fn program_event_encodes_as_single_sse_frame() {
        let frame = program_event_sse(&sample());
        assert!(frame.starts_with("event: program\ndata: "));
        assert!(frame.ends_with("\n\n"));
        assert_eq!(frame.matches("\n\n").count(), 1);
        assert!(!frame["event: program\ndata: ".len()..frame.len() - 2].contains('\n'));
    }
}
