use std::io::Cursor;

use crate::domain::AssetDomainPolicy;
use crate::hashing::sha256_hex;
use crate::jpdb::{
    JpdbPitchAbsenceEvidence, JpdbPitchAcquired, JpdbPitchFailure, JpdbPitchOutcome,
    JpdbPitchQuery, JpdbPitchRequest, JpdbPitchSelection, JpdbPitchStage, JpdbVocabularyCandidate,
};
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanAttestation, HumanDecision, LifecycleState,
    Provenance, SemanticStatus, ValidationRecord,
};
use crate::pitch_accent::{
    PitchAccentDomainMetadata, PitchAccentDomainPolicy, PitchAccentImageValidator,
    PitchAccentResolvedForm,
};
use crate::pitch_batch::{
    PitchAccentBatch, PitchAccentBatchRuntime, PitchBatchItemStatus, PitchBatchOwnerSnapshot,
};
use crate::temp_workspace::TempWorkspace;
use crate::validation::SemanticValidator;

fn request(surface: &str, reading: Option<&str>) -> JpdbPitchRequest {
    JpdbPitchRequest::new(JpdbPitchQuery::new(surface, reading.map(str::to_owned)))
}

fn batch(batch_id: &str, surface: &str, reading: Option<&str>) -> PitchAccentBatch {
    PitchAccentBatch::new(
        batch_id,
        vec![request(surface, reading)],
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap()
}

fn assert_retry_selection_matches_targeted_retry(batch: &PitchAccentBatch) {
    batch.validate().unwrap();
    let reason = "Проверка общего критерия повтора";
    let allowed: Vec<_> = batch
        .items
        .iter()
        .filter(|item| {
            let mut targeted = batch.clone();
            targeted.retry(&item.identity.key, reason.into()).is_ok()
        })
        .map(|item| item.identity.key.clone())
        .collect();
    assert_eq!(batch.retryable_failure_surfaces(), allowed);

    let mut targeted = batch.clone();
    for surface in &allowed {
        targeted.retry(surface, reason.into()).unwrap();
    }
    let mut wide = batch.clone();
    assert_eq!(
        wide.retry_retryable_failures(reason.into()).unwrap(),
        allowed
    );
    assert_eq!(
        serde_json::to_value(wide).unwrap(),
        serde_json::to_value(targeted).unwrap(),
        "пакетный повтор должен совпадать с адресными повторами в порядке пакета"
    );
}

struct TemporaryStore {
    workspace: TempWorkspace,
}

impl TemporaryStore {
    fn new() -> Self {
        Self {
            workspace: TempWorkspace::create("asset-store-pitch-batch-tests").unwrap(),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.workspace.path()
    }
}

fn temp_store() -> TemporaryStore {
    TemporaryStore::new()
}

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

fn metadata(surface: &str, reading: &str, vocabulary_id: u64) -> PitchAccentDomainMetadata {
    crate::pitch_accent::test_support::single_variant_metadata(
        surface,
        reading,
        vocabulary_id,
        format!("https://jpdb.io/vocabulary/{vocabulary_id}/{surface}/{reading}#a"),
        browser(),
    )
}

fn png(alternate: bool) -> Vec<u8> {
    crate::pitch_accent::test_support::single_variant_png(alternate)
}

fn acquired(surface: &str, alternate: bool) -> JpdbPitchOutcome {
    acquired_with_metadata(png(alternate), metadata(surface, "ゆうれい", 123))
}

fn acquired_with_metadata(bytes: Vec<u8>, metadata: PitchAccentDomainMetadata) -> JpdbPitchOutcome {
    JpdbPitchOutcome::Acquired {
        asset: Box::new(JpdbPitchAcquired { bytes, metadata }),
    }
}

fn verified_record(surface: &str, alternate: bool) -> AssetRecord {
    let bytes = png(alternate);
    verified_record_with_metadata(surface, bytes, metadata(surface, "ゆうれい", 123))
}

fn verified_record_with_metadata(
    surface: &str,
    bytes: Vec<u8>,
    metadata: PitchAccentDomainMetadata,
) -> AssetRecord {
    let identity = AssetIdentity::new("pitch_accent", surface).unwrap();
    let sha256 = sha256_hex(&bytes);
    let location = PitchAccentDomainPolicy
        .canonical_location(&identity, &sha256, DetectedFormat::Png)
        .unwrap();
    let mut record = AssetRecord {
        identity,
        storage_path: location.storage_path,
        consumer_filename: location.consumer_filename,
        sha256: sha256.clone(),
        byte_length: bytes.len() as u64,
        format: DetectedFormat::Png,
        provenance: Provenance {
            source_kind: "jpdb_browser_render".into(),
            source_name: "jpdb-vocabulary-123.png".into(),
        },
        lifecycle: LifecycleState::Pending,
        validation: None,
        human_attestation: None,
        domain_metadata: Some(serde_json::to_value(metadata).unwrap()),
    };
    let decision = PitchAccentImageValidator
        .validate(&record, &mut Cursor::new(&bytes))
        .unwrap();
    assert_eq!(decision.status, SemanticStatus::Verified);
    record.lifecycle = LifecycleState::Verified;
    record.validation = Some(ValidationRecord {
        status: decision.status,
        validator: PitchAccentImageValidator::validator_identity(),
        content_sha256: sha256,
        evidence: decision.evidence,
    });
    record
}

fn rejected_owner(mut record: AssetRecord) -> AssetRecord {
    record.lifecycle = LifecycleState::Quarantined;
    record.human_attestation = Some(HumanAttestation {
        identity: record.identity.clone(),
        content_sha256: record.sha256.clone(),
        decision: HumanDecision::Reject,
        reason: "Синтетический отказ для точного SHA".into(),
    });
    record
}

fn record_provider_outcome(
    runtime: &mut PitchAccentBatchRuntime,
    batch: &mut PitchAccentBatch,
    surface: &str,
    outcome: JpdbPitchOutcome,
) -> String {
    let token = batch.item_token(surface).unwrap();
    assert!(runtime.record_outcome(batch, &token, outcome).unwrap());
    batch
        .item(surface)
        .unwrap()
        .current_candidate_sha256
        .clone()
        .unwrap_or_default()
}

#[test]
fn incompatible_surface_inputs_are_identity_conflicts_and_exact_duplicates_fold() {
    let first = request("幽霊", Some("ゆうれい"));
    let duplicate = first.clone();
    let collapsed = PitchAccentBatch::new(
        "dupe-fold",
        vec![first.clone(), duplicate],
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(collapsed.items.len(), 1);

    let incompatible = PitchAccentBatch::new(
        "reading-conflict",
        vec![request("幽霊", Some("ゆうれい")), request("幽霊", None)],
        PitchAccentImageValidator::validator_identity(),
    );
    assert!(incompatible.is_err());
}

#[test]
fn legacy_batch_schema_without_exact_attempt_and_plan_identity_fails_closed() {
    let mut current = batch("legacy-batch-schema", "幽霊", Some("ゆうれい"));
    current.schema_version = 1;
    assert!(current.validate().is_err());

    let mut legacy =
        serde_json::to_value(batch("legacy-batch-json", "幽霊", Some("ゆうれい"))).unwrap();
    let root = legacy.as_object_mut().unwrap();
    root.insert("schema_version".into(), serde_json::json!(1));
    root.remove("original_plan");
    root["items"][0]
        .as_object_mut()
        .unwrap()
        .remove("current_candidate_attempt_index");
    assert!(serde_json::from_value::<PitchAccentBatch>(legacy).is_err());
}

#[test]
fn stored_state_of_the_previous_validator_is_refused_before_typed_parsing() {
    let temporary = temp_store();
    let mut batch = batch("previous-validator-state", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(temporary.path(), &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(&mut batch, &token, acquired("幽霊", false))
            .unwrap()
    );
    drop(runtime);
    let state_path = temporary
        .path()
        .join(".runtime/batches/previous-validator-state/state.json");
    let current: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();

    // Состояние схемы 2: валидатор v5 и свидетельства объединения всех графиков.
    let mut previous = current.clone();
    previous["schema_version"] = serde_json::json!(2);
    previous["validator"]["version"] = serde_json::json!("5");
    let evidence =
        &mut previous["items"][0]["attempts"][0]["outcome"]["candidate"]["metadata"]["evidence"];
    let evidence = evidence.as_object_mut().unwrap();
    evidence.remove("variant_selection");
    evidence.insert("graph_count".into(), serde_json::json!(1));
    let render = evidence["render"].as_object_mut().unwrap();
    render.remove("captured_graph_count");
    render.remove("padding_css_px");
    render.insert("graph_union_rect".into(), render["capture_rect"].clone());
    let previous = serde_json::to_vec(&previous).unwrap();

    let mut current_schema_old_validator = current;
    current_schema_old_validator["validator"]["version"] = serde_json::json!("5");
    let current_schema_old_validator = serde_json::to_vec(&current_schema_old_validator).unwrap();

    for stored in [previous, current_schema_old_validator] {
        std::fs::write(&state_path, &stored).unwrap();
        let mut reopened =
            PitchAccentBatchRuntime::open(temporary.path(), "previous-validator-state").unwrap();
        let error = reopened.load().unwrap_err();
        assert_eq!(
            error.code,
            crate::error::ErrorCode::UnsupportedSchemaVersion
        );
        assert_eq!(std::fs::read(&state_path).unwrap(), stored);
    }
}

#[test]
fn transient_retry_is_targeted_and_page_contract_failure_is_not_retryable() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("retry-lifecycle", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();

    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::Timeout {
                        stage: JpdbPitchStage::DetailReadiness,
                        diagnostic: Some("Истёк лимит запроса".into()),
                    },
                },
            )
            .unwrap()
    );
    let timed_out_token = token;
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::TechnicalFailure
    );
    batch
        .retry("幽霊", "Повторить после временного истечения лимита".into())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert_eq!(batch.item("幽霊").unwrap().generation, 1);
    assert_eq!(batch.item("幽霊").unwrap().attempts.len(), 1);
    assert!(
        !runtime
            .record_outcome(
                &mut batch,
                &timed_out_token,
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::Timeout {
                        stage: JpdbPitchStage::Capture,
                        diagnostic: Some("Устаревший результат до повторной попытки".into()),
                    },
                },
            )
            .unwrap()
    );
    assert_eq!(batch.item("幽霊").unwrap().attempts.len(), 1);
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Pending
    );

    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::PageContract {
                        stage: JpdbPitchStage::DetailVerification,
                        message: "Неожиданная страница источника".into(),
                    },
                },
            )
            .unwrap()
    );
    assert!(
        batch
            .retry(
                "幽霊",
                "Нарушение контракта страницы нельзя повторять".into()
            )
            .is_err()
    );

    drop(runtime);
}

#[test]
fn typed_source_failure_text_survives_checkpoint_reload_and_json() {
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
        let temporary = temp_store();
        let root = temporary.path();
        let mut batch = batch("durable-source-failure", "幽霊", Some("ゆうれい"));
        let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
        batch = runtime.load().unwrap().unwrap();
        record_provider_outcome(
            &mut runtime,
            &mut batch,
            "幽霊",
            JpdbPitchOutcome::Failed {
                error: failure.clone(),
            },
        );
        drop(runtime);
        let mut runtime = PitchAccentBatchRuntime::open(root, "durable-source-failure").unwrap();
        let reloaded = runtime.load().unwrap().unwrap();
        let item = reloaded.item("幽霊").unwrap();
        assert_eq!(item.status(), PitchBatchItemStatus::TechnicalFailure);
        assert_eq!(item.attempts.len(), 1);
        assert_eq!(
            item.attempts[0].outcome,
            crate::pitch_batch::PitchBatchOutcome::Failed {
                error: failure.clone()
            }
        );
        let saved_json = serde_json::to_value(&reloaded).unwrap();
        assert_eq!(
            saved_json["items"][0]["attempts"][0]["outcome"]["error"],
            serde_json::to_value(failure).unwrap()
        );
        drop(runtime);
    }
}

#[test]
fn navigation_retry_classifier_requires_error_or_http_context() {
    let navigation = |message: &str| JpdbPitchFailure::Navigation {
        stage: JpdbPitchStage::SearchNavigation,
        message: message.into(),
    };
    assert!(crate::pitch_batch::is_retryable_failure(&navigation(
        "переход вернул HTTP status 503"
    )));
    assert!(crate::pitch_batch::is_retryable_failure(&navigation(
        "временный сбой разрешения адреса: net::ERR_DNS_TIMED_OUT"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "запрос занял 500 ms на https://jpdb.io/vocabulary/500/幽霊/ゆうれい"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "запрос через HTTP/1.1 занял 500 мс"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "статус HTTP/1.1 проверен через 500 мс"
    )));
    assert!(crate::pitch_batch::is_retryable_failure(&navigation(
        "HTTP status code 429"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "HTTP status 404"
    )));
    assert!(!crate::pitch_batch::is_retryable_failure(&navigation(
        "DNS_PROBE_FINISHED_NXDOMAIN"
    )));

    assert!(!crate::pitch_batch::is_retryable_failure(
        &JpdbPitchFailure::InvalidQuery {
            stage: JpdbPitchStage::SearchNavigation,
            message: "Некорректный поисковый запрос".into(),
        }
    ));
    assert!(!crate::pitch_batch::is_retryable_failure(
        &JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: "Неожиданный контракт страницы".into(),
        }
    ));
    assert!(!crate::pitch_batch::is_retryable_failure(
        &JpdbPitchFailure::DetailIdentityMismatch {
            stage: JpdbPitchStage::DetailVerification,
            expected_surface: "幽霊".into(),
            expected_reading: Some("ゆうれい".into()),
            vocabulary_id: Some(123),
            observed_surface_forms: vec!["亡霊".into()],
            observed_readings: vec!["ぼうれい".into()],
        }
    ));
    assert!(!crate::pitch_batch::is_retryable_failure(
        &JpdbPitchFailure::InvalidSelection {
            stage: JpdbPitchStage::SearchResolution,
            message: "Некорректный выбор словарной записи".into(),
        }
    ));
    assert!(!crate::pitch_batch::is_retryable_failure(
        &JpdbPitchFailure::ExplicitSelectionMismatch {
            stage: JpdbPitchStage::DetailVerification,
            vocabulary_id: 123,
            detail_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
            message: "Выбранная словарная запись не соответствует запросу".into(),
        }
    ));
}

#[test]
fn ambiguity_selection_uses_exact_inventory_id_and_route_then_starts_new_generation() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("ambiguity-selection", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let immutable_plan = batch.original_plan.clone();
    let token = batch.item_token("幽霊").unwrap();
    let candidate = JpdbVocabularyCandidate {
        vocabulary_id: 123,
        surface_forms: vec!["幽霊".into()],
        readings: vec!["ゆうれい".into()],
        resolved_forms: vec![PitchAccentResolvedForm {
            surface: "幽霊".into(),
            reading: "ゆうれい".into(),
        }],
        part_of_speech: vec!["noun".into()],
        meanings: vec!["ghost".into()],
        detail_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
    };
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::AmbiguousVocabulary {
                    surface: "幽霊".into(),
                    reading: Some("ゆうれい".into()),
                    candidates: vec![candidate],
                },
            )
            .unwrap()
    );
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::AmbiguousVocabulary
    );
    let ambiguous_item = batch.item("幽霊").unwrap();
    assert!(ambiguous_item.request.selection.is_none());
    assert!(ambiguous_item.current_candidate_sha256.is_none());
    assert!(batch.item_token("幽霊").is_err());
    assert!(
        batch
            .select_candidate("幽霊", 999, "https://jpdb.io/vocabulary/999/幽霊/ゆうれい",)
            .is_err()
    );
    batch
        .select_candidate("幽霊", 123, "https://jpdb.io/vocabulary/123/幽霊/ゆうれい")
        .unwrap();
    assert_eq!(batch.item("幽霊").unwrap().generation, 1);
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert_eq!(
        batch.item("幽霊").unwrap().request.selection,
        Some(
            JpdbPitchSelection::new(123, "https://jpdb.io/vocabulary/123/幽霊/ゆうれい",).unwrap()
        )
    );
    assert_eq!(batch.original_plan, immutable_plan);
    let mut tampered_plan_identity = batch.clone();
    tampered_plan_identity.original_plan.requests[0]
        .query
        .reading = None;
    assert!(tampered_plan_identity.validate().is_err());
    drop(runtime);
}

#[test]
fn no_pitch_is_a_typed_terminal_outcome_without_canonical_cache() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("no-pitch-terminal", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    let evidence = JpdbPitchAbsenceEvidence {
        surface: "幽霊".into(),
        reading: "ゆうれい".into(),
        jpdb_vocabulary_id: 123,
        source_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
        resolved_forms: vec![PitchAccentResolvedForm {
            surface: "幽霊".into(),
            reading: "ゆうれい".into(),
        }],
        section_inventory: vec!["Meanings".into(), "Forms".into()],
        base_page_contract_valid: true,
        pitch_section_present: false,
        pitch_marker_count: 0,
        browser: browser(),
    };
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::NoPitchAccentOnSource { evidence },
            )
            .unwrap()
    );
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::NoPitchAccentOnSource);
    assert!(batch.is_resolved());
    assert!(item.canonical_sha256.is_none());
    assert!(item.published_sha256.is_none());
    drop(runtime);
}

#[test]
fn vocabulary_not_found_is_preserved_as_a_typed_non_acquired_outcome() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("vocabulary-not-found", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::VocabularyNotFound {
                    surface: "幽霊".into(),
                    reading: Some("ゆうれい".into()),
                },
            )
            .unwrap()
    );

    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::VocabularyNotFound);
    assert!(!batch.is_resolved());
    assert!(item.request.selection.is_none());
    assert!(item.current_candidate_sha256.is_none());
    assert!(item.canonical_sha256.is_none());
    std::assert_matches!(
        item.current_outcome(),
        Some(crate::pitch_batch::PitchBatchOutcome::VocabularyNotFound {
            surface,
            reading: Some(reading),
        }) if surface == "幽霊" && reading == "ゆうれい"
    );

    drop(runtime);
}

#[test]
fn absence_evidence_must_match_explicit_vocabulary_id_and_detail_route() {
    let selection_url = "https://jpdb.io/vocabulary/123/幽霊/ゆうれい";
    let selection = JpdbPitchSelection::new(123, selection_url).unwrap();
    let request = JpdbPitchRequest::with_selection(
        JpdbPitchQuery::new("幽霊", Some("ゆうれい".into())),
        selection,
    );
    let evidence = |vocabulary_id: u64, source_url: &str| JpdbPitchAbsenceEvidence {
        surface: "幽霊".into(),
        reading: "ゆうれい".into(),
        jpdb_vocabulary_id: vocabulary_id,
        source_url: source_url.into(),
        resolved_forms: vec![PitchAccentResolvedForm {
            surface: "幽霊".into(),
            reading: "ゆうれい".into(),
        }],
        section_inventory: vec!["Word details".into()],
        base_page_contract_valid: true,
        pitch_section_present: false,
        pitch_marker_count: 0,
        browser: browser(),
    };
    let accepted = |batch_id: &str, evidence| {
        let temporary = temp_store();
        let root = temporary.path();
        let batch = PitchAccentBatch::new(
            batch_id,
            vec![request.clone()],
            PitchAccentImageValidator::validator_identity(),
        )
        .unwrap();
        let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
        let mut batch = runtime.load().unwrap().unwrap();
        let token = batch.item_token("幽霊").unwrap();
        let result = runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::NoPitchAccentOnSource { evidence },
            )
            .is_ok_and(|recorded| recorded);
        drop(runtime);

        result
    };

    assert!(accepted(
        "absence-selection-match",
        evidence(123, selection_url)
    ));
    assert!(!accepted(
        "absence-selection-wrong-id",
        evidence(124, "https://jpdb.io/vocabulary/124/幽霊/ゆうれい")
    ));
    assert!(!accepted(
        "absence-selection-wrong-route",
        evidence(123, "https://jpdb.io/vocabulary/123/幽霊/ゆうれい-variant")
    ));
}

#[test]
fn acquired_png_reopens_verifies_and_rejects_stale_item_token() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("durable-candidate", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    let sha = record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    assert!(
        !runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::Timeout {
                        stage: JpdbPitchStage::Capture,
                        diagnostic: None,
                    },
                },
            )
            .unwrap()
    );
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::AcquiredVerified
    );
    drop(runtime);

    let mut reopened = PitchAccentBatchRuntime::open(root, "durable-candidate").unwrap();
    let batch = reopened.load().unwrap().unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::AcquiredVerified);
    assert!(batch.item_token("幽霊").is_err());
    let candidate = item.candidate(&sha).unwrap();
    let bytes = reopened.read_candidate(&batch, candidate).unwrap();
    assert_eq!(sha256_hex(&bytes), sha);
    assert_eq!(bytes, png(false));
    drop(reopened);
}

#[test]
fn publication_reconcile_recovers_crash_after_owner_publish_before_final_state_save() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("publication-recovery", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let sha = record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    batch.begin_publication("幽霊", &sha, None).unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);

    // Публикация ресурса владельца завершилась, затем процесс упал до обновления state.json.
    let owner_record = verified_record("幽霊", false);
    assert_eq!(owner_record.sha256, sha);
    let snapshot = PitchBatchOwnerSnapshot::from_records(vec![owner_record]).unwrap();
    let mut reopened = PitchAccentBatchRuntime::open(root, "publication-recovery").unwrap();
    let mut recovered = reopened.load().unwrap().unwrap();
    assert_eq!(
        recovered.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::PublicationPending
    );
    recovered.reconcile_owner(&snapshot).unwrap();
    assert_eq!(
        recovered.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    let revision = recovered.revision;
    recovered.reconcile_owner(&snapshot).unwrap();
    assert_eq!(
        recovered.revision, revision,
        "повторный reconcile идемпотентен"
    );
    reopened.save(&recovered).unwrap();
    drop(reopened);

    let mut final_open = PitchAccentBatchRuntime::open(root, "publication-recovery").unwrap();
    let final_state = final_open.load().unwrap().unwrap();
    let item = final_state.item("幽霊").unwrap();
    assert_eq!(item.published_sha256.as_deref(), Some(sha.as_str()));
    assert_eq!(item.canonical_sha256.as_deref(), Some(sha.as_str()));
    assert!(final_state.is_resolved());
    drop(final_open);
}

#[test]
fn same_sha_in_new_generation_publishes_metadata_from_exact_current_attempt() {
    let temporary = temp_store();
    let root = temporary.path();
    let bytes = png(false);
    let metadata_a = metadata("幽霊", "ゆうれい", 123);
    let metadata_b = metadata("幽霊", "ゆうれい", 456);
    let mut batch = batch("same-sha-attempt-identity", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();

    let sha_a = record_provider_outcome(
        &mut runtime,
        &mut batch,
        "幽霊",
        acquired_with_metadata(bytes.clone(), metadata_a.clone()),
    );
    let owner_a = verified_record_with_metadata("幽霊", bytes.clone(), metadata_a.clone());
    batch.begin_publication("幽霊", &sha_a, None).unwrap();
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![owner_a.clone()]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    runtime.save(&batch).unwrap();

    batch
        .reacquire(
            "幽霊",
            "повторно получить те же байты с текущим свидетельством JPDB".into(),
        )
        .unwrap();
    runtime.save(&batch).unwrap();
    let sha_b = record_provider_outcome(
        &mut runtime,
        &mut batch,
        "幽霊",
        acquired_with_metadata(bytes.clone(), metadata_b.clone()),
    );
    assert_eq!(sha_a, sha_b);
    assert_eq!(
        batch.item("幽霊").unwrap().current_candidate_attempt_index,
        Some(2)
    );

    batch
        .begin_publication("幽霊", &sha_b, Some(sha_a.clone()))
        .unwrap();
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![owner_a]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::PublicationPending,
        "старый metadata A с тем же SHA не должен завершить новую публикацию"
    );
    let publication = batch.item("幽霊").unwrap().publication.as_ref().unwrap();
    let current = batch
        .item("幽霊")
        .unwrap()
        .candidate_at(
            publication.candidate_attempt_index,
            &publication.candidate_sha256,
        )
        .unwrap();
    assert_eq!(current.metadata, metadata_b);

    let owner_b = verified_record_with_metadata("幽霊", bytes, metadata_b.clone());
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![owner_b.clone()]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    let publication = batch.item("幽霊").unwrap().publication.as_ref().unwrap();
    let exact_candidate = batch
        .item("幽霊")
        .unwrap()
        .candidate_at(
            publication.candidate_attempt_index,
            &publication.candidate_sha256,
        )
        .unwrap();
    assert_eq!(exact_candidate.metadata, metadata_b);
    batch.validate().unwrap();

    drop(runtime);
}

#[test]
fn owner_sha_drift_after_refresh_blocks_stale_candidate_publication() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("refresh-cas-drift", "幽霊", Some("ゆうれい"));
    let old_owner = rejected_owner(verified_record("幽霊", false));
    let old_sha = old_owner.sha256.clone();
    batch.observe_owner(&old_owner).unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Conflict
    );
    batch
        .reacquire(
            "幽霊",
            "повторно получить ресурс после устаревшего состояния владельца".into(),
        )
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .refresh_expected_sha256
            .as_deref(),
        Some(old_sha.as_str())
    );

    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let sha = record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", true));
    assert_ne!(sha, old_sha);

    let changed_owner = verified_record("幽霊", true);
    let changed_sha = changed_owner.sha256.clone();
    let snapshot = PitchBatchOwnerSnapshot::from_records(vec![changed_owner]).unwrap();
    batch.reconcile_owner(&snapshot).unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(
        item.owner_current_sha256.as_deref(),
        Some(changed_sha.as_str())
    );
    assert_eq!(
        item.refresh_expected_sha256.as_deref(),
        Some(old_sha.as_str())
    );
    assert_eq!(item.status(), PitchBatchItemStatus::Conflict);
    assert!(
        batch
            .begin_publication("幽霊", &sha, Some(old_sha))
            .is_err()
    );

    let adopted_reason = "сверить текущий SHA владельца после внешней замены";
    batch.reacquire("幽霊", adopted_reason.into()).unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .refresh_expected_sha256
            .as_deref(),
        Some(changed_sha.as_str())
    );
    assert_eq!(
        batch.item("幽霊").unwrap().last_action_reason.as_deref(),
        Some(adopted_reason)
    );
    runtime.save(&batch).unwrap();
    let replacement_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    assert_ne!(replacement_sha, changed_sha);
    batch
        .begin_publication("幽霊", &replacement_sha, Some(changed_sha.clone()))
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .publication
            .as_ref()
            .unwrap()
            .expected_previous_sha256
            .as_deref(),
        Some(changed_sha.as_str())
    );
    let replacement_owner = verified_record("幽霊", false);
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![replacement_owner]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    batch.validate().unwrap();

    drop(runtime);
}

#[test]
fn publication_owner_drift_requires_reasoned_reacquire_with_new_cas_baseline() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("publication-cas-drift", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let candidate_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    batch
        .begin_publication("幽霊", &candidate_sha, None)
        .unwrap();

    let external_owner = verified_record("幽霊", true);
    let external_sha = external_owner.sha256.clone();
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![external_owner]).unwrap())
        .unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::Conflict);
    assert_eq!(
        item.owner_conflict
            .as_ref()
            .map(|conflict| conflict.code.as_str()),
        Some("refresh_owner_drift")
    );
    assert_eq!(
        item.publication
            .as_ref()
            .map(|publication| publication.status),
        Some(crate::pitch_batch::PitchBatchPublicationStatus::Conflict)
    );

    let reason = "принять новый SHA владельца после параллельной публикации";
    batch.reacquire("幽霊", reason.into()).unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .refresh_expected_sha256
            .as_deref(),
        Some(external_sha.as_str())
    );
    runtime.save(&batch).unwrap();
    let reacquired_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    assert_eq!(reacquired_sha, candidate_sha);
    batch
        .begin_publication("幽霊", &reacquired_sha, Some(external_sha.clone()))
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .publication
            .as_ref()
            .unwrap()
            .candidate_attempt_index,
        2
    );
    let published_owner = verified_record("幽霊", false);
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![published_owner]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    assert_eq!(
        batch.item("幽霊").unwrap().last_action_reason.as_deref(),
        Some(reason)
    );
    batch.validate().unwrap();

    drop(runtime);
}

#[test]
fn published_exact_sha_rejection_quarantines_and_reacquires_with_observed_cas() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("reject-published", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let published_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", false));
    batch
        .begin_publication("幽霊", &published_sha, None)
        .unwrap();
    let verified = verified_record("幽霊", false);
    let snapshot = PitchBatchOwnerSnapshot::from_records(vec![verified]).unwrap();
    batch.reconcile_owner(&snapshot).unwrap();
    runtime.save(&batch).unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );

    batch
        .reject_candidate(
            "幽霊",
            &published_sha,
            "изображение визуально не совпадает".into(),
        )
        .unwrap();
    let rejected_item = batch.item("幽霊").unwrap();
    assert_eq!(rejected_item.status(), PitchBatchItemStatus::Conflict);
    assert!(rejected_item.publication.is_none());
    assert_eq!(rejected_item.publication_history.len(), 1);
    assert_eq!(
        rejected_item.published_sha256.as_deref(),
        Some(published_sha.as_str())
    );
    batch.validate().unwrap();

    let quarantined = rejected_owner(verified_record("幽霊", false));
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![quarantined]).unwrap())
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .owner_conflict
            .as_ref()
            .unwrap()
            .code,
        "owner_rejected"
    );
    batch
        .reacquire(
            "幽霊",
            "заменить точный SHA ранее отклонённого кандидата".into(),
        )
        .unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::Pending);
    assert_eq!(
        item.refresh_expected_sha256.as_deref(),
        Some(published_sha.as_str())
    );
    assert_eq!(
        item.published_sha256.as_deref(),
        Some(published_sha.as_str())
    );

    runtime.save(&batch).unwrap();
    let token = batch.item_token("幽霊").unwrap();
    let replacement_sha =
        record_provider_outcome(&mut runtime, &mut batch, "幽霊", acquired("幽霊", true));
    assert_ne!(replacement_sha, published_sha);
    batch
        .begin_publication("幽霊", &replacement_sha, Some(published_sha.clone()))
        .unwrap();
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .publication
            .as_ref()
            .unwrap()
            .expected_previous_sha256,
        Some(published_sha.clone())
    );
    assert_eq!(batch.item("幽霊").unwrap().publication_history.len(), 1);
    assert_eq!(
        batch.item("幽霊").unwrap().published_sha256.as_deref(),
        Some(published_sha.as_str())
    );
    assert_eq!(
        batch.item("幽霊").unwrap().item_revision,
        token.item_revision + 2
    );
    batch.validate().unwrap();

    drop(runtime);
}

#[test]
fn current_verified_owner_is_reused_without_acquisition() {
    let record = verified_record("幽霊", false);
    let mut batch = batch("fast-reuse", "幽霊", Some("ゆうれい"));
    batch
        .reconcile_owner(&PitchBatchOwnerSnapshot::from_records(vec![record.clone()]).unwrap())
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::ExistingVerified
    );
    assert_eq!(
        batch.item("幽霊").unwrap().canonical_sha256.as_deref(),
        Some(record.sha256.as_str())
    );
    assert!(batch.item_token("幽霊").is_err());
}

/// Пакетный перевод в новое поколение обязан выбрать ровно текущие устранимые
/// технические сбои и не тронуть ни один другой элемент.
#[test]
fn batch_wide_retry_selects_only_current_retryable_technical_failures() {
    let temporary = temp_store();
    let root = temporary.path();
    let surfaces = ["一", "二", "三", "四", "五", "六"];
    let requests = surfaces
        .iter()
        .map(|surface| request(surface, Some("よみ")))
        .collect();
    let mut batch = PitchAccentBatch::new(
        "batch-wide-retry",
        requests,
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();

    let mut record = |batch: &mut PitchAccentBatch, surface: &str, outcome: JpdbPitchOutcome| {
        let cas = batch.item_token(surface).unwrap();
        assert!(runtime.record_outcome(batch, &cas, outcome).unwrap());
    };
    // Устранимый технический сбой: единственный кандидат на пакетный повтор.
    record(
        &mut batch,
        "一",
        JpdbPitchOutcome::Failed {
            error: JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::DetailReadiness,
                diagnostic: Some("Истёк лимит запроса".into()),
            },
        },
    );
    // Неустранимый технический сбой: повтор не поможет и остаётся как есть.
    record(
        &mut batch,
        "二",
        JpdbPitchOutcome::Failed {
            error: JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::DetailReadiness,
                message: "Страница не соответствует контракту".into(),
            },
        },
    );
    record(
        &mut batch,
        "三",
        JpdbPitchOutcome::NoPitchAccentOnSource {
            evidence: JpdbPitchAbsenceEvidence {
                surface: "三".into(),
                reading: "よみ".into(),
                jpdb_vocabulary_id: 123,
                source_url: "https://jpdb.io/vocabulary/123/三/よみ".into(),
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: "三".into(),
                    reading: "よみ".into(),
                }],
                section_inventory: vec!["Meanings".into(), "Forms".into()],
                base_page_contract_valid: true,
                pitch_section_present: false,
                pitch_marker_count: 0,
                browser: browser(),
            },
        },
    );
    record(
        &mut batch,
        "四",
        JpdbPitchOutcome::VocabularyNotFound {
            surface: "四".into(),
            reading: Some("よみ".into()),
        },
    );
    record(
        &mut batch,
        "五",
        JpdbPitchOutcome::AmbiguousVocabulary {
            surface: "五".into(),
            reading: Some("よみ".into()),
            candidates: vec![JpdbVocabularyCandidate {
                vocabulary_id: 123,
                surface_forms: vec!["五".into()],
                readings: vec!["よみ".into()],
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: "五".into(),
                    reading: "よみ".into(),
                }],
                part_of_speech: vec!["noun".into()],
                meanings: vec!["five".into()],
                detail_url: "https://jpdb.io/vocabulary/123/五/よみ".into(),
            }],
        },
    );
    // «六» остаётся нетронутым ожидающим элементом.

    assert_eq!(
        batch.item("一").unwrap().status(),
        PitchBatchItemStatus::TechnicalFailure
    );
    assert_eq!(
        batch.item("二").unwrap().status(),
        PitchBatchItemStatus::TechnicalFailure
    );
    assert_eq!(
        batch.retryable_failure_surfaces(),
        vec!["一".to_owned()],
        "пакетный выбор обязан совпадать с классификатором устранимости"
    );

    assert_retry_selection_matches_targeted_retry(&batch);

    let before = batch.clone();
    let selected = batch
        .retry_retryable_failures("Пакетный повтор временных сбоев".into())
        .unwrap();
    assert_eq!(selected, vec!["一".to_owned()]);
    assert_eq!(
        batch.item("一").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert_eq!(batch.item("一").unwrap().generation, 1);
    assert_eq!(batch.item("一").unwrap().attempts.len(), 1);
    // Все прочие элементы остаются байт-в-байт прежними.
    for surface in ["二", "三", "四", "五", "六"] {
        assert_eq!(
            serde_json::to_value(batch.item(surface).unwrap()).unwrap(),
            serde_json::to_value(before.item(surface).unwrap()).unwrap(),
            "элемент {surface} не должен меняться"
        );
    }
    // Повтор операции идемпотентен: уже переведённые элементы не выбираются.
    let after = batch.clone();
    assert!(batch.retryable_failure_surfaces().is_empty());
    assert!(
        batch
            .retry_retryable_failures("Повтор без выбранных элементов".into())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        serde_json::to_value(&batch).unwrap(),
        serde_json::to_value(&after).unwrap(),
        "пустой выбор не меняет сохранённое состояние"
    );
}

#[test]
fn batch_wide_retry_rejects_invalid_reason_without_changes() {
    let temporary = temp_store();
    let root = temporary.path();
    let mut batch = batch("batch-wide-atomic", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    let cas = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &cas,
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::Timeout {
                        stage: JpdbPitchStage::DetailReadiness,
                        diagnostic: Some("Истёк лимит запроса".into()),
                    },
                },
            )
            .unwrap()
    );
    let before = serde_json::to_value(&batch).unwrap();
    // Отказ валидации причины обязан оставить пакет без единого изменения.
    assert!(batch.retry_retryable_failures("  ".into()).is_err());
    assert_eq!(serde_json::to_value(&batch).unwrap(), before);
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::TechnicalFailure
    );
}

#[test]
fn batch_wide_retry_rolls_back_after_first_item_changes_and_second_overflows() {
    let temporary = temp_store();
    let mut batch = PitchAccentBatch::new(
        "batch-wide-overflow",
        vec![request("一", None), request("二", None)],
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(temporary.path(), &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    for surface in ["一", "二"] {
        record_provider_outcome(
            &mut runtime,
            &mut batch,
            surface,
            JpdbPitchOutcome::Failed {
                error: JpdbPitchFailure::Timeout {
                    stage: JpdbPitchStage::DetailReadiness,
                    diagnostic: None,
                },
            },
        );
    }
    batch.item_mut("二").unwrap().item_revision = u64::MAX;
    // Граница счётчика допустима в сохранённом состоянии, а следующий повтор — нет.
    batch.validate().unwrap();
    assert_eq!(batch.retryable_failure_surfaces(), vec!["一", "二"]);
    runtime.save(&batch).unwrap();
    assert_eq!(runtime.load().unwrap().unwrap(), batch);

    let reason = "Повтор с проверкой атомарности";
    let mut partial = batch.clone();
    partial.retry("一", reason.into()).unwrap();
    assert_eq!(partial.item("一").unwrap().generation, 1);
    assert_eq!(
        partial.item("一").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert_ne!(partial, batch);
    let error = partial.retry("二", reason.into()).unwrap_err();
    assert_eq!(error.message, "превышен номер изменения элемента");
    assert_eq!(partial.item("二").unwrap().generation, 1);

    let before = serde_json::to_vec(&batch).unwrap();
    let error = batch.retry_retryable_failures(reason.into()).unwrap_err();
    assert_eq!(error.message, "превышен номер изменения элемента");
    assert_eq!(serde_json::to_vec(&batch).unwrap(), before);
    assert_eq!(runtime.load().unwrap().unwrap(), batch);
}

#[test]
fn batch_wide_retry_and_runtime_load_reject_corrupt_attempt_history() {
    let temporary = temp_store();
    let mut batch = batch("batch-wide-invalid-history", "幽霊", Some("ゆうれい"));
    let mut runtime = PitchAccentBatchRuntime::create(temporary.path(), &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    record_provider_outcome(
        &mut runtime,
        &mut batch,
        "幽霊",
        JpdbPitchOutcome::Failed {
            error: JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::DetailReadiness,
                diagnostic: None,
            },
        },
    );
    batch.item_mut("幽霊").unwrap().attempts[0].index = 0;
    assert!(batch.validate().is_err());
    assert_eq!(batch.retryable_failure_surfaces(), vec!["幽霊"]);
    let before = serde_json::to_vec(&batch).unwrap();
    assert!(
        batch
            .retry_retryable_failures("Повтор повреждённой истории".into())
            .is_err()
    );
    assert_eq!(serde_json::to_vec(&batch).unwrap(), before);
    assert!(runtime.save(&batch).is_err());
    drop(runtime);

    let state_path = temporary
        .path()
        .join(".runtime/batches/batch-wide-invalid-history/state.json");
    std::fs::write(&state_path, &before).unwrap();
    let mut reopened = PitchAccentBatchRuntime::open(temporary.path(), &batch.batch_id).unwrap();
    assert!(reopened.load().is_err());
    assert_eq!(std::fs::read(state_path).unwrap(), before);
}

/// Пакетный повтор не трогает ни один элемент, который не является текущим
/// устранимым техническим сбоем, включая опубликованные и конфликтные.
#[test]
fn batch_wide_retry_leaves_published_conflict_and_pending_items_untouched() {
    let temporary = temp_store();
    let root = temporary.path();
    let surfaces = ["一", "二", "三", "四", "五", "六", "七", "八", "九"];
    let requests = surfaces
        .iter()
        .map(|surface| request(surface, Some("ゆうれい")))
        .collect();
    let mut batch = PitchAccentBatch::new(
        "batch-wide-statuses",
        requests,
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(root, &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();

    macro_rules! record {
        ($surface:expr, $outcome:expr) => {{
            let cas = batch.item_token($surface).unwrap();
            assert!(runtime.record_outcome(&mut batch, &cas, $outcome).unwrap());
        }};
    }

    // «一» — устранимый технический сбой: единственный кандидат на пакетный повтор.
    record!(
        "一",
        JpdbPitchOutcome::Failed {
            error: JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::DetailReadiness,
                diagnostic: Some("Истёк лимит запроса".into()),
            },
        }
    );
    // «二» — неустранимый технический сбой.
    record!(
        "二",
        JpdbPitchOutcome::Failed {
            error: JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::DetailReadiness,
                message: "Страница не соответствует контракту".into(),
            },
        }
    );
    // «三» — опубликованный элемент.
    let published_sha =
        record_provider_outcome(&mut runtime, &mut batch, "三", acquired("三", false));
    batch.begin_publication("三", &published_sha, None).unwrap();
    // «四» — конфликт с текущим состоянием владельца.
    record!(
        "四",
        JpdbPitchOutcome::Failed {
            error: JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::DetailReadiness,
                diagnostic: None,
            },
        }
    );
    let conflict_owner = rejected_owner(verified_record("四", false));
    // «五» — незавершённое намерение публикации.
    let pending_sha =
        record_provider_outcome(&mut runtime, &mut batch, "五", acquired("五", false));
    batch.begin_publication("五", &pending_sha, None).unwrap();
    batch
        .reconcile_owner(
            &PitchBatchOwnerSnapshot::from_records(vec![
                verified_record("三", false),
                conflict_owner.clone(),
            ])
            .unwrap(),
        )
        .unwrap();
    record!(
        "六",
        JpdbPitchOutcome::NoPitchAccentOnSource {
            evidence: JpdbPitchAbsenceEvidence {
                surface: "六".into(),
                reading: "ゆうれい".into(),
                jpdb_vocabulary_id: 123,
                source_url: "https://jpdb.io/vocabulary/123/六/ゆうれい".into(),
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: "六".into(),
                    reading: "ゆうれい".into(),
                }],
                section_inventory: vec!["Meanings".into(), "Forms".into()],
                base_page_contract_valid: true,
                pitch_section_present: false,
                pitch_marker_count: 0,
                browser: browser(),
            },
        }
    );
    record!(
        "七",
        JpdbPitchOutcome::AmbiguousVocabulary {
            surface: "七".into(),
            reading: Some("ゆうれい".into()),
            candidates: vec![JpdbVocabularyCandidate {
                vocabulary_id: 123,
                surface_forms: vec!["七".into()],
                readings: vec!["ゆうれい".into()],
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: "七".into(),
                    reading: "ゆうれい".into(),
                }],
                part_of_speech: vec!["noun".into()],
                meanings: vec!["seven".into()],
                detail_url: "https://jpdb.io/vocabulary/123/七/ゆうれい".into(),
            }],
        }
    );
    record!(
        "八",
        JpdbPitchOutcome::VocabularyNotFound {
            surface: "八".into(),
            reading: Some("ゆうれい".into()),
        }
    );
    // «九» остаётся ожидающим элементом.

    assert_eq!(
        batch.item("三").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    assert_eq!(
        batch.item("四").unwrap().status(),
        PitchBatchItemStatus::Conflict
    );
    assert_eq!(
        batch.item("五").unwrap().status(),
        PitchBatchItemStatus::PublicationPending
    );
    assert_eq!(
        batch.item("六").unwrap().status(),
        PitchBatchItemStatus::NoPitchAccentOnSource
    );
    assert_eq!(
        batch.item("七").unwrap().status(),
        PitchBatchItemStatus::AmbiguousVocabulary
    );
    assert_eq!(
        batch.item("八").unwrap().status(),
        PitchBatchItemStatus::VocabularyNotFound
    );
    assert_eq!(
        batch.item("九").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert_eq!(batch.retryable_failure_surfaces(), vec!["一".to_owned()]);

    assert_retry_selection_matches_targeted_retry(&batch);

    let before = batch.clone();
    let selected = batch
        .retry_retryable_failures("Пакетный повтор временных сбоев".into())
        .unwrap();
    assert_eq!(selected, vec!["一".to_owned()]);
    for surface in ["二", "三", "四", "五", "六", "七", "八", "九"] {
        assert_eq!(
            serde_json::to_value(batch.item(surface).unwrap()).unwrap(),
            serde_json::to_value(before.item(surface).unwrap()).unwrap(),
            "элемент {surface} не должен меняться"
        );
    }
    // Конфликт владельца и опубликованный SHA остаются в силе после пакетного повтора.
    assert_eq!(
        batch.item("四").unwrap().owner_current_sha256.as_deref(),
        Some(conflict_owner.sha256.as_str())
    );
    assert_eq!(
        batch.item("三").unwrap().published_sha256.as_deref(),
        Some(published_sha.as_str())
    );
}

#[test]
fn batch_wide_retry_matches_targeted_retry_after_owner_reconciliation() {
    let temporary = temp_store();
    let requests = ["一", "二", "三"]
        .iter()
        .map(|surface| request(surface, Some("ゆうれい")))
        .collect();
    let mut batch = PitchAccentBatch::new(
        "retry-reconciled-owner",
        requests,
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(temporary.path(), &batch).unwrap();
    batch = runtime.load().unwrap().unwrap();
    for surface in ["一", "二", "三"] {
        let token = batch.item_token(surface).unwrap();
        assert!(
            runtime
                .record_outcome(
                    &mut batch,
                    &token,
                    JpdbPitchOutcome::Failed {
                        error: JpdbPitchFailure::Timeout {
                            stage: JpdbPitchStage::DetailReadiness,
                            diagnostic: None,
                        },
                    },
                )
                .unwrap()
        );
    }

    let owner = verified_record("二", false);
    batch
        .reconcile_owner(
            &PitchBatchOwnerSnapshot::from_records(vec![
                owner.clone(),
                rejected_owner(verified_record("三", false)),
            ])
            .unwrap(),
        )
        .unwrap();
    let reconciled = batch.item("二").unwrap();
    assert_eq!(reconciled.status(), PitchBatchItemStatus::ExistingVerified);
    std::assert_matches!(
        reconciled.current_outcome(),
        Some(crate::pitch_batch::PitchBatchOutcome::Failed {
            error: JpdbPitchFailure::Timeout { .. }
        })
    );
    assert!(reconciled.owner_conflict.is_none());
    assert_eq!(
        batch.item("三").unwrap().status(),
        PitchBatchItemStatus::Conflict
    );
    assert_eq!(
        batch.retryable_failure_surfaces(),
        vec!["一".to_owned(), "二".to_owned()],
        "VERIFIED владельца не отменяет явный повтор сохранённого сбоя"
    );
    assert_retry_selection_matches_targeted_retry(&batch);

    let previous_generation = reconciled.generation;
    let before = batch.clone();
    assert_eq!(
        batch
            .retry_retryable_failures("Повтор после сверки владельца".into())
            .unwrap(),
        vec!["一".to_owned(), "二".to_owned()]
    );
    let retried = batch.item("二").unwrap();
    assert_eq!(retried.status(), PitchBatchItemStatus::Pending);
    assert_eq!(retried.generation, previous_generation + 1);
    assert_eq!(retried.attempts.len(), 1);
    assert_eq!(
        retried.owner_current_sha256.as_deref(),
        Some(owner.sha256.as_str())
    );
    assert_eq!(
        retried.refresh_expected_sha256.as_deref(),
        Some(owner.sha256.as_str())
    );
    assert_eq!(
        serde_json::to_value(batch.item("三").unwrap()).unwrap(),
        serde_json::to_value(before.item("三").unwrap()).unwrap()
    );
}
