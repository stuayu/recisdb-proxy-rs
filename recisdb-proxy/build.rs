
//! Build script for recisdb-proxy
//!
//! Compiles C++ wrapper code for BonDriver interface on Windows.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        build_bondriver_wrapper();
        build_aribb24_wrapper();
    }
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


fn build_aribb24_wrapper() {
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
    println!("cargo:rerun-if-changed={}", aribb24_src.join("win_compat_asprintf.c").display());
    println!("cargo:rerun-if-changed={}", aribb24_src.join("unistd.h").display());

    let mut b = cc::Build::new();
    b.warnings(false);
    b.flag_if_supported("/utf-8");

    // include ルート：<aribb24/...> が見えるように
    b.include(&aribb24_src);

    // asprintf/vasprintf を有効化（aribb24.c のログ等にも効く場合あり）[4](https://github.com/nkoriyama/aribb24/blob/master/src/drcs.c)
    b.define("HAVE_VASPRINTF", Some("1"));
    b.define("_GNU_SOURCE", Some("1"));
    b.define("__USE_MINGW_ANSI_STDIO", Some("1"));

    // 本体 + 依存
    b.file(wrap_c);
    b.file(aribb24_src.join("aribb24.c"));
    b.file(aribb24_src.join("decoder.c"));
    b.file(aribb24_src.join("parser.c"));
    b.file(aribb24_src.join("drcs.c"));
    b.file(aribb24_src.join("md5.c"));

    // Windows 互換（asprintf/vasprintf）
    b.file(aribb24_src.join("win_compat_asprintf.c"));

    b.compile("aribb24_wrap");
}
