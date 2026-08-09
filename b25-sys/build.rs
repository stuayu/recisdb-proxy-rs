extern crate pkg_config;

use std::env::var;
use std::path::{Path, PathBuf};

/// libaribb25 のソースは submodule (upstream tsukumijima/libaribb25) なので
/// リポジトリ内では直せない。ビルド時に OUT_DIR へコピーしてから、下記2件の
/// 不具合を潰したものを cmake に食わせる。
///
/// 1. **PC/SC の出力引数の型** — `b_cas_card.c` は `SCardListReaders` /
///    `SCardTransmit` の長さ引数に `unsigned long` のアドレスを渡している。
///    Linux の pcsclite と Windows では `DWORD == unsigned long` なので正しいが、
///    **macOS の PCSC.framework は `DWORD == uint32_t`**。64bit 変数のアドレスを
///    渡すと下位32bitしか書かれず、上位32bitはスタックのゴミが残る。結果
///    `len` が巨大値になり `prv->sbuf = prv->pool + len` がワイルドポインタと
///    なって、カードリーダー接続時に SIGSEGV で落ちる (実機で確認)。
///    `DWORD` に直せば3プラットフォームとも正しい型になる。
///
/// 2. **`pattern` が const** — `override_card_reader_name_pattern`
///    (= `b25_sys::set_card_reader_name`) は `static const TCHAR pattern[1024]`
///    に `_tcscpy` で書き込む。const オブジェクトへの書き込みは UB で、macOS では
///    読み取り専用セクションに置かれるため SIGBUS で落ちる (実機で確認)。
///    カードリーダーを名前で選ぶ機能そのものが使えないので const を外す。
///
/// 置換対象が見つからない場合は **panic する**。upstream が構造を変えたのに
/// 黙って未修正のままビルドが通ると、また実行時に落ちるため。
fn patched_libaribb25_dir() -> PathBuf {
    let src = Path::new("./externals/libaribb25");
    let dst = PathBuf::from(var("OUT_DIR").expect("OUT_DIR")).join("libaribb25-patched");

    if dst.exists() {
        std::fs::remove_dir_all(&dst).expect("failed to clear the patched libaribb25 copy");
    }
    copy_dir(src, &dst);

    let target = dst.join("aribb25").join("b_cas_card.c");
    let original = std::fs::read_to_string(&target).expect("failed to read b_cas_card.c");
    let mut patched = original.clone();

    // 1. PC/SC 出力引数を DWORD にする。宣言だけを対象にするため行全体で照合する。
    let mut retyped = 0usize;
    for decl in [
        "\tunsigned long len;",
        "\tunsigned long slen;",
        "\tunsigned long rlen;",
        "\tunsigned long rlen,protocol;",
    ] {
        let replacement = decl.replace("unsigned long", "DWORD");
        let hits = patched.matches(decl).count();
        if hits > 0 {
            retyped += hits;
            patched = patched.replace(decl, &replacement);
        }
    }
    assert!(
        retyped >= 10,
        "libaribb25 の PC/SC 長さ変数の宣言が想定と違う (置換できたのは {retyped} 箇所)。\
         upstream の b_cas_card.c を確認して build.rs のパッチを更新すること。"
    );

    // 2. カードリーダー名パターンを書き込み可能にする。
    let const_pattern = "static const TCHAR pattern[1024]";
    assert!(
        patched.contains(const_pattern),
        "libaribb25 の pattern 宣言が想定と違う。build.rs のパッチを更新すること。"
    );
    patched = patched.replace(const_pattern, "static TCHAR pattern[1024]");
    // 引数側も const のままだと _tcscpy の第1引数で型が合わなくなるため、
    // 代入先だけ書き換える (関数シグネチャの const は入力側なので触らない)。

    std::fs::write(&target, patched).expect("failed to write the patched b_cas_card.c");

    // 3. Windows ARM64 (aarch64-pc-windows-msvc) では multi2_simd.{h,c} が
    //    `__m128i`/`__m256i`/SSE2 intrinsic を無条件 (SIMD 無効時でも) に使っており
    //    ARM64 の MSVC には x86 SIMD 型が存在しないためコンパイルが総崩れになる。
    //    幸い、libaribb25 の CMakeLists.txt は MSVC 分岐で `USE_AVX2=ON` の時にしか
    //    `ENABLE_MULTI2_SIMD` を定義せず、b25-sys は x64/ARM64 どちらでもそれを
    //    有効化していない (x64 は明示的に `USE_AVX2=OFF`)。つまり Windows ビルドでは
    //    元々 SIMD 実装は使われておらず、ARM64 で無効化しても挙動もパフォーマンスも
    //    変わらない。
    if var("CARGO_CFG_TARGET_OS").ok().as_deref() == Some("windows")
        && var("CARGO_CFG_TARGET_ARCH").ok().as_deref() == Some("aarch64")
    {
        patch_multi2_simd_for_windows_arm64(&dst);
    }

    println!("cargo:rerun-if-changed=externals/libaribb25");
    dst
}

/// Windows ARM64 専用パッチ。x86 SIMD 型 (`__m128i`/`__m256i`) と SSE2 intrinsic
/// を ARM64 ビルドから排除する。置換対象が見つからない場合は panic する
/// (b_cas_card.c のパッチと同じ方針: upstream の構造が変わったのに黙って
/// 未修正のままビルドが通ると危険なため)。
fn patch_multi2_simd_for_windows_arm64(dst: &Path) {
    // 3a. multi2_simd.h: MULTI2_SIMD_WORK_KEY が __m128i/__m256i 配列を無条件
    //     (SIMD 無効時の #else 分岐含め) に使っている。サイズ (16 バイト × 8 /
    //     32 バイト × 8) を変えずに ARM64 では素の byte 配列に差し替える。
    let header_path = dst.join("aribb25").join("multi2_simd.h");
    let header_original =
        std::fs::read_to_string(&header_path).expect("failed to read multi2_simd.h");
    let mut header_patched = header_original.clone();

    let union_variant = "typedef union {\n\t__m256i key256[8];\n\t__m128i key[8];\n} MULTI2_SIMD_WORK_KEY;";
    let union_replacement = "typedef union {\n#if defined(_M_ARM64) || defined(__aarch64__)\n\tuint8_t key256[8][32];\n\tuint8_t key[8][16];\n#else\n\t__m256i key256[8];\n\t__m128i key[8];\n#endif\n} MULTI2_SIMD_WORK_KEY;";
    assert!(
        header_patched.contains(union_variant),
        "libaribb25 の MULTI2_SIMD_WORK_KEY (union 版) 宣言が想定と違う。\
         build.rs の Windows ARM64 パッチを更新すること。"
    );
    header_patched = header_patched.replacen(union_variant, union_replacement, 1);

    let struct_variant = "typedef struct {\n\t__m128i key[8];\n} MULTI2_SIMD_WORK_KEY;";
    let struct_replacement = "typedef struct {\n#if defined(_M_ARM64) || defined(__aarch64__)\n\tuint8_t key[8][16];\n#else\n\t__m128i key[8];\n#endif\n} MULTI2_SIMD_WORK_KEY;";
    assert!(
        header_patched.contains(struct_variant),
        "libaribb25 の MULTI2_SIMD_WORK_KEY (struct 版) 宣言が想定と違う。\
         build.rs の Windows ARM64 パッチを更新すること。"
    );
    header_patched = header_patched.replacen(struct_variant, struct_replacement, 1);

    assert_ne!(
        header_patched, header_original,
        "multi2_simd.h への Windows ARM64 パッチが反映されなかった"
    );
    std::fs::write(&header_path, header_patched).expect("failed to write patched multi2_simd.h");

    // 3b. multi2_simd.c: <emmintrin.h> (SSE2) を無条件 include し、SSE2/SSSE3/AVX2
    //     intrinsic を使う関数を大量に定義している。ARM64 では
    //     `#ifdef ENABLE_MULTI2_SSE2` 等で守られていない箇所があり ENABLE_MULTI2_SIMD
    //     未定義でも壊れるため、ファイル全体を無効化する
    //     (呼び出し側は multi2.c/arib_std_b25.c どちらも `#ifdef ENABLE_MULTI2_SIMD`
    //     の内側でしかこのファイルの関数を呼ばないので、空の翻訳単位でもリンクは通る)。
    let source_path = dst.join("aribb25").join("multi2_simd.c");
    let source_original =
        std::fs::read_to_string(&source_path).expect("failed to read multi2_simd.c");
    assert!(
        source_original.starts_with("#include <stdlib.h>"),
        "libaribb25 の multi2_simd.c の先頭が想定と違う。\
         build.rs の Windows ARM64 パッチを更新すること。"
    );
    let source_patched = format!(
        "#if !defined(_M_ARM64) && !defined(__aarch64__)\n{source_original}\n#else\n\
         /* Windows ARM64: x86 SIMD (SSE2/SSSE3/AVX2) intrinsic は使用不可のため無効化。\n\
         \x20* multi2_simd.h 側で ENABLE_MULTI2_SIMD が定義されない限り誰にも呼ばれない。 */\n\
         typedef int recisdb_multi2_simd_arm64_stub;\n#endif\n"
    );
    std::fs::write(&source_path, source_patched).expect("failed to write patched multi2_simd.c");
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).expect("failed to create the patched source directory");
    for entry in std::fs::read_dir(src).expect("failed to read the libaribb25 source directory") {
        let entry = entry.expect("failed to walk the libaribb25 source directory");
        let name = entry.file_name();
        // .git は submodule のメタデータ (ファイル)。ビルドには不要。
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type().expect("file_type").is_dir() {
            copy_dir(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap_or_else(|e| panic!("failed to copy {from:?}: {e}"));
        }
    }
}

#[derive(Clone)]
struct TargetVar {
    arch: Option<String>,
    env: Option<String>,
    feat: Option<String>,
    m_system: Option<String>,
    os: Option<String>,
    win: bool,
}

impl Default for TargetVar {
    fn default() -> Self {
        Self {
            arch: var("CARGO_CFG_TARGET_ARCH").ok(),
            env: var("CARGO_CFG_TARGET_ENV").ok(),
            feat: var("CARGO_CFG_TARGET_FEATURE").ok(),
            m_system: var("MSYSTEM").ok(),
            os: var("CARGO_CFG_TARGET_OS").ok(),
            win: var("CARGO_CFG_WINDOWS").is_ok(),
        }
    }
}

fn prep_cmake(cx: TargetVar) -> cmake::Config {
    let mut cm = cmake::Config::new(patched_libaribb25_dir());
    cm.very_verbose(true);
    cm.define("CMAKE_POLICY_VERSION_MINIMUM", "3.5");

    // Disble AVX2 for x64
    if matches!(cx.arch, Some(ref arch) if arch == "x86_64") {
        cm.define("USE_AVX2", "OFF");
    }

    if cx.win {
        if cx.env.clone().unwrap_or_default().contains("gnullvm") {
            unimplemented!("tier3 gnullvm")
        }
        match (
            cx.env.clone().unwrap_or_default().contains("gnu"),
            cx.m_system,
        ) {
            (false, _) => {
                // Let CMake auto-detect the newest installed Visual Studio.
                // Users can still override via the CMAKE_GENERATOR env var.
                if let Ok(generator) = var("CMAKE_GENERATOR") {
                    cm.generator(generator);
                }

                // CI ではランナーに入っている MSVC ツールセットを選びたい。
                // VS 2026 (MSVC 19.51) だと libaribb25 のビルドが通らないため、
                // CMAKE_GENERATOR_TOOLSET (例: "version=14.44") を cmake の -T に渡す。
                if let Ok(toolset) = var("CMAKE_GENERATOR_TOOLSET") {
                    if !toolset.is_empty() {
                        cm.generator_toolset(toolset);
                    }
                }

                if cx.feat.clone().unwrap_or_default().contains("crt-static") {
                    cm.define("CMAKE_MSVC_RUNTIME_LIBRARY", "MultiThreaded");
                }
                if cx.arch.clone().unwrap_or_default().contains("aarch64") {
                    cm.define("USE_NEON", "ON");
                }
            }
            (true, Some(sys_name)) if sys_name.to_lowercase().contains("mingw") => {
                cm.generator("Ninja");
            }
            (true, Some(sys_name)) if sys_name.to_lowercase().contains("ucrt") => {
                cm.generator("Ninja");
            }
            (true, Some(sys_name)) => {
                panic!("target_env:={sys_name} not supported.")
            }
            (true, _) => {
                cm.generator("Ninja");
            }
        }
    }

    // Staticaly link against libaribb25.so or aribb25.lib.
    if cx.env.clone().take().unwrap_or_default().contains("gnu") {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
    println!("cargo:rustc-link-lib=static=aribb25");

    #[cfg(not(debug_assertions))]
    cm.profile("Release");
    cm
}

fn main() {
    let cx = TargetVar::default();

    // Check feat
    #[cfg(all(
        feature = "prioritized_card_reader",
        any(feature = "block00cbc", feature = "block40cbc")
    ))]
    compile_error!(
        "features `crate/prioritized_card_reader` and `crate/block**cbc` are mutually exclusive"
    );

    let mut pc = pkg_config::Config::new();
    pc.statik(false);
    if cx.win {
        let res = prep_cmake(cx).build();
        println!("cargo:rustc-link-search=native={}/lib", res.display());
        println!("cargo:rustc-link-search=native={}/lib64", res.display());
        println!("cargo:rustc-link-lib=dylib=winscard");
    } else if cx.os.clone().unwrap_or_default().contains("macos") {
        // macOS: use the built-in PCSC.framework (no libpcsclite pkg-config needed).
        // cmake's find_package(PCSC REQUIRED) in libaribb25 automatically finds
        // /System/Library/Frameworks/PCSC.framework, so no extra cmake flags are needed.
        println!("cargo:rustc-link-lib=framework=PCSC");
        // libaribb25 is compiled as C++; link libc++ so exception-handling and
        // operator new/delete symbols resolve on macOS.
        println!("cargo:rustc-link-lib=dylib=c++");
        let res = prep_cmake(cx).build();
        println!("cargo:rustc-link-search=native={}/lib", res.display());
        println!("cargo:rustc-link-search=native={}/lib64", res.display());
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path");
    } else {
        // Linux and other Unix platforms: PC/SC is NOT linked at build time.
        // src/pcsc_shim.rs defines the SCard* symbols that the statically
        // linked libaribb25 references, and dlopen()s the actual backend at
        // runtime (libpcsckai.so if present, otherwise libpcsclite.so.1).
        // libpcsclite headers (winscard.h) are still required to compile
        // libaribb25, so check for them without emitting link directives.
        let mut pc_headers = pkg_config::Config::new();
        pc_headers.cargo_metadata(false);
        if pc_headers.probe("libpcsclite").is_err() {
            panic!("libpcsclite headers not found (install libpcsclite-dev / pcsclite-devel).")
        }
        if pc.probe("libaribb25").is_err() || cfg!(feature = "prioritized_card_reader") {
            let res = prep_cmake(cx.clone()).build();
            println!("cargo:rustc-link-search=native={}/lib", res.display());
            println!("cargo:rustc-link-search=native={}/lib64", res.display());
        }
        // dlopen/dlsym for the shim live in libdl on glibc < 2.34.
        // musl and the BSDs provide them in libc, so link -ldl only on
        // glibc-flavored Linux.
        if cx.os.clone().unwrap_or_default() == "linux"
            && !cx.env.clone().unwrap_or_default().contains("musl")
        {
            println!("cargo:rustc-link-lib=dylib=dl");
        }
        // No RPATH needed: rustc-link-arg does not propagate from a library
        // crate to dependent binaries anyway, and the shim itself probes
        // executable-adjacent .so paths before the system search.
    }
}
