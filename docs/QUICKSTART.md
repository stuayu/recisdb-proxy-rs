# recisdb-proxy クイックスタート

## インストール

### Windows

1. [Releases](https://github.com/stuayu/recisdb-proxy-rs/releases) から
   `recisdb-{tag}-win-x64.zip` をダウンロードして展開
2. 展開先の `recisdb-proxy.exe` と `recisdb-proxy-setup.exe` をそのまま使います

### Linux / macOS

コピー&ペーストだけで完了する手順を README にまとめています
(ダウンロード・展開・pcscd・udev ルール・サービス登録まで)。

- [Linux へのインストール](../README.md#linux-へのインストール-コピペで完了)
- [macOS へのインストール](../README.md#macos-へのインストール-コピペで完了)

要点だけ書くと、リリースの tar.gz は 1 階層のフォルダに
`recisdb-proxy` / `recisdb-proxy-setup` / `recisdb-proxy.toml.example` を含むので、
次のように展開します。

```bash
REPO=stuayu/recisdb-proxy-rs
TAG=$(wget -qO- "https://api.github.com/repos/$REPO/releases" | grep -m1 '"tag_name"' | cut -d'"' -f4)
wget -O /tmp/recisdb-proxy.tar.gz \
  "https://github.com/$REPO/releases/download/$TAG/recisdb-proxy-$TAG-linux-amd64.tar.gz"
sudo mkdir -p /opt/recisdb-proxy
sudo tar xzf /tmp/recisdb-proxy.tar.gz -C /opt/recisdb-proxy --strip-components=1
```

## はじめての方: かんたんセットアップ (GUI)

コマンドライン操作に慣れていない場合は、`recisdb-proxy-setup.exe` をダブルクリックしてください。  
画面の指示に従うだけで、設定ファイルの作成・チューナーの自動検出/登録・`recisdb-proxy` の起動・
ダッシュボードのオープンまで完了します。以降の「基本的な起動方法」はコマンドラインで手動設定したい方向けです。

確認画面の「OSのサービスとして登録し、PC起動時に自動で開始する」にチェックを入れると、
そのままサービスとして常時稼働させられます (サービス名も同じ画面で指定できます)。詳細は
「サービスとして常時稼働させる」を参照してください。

## 基本的な起動方法

### 1. 最小限の設定で起動

```bash
recisdb-proxy
```

デフォルト設定で起動します：
- **プロキシサーバー**: `0.0.0.0:40070`
- **Webダッシュボード**: `http://127.0.0.1:40080` (既定はループバックのみ。別PCから開くには
  `--web-listen 0.0.0.0:40080` か、設定ファイルの `web_listen` を指定)
- **DB**: `./recisdb-proxy.db`

### 2. BonDriverを指定して起動

```bash
recisdb-proxy --tuner "C:\BonDriver\BonDriver_PX-MLT1.dll"
```

### 3. カスタムポートで起動

```bash
recisdb-proxy --listen 0.0.0.0:40071 --web-listen 0.0.0.0:40081
```

### 4. 設定ファイルを使用

```bash
recisdb-proxy --config recisdb-proxy.toml
```

## サービスとして常時稼働させる

PC起動時に自動で開始させるには、OSのサービスとして登録します (Linux: systemd / macOS: launchd /
Windows: サービスコントロールマネージャー)。生成されるユニット/plist の中身、ログの見方、
ユーザースコープの注意点 (linger・udev) までは
[README の「サービスとして常時稼働させる」](../README.md#サービスとして常時稼働させる)
に詳しく書いています。

```bash
# システム全体に登録する (Linux/macOSは root 権限、Windowsは管理者権限が必要)
sudo recisdb-proxy service install --name recisdb-proxy

# ログインユーザー単位で登録する (Linux/macOSのみ。管理者権限は不要ですが、
# ログイン後にのみ動作します)
recisdb-proxy service install --name recisdb-proxy --user
```

主なオプション:

| オプション | 既定値 | 説明 |
| --- | --- | --- |
| `--name` | `recisdb-proxy` | サービス名。英数字と `.` `_` `-` のみ、64文字以内 |
| `--user` | off | ユーザー単位で登録する (Windows非対応) |
| `--config` | ― | サービスに渡す設定ファイル。省略時は `-f` の値、それも無ければ作業フォルダの `recisdb-proxy.toml` |
| `--working-dir` | 実行ファイルの場所 | サービスの作業ディレクトリ |

登録後の操作:

```bash
recisdb-proxy service status            # 登録状況・稼働状況を表示
sudo recisdb-proxy service stop         # 停止
sudo recisdb-proxy service start        # 開始
sudo recisdb-proxy service restart      # 再起動
sudo recisdb-proxy service uninstall    # 停止して登録を解除
```

Webダッシュボードの「設定」タブにも登録状況が表示され、そこからサーバーを再起動できます
(視聴中・録画中のセッションはすべて切断されます)。

## Webダッシュボードへのアクセス

サーバー起動後、ブラウザで `http://localhost:40080` を開くと以下の画面が表示されます：

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

#### クライアント設定タブ

TVTest / EDCB 側の設定はダッシュボードの「クライアント設定」タブで完結します:

1. **STEP 1** で接続先チューナー (グループ推奨) を選ぶ
2. **STEP 2** の `BonDriver_NetworkProxy.ini` をコピーして、クライアントPCの
   BonDriver_NetworkProxy.dll と同じフォルダに保存
3. **STEP 3** で TVTest 用 `.ch2` / EDCB 用 `ChSet4.txt`・`ChSet5.txt` を
   ダウンロードして配置すると、クライアント側のチャンネルスキャンを省略できます
   (「まとめてダウンロード」で INI・README 込みの zip も取得可能)
4. **STEP 4** の対応表で、クライアントに表示される空間・チャンネルを確認

なお、かんたんセットアップ (`recisdb-proxy-setup`) を使った場合は、インストール先の
`client-config/` フォルダにも配布用の INI・README が自動出力されています。

## よくあるシナリオ

### シナリオ1: PX-MLT1（4チューナー）を複数クライアントで共有

```bash
# サーバー起動
recisdb-proxy --tuner "C:\BonDriver\BonDriver_PX-MLT1.dll"

# ブラウザで http://localhost:40080 を開く
# 「チューナー設定」セクションで max_instances = 4 に設定
# （初期値は1なので、必ず4に変更してください）

# その後、最大4台のクライアント（TVTest）を接続可能
```

### シナリオ2: 地上波チューナー（PX-MLT1）と衛星波チューナー（PX-S）を両立

```bash
# recisdb-proxy.toml を作成
cat > recisdb-proxy.toml << 'EOF'
[server]
listen = "0.0.0.0:40070"
web_listen = "0.0.0.0:40080"
max_connections = 64
EOF

# サーバー起動
recisdb-proxy

# ブラウザで http://localhost:40080 を開き、
# Webダッシュボードの「チューナー設定」から以下を設定:
#   PX-MLT1: max_instances = 4（4チャンネル同時使用）
#   PX-S:    max_instances = 1（衛星波は1つのみ）
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
netstat -ano | findstr :40080

# Linux
netstat -tlnp | grep 40080
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
listen = "0.0.0.0:40070"

# Webダッシュボードのリッスンアドレス
web_listen = "0.0.0.0:40080"

# デフォルトチューナー（複数登録可能。DBに登録される）
tuner = "C:\\BonDriver\\BonDriver_PX-MLT1.dll"

# 最大同時接続数
max_connections = 64

[database]
# SQLiteデータベースファイルのパス
path = "./recisdb-proxy.db"

# TLS設定
# 注意: サーバー側TLSは現在未実装です（設定を書いても暗号化されません）。
# 対応状況は docs/DESIGN.md §9 / docs/REVIEW_2026-07.md S4 を参照してください。
```

## コマンドラインオプション一覧

```bash
recisdb-proxy --help
```

主要なオプション：

| オプション | 説明 | デフォルト |
|-----------|------|---------|
| `--listen ADDR` | プロキシサーバーのリッスンアドレス | 0.0.0.0:40070 |
| `--web-listen ADDR` | Webダッシュボードのリッスンアドレス | 0.0.0.0:40080 |
| `--tuner PATH` | デフォルトチューナーパス | （指定なし） |
| `--database PATH` | SQLiteデータベースファイル | recisdb-proxy.db |
| `--max-connections N` | 最大同時接続数 | 64 |
| `--config FILE` | 設定ファイルパス | （指定なし） |
| `--verbose` | デバッグログを有効化 | false |
| `--enable-scan` | チャンネルスキャンを有効化 | true |
| `--scan-on-start` | 起動時にスキャンを実行 | false |

## 次のステップ

1. [Webダッシュボード詳細ガイド](WEB_DASHBOARD.md) - API仕様やダッシュボード機能の詳細
2. [設計マスタ](DESIGN.md) - 優先度・排他制御やインスタンス制限を含む全体設計

## 技術サポート

問題が発生した場合：

1. サーバーログを確認（`--verbose` で詳細化）
2. Webダッシュボードの状態を確認
3. GitHubのIssueを確認・報告
