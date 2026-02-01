# recisdb-rs アーキテクチャドキュメント

## 概要

**recisdb-rs**は、クロスプラットフォーム対応のRust製TVチューナーリーダー兼ARIB STD-B25デコーダーです。recpt1、dvbv5-zap、b25などのレガシーツールを置き換え、メモリ効率とエラー処理を改善しています。

### 主な特徴

- クロスプラットフォーム対応 (Windows/Linux)
- メモリ効率の良いオンザフライデコード
- Rustによるメモリ安全性の保証
- シングルバイナリ配布

---

## プロジェクト構成

```
recisdb-rs/                    # ワークスペースルート
├── b25-sys/                   # FFIラッパークレート (約1,760行)
│   ├── src/lib.rs             # 公開API: StreamDecoder, DecoderOptions
│   ├── src/bindings/          # libaribb25へのFFIバインディング
│   │   ├── mod.rs             # InnerDecoderラッパー
│   │   ├── arib_std_b25.rs    # ARIB-STD-B25 C構造体バインディング
│   │   ├── error.rs           # エラー処理 (AribB25DecoderError)
│   │   └── ffi.rs             # ECM/EMM処理 (feature-gated)
│   ├── src/access_control/    # ARIB-B25アクセス制御 (オプション)
│   └── externals/libaribb25/  # Cライブラリサブモジュール
│
└── recisdb-rs/                # メインCLIクレート (約2,596行)
    ├── src/main.rs            # エントリポイント、非同期/タイムアウト処理
    ├── src/context.rs         # CLIパーサー (clap)
    ├── src/channels.rs        # チャンネル表現・パーサー (nom)
    ├── src/tuner/             # プラットフォーム別チューナー実装
    │   ├── mod.rs             # Tunable トレイト、Tuner Enum
    │   ├── error.rs           # エラー型定義
    │   ├── linux/             # Linux実装
    │   │   ├── character_device.rs  # /dev/px4video* 対応
    │   │   └── dvbv5/         # V4L-DVBインターフェース
    │   └── windows/           # Windows BonDriver対応
    │       ├── IBonDriver.rs  # FFIバインディング
    │       └── vtable_resolver/  # 仮想テーブル解決
    ├── src/commands/          # コマンド実装
    │   ├── mod.rs             # process_command() ディスパッチャ
    │   └── utils.rs           # 共通ユーティリティ
    ├── src/io.rs              # 非同期ストリーミングパイプライン
    └── src/utils.rs           # ロギング、プログレスバー
```

---

## 設計方針

### 1. Enumベースの抽象化

トレイトオブジェクト (`dyn Trait`) ではなくEnumでプラットフォーム差異を吸収しています。

```rust
pub enum Tuner {
    #[cfg(feature = "dvb")]
    DvbV5(dvbv5::Tuner),
    Character(character_device::Tuner),
}
```

**理由:**
- パターンマッチで統一的なインターフェースを提供
- コンパイル時の最適化が容易
- 動的ディスパッチのオーバーヘッドを回避

### 2. オンザフライデコード

`AsyncInOutTriple`がFutureトレイトを直接実装し、ストリーミング処理を実現しています。

**特徴:**
- 一時ファイルを使用しない
- ダブルバッファリングなし
- メモリ効率重視のシングルパスストリーミング

```rust
// io.rs - AsyncInOutTriple構造体
pin_project! {
    pub struct AsyncInOutTriple<R, D, W> {
        #[pin] src: R,      // 入力ソース (チューナー/ファイル)
        dec: Option<D>,      // デコーダー (オプション)
        #[pin] dst: W,      // 出力先 (ファイル/stdout)
        // ...
    }
}
```

### 3. Feature-Gatedコンパイル

コンパイル時にプラットフォームや機能を選択し、不要な依存関係を排除しています。

```toml
[features]
crypto = ["b25-sys/block00cbc", "b25-sys/block40cbc"]  # 暗号化サポート
dvb = ["dvbv5", "dvbv5-sys"]                           # DVBサポート (Linux)
prioritized_card_reader = ["b25-sys/prioritized_card_reader"]
default = ["bg-runtime", "prioritized_card_reader"]
```

### 4. 安全なFFI設計

Cライブラリとの連携において、Rustの安全性を最大限活用しています。

- `pin_project!`マクロで自己参照構造体を安全に扱う
- `NonNull<ARIB_STD_B25>`による生ポインタ管理
- Drop実装によるリソースの確実な解放
- libaribb25のCライブラリをRustで安全にラップ

---

## コア抽象化

### トレイト

| トレイト | 定義場所 | 役割 |
|----------|----------|------|
| `Tunable` | `tuner/mod.rs` | チューニング動作を定義。`tune()`メソッドを提供 |
| `AsyncRead` | futures-util | 非同期読み取り。Tunerが実装 |
| `AsyncBufRead` | futures-util | バッファ付き非同期読み取り |
| `Read` / `Write` | std::io | StreamDecoderがB25デコードを実装 |

### Enum

| Enum | 定義場所 | 役割 |
|------|----------|------|
| `ChannelType` | `channels.rs` | 地上波/BS/CS/CATV/BonChの表現 |
| `TsFilter` | `channels.rs` | TSフィルタリング方式 (RelTsNum/AbsTsId/AsIs) |
| `Tuner` | `tuner/*/mod.rs` | プラットフォーム別チューナーのラッパー |

### 主要構造体

| 構造体 | クレート | 役割 |
|--------|----------|------|
| `StreamDecoder` | b25-sys | ARIB-B25デコーダーの公開API |
| `DecoderOptions` | b25-sys | デコーダー設定 (SIMD, strip, EMM処理) |
| `AsyncInOutTriple` | recisdb-rs | 非同期パイプラインFuture |
| `Channel` | recisdb-rs | チャンネル情報の表現 |

---

## プラットフォーム対応

### Linux

| 方式 | 対応デバイス | 実装ファイル |
|------|--------------|--------------|
| キャラクタデバイス | `/dev/px4video*` 等 | `tuner/linux/character_device.rs` |
| DVBv5 | V4L-DVB対応デバイス | `tuner/linux/dvbv5/mod.rs` |

**ioctl操作:**
- `set_ch`: チャンネル設定
- `start_rec` / `stop_rec`: 録画制御
- `ptx_get_cnr`: 信号品質取得
- `ptx_enable_lnb`: LNB電源制御

### Windows

| 方式 | 対応バージョン | 実装ファイル |
|------|----------------|--------------|
| BonDriver | v1, v2, v3 | `tuner/windows/mod.rs` |

**特徴:**
- `libloading`による動的DLL読み込み
- 仮想テーブル解決によるインターフェースバージョン検出
- UTF-16ワイド文字列対応

---

## 依存関係

### b25-sys クレート

```
b25-sys
├── libaribb25 (Cライブラリ - ARIB-B25デコード)
├── libpcsclite (PC/SC-Lite - B-CASカード)
├── log (ロギングファサード)
├── pin-project-lite (Pin projection)
└── [optional] cryptography-00/40 (ソフトウェアキー)
```

### recisdb-rs クレート

```
recisdb-rs
├── b25-sys (ローカル依存)
├── clap (CLIパーサー)
├── nom (パーサーコンビネータ)
├── futures-util (非同期I/O)
├── futures-executor (block_on)
├── futures-time (タイムアウト)
├── indicatif (プログレスバー)
├── ctrlc (Ctrl+C処理)
├── colored (ターミナル色付け)
├── chrono (タイムスタンプ)
├── env_logger (ログ初期化)
├── [Linux] nix (ioctl)
├── [Linux] dvbv5, dvbv5-sys (DVBサポート)
└── [Windows] libloading (DLL読み込み)
```

---

## ビルドシステム

### b25-sys/build.rs

- **CMake統合**: libaribb25をexternalsからビルド
- **プラットフォーム検出**: Windows (MSVC/MinGW) / Linux
- **SIMD設定**: x86_64ではAVX2無効化、ARM64ではNEON有効化
- **静的リンク**: libaribb25を静的リンク

### recisdb-rs/build.rs

- **Bindgen**: BonDriver C++ → Rust FFIバインディング生成
- **C++コンパイル**: IBonDriver.cpp、vtable resolverのコンパイル
- **動的リンク設定**: BonDriver DLLへのリンク

---

## コマンド体系

| コマンド | 機能 | プラットフォーム |
|----------|------|------------------|
| `checksignal` | リアルタイム信号品質監視 (dB) | Linux |
| `tune` | デバイスチューニング + 録画 | 両方 |
| `decode` | 暗号化TSファイルのデコード | 両方 |
| `enumerate` | 利用可能チャンネル一覧 | Windows |

### 実行フロー

```
main()
 ├─ Clapでコマンドライン引数をパース → Cli構造体
 ├─ ロガー初期化 (タイムスタンプ付きカラー出力)
 ├─ commands::process_command() へディスパッチ
 │  ├─ Checksignal: 信号品質ループ
 │  ├─ Tune: デバイスチューン + AsyncInOutTriple Future
 │  ├─ Decode: ファイル読込 + AsyncInOutTriple Future
 │  └─ Enumerate: BonDriverチャンネル列挙
 └─ オプションのタイムアウト付きでFuture実行
    ├─ futures_time::FutureExt::timeout()
    ├─ 別スレッドでプログレスバー表示
    └─ ctrlcによるCtrl+C処理
```

---

## エラーハンドリング

### 方針

- `std::io::Error`を全操作で使用 (anyhow不使用)
- 致命的エラーは`process::exit(1)`で終了
- `-e`フラグでデコーダーエラー時の継続動作をサポート

### エラー型

| エラー型 | 定義場所 | 用途 |
|----------|----------|------|
| `GeneralError` | `tuner/error.rs` | 環境互換性エラー |
| `BonDriverError` | `tuner/error.rs` | BonDriver固有エラー |
| `AribB25DecoderError` | `b25-sys` | ARIB-B25デコーダーエラー |

---

## テスト方針

### 現状

- **ユニットテスト**: チャンネルパーサーのテストが主 (約200行)
- **インテグレーションテスト**: ハードウェア依存のため自動化なし
- **CI/CD**: GitHub Actionsでビルド・フォーマット検証

### テスト対象

```rust
#[cfg(test)]
mod tests {
    // チャンネルパーサーテスト
    fn test_terrestrial_ch_num() { ... }
    fn test_catv_ch_num() { ... }
    fn test_bs_ch_num() { ... }
    fn test_cs_ch_num() { ... }
    fn test_bon_chspace_from_str() { ... }

    // 変換テスト
    fn ch_to_ioctl_freq() { ... }
}
```

---

## 設計判断の根拠

| 決定 | 理由 |
|------|------|
| シングルスレッド非同期 (`block_on`) | チューナーI/Oは逐次的。シンプルさを優先 |
| オンザフライデコード | メモリ効率、ディスクI/O削減 |
| ハードコードされたチャンネルテーブル | ISDB-S仕様は頻繁に変更されない |
| Enumベースの抽象化 | トレイトオブジェクトより効率的、パターンマッチで明確 |
| Feature-gatedコンパイル | プラットフォーム別の最小依存関係 |
| Rust採用 | メモリ安全性、シングルバイナリ配布、C/C++との相互運用性 |

---

## デザインパターン

### 構造パターン

- **Adapter**: プラットフォーム別チューナー型をEnumでラップ
- **Builder**: `Channel::new()`, `DecoderOptions::default()`
- **Facade**: `AsyncInOutTriple`が複雑な非同期処理を隠蔽

### 振る舞いパターン

- **Strategy**: 複数のデコードバックエンド (デコーダーあり/なし)
- **Command**: コマンド処理の共通構造 (`process_command()`)

### Rust固有パターン

- **RAII**: `ManuallyDrop`による慎重なリソース解放 (BonDriver)
- **Pin Projection**: `pin_project!`マクロで自己参照構造体を安全に扱う
- **Marker Traits**: `Unpin`, `AsyncRead`, `AsyncBufRead`による型安全性

---

## 統計情報

| メトリクス | 値 |
|------------|-----|
| 総Rustコード行数 | 約4,356行 |
| b25-sys | 約1,760行 |
| recisdb-rs | 約2,596行 |
| モジュール数 | 17+ |
| C++統合ファイル | 3ファイル |
| テストコード | 約200行 |

---

## 関連リソース

- [libaribb25](https://github.com/tsukumijima/libaribb25) - ARIB-B25デコードライブラリ
- [px4_drv](https://github.com/nns779/px4_drv) - キャラクタデバイスリファレンス
- [ISDBScanner](https://github.com/tsukumijima/ISDBScanner) - チャンネルスキャンツール
