
//! Build script for recisdb-proxy
//!
//! Compiles C++ wrapper code for BonDriver interface on Windows.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        build_bondriver_wrapper();
    }
    // aribb24 wrapper is built on all platforms
    build_aribb24_wrapper(&target_os);

    emit_version();
}

/// Determines the build version and emits it as `RECISDB_PROXY_VERSION` for
/// `env!("RECISDB_PROXY_VERSION")` in `src/lib.rs`. Priority:
/// 1. `RECISDB_PROXY_VERSION` env var (CI override — set from the release
///    tag, since `actions/checkout`'s shallow clone doesn't reliably carry
///    tag history for `git describe`).
/// 2. `git describe --tags --always --dirty` (e.g. `v0.0.1-alpha.6` on a
///    tagged commit, `v0.0.1-alpha.6-1-g05a127c` on commits after it).
/// 3. `CARGO_PKG_VERSION` (Cargo.toml's fixed `0.1.0`) if git isn't
///    available at all (e.g. building from a release source tarball with no
///    `.git` directory) — must never fail the build.
///
/// A leading `v` is stripped so the dashboard's `v${version}` display
/// doesn't double up.
fn emit_version() {
    println!("cargo:rerun-if-env-changed=RECISDB_PROXY_VERSION");

    if let Ok(v) = std::env::var("RECISDB_PROXY_VERSION") {
        if !v.trim().is_empty() {
            emit_version_str(&v);
            return;
        }
    }

    // Re-run when HEAD or the ref it points at changes, so a new commit/tag
    // picked up by `git describe` triggers a rebuild. Best-effort: harmless
    // if `.git` doesn't exist (e.g. building from a source tarball).
    println!("cargo:rerun-if-changed=../.git/HEAD");

    if let Some(v) = git_describe() {
        emit_version_str(&v);
        return;
    }

    emit_version_str(env!("CARGO_PKG_VERSION"));
}

fn git_describe() -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let output = std::process::Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty"])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn emit_version_str(v: &str) {
    let v = v.strip_prefix('v').unwrap_or(v);
    println!("cargo:rustc-env=RECISDB_PROXY_VERSION={v}");
}

fn build_bondriver_wrapper() {
    use std::env;
    use std::path::PathBuf;

    let out_dir = env::var("OUT_DIR").unwrap();
    let out_path = PathBuf::from(&out_dir);

    // Path to recisdb-rs source
    let recisdb_src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("..")
        .join("recisdb-rs")
        .join("src")
        .join("tuner")
        .join("windows");

    // Generate bindings for IBonDriver
    let header_path = recisdb_src.join("IBonDriver.hpp");

    println!("cargo:rerun-if-changed={}", header_path.display());

    let bindings = bindgen::builder()
        .allowlist_type("IBonDriver[1-9]?")
        .allowlist_function("CreateBonDriver")
        .header(header_path.to_str().unwrap())
        .dynamic_library_name("BonDriver")
        .dynamic_link_require_all(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("Unable to generate bindings");

    bindings
        .write_to_file(out_path.join("BonDriver_binding.rs"))
        .expect("Couldn't write bindings");

    // Compile C++ wrapper
    let mut compiler = cc::Build::new();

    // Main IBonDriver wrapper
    let cpp_file = recisdb_src.join("IBonDriver.cpp");
    println!("cargo:rerun-if-changed={}", cpp_file.display());
    compiler.file(&cpp_file);

    // vtable resolver files
    let vtable_dir = recisdb_src.join("vtable_resolver");
    for entry in glob::glob(vtable_dir.join("*.cpp").to_str().unwrap()).unwrap() {
        let path = entry.unwrap();
        println!("cargo:rerun-if-changed={}", path.display());
        compiler.file(path);
    }

    compiler
        .cpp(true)
        .warnings(false)
        .flag_if_supported("/utf-8") // 文字コード警告(C4819)の抑止に有効
        .flag_if_supported("/EHa")   // SEH例外もcatch(...)で捕捉可能にする
        .compile("BonDriver_dynamic_cast_ffi");
}


fn build_aribb24_wrapper(target_os: &str) {
    use std::env;
    use std::path::PathBuf;

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let aribb24_dir = manifest_dir.join("vendor").join("aribb24");
    let aribb24_src = aribb24_dir.join("src");

    let wrap_c = manifest_dir.join("src").join("aribb24_wrap.c");

    // 変更検知
    println!("cargo:rerun-if-changed={}", wrap_c.display());
    println!("cargo:rerun-if-changed={}", aribb24_src.join("aribb24.c").display());
    println!("cargo:rerun-if-changed={}", aribb24_src.join("decoder.c").display());
    println!("cargo:rerun-if-changed={}", aribb24_src.join("parser.c").display());
    println!("cargo:rerun-if-changed={}", aribb24_src.join("drcs.c").display());
    println!("cargo:rerun-if-changed={}", aribb24_src.join("md5.c").display());

    let mut b = cc::Build::new();
    b.warnings(false);

    // include ルート：<aribb24/...> が見えるように
    b.include(&aribb24_src);

    if target_os == "windows" {
        b.flag_if_supported("/utf-8");
        b.define("__USE_MINGW_ANSI_STDIO", Some("1"));
        // Windows では asprintf/vasprintf が標準ではないため互換実装を使う
        println!("cargo:rerun-if-changed={}", aribb24_src.join("win_compat_asprintf.c").display());
        b.file(aribb24_src.join("win_compat_asprintf.c"));
    } else {
        // Linux/macOS では asprintf が POSIX 標準で利用可能
        b.define("_GNU_SOURCE", Some("1"));
        b.define("HAVE_VASPRINTF", Some("1"));
    }

    // 本体
    b.file(wrap_c);
    b.file(aribb24_src.join("aribb24.c"));
    b.file(aribb24_src.join("decoder.c"));
    b.file(aribb24_src.join("parser.c"));
    b.file(aribb24_src.join("drcs.c"));
    b.file(aribb24_src.join("md5.c"));

    b.compile("aribb24_wrap");
}
