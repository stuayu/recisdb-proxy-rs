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
pub mod px4_installer;
pub mod scheduler;
pub mod server;
pub mod setup_helpers;
pub mod ts_analyzer;
pub mod tuner;
pub mod aribb24;
pub mod web;
