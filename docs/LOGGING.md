# ロギングシステム

recisdb-proxy はファイルベースのロギングシステムを備えており、ターミナルとファイルの両方へログを出力し、自動的なログローテーション機能を提供しています。

## 機能

- **デュアル出力**: ターミナル（コンソール）とログファイルの両方に同時に出力
- **構造化ログ**: タイムスタンプ、ログレベル、スレッドID、ファイル名、行番号などの情報を記録
- **日次ローテーション**: ログファイルは日付ごとに自動分割
- **古いログの自動削除**: DBに保存された保持日数より前のログファイルは自動的に削除
- **Webダッシュボードからの閲覧**: 直近5000件はインメモリのリングバッファにも複製され、
  ダッシュボードの「ログ」タブ (`GET /api/logs`) からブラウザで閲覧・検索できる。
  ローテーション済みのファイル自体も `GET /api/logs/files` 一覧経由でダウンロード可能。
  詳細は [WEB_DASHBOARD.md](WEB_DASHBOARD.md) の「ログ閲覧」節を参照。
- **ログレベル・保持日数はWebダッシュボードから変更可能**: 「設定 > ログ出力」から
  変更でき、データベース (`log_config` テーブル) に保存される。レベルの変更は
  `tracing_subscriber` の reload レイヤ経由で**再起動なしに即座に反映**される。
  保持日数の変更は次回のログクリーンアップ（変更直後にも1回実行される）から反映される。

## 設定オプション

### コマンドラインオプション

ログ出力先ディレクトリのみコマンドラインで指定します。レベル・保持日数は
Webダッシュボードから変更してください（下記）。

```bash
recisdb-proxy [OPTIONS]

  --log-dir <LOG_DIR>
      ログファイルを保存するディレクトリ [default: logs]

  --log-retention-days <LOG_RETENTION_DAYS>
      起動直後（データベースを開く前）の初回クリーンアップに使う保持日数
      [default: 7]。データベースを開いた後は `log_config` テーブルの値が
      優先され、以降のクリーンアップはすべてそちらに従う。

  -v, --verbose
      起動時点でのデバッグレベルのログを有効化（起動直後にDBの設定で
      上書きされる。恒久的に変更したい場合はダッシュボードから設定すること）
```

### Webダッシュボードからの設定

「設定 > ログ出力」パネル (`GET`/`POST /api/log-config`) からログレベル
(`trace`/`debug`/`info`/`warn`/`error`) と保持日数 (1〜365日) を変更できます。
かつて存在した `recisdb-proxy.toml` の `[logging]` セクションは廃止されました
（残っていても無視されるだけで害はありません）。

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
ただしこれは**起動直後の一時的な初期値**で、データベースを開いた時点で `log_config` の値に
上書きされます。恒久的にDEBUGへ変更したい場合はダッシュボードの「設定 > ログ出力」を使ってください。

```bash
./recisdb-proxy -v
```

## ログローテーション

### 自動削除

サーバー起動時（データベースを開く前は `--log-retention-days`、開いた後は
`log_config.retention_days` の値で改めて実行）と、ダッシュボードから保持日数を
変更した直後に、その日数より古いログファイルが自動的に削除されます。

例えば保持日数7日の場合：

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

- ダッシュボードの「設定 > ログ出力」で保持日数を短縮してください
- 同じ画面でログレベルを`info`に下げてください
- 特定のモジュールのログを無効化する場合は、環境変数で制御可能です：

```bash
RUST_LOG=recisdb_proxy=info,recisdb_proxy::scheduler=warn ./recisdb-proxy
```

## 環境変数

起動直後（データベースを開くまで）のログレベルは `RUST_LOG` 環境変数でも制御できます。
ただし `RUST_LOG` が設定されていても、データベースを開いた時点で `log_config` の値が
改めて適用されます（起動ログに「RUST_LOG is set...」という警告が出ます）。モジュール別の
細かい制御をしたい場合は `RUST_LOG` を、単純にレベルを変えたいだけならダッシュボードを
使うのがおすすめです：

```bash
# すべてのログをDEBUGレベルで出力
RUST_LOG=debug ./recisdb-proxy

# 特定のモジュールのログレベルを指定
RUST_LOG=recisdb_proxy=debug,recisdb_proxy::server=info ./recisdb-proxy

# スケジューラーのログのみ出力
RUST_LOG=recisdb_proxy::scheduler=debug ./recisdb-proxy
```

