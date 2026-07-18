extern crate pkg_config;

use std::env::var;

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
    let mut cm = cmake::Config::new("./externals/libaribb25");
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
