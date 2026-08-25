use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_void};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use url::Url;

const CACHE_VERSION: u32 = 1;

unsafe extern "C" {
    fn ct_cpp_addon_factory_instance() -> *mut c_void;
}

// Rust cdylibs export Rust ABI entrypoints through a generated version script.
// Forward the loader symbol explicitly so the C++ factory remains visible to
// Fcitx's dlsym-based addon loader.
#[unsafe(no_mangle)]
pub extern "C" fn fcitx_addon_factory_instance() -> *mut c_void {
    unsafe { ct_cpp_addon_factory_instance() }
}

#[derive(Clone, Debug, Default)]
struct Config {
    enabled: bool,
    base_url: String,
    model: String,
    api_key: String,
    timeout: Duration,
    debounce: Duration,
    cache_entries: usize,
    cache_path: PathBuf,
}

impl Config {
    fn ready(&self) -> bool {
        self.enabled
            && !self.base_url.trim().is_empty()
            && !self.model.trim().is_empty()
            && !self.api_key.is_empty()
    }
}

#[derive(Clone, Debug)]
struct JobItem {
    index: u32,
    source: String,
}

#[derive(Clone)]
struct Job {
    request_id: u64,
    target: String,
    items: Vec<JobItem>,
    user_data: usize,
    callback: ResultCallback,
}

type ResultCallback = unsafe extern "C" fn(*mut c_void, u64, *mut CtResult);

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CacheEntry {
    key: String,
    text: String,
    last_used: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: Vec<CacheEntry>,
}

#[derive(Default)]
struct Cache {
    entries: HashMap<String, CacheEntry>,
    clock: u64,
    loaded_path: Option<PathBuf>,
}

impl Cache {
    fn ensure_loaded(&mut self, path: &Path, limit: usize) {
        if self.loaded_path.as_deref() == Some(path) {
            self.evict(limit);
            return;
        }
        self.entries.clear();
        self.clock = 0;
        self.loaded_path = Some(path.to_path_buf());
        let Ok(bytes) = fs::read(path) else { return };
        let Ok(file) = serde_json::from_slice::<CacheFile>(&bytes) else {
            return;
        };
        if file.version != CACHE_VERSION {
            return;
        }
        for entry in file.entries {
            self.clock = self.clock.max(entry.last_used);
            self.entries.insert(entry.key.clone(), entry);
        }
        self.evict(limit);
    }

    fn get(&mut self, key: &str) -> Option<String> {
        let entry = self.entries.get_mut(key)?;
        self.clock = self.clock.saturating_add(1);
        entry.last_used = self.clock;
        Some(entry.text.clone())
    }

    fn put(&mut self, key: String, text: String, limit: usize) {
        self.clock = self.clock.saturating_add(1);
        self.entries.insert(
            key.clone(),
            CacheEntry {
                key,
                text,
                last_used: self.clock,
            },
        );
        self.evict(limit);
    }

    fn evict(&mut self, limit: usize) {
        while self.entries.len() > limit {
            let Some(key) = self
                .entries
                .values()
                .min_by_key(|entry| entry.last_used)
                .map(|entry| entry.key.clone())
            else {
                break;
            };
            self.entries.remove(&key);
        }
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut entries: Vec<_> = self.entries.values().cloned().collect();
        entries.sort_by_key(|entry| entry.last_used);
        let bytes = serde_json::to_vec(&CacheFile {
            version: CACHE_VERSION,
            entries,
        })
        .map_err(|error| error.to_string())?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        fs::rename(&tmp, path).map_err(|error| error.to_string())
    }
}

#[derive(Default)]
struct State {
    config: Config,
    cache: Cache,
    pending: Option<Job>,
    pending_generation: u64,
    stopping: bool,
    cooldown_until: Option<Instant>,
}

struct Engine {
    shared: Arc<(Mutex<State>, Condvar)>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Engine {
    fn new() -> Self {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("fcitx5-translator".into())
            .spawn(move || worker_loop(worker_shared))
            .expect("failed to start translator worker");
        Self {
            shared,
            worker: Mutex::new(Some(worker)),
        }
    }

    fn ensure_worker(&self) {
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if worker.is_some() {
            return;
        }
        {
            let (lock, _) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
            state.stopping = false;
        }
        let shared = Arc::clone(&self.shared);
        *worker = Some(
            thread::Builder::new()
                .name("fcitx5-translator".into())
                .spawn(move || worker_loop(shared))
                .expect("failed to restart translator worker"),
        );
    }
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

fn engine() -> &'static Engine {
    ENGINE.get_or_init(Engine::new)
}

#[derive(Clone)]
struct Translation {
    index: u32,
    text: CString,
}

#[repr(C)]
pub struct CtResult {
    translations: Vec<Translation>,
    error: CString,
}

fn c_string(value: &str) -> CString {
    CString::new(value.replace('\0', "")).unwrap_or_default()
}

fn cache_key(config: &Config, target: &str, source: &str) -> String {
    serde_json::to_string(&[
        config.base_url.trim_end_matches('/'),
        config.model.as_str(),
        target,
        source,
    ])
    .unwrap_or_default()
}

fn worker_loop(shared: Arc<(Mutex<State>, Condvar)>) {
    let (lock, condvar) = &*shared;
    loop {
        let (job, config) = {
            let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
            while state.pending.is_none() && !state.stopping {
                state = condvar
                    .wait(state)
                    .unwrap_or_else(|error| error.into_inner());
            }
            if state.stopping {
                return;
            }

            loop {
                let generation = state.pending_generation;
                let debounce = state.config.debounce;
                let (next, _) = condvar
                    .wait_timeout(state, debounce)
                    .unwrap_or_else(|error| error.into_inner());
                state = next;
                if state.stopping {
                    return;
                }
                if generation == state.pending_generation {
                    break;
                }
            }
            let job = state.pending.take().expect("pending job disappeared");
            (job, state.config.clone())
        };

        let result = {
            let state = lock.lock().unwrap_or_else(|error| error.into_inner());
            if state
                .cooldown_until
                .is_some_and(|deadline| deadline > Instant::now())
            {
                Err("translation service is cooling down after an error".to_string())
            } else {
                drop(state);
                translate(&config, &job.target, &job.items)
            }
        };

        let mut output = CtResult {
            translations: Vec::new(),
            error: CString::default(),
        };
        match result {
            Ok(translations) => {
                let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
                state.cooldown_until = None;
                let cache_path = state.config.cache_path.clone();
                let cache_limit = state.config.cache_entries;
                for (index, source, text) in translations {
                    let key = cache_key(&config, &job.target, &source);
                    state.cache.put(key, text.clone(), cache_limit);
                    output.translations.push(Translation {
                        index,
                        text: c_string(&text),
                    });
                }
                let _ = state.cache.save(&cache_path);
            }
            Err(error) => {
                let mut state = lock.lock().unwrap_or_else(|poison| poison.into_inner());
                state.cooldown_until = Some(Instant::now() + Duration::from_secs(5));
                output.error = c_string(&error);
            }
        }

        let result = Box::into_raw(Box::new(output));
        unsafe {
            (job.callback)(job.user_data as *mut c_void, job.request_id, result);
        }
    }
}

fn validate_url(base_url: &str) -> Result<String, String> {
    let parsed = Url::parse(base_url).map_err(|error| format!("invalid Base URL: {error}"))?;
    let loopback = matches!(parsed.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err("Base URL must use HTTPS (HTTP is allowed only for loopback)".into());
    }
    Ok(format!(
        "{}/chat/completions",
        base_url.trim_end_matches('/')
    ))
}

fn translate(
    config: &Config,
    target: &str,
    items: &[JobItem],
) -> Result<Vec<(u32, String, String)>, String> {
    if !config.ready() {
        return Err("translation API is not fully configured".into());
    }
    let endpoint = validate_url(&config.base_url)?;
    let candidates: Vec<_> = items
        .iter()
        .map(|item| json!({"index": item.index, "text": item.source}))
        .collect();
    let system = format!(
        "Translate each Simplified Chinese input-method candidate into {target}. Return concise dictionary-style translations, no explanations. Respond with JSON only using {{\"translations\":[{{\"index\":0,\"text\":\"...\"}}]}}. Preserve every supplied index."
    );
    let body = json!({
        "model": config.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": serde_json::to_string(&candidates).unwrap_or_default()}
        ],
        "stream": false
    });
    let client = Client::builder()
        .connect_timeout(config.timeout.min(Duration::from_secs(2)))
        .timeout(config.timeout)
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .post(endpoint)
        .bearer_auth(&config.api_key)
        .json(&body)
        .send()
        .map_err(|error| format!("translation request failed: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("translation service returned HTTP {status}"));
    }
    let payload: Value = response
        .json()
        .map_err(|error| format!("invalid chat completion JSON: {error}"))?;
    let content = payload
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or_else(|| "chat completion did not contain message content".to_string())?;
    parse_translations(content, items)
}

fn parse_translations(
    content: &str,
    items: &[JobItem],
) -> Result<Vec<(u32, String, String)>, String> {
    let start = content
        .find('{')
        .ok_or_else(|| "translation response did not contain JSON".to_string())?;
    let end = content
        .rfind('}')
        .ok_or_else(|| "translation response did not contain complete JSON".to_string())?;
    let payload: Value = serde_json::from_str(&content[start..=end])
        .map_err(|error| format!("invalid translation JSON: {error}"))?;
    let values = payload
        .get("translations")
        .and_then(Value::as_array)
        .ok_or_else(|| "translation JSON has no translations array".to_string())?;
    let sources: HashMap<u32, &str> = items
        .iter()
        .map(|item| (item.index, item.source.as_str()))
        .collect();
    let mut output = Vec::new();
    for value in values {
        let Some(index) = value.get("index").and_then(Value::as_u64) else {
            continue;
        };
        let Ok(index) = u32::try_from(index) else {
            continue;
        };
        let Some(source) = sources.get(&index) else {
            continue;
        };
        let Some(raw) = value.get("text").and_then(Value::as_str) else {
            continue;
        };
        let text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() || text.chars().count() > 256 {
            continue;
        }
        output.push((index, (*source).to_string(), text));
    }
    if output.is_empty() {
        return Err("translation JSON contained no usable translations".into());
    }
    Ok(output)
}

unsafe fn cstr(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return String::new();
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

#[unsafe(no_mangle)]
/// Configure the process-wide translator worker.
///
/// # Safety
/// Every string pointer must be null or point to a valid NUL-terminated string
/// for the duration of this call.
pub unsafe extern "C" fn ct_configure(
    enabled: bool,
    base_url: *const c_char,
    model: *const c_char,
    api_key: *const c_char,
    timeout_ms: u64,
    debounce_ms: u64,
    cache_entries: usize,
    cache_path: *const c_char,
) {
    let engine = engine();
    engine.ensure_worker();
    let (lock, condvar) = &*engine.shared;
    let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
    state.config = Config {
        enabled,
        base_url: unsafe { cstr(base_url) },
        model: unsafe { cstr(model) },
        api_key: unsafe { cstr(api_key) },
        timeout: Duration::from_millis(timeout_ms.clamp(500, 15_000)),
        debounce: Duration::from_millis(debounce_ms.clamp(0, 2_000)),
        cache_entries: cache_entries.min(100_000),
        cache_path: PathBuf::from(unsafe { cstr(cache_path) }),
    };
    let path = state.config.cache_path.clone();
    let limit = state.config.cache_entries;
    state.cache.ensure_loaded(&path, limit);
    state.cooldown_until = None;
    condvar.notify_all();
}

#[unsafe(no_mangle)]
/// Return a Rust-allocated cached translation, or null on a cache miss.
///
/// # Safety
/// Both arguments must be null or valid NUL-terminated strings. A non-null
/// return value must be released exactly once with [`ct_string_free`].
pub unsafe extern "C" fn ct_lookup(target: *const c_char, source: *const c_char) -> *mut c_char {
    let engine = engine();
    let (lock, _) = &*engine.shared;
    let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
    let target = unsafe { cstr(target) };
    let source = unsafe { cstr(source) };
    let key = cache_key(&state.config, &target, &source);
    state
        .cache
        .get(&key)
        .map(|text| c_string(&text).into_raw())
        .unwrap_or(ptr::null_mut())
}

#[unsafe(no_mangle)]
/// Queue an asynchronous translation request.
///
/// # Safety
/// `indices` and `sources` must each reference `len` readable elements, every
/// source must be a valid NUL-terminated string, and `user_data` must remain
/// valid until the callback runs or [`ct_shutdown`] returns.
pub unsafe extern "C" fn ct_submit(
    request_id: u64,
    target: *const c_char,
    indices: *const u32,
    sources: *const *const c_char,
    len: usize,
    user_data: *mut c_void,
    callback: Option<ResultCallback>,
) {
    let Some(callback) = callback else { return };
    if indices.is_null() || sources.is_null() || len == 0 || len > 64 {
        return;
    }
    let indices = unsafe { std::slice::from_raw_parts(indices, len) };
    let sources = unsafe { std::slice::from_raw_parts(sources, len) };
    let items = indices
        .iter()
        .zip(sources)
        .map(|(&index, &source)| JobItem {
            index,
            source: unsafe { cstr(source) },
        })
        .collect();
    let job = Job {
        request_id,
        target: unsafe { cstr(target) },
        items,
        user_data: user_data as usize,
        callback,
    };
    let engine = engine();
    let (lock, condvar) = &*engine.shared;
    let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
    let replaced = state.pending.replace(job);
    state.pending_generation = state.pending_generation.wrapping_add(1);
    condvar.notify_all();
    drop(state);
    if let Some(replaced) = replaced {
        let result = Box::into_raw(Box::new(CtResult {
            translations: Vec::new(),
            error: c_string("translation request was superseded"),
        }));
        unsafe {
            (replaced.callback)(
                replaced.user_data as *mut c_void,
                replaced.request_id,
                result,
            );
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ct_clear_cache() {
    let engine = engine();
    let (lock, _) = &*engine.shared;
    let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
    state.cache.entries.clear();
    let path = state.config.cache_path.clone();
    let _ = fs::remove_file(path);
}

#[unsafe(no_mangle)]
/// Release a string returned by [`ct_lookup`].
///
/// # Safety
/// `value` must be null or a pointer returned by [`ct_lookup`] that has not
/// already been freed.
pub unsafe extern "C" fn ct_string_free(value: *mut c_char) {
    if !value.is_null() {
        drop(unsafe { CString::from_raw(value) });
    }
}

#[unsafe(no_mangle)]
/// Return the number of translations stored in a callback result.
///
/// # Safety
/// `result` must be null or a live result supplied to `ResultCallback`.
pub unsafe extern "C" fn ct_result_len(result: *const CtResult) -> usize {
    unsafe { result.as_ref() }
        .map(|result| result.translations.len())
        .unwrap_or(0)
}

#[unsafe(no_mangle)]
/// Return the original candidate index at `pos`.
///
/// # Safety
/// `result` must be a live callback result and `pos` should be smaller than
/// [`ct_result_len`].
pub unsafe extern "C" fn ct_result_index(result: *const CtResult, pos: usize) -> u32 {
    unsafe { result.as_ref() }
        .and_then(|result| result.translations.get(pos))
        .map(|translation| translation.index)
        .unwrap_or(u32::MAX)
}

#[unsafe(no_mangle)]
/// Borrow the translated text at `pos`.
///
/// # Safety
/// `result` must be a live callback result and the returned pointer must not be
/// used after [`ct_result_free`].
pub unsafe extern "C" fn ct_result_text(result: *const CtResult, pos: usize) -> *const c_char {
    unsafe { result.as_ref() }
        .and_then(|result| result.translations.get(pos))
        .map(|translation| translation.text.as_ptr())
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
/// Borrow the error message from a callback result.
///
/// # Safety
/// `result` must be null or a live callback result and the returned pointer
/// must not be used after [`ct_result_free`].
pub unsafe extern "C" fn ct_result_error(result: *const CtResult) -> *const c_char {
    unsafe { result.as_ref() }
        .map(|result| result.error.as_ptr())
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
/// Release a callback result and all strings owned by it.
///
/// # Safety
/// `result` must be null or a live callback result that has not already been
/// released.
pub unsafe extern "C" fn ct_result_free(result: *mut CtResult) {
    if !result.is_null() {
        drop(unsafe { Box::from_raw(result) });
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn ct_shutdown() {
    let Some(engine) = ENGINE.get() else { return };
    let (lock, condvar) = &*engine.shared;
    {
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        state.stopping = true;
        state.pending = None;
        condvar.notify_all();
    }
    if let Some(worker) = engine
        .worker
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        let _ = worker.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    unsafe extern "C" {
        fn ct_cpp_self_test() -> bool;
    }

    fn items() -> Vec<JobItem> {
        vec![
            JobItem {
                index: 0,
                source: "背单词".into(),
            },
            JobItem {
                index: 2,
                source: "被单".into(),
            },
        ]
    }

    #[test]
    fn parses_json_and_code_fences() {
        let result = parse_translations(
            "```json\n{\"translations\":[{\"index\":0,\"text\":\"recite words\"},{\"index\":2,\"text\":\"bed sheet\"}]}\n```",
            &items(),
        )
        .unwrap();
        assert_eq!(result[0], (0, "背单词".into(), "recite words".into()));
        assert_eq!(result[1].0, 2);
    }

    #[test]
    fn rejects_unknown_and_empty_results() {
        assert!(
            parse_translations(
                "{\"translations\":[{\"index\":9,\"text\":\"wrong\"}]}",
                &items(),
            )
            .is_err()
        );
    }

    #[test]
    fn lru_evicts_oldest_entry() {
        let mut cache = Cache::default();
        cache.put("a".into(), "A".into(), 2);
        cache.put("b".into(), "B".into(), 2);
        assert_eq!(cache.get("a"), Some("A".into()));
        cache.put("c".into(), "C".into(), 2);
        assert!(!cache.entries.contains_key("b"));
        assert!(cache.entries.contains_key("a"));
    }

    #[test]
    fn only_allows_secure_or_loopback_urls() {
        assert!(validate_url("https://example.com/v1").is_ok());
        assert!(validate_url("http://127.0.0.1:8000/v1").is_ok());
        assert!(validate_url("http://example.com/v1").is_err());
    }

    #[test]
    fn cpp_accessor_preserves_candidate_type() {
        assert!(unsafe { ct_cpp_self_test() });
    }

    #[test]
    fn calls_chat_completions_and_maps_indices() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                // Some build sandboxes prohibit even loopback sockets.
                return;
            }
            Err(error) => panic!("failed to bind mock HTTP server: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let header_end;
            loop {
                let size = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..size]);
                if let Some(pos) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap();
            while request.len() < header_end + content_length {
                let size = stream.read(&mut buffer).unwrap();
                request.extend_from_slice(&buffer[..size]);
            }
            sender.send(String::from_utf8(request).unwrap()).unwrap();
            let content = "{\"translations\":[{\"index\":0,\"text\":\"単語を暗記する\"},{\"index\":2,\"text\":\"シーツ\"}]}";
            let payload = serde_json::to_string(&json!({
                "choices": [{"message": {"content": content}}]
            }))
            .unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            )
            .unwrap();
        });

        let config = Config {
            enabled: true,
            base_url: format!("http://{address}/v1"),
            model: "test-model".into(),
            api_key: "secret-token".into(),
            timeout: Duration::from_secs(2),
            ..Config::default()
        };
        let output = translate(&config, "Japanese", &items()).unwrap();
        assert_eq!(output[0], (0, "背单词".into(), "単語を暗記する".into()));
        let request = receiver.recv().unwrap();
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer secret-token")
        );
        assert!(request.contains("test-model"));
        assert!(request.contains("Japanese"));
        server.join().unwrap();
    }

    #[test]
    fn cache_round_trips_and_uses_private_permissions() {
        let unique = format!(
            "fcitx-translator-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(unique);
        let path = directory.join("cache.json");
        let mut cache = Cache::default();
        cache.put("key".into(), "translation".into(), 8);
        cache.save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let mut loaded = Cache::default();
        loaded.ensure_loaded(&path, 8);
        assert_eq!(loaded.get("key"), Some("translation".into()));
        fs::remove_dir_all(directory).unwrap();
    }
}
