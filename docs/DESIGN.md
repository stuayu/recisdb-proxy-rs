# recisdb-proxy 設計マスタ

**このファイルがプロジェクト設計の正本(Single Source of Truth)。**
実装と食い違いを見つけたら、実装を直すかこのファイルを直すこと。
時点レポート(進捗・修正記録)は [archive/](archive/) にあり、正本ではない。

- 現状の課題と改善計画: [REVIEW_2026-07.md](REVIEW_2026-07.md)
- 利用者向けガイド: [QUICKSTART.md](QUICKSTART.md) / [WEB_DASHBOARD.md](WEB_DASHBOARD.md) / [LOGGING.md](LOGGING.md)
- CLI (recisdb-rs) の内部設計: [ARCHITECTURE.md](ARCHITECTURE.md)

最終更新: 2026-07-02

---

## 1. 目的と全体像

recisdb-proxy は、Windows/Linux 上の TV チューナー (BonDriver / キャラクタデバイス) をネットワーク越しに
複数クライアント (TVTest 等) へ共有するプロキシサーバー。同一チャンネルの視聴を 1 本のチューナーに合流させ、
優先度・排他制御で録画を保護し、チャンネル情報を SQLite に自動スキャン・蓄積する。

```
┌──────────────┐   BNDP/TCP    ┌──────────────────────────── recisdb-proxy ─┐
│ TVTest        │◀────────────▶│ Listener ─ Session ─┬─ TunerPool           │
│ +BonDriver_   │  (40070)     │   (per-client task)  │   └ SharedTuner ──▶ BonDriver DLL /
│  NetworkProxy │              │                      │      (broadcast)     キャラクタデバイス
└──────────────┘              │                      ├─ Database (SQLite)  │
┌──────────────┐   HTTP       │ Web (axum) ──────────┤─ ScanScheduler      │
│ ブラウザ       │◀────────────▶│   (40080)            ├─ AlertManager       │
└──────────────┘              │                      └─ PassiveScanner     │
                              └──────────────────────────────────────────────┘
```

### 設計原則

1. **配信経路を最優先** — TS データパスに同期 I/O・ロック競合・コピーを持ち込まない。
   制御メッセージと TS データは書き込みチャネルを分離し、TS は満杯時に最古を捨てる (遅延より欠落を選ぶ)。
2. **チャンネル切替は BonDriverProxy(Ex) 互換の応答性** — SetChannel を DLL が受理したら即 ACK。
   シグナル待ちで ACK を遅らせない (連続切替で ReadTimeout を誘発するため)。
3. **状態の正はサーバー** — クライアント DLL は薄く、チャンネル定義・優先度既定・容量制限はサーバー側 DB が持つ。
4. **設定は 3 層** — CLI 引数 > TOML ファイル > DB (実行時変更可能なチューニング値)。

---

## 2. ワークスペース構成

| クレート | 役割 | 実行形態 |
|---|---|---|
| `recisdb-proxy` | プロキシサーバー本体 + `recisdb-proxy-setup` (対話式初期設定) | バイナリ ×2 |
| `bondriver-proxy-client` | クライアント DLL (`BonDriver_NetworkProxy.dll`) | cdylib (Windows) |
| `recisdb-protocol` | ワイヤプロトコル定義 (型・コーデック・NID 分類) | lib (no tokio 依存) |
| `recisdb-rs` | CLI チューナーツール (recpt1 代替)。プロキシからは独立 | バイナリ |
| `b25-sys` | ARIB STD-B25 デコード FFI。プロキシ・CLI 双方が使用 | lib |

依存方向: `recisdb-proxy` → `recisdb-protocol`, `b25-sys` / `bondriver-proxy-client` → `recisdb-protocol`。
`recisdb-proxy` は `recisdb-rs` に依存しない (チューナーアクセスは `src/bondriver/{windows,unix}.rs` に自前実装)。

### 非同期方針

- サーバー: tokio マルチスレッドランタイム。BonDriver DLL は `Send` 非対応のため
  **`spawn_blocking` 内で開いて読み取りループごと閉じ込める** (SharedTuner)。
- クライアント DLL: TVTest の同期呼び出しに応えるため、内部に小さな tokio ランタイムを持ち、
  FFI 境界は `std::sync` (parking_lot / Condvar / mpsc) で同期化する。DLL 内で async を表に出さない。

---

## 3. プロトコル (BNDP)

定義: `recisdb-protocol/src/{types,codec}.rs`

### フレーム

```
+--------+----------+---------+------------------+
| Magic  | Length   | Type    | Payload          |
| "BNDP" | u32 LE   | u16 LE  | (可変)           |
+--------+----------+---------+------------------+
  4B       4B         2B        Length バイト
```

- ヘッダ 10 バイト、最大フレーム 16MB (`MAX_FRAME_SIZE`)。超過・Magic 不一致はプロトコルエラー。
- 数値はすべて LE。文字列は `u16 LE 長 + UTF-8`。

### メッセージ (ClientMessage → ServerMessage)

| 系 | Client → Server | Server → Client | 備考 |
|---|---|---|---|
| ハンドシェイク | `Hello{version}` / `Ping` | `HelloAck` / `Pong` | **認証なし (既知の課題 → REVIEW S3)** |
| チューナー | `OpenTuner{path}` / `OpenTunerWithGroup{group}` / `CloseTuner` | `OpenTunerAck{bondriver_version}` 他 | グループ指定時はサーバーがドライバー自動選択 |
| 選局 | `SetChannel{ch,priority,exclusive}` (v1) / `SetChannelSpace{space,ch,priority,exclusive}` (v2) / `SetChannelSpaceInGroup{...}` / `SelectLogicalChannel{nid,tsid,sid?}` | 各 Ack | priority=0 は DB 既定を使用。exclusive は i32::MAX 扱い |
| 列挙 | `EnumTuningSpace` / `EnumChannelName` / `GetChannelList{filter}` | 各 Ack | 列挙は DB の仮想空間 (SpaceGenerator) を返す |
| ストリーム | `StartStream` / `StopStream` / `PurgeStream` / `SetServiceFilter{single_service}` | `TsData{data}` ほか | TsData のみ高頻度。サービスフィルタで単一 SID 配信 |
| その他 | `GetSignalLevel` / `SetLnbPower` | 各 Ack / `Error{code,msg}` | |

**互換性ポリシー**: `MessageType` の値は追加のみ・変更禁止。ペイロード拡張は新バリアント追加で行い、
`Hello.version` でネゴシエーションする (現在 v1)。

---

## 4. サーバー設計 (recisdb-proxy)

### 4.1 モジュールマップ

```
src/
├ main.rs            起動・設定マージ・各サブシステム spawn
├ server/
│  ├ listener.rs     accept ループ、セッション毎に read/write 分離、writer タスク
│  └ session.rs      ステートマシン・選局/容量制御・tsreplace パイプ (※分割予定: REVIEW §1-4)
├ tuner/
│  ├ pool.rs         TunerPool: ChannelKey → SharedTuner、keep-alive/idle-close
│  ├ shared.rs       SharedTuner: spawn_blocking 読み取りループ + broadcast 配信
│  ├ selector.rs     スコアベースのチューナー候補選択 (信号/負荷/優先度の重み付け)
│  ├ group_space.rs  グループ統合ビュー (GroupSpaceInfo)
│  ├ space_generator.rs 仮想チューナー空間の自動生成
│  ├ lock.rs / channel_key.rs / passive_scanner.rs / logo_collector.rs / ts_parser.rs
├ bondriver/         windows.rs (DLL FFI) / unix.rs (キャラクタデバイス)
├ scheduler/scan_scheduler.rs  定期チャンネルスキャン
├ ts_analyzer/       PAT/PMT/SDT/NIT 解析、service_filter
├ database/          rusqlite ラッパー (schema.rs が正)
├ web/               axum: api.rs / dashboard.rs (インライン HTML) / state.rs (SessionRegistry)
├ alert.rs           しきい値監視 + Webhook 通知 (feature "webhook")
├ logging.rs         tracing-subscriber: コンソール + 日次ローテーションファイル
└ metrics.rs         【デッドコード — 削除予定。実メトリクスは web/state.rs】
```

### 4.2 セッションのステートマシン

```
Initial ─Hello/HelloAck→ Ready ─OpenTuner→ TunerOpen ─StartStream→ Streaming
                            ▲                  │  ▲                    │
                            └──CloseTuner──────┘  └────StopStream──────┘
                切断/エラー → Closing (unsubscribe → idle-close スケジュール)
```

- セッションは **単一 async タスク**。`SetChannelSpace` 等の処理中は他メッセージを処理しない (直列)。
  そのため選局経路のブロッキング時間が短いことがプロトコル全体の応答性を決める。
- ソケット書き込みは専用 writer タスクに分離。チャネルは 2 本:
  - 制御: bounded + `send().await` (低頻度・損失不可)
  - TS: bounded + `try_send` (満杯時は最古を破棄。TCP 輻輳でセッションループを止めない)

### 4.3 チューナー共有 (TunerPool / SharedTuner)

- `ChannelKey = (tuner_path, ChannelKeySpec)`。同一チャンネル要求は既存 SharedTuner に**合流** (subscriber += 1)。
- SharedTuner は `tokio::sync::broadcast` で TS チャンクを全購読者へ配布。
  読み取りループは `spawn_blocking` 内 (BonDriver が Send 非対応のため)。
- **keep-alive**: subscriber が 0 になっても `keep_alive_secs` (既定 60s、DB `tuner_config` で変更可) は
  リーダーを維持し、ザッピング戻りを即応させる。`cancel_idle_close()` で復帰。
- **prewarm**: OpenTuner 時に DLL 未使用ならウォームチューナーを起動して初回選局を高速化。
- **選局シーケンス (Round 3 確定仕様)**: `SetChannel → Purge → 短い sleep → ACK 送信 → (シグナルはログのみ)`。
  シグナルロック待ちで ACK を遅らせてはならない。停止応答は `wait_ts_stream(100ms)`、stop_reader タイムアウト 1s。

### 4.4 優先度・排他・容量制御

- 優先度の決定順: `exclusive=true → i32::MAX` > `クライアント指定 (>0)` > `channels.priority (DB)` > `0`。
- 目安: 録画(排他)=255 / 録画=200 / 視聴=10 / スキャン=0。
  **サーバー側でクライアント申告値を制限する仕組みは未実装 (REVIEW S3)。**
- 容量: `bon_drivers.max_instances` が DLL 毎の同時チャンネル数上限。
  超過時は「要求チャンネルが既に稼働中なら合流を優先し、退避しない」チェックの後、
  優先度最低 (idle 優先) のチューナーを退避する。実装は session.rs 内 (pool への移設が課題)。
- 排他 (`exclusive=true`): 同一 DLL 上の他チューナーを停止して独占。要求チャンネル稼働中なら合流にフォールバック。

### 4.5 グループ選局と仮想チューナー空間

- `bon_drivers.group_name` (DLL 名から自動推測、例: BonDriver_MLT1.dll → "PX-MLT") で複数 DLL を束ねる。
- `SpaceGenerator` がチャンネル DB から仮想空間を生成:
  地上波(都道府県毎に分割) → BS → CS → 4K → その他 の順で空間を割当て、存在しない帯域は詰める。
  帯域は `BandType::from_nid()`、都道府県は `get_prefecture_name(nid)` (TVTest 互換) で判定。
- `GroupSpaceInfo` がグループ内全ドライバーの空間を統合し、`(仮想space, ch)` → 配信可能ドライバー群を解決。
  選択は selector のスコア (信号強度・購読数・優先度・空き) 順。グループ内でチャンネル可用性が
  ドライバー毎に異なることは仕様として許容 (ベストエフォート)。

### 4.6 チャンネルスキャン

- **ScanScheduler**: `bon_drivers.next_scan_at` を定期チェック (既定 60s) し、期限が来たドライバーを
  スキャン優先度順に実行 (同時実行数上限あり、既定 1)。チューナー使用中は譲る (scan priority=0)。
- **ts_analyzer**: PAT→SDT→NIT を解析し NID/SID/TSID・サービス名・物理 ch・リモコンキーを取得、
  `channels` に upsert。ARIB 文字は `aribb24` ラッパーでデコード (Linux の文字化け対応済み)。
- **PassiveScanner**: 配信中の TS から同情報を抽出して DB を裏で更新 (`passive_scan_enabled`)。
- **logo_collector**: 配信中 TS からロゴ (CDT) を収集し Web `/logos/:file` で配信。

### 4.7 Web ダッシュボード / API

- axum。`/` にインライン HTML ダッシュボード (5 秒ポーリング)、`/api/*` に JSON API。
- 主なリソース: tuners / bondrivers (CRUD+scan+品質) / channels (CRUD+import/export+batch) /
  clients (品質・履歴・切断・制御) / session-history / alert-rules / scan-config / tuner-config / tsreplace-config。
- **認証 (実装済み・2026-07-04, REVIEW S2)**: `/api/*` は `Authorization: Bearer <token>` 必須
  (`web/auth.rs::require_auth`、`axum::middleware::from_fn_with_state` で `/api` 配下のみに適用)。
  `GET /` (ダッシュボード HTML 本体) と `/logos/:file` は無認証のまま (トークン入力 UI を表示するため)。
  トークンは起動時に TOML `[web] auth_token` > DB (`web_auth_config` テーブル、単一行) > 新規生成 の順で解決し、
  新規生成時のみ起動ログに一度だけ表示する。`[web] auth_enabled = false` で無効化可能 (無効時は起動時に WARN)。
  ブラウザ側は初回アクセス時に `prompt()` でトークン入力 → `localStorage` に保存 → `window.fetch` を
  ラップして全 `/api/*` 呼び出しに自動付与 (`dashboard.rs`)。401 応答時は再入力を促す。
- **CORS**: `CorsLayer::permissive()` は廃止し、CORS レイヤー自体を外した (ダッシュボードは同一オリジン配信のため
  ブラウザの同一オリジンポリシーで十分。他オリジンからの `fetch` はブラウザ側で拒否される)。
- **既定 bind**: `web_listen` の既定は `127.0.0.1:40080` (REVIEW P0)。LAN 公開は `--web-listen 0.0.0.0:40080` などで明示オプトイン。
- `SessionRegistry` (web/state.rs) がセッションのライブメトリクス
  (signal/drop/scramble/bitrate、5 分の履歴リングバッファ) を保持。セッション終了時に `session_history` へ永続化。

### 4.8 tsreplace (外部エンコーダ) パイプ

- `tsreplace_config` の設定でセッションの TS を外部コマンド (既定: tsreplace + QSVEncC) に通してから配信できる。
- stdin/stdout パイプ、stderr はログへ。エラー時 passthrough 可 (`passthrough_on_error`)。
- **`command_path` は API/DB から変更不可 (実装済み・2026-07-04, REVIEW S1)**: `command_path` は
  `Command::new(command_path)` で直接実行されるため信頼境界そのもの。`POST /api/tsreplace-config`
  (`UpdateTsreplaceConfigRequest`) にはフィールド自体が存在せず、送っても無視され既存値が維持される。
  変更できるのは起動時の TOML `[tsreplace] command_path` のみ (`Database::set_tsreplace_command_path`、
  main.rs から一度だけ呼ばれる)。`arguments`/`enabled`/`read_timeout_ms`/`passthrough_on_error`/
  `max_concurrent_encoders` は従来どおり API から変更可能。ダッシュボードのフォームも実行コマンド欄を
  読み取り専用表示に変更済み。

### 4.9 アラート

- `alert_rules` (metric: drop_rate/scramble_rate/error_rate/signal_level/bitrate、条件 gt/lt/gte/lte) を
  5 秒周期で全セッションに対して評価。発火/解消を `alert_history` に記録、`webhook_url` へ通知
  (feature `webhook`、generic/Discord 等 format 指定)。

---

## 5. クライアント DLL 設計 (bondriver-proxy-client)

```
src/
├ lib.rs              CreateBonDriver (catch_unwind 済み)
├ bondriver/
│  ├ interface.rs     IBonDriver/2/3 vtable (RTTI 対応)
│  └ exports.rs       各エクスポートの実装
├ client/
│  ├ connection.rs    TCP 接続・RPC (std::sync::mpsc + recv_timeout)・再接続
│  └ buffer.rs        TsRingBuffer (Condvar 通知、ポーリングなし)
└ config.rs           INI (BonDriver_NetworkProxy.ini) + 環境変数
```

- **同期化の要**: TVTest からの呼び出しはすべて同期。RPC は `send_request_with_timeout()` で
  必ずタイムアウトを持つ (UI フリーズ防止)。`WaitTsStream` は `buffer.wait_data()` (Condvar)。
- **ファストパス**: `TsData` はメッセージデコードを迂回してリングバッファへ直行。読みバッファ 256KB。
- **キャッシュ**: `GetSignalLevel` は 2 秒 TTL。space/channel 名は上限付きキャッシュ
  (MAX_SPACES=256, MAX_CHANNELS_PER_SPACE=1024 — 不正値による OOM 防止)。
- **防御**: `GetTsStream` は null/サイズ 0/上限超過を検証。パニックは FFI 境界で捕捉
  (現状 CreateBonDriver のみ → 全境界へ拡大予定)。
- INI 主要キー: `Address`, `Priority`, `Exclusive`, `GroupName` (グループ選局), TLS 設定 (feature 時)。

---

## 6. データベース設計 (SQLite)

正: `recisdb-proxy/src/database/schema.rs`。起動時に `IF NOT EXISTS` + `apply_migrations()` で自己構築・自己修復
(band_type/terrestrial_region の NULL 埋め等)。`docs/migrations/*.sql` は参考用スナップショット。

| テーブル | 役割 | 要点 |
|---|---|---|
| `bon_drivers` | DLL 登録・グループ・スキャン設定・容量 | `group_name`, `max_instances`, `auto_scan_enabled`, `scan_interval_hours`, `next_scan_at`, `passive_scan_enabled` |
| `channels` | チャンネル正規化情報 | 一意キー `(bon_driver_id, nid, sid, tsid, manual_sheet)`。`band_type`/`region_id`/`terrestrial_region` は自動判定。`priority` は論理選局の既定優先度、`failure_count` で不良チャンネル追跡 |
| `scan_history` | スキャン実行記録 | 成否・件数・エラー |
| `scan_scheduler_config` / `tuner_config` / `tsreplace_config` | 単一行 (id=1) の実行時設定 | Web API から変更 → 次回参照時に反映 |
| `session_history` | セッション統計の永続化 | drop/scramble/error、平均ビットレート・信号、切断理由 |
| `alert_rules` / `alert_history` | アラート定義と発火記録 | webhook_url/format |
| `driver_quality_stats` | ドライバー個体の品質スコア | `quality_score` 0.0-1.0 (**selector 未接続 — REVIEW 5-5**) |

運用ノート: WAL 未設定・単一コネクション + `tokio::sync::Mutex` (REVIEW 5-1/2 で改善予定)。

---

## 7. 設定レイヤー

優先順位: **CLI 引数 > TOML (`recisdb-proxy.toml`、カレント自動検出) > DB (実行時値) > ハードコード既定**

| 層 | 内容 |
|---|---|
| CLI / TOML | `listen` (既定 **0.0.0.0:40070**), `web_listen` (既定 **127.0.0.1:40080**、2026-07-04 変更。REVIEW P0), `tuner`, `database`, `max_connections` (64), ログ設定, TLS パス, `[web] auth_enabled`/`auth_token`, `[tsreplace] command_path` |
| DB (Web API から変更) | `tuner_config` (keep_alive=60s, prewarm, SetChannel リトライ, signal 待ち), `scan_scheduler_config`, `tsreplace_config` (`command_path` を除く — API からは変更不可), `web_auth_config` (トークン永続化、API からは変更不可・main.rs のみが書き込む) |

TLS 設定 (`[tls]`) は **現状サーバー側で機能しない** (パースのみ、accept 経路未結線)。実装完了までは使用しない。

---

## 8. ロギング / デバッグ

- `tracing-subscriber` + `tracing-appender`: コンソールと `logs/recisdb-proxy.log.YYYY-MM-DD` に二重出力、
  日次ローテーション + `retention_days` で自動削除。ファイル側は ThreadId・ファイル:行番号付き。
- 制御: `-v` / `RUST_LOG` (EnvFilter) / TOML `[logging] level`。詳細は [LOGGING.md](LOGGING.md)。
- クライアント DLL: INI 指定のファイルログ (TVTest プロセス内デバッグ用)。
- 方針: DEBUG=状態遷移、INFO=リソース生成/破棄と選局イベント、WARN=リカバリ可能、ERROR=要対応のみ。

---

## 9. 既知の制約・意図的な未実装 (2026-07 時点)

| 項目 | 状態 |
|---|---|
| サーバー側 TLS | 設定パースのみ。accept 経路に TlsAcceptor 未結線 |
| プロトコル認証 | なし (Hello はバージョンのみ、REVIEW S3 未着手) |
| Web API 認証 | **実装済み (2026-07-04)**: `/api/*` に Bearer トークン認証、CORS はレイヤー自体を撤去、`web_listen` 既定 `127.0.0.1` (REVIEW S2/P0)。プロトコル認証 (S3) は別課題として未着手 |
| 容量制御の場所 | session.rs にアドホック実装 (pool.rs の Semaphore 計画は未着手、MuxKey は未使用) |
| metrics.rs | デッドコード (実体は web/state.rs) |
| 統合テスト | なし (単体テストは protocol/db/ts_analyzer/space_generator 等に散在) |
| graceful shutdown | なし (プロセス kill 前提) |

改善順序は [REVIEW_2026-07.md §7 ロードマップ](REVIEW_2026-07.md) に従う。

---

## 10. ドキュメントマップ

| ファイル | 位置づけ |
|---|---|
| **DESIGN.md** (本書) | 設計の正本 |
| [STREAMING_DESIGN.md](STREAMING_DESIGN.md) | TS 配信・信頼性・固定秒数バッファ・tsreplace 最適化・ブラウザプレビューの設計 |
| [REVIEW_2026-07.md](REVIEW_2026-07.md) | 最新レビュー・改善ロードマップ |
| [QUICKSTART.md](QUICKSTART.md) | 導入手順 (利用者向け) |
| [WEB_DASHBOARD.md](WEB_DASHBOARD.md) | ダッシュボード / API リファレンス (利用者向け) |
| [LOGGING.md](LOGGING.md) | ログ設定リファレンス |
| [ARCHITECTURE.md](ARCHITECTURE.md) | recisdb-rs (CLI) の内部設計 |
| [migrations/](migrations/) | DB スキーマ変更の参考 SQL |
| [archive/](archive/) | 時点レポート (旧計画・実装進捗・修正記録)。正本ではない |
