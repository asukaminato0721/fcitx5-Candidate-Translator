use crate::{Translator, new_translator};

#[cxx::bridge(namespace = "candidate_translator")]
pub mod ffi {
    #[derive(Default)]
    struct BackendConfig {
        enabled: bool,
        base_url: String,
        model: String,
        api_key: String,
        reasoning_effort: String,
        dictionary_path: String,
        timeout_ms: u64,
        debounce_ms: u64,
        cache_entries: usize,
        cache_path: String,
    }

    #[derive(Clone, Debug)]
    struct Candidate {
        index: u32,
        source: String,
    }

    struct Translation {
        index: u32,
        text: String,
    }

    struct TranslationResult {
        request_id: u64,
        translations: Vec<Translation>,
        error: String,
    }

    extern "Rust" {
        type Translator;
        fn new_translator() -> Result<Box<Translator>>;
        // configure, cancel_requests, and clear_cache discard outstanding work.
        // The host must also discard its pending request IDs.
        fn configure(self: &Translator, config: BackendConfig);
        // An empty string denotes a miss; translations are never empty.
        fn lookup(self: &Translator, target: &str, source: &str) -> String;
        fn submit(self: &Translator, request_id: u64, target: &str, items: Vec<Candidate>);
        fn cancel_requests(self: &Translator);
        fn clear_cache(self: &Translator);
        // Borrowed descriptor: watch for readability, but do not close or read it.
        // Remove the watcher before dropping Translator.
        fn result_fd(self: &Translator) -> i32;
        fn take_results(self: &Translator) -> Vec<TranslationResult>;
    }

    unsafe extern "C++" {
        include!("fcitx5-candidate-translator/cpp/bridge.h");
        #[namespace = "fcitx"]
        type AddonFactory;
        fn addon_factory() -> *mut AddonFactory;
        // Built with the native bridge; exercised by Rust's test harness.
        #[allow(dead_code)]
        fn cpp_self_test() -> bool;
        #[allow(dead_code)]
        fn cpp_wait_for_results(translator: &Translator) -> Vec<TranslationResult>;
    }
}
