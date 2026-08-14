# EPGStation 互換 — クライアント側仕様の調査台帳

Mirakurun 互換 API (`/mirakurun/api/*`, `web/mirakurun.rs`) の接続先クライアントとして
**EPGStation (stuayu フォーク)** を想定したときに、クライアント側が何を要求するかを記録する台帳。

- 調査対象: `/Users/ayumu/prog/EPGStation` (stuayu フォーク, main)
- クライアントライブラリ: npm `mirakurun` (`git+https://github.com/stuayu/Mirakurun.git#4.2.0-stuayu`)
- **本家 Mirakurun とフォークで型が違う箇所がある**ため、本家のドキュメントだけを見て実装すると動かない (後述の `Service.channel`)
- **フォーク自身の同梱ドキュメント (`api.yml` = `/docs` の中身) と実装が食い違っていた** (後述 §1)。
  上流 (`/Users/ayumu/prog/Mirakurun`) では 2026-08-09 に `api.yml` を実装側 (配列) へ合わせて修正済み (未コミット) だが、
  EPGStation が同梱している版には未反映。**同梱の `api.yml` を鵜呑みにせず、実装と `api.d.ts` で裏を取ること**
- 初回調査日: 2026-08-08 (最終更新: 2026-08-09) / 静的解析のみ (実起動での疎通は未実施)。
  §1・§3・§4・§6 に記す EPGStation 側の改修は、EPGStation リポジトリの作業ツリーに**未コミットの差分として実装は完了している**
  (2026-08-09 時点。まだ正式リリースには入っていない)
- **2026-08-09: EPGStation を動かすのに必要な範囲の Mirakurun 互換 API を一通り実装した** —
  `GET /docs`・`/programs/{id}/stream`・`/events/stream`・`/tuners`・`/config/server`、および
  `Service.channel` の配列化。差分は §6、実装にあたって新たに判明したクライアント側の要求は §1 と §5.1。
  **ただし実起動での疎通確認はまだ行っていない** (§7)

**記録の義務**: EPGStation 側の仕様を新たに調べたとき、および Mirakurun 互換 API を追加・変更したときは、
同じ作業の中でこのファイルを更新すること (CLAUDE.md「ドキュメント」節を参照)。
「調べたが実装しなかった」ことも判断の根拠として残す。

---

## 1. クライアントの呼び出し機構 — `GET /docs` が全ての前提

npm `mirakurun` のクライアントは、すべての API 呼び出しを `call(operationId, param)` 経由で行う。
`call()` は初回に **`GET {basePath}/docs` を取得して OpenAPI 定義を読み、operationId から
HTTP メソッド・パス・パラメータの位置 (path / query / header / body) を解決する**。

- 解決処理: `node_modules/mirakurun/lib/client.js:116-182`
- docs 取得: 同 `481-485` (`this.docsPath = "/docs"`, `client.js:57`)
- 見つからない場合: `operationId "..." is not found.` を throw

つまり **`/docs` が OpenAPI JSON を返さない限り、番組表取得もストリームも 1 つも動かない**。
パスを個別に実装するだけでは不十分で、`operationId` を含む OpenAPI 定義の提供が必須。

`/docs` の取得自体はクライアントライブラリ内部で `call()` の初回実行時に遅延して行われ、EPGStation 側の
`ConnectionCheckModel.checkMirakurun()` (`src/model/ConnectionCheckModel.ts:32-56`) が使う疎通確認は `GET /status`
(`getStatus`) であって `/docs` ではない。そのため **`/docs` の取得に失敗しても起動時の疎通確認では検出されず、
実際に何らかの API を初めて呼んだ瞬間に `operationId "..." is not found.` の例外として現れる**。

> **EPGStation 側の対応状況 (2026-08-09 時点、実装完了・未コミット)**: `/docs` が取得できない場合に原因を切り分けるログが入った。
> `src/model/ConnectionCheckModel.ts` の疎通確認 (`checkMirakurun()`) が `getStatus()` の失敗を検出した際、
> それが `operationId "..." is not found.` というメッセージパターン、または docs エンドポイント自体が無いと
> 判断できる 404 / 501 であれば、`getDocs()` を明示的に呼び直して
> 「docs 自体が取得できないのか」「docs は取得できたが内容が Mirakurun と一致しないのか」を切り分け、warn ログに出す
> (`logDocsResolutionHintIfNeeded()`)。ただしこれは**疎通確認 (`GET /status`) の失敗時にだけ働く**ログの改善であり、
> `/docs` の取得タイミング自体 (`call()` の初回実行時に遅延して行われる、上記の説明) は変わっていない。

EPGStation が使う operationId は次のとおり (定義に含める必要がある):

| operationId | 実際のパス (本家 Mirakurun) |
| --- | --- |
| `getServices` | `GET /api/services` |
| `getPrograms` | `GET /api/programs` |
| `getServiceStream` | `GET /api/services/{id}/stream` |
| `getProgramStream` | `GET /api/programs/{id}/stream` |
| `getEventsStream` | `GET /api/events/stream` |
| `getTuners` | `GET /api/tuners` |
| `getStatus` | `GET /api/status` |
| `getServerConfig` | `GET /api/config/server` |
| `getLogoImage` | `GET /api/services/{id}/logo` |

`getChannels` / `getChannelStream` などクライアントが持つ他のメソッドは EPGStation からは呼ばれない。

### `/docs` の中身が満たすべき条件 (client.js の実装から確定。2026-08-09 追記)

`/docs` を実装するにあたって `client.js` を読み直した結果、**単に OpenAPI らしい JSON を返すだけでは動かない**
ことが分かった。以下はいずれも守らないと壊れる。

- **`Content-Type` は `application/json` でなければならない** (`client.js:84`)。それ以外だとレスポンスボディは
  `Buffer` のまま渡され、`this._docs.paths` が `undefined` になって全 API が落ちる
- **すべてのパスオブジェクトに `parameters` 配列を置く** (空でも `"parameters": []`)。`call()` は
  operationId が一致するかを判定する**前に** `[...p.parameters, ...(p.get.parameters || [])]` を評価するため
  (`client.js:127-140`)、無関係なパスに `parameters` が無いだけで、他の operationId の解決中に TypeError になる
- **すべての operation に `tags` 配列を置く**。`call()` は `operation.tags.indexOf("stream")` で
  ストリーム応答か否かを分岐する (`client.js:176`)。**`getServiceStream` / `getProgramStream` /
  `getEventsStream` / `getChannelStream` には `"stream"` を含める**。逆に、JSON を返すエンドポイントに
  誤って `"stream"` を付けると、応答が `JSON.parse` されずチャンクストリームとして扱われる
- **`paths` のキーは `basePath` を含まない相対パス** (`/services` など)。リクエストパスは
  `this.basePath + path` で組み立てられる (`client.js:407`) が、その `basePath` は
  EPGStation 側の設定 (§2) から来るもので、`/docs` 内の `basePath` 宣言は routing には使われない
- `required: true` のパラメータをクライアントが渡さないと、リクエスト送信前に例外になる (`client.js:145-149`)。
  クライアントが送るとは限らないものを `required` にしない

### operationId は同梱 `api.yml` からは取れない (2026-08-09 追記)

EPGStation 同梱の `node_modules/mirakurun/api.yml` は **`paths: {}` が空**で、operationId は 1 つも書かれていない
(`definitions` だけが入っている)。§1 冒頭で「`api.yml` を単独の根拠にするな」と書いた理由がここでも当てはまる。

実際の operationId は **`node_modules/mirakurun/lib/Mirakurun/api/**/*.js` の `apiDoc.operationId`** にあり、
本プロジェクトの `/docs` はここから採った。この過程で、想像で付けた名前と本家の名前が食い違う箇所が見つかっている:

- `GET /version` → **`checkVersion`** (`getVersion` ではない)
- `GET /channels/{type}/{channel}/stream` → **`getChannelStream`** (`getServiceStreamByChannel` は
  `GET /channels/{type}/{channel}/services/{sid}/stream` の方)

### `/docs` の内容を鵜呑みにしてはいけない例 (stuayu 版 Mirakurun 自身のバグ → 上流で修正済み)

`node_modules/mirakurun/lib/Mirakurun/ServiceItem.js:126` の `toItem()` は `switch (this._channel[0].type)`
のように **`channel` を配列として実装している** (= 実際に `GET /api/services` が返す `Service.channel` は配列)。
サーバー側の `export()` (`src/Mirakurun/ServiceItem.ts:138-153`) も `channel: this._channel` (= `ChannelItem[]`) を
そのまま入れている。

ところが同梱の `node_modules/mirakurun/api.yml:166-167` (`GET /api/docs` がそのまま返す OpenAPI 定義) は

```yaml
channel:
  $ref: '#/definitions/Channel'
```

と**単数の `Channel` のまま**で、実装と食い違っていた。EPGStation 側は `node_modules/mirakurun/api.d.ts:65`
(`channel?: Channel[];`) だけが配列に直っており、**同梱の `api.yml` (= `/docs` が返す定義) は直っていなかった**。

> **上流 (stuayu/Mirakurun) の対応状況 (2026-08-09 時点、実装完了・未コミット)**: `/Users/ayumu/prog/Mirakurun` の
> `api.yml` を修正し、`Service.channel` を
>
> ```yaml
> channel:
>   type: array
>   items:
>     $ref: '#/definitions/Channel'
> ```
>
> と配列に直した (作業ツリーの未コミット差分)。あわせて `api.yml` 全体を `api.d.ts` / 実装と突き合わせ、
> `ChannelType` の `GR` / `BS` / `CS` / `SKY` + `NW1`〜`NW40`、`Program` の
> `video` / `audios` / `extended` / `relatedItems` / `series` / `genres` は不整合なしと確認した
> (`ConfigChannelsItem` や `streamSetting.channel` は元から単数で正しい)。
> **したがって、これが EPGStation 側の `node_modules/mirakurun` に反映されたあとの `/docs` は配列を宣言する。**
> 反映前の版 (現在 EPGStation が同梱している版) は依然として単数を宣言している点に注意。

**互換実装を作る側にとっての意味**: §4 で述べるとおり EPGStation 側は `Service.channel` を配列・単一オブジェクトの
どちらでも受け付けるようになったため、`Service.channel` の解釈自体はどちらの形でも壊れない見込みである。
ただし **`/docs` の定義と実レスポンスの形は一致させる必要がある**点は変わらない
(定義が配列なのに実レスポンスが単一オブジェクト、あるいはその逆だと、EPGStation 以外のクライアントも含めて
クライアント側の型解釈と実データが食い違いになる)。
そのうえで、**上流の実装・`api.d.ts`・修正後の `api.yml` がすべて配列で揃った**ため、本プロジェクトも
**配列で返し、`/docs` でも配列と宣言する**のが本家互換として素直な選択になった (§6 の着手順を参照)。

## 2. ベース URL の決まり方

`src/model/MirakurunClientModel.ts` が `config.yml` の 2 項目からクライアントを組み立てる。

- `basePath = path.posix.join(new URL(mirakurunPath).pathname, mirakurunAPIPath ?? クライアント既定値)`
- named pipe (`\\.\pipe\...`) と Unix socket (`http+unix://` / 旧 `http://unix:`) にも対応する

したがって本プロジェクトの `/mirakurun/api` というマウント位置は、EPGStation 側の設定で吸収できる。

```yaml
mirakurunPath: 'http://<host>:<port>/mirakurun'
mirakurunAPIPath: '/api'
```

加えて `recisdb-proxy.toml` の `[mirakurun] enabled = true` が必要 (既定 `false`)。

## 3. EPGStation が呼ぶ API と、無い場合に壊れるもの

### 必須 (無いと起動・録画が成立しない)

- **`GET /docs`** — 上記のとおり全 API の前提
- **`GET /services`** (`getServices`) — 放送局一覧。`EPGUpdateManageModel.ts:190` が定期取得し `ChannelDB.insert()` へ渡す
- **`GET /programs`** (`getPrograms`) — 番組表。`EPGUpdateManageModel.ts:111`
- **`GET /programs/{id}/stream`** (`getProgramStream`) — **programId 予約の録画で使う**。`RecordingStreamCreator.ts:248` が `{ id, decode, signal }` で呼ぶ。EPG 予約の録画はすべてこの経路
- **`GET /services/{id}/stream`** (`getServiceStream`) — 時刻指定予約 (`RecordingStreamCreator.ts:279`)、ライブ視聴 (`LiveStreamBaseModel.ts:290`)、データ放送 (`DataBroadcastingManageModel.ts:147`)

### 推奨 (無いと機能が落ちる)

- **`GET /config/server`** (`getServerConfig`) — **実装済み (2026-08-09)**。下記のとおり EPGStation 側の
  `config.yml` に `tunerServerType: mirakurun` を書けば回避もできるが、実装したので `auto` のままでも
  Mirakurun と判定される。以下は当時「実装しない」と判断した経緯の記録として残す
  > **EPGStation 側の対応状況 (2026-08-09 時点、実装完了・未コミット)**: `checkTunerServerType()`
  > (`src/model/epgUpdater/EPGUpdateManageModel.ts`) は `config.yml` の `tunerServerType` (`mirakurun` / `mirakc` / `auto`、
  > 既定は未指定 = `auto`) を先に見るようになった。`mirakurun` / `mirakc` が明示されていれば `getServerConfig()` を
  > 一切呼ばずに種別を確定する。**したがって recisdb-proxy 側が `/config/server` を実装しなくても、
  > 接続先の EPGStation の `config.yml` に `tunerServerType: mirakurun` と書けば Mirakurun として扱わせられる**
  > (この設定は EPGStation 側のファイルであり本プロジェクトが用意するものではないが、検証・運用の回避策として使える)。
  > `auto` の場合の判定ロジックも改善されており、`getServerConfig()` の失敗が `operationId ... is not found` や
  > 404 / 501 (エンドポイントが無いと判断できる応答) なら mirakc と判定してキャッシュする一方、
  > 接続不能や 5xx などの一時的な失敗は判定を確定させずキャッシュしない (次回呼び出しで再判定する) ようになった。
  > 未実装のまま `auto` で待ち受ける場合、明確な 404 を返すほうが「一時的な失敗」と誤認されず速く mirakc 判定される
- **`GET /events/stream`** (`getEventsStream`) — EPG の差分更新。無いと定期全取得のみになり、EIT[p/f] の即時反映と予約の追従 (`updateOnAirProgram`) が効かず、番組の延長・繰り上げの反映が最大 `epgUpdateIntervalTime` 遅れる。
  応答は JSON 配列を逐次流す形式で、クライアントは受信バッファの末尾 3 文字を落として `[...]` として `JSON.parse` する (`EPGUpdateManageModel.ts:339`)
- **`GET /tuners`** (`getTuners`) — `GET /api/status` のチューナー情報表示
- **`GET /status`** (`getStatus`) — 疎通確認 (`ConnectionCheckModel.ts:37`)。タイムアウト付きで叩かれる

### 実害が小さいもの

- `GET /services/{id}/logo` (`getLogoImage`) — `ChannelApiModel.ts:101`。実装済み (2026-08-14)。ロゴ収集器 (`tuner/logo_collector.rs`) が CDT から取り出して `logos/<nid>_<sid>.png` へ保存したものを返す。EPGStation は `hasLogoData` が `true` のサービスにしかロゴを要求しないため、まだ一度も選局していない局は従来どおり要求されない

## 4. データ構造の要求

### `Service.channel` は配列・単一オブジェクトのどちらでも EPGStation 側が受け付けるようになった

`node_modules/mirakurun/api.d.ts:65` の型定義自体は `channel?: Channel[]` (配列) のままだが、
実際に DB へ変換する `ChannelDB.createInsertValue()` (`src/model/db/ChannelDB.ts`) は
**配列でも単一オブジェクトでも受け付ける**ように改修された (`Array.isArray()` で分岐し、
単一オブジェクトの場合は型アサーション経由でそのまま使う)。`Service.channel` を
単一オブジェクトのまま返す実装でも、放送局登録が壊れなくなった見込みである。
ただし本家 (および stuayu フォーク) の**実装が返す形は配列**であり、上流の `api.yml` も配列に修正された (§1) ため、
本家互換として素直なのは配列で返すほうである。EPGStation の両対応はあくまで保険と考えること。

**旧挙動 (2026-08-08 時点、修正前)**: `insert()` は Mirakurun から受け取った `Service[]` を
DB 挿入用データへ変換するループを 1 つ回すが、このループ**全体**が 1 つの try/catch に包まれていた
(`src/model/db/ChannelDB.ts` 旧実装 43-70 行相当)。単一オブジェクトを返す実装では**先頭のサービスの変換で** `[0]` が
`undefined` になって例外が起き、**その時点でループが止まるため、例外が起きたサービス以降が変換されずに未登録のまま処理が続く**。
出力されるのはエラーログ 1 行のみ。先頭サービスで必ず例外になる実装では結果的に 0 件になるが、
「常に 0 件になる」が正しいのではなく「**例外が起きた位置以降の放送局が抜け落ちる**」が正確な挙動だった。

> **EPGStation 側の対応状況 (2026-08-09 時点、実装完了・未コミット)**: `src/model/db/ChannelDB.ts` の修正は完了しており、
> 作業ツリーには未コミットの差分として反映済みである。変換ループの try/catch を **1 サービス単位** に移し、
> 1 件の失敗が以降のサービスを巻き込まないようにしたうえで、上記のとおり `createInsertValue()` が配列・単一オブジェクトの
> 両方を受け付けるようにした。テストは `test/ut/channel-db.test.js` (配列・単一オブジェクト双方の変換、1 件失敗時に
> 以降のサービスが処理されることを検証)。ただし本稿執筆時点ではまだ未コミットで、正式版には入っていない。
> **実起動での疎通確認 (実際に本家 Mirakurun や mirakc に接続して確認すること) はまだ行われていない**。

参照されるフィールド:

- `id` (= `networkId * 100000 + serviceId`)、`serviceId`、`networkId`、`name`
- `channel[0].type` / `channel[0].channel`
- `remoteControlKeyId` (未定義なら null。地上波の並び順に使う)
- `hasLogoData`
- `type` (ARIB のサービス種別。数値でなければ null 扱い)

`channel[0].type` は `channelTypeId` の解決に使われるため、EPGStation 側が知らない値だと放送局を分類できない。
**このフォークは `GR` / `BS` / `CS` / `SKY` に加えて `NW1`〜`NW40` (県外地上波) を持つ**。

### `Program`

`api.d.ts:68-` の `Program`。EPGStation が参照する主なもの:

- `id` / `eventId` / `serviceId` / `networkId` / `startAt` (ミリ秒) / `duration` (ミリ秒) / `isFree`
- `name` / `description` / `extended` / `genres`
- `video` / `audio` (または `audios`) — 録画時のメタデータに入る
- `relatedItems` — **イベントリレー処理で参照する** (`EPGUpdateManageModel.ts:145-171`)

**放送時間未定の番組**: ARIB の `duration = 0xFFFFFF` を本家 Mirakurun は `duration: 1` として返す。
EPGStation はこれを特別扱いし、暫定の終了時刻 (3 時間) を与えたうえで番組表 API で次番組の開始時刻まで切り詰める (`src/util/ProgramDuration.ts`)。
実際の長さや 0 を返すとこの処理が働かず、番組表と予約が壊れる。

### `ChannelType` に `BS4K` は無い — 4K は `BS` として出す (2026-08-10 追記)

BS4K 対応 (`docs/FOURK_SETUP.md`) を入れるにあたり、4K サービスをどの
`type` で出すべきかを EPGStation 側の実コードで確認した。

**根拠 (EPGStation 同梱物・実コード):**

- `node_modules/mirakurun/api.d.ts:48-52`
  ```
  export type ChannelType = "GR" | "BS" | "CS" | "SKY" |
      "NW1" | ... | "NW40";
  ```
  **`BS4K` は宣言に存在しない。** stuayu フォークが足したのは `NW1`〜`NW40`
  であって 4K 用の型ではない。
- `src/model/db/ChannelDB.ts:169` `getChannelTypeId(type)` は
  `GR`→0 / `BS`→1 / `CS`→2 / `SKY`→3 / `NW1`〜`NW40`→4〜43 の switch で、
  **`default: return 44`**。未知の型でも例外にはならず、単一のその他枠に
  落ちる。

つまり `BS4K` を出しても EPGStation は落ちないが、型宣言の外であり、
チャンネル種別が「その他」に丸められる。

**判断: 出力は `BS` のままにする。** 4K は実運用上ほぼ BS 配信であり、
`BS` なら EPGStation の GUI でも BS グループに正しく並ぶ。

**入力としては `BS4K` を受け付ける** (`web/mirakurun.rs`)。
4K 対応フォークの [MMirakurun](https://github.com/otya128/MMirakurun) は
`BS4K` を実在の ChannelType として定義しているため、それに合わせて書かれた
クライアントが `type=BS4K` で問い合わせたときに空を返さないようにする。
追加分岐なので既存の挙動は変わらない。

**未確認**: EPGStation が 4K サービス (H.265 / 2160p) を録画・再生時に
どう扱うか。型の問題とは別に、エンコード設定側での対応が要る可能性がある。

## 5. ストリームのセマンティクス

EPGStation は録画の開始・終了をストリームの挙動そのもので判定する。パスを生やすだけでは足りない。

- **`programs/{id}/stream` は、対象イベントが EIT[p/f] で present になるまでデータを流さない**。
  EPGStation はこれを「まだ番組が始まっていない」と解釈し、チューナー異常とは別枠で既定 3 時間まで待つ (`RecordingRetryPolicy`)
- **録画の終了はストリームの終了で判定する**。別イベントが present になって Mirakurun 側が閉じることを前提にしており、
  programId 予約では `reserve.endAt` を停止に使わない
- 時刻指定予約は `services/{id}/stream` を使うため予定時刻から即データが流れる。
  EPGStation 側は `RecordingStartGate` が EIT[p/f] present を読んで、予約した番組が始まるまで録画ファイルを作らない
- **優先度は `X-Mirakurun-Priority` ヘッダで送られる** (`recPriority` / `conflictPriority` / `streamingPriority`)。
  無視すると、チューナー競合時に録画がライブ視聴に負ける
- `decode` はクエリで送られる (常時デコード済みで返すなら無視でも実害はない)

### 5.1 `GET /events/stream` のフレーミング (2026-08-09 追記)

**これは NDJSON でも SSE でもない独自形式**で、区切り方を間違えるとクライアント側で無言のまま
バッファに溜まり続ける。EPGStation の受信側実装 (`EPGUpdateManageModel.ts:389-431` と
同ファイル 921-922 行の定数) と、本家の送信側実装 (`Mirakurun/src/Mirakurun/api/events/stream.ts`) の
両方で裏を取った結果、次のとおり:

- 接続直後に **`[\n` (`5b 0a`) を単独のチャンクとして 1 回だけ書く**。クライアントは
  「チャンクが `[\n` と完全一致したら無視する」処理を持っており、他のデータと結合して送ると
  その `[\n` がイベント JSON の一部として扱われてパースに失敗する
- 以降は **1 イベントにつき `JSON.stringify(event) + "\n,\n"`** を書く。クライアントは受信バッファ `tmp` の
  末尾 4 バイトが `}\n,\n` (`7d 0a 2c 0a`) になった瞬間に `JSON.parse("[" + tmp.slice(0, -3) + "]")` を実行し、
  `tmp` を空に戻す
- **JSON 本文に生の改行を含めてはいけない** (整形出力にしない)。`serde_json::to_string` は
  文字列中の制御文字をエスケープするので、手書きで JSON を組み立てない限り自動的に満たされる
- **正常系でストリームを閉じてはいけない。** クライアントは `end` / `close` をエラー扱いして
  例外を投げる (`EPGUpdateManageModel.ts:377-386`)。したがって broadcast の `Lagged` でも
  ストリームを終わらせず、ログを出して継続するのが正しい (取りこぼしは EPGStation 側の定期全取得で埋まる)
- イベントの形は `api.d.ts:257` の `Event` = `{ resource, type, data, time }` (`time` はミリ秒)。
  `resource === "program"` の `data` は `GET /programs` の要素と同じ `Program`
- クライアントは `data.name` が `undefined` のイベントを捨てる (`EPGUpdateManageModel.ts:554`) ので、
  名前の無い行は送らない
- 本家は `?resource=` / `?type=` のクエリフィルタを持つ (一致しないイベントを書かないだけ)。
  EPGStation は無引数で呼ぶので必須ではない

**イベント同士が 1 チャンクに結合しても壊れない**点は補足しておく価値がある。`{A}\n,\n{B}\n,\n` が
まとめて届いても、`slice(0, -3)` 後に `[{A}\n,\n{B}]` となり有効な JSON 配列としてパースされる。
壊れるのは**先頭の `[\n` が後続と結合した場合だけ**なので、`[\n` を単独チャンクで送ることが要点になる。

## 5.2 実起動での疎通確認 (2026-08-12)

稼働中のサーバー (`https://fuku-recisdb-web.stuayu.com/mirakurun`) に対し、EPGStation が実際に使う
`mirakurun` npm クライアント (`node_modules/mirakurun`, 4.3.0-stuayu) 経由で全エンドポイントを実行し、
同じ受信環境で動いている本物の Mirakurun (`https://fuku-mirak.stuayu.com`, 4.2.0-stuayu) と応答を突き合わせた。

**疎通したもの** — `getDocs` / `getStatus` / `getServerConfig` / `getServices` / `getChannels` / `getTuners` /
`getPrograms` / `getEventsStream` / `getServiceStream` がすべて成功。`/docs` 経由の operationId 解決 (§1) は
実クライアントで通ることを確認した。`/events/stream` のフレーミング (§5.1) も実際に `[\n` が単独で先行し、
以降 1 イベント 1 チャンクで届いていた。`/programs` は 52,620 件を返し、`(networkId, serviceId)` が
`/services` に無いために EPGStation 側で捨てられる番組は 0 件だった。

**静的解析では見えていなかった不具合** (いずれも本パスで修正済み):

1. **`/services` が同じサービスを何度も返していた** — `channels` テーブルは (BonDriver, サービス) ごとに
   1 行なので、4 台のチューナーで受かるサービスは 4 行ある。本番データでは 770 行 = 実サービス 307 件で、
   181 個の `id` が重複していた。EPGStation は `id` をキーに INSERT → 失敗したら UPDATE する
   (`src/model/db/ChannelDB.ts:88-118`) ので登録自体は通るが、**最後の行が勝つ**ため、SDT 取得前の
   仮名 (`"BS09/TS1"`) や別ドライバのチャンネル表に属する `channel` 文字列が採用されうる。実際に
   51 サービスが「同じ `id` なのに `channel` が食い違う」状態で、`400211` は `BS 8` と `BS 9` の
   両方を名乗っていた。→ `unique_services()` で `(nid, sid)` ごとに 1 行へ畳む
2. **`(type, channel)` が multiplex を一意に指していなかった** — 地上波の `channel` は `physical_ch`
   (無ければ `bon_channel`) をそのまま使っていた。本プロジェクトは複数地域のチューナーを束ねる用途なので
   物理 15ch には 7 つの networkId が乗っており、`GET /channels` は別局のサービスを 1 つの multiplex として
   束ね、`/channels/GR/15/stream` は最初の 1 局しか返せなかった。→ `assign_channel_strings()` が衝突した
   ものだけ `15_32416` 形式へ振り分ける (本番データでは 99 multiplex すべてが一意になり、うち 52 が
   サフィックス付き)
3. **`remoteControlKeyId` を返していなかった** — DB (`channels.remote_control_key`) には入っているのに
   応答に載せておらず、EPGStation の番組表でリモコン番号が使えなかった。本物 Mirakurun は地上波
   (`GR`/`NW*`) の全サービスに付け、BS/CS には付けない。CS110 では同じ列に 3 桁チャンネル番号が入るため、
   地上波の行だけ返す
4. **`/status` の `tunerCount` が実態と違っていた** — `TunerPool` のキー数を数えていたので、14 台構成の
   サーバーが `tunerCount: 1` を返していた (`/tuners` の配列長 14 とも食い違う)。→ `bon_drivers` の行数
5. **`/services/{id}/stream` が multiplex 全体を流していた** — 同じサービスで本物が 21 PID なのに対し
   34 PID (ワンセグ・サブチャンネル・データ放送が同居) を返していた。EPGStation は録画にこの
   エンドポイントを使うので、録画サイズとドロップ判定に直接効く。→ BNDP セッションと同じ
   `TsServiceFilter` を通す。実 TS 5.7MB で検証し、出力は本物の PID 集合の部分集合になった
   (差は本物側にしか無い CAT と、対象サービスの PMT に載っていない PID 1 つ)
6. **サービスフィルタが BIT (PID 0x24) を落としていた** — 5 の実測比較で判明。BIT の `affiliation_id` は
   API からは取れず、EPGStation のフォークは受信したストリームから受動収集する
   (`src/model/channel/BitParser.ts`)。→ `ALWAYS_PASS_PIDS` に追加 (BNDP 経路にも効く)
7. **`/tuners` の `types` が常に `[]` だった** — スキャン済みチャンネルの band から埋められる。
   → `channel_types_by_driver()`

**残った差分** (いずれも既知・§6 の表のとおり): `Program` の `extended`/`video`/`audio`、`isFree` 固定、
ロゴ、`X-Mirakurun-Priority` の反映。`NW1`〜`NW40` もこの時点では未対応だったが、§5.3 で実装した。

## 5.3 `NW1`〜`NW40` (県外地上波) の割り当て (2026-08-12)

§5.2 の比較で、**同じ受信環境の本物 Mirakurun は地上波 573 サービスのうち 21 だけを `GR` とし、
残りを `NW1`〜`NW27` に分けている**ことが分かった。本物では `tuners.yml` に人手で書く定義だが、
proxy 側は全地上波を `GR` に入れていたため、EPGStation の番組表に数百局が 1 タブに並ぶ状態だった。

`[mirakurun] home_region` (都道府県名) を追加した。設定するとその地域の地上波だけが `GR` になり、
他の地域は **地域ID (`channels.region_id`、無ければ networkId から導出) の昇順で `NW1`〜`NW40`** に
割り当てられる (`web/mirakurun.rs::terrestrial_type_map`)。未設定なら従来どおり全地上波が `GR` なので、
単一地域の構成の挙動は変わらない。

- 都道府県名は 1 つの地域IDとは限らない (北海道は 8 個、東京・大阪・愛知は広域と県域の 2 個) ため、
  `recisdb_protocol::broadcast_region::region_ids_from_prefecture_name()` が返す**全ての地域ID**を
  `GR` にする。`home_region = "東京"` なら関東広域 (1) と東京県域 (23) の両方
- `NW40` を超える地域は `GR` へフォールバックする。EPGStation の `ChannelType` は `NW40` までで、
  範囲外の型は `ChannelDB.getChannelTypeId` が catch-all バケット (`src/model/db/ChannelDB.ts:169`) に
  落としてしまうため
- **`NWn` の番号はスキャン結果から決まる**ので、新しい地域を受信すると後ろの地域の番号がずれる。
  ずれても放送局 ID (`networkId * 100000 + serviceId`) は変わらないため、EPGStation の録画・予約・
  ルールは維持され、番組表のタブ位置が変わるだけ
- 地域が分かれると `(type, channel)` の衝突自体が減る (§5.2-2 のサフィックスは `NWn` ごとに
  名前空間が分かれる分だけ不要になる)。`GET /channels/{type}/{channel}/stream` は帯域だけでなく
  **割り当て済みの type も突き合わせて**引く (`GR` と `NW3` はどちらも地上波のため)

本番データ (地上波 194 サービス、15 地域) に `home_region = "福島"` を当てた場合の割り当て:
福島 22 局が `GR`、以降 `NW1` 東京 27 局 / `NW2` 北海道 20 局 / … / `NW14` 新潟 23 局。

## 5.4 `remoteControlKeyId` の欠損と NIT からの補完 (2026-08-14)

**EPGStation 側の仕様**: 番組表・放映中の放送局の並び順は `remoteControlKeyId` 昇順で、
**値が無い局は末尾へ回される** (`src/model/db/ChannelDB.ts:381-385` / `411-415`。
`findChannleTypes()` と `findAll()` の両方が同じ ORDER BY を持つ)。この 2 つは
`ScheduleApiModel` の番組表・放映中の入口 (`src/model/api/schedule/ScheduleApiModel.ts:202` /
`364`) がそのまま使うため、**キーが無い局は番組表でも放映中でも、キー順に並んだ列の後ろに
まとめて置かれる**。クライアント側は地域・系列で絞り込むだけで並び替えないので
(`client/src/model/state/guide/GuideState.ts:245-256`)、サーバーが返した順がそのまま画面に出る。

**本番で起きていたこと**: 地上波 183 局のうち 48 局が `remoteControlKeyId: null` で、
テレ玉・とちぎテレビ・チバテレ・tvk・TOKYO MX・NHK 総合 (東京)・福島の民放 4 局などが
番組表の末尾に固まっていた。該当行はいずれも **CSV インポートまたは `POST /api/channels` で
手動登録した行** で、その 2 経路は `remote_control_key` / `physical_ch` / `network_name` /
`raw_name` を NULL 固定で INSERT する (`web/api/channels.rs`)。スキャン経由の行は NIT の
TS情報記述子からキーが入るため、共有チューナー (BonDriverProxyEx 経由) のようにスキャンを
回していない構成だけが欠損する。

**対応**: 視聴・EPG 収集中の TS から NIT (PID 0x0010) を読み、**NULL の列だけ**埋める。

- `tuner/nit_collector.rs` — `EpgCollector` と同じ形。NIT actual (0x40) / other (0x41) の両方を
  読む (地上波の NIT other は近隣局を各局の `original_network_id` 付きで載せるので、
  手動登録行に足りない情報がここから取れる)。ネットワーク名だけは、そのテーブル自身が
  記述するネットワークのエントリにしか付けない (他ネットワークのエントリに付けると別局の名前になる)
- `nit_writer.rs` — `Database::fill_missing_terrestrial_metadata()` を呼ぶ。`COALESCE` で
  **既存値は上書きしない** (スキャン結果が常に優先)。適用済みの networkId を憶えておき、
  NIT が繰り返し届いても DB ロックを取らない
- **照合は networkId だけ**で `(nid, tsid)` ではない。手動登録行の tsid はプレースホルダ
  (実データでは `tsid == nid`) のことが多く、それこそが直したい行のため。地上波は
  networkId と TS が 1:1 なので別の multiplex を巻き込まない。衛星は 1 つの networkId が
  多数の tsid にまたがるので、収集側で BS/CS のエントリを捨てている
  (BS/CS のキーは TSID/SID から導出する既存経路が持つ)
- 物理チャンネルの導出 (`uhf_channel_from_frequency` / `NitTransportStream::physical_ch()`) は
  `ts_analyzer/nit.rs` に集約し、スキャン経路 (`scheduler/scan_scheduler.rs`) と共有する

**残る制約**: 埋まるのは**一度でも選局した局だけ**。ロゴ (§6) と同じで、受信していない局は
NULL のまま = EPGStation 側では末尾に並ぶ。スキャンを回すか、手動で値を入れる必要がある。

## 5.5 EPGStation の視聴・録画をダッシュボードに出す (2026-08-14)

EPGStation は Mirakurun 互換 API の `GET /services/{id}/stream` (視聴) と
`GET /programs/{id}/stream` (録画) でチューナーを占有するが、**ダッシュボードのクライアント一覧には
一切出ていなかった**。`SessionRegistry` へ登録していたのが BNDP セッション
(`server/listener.rs`) だけだったため。結果として「チューナーは動いているのにクライアントは 0 件」に
見え、録画中かどうかを画面から判断できなかった。

- HTTP 経路 (Mirakurun 互換 API と `web/stream.rs` のダッシュボード用配信) も登録するようにした。
  登録・解除はレスポンスボディの寿命に紐づく RAII (`web/http_session.rs`)
- 行には `protocol` (`bndp` / `http` / `mirakurun`) が付き、UI は「接続方式」列で見分ける。
  `GET /programs/{id}/stream` は録画なので `stream_class` を `record` で登録する
- `POST /api/clients/{id}/disconnect` は EPGStation のストリームにも効く (ボディがそこで終わる)。
  **EPGStation 側は切断を録画失敗として扱う**ので、録画中の行を切るときは承知の上で
- 信号レベル・ドロップ数は 0 のまま。HTTP は共有 broadcast を読むだけで、BNDP のような
  クライアント単位の送信キューが無く、そこでの取りこぼしという概念が無い

## 6. 現状の実装との差分 (2026-08-09 時点、2026-08-12 更新)

`web/mirakurun.rs` / `web/mod.rs:126-136` を読んだ結果 + §5.2 の実起動確認。

| 項目 | 状態 | 影響 |
| --- | --- | --- |
| `GET /docs` | 実装済み (2026-08-09) | `web/mirakurun_docs.rs`。EPGStation が呼ぶ operationId をすべて宣言。詳細と、client.js のパースが課す制約は §1 参照 |
| `Service.channel` の配列化 | 実装済み (2026-08-09) | 1 要素の配列で返し、`/docs` でも配列と宣言している。§1/§4 |
| `GET /programs/{id}/stream` | 実装済み (2026-08-09) | `web/mirakurun.rs::stream_program_by_mirakurun_id` + `web/mirakurun_program_stream.rs::ProgramGate`。§5 のセマンティクス(EIT[p/f] present まで無音、別イベント present で終了)を満たす。対象イベントが実機で一度も present にならない場合に備え、`programs.start_at + duration_secs` から 1 時間後を打ち切り期限として持つ(EPGStation 側の 3 時間待ちより短いので、こちらが先にストリームを閉じる)。実機 EIT では未検証(§7 参照) |
| `GET /events/stream` | 実装済み (2026-08-09) | `web/mirakurun_events.rs`。イベント源は `epg_writer.rs::EpgWriter::flush()` の UPSERT 成功直後で、`tokio::sync::broadcast` (容量 1024) で配信する。フレーミングは本家と同一 (§5.1)。`resource` / `type` クエリフィルタも本家同様に実装済み。`resource` は `program`、`type` は `update` 固定 (本プロジェクトの UPSERT は新規/更新を区別せず、EPGStation も `create`/`update` を同一に扱う。`remove` は EIT からの消滅を検出する仕組みが無いので出さない) |
| `GET /config/server` | 実装済み (2026-08-09) | `api.d.ts` の `ConfigServer` のうち必須の `allowOrigins` / `allowPNA` のみ実値。EPGStation の `tunerServerType: auto` 判定を通すことが目的 |
| `GET /tuners` | 実装済み (2026-08-09、`types` は 2026-08-12) | `bon_drivers` 1 行 = 1 tuner。`isUsing`/`isFree` は `TunerPool` の実行状態から算出。`types` はスキャン済みチャンネルの band から算出 (`channel_types_by_driver`)。`pid`/`users`/`isRemote`/`isFault` は既定値 (理由はハンドラの doc コメントに記載) |
| `GET /status` | 実装済み (`tunerCount` は 2026-08-12 修正) | `tunerCount` は `bon_drivers` の行数 (旧実装は `TunerPool` のキー数を数えており、14 台のサーバーが 1 を返していた)。§5.2-4 |
| `GET /services` / `/programs` | 実装済み (`/services` は 2026-08-12 修正) | `/services` は `(networkId, serviceId)` ごとに 1 件へ重複排除し、`(type, channel)` が multiplex を一意に指すよう衝突を解消する。`remoteControlKeyId` は地上波のみ。§5.2-1/2/3 |
| `GET /services/{id}/stream` | 実装済み (2026-08-12 にサービスフィルタ追加) | `TsServiceFilter` で対象サービスのみへ絞る (旧実装は multiplex 全体)。§5.2-5/6 |
| `GET /services/{id}/logo` | 実装済み (2026-08-14) | ロゴ収集器が CDT から保存した `logos/<nid>_<sid>.png` を `image/png` で返す。`hasLogoData` はそのファイルの有無 (`collected_logo_keys()` がディレクトリを 1 回読んで判定するので、数百サービス分を stat しない)。**ロゴは放送波からしか手に入らないため、一度も選局していない局には出ない**。ファイルが無ければ 404、サービス id として解釈できない値なら 400 |
| `remoteControlKeyId` の欠損 | 実装済み (2026-08-14) | 手動登録 (CSV インポート / `POST /api/channels`) した行は `remote_control_key` が NULL で、EPGStation の番組表・放映中で末尾に回されていた。視聴・EPG 収集中の NIT から NULL の列だけ補完する (`tuner/nit_collector.rs` → `nit_writer.rs`)。**一度も選局していない局は埋まらない**。§5.4 |
| ダッシュボードのクライアント表示 | 実装済み (2026-08-14) | EPGStation の視聴・録画ストリームがクライアント一覧に出るようになった (`protocol` = `mirakurun`)。切断・グラフ・プレビューも同じ行から使える。§5.5 |
| `X-Mirakurun-Priority` | **受理のみ** | パースしてログに出すが、チューナー競合には反映していない。**録画がライブ視聴に負ける状態は解消していない**。反映には `tuner/policy.rs::decide()` の設計変更が要る (CLAUDE.md「選局」の不変条件) |
| `NW1`〜`NW40` | 実装済み (2026-08-12) | `[mirakurun] home_region` に地元の都道府県名を設定すると、その地域の地上波だけ `GR`、他は地域ID昇順で `NW1`〜`NW40` (`web/mirakurun.rs::terrestrial_type_map`)。未設定なら従来どおり全地上波が `GR`。§5.3 |
| `Program.extended` / `video` / `audio` / `relatedItems` | 無し | 番組詳細が埋まらない・イベントリレー不可。`relatedItems` が無いこと自体は EPGStation の `isMainProgram()` が「未定義なら true」を返すため無害 (`EPGUpdateManageModel.ts:144-147`) |
| `isFree` | 常に `true` 固定 | 無料/有料の判別ができない |
| 放送時間未定 (`duration: 1`) | 未確認 | 番組表・予約が壊れうる |
| 衛星の `channel` 文字列 (`BS15_0` 形式) | 簡略化 | 表示のみ。実害は小さい |

**EPGStation 側の対応が正式版に取り込まれた場合の影響**: 本プロジェクトは `Service.channel` を配列で返すよう
実装したため、§4 の `ChannelDB` 修正 (配列・単一オブジェクトの両対応) が正式リリースに入るかどうかに関わらず
放送局登録は通る。`tunerServerType` についても `GET /config/server` を実装したため、EPGStation 側が
`auto` のままでも Mirakurun と判定される。**したがって、この 2 点はもう EPGStation 側の未コミット修正に
依存しない。**

### 残っている作業 (優先度順)

1. ~~**実起動での疎通確認**~~ — 2026-08-12 に実施済み (§5.2)。ただし**録画そのものは未検証**:
   `GET /programs/{id}/stream` の EIT[p/f] ゲートが実際の放送波で開閉するかは、EPGStation から
   予約を通して初めて確認できる (§7)
2. `X-Mirakurun-Priority` をチューナー競合ポリシーへ反映する (録画 > ライブ視聴)。`tuner/policy.rs::decide()` の設計変更が要る
3. `Program.extended` / `video` / `audio` / `relatedItems`、`isFree`、放送時間未定 (`duration: 1`) — いずれも `programs` テーブルのスキーマ拡張が前提
4. ~~`NW1`〜`NW40` (県外地上波)~~ — 2026-08-12 実装 (§5.3)。衛星の `channel` 文字列 (`BS15_0` 形式) は未対応のまま

## 7. 未確認事項

- **API の疎通確認は 2026-08-12 に実施済み** (§5.2)。上の 3 点のうち 2 (`/events/stream` の
  チャンク境界) と 3 (`/docs` 経由の operationId 解決) は実サーバーに対して確認できた。
  **未確認のまま残っているのは 1 の `GET /programs/{id}/stream` の EIT[p/f] ゲート**
  (present の検出、別イベント present での終了) で、これは EPGStation から実際に予約を入れて
  録画させないと観測できない。§5.2 で入れたサービスフィルタもこの経路を通るため、
  **最初の 1 本は録画結果 (映像・音声・字幕が揃っているか、ファイル冒頭が欠けていないか) を必ず確認すること**
- §5.2 / §5.3 の修正はいずれも**稼働中のサーバーへはまだ反映されていない** (このワークツリーでの
  ビルドまで)。デプロイ後に確認すること:
  1. `/services` の件数が実サービス数まで減っていること (本番データでは 770 → 307)、EPGStation 側の
     放送局一覧が重複なく登録されること
  2. `[mirakurun] home_region` を設定した場合、番組表のタブが地域ごとに分かれること。**設定を
     後から変えると EPGStation 側の `channelType` が一斉に変わる**ため、変更するなら EPGStation の
     放送局更新を挟むこと (放送局 ID は変わらないので予約・録画は維持される)
- `decode` クエリを無視して常時デコード済みを返す運用で、EPGStation 側に不都合が出ないか
- `X-Mirakurun-Priority` を無視したまま運用したときに、実際にどの程度「録画がライブ視聴に負ける」か
  (競合が起きる構成でしか観測できない)
- §1・§3・§4・§6 に記した EPGStation 側の改修 (`ChannelDB` の配列/単一オブジェクト両対応、
  `tunerServerType` の明示指定、`/docs` 取得失敗時のログ) は**実装は完了しているが未コミット**であり、
  いつ・どのバージョンで正式リリースに取り込まれるかは未確定。本ファイルの当該記述は
  2026-08-09 時点の作業ツリーの状態に基づく (`git status` で未コミットの差分として確認済み)。
  いずれも実起動での疎通確認はまだ行われていない
- §1 に記した上流 Mirakurun (`/Users/ayumu/prog/Mirakurun`, `stuayu-main`) の `api.yml` 修正
  (`Service.channel` の配列化) も**未コミット**であり、EPGStation の `node_modules/mirakurun` に反映される
  タイミングは未確定。反映前の EPGStation 環境では `/docs` 相当の同梱定義は依然として単数を宣言している
