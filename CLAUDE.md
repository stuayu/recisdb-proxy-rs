# CLAUDE.md — recisdb-rs ワークスペース

Rust製TVチューナーリーダー / ARIB STD-B25デコーダーと、BonDriverネットワークプロキシのワークスペース。

## クレート構成

| クレート | 役割 |
|---|---|
| `recisdb-rs/` | メインCLI(チューナー読み出し・B25デコード) |
| `b25-sys/` | libaribb25 FFIラッパー(`externals/libaribb25` はgitサブモジュール) |
| `recisdb-proxy/` | BonDriverプロキシサーバー(選局・スキャン・Webダッシュボード・Mirakurun互換API) |
| `recisdb-protocol/` | クライアント/サーバー共通プロトコル・NID分類(`broadcast_region.rs`) |
| `bondriver-proxy-client/` | TVTest/EDCB用クライアントDLL(BonDriver_NetworkProxy) |

## ビルド・テスト

```powershell
cargo build -p recisdb-proxy          # プロキシのみ(b25-sysのCビルドを避けたい時もこれ)
cargo test -p recisdb-proxy           # プロキシのテスト(DBはin-memory SQLiteで完結)
cargo test -p recisdb-protocol
cargo test -p bondriver-proxy-client
cargo build --release                 # 配布用。debugと挙動が変わり得る(下記FFI注意)
```

- `[profile.release]` は `overflow-checks = true` / `debug = 2`。
- 実機BonDriver DLLがないと結合的な動作確認はできない。テストはDB/ロジック層に限定される。

## 絶対に守る不変条件

### BonDriver FFI(Windows)
- GetTsStreamは必ず **`C_GetTsStream2`(BYTE**ゼロコピー版)** を使う。BYTE*コピー版は未実装/バグ持ちのDLLが多く、リリースビルドのみクラッシュするUBの原因。
- C++側FFIラッパーは必ず `try { } catch (...) { }` で包む。Rustの `catch_unwind` はC++/SEH例外を捕まえられない。SEHには `/EHa`。
- Rust panic がFFI境界(`extern "system"`)を越えるとプロセスabort(TVTestが無言で落ちる)。クライアントDLL内では panic し得るコード(unwrap/添字)を書かない。
- `from_wide_ptr` は最大長キャップ(32768)+ `?` 必須(`.unwrap()` 禁止)。

### recisdb-proxy のDB
- スキーマ変更は `database/mod.rs` の `MIGRATIONS` 台帳(PRAGMA user_version)に追記する。**全マイグレーションは冪等**であること(pre-ledger DBは user_version=0 から全再生される)。
- `bon_drivers.next_scan_at` のセマンティクス: `0` = 即時ワンショットスキャン要求 / `NULL` = 予約なし / それ以外 = 次回定期スキャン時刻。定期再スキャンは **opt-in**(`auto_scan_enabled=1` かつ `scan_interval_hours>0`)。初回スキャンは `request_immediate_scan`(next_scan_at=0)で走る。

### チャンネル列挙とインデックス
- クライアントに見せる space/channel は**仮想インデックス**(`server/client_view.rs`)。space=放送地域(地デジ広域圏→BS→CS)、channel=(NID,TSID)で重複排除し (NID,TSID) 昇順。
- 関東広域(NHK等)と県域局(テレ玉等)は**同じ「関東」スペース**に入り、県域局のNIDの方が小さいので先頭側に並ぶ。
- インデックスはDBの channels テーブルの内容から導出されるため、**スキャンでDBが変わると既存クライアント(.ch2/TVTestスキャン結果)のインデックスとズレる**。チャンネル列挙順に影響する変更をしたら .ch2/ChSet の再生成が必要な旨を必ず告知すること。

### 選局(チューナー選択)
- 選局の**決定**は `tuner/policy.rs::decide()` の純関数のみ。I/O・async・ログを持ち込まない。
- 決定の**実行**(スロット予約・退避・リーダー起動)は `tuner/acquire.rs::acquire()` のみ。
  BNDP v1/v2・論理チャンネル選局・HTTP/Mirakurun の4経路すべてがここを通る。
  **新しい選局経路を `session.rs` や `web/` に直接書かない。** 経路が持ってよいのは
  「要求の組み立て」と「成功後のメタデータ適用」だけ。
- 容量(`max_instances`)は数えずに**取る**。`TunerPool::acquire_slot` が返す `SlotPermit`
  なしにリーダーを起動できない(`start_reader` の必須引数)。計数関数は診断用。
- 同一DLL上のチャンネル切り替えは permit を**移譲**する(解放→再取得は `max_instances=1` で破綻)。
- `ReaderState` の `Reserved`(作られたが起動前)と `Starting`(起動実行中)を混同しない。
  混ぜると「起動すべきか」の判定が必ず誤る(起動をサボる/二重オープン)。
- 詳細と設計判断: `docs/TUNER_PIPELINE_REDESIGN.md`。

### ストリーミング
- TS配信は tokio broadcast channel(容量4096)。`RecvError::Lagged` は必ず明示的に処理する。
- リーダーの読み取りループは「読む・B25デコード・配る」のみ。SI解析(ロゴ/EPG)など
  チャンク毎の処理を追加しない(`spawn_si_collector` のように broadcast の購読側でやる)。
- `SharedTuner::set_state` は `watch::Sender::send` ではなく `send_replace` を使う。
  `send` は購読者0のとき値を更新せずエラーを返し、後から購読した側が古い状態を見る。

## 設定・ログ

- 設定: `recisdb-proxy.toml`(CWD自動検出 or `-f`)。テンプレートは `recisdb-proxy.toml.example`(セットアップウィザードにも埋め込まれる唯一の情報源。構成変更はこのファイルへ)。
- ログ優先度: `RUST_LOG` > `--verbose` > 設定 `[logging] level` > `"info"`。

## ドキュメント

- `docs/BUILD.md` — ビルドガイド(通常ビルド・macOS→Linux amd64クロスビルド・PC/SC実行時選択)
- `docs/ARCHITECTURE.md` — recisdb-rs本体の設計
- `docs/DESIGN.md` / `docs/STREAMING_DESIGN.md` — プロキシ設計・ストリーミング設計(§番号がコード内コメントから参照される)
- `docs/EPG_DESIGN.md` — 番組表(EIT)収集・保存・配信の設計
- `docs/TUNER_PIPELINE_REDESIGN.md` — チューナー選択・配信・切り替え経路の再設計(2026-08)
- `docs/SYSTEM_REVIEW_2026-07.md` — レビュー指摘と対応状況の台帳
- `docs/WEB_DASHBOARD.md` / `docs/QUICKSTART.md`
- `docs/archive/`, `docs/old/` — 歴史的経緯(現状の仕様としては参照しない)

**ドキュメント更新の義務**: コード変更が上記ドキュメントの記載内容(ビルド手順・設計・API・設定など)に影響する場合は、同じ作業の中で該当ドキュメントも必ず更新すること。ドキュメントに影響しうる変更を委譲する際は、委譲プロンプトに対象ドキュメントの更新も含める。

## マルチエージェント運用ポリシー

このリポジトリでは **Fable(メインセッション)= 指揮役** とし、実作業はサブエージェントに委譲してトークン消費を最適化する。**ユーザーから特に指示がない限り、実作業は Sonnet / Haiku への委譲を優先する**(Fable が自ら手を動かすのは、設計判断・レビュー・不変条件に触る差分の確認と、委譲コストの方が高い微小変更のみ)。

| 作業 | 委譲先 |
|---|---|
| 設計判断・タスク分解・レビュー・最終報告 | Fable自身(委譲しない) |
| 実装・リファクタ・バグ修正・テスト作成 | `sonnet-worker` |
| 検索・列挙・単純な機械的編集・テスト実行・ログ/出力の要約・ドキュメント微修正 | `haiku-worker` |
| 広範囲のコード調査(結論だけ欲しい場合) | 組み込み `Explore`(読み取り専用) |

運用ルール:
1. **簡単な作業ほどHaikuへ。** 「どのファイルか探す」「決まった変更を複数箇所に適用」「cargo testを回して結果を報告」はhaiku-workerで十分。
2. 委譲プロンプトには **対象ファイルパス・期待する成果物・検証コマンド** を明記する(サブエージェントはコールドスタートであることを忘れない)。
3. 同一エージェントに続きを頼むときは新規spawnせず `SendMessage` で継続(コンテキスト再構築のコストを払わない)。
4. FFI境界・マイグレーション・チャンネル列挙順など上記「不変条件」に触る変更は、委譲後にFable自身が差分レビューする。
5. 1〜2ファイルの小さな変更をわざわざ委譲しない(spawnのオーバーヘッドの方が高い)。
