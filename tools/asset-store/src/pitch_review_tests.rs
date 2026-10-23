use crate::batch_runtime::RuntimeBlobRef;
use crate::browser_runtime::{BrowserExecutableSource, BrowserRuntimeProvenance};
use crate::domain::AssetDomainPolicy;
use crate::hashing::sha256_hex;
use crate::jpdb::{
    JpdbPitchAbsenceEvidence, JpdbPitchFailure, JpdbPitchQuery, JpdbPitchRequest, JpdbPitchStage,
    JpdbVocabularyCandidate,
};
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, LifecycleState, Provenance, SemanticStatus,
    ValidationEvidence, ValidationRecord, ValidatorIdentity,
};
use crate::pitch_accent::{
    PitchAccentDomainMetadata, PitchAccentDomainPolicy, PitchAccentImageValidator,
    PitchAccentResolvedForm,
};
use crate::pitch_batch::{
    PitchAccentBatch, PitchBatchAttempt, PitchBatchConflict, PitchBatchItem, PitchBatchOutcome,
    PitchBatchPublication, PitchBatchPublicationStatus,
};

fn validator() -> ValidatorIdentity {
    PitchAccentImageValidator::validator_identity()
}

fn browser() -> BrowserRuntimeProvenance {
    BrowserRuntimeProvenance {
        product: "Chrome/140".into(),
        protocol_version: "1.3".into(),
        revision: "1234567".into(),
        user_agent: "Mozilla/5.0 synthetic".into(),
        js_version: "V8 14.0".into(),
        executable_source: BrowserExecutableSource::PathLookup,
    }
}

fn metadata(surface: &str) -> PitchAccentDomainMetadata {
    crate::pitch_accent::test_support::single_variant_metadata(
        surface,
        "ゆうれい",
        123,
        "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
        browser(),
    )
}

fn png() -> Vec<u8> {
    crate::pitch_accent::test_support::single_variant_png(false)
}

fn candidate(
    surface: &str,
    bytes: &[u8],
    status: SemanticStatus,
) -> crate::pitch_batch::PitchBatchCandidate {
    let hash = sha256_hex(bytes);
    crate::pitch_batch::PitchBatchCandidate {
        blob: RuntimeBlobRef {
            sha256: hash.clone(),
            storage_path: format!("candidates/{hash}.png"),
        },
        sha256: hash.clone(),
        byte_length: bytes.len() as u64,
        metadata: metadata(surface),
        validation: ValidationRecord {
            status,
            validator: validator(),
            content_sha256: hash.clone(),
            evidence: vec![ValidationEvidence {
                kind: "synthetic_render_validation".into(),
                summary: "deterministic fixture".into(),
                details: Some(serde_json::json!({"pixel_width": 6, "pixel_height": 6})),
            }],
        },
        validation_context_sha256: hash,
    }
}

fn base_batch(surfaces: &[&str]) -> PitchAccentBatch {
    PitchAccentBatch::new(
        "synthetic-pitch-review",
        surfaces
            .iter()
            .map(|surface| JpdbPitchRequest::new(JpdbPitchQuery::new(*surface, None)))
            .collect(),
        validator(),
    )
    .unwrap()
}

fn item_mut<'a>(batch: &'a mut PitchAccentBatch, surface: &str) -> &'a mut PitchBatchItem {
    batch
        .items
        .iter_mut()
        .find(|item| item.identity.key == surface)
        .unwrap()
}

fn add_outcome(item: &mut PitchBatchItem, outcome: PitchBatchOutcome) {
    if matches!(&outcome, PitchBatchOutcome::Acquired { candidate }
        if item.current_candidate_sha256.as_deref() == Some(candidate.sha256.as_str()))
    {
        item.current_candidate_attempt_index = Some(item.attempts.len() as u32 + 1);
    }
    item.attempts.push(PitchBatchAttempt {
        index: item.attempts.len() as u32 + 1,
        generation: item.generation,
        request: item.request.clone(),
        outcome,
    });
}

fn absence(surface: &str) -> JpdbPitchAbsenceEvidence {
    JpdbPitchAbsenceEvidence {
        surface: surface.into(),
        reading: "ゆうれい".into(),
        jpdb_vocabulary_id: 123,
        source_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
        resolved_forms: vec![PitchAccentResolvedForm {
            surface: surface.into(),
            reading: "ゆうれい".into(),
        }],
        section_inventory: vec!["Definitions".into(), "Examples".into()],
        base_page_contract_valid: true,
        pitch_section_present: false,
        pitch_marker_count: 0,
        browser: browser(),
    }
}

fn owner_record(
    surface: &str,
    bytes: &[u8],
    candidate: &crate::pitch_batch::PitchBatchCandidate,
) -> AssetRecord {
    let identity = AssetIdentity::new("pitch_accent", surface).unwrap();
    let location = PitchAccentDomainPolicy
        .canonical_location(&identity, &candidate.sha256, DetectedFormat::Png)
        .unwrap();
    AssetRecord {
        identity,
        storage_path: location.storage_path,
        consumer_filename: location.consumer_filename,
        sha256: candidate.sha256.clone(),
        byte_length: bytes.len() as u64,
        format: DetectedFormat::Png,
        provenance: Provenance {
            source_kind: "jpdb_browser".into(),
            source_name: format!("{surface}.png"),
        },
        lifecycle: LifecycleState::Verified,
        validation: Some(candidate.validation.clone()),
        human_attestation: None,
        domain_metadata: Some(serde_json::to_value(&candidate.metadata).unwrap()),
    }
}

#[test]
fn review_renders_every_typed_outcome_and_all_batch_statuses() {
    let bytes = png();
    let mut batch = base_batch(&[
        "текущий",
        "pending",
        "canonical",
        "rejected",
        "absent",
        "ambiguous",
        "not-found",
        "failed",
        "pending-publication",
        "published",
        "conflict",
    ]);
    let current = candidate("текущий", &bytes, SemanticStatus::Verified);
    let current_sha = current.sha256.clone();
    {
        let item = item_mut(&mut batch, "текущий");
        item.current_candidate_sha256 = Some(current_sha.clone());
        add_outcome(
            item,
            PitchBatchOutcome::Acquired {
                candidate: Box::new(current),
            },
        );
    }
    let canonical = candidate("canonical", &bytes, SemanticStatus::Verified);
    let canonical_sha = canonical.sha256.clone();
    {
        let item = item_mut(&mut batch, "canonical");
        item.canonical_sha256 = Some(canonical_sha.clone());
        item.owner_current_sha256 = Some(canonical_sha.clone());
        item.existing_verified_sha256 = Some(canonical_sha.clone());
    }
    let rejected = candidate("rejected", &bytes, SemanticStatus::Rejected);
    let rejected_sha = rejected.sha256.clone();
    {
        let item = item_mut(&mut batch, "rejected");
        item.current_candidate_sha256 = Some(rejected_sha.clone());
        add_outcome(
            item,
            PitchBatchOutcome::Acquired {
                candidate: Box::new(rejected),
            },
        );
    }
    add_outcome(
        item_mut(&mut batch, "absent"),
        PitchBatchOutcome::NoPitchAccentOnSource {
            evidence: absence("absent"),
        },
    );
    add_outcome(
        item_mut(&mut batch, "ambiguous"),
        PitchBatchOutcome::AmbiguousVocabulary {
            surface: "ambiguous".into(),
            reading: None,
            candidates: vec![JpdbVocabularyCandidate {
                vocabulary_id: 321,
                surface_forms: vec!["ambiguous".into()],
                readings: vec!["あんびぎゅあす".into()],
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: "ambiguous".into(),
                    reading: "あんびぎゅあす".into(),
                }],
                part_of_speech: vec!["noun".into()],
                meanings: vec!["unclear".into()],
                detail_url: "https://jpdb.io/vocabulary/321/ambiguous/あんびぎゅあす".into(),
            }],
        },
    );
    add_outcome(
        item_mut(&mut batch, "not-found"),
        PitchBatchOutcome::VocabularyNotFound {
            surface: "not-found".into(),
            reading: None,
        },
    );
    add_outcome(
        item_mut(&mut batch, "failed"),
        PitchBatchOutcome::Failed {
            error: JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: "synthetic technical failure".into(),
            },
        },
    );
    for surface in ["pending-publication", "published", "conflict"] {
        let candidate = candidate(surface, &bytes, SemanticStatus::Verified);
        let hash = candidate.sha256.clone();
        let item = item_mut(&mut batch, surface);
        item.current_candidate_sha256 = Some(hash.clone());
        add_outcome(
            item,
            PitchBatchOutcome::Acquired {
                candidate: Box::new(candidate),
            },
        );
        item.publication = Some(PitchBatchPublication {
            candidate_sha256: hash.clone(),
            candidate_attempt_index: item.current_candidate_attempt_index.unwrap(),
            expected_previous_sha256: None,
            status: match surface {
                "pending-publication" => PitchBatchPublicationStatus::Pending,
                "published" => PitchBatchPublicationStatus::Published,
                _ => PitchBatchPublicationStatus::Conflict,
            },
            conflict_code: (surface == "conflict").then(|| "identity_conflict".into()),
            conflict_message: (surface == "conflict").then(|| "synthetic conflict".into()),
        });
        if surface == "published" || surface == "conflict" {
            item.canonical_sha256 = Some(hash.clone());
        }
        if surface == "published" {
            item.owner_current_sha256 = Some(item.current_candidate_sha256.clone().unwrap());
            item.published_sha256 = item.current_candidate_sha256.clone();
        }
        if surface == "conflict" {
            item.owner_current_sha256 = Some(hash);
            item.owner_conflict = Some(PitchBatchConflict {
                code: "identity_conflict".into(),
                message: "synthetic conflict".into(),
            });
        }
    }

    let canonical_candidate = candidate("canonical", &bytes, SemanticStatus::Verified);
    assert_eq!(canonical_candidate.sha256, canonical_sha);
    let owner_records = vec![
        owner_record("canonical", &bytes, &canonical_candidate),
        owner_record(
            "published",
            &bytes,
            &candidate("published", &bytes, SemanticStatus::Verified),
        ),
        owner_record(
            "conflict",
            &bytes,
            &candidate("conflict", &bytes, SemanticStatus::Verified),
        ),
    ];
    let html = super::render(&batch, &owner_records, |_| Ok(bytes.clone())).unwrap();

    for status in [
        "pending",
        "acquired_verified",
        "candidate_rejected",
        "no_pitch_accent_on_source",
        "ambiguous_vocabulary",
        "vocabulary_not_found",
        "technical_failure",
        "publication_pending",
        "published",
        "existing_verified",
        "conflict",
    ] {
        assert!(html.contains(status), "missing status {status}");
    }
    for evidence_label in [
        "Данные источника",
        "Данные отрисовки",
        "Данные браузера",
        "Свидетельства отсутствия",
        "Техническая диагностика",
        "Фактический адрес страницы",
        "Части речи",
        "Значения",
        "Канонический PNG",
        "вариантов графика у записи: 1",
        "основание выбора: воспроизводимый хеш",
        "графиков в PNG: 1",
        "статус проверки",
    ] {
        assert!(html.contains(evidence_label), "missing {evidence_label}");
    }
    assert!(html.contains(&format!("src=\"candidates/{current_sha}.png\"")));
    assert!(html.contains("src=\"../../../assets/png/canonical.png\""));
    assert!(html.contains("Content-Security-Policy"));
    assert!(html.contains("default-src 'none'; img-src 'self' file:"));
    assert!(!html.contains("<button"));
    assert!(!html.contains("<form"));
    assert!(!html.contains("<input"));
    assert!(!html.contains("<select"));
    assert!(!html.contains("<textarea"));
    assert!(!html.contains("<script"));
}

#[test]
fn review_marks_only_exact_current_attempt_when_candidate_sha_is_reused() {
    let bytes = png();
    let mut first = candidate("幽霊", &bytes, SemanticStatus::Verified);
    first.metadata.reading = "古い読み".into();
    first.metadata.evidence.source_url = "https://jpdb.io/old-attempt".into();
    let sha = first.sha256.clone();
    let mut second = candidate("幽霊", &bytes, SemanticStatus::Verified);
    second.metadata.reading = "新しい読み".into();
    second.metadata.evidence.source_url = "https://jpdb.io/current-attempt".into();
    assert_eq!(second.sha256, sha);

    let mut batch = base_batch(&["幽霊"]);
    batch.items[0].current_candidate_sha256 = Some(sha);
    add_outcome(
        &mut batch.items[0],
        PitchBatchOutcome::Acquired {
            candidate: Box::new(first),
        },
    );
    add_outcome(
        &mut batch.items[0],
        PitchBatchOutcome::Acquired {
            candidate: Box::new(second),
        },
    );
    assert_eq!(batch.items[0].current_candidate_attempt_index, Some(2));
    assert_eq!(
        batch.items[0].current_candidate().unwrap().metadata.reading,
        "新しい読み"
    );

    let reads = std::cell::Cell::new(0);
    let html = super::render(&batch, &[], |_| {
        reads.set(reads.get() + 1);
        Ok(bytes.clone())
    })
    .unwrap();
    let first_attempt = html.find("Попытка 1").unwrap();
    let second_attempt = html.find("Попытка 2").unwrap();
    let first_markup = &html[first_attempt..second_attempt];
    let second_markup = &html[second_attempt..];

    assert!(first_markup.contains("чтение: 古い読み"));
    assert!(!first_markup.contains("Текущий кандидат"));
    assert!(second_markup.contains("чтение: 新しい読み"));
    assert!(second_markup.contains("Текущий кандидат · SHA-256"));
    assert_eq!(reads.get(), 1, "байты одного SHA должны читаться один раз");
}

#[test]
fn review_does_not_link_a_trusted_record_that_is_not_the_current_owner_asset() {
    let bytes = png();
    let candidate = candidate("幽霊", &bytes, SemanticStatus::Verified);
    let old_sha = candidate.sha256.clone();
    let old_record = owner_record("幽霊", &bytes, &candidate);
    let mut batch = base_batch(&["幽霊"]);
    batch.items[0].owner_current_sha256 = Some("a".repeat(64));
    batch.items[0].existing_verified_sha256 = Some(old_sha);

    let html = super::render(&batch, &[old_record], |_| {
        panic!("без текущего кандидата не должны читать изображение")
    })
    .unwrap();

    assert!(!html.contains("<img"));
    assert!(!html.contains("../../../assets/png/"));
}

#[test]
fn review_escapes_hostile_strings_and_never_invents_images_for_non_acquired_outcomes() {
    let mut batch = base_batch(&["обычный"]);
    batch.items[0].request.query.reading = Some("<script>alert('user')</script>\"&".into());
    add_outcome(
        &mut batch.items[0],
        PitchBatchOutcome::AmbiguousVocabulary {
            surface: "<img src=x onerror=alert(1)>".into(),
            reading: Some("</dd><script>alert('jpdb')</script>".into()),
            candidates: vec![JpdbVocabularyCandidate {
                vocabulary_id: 9,
                surface_forms: vec!["<svg/onload=alert(1)>".into()],
                readings: vec!["<script>reading</script>".into()],
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: "<script>form</script>".into(),
                    reading: "<script>reading</script>".into(),
                }],
                part_of_speech: vec!["<b>noun</b>".into()],
                meanings: vec!["<script>alert('meaning')</script>".into()],
                detail_url: "https://jpdb.io/vocabulary/9/x\"><script>alert(1)</script>".into(),
            }],
        },
    );
    add_outcome(
        &mut batch.items[0],
        PitchBatchOutcome::NoPitchAccentOnSource {
            evidence: JpdbPitchAbsenceEvidence {
                source_url: "https://jpdb.io/\"><script>source</script>".into(),
                ..absence("обычный")
            },
        },
    );
    add_outcome(
        &mut batch.items[0],
        PitchBatchOutcome::VocabularyNotFound {
            surface: "<b>missing</b>".into(),
            reading: Some("<script>reading</script>".into()),
        },
    );
    add_outcome(
        &mut batch.items[0],
        PitchBatchOutcome::Failed {
            error: JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: "<script>alert('failure')</script> \" &".into(),
            },
        },
    );

    let html = super::render(&batch, &[], |_| {
        panic!("non-acquired outcome must not read a candidate")
    })
    .unwrap();
    assert!(!html.contains("<script"));
    assert!(!html.contains("<img"));
    assert!(!html.contains("<svg"));
    assert!(html.contains("&lt;script&gt;alert(&#39;user&#39;)&lt;/script&gt;"));
    assert!(html.contains("&amp;"));
    assert!(html.contains("&quot;"));
    assert!(html.contains("&lt;script&gt;alert(&#39;meaning&#39;)&lt;/script&gt;"));
    assert!(html.contains("&lt;script&gt;alert(&#39;failure&#39;)&lt;/script&gt;"));
    assert!(html.contains("ambiguous_vocabulary"));
    assert!(html.contains("no_pitch_accent_on_source"));
    assert!(html.contains("vocabulary_not_found"));
    assert!(html.contains("technical_failure"));
}

#[test]
fn candidate_bytes_are_checked_before_a_relative_image_reference_is_emitted() {
    let bytes = png();
    let mut batch = base_batch(&["幽霊"]);
    let corrupt_candidate = candidate("幽霊", &bytes, SemanticStatus::Corrupt);
    let hash = corrupt_candidate.sha256.clone();
    batch.items[0].current_candidate_sha256 = Some(hash.clone());
    add_outcome(
        &mut batch.items[0],
        PitchBatchOutcome::Acquired {
            candidate: Box::new(corrupt_candidate),
        },
    );

    let error = super::render(&batch, &[], |_| Ok(b"different bytes".to_vec())).unwrap_err();
    assert_eq!(error.code, crate::error::ErrorCode::IntegrityMismatch);

    let html = super::render(&batch, &[], |_| Ok(bytes.clone())).unwrap();
    assert!(html.contains(&format!("src=\"candidates/{hash}.png\"")));
    assert!(html.contains("статус проверки: <strong><code>corrupt</code></strong>"));
    assert!(!html.contains("<button"));

    let mut mismatched = candidate("幽霊", &bytes, SemanticStatus::Uncertain);
    mismatched.metadata.evidence.render.pixel_width = 7;
    let mismatched_hash = mismatched.sha256.clone();
    let mut mismatch_batch = base_batch(&["幽霊"]);
    mismatch_batch.items[0].current_candidate_sha256 = Some(mismatched_hash);
    add_outcome(
        &mut mismatch_batch.items[0],
        PitchBatchOutcome::Acquired {
            candidate: Box::new(mismatched),
        },
    );
    let html = super::render(&mismatch_batch, &[], |_| Ok(bytes.clone())).unwrap();
    assert!(!html.contains("<img"));
    assert!(html.contains("не совпали со свидетельством отрисовки"));
}

#[test]
fn candidate_path_is_derived_from_sha_instead_of_trusting_state_path() {
    let bytes = png();
    let mut batch = base_batch(&["幽霊"]);
    let mut candidate = candidate("幽霊", &bytes, SemanticStatus::Verified);
    let hash = candidate.sha256.clone();
    candidate.blob.storage_path = "../../outside.png".into();
    batch.items[0].current_candidate_sha256 = Some(hash);
    add_outcome(
        &mut batch.items[0],
        PitchBatchOutcome::Acquired {
            candidate: Box::new(candidate),
        },
    );

    let error = super::render(&batch, &[], |_| {
        panic!("unsafe candidate path must fail before read")
    })
    .unwrap_err();
    assert_eq!(error.code, crate::error::ErrorCode::PathTraversal);
}
