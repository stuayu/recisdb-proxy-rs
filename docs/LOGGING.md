# ロギングシステム

recisdb-proxy はファイルベースのロギングシステムを備えており、ターミナルとファイルの両方へログを出力し、自動的なログローテーション機能を提供しています。

## 機能

- **デュアル出力**: ターミナル（コンソール）とログファイルの両方に同時に出力
- **構造化ログ**: タイムスタンプ、ログレベル、スレッドID、ファイル名、行番号などの情報を記録
- **日次ローテーション**: ログファイルは日付ごとに自動分割
- **古いログの自動削除**: 指定日数以上前のログファイルは自動的に削除

## 設定オプション

### コマンドラインオプション

```bash
recisdb-proxy [OPTIONS]

  --log-dir <LOG_DIR>
      ログファイルを保存するディレクトリ [default: logs]

  --log-retention-days <LOG_RETENTION_DAYS>
      ログファイルを保持する日数 [default: 7]

  -v, --verbose
      デバッグレベルのログを有効化
```

### 設定ファイル

`recisdb-proxy.toml` で設定することも可能です：

```toml
[logging]
# ログファイルの保存先ディレクトリ (デフォルト: logs)
log_dir = "logs"

# ログファイルの保持期間（日数） (デフォルト: 7)
retention_days = 7
```

## 使用例

### デフォルト設定で実行

```bash
./recisdb-proxy
```

これにより、`logs` ディレクトリにログファイルが作成されます。

### カスタムログディレクトリを指定

```bash
./recisdb-proxy --log-dir /var/log/recisdb-proxy --log-retention-days 30
```

### 設定ファイルを使用

```bash
./recisdb-proxy -f recisdb-proxy.toml
```

## ログファイル形式

ログファイルは以下の形式で保存されます：

```
logs/recisdb-proxy.log.2026-01-31
```

ファイルの内容例：

```
2026-01-31T23:22:12.315906Z  INFO ThreadId(01) recisdb_proxy: recisdb-proxy\src\main.rs:206: Opening database: "recisdb-proxy.db"
2026-01-31T23:22:12.317712Z  INFO ThreadId(01) recisdb_proxy: recisdb-proxy\src\main.rs:284: recisdb-proxy starting...
2026-01-31T23:22:12.319330Z DEBUG ThreadId(03) recisdb_proxy::scheduler::scan_scheduler: recisdb-proxy\src\scheduler\scan_scheduler.rs:150: ScanScheduler: No BonDrivers due for scanning
```

### ログフォーマット

各ログエントリには以下の情報が含まれます：

- **タイムスタンプ**: `2026-01-31T23:22:12.315906Z` (ISO 8601形式)
- **ログレベル**: `INFO`、`DEBUG`、`WARN`、`ERROR` など
- **スレッドID**: `ThreadId(01)` (ファイルのみ、コンソールには出力されません)
- **モジュール**: `recisdb_proxy` (ファイル/コンソール共通)
- **ファイル位置**: `recisdb-proxy/src/main.rs:206` (ファイルのみ)
- **メッセージ**: 実際のログメッセージ

## ログレベル

デフォルトではINFOレベル以上のログが出力されます。

| レベル | 説明 |
|--------|------|
| ERROR | エラー |
| WARN | 警告 |
| INFO | 情報（デフォルト） |
| DEBUG | デバッグ情報 |
| TRACE | トレース情報 |

詳細なログを見るには `-v` または `--verbose` オプションを使用してDEBUGレベルを有効化してください。

```bash
./recisdb-proxy -v
```

## ログローテーション

### 自動削除

サーバー起動時に、`retention_days` より古いログファイルが自動的に削除されます。

例えば、`--log-retention-days 7` の場合：

```
現在の時刻: 2026-02-01
ローテーション前のファイル:
- recisdb-proxy.log.2026-01-31 （1日前）→ 保持
- recisdb-proxy.log.2026-01-25 （7日前）→ 保持
- recisdb-proxy.log.2026-01-24 （8日前）→ 削除
```

### ディスク容量への影響

デフォルト設定（7日保持）の場合、1日あたりのログサイズが数MBから数十MB程度であれば、ディスク容量への影響は最小限です。ログのボリュームに応じて `retention_days` を調整してください。

## トラブルシューティング

### ログファイルが作成されない場合

1. ログディレクトリへの書き込み権限があることを確認
2. ディレクトリが存在することを確認（存在しない場合は自動作成されます）
3. コンソール出力を確認して、初期化エラーがないか確認

### ログファイルが増え続ける場合

- `retention_days` を短縮してください
- ログレベルを`INFO`に下げてください（`-v`オプションを使わない）
- 特定のモジュールのログを無効化する場合は、環境変数で制御可能です：

```bash
RUST_LOG=recisdb_proxy=info,recisdb_proxy::scheduler=warn ./recisdb-proxy
```

## 環境変数

ログレベルは `RUST_LOG` 環境変数で制御できます：

```bash
# すべてのログをDEBUGレベルで出力
RUST_LOG=debug ./recisdb-proxy

# 特定のモジュールのログレベルを指定
RUST_LOG=recisdb_proxy=debug,recisdb_proxy::server=info ./recisdb-proxy

# スケジューラーのログのみ出力
RUST_LOG=recisdb_proxy::scheduler=debug ./recisdb-proxy
```

