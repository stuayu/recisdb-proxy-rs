---
name: build-and-test
description: recisdb-rs ワークスペースのビルド・テスト・動作確認の標準手順。コード変更後の検証、CI相当のチェック、リリースビルド確認を行うときに使う。
---

# recisdb-rs ビルド & テスト手順

## 基本方針

ワークスペース全体のビルドは `b25-sys`(libaribb25のCビルド、MSVC必須)が走って重い。
**変更したクレートだけを対象にする**のが原則。

```powershell
# 変更クレートのみ(高速)
cargo check -p recisdb-proxy
cargo test  -p recisdb-proxy

# 他クレート
cargo test -p recisdb-protocol
cargo test -p bondriver-proxy-client
cargo test -p recisdb-rs          # b25-sys のCビルドが走る(初回は数分)
```

## クレート間の依存に注意

- `recisdb-proxy` と `bondriver-proxy-client` は `recisdb-protocol` に依存。protocol を変えたら両方テストする。
- `recisdb-proxy/src/server/client_view.rs`(チャンネル列挙)を変えたら、`web/channel_files.rs`(.ch2/ChSet生成)と `server/session.rs`(SetChannelSpace)のテストも通っていることを確認する。列挙順とファイル生成は同一の情報源を共有している。

## テストの性質

- `recisdb-proxy` のDBテストは in-memory SQLite(`Database::open_in_memory()`)で完結し、実機不要。
- 実機BonDriver DLLが必要な経路(チューナーopen、TS読み出し、スキャン実行)は自動テスト不能。
  変更した場合は「ビルド+ユニットテスト通過」までが自動検証の限界であることを報告に明記する。
- マイグレーションを追加したら `migrations_replay_harmlessly_from_user_version_zero` テスト(database/mod.rs)が通ることを必ず確認(冪等性の回帰テスト)。

## リリースビルド確認(FFI変更時は必須)

BonDriver FFI まわりは debug と release で挙動が変わる(UB顕在化)ため、
`b25-sys` / `bondriver-proxy-client` / `recisdb-proxy/src/bondriver` に触れたら:

```powershell
cargo build --release -p <crate>
```

`[profile.release]` は overflow-checks=on / debug=2 なのでスタックトレースは取れる。

## よくある落とし穴

- `b25-sys/externals/libaribb25` はgitサブモジュール。ビルドエラー時は `git submodule update --init` を疑う。
- Windows のパスをTOMLに書くテストではバックスラッシュのエスケープに注意(`setup_helpers.rs` 参照)。
- doc-test で `unused_parens` 等の警告が出ても失敗ではない。exit code で判定する。
