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

## セットアップウィザード

`recisdb-proxy-setup` の本体インストールでは、チューナー選択後に「詳細設定」ステップがある。
アクセス範囲、Mirakurun互換API、地元都道府県、分散ノード名、ブラウザプレビュー、tsreplaceを指定できる。
既定値はLAN公開・Web認証有効・Mirakurun互換API無効。地元設定例は「大阪」にする。

LAN公開時、ノード間通信は視聴ポート+1 (`40070`なら`40071`)。Windowsではウィザードが
`recisdb-proxy.exe`単位の受信許可ルールをPrivate/Domainだけに追加する。Linux/macOSでは
使用中の視聴・ノード・Webポートをファイアウォールで許可する。

ブラウザプレビューはffmpegとtsreadexを検出または自動取得してTOMLへ保存する。tsreplaceは
Linux/macOSではPATHから検出する。Windowsの7z自動展開に必要な依存がない場合は、既存の
`<install_dir>\\thirdparty\\tsreplace\\tsreplace.exe` またはPATHを検出し、未導入時はrigaya/tsreplaceの取得先をログへ出す。

## 更新方式と認証

- 画面更新は **`GET /api/events` のSSE** を主経路とし、接続できない環境では 30 秒ポーリングへフォールバックします。
- `/api/*` は `Authorization: Bearer <token>` 認証です。Vue側は保存済みトークンを通常のAPI呼び出しとSSE接続の両方に付与します。
- `/static/vue/*` のフロントエンド成果物と `/logos/:file` は、画面表示に必要な静的資産のため無認証で配信されます。
- **レスポンスは gzip で圧縮します** (`CompressionLayer`)。圧縮するのは
  JSON / JavaScript / CSS / HTML だけの**許可リスト方式**です。TS 配信 (`video/mp2t`) と
  SSE (`text/event-stream`) は一覧に無いので対象から外れます。TS に圧縮を挟むと
  バッファリングでリアルタイム性が壊れるうえ、既に圧縮済みのバイナリなので CPU を
  捨てるだけになります。SSE は圧縮バッファにイベントが溜まって届かなくなります。
  拒否リストにしないのは、あとからストリーム系のエンドポイントが増えても
  既定で圧縮されないようにするためです。
- **DPlayer と `mpegts.js` は動的 import です。** ブラウザプレビューでしか使わないため、
  初期バンドルには入れません。実測で `app.js` は 869KB → 306KB (分割後) → gzip 105KB。
  拠点間 WAN 越しに使う構成では、この転送量が体感を支配します
  (API も描画も本番規模で数十 ms しかかかっていませんでした)。
- **ビルド成果物は内容ハッシュ付きの名前です** (`assets/app-<hash>.js` など。
  `web-ui/vite.config.ts`)。`index.html` (`/`) は `Cache-Control: no-cache`、
  `/static/vue/assets/*` は `public, max-age=31536000, immutable` で配ります
  (`web/api/statics.rs::cache_control_for`)。以前は固定名 `app.css` をヘッダ無しで
  配っており、Cloudflare が `max-age=14400` を付けるため、更新後もスマホだけ
  最大 4 時間古い CSS/JS のままになっていました (番組表の時刻軸の着色が
  スマホに反映されなかった件)。

## 機能

### EPG自動取得

「設定 > EPG自動取得」でDB管理のglobal設定を変更できる。更新頻度と保持日数は人間向けの
選択肢、秒単位の滞在時間とCPU上限はエキスパート設定に分離している。保存結果は再起動不要で
次回判定から反映される。プリセット一覧は同じ画面に説明付きで表示され、system presetは
削除不可。effective値の表示・physical tuner override・状態/history画面はAPI基盤追加後に
段階実装する。

設定画面では `scheduler_interval_secs`、`startup_delay_secs`、`startup_jitter_secs` を設定
できる。`reserve_tuners`、`preemptible`、`reserve_for_recording_override` はAPI/UIから
削除した。DB列は既存データとの互換性
のため残るが、実行時には使わない。スキャン枠は `max_concurrent_scans` で管理し、EPGは
録画・視聴へ常に退避する。

Active scan状態は「取得中」「延期理由」「最終更新」「次回判定」をstatus APIから表示する。
延期理由はbackendの構造化statusをUIで日本語化し、CPU負荷・録画/視聴による占有を区別する。

プリセット編集は基本・チューナー・負荷・リモート・詳細へ分割し、個別値を「全体設定に戻す」
操作で継承へ戻せる。system presetは複製のみ可能。EPG status/historyの理由は`code`と
`details`に加えて`networkId`/`tsid`/`label`/`tunerId`/`nodeId`/`count`を持つJSONで返す。
同一codeは系統ごとにまとめ、最大3系統まで返す。画面側はコード対応表と系統情報を表示する。
トップレベルの`reason`は従来形式の先頭理由を後方互換のため維持する。

### 分散ノードのセットアップ

「分散ノード」画面は、登録フォームではなく現在の構成と状態を表示します。
「＋ 別のPC・拠点を追加」から、用途の選択、接続情報の貼り付け、通信方法の
自動確認、保存前の確認を行うウィザードを開始できます。接続情報は
`recisdb://pair?endpoint=...&code=...` 形式に対応し、既存の
`POST /api/nodes/pairing` と `/pairing/redeem` を使用します。

ペアリング画面は、`0.0.0.0`/`::` の待受アドレスを接続先として表示しません。
サーバーが列挙した `http://<実インターフェイスIP>:<ノードポート>` 候補を
Tailscale、LAN、その他の順に使います。コード発行側は先頭候補をQR・コピー文字列へ
入れ、候補一覧は `endpoints` として返します。コード入力側のSTEP 3では、このPCの
候補を全選択した状態で表示し、チェック解除とURL編集ができます。選択した候補は
redeem時に相手ノードへ渡るため、登録が片方向になりません。候補が旧版で欠落する
場合も、従来の `node_listen_addr` と空配列を受理する後方互換を維持します。

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

ペアリングウィザードは接続確認後にVIEW/PREVIEW/RECORD probeを実行します。
VIEW経路がない場合は仮登録したローカルノードを自動削除し、失敗理由と再試行を
表示します。topologyはprobe済み経路だけ測定値を表示し、未probe経路は「未測定」
として通信テストへの導線を表示します。

接続情報は`recisdb://pair?endpoint=...&code=...`形式でQR表示・コピーできます。
QRはブラウザ用`qrcode`エントリをバンドルして生成するため外部CDN不要です。
期限切れは画面で明示し、再発行できます。文字列も常に併記します。

ノード削除・診断ロールバック時は、認証済みの`DELETE /node/v3/peer`で相手側の
reciprocal peerも削除します。相手へ到達できない場合は、ローカル削除後に相手側で
このPCの接続設定を削除するよう警告します。

responsive確認スクリプトはfile URLをPlaywright Chromiumで実測します。Chromiumが
利用できない場合は静的検査を補助的に行いますが、コマンド自体は失敗します。
対象幅は390px、768px、1280pxです。

### 1. リアルタイム監視

**システム負荷**
- 概要タブの「システム負荷」で、ホスト全体のCPU使用率・コア数・load average、メモリ使用量、全ネットワークインターフェースの受信/送信量を表示する。
- ネットワークは累計バイト数と、直近のサンプル間隔から計算した受信/送信スループットを表示する。値は5秒ごとに更新し、直近15分（180サンプル）を折れ線グラフで保持する。履歴はメモリ上だけに保持し、DBには保存しない。
- GPUはベストエフォートで、起動時に利用できたプローブだけを保持し、5秒ごとに取得する。複数GPUはベンダー内の `index` 付きで個別表示する。値を取得できない項目は `未取得` と表示し、0には置き換えない。GPUプローブが1つも成功しない場合、GPUセクションは表示しない。

| ベンダー / 環境 | 取得元 | 取得できる値 | 取得できない場合・確認方法 |
| --- | --- | --- | --- |
| NVIDIA (Linux / Windows) | `nvidia-smi --query-gpu=index,name,utilization.gpu,memory.used,memory.total --format=csv,noheader,nounits` | index、名前、使用率、VRAM使用量・総量 | コマンドがPATH上にあり、終了ステータスが成功する必要がある。全行を対象にし、壊れた行だけ破棄する |
| AMD (Linux) | 第一候補 `rocm-smi --showuse --showmeminfo vram --json`、フォールバック `/sys/class/drm/card*/device/{vendor,gpu_busy_percent,mem_info_vram_used,mem_info_vram_total}` | 名前 (ROCm未提供時は `AMD GPU N`)、使用率、VRAM使用量・総量 | sysfsのvendorが `0x1002` である必要がある。ファイルが無い項目は未取得。ROCm/sysfsとも無い場合は非表示 |
| Intel (Linux) | `/sys/class/drm/card*/device/vendor` (`0x8086`)、任意の `gpu_busy_percent` | 存在、名前 ( `Intel GPU N` )、使用率 (ファイルがある場合) | VRAMは未取得。使用率ファイルが無い場合もGPU自体は表示する。`intel_gpu_top` は使用しない |
| AMD / Intel (Windows) | PowerShell `Get-Counter "\\GPU Engine(*)\\Utilization Percentage"` | アダプタ単位の使用率 | VRAM、名前、ベンダーは未取得。PowerShellが無い・権限不足・タイムアウト時は非表示 |
| Apple (macOS) | 対応なし | なし | 開発機ではGPU情報を表示しない |

外部コマンドは2秒のタイムアウト付き。起動時探索で失敗したプローブは以後呼ばない。Windowsの共通カウンタはサンプリング間隔内で実行し、失敗しても他のメトリクス収集を止めない。

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
- 排他 claim を持つセッションには `🔒 ロック中` を表示する。ロック解除は確認後に
  `override_exclusive=false` を即時適用し、「解除済み(override)」と「元に戻す」を表示する。
- `GET /api/stats` の `stats.locked_tuners` はロック中チューナーとセッション ID・接続方式・アドレスを返す。
  概要画面は「ロック中(セッションX)」として、チューナーが埋まる理由を表示する。

**サーバー統計**
- 総セッション数
- アクティブセッション数
- アクティブなチューナー数
- サーバー稼働時間

`GET /api/clients` の既存フィールドは維持したまま、`locked`、`lock_override` を追加する。
`POST /api/client/:id/controls` の `override_exclusive=false` は次の選局を待たず、対象の
購読中 claim を非exclusiveへ更新する。`null` で元のクライアント設定へ戻す。

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

本番規模 (channels テーブル 1637 行) では全行を一度に描画すると操作できなくなるため、
一覧は**ページング**で表示します (既定 100 件。50 / 100 / 200 / 500 から選択でき、
選択はブラウザに記憶されます)。絞り込み・ソートを変更すると 1 ページ目へ戻ります。
絞り込みの入力は 200ms のデバウンスを挟んでから適用されます。
狭い画面 (1100px 以下) では主要列だけを表示し、ツールバーの
**「狭い画面では主要列だけ表示」** を外すと全列に戻せます。

**Drop / Error 統計の見方 (概要タブのクライアント一覧)**:
- **Drop** はCC(連続性カウンタ)の欠落です。チャンネル切替直後や、配信バッファの
  ラグ回復(別途 broadcast_lag として計上)による既知のギャップはカウントされません。
- **Error** はチューナーのデモジュレータが立てる transport_error_indicator の
  実受信エラーです。BS/CS でこの値が継続的に増える場合は、アンテナレベル・
  ケーブル・LNB給電など信号品質側の確認を推奨します。

### 5. 番組表 (「番組表」タブ)

収集済みの EIT から番組表を組み立てて表示します (データの出どころと収集の仕組みは
`docs/EPG_DESIGN.md`、API は `GET /api/programs?since=&until=&nid=&sid=`)。

**列にするのは、表示日に番組を持つサービスだけです** (2026-09)。
列の元は `channels` ですが、`channels` には全国のスキャン結果 (受信できない地域の局を含む)
が入っているため、そのまま列にすると NID 昇順の先頭が番組のない局 (九州など) で埋まります。
番組は見えている列の分しか取得しないので、先頭が全部空だと取得結果が 0 件になり、
空表示がグリッドごと隠して横スクロールもできず、番組のある局へ永久に
到達しませんでした (本番で実際に起きた)。表示日の窓で `GET /api/programs/services`
を先に呼び、番組のある `nid:sid` との積集合を列にします (EDCB の EpgTimer が
`EpgViewBase.cs` で「表示対象サービス ∩ EPG データのあるサービス」を列にするのと同じ)。
収集中に初めて番組が届いた局は、SSE の番組イベントで列に加わります。
地域プルダウンの選択肢も番組のある地域だけになります。

列の並びは、地上波・BS・CS・その他の順です。地上波は都道府県コード、EDCBと同じ
チャンネル番号、NID、SIDの順で並びます。地上波のチャンネル番号はリモコン番号が
1〜12なら「リモコン番号×10＋同一NID内の枝番」、リモコン番号がない場合はSIDの
下3桁です。BS/CSはSIDを使います。初期地域は「設定 > 番組表」で自動（番組のある
地域の都道府県コード最小）、すべて、または特定地域から選べます。

サブチャンネルは、親と同じ開始時刻・番組名の重複を除き、実番組を放送している間だけ
既定で表示します。「番組未定」「放送休止」「休止中」「ご覧ください」を含む名前や
空の名前はプレースホルダーとして表示根拠にしません。「サブCH」をオンにすると、
重複セルを含めて全サービスを表示します。この判定はKomorebi/EDCB互換の親子群規則に
基づきます。

本番では放送局が 584 サービス (226 列)、当日の番組が 2 万件を超えるため、
**縦 (時間方向) と横 (放送局方向) の両方で、画面に入る範囲だけを DOM へ出します**。
列は CSS グリッドではなく絶対配置で、スクロールに応じて描画範囲を入れ替えます
(可視範囲の更新はスクロール量が範囲の半分を超えたときだけ行い、
`requestAnimationFrame` へ集約しています)。
描画の密度はユーザーが選びます (2026-09)。列幅と 1 分あたりの高さは定数ではなく、
**スクロール領域の実測サイズから選択された「局数」「時間幅」で割って求めます**
(`columnWidth = (実測幅 - 時刻軸幅) / 局数`、`pxPerMin = (実測高 - ヘッダー高) / (時間幅 × 60)`)。
実測は `ResizeObserver` で追従します (`window.innerWidth` だけを見ると、
サイドバーの開閉やスクロールバーの有無に追従しません)。
グリッド自体は選択に関わらず 24 時間ぶんを保ち、1 画面に収まる範囲だけが変わります。
先読みの範囲は端末で切り替わります (デスクトップ 2 時間 / スマートフォン 3 時間)。

**表示する日付は「放送日」です。** `GRID_START_HOUR` (6 時) を基準に、
ある日付の番組表はその日の 06:00 から翌日 06:00 までを指します。したがって
深夜 0〜6 時は前日の放送日を選びます (`useGuideDate.ts` の `broadcastDateInput`)。
暦の今日をそのまま使うと、深夜に開いたとき番組表の開始が現在時刻より先になり、
取得窓が潰れて番組がほとんど読めなくなります (実際に起きていた)。

**列に出すのはメインのサービスだけです。** 同じ局のサブチャンネル
(ＮＨＫ総合２、ＴＢＣテレビ２ など) や service_type が NULL の疑似サービスまで
列にすると、1 局あたりの幅が半分以下になります。畳む単位は
**`(network_id, tsid, remote_control_key)`** で、グループごとに最小 SID を残します
(`useGuideServices.ts`)。**`tsid` だけで束ねてはいけません** — CS は 1 つの TS に
独立した別局が最大 8 つ載るため、それらが 1 局に潰れます。同じ局のサブチャンネルは
リモコン番号が一致し (ＢＳ朝日１/２/３ はいずれも 5)、CS の各局は固有の値を持つので
これで正しく分かれます。リモコン番号を持たないサービスは束ねず独立させます
(根拠のない値で束ねると別局が消えるため)。マルチ編成を見るための
「サブCH」トグルをツールバーに置いており、オンにすると全サービスを列にします。

画面はテレビの電子番組表に寄せた高密度レイアウトです (2026-09)。

- **画面の大半を番組表に使う。** 番組表タブのときだけ `main` に `main-guide` クラスが付き、
  余白が 8px (狭幅 6px) になります。見出しと説明文は廃し、日付操作・放送種別・地域・
  検索・更新を高さ 30px 前後の 1 本のツールバー (`.guide-topbar`) にまとめています。
  1280x720 で番組表がビューポート高の約 78%、1920x1080 で約 86% を占めます。
- **高さは固定 px ではなくフレックスで配り、ページ自体はスクロールさせません** (2026-09)。
  番組表タブのときは `.app` に `app-guide` が付き、シェル全体を `100dvh` に固定した
  縦フレックスにします (トップバー → `.layout` → `main.main-guide` → `.guide-view` →
  `.guide-scroll`)。以前は `.guide-view` を `calc(100dvh - シェルの推定高)` にしていたため、
  シェルの実高 (更新通知の有無、スマホ横向きでのデスクトップ配置) とずれるとページが
  はみ出し、iPhone で指の慣性がページ側へ抜けて番組表ごと上へ流れ、局名ヘッダーが
  画面外へ消えていました。狭幅の `.layout` は通常 `display: block` ですが、番組表の
  ときだけ flex にします (block のままだと `main` が中身の高さまで伸び、
  `.guide-scroll` がスクロールコンテナでなくなります)。
- **狭幅 (700px 以下) とスマホ横向き (高さ 500px 以下) では、表示設定と絞り込みを畳みます。**
  局数・時間幅・サブCH・地域・検索は「表示設定」ボタンの中です (既定から外れた絞り込みが
  あるときはボタンに ● が付きます)。1 行目 = 日付操作・更新、2 行目 = 放送種別・表示設定。
  iPhone 16 Pro 縦でツールバーが約 180px → 90px になり、番組表の高さは 367px → 455px、
  横向きではトップバーも詰めて 152px → 262px になりました。
  狭幅のトップバーは高さを固定せず、長いバージョン文字列は省略表示・副題は非表示です
  (開発版のバージョンが折り返して鍵/テーマのボタンがはみ出していた)。
- **表示密度の切替** (`.guide-density-button`) をツールバーに置いています。
  局数は 5 / 7 / 9、時間幅は 3 / 4 / 6 時間。選択は `localStorage` の `guide:density`
  (`{"channels":7,"hours":4}`) に保存し、読み書きは try/catch で囲んで、
  値が無い・壊れている場合は既定へ落とします。
  **狭幅 (700px 以下) では 9 局を選択肢から外します** (390px で 9 局にすると
  1 列 40px となり局名が読めません)。ユーザーがまだ選んでいない間の既定は
  幅で切り替わり、広幅 7 局 / 狭幅 5 局です。明示的に選んだあとは幅で上書きしません
  (狭幅で 9 局が保存されている場合だけ 1 段落とします)。
- **チャンネルヘッダー**はリモコン番号 (`remote_control_key`) のバッジ + ロゴ + 局名の
  2 段組 (上段 = 番号 + ロゴ、下段 = 局名。Komorebi の番組表と同じ構成) で、狭幅でも
  ロゴを小さくして残します。EPG 取得状態の点 (●) は右上に絶対配置で重ね、
  タップ領域は `::before` で広げます。以前はフローに置いた 32px のボタンが上段を
  押し広げ、狭幅 (ヘッダー 48px) では局名が高さ 3px に潰れて見えませんでした。ヘッダー行・時刻軸・その交点 (`.guide-corner`) はいずれも sticky です。
  局ごとに色が付きます (下端 4px のラインとヘッダー背景への混色)。色はリモコン番号
  1〜12 に対応する `--guide-ch-1`〜`--guide-ch-12` から選び、リモコン番号を持たない
  BS/CS は `(nid, sid)` から決定的に割り当てます (同じ局は常に同じ色)。
- **番組セル**は開始分を左に張り出す「ぶら下げインデント」で、番組名を主・説明文を
  従 (弱い色) にしています。左端の 4px がジャンル色、セル背景がその淡色です。
- **4 つの状態を別々の表現にしています。** マウスオーバー = 浮き上がり (影)、
  フォーカス = アクセント色の 2px 枠、選択中 = `--guide-select` の 3px 枠、
  放送中 = アクセント色の内側リング + 開始分の前の丸印。
  選択枠はフォーカス枠より後・高い詳細度で書くこと (同じ `outline` を奪い合うため、
  先に置くとクリック直後に選択が見えなくなる)。
  放送中は丸印も出すので、色だけの区別になっていません。
- **現在時刻**は赤い横線 + 時刻チップ (`.guide-now-badge`) で示します。
  現在時刻ラインと放送中セルの強調は役割が別です。
- **キーボード操作**: `.guide-scroll` の keydown で、矢印キーで番組セル間を移動、
  Enter で詳細、Escape で詳細 → プレビュー → 選択解除の順に閉じます。
  リスナーは `window` ではなくグリッドに付けており、検索欄の矢印キーは奪いません。
- **選択中の番組の詳細はポップアップ** (`.guide-program-popup`) に出します。
  常設の右ペインは廃止しました (画面幅を常時奪うため。その幅は番組表本体へ回しています)。
  ホバー・フォーカス・矢印キーでの移動で開き、**狭幅ではタップで開きます**
  (スマートフォンにホバーは無いため、hover 前提の操作にしていません)。
  ポップアップは **表示中の 1 つだけを DOM に置きます** (全セルぶん置くと、
  可視範囲だけを DOM へ出すという番組表の前提が崩れます)。
  位置はアンカーの矩形から求め、画面端では反転してビューポート内へクランプします。
  スクロールしても閉じず**位置を追従**します (アンカーが可視範囲から外れたときだけ閉じる)。
  再描画でアンカーの DOM が差し替わるため、`data-program-key` から引き直します。
  **番組取得による再描画で閉じてはいけません** — スクロール由来の close をそのまま
  残していたため、番組が読み込まれた瞬間にポップアップが消える不具合が実際に起きました。
  **セルのクリックは「選択」だけを行い、ダイアログは開きません**
  (ポップアップに出るものをモーダルで二重に見せないため)。ダイアログは Enter か
  ポップアップ内の「番組詳細」で開きます。「視聴」も同じ場所です。
  番組を選ばなくても押せる「現在時刻へ」はツールバーに置いています。
- **番組の取得は「可視サービス × 時間窓」で管理します。** 読み込み済みをサービス単位で
  持ち、可視サービスのうちその窓を未取得のものだけを `/programs` へ問い合わせ、
  成功したら**そのとき問い合わせたサービスにだけ**窓を記録します。時間窓だけで
  管理すると、初回に数列ぶんを取った時点でその窓が読み込み済みになり、横スクロールで
  現れた局が永久に空のままになります (実際に起きていた)。
  取りに行く窓は**可視範囲から求めます**。グリッドの端からの距離で判定すると、
  初回に読んだ範囲より先へスクロールしても終端に近づくまで発火せず、やはり空のままです
  (実際に起きていた)。窓の境界は `PROGRAM_WINDOW_STEP_SECS` へスナップします
  (スクロール位置ごとに半端な窓を投げると読み込み済みの一覧に溜まり続け、
  重複判定が効かなくなります)。計算は `useGuideProgramWindow.ts` の純粋関数です。
  **全サービスぶんを一度に取りに行ってはいけません** (本番は当日だけで番組が 2 万件超)。
- **番組の取得は「見えている局 × 見えている時間帯」単位です** (`loadVisiblePrograms`)。
  スクロールに加え、**列の顔ぶれが変わったとき (帯域タブ・地域・検索・サブCH・局数)** にも
  走り、取得済みの局は飛ばします。以前はスクロールでしか走らず、帯域タブを押すと
  新しく並んだ局が、指でスクロールするまで空のままでした。応答待ちの範囲も取得済みと
  みなして二重に要求せず、日付変更などで `loadPrograms` が走ったら古い応答は捨てます。
- **説明文は選択時に 1 番組ぶんだけ後から取ります。** 一覧の行には説明文が入っていない
  ためで、矢印キーの押しっぱなしで通過した番組ぶん要求が飛ばないよう、
  250ms 止まってから引きます (`loadProgramDetail`)。

番組表は `GET /api/epg/events` のSSEも購読する。`program` を受け取ると、
`(nid, tsid, sid, event_id)` をキーにしたMapへ反映し、300ms単位で差分描画する。現行の送信
フレームのpayload `type` は `update` である（サーバー側のenumには `create` / `update` /
`delete` が定義されている）。`ping` は15秒ごと、`epg_status` は30秒ごとに送られる。
`lagged` を受け取った場合はskipped件数を使って全件再取得する。認証は `/api/programs` と
同じである。

SSE接続はレスポンスの `Content-Type` が `text/event-stream` の場合だけ有効な接続として
扱う。1フレームも受信せず5秒未満で切断した場合は再接続バックオフを初期値へ戻さない。
SSEを通さないリバースプロキシや非ストリームの200応答で、接続・即切断・1秒後の全件再取得を
繰り返さないためである。1フレームを受信した場合、または5秒以上接続が続いた場合は安定した
接続としてバックオフを戻す。

### GET /api/epg/status

EPGの取得状態、coverage、延期・失敗理由、CPU情報を返す。既存のレスポンスフィールドに加え、
`dropped_program_rows` がboundedなEPG行キューの満杯で破棄した累計行数を示す。

チャンネル列ヘッダにはEPG取得状態のドットを表示する。状態は取得中、取得済み、一部取得、
古い、未取得、失敗で、番組表を見ながら対象muxの取得状況を把握できる。

番組一覧だけがテレビ風で、番組詳細ダイアログとブラウザプレビュー (PreviewPlayer) は
従来どおりの Web UI のままです。

**番組表の検証**は `cd web-ui && npm run qa:guide` (`scripts/guide-verify.mjs`)。
本番相当のモック EPG (70 サービス / 約 2800 番組) を実ブラウザへ食わせて、
仮想化が効いていること・sticky・4 状態・キーボード操作・放送種別フィルタ・
表示密度の切替 (局数を変えると列が増える / 時間幅を変えると 1 時間の高さが変わる /
localStorage に残る / 狭幅で 9 局が出ない)・クリックでポップアップが 1 つだけ出て
番組名と時刻が入っていること・狭幅でポップアップがビューポートからはみ出さないこと・
詳細とプレビュー・ダークモード・1920/1280/768/390/852x393 (スマホ横向き) での横スクロール無し・
ページ自体の縦スクロール無し・局名ヘッダーが潰れていないことを確認します。
`npm run qa:responsive` と併せて、UI を変更したら流してください。

> `index.html` は `/static/vue/assets/*` を絶対パスで読むため、両スクリプトは
> ビルド成果物をローカル HTTP で配ってから開きます。かつて `file://` で開いており、
> CORS でスクリプトごと読めないまま空ページの幅を測って「合格」していました。

実サーバー(実DB)に対する目視確認は
`node scripts/local-check.mjs <origin> [YYYY-MM-DD]` で、1920/1280/390 と
ダークモードのスクリーンショットを `web-ui/test-results/local/` へ出します。
モックでは出ない実データ固有の崩れ(サブチャンネルと重なって列が半分幅になる、
局名が長い、EPG が歯抜けで `name` が null など)はこちらでしか見つかりません。

### 6. ログ閲覧 (「ログ」タブ)

サーバーの直近ログ(最大5000行、インメモリのリングバッファ)をブラウザから閲覧できます。
`logs/recisdb-proxy.log.*` を直接 tail する必要はありません。

- **種別切り替え(すべて / サーバー / アクセス)**: HTTP アクセスログ
  (`web/mod.rs` の `access_log` ミドルウェアが出す `192.0.2.10:65290 "GET
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

### GET /api/epg/events

番組表のリアルタイム差分用Server-Sent Eventsエンドポイント。`program`、`ping`、`lagged`、
`epg_status`を配信する。`program`のpayloadは番組データと`type`を含み、番組を識別する
`nid`、`tsid`、`sid`、`event_id`を持つ。現行の送信値は`type: update`である。`ping`は15秒間隔、
`epg_status`は30秒間隔で、`lagged`には取りこぼした件数を含む。クライアントは`lagged`時に
番組表を全件再取得する。`/api/programs` と同じBearer認証を使う。

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

EPG状態APIは全体サマリに加えて、`network_id`/`tsid`ごとのcoverage、最終取得時刻、
失敗状態を返す。`reasons`は理由codeと系統の表示名（解決不能時はNID/TSID）、最終使用
チューナー/ノード、同じcodeの系統数を返す。全体の番組情報表示は最もcoverageが薄い系統を
基準にする。CPU負荷の取得元と、取得不能時に制限を無効化していることもAPIで判別できる。

チャンネル編集のチューナー候補は `GET /api/channels/:id/candidate-tuners` で取得する。
Backendがchannelの帯域と有効な受信実績から候補を判定し、候補なしの場合は理由コードを返す。

以下はすべて実装済みです:

- クライアント毎の Drop/Scramble/Error 統計表示 (概要タブのクライアント一覧)
- 配信ストリーム品質の可視化 — ビットレート・パケットロス・信号レベルのスパークライン
- リモートからの強制切断・優先度/排他の上書き (クライアント行の操作)
- セッション履歴タブ
- アラートルール設定と Webhook 通知 (アラートタブ)
