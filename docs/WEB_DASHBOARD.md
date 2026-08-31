# recisdb-proxy Webダッシュボード

## 概要

recisdb-proxy にはリアルタイム監視と設定管理用の統合Webサーバーがついています。ブラウザから以下の情報が確認でき、設定値も編集できます。

2026-07以降のフロントエンドは **Vue 3 + Vite + TypeScript** を `web-ui/` でビルドし、成果物を `rust-embed` でサーバーバイナリへ埋め込む構成です。旧HTMLダッシュボードは削除済みのため、Rustをビルドする前に `web-ui` のビルドを実行してください。成果物がない場合、`GET /` は `503 Service Unavailable` を返します。

## アクセス方法

デフォルトでは `http://localhost:40080` で利用可能です。

```bash
# サーバー起動時にWebダッシュボード用アドレスを指定
recisdb-proxy --listen 0.0.0.0:40070 --web-listen 0.0.0.0:40080
```

## 更新方式と認証

- 画面更新は **`GET /api/events` のSSE** を主経路とし、接続できない環境では 30 秒ポーリングへフォールバックします。
- `/api/*` は `Authorization: Bearer <token>` 認証です。Vue側は保存済みトークンを通常のAPI呼び出しとSSE接続の両方に付与します。
- `/static/vue/*` のフロントエンド成果物と `/logos/:file` は、画面表示に必要な静的資産のため無認証で配信されます。`mpegts.js` はVueバンドルへ同梱されます。

## 機能

### 分散ノードのセットアップ

「分散ノード」画面は、登録フォームではなく現在の構成と状態を表示します。
「＋ 別のPC・拠点を追加」から、用途の選択、接続情報の貼り付け、通信方法の
自動確認、保存前の確認を行うウィザードを開始できます。接続情報は
`recisdb://pair?endpoint=...&code=...` 形式に対応し、既存の
`POST /api/nodes/pairing` と `/pairing/redeem` を使用します。

通常画面では Node ID、credential、EndpointKind、weight、RTT や帯域の生値を
要求しません。これらはノードカードの「詳細設定（Expert Mode）」に分離しています。
Route Group は「受信エリア」と表示し、ノードカードでは「接続中」「快適」
「推奨」「利用不可」などの文字付き状態を使います。Cloudflare Public など
`record_allowed=false` の経路は「視聴には使用できます／録画には使用しません」
と表示します。

「自動」は利用可能な通信方法を確認し、現在の静的優先順に従って推奨経路を選ぶ
意味です。実測値を継続保存して常に最適化する機能ではありません。

分散ノードAPIの`GET /api/nodes`は、受信エリアの所属拠点、`setup_status`、
`topology`を返します。受信エリアは画面から作成・名前変更・所属解除・削除が
可能です。所属の選択肢は自動（weight=100）、優先（200）、予備（50）で、
既存の`node/route.rs`の選択優先度は変更しません。数値weightは通常画面に
表示せず、内部保存値だけ維持します。

ノード更新は既存の`POST /api/nodes`を使い、credential未指定時はDBの既存値を
保持します。APIエラーには`error_code`を付け、UIはコードで利用者向け文言へ
変換します。生エラーは診断詳細として保持します。

### 1. リアルタイム監視

**チューナー状況**
- 登録されたすべてのBonDriverを表示
- 各BonDriverの最大インスタンス数
- 現在の使用インスタンス数

**クライアント接続状況**
- 接続中のセッション一覧
- クライアントのIPアドレス
- 現在のセッション状態
- 接続先チューナーと選択チャンネル
- **接続方式** (`BonDriver` / `HTTP` / `Mirakurun`) — TVTest・EDCB のような BonDriver クライアントだけ
  でなく、ダッシュボードのプレビューと Mirakurun 互換 API 経由の視聴・録画 (EPGStation 等) も
  同じ一覧に並ぶ。切断・グラフ・プレビューはどの方式でも同じように使える
  (Mirakurun 経由の録画を切断すると EPGStation 側では録画失敗になる)

**サーバー統計**
- 総セッション数
- アクティブセッション数
- アクティブなチューナー数
- サーバー稼働時間

### 2. データベース設定編集

BonDriver毎の以下の設定をWeb UIから編集可能：

```json
{
  "id": 1,
  "dll_path": "C:\\BonDriver\\BonDriver_PX-MLT1.dll",
  "display_name": "PX-MLT1",
  "group_name": "PX-MLT",
  "max_instances": 4
}
```

**設定フィールドの説明:**
- `group_name`: グループ名（複数ドライバーを統合した場合）。例：PX-MLT, PX-S など
- `max_instances`: BonDriver が同時にサポートできるチャンネル数の上限
- 複数クライアントが異なるチャンネルを同時要求した場合、優先度によって割り当てが決定される

### 3. クライアント設定ガイド (「クライアント設定」タブ)

TVTest / EDCB 側の設定を画面の指示どおりに進められるガイドです。

- **STEP 1**: `Tuner=` に指定できる名前 (チューナーグループ / 個別ドライバー) の一覧から接続先を選択
- **STEP 2**: 接続先アドレス・選択チューナー入りの `BonDriver_NetworkProxy.ini` をワンクリックでコピー
- **STEP 3**: チャンネル設定ファイルのダウンロード
  - TVTest 用 `.ch2` (Shift_JIS) — BonDriver_NetworkProxy.dll と同じフォルダに配置
  - EDCB 用 `ChSet4.txt` / `ChSet5.txt` (UTF-8 BOM) — EDCB の Setting フォルダに配置
  - 「まとめてダウンロード」で INI・README を含む zip を取得可能
- **STEP 4**: クライアントに列挙されるチューニング空間・チャンネルの対応表 (空間番号・CH番号は
  クライアントが `SetChannel(space, channel)` に渡す実際の値)

チャンネル列挙はセッションが実際に使う `server/client_view.rs` と同一コードで生成されるため、
表示内容とクライアントの動作が食い違うことはありません。

### 4. チャンネル一覧 (「チャンネル」タブ)

スキャンで登録された全チャンネルの一覧・編集画面です。

**表示列**: 既定では 有効 / チャンネル名 / NID / SID / TSID / バンド / 地域 /
ネットワーク / チューナー / BonSpace / BonChannel / 優先度 を表示します。
テーブル上部の **「表示列を調整」** から、DBが保持する残りの情報も列として
追加できます (選択はブラウザに記憶されます):

| 列 | 内容 |
| --- | --- |
| ID | channels テーブルの主キー |
| raw名 | スキャン時に取得した生のサービス名 (channel_name は編集可能な表示名) |
| 枝番 | manual_sheet |
| 物理CH | 物理チャンネル番号 |
| リモコン | リモコン番号 (NIT TS情報記述子由来。古いスキャン結果では空 — 再スキャンで取得) |
| サービス種別 | TV / 音声 / 臨時 / プロモ / データ (不明値は16進表示) |
| BonDriver | チャンネルを保持する BonDriver の DLL パス |
| 失敗回数 | 選局失敗のカウント |
| スキャン日時 / 最終確認 | スキャン実行時刻と最終確認時刻 |
| 登録日時 / 更新日時 | DB レコードの作成・更新時刻 |

すべての列はヘッダクリック (モバイルは並び替えセレクト) でソートできます。
「編集モード」ではチャンネル名・優先度・有効/無効・物理割当の一括編集、
行の追加・削除、CSVエクスポート/インポートが可能です。

**Drop / Error 統計の見方 (概要タブのクライアント一覧)**:
- **Drop** はCC(連続性カウンタ)の欠落です。チャンネル切替直後や、配信バッファの
  ラグ回復(別途 broadcast_lag として計上)による既知のギャップはカウントされません。
- **Error** はチューナーのデモジュレータが立てる transport_error_indicator の
  実受信エラーです。BS/CS でこの値が継続的に増える場合は、アンテナレベル・
  ケーブル・LNB給電など信号品質側の確認を推奨します。

### 5. ログ閲覧 (「ログ」タブ)

サーバーの直近ログ(最大5000行、インメモリのリングバッファ)をブラウザから閲覧できます。
`logs/recisdb-proxy.log.*` を直接 tail する必要はありません。

- **種別切り替え(すべて / サーバー / アクセス)**: HTTP アクセスログ
  (`web/mod.rs` の `access_log` ミドルウェアが出す `100.90.205.104:65290 "GET
  /api/stats" 200 0ms` のような行)とサーバー側の処理ログ(スキャン・チューナー・
  EPG など)を分けて表示する。ダッシュボードを開いているだけでアクセスログが
  ポーリング間隔ごとに流れて目的のログが埋もれる問題があったため、**既定は
  「サーバー」(アクセスログ以外)**。アクセスログだけを見たいときは「アクセス」
  に切り替える。内部的には `target` が `recisdb_proxy::access` かどうかで判定
  しており(`GET /api/logs` の `category` パラメータ、後述)、既存のターゲット
  絞り込み・メッセージ検索と併用できる。
- **レベルフィルタ**: 選択したレベル**以上**を表示 (ERROR > WARN > INFO > DEBUG > TRACE)。
- **ターゲット絞り込み** / **メッセージ検索**: いずれもサーバー側でフィルタしてから返す
  (取得済みの表示行だけを対象にしたクライアント側フィルタではない)。
- **リアルタイム追尾**: 2秒間隔のポーリング (`after_seq` によるインクリメンタル取得)。
  最下部にいる間は新着で自動スクロールし、上にスクロールすると追尾を解除して
  「最新へ」ボタンを表示。「一時停止」でポーリング自体を止められる。タブを離れる
  (他のタブへ切り替える) と自動的にポーリングを停止する。
- レベル別に色分け表示 (ERROR=赤系 / WARN=黄系 / INFO=通常 / DEBUG・TRACE=淡色)。
  表示中エントリの ERROR / WARN 件数をバッジで表示 (未読管理ではなく、現在の表示分の集計)。
- 折りたたみセクションから過去のログファイル (`recisdb-proxy.log.YYYY-MM-DD`) の一覧
  (ファイル名・サイズ・更新日時) を確認し、クリックでダウンロードできる。

バッファは容量5000件を超えると古い行から破棄される。ポーリングで取得しようとした
`after_seq` が既に破棄済みの範囲を指していた場合、レスポンスの `dropped: true` を見て
全件再取得するのはフロントエンド側の責務(`GET /api/logs` 参照)。

**ログレベル・保持日数の変更**は「ログ」タブではなく「設定」タブの「ログ出力」パネルから行う
(`GET`/`POST /api/log-config`、後述)。かつての `recisdb-proxy.toml` の `[logging]` セクション
は廃止され、DBの `log_config` テーブルが正となった。レベルの変更は再起動不要で即座に反映される。

## API エンドポイント

### GET /api/channels

登録チャンネルの一覧を返す。クエリ: `bondriver_id=<id>` (特定BonDriverのみ)、
`enabled_only=true` (有効のみ)、`group_logical=true` (NID-SID-TSID で論理チャンネルに
まとめ、保持チューナー数 `tuner_count`/`tuner_names` を付与)。

各チャンネルは DB の全カラムを含む: `id, bon_driver_id, bon_driver_path, nid, sid,
tsid, manual_sheet, raw_name, channel_name, physical_ch, remote_control_key,
service_type, network_name, bon_space, bon_channel, band_type, region_id,
terrestrial_region, is_enabled, priority, failure_count, scan_time, last_seen,
created_at, updated_at` (タイムスタンプは UNIX 秒。`group_logical=true` では
created_at は最古、updated_at は最新をマージ)。

### GET /api/client-view/targets

クライアントの `Tuner=` に指定できる候補一覧 (グループ優先) と、配布用 INI 生成に使う
プロキシ待受ポートを返す。

### GET /api/client-view?tuner=&lt;名前&gt;

指定した Tuner 名で接続したクライアントが列挙する仮想チューニング空間・チャンネルの一覧
(クライアントが指定する space/channel インデックス、表示名、物理マッピング) を返す。
名前解決は OpenTuner と同じ優先順位 (DLLパス → グループ名 → 表示名)。

### GET /api/client-view/files/:kind?tuner=&lt;名前&gt;

チャンネル設定ファイルを生成してダウンロードする。`kind`:

| kind | 内容 | エンコーディング |
| --- | --- | --- |
| `tvtest-ch2` | TVTest 用 `BonDriver_NetworkProxy.ch2` | Shift_JIS (表現不能時 UTF-16LE BOM) |
| `chset4` | EDCB 用 `BonDriver_NetworkProxy(BonDriver_NetworkProxy).ChSet4.txt` | UTF-8 BOM |
| `chset5` | EDCB 用 `ChSet5.txt` | UTF-8 BOM |
| `bundle` | 上記 + `BonDriver_NetworkProxy.ini` + README の zip | ― |

### GET /api/tuners

すべてのBonDriver情報を取得

**レスポンス例:**
```json
{
  "success": true,
  "tuners": [
    {
      "id": 1,
      "dll_path": "C:\\BonDriver\\BonDriver_PX-MLT1.dll",
      "display_name": "PX-MLT1",
      "group_name": "PX-MLT",
      "max_instances": 4
    }
  ],
  "count": 1
}
```

### GET /api/clients

接続中のクライアント一覧を取得

**レスポンス例:**
```json
{
  "success": true,
  "clients": [
    {
      "session_id": 1,
      "address": "192.168.1.100:54321",
      "state": "STREAMING",
      "tuner_path": "C:\\BonDriver\\BonDriver_PX-MLT1.dll",
      "current_space": 0,
      "current_channel": 27
    }
  ],
  "count": 1
}
```

### GET /api/events

ダッシュボード更新通知用の Server-Sent Events エンドポイント。`event: refresh` を受け取ったクライアントは `/api/stats` や `/api/clients` を再取得する。

### GET /api/stats

サーバー統計情報を取得

**レスポンス例:**
```json
{
  "success": true,
  "stats": {
    "total_sessions": 5,
    "active_sessions": 2,
    "total_tuners": 2,
    "active_tuners": 1,
    "uptime_seconds": 3600
  }
}
```

### GET /api/version

稼働中サーバーのバージョン(`{"version": "0.1.0"}`)を取得。ダッシュボードはこれをヘッダーのバージョン表示に使うほか、GitHub最新リリース(`stuayu/recisdb-proxy-rs`)とのバージョン比較の基準値としても使う(6時間キャッシュ、`localStorage`)。

このバージョン文字列は `Cargo.toml` の固定値ではなく、ビルド時に `recisdb-proxy/build.rs` が決定して埋め込む(`crate::VERSION` / `RECISDB_PROXY_VERSION`)。リリースタグ(例: `v0.0.1-alpha.6`)そのままのビルドでは `0.0.1-alpha.6` に、タグ間のdevビルドでは `git describe --tags --always --dirty` により `0.0.1-alpha.6-1-g05a127c` のような形式になる。詳細は `docs/BUILD.md` を参照。Mirakurun互換API(`/mirakurun/api/version`, `/mirakurun/api/status`)はEPGStation等の互換性のためあえて `Cargo.toml` 固定値のまま。

### GET /api/update/check

サーバー側で GitHub releases (`stuayu/recisdb-proxy-rs`) を取得し、現在のバージョンより新しい stable / prerelease を判定して返す(実装: `web/api/update.rs`)。ブラウザが GitHub に直接アクセスする必要はない。

- サーバー内メモリに6時間キャッシュ(DBには保存しない)。`?force=true` でキャッシュを無視して再取得。ヘッダーのバージョン表示横の「更新確認」ボタンがこの force 取得を呼ぶ(以前×で閉じた更新通知もこの操作で再表示される)。
- `stable`: draftを除く最新の非プレリリースで、現行より新しいもの(なければ `null`)。
- `prerelease`: 最新のプレリリースで、現行より新しく、かつ `stable` より新しいもの(`stable` に劣後するプレリリースは出さない。なければ `null`)。
- `self_update_supported`: このビルドが自己更新に対応しているか(Linux x86_64/aarch64、Windows x86_64/x86 のみ `true`。macOSビルド等は `false`)。
- GitHub到達失敗時はエラーにせず、`stable`/`prerelease` を `null` にしたまま `200` を返す(ダッシュボードを壊さない)。

**レスポンス例:**

```json
{
  "current_version": "0.1.0",
  "stable": {"tag": "v0.2.0", "url": "https://github.com/stuayu/recisdb-proxy-rs/releases/tag/v0.2.0", "published_at": "2026-07-01T12:00:00Z"},
  "prerelease": null,
  "self_update_supported": true
}
```

### POST /api/update/apply

指定タグの自己更新を開始する。**BonDriver_NetworkProxy.dll クライアント(TVTest/EDCB側)は対象外** — 更新されるのは `recisdb-proxy` サーバー本体の実行ファイルのみ。

- リクエスト: `{"tag": "v0.2.0"}`。
- `self_update_supported` が `false` なビルド(macOS等)では `501 Not Implemented`。
- 指定タグがリリース一覧に無ければ `404`。
- 既に自己更新が進行中(`downloading`/`extracting`/`replacing`/`restarting`)なら `409 Conflict`。
- 成功時は `202`(実体はバックグラウンドで進行)。ダウンロード→展開→検証(サイズ・マジックバイト)→`self-replace`crateによる実行中バイナリの置換→約1秒待って再起動、の順に進む。**自バイナリに触れるのは置換の直前のみ**で、それより前の失敗では元のバイナリは無傷のまま。
- 再起動の仕組み: バイナリの置換自体はどちらのOSでも**プロセスを止めずに**行える(Linuxはrename、Windowsは`self-replace`が実行中exeを退避リネームして新exeを配置)。その後、
  - **Linux**: `exec()` で自プロセスのイメージを新バイナリに差し替える。PID・cgroupが変わらないため、systemdサービス(`Restart=always`)でも素の起動でもそのまま成立し、`systemctl stop/start`(root権限)は不要。
  - **Windows**: リッスンポートの競合を避けるため、デタッチした`cmd`リランチャー(約3秒待機後に新exeを`start`)をspawnして自プロセスは即終了する。手動起動・タスクスケジューラ起動を想定。**Windowsサービスとして登録して運用している場合はSCM管理下に戻らないため対象外**(サービス側の再起動設定で復帰させること)。
- 実行ファイルのあるディレクトリに書き込み権限が必要(`Program Files` 直下等では失敗し、`error` 状態で停止する)。

### GET /api/update/status

`POST /api/update/apply` で開始した自己更新の進行状況を返す。

```json
{ "state": "idle" | "downloading" | "extracting" | "replacing" | "restarting" | "error", "message": null }
```

### GET /api/logs

インメモリのログリングバッファ(最大5000件、実装: `logging/buffer.rs`)から直近ログを返す。
「ログ」タブがポーリングで叩くエンドポイント。

クエリ(すべて省略可):

| パラメータ | 内容 |
| --- | --- |
| `level` | 指定レベル**以上**のみ返す(`error`/`warn`/`info`/`debug`/`trace`、大文字小文字不問) |
| `target` | `target` (tracingのモジュールパス) への部分一致(大文字小文字を区別) |
| `category` | `all`(既定) / `server`(HTTPアクセスログ以外) / `access`(HTTPアクセスログのみ)。判定は `target` が `recisdb_proxy::access`(`access_log` ミドルウェア専用のターゲット)と一致するかどうか。未知の値は `all` として扱われる。`target` の部分一致フィルタとは AND で併用できる |
| `q` | `message` への部分一致(大文字小文字不問) |
| `after_seq` | この連番より新しい行のみ返す(インクリメンタル取得用、既定 `0`) |
| `limit` | 最大返却件数。既定 `500`、最大 `2000` |

**レスポンス例:**

```json
{
  "entries": [
    {"seq": 1042, "timestamp": "2026-07-19T21:00:00.123456+09:00", "level": "WARN", "target": "recisdb_proxy::tuner::pool", "message": "..."}
  ],
  "last_seq": 1042,
  "dropped": false
}
```

- `last_seq`: バッファが現在保持している最新の連番。次回ポーリング時の `after_seq` に使う。
- `dropped`: `after_seq` が指していた地点より古い行がバッファから既に破棄されていた場合 `true`。
  この場合レスポンスの `entries` はベストエフォートで、間に破棄された行があり得るため、
  クライアントは `after_seq` を破棄して(`0` から)全件を取り直すべき。

### GET /api/logs/files

`--log-dir` (既定 `logs/`) 直下のローテーション済みログファイル
(`recisdb-proxy.log.YYYY-MM-DD`) の一覧をファイル名の降順(新しい日付が先頭)で返す。

```json
{ "files": [{"name": "recisdb-proxy.log.2026-07-19", "size": 123456, "modified": "2026-07-19T21:00:00+09:00"}] }
```

### GET /api/logs/files/:name

指定したログファイルをダウンロードする(`Content-Disposition: attachment`)。`name` は
`recisdb-proxy.log.` で始まり、パス区切り文字・`..` を含まない、`log_dir` 直下の
実在するファイル名のみ許可(パストラバーサル対策、`web/api/logs.rs`)。それ以外は `400`。

### GET /api/log-config

現在のログレベル・保持日数(DBの `log_config` テーブル、実装: `database/mod.rs` migration
022)を返す。`env_override` は起動時に `RUST_LOG` 環境変数が設定されていたかどうか
(モジュール別の細かい指定はこの値では表現されない)。

```json
{ "success": true, "config": { "level": "info", "retention_days": 7, "env_override": false } }
```

### POST /api/log-config

ログレベル・保持日数を変更する。両フィールドとも省略可(省略時は現状維持)。

```json
{ "level": "debug", "retention_days": 14 }
```

- `level`: `trace`/`debug`/`info`/`warn`/`error` のいずれか(大文字小文字不問)。それ以外は
  `400`。受理されると `tracing_subscriber` の reload レイヤ経由で**即座に**反映される
  (再起動不要)。
- `retention_days`: `1`〜`365` の範囲。範囲外は `400`。保存後、その場でログクリーンアップを
  1回実行する(次回起動を待たずに古いログが消える)。

### GET /api/config

現在の設定を取得

### POST /api/config

複数のBonDriver設定を一括更新

**リクエスト例:**
```json
{
  "bon_drivers": [
    {
      "id": 1,
      "dll_path": "C:\\BonDriver\\BonDriver_PX-MLT1.dll",
      "display_name": "PX-MLT1 Updated",
      "max_instances": 6
    }
  ]
}
```

### POST /api/bondriver/:id

特定のBonDriver設定を更新

**リクエスト例:**
```json
{
  "display_name": "PX-MLT1",
  "group_name": "PX-MLT",
  "max_instances": 4
}
```

## 設定例

### 複数チューナーの初期設定

サーバー起動時に、DBにBonDriverを登録し、`max_instances` を設定：

```bash
sqlite3 recisdb-proxy.db << EOF
UPDATE bon_drivers SET max_instances = 4 WHERE dll_path LIKE '%PX-MLT1%';
UPDATE bon_drivers SET max_instances = 1 WHERE dll_path LIKE '%PX-S%';
EOF
```

その後、WebダッシュボードからGUIで設定値を変更可能です。

## 開発メモ

- Vueソース: `web-ui/`
- ビルド出力: `recisdb-proxy/static/vue/`
- サーバー側埋め込み: `recisdb-proxy/src/web/dashboard.rs` (`rust-embed`)
- CI: `npm install` → `npm run build` → `cargo build`

## トラブルシューティング

### ダッシュボードにアクセスできない
- サーバーのポートが開いているか確認: `netstat -ano | findstr :40080`
- ファイアウォール設定を確認
- `--web-listen` オプションで正しいアドレスが指定されているか確認

### 設定変更が反映されない
- ブラウザのキャッシュをクリア
- 5秒ごとに自動更新されるので、しばらく待つ
- サーバーログで同期エラーが出ていないか確認

## 実装済みの主な機能

以下はすべて実装済みです:

- クライアント毎の Drop/Scramble/Error 統計表示 (概要タブのクライアント一覧)
- 配信ストリーム品質の可視化 — ビットレート・パケットロス・信号レベルのスパークライン
- リモートからの強制切断・優先度/排他の上書き (クライアント行の操作)
- セッション履歴タブ
- アラートルール設定と Webhook 通知 (アラートタブ)
