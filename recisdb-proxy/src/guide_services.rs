//! Pure rules shared by the program-services API and the guide UI contract.

use recisdb_protocol::broadcast_region::classify_nid;
use recisdb_protocol::types::BroadcastType;

pub const MEDIA_SERVICE_TYPES: &[u8] = &[1, 2, 0xA1, 0xA2, 0xA5, 0xA6, 0xAD];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParentGroup {
    Terrestrial(u16),
    Bs(u16, u16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceMeta {
    pub nid: u16,
    pub sid: u16,
    pub service_type: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramNameAt {
    pub nid: u16,
    pub sid: u16,
    pub start_at: i64,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubchannelInfo {
    pub service: (u16, u16),
    pub parent: (u16, u16),
    pub distinct: bool,
}

pub fn is_media_service(service_type: Option<u8>) -> bool {
    service_type.is_none() || service_type.is_some_and(|value| MEDIA_SERVICE_TYPES.contains(&value))
}

pub fn parent_group(nid: u16, sid: u16) -> Option<ParentGroup> {
    match classify_nid(nid).0 {
        BroadcastType::Terrestrial => Some(ParentGroup::Terrestrial(nid)),
        BroadcastType::BS => bs_service_group(sid).map(|group| ParentGroup::Bs(nid, group)),
        _ => None,
    }
}

fn bs_service_group(sid: u16) -> Option<u16> {
    match sid {
        101..=189 => Some(sid / 10),
        231..=233 => Some(23),
        _ => None,
    }
}

pub fn channel_number(nid: u16, sid: u16, remote_control_key: Option<u16>, video_sids: &[u16]) -> u16 {
    if matches!(classify_nid(nid).0, BroadcastType::Terrestrial) {
        if let Some(rck @ 1..=12) = remote_control_key {
            let branch = video_sids.iter().filter(|candidate| **candidate <= sid).count().clamp(1, 8) as u16;
            return rck * 10 + branch;
        }
        return sid % 1000;
    }
    sid
}

pub fn is_placeholder_program_name(name: Option<&str>) -> bool {
    let Some(name) = name.map(str::trim).filter(|name| !name.is_empty()) else {
        return true;
    };
    ["ご覧ください", "番組未定", "放送休止", "休止中"]
        .iter()
        .any(|marker| name.contains(marker))
}

pub fn subchannels(services: &[ServiceMeta], programs: &[ProgramNameAt]) -> Vec<SubchannelInfo> {
    use std::collections::{HashMap, HashSet};

    // channels は (BonDriver, サービス) ごとに行があるので (nid, sid) で畳む。
    // SID 0 の仮行は親に選ばれると本物のメインがサブ扱いになるので入れない。
    let mut groups = HashMap::<ParentGroup, Vec<(u16, u16)>>::new();
    let mut seen = HashSet::<(u16, u16)>::new();
    for service in services {
        if service.sid == 0 || !is_media_service(service.service_type) || !seen.insert((service.nid, service.sid)) {
            continue;
        }
        if let Some(group) = parent_group(service.nid, service.sid) {
            groups.entry(group).or_default().push((service.nid, service.sid));
        }
    }
    // 本番の 1 日窓は 2 万件前後。サブの番組ごとに全件を舐めると数億回比較になるので、
    // 「(サービス, 開始時刻, 番組名)」を索引にして 1 回で引く。
    let index: HashSet<((u16, u16), i64, &str)> = programs
        .iter()
        .filter_map(|p| p.name.as_deref().map(|name| ((p.nid, p.sid), p.start_at, name.trim())))
        .collect();
    let mut own_program = HashSet::<(u16, u16)>::new();
    let parent_of: HashMap<(u16, u16), (u16, u16)> = groups
        .values()
        .filter_map(|members| members.iter().min_by_key(|(_, sid)| *sid).map(|parent| (members, *parent)))
        .flat_map(|(members, parent)| members.iter().filter(move |m| **m != parent).map(move |m| (*m, parent)))
        .collect();
    for program in programs {
        let service = (program.nid, program.sid);
        let Some(parent) = parent_of.get(&service) else { continue };
        if own_program.contains(&service) || is_placeholder_program_name(program.name.as_deref()) {
            continue;
        }
        let name = program.name.as_deref().map(str::trim).unwrap_or_default();
        if !index.contains(&(*parent, program.start_at, name)) {
            own_program.insert(service);
        }
    }
    let mut result: Vec<SubchannelInfo> = parent_of
        .iter()
        .map(|(service, parent)| SubchannelInfo {
            service: *service,
            parent: *parent,
            distinct: own_program.contains(service),
        })
        .collect();
    result.sort_by_key(|item| item.service);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_names_include_real_db_examples() {
        assert!(is_placeholder_program_name(None));
        assert!(is_placeholder_program_name(Some("   ")));
        assert!(is_placeholder_program_name(Some("この時間は０４１ｃｈをご覧ください。")));
        assert!(is_placeholder_program_name(Some("番組未定")));
        assert!(!is_placeholder_program_name(Some("ＭＸショッピング")));
        assert!(!is_placeholder_program_name(Some("ＬＩＶＥカメラ")));
    }

    #[test]
    fn subchannel_is_distinct_only_for_a_real_non_duplicate_program() {
        let services = vec![
            ServiceMeta { nid: 0x7FE8, sid: 1024, service_type: Some(1) },
            ServiceMeta { nid: 0x7FE8, sid: 1025, service_type: Some(1) },
        ];
        let programs = vec![
            ProgramNameAt { nid: 0x7FE8, sid: 1024, start_at: 10, name: Some("共通番組".into()) },
            ProgramNameAt { nid: 0x7FE8, sid: 1025, start_at: 10, name: Some("共通番組".into()) },
            ProgramNameAt { nid: 0x7FE8, sid: 1025, start_at: 20, name: Some("ＭＸショッピング".into()) },
        ];
        assert_eq!(subchannels(&services, &programs)[0].distinct, true);
    }

    #[test]
    fn placeholder_rows_and_duplicate_rows_do_not_change_the_parent() {
        let services = vec![
            ServiceMeta { nid: 0x7FE8, sid: 0, service_type: None },
            ServiceMeta { nid: 0x7FE8, sid: 1024, service_type: Some(1) },
            ServiceMeta { nid: 0x7FE8, sid: 1024, service_type: None },
            ServiceMeta { nid: 0x7FE8, sid: 1025, service_type: Some(1) },
        ];
        let programs = vec![
            ProgramNameAt { nid: 0x7FE8, sid: 1024, start_at: 10, name: Some("共通番組".into()) },
            ProgramNameAt { nid: 0x7FE8, sid: 1025, start_at: 10, name: Some(" 共通番組 ".into()) },
            ProgramNameAt { nid: 0x7FE8, sid: 1025, start_at: 20, name: None },
        ];
        assert_eq!(
            subchannels(&services, &programs),
            vec![SubchannelInfo { service: (0x7FE8, 1025), parent: (0x7FE8, 1024), distinct: false }]
        );
    }

    #[test]
    fn channel_number_follows_edcb_branch_rule() {
        assert_eq!(channel_number(0x7FE8, 1025, Some(1), &[1024, 1025]), 12);
        assert_eq!(channel_number(4, 151, Some(5), &[151]), 151);
    }
}
