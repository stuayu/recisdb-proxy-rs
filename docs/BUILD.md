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
