# 自己更新

recisdb-proxy は 2 つの経路で自分を更新できる。どちらも「ダウンロード → 展開 →
検証 → 実行ファイルの入れ替え → 再起動」という同じ流れを通る
(`web/api/update.rs`)。

全プラットフォームで、リリース／開発版アーティファクトに含まれる実行ファイルを
**同じ版の一式**として更新する。どれか1つでも欠けていれば更新を開始しない。

Linux／macOSの必須ファイル:

- `recisdb-proxy`
- `recisdb`
- `recisdb-proxy-setup`

Windowsの必須ファイル:

- `recisdb-proxy.exe`
- `recisdb.exe`
- `recisdb-proxy-setup.exe`
- `BonDriver_NetworkProxy.dll`

PDBと `BonDriver_NetworkProxy.ini.sample`、`recisdb-proxy.toml.example` も、
アーカイブに含まれていれば更新する。実運用中の `.ini` と `.toml` は設定を
失わないよう上書きしない。

各ファイルは全プラットフォームで同じディレクトリの `*.previous.*` へ退避してから差し替える。
`recisdb.exe` やDLLがMirakurun／TVTestなどで使用中のため置換できない場合は、
それまでに差し替えたファイルを元へ戻し、proxy本体は更新しない。利用中の
プロセスを止めてから再実行すること。

自動更新が対象にするDLLは、recisdb-proxyのインストールディレクトリに同梱された
`BonDriver_NetworkProxy.dll` である。TVTestやMirakurunの別ディレクトリへ別名で
複製したDLLは配置場所を安全に特定できないため、セットアップGUIの
「既存クライアントDLLを更新」で反映する。

| 経路 | 取得元 | 認証 | 画面 |
|---|---|---|---|
| リリース版 | GitHub Releases の資産 | 不要 | ヘッダーの更新通知 |
| 開発版 | GitHub Actions のアーティファクト | **GitHub トークンが必要** | 設定 → 開発版に更新 |

## 開発版に更新するには GitHub トークンが要る

アーティファクトのダウンロード API は、**公開リポジトリでも認証を要求する**
(リリース資産は匿名で取得できるのと違う)。またリダイレクト URL は 1 分で失効
する。

- fine-grained トークン: `Actions: read`
- classic トークン: `repo` スコープ

設定 → 開発版に更新 から保存する。保存後はサーバー側の DB に置かれ、API は
「設定済みかどうか」しか返さない (値は返さない)。

## 入れ替える前に必ず起動を確認する

マジックバイト検査だけでは「実行ファイルの形をしている」ことしか分からない。
次のいずれも検査を通過したうえで起動時に失敗し、その時点では入れ替え済みで
サービスが戻らない:

- アーキテクチャ違いのビルド
- ランタイム DLL の不足
- **Windows の SmartScreen / Defender による、署名なし実行ファイルのブロック**

そのため入れ替え前に、落としたバイナリを `--version` で実際に起動して正常終了
することを確かめる。失敗した場合は**更新を中止し、動いているサーバーをその
まま残す**。手作業で直すしかない停止状態を作るよりよい。

あわせて、入れ替え前のproxy本体を実行ファイルと同じディレクトリに
`recisdb-proxy.previous` として残す。その他の同梱ファイルも、Linux／macOSなら
`recisdb.previous`、Windowsなら `recisdb.previous.exe` や
`BonDriver_NetworkProxy.previous.dll` のような名前で残す。`self_replace` は
復元できるものを残さないため、これがないと
「サーバーが落ちている機械で手動でリリースを取ってくる」しか復旧手段がない。

### Windows サービスとして動いている場合の再起動

Windows では入れ替えたあとの再起動を、隠しフラグ
`--service-restart-watchdog <サービス名>` を付けた自分自身を切り離しプロセスと
して起動することで行う。この補助プロセスは

1. 対象サービスへ停止を要求する
2. **SCM の状態が STOPPED になるまで待つ**
3. 起動を要求し、RUNNING になるまで待つ (失敗した場合は間を空けて再試行)

という順で進む。2 を待たずに固定秒数で起動を撃つと、停止が終わっていないうちに
`sc start` が失敗し、サービスが停止したまま取り残される。

サービス本体の側にも、停止が確実に終わるための作りが入っている。

- 停止要求を受けたら即座に SCM へ `STOP_PENDING` を報告する。報告しないと SCM
  から見て状態が RUNNING のまま停止要求だけが滞留し、以後の制御要求が
  `ERROR_SERVICE_CANNOT_ACCEPT_CTRL` (1061) で弾かれる。こうなると
  `sc stop` も `sc start` も効かず、更新は**プロセスを強制終了するまで
  反映されない**。
- サーバー本体が終了したら、ブロッキングスレッド (BonDriver のリーダーループ)
  の後片付けを待ちすぎないよう時間を区切り、ログを書き切ってからプロセスを
  終了する。ここを待ち切ろうとすると、SCM に STOPPED を報告したあとも古い
  プロセスが残り、新しいインスタンスと DB や BonDriver ハンドルを奪い合う。
- 30 秒経っても停止が終わらない場合は、SCM へ STOPPED を報告してから
  プロセスを落とす。報告してから落とすので、SCM は「予期しない終了」と
  見なさず、failure actions による再起動が二重に走らない。

停止時のログ (`SCM stop control received` / `Shutdown signal received`) は
`WARN` で出る。運用時のログレベルが `WARN` でも「なぜ止まったか」が残るように
してある。

### Windows でブロックされた場合

症状: 更新後にサービスが起動しない。あるいは (上の確認が入ったビルドなら)
「the downloaded binary could not be started」で更新が中止される。

対処:

1. 実行ファイルを直接起動して確認する

   ```powershell
   cd C:\DTV\recisdb-proxy
   .\recisdb-proxy.exe --version
   ```

2. ブロックされているなら、インストール先ディレクトリを Defender の除外に
   追加するか、実行ファイルのブロックを解除する

   ```powershell
   # ダウンロード由来のマーク (Zone.Identifier) が付いている場合
   Unblock-File .\recisdb-proxy.exe
   ```

   なお recisdb-proxy 自身のダウンロードは通常このマークを付けない
   (ブラウザや Explorer の展開と違い、素のファイル書き込みのため)。念のため
   入れ替え前に除去はしているが、SmartScreen / Defender の**レピュテーション
   判定**はこれとは別の仕組みで、除外設定でしか回避できない。

3. 復旧するだけなら、残っている `recisdb-proxy.previous` を戻す

   ```powershell
   Stop-Service recisdb-proxy
   Move-Item -Force .\recisdb-proxy.previous .\recisdb-proxy.exe
   Start-Service recisdb-proxy
   ```

## 開発版アーティファクトの注意

- **アーティファクトは常に zip**。API がワークフローの成果物を zip で固める。
  リリース資産は Windows が zip でそれ以外は tar.gz なので、形式は取得元から
  決める (`ArchiveKind`)
- アーティファクト名は `build.yml` の `recisdb-${{ matrix.label }}` と一致
  させる必要がある。ずれると何も見つからないまま成功したように見えるので、
  対応表はテストで固定してある
- 保持期間を過ぎたアーティファクトはメタデータだけ残りダウンロードは 404 に
  なるため、候補から外している
- 開発版のアーティファクトには `.pdb` が含まれる (`[profile.release] debug = 2`)。
  リリース資産より大きく、ダウンロードに時間がかかる
