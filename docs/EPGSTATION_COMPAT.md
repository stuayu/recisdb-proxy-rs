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

- `GET /services/{id}/logo` (`getLogoImage`) — `ChannelApiModel.ts:101`。`hasLogoData: false` を返している限り EPGStation はロゴを要求しない

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

## 6. 現状の実装との差分 (2026-08-09 時点)

`web/mirakurun.rs` / `web/mod.rs:126-136` を読んだ結果。

| 項目 | 状態 | 影響 |
| --- | --- | --- |
| `GET /docs` | 実装済み (2026-08-09) | `web/mirakurun_docs.rs`。EPGStation が呼ぶ operationId をすべて宣言。詳細と、client.js のパースが課す制約は §1 参照 |
| `Service.channel` の配列化 | 実装済み (2026-08-09) | 1 要素の配列で返し、`/docs` でも配列と宣言している。§1/§4 |
| `GET /programs/{id}/stream` | 実装済み (2026-08-09) | `web/mirakurun.rs::stream_program_by_mirakurun_id` + `web/mirakurun_program_stream.rs::ProgramGate`。§5 のセマンティクス(EIT[p/f] present まで無音、別イベント present で終了)を満たす。対象イベントが実機で一度も present にならない場合に備え、`programs.start_at + duration_secs` から 1 時間後を打ち切り期限として持つ(EPGStation 側の 3 時間待ちより短いので、こちらが先にストリームを閉じる)。実機 EIT では未検証(§7 参照) |
| `GET /events/stream` | 実装済み (2026-08-09) | `web/mirakurun_events.rs`。イベント源は `epg_writer.rs::EpgWriter::flush()` の UPSERT 成功直後で、`tokio::sync::broadcast` (容量 1024) で配信する。フレーミングは本家と同一 (§5.1)。`resource` / `type` クエリフィルタも本家同様に実装済み。`resource` は `program`、`type` は `update` 固定 (本プロジェクトの UPSERT は新規/更新を区別せず、EPGStation も `create`/`update` を同一に扱う。`remove` は EIT からの消滅を検出する仕組みが無いので出さない) |
| `GET /config/server` | 実装済み (2026-08-09) | `api.d.ts` の `ConfigServer` のうち必須の `allowOrigins` / `allowPNA` のみ実値。EPGStation の `tunerServerType: auto` 判定を通すことが目的 |
| `GET /tuners` | 実装済み (2026-08-09) | `bon_drivers` 1 行 = 1 tuner。`isUsing`/`isFree` は `TunerPool` の実行状態から算出。`types`/`pid`/`users`/`isRemote`/`isFault` は既定値 (理由はハンドラの doc コメントに記載) |
| `GET /status` | 実装済み | — |
| `GET /services` / `/programs` | 実装済み | — |
| `GET /services/{id}/stream` | 実装済み | — |
| `GET /services/{id}/logo` | スタブ (404) | `hasLogoData` が常に `false` なので EPGStation からは呼ばれない。`/docs` との整合のためルートだけ存在する |
| `X-Mirakurun-Priority` | **受理のみ** | パースしてログに出すが、チューナー競合には反映していない。**録画がライブ視聴に負ける状態は解消していない**。反映には `tuner/policy.rs::decide()` の設計変更が要る (CLAUDE.md「選局」の不変条件) |
| `NW1`〜`NW40` | 未対応 | 県外地上波を扱えない |
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

1. **実起動での疎通確認** — 本ファイルの内容も実装も、すべて静的解析に基づく。§7 参照
2. `X-Mirakurun-Priority` をチューナー競合ポリシーへ反映する (録画 > ライブ視聴)。`tuner/policy.rs::decide()` の設計変更が要る
3. `Program.extended` / `video` / `audio` / `relatedItems`、`isFree`、放送時間未定 (`duration: 1`) — いずれも `programs` テーブルのスキーマ拡張が前提
4. `NW1`〜`NW40` (県外地上波)、衛星の `channel` 文字列 (`BS15_0` 形式)

## 7. 未確認事項

- **実起動での疎通確認は未実施**。本ファイルの内容も、§6 で「実装済み」としたエンドポイントの検証も、
  すべて静的解析とユニット/統合テスト (in-memory DB + `tower::ServiceExt::oneshot`) による。
  実機 BonDriver も実際の EPGStation も、この環境では動かせていない。
  **実起動できたら、静的解析での推測と食い違った点を最優先でここに記録すること** (CLAUDE.md の記録義務)。
  特に確認したいのは次の 3 点:
  1. `GET /programs/{id}/stream` の EIT[p/f] ゲートが実際の放送波で意図どおり開閉するか
     (present の検出、別イベント present での終了)
  2. `GET /events/stream` のチャンク境界が実際に 1 チャンク = 1 イベントで届くか
     (特に先頭の `[\n` が後続イベントと結合しないか。§5.1)
  3. `GET /docs` から解決した経路で EPGStation が実際に番組表を引けるか
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
