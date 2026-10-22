use std::fs;
use std::path::{Path, PathBuf};

use super::*;
use crate::hashing::sha256_hex;
use crate::jpdb::{
    JpdbPitchAcquired, JpdbPitchOutcome, JpdbPitchQuery, JpdbPitchRequest, JpdbVocabularyCandidate,
};
use crate::model::Provenance;
use crate::pitch_accent::{
    PitchAccentDomainPolicy, PitchAccentImageValidator, PitchAccentResolvedForm,
};
use crate::store::{StoreOptions, VerifiedIngestRequest};
use crate::temp_workspace::TempWorkspace;

const BATCH: &str = "review-pitch";

struct Fixture {
    _workspace: TempWorkspace,
    store: AssetStore,
    verified_sha: String,
    rejected_sha: String,
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

fn acquired(surface: &str, reading: &str, id: u64, bytes: Vec<u8>) -> JpdbPitchOutcome {
    JpdbPitchOutcome::Acquired {
        asset: Box::new(JpdbPitchAcquired {
            bytes,
            metadata: crate::pitch_accent::test_support::single_variant_metadata(
                surface,
                reading,
                id,
                format!("https://jpdb.io/vocabulary/{id}/{surface}/{reading}"),
                browser(),
            ),
        }),
    }
}

/// Изображение без графика: валидатор v6 не даёт ему `VERIFIED`.
fn blank_png() -> Vec<u8> {
    let image = image::RgbaImage::from_pixel(78, 48, image::Rgba([24, 36, 48, 255]));
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
}

fn ambiguity(surface: &str, reading: &str) -> JpdbPitchOutcome {
    JpdbPitchOutcome::AmbiguousVocabulary {
        surface: surface.into(),
        reading: Some(reading.into()),
        candidates: [201_u64, 202]
            .into_iter()
            .map(|id| JpdbVocabularyCandidate {
                vocabulary_id: id,
                surface_forms: vec![surface.into()],
                readings: vec![reading.into()],
                resolved_forms: vec![PitchAccentResolvedForm {
                    surface: surface.into(),
                    reading: reading.into(),
                }],
                part_of_speech: vec!["Noun".into()],
                meanings: vec![format!("meaning {id}")],
                detail_url: format!("https://jpdb.io/vocabulary/{id}/{surface}/{reading}"),
            })
            .collect(),
    }
}

/// Пакет: проверенный кандидат, отвергнутый валидатором кандидат,
/// неоднозначная запись и незатрагиваемый ожидающий элемент.
fn fixture() -> Fixture {
    let workspace = TempWorkspace::create("pitch-review-decisions-test").unwrap();
    let store = AssetStore::open_with_policy(
        StoreOptions::new(workspace.path().join("store")),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let requests = [
        ("幽霊", "ゆうれい"),
        ("化物", "ばけもの"),
        ("空白", "くうはく"),
        ("未着", "みちゃく"),
    ]
    .map(|(surface, reading)| {
        JpdbPitchRequest::new(JpdbPitchQuery::new(surface, Some(reading.into())))
    });
    let mut batch = PitchAccentBatch::new(
        BATCH,
        requests.to_vec(),
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(store.root(), &batch).unwrap();
    let verified = crate::pitch_accent::test_support::single_variant_png(false);
    let rejected = blank_png();
    for (surface, outcome) in [
        ("幽霊", acquired("幽霊", "ゆうれい", 123, verified.clone())),
        ("化物", ambiguity("化物", "ばけもの")),
        ("空白", acquired("空白", "くうはく", 321, rejected.clone())),
    ] {
        let token = batch.item_token(surface).unwrap();
        assert!(runtime.record_outcome(&mut batch, &token, outcome).unwrap());
    }
    let state = runtime.load().unwrap().unwrap();
    assert_eq!(
        state.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::AcquiredVerified
    );
    assert_eq!(
        state.item("空白").unwrap().status(),
        PitchBatchItemStatus::CandidateRejected
    );
    assert_ne!(
        state
            .item("空白")
            .unwrap()
            .current_candidate()
            .unwrap()
            .validation
            .status,
        SemanticStatus::Verified
    );
    drop(runtime);
    Fixture {
        _workspace: workspace,
        store,
        verified_sha: sha256_hex(&verified),
        rejected_sha: sha256_hex(&rejected),
    }
}

fn load(store: &AssetStore) -> PitchAccentBatch {
    PitchAccentBatchRuntime::open(store.root(), BATCH)
        .unwrap()
        .load()
        .unwrap()
        .unwrap()
}

/// Заполняет только первую колонку строк указанных идентичностей.
fn decide(path: &Path, decisions: &[(&str, &str)]) {
    let text = fs::read_to_string(path).unwrap();
    let edited = text
        .lines()
        .map(|line| {
            let Some(columns) = line.strip_prefix('\t') else {
                return line.to_owned();
            };
            let surface = columns.split('\t').next().unwrap();
            match decisions.iter().find(|(target, _)| *target == surface) {
                Some((_, action)) => format!("{action}\t{columns}"),
                None => line.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{edited}\n")).unwrap();
}

fn journal_path(store: &AssetStore) -> PathBuf {
    store
        .root()
        .join(".journal")
        .join(crate::journal::JOURNAL_FILE)
}

fn journal_lines(store: &AssetStore) -> usize {
    fs::read_to_string(journal_path(store))
        .map(|text| text.lines().count())
        .unwrap_or(0)
}

#[test]
fn export_lists_all_reviewable_rows_with_exact_copies_and_keeps_filled_decisions() {
    let fixture = fixture();
    let (batch, bundle) = export(&fixture.store, BATCH).unwrap();
    let directory = PathBuf::from(&bundle.directory);
    let snapshot: PitchReviewSnapshot =
        serde_json::from_slice(&fs::read(directory.join("snapshot.json")).unwrap()).unwrap();
    assert_eq!(
        sha256_hex(fs::read(directory.join("snapshot.json")).unwrap()),
        bundle.id
    );
    // Неоднозначная запись первой, затем кандидаты в порядке пакета; ожидающий элемент
    // и элементы без решения человека в снимок не попадают.
    let keys = snapshot
        .rows
        .iter()
        .map(|row| (row.identity.key.as_str(), row.kind))
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        vec![
            ("化物", PitchReviewKind::AmbiguousVocabulary),
            ("幽霊", PitchReviewKind::Candidate),
            ("空白", PitchReviewKind::Candidate),
        ]
    );
    assert_eq!(snapshot.rows[0].vocabulary_candidates.len(), 2);
    assert_eq!(
        snapshot.rows[2].validation_status,
        batch
            .item("空白")
            .unwrap()
            .current_candidate()
            .map(|candidate| candidate.validation.status)
    );
    for (surface, sha) in [
        ("幽霊", &fixture.verified_sha),
        ("空白", &fixture.rejected_sha),
    ] {
        let copy = fs::read(directory.join(format!("{surface}-{sha}.png"))).unwrap();
        assert_eq!(&sha256_hex(&copy), sha);
    }
    assert!(
        fs::read_to_string(directory.join("index.html"))
            .unwrap()
            .contains("data:image/png;base64,")
    );
    // Повторный export того же снимка сохраняет заполненный файл решений.
    decide(&bundle.decisions, &[("幽霊", "y")]);
    let filled = fs::read(&bundle.decisions).unwrap();
    let (_, again) = export(&fixture.store, BATCH).unwrap();
    assert_eq!(again.id, bundle.id);
    assert_eq!(fs::read(&again.decisions).unwrap(), filled);
}

#[test]
fn apply_selects_rejects_and_acknowledges_atomically_without_touching_neighbours() {
    let fixture = fixture();
    let before = load(&fixture.store);
    let untouched = serde_json::to_value(before.item("未着").unwrap()).unwrap();
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    decide(
        &bundle.decisions,
        &[("化物", "202"), ("幽霊", "y"), ("空白", "r")],
    );
    let (batch, applied) = apply(&fixture.store, BATCH, &bundle.id).unwrap();
    assert_eq!(
        applied,
        PitchReviewApplied {
            applied: 2,
            selected: 1,
            rejected: 1,
            acknowledged: 1,
            skipped: 0,
            already_applied: 0,
            owner_rejections: 0,
            changed: true,
        }
    );
    let saved = load(&fixture.store);
    assert_eq!(saved, batch);
    let selected = saved.item("化物").unwrap();
    assert_eq!(
        selected
            .request
            .selection
            .as_ref()
            .map(|selection| (selection.vocabulary_id, selection.detail_url.as_str())),
        Some((202, "https://jpdb.io/vocabulary/202/化物/ばけもの"))
    );
    assert_eq!(
        selected.generation,
        before.item("化物").unwrap().generation + 1
    );
    assert_eq!(selected.status(), PitchBatchItemStatus::Pending);
    let rejected = saved.item("空白").unwrap();
    assert!(rejected.rejected_candidates.iter().any(|rejection| {
        rejection.candidate_sha256 == fixture.rejected_sha
            && rejection.reason == rejection_reason(&bundle.id)
    }));
    // Подтверждение просмотра не меняет состояние проверенного кандидата.
    assert_eq!(saved.item("幽霊").unwrap(), before.item("幽霊").unwrap());
    assert_eq!(
        serde_json::to_value(saved.item("未着").unwrap()).unwrap(),
        untouched
    );

    // Повтор применения идемпотентен: ничего не меняет и не дублирует решения.
    let (_, replay) = apply(&fixture.store, BATCH, &bundle.id).unwrap();
    assert_eq!(replay.already_applied, 2);
    assert_eq!(replay.applied, 0);
    assert_eq!(replay.acknowledged, 1);
    assert!(!replay.changed);
    assert_eq!(load(&fixture.store).revision, saved.revision);
    assert_eq!(
        load(&fixture.store)
            .item("空白")
            .unwrap()
            .rejected_candidates
            .len(),
        1
    );
}

fn assert_refused_without_change(
    fixture: &Fixture,
    review_id: &str,
    code: ErrorCode,
) -> AssetError {
    let before = load(&fixture.store);
    let journal = journal_lines(&fixture.store);
    let error = apply(&fixture.store, BATCH, review_id).unwrap_err();
    assert_eq!(error.code, code, "{}", error.message);
    assert_eq!(load(&fixture.store), before);
    assert_eq!(journal_lines(&fixture.store), journal);
    error
}

#[test]
fn approving_png_without_current_verified_is_refused_for_the_whole_file() {
    let fixture = fixture();
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    // Корректные решения в том же файле не применяются частично.
    decide(&bundle.decisions, &[("化物", "201"), ("空白", "y")]);
    let error =
        assert_refused_without_change(&fixture, &bundle.id, ErrorCode::InvalidValidationEvidence);
    assert!(error.message.contains("VERIFIED"));
}

#[test]
fn unknown_vocabulary_id_or_action_is_refused_without_change() {
    let fixture = fixture();
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    let original = fs::read(&bundle.decisions).unwrap();
    for (surface, action, code) in [
        ("化物", "999", ErrorCode::IdentityConflict),
        ("化物", "+201", ErrorCode::IdentityConflict),
        ("化物", "y", ErrorCode::IdentityConflict),
        ("幽霊", "201", ErrorCode::InvalidTransition),
        ("幽霊", "yes", ErrorCode::InvalidTransition),
    ] {
        fs::write(&bundle.decisions, &original).unwrap();
        decide(&bundle.decisions, &[(surface, action)]);
        assert_refused_without_change(&fixture, &bundle.id, code);
    }
}

#[test]
fn tampered_snapshot_copy_or_metadata_is_refused_without_change() {
    let fixture = fixture();
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    decide(&bundle.decisions, &[("空白", "r")]);
    let directory = PathBuf::from(&bundle.directory);
    let decisions = fs::read_to_string(&bundle.decisions).unwrap();
    let item = load(&fixture.store);
    let item = item.item("空白").unwrap().token();
    let revision = format!(
        "\tcandidate_rejected\t{}\t{}\t",
        item.generation, item.item_revision
    );
    let forged_revision = format!(
        "\tcandidate_rejected\t{}\t{}\t",
        item.generation,
        item.item_revision + 1
    );
    // Подмена колонки: чужой SHA, иной URL словарной записи, иная ревизия.
    for (from, to) in [
        (fixture.rejected_sha.as_str(), fixture.verified_sha.as_str()),
        ("vocabulary/202/", "vocabulary/203/"),
        (revision.as_str(), forged_revision.as_str()),
    ] {
        assert!(decisions.contains(from), "{from}");
        fs::write(&bundle.decisions, decisions.replacen(from, to, 1)).unwrap();
        assert_refused_without_change(&fixture, &bundle.id, ErrorCode::InvalidTransition);
    }
    fs::write(&bundle.decisions, &decisions).unwrap();
    // Подмена копии изображения.
    let copy = directory.join(format!("空白-{}.png", fixture.rejected_sha));
    let original_copy = fs::read(&copy).unwrap();
    fs::write(
        &copy,
        crate::pitch_accent::test_support::single_variant_png(true),
    )
    .unwrap();
    assert_refused_without_change(&fixture, &bundle.id, ErrorCode::InvalidTransition);
    fs::write(&copy, &original_copy).unwrap();
    // Подмена метаданных снимка не совпадает с его адресом.
    let snapshot = directory.join("snapshot.json");
    let original_snapshot = fs::read_to_string(&snapshot).unwrap();
    fs::write(
        &snapshot,
        original_snapshot.replace(
            "\"status\":\"candidate_rejected\"",
            "\"status\":\"acquired_verified\"",
        ),
    )
    .unwrap();
    assert_refused_without_change(&fixture, &bundle.id, ErrorCode::InvalidTransition);
    fs::write(&snapshot, &original_snapshot).unwrap();
    // Чужой или несуществующий review_id.
    assert_refused_without_change(&fixture, &"f".repeat(64), ErrorCode::MissingAssetFile);
    // После восстановления исходных файлов снимок снова применим.
    apply(&fixture.store, BATCH, &bundle.id).unwrap();
}

#[test]
fn stale_review_after_competing_command_is_refused_atomically() {
    let fixture = fixture();
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    decide(&bundle.decisions, &[("化物", "201"), ("空白", "r")]);
    // Конкурентная адресная команда меняет ревизию элемента после export.
    {
        let mut runtime = PitchAccentBatchRuntime::open(fixture.store.root(), BATCH).unwrap();
        let mut batch = runtime.load().unwrap().unwrap();
        batch
            .select_candidate("化物", 202, "https://jpdb.io/vocabulary/202/化物/ばけもの")
            .unwrap();
        runtime.save(&batch).unwrap();
    }
    let error = assert_refused_without_change(&fixture, &bundle.id, ErrorCode::InvalidTransition);
    assert!(error.message.contains("устарел"));
    // Отказ строки 化物 не применил решение соседней строки 空白.
    assert!(
        load(&fixture.store)
            .item("空白")
            .unwrap()
            .rejected_candidates
            .is_empty()
    );
}

#[test]
fn apply_waits_for_competing_command_and_then_refuses_stale_rows() {
    let fixture = fixture();
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    decide(&bundle.decisions, &[("化物", "201"), ("空白", "r")]);
    // Конкурирующая команда держит блокировку пакета, пока `apply` ждёт её.
    let mut holder = PitchAccentBatchRuntime::open(fixture.store.root(), BATCH).unwrap();
    let journal = journal_lines(&fixture.store);
    let competing = std::thread::scope(|scope| {
        let pending = scope.spawn(|| apply(&fixture.store, BATCH, &bundle.id));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!pending.is_finished());
        let mut batch = holder.load().unwrap().unwrap();
        batch
            .select_candidate("化物", 202, "https://jpdb.io/vocabulary/202/化物/ばけもの")
            .unwrap();
        holder.save(&batch).unwrap();
        drop(holder);
        let error = pending.join().unwrap().unwrap_err();
        (batch, error)
    });
    let (batch, error) = competing;
    assert_eq!(error.code, ErrorCode::InvalidTransition);
    assert!(error.message.contains("устарел"), "{}", error.message);
    // Конкурирующее изменение сохранено, соседняя строка не применена частично.
    assert_eq!(load(&fixture.store), batch);
    assert!(batch.item("空白").unwrap().rejected_candidates.is_empty());
    assert_eq!(journal_lines(&fixture.store), journal);
}

/// Публикует текущего кандидата у владельца, не сверяя с ним пакет.
fn ingest_verified_owner(fixture: &Fixture) -> (PitchAccentBatchRuntime, PitchAccentBatch) {
    let mut runtime = PitchAccentBatchRuntime::open(fixture.store.root(), BATCH).unwrap();
    let batch = ingest_verified_owner_locked(fixture, &mut runtime);
    (runtime, batch)
}

/// То же под уже удерживаемой блокировкой пакета.
fn ingest_verified_owner_locked(
    fixture: &Fixture,
    runtime: &mut PitchAccentBatchRuntime,
) -> PitchAccentBatch {
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .begin_publication("幽霊", &fixture.verified_sha, None)
        .unwrap();
    runtime.save(&batch).unwrap();
    let metadata = crate::pitch_accent::test_support::single_variant_metadata(
        "幽霊",
        "ゆうれい",
        123,
        "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
        browser(),
    );
    fixture
        .store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes: crate::pitch_accent::test_support::single_variant_png(false),
                provenance: Provenance {
                    source_kind: "jpdb_browser_render".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(serde_json::to_value(metadata).unwrap()),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    batch
}

/// Публикует текущего кандидата штатными шагами пакета и хранилища без браузера.
fn publish_verified(fixture: &Fixture) {
    let (mut runtime, mut batch) = ingest_verified_owner(fixture);
    batch
        .reconcile_owner(
            &PitchBatchOwnerSnapshot::from_records(fixture.store.verify_integrity().unwrap())
                .unwrap(),
        )
        .unwrap();
    runtime.save(&batch).unwrap();
}

#[test]
fn refused_apply_does_not_persist_owner_reconciliation() {
    let fixture = fixture();
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    decide(&bundle.decisions, &[("化物", "999")]);
    // Владелец изменился после export: сверка в `apply` меняет пакет в памяти.
    drop(ingest_verified_owner(&fixture));
    let owner =
        PitchBatchOwnerSnapshot::from_records(fixture.store.verify_integrity().unwrap()).unwrap();
    let mut reconciled = load(&fixture.store);
    reconciled.reconcile_owner(&owner).unwrap();
    assert_ne!(reconciled, load(&fixture.store));
    assert_refused_without_change(&fixture, &bundle.id, ErrorCode::IdentityConflict);
}

#[test]
fn apply_reconciles_with_owner_state_observed_under_batch_lock() {
    let fixture = fixture();
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    decide(&bundle.decisions, &[("空白", "r")]);
    // Пока `apply` ждёт блокировку, конкурирующая команда публикует 幽霊.
    let mut holder = PitchAccentBatchRuntime::open(fixture.store.root(), BATCH).unwrap();
    let (batch, applied) = std::thread::scope(|scope| {
        let pending = scope.spawn(|| apply(&fixture.store, BATCH, &bundle.id));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!pending.is_finished());
        let mut batch = ingest_verified_owner_locked(&fixture, &mut holder);
        batch
            .reconcile_owner(
                &PitchBatchOwnerSnapshot::from_records(fixture.store.verify_integrity().unwrap())
                    .unwrap(),
            )
            .unwrap();
        holder.save(&batch).unwrap();
        drop(holder);
        pending.join().unwrap().unwrap()
    });
    assert_eq!(applied.rejected, 1);
    // Сверка не откатила публикацию по снимку владельца, снятому до ожидания.
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    assert_eq!(load(&fixture.store), batch);
}

#[test]
fn rejecting_published_png_quarantines_owner_once_through_store_attestation() {
    let fixture = fixture();
    publish_verified(&fixture);
    assert_eq!(
        load(&fixture.store).item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    let journal = journal_lines(&fixture.store);
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    decide(&bundle.decisions, &[("幽霊", "r")]);
    let (batch, applied) = apply(&fixture.store, BATCH, &bundle.id).unwrap();
    assert_eq!((applied.rejected, applied.owner_rejections), (1, 1));
    let owner = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity.key == "幽霊")
        .unwrap();
    assert_eq!(owner.sha256, fixture.verified_sha);
    assert_eq!(owner.lifecycle, LifecycleState::Quarantined);
    assert_eq!(owner.current_human_decision(), Some(HumanDecision::Reject));
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Conflict
    );
    assert_eq!(load(&fixture.store), batch);
    let events = fs::read_to_string(journal_path(&fixture.store)).unwrap();
    let revoked = events
        .lines()
        .skip(journal)
        .filter(|line| {
            line.contains("\"operation\":\"trust_revoked\"")
                && line.contains("\"outcome\":\"completed\"")
        })
        .count();
    assert_eq!(revoked, 1, "{events}");
    // Повтор не пишет второе событие и не меняет канонический корпус.
    let after = journal_lines(&fixture.store);
    let (_, replay) = apply(&fixture.store, BATCH, &bundle.id).unwrap();
    assert_eq!((replay.already_applied, replay.owner_rejections), (1, 0));
    assert!(!replay.changed);
    assert_eq!(journal_lines(&fixture.store), after);
}

#[test]
fn review_cannot_confer_trust_on_automated_domain() {
    // Политика домена остаётся AUTOMATED_VERIFIED_ONLY: `y` не публикует и не
    // меняет запись владельца, а только подтверждает просмотр VERIFIED.
    let fixture = fixture();
    let before = fixture.store.verify_integrity().unwrap();
    let (_, bundle) = export(&fixture.store, BATCH).unwrap();
    decide(&bundle.decisions, &[("幽霊", "y")]);
    let (_, applied) = apply(&fixture.store, BATCH, &bundle.id).unwrap();
    assert_eq!((applied.acknowledged, applied.applied), (1, 0));
    assert_eq!(fixture.store.verify_integrity().unwrap(), before);
}
