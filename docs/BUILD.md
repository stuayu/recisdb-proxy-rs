# ビルドガイド

recisdb-rs ワークスペースのビルド方法。通常ビルドと、macOS から Linux amd64 へのクロスビルド手順をまとめる。

## 前提

- Rust (stable)。`b25-sys` を含むビルドには **cmake** と **pkg-config** も必要
- git サブモジュール: `git submodule update --init --recursive`(`b25-sys/externals/libaribb25`)

## 通常ビルド

```sh
cargo build -p recisdb-proxy          # プロキシのみ (b25-sysのCビルド不要。最速)
cargo build -p recisdb                # メインCLI (libaribb25をcmakeでビルド)
cargo build --release                 # 配布用 (release は overflow-checks=on / debug=2)
```

テスト:

```sh
cargo test -p recisdb-proxy           # DBはin-memory SQLiteで完結
cargo test -p recisdb-protocol
cargo test -p bondriver-proxy-client
```

### recisdb-proxy のバージョン表記

`recisdb-proxy` のバージョン(ダッシュボードの `/api/version`・自己更新チェック等)は `Cargo.toml` の固定値ではなく、`recisdb-proxy/build.rs` がビルド時に決定して埋め込む。優先順位:

1. 環境変数 `RECISDB_PROXY_VERSION`(CIがリリースタグから設定。先頭の `v` は自動で除去)
2. `git describe --tags --always --dirty`(タグ通りのコミットなら `0.0.1-alpha.6`、タグから進んだコミットなら `0.0.1-alpha.6-1-g05a127c` のような形式)
3. `git` が使えない環境(ソースtarballからのビルド等)では `Cargo.toml` の `CARGO_PKG_VERSION` にフォールバック

手元でリリースタグ相当のバージョンを付けてビルドしたい場合は `RECISDB_PROXY_VERSION=0.0.1-alpha.6 cargo build -p recisdb-proxy` のように上書きできる。

### プラットフォーム別の要件 (b25-sys / recisdb)

| プラットフォーム | 要件 |
|---|---|
| Linux | `libpcsclite-dev`(**ヘッダのみ使用**。後述の通りリンクはしない)、gcc/g++ |
| macOS | Xcode CLT のみ(PCSC.framework を使用) |
| Windows (MSVC) | Visual Studio。`winscard.lib` は SDK 付属 |
| Windows (GNU) | MSYS2 UCRT64/MinGW64 + Ninja |

### Linux の PC/SC バックエンドは実行時選択

Linux ビルドは pcsclite を**リンクしない**。`b25-sys/src/pcsc_shim.rs` が SCard* シンボルを提供し、初回利用時に以下の順で dlopen する:

1. 環境変数 `B25_PCSC_LIB` で指定されたパス
2. 実行ファイルと同じディレクトリの `libpcsckai.so` → `libpcsclite.so.1` → `libpcsclite.so`
3. システムの `libpcsckai.so` → `libpcsclite.so.1` → `libpcsclite.so`

したがって**実行時にも libpcsclite は必須ではない**(libpcsckai だけの環境でも動く)。どれも見つからない場合はカードリーダー初期化失敗(`SCARD_E_NO_SERVICE`)として扱われる。選択結果は `RUST_LOG=info` で `pcsc_shim: using PC/SC backend ...` と出力される。

注意: システムに共有 `libaribb25.so` がインストールされていると build.rs はそちらを優先リンクし、その .so 自身が libpcsclite に依存するためシムは効かない。実行時切り替えを使う場合は同梱ビルド(静的 aribb25)にすること。

## クロスビルド: macOS → Linux amd64

macOS (Apple Silicon) 上で `x86_64-unknown-linux-gnu` バイナリを作る手順。2026-07 に検証済み。

### 1. ツールチェーン

```sh
brew install messense/macos-cross-toolchains/x86_64-unknown-linux-gnu
rustup target add x86_64-unknown-linux-gnu
```

このツールチェーンの glibc は **2.28**。生成物は glibc 2.28+(Debian 10+ / Ubuntu 18.10+)で動く。逆に言うと、**sysroot に入れる Debian パッケージは glibc 2.28 以下の世代**でなければリンクに失敗する(trixie の libpcsclite は GLIBC_2.34 要求、OpenSSL 3.x 静的は `__isoc23_*` 要求で NG)。

注意 (この開発機固有): Homebrew の rustc が rustup を隠しているため、cargo/rustc は `~/.rustup/toolchains/<host>/bin/` のものを直接使う。

### 2. sysroot の構成

作業ディレクトリ(以下 `$SR`)に Debian パッケージを展開して疑似 sysroot を作る。

```sh
mkdir -p $SR && cd $SR

# pcsclite: ヘッダだけ本物を使う (dev パッケージから usr/include/PCSC を展開)
curl -sLO https://deb.debian.org/debian/pool/main/p/pcsc-lite/libpcsclite-dev_2.3.3-1_amd64.deb
ar -x libpcsclite-dev_2.3.3-1_amd64.deb data.tar.xz && tar -xf data.tar.xz && rm data.tar.xz

# OpenSSL: bullseye の 1.1.1w (glibc 2.28 世代と互換な静的ライブラリ)
curl -sLO https://deb.debian.org/debian/pool/main/o/openssl/libssl-dev_1.1.1w-0+deb11u1_amd64.deb
ar -x libssl-dev_1.1.1w-0+deb11u1_amd64.deb data.tar.xz && tar -xf data.tar.xz && rm data.tar.xz
# Debian は arch 固有ヘッダが別置きなので共通 include にコピー
cp usr/include/x86_64-linux-gnu/openssl/*.h usr/include/openssl/
```

libpcsclite の**ライブラリ本体はスタブで良い**(cmake が作る補助実行ファイル b25/b1 のリンク充足にしか使われず、Rust バイナリはシムのため pcsclite をリンクしない):

```sh
cat > pcsc_stub.c <<'EOF'
long SCardEstablishContext(unsigned long a, const void *b, const void *c, long *d) { return 0x8010001D; }
long SCardReleaseContext(long a) { return 0x8010001D; }
long SCardListReaders(long a, const char *b, char *c, unsigned long *d) { return 0x8010001D; }
long SCardConnect(long a, const char *b, unsigned long c, unsigned long d, long *e, unsigned long *f) { return 0x8010001D; }
long SCardDisconnect(long a, unsigned long b) { return 0x8010001D; }
long SCardTransmit(long a, const void *b, const unsigned char *c, unsigned long d, void *e, unsigned char *f, unsigned long *g) { return 0x8010001D; }
struct scard_io_request { unsigned long dwProtocol; unsigned long cbPciLength; };
const struct scard_io_request g_rgSCardT0Pci = { 1, sizeof(struct scard_io_request) };
const struct scard_io_request g_rgSCardT1Pci = { 2, sizeof(struct scard_io_request) };
const struct scard_io_request g_rgSCardRawPci = { 4, sizeof(struct scard_io_request) };
EOF
x86_64-unknown-linux-gnu-gcc -shared -fPIC -Wl,-soname,libpcsclite.so.1 \
  -o usr/lib/x86_64-linux-gnu/libpcsclite.so.1 pcsc_stub.c
ln -sf libpcsclite.so.1 usr/lib/x86_64-linux-gnu/libpcsclite.so
```

### 3. cmake ツールチェーンファイル

`linux-amd64-toolchain.cmake`(cmake 3.21+ は環境変数 `CMAKE_TOOLCHAIN_FILE` を直接読む):

```cmake
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR x86_64)
set(CMAKE_C_COMPILER x86_64-unknown-linux-gnu-gcc)
set(CMAKE_CXX_COMPILER x86_64-unknown-linux-gnu-g++)
set(CMAKE_LIBRARY_ARCHITECTURE x86_64-linux-gnu)
set(CMAKE_FIND_ROOT_PATH "<$SRの絶対パス>/usr")
set(CMAKE_FIND_ROOT_PATH_MODE_PROGRAM NEVER)
set(CMAKE_FIND_ROOT_PATH_MODE_LIBRARY ONLY)
set(CMAKE_FIND_ROOT_PATH_MODE_INCLUDE ONLY)
```

### 4. ビルド

```sh
export SR=<sysrootの絶対パス>
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_LIBDIR=$SR/usr/lib/x86_64-linux-gnu/pkgconfig   # ホストの .pc を遮断
export PKG_CONFIG_SYSROOT_DIR=$SR
export CMAKE_TOOLCHAIN_FILE=<linux-amd64-toolchain.cmake の絶対パス>
export CC_x86_64_unknown_linux_gnu=x86_64-unknown-linux-gnu-gcc
export CXX_x86_64_unknown_linux_gnu=x86_64-unknown-linux-gnu-g++
export AR_x86_64_unknown_linux_gnu=x86_64-unknown-linux-gnu-ar
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-unknown-linux-gnu-gcc
export OPENSSL_STATIC=1                                            # libssl 実行時依存を無くす
export OPENSSL_LIB_DIR=$SR/usr/lib/x86_64-linux-gnu
export OPENSSL_INCLUDE_DIR=$SR/usr/include

cargo build --release --target x86_64-unknown-linux-gnu -p recisdb -p recisdb-proxy
```

配布時は debug=2 のデバッグ情報を落とす:

```sh
x86_64-unknown-linux-gnu-strip target/x86_64-unknown-linux-gnu/release/recisdb{,-proxy}
```

### 5. 検証

```sh
R=x86_64-unknown-linux-gnu-readelf; T=target/x86_64-unknown-linux-gnu/release
$R -d $T/recisdb | grep NEEDED          # libpcsclite が無いこと
x86_64-unknown-linux-gnu-nm $T/recisdb | grep " T SCard"   # シムの6関数が定義済みなこと
$R -V $T/recisdb | grep -oE "GLIBC_2\.[0-9]+" | sort -Vu | tail -1   # 2.28 以下なこと
```

### トラブルシューティング

| 症状 | 原因と対処 |
|---|---|
| `undefined reference to 'dlsym@GLIBC_2.34'` 等 | sysroot に入れた .so がツールチェーンの glibc より新しい世代。古い Debian のパッケージ(またはスタブ)に差し替える |
| `undefined reference to '__isoc23_strtol'` | OpenSSL 3.x の静的ライブラリが glibc 2.38+ でビルドされている。bullseye の 1.1.1w を使う |
| openssl-sys の "Header expansion error" | `usr/include/x86_64-linux-gnu/openssl/` の arch 固有ヘッダを共通 include にコピーし忘れ |
| `undefined reference to 'g_rgSCardT1Pci'` | スタブ .so にデータシンボル3つ(T0/T1/Raw)が足りていない |
| ホストの Homebrew ライブラリを誤検出 | `PKG_CONFIG_LIBDIR` を sysroot 内に固定できていない(`PKG_CONFIG_PATH` の追加だけでは不十分) |

## macOS で PX4 系チューナーを使う (px4_drv デーモン経由)

macOS はカーネル拡張なしにユーザー空間から `/dev/*` を作れないため、px4_drv の
macOS 版はキャラクタデバイスではなくユーザー空間デーモン `DriverHost_PX4` として
動作する。recisdb-proxy はこれを `px4daemon:` バックエンド
(`bondriver/px4_daemon.rs`) で扱う。

### 1. デーモンを起動しておく

```bash
cd /path/to/px4_drv/macos/build
./DriverHost_PX4 &
```

デーモンは最後のクライアントが切断してから約 15 秒で自動終了する。常駐させたい
場合は launchd などを使う。**recisdb-proxy は意図的にデーモンを自動起動しない** —
ハードウェアを占有するプロセスをサーバーが黙って立ち上げるのは、運用者が選ぶべき
副作用であるため。

### 2. チューナーパスの書式

`bon_drivers.dll_path` に以下を設定する。

| 書式 | 意味 |
|---|---|
| `px4daemon:0` | 受信系統インデックス 0 |
| `px4daemon:any` | 空いている系統を daemon に選ばせる |
| `px4daemon:0+lnb` | LNB 給電を有効化 (BS/CS を受信するなら必須) |
| `px4daemon:1@/run/px4_ctrl.sock` | 制御ソケットのパスを変更 (データソケットは `ctrl`→`data` 置換で導出) |

`+lnb` は opt-in。px4_drv 自身の BonDriver も `LNBPower` 設定の裏に置いており、
別の機器が既に給電している線に電圧を乗せるのは既定にすべきでないため同じ扱いに
した。**付けないと BS/CS は一切ロックしない** (他に給電装置がある構成を除く)。

PX-MLT5PE なら `px4daemon:0` 〜 `px4daemon:4` の 5 本を、それぞれ
`max_instances = 1` で登録する。1 系統 = 1 チャンネルなので、これがハードウェアの
実態と一致する。

### 3. 動作確認

```bash
# デーモンを起動した状態で
cargo test -p recisdb-proxy --lib px4_daemon -- --ignored --nocapture
```

`PX4_TEST_RECEIVER` (既定 `px4daemon:0`) と `PX4_TEST_CHANNEL`
(既定 `0` = UHF 13) で対象を変えられる。CNR と、読み出した TS の同期バイト
ストライドが保たれているかを表示する。

### 制約

- B25 デスクランブルは recisdb-proxy 側の `b25-sys` (libaribb25) を使う。
  macOS で libaribb25 が初期化できない場合はスクランブル済みの生 TS が流れる。
- 再選局は capture を止めずに SET_PARAMS + TUNE + PURGE で行う。daemon 側は
  `SET_CAPTURE(false)` でストリームスレッドを終了させ、`SET_DATA_ID` は接続あたり
  1 回しか効かないため、capture を止めるとデータソケットが二度と復活しない。

## Linux で Linux DVB API (DVBv5) チューナーを使う

px4-drv/pt3-drv の `/dev/px4videoN` 系とは別に、カーネル標準の DVB ドライバ
(`smsdvb` など。PX-Q1UD のような Siano チップ機がこれに該当する)が生やす
`/dev/dvb/adapterN` を直接扱うバックエンドが `bondriver/dvbv5.rs` にある。
`bon_drivers.dll_path` が `/dev/dvb/` で始まるパスなら自動的にこちらが選ばれる
(`/dev/px4videoN` などそれ以外のパスは従来どおり `unix.rs` の px4-drv/pt3-drv
ioctl バックエンドに渡る)。

### 追加のビルド依存は不要

このバックエンドは `libdvbv5` などの外部 C ライブラリを一切使わず、
`linux/dvb/frontend.h` / `linux/dvb/dmx.h` の ioctl 番号・構造体を `nix` の
ioctl マクロと `#[repr(C)]` 構造体で直接叩く。追加の apt パッケージやクロス
ビルド sysroot への追加は不要(既存の `nix`/`libc` 依存のみ)。

`recisdb-rs/src/tuner/linux/dvbv5.rs` には `libdvbv5` (dvbv5-sys crate) 経由の
別実装があるが、recisdb-proxy 側はクロスビルド(本ドキュメント「クロスビルド:
macOS → Linux amd64」参照)の sysroot 依存を増やしたくないため、あえて別実装
にしている。

### チューナーパスの書式

| 書式 | 意味 |
|---|---|
| `/dev/dvb/adapter0` | adapter 0 の frontend0/demux0/dvr0 を使う |
| `/dev/dvb/adapter0/frontend1` | adapter 0 の frontend1/demux1/dvr1 を使う(複数フロントエンド機) |

PX-Q1UD のようにチューナーを複数内蔵する機種は、チューナーごとに別々の
adapter (`/dev/dvb/adapter0` 〜 `/dev/dvb/adapter3`) として現れる。同時に使い
たい本数だけ `bon_drivers` に別レコードとして登録すること。

### スコープ

- 現状は **地上デジタル (ISDB-T) のみ**。frontend が ISDB-S (BS/CS) に対応して
  いても、このバックエンドは BS/CS のチューニング空間を一切公開しない
  (`enum_tuning_space` が space=0 (GR) しか返さない)。BS/CS 対応は未実装。
- 空きチャンネルへの選局はロックしなくてもエラーにしない(スキャナーが地デジ
  50 チャンネルを総当たりする都合上、実在しない局で失敗させると
  `scan_scheduler` の連続失敗カウンタに引っかかりスキャン全体が止まるため)。
  信号の有無は `get_signal_level()` の値で上位が判断する。

### 実行時要件

- 実行ユーザーが `/dev/dvb/adapterN/*` に読み書きできる必要がある。多くの
  ディストリでは `video` グループのメンバーであれば十分(udev ルールによる)。
  グループに入っていないと `open` が permission denied で失敗する。
- CNR (信号レベル) は `DTV_STAT_CNR` が dB 単位の値を返すドライバではそのまま
  使うが、`smsdvb` のように相対値 (0..65535) しか返さないドライバでは概算値
  にスケールしたものを返す。実測 dB ではない点に注意。

## CI とリリース成果物

`.github/workflows/build.yml` が push ごとの CI ビルド、`release.yml` がタグ push
時のリリース作成とアセット添付を担当する。

### 本体のプラットフォーム

| プラットフォーム | ラベル | 備考 |
|---|---|---|
| Windows x64 | `win-x64` | |
| Windows x86 | `win-x86` | |
| Windows arm64 | `win-arm64` | クロスコンパイル。CI 上でテスト実行はしない |
| Linux amd64 | `linux-amd64` | deb パッケージも生成 |
| Linux arm64 | `linux-arm64` | deb パッケージも生成 |
| macOS amd64 | `macos-amd64` | |
| macOS arm64 | `macos-arm64` | |

リリースアセット名は Web ダッシュボードの自己更新機能
(`web/api/update.rs` の `asset_filename`) が決め打ちで参照する。**アセット名を
変えたらそちらも必ず合わせること。**

#### Linux 配布バイナリの実行時要件

Linux 版は `ubuntu:22.04` ベースのコンテナ (`.github/workflows/Dockerfile`) でネイティブ
ビルドしている。そのため配布バイナリの要件は次のとおり (v0.0.1-alpha.11 の arm64 バイナリ
を実測):

- **glibc 2.34 以上** — Ubuntu 22.04+ / Debian 12 (bookworm) 以降。Debian 11 (bullseye,
  glibc 2.31) では起動しない
- **OpenSSL 3.x** — `libssl.so.3` / `libcrypto.so.3` を DT_NEEDED で動的リンクしている
  (bullseye の 1.1 では不可)
- `libpcsclite` は **NEEDED に入らない** (`pcsc_shim` が実行時 dlopen するため。上記
  「Linux の PC/SC バックエンドは実行時選択」を参照)
- 32bit ARM (armhf / armv7l) 向けのアセットは無い。Raspberry Pi は 64bit OS を使う

ビルド元イメージを上げ下げすると glibc の下限が変わり、古いディストリで動かなくなる。
`build-args: IMAGE=ubuntu:22.04` を変更する際は README の「動作要件」表も更新すること。

### tsreadex の同梱ビルド

プレビュー配信の前処理に使う [tsreadex](https://github.com/xtne6f/tsreadex)
(MIT) は、上流が Windows x86/x64 のバイナリしか配布していない。そのままだと
Linux/macOS では利用側にビルドツールチェーンを要求することになるため、
`.github/workflows/tsreadex.yml` で上流のソースから本体と同じ 7 プラットフォーム分を
ビルドし、本体のリリースに
`tsreadex-<タグ>-<ラベル>.zip` / `.tar.gz` として添付している。

- Windows は CMake + MSVC (`-A x64` / `-A ARM64`)、それ以外は同梱の `Makefile`。
- MIT ライセンスの再配布要件を満たすため、`License.txt` を必ず同梱する。
- `workflow_dispatch` で単体実行もできる (この場合はリリースに添付せず
  Actions の artifact のみ)。`upstream_ref` を指定すると特定タグをビルドする。

取得側 (`recisdb-proxy/src/preview_setup.rs`) は、

1. 既存の tsreadex を検出 (`thirdparty/`、KonomiTV の同梱、`PATH`)
2. 本プロジェクトのリリースからこのプラットフォーム向けアセットを取得
3. 上流のリリース (Windows はプリビルド、Unix はソース + `make`)

の順に解決する。**ワークフローのラベルとアセット名を変えると 2 が黙って失敗して
3 に落ちる**ので、変更時は `preview_setup.rs` の `own_asset_suffix` も合わせること。

### ffmpeg の自動ダウンロード

プレビュー用 ffmpeg は BtbN/FFmpeg-Builds の静的 GPL ビルドを使う。
アセット名にバージョンサフィックスが付いたり外れたりする実績があるため、
ファイル名を決め打ちせず GitHub のリリース API から解決している
(`preview_setup.rs` の `btbn_asset_url`)。macOS 向けビルドは配布されていないので
`brew install ffmpeg` にフォールバックする。
