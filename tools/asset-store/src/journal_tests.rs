//! Регрессии долговременного журнала операций: двухфазная запись, сверка и целостность.
use super::*;
use crate::journal::JOURNAL_FILE;
use crate::model::ValidationEvidence;
use crate::temp_workspace::TempWorkspace;

const CANDIDATE_BYTES: &[u8] = b"GIF89a journal fixture bytes";

struct Fixture {
    store: AssetStore,
    root: PathBuf,
    workspace: TempWorkspace,
}

impl Fixture {
    fn new(purpose: &str) -> Self {
        let workspace = TempWorkspace::create(purpose).expect("создаётся временный каталог теста");
        let root = workspace.path().join("store");
        let store = AssetStore::open_kanji(StoreOptions::new(&root)).expect("открывается store");
        Self {
            store,
            root,
            workspace,
        }
    }

    fn path(&self) -> &Path {
        self.workspace.path()
    }

    fn reopen(&self) -> AssetStore {
        AssetStore::open_kanji_existing(StoreOptions::new(&self.root)).expect("store переоткрыт")
    }

    fn events(&self) -> Vec<JournalEvent> {
        self.store
            .journal_events(None, None, None)
            .expect("журнал читается")
    }

    fn events_for(&self, identity: &AssetIdentity) -> Vec<JournalEvent> {
        self.store
            .journal_events(None, Some(identity.key.as_str()), None)
            .expect("журнал читается")
    }

    fn ingest_candidate(&self, identity: &AssetIdentity, bytes: &[u8]) -> IngestOutcome {
        let source = self.path().join("candidate.gif");
        fs::write(&source, bytes).expect("записан синтетический кандидат");
        self.store
            .ingest(IngestRequest {
                identity: identity.clone(),
                source_path: source,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .expect("кандидат импортирован")
    }

    fn publish(
        &self,
        identity: &AssetIdentity,
        bytes: &[u8],
    ) -> Result<VerifiedIngestOutcome, AssetError> {
        self.store.ingest_verified(
            VerifiedIngestRequest {
                identity: identity.clone(),
                bytes: bytes.to_vec(),
                provenance: Provenance {
                    source_kind: "fixture".to_owned(),
                    source_name: "candidate.gif".to_owned(),
                },
                domain_metadata: None,
                replace_expected_sha256: None,
            },
            &AcceptValidator::default(),
        )
    }
}

/// Валидатор с настраиваемой идентичностью: его второе решение меняет
/// `validation` записи, не меняя её SHA-256 и жизненный цикл.
#[derive(Default)]
struct AcceptValidator {
    id: &'static str,
}

impl AcceptValidator {
    fn other() -> Self {
        Self {
            id: "journal-fixture-second",
        }
    }
}

impl SemanticValidator for AcceptValidator {
    fn identity(&self) -> ValidatorIdentity {
        ValidatorIdentity {
            id: if self.id.is_empty() {
                "journal-fixture".to_owned()
            } else {
                self.id.to_owned()
            },
            version: "1".to_owned(),
        }
    }

    fn validate(
        &self,
        _asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let mut contents = Vec::new();
        bytes
            .read_to_end(&mut contents)
            .map_err(|error| ValidatorFailure::new("read_failed", error.to_string()))?;
        Ok(SemanticDecision::new(
            SemanticStatus::Verified,
            vec![ValidationEvidence {
                kind: "synthetic".to_owned(),
                summary: format!("{} synthetic bytes", contents.len()),
                details: None,
            }],
        ))
    }
}

fn failed_publish(error: &AssetError) -> bool {
    error.code == ErrorCode::IoFailure
}

#[test]
fn journal_keeps_sequence_and_intent_pair_across_store_reopen() {
    let fixture = Fixture::new("asset-store-journal-persist");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    let outcome = fixture.ingest_candidate(&identity, CANDIDATE_BYTES);
    let sha256 = outcome.asset.sha256.clone();

    // Перезапуск CLI — это новое открытие того же store.
    let reopened = fixture.reopen();
    let events = reopened.journal_events(None, None, None).unwrap();
    assert_eq!(events.len(), 2, "интент и результат записи кандидата");
    assert_eq!(events[0].seq, 1);
    assert_eq!(events[1].seq, 2);
    assert_eq!(events[0].operation, JournalOperation::CandidateWritten);
    assert_eq!(events[0].outcome, JournalOutcome::Pending);
    assert_eq!(events[1].outcome, JournalOutcome::Completed);
    assert_eq!(
        events[1].intent_id.as_deref(),
        Some(events[0].event_id.as_str()),
        "результат ссылается на event_id интента"
    );
    assert_ne!(events[0].event_id, events[1].event_id);
    assert_ne!(events[0].event_key, events[1].event_key);
    assert_eq!(events[1].previous_sha256, None);
    assert_eq!(events[1].new_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(events[1].new_lifecycle.as_deref(), Some("pending"));
    assert_eq!(events[1].verified, Some(false));
    assert!(reopened.journal_check().unwrap().is_clean());
}

#[test]
fn journal_survives_removal_of_a_batch_directory() {
    let fixture = Fixture::new("asset-store-journal-batch");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture.ingest_candidate(&identity, CANDIDATE_BYTES);

    // Локальное состояние пакета живёт в `.runtime/batches/<id>` и может быть
    // удалено без потери журнала: журнал лежит вне `.runtime`.
    let batch = fixture.root.join(".runtime/batches/review-batch");
    fs::create_dir_all(&batch).unwrap();
    fs::write(batch.join("state.json"), b"{}").unwrap();
    let with_batch = fixture.reopen();
    assert_eq!(
        with_batch.journal_events(None, None, None).unwrap().len(),
        2
    );
    drop(with_batch);

    fs::remove_dir_all(&batch).unwrap();
    let without_batch = fixture.reopen();
    assert_eq!(
        without_batch
            .journal_events(None, None, None)
            .unwrap()
            .len(),
        2
    );
    assert!(fixture.root.join(JOURNAL_DIR).join(JOURNAL_FILE).is_file());
    assert!(without_batch.journal_check().unwrap().is_clean());
}

#[test]
fn repeated_trusted_reuse_writes_one_reused_event_and_no_second_publication() {
    let fixture = Fixture::new("asset-store-journal-reuse");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    let first = fixture.publish(&identity, CANDIDATE_BYTES).unwrap();
    assert!(first.changed);
    let second = fixture.publish(&identity, CANDIDATE_BYTES).unwrap();
    assert!(!second.changed, "повторный reuse ничего не меняет");
    let third = fixture.publish(&identity, CANDIDATE_BYTES).unwrap();
    assert!(!third.changed);

    let events = fixture.events_for(&identity);
    let published = events
        .iter()
        .filter(|event| {
            event.operation == JournalOperation::VerifiedPublished
                && event.outcome == JournalOutcome::Completed
        })
        .count();
    assert_eq!(published, 1, "публикация записана ровно один раз");
    let reused = events
        .iter()
        .filter(|event| event.operation == JournalOperation::VerifiedReused)
        .count();
    assert_eq!(reused, 1, "повторный reuse не пишет второе событие");
    assert!(fixture.store.journal_check().unwrap().is_clean());
}

#[test]
fn reconcile_closes_an_interrupted_publication_as_recovered() {
    let fixture = Fixture::new("asset-store-journal-recovered");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    FAIL_BEFORE_JOURNAL_CLOSE.with(|hook| hook.set(true));
    let outcome = fixture.publish(&identity, CANDIDATE_BYTES).unwrap();
    assert!(outcome.changed, "изменение состояния состоялось");

    let check = fixture.store.journal_check().unwrap();
    assert!(!check.is_clean(), "интент остался незакрытым");
    assert_eq!(check.open_intents.len(), 1);

    let appended = fixture.store.reconcile_journal().unwrap();
    assert_eq!(appended.len(), 1);
    assert_eq!(appended[0].outcome, JournalOutcome::Recovered);
    assert_eq!(appended[0].operation, JournalOperation::VerifiedPublished);
    assert_eq!(
        appended[0].new_sha256.as_deref(),
        Some(outcome.asset.as_ref().unwrap().sha256.as_str())
    );
    assert_eq!(appended[0].verified, Some(true));

    assert!(fixture.store.journal_check().unwrap().is_clean());
    assert!(
        fixture.store.reconcile_journal().unwrap().is_empty(),
        "повторная сверка ничего не дописывает"
    );
}

#[test]
fn reconcile_closes_an_interrupted_refusal_as_aborted() {
    let fixture = Fixture::new("asset-store-journal-aborted");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture.store.fail_next_manifest_write();
    FAIL_BEFORE_JOURNAL_CLOSE.with(|hook| hook.set(true));
    let error = fixture.publish(&identity, CANDIDATE_BYTES).unwrap_err();
    assert!(failed_publish(&error), "{error:?}");

    let check = fixture.store.journal_check().unwrap();
    assert_eq!(check.open_intents.len(), 1, "интент остался незакрытым");

    let appended = fixture.store.reconcile_journal().unwrap();
    assert_eq!(appended.len(), 1);
    assert_eq!(appended[0].outcome, JournalOutcome::Aborted);
    assert_eq!(appended[0].previous_sha256, None);
    assert_eq!(appended[0].new_sha256, None);
    assert!(fixture.store.journal_check().unwrap().is_clean());
    assert!(
        fixture.store.reconcile_journal().unwrap().is_empty(),
        "повторная сверка ничего не дописывает"
    );
}

#[test]
fn a_standard_refusal_closes_its_intent_with_a_reason() {
    let fixture = Fixture::new("asset-store-journal-refused");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture.store.fail_next_manifest_write();
    let error = fixture.publish(&identity, CANDIDATE_BYTES).unwrap_err();
    assert!(failed_publish(&error), "{error:?}");

    let events = fixture.events_for(&identity);
    let intent = events
        .iter()
        .find(|event| event.outcome == JournalOutcome::Pending)
        .expect("интент записан");
    let refused = events
        .iter()
        .find(|event| event.outcome == JournalOutcome::Refused)
        .expect("отказ закрыл интент");
    assert_eq!(
        refused.intent_id.as_deref(),
        Some(intent.event_id.as_str()),
        "отказ ссылается на свой интент"
    );
    assert!(
        refused
            .reason
            .as_deref()
            .is_some_and(|reason| !reason.is_empty()),
        "отказ обязан нести причину"
    );
    assert!(
        fixture.store.journal_check().unwrap().is_clean(),
        "штатный отказ не оставляет висящих интентов"
    );
}

#[test]
fn an_error_after_the_canonical_commit_leaves_the_intent_to_reconciliation() {
    let fixture = Fixture::new("asset-store-journal-after-commit");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture.ingest_candidate(&identity, CANDIDATE_BYTES);
    FAIL_AFTER_CANONICAL_TRANSITION.with(|hook| hook.set(true));
    let error = fixture.publish(&identity, CANDIDATE_BYTES).unwrap_err();
    assert!(failed_publish(&error), "{error:?}");

    // Канонический манифест уже содержит проверенную запись, поэтому отказ с
    // прежними фактами был бы ложью о состоянии хранилища.
    let events = fixture.events_for(&identity);
    assert!(
        !events
            .iter()
            .any(|event| event.outcome == JournalOutcome::Refused),
        "переход зафиксировал запись: закрывать интент отказом нельзя"
    );
    assert_eq!(
        fixture.store.journal_check().unwrap().open_intents.len(),
        1,
        "интент ждёт сверки"
    );

    // Сверка разбирает его по фактическому состоянию манифеста.
    let appended = fixture.store.reconcile_journal().unwrap();
    assert_eq!(appended.len(), 1);
    assert_eq!(appended[0].outcome, JournalOutcome::Recovered);
    assert_eq!(appended[0].new_lifecycle.as_deref(), Some("verified"));
    assert!(fixture.store.journal_check().unwrap().is_clean());
}

#[test]
fn a_transition_that_changes_only_validation_is_not_called_recovered() {
    let fixture = Fixture::new("asset-store-journal-validation-only");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture.publish(&identity, CANDIDATE_BYTES).unwrap();

    // Решение второго валидатора меняет только `validation`: SHA-256 и жизненный
    // цикл остаются прежними, поэтому манифест не различает применённое и
    // неприменённое изменение.
    FAIL_BEFORE_JOURNAL_CLOSE.with(|hook| hook.set(true));
    fixture
        .store
        .validate(SelectionMode::Full, &AcceptValidator::other())
        .expect("повторная проверка");

    let appended = fixture.store.reconcile_journal().unwrap();
    assert_eq!(appended.len(), 1, "сверка закрыла интент: {appended:?}");
    assert_eq!(
        appended[0].outcome,
        JournalOutcome::Unknown,
        "исход не выдаётся за восстановленный: {:?}",
        appended[0]
    );
    assert!(
        appended[0]
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("не различает")),
        "причина объясняет неизвестность: {:?}",
        appended[0].reason
    );
    assert!(fixture.store.journal_check().unwrap().is_clean());
}

#[test]
fn a_refusal_of_a_validation_only_transition_stays_open_for_reconciliation() {
    let fixture = Fixture::new("asset-store-journal-validation-only-refusal");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture.publish(&identity, CANDIDATE_BYTES).unwrap();

    fixture.store.fail_next_manifest_write();
    fixture
        .store
        .validate(SelectionMode::Full, &AcceptValidator::other())
        .unwrap_err();

    // Отказ до фиксации неотличим от уже применённого изменения, поэтому интент
    // не закрывается событием `Refused`, а остаётся сверке.
    let refused: Vec<_> = fixture
        .events()
        .into_iter()
        .filter(|event| event.outcome == JournalOutcome::Refused)
        .collect();
    assert!(refused.is_empty(), "ложного отказа нет: {refused:?}");
    let check = fixture.store.journal_check().unwrap();
    assert_eq!(check.open_intents.len(), 1, "интент ждёт сверки");

    let appended = fixture.store.reconcile_journal().unwrap();
    assert_eq!(appended.len(), 1, "сверка закрыла интент: {appended:?}");
    assert_eq!(appended[0].outcome, JournalOutcome::Unknown);
    assert!(fixture.store.journal_check().unwrap().is_clean());
}

#[test]
fn recovery_facts_survive_a_failing_reconciliation() {
    let fixture = Fixture::new("asset-store-journal-recovery-before-failure");
    let removed = AssetIdentity::new("kanji", "一").unwrap();
    let marker = AssetIdentity::new("kanji", "日").unwrap();
    let record = fixture
        .publish(&removed, CANDIDATE_BYTES)
        .unwrap()
        .asset
        .unwrap();
    fixture.publish(&marker, CANDIDATE_BYTES).unwrap();

    // Смерть процесса между фиксацией удаления и снятием байтов: открытие
    // завершит удаление и обязано записать событие о нём.
    {
        let lock = fixture.store.lock_exclusive().unwrap();
        let mut manifest = load_manifest(&fixture.store.root_handle).unwrap();
        FAIL_AFTER_REMOVAL_MANIFEST.with(|hook| hook.set(true));
        remove_record_from_area(
            &fixture.store.root_handle,
            &mut manifest,
            &record,
            fixture.store.policy.as_ref(),
        )
        .unwrap_err();
        lock.unlock().unwrap();
    }
    // Второй сбой — расхождение transition marker: сверка откажет уже после
    // того, как восстановление удаления состоялось.
    begin_transition(
        &fixture.store.root_handle,
        &marker,
        TransitionTarget::Canonical,
        &"a".repeat(64),
    )
    .unwrap();

    let error = AssetStore::open_kanji_existing(StoreOptions::new(&fixture.root)).unwrap_err();
    assert_eq!(error.code, ErrorCode::IntegrityMismatch, "{error:?}");

    // Факт восстановления остаётся в журнале: повторный запуск маркер удаления
    // уже не найдёт и восстановить его не сможет.
    let root = std::fs::File::open(&fixture.root).unwrap();
    let mut journal = crate::journal::Journal::open(&root).unwrap();
    let recovered: Vec<_> = journal
        .read()
        .unwrap()
        .events
        .into_iter()
        .filter(|event| event.operation == JournalOperation::Recovered)
        .collect();
    assert_eq!(recovered.len(), 1, "событие восстановления записано");
    assert_eq!(recovered[0].identity, removed.key);
}

#[test]
fn recovery_describes_the_surviving_record_of_an_overlap() {
    let fixture = Fixture::new("asset-store-journal-overlap-recovery");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    let sha256 = fixture
        .ingest_candidate(&identity, CANDIDATE_BYTES)
        .asset
        .sha256;
    FAIL_AFTER_CANONICAL_TRANSITION.with(|hook| hook.set(true));
    let error = fixture.publish(&identity, CANDIDATE_BYTES).unwrap_err();
    assert!(failed_publish(&error), "{error:?}");

    // Переоткрытие завершает прерванный переход: запись остаётся в каноническом
    // манифесте, поэтому событие обязано описывать её фактическое состояние, а
    // не исчезновение идентичности.
    let reopened = fixture.reopen();
    let recovered: Vec<_> = reopened
        .journal_events(None, Some(identity.key.as_str()), None)
        .unwrap()
        .into_iter()
        .filter(|event| event.operation == JournalOperation::Recovered)
        .collect();
    assert_eq!(recovered.len(), 1, "одно событие о завершённом переходе");
    assert_eq!(recovered[0].new_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(recovered[0].new_lifecycle.as_deref(), Some("verified"));
    assert_eq!(recovered[0].verified, Some(true));
}

#[test]
fn a_retry_after_a_refusal_starts_a_new_intent() {
    let fixture = Fixture::new("asset-store-journal-retry-intent");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture.store.fail_next_manifest_write();
    let error = fixture.publish(&identity, CANDIDATE_BYTES).unwrap_err();
    assert!(failed_publish(&error), "{error:?}");

    // Состояние записи не изменилось, поэтому повторная попытка повторяет
    // материал интента: он обязан остаться новым событием, а не закрыться
    // вторым результатом того же перехода.
    let outcome = fixture.publish(&identity, CANDIDATE_BYTES).unwrap();
    assert_eq!(
        outcome.asset.as_ref().unwrap().lifecycle,
        LifecycleState::Verified
    );

    let events = fixture.events();
    let intents: Vec<_> = events
        .iter()
        .filter(|event| event.is_pending_intent())
        .collect();
    assert_eq!(intents.len(), 2, "каждая попытка публикации — свой интент");
    for intent in intents {
        let closings: Vec<_> = events
            .iter()
            .filter(|event| event.intent_id.as_deref() == Some(intent.event_id.as_str()))
            .collect();
        assert_eq!(closings.len(), 1, "интент закрыт ровно один раз");
    }
    let check = fixture.store.journal_check().unwrap();
    assert!(check.is_clean(), "{:?}", check.problems);
}

#[test]
fn audit_events_distinguish_publication_refusal_and_reuse() {
    let fixture = Fixture::new("asset-store-journal-audit");
    let published_identity = AssetIdentity::new("kanji", "日").unwrap();
    let refused_identity = AssetIdentity::new("kanji", "一").unwrap();
    let outcome = fixture
        .publish(&published_identity, CANDIDATE_BYTES)
        .unwrap();
    let sha256 = outcome.asset.as_ref().unwrap().sha256.clone();
    fixture
        .publish(&published_identity, CANDIDATE_BYTES)
        .unwrap();

    fixture.store.fail_next_manifest_write();
    let error = fixture
        .publish(&refused_identity, CANDIDATE_BYTES)
        .unwrap_err();
    assert!(failed_publish(&error), "{error:?}");

    let events = fixture.events();
    let publication = events
        .iter()
        .find(|event| {
            event.operation == JournalOperation::VerifiedPublished
                && event.outcome == JournalOutcome::Completed
        })
        .expect("публикация записана");
    assert_eq!(publication.previous_sha256, None);
    assert_eq!(publication.new_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(publication.new_lifecycle.as_deref(), Some("verified"));
    assert_eq!(publication.verified, Some(true));
    assert_eq!(
        publication.provenance.as_deref(),
        Some("fixture:candidate.gif")
    );

    let reuse = events
        .iter()
        .find(|event| event.operation == JournalOperation::VerifiedReused)
        .expect("повторное использование записано");
    assert_eq!(reuse.previous_sha256, reuse.new_sha256);
    assert_eq!(reuse.new_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(reuse.verified, Some(true));

    let refusal = events
        .iter()
        .find(|event| {
            event.operation == JournalOperation::VerifiedPublished
                && event.outcome == JournalOutcome::Refused
        })
        .expect("отказ записан");
    assert_eq!(refusal.previous_sha256, None);
    assert_eq!(refusal.new_sha256, None);
    assert_eq!(refusal.new_lifecycle, None);
    assert_eq!(refusal.verified, Some(false));

    let audit: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                (event.operation, event.outcome),
                (
                    JournalOperation::VerifiedPublished,
                    JournalOutcome::Completed
                ) | (JournalOperation::VerifiedReused, JournalOutcome::Completed)
                    | (JournalOperation::VerifiedPublished, JournalOutcome::Refused)
            )
        })
        .map(|event| (event.operation, event.outcome))
        .collect();
    assert_eq!(audit.len(), 3);
    let unique: BTreeSet<_> = audit.iter().collect();
    assert_eq!(unique.len(), 3, "три разных audit-события: {audit:?}");
}

#[test]
fn journal_context_marks_the_events_of_the_current_run() {
    let fixture = Fixture::new("asset-store-journal-context");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture
        .store
        .set_journal_context(Some("run-1"), Some("pitch-batch-1"));
    fixture.ingest_candidate(&identity, CANDIDATE_BYTES);

    let events = fixture.events_for(&identity);
    assert_eq!(events.len(), 2);
    for event in &events {
        assert_eq!(event.run_id.as_deref(), Some("run-1"));
        assert_eq!(event.batch_id.as_deref(), Some("pitch-batch-1"));
    }

    // Контекст не входит в ключ события: повтор того же перехода остаётся
    // идемпотентным и при другом контексте.
    fixture.store.set_journal_context(None, None);
    fixture.ingest_candidate(&identity, CANDIDATE_BYTES);
    assert_eq!(fixture.events_for(&identity).len(), 2);
}

#[test]
fn baseline_is_written_once_and_refuses_a_repeat() {
    let fixture = Fixture::new("asset-store-journal-baseline");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture.ingest_candidate(&identity, CANDIDATE_BYTES);

    let baseline = fixture.store.journal_baseline().unwrap();
    assert_eq!(baseline.operation, JournalOperation::Baseline);
    assert_eq!(baseline.outcome, JournalOutcome::Completed);
    assert_eq!(baseline.identity, STORE_WIDE_IDENTITY);
    assert_eq!(baseline.namespace, "kanji");
    let snapshot = baseline.snapshot.as_ref().expect("baseline несёт снимок");
    assert_eq!(snapshot.assets, 1);
    assert_eq!(snapshot.pending, 1);
    assert_eq!(snapshot.verified, 0);

    let error = fixture.store.journal_baseline().unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidTransition);
    let events = fixture
        .store
        .journal_events(None, None, Some(JournalOperation::Baseline))
        .unwrap();
    assert_eq!(events.len(), 1, "baseline записан ровно один раз");
}

#[test]
fn check_reports_sequence_gaps_and_dangling_intents_from_the_file() {
    let fixture = Fixture::new("asset-store-journal-check");
    let identity = AssetIdentity::new("kanji", "日").unwrap();
    fixture.ingest_candidate(&identity, CANDIDATE_BYTES);
    let file = fixture.root.join(JOURNAL_DIR).join(JOURNAL_FILE);
    let text = fs::read_to_string(&file).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    // Ломаем нумерацию: результат получает seq=5 вместо 2.
    let mut broken = lines[1].replace("\"seq\":2", "\"seq\":5");
    // Интент перестаёт быть закрытым: ссылка на него исчезает.
    let marker = "\"intent_id\":\"";
    if let Some(value_start) = broken.find(marker) {
        let quote_start = value_start + marker.len();
        if let Some(offset) = broken[quote_start..].find('"') {
            broken.replace_range(value_start..quote_start + offset + 1, "\"intent_id\":null");
        }
    }
    fs::write(&file, format!("{}\n{broken}\n", lines[0])).unwrap();

    let check = fixture.store.journal_check().unwrap();
    assert!(!check.is_clean());
    assert!(
        check
            .problems
            .iter()
            .any(|problem| problem.contains("нарушена нумерация seq")),
        "{:?}",
        check.problems
    );
    assert_eq!(check.open_intents.len(), 1);
    assert!(
        check
            .problems
            .iter()
            .any(|problem| problem.contains("не закрыт событием результата")),
        "{:?}",
        check.problems
    );
}
