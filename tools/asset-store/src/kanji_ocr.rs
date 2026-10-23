//! Необязательные предложения OCR: никогда не источник доверия.

pub(crate) const STRUCTURE_PROMPT: &str = r#"Perform exact-shape OCR of the ONE isolated Japanese kanji shown in the image.
Inspect the entire glyph: outer shape, enclosing strokes, inner marks, and all left/right and top/bottom components.
Distinguish a complete kanji from a radical or an incomplete lookalike. Check whether enclosing shapes are open or closed.
Transcribe the actual visible character exactly, including its Japanese glyph form; do not silently convert it to a traditional, simplified, or other variant.
Do not use meaning, pronunciation, frequency, or a guessed word as evidence.
If fine visual details do not support a reliable identification, return "?".
Return ONLY a JSON object with one string field named "observed". No explanation."#;

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::{Host, Url};

use crate::batch::{BatchCandidate, BatchRuntime, KanjiBatch};
use crate::hashing::sha256_hex;
use crate::kanji_domain::parse_kanji_character;
use crate::kanji_validator::prepare_ocr_image;
use crate::model::{AssetIdentity, DetectedFormat, SemanticStatus};
use crate::{AssetError, ErrorCode};

const PROMPT_VERSION: &str = "structure-v1";
const TRANSFORM_VERSION: &str = "v6-representative-white-lanczos3-2x-image025-v1";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationOptions {
    temperature: u32,
    num_predict: u32,
    num_ctx: u32,
    think: bool,
    stream: bool,
}

const GENERATION: GenerationOptions = GenerationOptions {
    temperature: 0,
    num_predict: 128,
    num_ctx: 4096,
    think: false,
    stream: false,
};

#[derive(Debug, Clone, clap::Args)]
pub struct OcrOptions {
    /// Имя установленной vision-модели; автоматической загрузки нет.
    #[arg(long)]
    pub model: String,
    /// Базовый URL Ollama. Для WSL укажите доступный адрес Windows-host.
    #[arg(long, default_value = "http://127.0.0.1:11434")]
    pub endpoint: String,
    /// Разрешает отправку изображений адресу вне буквального loopback (в том числе WSL-host).
    #[arg(long)]
    pub allow_remote: bool,
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=600))]
    pub timeout_seconds: u64,
    /// Явный повтор вместо кэша, включая сохранённые отказы.
    #[arg(long)]
    pub rescan: bool,
}

impl OcrOptions {
    pub(crate) fn validate(&self) -> Result<Url, AssetError> {
        let url = Url::parse(&self.endpoint).map_err(|_| invalid("неверный URL Ollama"))?;
        if url.scheme() != "http"
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "нужен базовый HTTP URL Ollama без credentials, пути, query и fragment",
            ));
        }
        // Имена DNS не считаем loopback: DNS/hosts могут направить их наружу.
        let loopback = match url.host() {
            Some(Host::Ipv4(ip)) => ip.is_loopback(),
            Some(Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !loopback && !self.allow_remote {
            return Err(invalid(
                "адрес вне loopback требует --allow-remote: изображения будут отправлены этому серверу",
            ));
        }
        if self.model.trim() != self.model
            || self.model.is_empty()
            || self.model.len() > 256
            || self.model.chars().any(char::is_control)
            || !(1..=600).contains(&self.timeout_seconds)
        {
            return Err(invalid("неверная модель или timeout OCR"));
        }
        Ok(url)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OcrConfig {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub model_digest: String,
    pub prompt_version: String,
    pub prompt_sha256: String,
    pub transform_version: String,
    pub timeout_seconds: u64,
    pub generation: GenerationOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OcrSuggestion {
    pub identity: AssetIdentity,
    pub source_sha256: String,
    pub config: OcrConfig,
    pub observed: Option<String>,
    pub matches_expected: bool,
    pub failure: Option<String>,
    pub format: DetectedFormat,
    pub representative_frame: Option<usize>,
    pub dimensions: Option<[u32; 2]>,
    pub prepared_sha256: Option<String>,
    pub timestamp_unix_seconds: u64,
}

impl OcrSuggestion {
    pub(crate) fn key(&self) -> String {
        cache_key(&self.identity, &self.source_sha256, &self.config)
    }

    pub(crate) fn group(&self) -> u8 {
        if self.failure.is_some() || self.observed.as_deref().is_none_or(|v| v == "?") {
            2
        } else if self.matches_expected {
            0
        } else {
            1
        }
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "observed={}; совпадение={}; отказ={}; модель={} @ {}; prompt={}; transform={}",
            self.observed.as_deref().unwrap_or("нет"),
            self.matches_expected,
            self.failure.as_deref().unwrap_or("нет"),
            self.config.model,
            self.config.model_digest,
            self.config.prompt_version,
            self.config.transform_version
        )
    }
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn cache_key(identity: &AssetIdentity, sha: &str, config: &OcrConfig) -> String {
    sha256_hex(serde_json::to_vec(&(identity, sha, config)).expect("сериализация простых полей"))
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    records: BTreeMap<String, OcrSuggestion>,
    latest: BTreeMap<String, String>,
}

fn read_ledger(runtime: &BatchRuntime) -> Result<Ledger, AssetError> {
    match runtime.artifact_directory("ocr")?.read("suggestions.json") {
        Ok(bytes) => {
            let ledger: Ledger = serde_json::from_slice(&bytes)
                .map_err(|error| invalid(format!("повреждён OCR-кэш: {error}")))?;
            for (key, suggestion) in &ledger.records {
                crate::batch::validate_hash(&suggestion.source_sha256)?;
                if key != &suggestion.key()
                    || suggestion.matches_expected
                        != (suggestion.failure.is_none()
                            && suggestion.observed.as_deref()
                                == Some(suggestion.identity.key.as_str()))
                    || suggestion.observed.as_deref().is_some_and(|observed| {
                        observed != "?" && parse_kanji_character(observed).is_err()
                    })
                {
                    return Err(invalid("неверная привязка или Unicode OCR-кэша"));
                }
            }
            Ok(ledger)
        }
        Err(error) if error.code == ErrorCode::MissingAssetFile => Ok(Ledger::default()),
        Err(error) => Err(error),
    }
}

pub(crate) fn saved_suggestions(
    runtime: &BatchRuntime,
) -> Result<BTreeMap<AssetIdentity, OcrSuggestion>, AssetError> {
    let ledger = read_ledger(runtime)?;
    ledger
        .latest
        .values()
        .map(|key| {
            let suggestion = ledger
                .records
                .get(key)
                .ok_or_else(|| invalid("latest OCR ссылается на отсутствующую запись"))?;
            Ok((suggestion.identity.clone(), suggestion.clone()))
        })
        .collect()
}

pub(crate) fn saved_suggestions_or_empty(
    runtime: &BatchRuntime,
) -> BTreeMap<AssetIdentity, OcrSuggestion> {
    match saved_suggestions(runtime) {
        Ok(suggestions) => suggestions,
        Err(error) => {
            tracing::warn!(
                code = error.code.as_str(),
                "OCR-кэш недоступен; страница review будет построена без предложений"
            );
            BTreeMap::new()
        }
    }
}

fn save_suggestion(
    runtime: &BatchRuntime,
    ledger: &mut Ledger,
    suggestion: OcrSuggestion,
) -> Result<bool, AssetError> {
    let key = suggestion.key();
    if ledger.records.get(&key) == Some(&suggestion)
        && ledger.latest.get(&suggestion.identity.key) == Some(&key)
    {
        return Ok(false);
    }
    ledger
        .latest
        .insert(suggestion.identity.key.clone(), key.clone());
    ledger.records.insert(key, suggestion);
    let encoded = serde_json::to_vec(&ledger).map_err(|error| invalid(error.to_string()))?;
    runtime
        .artifact_directory("ocr")?
        .write("suggestions.json", &encoded)?;
    Ok(true)
}

#[derive(Debug, Default, Serialize)]
pub(crate) struct OcrScanReport {
    pub saved: usize,
    pub cache_hits: usize,
    pub chat_requests: usize,
    #[serde(skip)]
    pub failures: Vec<(AssetIdentity, String)>,
}

struct Ollama {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    base: Url,
    model: String,
}

impl Ollama {
    fn new(options: &OcrOptions) -> Result<Self, AssetError> {
        let base = options.validate()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| AssetError::io("runtime OCR", error))?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(options.timeout_seconds))
            .connect_timeout(Duration::from_secs(options.timeout_seconds.min(10)))
            .build()
            .map_err(|error| invalid(error.to_string()))?;
        Ok(Self {
            runtime,
            client,
            base,
            model: options.model.clone(),
        })
    }

    fn request(&self, path: &str, body: Option<Value>) -> Result<Value, String> {
        self.runtime.block_on(async {
            let url = self.base.join(path).map_err(|error| error.to_string())?;
            let request = match body {
                Some(body) => self
                    .client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(serde_json::to_vec(&body).map_err(|error| error.to_string())?),
                None => self.client.get(url),
            };
            let mut response = request.send().await.map_err(network_failure)?;
            if !response.status().is_success() {
                return Err(format!("ollama_http_{}", response.status().as_u16()));
            }
            if response
                .content_length()
                .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
            {
                return Err("response_too_large".into());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(network_failure)? {
                if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err("response_too_large".into());
                }
                bytes.extend_from_slice(&chunk);
            }
            serde_json::from_slice(&bytes).map_err(|_| "invalid_response_json".into())
        })
    }

    fn tags_digest(&self) -> Result<String, String> {
        let tags = self.request("/api/tags", None)?;
        let name = if self.model.contains(':') {
            self.model.clone()
        } else {
            format!("{}:latest", self.model)
        };
        let digest = tags
            .get("models")
            .and_then(Value::as_array)
            .and_then(|models| {
                models
                    .iter()
                    .find(|model| model["name"] == name || model["model"] == name)
            })
            .and_then(|model| model.get("digest"))
            .and_then(Value::as_str)
            .filter(|digest| {
                !digest.is_empty() && digest.len() <= 256 && !digest.chars().any(char::is_control)
            })
            .ok_or("model_not_installed_or_digest_missing")?
            .to_owned();
        Ok(digest)
    }

    fn model_digest(&self) -> Result<String, String> {
        let digest = self.tags_digest()?;
        let show = self.request("/api/show", Some(json!({"model": self.model})))?;
        if !show
            .get("capabilities")
            .and_then(Value::as_array)
            .is_some_and(|caps| caps.iter().any(|cap| cap == "vision"))
        {
            return Err("model_does_not_support_vision".into());
        }
        // Облачные модели Ollama не являются локальным OCR-провайдером.
        if show
            .get("remote_host")
            .is_some_and(|v| !v.is_null() && v != "")
        {
            return Err("cloud_model_not_allowed".into());
        }
        Ok(digest)
    }

    fn recognize(
        &self,
        png: &[u8],
        expected_digest: &str,
        chat_requests: &mut usize,
    ) -> Result<String, String> {
        if self.tags_digest()? != expected_digest {
            return Err("model_changed_during_scan".into());
        }
        *chat_requests += 1;
        let response = self.request("/api/chat", Some(json!({
            "model": self.model, "stream": GENERATION.stream, "think": GENERATION.think,
            "options": {"temperature": GENERATION.temperature, "num_predict": GENERATION.num_predict, "num_ctx": GENERATION.num_ctx},
            "format": {"type":"object", "properties":{"observed":{"type":"string"}}, "required":["observed"], "additionalProperties":false},
            "messages": [{"role":"user", "content":STRUCTURE_PROMPT, "images":[base64::engine::general_purpose::STANDARD.encode(png)]}]
        })))?;
        if response["done"] != true || response["done_reason"] != "stop" {
            return Err(if response["done_reason"] == "length" {
                "token_limit"
            } else {
                "incomplete_response"
            }
            .into());
        }
        let content = response["message"]["content"]
            .as_str()
            .ok_or("missing_content")?;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Observed {
            observed: String,
        }
        let observed: Observed =
            serde_json::from_str(content).map_err(|_| "invalid_observed_json")?;
        if observed.observed != "?" && parse_kanji_character(&observed.observed).is_err() {
            return Err("observed_not_single_han".into());
        }
        if self.tags_digest()? != expected_digest {
            return Err("model_changed_during_scan".into());
        }
        Ok(observed.observed)
    }
}

fn network_failure(error: reqwest::Error) -> String {
    if error.is_timeout() {
        "ollama_timeout"
    } else {
        "ollama_network_error"
    }
    .into()
}

/// Клиент создаётся лишь после проверки наличия подходящих сохранённых кандидатов.
/// Запись контрольной точки журнала после каждого SHA атомарна; Ctrl+C оставляет предыдущие результаты.
pub(crate) fn scan(
    runtime: &mut BatchRuntime,
    state: &mut KanjiBatch,
    options: &OcrOptions,
) -> Result<OcrScanReport, AssetError> {
    options.validate()?;
    let targets: Vec<_> = state
        .review_queue()
        .into_iter()
        .filter_map(|item| {
            let candidate = current_candidate(state, &item.identity)?;
            (candidate.technically_valid && candidate.automated.status == SemanticStatus::Uncertain)
                .then(|| (item.identity.clone(), item.generation, candidate.clone()))
        })
        .collect();
    if targets.is_empty() {
        return Ok(OcrScanReport::default());
    }
    let ollama = Ollama::new(options)?;
    runtime.release_lock()?;
    let digest = ollama.model_digest();
    runtime.reacquire_lock()?;
    *state = runtime
        .reload_cached()?
        .ok_or_else(|| invalid("пакет исчез во время OCR"))?;
    let mut ledger = read_ledger(runtime)?;
    let mut report = OcrScanReport::default();
    for (identity, generation, candidate) in targets {
        if !still_current(state, &identity, generation, &candidate) {
            continue;
        }
        let config = OcrConfig {
            provider: "ollama".into(),
            endpoint: ollama.base.to_string(),
            model: options.model.clone(),
            model_digest: digest.clone().unwrap_or_else(|_| "unavailable".into()),
            prompt_version: PROMPT_VERSION.into(),
            prompt_sha256: sha256_hex(STRUCTURE_PROMPT.as_bytes()),
            transform_version: TRANSFORM_VERSION.into(),
            timeout_seconds: options.timeout_seconds,
            generation: GENERATION,
        };
        let key = cache_key(&identity, &candidate.sha256, &config);
        if digest.is_ok()
            && !options.rescan
            && let Some(saved) = ledger.records.get(&key).cloned()
        {
            if let Some(failure) = &saved.failure {
                report.failures.push((identity.clone(), failure.clone()));
            }
            report.cache_hits += 1;
            report.saved += usize::from(save_suggestion(runtime, &mut ledger, saved)?);
            continue;
        }
        let mut suggestion = OcrSuggestion {
            identity: identity.clone(),
            source_sha256: candidate.sha256.clone(),
            config,
            observed: None,
            matches_expected: false,
            failure: None,
            format: candidate.format,
            representative_frame: None,
            dimensions: None,
            prepared_sha256: None,
            timestamp_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| invalid(error.to_string()))?
                .as_secs(),
        };
        let observed = match &digest {
            Err(failure) => Err(failure.clone()),
            Ok(digest) => {
                let bytes = runtime.read_candidate(&candidate)?;
                match prepare_ocr_image(&bytes) {
                    Ok(image) if image.format == candidate.format => {
                        suggestion.representative_frame = Some(image.frame);
                        suggestion.dimensions = Some(image.dimensions);
                        suggestion.prepared_sha256 = Some(sha256_hex(&image.png));
                        runtime.release_lock()?;
                        let result =
                            ollama.recognize(&image.png, digest, &mut report.chat_requests);
                        runtime.reacquire_lock()?;
                        *state = runtime
                            .reload_cached()?
                            .ok_or_else(|| invalid("пакет исчез во время OCR"))?;
                        ledger = read_ledger(runtime)?;
                        result
                    }
                    Ok(_) => Err("format_mismatch".into()),
                    Err(reason) => Err(format!("image_prepare_failed: {reason}")),
                }
            }
        };
        match observed {
            Ok(observed) => {
                suggestion.matches_expected = observed == identity.key;
                suggestion.observed = Some(observed);
            }
            Err(reason) => {
                suggestion.failure = Some(reason);
            }
        }
        // SHA/generation/current status сверяются после освобождения lock.
        if still_current(state, &identity, generation, &candidate) {
            if let Some(reason) = &suggestion.failure {
                report.failures.push((identity.clone(), reason.clone()));
            }
            report.saved += usize::from(save_suggestion(runtime, &mut ledger, suggestion)?);
        }
    }
    Ok(report)
}

fn current_candidate<'a>(
    state: &'a KanjiBatch,
    identity: &AssetIdentity,
) -> Option<&'a BatchCandidate> {
    let item = state.items.iter().find(|item| &item.identity == identity)?;
    let hash = item.current_sha256.as_ref()?;
    item.attempts
        .iter()
        .rev()
        .find_map(|attempt| match &attempt.result {
            crate::batch::BatchAttemptInput::Candidate { candidate }
                if &candidate.sha256 == hash =>
            {
                Some(candidate)
            }
            _ => None,
        })
}

fn still_current(
    state: &KanjiBatch,
    identity: &AssetIdentity,
    generation: u32,
    candidate: &BatchCandidate,
) -> bool {
    state
        .review_queue()
        .iter()
        .any(|item| &item.identity == identity && item.generation == generation)
        && current_candidate(state, identity) == Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn structure_instruction_hash_pins_the_exact_research_prompt() {
        assert_eq!(
            sha256_hex(STRUCTURE_PROMPT.as_bytes()),
            "1e65ac02562af176e112739e1409edc93e60a3183473ff74031477ad533de8b5"
        );
    }
    #[test]
    fn every_byte_model_instruction_and_transform_setting_changes_the_cache_key() {
        let identity = AssetIdentity::new("kanji", "保").unwrap();
        let sha = "a".repeat(64);
        let config = OcrConfig {
            provider: "ollama".into(),
            endpoint: "http://127.0.0.1:11434/".into(),
            model: "fixture:vision".into(),
            model_digest: "digest".into(),
            prompt_version: PROMPT_VERSION.into(),
            prompt_sha256: sha256_hex(STRUCTURE_PROMPT.as_bytes()),
            transform_version: TRANSFORM_VERSION.into(),
            timeout_seconds: 120,
            generation: GENERATION,
        };
        let key = cache_key(&identity, &sha, &config);
        assert_ne!(key, cache_key(&identity, &"b".repeat(64), &config));
        assert_ne!(
            key,
            cache_key(&AssetIdentity::new("kanji", "倉").unwrap(), &sha, &config)
        );
        for change in [
            OcrConfig {
                model: "different".into(),
                ..config.clone()
            },
            OcrConfig {
                model_digest: "new-digest".into(),
                ..config.clone()
            },
            OcrConfig {
                prompt_version: "next".into(),
                ..config.clone()
            },
            OcrConfig {
                prompt_sha256: "c".repeat(64),
                ..config.clone()
            },
            OcrConfig {
                transform_version: "next".into(),
                ..config.clone()
            },
            OcrConfig {
                generation: GenerationOptions {
                    num_ctx: 8192,
                    ..GENERATION
                },
                ..config.clone()
            },
        ] {
            assert_ne!(key, cache_key(&identity, &sha, &change));
        }
    }

    #[test]
    fn endpoint_requires_explicit_permission_outside_literal_loopback() {
        let mut options = OcrOptions {
            model: "vision".into(),
            endpoint: "http://127.0.0.1:11434".into(),
            allow_remote: false,
            timeout_seconds: 120,
            rescan: false,
        };
        assert!(options.validate().is_ok());
        for endpoint in [
            "http://localhost:11434",
            "http://192.168.1.2:11434",
            "http://example.com:11434",
        ] {
            options.endpoint = endpoint.into();
            assert!(options.validate().is_err());
            options.allow_remote = true;
            assert!(options.validate().is_ok());
            options.allow_remote = false;
        }
        options.allow_remote = true;
        for endpoint in [
            "https://example.com",
            "http://user:pass@127.0.0.1:11434",
            "http://127.0.0.1/api/chat",
            "http://127.0.0.1?query=1",
            "file:///tmp/image",
        ] {
            options.endpoint = endpoint.into();
            assert!(options.validate().is_err());
        }
        options.endpoint = "http://[::1]:11434".into();
        options.allow_remote = false;
        assert!(options.validate().is_ok());
    }
}
