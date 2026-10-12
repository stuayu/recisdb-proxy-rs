//! Periodic program-table synchronization from paired nodes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::database::{ProgramUpsertWithClocks, Result as DatabaseResult};
use crate::server::listener::DatabaseHandle;

use super::store::NodeStore;
use super::transport::{
    EpgProgramsReply, EpgProgramsRequest, NodeTransportClient, NodeTransportState,
};
use super::{NodeCredential, NodeId};

pub const INITIAL_DELAY: Duration = Duration::from_secs(60);
pub const SYNC_INTERVAL: Duration = Duration::from_secs(600);
const MAX_PAGES_PER_CYCLE: usize = 100;
const PAGE_LIMIT: u32 = 1000;
/// 周期をまたぐときにカーソルを巻き戻す秒数。供給側の EpgWriter は受信から
/// 最大10秒遅れて書き込むので、読んだ時点のカーソルより古い `updated_at` の行が
/// あとから現れる。巻き戻した分は UPSERT が同じ行として吸収する。
const CURSOR_REWIND_SECS: i64 = 120;

pub struct EpgProgramSync {
    state: Arc<NodeTransportState>,
    database: DatabaseHandle,
    cursors: HashMap<NodeId, (i64, i64)>,
}

impl EpgProgramSync {
    pub fn new(state: Arc<NodeTransportState>, database: DatabaseHandle) -> Self {
        Self {
            state,
            database,
            cursors: HashMap::new(),
        }
    }

    pub fn spawn(self) {
        tokio::spawn(async move {
            tokio::time::sleep(INITIAL_DELAY).await;
            let mut task = self;
            loop {
                task.sync_once().await;
                tokio::time::sleep(SYNC_INTERVAL).await;
            }
        });
    }

    async fn peers(
        &self,
    ) -> Vec<(
        NodeId,
        String,
        NodeCredential,
        Vec<super::types::NodeEndpoint>,
    )> {
        let db = self.database.lock().await;
        let Ok(store) = NodeStore::new(&db) else {
            return Vec::new();
        };
        let Ok(nodes) = store.list_nodes() else {
            return Vec::new();
        };
        nodes
            .into_iter()
            .filter(|node| node.enabled)
            .filter_map(|node| {
                let credential = store.credential_for(&node.node_id).ok().flatten()?;
                let endpoints = store.endpoints(&node.node_id).unwrap_or_default();
                Some((node.node_id, node.display_name, credential, endpoints))
            })
            .collect()
    }

    async fn sync_once(&mut self) {
        let enabled = {
            let db = self.database.lock().await;
            match db.get_epg_effective(None) {
                Ok(config) if config.effective.enabled && config.effective.allow_remote => true,
                Ok(_) => false,
                Err(error) => {
                    log::warn!("[node] EPG program sync settings failed: {error}");
                    false
                }
            }
        };
        if !enabled {
            return;
        }

        let network_ids = {
            let db = self.database.lock().await;
            match db.get_enabled_network_ids() {
                Ok(ids) => ids,
                Err(error) => {
                    log::warn!("[node] EPG program sync channels failed: {error}");
                    return;
                }
            }
        };
        for (node_id, name, credential, endpoints) in self.peers().await {
            let Ok(client) =
                NodeTransportClient::new(self.state.identity.node_id.clone(), credential)
            else {
                continue;
            };
            let mut cursor = *self.cursors.get(&node_id).unwrap_or(&(0, 0));
            let mut rows = 0usize;
            let mut muxes = 0usize;
            let mut failed = None;
            let mut completed = false;
            for page in 0..MAX_PAGES_PER_CYCLE {
                let request = EpgProgramsRequest {
                    network_ids: network_ids.clone(),
                    cursor_updated_at: cursor.0,
                    cursor_id: cursor.1,
                    limit: PAGE_LIMIT,
                    include_coverage: page == 0,
                };
                let mut reply = None;
                let mut last_error = None;
                for endpoint in endpoints.iter().filter(|endpoint| endpoint.enabled) {
                    match client.fetch_epg_programs(&endpoint.address, &request).await {
                        Ok(value) => {
                            reply = Some(value);
                            break;
                        }
                        Err(error) => last_error = Some(error),
                    }
                }
                let Some(reply) = reply else {
                    if let Some(error) = last_error {
                        if matches!(
                            error.status(),
                            Some(
                                reqwest::StatusCode::NOT_FOUND
                                    | reqwest::StatusCode::METHOD_NOT_ALLOWED
                            )
                        ) {
                            log::debug!("[node] EPG program sync from {name} is unsupported");
                        } else {
                            failed = Some(error.to_string());
                        }
                    }
                    break;
                };
                let next = (reply.next_cursor_updated_at, reply.next_cursor_id);
                match apply_reply(&self.database, &network_ids, &reply).await {
                    Ok((applied, coverage_count)) => {
                        rows += applied;
                        muxes += coverage_count;
                    }
                    Err(error) => {
                        failed = Some(error.to_string());
                        break;
                    }
                }
                cursor = next;
                if !reply.has_more {
                    completed = true;
                    break;
                }
                if next == (request.cursor_updated_at, request.cursor_id) {
                    failed = Some("peer returned an unchanged cursor".into());
                    break;
                }
                tokio::task::yield_now().await;
            }
            if completed || rows > 0 {
                let stored = if completed {
                    ((cursor.0 - CURSOR_REWIND_SECS).max(0), 0)
                } else {
                    cursor
                };
                self.cursors.insert(node_id.clone(), stored);
                if rows == 0 {
                    log::debug!("[node] EPG sync from {name}: 0 rows, {muxes} muxes coverage");
                } else {
                    log::info!("[node] EPG sync from {name}: {rows} rows, {muxes} muxes coverage");
                }
            }
            if let Some(error) = failed {
                log::warn!("[node] EPG sync from {name} failed: {error}");
            }
        }
    }
}

pub async fn apply_reply(
    database: &DatabaseHandle,
    allowed_network_ids: &[u16],
    reply: &EpgProgramsReply,
) -> DatabaseResult<(usize, usize)> {
    let allowed = |nid: u16| allowed_network_ids.is_empty() || allowed_network_ids.contains(&nid);
    let rows: Vec<_> = reply
        .programs
        .iter()
        .filter(|row| allowed(row.nid))
        .map(|row| ProgramUpsertWithClocks {
            id: 0,
            program: row.clone().into(),
            pf_updated_at: row.pf_updated_at,
            schedule_updated_at: row.schedule_updated_at,
        })
        .collect();
    let mut db = database.lock().await;
    let count = db.upsert_programs_with_clocks(&rows)?;
    let mut coverage_count = 0;
    for coverage in &reply.coverage {
        if allowed(coverage.network_id) {
            db.merge_epg_remote_coverage(
                coverage.network_id,
                coverage.tsid,
                coverage.section_coverage_until,
                coverage.last_eit_received_at,
                coverage.last_complete_at,
            )?;
            coverage_count += 1;
        }
    }
    Ok((count, coverage_count))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{Database, EpgSource, ProgramUpsert};
    use crate::node::serve::ProgramUpsertWire;

    #[tokio::test]
    async fn apply_reply_filters_unknown_network_and_preserves_supplier_clock() {
        let database = Arc::new(tokio::sync::Mutex::new(Database::open_in_memory().unwrap()));
        {
            let mut db = database.lock().await;
            db.upsert_programs(&[ProgramUpsert {
                nid: 1,
                sid: 2,
                tsid: 3,
                event_id: 4,
                start_at: 10_000,
                duration_secs: 100,
                free_ca_mode: false,
                name: Some("new".into()),
                description: None,
                extended: None,
                genre: None,
                updated_at: 100,
                source: EpgSource::PresentFollowing,
                basic_updated_at: Some(100),
                extended_updated_at: None,
                shared_sid: None,
                shared_event_id: None,
            }])
            .unwrap();
            db.merge_epg_remote_coverage(1, 3, Some(1000), Some(1000), Some(1000))
                .unwrap();
        }
        let old = ProgramUpsertWire {
            nid: 1,
            sid: 2,
            tsid: 3,
            event_id: 4,
            start_at: 20_000,
            duration_secs: 100,
            free_ca_mode: false,
            name: Some("old".into()),
            description: None,
            extended: None,
            genre: None,
            updated_at: 50,
            source: 0,
            basic_updated_at: Some(50),
            extended_updated_at: None,
            shared_sid: None,
            shared_event_id: None,
            pf_updated_at: Some(50),
            schedule_updated_at: Some(50),
        };
        let unknown = ProgramUpsertWire {
            nid: 99,
            ..old.clone()
        };
        let reply = EpgProgramsReply {
            programs: vec![old, unknown],
            coverage: vec![super::super::transport::EpgProgramCoverageWire {
                network_id: 1,
                tsid: 3,
                section_coverage_until: Some(900),
                last_eit_received_at: Some(900),
                last_complete_at: Some(900),
            }],
            ..Default::default()
        };
        assert_eq!(apply_reply(&database, &[1], &reply).await.unwrap(), (1, 1));
        let db = database.lock().await;
        let row: (String, i64, i64, i64) = db.connection().query_row(
            "SELECT name,updated_at,pf_updated_at,schedule_updated_at FROM programs WHERE nid=1 AND sid=2 AND event_id=4",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();
        assert_eq!(row, ("new".into(), 100, 100, 50));
        let coverage: (i64, i64) = db.connection().query_row(
            "SELECT section_coverage_until,last_eit_received_at FROM epg_scan_states WHERE network_id=1 AND tsid=3",
            [], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!(coverage, (1000, 1000));
    }
}
