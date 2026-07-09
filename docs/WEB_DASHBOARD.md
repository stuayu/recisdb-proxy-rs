# recisdb-proxy Webダッシュボード

## 概要

recisdb-proxy にはリアルタイム監視と設定管理用の統合Webサーバーがついています。ブラウザから以下の情報が確認でき、設定値も編集できます。

## アクセス方法

デフォルトでは `http://localhost:40080` で利用可能です。

```bash
# サーバー起動時にWebダッシュボード用アドレスを指定
recisdb-proxy --listen 0.0.0.0:40070 --web-listen 0.0.0.0:40080
```

## 機能

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

## API エンドポイント

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

## トラブルシューティング

### ダッシュボードにアクセスできない
- サーバーのポートが開いているか確認: `netstat -ano | findstr :40080`
- ファイアウォール設定を確認
- `--web-listen` オプションで正しいアドレスが指定されているか確認

### 設定変更が反映されない
- ブラウザのキャッシュをクリア
- 5秒ごとに自動更新されるので、しばらく待つ
- サーバーログで同期エラーが出ていないか確認

## 今後の拡張予定

- クライアント毎のDrop/Scramble/Error統計表示
- 配信ストリーム品質の可視化（ビットレート、パケットロス等）
- リモートからの強制切断機能
- セッション履歴とログ出力
- アラート設定（異常検知時の通知）
