# recisdb-proxy クイックスタート

## インストール

### Windows

1. [Releases](https://github.com/kazuki0824/recisdb-rs/releases) から最新の `recisdb-proxy.exe` をダウンロード
2. 適当なフォルダに配置

### Linux (Ubuntu 20.04+)

```bash
wget https://github.com/kazuki0824/recisdb-rs/releases/download/<VERSION>/recisdb-proxy
chmod +x recisdb-proxy
```

## 基本的な起動方法

### 1. 最小限の設定で起動

```bash
recisdb-proxy
```

デフォルト設定で起動します：
- **プロキシサーバー**: `0.0.0.0:12345`
- **Webダッシュボード**: `http://0.0.0.0:8080`
- **DB**: `./recisdb-proxy.db`

### 2. BonDriverを指定して起動

```bash
recisdb-proxy --tuner "C:\BonDriver\BonDriver_PX-MLT1.dll"
```

### 3. カスタムポートで起動

```bash
recisdb-proxy --listen 0.0.0.0:12346 --web-listen 0.0.0.0:8081
```

### 4. 設定ファイルを使用

```bash
recisdb-proxy --config recisdb-proxy.toml
```

## Webダッシュボードへのアクセス

サーバー起動後、ブラウザで `http://localhost:8080` を開くと以下の画面が表示されます：

### ダッシュボード機能

#### リアルタイム監視セクション
- **アクティブなチューナー**: 現在利用可能なBonDriver一覧
- **接続中のクライアント**: TVTest等の接続状況
- **サーバー統計**: セッション数、稼働時間等

#### チューナー設定セクション
各BonDriverの以下の値が編集可能：
- `display_name`: 表示名（任意）
- `max_instances`: 最大同時使用チャンネル数

**設定変更方法**:
1. 「編集」ボタンをクリック
2. 設定値を変更
3. 「保存」をクリック

変更はリアルタイムでデータベースに反映されます。

## よくあるシナリオ

### シナリオ1: PX-MLT1（4チューナー）を複数クライアントで共有

```bash
# サーバー起動
recisdb-proxy --tuner "C:\BonDriver\BonDriver_PX-MLT1.dll"

# ブラウザで http://localhost:8080 を開く
# 「チューナー設定」セクションで max_instances = 4 に設定
# （初期値は1なので、必ず4に変更してください）

# その後、最大4台のクライアント（TVTest）を接続可能
```

### シナリオ2: 地上波チューナー（PX-MLT1）と衛星波チューナー（PX-S）を両立

```bash
# recisdb-proxy.toml を作成
cat > recisdb-proxy.toml << 'EOF'
[server]
listen = "0.0.0.0:12345"
web_listen = "0.0.0.0:8080"
max_connections = 64
EOF

# SQLiteで事前設定
sqlite3 recisdb-proxy.db << 'EOF'
-- PX-MLT1: 最大4チャンネル
UPDATE bon_drivers 
SET max_instances = 4 
WHERE dll_path LIKE '%PX-MLT1%';

-- PX-S: 最大1チャンネル（衛星波は1つのみ）
UPDATE bon_drivers 
SET max_instances = 1 
WHERE dll_path LIKE '%PX-S%';
EOF

# サーバー起動
recisdb-proxy
```

### シナリオ3: 優先度付きアクセス制御

TVTest等のクライアント側で優先度を指定して接続：

```
クライアントA（TVTest①）: priority=100（高優先度、録画用）
クライアントB（TVTest②）: priority=10（低優先度、視聴用）

→ 同一チャンネルを要求した場合、AがBを優先的に取得
→ Bはチャンネル変更が拒否される
```

## トラブルシューティング

### Webダッシュボードにアクセスできない

```bash
# ポートが開いているか確認
# Windows
netstat -ano | findstr :8080

# Linux
netstat -tlnp | grep 8080
```

### 接続したクライアントが見えない

1. クライアント接続直後は表示に数秒の遅延がある（5秒毎更新）
2. サーバーログを確認
   ```bash
   RUST_LOG=debug recisdb-proxy
   ```

### DB設定が反映されない

1. ブラウザのキャッシュをクリア（Ctrl+Shift+Delete）
2. Webダッシュボードの「更新」ボタンを手動クリック
3. サーバーを再起動

## ログ出力

詳細なログを確認する場合：

```bash
# デバッグレベルで起動
recisdb-proxy --verbose

# または環境変数で設定
RUST_LOG=debug recisdb-proxy
```

## 設定ファイル例

### フルカスタマイズ版

```toml
# recisdb-proxy.toml
[server]
# プロキシサーバーのリッスンアドレス
listen = "0.0.0.0:12345"

# Webダッシュボードのリッスンアドレス
web_listen = "0.0.0.0:8080"

# デフォルトチューナー（複数登録可能。DBに登録される）
tuner = "C:\\BonDriver\\BonDriver_PX-MLT1.dll"

# 最大同時接続数
max_connections = 64

[database]
# SQLiteデータベースファイルのパス
path = "./recisdb-proxy.db"

# TLS設定（オプション）
# [tls]
# enabled = true
# ca_cert = "./certs/ca.crt"
# server_cert = "./certs/server.crt"
# server_key = "./certs/server.key"
# require_client_cert = false
```

## コマンドラインオプション一覧

```bash
recisdb-proxy --help
```

主要なオプション：

| オプション | 説明 | デフォルト |
|-----------|------|---------|
| `--listen ADDR` | プロキシサーバーのリッスンアドレス | 0.0.0.0:12345 |
| `--web-listen ADDR` | Webダッシュボードのリッスンアドレス | 0.0.0.0:8080 |
| `--tuner PATH` | デフォルトチューナーパス | （指定なし） |
| `--database PATH` | SQLiteデータベースファイル | recisdb-proxy.db |
| `--max-connections N` | 最大同時接続数 | 64 |
| `--config FILE` | 設定ファイルパス | （指定なし） |
| `--verbose` | デバッグログを有効化 | false |
| `--enable-scan` | チャンネルスキャンを有効化 | true |
| `--scan-on-start` | 起動時にスキャンを実行 | false |

## 次のステップ

1. [Webダッシュボード詳細ガイド](WEB_DASHBOARD.md) - API仕様やダッシュボード機能の詳細
2. [プライオリティチャンネル選択](PriorityChannelSelection.md) - 優先度制御の詳細
3. [BonDriver容量制御](BonDriverCapacityControl.md) - インスタンス制限の設計

## 技術サポート

問題が発生した場合：

1. サーバーログを確認（`--verbose` で詳細化）
2. Webダッシュボードの状態を確認
3. データベースの内容を確認
   ```bash
   sqlite3 recisdb-proxy.db "SELECT * FROM bon_drivers;"
   ```
4. GitHubのIssueを確認・報告
