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
- **GetTsStreamが返した `size` は全バイト受け取る。** DLLは返した時点で内部バッファを消費済み。呼び出し側バッファに入らない分を切り捨てると、そのTSは永久に失われ、しかも切れ目が188境界に乗らないので以降の同期も壊れる。入らなかった分は `IBon::pending` に退避し次回返す(`bondriver/windows.rs`)。プロキシ系DLL(BonDriver_NetworkProxy / BonDriverProxyEx)はハードウェアDLLよりずっと大きいチャンクを返すため、多段構成で顕在化する。
- **クライアントDLLは `CreateBonDriver()` ごとに独立インスタンスを返す。** 接続・リングバッファ・選局状態をプロセス共有にしない(詳細と理由: `docs/DESIGN.md` §5)。
- **4Kドライバ(`bon_drivers.stream_format='mmttlv'`)は生MMT/TLVを返す。** リーダーは `tuner/mmt_pipe.rs` の外部変換器(dantto4k CLI)を通してからbroadcastする。変換をbroadcastの後(セッション側)でやらない——TS解析・EPG/ロゴ収集もTSしか解さない。
- **4K(NID 0x000B/0x000C)ではB25を走らせない。** 変換器が復号済みにするがPMTにCA記述子が残り、そのCA system IDが0x0005で我々のB-CASシムと一致するため、libaribb25が死んだECM PIDを掴む。判定は `tuner/acquire.rs::b25_enabled_for` の1箇所(詳細: `docs/FOURK_SETUP.md`)。
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
- 空間の並びは **地デジ→BS→CS→BS4K**。4Kは必ず末尾に置く(途中に挿すと以降のインデックスが全部ずれる)。

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

### Web ダッシュボード(UI)
- **スマートフォン対応は必須。** 画面を追加・変更したら、必ず狭幅(360〜430px程度)でも
  破綻しないことを確認する。運用中の確認(視聴中のクライアント、スキャン状況、
  チューナーの空き)は出先のスマホから行われる前提。
  - 横スクロールを発生させない。テーブルは狭幅でカード表示へ落とすか、
    `data-label` 付きで縦積みにする(既存 `.data-table` の作りに合わせる)。
  - タップ対象は指で押せる大きさを確保する。hover 前提の操作を作らない。
  - 固定幅・固定高さのpxを積まない。既存のブレークポイント(`@media (max-width: 700px)`)に
    合わせる。
- 新しい列や指標を足すときは、**狭幅で何を残し何を隠すか**まで決める。
  `useColumnVisibility` の既定表示列がそのままスマホの初期表示になる。
- チューナーが埋まっている理由(視聴中/スキャン中)は必ず画面に出す。理由が見えないと
  「視聴できない」の原因を利用者が追えない。
- ソースは `web-ui/`(Vue)。`npm run build` の出力を `recisdb-proxy/static/vue/` へ吐き、
  RustEmbed でバイナリに埋め込む。**UIを変更したらビルドしてからサーバーをビルドする。**
  `static/vue/.gitkeep` はビルドで消えることがあるので消さない。

## 設定・ログ

- 設定: `recisdb-proxy.toml`(CWD自動検出 or `-f`)。テンプレートは `recisdb-proxy.toml.example`(セットアップウィザードにも埋め込まれる唯一の情報源。構成変更はこのファイルへ)。
- ログレベル・保持日数はTOML `[logging]`ではなくDB(`log_config`テーブル)管理。起動時点(DBを開く前)の初期値は `RUST_LOG` > `--verbose` > `"info"`。DBを開いた直後にDB設定を`LogLevelHandle::set_level`で適用し(reloadレイヤ経由で反映、再起動不要)、以後はWebダッシュボード「設定 > ログ出力」(`GET`/`POST /api/log-config`)から変更する。ログ出力先ディレクトリのみCLI `--log-dir`(既定 `logs`)。

## ドキュメント

- `docs/BUILD.md` — ビルドガイド(通常ビルド・macOS→Linux amd64クロスビルド・PC/SC実行時選択)
- `docs/ARCHITECTURE.md` — recisdb-rs本体の設計
- `docs/DESIGN.md` / `docs/STREAMING_DESIGN.md` — プロキシ設計・ストリーミング設計(§番号がコード内コメントから参照される)
- `docs/EPG_DESIGN.md` — 番組表(EIT)収集・保存・配信の設計
- `docs/TUNER_PIPELINE_REDESIGN.md` — チューナー選択・配信・切り替え経路の再設計(2026-08)
- `docs/SYSTEM_REVIEW_2026-07.md` — レビュー指摘と対応状況の台帳
- `docs/CLIENT_REVIEW_2026-08.md` — **クライアントDLLのレビュー台帳**。多段構成・拠点間WAN前提で洗い出した18件と対応状況
- `docs/UPDATE.md` — 自己更新(リリース版/開発版アーティファクト)。トークン要件、入れ替え前の起動確認、Windowsでブロックされた場合の復旧
- `docs/FOURK_SETUP.md` — **BS4K対応**。生MMT/TLVを本体で読みdantto4k CLIで変換する構成、復号失敗の見分け方、4Kの分類とB25無効化の理由、未対応事項
- `docs/EPGSTATION_COMPAT.md` — **Mirakurun互換API(`/mirakurun/api/*`)のクライアント側仕様の調査台帳**。EPGStation(stuayuフォーク)が要求するAPI・データ構造・ストリームの挙動と、現状の実装との差分
- `docs/WEB_DASHBOARD.md` / `docs/QUICKSTART.md`
- `docs/archive/`, `docs/old/` — 歴史的経緯(現状の仕様としては参照しない)

**ドキュメント更新の義務**: コード変更が上記ドキュメントの記載内容(ビルド手順・設計・API・設定など)に影響する場合は、同じ作業の中で該当ドキュメントも必ず更新すること。ドキュメントに影響しうる変更を委譲する際は、委譲プロンプトに対象ドキュメントの更新も含める。

**Mirakurun互換まわりの記録義務**: 次のいずれかを行ったら、同じ作業の中で `docs/EPGSTATION_COMPAT.md` を必ず更新する。

1. **クライアント側(EPGStation)の仕様を調べたとき** — 呼び出すエンドポイント・要求するJSONの形・ストリームの終了条件・ヘッダなど。**根拠としてEPGStation側のファイルパスと行番号を必ず添える**(例: `src/model/db/ChannelDB.ts:62`)。調べた結果「実装しない」と判断したものも、判断の根拠として残す
2. **Mirakurun互換APIを追加・変更したとき** — §6の差分表の「状態」を更新する。実装したら「実装済み」にし、着手順の目安も見直す
3. **実起動で疎通を確認したとき** — 静的解析での推測と実際の挙動が食い違った点を最優先で記録する(現状このファイルは静的解析のみで書かれている)

本家Mirakurunとstuayuフォークで型が違う箇所がある(`ChannelType`の`NW1`〜`NW40`など)。**本家のドキュメントや公開仕様だけを根拠にしないこと**——必ずフォーク側の `node_modules/mirakurun/api.d.ts` と EPGStation の実コードで裏を取る。

さらに、**フォーク同梱の `api.yml`(= `GET /docs` が返すOpenAPI定義)自体が実装と食い違っていた**ことがある(`Service.channel`が実装は配列・`api.yml`は単数)。上流 `/Users/ayumu/prog/Mirakurun` では2026-08-09に `api.yml` を配列へ修正済み(未コミット)だが、EPGStation同梱版への反映は未確定。**`api.yml` を単独の根拠にせず、実装(`src/Mirakurun/**`)・`api.d.ts`・`api.yml` の三者を突き合わせること**。本プロジェクトが `/docs` を実装する際は、宣言と実レスポンスの形を必ず一致させる。

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
