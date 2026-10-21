use std::collections::BTreeSet;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use asset_store::domain::AssetDomainPolicy;
use asset_store::hashing::sha256_hex;
use asset_store::jpdb::{
    JpdbPitchAcquisitionReport, JpdbPitchFailure, JpdbPitchOutcome, JpdbPitchProvider,
    JpdbPitchQuery, JpdbPitchRequest, JpdbPitchSelection,
};
use asset_store::model::{
    AssetIdentity, AssetRecord, DetectedFormat, LifecycleState, Provenance, SemanticStatus,
};
use asset_store::pitch_accent::{
    PitchAccentDomainPolicy, PitchAccentImageValidator, jpdb_readings_equivalent,
};
use asset_store::validation::SemanticValidator;
use clap::Parser;
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::instrument::WithSubscriber;

#[path = "common/evidence_zip.rs"]
mod evidence_zip;
use evidence_zip::EvidenceRun;

#[derive(Debug, Parser)]
#[command(
    name = "jpdb_pitch_acceptance",
    about = "Проверить PNG pitch accent, полученные браузером с JPDB, по ожиданиям плана"
)]
struct Args {
    /// JSON-план: каждый элемент задаёт `surface` и обязательный `expected_outcome`.
    /// План может находиться вне рабочего каталога.
    #[arg(long, value_name = "PATH")]
    plan: PathBuf,

    /// Новый ZIP вне checkout и run workspace; parent должен существовать.
    /// По умолчанию: системный temp root/anki-decks-evidence/<уникальный run>.zip.
    #[arg(long, value_name = "ZIP")]
    output: Option<PathBuf>,

    /// Внешний текстовый transcript проверок; безопасная копия попадёт в ZIP.
    #[arg(long, value_name = "PATH")]
    transcript: Option<PathBuf>,

    /// JSON результатов fault injection; отдельная безопасная копия в ZIP.
    #[arg(long, value_name = "PATH")]
    fault_evidence: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PlanItem {
    surface: String,
    reading: Option<String>,
    expected_outcome: ExpectedOutcome,
    expected_vocabulary_id: Option<u64>,
    min_variant_count: Option<u32>,
    expected_candidate_ids: Option<Vec<u64>>,
    selection: Option<JpdbPitchSelection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExpectedOutcome {
    Acquired,
    NoPitchAccentOnSource,
    AmbiguousVocabulary,
    VocabularyNotFound,
}

impl ExpectedOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Acquired => "acquired",
            Self::NoPitchAccentOnSource => "no_pitch_accent_on_source",
            Self::AmbiguousVocabulary => "ambiguous_vocabulary",
            Self::VocabularyNotFound => "vocabulary_not_found",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlan {
    items: Vec<RawPlanItem>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlanItem {
    surface: String,
    reading: Option<String>,
    expected_outcome: ExpectedOutcome,
    expected_vocabulary_id: Option<u64>,
    min_variant_count: Option<u32>,
    expected_candidate_ids: Option<Vec<u64>>,
    selection: Option<RawSelection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSelection {
    vocabulary_id: u64,
    detail_url: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    run(Args::parse()).await
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let plan_path = args.plan.canonicalize()?;
    let plan_json: Value = serde_json::from_slice(&fs::read(&plan_path)?)?;
    let items = read_plan_items(&plan_json)?;

    let run = EvidenceRun::start(
        "jpdb_pitch_acceptance",
        args.output.as_deref(),
        &checkout_root()?,
    )?;
    if let Some(transcript) = &args.transcript
        && let Err(error) = run.add_transcript(transcript)
    {
        return run.finish(Err(error), Ok(()));
    }
    if let Some(source) = &args.fault_evidence
        && let Err(error) = run.add_fault_evidence(source)
    {
        return run.finish(Err(error), Ok(()));
    }
    let log = match run.log_guard() {
        Ok(log) => log,
        Err(error) => return run.finish(Err(error), Ok(())),
    };
    let result = run_body(&items, &plan_path, &run)
        .with_subscriber(log.dispatch())
        .await;
    tokio::task::block_in_place(|| run.finish(result, log.finish()))
}

async fn run_body(
    items: &[PlanItem],
    plan_path: &Path,
    run: &EvidenceRun,
) -> Result<(), Box<dyn std::error::Error>> {
    let report_dir = run.report_dir();
    let requests = items.iter().map(provider_request).collect::<Vec<_>>();

    let acquisition_report =
        JpdbPitchProvider::acquire_requests_in_workspace(&requests, run.path()).await;
    let (rows, all_checks_passed) = process_outcomes(
        items,
        &acquisition_report,
        &report_dir,
        |index, item, outcome, report_dir| Ok(render_outcome(index, item, outcome, report_dir)),
    );

    for row in &rows {
        run.event(json!({"event":"item_result", "result":row}))?;
    }

    let run_status = if all_checks_passed {
        "automatic_checks_passed_waiting_for_user_review"
    } else {
        "verification_failed"
    };
    save_evidence(
        run_status,
        plan_path,
        items,
        &rows,
        &acquisition_report,
        &report_dir,
    )?;

    let verified_count = rows
        .iter()
        .filter(|row| row["item_status"].as_str() == Some("verified_candidate"))
        .count();
    if !all_checks_passed {
        return Err(format!(
            "Проверка плана не пройдена; подтверждено PNG: {verified_count}/{}",
            items.len()
        )
        .into());
    }

    println!(
        "Ожидания плана выполнены; PNG-кандидатов подтверждено: {verified_count}. Проверьте исходы и изображения вручную в ZIP evidence."
    );
    Ok(())
}

fn outcome_name(outcome: &JpdbPitchOutcome) -> &'static str {
    match outcome {
        JpdbPitchOutcome::Acquired { .. } => "acquired",
        JpdbPitchOutcome::NoPitchAccentOnSource { .. } => "no_pitch_accent_on_source",
        JpdbPitchOutcome::AmbiguousVocabulary { .. } => "ambiguous_vocabulary",
        JpdbPitchOutcome::VocabularyNotFound { .. } => "vocabulary_not_found",
        JpdbPitchOutcome::Failed { .. } => "failed",
    }
}

fn outcome_matches_expected(expected: ExpectedOutcome, actual: &str) -> bool {
    expected.as_str() == actual
}

fn provider_request(item: &PlanItem) -> JpdbPitchRequest {
    let query = JpdbPitchQuery {
        surface: item.surface.trim().to_owned(),
        reading: item
            .reading
            .as_deref()
            .map(|reading| reading.trim().to_owned()),
    };
    match item.selection.clone() {
        Some(selection) => JpdbPitchRequest::with_selection(query, selection),
        None => JpdbPitchRequest::new(query),
    }
}

fn process_outcomes<F>(
    items: &[PlanItem],
    acquisition_report: &JpdbPitchAcquisitionReport,
    report_dir: &Path,
    mut render: F,
) -> (Vec<Value>, bool)
where
    F: FnMut(usize, &PlanItem, &JpdbPitchOutcome, &Path) -> Result<(Value, bool), String>,
{
    let outcomes = &acquisition_report.outcomes;
    let mut rows = Vec::with_capacity(items.len().max(outcomes.len()));
    let mut all_checks_passed =
        acquisition_report.session_failure.is_none() && outcomes.len() == items.len();

    for (index, item) in items.iter().enumerate() {
        let Some(outcome) = outcomes.get(index) else {
            all_checks_passed = false;
            rows.push(if acquisition_report.session_failure.is_some() {
                not_started_session_failure_row(index, item)
            } else {
                missing_result_row(index, item)
            });
            continue;
        };

        let (mut row, acquisition_verified) = match render(index, item, outcome, report_dir) {
            Ok(rendered) => rendered,
            Err(error) => {
                all_checks_passed = false;
                let mut row = base_row(index, item, outcome_name(outcome));
                let mut issues = expectation_issues(item, outcome, false);
                issues.push(format!("обработка элемента завершилась ошибкой: {error}"));
                extend_object(
                    &mut row,
                    json!({
                        "item_status": "item_processing_failed",
                        "item_processing_error": error,
                        "expectation_status": "mismatched",
                        "expectation_issues": issues,
                    }),
                );
                rows.push(row);
                continue;
            }
        };

        let issues = expectation_issues(item, outcome, acquisition_verified);
        let expectation_matched = issues.is_empty();
        extend_object(
            &mut row,
            json!({
                "expectation_status": if expectation_matched { "matched" } else { "mismatched" },
                "expectation_issues": issues,
            }),
        );

        if matches!(outcome, JpdbPitchOutcome::Acquired { .. }) {
            row["item_status"] = Value::String(
                if expectation_matched {
                    "verified_candidate"
                } else {
                    "candidate_rejected"
                }
                .into(),
            );
        }
        if !expectation_matched || row.get("item_processing_error").is_some() {
            all_checks_passed = false;
        }
        rows.push(row);
    }

    for (offset, outcome) in outcomes.iter().skip(items.len()).enumerate() {
        all_checks_passed = false;
        rows.push(json!({
            "provider_result_index": items.len() + offset,
            "outcome": outcome_name(outcome),
            "item_status": "unmatched_provider_result",
            "expectation_status": "mismatched",
            "expectation_issues": ["результату провайдера не соответствует элемент плана"],
            "semantic_status": Value::Null,
        }));
    }

    (rows, all_checks_passed)
}

fn base_row(index: usize, item: &PlanItem, outcome: &str) -> Value {
    json!({
        "plan_index": index,
        "surface": item.surface,
        "requested_reading": item.reading,
        "normalized_surface": item.surface.trim(),
        "normalized_reading": item.reading.as_deref().map(str::trim),
        "expected_outcome": item.expected_outcome.as_str(),
        "expected_vocabulary_id": item.expected_vocabulary_id,
        "min_variant_count": item.min_variant_count,
        "expected_candidate_ids": item.expected_candidate_ids,
        "selection": item.selection,
        "outcome": outcome,
    })
}

fn missing_result_row(index: usize, item: &PlanItem) -> Value {
    let mut row = base_row(index, item, "missing_provider_result");
    extend_object(
        &mut row,
        json!({
            "item_status": "provider_result_missing",
            "expectation_status": "mismatched",
            "expectation_issues": ["провайдер не вернул результат для элемента плана"],
            "semantic_status": Value::Null,
        }),
    );
    row
}

fn not_started_session_failure_row(index: usize, item: &PlanItem) -> Value {
    let mut row = base_row(index, item, "not_started");
    extend_object(
        &mut row,
        json!({
            "item_status": "not_started_session_failure",
            "session_failure_ref": "#/session_failure",
            "expectation_status": "not_evaluated",
            "expectation_issues": ["обработка элемента не началась: сессия браузера остановлена"],
            "semantic_status": Value::Null,
        }),
    );
    row
}

fn render_outcome(
    index: usize,
    item: &PlanItem,
    outcome: &JpdbPitchOutcome,
    report_dir: &Path,
) -> (Value, bool) {
    let mut row = base_row(index, item, outcome_name(outcome));
    let mut acquisition_verified = false;

    match outcome {
        JpdbPitchOutcome::Acquired { asset } => {
            let metadata = &asset.metadata;
            let bytes = &asset.bytes;
            let sha256 = sha256_hex(bytes);
            let png_signature_valid = bytes.starts_with(b"\x89PNG\r\n\x1a\n");
            let decoded = image::load_from_memory_with_format(bytes, image::ImageFormat::Png);
            let dimensions = decoded.as_ref().ok().map(GenericImageView::dimensions);

            // В отчёт записываются только исходные байты результата `Acquired`. Невалидный PNG
            // остаётся доступен как .bin и не встраивается в HTML под видом картинки.
            let extension = if png_signature_valid && dimensions.is_some() {
                "png"
            } else {
                "bin"
            };
            let image_relative_path = format!("images/{:04}.{extension}", index + 1);
            let image_write_error = fs::create_dir_all(report_dir.join("images"))
                .and_then(|()| fs::write(report_dir.join(&image_relative_path), bytes))
                .err()
                .map(|error| error.to_string());

            let metadata_matches_plan = metadata.surface == item.surface.trim()
                && item.reading.as_deref().is_none_or(|reading| {
                    jpdb_readings_equivalent(&metadata.reading, reading.trim())
                });
            let validation = match AssetIdentity::new("pitch_accent", metadata.surface.clone()) {
                Ok(identity) => match serde_json::to_value(metadata) {
                    Ok(domain_metadata) => {
                        let provisional = AssetRecord {
                            identity,
                            // Временная запись передаёт действующему валидатору фактические
                            // метаданные провайдера; приёмочный инструмент не создаёт каноническое хранилище.
                            storage_path: String::new(),
                            consumer_filename: String::new(),
                            sha256: sha256.clone(),
                            byte_length: bytes.len() as u64,
                            format: DetectedFormat::from_signature(bytes),
                            provenance: Provenance {
                                source_kind: "jpdb_browser_render".into(),
                                source_name: format!(
                                    "jpdb-vocabulary-{}.png",
                                    metadata.jpdb_vocabulary_id
                                ),
                            },
                            lifecycle: LifecycleState::Pending,
                            validation: None,
                            human_attestation: None,
                            domain_metadata: Some(domain_metadata),
                        };
                        PitchAccentImageValidator
                            .validate(&provisional, &mut Cursor::new(bytes))
                            .map_err(
                                |error| json!({ "code": error.code, "message": error.message }),
                            )
                    }
                    Err(error) => Err(json!({
                        "code": "metadata_serialization_failed",
                        "message": error.to_string(),
                    })),
                },
                Err(error) => Err(json!({
                    "code": "invalid_asset_identity",
                    "message": error,
                })),
            };

            let (semantic_status, validation_evidence, validator_failure) = match validation {
                Ok(decision) => (
                    Some(decision.status.as_str().to_owned()),
                    serde_json::to_value(decision.evidence).unwrap_or(Value::Null),
                    Value::Null,
                ),
                Err(error) => (None, Value::Null, error),
            };
            acquisition_verified = semantic_status.as_deref()
                == Some(SemanticStatus::Verified.as_str())
                && metadata_matches_plan
                && png_signature_valid
                && dimensions.is_some()
                && image_write_error.is_none();

            extend_object(
                &mut row,
                json!({
                    "jpdb_vocabulary_id": metadata.jpdb_vocabulary_id,
                    "detail_url": metadata.evidence.source_url,
                    "canonical_reading": metadata.reading,
                    "detected_variant_count": metadata.evidence.variant_selection.detected_variant_count,
                    "selected_variant_index": metadata.evidence.variant_selection.selected_index,
                    "selection_basis": metadata.evidence.variant_selection.basis,
                    "captured_graph_count": metadata.evidence.render.captured_graph_count,
                    "variant_selection": metadata.evidence.variant_selection,
                    "dark_theme_proof": metadata.evidence.render.dark_theme,
                    "capture_geometry": metadata.evidence.render,
                    "browser": metadata.evidence.browser,
                    "metadata_matches_plan": metadata_matches_plan,
                    "sha256": sha256,
                    "byte_length": bytes.len(),
                    "png_signature_valid": png_signature_valid,
                    "dimensions": dimensions,
                    "semantic_status": semantic_status,
                    "validation_evidence": validation_evidence,
                    "validator_failure": validator_failure,
                    "image_path": if image_write_error.is_none() { Some(image_relative_path) } else { None },
                    "image_mime": if extension == "png" { "image/png" } else { "application/octet-stream" },
                    "image_write_error": image_write_error,
                    "acquisition_validation_verified": acquisition_verified,
                    "item_status": if acquisition_verified { "verified_candidate" } else { "candidate_rejected" },
                }),
            );
        }
        JpdbPitchOutcome::NoPitchAccentOnSource { evidence } => {
            extend_object(
                &mut row,
                json!({
                    "absence_evidence": evidence,
                    "jpdb_vocabulary_id": evidence.jpdb_vocabulary_id,
                    "detail_url": evidence.source_url,
                    "item_status": "no_pitch_accent_on_source",
                    "semantic_status": Value::Null,
                }),
            );
        }
        JpdbPitchOutcome::AmbiguousVocabulary {
            surface,
            reading,
            candidates,
        } => {
            extend_object(
                &mut row,
                json!({
                    "resolved_surface": surface,
                    "resolved_reading": reading,
                    "candidates": candidates,
                    "candidate_ids": candidates.iter().map(|candidate| candidate.vocabulary_id).collect::<Vec<_>>(),
                    "item_status": "ambiguous_vocabulary",
                    "semantic_status": Value::Null,
                }),
            );
        }
        JpdbPitchOutcome::VocabularyNotFound { surface, reading } => {
            extend_object(
                &mut row,
                json!({
                    "resolved_surface": surface,
                    "resolved_reading": reading,
                    "item_status": "vocabulary_not_found",
                    "semantic_status": Value::Null,
                }),
            );
        }
        JpdbPitchOutcome::Failed { error } => {
            extend_object(
                &mut row,
                json!({
                    "failure": error,
                    "item_status": "acquisition_failed",
                    "semantic_status": Value::Null,
                }),
            );
        }
    }

    (row, acquisition_verified)
}

fn expectation_issues(
    item: &PlanItem,
    outcome: &JpdbPitchOutcome,
    acquisition_verified: bool,
) -> Vec<String> {
    let mut issues = Vec::new();
    if !outcome_matches_expected(item.expected_outcome, outcome_name(outcome)) {
        issues.push(format!(
            "ожидался исход `{}`, получен `{}`",
            item.expected_outcome.as_str(),
            outcome_name(outcome)
        ));
        return issues;
    }

    match outcome {
        JpdbPitchOutcome::Acquired { asset } => {
            let metadata = &asset.metadata;
            issues.extend(acquired_expectation_issues(
                item,
                &metadata.surface,
                &metadata.reading,
                metadata.jpdb_vocabulary_id,
                metadata.evidence.variant_selection.detected_variant_count,
                metadata.evidence.render.captured_graph_count,
                acquisition_verified,
            ));
        }
        JpdbPitchOutcome::NoPitchAccentOnSource { evidence } => {
            let resolved_query_pair = evidence.resolved_forms.iter().any(|form| {
                form.surface == item.surface.trim()
                    && !form.reading.trim().is_empty()
                    && item.reading.as_deref().is_none_or(|reading| {
                        jpdb_readings_equivalent(&form.reading, reading.trim())
                    })
            });
            if evidence.surface != item.surface.trim()
                || item.reading.as_deref().is_some_and(|reading| {
                    !jpdb_readings_equivalent(&evidence.reading, reading.trim())
                })
                || !resolved_query_pair
                || JpdbPitchSelection::new(evidence.jpdb_vocabulary_id, evidence.source_url.clone())
                    .is_err()
                || !evidence.base_page_contract_valid
                || !evidence
                    .section_inventory
                    .iter()
                    .any(|label| label == "Meanings")
                || evidence
                    .section_inventory
                    .iter()
                    .any(|label| label == "Pitch accent")
                || evidence.pitch_section_present
                || evidence.pitch_marker_count != 0
            {
                issues.push(
                    "свидетельства отсутствия pitch accent не подтверждают связанную пару запроса или контракт страницы источника"
                        .into(),
                );
            }
        }
        JpdbPitchOutcome::AmbiguousVocabulary {
            surface,
            reading,
            candidates,
        } => {
            if surface != item.surface.trim()
                || item.reading.as_deref().is_some_and(|requested| {
                    reading
                        .as_deref()
                        .is_none_or(|actual| !jpdb_readings_equivalent(actual, requested.trim()))
                })
            {
                issues.push("результат ambiguity не совпадает с запросом".into());
            }
            if candidates.is_empty() {
                issues.push("ambiguity не содержит кандидатов".into());
            }
            let mut candidate_ids = BTreeSet::new();
            for candidate in candidates {
                if candidate.vocabulary_id == 0 || !candidate_ids.insert(candidate.vocabulary_id) {
                    issues.push(
                        "результат `ambiguous_vocabulary` содержит нулевой или повторный ID словарной записи"
                            .into(),
                    );
                }
                if JpdbPitchSelection::new(candidate.vocabulary_id, candidate.detail_url.clone())
                    .is_err()
                {
                    issues.push(format!(
                        "URL словарной записи кандидата {} не соответствует его ID JPDB",
                        candidate.vocabulary_id
                    ));
                }
                let query_pair_is_present = candidate.resolved_forms.iter().any(|form| {
                    form.surface == item.surface.trim()
                        && !form.reading.trim().is_empty()
                        && item.reading.as_deref().is_none_or(|reading| {
                            jpdb_readings_equivalent(&form.reading, reading.trim())
                        })
                });
                if candidate.resolved_forms.is_empty() || !query_pair_is_present {
                    issues.push(format!(
                        "candidate {} не содержит связанную пару написания и чтения запроса",
                        candidate.vocabulary_id
                    ));
                }
                let has_part_of_speech = candidate
                    .part_of_speech
                    .iter()
                    .any(|value| !value.trim().is_empty());
                let has_meaning = candidate
                    .meanings
                    .iter()
                    .any(|value| !value.trim().is_empty());
                if !has_part_of_speech || !has_meaning {
                    issues.push(format!(
                        "candidate {} не содержит части речи и значения",
                        candidate.vocabulary_id
                    ));
                }
            }
            if let Some(expected_ids) = &item.expected_candidate_ids {
                let actual_ids = candidates
                    .iter()
                    .map(|candidate| candidate.vocabulary_id)
                    .collect::<BTreeSet<_>>();
                let expected_ids = expected_ids.iter().copied().collect::<BTreeSet<_>>();
                let missing_ids = expected_ids
                    .difference(&actual_ids)
                    .copied()
                    .collect::<Vec<_>>();
                let unexpected_ids = actual_ids
                    .difference(&expected_ids)
                    .copied()
                    .collect::<Vec<_>>();
                if !missing_ids.is_empty() || !unexpected_ids.is_empty() {
                    issues.push(format!(
                        "набор ID кандидатов неоднозначного результата не совпадает: отсутствуют {missing_ids:?}, лишние {unexpected_ids:?}"
                    ));
                }
            }
        }
        JpdbPitchOutcome::VocabularyNotFound { surface, reading } => {
            if surface != item.surface.trim()
                || item.reading.as_deref().is_some_and(|requested| {
                    reading
                        .as_deref()
                        .is_none_or(|actual| !jpdb_readings_equivalent(actual, requested.trim()))
                })
            {
                issues.push("результат not-found не совпадает с запросом".into());
            }
        }
        JpdbPitchOutcome::Failed { .. } => {
            issues.push("техническая ошибка не является ожидаемым исходом плана".into());
        }
    }
    issues
}

fn acquired_expectation_issues(
    item: &PlanItem,
    actual_surface: &str,
    actual_reading: &str,
    actual_vocabulary_id: u64,
    actual_variant_count: u32,
    captured_graph_count: u32,
    acquisition_verified: bool,
) -> Vec<String> {
    let mut issues = Vec::new();
    if actual_surface != item.surface.trim() {
        issues.push("результат `acquired` содержит другое написание".into());
    }
    if item
        .reading
        .as_deref()
        .is_some_and(|reading| !jpdb_readings_equivalent(actual_reading, reading.trim()))
    {
        issues.push("результат `acquired` содержит другое чтение".into());
    }
    if let Some(expected_id) = item.expected_vocabulary_id
        && actual_vocabulary_id != expected_id
    {
        issues.push(format!(
            "ожидался ID словарной записи {expected_id}, получен {actual_vocabulary_id}"
        ));
    }
    if let Some(min_variant_count) = item.min_variant_count
        && actual_variant_count < min_variant_count
    {
        issues.push(format!(
            "ожидалось не менее {min_variant_count} обнаруженных вариантов графика, получено {actual_variant_count}"
        ));
    }
    if captured_graph_count != 1 {
        issues.push(format!(
            "PNG должен содержать ровно один график, в свидетельствах {captured_graph_count}"
        ));
    }
    if !acquisition_verified {
        issues.push(
            "acquired не прошёл production-валидатор, проверку PNG или сохранения отчёта".into(),
        );
    }
    issues
}

fn extend_object(target: &mut Value, extra: Value) {
    if let (Some(target), Some(extra)) = (target.as_object_mut(), extra.as_object()) {
        target.extend(
            extra
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
    }
}

fn read_plan_items(plan: &Value) -> Result<Vec<PlanItem>, Box<dyn std::error::Error>> {
    let raw: RawPlan = serde_json::from_value(plan.clone()).map_err(|error| {
        format!("неверная структура JSON-плана (проверьте поля и типы): {error}")
    })?;
    if raw.items.is_empty() {
        return Err("plan.items не должен быть пустым".into());
    }

    let policy = PitchAccentDomainPolicy;
    let mut items = Vec::with_capacity(raw.items.len());
    let mut seen_requests = BTreeSet::new();
    for (index, raw_item) in raw.items.into_iter().enumerate() {
        validate_plan_text(&raw_item.surface, "surface", index)?;
        if raw_item.surface.trim().is_empty() {
            return Err(format!("plan.items[{index}].surface не должен быть пустым").into());
        }
        if let Some(reading) = &raw_item.reading {
            validate_plan_text(reading, "reading", index)?;
            if reading.trim().is_empty() {
                return Err(format!("plan.items[{index}].reading не должен быть пустым").into());
            }
        }

        let normalized_surface = raw_item.surface.trim();
        let identity = AssetIdentity::new("pitch_accent", normalized_surface)?;
        policy.validate_identity(&identity).map_err(|error| {
            format!(
                "plan.items[{index}].surface нельзя использовать в имени файла consumer-а: {error}"
            )
        })?;

        if raw_item.expected_vocabulary_id == Some(0) {
            return Err(format!(
                "plan.items[{index}].expected_vocabulary_id должен быть положительным"
            )
            .into());
        }
        if raw_item.min_variant_count == Some(0) {
            return Err(
                format!("plan.items[{index}].min_variant_count должен быть положительным").into(),
            );
        }
        if raw_item
            .expected_candidate_ids
            .as_ref()
            .is_some_and(Vec::is_empty)
        {
            return Err(format!(
                "plan.items[{index}].expected_candidate_ids не должен быть пустым"
            )
            .into());
        }
        if let Some(candidate_ids) = &raw_item.expected_candidate_ids {
            let mut unique = BTreeSet::new();
            if candidate_ids
                .iter()
                .any(|candidate_id| *candidate_id == 0 || !unique.insert(*candidate_id))
            {
                return Err(format!("plan.items[{index}].expected_candidate_ids должен содержать уникальные положительные ID").into());
            }
        }

        if raw_item.expected_vocabulary_id.is_some()
            && raw_item.expected_outcome != ExpectedOutcome::Acquired
        {
            return Err(format!("plan.items[{index}].expected_vocabulary_id допустим только для expected_outcome=acquired").into());
        }
        if raw_item.min_variant_count.is_some()
            && raw_item.expected_outcome != ExpectedOutcome::Acquired
        {
            return Err(format!(
                "plan.items[{index}].min_variant_count допустим только для expected_outcome=acquired"
            )
            .into());
        }
        if raw_item.expected_candidate_ids.is_some()
            && raw_item.expected_outcome != ExpectedOutcome::AmbiguousVocabulary
        {
            return Err(format!("plan.items[{index}].expected_candidate_ids допустим только для expected_outcome=ambiguous_vocabulary").into());
        }

        let selection = raw_item
            .selection
            .map(|selection| {
                JpdbPitchSelection::new(selection.vocabulary_id, selection.detail_url).map_err(
                    |message| {
                        format!("plan.items[{index}].selection не прошёл проверку: {message}")
                    },
                )
            })
            .transpose()?;
        if let Some(selection) = &selection {
            if raw_item.expected_outcome != ExpectedOutcome::Acquired {
                return Err(format!(
                    "plan.items[{index}].selection допустим только при expected_outcome=acquired"
                )
                .into());
            }
            if raw_item.expected_vocabulary_id != Some(selection.vocabulary_id) {
                return Err(format!("plan.items[{index}].selection.vocabulary_id должен совпадать с expected_vocabulary_id").into());
            }
        }

        let duplicate_key = (
            normalized_surface.to_owned(),
            raw_item
                .reading
                .as_deref()
                .map(|reading| reading.trim().to_owned()),
            selection
                .as_ref()
                .map(|selection| (selection.vocabulary_id, selection.detail_url.clone())),
        );
        if !seen_requests.insert(duplicate_key) {
            return Err(format!(
                "повтор query/selection в плане для surface: {}",
                raw_item.surface
            )
            .into());
        }

        items.push(PlanItem {
            surface: raw_item.surface,
            reading: raw_item.reading,
            expected_outcome: raw_item.expected_outcome,
            expected_vocabulary_id: raw_item.expected_vocabulary_id,
            min_variant_count: raw_item.min_variant_count,
            expected_candidate_ids: raw_item.expected_candidate_ids,
            selection,
        });
    }
    Ok(items)
}

fn validate_plan_text(
    value: &str,
    name: &str,
    index: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if value.chars().any(char::is_control) {
        return Err(
            format!("plan.items[{index}].{name} не должен содержать управляющие символы").into(),
        );
    }
    Ok(())
}

fn checkout_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let package_dir = Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize()?;
    let workspace_root = package_dir
        .parent()
        .and_then(Path::parent)
        .ok_or("не удалось определить корень checkout")?;
    Ok(workspace_root.canonicalize()?)
}

fn save_evidence(
    run_status: &str,
    _plan_path: &Path,
    items: &[PlanItem],
    rows: &[Value],
    acquisition_report: &JpdbPitchAcquisitionReport,
    report_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut rows = rows.to_vec();
    rows.iter_mut().for_each(evidence_zip::redact);
    let mut report = json!({
        "schema_version": 2,
        "run_status": run_status,
        "planned_item_count": items.len(),
        "processed_item_count": acquisition_report.outcomes.len(),
        "session_failure": acquisition_report.session_failure,
        "validator": PitchAccentImageValidator::validator_identity(),
        "items": rows,
    });
    evidence_zip::redact(&mut report);
    fs::write(
        report_dir.join("evidence.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    save_html_report(
        run_status,
        items,
        &rows,
        acquisition_report.session_failure.as_ref(),
        report_dir,
    )?;
    Ok(())
}

fn save_html_report(
    run_status: &str,
    items: &[PlanItem],
    rows: &[Value],
    session_failure: Option<&JpdbPitchFailure>,
    report_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut html = format!(
        r##"<!doctype html>
<html lang="ru">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>JPDB — приёмка pitch-accent PNG</title>
<style>
body{{font:16px/1.5 system-ui,sans-serif;margin:2rem;color:#202428;background:#f4f6f8}}
h1,h2{{line-height:1.2}}.summary{{background:#fff;border:1px solid #c7cdd2;padding:1rem;max-width:75rem}}
.items{{display:grid;grid-template-columns:repeat(auto-fit,minmax(30rem,1fr));gap:1rem}}
article{{background:#fff;border:1px solid #c7cdd2;padding:1rem;min-width:0}}
article h2{{font-size:1.2rem;margin:.1rem 0 .5rem}}.status{{font-weight:700}}
.image-viewport{{overflow:auto;max-width:100%;background:#e4e8eb;border:1px solid #c7cdd2;padding:.5rem}}
.image-viewport img{{display:block;width:auto;height:auto;max-width:none;max-height:none}}
dl{{display:grid;grid-template-columns:max-content 1fr;gap:.15rem .75rem;margin:.8rem 0 0}}
dt{{font-weight:650}}dd{{margin:0;overflow-wrap:anywhere}}code{{font-size:.9em;overflow-wrap:anywhere}}
pre{{white-space:pre-wrap;overflow-wrap:anywhere;background:#f5f6f7;padding:.6rem}}
small{{color:#505960}}
</style>
</head>
<body>
<h1>JPDB — приёмка pitch-accent PNG</h1>
<div class="summary">
<p><strong>Статус прогона:</strong> {}</p>
<p>Элементов плана: {}. Для <code>acquired</code> показаны исходные байты из браузера в естественном размере, свидетельства действующего валидатора и сверка с ожиданиями.</p>
<p><a href="evidence.json">Машиночитаемый отчёт (JSON)</a></p>
</div>
"##,
        escape_html(run_status),
        items.len()
    );

    if let Some(error) = session_failure {
        let mut safe_error = serde_json::to_value(error)?;
        evidence_zip::redact(&mut safe_error);
        html.push_str(&format!(
            "<div class=\"summary\" id=\"session-failure\"><p><strong>Ошибка сессии браузера:</strong></p><pre>{}</pre></div>",
            escape_html(&serde_json::to_string_pretty(&safe_error)?)
        ));
    }

    html.push_str("<section class=\"items\">");
    for (index, item) in items.iter().enumerate() {
        append_html_item(&mut html, index, item, rows.get(index));
    }
    html.push_str("</section></body></html>\n");
    fs::write(report_dir.join("index.html"), html)?;
    Ok(())
}

fn append_html_item(html: &mut String, index: usize, item: &PlanItem, row: Option<&Value>) {
    let row = row.cloned().unwrap_or(Value::Null);
    let subtitle = item
        .reading
        .as_deref()
        .map(|reading| format!("{} · {}", item.surface, reading))
        .unwrap_or_else(|| item.surface.clone());
    html.push_str(&format!(
        "<article><h2>№ {} · {}</h2><p class=\"status\">{}</p>",
        index + 1,
        escape_html(&subtitle),
        escape_html(
            row["item_status"]
                .as_str()
                .unwrap_or("provider_result_missing")
        )
    ));
    if row["session_failure_ref"].as_str().is_some() {
        html.push_str("<p><a href=\"#session-failure\">Причина остановки сессии браузера</a></p>");
    }
    if let Some(image_path) = row["image_path"].as_str() {
        if row["image_mime"].as_str() == Some("image/png") {
            html.push_str(&format!(
                "<div class=\"image-viewport\"><a href=\"{}\"><img src=\"{}\" alt=\"{}\"></a></div>",
                escape_html(image_path),
                escape_html(image_path),
                escape_html(&item.surface)
            ));
        } else {
            html.push_str(&format!(
                "<p>Полученные исходные байты: <a href=\"{}\">скачать</a> (PNG не подтверждён).</p>",
                escape_html(image_path)
            ));
        }
    }

    let fields = vec![
        ("Ожидаемый исход", value_text(&row["expected_outcome"])),
        ("Фактический исход", value_text(&row["outcome"])),
        (
            "Сверка с ожиданиями",
            value_text(&row["expectation_status"]),
        ),
        ("Расхождения плана", value_text(&row["expectation_issues"])),
        (
            "Запрошенное чтение",
            item.reading.clone().unwrap_or_else(|| "—".into()),
        ),
        (
            "ID словарной записи",
            value_text(&row["jpdb_vocabulary_id"]),
        ),
        ("Явный выбор", value_text(&row["selection"])),
        (
            "Ожидаемые ID кандидатов",
            value_text(&row["expected_candidate_ids"]),
        ),
        ("Кандидаты", value_text(&row["candidates"])),
        ("URL словарной записи", value_text(&row["detail_url"])),
        ("Каноническое чтение", value_text(&row["canonical_reading"])),
        (
            "Обнаружено вариантов графика",
            value_text(&row["detected_variant_count"]),
        ),
        (
            "Выбранный вариант (индекс DOM)",
            value_text(&row["selected_variant_index"]),
        ),
        ("Основание выбора", value_text(&row["selection_basis"])),
        ("Графиков в PNG", value_text(&row["captured_graph_count"])),
        (
            "Варианты и оценки выбора",
            value_text(&row["variant_selection"]),
        ),
        (
            "Геометрия захвата (CSS px, координаты документа)",
            value_text(&row["capture_geometry"]),
        ),
        (
            "Наблюдаемая тёмная тема",
            value_text(&row["dark_theme_proof"]),
        ),
        ("Размер PNG в пикселях", value_text(&row["dimensions"])),
        ("SHA-256", value_text(&row["sha256"])),
        (
            "Метаданные совпадают с планом",
            value_text(&row["metadata_matches_plan"]),
        ),
        (
            "Ошибка обработки элемента",
            value_text(&row["item_processing_error"]),
        ),
        (
            "Ошибка сохранения PNG",
            value_text(&row["image_write_error"]),
        ),
    ];

    html.push_str("<dl>");
    for (label, value) in fields {
        html.push_str(&format!(
            "<dt>{}</dt><dd>{}</dd>",
            escape_html(label),
            escape_html(&value)
        ));
    }
    html.push_str("</dl>");
    if row["absence_evidence"].is_object() {
        html.push_str(&format!(
            "<details><summary>Доказательство отсутствия pitch accent</summary><pre>{}</pre></details>",
            escape_html(&value_text(&row["absence_evidence"]))
        ));
    }
    if row["failure"].is_object() {
        html.push_str(&format!(
            "<details><summary>Ошибка provider</summary><pre>{}</pre></details>",
            escape_html(&value_text(&row["failure"]))
        ));
    }
    if row["validator_failure"].is_object() {
        html.push_str(&format!(
            "<details><summary>Ошибка validator</summary><pre>{}</pre></details>",
            escape_html(&value_text(&row["validator_failure"]))
        ));
    }
    if row["validation_evidence"].is_array() {
        html.push_str(&format!(
            "<details><summary>Доказательства валидатора</summary><pre>{}</pre></details>",
            escape_html(&value_text(&row["validation_evidence"]))
        ));
    }
    if let Some(browser) = row.get("browser") {
        html.push_str(&format!(
            "<details><summary>Среда выполнения браузера</summary><pre>{}</pre></details>",
            escape_html(&value_text(browser))
        ));
    }
    html.push_str("</article>");
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Null => "—".to_owned(),
        Value::String(value) => value.clone(),
        _ => serde_json::to_string_pretty(value).unwrap_or_else(|_| "—".to_owned()),
    }
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn plan_item(expected_outcome: ExpectedOutcome) -> PlanItem {
        PlanItem {
            surface: "合成語".into(),
            reading: Some("ごうせいご".into()),
            expected_outcome,
            expected_vocabulary_id: None,
            min_variant_count: None,
            expected_candidate_ids: None,
            selection: None,
        }
    }

    fn ambiguity(item: &PlanItem, candidate_ids: &[u64]) -> JpdbPitchOutcome {
        JpdbPitchOutcome::AmbiguousVocabulary {
            surface: item.surface.clone(),
            reading: item.reading.clone(),
            candidates: candidate_ids
                .iter()
                .map(|vocabulary_id| asset_store::jpdb::JpdbVocabularyCandidate {
                    vocabulary_id: *vocabulary_id,
                    surface_forms: vec![item.surface.clone()],
                    readings: item.reading.iter().cloned().collect(),
                    resolved_forms: vec![asset_store::pitch_accent::PitchAccentResolvedForm {
                        surface: item.surface.trim().into(),
                        reading: item
                            .reading
                            .as_deref()
                            .unwrap_or("ごうせいご")
                            .trim()
                            .into(),
                    }],
                    part_of_speech: vec!["noun".into()],
                    meanings: vec!["синтетическое значение".into()],
                    detail_url: format!(
                        "https://jpdb.io/vocabulary/{vocabulary_id}/合成語/ごうせいご"
                    ),
                })
                .collect(),
        }
    }

    #[test]
    fn plan_requires_expectation_and_accepts_each_supported_outcome() {
        for token in [
            "acquired",
            "no_pitch_accent_on_source",
            "ambiguous_vocabulary",
            "vocabulary_not_found",
        ] {
            let plan = json!({"items": [{"surface": "合成語", "expected_outcome": token}]});
            assert_eq!(read_plan_items(&plan).unwrap().len(), 1);
        }
        assert!(read_plan_items(&json!({"items": [{"surface": "合成語"}]})).is_err());
    }

    #[test]
    fn query_boundary_trims_input_but_plan_keeps_raw_text_for_diagnostics() {
        let plan = json!({"items": [{
            "surface": "  合成語  ",
            "reading": " ごうせいご ",
            "expected_outcome": "vocabulary_not_found"
        }]});
        let items = read_plan_items(&plan).unwrap();
        assert_eq!(items[0].surface, "  合成語  ");
        assert_eq!(items[0].reading.as_deref(), Some(" ごうせいご "));
        let request = provider_request(&items[0]);
        assert_eq!(request.query.surface, "合成語");
        assert_eq!(request.query.reading.as_deref(), Some("ごうせいご"));

        let duplicate_after_normalization = json!({"items": [
            {"surface": "合成語", "expected_outcome": "ambiguous_vocabulary"},
            {"surface": " 合成語 ", "expected_outcome": "vocabulary_not_found"}
        ]});
        assert!(read_plan_items(&duplicate_after_normalization).is_err());
    }

    #[test]
    fn expectation_matrix_accepts_only_the_requested_domain_outcome() {
        let cases = [
            ExpectedOutcome::Acquired,
            ExpectedOutcome::NoPitchAccentOnSource,
            ExpectedOutcome::AmbiguousVocabulary,
            ExpectedOutcome::VocabularyNotFound,
        ];
        let actual_tokens = [
            "acquired",
            "no_pitch_accent_on_source",
            "ambiguous_vocabulary",
            "vocabulary_not_found",
        ];
        for (expected_index, expected) in cases.into_iter().enumerate() {
            assert_eq!(expected.as_str(), actual_tokens[expected_index]);
            for (actual_index, actual) in actual_tokens.iter().enumerate() {
                assert_eq!(
                    outcome_matches_expected(expected, actual),
                    expected_index == actual_index
                );
            }
        }
    }

    #[test]
    fn positive_expectation_rejects_not_found_and_expected_negative_outcomes_can_pass() {
        let mut positive = plan_item(ExpectedOutcome::Acquired);
        positive.expected_vocabulary_id = Some(42);
        let not_found = JpdbPitchOutcome::VocabularyNotFound {
            surface: positive.surface.clone(),
            reading: positive.reading.clone(),
        };
        assert!(!expectation_issues(&positive, &not_found, false).is_empty());

        let mut ambiguity_plan = plan_item(ExpectedOutcome::AmbiguousVocabulary);
        ambiguity_plan.expected_candidate_ids = Some(vec![42, 43]);
        let ambiguity_outcome = ambiguity(&ambiguity_plan, &[43, 42]);
        assert!(expectation_issues(&ambiguity_plan, &ambiguity_outcome, false).is_empty());

        let not_found_plan = plan_item(ExpectedOutcome::VocabularyNotFound);
        assert!(expectation_issues(&not_found_plan, &not_found, false).is_empty());
        let no_pitch_plan = plan_item(ExpectedOutcome::NoPitchAccentOnSource);
        let no_pitch = JpdbPitchOutcome::NoPitchAccentOnSource {
            evidence: asset_store::jpdb::JpdbPitchAbsenceEvidence {
                surface: no_pitch_plan.surface.clone(),
                reading: no_pitch_plan.reading.clone().unwrap(),
                jpdb_vocabulary_id: 42,
                source_url: "https://jpdb.io/vocabulary/42/合成語/ごうせいご".into(),
                resolved_forms: vec![asset_store::pitch_accent::PitchAccentResolvedForm {
                    surface: no_pitch_plan.surface.trim().into(),
                    reading: no_pitch_plan.reading.clone().unwrap().trim().into(),
                }],
                section_inventory: vec!["Meanings".into(), "Forms".into()],
                base_page_contract_valid: true,
                pitch_section_present: false,
                pitch_marker_count: 0,
                browser: asset_store::browser_runtime::BrowserRuntimeProvenance {
                    product: "Chromium".into(),
                    protocol_version: "1.3".into(),
                    revision: "synthetic".into(),
                    user_agent: "synthetic".into(),
                    js_version: "synthetic".into(),
                    executable_source:
                        asset_store::browser_runtime::BrowserExecutableSource::PathLookup,
                },
            },
        };
        assert!(expectation_issues(&no_pitch_plan, &no_pitch, false).is_empty());
        let unexpected_candidate = ambiguity(&ambiguity_plan, &[42, 43, 44]);
        assert!(!expectation_issues(&ambiguity_plan, &unexpected_candidate, false).is_empty());
        let mut missing_meaning = ambiguity(&ambiguity_plan, &[42, 43]);
        if let JpdbPitchOutcome::AmbiguousVocabulary { candidates, .. } = &mut missing_meaning {
            candidates[0].meanings.clear();
        }
        assert!(!expectation_issues(&ambiguity_plan, &missing_meaning, false).is_empty());
        let missing_candidate = ambiguity(&ambiguity_plan, &[42]);
        assert!(!expectation_issues(&ambiguity_plan, &missing_candidate, false).is_empty());
    }

    #[test]
    fn acquired_requires_verified_status_and_all_declared_constraints() {
        let mut item = plan_item(ExpectedOutcome::Acquired);
        item.expected_vocabulary_id = Some(42);
        item.min_variant_count = Some(2);
        assert!(
            acquired_expectation_issues(&item, "合成語", "ごうせいご", 42, 2, 1, true).is_empty()
        );
        let mut katakana_surface = plan_item(ExpectedOutcome::Acquired);
        katakana_surface.surface = "ネコ".into();
        katakana_surface.reading = Some("ねこ".into());
        assert!(
            acquired_expectation_issues(&katakana_surface, "ネコ", "ネコ", 1467640, 1, 1, true)
                .is_empty()
        );
        let issues = acquired_expectation_issues(&item, "合成語", "ごうせいご", 43, 1, 1, false);
        assert_eq!(issues.len(), 3);
        let issues = acquired_expectation_issues(&item, "合成語", "ごうせいご", 42, 3, 3, true);
        assert_eq!(
            issues,
            ["PNG должен содержать ровно один график, в свидетельствах 3"],
            "обнаружение нескольких вариантов допустимо, несколько графиков в PNG — нет"
        );
    }

    #[test]
    fn plan_prevalidates_explicit_selection_identity_and_candidate_constraints() {
        let valid_selection = json!({
            "items": [{
                "surface": "合成語",
                "reading": "ごうせいご",
                "expected_outcome": "acquired",
                "expected_vocabulary_id": 42,
                "min_variant_count": 2,
                "selection": {
                    "vocabulary_id": 42,
                    "detail_url": "https://jpdb.io/vocabulary/42/合成語/ごうせいご"
                }
            }]
        });
        assert!(read_plan_items(&valid_selection).is_ok());

        let invalid_selection_id = json!({
            "items": [{
                "surface": "合成語",
                "expected_outcome": "acquired",
                "expected_vocabulary_id": 42,
                "selection": {
                    "vocabulary_id": 43,
                    "detail_url": "https://jpdb.io/vocabulary/42/合成語/ごうせいご"
                }
            }]
        });
        assert!(read_plan_items(&invalid_selection_id).is_err());

        let invalid_selection_route = json!({
            "items": [{
                "surface": "合成語",
                "expected_outcome": "acquired",
                "expected_vocabulary_id": 42,
                "selection": {
                    "vocabulary_id": 42,
                    "detail_url": "https://example.com/vocabulary/42/合成語/ごうせいご"
                }
            }]
        });
        assert!(read_plan_items(&invalid_selection_route).is_err());

        let invalid_selection_expectation = json!({
            "items": [{
                "surface": "合成語",
                "expected_outcome": "ambiguous_vocabulary",
                "selection": {
                    "vocabulary_id": 42,
                    "detail_url": "https://jpdb.io/vocabulary/42/合成語/ごうせいご"
                }
            }]
        });
        assert!(read_plan_items(&invalid_selection_expectation).is_err());

        let invalid_candidates = json!({
            "items": [{
                "surface": "合成語",
                "expected_outcome": "ambiguous_vocabulary",
                "expected_candidate_ids": [0]
            }]
        });
        assert!(read_plan_items(&invalid_candidates).is_err());

        let invalid_consumer_filename = json!({"items": [{
            "surface": "合/成語",
            "expected_outcome": "acquired"
        }]});
        assert!(read_plan_items(&invalid_consumer_filename).is_err());
    }

    #[test]
    fn same_query_can_have_distinct_selection_request_but_exact_request_is_rejected() {
        let plan = json!({"items": [
            {"surface": "合成語", "expected_outcome": "ambiguous_vocabulary"},
            {
                "surface": "合成語",
                "expected_outcome": "acquired",
                "expected_vocabulary_id": 42,
                "selection": {"vocabulary_id": 42, "detail_url": "https://jpdb.io/vocabulary/42/合成語/ごうせいご"}
            }
        ]});
        assert_eq!(read_plan_items(&plan).unwrap().len(), 2);

        let duplicate = json!({"items": [
            {"surface": "合成語", "expected_outcome": "ambiguous_vocabulary"},
            {"surface": "合成語", "expected_outcome": "vocabulary_not_found"}
        ]});
        assert!(read_plan_items(&duplicate).is_err());
    }

    #[test]
    fn missing_provider_result_is_an_expectation_failure() {
        let item = plan_item(ExpectedOutcome::Acquired);
        let row = missing_result_row(0, &item);
        assert_eq!(row["outcome"], "missing_provider_result");
        assert_eq!(row["expectation_status"], "mismatched");
        assert_eq!(row["item_status"], "provider_result_missing");
    }

    #[test]
    fn startup_failure_leaves_every_item_unstarted_without_rendering() {
        let items = [
            plan_item(ExpectedOutcome::Acquired),
            plan_item(ExpectedOutcome::VocabularyNotFound),
        ];
        let acquisition_report = JpdbPitchAcquisitionReport {
            outcomes: Vec::new(),
            session_failure: Some(JpdbPitchFailure::BrowserSetup {
                stage: asset_store::jpdb::JpdbPitchStage::ConfigureBrowser,
                message: "Chromium не запущен".into(),
            }),
        };
        let (rows, passed) = process_outcomes(
            &items,
            &acquisition_report,
            Path::new("synthetic-report"),
            |_, _, _, _| panic!("необработанный элемент не должен рендериться"),
        );
        assert!(!passed);
        assert_eq!(rows.len(), items.len());
        for row in rows {
            assert_eq!(row["outcome"], "not_started");
            assert_eq!(row["item_status"], "not_started_session_failure");
            assert_eq!(row["expectation_status"], "not_evaluated");
            assert_eq!(row["session_failure_ref"], "#/session_failure");
            assert!(row.get("provider_failure").is_none());
            assert!(row.get("image_path").is_none());
        }
    }

    #[test]
    fn session_stop_keeps_processed_prefix_and_run_level_evidence() {
        let items = [
            plan_item(ExpectedOutcome::VocabularyNotFound),
            plan_item(ExpectedOutcome::Acquired),
        ];
        let failure = JpdbPitchFailure::SessionFailure {
            stage: asset_store::jpdb::JpdbPitchStage::SearchReadiness,
            message: "Монитор CDP остановился".into(),
        };
        let acquisition_report = JpdbPitchAcquisitionReport {
            outcomes: vec![JpdbPitchOutcome::VocabularyNotFound {
                surface: items[0].surface.clone(),
                reading: items[0].reading.clone(),
            }],
            session_failure: Some(failure.clone()),
        };
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("acceptance-fixture").unwrap();

        let report_dir = workspace.path();
        let mut rendered_count = 0;
        let (rows, passed) = process_outcomes(
            &items,
            &acquisition_report,
            report_dir,
            |index, item, outcome, directory| {
                rendered_count += 1;
                Ok(render_outcome(index, item, outcome, directory))
            },
        );
        assert!(!passed);
        assert_eq!(rendered_count, 1);
        assert_eq!(rows[0]["outcome"], "vocabulary_not_found");
        assert_eq!(rows[0]["expectation_status"], "matched");
        assert_eq!(rows[1]["item_status"], "not_started_session_failure");
        save_evidence(
            "verification_failed",
            Path::new("synthetic-plan.json"),
            &items,
            &rows,
            &acquisition_report,
            report_dir,
        )
        .unwrap();
        let evidence: Value =
            serde_json::from_slice(&fs::read(report_dir.join("evidence.json")).unwrap()).unwrap();
        assert_eq!(
            evidence["session_failure"],
            serde_json::to_value(failure).unwrap()
        );
        assert_eq!(evidence["processed_item_count"], 1);
        assert_eq!(evidence["planned_item_count"], 2);
        assert_eq!(evidence["items"][0], rows[0]);
        let html = fs::read_to_string(report_dir.join("index.html")).unwrap();
        assert!(html.contains("Монитор CDP остановился"));
        assert!(html.contains("href=\"#session-failure\""));
    }

    #[test]
    fn session_failure_rejects_run_even_when_every_processed_item_matches() {
        let items = [plan_item(ExpectedOutcome::VocabularyNotFound)];
        let acquisition_report = JpdbPitchAcquisitionReport {
            outcomes: vec![JpdbPitchOutcome::VocabularyNotFound {
                surface: items[0].surface.clone(),
                reading: items[0].reading.clone(),
            }],
            session_failure: Some(JpdbPitchFailure::SessionFailure {
                stage: asset_store::jpdb::JpdbPitchStage::PostCaptureVerification,
                message: "Монитор CDP остановился после последнего элемента".into(),
            }),
        };
        let (rows, passed) = process_outcomes(
            &items,
            &acquisition_report,
            Path::new("synthetic-report"),
            |index, item, outcome, directory| Ok(render_outcome(index, item, outcome, directory)),
        );
        assert!(!passed);
        assert_eq!(rows[0]["expectation_status"], "matched");
    }

    #[test]
    fn saved_typed_failure_is_exported_with_its_exact_text_to_evidence() {
        use asset_store::jpdb::JpdbPitchStage;
        use asset_store::pitch_batch::{
            PitchAccentBatch, PitchAccentBatchRuntime, PitchBatchOutcome,
        };

        let failures = [
            JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: "Строка JPDB не содержит подтверждённые формы и фактическую ссылку".into(),
            },
            JpdbPitchFailure::Telemetry {
                stage: JpdbPitchStage::PitchInspection,
                message: "Критический запрос вернул HTTP 503: https://jpdb.io/search".into(),
            },
            JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::DetailReadiness,
                diagnostic: Some("DOM пока не содержит проверяемых форм и блока значений".into()),
            },
        ];
        for failure in failures {
            let workspace =
                asset_store::temp_workspace::TempWorkspace::create("acceptance-fixture").unwrap();

            let root = workspace.path();
            let item = plan_item(ExpectedOutcome::Acquired);
            let mut batch = PitchAccentBatch::new(
                "typed-failure",
                vec![provider_request(&item)],
                PitchAccentImageValidator::validator_identity(),
            )
            .unwrap();
            let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
            batch = runtime.load().unwrap().unwrap();
            let token = batch.item_token(&item.surface).unwrap();
            assert!(
                runtime
                    .record_outcome(
                        &mut batch,
                        &token,
                        JpdbPitchOutcome::Failed {
                            error: failure.clone()
                        }
                    )
                    .unwrap()
            );
            drop(runtime);
            let mut runtime = PitchAccentBatchRuntime::open(root, "typed-failure").unwrap();
            let reloaded = runtime.load().unwrap().unwrap();
            let PitchBatchOutcome::Failed { error } = &reloaded.items[0].attempts[0].outcome else {
                panic!("Типизированная ошибка должна сохраниться после загрузки пакета");
            };
            let report = JpdbPitchAcquisitionReport {
                outcomes: vec![JpdbPitchOutcome::Failed {
                    error: error.clone(),
                }],
                session_failure: None,
            };
            let directory = root.join("evidence");
            fs::create_dir(&directory).unwrap();
            let items = [item];
            let (rows, passed) = process_outcomes(
                &items,
                &report,
                &directory,
                |index, item, outcome, directory| {
                    Ok(render_outcome(index, item, outcome, directory))
                },
            );
            assert!(!passed);
            save_evidence(
                "verification_failed",
                Path::new("synthetic-plan.json"),
                &items,
                &rows,
                &report,
                &directory,
            )
            .unwrap();
            let evidence: Value =
                serde_json::from_slice(&fs::read(directory.join("evidence.json")).unwrap())
                    .unwrap();
            assert_eq!(
                evidence["items"][0]["failure"],
                serde_json::to_value(failure).unwrap()
            );
            assert!(evidence["items"][0].get("dom").is_none());
            assert!(evidence["items"][0].get("raw_dom").is_none());
            drop(runtime);
        }
    }

    #[test]
    fn an_item_processing_error_keeps_prior_rows_and_final_report_durable() {
        let items = [
            plan_item(ExpectedOutcome::VocabularyNotFound),
            plan_item(ExpectedOutcome::VocabularyNotFound),
        ];
        let outcomes = items
            .iter()
            .map(|item| JpdbPitchOutcome::VocabularyNotFound {
                surface: item.surface.clone(),
                reading: item.reading.clone(),
            })
            .collect::<Vec<_>>();
        let acquisition_report = JpdbPitchAcquisitionReport {
            outcomes,
            session_failure: None,
        };
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("acceptance-fixture").unwrap();

        let report_dir = workspace.path();

        let (rows, passed) = process_outcomes(
            &items,
            &acquisition_report,
            report_dir,
            |index, item, outcome, _| {
                if index == 0 {
                    return Err("ошибка обработки синтетического элемента".into());
                }
                Ok(render_outcome(index, item, outcome, report_dir))
            },
        );
        assert!(!passed);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["item_status"], "item_processing_failed");
        assert_eq!(rows[1]["expectation_status"], "matched");

        save_evidence(
            "verification_failed",
            Path::new("synthetic-plan.json"),
            &items,
            &rows,
            &acquisition_report,
            report_dir,
        )
        .unwrap();
        let evidence: Value =
            serde_json::from_slice(&fs::read(report_dir.join("evidence.json")).unwrap()).unwrap();
        let html = fs::read_to_string(report_dir.join("index.html")).unwrap();
        assert_eq!(evidence["items"].as_array().unwrap().len(), 2);
        assert!(html.contains("ошибка обработки синтетического элемента"));
    }

    #[test]
    fn cli_parser_fixture_uses_a_relative_synthetic_output_path() {
        assert!(Args::try_parse_from(["jpdb_pitch_acceptance"]).is_err());
        let args = Args::try_parse_from([
            "jpdb_pitch_acceptance",
            "--plan",
            "plan.json",
            "--output",
            "report-fixture.zip",
        ])
        .unwrap();
        assert_eq!(args.plan, PathBuf::from("plan.json"));
        assert_eq!(args.output, Some(PathBuf::from("report-fixture.zip")));
    }

    #[test]
    fn zip_outputs_must_be_new_and_outside_checkout() {
        let checkout = checkout_root().unwrap();
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("acceptance-output-fixture")
                .unwrap();
        let output = workspace.path().join("report.zip");
        assert_eq!(
            evidence_zip::resolve_output(
                Some(&output),
                &checkout,
                Path::new("/nonexistent-run"),
                "test"
            )
            .unwrap(),
            output
        );
        fs::write(&output, b"existing").unwrap();
        assert!(
            evidence_zip::resolve_output(
                Some(&output),
                &checkout,
                Path::new("/nonexistent-run"),
                "test"
            )
            .is_err()
        );
        let checkout_output = checkout.join("report.zip");
        assert!(
            evidence_zip::resolve_output(
                Some(&checkout_output),
                &checkout,
                workspace.path(),
                "test"
            )
            .is_err()
        );
        assert!(!checkout_output.exists());
    }

    #[test]
    fn non_acquired_rows_have_no_fake_image_and_duplicate_surface_rows_stay_distinct() {
        let items = [
            plan_item(ExpectedOutcome::AmbiguousVocabulary),
            plan_item(ExpectedOutcome::Acquired),
        ];
        let rows = [
            json!({"item_status": "ambiguous_vocabulary", "outcome": "ambiguous_vocabulary"}),
            json!({"item_status": "candidate_rejected", "outcome": "acquired"}),
        ];
        let workspace =
            asset_store::temp_workspace::TempWorkspace::create("acceptance-fixture").unwrap();

        let report_dir = workspace.path();
        save_html_report("test", &items, &rows, None, report_dir).unwrap();
        let html = fs::read_to_string(report_dir.join("index.html")).unwrap();
        assert!(html.contains("ambiguous_vocabulary"));
        assert!(html.contains("candidate_rejected"));
        assert!(!html.contains("<img"));
        assert!(!report_dir.join("images").exists());
    }

    #[test]
    fn permanent_harness_has_no_live_reference_word_or_dimension_table() {
        let source = include_str!("jpdb_pitch_acceptance.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains("reference_dimensions"));
        assert!(!production.contains("visual_reference"));
    }
}
