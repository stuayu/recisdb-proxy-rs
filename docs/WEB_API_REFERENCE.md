# Web APIリファレンス

最終更新: 2026-08-01

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

## サーバー管理

| Method | Path | 用途 |
| --- | --- | --- |
| GET | `/api/service/status` | OSサービスの登録状況・稼働状況と、現在の再起動方式 |
| POST | `/api/service/restart` | サーバー自身の再起動 |

`GET /api/service/status` は既定で自プロセスの登録名（サービスとして起動されていない場合は `recisdb-proxy`）をシステムスコープで問い合わせます。`?name=<名前>`・`?scope=user` で対象を変えられます。名前に使えるのは英数字と `.` `_` `-` のみで、それ以外は400を返します。

レスポンス例:

```json
{
  "success": true,
  "supported": true,
  "running_under_service_manager": true,
  "restart_method": "service_manager_respawn",
  "service": {
    "supported": true,
    "manager": "systemd",
    "name": "recisdb-proxy",
    "scope": "system",
    "installed": true,
    "running": true,
    "enabled": true,
    "detail": "active"
  }
}
```

`restart_method` は `POST /api/service/restart` が実際に取る手段です。

- `service_manager_respawn` — プロセスを終了し、systemd の `Restart=always` / launchd の `KeepAlive` による自動再起動に任せる（root権限不要）
- `service_control_manager` — 切り離したプロセスから `sc stop` → `sc start` を実行する（Windowsサービスとして動作中）
- `exec_self` — サービス管理下ではないので、同じ引数で自分自身を起動し直す

再起動すると視聴中・録画中のセッションはすべて切断されます。応答を返し切ってから再起動するため、実際の停止までには約1秒の猶予があります。

サービスの**登録・削除はWeb APIにはありません**。管理者権限が必要な操作であり、任意の実行ファイルをネットワーク越しに常駐登録できると権限昇格の経路になるためです。登録はセットアップウィザードか `recisdb-proxy service install` から行います。

## 分散ノード

`[node] enabled = true` のときに使う。これらは**ダッシュボード側**のAPIで、通常の `/api/*` 認証に従う。
ノード間通信そのものは別リスナー・別名前空間 (`/node/v3/*`、`NodeCredential` 認証) で、
ダッシュボードのBearerトークンとは無関係。

| Method | Path | 用途 |
| --- | --- | --- |
| GET | `/api/nodes` | ローカルID・登録ノード・エンドポイント・ルートグループ・発行済みペアリング・ノードごとの受信可能経路数の一覧 |
| POST | `/api/nodes` | リモートノードの手動登録・編集（エンドポイントは丸ごと置換） |
| POST | `/api/nodes/:id/probe` | 全エンドポイントを実測し、VIEW/PREVIEW/RECORD ごとの最良経路を返す |
| POST | `/api/nodes/pairing` | ワンタイムのペアリングコードを発行する |
| POST | `/api/nodes/pairing/redeem` | 相手ノードのURLとコードを指定してペアリングする |
| POST | `/api/node-route-groups/member` | ルートグループを作成し、ノードを重み付きで追加・更新する |

**クレデンシャルはレスポンスに出さない。** `GET /api/nodes` が返すのは `paired: true/false` だけ。
`POST /api/nodes/pairing` が返す平文コードは**その1回だけ**で、サーバーにはSHA-256しか保存しない
（失くしたら再発行する）。有効期限10分・1回限り。詳細は `docs/DISTRIBUTED_TUNER_FABRIC.md` §4.1。

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
