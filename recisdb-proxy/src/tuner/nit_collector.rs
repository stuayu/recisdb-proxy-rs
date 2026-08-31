//! Lightweight NIT collector for terrestrial channel metadata.
//!
//! Same shape as [`crate::tuner::epg_collector::EpgCollector`]: it listens to
//! raw TS chunks from the live tuner reader loop, reassembles NIT sections
//! (PID 0x0010) and forwards what the 地上分配システム記述子 / TS情報記述子
//! carry — remote-control key, physical UHF channel, network name — through a
//! process-wide channel drained by [`crate::nit_writer::NitWriter`].
//!
//! # Why this exists
//!
//! `channels` rows normally get these columns from a channel scan
//! (`scheduler/scan_scheduler.rs`), but rows created through the manual
//! routes — CSV import and `POST /api/channels`
//! (`web/api/channels.rs`) — insert `remote_control_key`, `physical_ch` and
//! `network_name` as `NULL`, and nothing filled them in afterwards. Such rows
//! are common for shared tuners reached through BonDriverProxyEx, where a
//! full scan is not run. A `NULL` remote-control key is not cosmetic:
//! EPGStation sorts its guide and now-on-air columns by
//! `remoteControlKeyId` and pushes the rows that lack one to the end
//! (`src/model/db/ChannelDB.ts:382`), so those stations appear in a jumbled
//! block after everything else.
//!
//! Collecting the NIT from the live stream fills them in without a scan: the
//! table repeats every few hundred milliseconds on every terrestrial
//! multiplex the proxy ever tunes for viewing or EPG collection.
//!
//! # Terrestrial only
//!
//! BS/CS carry no TS情報記述子 and their physical "channel" is the
//! transponder derived from the TSID, which the scan path already fills in
//! (`scan_scheduler::physical_ch_for` / `satellite_remote_control_key`).
//! Observations for satellite network ids are dropped here so this collector
//! can never disagree with that derivation.

use log::{debug, trace};
use tokio::sync::mpsc;

use std::collections::HashSet;
use std::sync::OnceLock;

use crate::ts_analyzer::{
    pid, table_id, NitTable, PsiSection, SectionCollector, TsPacket, TS_PACKET_SIZE,
};
use recisdb_protocol::BandType;

/// One terrestrial transport stream as described by a received NIT.
///
/// Every field other than the ids is optional: a NIT may carry the TS情報
/// 記述子 without the 地上分配システム記述子 (and vice versa), and the
/// network name only belongs to the network the table itself describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NitObservation {
    /// Original network id of the described transport stream.
    pub nid: u16,
    /// Transport stream id of the described transport stream.
    pub tsid: u16,
    /// Remote-control key id (TS情報記述子 0xCD).
    pub remote_control_key: Option<u8>,
    /// Physical UHF channel (地上分配システム記述子 0xFA, first frequency).
    pub physical_ch: Option<u8>,
    /// Network name (ネットワーク名記述子 0x40) — only set when the entry
    /// belongs to the network this NIT describes.
    pub network_name: Option<String>,
}

impl NitObservation {
    /// Whether this observation carries anything worth storing.
    fn is_useful(&self) -> bool {
        self.remote_control_key.is_some()
            || self.physical_ch.is_some()
            || self.network_name.is_some()
    }
}

/// Process-wide sender for parsed NIT rows. See module doc comment.
static NIT_SENDER: OnceLock<mpsc::UnboundedSender<NitObservation>> = OnceLock::new();

/// Install the process-wide NIT sender. Called once from
/// `crate::nit_writer::NitWriter::new`. Returns `false` (leaving the
/// previously-installed sender in place) if a sender was already set.
pub fn set_global_sender(tx: mpsc::UnboundedSender<NitObservation>) -> bool {
    NIT_SENDER.set(tx).is_ok()
}

fn global_sender() -> Option<&'static mpsc::UnboundedSender<NitObservation>> {
    NIT_SENDER.get()
}

const NIT_PID: u16 = pid::NIT;

/// Collects NIT sections from a live TS stream and forwards what they say
/// about terrestrial transport streams.
pub struct NitCollector {
    collector: SectionCollector,
    /// `(nid, tsid)` already forwarded by this collector instance. The NIT
    /// repeats constantly and a reader task can live for hours, so without
    /// this the writer would be handed the same rows thousands of times.
    /// A fresh collector is built per reader-task start, so a channel switch
    /// naturally re-reports (cheap: the writer skips rows already stored).
    seen: HashSet<(u16, u16)>,
}

impl NitCollector {
    pub fn new() -> Self {
        Self {
            collector: SectionCollector::new(),
            seen: HashSet::new(),
        }
    }

    /// Feed a raw chunk of TS packets (as read from the tuner). Best-effort:
    /// malformed packets/sections are silently skipped, same convention as
    /// [`crate::tuner::epg_collector::EpgCollector::process_ts_chunk`].
    pub fn process_ts_chunk(&mut self, data: &[u8]) {
        let mut offset = 0usize;
        while offset + TS_PACKET_SIZE <= data.len() {
            if data[offset] != 0x47 {
                offset += 1;
                continue;
            }

            if let Ok(packet) = TsPacket::parse(&data[offset..offset + TS_PACKET_SIZE]) {
                self.process_packet(&packet);
            }

            offset += TS_PACKET_SIZE;
        }
    }

    fn process_packet(&mut self, packet: &TsPacket<'_>) {
        if packet.header.pid != NIT_PID {
            return;
        }
        if packet.header.transport_error
            || packet.header.is_scrambled()
            || !packet.header.has_payload()
        {
            return;
        }

        let sections = self.collector.add_data(
            packet.payload,
            packet.header.continuity_counter,
            packet.header.payload_unit_start,
        );
        for section_data in &sections {
            self.process_section(section_data);
        }
    }

    fn process_section(&mut self, section_data: &[u8]) {
        let Ok(section) = PsiSection::parse(section_data) else {
            return;
        };
        if section.header.table_id != table_id::NIT_ACTUAL
            && section.header.table_id != table_id::NIT_OTHER
        {
            return;
        }
        let Ok(nit) = NitTable::parse(&section) else {
            return;
        };

        for observation in observations_from_nit(&nit) {
            let key = (observation.nid, observation.tsid);
            if !self.seen.insert(key) {
                continue;
            }

            let Some(tx) = global_sender() else {
                trace!(
                    "[NitCollector] no writer installed, dropping nid={} tsid={}",
                    observation.nid,
                    observation.tsid
                );
                return;
            };
            if tx.send(observation).is_err() {
                debug!(
                    "[NitCollector] writer task gone, dropping remaining entries for this section"
                );
                return;
            }
        }
    }
}

impl Default for NitCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the useful terrestrial entries of a parsed NIT.
///
/// NIT other (0x41) is accepted alongside NIT actual (0x40): on terrestrial
/// networks it describes neighbouring stations with their own
/// `original_network_id`, which is exactly the metadata missing from rows
/// registered by hand. The network name is attached only to entries of the
/// network the table itself describes — for the other-network entries it
/// would name the wrong network.
fn observations_from_nit(nit: &NitTable) -> Vec<NitObservation> {
    nit.transport_streams
        .iter()
        .filter(|ts| {
            // CATV も含めるのは Mirakurun 互換 API がリモコンキーを返す帯と
            // 揃えるため (`web/mirakurun.rs::remote_control_key_id`)。CATV の
            // 周波数は UHF 帯の外なので `physical_ch()` は自然と None になる。
            matches!(
                BandType::from_nid(ts.original_network_id),
                BandType::Terrestrial | BandType::CATV
            )
        })
        .map(|ts| NitObservation {
            nid: ts.original_network_id,
            tsid: ts.transport_stream_id,
            remote_control_key: ts.remote_control_key,
            physical_ch: ts.physical_ch(),
            network_name: if ts.original_network_id == nit.network_id {
                nit.network_name.clone()
            } else {
                None
            },
        })
        .filter(NitObservation::is_useful)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ts_analyzer::{NitTransportStream, TerrestrialDeliveryDescriptor};

    fn ts(nid: u16, tsid: u16, key: Option<u8>, freq: Option<u32>) -> NitTransportStream {
        NitTransportStream {
            transport_stream_id: tsid,
            original_network_id: nid,
            descriptors: vec![],
            terrestrial_delivery: freq.map(|f| TerrestrialDeliveryDescriptor {
                frequencies: vec![f],
                ..Default::default()
            }),
            remote_control_key: key,
        }
    }

    fn nit(network_id: u16, streams: Vec<NitTransportStream>) -> NitTable {
        NitTable {
            network_id,
            version_number: 0,
            network_name: Some("Ｎｅｔ００１".to_string()),
            network_descriptors: vec![],
            transport_streams: streams,
        }
    }

    #[test]
    fn extracts_remote_control_key_and_physical_channel() {
        let table = nit(0x7FE0, vec![ts(0x7FE0, 0x7FE0, Some(1), Some(557_142_857))]);
        let got = observations_from_nit(&table);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].nid, 0x7FE0);
        assert_eq!(got[0].remote_control_key, Some(1));
        assert_eq!(got[0].physical_ch, Some(27));
        assert_eq!(got[0].network_name, Some("Ｎｅｔ００１".to_string()));
    }

    #[test]
    fn network_name_is_only_attached_to_its_own_network() {
        // NIT other 由来の他ネットワークのエントリに、このテーブルの
        // ネットワーク名を付けてはいけない (別局の名前になる)。
        let table = nit(0x7FE0, vec![ts(0x7FE1, 0x7FE1, Some(4), None)]);
        let got = observations_from_nit(&table);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].remote_control_key, Some(4));
        assert_eq!(got[0].network_name, None);
    }

    #[test]
    fn satellite_entries_are_dropped() {
        // BS (NID 4) / CS (NID 7) はスキャン側が TSID から導出するので触らない。
        let table = nit(
            0x0004,
            vec![
                ts(0x0004, 0x4031, Some(1), None),
                ts(0x0007, 0x6020, None, None),
            ],
        );
        assert!(observations_from_nit(&table).is_empty());
    }

    #[test]
    fn entries_without_any_metadata_are_dropped() {
        let table = nit(0x7FE0, vec![ts(0x7FE1, 0x7FE1, None, None)]);
        assert!(observations_from_nit(&table).is_empty());
    }

    #[test]
    fn test_process_ts_chunk_no_panic_on_garbage() {
        let mut collector = NitCollector::new();
        collector.process_ts_chunk(&[0u8; 100]);
    }

    #[test]
    fn test_process_ts_chunk_ignores_short_input() {
        let mut collector = NitCollector::new();
        collector.process_ts_chunk(&[0x47, 0x00, 0x00]);
    }

    #[test]
    fn test_set_global_sender_reports_whether_it_won() {
        // `EpgCollector` の同名テストと同じ理由で、静的変数の最終状態には
        // 依存しない (テストバイナリ全体で共有されるため)。
        let (tx, _rx) = mpsc::unbounded_channel::<NitObservation>();
        let _ = set_global_sender(tx);
    }
}
