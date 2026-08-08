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

pub mod bondriver;
pub mod database;
pub mod logging;
pub mod alert;
pub mod epg_writer;
pub mod preview_setup;
pub mod px4_installer;
pub mod scheduler;
pub mod server;
pub mod service;
pub mod setup_helpers;
pub mod ts_analyzer;
pub mod tuner;
pub mod aribb24;
pub mod web;

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
        log::warn!("libaribb25 rejected the card reader name {:?}; falling back to probing every reader", name);
    }
}
