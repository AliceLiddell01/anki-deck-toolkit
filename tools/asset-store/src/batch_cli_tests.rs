use super::*;
use crate::temp_workspace::TempWorkspace;
use std::fs;
use std::sync::atomic::Ordering;

fn read_kanji_verified(
    root: impl AsRef<std::path::Path>,
    identities: &[crate::model::AssetIdentity],
    validator: &crate::model::ValidatorIdentity,
) -> Result<Vec<crate::store::VerifiedAssetBytes>, crate::error::AssetError> {
    AssetStore::read_verified_with_policy(
        root,
        identities,
        validator,
        &crate::domain::KanjiDomainPolicy,
    )
}

struct Fixture {
    store: AssetStore,
    workspace: TempWorkspace,
}
impl Fixture {
    fn new() -> Self {
        let workspace = TempWorkspace::create("asset-store-kanji-batch-cli-tests").unwrap();
        let store =
            AssetStore::open_kanji(StoreOptions::new(workspace.path().join("corpus"))).unwrap();
        Self { store, workspace }
    }
    fn directory(&self) -> &std::path::Path {
        self.workspace.path()
    }
    fn start(&self, id: &str, characters: &[char]) {
        let (_, code) = execute_command(
            &self.store,
            self.summary(),
            &BatchCommand::Start {
                batch_id: Some(id.into()),
                characters: characters.iter().map(char::to_string).collect(),
            },
            false,
        )
        .unwrap();
        assert_eq!(code, 0);
    }
    fn load(&self, id: &str) -> KanjiBatch {
        BatchRuntime::open(self.store.root(), id)
            .unwrap()
            .load()
            .unwrap()
            .unwrap()
    }
    fn summary(&self) -> StoreSummary {
        StoreSummary {
            path: self.store.root().display().to_string(),
            store_id: Some(self.store.store_id().into()),
            layout_migrated_on_open: self.store.layout_migrated_on_open(),
        }
    }
}
#[derive(Default)]
struct CapturedProgress {
    events: Vec<BatchProgressEvent>,
}

impl BatchProgressSink for CapturedProgress {
    fn emit(&mut self, event: BatchProgressEvent) -> Result<(), AssetError> {
        self.events.push(event);
        Ok(())
    }
}

fn emit_stream_item(
    on_event: &mut dyn FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    index: usize,
    outcome: Result<AcquiredMedia, String>,
) -> Result<(), AcquisitionStreamError> {
    on_event(AcquisitionEvent::ItemStarted { index }).map_err(AcquisitionStreamError::Consumer)?;
    on_event(AcquisitionEvent::ItemCompleted {
        index,
        outcome: Box::new(outcome),
        diagnostics: None,
    })
    .map_err(AcquisitionStreamError::Consumer)
}

fn worker_event(
    worker_id: usize,
    index: usize,
    event: AcquisitionWorkerEventKind,
) -> BatchAcquisitionEvent {
    BatchAcquisitionEvent::Worker(AcquisitionWorkerEvent {
        worker_id,
        index: Some(index),
        generation: Some(0),
        owner_attempt: Some(1),
        session: 1,
        browser_session: Some(worker_id as u64 + 100),
        attempt: 1,
        event,
    })
}

fn identity(value: char) -> AssetIdentity {
    AssetIdentity::new("kanji", value.to_string()).unwrap()
}
fn glyph(value: char) -> Vec<u8> {
    crate::kanji_validator::synthetic_reference_png(value)
}
fn media(character: &str, bytes: Vec<u8>) -> AcquiredMedia {
    // Источник публикует Unicode статьи как шестнадцатеричную кодовую точку, а не как символ.
    let code = character
        .chars()
        .next()
        .map(|ch| format!("{:X}", u32::from(ch)))
        .expect("символ синтетического материала указан");
    let evidence = crate::yarxi::AcquisitionEvidence {
        provider: "synthetic".into(),
        provider_version: "1".into(),
        article_unicode: code.clone(),
        article_number: Some(1),
        frequency_index: None,
        selection: SelectionResult::LeftmostPngFallback,
        target: crate::yarxi::AcquisitionTarget::PreferredSource,
        fallback_absence_proof: Some("synthetic".into()),
        rendered_font_sample: None,
        source_url: "https://example.invalid/synthetic.png".into(),
        browser_runtime: None,
        tls_exception: None,
        acquisition_attempts: 1,
    };
    AcquiredMedia {
        character: character.into(),
        source_url: evidence.source_url.clone(),
        article_unicode: code,
        selection: evidence.selection,
        bytes,
        evidence,
    }
}

fn media_with_codes(character: &str, article_unicode: &str, bytes: Vec<u8>) -> AcquiredMedia {
    let mut acquired = media(character, bytes);
    acquired.article_unicode = article_unicode.into();
    acquired.evidence.article_unicode = article_unicode.into();
    acquired
}

#[test]
fn provider_article_unicode_is_a_hex_code_point_not_the_character() {
    let fixture = Fixture::new();
    fixture.start("identity", &['漢']);
    // Источник публикует Unicode статьи как шестнадцатеричную кодовую точку (U+6F22 -> "6F22").
    // Сравнение с самим символом приводило бы к ошибке при любом реальном получении.
    let (state, _, issues) = run_batch(&fixture.store, "identity", 1, |characters| {
        assert_eq!(characters, ["漢"]);
        Ok(vec![Ok(media_with_codes("漢", "6F22", glyph('漢')))])
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(matches!(
        state.items[0].attempts[0].result,
        BatchAttemptInput::Candidate { .. }
    ));
    assert_eq!(
        state.items[0].current_sha256.as_deref(),
        Some(sha256_hex(glyph('漢')).as_str())
    );

    for wrong in ["漢", "6F23", ""] {
        let fixture = Fixture::new();
        fixture.start("identity", &['漢']);
        let (state, _, _) = run_batch(&fixture.store, "identity", 1, |_| {
            Ok(vec![Ok(media_with_codes("漢", wrong, glyph('漢')))])
        })
        .unwrap();
        assert!(
            matches!(
                &state.items[0].attempts[0].result,
                BatchAttemptInput::Failed { code, .. } if code == "source_identity_mismatch"
            ),
            "article_unicode {wrong:?} должен отвергаться"
        );
        assert_eq!(state.items[0].current_sha256, None);
    }
}

#[test]
fn cli_parser_supports_structured_batch_actions_and_rejects_free_text() {
    let cli = Cli::try_parse_from([
        "kanji-assets",
        "--output",
        "json",
        "batch",
        "decide",
        "--batch-id",
        "fixture",
        "--character",
        "漢",
        "--sha256",
        &"a".repeat(64),
        "--action",
        "confirm",
        "--reason",
        "漢 - подтверждён",
    ])
    .unwrap();
    assert!(matches!(
        cli.command,
        Command::Batch {
            command: BatchCommand::Decide {
                action: BatchActionArg::Confirm,
                ..
            }
        }
    ));
    assert!(
        Cli::try_parse_from([
            "kanji-assets",
            "batch",
            "decide",
            "--batch-id",
            "fixture",
            "--character",
            "漢",
            "--sha256",
            &"a".repeat(64),
            "--action",
            "подтверждён",
            "--reason",
            "explicit"
        ])
        .is_err()
    );
    let default_run =
        Cli::try_parse_from(["kanji-assets", "batch", "run", "--batch-id", "fixture"]).unwrap();
    assert!(default_run.allow_insecure_tls);
    let Command::Batch { command } = &default_run.command else {
        panic!("команда пакета должна разбираться как batch");
    };
    assert!(matches!(
        command,
        BatchCommand::Run {
            rounds: MAX_ACQUISITION_ROUNDS,
            workers: 4,
            ..
        }
    ));
    assert!(prevalidate(command).is_ok());
    assert!(
        Cli::try_parse_from([
            "kanji-assets",
            "batch",
            "run",
            "--batch-id",
            "fixture",
            "--workers",
            "0",
        ])
        .is_err()
    );
    assert!(
        Cli::try_parse_from([
            "kanji-assets",
            "batch",
            "run",
            "--batch-id",
            "fixture",
            "--workers",
            "5",
        ])
        .is_err()
    );

    let explicit_default = Cli::try_parse_from([
        "kanji-assets",
        "--allow-insecure-tls",
        "batch",
        "run",
        "--batch-id",
        "fixture",
    ])
    .unwrap();
    assert!(explicit_default.allow_insecure_tls);
    let strict_tls = Cli::try_parse_from([
        "kanji-assets",
        "--allow-insecure-tls=false",
        "batch",
        "run",
        "--batch-id",
        "fixture",
    ])
    .unwrap();
    assert!(!strict_tls.allow_insecure_tls);

    #[cfg(feature = "session-rotation-acceptance")]
    {
        let forced = Cli::try_parse_from([
            "kanji-assets",
            "batch",
            "run",
            "--batch-id",
            "fixture",
            "--acceptance-rotate-after-items",
            "2",
        ])
        .unwrap();
        assert!(matches!(
            forced.command,
            Command::Batch {
                command: BatchCommand::Run {
                    acceptance_rotate_after_items: Some(limit),
                    ..
                }
            } if limit.get() == 2
        ));
        assert!(
            Cli::try_parse_from([
                "kanji-assets",
                "batch",
                "run",
                "--batch-id",
                "fixture",
                "--acceptance-rotate-after-items",
                "0",
            ])
            .is_err()
        );
    }

    #[cfg(not(feature = "session-rotation-acceptance"))]
    assert!(
        Cli::try_parse_from([
            "kanji-assets",
            "batch",
            "run",
            "--batch-id",
            "fixture",
            "--acceptance-rotate-after-items",
            "2",
        ])
        .is_err()
    );

    let too_many = Cli::try_parse_from([
        "kanji-assets",
        "batch",
        "run",
        "--batch-id",
        "fixture",
        "--rounds",
        &(MAX_ACQUISITION_ROUNDS + 1).to_string(),
    ])
    .unwrap();
    let Command::Batch { command } = &too_many.command else {
        panic!("команда пакета должна разбираться как batch");
    };
    assert!(prevalidate(command).is_err());
}

#[test]
fn human_reason_uses_one_byte_limit_at_cli_boundary() {
    assert!(validate_reason(&"r".repeat(MAX_HUMAN_REASON_BYTES)).is_ok());
    assert!(validate_reason(&"r".repeat(MAX_HUMAN_REASON_BYTES + 1)).is_err());
    assert!(validate_reason(" \n\t ").is_err());
}

#[test]
fn domain_round_limit_rejects_overflow_before_acquisition() {
    let fixture = Fixture::new();
    fixture.start("round-limit", &['漢']);
    let mut called = false;
    let result = run_batch(
        &fixture.store,
        "round-limit",
        MAX_ACQUISITION_ROUNDS + 1,
        |_| {
            called = true;
            Ok(Vec::new())
        },
    );
    assert!(result.is_err());
    assert!(!called);
}

#[test]
fn human_cli_output_uses_russian_labels() {
    let fixture = Fixture::new();
    let output = execute(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Start {
            batch_id: Some("localized".into()),
            characters: vec!["漢".into()],
        },
        OutputFormat::Human,
        false,
    );
    assert_eq!(output.exit_code, 0);
    assert!(
        output
            .stdout
            .contains("Операция: создание пакета; результат: создан")
    );
    assert!(
        output
            .stdout
            .contains("попыток_получения=0 разных_кандидатов_SHA-256=0")
    );
    assert!(!output.stdout.contains("attempts="));
    assert!(!output.stdout.contains("review:"));
}

#[test]
fn ocr_and_match_confirmation_localize_human_output_without_changing_json_ids() {
    let fixture = Fixture::new();
    let state = KanjiBatch::new(
        "localized-ocr".into(),
        vec![identity('漢')],
        KanjiImageValidator::validator_identity(),
    )
    .unwrap();

    let make_ocr_response = || {
        let mut response = batch_response(
            "batch_ocr",
            fixture.summary(),
            state.clone(),
            false,
            Vec::new(),
            None,
            "ocr_suggestions_saved",
        );
        response.review_id = Some("a".repeat(64));
        let report = crate::kanji_ocr::OcrScanReport {
            saved: 1,
            chat_requests: 1,
            ..Default::default()
        };
        response.ocr_summary = Some(report);
        response
    };

    let human = render_batch_response(make_ocr_response(), OutputFormat::Human, 0, None);
    assert!(
        human
            .stdout
            .contains("Операция: OCR сохранённых кандидатов; результат: предложения OCR сохранены")
    );
    assert!(human.stdout.contains("Идентификатор снимка проверки: "));
    assert!(!human.stdout.contains("Review ID:"));

    let json = render_batch_response(make_ocr_response(), OutputFormat::Json, 0, None);
    let value: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(value["operation"], "batch_ocr");
    assert_eq!(value["outcome"], "ocr_suggestions_saved");
    assert_eq!(value["review_id"], "a".repeat(64));
    assert_eq!(value["ocr_summary"]["saved"], 1);
    assert_eq!(value["ocr_summary"]["chat_requests"], 1);

    let make_confirm_response = || {
        let mut response = batch_response(
            "batch_review_confirm_matches",
            fixture.summary(),
            state.clone(),
            false,
            Vec::new(),
            None,
            "ok",
        );
        response.review_id = Some("b".repeat(64));
        response
    };
    let human = render_batch_response(make_confirm_response(), OutputFormat::Human, 0, None);
    assert!(
        human
            .stdout
            .contains("Операция: подтверждение просмотренных совпадений OCR; результат: успешно")
    );
    assert!(human.stdout.contains("Идентификатор снимка проверки: "));
    let json = render_batch_response(make_confirm_response(), OutputFormat::Json, 0, None);
    let value: serde_json::Value = serde_json::from_str(&json.stdout).unwrap();
    assert_eq!(value["operation"], "batch_review_confirm_matches");
    assert_eq!(value["outcome"], "ok");
    assert_eq!(value["review_id"], "b".repeat(64));
}

#[test]
fn start_status_review_are_json_and_idempotent() {
    let fixture = Fixture::new();
    fixture.start("status", &['漢']);
    let command = BatchCommand::Status {
        batch_id: "status".into(),
    };
    let result = execute(
        &fixture.store,
        fixture.summary(),
        &command,
        OutputFormat::Json,
        false,
    );
    assert_eq!(result.exit_code, 0);
    let value: serde_json::Value = serde_json::from_str(&result.stdout).unwrap();
    assert_eq!(value["batch_id"], "status");
    assert_eq!(value["counts"]["requested"], 1);
    assert_eq!(value["items"][0]["item_outcome"], "unresolved");
    assert!(
        value["batch"]["items"][0]["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    fixture.start("status", &['漢']);
    assert!(
        execute_command(
            &fixture.store,
            fixture.summary(),
            &BatchCommand::Start {
                batch_id: Some("status".into()),
                characters: vec!["字".into()]
            },
            false
        )
        .is_err()
    );
    let review = execute(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Review {
            batch_id: "status".into(),
        },
        OutputFormat::Json,
        false,
    );
    assert_eq!(review.exit_code, 0);
    let value: serde_json::Value = serde_json::from_str(&review.stdout).unwrap();
    assert!(PathBuf::from(value["review_artifact"].as_str().unwrap()).is_file());
}

#[test]
fn acquisition_releases_runtime_lock_and_finishes_all_items_before_retry() {
    let fixture = Fixture::new();
    fixture.start("breadth", &['元', '漢', '字']);
    let mut requests = Vec::new();
    let (state, _, _) = run_batch(&fixture.store, "breadth", 2, |characters| {
        // Повторное открытие той же flock во время вызова источника подтверждает,
        // что блокировка снята.
        let _probe = BatchRuntime::open(fixture.store.root(), "breadth").unwrap();
        requests.push(characters.to_vec());
        Ok(characters
            .iter()
            .map(|value| {
                if value == "元" {
                    Ok(media(value, glyph('元')))
                } else {
                    Err("синтетическая ошибка сети".into())
                }
            })
            .collect())
    })
    .unwrap();
    assert_eq!(requests[0], ["元", "漢", "字"]);
    assert_eq!(requests[1], ["漢", "字"]);
    assert!(state.items[0].is_ready());
    assert_eq!(state.items[0].attempts.len(), 1);
    assert_eq!(state.items[1].attempts.len(), 2);
    assert_eq!(state.items[2].attempts.len(), 2);
    assert_eq!(
        state.items[0].attempts[0].result.clone(),
        fixture.load("breadth").items[0].attempts[0].result
    );
    let record = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(record[0].bytes, glyph('元'));
    assert!(
        record[0].record.domain_metadata.as_ref().unwrap()["yarxi"]["source_url"]
            .as_str()
            .unwrap()
            .contains("example.invalid")
    );
}

#[test]
fn completed_identity_is_durable_before_provider_returns_and_resume_skips_it() {
    let fixture = Fixture::new();
    fixture.start("partial-checkpoint", &['元', '漢', '字']);
    let mut progress = CapturedProgress::default();
    let first = run_batch_with_stream_and_progress(
        &fixture.store,
        "partial-checkpoint",
        1,
        |characters, _generations, on_event| {
            assert_eq!(characters, ["元", "漢", "字"]);
            // Во время ожидания поставщика другой владелец может открыть среду пакета.
            let probe = BatchRuntime::open(fixture.store.root(), "partial-checkpoint").unwrap();
            drop(probe);
            on_event(AcquisitionEvent::SessionStarted { session: 1 })
                .map_err(AcquisitionStreamError::Consumer)?;
            on_event(AcquisitionEvent::ItemStarted { index: 0 })
                .map_err(AcquisitionStreamError::Consumer)?;
            on_event(AcquisitionEvent::Heartbeat {
                index: 0,
                attempt: 1,
            })
            .map_err(AcquisitionStreamError::Consumer)?;
            on_event(AcquisitionEvent::ItemCompleted {
                index: 0,
                outcome: Box::new(Ok(media("元", glyph('元')))),
                diagnostics: None,
            })
            .map_err(AcquisitionStreamError::Consumer)?;

            // Обработчик элемента вернулся только после атомарного сохранения.
            let checkpoint = fixture.load("partial-checkpoint");
            assert_eq!(checkpoint.items[0].attempts.len(), 1);
            let BatchAttemptInput::Candidate { candidate } =
                &checkpoint.items[0].attempts[0].result
            else {
                panic!("полученный кандидат должен быть сохранён до продолжения поставщика")
            };
            let runtime = BatchRuntime::open(fixture.store.root(), "partial-checkpoint").unwrap();
            assert_eq!(runtime.read_candidate(candidate).unwrap(), glyph('元'));
            for item in &checkpoint.items[1..] {
                assert!(item.attempts.is_empty());
                assert_eq!(item.generation, 0);
                assert_eq!(item.status, BatchItemStatus::Unresolved);
            }
            Err(AcquisitionStreamError::Provider(
                "синтетическое прерывание после первого сохранённого результата".into(),
            ))
        },
        &mut StoreSnapshotReader,
        &mut progress,
    );
    assert!(first.is_err());
    assert!(progress.events.iter().any(|event| {
        event.event == "item_checkpointed"
            && event
                .identity
                .as_ref()
                .is_some_and(|identity| identity.key == "元")
            && event.outcome.as_deref() == Some("candidate_recorded")
    }));
    let heartbeat = progress
        .events
        .iter()
        .find(|event| event.event == "heartbeat")
        .unwrap();
    let checkpoint_event = progress
        .events
        .iter()
        .find(|event| event.event == "item_checkpointed")
        .unwrap();
    assert_eq!(heartbeat.identity.as_ref().unwrap().key, "元");
    assert!(heartbeat.elapsed_ms <= checkpoint_event.elapsed_ms);
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.event == "run_stopped")
    );
    assert_eq!(
        fixture.load("partial-checkpoint").items[0].attempts.len(),
        1
    );

    let mut resumed_requests = Vec::new();
    let mut resumed_progress = CapturedProgress::default();
    let (state, _, _) = run_batch_with_stream_and_progress(
        &fixture.store,
        "partial-checkpoint",
        1,
        |characters, _generations, on_event| {
            resumed_requests.push(characters.to_vec());
            assert_eq!(characters, ["漢", "字"]);
            for (index, character) in characters.iter().enumerate() {
                emit_stream_item(
                    on_event,
                    index,
                    Ok(media(character, glyph(character.chars().next().unwrap()))),
                )?;
            }
            Ok(())
        },
        &mut StoreSnapshotReader,
        &mut resumed_progress,
    )
    .unwrap();
    assert_eq!(resumed_requests, [vec!["漢".to_owned(), "字".to_owned()]]);
    assert!(state.is_resolved());
    assert!(state.items.iter().all(|item| item.attempts.len() == 1));
    assert!(resumed_progress.events.iter().any(|event| {
        event.event == "run_finished" && event.outcome.as_deref() == Some("resolved")
    }));
}

#[test]
fn owner_generation_is_passed_to_the_acquisition_stream() {
    let fixture = Fixture::new();
    fixture.start("generation-context", &['漢']);
    {
        let mut runtime = BatchRuntime::open(fixture.store.root(), "generation-context").unwrap();
        let mut state = runtime.load().unwrap().unwrap();
        for _ in 0..MAX_ACQUISITION_ROUNDS {
            state
                .record_attempt(
                    &identity('漢'),
                    BatchAttemptInput::Failed {
                        code: "network".into(),
                        message: "источник временно недоступен".into(),
                    },
                )
                .unwrap();
        }
        assert_eq!(state.items[0].status, BatchItemStatus::AwaitingHuman);
        state
            .retry_acquisition(&identity('漢'), "повтор после временных отказов".into())
            .unwrap();
        assert_eq!(state.items[0].generation, 1);
        runtime.save(&state).unwrap();
    }

    let mut observed_generations = Vec::new();
    let mut progress = CapturedProgress::default();
    let result = run_batch_with_stream_and_progress(
        &fixture.store,
        "generation-context",
        1,
        |characters, generations, on_event| {
            assert_eq!(characters, ["漢"]);
            observed_generations.extend_from_slice(generations);
            emit_stream_item(on_event, 0, Err("синтетическая ошибка получения".into()))
        },
        &mut StoreSnapshotReader,
        &mut progress,
    );

    assert!(result.is_ok());
    assert_eq!(observed_generations, [1]);
}

#[test]
fn parallel_frontier_checkpoints_out_of_order_before_worker_reassignment() {
    let fixture = Fixture::new();
    fixture.start("parallel-order", &['元', '漢', '字']);
    let mut progress = CapturedProgress::default();
    let (state, _, _) = run_batch_with_worker_stream_and_progress(
        &fixture.store,
        "parallel-order",
        1,
        2,
        |characters, generations, owner_attempts, workers, on_event| {
            assert_eq!(characters, ["元", "漢", "字"]);
            assert_eq!(generations, [0, 0, 0]);
            assert_eq!(owner_attempts, [1, 1, 1]);
            assert_eq!(workers, 2);
            for (worker, index) in [(1, 0), (2, 1)] {
                on_event(worker_event(
                    worker,
                    index,
                    AcquisitionWorkerEventKind::Assigned,
                ))
                .map_err(AcquisitionStreamError::Consumer)?;
                on_event(worker_event(
                    worker,
                    index,
                    AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemStarted { index }),
                ))
                .map_err(AcquisitionStreamError::Consumer)?;
            }
            on_event(worker_event(
                2,
                1,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                    index: 1,
                    outcome: Box::new(Err("синтетическая ошибка 漢".into())),
                    diagnostics: None,
                }),
            ))
            .map_err(AcquisitionStreamError::Consumer)?;
            let checkpoint = fixture.load("parallel-order");
            assert!(checkpoint.items[0].attempts.is_empty());
            assert_eq!(checkpoint.items[1].attempts.len(), 1);
            assert!(checkpoint.items[2].attempts.is_empty());

            // Обработчик 2 получает новый элемент только после возврата callback
            // выше, то есть после синхронного durable checkpoint элемента 漢.
            on_event(worker_event(2, 2, AcquisitionWorkerEventKind::Assigned))
                .map_err(AcquisitionStreamError::Consumer)?;
            on_event(worker_event(
                2,
                2,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemStarted { index: 2 }),
            ))
            .map_err(AcquisitionStreamError::Consumer)?;
            on_event(worker_event(
                2,
                2,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                    index: 2,
                    outcome: Box::new(Err("синтетическая ошибка 字".into())),
                    diagnostics: None,
                }),
            ))
            .map_err(AcquisitionStreamError::Consumer)?;
            on_event(worker_event(
                1,
                0,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                    index: 0,
                    outcome: Box::new(Err("синтетическая ошибка 元".into())),
                    diagnostics: None,
                }),
            ))
            .map_err(AcquisitionStreamError::Consumer)?;
            Ok(())
        },
        &mut StoreSnapshotReader,
        &mut progress,
    )
    .unwrap();

    assert!(state.items.iter().all(|item| item.attempts.len() == 1));
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_checkpointed")
            .map(|event| event.identity.as_ref().unwrap().key.as_str())
            .collect::<Vec<_>>(),
        ["漢", "字", "元"]
    );
    for item in &state.items {
        let BatchAttemptInput::Failed { message, .. } = &item.attempts[0].result else {
            panic!("ожидалась сохранённая ошибка именно этого символа");
        };
        assert!(
            message.contains(&item.identity.key),
            "{}: {message}",
            item.identity.key
        );
    }
    assert_eq!(
        progress.events.iter().map(|event| event.in_flight).max(),
        Some(2)
    );
    let first = progress
        .events
        .iter()
        .find(|event| event.event == "item_checkpointed")
        .unwrap();
    assert_eq!(first.worker_id, Some(2));
    assert_eq!(first.generation, Some(0));
    assert_eq!(first.owner_attempt, Some(1));
    assert_eq!(first.in_flight, 1);
}

#[test]
fn parallel_frontier_rejects_duplicate_terminal_and_incomplete_success() {
    let fixture = Fixture::new();
    fixture.start("parallel-protocol", &['元', '漢']);
    let mut progress = CapturedProgress::default();
    let duplicate = run_batch_with_worker_stream_and_progress(
        &fixture.store,
        "parallel-protocol",
        3,
        2,
        |_characters, _generations, _owner_attempts, _workers, on_event| {
            on_event(worker_event(1, 0, AcquisitionWorkerEventKind::Assigned))
                .map_err(AcquisitionStreamError::Consumer)?;
            on_event(worker_event(
                1,
                0,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemStarted { index: 0 }),
            ))
            .map_err(AcquisitionStreamError::Consumer)?;
            let completed = worker_event(
                1,
                0,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                    index: 0,
                    outcome: Box::new(Err("синтетическая ошибка 元".into())),
                    diagnostics: None,
                }),
            );
            on_event(completed).map_err(AcquisitionStreamError::Consumer)?;
            on_event(worker_event(
                1,
                0,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                    index: 0,
                    outcome: Box::new(Err("дубликат".into())),
                    diagnostics: None,
                }),
            ))
            .map_err(AcquisitionStreamError::Consumer)
        },
        &mut StoreSnapshotReader,
        &mut progress,
    );
    assert!(duplicate.is_err());
    assert_eq!(fixture.load("parallel-protocol").items[0].attempts.len(), 1);
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "round_started")
            .count(),
        1
    );

    let fixture = Fixture::new();
    fixture.start("parallel-incomplete", &['元', '漢']);
    let mut progress = CapturedProgress::default();
    let incomplete = run_batch_with_worker_stream_and_progress(
        &fixture.store,
        "parallel-incomplete",
        3,
        2,
        |_characters, _generations, _owner_attempts, _workers, on_event| {
            on_event(worker_event(1, 0, AcquisitionWorkerEventKind::Assigned))
                .map_err(AcquisitionStreamError::Consumer)?;
            on_event(worker_event(
                1,
                0,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemStarted { index: 0 }),
            ))
            .map_err(AcquisitionStreamError::Consumer)?;
            on_event(worker_event(
                1,
                0,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                    index: 0,
                    outcome: Box::new(Err("синтетическая ошибка 元".into())),
                    diagnostics: None,
                }),
            ))
            .map_err(AcquisitionStreamError::Consumer)
        },
        &mut StoreSnapshotReader,
        &mut progress,
    );
    assert!(
        incomplete
            .unwrap_err()
            .message
            .starts_with("batch_frontier_incomplete:")
    );
    let state = fixture.load("parallel-incomplete");
    assert_eq!(state.items[0].attempts.len(), 1);
    assert!(state.items[1].attempts.is_empty());
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "round_started")
            .count(),
        1,
        "следующий раунд нельзя начать до сохранения результата всей границы обхода"
    );
}

#[test]
fn interrupted_frontier_tail_stays_pending_and_retry_fairness_is_preserved() {
    let fixture = Fixture::new();
    fixture.start("frontier-tail", &['元', '漢', '字']);
    let interrupted = run_batch_with_stream_and_progress(
        &fixture.store,
        "frontier-tail",
        1,
        |_characters, _generations, on_event| {
            emit_stream_item(on_event, 0, Ok(media("元", glyph('元'))))?;
            Err(AcquisitionStreamError::Provider(
                "синтетическая остановка сессии".into(),
            ))
        },
        &mut StoreSnapshotReader,
        &mut CapturedProgress::default(),
    );
    assert!(interrupted.is_err());

    let state = fixture.load("frontier-tail");
    assert_eq!(state.items[0].attempts.len(), 1);
    for item in &state.items[1..] {
        assert!(item.attempts.is_empty());
        assert_eq!(item.generation, 0);
        assert_eq!(item.status, BatchItemStatus::Unresolved);
        assert!(item.aggregate.distinct_valid_hashes.is_empty());
    }
    assert_eq!(state.next_round(), [identity('漢'), identity('字')]);

    let mut resumed_requests = Vec::new();
    let (state, _, _) = run_batch_with_stream_and_progress(
        &fixture.store,
        "frontier-tail",
        1,
        |characters, _generations, on_event| {
            resumed_requests.push(characters.to_vec());
            assert_eq!(characters, ["漢", "字"]);
            emit_stream_item(on_event, 0, Err("ошибка получения символа 漢".into()))?;
            assert_eq!(fixture.load("frontier-tail").next_round(), [identity('字')]);
            emit_stream_item(on_event, 1, Err("ошибка получения символа 字".into()))?;
            assert_eq!(
                fixture.load("frontier-tail").next_round(),
                [identity('漢'), identity('字')],
                "вторая попытка соседей разрешена только после первой попытки всей очереди раунда"
            );
            Ok(())
        },
        &mut StoreSnapshotReader,
        &mut CapturedProgress::default(),
    )
    .unwrap();
    assert_eq!(resumed_requests, [vec!["漢".to_owned(), "字".to_owned()]]);
    assert_eq!(state.items[0].attempts.len(), 1);
    assert_eq!(state.items[1].attempts.len(), 1);
    assert_eq!(state.items[2].attempts.len(), 1);
    assert_eq!(state.next_round(), [identity('漢'), identity('字')]);
}

#[test]
fn partial_checkpoint_error_response_reports_durable_change_and_saved_tail() {
    for interrupted in [true, false] {
        let mut fixture = Fixture::new();
        let batch_id = "partial-error-response";
        fixture.start(batch_id, &['元', '漢', '字']);
        fixture.store = AssetStore::open_kanji(StoreOptions::new(fixture.store.root())).unwrap();
        assert!(!fixture.store.did_mutate_on_open());
        let initial_revision = fixture.load(batch_id).revision;
        let error = run_batch_with_stream_and_progress(
            &fixture.store,
            batch_id,
            2,
            |_characters, _generations, on_event| {
                emit_stream_item(
                    on_event,
                    0,
                    Err("сохранённая ошибка первого символа".into()),
                )?;
                if interrupted {
                    Err(AcquisitionStreamError::Interrupted)
                } else {
                    Err(AcquisitionStreamError::Provider(
                        "сессия браузера недоступна".into(),
                    ))
                }
            },
            &mut StoreSnapshotReader,
            &mut CapturedProgress::default(),
        )
        .unwrap_err();
        let final_output = render_batch_error(
            &fixture.store,
            fixture.summary(),
            OutputFormat::Json,
            BatchRunFailure {
                batch_id,
                operation: "batch_run",
                error,
                initial_revision: Some(initial_revision),
                diagnostic_log: None,
            },
        );
        assert!(final_output.stderr.is_empty());
        let response: serde_json::Value = serde_json::from_str(&final_output.stdout).unwrap();
        assert_eq!(response["batch_id"], batch_id);
        assert_eq!(response["changed"], true);
        assert_eq!(response["items"][0]["acquisition_attempts"], 1);
        assert_eq!(
            response["batch"]["items"][0]["attempts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            response["batch"]["revision"],
            fixture.load(batch_id).revision
        );
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap()
                .contains(if interrupted {
                    "Ctrl+C"
                } else {
                    "сессия браузера недоступна"
                })
        );
        for index in 1..3 {
            assert_eq!(response["items"][index]["acquisition_attempts"], 0);
            assert!(
                response["batch"]["items"][index]["attempts"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(fixture.load(batch_id).items[index].generation, 0);
        }
    }
}

#[test]
fn batch_error_without_durable_mutation_reports_unchanged_existing_state() {
    let mut fixture = Fixture::new();
    fixture.start("unchanged-error", &['漢']);
    fixture.store = AssetStore::open_kanji(StoreOptions::new(fixture.store.root())).unwrap();
    assert!(!fixture.store.did_mutate_on_open());
    let revision = fixture.load("unchanged-error").revision;
    let final_output = render_batch_error(
        &fixture.store,
        fixture.summary(),
        OutputFormat::Json,
        BatchRunFailure {
            batch_id: "unchanged-error",
            operation: "batch_run",
            error: AssetError::new(ErrorCode::IoFailure, "ошибка запуска браузера"),
            initial_revision: Some(revision),
            diagnostic_log: None,
        },
    );
    let response: serde_json::Value = serde_json::from_str(&final_output.stdout).unwrap();
    assert_eq!(response["changed"], false);
    assert_eq!(response["batch_id"], "unchanged-error");
    assert_eq!(response["batch"]["revision"], revision);
    assert_eq!(response["items"][0]["acquisition_attempts"], 0);
}

#[test]
fn interruption_diagnostics_link_session_attempt_and_durable_checkpoint_without_progress_changes() {
    // Поставщик принимает отмену до элемента, во время него и после синхронного
    // обратного вызова завершения. Только последний случай имеет надёжную контрольную точку.
    for boundary in 0..3 {
        let fixture = Fixture::new();
        let batch_id = "interruption-evidence";
        fixture.start(batch_id, &['漢', '字']);
        let log_path = fixture.directory().join("interruption.jsonl");
        let guard = RunLogGuard::new(
            fs::File::create(&log_path).unwrap(),
            DiagnosticOutputMode::Json,
        );
        let mut progress = CapturedProgress::default();
        let error = guard
            .with_default(|| {
                run_batch_with_stream_and_progress(
                    &fixture.store,
                    batch_id,
                    2,
                    |_characters, _generations, on_event| {
                        on_event(AcquisitionEvent::SessionStarted { session: 7 })
                            .map_err(AcquisitionStreamError::Consumer)?;
                        if boundary > 0 {
                            on_event(AcquisitionEvent::ItemStarted { index: 0 })
                                .map_err(AcquisitionStreamError::Consumer)?;
                            on_event(AcquisitionEvent::RetryStarted {
                                index: 0,
                                attempt: 2,
                            })
                            .map_err(AcquisitionStreamError::Consumer)?;
                        }
                        if boundary == 2 {
                            on_event(AcquisitionEvent::ItemCompleted {
                                index: 0,
                                outcome: Box::new(Err("сохранённый отказ поставщика".into())),
                                diagnostics: None,
                            })
                            .map_err(AcquisitionStreamError::Consumer)?;
                            assert_eq!(fixture.load(batch_id).items[0].attempts.len(), 1);
                        }
                        Err(AcquisitionStreamError::Interrupted)
                    },
                    &mut StoreSnapshotReader,
                    &mut progress,
                )
            })
            .unwrap_err();
        assert!(error.message.contains("batch_interrupted"));
        guard.finish().unwrap();
        let events = fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let stop = &events
            .iter()
            .find(|event| {
                event["fields"]["diagnostic_schema"] == "kanji_batch_progress_v1"
                    && event["fields"]["stage"] == "run_stopped"
            })
            .unwrap()["fields"];
        assert_eq!(stop["provider"], "yarxi");
        assert_eq!(stop["domain"], "kanji");
        assert_eq!(stop["browser_session"], 7);
        assert_eq!(stop["run_stop_reason"], "interrupted");
        assert_eq!(stop["checkpointed_items"], usize::from(boundary == 2));
        assert_eq!(stop["item_pending_checkpoint"], boundary == 1);
        let saved = fixture.load(batch_id);
        assert_eq!(saved.items[0].attempts.len(), usize::from(boundary == 2));
        assert!(saved.items[1].attempts.is_empty());
        assert_eq!(saved.items[1].generation, 0);
        if boundary > 0 {
            assert_eq!(stop["identity"], "漢");
            assert_eq!(stop["generation"], 0);
            assert_eq!(stop["owner_attempt"], 1);
            assert_eq!(stop["acquisition_attempt"], 2);
            let started = progress
                .events
                .iter()
                .find(|event| event.event == "item_started")
                .unwrap();
            assert!(started.session.is_none());
            assert!(started.attempt.is_none());
            let public = serde_json::to_value(started).unwrap();
            assert!(public.get("diagnostic_schema").is_none());
            assert!(public.get("browser_session").is_none());
            assert!(public.get("last_checkpoint_revision").is_none());
        } else {
            assert!(stop.get("identity").is_none());
            assert!(stop.get("acquisition_attempt").is_none());
        }
        if boundary == 2 {
            assert_eq!(stop["last_checkpointed_identity"], "漢");
            assert_eq!(stop["last_checkpoint_revision"], saved.revision);
            assert_eq!(saved.next_round(), [identity('字')]);
        } else {
            assert!(stop.get("last_checkpointed_identity").is_none());
            assert!(stop.get("last_checkpoint_revision").is_none());
            assert_eq!(saved.next_round(), [identity('漢'), identity('字')]);
        }
        assert_eq!(
            progress
                .events
                .iter()
                .filter(|event| event.event == "round_started")
                .count(),
            1
        );
        assert!(
            !progress
                .events
                .iter()
                .any(|event| event.event == "run_finished")
        );
    }
}

#[test]
fn signal_accepted_in_last_checkpoint_stops_before_second_round_acquisition() {
    use std::sync::atomic::AtomicBool;
    struct StopAtCheckpoint<'a> {
        stopped: &'a AtomicBool,
        events: Vec<BatchProgressEvent>,
    }
    impl BatchProgressSink for StopAtCheckpoint<'_> {
        fn emit(&mut self, event: BatchProgressEvent) -> Result<(), AssetError> {
            if event.event == "item_checkpointed" {
                self.stopped.store(true, Ordering::SeqCst);
            }
            self.events.push(event);
            Ok(())
        }
    }
    let fixture = Fixture::new();
    fixture.start("stop-round-boundary", &['漢']);
    let stopped = AtomicBool::new(false);
    let mut progress = StopAtCheckpoint {
        stopped: &stopped,
        events: Vec::new(),
    };
    let mut acquisitions = 0;
    let error = run_batch_with_stream_and_progress(
        &fixture.store,
        "stop-round-boundary",
        2,
        |characters, _generations, on_event| {
            acquisitions += 1;
            assert_eq!(characters, ["漢"]);
            emit_stream_item(on_event, 0, Err("ошибка получения первого раунда".into()))?;
            // Сигнал принят из синхронного обратного вызова после надёжного сохранения.
            // Поставщик обязан проверить тот же флаг остановки до успешного возврата.
            assert_eq!(
                fixture.load("stop-round-boundary").items[0].attempts.len(),
                1
            );
            assert!(stopped.load(Ordering::SeqCst));
            Err(AcquisitionStreamError::Interrupted)
        },
        &mut StoreSnapshotReader,
        &mut progress,
    )
    .unwrap_err();
    assert!(error.message.contains("batch_interrupted"));
    assert_eq!(acquisitions, 1);
    let saved = fixture.load("stop-round-boundary");
    assert_eq!(saved.items[0].attempts.len(), 1);
    assert_eq!(saved.next_round(), [identity('漢')]);
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "round_started")
            .count(),
        1
    );
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.event == "run_stopped")
    );
    assert!(
        !progress
            .events
            .iter()
            .any(|event| event.event == "run_finished")
    );
}

#[test]
fn human_progress_localizes_event_and_outcome_without_changing_machine_values() {
    let event = BatchProgressEvent {
        schema_version: 1,
        operation: "batch_run",
        event: "browser_session_started",
        batch_id: "localized-progress".into(),
        elapsed_ms: 100,
        round: Some(1),
        round_limit: 2,
        round_completed: 0,
        round_total: 1,
        run_completed: 0,
        batch_total: 1,
        workers: 1,
        in_flight: 0,
        identity: None,
        session: Some(1),
        worker_id: None,
        browser_session_id: None,
        attempt: None,
        generation: None,
        owner_attempt: None,
        item_elapsed_ms: None,
        stage_timings: None,
        outcome: Some("candidate_recorded".into()),
        reason: None,
    };
    let mut human = Vec::new();
    write_progress_event(&event, OutputFormat::Human, &mut human).unwrap();
    let human = String::from_utf8(human).unwrap();
    assert!(human.contains("сессия браузера запущена"));
    assert!(human.contains("кандидат сохранён"));
    for machine in ["browser_session_started", "candidate_recorded", "identity="] {
        assert!(!human.contains(machine));
    }
    let mut machine = Vec::new();
    write_progress_event(&event, OutputFormat::Json, &mut machine).unwrap();
    let machine: serde_json::Value = serde_json::from_slice(&machine).unwrap();
    assert_eq!(machine["event"], "browser_session_started");
    assert_eq!(machine["outcome"], "candidate_recorded");
}

#[test]
fn progress_jsonl_is_separate_from_the_single_final_json_response() {
    let fixture = Fixture::new();
    fixture.start("progress-contract", &['漢']);
    let event = BatchProgressEvent {
        schema_version: 1,
        operation: "batch_run",
        event: "item_checkpointed",
        batch_id: "progress-contract".into(),
        elapsed_ms: 1250,
        round: Some(1),
        round_limit: 5,
        round_completed: 1,
        round_total: 1,
        run_completed: 1,
        batch_total: 1,
        workers: 1,
        in_flight: 0,
        identity: Some(identity('漢')),
        session: Some(1),
        worker_id: None,
        browser_session_id: None,
        attempt: None,
        generation: None,
        owner_attempt: None,
        item_elapsed_ms: None,
        stage_timings: None,
        outcome: Some("acquisition_failed".into()),
        reason: None,
    };
    let mut stderr = Vec::new();
    write_progress_event(&event, OutputFormat::Json, &mut stderr).unwrap();
    let progress_line = std::str::from_utf8(&stderr).unwrap();
    assert_eq!(progress_line.lines().count(), 1);
    let progress_json: serde_json::Value = serde_json::from_str(progress_line).unwrap();
    assert_eq!(progress_json["event"], "item_checkpointed");
    assert_eq!(progress_json["round_completed"], 1);
    assert_eq!(progress_json["identity"]["key"], "漢");

    let final_output = execute(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Status {
            batch_id: "progress-contract".into(),
        },
        OutputFormat::Json,
        false,
    );
    assert!(final_output.stderr.is_empty());
    let final_json: serde_json::Value = serde_json::from_str(&final_output.stdout).unwrap();
    assert_eq!(final_json["operation"], "batch_status");
    assert_eq!(final_json["batch_id"], "progress-contract");
}

#[test]
fn reuse_trusted_assets_and_publication_resume_do_not_acquire_again() {
    let fixture = Fixture::new();
    fixture.start("original", &['元']);
    let (state, _, _) = run_batch(&fixture.store, "original", 1, |characters| {
        Ok(characters
            .iter()
            .map(|value| Ok(media(value, glyph('元'))))
            .collect())
    })
    .unwrap();
    assert!(state.is_resolved());
    fixture.start("reuse", &['元']);
    let (state, changed, _) = run_batch(&fixture.store, "reuse", 5, |_| {
        panic!("для доверенных байтов получение не запускается")
    })
    .unwrap();
    assert!(!changed);
    assert!(state.is_resolved());
    assert_eq!(state.items[0].status, BatchItemStatus::ExistingVerified);
    // Имитируем канонический коммит до отметки и сохранения состояния.
    let mut runtime = BatchRuntime::open(fixture.store.root(), "original").unwrap();
    let mut before_mark = runtime.load().unwrap().unwrap();
    before_mark.items[0].published_sha256 = None;
    before_mark.items[0].publication_source = None;
    before_mark.revision += 1;
    runtime.save(&before_mark).unwrap();
    drop(runtime);
    let (state, _, _) = run_batch(&fixture.store, "original", 5, |_| {
        panic!("возобновление публикации не должно запускать получение")
    })
    .unwrap();
    assert!(state.is_resolved());
}

#[test]
fn human_confirm_reject_and_targeted_reacquire_cross_owner_boundaries() {
    let fixture = Fixture::new();
    fixture.start("review", &['元', '漢', '字']);
    let (state, _, _) = run_batch(&fixture.store, "review", 5, |characters| {
        Ok(characters
            .iter()
            .map(|value| Ok(media(value, glyph('元'))))
            .collect())
    })
    .unwrap();
    assert!(state.items[0].is_ready());
    assert_eq!(state.review_queue().len(), 2);
    // Кандидат REJECTED показывает, что другой эталон Unicode ближе, поэтому он
    // не считается независимым пригодным образцом этого символа.
    assert!(state.items[1].aggregate.distinct_valid_hashes.is_empty());
    assert_eq!(state.items[1].attempts.len(), 1);
    let confirm_hash = state.items[1].current_sha256.clone().unwrap();
    let (state, issues) = decide_exact(
        &fixture.store,
        "review",
        HumanBatchDecision {
            identity: identity('漢'),
            candidate_sha256: confirm_hash.clone(),
            action: HumanBatchAction::Confirm,
            reason: "синтетическое явное подтверждение человека".into(),
        },
    )
    .unwrap();
    assert!(issues.is_empty());
    assert!(state.items[1].is_ready());
    let human = read_kanji_verified(
        fixture.store.root(),
        &[identity('漢')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(
        human[0].record.current_human_decision(),
        Some(HumanDecision::Approve)
    );
    assert_ne!(
        human[0].record.validation.as_ref().unwrap().status,
        SemanticStatus::Verified
    );
    let reject_hash = state.items[2].current_sha256.clone().unwrap();
    let (state, issues) = decide_exact(
        &fixture.store,
        "review",
        HumanBatchDecision {
            identity: identity('字'),
            candidate_sha256: reject_hash.clone(),
            action: HumanBatchAction::Reject,
            reason: "синтетический повреждённый артефакт".into(),
        },
    )
    .unwrap();
    assert!(issues.is_empty());
    assert_eq!(state.next_round(), [identity('字')]);
    assert!(
        read_kanji_verified(
            fixture.store.root(),
            &[identity('字')],
            &KanjiImageValidator::validator_identity()
        )
        .is_err()
    );
    let owner = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('字'))
        .unwrap();
    assert_eq!(owner.current_human_decision(), Some(HumanDecision::Reject));
    let (state, _, _) = run_batch(&fixture.store, "review", 1, |characters| {
        assert_eq!(characters, ["字"]);
        Ok(vec![Ok(media("字", glyph('字')))])
    })
    .unwrap();
    assert_ne!(
        state.items[2].current_sha256.as_deref(),
        Some(reject_hash.as_str())
    );
    assert!(state.items[0].is_ready());
    assert!(state.items[1].is_ready());
    // Новый кандидат может получить индивидуальный VERIFIED или ждать уточнения;
    // целевой повтор не должен запускать получение для уже готовых соседей.
    assert_eq!(state.items[2].attempts.len(), 2);
}

#[test]
fn confirm_of_exact_asset_without_valid_automated_evidence_does_not_approve_owner() {
    let fixture = Fixture::new();
    fixture.start("confirm-without-evidence", &['漢']);
    let bytes = "GIF89a байты не содержат закодированное изображение".as_bytes();
    let hash = sha256_hex(bytes);
    let mut runtime = BatchRuntime::open(fixture.store.root(), "confirm-without-evidence").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    let candidate = runtime
        .persist_candidate(
            bytes,
            ValidationRecord {
                status: SemanticStatus::Uncertain,
                validator: KanjiImageValidator::validator_identity(),
                content_sha256: hash.clone(),
                evidence: vec![ValidationEvidence {
                    kind: "pixel_reference_comparison".into(),
                    summary: "синтетическое свидетельство для теста границы подтверждения".into(),
                    details: None,
                }],
            },
            true,
        )
        .unwrap();
    state
        .record_attempt(&identity('漢'), BatchAttemptInput::Candidate { candidate })
        .unwrap();
    runtime.save(&state).unwrap();
    drop(runtime);

    assert!(fixture.store.verify_integrity().unwrap().is_empty());
    let (response, exit_code) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Decide {
            batch_id: "confirm-without-evidence".into(),
            character: "漢".into(),
            sha256: hash,
            action: BatchActionArg::Confirm,
            reason: "проверить, что автоматическое свидетельство обязательно".into(),
        },
        false,
    )
    .unwrap();

    assert_eq!(exit_code, 3);
    assert_eq!(response.outcome, "publication_blocked");
    assert!(!response.issues.is_empty());
    assert!(!response.items[0].effective_verified);
    let owner = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('漢'))
        .unwrap();
    assert_ne!(owner.current_human_decision(), Some(HumanDecision::Approve));
    assert_eq!(
        owner.validation.as_ref().unwrap().status,
        SemanticStatus::Corrupt
    );
}

#[test]
fn exact_source_cas_fails_before_manifest_mutation() {
    let fixture = Fixture::new();
    let source = fixture.directory().join("candidate.png");
    fs::write(&source, glyph('元')).unwrap();
    let before = fixture.store.verify_integrity().unwrap();
    let error = fixture
        .store
        .ingest(IngestRequest {
            identity: identity('元'),
            source_path: source,
            expected_source_sha256: Some("a".repeat(64)),
            domain_metadata: None,
            replace_expected_sha256: None,
        })
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::IntegrityMismatch);
    assert_eq!(fixture.store.verify_integrity().unwrap(), before);
}

#[test]
fn single_candidate_uncertain_legacy_metrics_do_not_publish() {
    let fixture = Fixture::new();
    fixture.start("aggregate", &['漢']);
    let bytes = glyph('漢');
    let mut runtime = BatchRuntime::open(fixture.store.root(), "aggregate").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    let record = ValidationRecord {
        status: SemanticStatus::Uncertain,
        validator: KanjiImageValidator::validator_identity(),
        content_sha256: sha256_hex(&bytes),
        evidence: vec![ValidationEvidence {
            kind: "pixel_reference_comparison".into(),
            summary: "устаревшие метрики".into(),
            details: Some(
                serde_json::json!({"expected_distance":0.005,"nearest_margin":0.01,"nearest_other":"元"}),
            ),
        }],
    };
    let candidate = runtime.persist_candidate(&bytes, record, true).unwrap();
    state
        .record_attempt(&identity('漢'), BatchAttemptInput::Candidate { candidate })
        .unwrap();
    runtime.save(&state).unwrap();
    drop(runtime);
    let (state, _, issues) = run_batch(&fixture.store, "aggregate", 5, |_| {
        panic!("сохранённый неопределённый кандидат не скачивается снова")
    })
    .unwrap();
    assert!(issues.is_empty());
    assert_eq!(state.items[0].status, BatchItemStatus::AwaitingHuman);
    assert!(!state.items[0].aggregate.accepted);
    assert!(state.items[0].published_sha256.is_none());
    assert_eq!(state.items[0].attempts.len(), 1);
}

#[test]
fn single_candidate_rejected_auto_status_waits_for_human() {
    let fixture = Fixture::new();
    fixture.start("rejected", &['漢']);
    let mut image = image::load_from_memory(&glyph('漢')).unwrap().to_rgba8();
    image.put_pixel(0, 0, image::Rgba([1, 111, 222, 255]));
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let bytes = encoded.into_inner();
    let rejected_hash = sha256_hex(&bytes);
    let mut runtime = BatchRuntime::open(fixture.store.root(), "rejected").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    let record = ValidationRecord {
        status: SemanticStatus::Rejected,
        validator: KanjiImageValidator::validator_identity(),
        content_sha256: rejected_hash.clone(),
        evidence: vec![ValidationEvidence {
            kind: "pixel_reference_comparison".into(),
            summary: "синтетические независимые метрики".into(),
            details: Some(
                serde_json::json!({"expected_distance": 0.015, "nearest_margin": -0.015, "nearest_other": "漠"}),
            ),
        }],
    };
    let candidate = runtime.persist_candidate(&bytes, record, true).unwrap();
    state
        .record_attempt(&identity('漢'), BatchAttemptInput::Candidate { candidate })
        .unwrap();
    runtime.save(&state).unwrap();
    drop(runtime);
    let (state, _, _) = run_batch(&fixture.store, "rejected", 5, |_| {
        panic!("возобновление не должно запускать получение")
    })
    .unwrap();
    let item = &state.items[0];
    assert!(!item.aggregate.accepted);
    assert_eq!(item.aggregate.distinct_valid_hashes.len(), 0);
    assert!(
        !item
            .aggregate
            .distinct_valid_hashes
            .contains(&rejected_hash)
    );
    assert_eq!(
        item.aggregate.selected_sha256.as_deref(),
        Some(rejected_hash.as_str())
    );
    assert_ne!(item.status, BatchItemStatus::AutoVerified);
    assert!(item.published_sha256.is_none());
    assert_eq!(item.status, BatchItemStatus::AwaitingHuman);
    assert!(
        fixture
            .store
            .verify_integrity()
            .unwrap()
            .iter()
            .all(|record| record.sha256 != rejected_hash),
        "отклонённые байты не должны попасть в канонический корпус"
    );
}

#[test]
fn stale_browser_result_is_skipped_after_concurrent_resolution() {
    let fixture = Fixture::new();
    fixture.start("stale", &['元']);
    let (state, _, _) = run_batch(&fixture.store, "stale", 1, |_| {
        let mut runtime = BatchRuntime::open(fixture.store.root(), "stale").unwrap();
        let mut state = runtime.load().unwrap().unwrap();
        let bytes = glyph('元');
        let outcome = fixture
            .store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: identity('元'),
                    bytes,
                    provenance: Provenance {
                        source_kind: "synthetic".into(),
                        source_name: "concurrent.png".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &KanjiImageValidator::new(),
            )
            .unwrap();
        state
            .mark_existing_ready(&identity('元'), &outcome.sha256)
            .unwrap();
        runtime.save(&state).unwrap();
        Ok(vec![Ok(media("元", glyph('元')))])
    })
    .unwrap();
    assert!(state.items[0].attempts.is_empty());
    assert_eq!(state.items[0].current_sha256, Some(sha256_hex(glyph('元'))));
}

#[test]
fn explicit_reacquire_preserves_current_canonical_trust() {
    let fixture = Fixture::new();
    fixture.start("reacquire", &['元']);
    let (state, _, _) = run_batch(&fixture.store, "reacquire", 1, |characters| {
        Ok(characters
            .iter()
            .map(|value| Ok(media(value, glyph('元'))))
            .collect())
    })
    .unwrap();
    let hash = state.items[0].current_sha256.clone().unwrap();
    let (state, issues) = decide_exact(
        &fixture.store,
        "reacquire",
        HumanBatchDecision {
            identity: identity('元'),
            candidate_sha256: hash.clone(),
            action: HumanBatchAction::Reacquire,
            reason: "явное новое получение".into(),
        },
    )
    .unwrap();
    assert!(issues.is_empty());
    assert_eq!(state.next_round(), [identity('元')]);
    let owner = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(owner[0].record.sha256, hash);
    assert_ne!(
        owner[0].record.current_human_decision(),
        Some(HumanDecision::Reject)
    );
    let (state, _, issues) = run_batch(&fixture.store, "reacquire", 1, |_| {
        Ok(vec![Ok(media("元", glyph('元')))])
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(state.is_resolved());
}

#[test]
fn targeted_owner_validation_leaves_unrelated_pending_candidate_untouched() {
    let fixture = Fixture::new();
    let mut hashes = Vec::new();
    for (index, character) in ['漢', '字'].into_iter().enumerate() {
        let bytes = glyph('元');
        let path = fixture.directory().join(format!("candidate-{index}.png"));
        fs::write(&path, &bytes).unwrap();
        let outcome = fixture
            .store
            .ingest(IngestRequest {
                identity: identity(character),
                source_path: path,
                expected_source_sha256: Some(sha256_hex(&bytes)),
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .unwrap();
        hashes.push(outcome.asset.sha256);
    }
    let report = fixture
        .store
        .validate_exact(&identity('漢'), &hashes[0], &KanjiImageValidator::new())
        .unwrap();
    assert_eq!(report.considered, 1);
    assert_eq!(report.attempts[0].identity, identity('漢'));
    let neighbor = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('字'))
        .unwrap();
    assert_eq!(neighbor.lifecycle, LifecycleState::Pending);
    assert!(neighbor.validation.is_none());
}

#[test]
fn persisted_rejection_before_owner_commit_resumes_before_acquisition() {
    let fixture = Fixture::new();
    fixture.start("rejection-resume", &['漢']);
    let (state, _, _) = run_batch(&fixture.store, "rejection-resume", 5, |_| {
        Ok(vec![Ok(media("漢", glyph('元')))])
    })
    .unwrap();
    let hash = state.items[0].current_sha256.clone().unwrap();
    let mut runtime = BatchRuntime::open(fixture.store.root(), "rejection-resume").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    let candidate = current_candidate(&state.items[0]).unwrap().clone();
    let bytes = runtime.read_candidate(&candidate).unwrap();
    state
        .decide(
            HumanBatchDecision {
                identity: identity('漢'),
                candidate_sha256: hash.clone(),
                action: HumanBatchAction::Reject,
                reason: "сохранённое намерение пользователя до прерванной записи владельца".into(),
            },
            &bytes,
        )
        .unwrap();
    runtime.save(&state).unwrap();
    drop(runtime);
    run_batch(&fixture.store, "rejection-resume", 1, |_| {
        let owner = fixture
            .store
            .verify_integrity()
            .unwrap()
            .into_iter()
            .find(|record| record.identity == identity('漢'))
            .unwrap();
        assert_eq!(owner.sha256, hash);
        assert_eq!(owner.current_human_decision(), Some(HumanDecision::Reject));
        Ok(vec![Err("синтетический повтор недоступен".into())])
    })
    .unwrap();
}

fn publish_reference(fixture: &Fixture, batch_id: &str) -> String {
    fixture.start(batch_id, &['元']);
    let (state, _, issues) = run_batch(&fixture.store, batch_id, 1, |_| {
        Ok(vec![Ok(media("元", glyph('元')))])
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(state.is_resolved());
    state.items[0].current_sha256.clone().unwrap()
}

fn distinct_reference_bytes() -> Vec<u8> {
    let mut image = image::load_from_memory(&glyph('元')).unwrap().to_rgba8();
    image.put_pixel(0, 0, image::Rgba([254, 255, 255, 255]));
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    encoded.into_inner()
}

#[test]
fn confirm_published_auto_and_existing_candidates_records_owner_approval() {
    for reused in [false, true] {
        let fixture = Fixture::new();
        let hash = publish_reference(&fixture, "auto");
        let batch_id = if reused {
            fixture.start("reused", &['元']);
            "reused"
        } else {
            "auto"
        };
        let expected_status = if reused {
            BatchItemStatus::ExistingVerified
        } else {
            BatchItemStatus::AutoVerified
        };
        assert_eq!(fixture.load(batch_id).items[0].status, expected_status);
        let (state, issues) = decide_exact(
            &fixture.store,
            batch_id,
            HumanBatchDecision {
                identity: identity('元'),
                candidate_sha256: hash.clone(),
                action: HumanBatchAction::Confirm,
                reason: "явное подтверждение уже опубликованных точных байтов".into(),
            },
        )
        .unwrap();
        assert!(issues.is_empty());
        assert!(state.is_resolved());
        assert_eq!(state.items[0].status, BatchItemStatus::HumanVerified);
        assert_eq!(
            state.items[0].publication_source,
            Some(BatchTrustSource::Human)
        );
        let owner = read_kanji_verified(
            fixture.store.root(),
            &[identity('元')],
            &KanjiImageValidator::validator_identity(),
        )
        .unwrap();
        assert_eq!(owner[0].record.sha256, hash);
        assert_eq!(
            owner[0].record.current_human_decision(),
            Some(HumanDecision::Approve)
        );
        assert_eq!(fixture.load(batch_id), state);
    }
}

#[test]
fn reject_existing_asset_with_approval_from_an_older_validator() {
    struct PriorValidator;
    impl SemanticValidator for PriorValidator {
        fn identity(&self) -> ValidatorIdentity {
            ValidatorIdentity::new("prior-kanji-validator", "1").unwrap()
        }
        fn validate(
            &self,
            _: &AssetRecord,
            _: &mut dyn Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            Ok(SemanticDecision::new(
                SemanticStatus::Verified,
                vec![ValidationEvidence {
                    kind: "prior_validator".into(),
                    summary: "проверка предыдущей версии валидатора".into(),
                    details: None,
                }],
            ))
        }
    }

    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "prior-validator-source");
    fixture
        .store
        .validate_exact(&identity('元'), &hash, &PriorValidator)
        .unwrap();
    fixture
        .store
        .attest(HumanAttestationRequest {
            identity: identity('元'),
            expected_sha256: hash.clone(),
            decision: HumanDecision::Approve,
            reason: "явное подтверждение точных байтов".into(),
        })
        .unwrap();

    fixture.start("prior-validator-reject", &['元']);
    assert_eq!(
        fixture.load("prior-validator-reject").items[0].status,
        BatchItemStatus::ExistingVerified
    );
    let decision = HumanBatchDecision {
        identity: identity('元'),
        candidate_sha256: hash,
        action: HumanBatchAction::Reject,
        reason: "явный отказ после повторной проверки".into(),
    };
    let (state, issues) =
        decide_exact(&fixture.store, "prior-validator-reject", decision.clone()).unwrap();

    assert!(issues.is_empty());
    assert_eq!(state.items[0].status, BatchItemStatus::Reacquire);
    assert_eq!(state.items[0].human_decisions.last(), Some(&decision));
    let owner_record = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('元'))
        .unwrap();
    assert_eq!(
        owner_record.current_human_decision(),
        Some(HumanDecision::Reject)
    );
}

#[test]
fn stale_reject_never_materializes_or_demotes_newer_owner_sha() {
    let fixture = Fixture::new();
    let old_hash = publish_reference(&fixture, "old-batch");
    let new_bytes = distinct_reference_bytes();
    let new = fixture
        .store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity('元'),
                bytes: new_bytes.clone(),
                provenance: Provenance {
                    source_kind: "synthetic".into(),
                    source_name: "newer.png".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: Some(old_hash.clone()),
            },
            &KanjiImageValidator::new(),
        )
        .unwrap();
    assert_eq!(new.status, SemanticStatus::Verified);
    assert_ne!(new.sha256, old_hash);
    let before = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    let (state, issues) = decide_exact(
        &fixture.store,
        "old-batch",
        HumanBatchDecision {
            identity: identity('元'),
            candidate_sha256: old_hash.clone(),
            action: HumanBatchAction::Reject,
            reason: "отказ относится только к прежним проверенным байтам".into(),
        },
    )
    .unwrap();
    assert!(issues.is_empty());
    assert_eq!(state.items[0].status, BatchItemStatus::Reacquire);
    assert_eq!(
        state.items[0]
            .human_decisions
            .last()
            .unwrap()
            .candidate_sha256,
        old_hash
    );
    let after = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(after[0].record, before[0].record);
    assert_eq!(after[0].bytes, new_bytes);
    let (state, _, _) = run_batch(&fixture.store, "old-batch", 1, |_| {
        Ok(vec![Err("синтетическое получение недоступно".into())])
    })
    .unwrap();
    assert!(!state.is_resolved());
    let after_resume = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(after_resume[0].record, before[0].record);
}

#[test]
fn owner_rejection_in_another_batch_invalidates_cached_status_review_and_run() {
    for review_first in [false, true] {
        let fixture = Fixture::new();
        let hash = publish_reference(&fixture, "batch-a");
        fixture.start("batch-b", &['元']);
        assert_eq!(
            fixture.load("batch-b").items[0].status,
            BatchItemStatus::ExistingVerified
        );
        let (_, issues) = decide_exact(
            &fixture.store,
            "batch-b",
            HumanBatchDecision {
                identity: identity('元'),
                candidate_sha256: hash.clone(),
                action: HumanBatchAction::Reject,
                reason: "явный отказ владельца в другом пакете".into(),
            },
        )
        .unwrap();
        assert!(issues.is_empty());
        let command = if review_first {
            BatchCommand::Review {
                batch_id: "batch-a".into(),
            }
        } else {
            BatchCommand::Status {
                batch_id: "batch-a".into(),
            }
        };
        let (response, code) =
            execute_command(&fixture.store, fixture.summary(), &command, false).unwrap();
        assert_eq!(code, 0);
        assert_eq!(response.counts.effective_verified, 0);
        assert_eq!(response.items[0].state, BatchItemStatus::Reacquire);
        assert!(response.changed);
        let state = fixture.load("batch-a");
        assert_eq!(
            state.items[0].observed_owner_rejections[0].candidate_sha256,
            hash
        );
        assert_eq!(
            state.items[0].observed_owner_rejections[0].identity,
            identity('元')
        );
        let (state, _, _) = run_batch(&fixture.store, "batch-a", 1, |_| {
            Ok(vec![Ok(media("元", glyph('元')))])
        })
        .unwrap();
        assert!(!state.is_resolved());
        assert!(state.items[0].aggregate.distinct_valid_hashes.is_empty());
        assert!(
            read_kanji_verified(
                fixture.store.root(),
                &[identity('元')],
                &KanjiImageValidator::validator_identity()
            )
            .is_err()
        );
        let new_bytes = distinct_reference_bytes();
        let (state, _, _) = run_batch(&fixture.store, "batch-a", 1, |_| {
            Ok(vec![Ok(media("元", new_bytes.clone()))])
        })
        .unwrap();
        assert!(state.is_resolved());
        assert_ne!(state.items[0].current_sha256.as_ref().unwrap(), &hash);
        let owner = read_kanji_verified(
            fixture.store.root(),
            &[identity('元')],
            &KanjiImageValidator::validator_identity(),
        )
        .unwrap();
        assert_eq!(owner[0].bytes, new_bytes);
        assert_ne!(
            owner[0].record.current_human_decision(),
            Some(HumanDecision::Reject)
        );
    }
}

#[test]
fn owner_current_hash_change_clears_saved_readiness() {
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "saved-ready");
    fixture
        .store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: identity('元'),
                bytes: distinct_reference_bytes(),
                provenance: Provenance {
                    source_kind: "synthetic".into(),
                    source_name: "changed.png".into(),
                },
                domain_metadata: None,
                replace_expected_sha256: Some(hash),
            },
            &KanjiImageValidator::new(),
        )
        .unwrap();

    let (response, _) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Status {
            batch_id: "saved-ready".into(),
        },
        false,
    )
    .unwrap();
    assert_eq!(response.counts.effective_verified, 0);
    assert_eq!(response.items[0].state, BatchItemStatus::Reacquire);
    assert!(response.items[0].published_sha256.is_none());
    assert!(!fixture.load("saved-ready").is_resolved());
}

#[test]
fn failed_owner_snapshot_preserves_all_cached_ready_items() {
    let fixture = Fixture::new();
    fixture.start("snapshot-error", &['元', '漢']);
    let (before, _, issues) = run_batch(&fixture.store, "snapshot-error", 1, |characters| {
        Ok(characters
            .iter()
            .map(|character| {
                let value = character.chars().next().unwrap();
                Ok(media(character, glyph(value)))
            })
            .collect())
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(before.items.iter().all(|item| item.is_ready()));

    let record = fixture
        .store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity == identity('元'))
        .unwrap();
    fs::remove_file(fixture.store.root().join(record.storage_path)).unwrap();

    let error = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Status {
            batch_id: "snapshot-error".into(),
        },
        false,
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::MissingAssetFile);
    let after = fixture.load("snapshot-error");
    assert_eq!(after.revision, before.revision);
    assert!(after.items.iter().all(|item| item.is_ready()));
}

#[test]
fn run_observes_external_owner_reject_before_restoring_cached_auto_ready() {
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "direct-run");
    fixture
        .store
        .attest(HumanAttestationRequest {
            identity: identity('元'),
            expected_sha256: hash.clone(),
            decision: HumanDecision::Reject,
            reason: "отказ владельца до сверки состояния и проверки".into(),
        })
        .unwrap();
    assert!(fixture.load("direct-run").is_resolved());
    let (state, _, _) = run_batch(&fixture.store, "direct-run", 1, |characters| {
        assert_eq!(characters, ["元"]);
        Ok(vec![Ok(media("元", glyph('元')))])
    })
    .unwrap();
    assert!(!state.is_resolved());
    assert_eq!(state.items[0].status, BatchItemStatus::Reacquire);
    assert_eq!(
        state.items[0].observed_owner_rejections[0].candidate_sha256,
        hash
    );
    assert!(state.items[0].aggregate.distinct_valid_hashes.is_empty());
    assert!(
        read_kanji_verified(
            fixture.store.root(),
            &[identity('元')],
            &KanjiImageValidator::validator_identity()
        )
        .is_err()
    );
}

#[test]
fn expected_validator_trust_loss_invalidates_cached_ready() {
    struct OtherValidator;
    impl SemanticValidator for OtherValidator {
        fn identity(&self) -> ValidatorIdentity {
            ValidatorIdentity::new("synthetic-other", "1").unwrap()
        }
        fn validate(
            &self,
            _: &AssetRecord,
            _: &mut dyn Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            Ok(SemanticDecision::new(
                SemanticStatus::Verified,
                vec![ValidationEvidence {
                    kind: "synthetic_other_classifier".into(),
                    summary: "точная идентичность другого валидатора".into(),
                    details: None,
                }],
            ))
        }
    }
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "validator-loss");
    fixture
        .store
        .validate_exact(&identity('元'), &hash, &OtherValidator)
        .unwrap();
    let (response, _) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Status {
            batch_id: "validator-loss".into(),
        },
        false,
    )
    .unwrap();
    assert_eq!(response.counts.effective_verified, 0);
    assert_eq!(response.items[0].state, BatchItemStatus::Reacquire);
    assert!(response.batch.items[0].observed_owner_rejections.is_empty());
}

#[test]
fn stale_pinned_validator_blocks_decision_before_mutation() {
    struct OtherValidator;
    impl SemanticValidator for OtherValidator {
        fn identity(&self) -> ValidatorIdentity {
            ValidatorIdentity::new("synthetic-other", "1").unwrap()
        }
        fn validate(
            &self,
            _: &AssetRecord,
            _: &mut dyn Read,
        ) -> Result<SemanticDecision, ValidatorFailure> {
            Ok(SemanticDecision::new(
                SemanticStatus::Verified,
                vec![ValidationEvidence {
                    kind: "synthetic_other_classifier".into(),
                    summary: "точная идентичность другого валидатора".into(),
                    details: None,
                }],
            ))
        }
    }
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "seed");
    // Пакет создан до обновления рабочего валидатора: закреплённая идентичность
    // отличается от текущей, поэтому ни решение, ни повторное получение не могут
    // публиковать соседние элементы текущим валидатором.
    let pinned = OtherValidator.identity();
    {
        let mut runtime = BatchRuntime::open(fixture.store.root(), "pinned").unwrap();
        let state = KanjiBatch::new("pinned".into(), vec![identity('元')], pinned.clone()).unwrap();
        runtime.save(&state).unwrap();
    }
    let before = fixture.load("pinned");
    assert_eq!(before.policy.validator, pinned);
    let error = decide_exact(
        &fixture.store,
        "pinned",
        HumanBatchDecision {
            identity: identity('元'),
            candidate_sha256: hash,
            action: HumanBatchAction::Confirm,
            reason: "решение с устаревшим закреплённым валидатором".into(),
        },
    )
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidValidatorIdentity);
    let after = fixture.load("pinned");
    assert_eq!(after.revision, before.revision);
    assert!(after.items[0].human_decisions.is_empty());
    assert_eq!(after.items[0].attempts.len(), 0);
    let retry = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Retry {
            batch_id: "pinned".into(),
            character: "元".into(),
            sha256: None,
            reason: "повтор с устаревшим закреплённым валидатором".into(),
        },
        false,
    )
    .unwrap_err();
    assert_eq!(retry.code, ErrorCode::InvalidValidatorIdentity);
    let after = fixture.load("pinned");
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.items[0].generation, 0);
    assert_eq!(after.items[0].attempts.len(), 0);
}

#[test]
fn repeated_exact_confirm_passes_owner_attestation_after_external_reject() {
    let fixture = Fixture::new();
    let hash = publish_reference(&fixture, "repeat-confirm");
    let decision = HumanBatchDecision {
        identity: identity('元'),
        candidate_sha256: hash.clone(),
        action: HumanBatchAction::Confirm,
        reason: "явное подтверждение точных текущих байтов".into(),
    };
    decide_exact(&fixture.store, "repeat-confirm", decision.clone()).unwrap();
    fixture
        .store
        .attest(HumanAttestationRequest {
            identity: identity('元'),
            expected_sha256: hash,
            decision: HumanDecision::Reject,
            reason: "параллельный независимый отказ владельца".into(),
        })
        .unwrap();
    let (state, issues) = decide_exact(&fixture.store, "repeat-confirm", decision).unwrap();
    assert!(issues.is_empty());
    assert!(state.is_resolved());
    let owner = read_kanji_verified(
        fixture.store.root(),
        &[identity('元')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(
        owner[0].record.current_human_decision(),
        Some(HumanDecision::Approve)
    );
}

#[test]
fn run_error_response_exposes_a_flushed_per_run_diagnostic_log() {
    let fixture = Fixture::new();
    let command = BatchCommand::Run {
        batch_id: "diagnostic-missing-batch".into(),
        rounds: 1,
        workers: 1,
        interactive_review: false,
        #[cfg(feature = "session-rotation-acceptance")]
        acceptance_rotate_after_items: None,
    };
    let output = execute_with_progress(
        &fixture.store,
        fixture.summary(),
        &command,
        OutputFormat::Json,
        false,
        &mut CapturedProgress::default(),
    );
    assert!(!output.stderr.lines().any(|line| !line.is_empty()));
    let response: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
    let log_path = std::path::PathBuf::from(response["diagnostic_log"].as_str().unwrap());
    assert!(log_path.exists());
    assert!(log_path.to_string_lossy().contains("/logs/"));
    let events = fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        events
            .iter()
            .any(|event| event["fields"]["event"] == "run_started")
    );
    let stopped = events
        .iter()
        .find(|event| event["fields"]["event"] == "run_stopped")
        .unwrap();
    assert!(
        stopped["spans"].as_array().unwrap().iter().any(|span| {
            span["name"] == "kanji_batch_run"
                && span["run_id"] == stopped["fields"]["run_id"]
                && span["diagnostic_log"] == response["diagnostic_log"]
        }),
        "стоп-событие: {stopped}"
    );
}

#[test]
fn successful_run_response_exposes_a_flushed_per_run_diagnostic_log() {
    let fixture = Fixture::new();
    publish_reference(&fixture, "diagnostic-success");
    let command = BatchCommand::Run {
        batch_id: "diagnostic-success".into(),
        rounds: 1,
        workers: 1,
        interactive_review: false,
        #[cfg(feature = "session-rotation-acceptance")]
        acceptance_rotate_after_items: None,
    };
    let output = execute_with_progress(
        &fixture.store,
        fixture.summary(),
        &command,
        OutputFormat::Json,
        false,
        &mut CapturedProgress::default(),
    );
    let response: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
    let log_path = std::path::PathBuf::from(response["diagnostic_log"].as_str().unwrap());
    assert!(log_path.exists());
    assert!(log_path.to_string_lossy().contains("/logs/"));
    let events = fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let finished = events
        .iter()
        .find(|event| event["fields"]["event"] == "run_finished")
        .unwrap();
    assert!(finished["spans"].as_array().unwrap().iter().any(|span| {
        span["name"] == "kanji_batch_run"
            && span["run_id"] == finished["fields"]["run_id"]
            && span["diagnostic_log"] == response["diagnostic_log"]
    }));
}

fn capture_browser_checkpoint_events(
    fixture: &Fixture,
    operation: impl FnOnce(),
) -> Vec<serde_json::Value> {
    let log_path = fixture.directory().join("browser-checkpoint.jsonl");
    let guard = RunLogGuard::new(
        fs::File::create(&log_path).unwrap(),
        DiagnosticOutputMode::Json,
    );
    guard.with_default(operation);
    guard.finish().unwrap();
    fs::read_to_string(log_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["fields"]["schema"] == "browser_acquisition_v1")
        .map(|event| event["fields"].clone())
        .collect()
}

fn checkpoint_diagnostics(generation: u32) -> crate::yarxi::AcquisitionItemDiagnostics {
    use crate::browser_diagnostics::{BrowserItemContext, BrowserItemTimer};
    let timer = BrowserItemTimer::new(
        BrowserItemContext::new("yarxi-suu-browser", "元", 1, u64::from(generation))
            .with_browser_session(17),
    );
    timer.set_attempt(2);
    crate::yarxi::AcquisitionItemDiagnostics::new(timer, None)
}

#[test]
fn browser_item_keeps_context_and_duration_through_final_validation_and_durable_checkpoint() {
    let fixture = Fixture::new();
    let batch_id = "browser-checkpoint-context";
    fixture.start(batch_id, &['元']);
    {
        let mut runtime = BatchRuntime::open(fixture.store.root(), batch_id).unwrap();
        let mut state = runtime.load().unwrap().unwrap();
        for _ in 0..MAX_ACQUISITION_ROUNDS {
            state
                .record_attempt(&identity('元'), failure("network", "временный отказ"))
                .unwrap();
        }
        state
            .retry_acquisition(&identity('元'), "новое получение".into())
            .unwrap();
        runtime.save(&state).unwrap();
    }
    let events = capture_browser_checkpoint_events(&fixture, || {
        let mut progress = CapturedProgress::default();
        run_batch_with_stream_and_progress(
            &fixture.store,
            batch_id,
            1,
            |characters, generations, on_event| {
                assert_eq!(characters, ["元"]);
                assert_eq!(generations, [1]);
                on_event(AcquisitionEvent::ItemStarted { index: 0 })?;
                let diagnostics = checkpoint_diagnostics(generations[0]);
                diagnostics.timing.stage("resource_read").finish_success();
                on_event(AcquisitionEvent::ItemCompleted {
                    index: 0,
                    outcome: Box::new(Ok(media("元", glyph('元')))),
                    diagnostics: Some(diagnostics),
                })?;
                // Возврат из обратного вызова означает, что попытка того же поколения уже сохранена.
                let state = fixture.load(batch_id);
                let attempt = state.items[0].attempts.last().unwrap();
                assert_eq!(attempt.generation, 1);
                assert!(matches!(
                    attempt.result,
                    BatchAttemptInput::Candidate { .. }
                ));
                Ok(())
            },
            &mut StoreSnapshotReader,
            &mut progress,
        )
        .unwrap();
    });
    let stages: Vec<_> = events
        .iter()
        .map(|event| event["stage"].as_str().unwrap())
        .collect();
    assert_eq!(
        stages,
        [
            "resource_read",
            "semantic_validation",
            "final_validation",
            "checkpoint",
            "item"
        ]
    );
    for event in &events {
        assert_eq!(event["provider"], "yarxi-suu-browser");
        assert_eq!(event["identity"], "元");
        assert_eq!(event["attempt"], 2);
        assert_eq!(event["generation"], 1);
        assert_eq!(event["browser_session"], 17);
    }
    let checkpoint = &events[3];
    let item = &events[4];
    assert_eq!(checkpoint["outcome"], "success");
    assert_eq!(item["outcome"], "success");
    assert_eq!(item["event"], "browser_acquisition_item");
    // Единственный элемент завершается после контрольной точки тем же монотонным таймером.
    assert!(
        item["item_duration_ms"].as_u64().unwrap()
            >= checkpoint["item_duration_ms"].as_u64().unwrap()
    );
    assert!(
        item["item_duration_ms"].as_u64().unwrap()
            >= checkpoint["stage_duration_ms"].as_u64().unwrap()
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "browser_acquisition_item")
            .count(),
        1
    );
}

#[test]
fn browser_checkpoint_stale_recorded_failures_and_store_failure_never_emit_item_success() {
    for scenario in ["stale", "acquisition", "validation", "semantic", "store"] {
        let fixture = Fixture::new();
        fixture.start(scenario, &['元']);
        let mut runtime = BatchRuntime::open(fixture.store.root(), scenario).unwrap();
        runtime.load().unwrap();
        runtime.release_lock().unwrap();
        let events = capture_browser_checkpoint_events(&fixture, || {
            let diagnostics = checkpoint_diagnostics(0);
            let diagnostics = if scenario == "acquisition" {
                crate::yarxi::AcquisitionItemDiagnostics::new(
                    diagnostics.timing,
                    Some(("media_not_ready".into(), false)),
                )
            } else {
                diagnostics
            };
            if scenario == "stale" {
                let mut concurrent = BatchRuntime::open(fixture.store.root(), scenario).unwrap();
                let mut state = concurrent.load().unwrap().unwrap();
                state
                    .record_attempt(
                        &identity('元'),
                        failure("network", "результат другого владельца"),
                    )
                    .unwrap();
                concurrent.save(&state).unwrap();
            } else if scenario == "store" {
                let path = fixture
                    .store
                    .root()
                    .join(".runtime/batches")
                    .join(scenario)
                    .join("state.json");
                fs::write(path, b"{").unwrap();
            }
            let outcome = match scenario {
                "acquisition" => Err("media_not_ready: синтетический отказ получения".into()),
                "validation" => Ok(media("字", glyph('字'))),
                "semantic" => Ok(media("元", glyph('字'))),
                _ => Ok(media("元", glyph('元'))),
            };
            let result = checkpoint_acquisition_outcome(
                &mut runtime,
                &identity('元'),
                (0, 0),
                outcome,
                Some(diagnostics),
                &mut Vec::new(),
            );
            match scenario {
                "stale" => assert!(!result.unwrap().1),
                "store" => assert!(result.is_err()),
                _ => assert!(result.unwrap().1),
            }
        });
        let item = events
            .iter()
            .find(|event| event["event"] == "browser_acquisition_item")
            .unwrap();
        assert_ne!(
            item["outcome"], "success",
            "сценарий {scenario} не разрешает успех элемента"
        );
        assert_eq!(item["generation"], 0);
        if scenario == "stale" {
            assert_eq!(item["outcome"], "discarded_stale");
            assert_eq!(events.len(), 2);
            assert_eq!(fixture.load(scenario).items[0].attempts.len(), 1);
        } else if scenario == "semantic" {
            assert!(matches!(
                item["outcome"].as_str(),
                Some("failure" | "uncertain")
            ));
            assert!(matches!(
                fixture.load(scenario).items[0].attempts[0].result,
                BatchAttemptInput::Candidate { .. }
            ));
        } else {
            assert_eq!(item["outcome"], "failure");
            assert!(item["failure_code"].is_string());
            if scenario == "acquisition" {
                assert_eq!(item["failure_code"], "media_not_ready");
            }
            if scenario == "validation" {
                assert_eq!(item["failure_code"], "source_identity_mismatch");
                assert_eq!(events[0]["stage"], "final_validation");
                assert_eq!(events[0]["outcome"], "failure");
            }
        }
    }
}

#[test]
fn browser_item_cancelled_before_checkpoint_remains_interrupted_and_resumable() {
    let fixture = Fixture::new();
    let batch_id = "browser-before-checkpoint-interrupted";
    fixture.start(batch_id, &['元']);
    let events = capture_browser_checkpoint_events(&fixture, || {
        let diagnostics = checkpoint_diagnostics(0);
        let stage = diagnostics.timing.stage("resource_read");
        diagnostics
            .timing
            .set_interruption("ctrl_c", "acquisition_interrupted", false);
        drop(stage);
        drop(diagnostics);
    });
    assert_eq!(events.len(), 2);
    for event in events {
        assert_eq!(event["outcome"], "interrupted");
        assert_eq!(event["failure_code"], "acquisition_interrupted");
        assert_eq!(event["browser_session"], 17);
        assert_eq!(event["generation"], 0);
    }
    assert!(fixture.load(batch_id).items[0].attempts.is_empty());
    assert_eq!(fixture.load(batch_id).next_round(), [identity('元')]);
}

#[test]
fn parallel_progress_events_have_exact_human_labels_and_optional_context() {
    let mut event = BatchProgressEvent {
        schema_version: 1,
        operation: "batch_run",
        event: "worker_assigned",
        batch_id: "human-progress".into(),
        elapsed_ms: 0,
        round: Some(1),
        round_limit: 1,
        round_completed: 0,
        round_total: 1,
        run_completed: 0,
        batch_total: 1,
        workers: 2,
        in_flight: 1,
        identity: Some(identity('漢')),
        session: Some(3),
        worker_id: Some(2),
        browser_session_id: Some(17),
        attempt: None,
        generation: Some(4),
        owner_attempt: Some(5),
        item_elapsed_ms: None,
        stage_timings: None,
        outcome: None,
        reason: None,
    };
    for (name, expected) in [
        ("worker_assigned", "элемент назначен обработчику"),
        (
            "browser_session_bootstrap_started",
            "подготовка сессии браузера",
        ),
        (
            "browser_session_recovery_started",
            "восстановление сессии браузера",
        ),
        ("item_deferred", "элемент отложен"),
        ("item_requeued", "элемент возвращён в очередь"),
        ("worker_retired", "обработчик выведен из пула"),
        ("worker_stopped", "обработчик остановлен"),
        ("worker_exhausted", "пул обработчиков исчерпан"),
    ] {
        event.event = name;
        assert_eq!(progress_event_label(name), expected);
        let mut output = Vec::new();
        write_progress_event(&event, OutputFormat::Human, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        for fragment in [
            expected,
            "обработчик=2",
            "сессия=3",
            "браузер=17",
            "поколение=4",
            "попытка владельца=5",
        ] {
            assert!(output.contains(fragment), "{output}");
        }
        let mut machine = Vec::new();
        write_progress_event(&event, OutputFormat::Json, &mut machine).unwrap();
        let machine: serde_json::Value = serde_json::from_slice(&machine).unwrap();
        assert_eq!(machine["event"], name);
        assert_eq!(machine["worker_id"], 2);
        assert_eq!(machine["browser_session_id"], 17);
    }
    event.worker_id = None;
    event.session = None;
    event.browser_session_id = None;
    event.generation = None;
    event.owner_attempt = None;
    event.event = "worker_stopped";
    event.reason = Some("finished".into());
    let mut output = Vec::new();
    write_progress_event(&event, OutputFormat::Human, &mut output).unwrap();
    let output = String::from_utf8(output).unwrap();
    for fragment in [
        "обработчик=",
        "сессия=",
        "браузер=",
        "поколение=",
        "попытка владельца=",
        "finished",
    ] {
        assert!(!output.contains(fragment), "{output}");
    }
    assert!(output.contains("работа завершена"));
    let mut machine = Vec::new();
    write_progress_event(&event, OutputFormat::Json, &mut machine).unwrap();
    let machine: serde_json::Value = serde_json::from_slice(&machine).unwrap();
    assert_eq!(machine["reason"], "finished");
    assert!(machine.get("worker_id").is_none());
}

#[test]
fn kanji_workers_default_and_range_are_enforced_by_cli_and_batch_entry() {
    let parsed = <crate::cli::Cli as clap::Parser>::try_parse_from([
        "kanji-assets",
        "batch",
        "run",
        "--batch-id",
        "worker-default",
    ])
    .unwrap();
    assert!(matches!(
        parsed.command,
        crate::cli::Command::Batch {
            command: BatchCommand::Run { workers: 4, .. }
        }
    ));
    for workers in ["0", "5"] {
        assert!(
            <crate::cli::Cli as clap::Parser>::try_parse_from([
                "kanji-assets",
                "batch",
                "run",
                "--batch-id",
                "worker-range",
                "--workers",
                workers
            ])
            .is_err()
        );
    }
    let fixture = Fixture::new();
    fixture.start("worker-range", &['漢']);
    for workers in [0, 5] {
        let mut progress = CapturedProgress::default();
        assert!(
            run_batch_with_worker_stream_and_progress(
                &fixture.store,
                "worker-range",
                1,
                workers,
                |_, _, _, _, _| panic!("невалидное число обработчиков не допускает запуск"),
                &mut StoreSnapshotReader,
                &mut progress
            )
            .is_err()
        );
        assert!(progress.events.is_empty());
        assert!(fixture.load("worker-range").items[0].attempts.is_empty());
    }
}

#[test]
fn parallel_frontier_rejects_invalid_worker_ownership_and_tokens_before_checkpoint() {
    for case in [
        "zero",
        "out-of-range",
        "busy",
        "generation",
        "attempt",
        "foreign-terminal",
        "wrong-index",
    ] {
        let fixture = Fixture::new();
        fixture.start("worker-protocol", &['漢', '字']);
        let mut progress = CapturedProgress::default();
        let result = run_batch_with_worker_stream_and_progress(
            &fixture.store,
            "worker-protocol",
            1,
            2,
            |_, _, _, _, on_event| {
                if matches!(case, "busy" | "foreign-terminal" | "wrong-index") {
                    on_event(worker_event(1, 0, AcquisitionWorkerEventKind::Assigned))
                        .map_err(AcquisitionStreamError::Consumer)?;
                }
                let mut event = worker_event(
                    1,
                    if case == "busy" { 1 } else { 0 },
                    AcquisitionWorkerEventKind::Assigned,
                );
                let BatchAcquisitionEvent::Worker(ref mut worker) = event else {
                    unreachable!()
                };
                match case {
                    "zero" => worker.worker_id = 0,
                    "out-of-range" => worker.worker_id = 3,
                    "generation" => worker.generation = Some(1),
                    "attempt" => worker.owner_attempt = Some(2),
                    "foreign-terminal" | "wrong-index" => {
                        if case == "foreign-terminal" {
                            worker.worker_id = 2;
                        }
                        worker.event =
                            AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                                index: if case == "wrong-index" { 1 } else { 0 },
                                outcome: Box::new(Err("не должен быть записан".into())),
                                diagnostics: None,
                            });
                    }
                    _ => (),
                }
                on_event(event).map_err(AcquisitionStreamError::Consumer)
            },
            &mut StoreSnapshotReader,
            &mut progress,
        );
        assert!(result.is_err(), "{case}");
        assert!(
            fixture
                .load("worker-protocol")
                .items
                .iter()
                .all(|item| item.attempts.is_empty()),
            "{case}"
        );
        assert!(
            !progress
                .events
                .iter()
                .any(|event| event.event == "item_checkpointed"),
            "{case}"
        );
        assert!(progress.events.iter().all(|event| event.in_flight <= 2));
    }
}

#[test]
fn parallel_distinct_media_out_of_order_preserves_identity_bytes_and_cas() {
    let fixture = Fixture::new();
    fixture.start("parallel-media", &['漢', '字']);
    let mut progress = CapturedProgress::default();
    let (state, _, _) = run_batch_with_worker_stream_and_progress(
        &fixture.store,
        "parallel-media",
        1,
        2,
        |characters, _, _, _, on_event| {
            for index in 0..2 {
                on_event(worker_event(
                    index + 1,
                    index,
                    AcquisitionWorkerEventKind::Assigned,
                ))
                .map_err(AcquisitionStreamError::Consumer)?;
            }
            for index in [1, 0] {
                let character = &characters[index];
                on_event(worker_event(
                    index + 1,
                    index,
                    AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                        index,
                        outcome: Box::new(Ok(media(
                            character,
                            glyph(character.chars().next().unwrap()),
                        ))),
                        diagnostics: None,
                    }),
                ))
                .map_err(AcquisitionStreamError::Consumer)?;
            }
            Ok(())
        },
        &mut StoreSnapshotReader,
        &mut progress,
    )
    .unwrap();
    for item in &state.items {
        assert_eq!(item.attempts.len(), 1);
        let BatchAttemptInput::Candidate { candidate } = &item.attempts[0].result else {
            panic!("ожидался кандидат именно этого кандзи");
        };
        let expected = glyph(item.identity.key.chars().next().unwrap());
        assert_eq!(candidate.sha256, crate::hashing::sha256_hex(&expected));
        let bytes = fs::read(
            fixture
                .store
                .root()
                .join(".runtime/batches/parallel-media")
                .join(&candidate.storage_path),
        )
        .unwrap();
        assert_eq!(bytes, expected);
    }
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_checkpointed")
            .map(|event| event.identity.as_ref().unwrap().key.as_str())
            .collect::<Vec<_>>(),
        ["字", "漢"]
    );
}

#[test]
fn parallel_stale_outcome_cannot_append_to_a_concurrently_changed_attempt() {
    let fixture = Fixture::new();
    fixture.start("parallel-stale", &['漢']);
    let mut progress = CapturedProgress::default();
    let (state, _, _) = run_batch_with_worker_stream_and_progress(
        &fixture.store,
        "parallel-stale",
        1,
        2,
        |_, _, _, _, on_event| {
            on_event(worker_event(1, 0, AcquisitionWorkerEventKind::Assigned))
                .map_err(AcquisitionStreamError::Consumer)?;
            {
                let mut runtime =
                    BatchRuntime::open(fixture.store.root(), "parallel-stale").unwrap();
                let mut current = runtime.load().unwrap().unwrap();
                current
                    .record_attempt(
                        &identity('漢'),
                        BatchAttemptInput::Failed {
                            code: "concurrent".into(),
                            message: "исход другого запуска".into(),
                        },
                    )
                    .unwrap();
                runtime.save(&current).unwrap();
            }
            on_event(worker_event(
                1,
                0,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                    index: 0,
                    outcome: Box::new(Err("устаревший исход".into())),
                    diagnostics: None,
                }),
            ))
            .map_err(AcquisitionStreamError::Consumer)
        },
        &mut StoreSnapshotReader,
        &mut progress,
    )
    .unwrap();
    assert_eq!(state.items[0].attempts.len(), 1);
    assert!(
        matches!(&state.items[0].attempts[0].result, BatchAttemptInput::Failed { code, .. } if code == "concurrent")
    );
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.event == "item_discarded_stale")
    );
    assert!(
        !progress
            .events
            .iter()
            .any(|event| event.event == "item_checkpointed")
    );
}

fn review_fixture(id: &str, characters: &[char]) -> Fixture {
    let fixture = Fixture::new();
    fixture.start(id, characters);
    let bytes = glyph('元');
    let (state, _, _) = run_batch(&fixture.store, id, 1, |characters| {
        Ok(characters
            .iter()
            .map(|character| Ok(media(character, bytes.clone())))
            .collect())
    })
    .unwrap();
    assert_eq!(state.review_queue().len(), characters.len());
    assert!(state.items.iter().all(|item| item.attempts.len() == 1));
    fixture
}

fn review_bundle(fixture: &Fixture, id: &str) -> crate::kanji_review::ReviewBundle {
    let mut runtime = BatchRuntime::open(fixture.store.root(), id).unwrap();
    let state = runtime.load().unwrap().unwrap();
    crate::kanji_review::prepare(&runtime, &state).unwrap()
}

#[test]
fn review_export_restores_missing_artifacts_without_overwriting_decisions() {
    let fixture = review_fixture("review-export-recovery", &['保']);
    let bundle = review_bundle(&fixture, "review-export-recovery");
    fill_review(&bundle.decisions, &["y"]);
    let decisions = fs::read(&bundle.decisions).unwrap();
    let image_name = {
        let runtime = BatchRuntime::open(fixture.store.root(), "review-export-recovery").unwrap();
        let (snapshot, _) = crate::kanji_review::read(&runtime, &bundle.id).unwrap();
        snapshot.rows[0].filename.clone()
    };
    let image_path = bundle.directory.join(&image_name);
    let image = fs::read(&image_path).unwrap();
    fs::remove_file(&image_path).unwrap();

    let restored = review_bundle(&fixture, "review-export-recovery");
    assert_eq!(restored.id, bundle.id);
    assert_eq!(fs::read(&restored.decisions).unwrap(), decisions);
    assert_eq!(fs::read(&image_path).unwrap(), image);

    fs::remove_file(&restored.decisions).unwrap();
    let restored = review_bundle(&fixture, "review-export-recovery");
    let template = fs::read_to_string(&restored.decisions).unwrap();
    assert!(template.lines().any(|line| line.starts_with("\t保\t")));
    assert!(!template.lines().any(|line| line.starts_with("y\t")));
}

#[test]
fn manual_review_pages_survive_invalid_optional_ocr_cache() {
    let fixture = review_fixture("review-invalid-ocr-cache", &['保']);
    let mut runtime = BatchRuntime::open(fixture.store.root(), "review-invalid-ocr-cache").unwrap();
    let state = runtime.load().unwrap().unwrap();
    runtime
        .artifact_directory("ocr")
        .unwrap()
        .write("suggestions.json", b"{")
        .unwrap();

    let html_path = runtime.write_review(&state).unwrap();
    let html = fs::read_to_string(html_path).unwrap();
    assert!(html.contains("default-src 'none'; img-src 'self' file:"));

    let bundle = crate::kanji_review::prepare(&runtime, &state).unwrap();
    let preview = fs::read_to_string(bundle.directory.join("index.html")).unwrap();
    assert!(preview.contains("default-src 'none'; img-src 'self' file:"));
    assert!(preview.contains("src=\"data:image/"));
    let (snapshot, _) = crate::kanji_review::read(&runtime, &bundle.id).unwrap();
    assert!(snapshot.rows.iter().all(|row| row.ocr.is_none()));
}

fn fill_review(path: &std::path::Path, actions: &[&str]) {
    let original = fs::read_to_string(path).unwrap();
    let mut index = 0;
    let text = original
        .lines()
        .map(|line| {
            let columns = line.strip_prefix("-\t").or_else(|| line.strip_prefix('\t'));
            if let Some(columns) = columns {
                let next = format!("{}\t{columns}", actions[index]);
                index += 1;
                next
            } else {
                line.into()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    fs::write(path, format!("\u{feff}{text}\r\n")).unwrap();
}

#[test]
fn review_applies_atomic_intents_exact_bytes_and_idempotent_recovery() {
    let fixture = review_fixture("review-actions", &['保', '倉', '点', '門']);
    let bundle = review_bundle(&fixture, "review-actions");
    fill_review(&bundle.decisions, &["y", "e", "侏", "-"]);
    let (state, issues, counts) =
        apply_review(&fixture.store, "review-actions", &bundle.id).unwrap();
    assert!(issues.is_empty());
    assert_eq!(
        (counts.confirmed, counts.rejected, counts.skipped),
        (1, 2, 1)
    );
    assert_eq!(state.items[0].status, BatchItemStatus::HumanVerified);
    assert!(state.items[0].is_ready());
    assert_eq!(state.items[1].status, BatchItemStatus::Rejected);
    assert_eq!(state.items[2].status, BatchItemStatus::Rejected);
    assert!(
        state.items[2]
            .human_decisions
            .last()
            .unwrap()
            .reason
            .contains("actual_character=侏")
    );
    assert!(state.next_round().is_empty());
    let hash = state.items[0].current_sha256.clone().unwrap();
    let verified = read_kanji_verified(
        fixture.store.root(),
        &[identity('保')],
        &state.policy.validator,
    )
    .unwrap();
    assert_eq!(verified[0].record.sha256, hash);
    assert_eq!(
        verified[0].record.current_human_decision(),
        Some(HumanDecision::Approve)
    );
    let revision = state.revision;
    let (repeated, issues, counts) =
        apply_review(&fixture.store, "review-actions", &bundle.id).unwrap();
    assert!(issues.is_empty());
    assert_eq!(
        (counts.applied, counts.already_applied, counts.skipped),
        (0, 3, 1)
    );
    assert_eq!(repeated.revision, revision);
    assert_eq!(fixture.load("review-actions"), repeated);
    // Отказ из файла сам не повторяется; пользовательский retry снимает hold.
    execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Retry {
            batch_id: "review-actions".into(),
            character: "倉".into(),
            sha256: None,
            reason: state.items[1]
                .human_decisions
                .last()
                .unwrap()
                .reason
                .clone(),
        },
        false,
    )
    .unwrap();
    assert_eq!(
        fixture.load("review-actions").next_round(),
        [identity('倉')]
    );
    assert!(apply_review(&fixture.store, "review-actions", &bundle.id).is_err());
}

#[test]
fn review_preflight_rejects_bad_later_row_without_partial_confirmation() {
    let fixture = review_fixture("review-preflight", &['保', '倉']);
    let bundle = review_bundle(&fixture, "review-preflight");
    fill_review(&bundle.decisions, &["y", "unknown"]);
    let before = fixture.load("review-preflight");
    let manifest = fs::read(fixture.store.root().join("manifest.json")).unwrap();
    assert!(apply_review(&fixture.store, "review-preflight", &bundle.id).is_err());
    assert_eq!(fixture.load("review-preflight"), before);
    assert_eq!(
        fs::read(fixture.store.root().join("manifest.json")).unwrap(),
        manifest
    );
    // Резервная копия редактора не считается решением.
    fs::write(
        bundle.directory.join("decisions.tsv~"),
        fs::read(&bundle.decisions).unwrap(),
    )
    .unwrap();
    fs::remove_file(&bundle.decisions).unwrap();
    assert!(apply_review(&fixture.store, "review-preflight", &bundle.id).is_err());
}

#[test]
fn review_rejects_symlinks_and_preserves_editor_contents_on_export() {
    use std::os::unix::fs::symlink;
    let fixture = review_fixture("review-symlink", &['保']);
    let bundle = review_bundle(&fixture, "review-symlink");
    fill_review(&bundle.decisions, &["y"]);
    let text = fs::read(&bundle.decisions).unwrap();
    let same = review_bundle(&fixture, "review-symlink");
    assert_eq!(same.id, bundle.id);
    assert_eq!(fs::read(&same.decisions).unwrap(), text);
    let outside = fixture.directory().join("outside.tsv");
    fs::write(&outside, &text).unwrap();
    fs::remove_file(&bundle.decisions).unwrap();
    symlink(&outside, &bundle.decisions).unwrap();
    assert!(apply_review(&fixture.store, "review-symlink", &bundle.id).is_err());
    assert_eq!(fs::read(&outside).unwrap(), text);
    assert!(apply_review(&fixture.store, "review-symlink", "../outside").is_err());
}

#[test]
fn review_recovers_saved_confirm_intent_without_applying_new_bytes() {
    let fixture = review_fixture("review-recovery", &['保']);
    let bundle = review_bundle(&fixture, "review-recovery");
    fill_review(&bundle.decisions, &["y"]);
    {
        let mut runtime = BatchRuntime::open(fixture.store.root(), "review-recovery").unwrap();
        let mut state = runtime.load().unwrap().unwrap();
        let (snapshot, decisions) = crate::kanji_review::read(&runtime, &bundle.id).unwrap();
        let bytes = runtime
            .read_candidate(crate::kanji_review::candidate(&state, &snapshot.rows[0]).unwrap())
            .unwrap();
        let crate::kanji_review::ParsedDecision::Apply(decision) = decisions[0].clone() else {
            panic!("шаблон содержит явное подтверждение");
        };
        state.decide(decision, &bytes).unwrap();
        runtime.save(&state).unwrap(); // точка остановки после durable intent, до publish
    }
    let (state, issues, counts) =
        apply_review(&fixture.store, "review-recovery", &bundle.id).unwrap();
    assert!(issues.is_empty());
    assert_eq!(counts.already_applied, 1);
    assert!(state.items[0].is_ready());
    assert_eq!(state.items[0].human_decisions.len(), 1);
}

struct ReviewSpy {
    eligible: bool,
    answers: std::collections::VecDeque<bool>,
    asks: usize,
    opens: usize,
    fail_open: bool,
    fill: bool,
}
impl super::super::review_ui::ReviewInteraction for ReviewSpy {
    fn eligible(&self) -> bool {
        self.eligible
    }
    fn ask(&mut self, _: &str) -> Result<bool, AssetError> {
        self.asks += 1;
        Ok(self.answers.pop_front().unwrap_or(false))
    }
    fn open(
        &mut self,
        request: &super::super::review_ui::ReviewOpen<'_>,
    ) -> Result<(), AssetError> {
        self.opens += 1;
        if self.fail_open {
            return Err(invalid("редактор недоступен"));
        }
        if self.fill {
            fill_review(request.decisions, &["y"]);
        }
        Ok(())
    }
}
fn spy() -> ReviewSpy {
    ReviewSpy {
        eligible: true,
        answers: [true, true].into(),
        asks: 0,
        opens: 0,
        fail_open: false,
        fill: false,
    }
}

#[test]
fn interactive_review_keeps_issues_for_pending_items_and_drops_only_ready_identities() {
    let fixture = review_fixture("review-preserves-issues", &['保', '倉']);
    let bundle = review_bundle(&fixture, "review-preserves-issues");
    fill_review(&bundle.decisions, &["y", "-"]);

    let first = identity('保');
    let second = identity('倉');
    let prior_error = AssetError::new(
        ErrorCode::IdentityConflict,
        "предыдущая проблема публикации",
    );
    let issues = vec![issue(&first, &prior_error), issue(&second, &prior_error)];
    let mut response = batch_response(
        "batch_run",
        fixture.summary(),
        fixture.load("review-preserves-issues"),
        false,
        issues,
        None,
        "awaiting_human",
    );

    let mut interaction = spy();
    finish_interactive_review(&fixture.store, &mut response, &mut interaction).unwrap();

    assert!(response.batch.items[0].is_ready());
    assert!(!response.batch.items[1].is_ready());
    assert_eq!(response.issues.len(), 1);
    assert_eq!(response.issues[0].identity, second);
    assert_eq!(
        response.issues[0].code,
        ErrorCode::IdentityConflict.as_str()
    );
    assert_eq!(
        response.blockers,
        vec![format!(
            "{}:{}",
            second,
            ErrorCode::IdentityConflict.as_str()
        )]
    );
}

#[test]
fn actual_dispatch_has_no_prompt_or_launcher_without_opt_in_or_in_json_or_headless() {
    let fixture = review_fixture("review-gate", &['保']);
    let original = fixture.load("review-gate");
    for (opt_in, output, eligible) in [
        (false, OutputFormat::Human, true),
        (false, OutputFormat::Json, true),
        (true, OutputFormat::Json, true),
        (true, OutputFormat::Human, false),
    ] {
        let mut spy = spy();
        spy.eligible = eligible;
        let command = Cli::try_parse_from(if opt_in {
            vec![
                "kanji-assets",
                "batch",
                "run",
                "--batch-id",
                "review-gate",
                "--interactive-review",
            ]
        } else {
            vec!["kanji-assets", "batch", "run", "--batch-id", "review-gate"]
        })
        .unwrap();
        let Command::Batch { command } = command.command else {
            unreachable!()
        };
        let mut progress = CapturedProgress::default();
        let result = execute_with_progress_and_interaction(
            &fixture.store,
            fixture.summary(),
            &command,
            output,
            false,
            &mut progress,
            &mut spy,
        );
        assert_eq!((spy.asks, spy.opens), (0, 0));
        if output == OutputFormat::Json {
            serde_json::from_str::<serde_json::Value>(&result.stdout).unwrap();
        }
        assert_eq!(fixture.load("review-gate"), original);
    }
}

#[test]
fn explicit_review_command_refuses_json_and_ineligible_terminal_without_prompt() {
    let fixture = review_fixture("review-explicit-gate", &['保']);
    let original = fixture.load("review-explicit-gate");
    for (output, eligible) in [(OutputFormat::Json, true), (OutputFormat::Human, false)] {
        let mut interaction = spy();
        interaction.eligible = eligible;
        let mut progress = CapturedProgress::default();
        let result = execute_with_progress_and_interaction(
            &fixture.store,
            fixture.summary(),
            &BatchCommand::ReviewInteractive {
                batch_id: "review-explicit-gate".into(),
            },
            output,
            false,
            &mut progress,
            &mut interaction,
        );
        assert_ne!(result.exit_code, 0);
        assert_eq!((interaction.asks, interaction.opens), (0, 0));
        assert_eq!(fixture.load("review-explicit-gate"), original);
    }
}

#[test]
fn interactive_decline_failure_cancel_and_explicit_apply_preserve_saved_work() {
    for case in ["decline", "eof", "editor-failure", "cancel", "apply"] {
        let fixture = review_fixture(case, &['保']);
        let original = fixture.load(case);
        let mut spy = spy();
        if case == "decline" {
            spy.answers = [false].into();
        }
        if case == "eof" {
            spy.answers.clear();
        }
        if case == "cancel" {
            spy.answers = [true, false].into();
        }
        spy.fail_open = case == "editor-failure";
        spy.fill = case == "apply";
        let command = BatchCommand::ReviewInteractive {
            batch_id: case.into(),
        };
        let mut progress = CapturedProgress::default();
        let result = execute_with_progress_and_interaction(
            &fixture.store,
            fixture.summary(),
            &command,
            OutputFormat::Human,
            false,
            &mut progress,
            &mut spy,
        );
        assert_eq!(spy.opens, usize::from(!matches!(case, "decline" | "eof")));
        if case == "apply" {
            assert_eq!(result.exit_code, 0);
            assert!(fixture.load(case).items[0].is_ready());
        } else {
            assert_eq!(fixture.load(case), original);
        }
    }
    let fixture = Fixture::new();
    fixture.start("empty-review", &['保']);
    let mut spy = spy();
    let mut progress = CapturedProgress::default();
    execute_with_progress_and_interaction(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::ReviewInteractive {
            batch_id: "empty-review".into(),
        },
        OutputFormat::Human,
        false,
        &mut progress,
        &mut spy,
    );
    assert_eq!((spy.asks, spy.opens), (0, 0));
}

#[test]
fn review_rejects_changed_gallery_and_stale_same_sha_generation() {
    let fixture = review_fixture("review-stale", &['保']);
    let bundle = review_bundle(&fixture, "review-stale");
    fill_review(&bundle.decisions, &["y"]);
    let gallery = fs::read_dir(&bundle.directory)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "png"))
        .unwrap();
    let bytes = fs::read(&gallery).unwrap();
    fs::write(&gallery, b"changed").unwrap();
    assert!(apply_review(&fixture.store, "review-stale", &bundle.id).is_err());
    assert!(
        fixture.load("review-stale").items[0]
            .human_decisions
            .is_empty()
    );
    fs::write(&gallery, &bytes).unwrap();
    let hash = fixture.load("review-stale").items[0]
        .current_sha256
        .clone()
        .unwrap();
    decide_exact(
        &fixture.store,
        "review-stale",
        HumanBatchDecision {
            identity: identity('保'),
            candidate_sha256: hash,
            action: HumanBatchAction::Reacquire,
            reason: "новое поколение".into(),
        },
    )
    .unwrap();
    run_batch(&fixture.store, "review-stale", 1, |characters| {
        Ok(characters
            .iter()
            .map(|character| Ok(media(character, bytes.clone())))
            .collect())
    })
    .unwrap();
    assert!(apply_review(&fixture.store, "review-stale", &bundle.id).is_err());
    assert!(
        fixture.load("review-stale").items[0]
            .human_decisions
            .iter()
            .all(|decision| decision.action != HumanBatchAction::Confirm)
    );
}

#[test]
fn revalidate_migrates_legacy_candidates_without_network_attempts_or_inheriting_old_auto() {
    let fixture = Fixture::new();
    let bytes = glyph('元');
    let old = ValidatorIdentity::new("previous-validator", "v0").unwrap();
    let mut state =
        KanjiBatch::new("review-migration".into(), vec![identity('保')], old.clone()).unwrap();
    state.policy = crate::batch::AggregatePolicy::current(old.clone());
    {
        let mut runtime = BatchRuntime::open(fixture.store.root(), "review-migration").unwrap();
        for _ in 0..MAX_ACQUISITION_ROUNDS {
            let record = ValidationRecord {
                status: SemanticStatus::Uncertain,
                validator: old.clone(),
                content_sha256: sha256_hex(&bytes),
                evidence: vec![ValidationEvidence {
                    kind: "previous-evidence".into(),
                    summary: "предыдущее свидетельство".into(),
                    details: None,
                }],
            };
            let candidate = runtime.persist_candidate(&bytes, record, true).unwrap();
            state
                .record_attempt(&identity('保'), BatchAttemptInput::Candidate { candidate })
                .unwrap();
        }
        runtime.save(&state).unwrap();
    }
    let (response, code) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Revalidate {
            batch_id: "review-migration".into(),
        },
        false,
    )
    .unwrap();
    assert_eq!(code, 0);
    let state = response.batch;
    assert_eq!(
        state.policy.validator,
        KanjiImageValidator::validator_identity()
    );
    assert_eq!(state.policy.version, "kanji-single-candidate-v1");
    assert_eq!(
        state.items[0].attempts.len(),
        MAX_ACQUISITION_ROUNDS as usize
    );
    assert!(state.next_round().is_empty());
    assert_eq!(
        state.items[0].current_sha256.as_deref(),
        Some(sha256_hex(&bytes).as_str())
    );
    for attempt in &state.items[0].attempts {
        let BatchAttemptInput::Candidate { candidate } = &attempt.result else {
            unreachable!()
        };
        assert_eq!(candidate.automated.validator, state.policy.validator);
        assert_ne!(candidate.automated.status, SemanticStatus::Verified);
    }
}

#[test]
fn revalidate_uses_current_decode_to_restore_a_previously_false_validity_flag() {
    let fixture = Fixture::new();
    let bytes = glyph('元');
    let old = ValidatorIdentity::new("previous-validator", "v0").unwrap();
    let mut state = KanjiBatch::new(
        "revalidate-decode".into(),
        vec![identity('元')],
        old.clone(),
    )
    .unwrap();
    state.policy = crate::batch::AggregatePolicy::single_candidate(old.clone());
    let mut runtime = BatchRuntime::open(fixture.store.root(), "revalidate-decode").unwrap();
    let candidate = runtime
        .persist_candidate(
            &bytes,
            ValidationRecord {
                status: SemanticStatus::Corrupt,
                validator: old,
                content_sha256: sha256_hex(&bytes),
                evidence: vec![ValidationEvidence {
                    kind: "previous_decode_failure".into(),
                    summary: "устаревшее свидетельство прежней проверки".into(),
                    details: None,
                }],
            },
            false,
        )
        .unwrap();
    let sha256 = candidate.sha256.clone();
    state
        .record_attempt(&identity('元'), BatchAttemptInput::Candidate { candidate })
        .unwrap();
    runtime.save(&state).unwrap();
    drop(runtime);

    let (response, code) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Revalidate {
            batch_id: "revalidate-decode".into(),
        },
        false,
    )
    .unwrap();

    assert_eq!(code, 0);
    let state = response.batch;
    let item = &state.items[0];
    let BatchAttemptInput::Candidate { candidate } = &item.attempts[0].result else {
        unreachable!()
    };
    assert_eq!(candidate.sha256, sha256);
    assert!(candidate.technically_valid);
    assert_eq!(candidate.automated.content_sha256, sha256);
    assert_eq!(
        candidate.automated.validator,
        KanjiImageValidator::validator_identity()
    );
    assert!(matches!(
        candidate.automated.status,
        SemanticStatus::Uncertain | SemanticStatus::Verified
    ));
    assert_eq!(item.attempts.len(), 1);
    assert_eq!(item.current_sha256.as_deref(), Some(sha256.as_str()));
    state.validate().unwrap();

    let mut runtime = BatchRuntime::open(fixture.store.root(), "revalidate-decode").unwrap();
    let reopened = runtime.load().unwrap().unwrap();
    assert_eq!(reopened, state);
    let BatchAttemptInput::Candidate { candidate } = &reopened.items[0].attempts[0].result else {
        unreachable!()
    };
    assert_eq!(runtime.read_candidate(candidate).unwrap(), bytes);
}

#[test]
fn revalidate_keeps_human_confirmation_for_the_same_currently_valid_candidate() {
    let fixture = Fixture::new();
    let bytes = glyph('元');
    let old = ValidatorIdentity::new("previous-validator", "v0").unwrap();
    let mut state = KanjiBatch::new(
        "revalidate-human-valid".into(),
        vec![identity('元')],
        old.clone(),
    )
    .unwrap();
    state.policy = crate::batch::AggregatePolicy::single_candidate(old.clone());
    let mut runtime = BatchRuntime::open(fixture.store.root(), "revalidate-human-valid").unwrap();
    let candidate = runtime
        .persist_candidate(
            &bytes,
            ValidationRecord {
                status: SemanticStatus::Uncertain,
                validator: old,
                content_sha256: sha256_hex(&bytes),
                evidence: vec![ValidationEvidence {
                    kind: "previous_uncertain_result".into(),
                    summary: "прежняя версия оставила кандидата человеку".into(),
                    details: None,
                }],
            },
            true,
        )
        .unwrap();
    let sha256 = candidate.sha256.clone();
    state
        .record_attempt(&identity('元'), BatchAttemptInput::Candidate { candidate })
        .unwrap();
    let decision = HumanBatchDecision {
        identity: identity('元'),
        candidate_sha256: sha256.clone(),
        action: HumanBatchAction::Confirm,
        reason: "подтверждение относится к неизменному изображению".into(),
    };
    state.decide(decision.clone(), &bytes).unwrap();
    assert_eq!(state.items[0].status, BatchItemStatus::HumanVerified);
    assert!(state.items[0].published_sha256.is_none());
    runtime.save(&state).unwrap();
    drop(runtime);

    let (response, code) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Revalidate {
            batch_id: "revalidate-human-valid".into(),
        },
        false,
    )
    .unwrap();

    assert_eq!(code, 0);
    let item = &response.batch.items[0];
    assert_eq!(item.status, BatchItemStatus::HumanVerified);
    assert_eq!(item.current_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(item.human_decisions, vec![decision]);
    assert!(item.published_sha256.is_none());
    assert_eq!(item.attempts.len(), 1);
    let BatchAttemptInput::Candidate { candidate } = &item.attempts[0].result else {
        unreachable!()
    };
    assert!(candidate.technically_valid);
    assert_eq!(candidate.automated.content_sha256, sha256);
    response.batch.validate().unwrap();
}

#[test]
fn revalidate_saves_corrupt_human_candidate_as_unconfirmed_and_keeps_history() {
    let fixture = Fixture::new();
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(b"truncated image");
    let old = ValidatorIdentity::new("previous-validator", "v0").unwrap();
    let mut state = KanjiBatch::new(
        "revalidate-human-corrupt".into(),
        vec![identity('漢')],
        old.clone(),
    )
    .unwrap();
    state.policy = crate::batch::AggregatePolicy::single_candidate(old.clone());
    let mut runtime = BatchRuntime::open(fixture.store.root(), "revalidate-human-corrupt").unwrap();
    let candidate = runtime
        .persist_candidate(
            &bytes,
            ValidationRecord {
                status: SemanticStatus::Uncertain,
                validator: old,
                content_sha256: sha256_hex(&bytes),
                evidence: vec![ValidationEvidence {
                    kind: "previous_image_check".into(),
                    summary: "устаревшая проверка считала байты пригодными".into(),
                    details: None,
                }],
            },
            true,
        )
        .unwrap();
    let sha256 = candidate.sha256.clone();
    state
        .record_attempt(&identity('漢'), BatchAttemptInput::Candidate { candidate })
        .unwrap();
    let decision = HumanBatchDecision {
        identity: identity('漢'),
        candidate_sha256: sha256.clone(),
        action: HumanBatchAction::Confirm,
        reason: "историческое подтверждение точного SHA".into(),
    };
    state.decide(decision.clone(), &bytes).unwrap();
    state
        .mark_published_ready(&identity('漢'), &sha256, BatchTrustSource::Human)
        .unwrap();
    runtime.save(&state).unwrap();
    drop(runtime);

    let command = BatchCommand::Revalidate {
        batch_id: "revalidate-human-corrupt".into(),
    };
    let (response, code) =
        execute_command(&fixture.store, fixture.summary(), &command, false).unwrap();
    assert_eq!(code, 0);
    let migrated = response.batch;
    let item = &migrated.items[0];
    assert_eq!(item.status, BatchItemStatus::Unresolved);
    assert_eq!(item.current_sha256.as_deref(), Some(sha256.as_str()));
    assert_eq!(item.human_decisions, vec![decision.clone()]);
    assert!(item.published_sha256.is_none());
    assert!(item.publication_source.is_none());
    assert_eq!(item.attempts.len(), 1);
    let BatchAttemptInput::Candidate { candidate } = &item.attempts[0].result else {
        unreachable!()
    };
    assert_eq!(candidate.automated.status, SemanticStatus::Corrupt);
    assert!(!candidate.technically_valid);
    assert!(!item.is_ready());
    migrated.validate().unwrap();

    let mut runtime = BatchRuntime::open(fixture.store.root(), "revalidate-human-corrupt").unwrap();
    let reopened = runtime.load().unwrap().unwrap();
    assert_eq!(reopened, migrated);
    let BatchAttemptInput::Candidate { candidate } = &reopened.items[0].attempts[0].result else {
        unreachable!()
    };
    assert_eq!(runtime.read_candidate(candidate).unwrap(), bytes);
    drop(runtime);

    let (repeated, repeated_code) =
        execute_command(&fixture.store, fixture.summary(), &command, false).unwrap();
    assert_eq!(repeated_code, 0);
    assert_eq!(repeated.batch.items[0].status, BatchItemStatus::Unresolved);
    assert_eq!(
        repeated.batch.items[0].current_sha256.as_deref(),
        Some(sha256.as_str())
    );
    assert_eq!(repeated.batch.items[0].human_decisions, vec![decision]);
    assert_eq!(repeated.batch.items[0].attempts.len(), 1);
    assert!(repeated.batch.items[0].published_sha256.is_none());
    repeated.batch.validate().unwrap();
    assert!(fixture.store.verify_integrity().unwrap().is_empty());
}

#[test]
fn revalidate_preserves_existing_owner_sha_until_owner_trust_changes() {
    let fixture = Fixture::new();
    fixture.start("existing-source", &['元']);
    let (published, _, _) = run_batch(&fixture.store, "existing-source", 1, |characters| {
        Ok(characters
            .iter()
            .map(|character| Ok(media(character, glyph('元'))))
            .collect())
    })
    .unwrap();
    let sha256 = published.items[0].published_sha256.clone().unwrap();
    fixture.start("existing-reuse", &['元']);
    assert_eq!(
        fixture.load("existing-reuse").items[0].status,
        BatchItemStatus::ExistingVerified
    );

    let (revalidated, code) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Revalidate {
            batch_id: "existing-reuse".into(),
        },
        false,
    )
    .unwrap();
    assert_eq!(code, 0);
    assert_eq!(
        revalidated.batch.items[0].status,
        BatchItemStatus::ExistingVerified
    );
    assert_eq!(
        revalidated.batch.items[0].published_sha256.as_deref(),
        Some(sha256.as_str())
    );
    assert_eq!(revalidated.batch.items[0].attempts.len(), 0);

    let (_, issues) = decide_exact(
        &fixture.store,
        "existing-source",
        HumanBatchDecision {
            identity: identity('元'),
            candidate_sha256: sha256,
            action: HumanBatchAction::Reject,
            reason: "проверка смены доверия владельца".into(),
        },
    )
    .unwrap();
    assert!(issues.is_empty());

    let (invalidated, code) = execute_command(
        &fixture.store,
        fixture.summary(),
        &BatchCommand::Revalidate {
            batch_id: "existing-reuse".into(),
        },
        false,
    )
    .unwrap();
    assert_eq!(code, 3);
    let item = &invalidated.batch.items[0];
    assert_eq!(item.status, BatchItemStatus::Reacquire);
    assert!(item.current_sha256.is_none());
    assert!(item.published_sha256.is_none());
    assert!(item.publication_source.is_none());
    assert_eq!(item.attempts.len(), 0);
    assert_eq!(item.generation, 1);
    assert!(!item.observed_owner_rejections.is_empty());
    invalidated.batch.validate().unwrap();
}

#[test]
fn failed_run_never_enters_opted_in_review_phase() {
    let fixture = Fixture::new();
    let mut spy = spy();
    let command = Cli::try_parse_from([
        "kanji-assets",
        "batch",
        "run",
        "--batch-id",
        "missing-review",
        "--interactive-review",
    ])
    .unwrap();
    let Command::Batch { command } = command.command else {
        unreachable!()
    };
    let mut progress = CapturedProgress::default();
    let output = execute_with_progress_and_interaction(
        &fixture.store,
        fixture.summary(),
        &command,
        OutputFormat::Human,
        false,
        &mut progress,
        &mut spy,
    );
    assert_ne!(output.exit_code, 0);
    assert_eq!((spy.asks, spy.opens), (0, 0));
}

// Совместимость сохранённых v2-пакетов проверяется настоящим путём публикации.
#[test]
fn legacy_aggregate_publication_is_exact_versioned_and_decoded() {
    let fixture = Fixture::new();
    fixture.start("aggregate", &['漢']);
    // Байты должны приниматься рабочим валидатором: агрегат не повышает кандидата
    // REJECTED, поэтому синтетика строится на эталоне 漢, а независимые метрики
    // задаются сохранёнными свидетельствами.
    let first = glyph('漢');
    let mut image = image::load_from_memory(&first).unwrap().to_rgba8();
    image.put_pixel(0, 0, image::Rgba([254, 255, 255, 255]));
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let second = encoded.into_inner();
    let mut runtime = BatchRuntime::open(fixture.store.root(), "aggregate").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    state.policy = crate::batch::AggregatePolicy::current(state.policy.validator.clone());
    for (bytes, distance, margin) in [(&first, 0.025, 0.010), (&second, 0.005, 0.005)] {
        let record = ValidationRecord {
            status: SemanticStatus::Uncertain,
            validator: KanjiImageValidator::validator_identity(),
            content_sha256: sha256_hex(bytes),
            evidence: vec![ValidationEvidence {
                kind: "pixel_reference_comparison".into(),
                summary: "синтетические независимые метрики".into(),
                details: Some(
                    serde_json::json!({"expected_distance": distance, "nearest_margin": margin, "nearest_other": "元"}),
                ),
            }],
        };
        let candidate = runtime.persist_candidate(bytes, record, true).unwrap();
        state
            .record_attempt(&identity('漢'), BatchAttemptInput::Candidate { candidate })
            .unwrap();
    }
    runtime.save(&state).unwrap();
    assert!(!state.is_resolved());
    drop(runtime);
    let (state, _, issues) = run_batch(&fixture.store, "aggregate", 5, |_| {
        panic!("публикация агрегата должна продолжиться с точных байтов")
    })
    .unwrap();
    assert!(issues.is_empty());
    assert!(state.is_resolved());
    assert_eq!(
        state.items[0].publication_source,
        Some(BatchTrustSource::Aggregate)
    );
    let read = read_kanji_verified(
        fixture.store.root(),
        &[identity('漢')],
        &KanjiImageValidator::validator_identity(),
    )
    .unwrap();
    assert_eq!(read[0].bytes, second);
    let evidence = &read[0].record.validation.as_ref().unwrap().evidence;
    assert!(
        evidence
            .iter()
            .any(|evidence| evidence.kind == "kanji_batch_aggregate")
    );
    assert!(
        evidence
            .iter()
            .any(|evidence| evidence.kind == "pixel_reference_comparison")
    );
}

// Совместимость сохранённых v2-пакетов проверяется настоящим путём публикации.
#[test]
fn legacy_rejected_candidate_never_votes_or_publishes_as_aggregate() {
    let fixture = Fixture::new();
    fixture.start("rejected", &['漢']);
    let base = glyph('漢');
    let variants: Vec<Vec<u8>> = (0..5u8)
        .map(|index| {
            let mut image = image::load_from_memory(&base).unwrap().to_rgba8();
            image.put_pixel(0, 0, image::Rgba([index + 1, 111, 222, 255]));
            let mut encoded = Cursor::new(Vec::new());
            image
                .write_to(&mut encoded, image::ImageFormat::Png)
                .unwrap();
            encoded.into_inner()
        })
        .collect();
    let mut hashes: Vec<_> = variants.iter().map(sha256_hex).collect();
    hashes.sort();
    hashes.dedup();
    assert_eq!(
        hashes.len(),
        5,
        "синтетические кандидаты должны различаться"
    );
    // Один REJECTED с привлекательными метриками и четыре UNCERTAIN: прежний
    // фильтр включал REJECTED в среднее, поэтому порог проходился, а выбирался
    // именно отклонённый кандидат с минимальной дистанцией.
    let plan = [
        (SemanticStatus::Rejected, 0.015, -0.015),
        (SemanticStatus::Uncertain, 0.021, 0.009),
        (SemanticStatus::Uncertain, 0.021, 0.009),
        (SemanticStatus::Uncertain, 0.021, 0.009),
        (SemanticStatus::Uncertain, 0.021, 0.009),
    ];
    let mut runtime = BatchRuntime::open(fixture.store.root(), "rejected").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    state.policy = crate::batch::AggregatePolicy::current(state.policy.validator.clone());
    for (bytes, (status, distance, margin)) in variants.iter().zip(plan) {
        let record = ValidationRecord {
            status,
            validator: KanjiImageValidator::validator_identity(),
            content_sha256: sha256_hex(bytes),
            evidence: vec![ValidationEvidence {
                kind: "pixel_reference_comparison".into(),
                summary: "синтетические независимые метрики".into(),
                details: Some(
                    serde_json::json!({"expected_distance": distance, "nearest_margin": margin, "nearest_other": "漠"}),
                ),
            }],
        };
        let candidate = runtime.persist_candidate(bytes, record, true).unwrap();
        state
            .record_attempt(&identity('漢'), BatchAttemptInput::Candidate { candidate })
            .unwrap();
    }
    let rejected_hash = sha256_hex(&variants[0]);
    runtime.save(&state).unwrap();
    drop(runtime);
    let (state, _, _) = run_batch(&fixture.store, "rejected", 5, |_| {
        panic!("возобновление не должно запускать получение")
    })
    .unwrap();
    let item = &state.items[0];
    assert!(!item.aggregate.accepted);
    assert_eq!(item.aggregate.distinct_valid_hashes.len(), 4);
    assert!(
        !item
            .aggregate
            .distinct_valid_hashes
            .contains(&rejected_hash)
    );
    assert_ne!(
        item.aggregate.selected_sha256.as_deref(),
        Some(rejected_hash.as_str())
    );
    assert_ne!(item.status, BatchItemStatus::AutoVerified);
    assert!(item.published_sha256.is_none());
    assert_eq!(item.status, BatchItemStatus::AwaitingHuman);
    assert!(
        fixture
            .store
            .verify_integrity()
            .unwrap()
            .iter()
            .all(|record| record.sha256 != rejected_hash),
        "отклонённые байты не должны попасть в канонический корпус"
    );
}
