//! Необязательные предложения OCR: никогда не источник доверия.
//!
//! Слепой OCR: модели передаётся только подготовленный кадр изображения и
//! промпт задачи `kanji_ocr` — без ожидаемого символа, имени файла, значения и
//! чтения. Ответ — ровно один поддерживаемый Han либо `?`; формат ответа и его
//! проверка принадлежат этому модулю, а не тексту промпта.
//!
//! Записи прежнего Ollama-only пути (`OcrProvenance::LegacyOllama`) читаются и
//! показываются в review с прежним ключом, но новый запуск никогда не
//! принимает их за попадание кэша: идентичность нового результата —
//! `InferenceProvenance` с SHA весов, промпта и параметров.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::batch::{BatchCandidate, BatchRuntime, KanjiBatch};
use crate::hashing::sha256_hex;
use crate::inference::{
    InferenceProvenance, InferenceSession, ResponseContract, RunTimings, SessionReport,
    json_object_text,
};
use crate::inference_cli::InferenceArgs;
use crate::inference_config::TaskId;
use crate::kanji_domain::parse_kanji_character;
use crate::kanji_validator::prepare_ocr_image;
use crate::model::{AssetIdentity, DetectedFormat, SemanticStatus};
use crate::{AssetError, ErrorCode};

pub(crate) const TRANSFORM_VERSION: &str = "v6-representative-white-lanczos3-2x-image025-v1";
const CONTRACT_ID: &str = "kanji-observed-v1";
const CONTRACT_INSTRUCTION: &str =
    "Return ONLY a JSON object with one string field named \"observed\". No explanation.";

/// Контракт ответа задачи: инструкция и схема задаются кодом.
pub(crate) fn contract() -> ResponseContract {
    ResponseContract {
        id: CONTRACT_ID,
        instruction: CONTRACT_INSTRUCTION,
        schema: json!({
            "type": "object",
            "properties": {"observed": {"type": "string"}},
            "required": ["observed"],
            "additionalProperties": false,
        }),
        requires_image: true,
    }
}

/// Строгая проверка ответа: один Han либо `?`.
pub(crate) fn interpret(content: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Observed {
        observed: String,
    }
    let observed: Observed = serde_json::from_str(json_object_text(content))
        .map_err(|_| "invalid_observed_json".to_owned())?;
    if observed.observed != "?" && parse_kanji_character(&observed.observed).is_err() {
        return Err("observed_not_single_han".into());
    }
    Ok(observed.observed)
}

/// Подготовка кадра штатным путём OCR для `inference probe`.
pub(crate) fn prepare_for_probe(bytes: &[u8]) -> Result<(Vec<u8>, Value), String> {
    let image =
        prepare_ocr_image(bytes).map_err(|reason| format!("image_prepare_failed: {reason}"))?;
    Ok((
        image.png,
        json!({"format": image.format, "frame": image.frame, "dimensions": image.dimensions}),
    ))
}

/// Параметры генерации прежнего Ollama-only пути.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyGenerationOptions {
    temperature: u32,
    num_predict: u32,
    num_ctx: u32,
    think: bool,
    stream: bool,
}

/// Конфигурация прежнего Ollama-only пути: только чтение сохранённых записей.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyOcrConfig {
    pub provider: String,
    pub endpoint: String,
    pub model: String,
    pub model_digest: String,
    pub prompt_version: String,
    pub prompt_sha256: String,
    pub transform_version: String,
    pub timeout_seconds: u64,
    pub generation: LegacyGenerationOptions,
}

/// Идентичность результата. Без тега: сериализация записи прежней схемы
/// побайтно совпадает с прежней, поэтому её ключ кэша не меняется.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum OcrProvenance {
    Inference(InferenceProvenance),
    LegacyOllama(LegacyOcrConfig),
}

impl OcrProvenance {
    fn describe(&self) -> String {
        match self {
            Self::Inference(provenance) => provenance.summary(),
            Self::LegacyOllama(config) => format!(
                "прежний Ollama-only путь (только просмотр): модель {} @ {}; prompt={}; transform={}",
                config.model, config.model_digest, config.prompt_version, config.transform_version
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OcrSuggestion {
    pub identity: AssetIdentity,
    pub source_sha256: String,
    pub config: OcrProvenance,
    pub observed: Option<String>,
    pub matches_expected: bool,
    pub failure: Option<String>,
    pub format: DetectedFormat,
    pub representative_frame: Option<usize>,
    pub dimensions: Option<[u32; 2]>,
    pub prepared_sha256: Option<String>,
    pub timestamp_unix_seconds: u64,
    /// Времена запроса; не входят в ключ кэша.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunTimings>,
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
            "observed={}; совпадение={}; отказ={}; {}",
            self.observed.as_deref().unwrap_or("нет"),
            self.matches_expected,
            self.failure.as_deref().unwrap_or("нет"),
            self.config.describe()
        )
    }
}

/// Аргументы `batch ocr`: конфигурация inference и явный повтор.
#[derive(Debug, Clone, clap::Args)]
pub struct OcrOptions {
    #[command(flatten)]
    pub inference: InferenceArgs,
    /// Явный повтор вместо кэша, включая сохранённые отказы.
    #[arg(long)]
    pub rescan: bool,
}

impl OcrOptions {
    pub(crate) fn validate(&self) -> Result<(), AssetError> {
        self.inference.validate_syntax()
    }
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn cache_key(identity: &AssetIdentity, sha: &str, config: &OcrProvenance) -> String {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference: Option<SessionReport>,
    /// Остановка собственного сервера не доказана: команда завершается блокировкой.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stop_unproven: bool,
    /// Идентичность модели не проверяема (внешний сервер сообщил лишь путь
    /// или projector неизвестен): сохранённые результаты не переиспользуются.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cache_reuse_blocked: bool,
    #[serde(skip)]
    pub failures: Vec<(AssetIdentity, String)>,
}

/// Модель запускается лишь после проверки наличия подходящих сохранённых
/// кандидатов. Запись после каждого SHA атомарна; Ctrl+C останавливает
/// собственный сервер и оставляет уже сохранённые результаты.
pub(crate) fn scan(
    runtime: &mut BatchRuntime,
    state: &mut KanjiBatch,
    options: &OcrOptions,
) -> Result<OcrScanReport, AssetError> {
    let task = options.inference.resolve(TaskId::KanjiOcr)?;
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
    task.require_ready()?;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AssetError::io("runtime OCR", error))?;
    let unavailable = InferenceProvenance::unavailable(&task, &contract(), TRANSFORM_VERSION);
    runtime.release_lock()?;
    let prepared = executor.block_on(InferenceSession::prepare(
        task,
        contract(),
        TRANSFORM_VERSION,
    ));
    runtime.reacquire_lock()?;
    let mut report = OcrScanReport::default();
    let (mut session, provenance, preparation_failure) = match prepared {
        Ok(session) => {
            let provenance = session.provenance().clone();
            (Some(session), provenance, None)
        }
        Err(error) => {
            let (failure, session_report) = *error;
            report.stop_unproven = failure.stop_unproven();
            report.inference = Some(session_report);
            if failure.is_interrupted() {
                return Err(crate::inference::with_session_evidence(
                    failure.into_asset_error(),
                    report.stop_unproven,
                    report.inference.as_ref(),
                ));
            }
            (None, unavailable, Some(failure.code))
        }
    };
    let outcome = scan_targets(
        runtime,
        state,
        options,
        &executor,
        &mut session,
        &provenance,
        preparation_failure,
        targets,
        &mut report,
    );
    if let Some(session) = session {
        let finished = executor.block_on(session.finish());
        report.stop_unproven = finished.stop.as_ref().is_some_and(|stop| !stop.proven());
        report.inference = Some(finished);
    }
    match outcome {
        Ok(true) => Err(crate::inference::interrupted_error(
            "OCR прерван сигналом; уже сохранённые предложения остаются в журнале",
            report.saved,
            report.stop_unproven,
            report.inference.as_ref(),
        )),
        Ok(false) => Ok(report),
        Err(error) => Err(crate::inference::with_session_evidence(
            error,
            report.stop_unproven,
            report.inference.as_ref(),
        )),
    }
}

/// Возвращает `true`, если обход прерван сигналом.
#[allow(clippy::too_many_arguments)]
fn scan_targets(
    runtime: &mut BatchRuntime,
    state: &mut KanjiBatch,
    options: &OcrOptions,
    executor: &tokio::runtime::Runtime,
    session: &mut Option<InferenceSession>,
    provenance: &InferenceProvenance,
    preparation_failure: Option<String>,
    targets: Vec<(AssetIdentity, u32, BatchCandidate)>,
    report: &mut OcrScanReport,
) -> Result<bool, AssetError> {
    *state = runtime
        .reload_cached()?
        .ok_or_else(|| invalid("пакет исчез во время OCR"))?;
    let mut ledger = read_ledger(runtime)?;
    let config = OcrProvenance::Inference(provenance.clone());
    let reusable = provenance.reusable();
    report.cache_reuse_blocked = session.is_some() && !reusable;
    for (identity, generation, candidate) in targets {
        if session.as_mut().is_some_and(InferenceSession::interrupted) {
            return Ok(true);
        }
        if !still_current(state, &identity, generation, &candidate) {
            continue;
        }
        let key = cache_key(&identity, &candidate.sha256, &config);
        if session.is_some()
            && !options.rescan
            && reusable
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
            config: config.clone(),
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
            run: None,
        };
        let observed = match (session.as_mut(), &preparation_failure) {
            (None, failure) => Err(failure
                .clone()
                .unwrap_or_else(|| "inference_unavailable".into())),
            (Some(session), _) => {
                let bytes = runtime.read_candidate(&candidate)?;
                match prepare_ocr_image(&bytes) {
                    Ok(image) if image.format == candidate.format => {
                        suggestion.representative_frame = Some(image.frame);
                        suggestion.dimensions = Some(image.dimensions);
                        suggestion.prepared_sha256 = Some(sha256_hex(&image.png));
                        runtime.release_lock()?;
                        report.chat_requests += 1;
                        let result = executor.block_on(session.run(Some(&image.png)));
                        runtime.reacquire_lock()?;
                        *state = runtime
                            .reload_cached()?
                            .ok_or_else(|| invalid("пакет исчез во время OCR"))?;
                        ledger = read_ledger(runtime)?;
                        match result {
                            Err(failure) if failure.is_interrupted() => return Ok(true),
                            Err(failure) => Err(failure.code),
                            Ok(response) => {
                                suggestion.run = Some(response.timings);
                                interpret(&response.content)
                            }
                        }
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
    Ok(false)
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
    use crate::inference::compose_prompt;
    use crate::inference_config::{ConfigSources, LoadedConfig};

    fn default_task() -> crate::inference_config::ResolvedTask {
        LoadedConfig::load(&ConfigSources {
            skip_local_discovery: true,
            ..ConfigSources::default()
        })
        .unwrap()
        .resolve(TaskId::KanjiOcr)
        .unwrap()
    }

    #[test]
    fn default_prompt_file_and_code_contract_reproduce_the_research_prompt() {
        let task = default_task();
        assert_eq!(task.prompt.source, "repository:prompts/kanji_ocr.txt");
        assert_eq!(
            sha256_hex(compose_prompt(&task.prompt.text, &contract()).as_bytes()),
            "1e65ac02562af176e112739e1409edc93e60a3183473ff74031477ad533de8b5"
        );
    }

    #[test]
    fn legacy_record_keeps_its_key_and_never_equals_a_new_identity() {
        // Запись прежней схемы в порядке полей, в котором её сериализовал прежний код.
        let legacy = r#"{"provider":"ollama","endpoint":"http://127.0.0.1:11434/","model":"fixture:vision","model_digest":"digest","prompt_version":"structure-v1","prompt_sha256":"1e65ac02562af176e112739e1409edc93e60a3183473ff74031477ad533de8b5","transform_version":"v6-representative-white-lanczos3-2x-image025-v1","timeout_seconds":120,"generation":{"temperature":0,"num_predict":128,"num_ctx":4096,"think":false,"stream":false}}"#;
        let parsed: OcrProvenance = serde_json::from_str(legacy).unwrap();
        assert!(matches!(parsed, OcrProvenance::LegacyOllama(_)));
        // Сериализация побайтно прежняя, значит и ключ кэша прежний.
        assert_eq!(serde_json::to_string(&parsed).unwrap(), legacy);
        let identity = AssetIdentity::new("kanji", "保").unwrap();
        let sha = "a".repeat(64);
        let current = OcrProvenance::Inference(InferenceProvenance::unavailable(
            &default_task(),
            &contract(),
            TRANSFORM_VERSION,
        ));
        assert_ne!(
            cache_key(&identity, &sha, &parsed),
            cache_key(&identity, &sha, &current)
        );
    }

    #[test]
    fn every_identity_field_changes_the_cache_key() {
        let identity = AssetIdentity::new("kanji", "保").unwrap();
        let sha = "a".repeat(64);
        let mut base =
            InferenceProvenance::unavailable(&default_task(), &contract(), TRANSFORM_VERSION);
        base.model.weights = "sha256:aaaa".into();
        let key = cache_key(&identity, &sha, &OcrProvenance::Inference(base.clone()));
        assert_ne!(
            key,
            cache_key(
                &identity,
                &"b".repeat(64),
                &OcrProvenance::Inference(base.clone())
            )
        );
        assert_ne!(
            key,
            cache_key(
                &AssetIdentity::new("kanji", "倉").unwrap(),
                &sha,
                &OcrProvenance::Inference(base.clone())
            )
        );
        type Change = Box<dyn Fn(&mut InferenceProvenance)>;
        let changes: Vec<Change> = vec![
            Box::new(|p| p.model.weights = "sha256:bbbb".into()),
            Box::new(|p| p.model.projector = Some("sha256:cccc".into())),
            Box::new(|p| p.model.name = "other.gguf".into()),
            Box::new(|p| {
                p.model.load.insert("context_size".into(), "8192".into());
            }),
            Box::new(|p| p.backend.kind = "ollama".into()),
            Box::new(|p| p.backend.version = Some("b2".into())),
            Box::new(|p| p.prompt_sha256 = "d".repeat(64)),
            Box::new(|p| p.transform_version = "next".into()),
            Box::new(|p| p.contract = "kanji-observed-v2".into()),
            Box::new(|p| {
                p.generation.insert("temperature".into(), "0.2".into());
            }),
            Box::new(|p| {
                p.generation
                    .insert("structured_output".into(), "none".into());
            }),
        ];
        for change in changes {
            let mut changed = base.clone();
            change(&mut changed);
            assert_ne!(
                key,
                cache_key(&identity, &sha, &OcrProvenance::Inference(changed))
            );
        }
    }

    #[test]
    fn response_validation_is_owned_by_code() {
        assert_eq!(interpret("{\"observed\":\"保\"}").unwrap(), "保");
        assert_eq!(
            interpret("```json\n{\"observed\":\"?\"}\n```").unwrap(),
            "?"
        );
        assert_eq!(
            interpret("{\"observed\":\"保\",\"x\":1}").unwrap_err(),
            "invalid_observed_json"
        );
        assert_eq!(
            interpret("{\"observed\":\"保倉\"}").unwrap_err(),
            "observed_not_single_han"
        );
        assert_eq!(interpret("保").unwrap_err(), "invalid_observed_json");
    }
}
