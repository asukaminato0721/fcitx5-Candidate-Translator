use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use url::Url;

const CACHE_VERSION: u32 = 1;

mod bridge;
use bridge::ffi::{BackendConfig, Candidate as JobItem, Translation, TranslationResult};

// Rust cdylibs export Rust ABI entrypoints through a generated version script.
// Forward the loader symbol explicitly so the C++ factory remains visible to
// Fcitx's dlsym-based addon loader.
#[unsafe(no_mangle)]
pub extern "C" fn fcitx_addon_factory_instance() -> *mut c_void {
    bridge::ffi::addon_factory().cast()
}

#[derive(Clone, Debug, Default)]
struct Config {
    enabled: bool,
    base_url: String,
    model: String,
    api_key: String,
    reasoning_effort: String,
    dictionary_path: PathBuf,
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

struct Job {
    request_id: u64,
    target: String,
    items: Vec<JobItem>,
    ready_at: Instant,
}

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

#[derive(Clone, Debug)]
struct DictionaryEntry {
    english: String,
    japanese: String,
}

#[derive(Default)]
struct ShortDictionary {
    entries: HashMap<String, DictionaryEntry>,
    loaded_path: Option<PathBuf>,
}

impl ShortDictionary {
    fn ensure_loaded(&mut self, path: &Path) {
        if path.as_os_str().is_empty() || self.loaded_path.as_deref() == Some(path) {
            return;
        }
        self.entries.clear();
        self.loaded_path = Some(path.to_path_buf());
        let Ok(file) = fs::File::open(path) else {
            return;
        };
        for (line_number, line) in BufReader::new(file).lines().enumerate() {
            let Ok(line) = line else { continue };
            if line_number == 0 {
                continue;
            }
            let Some(fields) = parse_csv_line(line.trim_start_matches('\u{feff}')) else {
                continue;
            };
            if fields.len() != 5 {
                continue;
            }
            let source = fields[1].trim();
            let length = source.chars().count();
            if !(1..=4).contains(&length) || !source.chars().any(is_han) {
                continue;
            }
            let english = first_gloss(&fields[3], false);
            let japanese = first_gloss(&fields[4], true);
            if english.is_empty() && japanese.is_empty() {
                continue;
            }
            let candidate = DictionaryEntry { english, japanese };
            self.entries
                .entry(source.to_string())
                .and_modify(|current| {
                    if entry_score(&candidate) < entry_score(current) {
                        *current = candidate.clone();
                    }
                })
                .or_insert(candidate);
        }
    }

    fn lookup(&self, target: &str, source: &str) -> Option<String> {
        let entry = self.entries.get(source)?;
        if target == "English" {
            return (!entry.english.is_empty()).then(|| entry.english.clone());
        }
        if !target.starts_with("Japanese") || entry.japanese.is_empty() {
            return None;
        }
        if target != "JapaneseWithKana" {
            return Some(entry.japanese.clone());
        }
        let reading = kana_reading(&entry.japanese);
        match reading {
            Some(reading) if reading != entry.japanese => {
                Some(format!("{}（{}）", entry.japanese, reading))
            }
            _ => Some(entry.japanese.clone()),
        }
    }
}

fn entry_score(entry: &DictionaryEntry) -> usize {
    entry.english.chars().count() + entry.japanese.chars().count()
}

fn parse_csv_line(line: &str) -> Option<Vec<String>> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut characters = line.chars().peekable();
    let mut quoted = false;
    while let Some(character) = characters.next() {
        match character {
            '"' if quoted && characters.peek() == Some(&'"') => {
                field.push('"');
                characters.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => fields.push(std::mem::take(&mut field)),
            _ => field.push(character),
        }
    }
    if quoted {
        return None;
    }
    fields.push(field);
    Some(fields)
}

fn first_gloss(value: &str, japanese: bool) -> String {
    let separators: &[char] = if japanese {
        &['／', '/', '；', ';']
    } else {
        &['/', ';']
    };
    let mut gloss = value
        .split(separators)
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    loop {
        let closing = if gloss.starts_with('(') {
            gloss.find(')').map(|index| index + 1)
        } else if gloss.starts_with('（') {
            gloss.find('）').map(|index| index + '）'.len_utf8())
        } else {
            None
        };
        let Some(closing) = closing else { break };
        gloss = gloss[closing..].trim().to_string();
    }
    if gloss.chars().count() > 48 {
        String::new()
    } else {
        gloss
    }
}

fn is_han(character: char) -> bool {
    matches!(character as u32, 0x3400..=0x9fff | 0x20000..=0x323af)
}

fn kana_reading(text: &str) -> Option<String> {
    if text.chars().count() > 32 {
        return None;
    }
    let hiragana: String = text
        .chars()
        .map(|character| {
            let codepoint = character as u32;
            if (0x30a1..=0x30f6).contains(&codepoint) {
                char::from_u32(codepoint - 0x60).unwrap_or(character)
            } else {
                character
            }
        })
        .collect();
    if !hiragana.chars().any(is_han) {
        return is_kana_reading(&hiragana).then_some(hiragana);
    }

    let mut child = Command::new("kakasi")
        .args(["-JH", "-KH", "-iutf8", "-outf8"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    if stdin.write_all(text.as_bytes()).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    drop(stdin);

    let deadline = Instant::now() + Duration::from_millis(200);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(2));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    if !status.success() {
        return None;
    }
    let mut output = Vec::new();
    child.stdout.take()?.read_to_end(&mut output).ok()?;
    let reading = String::from_utf8(output)
        .ok()?
        .split_whitespace()
        .collect::<String>();
    (!reading.is_empty() && is_kana_reading(&reading)).then_some(reading)
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
    dictionary: ShortDictionary,
    pending: Option<Job>,
    stopping: bool,
    epoch: u64,
    results: Vec<TranslationResult>,
    cooldown_until: Option<Instant>,
    structured_output_unsupported: HashSet<String>,
}

pub struct Translator {
    shared: Arc<(Mutex<State>, Condvar)>,
    worker: Option<JoinHandle<()>>,
    receiver: UnixStream,
    notifier: UnixStream,
}

fn new_translator() -> std::io::Result<Box<Translator>> {
    let (receiver, notifier) = UnixStream::pair()?;
    receiver.set_nonblocking(true)?;
    notifier.set_nonblocking(true)?;
    let worker_notifier = notifier.try_clone()?;
    let shared = Arc::new((Mutex::new(State::default()), Condvar::new()));
    let worker_shared = Arc::clone(&shared);
    let worker = thread::Builder::new()
        .name("fcitx5-translator".into())
        .spawn(move || worker_loop(worker_shared, worker_notifier))?;
    Ok(Box::new(Translator {
        shared,
        worker: Some(worker),
        receiver,
        notifier,
    }))
}

// Called while holding the state lock, after publishing a result. A full
// nonblocking socket already has a wakeup pending, so notifications coalesce.
fn notify(mut socket: &UnixStream) {
    loop {
        match socket.write(&[1]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            _ => break,
        }
    }
}

impl Drop for Translator {
    fn drop(&mut self) {
        let (lock, condvar) = &*self.shared;
        {
            let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
            state.stopping = true;
            state.pending = None;
            condvar.notify_all();
        }
        // Blocking HTTP attempts finish under their configured timeouts.
        // Keep both socket endpoints alive until the worker has exited.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn cache_key(config: &Config, target: &str, source: &str) -> String {
    serde_json::to_string(&[
        config.base_url.trim_end_matches('/'),
        config.model.as_str(),
        config.reasoning_effort.as_str(),
        target,
        source,
    ])
    .unwrap_or_default()
}

fn backend_key(config: &Config) -> String {
    format!(
        "{}\x1f{}",
        config.base_url.trim_end_matches('/'),
        config.model
    )
}

#[derive(Debug)]
struct TranslationBatch {
    translations: Vec<(u32, String, String)>,
    structured_output_unsupported: bool,
}

#[derive(Debug)]
struct TranslationFailure {
    message: String,
    structured_output_unsupported: bool,
}

enum AttemptError {
    StructuredOutputUnsupported,
    Other(String),
}

fn worker_loop(shared: Arc<(Mutex<State>, Condvar)>, notifier: UnixStream) {
    let (lock, condvar) = &*shared;
    loop {
        let (job, config, epoch) = {
            let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
            loop {
                if state.stopping {
                    return;
                }
                if let Some(job) = &state.pending {
                    let remaining = job.ready_at.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    state = condvar
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|error| error.into_inner())
                        .0;
                } else {
                    state = condvar
                        .wait(state)
                        .unwrap_or_else(|error| error.into_inner());
                }
            }
            let job = state.pending.take().expect("pending job is ready");
            (job, state.config.clone(), state.epoch)
        };

        let result = {
            let state = lock.lock().unwrap_or_else(|error| error.into_inner());
            if state.stopping || state.epoch != epoch {
                continue;
            }
            if state
                .cooldown_until
                .is_some_and(|deadline| deadline > Instant::now())
            {
                Err(TranslationFailure {
                    message: "translation service is cooling down after an error".to_string(),
                    structured_output_unsupported: false,
                })
            } else {
                let try_structured_output = !state
                    .structured_output_unsupported
                    .contains(&backend_key(&config));
                drop(state);
                translate_with_fallback(&config, &job.target, &job.items, try_structured_output)
            }
        };

        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        if state.stopping {
            return;
        }
        if state.epoch != epoch {
            continue;
        }
        let mut output = TranslationResult {
            request_id: job.request_id,
            translations: Vec::new(),
            error: String::new(),
        };
        match result {
            Ok(batch) => {
                state.cooldown_until = None;
                if batch.structured_output_unsupported {
                    state
                        .structured_output_unsupported
                        .insert(backend_key(&config));
                }
                let cache_path = state.config.cache_path.clone();
                let cache_limit = state.config.cache_entries;
                for (index, source, text) in batch.translations {
                    let key = cache_key(&config, &job.target, &source);
                    state.cache.put(key, text.clone(), cache_limit);
                    output.translations.push(Translation { index, text });
                }
                let _ = state.cache.save(&cache_path);
            }
            Err(error) => {
                if error.structured_output_unsupported {
                    state
                        .structured_output_unsupported
                        .insert(backend_key(&config));
                }
                state.cooldown_until = Some(Instant::now() + Duration::from_secs(5));
                output.error = error.message;
            }
        }

        state.results.push(output);
        notify(&notifier);
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

fn translate_with_fallback(
    config: &Config,
    target: &str,
    items: &[JobItem],
    try_structured_output: bool,
) -> Result<TranslationBatch, TranslationFailure> {
    if try_structured_output {
        match translate_attempt(config, target, items, true) {
            Ok(translations) => {
                return Ok(TranslationBatch {
                    translations,
                    structured_output_unsupported: false,
                });
            }
            Err(AttemptError::StructuredOutputUnsupported) => {
                return translate_attempt(config, target, items, false)
                    .map(|translations| TranslationBatch {
                        translations,
                        structured_output_unsupported: true,
                    })
                    .map_err(|error| TranslationFailure {
                        message: match error {
                            AttemptError::StructuredOutputUnsupported => {
                                "structured output is unsupported".to_string()
                            }
                            AttemptError::Other(message) => message,
                        },
                        structured_output_unsupported: true,
                    });
            }
            Err(AttemptError::Other(message)) => {
                return Err(TranslationFailure {
                    message,
                    structured_output_unsupported: false,
                });
            }
        }
    }

    translate_attempt(config, target, items, false)
        .map(|translations| TranslationBatch {
            translations,
            structured_output_unsupported: false,
        })
        .map_err(|error| TranslationFailure {
            message: match error {
                AttemptError::StructuredOutputUnsupported => {
                    "structured output is unsupported".to_string()
                }
                AttemptError::Other(message) => message,
            },
            structured_output_unsupported: false,
        })
}

fn structured_output_format(with_kana: bool) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "index".into(),
        json!({
            "type": "integer",
            "description": "The unchanged index supplied with the candidate."
        }),
    );
    properties.insert(
        "text".into(),
        json!({
            "type": "string",
            "description": "A concise dictionary-style translation."
        }),
    );
    let mut required = vec!["index", "text"];
    if with_kana {
        properties.insert(
            "reading".into(),
            json!({
                "type": "string",
                "description": "The complete pronunciation of the Japanese translation in hiragana, without romaji."
            }),
        );
        required.push("reading");
    }
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "candidate_translations",
            "strict": true,
            "schema": {
                "type": "object",
                "properties": {
                    "translations": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": properties,
                            "required": required,
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["translations"],
                "additionalProperties": false
            }
        }
    })
}

fn translate_attempt(
    config: &Config,
    target: &str,
    items: &[JobItem],
    structured_output: bool,
) -> Result<Vec<(u32, String, String)>, AttemptError> {
    if !config.ready() {
        return Err(AttemptError::Other(
            "translation API is not fully configured".into(),
        ));
    }
    let endpoint = validate_url(&config.base_url).map_err(AttemptError::Other)?;
    let candidates: Vec<_> = items
        .iter()
        .map(|item| json!({"index": item.index, "text": item.source}))
        .collect();
    let with_kana = target == "JapaneseWithKana";
    let target_name = if with_kana { "Japanese" } else { target };
    let schema = if with_kana {
        r#"{"translations":[{"index":0,"text":"日本語訳","reading":"にほんごやく"}]}"#
    } else {
        r#"{"translations":[{"index":0,"text":"..."}]}"#
    };
    let reading_instruction = if with_kana {
        " For every Japanese translation, include its complete pronunciation in hiragana in the reading field. Do not use romaji."
    } else {
        ""
    };
    let semantic_instruction = format!(
        "Translate each Simplified Chinese input-method candidate into {target_name}. Return concise dictionary-style translations, no explanations. Put every translation in a field named text.{reading_instruction} Preserve every supplied index."
    );
    let system = if structured_output {
        semantic_instruction
    } else {
        format!("{semantic_instruction} Respond with JSON only using {schema}.")
    };
    let mut body = json!({
        "model": config.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": serde_json::to_string(&candidates).unwrap_or_default()}
        ],
        "stream": false
    });
    if structured_output {
        body["response_format"] = structured_output_format(with_kana);
    }
    if !config.reasoning_effort.is_empty() {
        body["reasoning_effort"] = Value::String(config.reasoning_effort.clone());
    }
    let client = Client::builder()
        .connect_timeout(config.timeout.min(Duration::from_secs(2)))
        .timeout(config.timeout)
        .build()
        .map_err(|error| AttemptError::Other(error.to_string()))?;
    let response = client
        .post(endpoint)
        .bearer_auth(&config.api_key)
        .json(&body)
        .send()
        .map_err(|error| AttemptError::Other(format!("translation request failed: {error}")))?;
    let status = response.status();
    let response_body = response
        .text()
        .map_err(|error| AttemptError::Other(format!("failed to read response: {error}")))?;
    if !status.is_success() {
        let lower = response_body.to_ascii_lowercase();
        let format_rejected = structured_output
            && matches!(status.as_u16(), 400 | 404 | 405 | 415 | 422)
            && (lower.contains("response_format")
                || lower.contains("json_schema")
                || lower.contains("structured output")
                || lower.contains("structured_output"));
        let message = http_error_message(status, &response_body);
        if format_rejected {
            return Err(AttemptError::StructuredOutputUnsupported);
        }
        return Err(AttemptError::Other(message));
    }
    let payload: Value = serde_json::from_str(&response_body)
        .map_err(|error| AttemptError::Other(format!("invalid chat completion JSON: {error}")))?;
    let content = payload
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AttemptError::Other("chat completion did not contain message content".to_string())
        })?;
    parse_translations(content, items, with_kana).map_err(AttemptError::Other)
}

fn http_error_message(status: reqwest::StatusCode, response_body: &str) -> String {
    let detail = serde_json::from_str::<Value>(response_body)
        .ok()
        .and_then(|payload| {
            payload
                .pointer("/error/message")
                .or_else(|| payload.get("message"))
                .or_else(|| payload.get("error"))
                .and_then(Value::as_str)
                .map(|message| message.split_whitespace().collect::<Vec<_>>().join(" "))
        })
        .filter(|message| !message.is_empty())
        .map(|message| {
            let truncated = message.chars().take(300).collect::<String>();
            if message.chars().count() > 300 {
                format!("{truncated}…")
            } else {
                truncated
            }
        });
    match detail {
        Some(detail) => format!("translation service returned HTTP {status}: {detail}"),
        None => format!("translation service returned HTTP {status}"),
    }
}

fn translation_values(content: &str) -> Result<Vec<Value>, String> {
    let trimmed = content.trim();
    let mut candidates = Vec::new();
    if matches!(trimmed.as_bytes().first(), Some(b'{' | b'[')) {
        candidates.push(trimmed);
    }
    for (open, close) in [('{', '}'), ('[', ']')] {
        if let (Some(start), Some(end)) = (content.find(open), content.rfind(close))
            && start <= end
        {
            let candidate = &content[start..=end];
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    if candidates.is_empty() {
        return Err("translation response did not contain JSON".into());
    }

    let mut parsed_json = false;
    let mut last_error = None;
    for candidate in candidates {
        match serde_json::from_str::<Value>(candidate) {
            Ok(Value::Array(values)) => return Ok(values),
            Ok(Value::Object(mut object)) => {
                parsed_json = true;
                if let Some(Value::Array(values)) = object.remove("translations") {
                    return Ok(values);
                }
            }
            Ok(_) => parsed_json = true,
            Err(error) => last_error = Some(error),
        }
    }
    if parsed_json {
        Err("translation JSON has no translations array".into())
    } else {
        Err(format!(
            "invalid translation JSON: {}",
            last_error.expect("JSON candidate disappeared")
        ))
    }
}

fn parse_translations(
    content: &str,
    items: &[JobItem],
    with_kana: bool,
) -> Result<Vec<(u32, String, String)>, String> {
    let values = translation_values(content)?;
    let sources: HashMap<u32, &str> = items
        .iter()
        .map(|item| (item.index, item.source.as_str()))
        .collect();
    let mut output = Vec::new();
    for value in &values {
        let Some(index) = value.get("index").and_then(Value::as_u64) else {
            continue;
        };
        let Ok(index) = u32::try_from(index) else {
            continue;
        };
        let Some(source) = sources.get(&index) else {
            continue;
        };
        let Some(raw) = value
            .get("text")
            .or_else(|| value.get("translation"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let mut text = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.is_empty() || text.chars().count() > 256 {
            continue;
        }
        if with_kana {
            let reading = value
                .get("reading")
                .and_then(Value::as_str)
                .map(|reading| reading.split_whitespace().collect::<Vec<_>>().join(""))
                .filter(|reading| {
                    !reading.is_empty()
                        && reading.chars().count() <= 256
                        && is_kana_reading(reading)
                });
            if let Some(reading) = reading.filter(|reading| reading != &text) {
                text.push('（');
                text.push_str(&reading);
                text.push('）');
            }
        }
        output.push((index, (*source).to_string(), text));
    }
    if output.is_empty() {
        return Err("translation JSON contained no usable translations".into());
    }
    Ok(output)
}

fn is_kana_reading(reading: &str) -> bool {
    reading.chars().all(|character| {
        matches!(
            character as u32,
            0x3040..=0x30ff | 0x31f0..=0x31ff | 0x3000..=0x303f
        ) || matches!(character, '-' | '・' | '＝')
    })
}

impl Translator {
    fn configure(&self, config: BackendConfig) {
        let (lock, condvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        Self::invalidate(&mut state);
        state.config = Config {
            enabled: config.enabled,
            base_url: config.base_url,
            model: config.model,
            api_key: config.api_key,
            reasoning_effort: config.reasoning_effort,
            dictionary_path: config.dictionary_path.into(),
            timeout: Duration::from_millis(config.timeout_ms.clamp(500, 15_000)),
            debounce: Duration::from_millis(config.debounce_ms.min(2_000)),
            cache_entries: config.cache_entries.min(100_000),
            cache_path: config.cache_path.into(),
        };
        let path = state.config.cache_path.clone();
        let limit = state.config.cache_entries;
        state.cache.ensure_loaded(&path, limit);
        let dictionary_path = state.config.dictionary_path.clone();
        state.dictionary.ensure_loaded(&dictionary_path);
        state.cooldown_until = None;
        condvar.notify_all();
    }

    fn lookup(&self, target: &str, source: &str) -> String {
        let (lock, _) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        let key = cache_key(&state.config, target, source);
        state
            .cache
            .get(&key)
            .or_else(|| state.dictionary.lookup(target, source))
            .unwrap_or_default()
    }

    fn submit(&self, request_id: u64, target: &str, items: Vec<JobItem>) {
        let (lock, condvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        if items.is_empty() || items.len() > 64 || !state.config.ready() {
            state.results.push(TranslationResult {
                request_id,
                translations: Vec::new(),
                error: "invalid translation request or incomplete configuration".into(),
            });
            notify(&self.notifier);
            return;
        }
        let job = Job {
            request_id,
            target: target.into(),
            items,
            ready_at: Instant::now() + state.config.debounce,
        };
        if let Some(replaced) = state.pending.replace(job) {
            state.results.push(TranslationResult {
                request_id: replaced.request_id,
                translations: Vec::new(),
                error: "translation request was superseded".into(),
            });
            notify(&self.notifier);
        }
        condvar.notify_all();
    }

    // Callers discard their matching request bookkeeping when invalidating.
    // In-flight responses must not repopulate the cache or result queue.
    fn invalidate(state: &mut State) {
        state.epoch = state.epoch.wrapping_add(1);
        state.pending = None;
        state.results.clear();
    }

    fn cancel_requests(&self) {
        let (lock, condvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        Self::invalidate(&mut state);
        condvar.notify_all();
    }

    fn clear_cache(&self) {
        let (lock, condvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        Self::invalidate(&mut state);
        state.cache.entries.clear();
        let _ = fs::remove_file(&state.config.cache_path);
        condvar.notify_all();
    }

    fn result_fd(&self) -> i32 {
        self.receiver.as_raw_fd()
    }

    fn take_results(&self) -> Vec<TranslationResult> {
        let (lock, _) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        // Drain notifications and results under the producer lock. A producer
        // arriving after this drain always writes a fresh wakeup.
        let mut receiver = &self.receiver;
        let mut buffer = [0; 256];
        loop {
            match receiver.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        std::mem::take(&mut state.results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;

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

    fn read_http_request(stream: &mut TcpStream) -> String {
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
        String::from_utf8(request).unwrap()
    }

    fn request_json(request: &str) -> Value {
        serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
    }

    fn write_json_response(stream: &mut TcpStream, status: &str, payload: &Value) {
        let payload = serde_json::to_string(payload).unwrap();
        write!(
            stream,
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        )
        .unwrap();
    }

    fn backend_config(base_url: &str) -> BackendConfig {
        BackendConfig {
            enabled: true,
            base_url: base_url.into(),
            model: "test".into(),
            api_key: "test-key".into(),
            timeout_ms: 2_000,
            cache_entries: 16,
            ..BackendConfig::default()
        }
    }

    fn translation_response(text: &str) -> Value {
        json!({"choices": [{"message": {"content": json!({
            "translations": [{"index": 0, "text": text}]
        }).to_string()}}]})
    }

    #[test]
    fn notifications_rearm_and_instances_are_independent() {
        let first = new_translator().unwrap();
        let second = new_translator().unwrap();
        assert_ne!(first.result_fd(), second.result_fd());
        // Incomplete configuration produces queued errors without networking.
        for request_id in 1..=2 {
            first.submit(request_id, "English", items());
            let results = bridge::ffi::cpp_wait_for_results(&first);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].request_id, request_id);
            assert!(!results[0].error.is_empty());
            assert!(first.take_results().is_empty());
            assert!(second.take_results().is_empty());
        }
        drop(first);
        second.submit(3, "English", items());
        assert_eq!(bridge::ffi::cpp_wait_for_results(&second)[0].request_id, 3);
    }

    #[test]
    fn full_notification_socket_does_not_block_or_lose_results() {
        let translator = new_translator().unwrap();
        // Fill the wakeup socket, then publish a result while it is full.
        let mut socket = &translator.notifier;
        loop {
            match socket.write(&[1; 4096]) {
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("failed to fill notification socket: {error}"),
            }
        }
        translator.submit(7, "English", items());
        let results = bridge::ffi::cpp_wait_for_results(&translator);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].request_id, 7);
        translator.submit(8, "English", items());
        assert_eq!(
            bridge::ffi::cpp_wait_for_results(&translator)[0].request_id,
            8
        );
    }

    #[test]
    fn superseded_and_cancelled_requests_do_not_reach_the_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let translator = new_translator().unwrap();
        let mut config = backend_config(&format!("http://{}", listener.local_addr().unwrap()));
        config.debounce_ms = 2_000;
        translator.configure(config);
        translator.submit(1, "English", items());
        translator.submit(2, "English", items());
        let results = translator.take_results();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].request_id, 1);
        assert!(results[0].error.contains("superseded"));
        translator.cancel_requests();
        assert!(translator.take_results().is_empty());
        drop(translator);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn worker_results_cross_cpp_bridge_and_cache_is_per_instance() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_http_request(&mut stream);
            write_json_response(&mut stream, "200 OK", &translation_response("単語を覚える"));
        });
        let translator = new_translator().unwrap();
        translator.configure(backend_config(&base_url));
        translator.submit(42, "Japanese", items());
        let results = bridge::ffi::cpp_wait_for_results(&translator);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].request_id, 42);
        assert!(results[0].error.is_empty(), "{}", results[0].error);
        assert_eq!(results[0].translations[0].text, "単語を覚える");
        assert_eq!(translator.lookup("Japanese", "背单词"), "単語を覚える");
        let other = new_translator().unwrap();
        other.configure(backend_config(&base_url));
        assert!(other.lookup("Japanese", "背单词").is_empty());
        translator.clear_cache();
        assert!(translator.lookup("Japanese", "背单词").is_empty());
        server.join().unwrap();
    }

    #[test]
    fn invalidation_discards_in_flight_results_and_cache_writes() {
        for reconfigure in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let (started_tx, started_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                read_http_request(&mut stream);
                started_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                write_json_response(&mut stream, "200 OK", &translation_response("obsolete"));
                let (mut stream, _) = listener.accept().unwrap();
                read_http_request(&mut stream);
                write_json_response(&mut stream, "200 OK", &translation_response("fresh"));
            });
            let translator = new_translator().unwrap();
            translator.configure(backend_config(&base_url));
            translator.submit(1, "English", items());
            started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if reconfigure {
                translator.configure(backend_config(&base_url));
            } else {
                translator.clear_cache();
            }
            translator.submit(
                2,
                "English",
                vec![JobItem {
                    index: 0,
                    source: "新词".into(),
                }],
            );
            release_tx.send(()).unwrap();
            let results = bridge::ffi::cpp_wait_for_results(&translator);
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].request_id, 2);
            assert!(results[0].error.is_empty(), "{}", results[0].error);
            assert!(translator.lookup("English", "背单词").is_empty());
            assert_eq!(translator.lookup("English", "新词"), "fresh");
            server.join().unwrap();
        }
    }

    #[test]
    fn dropping_translator_joins_active_worker() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let translator = new_translator().unwrap();
        translator.configure(backend_config(&format!(
            "http://{}",
            listener.local_addr().unwrap()
        )));
        translator.submit(1, "English", items());
        let (mut stream, _) = listener.accept().unwrap();
        read_http_request(&mut stream);
        let shared = Arc::clone(&translator.shared);
        let dropping = thread::spawn(move || drop(translator));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !shared.0.lock().unwrap().stopping {
            assert!(Instant::now() < deadline, "drop did not stop worker");
            thread::yield_now();
        }
        assert!(!dropping.is_finished());
        write_json_response(&mut stream, "200 OK", &translation_response("obsolete"));
        dropping.join().unwrap();
        let state = shared.0.lock().unwrap();
        assert!(state.results.is_empty());
        assert!(state.cache.entries.is_empty());
        assert_eq!(Arc::strong_count(&shared), 1, "worker still owns state");
    }

    #[test]
    fn parses_json_and_code_fences() {
        let result = parse_translations(
            "```json\n{\"translations\":[{\"index\":0,\"text\":\"recite words\"},{\"index\":2,\"text\":\"bed sheet\"}]}\n```",
            &items(),
            false,
        )
        .unwrap();
        assert_eq!(result[0], (0, "背单词".into(), "recite words".into()));
        assert_eq!(result[1].0, 2);
    }

    #[test]
    fn parses_top_level_array_from_schema_ignoring_backend() {
        let result = parse_translations(
            r#"[{"index":0,"text":"単語を覚える","reading":"たんごをおぼえる"}]"#,
            &items(),
            true,
        )
        .unwrap();
        assert_eq!(
            result[0],
            (
                0,
                "背单词".into(),
                "単語を覚える（たんごをおぼえる）".into()
            )
        );
    }

    #[test]
    fn accepts_translation_field_from_schema_ignoring_backend() {
        let result = parse_translations(
            r#"[{"index":0,"translation":"日本","reading":"にほん"},{"index":2,"translation":"日","reading":"ひ"}]"#,
            &items(),
            true,
        )
        .unwrap();
        assert_eq!(result[0], (0, "背单词".into(), "日本（にほん）".into()));
        assert_eq!(result[1], (2, "被单".into(), "日（ひ）".into()));
    }

    #[test]
    fn includes_safe_json_detail_in_http_errors() {
        assert_eq!(
            http_error_message(
                reqwest::StatusCode::NOT_FOUND,
                r#"{"error":{"message":"model is not available"}}"#,
            ),
            "translation service returned HTTP 404 Not Found: model is not available"
        );
    }

    #[test]
    fn rejects_unknown_and_empty_results() {
        assert!(
            parse_translations(
                "{\"translations\":[{\"index\":9,\"text\":\"wrong\"}]}",
                &items(),
                false,
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
    fn short_dictionary_parses_csv_and_returns_instant_glosses() {
        let unique = format!(
            "fcitx-short-dict-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let directory = std::env::temp_dir().join(unique);
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("dict.csv");
        fs::write(
            &path,
            "\u{feff}kanji,kanji_s,pron,means,means_ja\n測試,测试,ce4 shi4,\"to test (machinery, etc)/a test\",\"(機械などを)テストする／ベータ版\"\n",
        )
        .unwrap();

        let mut dictionary = ShortDictionary::default();
        dictionary.ensure_loaded(&path);
        assert_eq!(
            dictionary.lookup("English", "测试"),
            Some("to test (machinery, etc)".into())
        );
        assert_eq!(
            dictionary.lookup("Japanese", "测试"),
            Some("テストする".into())
        );
        assert_eq!(
            dictionary.lookup("JapaneseWithKana", "测试"),
            Some("テストする（てすとする）".into())
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn japanese_dictionary_gloss_splits_ascii_slashes() {
        assert_eq!(first_gloss("事柄/物事/仕事/出来事", true), "事柄");
        assert_eq!(first_gloss("時／時刻／時間", true), "時");
    }

    #[test]
    fn only_allows_secure_or_loopback_urls() {
        assert!(validate_url("https://example.com/v1").is_ok());
        assert!(validate_url("http://127.0.0.1:8000/v1").is_ok());
        assert!(validate_url("http://example.com/v1").is_err());
    }

    #[test]
    fn cpp_output_decoration_uses_text_copy() {
        assert!(bridge::ffi::cpp_self_test());
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
            sender.send(read_http_request(&mut stream)).unwrap();
            let content = "{\"translations\":[{\"index\":0,\"text\":\"単語を暗記する\",\"reading\":\"たんごをあんきする\"},{\"index\":2,\"text\":\"シーツ\",\"reading\":\"しーつ\"}]}";
            write_json_response(
                &mut stream,
                "200 OK",
                &json!({
                    "choices": [{"message": {"content": content}}]
                }),
            );
        });

        let config = Config {
            enabled: true,
            base_url: format!("http://{address}/v1"),
            model: "test-model".into(),
            api_key: "secret-token".into(),
            reasoning_effort: "none".into(),
            timeout: Duration::from_secs(2),
            ..Config::default()
        };
        let batch = translate_with_fallback(&config, "JapaneseWithKana", &items(), true).unwrap();
        let output = batch.translations;
        assert_eq!(
            output[0],
            (
                0,
                "背单词".into(),
                "単語を暗記する（たんごをあんきする）".into()
            )
        );
        let request = receiver.recv().unwrap();
        assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer secret-token")
        );
        assert!(request.contains("test-model"));
        assert!(request.contains("Japanese"));
        let body = request_json(&request);
        assert_eq!(body["reasoning_effort"], "none");
        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["properties"]["translations"]["items"]
                ["required"],
            json!(["index", "text", "reading"])
        );
        assert!(
            !body["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("Respond with JSON")
        );
        server.join().unwrap();
    }

    #[test]
    fn falls_back_when_backend_rejects_json_schema() {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("failed to bind mock HTTP server: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            sender.send(read_http_request(&mut first)).unwrap();
            write_json_response(
                &mut first,
                "400 Bad Request",
                &json!({"error": {"message": "response_format json_schema is not supported"}}),
            );

            let (mut second, _) = listener.accept().unwrap();
            sender.send(read_http_request(&mut second)).unwrap();
            let content = "{\"translations\":[{\"index\":0,\"text\":\"recite words\"},{\"index\":2,\"text\":\"bed sheet\"}]}";
            write_json_response(
                &mut second,
                "200 OK",
                &json!({"choices": [{"message": {"content": content}}]}),
            );
        });

        let config = Config {
            enabled: true,
            base_url: format!("http://{address}/v1"),
            model: "legacy-model".into(),
            api_key: "secret-token".into(),
            timeout: Duration::from_secs(2),
            ..Config::default()
        };
        let batch = translate_with_fallback(&config, "English", &items(), true).unwrap();
        assert!(batch.structured_output_unsupported);
        assert_eq!(batch.translations[0].2, "recite words");

        let first = request_json(&receiver.recv().unwrap());
        let second = request_json(&receiver.recv().unwrap());
        assert_eq!(first["response_format"]["type"], "json_schema");
        assert!(second.get("response_format").is_none());
        assert!(
            second["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("Respond with JSON")
        );
        server.join().unwrap();
    }

    #[test]
    fn formats_valid_kana_reading_and_ignores_invalid_reading() {
        let result = parse_translations(
            "{\"translations\":[{\"index\":0,\"text\":\"暗記する\",\"reading\":\"あんきする\"},{\"index\":2,\"text\":\"シーツ\",\"reading\":\"sheet\"}]}",
            &items(),
            true,
        )
        .unwrap();
        assert_eq!(result[0].2, "暗記する（あんきする）");
        assert_eq!(result[1].2, "シーツ");
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
