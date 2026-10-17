# KonomiTV 互換 — クライアント側仕様の調査台帳

Mirakurun 互換 API (`/mirakurun/api/*`) を KonomiTV のバックエンドとして使う場合の要求と、本プロジェクトの対応状況を記録する。

- 調査対象: `/Users/ayumu/prog/KonomiTV` (stuayu フォーク)
- 接続設定: `backend: Mirakurun`、`mirakurun_url: http://<host>:40080/mirakurun`
- 調査日: 2026-10-18。静的解析とハンドラテストによる確認。KonomiTV 実起動・実チューナー視聴は未確認。

## 1. 呼び出し一覧

KonomiTV は API のベース URL に `/api/...` を付ける。設定 URL の末尾スラッシュは除去される (`server/app/utils/__init__.py:63-81`)。

| API | KonomiTV の要求・用途 | 実装 |
| --- | --- | --- |
| `GET /api/tuners` | 起動時検査。20 秒、`User-Agent: KonomiTV/{VERSION}`。HTTP 200、JSON 配列、`Server` ヘッダーに `製品名/バージョン` が必要。ヘッダーに `mirakc` を含む場合は mirakc と判定、それ以外は Mirakurun として処理 (`config.py:195-234`)。 | 実装済み。`Server: Mirakurun/4.3.0-stuayu` を返す。配列の形はテスト済み。 |
| `GET /api/services` | チャンネル同期。5 秒、共通 User-Agent。HTTP 200 と配列を要求。各要素の `type`, `networkId`, `serviceId`, `name` を参照し、`remoteControlKeyId` は省略可能 (`models/Channel.py:133-147,185-234`)。 | 実装済み。1サービス1オブジェクト、`channel` は配列。 |
| `GET /api/programs` | 番組同期。10 秒、共通 User-Agent。HTTP 200 と配列を要求。`networkId`, `serviceId`, `eventId`, `startAt`, `duration`, `isFree`, `name` を読む。`relatedItems`, `description`, `extended`, `genres`, `video`, `audios` は任意。`video.streamContent` は null 可。`audios` があれば非空配列と先頭音声/lang を前提とし、無ければ旧形式 `audio` を読む (`models/Program.py:134-159,212-227,240-285,322-439`)。 | 実装済み。KonomiTV が利用する基本フィールドを出力。形式の網羅的なクライアント統合テストは未実施。 |
| `GET /api/tuners` (視聴時) | 5 秒、共通 User-Agent。`isAvailable`, `isFree`, `types`, `name` を読み、空きチューナーを短時間ポーリング (`streams/LiveEncodingTask.py:494-525`)。 | 実装済み。各要素に4フィールドを出力。 |
| `GET /api/services/{serviceId}/stream` | NID/SID を各5桁で連結した ID。`User-Agent` と `X-Mirakurun-Priority: 0`、接続 timeout 15 秒。200 の本文を TS として読み、188×256 バイト単位で処理。切断時の部分データも処理し、再接続・終了処理を行う (`LiveEncodingTask.py:740-783,880-895,946-949,1288-1303,1342-1349`)。 | 実装済み。優先度ヘッダーを解析。ストリームと排他制御は別途実チューナー確認が必要。 |
| `GET /api/services/{serviceId}/logo` | 共通 User-Agent、5 秒。200 のバイナリを PNG として利用。200 以外 (コメント上は 503 を想定) と通信失敗は既定ロゴへフォールバック (`routers/ChannelsRouter.py:440-460`)。 | 実装済み。ロゴがない場合は 404 (`mirakurun.rs` の `get_logo`)。KonomiTV は 200 以外をすべてロゴなし扱いにするため問題ない。 |

KonomiTV サーバー内の `GetMirakurunAPIEndpointURL` 呼び出しは上記の5系統。`/api/version` は起動時検査に使わない。設定検査は旧 Mirakurun のストリームドロップ問題を避けるため `/api/tuners` を使う (`config.py:200-205`)。

## 2. ヘッダーとバージョン判定

起動検査は `Server` ヘッダーが無い、またはバージョン部分が無い場合に設定 URL を拒否する。`mirakc` 文字列を含むヘッダーは mirakc と誤判定されるため、互換サーバーは `Mirakurun/<version>` を返す必要がある (`config.py:209-232`)。

本実装は `/api/version` と `/api/status` で使う互換バージョン定数 `4.3.0-stuayu` を `Server` ヘッダーにも使う。Mirakurun ルーターの全応答に付与し、KonomiTV の `/api/tuners` 検査を通す。

## 3. 差分と未確認事項

| 項目 | 状態 | 備考 |
| --- | --- | --- |
| `Server` ヘッダー | 対応済み | 全 Mirakurun API 応答で `Mirakurun/4.3.0-stuayu`。ハンドラ統合テストあり。 |
| `/tuners` の配列・フィールド | 対応済み | `isAvailable`, `isFree`, `types`, `name` を含む。設定検査用配列をテスト。 |
| `/services`・`/programs` の形 | 対応済み | 配列で返却。詳細は `docs/EPGSTATION_COMPAT.md` のデータマッピングを参照。 |
| ロゴ応答 | 対応済み | 既存ロゴは PNG、未収集時は 404。KonomiTV は 200 以外をロゴなしとして扱う。 |
| 実機ライブ視聴 | 未確認 | KonomiTV 起動、チューナー選局、TS連続受信、切断後の再接続を実機で確認する。 |
| KonomiTV の起動検査 | ハンドラテストのみ | 実際の設定読込・`httpx` クライアントからの接続確認は未実施。 |

## 4. 実起動で確認する項目

1. KonomiTV の `backend: Mirakurun` と `mirakurun_url: http://<host>:40080/mirakurun` で起動できる。
2. `/api/services` の局同期と `/api/programs` の番組同期が完了する。
3. ロゴが存在する局と未収集局で表示が破綻しない。
4. ライブ視聴時の `X-Mirakurun-Priority: 0` が受理され、TS が継続する。
5. チューナー不足、未知のチャンネル、途中切断時に KonomiTV が期待するエラー表示・復帰動作をする。
