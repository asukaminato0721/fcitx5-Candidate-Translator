use std::hash::{Hash, Hasher};

fn main() {
    println!("cargo:rerun-if-changed=cpp/candidate_translator.cpp");
    println!("cargo:rerun-if-changed=cpp/translator_ffi.h");
    println!("cargo:rerun-if-changed=data/candidate-translator.conf");

    // Compiler caches do not necessarily hash native archives supplied to
    // rustc. Add the native source content to rustc's arguments so changing
    // the bridge can never reuse a stale cdylib link result.
    let mut source_hash = std::hash::DefaultHasher::new();
    for path in ["cpp/candidate_translator.cpp", "cpp/translator_ffi.h"] {
        std::fs::read(path)
            .expect("failed to read native bridge source")
            .hash(&mut source_hash);
    }
    let source_hash = format!("{:016x}", source_hash.finish());
    println!("cargo:rustc-check-cfg=cfg(cpp_source_hash, values(\"{source_hash}\"))");
    println!("cargo:rustc-cfg=cpp_source_hash=\"{source_hash}\"");

    let core = pkg_config::Config::new()
        .atleast_version("5.1.20")
        .probe("Fcitx5Core")
        .expect("Fcitx5Core >= 5.1.20 is required");
    let config = pkg_config::Config::new()
        .atleast_version("5.1.20")
        .probe("Fcitx5Config")
        .expect("Fcitx5Config >= 5.1.20 is required");

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .warnings(true)
        .flag_if_supported("-fvisibility=hidden")
        .cargo_metadata(false)
        .file("cpp/candidate_translator.cpp");
    for path in core.include_paths.iter().chain(&config.include_paths) {
        build.include(path);
    }
    build.compile("candidate_translator_cpp");

    let out_dir = std::env::var("OUT_DIR").expect("Cargo did not set OUT_DIR");
    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static:+whole-archive=candidate_translator_cpp");
    println!("cargo:rustc-link-lib=dylib=stdc++");
}
