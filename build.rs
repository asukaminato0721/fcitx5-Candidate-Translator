use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

fn macos_fcitx_root() -> PathBuf {
    println!("cargo:rerun-if-env-changed=FCITX5_MACOS_ROOT");
    std::env::var_os("FCITX5_MACOS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/Library/Input Methods/Fcitx5.app/Contents"))
}

fn require_path(path: &Path, description: &str) {
    if !path.exists() {
        panic!(
            "{description} was not found at {}. Install the fcitx5-macos development package or build through `make build-macos`",
            path.display()
        );
    }
}

fn version_at_least(version: &str, minimum: &str) -> bool {
    let parse = |value: &str| {
        value
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    let version = parse(version);
    let minimum = parse(minimum);
    let length = version.len().max(minimum.len());
    (0..length)
        .map(|index| {
            (
                version.get(index).copied().unwrap_or(0),
                minimum.get(index).copied().unwrap_or(0),
            )
        })
        .find(|(actual, required)| actual != required)
        .is_none_or(|(actual, required)| actual > required)
}

fn require_macos_fcitx_version(root: &Path) {
    let metadata = root.join("lib/pkgconfig/Fcitx5Core.pc");
    require_path(&metadata, "Fcitx5 Core pkg-config metadata");
    let contents = std::fs::read_to_string(&metadata)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", metadata.display()));
    let version = contents
        .lines()
        .find_map(|line| line.strip_prefix("Version:").map(str::trim))
        .unwrap_or_else(|| panic!("{} does not declare a Version", metadata.display()));
    if !version_at_least(version, "5.1.20") {
        panic!("Fcitx5 >= 5.1.20 is required, but the macOS SDK contains {version}");
    }
}

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

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let mut include_paths = Vec::new();
    if target_os == "macos" {
        // fcitx5-macos ships its SDK inside Fcitx5.app. Its pkg-config files
        // intentionally use the final /Library path, which prevents building
        // against an SDK extracted elsewhere. Accept an explicit SDK root so
        // local and cross-architecture builds do not have to modify /Library.
        let root = macos_fcitx_root();
        require_macos_fcitx_version(&root);
        let include_root = root.join("include/Fcitx5");
        let library_root = root.join("lib");
        for component in ["Core", "Config", "Utils"] {
            let path = include_root.join(component);
            require_path(&path, &format!("Fcitx5 {component} headers"));
            include_paths.push(path);
        }
        require_path(
            &library_root.join("libFcitx5Core.dylib"),
            "Fcitx5 Core library",
        );

        println!("cargo:rustc-link-search=native={}", library_root.display());
        println!("cargo:rustc-link-lib=dylib=Fcitx5Core");
        println!("cargo:rustc-link-lib=dylib=Fcitx5Config");
        println!("cargo:rustc-link-lib=dylib=Fcitx5Utils");
        // Plugins live under ~/Library/fcitx5, so they cannot use a useful
        // loader-relative path to reach the libraries embedded in Fcitx5.app.
        println!("cargo:rustc-link-arg=-Wl,-rpath,/Library/Input Methods/Fcitx5.app/Contents/lib");
    } else {
        let core = pkg_config::Config::new()
            .atleast_version("5.1.20")
            .probe("Fcitx5Core")
            .expect("Fcitx5Core >= 5.1.20 is required");
        let config = pkg_config::Config::new()
            .atleast_version("5.1.20")
            .probe("Fcitx5Config")
            .expect("Fcitx5Config >= 5.1.20 is required");
        include_paths.extend(core.include_paths);
        include_paths.extend(config.include_paths);
    }

    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .warnings(true)
        .flag_if_supported("-fvisibility=hidden")
        .cargo_metadata(false)
        .file("cpp/candidate_translator.cpp");
    for path in &include_paths {
        build.include(path);
    }
    build.compile("candidate_translator_cpp");

    let out_dir = std::env::var("OUT_DIR").expect("Cargo did not set OUT_DIR");
    println!("cargo:rustc-link-search=native={out_dir}");
    println!("cargo:rustc-link-lib=static:+whole-archive=candidate_translator_cpp");
    if target_os == "macos" {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
}
