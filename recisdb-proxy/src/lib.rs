//! recisdb-proxy ライブラリ
//!
//! 各バイナリから共有されるモジュールを公開します。

/// このビルドのバージョン文字列。`build.rs` が以下の優先順で決定し、
/// `RECISDB_PROXY_VERSION` 環境変数経由で埋め込む(先頭の `v` は除去済み):
/// 1. 環境変数 `RECISDB_PROXY_VERSION`(CI がリリースタグから設定)
/// 2. `git describe --tags --always --dirty`(タグ間のdevビルドは
///    `0.0.1-alpha.6-1-g05a127c` のような形式になる)
/// 3. どちらも取れない場合は `CARGO_PKG_VERSION`(Cargo.tomlの固定値)
///
/// ダッシュボード表示・更新チェック(`web/api/statics.rs`,
/// `web/api/update.rs`)で使う。Mirakurun互換API(`web/mirakurun.rs`)は
/// EPGStation等がsemver形式を期待し得るため、あえて `CARGO_PKG_VERSION`
/// のまま(このモジュールとは独立)。
pub const VERSION: &str = env!("RECISDB_PROXY_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBonCasLinkValue {
    pub value: Option<String>,
    pub source: &'static str,
}

/// Resolve a DB override (empty means explicitly disabled) before its TOML fallback.
pub fn resolve_boncaslink_value(db: Option<String>, toml: Option<String>) -> ResolvedBonCasLinkValue {
    if let Some(value) = db {
        let value = (!value.trim().is_empty()).then_some(value);
        ResolvedBonCasLinkValue { value, source: "db" }
    } else if let Some(value) = toml {
        let value = (!value.trim().is_empty()).then_some(value);
        let source = if value.is_some() { "toml" } else { "none" };
        ResolvedBonCasLinkValue { value, source }
    } else {
        ResolvedBonCasLinkValue { value: None, source: "none" }
    }
}

pub mod alert;
pub mod aribb24;
pub mod bondriver;
pub mod config_file;
pub mod database;
pub mod encoder_probe;
pub mod epg_writer;
pub mod fourk_setup;
pub mod guide_services;
pub mod logging;
pub mod metrics;
pub mod nit_writer;
pub mod node;
pub mod preview_setup;
pub mod px4_installer;
pub mod scheduler;
pub mod server;
pub mod service;
pub mod setup_helpers;
pub mod ts_analyzer;
pub mod tuner;
pub mod web;

static B25_NETWORK_READERS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<u16, String>>,
> = std::sync::OnceLock::new();

/// Parse and install the resolved NID to reader table (DB setting, otherwise TOML fallback).
pub fn apply_network_card_reader_selections(values: &std::collections::HashMap<String, String>) {
    let mut readers = std::collections::HashMap::new();
    let mut entries = values.iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));
    for (raw_nid, raw_reader) in entries {
        let nid = match parse_b25_network_id(raw_nid) {
            Some(nid) => nid,
            None => {
                log::warn!("Ignoring invalid B25 network reader NID {:?}", raw_nid);
                continue;
            }
        };
        let reader = raw_reader.trim();
        if reader.is_empty() || reader.chars().count() >= 1024 || reader.as_bytes().contains(&0) {
            log::warn!("Ignoring invalid B25 reader for NID {:04X}", nid);
            continue;
        }
        let reader = if let Some(address) = reader.strip_prefix("boncaslink://") {
            match b25_sys::register_boncaslink_reader(address) {
                Ok(name) => name,
                Err(error) => {
                    log::warn!(
                        "Ignoring invalid BonCasLink reader for NID {:04X}: {}",
                        nid,
                        error
                    );
                    continue;
                }
            }
        } else {
            reader.to_owned()
        };
        if readers.insert(nid, reader).is_some() {
            log::warn!(
                "Duplicate B25 network reader NID {:04X}; lexically later key wins",
                nid
            );
        }
    }
    let mut installed = B25_NETWORK_READERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *installed = readers.clone();
    let mut rows = readers.into_iter().collect::<Vec<_>>();
    rows.sort_by_key(|(nid, _)| *nid);
    for (nid, name) in rows {
        log::info!("B25 network reader: NID={nid:04X} reader={name:?}");
    }
}

fn parse_b25_network_id(value: &str) -> Option<u16> {
    let value = value.trim();
    let digits = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .unwrap_or(value);
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    u16::from_str_radix(digits, 16).ok()
}

pub(crate) fn network_card_reader(nid: Option<u16>) -> Option<String> {
    let nid = nid?;
    B25_NETWORK_READERS
        .get()?
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&nid)
        .cloned()
}

#[cfg(test)]
mod b25_config_tests {
    use super::parse_b25_network_id;
    #[test]
    fn parses_network_reader_ids_as_hex() {
        assert_eq!(parse_b25_network_id("FFFE"), Some(0xfffe));
        assert_eq!(parse_b25_network_id("0x0004"), Some(4));
        assert_eq!(parse_b25_network_id("0XfFe"), Some(0x0ffe));
        assert_eq!(parse_b25_network_id("xyz"), None);
        assert_eq!(parse_b25_network_id("10000"), None);
    }
}

/// 選択された PC/SC カードリーダー名を libaribb25 に反映する。
///
/// 空文字列なら何もしない = libaribb25 の既定動作 (全リーダーを順に試す) の
/// まま。libaribb25 側はプロセス全体の状態なので、既に動いているデコーダには
/// 影響せず、**次にリーダーを起動したときから**効く。
pub fn apply_card_reader_selection(name: &str) {
    if name.is_empty() {
        return;
    }
    if b25_sys::set_card_reader_name(name) {
        log::info!("B-CAS card reader pinned to {:?}", name);
    } else {
        // 1024文字以上か内部にNULがある場合のみ。DBに入る経路では起きないが、
        // 黙って「自動」に落ちると原因が追えなくなるので必ず残す。
        log::warn!(
            "libaribb25 rejected the card reader name {:?}; falling back to probing every reader",
            name
        );
    }
}
