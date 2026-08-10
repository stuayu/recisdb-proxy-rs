# 自己更新

recisdb-proxy は 2 つの経路で自分を更新できる。どちらも「ダウンロード → 展開 →
検証 → 実行ファイルの入れ替え → 再起動」という同じ流れを通る
(`web/api/update.rs`)。

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

あわせて、入れ替え前のバイナリを実行ファイルと同じディレクトリに
`recisdb-proxy.previous` として残す。`self_replace` は復元できるものを残さない
ため、これがないと「サーバーが落ちている機械で手動でリリースを取ってくる」
しか復旧手段がない。

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
