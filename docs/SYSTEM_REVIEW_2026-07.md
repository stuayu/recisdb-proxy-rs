# システム全体レビュー (2026-07-10)

対象: feature/proxy ブランチ (`2ccb9a7` 時点)。
手法: 5系統の並列レビュー (①BNDPサーバ/セッション層 ②チューナー層 ③Web層
④DB/スキャン/TS解析層 ⑤クライアント/横断的関心) の結果を統合し、重要度順に整理。

---

## 1. 全体アーキテクチャ

```
TVTest / EDCB
   └─ BonDriver_NetworkProxy.dll (bondriver-proxy-client: C++ vtable ABI手実装 + tokio)
        └─ TCP / BNDP (recisdb-protocol: MAGIC+len+type framing, Hello v2, StreamClass)
             └─ recisdb-proxy
                  ├─ server/   listener → Session (状態機械 Initial→Ready→TunerOpen→Streaming)
                  │            client_view (クライアント向け空間/チャンネル列挙の単一真実源)
                  ├─ tuner/    TunerPool + SharedTuner (ChannelKey単位の共有, broadcast fan-out,
                  │            idle-close, warm tuner, EncoderPool, B25, FFI panic隔離)
                  ├─ web/      axum (認証付き/api, ダッシュボード単一raw string,
                  │            Mirakurun互換(無認証・opt-in), channel_files生成)
                  ├─ database/ 単一 rusqlite Connection を Arc<Mutex<>> で全層共有
                  ├─ scheduler/ アクティブスキャン (spawn_blocking + catch_unwind)
                  └─ ts_analyzer/ 本番PSIパーサ (aribb24静的リンクでARIB復号)
```

**設計の健全な点** (5レビュー共通の評価):
- FFI パニック隔離が徹底 (`catch_unwind` + panic hook + `NonNull`/`ManuallyDrop` の Drop 順序管理)。BonDriver DLL の不良でプロセスが落ちない。
- 書き込み経路の専用タスク化 (制御優先 biased select + TSバッチドレイン)、StreamClass 別の背圧ポリシー (RECORD は無音ドロップ禁止) が一貫。
- `prefill` / `client_view` / `channel_resolve` / codec 等、純関数として切り出された部分はテストが充実。
- セキュリティ境界 (トークン定数時間比較、無認証面の固定allow-list、エンコーダパスのTOML限定=RCE対策) が明文化・テスト済み。
- 設計判断が `STREAMING_DESIGN.md` / `REVIEW_2026-07.md` 参照コメントで追跡可能。

---

## 2. 問題点 (重要度順)

### 高

| # | 場所 | 問題 |
|---|------|------|
| H1 | web/dashboard.rs:1948,1950,2267,2268 | **XSS面**: `onclick='editBonDriver(${JSON.stringify(d)})'` 等、JSON.stringify 出力を単一引用符HTML属性へ直挿し。放送/EPG由来の半信頼文字列 (driver_name/channel_name) に `'` や `<` が含まれると属性を脱出し得る。escapeHtml はテキスト挿入のみカバーし属性/イベントハンドラ文脈が抜けている |
| H2 | server/session.rs | **god object**: `Session` が約60フィールド・4200行超・50メソッド。`handle_set_channel_space` は約1080行で、成功時後処理が**5箇所コピペ** (2331,2626,2738,2784,2904行付近)、NID+TSID候補再構築も2重、DLL稼働数カウントは6箇所以上に散在 |
| H3 | server/session.rs:1931-2055 | **DBミューテックスを await 跨ぎで保持**: グループ選局が `database.lock()` を握ったまま `tuner_pool.keys().await` 等を多数呼び、`drop(db)` は124行後。全セッション直列化 + 将来の lock-order 逆転リスク |
| H4 | database/mod.rs:69-72 + 全層 | **単一同期SQLite接続 × tokio::Mutex**: 全サブシステムが1本の `Mutex<Connection>` を奪い合い、同期クエリをロック保持のまま tokio ワーカーで実行。WAL/busy_timeout 未設定で読み書き並行性なし。api.rs だけで `.lock().await` 53箇所 |
| H5 | bondriver-proxy-client/src/client/buffer.rs:100-101, 227-231 | **リングバッファのSPSC不変条件違反**: 満杯時に producer が `read_pos` を書き換え、consumer も `consume()` で書く。非アトミックRMWの二スレッド競合で満杯時にTS破損の恐れ |
| H6 | bondriver-proxy-client/src/client/connection.rs:643-646 | **自動再接続なし**: reader が EOF で `Disconnected` にするだけ。サーバ再起動・瞬断でストリーム恒久停止 (reconnect/backoff 実装ゼロ)。旧 runtime のリーク懸念も |
| H7 | recisdb-rs ⇔ recisdb-proxy | **約2,800行のコード二重管理**: `ts_analyzer/` 9モジュール中6つがバイト一致コピー、`database/` は完全フォークして乖離済み。修正が片側に取り残されるドリフト源 (実際に ARIB 復号で発生、H8) |
| H8 | tuner/ts_parser.rs:495 (decode_arib_string) | **ARIB復号が実質未実装**: UTF-8試行→lossy のみで B24 の2バイト漢字面を復号できない。パッシブスキャナ経路の日本語SIは文字化けする (本番PSIパーサは aribb24 で正常という二重基準) |

### 中

| # | 場所 | 問題 |
|---|------|------|
| M1 | tuner/selector.rs, lock.rs, passive_scanner.rs, ts_parser.rs, space_generator.rs, group_space.rs, shared.rs(start_reader/poll_read_async), session.rs(handle_open_tuner_with_group ほか) | **デッドサブシステム群**: 呼び出し元のない共有/ロック/パーサ/空間生成の抽象が保守対象として残存。selector のスコア計算は priority を信号値として二重計上するロジック破綻あり (デッドなので実害なし=死蔵の証左) |
| M2 | 既知失敗5テスト | **原因はテスト側の陳腐化**: ts_analyzer 4件はフィクスチャが生ASCII前提で、正しくリンクされた aribb24 が ARIB 既定符号集合 (2バイト漢字面) として解釈するため化ける。space_generator 1件は旧NID (BS=0x4011) 前提で現行 `BandType::from_nid` (BS=0x0004) と不整合。どちらもプロダクトバグではない |
| M3 | web/api.rs (2,805行/約50ハンドラ) | 単一ファイルに全ドメイン混在。エラーを `{"success":false}` の **HTTP 200** で返す箇所が約46 (一方 logo/static は正しいステータス)。ステータス設計が二分 |
| M4 | web/dashboard.rs (3,900行) | 単一 raw string に HTML+CSS+JS。lint/型検査/テスト不能、JS変更に Rust 再ビルド必須。保守性の最大の負債 |
| M5 | database/channel.rs:255 (get_all_channels_with_drivers) | NID/SID特定目的でも全件JOIN+ソートを都度実行 (session/api の8箇所以上)。件数増で線形悪化し H4 を悪化させる |
| M6 | database/mod.rs:126-275 | マイグレーションがアドホック: 番号非順序 (…013の後に002)、`user_version` 台帳なし、DDLが schema.rs と mod.rs に分散 |
| M7 | scheduler/scan_scheduler.rs:542- | 1本の spawn_blocking がチャンネル数 × 最大300秒の同期sleepを直列消費。blocking プール枯渇の懸念 |
| M8 | tuner/pool.rs:336-357 | capacity 到達時の `retain(has_subscribers)` が subscribe 前の生成直後チューナーを誤 evict し得る競合。idle-close の check-then-insert TOCTOU も (軽微) |
| M9 | recisdb-proxy 全域 | エラー型不統一: thiserror 依存を持ちながら `Result<_, String>` が26箇所。DLL側は bool/Box\<dyn Error\> 混在 |
| M10 | main.rs (~690行) | Args 20+フィールド + TOML 10セクション + 手動マージが直書き。設定ロードのモジュール分離なし |
| M11 | CI (rust.yml) | テストは master トリガのみで開発ブランチで回らない。clippy/fmt チェックなし |

### 低 (抜粋)

- web: 一覧系のページネーションなし / DTO組み立て重複 / escapeHtml の falsy 値空文字化。
- tuner: Windows `GetTsStream` の黙った切り詰めによりバッファ拡張分岐が死んでいる / `wait_first_data` の50msポーリング / unix.rs の BS/CS 信号補間式が要実機照合。
- session: メッセージデコード失敗の握り潰し (ヘッダ失敗は切断と非対称) / デコードループ二重実装 / `SessionState::Closing` 未使用。
- DLL: `static mut` の RTTI/vtable (Rust 2024 非推奨パターン) / ログ毎行 flush / logging 二系統。
- docs/: DESIGN 系が直近のGUI/インストーラ/ChSet変更に未追随、docs/old・archive 残存。

---

## 3. リファクタリング・ロードマップ

**進捗 (2026-07-11 更新): Phase 0 全項目・Phase 1 全項目が完了。**
残タスクは Phase 2 のみ。各項目の完了コミットは git log を参照
(`9e1f462`〜`88e9fb1` 付近)。


### Phase 0 — 即効・低リスク (合計工数: 小)

1. ✅ **SQLite PRAGMA 追加** (`journal_mode=WAL; busy_timeout=3000; synchronous=NORMAL`) — database/mod.rs に数行。H4 の症状緩和。
2. ✅ **既知失敗5テストの是正** — ARIB エスケープ付きフィクスチャへ更新 / space_generator テストのNID実値化 (M1 でモジュール撤去なら削除)。CI を常時グリーンに。
3. ✅ **デッドコード撤去** (M1) — selector/lock/passive_scanner/ts_parser(※)/space_generator/group_space/start_reader/未実装group系ハンドラ/Closing。※ ts_parser はパッシブスキャン計画が生きているなら撤去でなく H8 の aribb24 化。
4. ✅ **選局成功時後処理の共通化** (実装名 `apply_channel_metadata`) — `apply_selected_channel()` ヘルパーで5コピペ→1 (H2 の即効部分)。
5. ✅ **設定ロードの分離** (`app_config.rs`) (M10) と **CI トリガ/clippy 追加** (M11)。

### Phase 1 — 中期 (工数: 中)

6. ✅ **XSS面の閉塞** (H1) — `JSON.stringify`→onclick を `data-*` 属性 + イベント委譲へ移行。属性文脈用エスケープの導入。
7. ✅ **統一エラーレスポンダ** (ボディ形状は互換維持、ステータスのみ是正) (M3) — `ApiError: IntoResponse` を導入し `success:false@200` を撤廃、`?` でボイラープレート削減。ダッシュボードJSの分岐も追随。
8. ✅ **api.rs のドメイン別分割** (web/api/ 9モジュール) — `api::{bondrivers, channels, alerts, config, encode_profiles, clients, client_view}`。7と同時実施が効率的。
9. ✅ **DBロックスコープ最小化** (H3) — 選局系で「DBから候補・スコア・容量を収集→drop(db)→プール操作」に統一。
10. ✅ **的確クエリ化** (get_channels_by_nid_tsid。一覧APIのlimit/offsetは未実施・データ量が問題化した時点で) (M5) — `get_all_channels_with_drivers` 乱用を NID/TSID/ドライバ指定クエリへ。一覧系APIに limit/offset。
11. ✅ **クライアント再接続** (H6) — 指数バックオフ + Reconnecting 状態 + runtime 再利用。
12. ✅ **リングバッファ SPSC 是正** (consumer側resyncでdrop-oldest復元) (H5) — 満杯時の上書きを consumer 側ドロップへ移すか、位置変数の役割分離。満杯競合テスト追加。
13. ✅ **ARIB復号の一本化** (MinimalTsParser自体をデッドコード撤去で解消) (H8) — MinimalTsParser の独自復号を `crate::aribb24` へ差し替え。
14. ✅ **マイグレーション台帳化** (M6) — `PRAGMA user_version` + DDL の schema.rs 集約。
15. ✅ **ログのflush改善** (毎行flush廃止。tracingへの完全統一は未実施・任意) — DLL 側を tracing + 非同期 appender へ、毎行 flush 廃止。

### Phase 2 — 大規模 (工数: 大, 要計画)

16. **共有クレート化** (H7) — `ts_analyzer` を独立クレート (または recisdb-protocol へ) に切り出し proxy/recisdb-rs 双方から参照。database はプロキシ側がスーパーセットのため共通コア+拡張の段階分離。
17. **SetChannel ポリシーエンジン抽出** (H2) — 選局決定を純関数 `decide(プール状態スナップショット, DB候補) → Decision{reuse/create/evict/fallback/fail}` に分離し、3経路 (SetChannel/SetChannelSpace/SelectLogicalChannel) を1本化。実機検証が難しいホットパスのため、Phase 0-4 の後処理共通化とテスト整備を先行させてから着手。
18. **Session 状態機械の型による強制** — `TunerOpen{tuner}` / `Streaming{tuner, rx}` のようにリソースを状態に内包し、不変条件を型で保証。
19. **ダッシュボードの静的アセット化** (M4) — HTML/JS/CSS をファイルに分離し `include_str!`/rust-embed フォールバック + 開発時ディスク配信。ESLint/型検査/JSテストを可能に。H1 の恒久対策もここで完結。
20. **DBアクセス戦略の刷新** (H4) — 計測の上で、読み取り専用コネクション分離 (WAL前提) or r2d2 プール or spawn_blocking ラッパの段階導入。
21. **セットアップ系の別クレート化** — setup_gui/helpers/px4_installer をクレート分離しサーバ本体のビルドを軽量化。

### 推奨着手順 (残 = Phase 2 のみ)

Phase 2 は 16 (共有クレート化) が他の前提になりやすいため最初に計画する。
17/18 はホットパスで実機検証が必要なため、変更前にテスト整備を先行させること。

---

## 4. 既知の失敗テスト (ベースライン)

**解消済み (2026-07-11)**: かつて既知失敗だった5件 (ts_analyzer×4 は ARIB
フィクスチャ修正、space_generator×1 はモジュール撤去) はすべて解消し、
テストスイートは全クレートでグリーン。
