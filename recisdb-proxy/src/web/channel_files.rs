//! TVTest `.ch2` / EDCB `ChSet4.txt` / `ChSet5.txt` generation for the
//! dashboard's client-setup guide.
//!
//! The space/channel *indices* written into these files are the ones the
//! proxy enumerates to BonDriver clients (server/client_view.rs) — both
//! TVTest and EDCB pass them straight to `IBonDriver2::SetChannel(space,
//! ch)`, so they must match the enumeration order exactly. That is why this
//! module assembles its input via `client_view` instead of reading the
//! channels table directly.
//!
//! Formats verified against upstream source (2026-07):
//! - .ch2: TVTest `src/ChannelList.cpp` `CTuningSpaceList::SaveToFile` —
//!   9 comma-separated fields, `;#SPACE(index,name)` space comments,
//!   Shift_JIS (or UTF-16LE+BOM when not representable; TVTest does NOT
//!   read UTF-8), CRLF.
//! - ChSet4/ChSet5: xtne6f/EDCB `Common/ParseTextInstances.cpp`
//!   `CParseChText4/5` — exactly 12 / 9 tab-separated fields, UTF-8 with
//!   BOM, CRLF.

use std::collections::HashSet;

use crate::server::client_view::{self, ChannelRow, ServiceEntry};

/// The client BonDriver's DLL stem. `GetTunerName()` in
/// bondriver-proxy-client/src/bondriver/exports.rs returns the same string,
/// which fixes both the .ch2 and the ChSet4 filename.
const CLIENT_BONDRIVER_NAME: &str = "BonDriver_NetworkProxy";

/// `.ch2` goes next to the client DLL, named after it.
pub const TVTEST_CH2_FILENAME: &str = "BonDriver_NetworkProxy.ch2";
/// EDCB Setting folder: `<dll stem>(<GetTunerName()>).ChSet4.txt`.
pub const CHSET4_FILENAME: &str = "BonDriver_NetworkProxy(BonDriver_NetworkProxy).ChSet4.txt";
/// EDCB Setting folder, global service list.
pub const CHSET5_FILENAME: &str = "ChSet5.txt";

/// One tuning space as it will appear to the client, with full service
/// detail per enumerated channel.
#[derive(Debug, Clone)]
pub struct FileSpace {
    pub index: u32,
    pub name: String,
    pub channels: Vec<FileChannel>,
}

/// One enumerated channel (= transport stream) within a space.
#[derive(Debug, Clone)]
pub struct FileChannel {
    /// Client-facing channel index (second arg of SetChannel).
    pub index: u32,
    /// EnumChannelName display name (used as ChSet4 chName).
    pub name: String,
    /// Services on this TS, sorted by SID. Non-empty by construction:
    /// every enumerated channel comes from at least one service row.
    pub services: Vec<ServiceEntry>,
}

/// Assemble the client-facing space/channel/service tree for `rows`
/// restricted to `driver_matches` — the exact enumeration a client opening
/// that tuner will see.
pub fn assemble_spaces<F: Fn(&str) -> bool + Copy>(
    rows: &[ChannelRow],
    driver_matches: F,
) -> Vec<FileSpace> {
    let space_result = client_view::build_space_list(rows, driver_matches);
    let mut channels_by_region = client_view::build_channels_by_region(rows, driver_matches);
    let mut services_by_ts = client_view::build_services_by_ts(rows, driver_matches);

    space_result
        .spaces
        .iter()
        .enumerate()
        .map(|(space_index, space)| FileSpace {
            index: space_index as u32,
            name: space.display_name.clone(),
            channels: channels_by_region
                .remove(space.region_key.as_str())
                .unwrap_or_default()
                .into_iter()
                .enumerate()
                .map(|(channel_index, ch)| FileChannel {
                    index: channel_index as u32,
                    name: ch.name,
                    services: services_by_ts.remove(&(ch.nid, ch.tsid)).unwrap_or_default(),
                })
                .collect(),
        })
        .collect()
}

/// Replace characters that would corrupt a line-oriented format: newlines
/// always, plus tabs (EDCB itself converts tabs in names to spaces on
/// save; a tab in a .ch2 name is harmless but normalized the same way).
fn sanitize_name(s: &str) -> String {
    s.replace(['\t', '\r', '\n'], " ")
}

/// CSV-quote a .ch2 name per TVTest's writer: quote when it starts with
/// `#`/`;` or contains `,`/`"`, doubling inner quotes.
fn ch2_quote(name: &str) -> String {
    let name = sanitize_name(name);
    if name.starts_with('#') || name.starts_with(';') || name.contains(',') || name.contains('"') {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name
    }
}

/// Space names go into `;#SPACE(index,name)` comments, whose parser scans
/// for the closing `)` — ASCII parens in the name (e.g. "地デジ (関東)")
/// would truncate it, so swap them for fullwidth ones. Display-only:
/// TVTest addresses spaces by index, never by this name.
fn ch2_space_name(name: &str) -> String {
    sanitize_name(name).replace('(', "（").replace(')', "）")
}

/// TVTest .ch2: one line per service. Field order (ChannelList.cpp):
/// 名称,チューニング空間,チャンネル,リモコン番号,サービスタイプ,サービスID,ネットワークID,TSID,状態
pub fn generate_tvtest_ch2(spaces: &[FileSpace]) -> String {
    let mut out = String::new();
    out.push_str("; TVTest チャンネル設定ファイル\r\n");
    out.push_str(
        "; 名称,チューニング空間,チャンネル,リモコン番号,サービスタイプ,サービスID,ネットワークID,TSID,状態\r\n",
    );
    for space in spaces {
        out.push_str(&format!(";#SPACE({},{})\r\n", space.index, ch2_space_name(&space.name)));
        for ch in &space.channels {
            for svc in &ch.services {
                // TVTest writes 0-valued type/ids as empty fields.
                let service_type = svc
                    .service_type
                    .filter(|&t| t != 0)
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                out.push_str(&format!(
                    "{},{},{},{},{},{},{},{},1\r\n",
                    ch2_quote(&svc.name),
                    space.index,
                    ch.index,
                    svc.remote_control_key.unwrap_or(0),
                    service_type,
                    svc.sid,
                    svc.nid,
                    svc.tsid,
                ));
            }
        }
    }
    out
}

/// EDCB ChSet4.txt: exactly 12 tab-separated fields per line
/// (chName, serviceName, networkName, space, ch, ONID, TSID, SID,
/// serviceType, partialFlag, useViewFlag, remoconID).
pub fn generate_chset4(spaces: &[FileSpace]) -> String {
    let mut out = String::new();
    for space in spaces {
        for ch in &space.channels {
            for svc in &ch.services {
                let service_type = svc.service_type.unwrap_or(1);
                let partial = i32::from(service_type == 192);
                let network_name = svc
                    .ts_name
                    .clone()
                    .unwrap_or_else(|| space.name.clone());
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t1\t{}\r\n",
                    sanitize_name(&ch.name),
                    sanitize_name(&svc.name),
                    sanitize_name(&network_name),
                    space.index,
                    ch.index,
                    svc.nid,
                    svc.tsid,
                    svc.sid,
                    service_type,
                    partial,
                    svc.remote_control_key.unwrap_or(0),
                ));
            }
        }
    }
    out
}

/// EDCB ChSet5.txt: exactly 9 tab-separated fields per line
/// (serviceName, networkName, ONID, TSID, SID, serviceType, partialFlag,
/// epgCapFlag, searchFlag), one line per unique (ONID, TSID, SID).
/// epgCapFlag is set on the first non-partial service of each TS.
pub fn generate_chset5(spaces: &[FileSpace]) -> String {
    let mut out = String::new();
    let mut seen: HashSet<(u16, u16, u16)> = HashSet::new();
    for space in spaces {
        for ch in &space.channels {
            let mut epg_assigned = false;
            for svc in &ch.services {
                if !seen.insert((svc.nid, svc.tsid, svc.sid)) {
                    continue;
                }
                let service_type = svc.service_type.unwrap_or(1);
                let partial = service_type == 192;
                let epg_cap = if !partial && !epg_assigned {
                    epg_assigned = true;
                    1
                } else {
                    0
                };
                let network_name = svc
                    .ts_name
                    .clone()
                    .unwrap_or_else(|| space.name.clone());
                out.push_str(&format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\r\n",
                    sanitize_name(&svc.name),
                    sanitize_name(&network_name),
                    svc.nid,
                    svc.tsid,
                    svc.sid,
                    service_type,
                    i32::from(partial),
                    epg_cap,
                    i32::from(!partial),
                ));
            }
        }
    }
    out
}

/// Encode a .ch2 body the way TVTest expects: Shift_JIS when losslessly
/// representable, otherwise UTF-16LE with BOM (TVTest reads only those two;
/// UTF-8 would be misread).
pub fn encode_ch2(content: &str) -> Vec<u8> {
    let (encoded, _, had_errors) = encoding_rs::SHIFT_JIS.encode(content);
    if !had_errors {
        return encoded.into_owned();
    }
    let mut bytes = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for unit in content.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// Encode a ChSet4/ChSet5 body: UTF-8 with BOM (what modern EDCB forks
/// write and read).
pub fn encode_utf8_bom(content: &str) -> Vec<u8> {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(content.as_bytes());
    bytes
}

/// Name of the client DLL, exposed for callers building bundles/README.
pub fn client_bondriver_name() -> &'static str {
    CLIENT_BONDRIVER_NAME
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(
        nid: u16,
        tsid: u16,
        sid: u16,
        name: &str,
        service_type: Option<i32>,
        remocon: Option<i32>,
        ts_name: Option<&str>,
    ) -> ServiceEntry {
        ServiceEntry {
            nid,
            tsid,
            sid,
            name: name.to_string(),
            service_type,
            remote_control_key: remocon,
            ts_name: ts_name.map(str::to_string),
        }
    }

    fn sample_spaces() -> Vec<FileSpace> {
        vec![
            FileSpace {
                index: 0,
                name: "地デジ (関東)".to_string(),
                channels: vec![FileChannel {
                    index: 0,
                    name: "NHK総合".to_string(),
                    services: vec![
                        svc(32736, 32736, 1024, "NHK総合・東京", Some(1), Some(1), Some("関東広域")),
                        svc(32736, 32736, 1408, "NHK携帯G", Some(192), Some(1), Some("関東広域")),
                    ],
                }],
            },
            FileSpace {
                index: 1,
                name: "BS".to_string(),
                channels: vec![FileChannel {
                    index: 0,
                    name: "NHKBS".to_string(),
                    services: vec![svc(4, 16400, 101, "NHKBS", Some(1), None, None)],
                }],
            },
        ]
    }

    #[test]
    fn ch2_has_space_comments_and_one_line_per_service() {
        let ch2 = generate_tvtest_ch2(&sample_spaces());
        let lines: Vec<&str> = ch2.lines().collect();
        assert_eq!(lines[0], "; TVTest チャンネル設定ファイル");
        // ASCII parens in the space name are swapped for fullwidth so the
        // ;#SPACE(...) comment's closing paren stays unambiguous.
        assert_eq!(lines[2], ";#SPACE(0,地デジ （関東）)");
        assert_eq!(lines[3], "NHK総合・東京,0,0,1,1,1024,32736,32736,1");
        assert_eq!(lines[4], "NHK携帯G,0,0,1,192,1408,32736,32736,1");
        assert!(lines[5].starts_with(";#SPACE(1,BS)"));
        // BS: no remote-control key -> 0, ids present, enabled.
        assert_eq!(lines[6], "NHKBS,1,0,0,1,101,4,16400,1");
        assert!(ch2.ends_with("\r\n"));
    }

    #[test]
    fn ch2_quotes_names_with_commas_and_quotes() {
        assert_eq!(ch2_quote("A,B"), "\"A,B\"");
        assert_eq!(ch2_quote("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(ch2_quote(";lead"), "\";lead\"");
        assert_eq!(ch2_quote("plain"), "plain");
    }

    #[test]
    fn chset4_lines_have_exactly_11_tabs() {
        let chset4 = generate_chset4(&sample_spaces());
        for line in chset4.lines() {
            assert_eq!(line.matches('\t').count(), 11, "line: {line}");
        }
        let first = chset4.lines().next().unwrap();
        assert_eq!(
            first,
            "NHK総合\tNHK総合・東京\t関東広域\t0\t0\t32736\t32736\t1024\t1\t0\t1\t1"
        );
        // One-seg service: partialFlag=1, serviceType=192.
        let oneseg = chset4.lines().nth(1).unwrap();
        assert_eq!(
            oneseg,
            "NHK総合\tNHK携帯G\t関東広域\t0\t0\t32736\t32736\t1408\t192\t1\t1\t1"
        );
    }

    #[test]
    fn chset5_lines_have_exactly_8_tabs_and_flag_semantics() {
        let chset5 = generate_chset5(&sample_spaces());
        let lines: Vec<&str> = chset5.lines().collect();
        for line in &lines {
            assert_eq!(line.matches('\t').count(), 8, "line: {line}");
        }
        // Primary service: epgCap=1, search=1. One-seg: partial=1, epg=0, search=0.
        assert_eq!(lines[0], "NHK総合・東京\t関東広域\t32736\t32736\t1024\t1\t0\t1\t1");
        assert_eq!(lines[1], "NHK携帯G\t関東広域\t32736\t32736\t1408\t192\t1\t0\t0");
        assert_eq!(lines[2], "NHKBS\tBS\t4\t16400\t101\t1\t0\t1\t1");
    }

    #[test]
    fn ch2_encodes_to_shift_jis_when_representable() {
        let bytes = encode_ch2("NHK総合,0,0,1,1,1024,32736,32736,1\r\n");
        // No UTF-16 BOM, and the Japanese text round-trips through SJIS.
        assert_ne!(&bytes[..2], &[0xFF, 0xFE]);
        let (decoded, _, _) = encoding_rs::SHIFT_JIS.decode(&bytes);
        assert!(decoded.contains("NHK総合"));
    }

    #[test]
    fn ch2_falls_back_to_utf16le_for_unrepresentable_names() {
        // 𠮷 (U+20BB7) is outside Shift_JIS.
        let bytes = encode_ch2("𠮷野家テレビ,0,0,1,1,1,1,1,1\r\n");
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
    }

    #[test]
    fn chset_encoding_has_utf8_bom() {
        let bytes = encode_utf8_bom("あ\t1\r\n");
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF]);
        assert_eq!(&bytes[3..], "あ\t1\r\n".as_bytes());
    }

    #[test]
    fn assemble_spaces_joins_channels_and_services() {
        use crate::database::{BonDriverRecord, ClientChannelRecord};
        let driver = BonDriverRecord {
            id: 1,
            dll_path: "BonDriver_A.dll".to_string(),
            driver_name: None,
            version: None,
            group_name: None,
            auto_scan_enabled: true,
            scan_interval_hours: 24,
            scan_priority: 0,
            last_scan: None,
            next_scan_at: None,
            passive_scan_enabled: true,
            max_instances: 1,
            created_at: 0,
            updated_at: 0,
        };
        let row = |nid: i32, tsid: i32, sid: i32, name: &str, ch: u32| {
            (
                ClientChannelRecord {
                    id: 0,
                    bon_driver_id: 1,
                    nid,
                    sid,
                    tsid,
                    service_name: Some(name.to_string()),
                    ts_name: None,
                    service_type: Some(1),
                    remote_control_key: None,
                    space: 0,
                    channel: ch,
                    is_enabled: true,
                    priority: 0,
                },
                Some(driver.clone()),
            )
        };
        let rows = vec![
            row(0x7FE8, 0x7FE8, 1024, "NHK総合", 27),
            row(0x7FE8, 0x7FE8, 1025, "NHK総合サブ", 27),
            row(0x0004, 0x4010, 101, "BS朝日", 0),
        ];
        let spaces = assemble_spaces(&rows, |_| true);
        assert_eq!(spaces.len(), 2);
        assert_eq!(spaces[0].name, "地デジ (関東)");
        assert_eq!(spaces[0].channels.len(), 1);
        assert_eq!(spaces[0].channels[0].services.len(), 2);
        assert_eq!(spaces[1].channels[0].services[0].name, "BS朝日");
    }
}
