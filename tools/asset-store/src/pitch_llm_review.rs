//! Необязательная LLM-проверка сохранённых PNG тонового ударения JPDB.
//!
//! Задача `jpdb_pitch_review` визуально оценивает уже сохранённый и
//! проверенный валидатором PNG текущего кандидата пакета: обрезание, неполные
//! глифы, артефакты, посторонние элементы, читаемость. Сайт повторно не
//! запрашивается; модели передаются только байты изображения, без словоформы и
//! чтения. Совпадение прочитанного моделью текста с чтением записи вычисляет код.
//!
//! Результат — предложение, а не решение: он не меняет статус валидатора,
//! `VERIFIED`, решения `decisions.tsv`, CAS и публикацию. Запись привязана к
//! точному токену элемента (поколение, ревизия, отпечаток запроса, попытка) и
//! SHA байтов; после `reacquire`, `reject` или смены SHA прежняя запись не
//! прикрепляется к новой строке review.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::inference::{
    InferenceProvenance, InferenceSession, ResponseContract, RunTimings, SessionReport,
    json_object_text,
};
use crate::inference_cli::InferenceArgs;
use crate::inference_config::TaskId;
use crate::model::AssetIdentity;
use crate::pitch_batch::{PitchAccentBatch, PitchAccentBatchRuntime};
use crate::pitch_review_decisions::{PitchReviewKind, PitchReviewRow};
use crate::store::AssetStore;

pub(crate) const TRANSFORM_VERSION: &str = "jpdb-png-rgba-lanczos3-upscale-448-v1";
const CONTRACT_ID: &str = "jpdb-pitch-visual-v1";
const CONTRACT_INSTRUCTION: &str = "Return ONLY a JSON object with exactly these fields: \"verdict\" (one of \"no_defect_observed\", \"defect_observed\", \"insufficient\"), \"observations\" (an array of at most 6 objects with \"category\" (one of \"cropping\", \"incomplete_glyph\", \"artifact\", \"foreign_element\", \"illegible_text\", \"other\") and \"detail\" (a short English note)), and \"visible_text\" (the kana you can read). Use an empty observations array when no defect is observed. No explanation outside the JSON object.";
const MAX_OBSERVATIONS: usize = 6;
const MAX_DETAIL_CHARS: usize = 200;
const MAX_VISIBLE_TEXT_CHARS: usize = 64;
const TARGET_LONG_SIDE: u32 = 448;
const MAX_SCALE: u32 = 3;

pub(crate) fn contract() -> ResponseContract {
    ResponseContract {
        id: CONTRACT_ID,
        instruction: CONTRACT_INSTRUCTION,
        schema: json!({
            "type": "object",
            "properties": {
                "verdict": {"type": "string", "enum": ["no_defect_observed", "defect_observed", "insufficient"]},
                "observations": {
                    "type": "array",
                    "maxItems": MAX_OBSERVATIONS,
                    "items": {
                        "type": "object",
                        "properties": {
                            "category": {"type": "string", "enum": ["cropping", "incomplete_glyph", "artifact", "foreign_element", "illegible_text", "other"]},
                            "detail": {"type": "string", "maxLength": MAX_DETAIL_CHARS},
                        },
                        "required": ["category", "detail"],
                        "additionalProperties": false,
                    },
                },
                "visible_text": {"type": "string", "maxLength": MAX_VISIBLE_TEXT_CHARS},
            },
            "required": ["verdict", "observations", "visible_text"],
            "additionalProperties": false,
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    NoDefectObserved,
    DefectObserved,
    Insufficient,
}

impl Verdict {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::NoDefectObserved => "no_defect_observed",
            Self::DefectObserved => "defect_observed",
            Self::Insufficient => "insufficient",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ObservationCategory {
    Cropping,
    IncompleteGlyph,
    Artifact,
    ForeignElement,
    IllegibleText,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Observation {
    pub category: ObservationCategory,
    pub detail: String,
}

/// Проверенная визуальная оценка модели.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PitchAssessment {
    pub verdict: Verdict,
    pub observations: Vec<Observation>,
    pub visible_text: String,
}

fn bounded_text(value: &str, limit: usize) -> bool {
    value.chars().count() <= limit && !value.chars().any(char::is_control)
}

/// Строгая проверка ответа: схема, границы и согласованность вердикта.
pub(crate) fn interpret(content: &str) -> Result<PitchAssessment, String> {
    let assessment: PitchAssessment = serde_json::from_str(json_object_text(content))
        .map_err(|_| "invalid_assessment_json".to_owned())?;
    if assessment.observations.len() > MAX_OBSERVATIONS
        || !bounded_text(&assessment.visible_text, MAX_VISIBLE_TEXT_CHARS)
        || assessment
            .observations
            .iter()
            .any(|observation| !bounded_text(&observation.detail, MAX_DETAIL_CHARS))
    {
        return Err("assessment_out_of_bounds".into());
    }
    let consistent = match assessment.verdict {
        Verdict::NoDefectObserved => assessment.observations.is_empty(),
        Verdict::DefectObserved => !assessment.observations.is_empty(),
        Verdict::Insufficient => true,
    };
    if !consistent {
        return Err("inconsistent_verdict".into());
    }
    Ok(assessment)
}

/// Изображение, переданное модели.
pub(crate) struct PreparedImage {
    pub png: Vec<u8>,
    pub dimensions: [u32; 2],
}

/// Детерминированная подготовка: только PNG; маленький график целочисленно
/// увеличивается Lanczos3 до длинной стороны около 448 px (не больше чем в 3 раза).
pub(crate) fn prepare_image(bytes: &[u8]) -> Result<PreparedImage, String> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .map_err(|_| "image_prepare_failed: нужен декодируемый PNG".to_owned())?
        .to_rgba8();
    let long_side = image.width().max(image.height()).max(1);
    let scale = TARGET_LONG_SIDE.div_ceil(long_side).clamp(1, MAX_SCALE);
    let dimensions = [image.width() * scale, image.height() * scale];
    let scaled = if scale == 1 {
        image
    } else {
        image::imageops::resize(
            &image,
            dimensions[0],
            dimensions[1],
            image::imageops::FilterType::Lanczos3,
        )
    };
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(scaled)
        .write_to(&mut png, image::ImageFormat::Png)
        .map_err(|error| format!("image_prepare_failed: {error}"))?;
    Ok(PreparedImage {
        png: png.into_inner(),
        dimensions,
    })
}

fn katakana_to_hiragana(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            'ァ'..='ヶ' => char::from_u32(character as u32 - 0x60).unwrap_or(character),
            other => other,
        })
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// Предложение модели для точного кандидата строки review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PitchLlmSuggestion {
    pub identity: AssetIdentity,
    pub sha256: String,
    pub generation: u32,
    pub item_revision: u64,
    pub request_fingerprint: String,
    pub attempt_index: u32,
    pub provenance: InferenceProvenance,
    pub assessment: Option<PitchAssessment>,
    /// Чтение записи из метаданных кандидата; модели не передаётся.
    pub expected_reading: String,
    pub visible_text_matches_reading: Option<bool>,
    pub failure: Option<String>,
    pub prepared_sha256: Option<String>,
    pub dimensions: Option<[u32; 2]>,
    pub timestamp_unix_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunTimings>,
}

impl PitchLlmSuggestion {
    pub(crate) fn key(&self) -> String {
        sha256_hex(
            serde_json::to_vec(&(
                &self.identity,
                &self.sha256,
                self.generation,
                self.item_revision,
                &self.request_fingerprint,
                self.attempt_index,
                &self.provenance,
            ))
            .expect("сериализация простых полей"),
        )
    }

    /// Принадлежит ли предложение точному кандидату строки review.
    pub(crate) fn matches_row(&self, row: &PitchReviewRow) -> bool {
        row.kind == PitchReviewKind::Candidate
            && self.identity == row.identity
            && Some(&self.sha256) == row.sha256.as_ref()
            && self.generation == row.generation
            && self.item_revision == row.item_revision
            && self.request_fingerprint == row.request_fingerprint
            && self.attempt_index == row.attempt_index
    }

    /// Короткая метка для неизменяемой колонки `decisions.tsv`.
    pub(crate) fn label(&self) -> String {
        match (&self.failure, &self.assessment) {
            (Some(failure), _) => format!("llm_failure:{failure}"),
            (None, Some(assessment)) => format!("llm:{}", assessment.verdict.as_str()),
            (None, None) => "llm:none".into(),
        }
    }

    pub(crate) fn summary(&self) -> String {
        let mut text = format!("LLM (предложение, не решение): {}", self.label());
        if let Some(assessment) = &self.assessment {
            text.push_str(&format!(
                "\nВидимый текст: «{}»; совпадает с чтением «{}»: {}",
                assessment.visible_text,
                self.expected_reading,
                match self.visible_text_matches_reading {
                    Some(true) => "да",
                    Some(false) => "нет",
                    None => "неизвестно",
                }
            ));
            for observation in &assessment.observations {
                text.push_str(&format!(
                    "\n- {}: {}",
                    serde_json::to_value(observation.category)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                    observation.detail
                ));
            }
        }
        text.push_str(&format!("\n{}", self.provenance.summary()));
        text
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    records: BTreeMap<String, PitchLlmSuggestion>,
    latest: BTreeMap<String, String>,
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

const LEDGER_DIRECTORY: &str = "llm-review";
const LEDGER_FILE: &str = "suggestions.json";

fn read_ledger(runtime: &PitchAccentBatchRuntime) -> Result<Ledger, AssetError> {
    match runtime
        .artifact_directory(LEDGER_DIRECTORY)?
        .read(LEDGER_FILE)
    {
        Ok(bytes) => {
            let ledger: Ledger = serde_json::from_slice(&bytes)
                .map_err(|error| invalid(format!("повреждён журнал LLM-проверки: {error}")))?;
            for (key, suggestion) in &ledger.records {
                crate::batch_runtime::validate_hash(&suggestion.sha256)?;
                if key != &suggestion.key()
                    || suggestion.failure.is_some() == suggestion.assessment.is_some()
                {
                    return Err(invalid("неверная привязка записи LLM-проверки"));
                }
            }
            Ok(ledger)
        }
        Err(error) if error.code == ErrorCode::MissingAssetFile => Ok(Ledger::default()),
        Err(error) => Err(error),
    }
}

/// Последние предложения по идентичности; ошибка чтения не ломает review.
pub(crate) fn saved_or_empty(
    runtime: &PitchAccentBatchRuntime,
) -> BTreeMap<AssetIdentity, PitchLlmSuggestion> {
    match read_ledger(runtime) {
        Ok(ledger) => ledger
            .latest
            .values()
            .filter_map(|key| ledger.records.get(key))
            .map(|suggestion| (suggestion.identity.clone(), suggestion.clone()))
            .collect(),
        Err(error) => {
            tracing::warn!(
                code = error.code.as_str(),
                "журнал LLM-проверки недоступен; review строится без предложений"
            );
            BTreeMap::new()
        }
    }
}

fn save(
    runtime: &PitchAccentBatchRuntime,
    ledger: &mut Ledger,
    suggestion: PitchLlmSuggestion,
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
        .artifact_directory(LEDGER_DIRECTORY)?
        .write(LEDGER_FILE, &encoded)?;
    Ok(true)
}

/// Аргументы `batch llm-review`.
#[derive(Debug, Clone, clap::Args)]
pub struct LlmReviewOptions {
    #[command(flatten)]
    pub inference: InferenceArgs,
    /// Повторить запрос вместо сохранённого результата, включая отказы.
    #[arg(long)]
    pub rescan: bool,
}

/// Итог команды для ответа CLI.
#[derive(Debug, Default, Serialize)]
pub(crate) struct LlmReviewReport {
    pub targets: usize,
    pub saved: usize,
    pub cache_hits: usize,
    pub requests: usize,
    pub stale_discarded: usize,
    pub verdicts: BTreeMap<String, usize>,
    pub failures: Vec<LlmReviewFailure>,
    pub interrupted: bool,
    pub stop_unproven: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference: Option<SessionReport>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LlmReviewFailure {
    pub surface: String,
    pub code: String,
}

struct Target {
    row: PitchReviewRow,
    reading: String,
}

fn targets(batch: &PitchAccentBatch) -> Result<Vec<Target>, AssetError> {
    crate::pitch_review_decisions::review_rows(batch)?
        .into_iter()
        .filter(|row| row.kind == PitchReviewKind::Candidate)
        .map(|row| {
            let item = batch
                .items
                .iter()
                .find(|item| item.identity == row.identity)
                .ok_or_else(|| invalid("строка не принадлежит пакету"))?;
            let candidate = item
                .candidate_at(row.attempt_index, row.sha256.as_deref().unwrap_or_default())
                .ok_or_else(|| invalid("кандидат отсутствует"))?;
            Ok(Target {
                reading: candidate.metadata.reading.clone(),
                row,
            })
        })
        .collect()
}

fn still_current(batch: &PitchAccentBatch, row: &PitchReviewRow) -> bool {
    crate::pitch_review_decisions::review_rows(batch)
        .map(|rows| rows.iter().any(|current| current == row))
        .unwrap_or(false)
}

/// Выполняет задачу над текущими кандидатами пакета. Блокировка пакета
/// освобождается на время запуска модели и каждого запроса; результат
/// сохраняется, только если строка осталась той же после повторной загрузки.
pub(crate) async fn run(
    store: &AssetStore,
    batch_id: &str,
    options: &LlmReviewOptions,
) -> Result<(PitchAccentBatch, LlmReviewReport), AssetError> {
    let task = options.inference.resolve(TaskId::JpdbPitchReview)?;
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    let (_, owner) = crate::pitch_review_decisions::owner_snapshot(store)?;
    let (stored, mut batch) = crate::pitch_review_decisions::load_reconciled(&mut runtime, &owner)?;
    if batch != stored {
        runtime.save(&batch)?;
    }
    let targets = targets(&batch)?;
    let mut report = LlmReviewReport {
        targets: targets.len(),
        ..LlmReviewReport::default()
    };
    if targets.is_empty() {
        return Ok((batch, report));
    }
    task.require_ready()?;
    let unavailable = InferenceProvenance::unavailable(&task, &contract(), TRANSFORM_VERSION);
    runtime.release_lock()?;
    let prepared = InferenceSession::prepare(task, contract(), TRANSFORM_VERSION).await;
    runtime.reacquire_lock()?;
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
                return Err(failure.into_asset_error());
            }
            (None, unavailable, Some(failure.code))
        }
    };
    let outcome = review_targets(
        &mut runtime,
        &mut batch,
        options,
        &mut session,
        &provenance,
        preparation_failure,
        targets,
        &mut report,
    )
    .await;
    if let Some(session) = session {
        let finished = session.finish().await;
        report.stop_unproven = finished.stop.as_ref().is_some_and(|stop| !stop.proven());
        report.inference = Some(finished);
    }
    outcome.map_err(|error| {
        crate::inference::with_session_evidence(
            error,
            report.stop_unproven,
            report.inference.as_ref(),
        )
    })?;
    if report.interrupted {
        return Err(AssetError::with_details(
            ErrorCode::Interrupted,
            "LLM-проверка прервана сигналом; уже сохранённые предложения остаются в журнале",
            json!({"saved": report.saved, "inference": report.inference}),
        ));
    }
    Ok((batch, report))
}

#[allow(clippy::too_many_arguments)]
async fn review_targets(
    runtime: &mut PitchAccentBatchRuntime,
    batch: &mut PitchAccentBatch,
    options: &LlmReviewOptions,
    session: &mut Option<InferenceSession>,
    provenance: &InferenceProvenance,
    preparation_failure: Option<String>,
    targets: Vec<Target>,
    report: &mut LlmReviewReport,
) -> Result<(), AssetError> {
    *batch = runtime
        .reload_cached()?
        .ok_or_else(|| invalid("пакет исчез во время LLM-проверки"))?;
    let mut ledger = read_ledger(runtime)?;
    for Target { row, reading } in targets {
        if session.as_mut().is_some_and(InferenceSession::interrupted) {
            report.interrupted = true;
            return Ok(());
        }
        if !still_current(batch, &row) {
            report.stale_discarded += 1;
            continue;
        }
        let sha256 = row.sha256.clone().expect("строка кандидата содержит SHA");
        let mut suggestion = PitchLlmSuggestion {
            identity: row.identity.clone(),
            sha256: sha256.clone(),
            generation: row.generation,
            item_revision: row.item_revision,
            request_fingerprint: row.request_fingerprint.clone(),
            attempt_index: row.attempt_index,
            provenance: provenance.clone(),
            assessment: None,
            expected_reading: reading.clone(),
            visible_text_matches_reading: None,
            failure: None,
            prepared_sha256: None,
            dimensions: None,
            timestamp_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| invalid(error.to_string()))?
                .as_secs(),
            run: None,
        };
        let key = suggestion.key();
        if session.is_some()
            && !options.rescan
            && let Some(saved) = ledger.records.get(&key).cloned()
        {
            report.cache_hits += 1;
            count(report, &saved);
            report.saved += usize::from(save(runtime, &mut ledger, saved)?);
            continue;
        }
        let result = match (session.as_mut(), &preparation_failure) {
            (None, failure) => Err(failure
                .clone()
                .unwrap_or_else(|| "inference_unavailable".into())),
            (Some(session), _) => {
                let item = batch
                    .items
                    .iter()
                    .find(|item| item.identity == row.identity)
                    .ok_or_else(|| invalid("строка не принадлежит пакету"))?;
                let candidate = item
                    .candidate_at(row.attempt_index, &sha256)
                    .ok_or_else(|| invalid("кандидат отсутствует"))?;
                // Байты читаются с проверкой SHA и текущего валидатора.
                let bytes = runtime.read_candidate(batch, candidate)?;
                match prepare_image(&bytes) {
                    Ok(prepared) => {
                        suggestion.prepared_sha256 = Some(sha256_hex(&prepared.png));
                        suggestion.dimensions = Some(prepared.dimensions);
                        runtime.release_lock()?;
                        report.requests += 1;
                        let response = session.run(&prepared.png).await;
                        runtime.reacquire_lock()?;
                        *batch = runtime
                            .reload_cached()?
                            .ok_or_else(|| invalid("пакет исчез во время LLM-проверки"))?;
                        ledger = read_ledger(runtime)?;
                        match response {
                            Err(failure) if failure.is_interrupted() => {
                                report.interrupted = true;
                                return Ok(());
                            }
                            Err(failure) => Err(failure.code),
                            Ok(response) => {
                                suggestion.run = Some(response.timings);
                                interpret(&response.content)
                            }
                        }
                    }
                    Err(error) => Err(error),
                }
            }
        };
        match result {
            Ok(assessment) => {
                suggestion.visible_text_matches_reading = (!assessment.visible_text.is_empty())
                    .then(|| {
                        katakana_to_hiragana(&assessment.visible_text)
                            == katakana_to_hiragana(&reading)
                    });
                suggestion.assessment = Some(assessment);
            }
            Err(code) => suggestion.failure = Some(code),
        }
        // Результат устаревшей строки отбрасывается: он не должен
        // подтверждать кандидат, сменившийся во время запроса.
        if !still_current(batch, &row) {
            report.stale_discarded += 1;
            continue;
        }
        count(report, &suggestion);
        report.saved += usize::from(save(runtime, &mut ledger, suggestion)?);
    }
    Ok(())
}

fn count(report: &mut LlmReviewReport, suggestion: &PitchLlmSuggestion) {
    match (&suggestion.failure, &suggestion.assessment) {
        (Some(code), _) => report.failures.push(LlmReviewFailure {
            surface: suggestion.identity.key.clone(),
            code: code.clone(),
        }),
        (None, Some(assessment)) => {
            *report
                .verdicts
                .entry(assessment.verdict.as_str().to_owned())
                .or_default() += 1;
        }
        (None, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_test_support::{MockLlama, external_config};
    use crate::jpdb::{JpdbPitchAcquired, JpdbPitchOutcome, JpdbPitchQuery, JpdbPitchRequest};
    use crate::model::SemanticStatus;
    use crate::pitch_accent::{PitchAccentDomainPolicy, PitchAccentImageValidator};
    use crate::pitch_batch::PitchBatchItemStatus;
    use crate::store::StoreOptions;
    use crate::temp_workspace::TempWorkspace;

    const BATCH: &str = "llm-pitch";

    fn browser() -> crate::browser_runtime::BrowserRuntimeProvenance {
        crate::browser_runtime::BrowserRuntimeProvenance {
            product: "Chrome/140".into(),
            protocol_version: "1.3".into(),
            revision: "1234567".into(),
            user_agent: "Mozilla/5.0 Chrome/140".into(),
            js_version: "V8 14.0".into(),
            executable_source: crate::browser_runtime::BrowserExecutableSource::PathLookup,
        }
    }

    /// Пакет с одним кандидатом `VERIFIED` валидатора v6.
    fn store_with_candidate(workspace: &TempWorkspace) -> AssetStore {
        let store = AssetStore::open_with_policy(
            StoreOptions::new(workspace.path().join("store")),
            PitchAccentDomainPolicy,
        )
        .unwrap();
        let request = JpdbPitchRequest::new(JpdbPitchQuery::new("幽霊", Some("ゆうれい".into())));
        let mut batch = PitchAccentBatch::new(
            BATCH,
            vec![request],
            PitchAccentImageValidator::validator_identity(),
        )
        .unwrap();
        let mut runtime = PitchAccentBatchRuntime::create(store.root(), &batch).unwrap();
        let token = batch.item_token("幽霊").unwrap();
        let outcome = JpdbPitchOutcome::Acquired {
            asset: Box::new(JpdbPitchAcquired {
                bytes: crate::pitch_accent::test_support::single_variant_png(false),
                metadata: crate::pitch_accent::test_support::single_variant_metadata(
                    "幽霊",
                    "ゆうれい",
                    123,
                    "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
                    browser(),
                ),
            }),
        };
        assert!(runtime.record_outcome(&mut batch, &token, outcome).unwrap());
        store
    }

    fn options(path: std::path::PathBuf) -> LlmReviewOptions {
        LlmReviewOptions {
            inference: InferenceArgs {
                inference_config: Some(path),
                ..InferenceArgs::default()
            },
            rescan: false,
        }
    }

    fn load(store: &AssetStore) -> PitchAccentBatch {
        PitchAccentBatchRuntime::open(store.root(), BATCH)
            .unwrap()
            .load()
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn suggestion_is_bound_to_exact_candidate_attached_to_review_and_never_changes_trust() {
        let workspace = TempWorkspace::create("pitch-llm-review").unwrap();
        let store = store_with_candidate(&workspace);
        let mock = MockLlama::new(
            r#"{"verdict":"defect_observed","observations":[{"category":"cropping","detail":"right edge cut"}],"visible_text":"ゆうれい"}"#,
        );
        let options = options(external_config(workspace.path(), &mock.url));
        let before = load(&store);
        let manifest_before = std::fs::read(store.root().join("manifest.json")).unwrap();
        let (_, report) = run(&store, BATCH, &options).await.unwrap();
        assert_eq!((report.targets, report.saved, report.requests), (1, 1, 1));
        assert_eq!(report.verdicts.get("defect_observed"), Some(&1));
        // Даже отрицательный вердикт не меняет пакет, статус валидатора и хранилище.
        let after = load(&store);
        assert_eq!(after, before);
        assert_eq!(
            after
                .item("幽霊")
                .unwrap()
                .current_candidate()
                .unwrap()
                .validation
                .status,
            SemanticStatus::Verified
        );
        assert_eq!(
            after.item("幽霊").unwrap().status(),
            PitchBatchItemStatus::AcquiredVerified
        );
        assert_eq!(
            std::fs::read(store.root().join("manifest.json")).unwrap(),
            manifest_before
        );
        // Модель не получает словоформу и чтение: только изображение и промпт.
        let chats = mock.chats();
        let text = chats[0].to_string();
        assert!(!text.contains("幽霊") && !text.contains("ゆうれい"));
        assert!(
            chats[0]["messages"][0]["content"][1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(chats[0]["response_format"]["type"], "json_schema");

        let (_, bundle) = crate::pitch_review_decisions::export(&store, BATCH).unwrap();
        let decisions = std::fs::read_to_string(&bundle.decisions).unwrap();
        let row = decisions
            .lines()
            .find(|line| line.starts_with("\t幽霊"))
            .unwrap();
        assert!(row.contains("\tllm:defect_observed\t"));
        // Строка без решения: предложение не записывает ничего в первую колонку.
        assert!(row.starts_with('\t'));
        let html = std::fs::read_to_string(bundle.directory.join("index.html")).unwrap();
        assert!(html.contains("LLM (предложение, не решение): llm:defect_observed"));
        assert!(html.contains("совпадает с чтением «ゆうれい»: да"));

        // Повтор берёт сохранённый результат той же идентичности.
        let (_, report) = run(&store, BATCH, &options).await.unwrap();
        assert_eq!((report.cache_hits, report.requests), (1, 0));
        assert_eq!(mock.chats().len(), 1);
        // Смена промпта — новая идентичность и новый запрос.
        let prompt = workspace.path().join("prompt.txt");
        std::fs::write(&prompt, "Inspect the graph.").unwrap();
        let mut changed = options.clone();
        changed.inference.inference_set.push(format!(
            "tasks.jpdb_pitch_review.prompt={}",
            prompt.display()
        ));
        let (_, report) = run(&store, BATCH, &changed).await.unwrap();
        assert_eq!((report.cache_hits, report.requests), (0, 1));
    }

    #[tokio::test]
    async fn result_for_a_candidate_replaced_during_the_request_is_discarded() {
        let workspace = TempWorkspace::create("pitch-llm-stale").unwrap();
        let store = store_with_candidate(&workspace);
        let mock = MockLlama::new(
            r#"{"verdict":"no_defect_observed","observations":[],"visible_text":"ゆうれい"}"#,
        );
        let root = store.root().to_path_buf();
        mock.state.lock().unwrap().on_chat = Some(Box::new(move || {
            // Пока модель отвечает, человек отправляет элемент на повторное получение.
            let mut runtime = PitchAccentBatchRuntime::open(&root, BATCH).unwrap();
            let mut batch = runtime.load().unwrap().unwrap();
            batch.reacquire("幽霊", "visual problem".into()).unwrap();
            runtime.save(&batch).unwrap();
        }));
        let options = options(external_config(workspace.path(), &mock.url));
        let (_, report) = run(&store, BATCH, &options).await.unwrap();
        assert_eq!((report.saved, report.stale_discarded), (0, 1));
        let runtime = PitchAccentBatchRuntime::open(store.root(), BATCH).unwrap();
        assert!(saved_or_empty(&runtime).is_empty());
    }

    #[tokio::test]
    async fn invalid_model_answer_is_a_failure_not_a_pass() {
        let workspace = TempWorkspace::create("pitch-llm-invalid").unwrap();
        let store = store_with_candidate(&workspace);
        let mock = MockLlama::new("Looks fine to me.");
        let options = options(external_config(workspace.path(), &mock.url));
        let (_, report) = run(&store, BATCH, &options).await.unwrap();
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].code, "invalid_assessment_json");
        assert!(report.verdicts.is_empty());
        let (_, bundle) = crate::pitch_review_decisions::export(&store, BATCH).unwrap();
        let decisions = std::fs::read_to_string(&bundle.decisions).unwrap();
        assert!(decisions.contains("llm_failure:invalid_assessment_json"));
    }

    #[test]
    fn verdict_and_observations_must_agree() {
        let ok = interpret(
            r#"{"verdict":"no_defect_observed","observations":[],"visible_text":"ドア"}"#,
        )
        .unwrap();
        assert_eq!(ok.verdict, Verdict::NoDefectObserved);
        assert_eq!(
            interpret(r#"{"verdict":"no_defect_observed","observations":[{"category":"artifact","detail":"x"}],"visible_text":""}"#).unwrap_err(),
            "inconsistent_verdict"
        );
        assert_eq!(
            interpret(r#"{"verdict":"defect_observed","observations":[],"visible_text":""}"#)
                .unwrap_err(),
            "inconsistent_verdict"
        );
        assert_eq!(
            interpret(r#"{"verdict":"maybe","observations":[],"visible_text":""}"#).unwrap_err(),
            "invalid_assessment_json"
        );
        assert_eq!(
            interpret(
                r#"{"verdict":"insufficient","observations":[],"visible_text":"","extra":1}"#
            )
            .unwrap_err(),
            "invalid_assessment_json"
        );
        let long = "あ".repeat(65);
        assert_eq!(
            interpret(&format!(
                r#"{{"verdict":"insufficient","observations":[],"visible_text":"{long}"}}"#
            ))
            .unwrap_err(),
            "assessment_out_of_bounds"
        );
    }

    #[test]
    fn kana_comparison_folds_script_and_whitespace() {
        assert_eq!(katakana_to_hiragana("ド ア"), "どあ");
        assert_eq!(katakana_to_hiragana("がっこう"), "がっこう");
    }

    #[test]
    fn preparation_is_deterministic_and_rejects_non_png() {
        let mut image = image::RgbaImage::from_pixel(150, 60, image::Rgba([24, 24, 24, 255]));
        image.put_pixel(10, 10, image::Rgba([255, 255, 255, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let first = prepare_image(bytes.get_ref()).unwrap();
        let second = prepare_image(bytes.get_ref()).unwrap();
        assert_eq!(first.png, second.png);
        assert_eq!(first.dimensions, [450, 180]);
        assert!(prepare_image(b"GIF89a").is_err());
    }
}
