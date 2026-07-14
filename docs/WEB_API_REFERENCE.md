# Web APIリファレンス

最終更新: 2026-07-14

Webダッシュボードが使用するAPIの一覧。`/api/*` は、認証が有効な場合に `Authorization: Bearer <token>` が必要です。ストリームとファイル取得を除き、レスポンスはJSONです。

## 監視・セッション

| Method | Path | 用途 |
| --- | --- | --- |
| GET | `/api/stats` | サーバー統計 |
| GET | `/api/clients` | 接続中クライアント一覧 |
| GET | `/api/events` | ダッシュボード更新SSE |
| GET | `/api/client/:id/quality` | クライアント品質 |
| GET | `/api/client/:id/metrics-history` | 5分間のメトリクス履歴 |
| POST | `/api/client/:id/disconnect` | クライアント切断 |
| POST | `/api/client/:id/controls` | 優先度・排他制御の上書き |
| GET | `/api/session-history` | セッション履歴 |

## BonDriver

| Method | Path | 用途 |
| --- | --- | --- |
| GET | `/api/tuners` | 互換用チューナー一覧 |
| GET | `/api/bondrivers` | BonDriver一覧 |
| POST | `/api/bondriver` | BonDriver登録 |
| GET | `/api/bondriver/:id` | BonDriver詳細 |
| POST | `/api/bondriver/:id` | BonDriver更新 |
| DELETE | `/api/bondriver/:id` | BonDriver削除 |
| POST | `/api/bondriver/:id/scan` | 手動スキャン開始 |
| GET | `/api/bondriver/:id/quality` | ドライバー品質 |
| GET | `/api/bondrivers/ranking` | ドライバー品質ランキング |
| GET | `/api/scan-history` | スキャン履歴 |

## チャンネル・クライアント設定

| Method | Path | 用途 |
| --- | --- | --- |
| GET | `/api/channels` | チャンネル一覧。`bondriver_id`、`enabled_only`、`group_logical`対応 |
| POST | `/api/channel` | チャンネル登録 |
| POST | `/api/channel/:id` | チャンネル更新 |
| DELETE | `/api/channel/:id` | チャンネル削除 |
| POST | `/api/channel/:id/toggle` | 有効・無効切替 |
| POST | `/api/channels/batch` | 複数行の更新・削除 |
| GET | `/api/channels/export` | CSVエクスポート |
| POST | `/api/channels/import` | CSVインポート (`text/csv`) |
| GET | `/api/client-view/targets` | `Tuner=` 候補一覧 |
| GET | `/api/client-view` | 仮想チューニング空間のプレビュー |
| GET | `/api/client-view/files/:kind` | TVTest・EDCB設定ファイル生成 |

チャンネル更新・一括更新は `channel_name`、`priority`、`is_enabled`、`bon_driver_id`、`nid`、`sid`、`tsid`、`bon_space`、`bon_channel` を扱います。CSV更新でも同じ物理割当を反映します。

## アラート

| Method | Path | 用途 |
| --- | --- | --- |
| GET | `/api/alerts` | アラート一覧 |
| POST | `/api/alerts/:id/acknowledge` | 確認済みに変更 |
| GET | `/api/alert-rules` | ルール一覧 |
| POST | `/api/alert-rules` | ルール登録 |
| DELETE | `/api/alert-rules/:id` | ルール削除 |

## 設定

| Method | Path | 用途 |
| --- | --- | --- |
| GET/POST | `/api/config` | 互換用一括設定 |
| GET/POST | `/api/scan-config` | スキャン設定 |
| GET/POST | `/api/tuner-config` | チューナー最適化設定 |
| GET/POST | `/api/tsreplace-config` | BNDPセッション向け外部エンコード設定 |
| GET/POST | `/api/preview-config` | ブラウザプレビュー設定 |
| GET/POST | `/api/encode-profiles` | エンコードプロファイル一覧・登録 |
| POST/DELETE | `/api/encode-profiles/:id` | エンコードプロファイル更新・削除 |

## ストリームと静的資産

| Method | Path | 用途 |
| --- | --- | --- |
| GET | `/api/stream/service/:sid` | 生TSまたは `?profile=preview` の変換TS |
| GET | `/logos/:file` | チャンネルロゴ |
| GET | `/static/vue/*path` | バイナリに埋め込んだVue成果物 |

`mpegts.js` はnpm依存としてVueバンドルへ同梱されるため、CDNや別置きの `/static/mpegts.js` には依存しません。

## Mirakurun互換API

`[mirakurun] enabled = true` の場合のみ `/mirakurun/api` にマウントされます。この名前空間はMirakurunクライアント互換性のためBearer認証を使用しません。

- `GET /version`
- `GET /status`
- `GET /channels`
- `GET /services`
- `GET /services/:id/stream`
- `GET /channels/:type/:channel/stream`
