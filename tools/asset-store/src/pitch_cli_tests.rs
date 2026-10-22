use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use tracing::Instrument;
use tracing::instrument::WithSubscriber;

use super::{
    CorpusCommand, OutputFormat, PitchBatchCommand, PitchCli, PitchCommand, PitchPlanItem,
    StoreSummary, create_batch, execute, load_batch, reject_batch, run_batch,
    validate_store_boundary,
};
use crate::browser_runtime::{BrowserExecutableSource, BrowserRuntimeProvenance};
use crate::error::ErrorCode;
use crate::hashing::sha256_hex;
use crate::jpdb::{
    JpdbPitchAcquired, JpdbPitchFailure, JpdbPitchOutcome, JpdbPitchQuery, JpdbPitchRequest,
    JpdbPitchStage, JpdbVocabularyCandidate,
};
use crate::model::{AssetIdentity, HumanDecision, LifecycleState, Provenance, SemanticStatus};
use crate::pitch_accent::{
    PitchAccentDomainMetadata, PitchAccentDomainPolicy, PitchAccentImageValidator,
    PitchAccentResolvedForm,
};
use crate::pitch_batch::{
    PitchAccentBatch, PitchAccentBatchRuntime, PitchBatchItemStatus, PitchBatchOwnerSnapshot,
};
use crate::store::{AssetStore, StoreOptions, VerifiedIngestRequest};
use crate::temp_workspace::TempWorkspace;

#[test]
fn panic_hook_subprocess_writes_sanitized_jsonl_to_real_stderr() {
    let parent_workspace = TempWorkspace::create("pitch-panic-hook-parent").unwrap();
    let response_path = parent_workspace.path().join("response.json");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "panic_hook_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("ASSET_STORE_PITCH_PANIC_HOOK_CHILD", "1")
        .env("ASSET_STORE_PITCH_PANIC_HOOK_RESPONSE", &response_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "дочерняя проверка паники завершилась ошибкой"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr дочернего процесса — UTF-8");
    let lines = stderr
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert!(
        !lines.is_empty(),
        "обработчик записал сообщение в настоящий stderr"
    );
    let events = lines
        .iter()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("строка stderr — JSON"))
        .collect::<Vec<_>>();
    assert_eq!(events[0]["schema_version"], 1);
    assert_eq!(events[0]["event"], "panic");
    assert_eq!(events[0].as_object().unwrap().len(), 3);
    assert!(!stderr.contains("secret-value"));

    let stdout = String::from_utf8(output.stdout).expect("stdout дочернего процесса — UTF-8");
    let response_text = fs::read_to_string(&response_path).unwrap();
    assert!(
        stdout.contains(&response_text),
        "итоговый JSON выведен в stdout процесса"
    );
    let response: serde_json::Value = serde_json::from_str(&response_text).unwrap();
    assert_eq!(response["error"]["code"], "validator_failure");
    assert_eq!(
        response["error"]["details"]["run_stop_reason"],
        "worker_panic"
    );
    assert!(
        !response["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("secret-value")
    );
    parent_workspace.close().unwrap();
}

#[test]
fn panic_hook_child_process() {
    if std::env::var_os("ASSET_STORE_PITCH_PANIC_HOOK_CHILD").is_none() {
        return;
    }

    super::install_safe_panic_hook(OutputFormat::Json);
    let workspace = TempWorkspace::create("pitch-panic-hook-subprocess").unwrap();
    let workspace_path = workspace.path().to_path_buf();
    let payload = std::panic::catch_unwind(|| panic!("token=secret-value"))
        .expect_err("паника должна быть перехвачена");
    let error = super::pitch_run_stopped("worker_panic", super::pitch_panic_message(payload));
    let output = super::render_error("batch_run".into(), None, error, OutputFormat::Json, false);
    assert_eq!(output.exit_code, 5);
    let response: serde_json::Value =
        serde_json::from_str(&output.stdout).expect("итоговый ответ — JSON");
    assert_eq!(response["error"]["code"], "validator_failure");
    assert_eq!(
        response["error"]["details"]["run_stop_reason"],
        "worker_panic"
    );
    assert!(!output.stdout.contains("secret-value"));
    workspace.close().unwrap();
    assert!(
        !workspace_path.exists(),
        "временное дерево удалено после перехвата паники"
    );
    let response_path = std::env::var_os("ASSET_STORE_PITCH_PANIC_HOOK_RESPONSE")
        .expect("путь ответа передан родительским тестом");
    fs::write(response_path, &output.stdout).unwrap();
    std::io::Write::write_all(&mut std::io::stdout().lock(), output.stdout.as_bytes()).unwrap();
}

#[test]
fn human_output_does_not_append_a_second_diagnostic_log_path() {
    for (stdout, stderr, exit_code) in [("завершено\n", "", 0), ("", "ошибка\n", 1)]
    {
        let output = super::attach_diagnostic_log(
            super::PitchCliOutput {
                stdout: stdout.into(),
                stderr: stderr.into(),
                exit_code,
            },
            OutputFormat::Human,
            Some("/tmp/diagnostic.jsonl"),
        );

        assert_eq!(output.stdout, stdout);
        assert_eq!(output.stderr, stderr);
    }
}

struct TemporaryRoot {
    workspace: TempWorkspace,
}

impl TemporaryRoot {
    fn new() -> Self {
        Self {
            workspace: TempWorkspace::create("asset-store-pitch-cli-tests").unwrap(),
        }
    }

    fn path(&self) -> &std::path::Path {
        self.workspace.path()
    }
}

#[test]
fn workspace_root_does_not_require_decks_and_supports_git_worktree_file() {
    let workspace = temp_root();
    let root = workspace.path();
    fs::write(root.join("Cargo.toml"), "[workspace]\n").unwrap();
    fs::write(root.join(".git"), "gitdir: /tmp/fixture\n").unwrap();
    let nested = root.join("tools/component");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("Cargo.toml"), "[package]\n").unwrap();
    let canonical = fs::canonicalize(root).unwrap();
    assert_eq!(super::find_workspace_root(&nested), Some(canonical.clone()));
    assert_eq!(
        super::store_path(None, &nested),
        canonical.join(".asset-store/pitch-accent")
    );
    fs::create_dir(root.join("decks")).unwrap();
    assert_eq!(super::find_workspace_root(&nested), Some(canonical));
}

fn temp_root() -> TemporaryRoot {
    TemporaryRoot::new()
}

fn browser() -> BrowserRuntimeProvenance {
    BrowserRuntimeProvenance {
        product: "Chrome/140".into(),
        protocol_version: "1.3".into(),
        revision: "1234567".into(),
        user_agent: "Mozilla/5.0 Chrome/140".into(),
        js_version: "V8 14.0".into(),
        executable_source: BrowserExecutableSource::PathLookup,
    }
}

fn metadata(surface: &str, reading: &str, vocabulary_id: u64) -> PitchAccentDomainMetadata {
    crate::pitch_accent::test_support::single_variant_metadata(
        surface,
        reading,
        vocabulary_id,
        format!("https://jpdb.io/vocabulary/{vocabulary_id}/{surface}/{reading}"),
        browser(),
    )
}

fn png() -> Vec<u8> {
    crate::pitch_accent::test_support::single_variant_png(false)
}

fn store_at(root: &std::path::Path) -> AssetStore {
    AssetStore::open_with_policy(
        StoreOptions::new(root.join("store")),
        PitchAccentDomainPolicy,
    )
    .unwrap()
}

fn save_durable_candidate(
    store: &AssetStore,
    batch_id: &str,
    surface: &str,
) -> (Vec<u8>, PitchAccentDomainMetadata, String) {
    let reading = "ゆうれい";
    let bytes = png();
    let metadata = metadata(surface, reading, 123);
    let mut batch = PitchAccentBatch::new(
        batch_id,
        vec![JpdbPitchRequest::new(JpdbPitchQuery::new(
            surface,
            Some(reading.into()),
        ))],
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(store.root(), &batch).unwrap();
    let token = batch.item_token(surface).unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Acquired {
                    asset: Box::new(JpdbPitchAcquired {
                        bytes: bytes.clone(),
                        metadata: metadata.clone(),
                    }),
                },
            )
            .unwrap()
    );
    let sha256 = sha256_hex(&bytes);
    drop(runtime);
    (bytes, metadata, sha256)
}

#[tokio::test]
async fn run_resumes_a_durable_candidate_without_reacquisition_and_resolved_rerun_is_noop() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let (bytes, _, expected_sha) = save_durable_candidate(&store, "candidate-resume", "幽霊");

    // Байты кандидата и состояние сохранены до этого запуска, как если бы
    // предыдущий процесс остановился перед публикацией. `resume` должен
    // опубликовать их из сохранённых данных runtime.
    let (batch, changed) = run_batch(
        &store,
        "candidate-resume",
        OutputFormat::Json,
        "batch_run",
        1,
    )
    .await
    .unwrap();
    assert!(changed);
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::Published);
    assert_eq!(item.attempts.len(), 1);
    assert_eq!(
        item.published_sha256.as_deref(),
        Some(expected_sha.as_str())
    );

    let assets = store.verify_integrity().unwrap();
    let canonical = assets
        .iter()
        .find(|asset| asset.identity.key == "幽霊")
        .unwrap();
    assert_eq!(canonical.sha256, expected_sha);
    assert_eq!(canonical.storage_path, "assets/png/幽霊.png");
    assert_eq!(canonical.consumer_filename, "幽霊.pitch.png");
    assert_eq!(
        fs::read(store.root().join(&canonical.storage_path)).unwrap(),
        bytes
    );

    // Второй запуск видит уже подтверждённую текущую запись владельца, не открывает браузер
    // и не добавляет новую попытку получения.
    let (rerun, changed) = run_batch(
        &store,
        "candidate-resume",
        OutputFormat::Json,
        "batch_run",
        1,
    )
    .await
    .unwrap();
    assert!(!changed);
    assert_eq!(rerun.item("幽霊").unwrap().attempts.len(), 1);
    assert_eq!(
        rerun.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );

    drop(store);
}

#[tokio::test]
async fn run_reconciles_a_store_commit_after_restart_without_reacquisition() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let (bytes, metadata, expected_sha) =
        save_durable_candidate(&store, "owner-commit-resume", "幽霊");

    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "owner-commit-resume").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .begin_publication("幽霊", &expected_sha, None)
        .unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);

    // Имитируем сбой после фиксации изменения в хранилище и до итогового обновления состояния пакета.
    let identity = AssetIdentity::new("pitch_accent", "幽霊").unwrap();
    let result = store
        .ingest_verified(
            VerifiedIngestRequest {
                identity,
                bytes: bytes.clone(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_render".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(serde_json::to_value(&metadata).unwrap()),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    assert_eq!(result.status, SemanticStatus::Verified);
    assert_eq!(result.sha256, expected_sha);

    let (batch, _changed) = run_batch(
        &store,
        "owner-commit-resume",
        OutputFormat::Json,
        "batch_run",
        1,
    )
    .await
    .unwrap();
    let item = batch.item("幽霊").unwrap();
    assert_eq!(item.status(), PitchBatchItemStatus::Published);
    assert_eq!(item.attempts.len(), 1);
    assert_eq!(
        item.published_sha256.as_deref(),
        Some(expected_sha.as_str())
    );
    assert_eq!(store.verify_integrity().unwrap().len(), 1);

    drop(store);
}

#[test]
fn reject_command_attests_the_exact_current_owner_sha_and_quarantines_it() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let (bytes, metadata, expected_sha) =
        save_durable_candidate(&store, "reject-owner-sha", "幽霊");

    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "reject-owner-sha").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .begin_publication("幽霊", &expected_sha, None)
        .unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes,
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

    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "reject-owner-sha").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .reconcile_owner(
            &PitchBatchOwnerSnapshot::from_records(store.verify_integrity().unwrap()).unwrap(),
        )
        .unwrap();
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::Published
    );
    runtime.save(&batch).unwrap();
    drop(runtime);

    let summary = StoreSummary {
        path: store.root().display().to_string(),
        store_id: store.store_id().to_owned(),
    };
    let response = reject_batch(
        &store,
        "reject-owner-sha",
        "幽霊",
        &expected_sha,
        "при визуальной проверке отклонено это точное изображение".into(),
        summary,
        OutputFormat::Json,
    );
    assert_eq!(response.exit_code, 0);

    let records = store.verify_integrity().unwrap();
    let owner = records
        .iter()
        .find(|record| record.identity.key == "幽霊")
        .unwrap();
    assert_eq!(owner.sha256, expected_sha);
    assert_eq!(owner.lifecycle, LifecycleState::Quarantined);
    assert_eq!(owner.current_human_decision(), Some(HumanDecision::Reject));

    let batch = load_batch(&store, "reject-owner-sha").unwrap();
    let item = batch.item("幽霊").unwrap();
    assert!(
        item.rejected_candidates
            .iter()
            .any(|rejection| rejection.candidate_sha256 == expected_sha)
    );
    assert_eq!(item.status(), PitchBatchItemStatus::Conflict);
    assert_eq!(item.owner_conflict.as_ref().unwrap().code, "owner_rejected");

    drop(store);
}

fn cli(
    store: PathBuf,
    repository_root: PathBuf,
    output: OutputFormat,
    command: PitchCommand,
) -> PitchCli {
    PitchCli {
        store: Some(store),
        repository_root,
        output,
        command,
    }
}

#[tokio::test]
async fn pitch_cli_routes_human_errors_to_stderr_and_json_errors_to_stdout() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    drop(store);

    let success = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Human,
        PitchCommand::Corpus {
            command: CorpusCommand::List,
        },
    ))
    .await;
    assert_eq!(success.exit_code, 0);
    assert!(success.stdout.contains("Операция: список корпуса"));
    assert!(success.stderr.is_empty());

    let human_status = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "missing-status".into(),
            },
        },
    ))
    .await;
    assert_ne!(human_status.exit_code, 0);
    assert!(human_status.stdout.is_empty());
    assert!(
        human_status
            .stderr
            .contains("сохранённое состояние batch не найдено")
    );

    let human_run = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Run {
                batch_id: "missing-run".into(),
                workers: 1,
            },
        },
    ))
    .await;
    assert_ne!(human_run.exit_code, 0);
    assert!(human_run.stdout.is_empty());
    assert!(
        human_run
            .stderr
            .contains("сохранённое состояние batch не найдено")
    );

    let json_error = execute(cli(
        store_root,
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "missing-json".into(),
            },
        },
    ))
    .await;
    assert_eq!(json_error.exit_code, human_status.exit_code);
    assert!(json_error.stderr.is_empty());
    let response: serde_json::Value = serde_json::from_str(&json_error.stdout).unwrap();
    assert_eq!(response["outcome"], "failed");
    assert_eq!(
        response["error"]["message"],
        "сохранённое состояние batch не найдено"
    );
}

#[tokio::test]
async fn human_status_explains_technical_failure_ambiguity_and_selection() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    let validator = PitchAccentImageValidator::validator_identity();

    let request = JpdbPitchRequest::new(JpdbPitchQuery::new("幽霊", Some("ゆうれい".into())));
    let mut failed_batch =
        PitchAccentBatch::new("human-failure", vec![request.clone()], validator.clone()).unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(store.root(), &failed_batch).unwrap();
    let token = failed_batch.item_token("幽霊").unwrap();
    runtime
        .record_outcome(
            &mut failed_batch,
            &token,
            JpdbPitchOutcome::Failed {
                error: JpdbPitchFailure::Navigation {
                    stage: JpdbPitchStage::SearchNavigation,
                    message: "net::ERR_NAME_RESOLUTION_FAILED".into(),
                },
            },
        )
        .unwrap();
    drop(runtime);

    let failure_output = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "human-failure".into(),
            },
        },
    ))
    .await;
    assert!(failure_output.stdout.contains("техническая ошибка"));
    assert!(failure_output.stdout.contains("переход к поиску"));
    assert!(failure_output.stdout.contains("ERR_NAME_RESOLUTION_FAILED"));
    assert!(failure_output.stdout.contains("повтор допустим"));

    let candidates = vec![
        JpdbVocabularyCandidate {
            vocabulary_id: 123,
            surface_forms: vec!["幽霊".into()],
            readings: vec!["ゆうれい".into()],
            resolved_forms: vec![PitchAccentResolvedForm {
                surface: "幽霊".into(),
                reading: "ゆうれい".into(),
            }],
            part_of_speech: vec!["Noun".into()],
            meanings: vec!["ghost".into()],
            detail_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
        },
        JpdbVocabularyCandidate {
            vocabulary_id: 124,
            surface_forms: vec!["幽霊".into()],
            readings: vec!["ゆうれい".into()],
            resolved_forms: vec![PitchAccentResolvedForm {
                surface: "幽霊".into(),
                reading: "ゆうれい".into(),
            }],
            part_of_speech: vec!["Noun".into()],
            meanings: vec!["phantom".into()],
            detail_url: "https://jpdb.io/vocabulary/124/幽霊/ゆうれい".into(),
        },
    ];
    let mut ambiguous_batch =
        PitchAccentBatch::new("human-ambiguity", vec![request], validator).unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(store.root(), &ambiguous_batch).unwrap();
    let token = ambiguous_batch.item_token("幽霊").unwrap();
    runtime
        .record_outcome(
            &mut ambiguous_batch,
            &token,
            JpdbPitchOutcome::AmbiguousVocabulary {
                surface: "幽霊".into(),
                reading: Some("ゆうれい".into()),
                candidates,
            },
        )
        .unwrap();
    drop(runtime);

    let ambiguity_output = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "human-ambiguity".into(),
            },
        },
    ))
    .await;
    assert!(
        ambiguity_output
            .stdout
            .contains("Требуется выбрать точную запись JPDB")
    );
    assert!(ambiguity_output.stdout.contains("ID 123"));
    assert!(ambiguity_output.stdout.contains("ID 124"));
    assert!(ambiguity_output.stdout.contains("ghost"));

    let selection_output = execute(cli(
        store_root,
        root.to_path_buf(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::Select {
                batch_id: "human-ambiguity".into(),
                surface: "幽霊".into(),
                vocabulary_id: 123,
                detail_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
            },
        },
    ))
    .await;
    assert!(
        selection_output
            .stdout
            .contains("Выбранная запись JPDB: ID 123")
    );
    assert!(
        selection_output
            .stdout
            .contains("https://jpdb.io/vocabulary/123/幽霊/ゆうれい")
    );

    drop(store);
}

#[tokio::test]
async fn batch_start_noop_and_status_reconcile_report_persistent_changes() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    drop(store);
    let plan_path = root.join("plan.json");
    fs::write(
        &plan_path,
        r#"{"schema_version":1,"items":[{"surface":"幽霊","reading":"ゆうれい"}]}"#,
    )
    .unwrap();

    let start = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("changed-contract".into()),
                plan: plan_path.clone(),
            },
        },
    ))
    .await;
    let start_response: serde_json::Value = serde_json::from_str(&start.stdout).unwrap();
    assert_eq!(start_response["changed"], true);

    let repeated_start = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("changed-contract".into()),
                plan: plan_path,
            },
        },
    ))
    .await;
    let repeated_response: serde_json::Value =
        serde_json::from_str(&repeated_start.stdout).unwrap();
    assert_eq!(repeated_response["changed"], false);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let bytes = png();
    let metadata = metadata("幽霊", "ゆうれい", 123);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes,
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(serde_json::to_value(metadata).unwrap()),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    drop(store);

    let status = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "changed-contract".into(),
            },
        },
    ))
    .await;
    let status_response: serde_json::Value = serde_json::from_str(&status.stdout).unwrap();
    assert_eq!(status_response["changed"], true);
    assert_eq!(status_response["items"][0]["status"], "existing_verified");
    assert_eq!(
        status_response["batch"]["items"][0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    let repeated_status = execute(cli(
        store_root,
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Status {
                batch_id: "changed-contract".into(),
            },
        },
    ))
    .await;
    let repeated_status_response: serde_json::Value =
        serde_json::from_str(&repeated_status.stdout).unwrap();
    assert_eq!(repeated_status_response["changed"], false);
}

#[tokio::test]
async fn ensure_reports_each_new_runtime_batch_as_a_change_for_verified_canonical_items() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes: png(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(
                    serde_json::to_value(metadata("幽霊", "ゆうれい", 123)).unwrap(),
                ),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    drop(store);

    let ensure = || PitchCommand::Ensure {
        plan: None,
        surface: Some("幽霊".into()),
        reading: Some("ゆうれい".into()),
        vocabulary_id: None,
        detail_url: None,
        refresh: false,
    };
    let first = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        ensure(),
    ))
    .await;
    let first_response: serde_json::Value = serde_json::from_str(&first.stdout).unwrap();
    assert_eq!(first_response["changed"], true);
    assert_eq!(first_response["items"][0]["status"], "existing_verified");

    let second = execute(cli(
        store_root,
        root.to_path_buf(),
        OutputFormat::Json,
        ensure(),
    ))
    .await;
    let second_response: serde_json::Value = serde_json::from_str(&second.stdout).unwrap();
    assert_eq!(second_response["changed"], true);
    assert_eq!(second_response["items"][0]["status"], "existing_verified");
    assert_ne!(first_response["batch_id"], second_response["batch_id"]);
}

#[tokio::test]
async fn cli_reacquire_publishes_current_metadata_when_png_sha_repeats() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    let bytes = png();
    let expected_sha = sha256_hex(&bytes);
    let original_metadata = metadata("幽霊", "ゆうれい", 123);
    let current_metadata = metadata("幽霊", "ゆうれい", 456);
    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes: bytes.clone(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(serde_json::to_value(&original_metadata).unwrap()),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    let item = PitchPlanItem {
        surface: "幽霊".into(),
        reading: Some("ゆうれい".into()),
        selection: None,
    };
    let (batch, created) = create_batch(&store, "same-sha-metadata", &[item], None).unwrap();
    assert!(created);
    assert_eq!(
        batch.item("幽霊").unwrap().status(),
        PitchBatchItemStatus::ExistingVerified
    );
    drop(store);

    let reacquire = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Reacquire {
                batch_id: "same-sha-metadata".into(),
                surface: "幽霊".into(),
                reason: "повторная проверка источника".into(),
            },
        },
    ))
    .await;
    assert_eq!(reacquire.exit_code, 0);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "same-sha-metadata").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::Acquired {
                    asset: Box::new(JpdbPitchAcquired {
                        bytes: bytes.clone(),
                        metadata: current_metadata.clone(),
                    }),
                },
            )
            .unwrap()
    );
    assert_eq!(
        batch
            .item("幽霊")
            .unwrap()
            .current_candidate_sha256
            .as_deref(),
        Some(expected_sha.as_str())
    );
    drop(runtime);
    drop(store);

    let run = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Run {
                batch_id: "same-sha-metadata".into(),
                workers: 1,
            },
        },
    ))
    .await;
    assert_eq!(run.exit_code, 0, "{}", run.stdout);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let canonical = store
        .verify_integrity()
        .unwrap()
        .into_iter()
        .find(|record| record.identity.key == "幽霊")
        .unwrap();
    assert_eq!(canonical.sha256, expected_sha);
    assert_eq!(
        canonical.domain_metadata,
        Some(serde_json::to_value(&current_metadata).unwrap())
    );

    drop(store);
}

fn ambiguity_candidates() -> Vec<JpdbVocabularyCandidate> {
    [(123, "ghost"), (124, "phantom")]
        .into_iter()
        .map(|(vocabulary_id, meaning)| JpdbVocabularyCandidate {
            vocabulary_id,
            surface_forms: vec!["幽霊".into()],
            readings: vec!["ゆうれい".into()],
            resolved_forms: vec![PitchAccentResolvedForm {
                surface: "幽霊".into(),
                reading: "ゆうれい".into(),
            }],
            part_of_speech: vec!["Noun".into()],
            meanings: vec![meaning.into()],
            detail_url: format!("https://jpdb.io/vocabulary/{vocabulary_id}/幽霊/ゆうれい"),
        })
        .collect()
}

#[tokio::test]
async fn batch_start_uses_immutable_original_plan_identity_after_selection() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    drop(store);
    let original_plan_path = root.join("original-plan.json");
    let history_only_plan_path = root.join("history-only-plan.json");
    fs::write(
        &original_plan_path,
        r#"{"schema_version":1,"items":[{"surface":"幽霊","reading":"ゆうれい"}]}"#,
    )
    .unwrap();
    fs::write(
        &history_only_plan_path,
        r#"{"schema_version":1,"items":[{"surface":"幽霊","reading":"ゆうれい","selection":{"vocabulary_id":123,"detail_url":"https://jpdb.io/vocabulary/123/幽霊/ゆうれい"}}]}"#,
    )
    .unwrap();

    let start = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("immutable-plan".into()),
                plan: original_plan_path.clone(),
            },
        },
    ))
    .await;
    assert_eq!(start.exit_code, 0, "{}", start.stdout);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "immutable-plan").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    runtime
        .record_outcome(
            &mut batch,
            &token,
            JpdbPitchOutcome::AmbiguousVocabulary {
                surface: "幽霊".into(),
                reading: Some("ゆうれい".into()),
                candidates: ambiguity_candidates(),
            },
        )
        .unwrap();
    drop(runtime);
    drop(store);

    let first_selection = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Select {
                batch_id: "immutable-plan".into(),
                surface: "幽霊".into(),
                vocabulary_id: 123,
                detail_url: "https://jpdb.io/vocabulary/123/幽霊/ゆうれい".into(),
            },
        },
    ))
    .await;
    assert_eq!(first_selection.exit_code, 0, "{}", first_selection.stdout);

    let store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "immutable-plan").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    let token = batch.item_token("幽霊").unwrap();
    runtime
        .record_outcome(
            &mut batch,
            &token,
            JpdbPitchOutcome::AmbiguousVocabulary {
                surface: "幽霊".into(),
                reading: Some("ゆうれい".into()),
                candidates: ambiguity_candidates(),
            },
        )
        .unwrap();
    drop(runtime);
    drop(store);

    let second_selection = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Select {
                batch_id: "immutable-plan".into(),
                surface: "幽霊".into(),
                vocabulary_id: 124,
                detail_url: "https://jpdb.io/vocabulary/124/幽霊/ゆうれい".into(),
            },
        },
    ))
    .await;
    assert_eq!(second_selection.exit_code, 0, "{}", second_selection.stdout);

    let repeated_original = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("immutable-plan".into()),
                plan: original_plan_path,
            },
        },
    ))
    .await;
    assert_eq!(
        repeated_original.exit_code, 0,
        "{}",
        repeated_original.stdout
    );

    let history_only = execute(cli(
        store_root,
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Start {
                batch_id: Some("immutable-plan".into()),
                plan: history_only_plan_path,
            },
        },
    ))
    .await;
    assert_ne!(history_only.exit_code, 0);
    let response: serde_json::Value = serde_json::from_str(&history_only.stdout).unwrap();
    assert_eq!(response["error"]["code"], "identity_conflict");
}

#[test]
fn pitch_store_boundary_rejects_decks_overlap_and_accepts_external_store() {
    let workspace = temp_root();
    let root = workspace.path();
    let repository = root.join("repository");
    let decks = repository.join("decks");
    fs::create_dir_all(&decks).unwrap();

    assert!(validate_store_boundary(&decks.join("pitch-store"), &repository).is_err());
    assert!(validate_store_boundary(&repository, &repository).is_err());
    assert!(validate_store_boundary(&root.join("outside-store"), &repository).is_ok());

    let hostile_repository_root = decks.join("nested-checkout");
    fs::create_dir_all(&hostile_repository_root).unwrap();
    assert!(
        validate_store_boundary(&root.join("safe-looking-store"), &hostile_repository_root)
            .is_err()
    );
    assert!(
        validate_store_boundary(&root.join("outside-store"), &root.join("missing-checkout"))
            .is_err()
    );
    assert!(validate_store_boundary(&decks.join("hidden-store"), root).is_err());
}

#[cfg(unix)]
#[test]
fn pitch_store_boundary_rejects_symlink_alias_into_decks() {
    use std::os::unix::fs::symlink;

    let workspace = temp_root();

    let root = workspace.path();
    let repository = root.join("repository");
    let decks = repository.join("decks");
    fs::create_dir_all(&decks).unwrap();
    let alias = root.join("decks-alias");
    symlink(&decks, &alias).unwrap();

    assert!(validate_store_boundary(&alias.join("pitch-store"), &repository).is_err());
}

// Провайдер и сессия подменяются только на границе CLI; запись пакета,
// token/CAS, проверка кандидата и публикация остаются боевым кодом.
enum ScriptedPitchAction {
    Complete,
    WaitForHeartbeat(futures::channel::oneshot::Receiver<()>),
    InterruptInFlight(futures::channel::oneshot::Sender<()>),
    ReadyWithSignal(futures::channel::oneshot::Sender<()>),
    SessionFailure {
        after_outcome: bool,
    },
    SetupFailure,
    ItemFailure,
    ItemFailureWithSessionFailure,
    /// Нарушение инварианта протокола: два результата на одно задание.
    InvalidReport,
    Reacquire,
    /// Портит сохранённое состояние пакета прямо перед отдачей результата.
    CorruptRuntime,
    /// Устаревший токен вместе с отказом сессии в одном отчёте.
    ReacquireSessionFailure,
}

impl Clone for ScriptedPitchAction {
    fn clone(&self) -> Self {
        match self {
            Self::Complete => Self::Complete,
            Self::WaitForHeartbeat(_) => panic!("ожидание сигнала нельзя повторять"),
            Self::InterruptInFlight(_) => panic!("прерывание в полёте нельзя повторять"),
            Self::ReadyWithSignal(_) => panic!("сигнал готовности нельзя повторять"),
            Self::SessionFailure { after_outcome } => Self::SessionFailure {
                after_outcome: *after_outcome,
            },
            Self::SetupFailure => Self::SetupFailure,
            Self::ItemFailure => Self::ItemFailure,
            Self::ItemFailureWithSessionFailure => Self::ItemFailureWithSessionFailure,
            Self::InvalidReport => Self::InvalidReport,
            Self::Reacquire => Self::Reacquire,
            Self::CorruptRuntime => Self::CorruptRuntime,
            Self::ReacquireSessionFailure => Self::ReacquireSessionFailure,
        }
    }
}

struct ScriptedPitchDriver {
    store_root: PathBuf,
    batch_id: String,
    actions: std::collections::VecDeque<ScriptedPitchAction>,
    launches: usize,
    fail_launch_on: Option<usize>,
    launch_error: Option<super::BrowserLaunchError>,
    /// Все запуски с номером не больше указанного терпят сбой.
    fail_launch_until: Option<usize>,
    /// Сессии с указанным номером не удаётся закрыть.
    fail_close_on: Option<usize>,
    slow_launch: Option<(usize, futures::channel::oneshot::Receiver<()>)>,
    interrupt_during_launch: Option<futures::channel::oneshot::Sender<()>>,
    active: Option<usize>,
    closed: Vec<usize>,
    seen: Vec<(usize, String)>,
    attempts_at_start: Vec<usize>,
}

impl ScriptedPitchDriver {
    fn new(store: &AssetStore, batch_id: &str, actions: Vec<ScriptedPitchAction>) -> Self {
        Self {
            store_root: store.root().into(),
            batch_id: batch_id.into(),
            actions: actions.into(),
            launches: 0,
            fail_launch_on: None,
            launch_error: None,
            fail_launch_until: None,
            fail_close_on: None,
            slow_launch: None,
            interrupt_during_launch: None,
            active: None,
            closed: Vec::new(),
            seen: Vec::new(),
            attempts_at_start: Vec::new(),
        }
    }

    fn batch(&self) -> PitchAccentBatch {
        PitchAccentBatchRuntime::open(&self.store_root, &self.batch_id)
            .unwrap()
            .load()
            .unwrap()
            .unwrap()
    }
}

impl super::PitchRunDriver for ScriptedPitchDriver {
    type Session = usize;

    fn record_item_runtime_diagnostics(
        &self,
        session: &Self::Session,
        item: &crate::browser_diagnostics::BrowserItemTimer,
    ) {
        item.set_browser_session(*session as u64);
        item.record_runtime_snapshot(&crate::browser_runtime::RuntimeSnapshot::default());
    }

    async fn launch(&mut self) -> Result<Self::Session, super::BrowserLaunchError> {
        assert!(self.active.is_none(), "сессии не должны пересекаться");
        self.launches += 1;
        if self
            .slow_launch
            .as_ref()
            .is_some_and(|(index, _)| *index == self.launches)
        {
            let (_, permit) = self.slow_launch.take().unwrap();
            permit.await.unwrap();
        }
        if let Some(signal) = self.interrupt_during_launch.take() {
            signal.send(()).unwrap();
            // Дать обработчику увидеть сигнал до создания сессии браузера.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        if let Some(error) = self.launch_error.take() {
            return Err(error);
        }
        if self.fail_launch_on == Some(self.launches)
            || self
                .fail_launch_until
                .is_some_and(|limit| self.launches <= limit)
        {
            return Err(super::BrowserLaunchError::Setup(
                "сбой запуска в автономном сценарии".into(),
            ));
        }
        self.active = Some(self.launches);
        Ok(self.launches)
    }

    async fn acquire(
        &mut self,
        session: &Self::Session,
        request: &JpdbPitchRequest,
    ) -> crate::jpdb::JpdbPitchAcquisitionReport {
        assert_eq!(self.active, Some(*session));
        self.attempts_at_start.push(
            self.batch()
                .items
                .iter()
                .map(|item| item.attempts.len())
                .sum(),
        );
        self.seen.push((*session, request.query.surface.clone()));
        let action = self
            .actions
            .pop_front()
            .expect("запрос вне автономного сценария");
        let mut failure = None;
        match action {
            ScriptedPitchAction::Complete => {}
            ScriptedPitchAction::WaitForHeartbeat(permit) => permit.await.unwrap(),
            ScriptedPitchAction::InterruptInFlight(signal) => {
                signal.send(()).unwrap();
                std::future::pending::<()>().await;
            }
            ScriptedPitchAction::ReadyWithSignal(signal) => signal.send(()).unwrap(),
            ScriptedPitchAction::ItemFailure
            | ScriptedPitchAction::ItemFailureWithSessionFailure => {
                return crate::jpdb::JpdbPitchAcquisitionReport {
                    outcomes: vec![JpdbPitchOutcome::Failed {
                        error: JpdbPitchFailure::Timeout {
                            stage: JpdbPitchStage::SearchNavigation,
                            diagnostic: Some("тайм-аут элемента в автономной проверке".into()),
                        },
                    }],
                    session_failure: matches!(
                        action,
                        ScriptedPitchAction::ItemFailureWithSessionFailure
                    )
                    .then(|| JpdbPitchFailure::SessionFailure {
                        stage: JpdbPitchStage::SearchNavigation,
                        message: "отказ сессии после сбоя элемента в автономной проверке".into(),
                    }),
                };
            }
            ScriptedPitchAction::SetupFailure => {
                return crate::jpdb::JpdbPitchAcquisitionReport {
                    outcomes: Vec::new(),
                    session_failure: Some(JpdbPitchFailure::BrowserConfiguration {
                        stage: JpdbPitchStage::ConfigureBrowser,
                        message: "сбой подготовки страницы в автономной проверке".into(),
                    }),
                };
            }
            ScriptedPitchAction::ReacquireSessionFailure => {
                failure = Some(JpdbPitchFailure::SessionFailure {
                    stage: JpdbPitchStage::SearchNavigation,
                    message: "остановка телеметрии в автономной проверке".into(),
                });
                reacquire_for_test(&self.store_root, &self.batch_id, &request.query.surface);
            }
            ScriptedPitchAction::SessionFailure { after_outcome } => {
                failure = Some(JpdbPitchFailure::SessionFailure {
                    stage: JpdbPitchStage::SearchNavigation,
                    message: "остановка телеметрии в автономной проверке".into(),
                });
                if !after_outcome {
                    return crate::jpdb::JpdbPitchAcquisitionReport {
                        outcomes: Vec::new(),
                        session_failure: failure,
                    };
                }
            }
            ScriptedPitchAction::InvalidReport => {
                let surface = request.query.surface.clone();
                return crate::jpdb::JpdbPitchAcquisitionReport {
                    outcomes: vec![
                        JpdbPitchOutcome::VocabularyNotFound {
                            surface: surface.clone(),
                            reading: request.query.reading.clone(),
                        },
                        JpdbPitchOutcome::VocabularyNotFound {
                            surface,
                            reading: request.query.reading.clone(),
                        },
                    ],
                    session_failure: None,
                };
            }
            ScriptedPitchAction::Reacquire => {
                reacquire_for_test(&self.store_root, &self.batch_id, &request.query.surface);
            }
            ScriptedPitchAction::CorruptRuntime => {
                fs::write(
                    self.store_root
                        .join(".runtime/batches")
                        .join(&self.batch_id)
                        .join("state.json"),
                    "недействительное состояние batch".as_bytes(),
                )
                .unwrap();
            }
        }
        crate::jpdb::JpdbPitchAcquisitionReport {
            outcomes: vec![JpdbPitchOutcome::VocabularyNotFound {
                surface: request.query.surface.clone(),
                reading: request.query.reading.clone(),
            }],
            session_failure: failure,
        }
    }

    async fn close(&mut self, session: Self::Session) {
        assert_eq!(self.active.take(), Some(session));
        self.closed.push(session);
    }

    async fn close_checked(&mut self, session: Self::Session) -> Result<(), String> {
        self.close(session).await;
        if self.fail_close_on == Some(session) {
            return Err("сбой закрытия сессии в автономном сценарии".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ParallelPitchAttempt {
    worker: u32,
    worker_session: u32,
    session: u32,
    surface: String,
}

#[derive(Default)]
struct ParallelPitchTraceState {
    active: usize,
    max_active: usize,
    next_session: u32,
    starts: Vec<ParallelPitchAttempt>,
    closed: Vec<ParallelPitchAttempt>,
    finished: Vec<u32>,
    workspace_paths: Vec<PathBuf>,
}

#[derive(Clone, Default)]
struct ParallelPitchTrace(Arc<Mutex<ParallelPitchTraceState>>);

impl ParallelPitchTrace {
    fn begin(&self, attempt: ParallelPitchAttempt) -> ActivePitchAcquire {
        let mut state = self.0.lock().unwrap();
        state.active += 1;
        state.max_active = state.max_active.max(state.active);
        state.starts.push(attempt);
        ActivePitchAcquire(self.clone())
    }

    fn launch(&self) -> u32 {
        let mut state = self.0.lock().unwrap();
        state.next_session += 1;
        state.next_session
    }

    fn close(&self, attempt: ParallelPitchAttempt) {
        self.0.lock().unwrap().closed.push(attempt);
    }

    fn finish(&self, worker: u32, workspace: PathBuf) {
        let mut state = self.0.lock().unwrap();
        state.finished.push(worker);
        state.workspace_paths.push(workspace);
    }

    fn snapshot(&self) -> ParallelPitchTraceStateSnapshot {
        let state = self.0.lock().unwrap();
        ParallelPitchTraceStateSnapshot {
            active: state.active,
            max_active: state.max_active,
            starts: state.starts.clone(),
            closed: state.closed.clone(),
            finished: state.finished.clone(),
            workspace_paths: state.workspace_paths.clone(),
        }
    }
}

struct ActivePitchAcquire(ParallelPitchTrace);

impl Drop for ActivePitchAcquire {
    fn drop(&mut self) {
        let mut state = self.0.0.lock().unwrap();
        state.active -= 1;
    }
}

struct ParallelPitchTraceStateSnapshot {
    active: usize,
    max_active: usize,
    starts: Vec<ParallelPitchAttempt>,
    closed: Vec<ParallelPitchAttempt>,
    finished: Vec<u32>,
    workspace_paths: Vec<PathBuf>,
}

enum ParallelPitchAction {
    Complete,
    /// Ровно `failures` последовательных отказов сессии на этом элементе, затем успех.
    SessionFailureTimes {
        failures: usize,
        started: Option<futures::channel::oneshot::Sender<()>>,
    },
    Wait {
        started: Option<futures::channel::oneshot::Sender<()>>,
        release: futures::channel::oneshot::Receiver<()>,
    },
    /// Ждёт разрешения, затем отказывает по сессии, не создавая попытку.
    WaitFailure {
        started: Option<futures::channel::oneshot::Sender<()>>,
        release: futures::channel::oneshot::Receiver<()>,
    },
}

/// Повторное получение в автономном сценарии: делает выданный токен устаревшим.
fn reacquire_for_test(store_root: &std::path::Path, batch_id: &str, surface: &str) {
    let mut runtime = PitchAccentBatchRuntime::open(store_root, batch_id).unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .reacquire(surface, "новое действие пользователя".into())
        .unwrap();
    runtime.save(&batch).unwrap();
}

struct ParallelPitchSession {
    worker: u32,
    worker_session: u32,
    session: u32,
}

struct ParallelPitchDriver {
    worker: u32,
    worker_session_count: u32,
    active_session: Option<u32>,
    trace: ParallelPitchTrace,
    actions: std::collections::BTreeMap<String, ParallelPitchAction>,
    workspace: Option<TempWorkspace>,
    fail_finish: bool,
    /// Все запуски с номером не больше указанного терпят сбой.
    fail_launch_until: Option<usize>,
}

impl ParallelPitchDriver {
    fn new(
        worker: u32,
        trace: ParallelPitchTrace,
        actions: impl IntoIterator<Item = (String, ParallelPitchAction)>,
    ) -> Self {
        let workspace = TempWorkspace::create("pitch-parallel-synthetic-worker").unwrap();
        Self {
            worker,
            worker_session_count: 0,
            active_session: None,
            trace,
            actions: actions.into_iter().collect(),
            workspace: Some(workspace),
            fail_finish: false,
            fail_launch_until: None,
        }
    }

    fn complete_actions(surfaces: &[&str]) -> Vec<(String, ParallelPitchAction)> {
        surfaces
            .iter()
            .map(|surface| ((*surface).into(), ParallelPitchAction::Complete))
            .collect()
    }
}

impl super::PitchRunDriver for ParallelPitchDriver {
    type Session = ParallelPitchSession;

    fn record_item_runtime_diagnostics(
        &self,
        session: &Self::Session,
        item: &crate::browser_diagnostics::BrowserItemTimer,
    ) {
        item.set_browser_session(u64::from(session.session));
        item.record_runtime_snapshot(&crate::browser_runtime::RuntimeSnapshot::default());
    }

    async fn launch(&mut self) -> Result<Self::Session, super::BrowserLaunchError> {
        assert!(
            self.active_session.is_none(),
            "у исполнителя не может быть двух сессий"
        );
        self.worker_session_count += 1;
        let worker_session = self.worker_session_count;
        if self
            .fail_launch_until
            .is_some_and(|limit| self.worker_session_count as usize <= limit)
        {
            return Err(super::BrowserLaunchError::Setup(
                "сбой запуска в параллельном сценарии".into(),
            ));
        }
        let session = self.trace.launch();
        self.active_session = Some(worker_session);
        Ok(ParallelPitchSession {
            worker: self.worker,
            worker_session,
            session,
        })
    }

    async fn acquire(
        &mut self,
        session: &Self::Session,
        request: &JpdbPitchRequest,
    ) -> crate::jpdb::JpdbPitchAcquisitionReport {
        assert_eq!(self.worker, session.worker);
        assert_eq!(self.active_session, Some(session.worker_session));
        let surface = request.query.surface.clone();
        let _active = self.trace.begin(ParallelPitchAttempt {
            worker: self.worker,
            worker_session: session.worker_session,
            session: session.session,
            surface: surface.clone(),
        });
        let action = self.actions.remove(&surface).unwrap_or_else(|| {
            panic!(
                "исполнитель {} получил неожиданную идентичность {:?}; действие не задано в сценарии",
                self.worker, surface
            )
        });
        let session_failure = match action {
            ParallelPitchAction::Complete => None,
            ParallelPitchAction::SessionFailureTimes { failures, started } => {
                if let Some(started) = started {
                    let _ = started.send(());
                }
                // Повторные запросы того же элемента получают отказ ровно `failures`
                // раз, дальше она завершается обычным путём.
                self.actions.insert(
                    surface.clone(),
                    if failures > 1 {
                        ParallelPitchAction::SessionFailureTimes {
                            failures: failures - 1,
                            started: None,
                        }
                    } else {
                        ParallelPitchAction::Complete
                    },
                );
                Some(JpdbPitchFailure::SessionFailure {
                    stage: JpdbPitchStage::SearchNavigation,
                    message: "искусственный сбой сессии исполнителя".into(),
                })
            }
            ParallelPitchAction::Wait { started, release } => {
                if let Some(started) = started {
                    let _ = started.send(());
                }
                release
                    .await
                    .expect("тест должен возобновить запрос исполнителя");
                None
            }
            ParallelPitchAction::WaitFailure { started, release } => {
                if let Some(started) = started {
                    let _ = started.send(());
                }
                release
                    .await
                    .expect("тест должен возобновить запрос исполнителя");
                Some(JpdbPitchFailure::SessionFailure {
                    stage: JpdbPitchStage::SearchNavigation,
                    message: "искусственный сбой сессии исполнителя".into(),
                })
            }
        };
        let outcomes = if session_failure.is_some() {
            Vec::new()
        } else {
            vec![JpdbPitchOutcome::VocabularyNotFound {
                surface: request.query.surface.clone(),
                reading: request.query.reading.clone(),
            }]
        };
        crate::jpdb::JpdbPitchAcquisitionReport {
            outcomes,
            session_failure,
        }
    }

    async fn close(&mut self, session: Self::Session) {
        assert_eq!(self.worker, session.worker);
        assert_eq!(self.active_session.take(), Some(session.worker_session));
        self.trace.close(ParallelPitchAttempt {
            worker: session.worker,
            worker_session: session.worker_session,
            session: session.session,
            surface: String::new(),
        });
    }

    fn finish(&mut self) -> Result<(), String> {
        if self.active_session.is_some() {
            return Err(format!(
                "исполнитель {} завершился с активной сессией",
                self.worker
            ));
        }
        let workspace = self
            .workspace
            .take()
            .expect("временное дерево исполнителя закрывается один раз");
        let workspace_path = workspace.path().to_path_buf();
        workspace
            .close()
            .map_err(|error| format!("очистка временного дерева исполнителя: {error}"))?;
        self.trace.finish(self.worker, workspace_path);
        if self.fail_finish {
            Err(format!(
                "искусственный сбой очистки для исполнителя {}",
                self.worker
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Default)]
struct CapturedPitchProgress {
    events: Vec<super::PitchProgressEvent>,
    jsonl: Vec<u8>,
    interrupt_on_checkpoint: Option<futures::channel::oneshot::Sender<()>>,
    checkpoint_signals: std::collections::BTreeMap<String, futures::channel::oneshot::Sender<()>>,
    session_end_signals: std::collections::BTreeMap<u32, futures::channel::oneshot::Sender<()>>,
    recovery_signals: std::collections::BTreeMap<u32, futures::channel::oneshot::Sender<()>>,
    requeue_signals: std::collections::BTreeMap<String, futures::channel::oneshot::Sender<()>>,
    release_on_heartbeat: Option<futures::channel::oneshot::Sender<()>>,
    release_on_session_heartbeat: Option<(u32, futures::channel::oneshot::Sender<()>)>,
    state_path: Option<PathBuf>,
    heartbeat_states: Vec<Vec<u8>>,
}

impl super::PitchProgressSink for CapturedPitchProgress {
    fn emit(&mut self, event: super::PitchProgressEvent) -> Result<(), crate::error::AssetError> {
        if event.event == "heartbeat" {
            if let Some(path) = &self.state_path {
                self.heartbeat_states.push(fs::read(path).unwrap());
            }
            if let Some(permit) = self.release_on_heartbeat.take() {
                permit.send(()).unwrap();
            }
            if self
                .release_on_session_heartbeat
                .as_ref()
                .is_some_and(|(session, _)| event.session == Some(*session))
            {
                let (_, permit) = self.release_on_session_heartbeat.take().unwrap();
                permit.send(()).unwrap();
            }
        }
        if event.event == "item_checkpointed"
            && let Some(signal) = self.interrupt_on_checkpoint.take()
        {
            signal.send(()).unwrap();
        }
        if event.event == "item_checkpointed"
            && let Some(identity) = &event.identity
            && let Some(signal) = self.checkpoint_signals.remove(&identity.key)
        {
            let _ = signal.send(());
        }
        if event.event == "browser_session_ended"
            && let Some(worker) = event.worker
            && let Some(signal) = self.session_end_signals.remove(&worker)
        {
            let _ = signal.send(());
        }
        if event.event == "browser_session_recovered"
            && let Some(worker) = event.worker
            && let Some(signal) = self.recovery_signals.remove(&worker)
        {
            let _ = signal.send(());
        }
        if event.event == "item_requeued"
            && let Some(identity) = &event.identity
            && let Some(signal) = self.requeue_signals.remove(&identity.key)
        {
            let _ = signal.send(());
        }
        super::write_pitch_progress(&event, OutputFormat::Json, &mut self.jsonl)?;
        self.events.push(event);
        Ok(())
    }
}

fn offline_pitch_batch(store: &AssetStore, batch_id: &str, surfaces: &[&str]) {
    let items = surfaces
        .iter()
        .map(|surface| PitchPlanItem {
            surface: (*surface).into(),
            reading: None,
            selection: None,
        })
        .collect::<Vec<_>>();
    create_batch(store, batch_id, &items, None).unwrap();
}

fn offline_pitch_policy(max_items: usize) -> super::PitchRunPolicy {
    super::PitchRunPolicy {
        max_items,
        max_age: std::time::Duration::from_secs(1200),
        heartbeat: std::time::Duration::from_millis(1),
    }
}

fn no_pitch_interruption() -> impl std::future::Future<Output = Result<(), String>> {
    std::future::pending()
}

async fn pitch_signal(receiver: futures::channel::oneshot::Receiver<()>) -> Result<(), String> {
    receiver.await.map_err(|error| error.to_string())
}

fn assert_untouched_pitch_tail(batch: &PitchAccentBatch, start: usize) {
    for item in &batch.items[start..] {
        assert_eq!(item.status(), PitchBatchItemStatus::Pending);
        assert!(item.attempts.is_empty());
        assert!(item.current_candidate_sha256.is_none());
    }
}

async fn wait_for_worker_signal<T: std::fmt::Debug, F: std::future::Future<Output = T>>(
    run: &mut std::pin::Pin<Box<F>>,
    signal: &mut futures::channel::oneshot::Receiver<()>,
) {
    tokio::select! {
        biased;
        result = run.as_mut() => panic!("запуск завершился до ожидаемого события исполнителя: {result:?}"),
        result = signal => result.expect("искусственный барьер должен быть снят"),
    }
}

fn parallel_pitch_policy(max_items: usize) -> super::PitchRunPolicy {
    super::PitchRunPolicy {
        max_items,
        max_age: std::time::Duration::from_secs(1200),
        heartbeat: std::time::Duration::from_secs(60),
    }
}

fn assert_parallel_workspaces_removed(trace: &ParallelPitchTrace, expected_workers: usize) {
    let snapshot = trace.snapshot();
    assert_eq!(snapshot.finished.len(), expected_workers);
    assert_eq!(
        snapshot
            .finished
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        (1..=expected_workers as u32).collect()
    );
    assert_eq!(snapshot.workspace_paths.len(), expected_workers);
    assert_eq!(
        snapshot
            .workspace_paths
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        expected_workers
    );
    assert!(snapshot.workspace_paths.iter().all(|path| !path.exists()));
}

#[tokio::test]
async fn parallel_pitch_worker_count_bounds_unique_dispatch_and_json_progress() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-one-worker", &["一", "二", "三", "四"]);

    let trace = ParallelPitchTrace::default();
    let driver = ParallelPitchDriver::new(
        1,
        trace.clone(),
        ParallelPitchDriver::complete_actions(&["一", "二", "三", "四"]),
    );
    let mut drivers = [driver];
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_drivers(
        &store,
        "parallel-one-worker",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();

    let snapshot = trace.snapshot();
    assert_eq!(snapshot.max_active, 1);
    assert_eq!(snapshot.active, 0);
    assert_eq!(snapshot.starts.len(), 4);
    assert_eq!(snapshot.closed.len(), 1);
    assert!(snapshot.starts.iter().all(|attempt| attempt.worker == 1));
    let dispatched = snapshot
        .starts
        .iter()
        .map(|attempt| attempt.surface.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(dispatched, ["一", "二", "三", "四"].into_iter().collect());
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
    assert_parallel_workspaces_removed(&trace, 1);

    let jsonl = String::from_utf8(progress.jsonl).unwrap();
    let events = jsonl
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(!events.is_empty());
    for event in &events {
        assert_eq!(event["workers"], 1);
        assert!(event["in_flight"].as_u64().unwrap() <= 1);
    }
    for event in events
        .iter()
        .filter(|event| event["event"] == "item_started" || event["event"] == "item_checkpointed")
    {
        assert_eq!(event["worker"], 1);
        assert_eq!(event["worker_session"], 1);
        assert!(event["session"].as_u64().is_some());
    }
    let session_started = events
        .iter()
        .find(|event| event["event"] == "browser_session_started")
        .unwrap();
    assert_eq!(session_started["in_flight"], 1);
    assert_eq!(session_started["identity"]["key"], "一");
}

#[tokio::test]
async fn parallel_pitch_rejects_invalid_worker_counts_before_creating_actors() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-invalid-workers", &["一"]);

    let mut no_drivers: Vec<ParallelPitchDriver> = Vec::new();
    let error = super::run_batch_with_drivers(
        &store,
        "parallel-invalid-workers",
        "batch_run",
        &mut no_drivers,
        &mut CapturedPitchProgress::default(),
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidIdentity);

    let too_many_trace = ParallelPitchTrace::default();
    let mut too_many = (1..=5)
        .map(|worker| ParallelPitchDriver::new(worker, too_many_trace.clone(), []))
        .collect::<Vec<_>>();
    let error = super::run_batch_with_drivers(
        &store,
        "parallel-invalid-workers",
        "batch_run",
        &mut too_many,
        &mut CapturedPitchProgress::default(),
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidIdentity);
    let snapshot = too_many_trace.snapshot();
    assert!(snapshot.starts.is_empty());
    assert!(snapshot.closed.is_empty());
    assert!(snapshot.finished.is_empty());
    drop(too_many);
}

#[test]
fn pitch_batch_workers_default_to_four_and_cli_rejects_values_outside_supported_range() {
    let parsed = <PitchCli as clap::Parser>::try_parse_from([
        "pitch-assets",
        "batch",
        "run",
        "--batch-id",
        "worker-cli-default",
    ])
    .unwrap();
    match parsed.command {
        PitchCommand::Batch {
            command: PitchBatchCommand::Run { workers, .. },
        } => assert_eq!(workers, 4),
        _ => panic!("ожидалась команда batch run"),
    }

    for workers in ["0", "5"] {
        assert!(
            <PitchCli as clap::Parser>::try_parse_from([
                "pitch-assets",
                "batch",
                "run",
                "--batch-id",
                "worker-cli-range",
                "--workers",
                workers,
            ])
            .is_err()
        );
    }
}

#[tokio::test]
async fn parallel_pitch_checkpoints_out_of_order_reports_by_identity_and_cas_token() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-out-of-order", &["一", "二", "三"]);
    let trace = ParallelPitchTrace::default();

    let (start_a_tx, mut start_a_rx) = futures::channel::oneshot::channel();
    let (release_a_tx, release_a_rx) = futures::channel::oneshot::channel();
    let (start_b_tx, mut start_b_rx) = futures::channel::oneshot::channel();
    let (release_b_tx, release_b_rx) = futures::channel::oneshot::channel();
    let (start_c_tx, mut start_c_rx) = futures::channel::oneshot::channel();
    let (release_c_tx, release_c_rx) = futures::channel::oneshot::channel();
    let (checkpoint_b_tx, mut checkpoint_b_rx) = futures::channel::oneshot::channel();
    let (checkpoint_c_tx, mut checkpoint_c_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [(
                "一".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_a_tx),
                    release: release_a_rx,
                },
            )],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [
                (
                    "二".into(),
                    ParallelPitchAction::Wait {
                        started: Some(start_b_tx),
                        release: release_b_rx,
                    },
                ),
                (
                    "三".into(),
                    ParallelPitchAction::Wait {
                        started: Some(start_c_tx),
                        release: release_c_rx,
                    },
                ),
            ],
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    progress
        .checkpoint_signals
        .insert("二".into(), checkpoint_b_tx);
    progress
        .checkpoint_signals
        .insert("三".into(), checkpoint_c_tx);
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-out-of-order",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    ));

    wait_for_worker_signal(&mut run, &mut start_a_rx).await;
    wait_for_worker_signal(&mut run, &mut start_b_rx).await;
    release_b_tx.send(()).unwrap();
    wait_for_worker_signal(&mut run, &mut checkpoint_b_rx).await;
    wait_for_worker_signal(&mut run, &mut start_c_rx).await;
    assert_eq!(
        load_batch(&store, "parallel-out-of-order")
            .unwrap()
            .item("二")
            .unwrap()
            .attempts
            .len(),
        1
    );
    assert!(
        load_batch(&store, "parallel-out-of-order")
            .unwrap()
            .item("一")
            .unwrap()
            .attempts
            .is_empty()
    );
    release_c_tx.send(()).unwrap();
    wait_for_worker_signal(&mut run, &mut checkpoint_c_rx).await;
    release_a_tx.send(()).unwrap();
    let (batch, _) = run.await.unwrap();

    let snapshot = trace.snapshot();
    assert_eq!(snapshot.max_active, 2);
    assert_eq!(snapshot.active, 0);
    assert_eq!(snapshot.starts.len(), 3);
    assert_eq!(
        snapshot
            .starts
            .iter()
            .filter(|attempt| attempt.surface == "一")
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .starts
            .iter()
            .filter(|attempt| attempt.surface == "二")
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .starts
            .iter()
            .filter(|attempt| attempt.surface == "三")
            .count(),
        1
    );
    let checkpoints = progress
        .events
        .iter()
        .filter(|event| event.event == "item_checkpointed")
        .map(|event| {
            (
                event.identity.as_ref().unwrap().key.as_str(),
                event.run_completed,
                event.worker,
                event.worker_session,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        checkpoints,
        vec![
            ("二", 1, Some(2), Some(1)),
            ("三", 2, Some(2), Some(1)),
            ("一", 3, Some(1), Some(1))
        ]
    );
    let jsonl = String::from_utf8(progress.jsonl).unwrap();
    let json_events = jsonl
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(
        json_events
            .iter()
            .all(|event| event["workers"] == 2 && event["in_flight"].as_u64().unwrap() <= 2)
    );
    assert_eq!(
        json_events
            .iter()
            .map(|event| event["in_flight"].as_u64().unwrap())
            .max(),
        Some(2)
    );
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_discards_stale_cas_result_without_counting_it_as_checkpoint() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-stale-cas", &["一", "二"]);
    let trace = ParallelPitchTrace::default();
    let (start_a_tx, mut start_a_rx) = futures::channel::oneshot::channel();
    let (release_a_tx, release_a_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [(
                "一".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_a_tx),
                    release: release_a_rx,
                },
            )],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            ParallelPitchDriver::complete_actions(&["二"]),
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-stale-cas",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    ));
    wait_for_worker_signal(&mut run, &mut start_a_rx).await;

    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "parallel-stale-cas").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    batch
        .reacquire("一", "параллельное изменение перед CAS".into())
        .unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);
    release_a_tx.send(()).unwrap();
    let (batch, _) = run.await.unwrap();

    assert_eq!(
        batch.item("一").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert!(batch.item("一").unwrap().attempts.is_empty());
    assert_eq!(batch.item("二").unwrap().attempts.len(), 1);
    let discarded = progress
        .events
        .iter()
        .find(|event| event.event == "item_discarded_stale")
        .unwrap();
    assert_eq!(discarded.identity.as_ref().unwrap().key, "一");
    assert_eq!(discarded.run_completed, 1);
    assert_eq!(progress.events.last().unwrap().run_completed, 1);
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_session_failure_recovers_locally_and_keeps_neighbor_dispatching() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(
        &store,
        "parallel-neighbor-failure",
        &["一", "二", "三", "四"],
    );
    let trace = ParallelPitchTrace::default();
    let (start_b_tx, mut start_b_rx) = futures::channel::oneshot::channel();
    let (release_b_tx, release_b_rx) = futures::channel::oneshot::channel();
    let (recovery_tx, mut recovery_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [
                (
                    "一".into(),
                    ParallelPitchAction::SessionFailureTimes {
                        failures: 1,
                        started: None,
                    },
                ),
                ("二".into(), ParallelPitchAction::Complete),
                ("三".into(), ParallelPitchAction::Complete),
                ("四".into(), ParallelPitchAction::Complete),
            ],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [
                ("一".into(), ParallelPitchAction::Complete),
                (
                    "二".into(),
                    ParallelPitchAction::Wait {
                        started: Some(start_b_tx),
                        release: release_b_rx,
                    },
                ),
                ("三".into(), ParallelPitchAction::Complete),
                ("四".into(), ParallelPitchAction::Complete),
            ],
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    progress.recovery_signals.insert(1, recovery_tx);
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-neighbor-failure",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    ));
    // Сосед держит своё задание, пока первый исполнитель пересоздаёт сессию.
    wait_for_worker_signal(&mut run, &mut start_b_rx).await;
    wait_for_worker_signal(&mut run, &mut recovery_rx).await;
    release_b_tx.send(()).unwrap();
    let (batch, _) = run.await.unwrap();

    // Отказ сессии одного исполнителя больше не останавливает выдачу задач.
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "run_stopped")
    );
    let recovered = progress
        .events
        .iter()
        .find(|event| event.event == "browser_session_recovered")
        .unwrap();
    assert_eq!(recovered.worker, Some(1));
    assert_eq!(recovered.worker_session, Some(2));
    assert_eq!(recovered.recovery, Some(1));
    assert_eq!(recovered.reason.as_deref(), Some("session_failure"));
    let requeued = progress
        .events
        .iter()
        .find(|event| event.event == "item_requeued")
        .unwrap();
    assert_eq!(requeued.identity.as_ref().unwrap().key, "一");
    assert_eq!(requeued.worker, Some(1));
    // Упавший элемент не получил попытку и был выполнен ровно один раз.
    for item in &batch.items {
        assert_eq!(item.attempts.len(), 1, "элемент {}", item.identity.key);
    }
    assert_eq!(batch.item("一").unwrap().attempts.len(), 1);
    let snapshot = trace.snapshot();
    assert_eq!(snapshot.closed.len(), 3);
    assert!(
        snapshot
            .starts
            .iter()
            .any(|attempt| attempt.surface == "一" && attempt.worker == 1)
    );
    // Упавшая попытка и её повтор принадлежат разным сессиям одного исполнителя.
    assert_eq!(
        snapshot
            .starts
            .iter()
            .filter(|attempt| attempt.surface == "一")
            .count(),
        2
    );
    assert_eq!(snapshot.starts.len(), 5);
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_two_workers_recover_independently() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-two-recoveries", &["一", "二", "三", "四"]);
    let trace = ParallelPitchTrace::default();
    let (recovery_one_tx, mut recovery_one_rx) = futures::channel::oneshot::channel();
    let (recovery_two_tx, mut recovery_two_rx) = futures::channel::oneshot::channel();
    let failures = |failures| ParallelPitchAction::SessionFailureTimes {
        failures,
        started: None,
    };
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [
                ("一".into(), failures(1)),
                ("二".into(), failures(1)),
                ("三".into(), ParallelPitchAction::Complete),
                ("四".into(), ParallelPitchAction::Complete),
            ],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [
                ("一".into(), failures(1)),
                ("二".into(), failures(1)),
                ("三".into(), ParallelPitchAction::Complete),
                ("四".into(), ParallelPitchAction::Complete),
            ],
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    progress.recovery_signals.insert(1, recovery_one_tx);
    progress.recovery_signals.insert(2, recovery_two_tx);
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-two-recoveries",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    ));
    // Оба исполнителя обязаны пересоздать сессию и продолжить работу.
    wait_for_worker_signal(&mut run, &mut recovery_one_rx).await;
    wait_for_worker_signal(&mut run, &mut recovery_two_rx).await;
    let (batch, _) = run.await.unwrap();

    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "run_stopped")
    );
    let recovered = progress
        .events
        .iter()
        .filter(|event| event.event == "browser_session_recovered")
        .collect::<Vec<_>>();
    assert_eq!(recovered.len(), 2);
    let mut recovered_workers = recovered
        .iter()
        .map(|event| event.worker)
        .collect::<Vec<_>>();
    recovered_workers.sort_unstable();
    assert_eq!(recovered_workers, vec![Some(1), Some(2)]);
    assert!(recovered.iter().all(|event| event.recovery == Some(1)));
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_requeued")
            .count(),
        2
    );
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "worker_recovery_exhausted")
    );
    for item in &batch.items {
        assert_eq!(item.attempts.len(), 1, "элемент {}", item.identity.key);
        assert_eq!(item.status(), PitchBatchItemStatus::VocabularyNotFound);
    }
    let snapshot = trace.snapshot();
    assert!(
        snapshot
            .starts
            .iter()
            .any(|attempt| attempt.worker == 1 && attempt.worker_session == 2)
    );
    assert!(
        snapshot
            .starts
            .iter()
            .any(|attempt| attempt.worker == 2 && attempt.worker_session == 2)
    );
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_exhausted_worker_retires_and_healthy_peer_finishes_frontier() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-worker-retired", &["一", "二", "三", "四"]);
    let trace = ParallelPitchTrace::default();
    let mut drivers = [
        ParallelPitchDriver::new(1, trace.clone(), []),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [
                ("一".into(), ParallelPitchAction::Complete),
                ("二".into(), ParallelPitchAction::Complete),
                ("三".into(), ParallelPitchAction::Complete),
                ("四".into(), ParallelPitchAction::Complete),
            ],
        ),
    ];
    // Первый исполнитель не может запустить браузер вообще: он обязан выйти из
    // пула, не утащив за собой ни один элемент.
    drivers[0].fail_launch_until = Some(super::PITCH_SESSION_RECOVERY_BUDGET as usize);
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_drivers(
        &store,
        "parallel-worker-retired",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();

    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "run_stopped")
    );
    let exhausted = progress
        .events
        .iter()
        .filter(|event| event.event == "worker_recovery_exhausted")
        .collect::<Vec<_>>();
    assert_eq!(exhausted.len(), 1);
    assert_eq!(exhausted[0].worker, Some(1));
    assert_eq!(
        exhausted[0].recovery,
        Some(super::PITCH_SESSION_RECOVERY_BUDGET)
    );
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_requeued")
            .count(),
        super::PITCH_SESSION_RECOVERY_BUDGET as usize
    );
    // Все элементы доведены до результата здоровым соседом.
    for item in &batch.items {
        assert_eq!(item.attempts.len(), 1, "элемент {}", item.identity.key);
        assert_eq!(item.status(), PitchBatchItemStatus::VocabularyNotFound);
    }
    let snapshot = trace.snapshot();
    assert!(
        snapshot.starts.iter().all(|attempt| attempt.worker == 2),
        "задания обязан разобрать единственный исправный исполнитель"
    );
    assert_eq!(snapshot.starts.len(), 4);
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_all_workers_exhausted_leaves_frontier_resumable() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-all-exhausted", &["一", "二", "三"]);
    let before = load_batch(&store, "parallel-all-exhausted").unwrap();
    let trace = ParallelPitchTrace::default();
    let mut drivers = [
        ParallelPitchDriver::new(1, trace.clone(), []),
        ParallelPitchDriver::new(2, trace.clone(), []),
    ];
    for driver in &mut drivers {
        driver.fail_launch_until = Some(super::PITCH_SESSION_RECOVERY_BUDGET as usize);
    }
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_drivers(
        &store,
        "parallel-all-exhausted",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();

    // Пул исполнителей исчерпан: запуск обязан отказать честно, оставив весь
    // неразобранный фронт в сохранённом состоянии для `batch resume`.
    assert_eq!(
        error.details["run_stop_reason"],
        "session_recovery_exhausted"
    );
    assert_eq!(
        load_batch(&store, "parallel-all-exhausted").unwrap(),
        before
    );
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "worker_recovery_exhausted")
            .count(),
        2
    );
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_requeued")
            .count(),
        2 * super::PITCH_SESSION_RECOVERY_BUDGET as usize
    );
    assert!(progress.events.iter().all(|event| event.run_completed == 0));
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
    let mut resumed = ParallelPitchDriver::new(
        1,
        trace.clone(),
        [
            ("一".into(), ParallelPitchAction::Complete),
            ("二".into(), ParallelPitchAction::Complete),
            ("三".into(), ParallelPitchAction::Complete),
        ],
    );
    let (batch, _) = super::run_batch_with_drivers(
        &store,
        "parallel-all-exhausted",
        "batch_resume",
        std::slice::from_mut(&mut resumed),
        &mut CapturedPitchProgress::default(),
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
}

#[tokio::test]
async fn parallel_pitch_ctrl_c_keeps_completed_set_and_resumes_without_reacquiring_it() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-interrupt-resume", &["一", "二", "三"]);
    let trace = ParallelPitchTrace::default();
    let (start_a_tx, mut start_a_rx) = futures::channel::oneshot::channel();
    let (_release_a_tx, release_a_rx) = futures::channel::oneshot::channel();
    let (start_b_tx, mut start_b_rx) = futures::channel::oneshot::channel();
    let (release_b_tx, release_b_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [(
                "一".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_a_tx),
                    release: release_a_rx,
                },
            )],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [
                (
                    "二".into(),
                    ParallelPitchAction::Wait {
                        started: Some(start_b_tx),
                        release: release_b_rx,
                    },
                ),
                ("四".into(), ParallelPitchAction::Complete),
            ],
        ),
    ];
    let (interrupt_tx, interrupt_rx) = futures::channel::oneshot::channel();
    let mut progress = CapturedPitchProgress {
        interrupt_on_checkpoint: Some(interrupt_tx),
        ..Default::default()
    };
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-interrupt-resume",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        pitch_signal(interrupt_rx),
    ));
    wait_for_worker_signal(&mut run, &mut start_a_rx).await;
    wait_for_worker_signal(&mut run, &mut start_b_rx).await;
    release_b_tx.send(()).unwrap();
    let error = run.await.unwrap_err();

    assert_eq!(error.details["run_stop_reason"], "interrupted");
    let batch = load_batch(&store, "parallel-interrupt-resume").unwrap();
    assert!(batch.item("一").unwrap().attempts.is_empty());
    assert_eq!(batch.item("二").unwrap().attempts.len(), 1);
    assert!(batch.item("三").unwrap().attempts.is_empty());
    let snapshot = trace.snapshot();
    assert_eq!(snapshot.active, 0);
    assert_eq!(snapshot.starts.len(), 2);
    assert_eq!(snapshot.closed.len(), 2);
    assert_parallel_workspaces_removed(&trace, 2);
    assert_eq!(progress.events.last().unwrap().run_completed, 1);

    let resume_trace = ParallelPitchTrace::default();
    let mut resume_drivers = [ParallelPitchDriver::new(
        1,
        resume_trace.clone(),
        ParallelPitchDriver::complete_actions(&["一", "三"]),
    )];
    let mut resume_progress = CapturedPitchProgress::default();
    let (resumed, _) = super::run_batch_with_drivers(
        &store,
        "parallel-interrupt-resume",
        "batch_resume",
        &mut resume_drivers,
        &mut resume_progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    let resume_snapshot = resume_trace.snapshot();
    assert_eq!(resume_snapshot.starts.len(), 2);
    assert!(
        resume_snapshot
            .starts
            .iter()
            .all(|attempt| attempt.surface != "二")
    );
    assert!(resumed.items.iter().all(|item| item.attempts.len() == 1));
    assert_eq!(resume_progress.events[0].run_total, 2);
    assert_parallel_workspaces_removed(&resume_trace, 1);
}

#[tokio::test]
async fn parallel_pitch_worker_sessions_rotate_independently() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(
        &store,
        "parallel-independent-rotation",
        &["一", "二", "三", "四"],
    );
    let trace = ParallelPitchTrace::default();
    let (start_b_tx, mut start_b_rx) = futures::channel::oneshot::channel();
    let (release_b_tx, release_b_rx) = futures::channel::oneshot::channel();
    let (start_c_tx, mut start_c_rx) = futures::channel::oneshot::channel();
    let (release_c_tx, release_c_rx) = futures::channel::oneshot::channel();
    let (worker_one_rotation_tx, mut worker_one_rotation_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [
                ("一".into(), ParallelPitchAction::Complete),
                (
                    "三".into(),
                    ParallelPitchAction::Wait {
                        started: Some(start_c_tx),
                        release: release_c_rx,
                    },
                ),
                ("四".into(), ParallelPitchAction::Complete),
            ],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [(
                "二".into(),
                ParallelPitchAction::Wait {
                    started: Some(start_b_tx),
                    release: release_b_rx,
                },
            )],
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    progress
        .session_end_signals
        .insert(1, worker_one_rotation_tx);
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-independent-rotation",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(1),
        no_pitch_interruption(),
    ));
    wait_for_worker_signal(&mut run, &mut start_b_rx).await;
    wait_for_worker_signal(&mut run, &mut worker_one_rotation_rx).await;
    wait_for_worker_signal(&mut run, &mut start_c_rx).await;

    let snapshot = trace.snapshot();
    assert!(snapshot.starts.iter().any(|attempt| {
        attempt.worker == 1 && attempt.surface == "一" && attempt.worker_session == 1
    }));
    assert!(snapshot.starts.iter().any(|attempt| {
        attempt.worker == 2 && attempt.surface == "二" && attempt.worker_session == 1
    }));
    assert!(snapshot.starts.iter().any(|attempt| {
        attempt.worker == 1 && attempt.surface == "三" && attempt.worker_session == 2
    }));
    assert_eq!(
        snapshot.active, 2,
        "worker 2 должен оставаться в своей первой сессии"
    );
    release_c_tx.send(()).unwrap();
    release_b_tx.send(()).unwrap();
    let (batch, _) = run.await.unwrap();
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
    let rotation = progress
        .events
        .iter()
        .find(|event| event.event == "browser_session_rotated" && event.worker == Some(1))
        .unwrap();
    assert_eq!(rotation.worker_session, Some(1));
    let worker_two_item = progress
        .events
        .iter()
        .find(|event| {
            event.event == "item_started"
                && event.worker == Some(2)
                && event
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.key == "二")
        })
        .unwrap();
    assert_eq!(worker_two_item.worker_session, Some(1));
    let worker_one_rotated_item = progress
        .events
        .iter()
        .find(|event| {
            event.event == "item_started"
                && event.worker == Some(1)
                && event
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.key == "三")
        })
        .unwrap();
    assert_eq!(worker_one_rotated_item.worker_session, Some(2));
    assert_eq!(rotation.next_session, worker_one_rotated_item.session);
    let snapshot = trace.snapshot();
    let sessions = snapshot
        .starts
        .iter()
        .map(|attempt| attempt.session)
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(sessions.len(), snapshot.starts.len());
    assert_parallel_workspaces_removed(&trace, 2);
}

#[tokio::test]
async fn parallel_pitch_reports_worker_workspace_cleanup_failure_after_checkpoint() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-cleanup-error", &["一"]);
    let trace = ParallelPitchTrace::default();
    let mut drivers = [ParallelPitchDriver::new(
        1,
        trace.clone(),
        ParallelPitchDriver::complete_actions(&["一"]),
    )];
    drivers[0].fail_finish = true;
    let error = super::run_batch_with_drivers(
        &store,
        "parallel-cleanup-error",
        "batch_run",
        &mut drivers,
        &mut CapturedPitchProgress::default(),
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .message
            .contains("искусственный сбой очистки для исполнителя 1")
    );
    assert_eq!(
        load_batch(&store, "parallel-cleanup-error")
            .unwrap()
            .item("一")
            .unwrap()
            .attempts
            .len(),
        1
    );
    assert_eq!(trace.snapshot().closed.len(), 1);
    assert_parallel_workspaces_removed(&trace, 1);
}

// Эти тесты проверяют аварийные границы независимо от сценариев распределения
// `ParallelPitchDriver`. Закрытие каждой сессии и запроса наблюдается через RAII.
#[derive(Clone, Copy)]
enum LifecyclePitchFault {
    None,
    ClosePanic,
    CloseError,
    FinishPanic,
    FinishError,
}

enum LifecyclePitchAction {
    Complete,
    Gated {
        started: futures::channel::oneshot::Sender<()>,
        release: futures::channel::oneshot::Receiver<()>,
        panic: bool,
    },
}

#[derive(Default)]
struct LifecyclePitchState {
    opened: Vec<u32>,
    released: Vec<u32>,
    close_calls: Vec<u32>,
    finished: Vec<u32>,
    acquired: Vec<(u32, String)>,
    active_acquisitions: std::collections::BTreeSet<u32>,
    workspaces: Vec<PathBuf>,
}

type LifecyclePitchTrace = Arc<Mutex<LifecyclePitchState>>;

struct LifecyclePitchSession {
    worker: u32,
    trace: LifecyclePitchTrace,
}

impl Drop for LifecyclePitchSession {
    fn drop(&mut self) {
        self.trace.lock().unwrap().released.push(self.worker);
    }
}

struct LifecyclePitchAcquisition {
    worker: u32,
    trace: LifecyclePitchTrace,
}

impl Drop for LifecyclePitchAcquisition {
    fn drop(&mut self) {
        assert!(
            self.trace
                .lock()
                .unwrap()
                .active_acquisitions
                .remove(&self.worker)
        );
    }
}

struct LifecyclePitchDriver {
    worker: u32,
    trace: LifecyclePitchTrace,
    actions: std::collections::BTreeMap<String, LifecyclePitchAction>,
    fault: LifecyclePitchFault,
    workspace: Option<TempWorkspace>,
    finished_signal: Option<futures::channel::oneshot::Sender<()>>,
}

impl LifecyclePitchDriver {
    fn new(
        worker: u32,
        trace: LifecyclePitchTrace,
        actions: impl IntoIterator<Item = (String, LifecyclePitchAction)>,
        fault: LifecyclePitchFault,
    ) -> Self {
        let workspace = TempWorkspace::create("pitch-lifecycle-synthetic-worker").unwrap();
        trace
            .lock()
            .unwrap()
            .workspaces
            .push(workspace.path().to_path_buf());
        Self {
            worker,
            trace,
            actions: actions.into_iter().collect(),
            fault,
            workspace: Some(workspace),
            finished_signal: None,
        }
    }

    fn complete_actions(surfaces: &[&str]) -> Vec<(String, LifecyclePitchAction)> {
        surfaces
            .iter()
            .map(|surface| ((*surface).into(), LifecyclePitchAction::Complete))
            .collect()
    }
}

impl super::PitchRunDriver for LifecyclePitchDriver {
    type Session = LifecyclePitchSession;

    fn record_item_runtime_diagnostics(
        &self,
        session: &Self::Session,
        item: &crate::browser_diagnostics::BrowserItemTimer,
    ) {
        item.set_browser_session(u64::from(session.worker));
        item.record_runtime_snapshot(&crate::browser_runtime::RuntimeSnapshot::default());
    }

    async fn launch(&mut self) -> Result<Self::Session, super::BrowserLaunchError> {
        self.trace.lock().unwrap().opened.push(self.worker);
        Ok(LifecyclePitchSession {
            worker: self.worker,
            trace: self.trace.clone(),
        })
    }

    async fn acquire(
        &mut self,
        session: &Self::Session,
        request: &JpdbPitchRequest,
    ) -> crate::jpdb::JpdbPitchAcquisitionReport {
        assert_eq!(session.worker, self.worker);
        let action = self
            .actions
            .remove(&request.query.surface)
            .unwrap_or_else(|| {
                panic!(
                    "исполнитель {} получил неожиданную идентичность {}",
                    self.worker, request.query.surface
                )
            });
        {
            let mut state = self.trace.lock().unwrap();
            assert!(state.active_acquisitions.insert(self.worker));
            state
                .acquired
                .push((self.worker, request.query.surface.clone()));
        }
        let _acquisition = LifecyclePitchAcquisition {
            worker: self.worker,
            trace: self.trace.clone(),
        };
        if let LifecyclePitchAction::Gated {
            started,
            release,
            panic,
        } = action
        {
            started.send(()).unwrap();
            release.await.expect("тест должен освободить запрос");
            assert!(!panic, "искусственная паника при получении запроса");
        }
        crate::jpdb::JpdbPitchAcquisitionReport {
            outcomes: vec![JpdbPitchOutcome::VocabularyNotFound {
                surface: request.query.surface.clone(),
                reading: request.query.reading.clone(),
            }],
            session_failure: None,
        }
    }

    async fn close(&mut self, session: Self::Session) {
        assert_eq!(session.worker, self.worker);
        self.trace.lock().unwrap().close_calls.push(self.worker);
        drop(session);
    }

    async fn close_checked(&mut self, session: Self::Session) -> Result<(), String> {
        assert_eq!(session.worker, self.worker);
        self.trace.lock().unwrap().close_calls.push(self.worker);
        // При панике или раннем Err сессия драйвера освобождается через `Drop`.
        match self.fault {
            LifecyclePitchFault::ClosePanic => panic!("искусственная паника в `close_checked`"),
            LifecyclePitchFault::CloseError => Err("искусственная ошибка close_checked".into()),
            _ => {
                drop(session);
                Ok(())
            }
        }
    }

    fn finish(&mut self) -> Result<(), String> {
        self.trace.lock().unwrap().finished.push(self.worker);
        let workspace = self.workspace.take().expect("`finish` вызывается один раз");
        // `TempWorkspace` удаляется через `Drop` даже при панике в `finish`.
        match self.fault {
            LifecyclePitchFault::FinishPanic => panic!("искусственная паника в `finish`"),
            LifecyclePitchFault::FinishError => Err("искусственная ошибка finish".into()),
            _ => {
                workspace.close().unwrap();
                if let Some(signal) = self.finished_signal.take() {
                    signal.send(()).unwrap();
                }
                Ok(())
            }
        }
    }
}

fn assert_lifecycle_pitch_cleanup(trace: &LifecyclePitchTrace, workers: usize) {
    let state = trace.lock().unwrap();
    assert!(state.active_acquisitions.is_empty());
    assert_eq!(state.opened.len(), workers);
    assert_eq!(state.released.len(), workers);
    assert_eq!(state.close_calls.len(), workers);
    assert_eq!(state.finished.len(), workers);
    for worker in 1..=workers as u32 {
        assert_eq!(
            state
                .opened
                .iter()
                .filter(|value| **value == worker)
                .count(),
            1
        );
        assert_eq!(
            state
                .released
                .iter()
                .filter(|value| **value == worker)
                .count(),
            1
        );
        assert_eq!(
            state
                .close_calls
                .iter()
                .filter(|value| **value == worker)
                .count(),
            1
        );
        assert_eq!(
            state
                .finished
                .iter()
                .filter(|value| **value == worker)
                .count(),
            1
        );
    }
    assert_eq!(state.workspaces.len(), workers);
    assert!(state.workspaces.iter().all(|path| !path.exists()));
}

#[tokio::test]
async fn parallel_pitch_acquire_panic_closes_and_joins_workers_without_fabricated_attempts() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    let batch_id = "acquire-panic-lifecycle";
    offline_pitch_batch(&store, batch_id, &["一", "二", "三"]);
    let trace = LifecyclePitchTrace::default();
    let (panic_started_tx, mut panic_started_rx) = futures::channel::oneshot::channel();
    let (panic_release_tx, panic_release_rx) = futures::channel::oneshot::channel();
    let (neighbor_started_tx, mut neighbor_started_rx) = futures::channel::oneshot::channel();
    let (neighbor_release_tx, neighbor_release_rx) = futures::channel::oneshot::channel();
    let (finished_tx, mut finished_rx) = futures::channel::oneshot::channel();
    let mut drivers = [
        LifecyclePitchDriver::new(
            1,
            trace.clone(),
            [(
                "一".into(),
                LifecyclePitchAction::Gated {
                    started: panic_started_tx,
                    release: panic_release_rx,
                    panic: true,
                },
            )],
            LifecyclePitchFault::None,
        ),
        LifecyclePitchDriver::new(
            2,
            trace.clone(),
            [(
                "二".into(),
                LifecyclePitchAction::Gated {
                    started: neighbor_started_tx,
                    release: neighbor_release_rx,
                    panic: false,
                },
            )],
            LifecyclePitchFault::None,
        ),
    ];
    drivers[0].finished_signal = Some(finished_tx);
    let mut progress = CapturedPitchProgress::default();
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        batch_id,
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    ));
    wait_for_worker_signal(&mut run, &mut panic_started_rx).await;
    wait_for_worker_signal(&mut run, &mut neighbor_started_rx).await;
    panic_release_tx.send(()).unwrap();
    wait_for_worker_signal(&mut run, &mut finished_rx).await;
    assert!(
        futures::poll!(run.as_mut()).is_pending(),
        "запуск должен дождаться назначенного запроса соседнего исполнителя"
    );
    {
        let state = trace.lock().unwrap();
        assert_eq!(state.acquired, [(1, "一".into()), (2, "二".into())]);
        assert_eq!(state.finished, [1]);
        assert_eq!(state.active_acquisitions, [2].into_iter().collect());
    }
    assert_untouched_pitch_tail(&load_batch(&store, batch_id).unwrap(), 0);
    neighbor_release_tx.send(()).unwrap();
    let error = run.await.unwrap_err();
    assert_eq!(error.code, ErrorCode::ValidatorFailure);
    assert_eq!(error.details["run_stop_reason"], "worker_panic");
    assert!(
        error
            .message
            .contains("искусственная паника при получении запроса")
    );
    let saved = load_batch(&store, batch_id).unwrap();
    assert_untouched_pitch_tail(&saved, 2);
    assert_eq!(
        saved.item("一").unwrap().status(),
        PitchBatchItemStatus::Pending
    );
    assert!(saved.item("一").unwrap().attempts.is_empty());
    assert!(saved.item("一").unwrap().current_candidate_sha256.is_none());
    assert_eq!(saved.item("二").unwrap().attempts.len(), 1);
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
    assert_eq!(progress.events.last().unwrap().run_completed, 1);
    assert_eq!(progress.events.last().unwrap().in_flight, Some(0));
    assert_lifecycle_pitch_cleanup(&trace, 2);
}

async fn assert_pitch_cleanup_fault_preserves_checkpoint_and_resume(fault: LifecyclePitchFault) {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    let batch_id = "cleanup-fault-lifecycle";
    offline_pitch_batch(&store, batch_id, &["一", "二", "三"]);
    let trace = LifecyclePitchTrace::default();
    let mut drivers = [LifecyclePitchDriver::new(
        1,
        trace.clone(),
        LifecyclePitchDriver::complete_actions(&["一"]),
        fault,
    )];
    let finish_fault = matches!(
        fault,
        LifecyclePitchFault::FinishPanic | LifecyclePitchFault::FinishError
    );
    let (interrupt_tx, interrupt_rx) = futures::channel::oneshot::channel();
    let mut interrupt_sender = Some(interrupt_tx);
    let mut progress = CapturedPitchProgress::default();
    if finish_fault {
        // Сигнал возникает после надёжного сохранения результата и до выдачи следующей identity.
        progress.interrupt_on_checkpoint = interrupt_sender.take();
    }
    let error = super::run_batch_with_drivers(
        &store,
        batch_id,
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(1),
        pitch_signal(interrupt_rx),
    )
    .await
    .unwrap_err();
    let expected_message = match fault {
        LifecyclePitchFault::ClosePanic => "worker_close_panic",
        LifecyclePitchFault::CloseError => "искусственная ошибка close_checked",
        LifecyclePitchFault::FinishPanic => "worker_finish_panic",
        LifecyclePitchFault::FinishError => "искусственная ошибка finish",
        LifecyclePitchFault::None => unreachable!(),
    };
    if finish_fault {
        assert_eq!(error.code, ErrorCode::InvalidTransition);
        assert_eq!(error.details["run_stop_reason"], "interrupted");
        let cleanup_errors = error.details["additional_errors"].as_array().unwrap();
        assert!(cleanup_errors.iter().any(|error| {
            error["code"] == "validator_failure"
                && error["details"]["run_stop_reason"] == "browser_cleanup_failed"
                && error["message"]
                    .as_str()
                    .unwrap()
                    .contains(expected_message)
        }));
    } else {
        assert_eq!(error.code, ErrorCode::ValidatorFailure);
        assert_eq!(error.details["run_stop_reason"], "browser_cleanup_failed");
        assert!(error.message.contains(expected_message));
    }
    let saved = load_batch(&store, batch_id).unwrap();
    assert_eq!(saved.item("一").unwrap().attempts.len(), 1);
    assert_untouched_pitch_tail(&saved, 1);
    assert_eq!(trace.lock().unwrap().acquired, [(1, "一".into())]);
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
    assert_eq!(progress.events.last().unwrap().run_completed, 1);
    assert_eq!(progress.events.last().unwrap().in_flight, Some(0));
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_checkpointed")
            .count(),
        1
    );
    assert_lifecycle_pitch_cleanup(&trace, 1);

    let resume_trace = LifecyclePitchTrace::default();
    let mut resume_drivers = [LifecyclePitchDriver::new(
        1,
        resume_trace.clone(),
        LifecyclePitchDriver::complete_actions(&["二", "三"]),
        LifecyclePitchFault::None,
    )];
    let mut resume_progress = CapturedPitchProgress::default();
    let (resumed, _) = super::run_batch_with_drivers(
        &store,
        batch_id,
        "batch_resume",
        &mut resume_drivers,
        &mut resume_progress,
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(
        resume_trace.lock().unwrap().acquired,
        [(1, "二".into()), (1, "三".into())]
    );
    assert_eq!(resumed.item("一"), saved.item("一"));
    assert!(resumed.items.iter().all(|item| item.attempts.len() == 1));
    assert_eq!(resume_progress.events[0].run_total, 2);
    assert_eq!(resume_progress.events.last().unwrap().run_completed, 2);
    assert_lifecycle_pitch_cleanup(&resume_trace, 1);
}

#[tokio::test]
async fn parallel_pitch_close_panic_preserves_checkpoint_and_resume() {
    assert_pitch_cleanup_fault_preserves_checkpoint_and_resume(LifecyclePitchFault::ClosePanic)
        .await;
}

#[tokio::test]
async fn parallel_pitch_close_error_preserves_checkpoint_and_resume() {
    assert_pitch_cleanup_fault_preserves_checkpoint_and_resume(LifecyclePitchFault::CloseError)
        .await;
}

#[tokio::test]
async fn parallel_pitch_finish_panic_preserves_checkpoint_and_resume() {
    assert_pitch_cleanup_fault_preserves_checkpoint_and_resume(LifecyclePitchFault::FinishPanic)
        .await;
}

#[tokio::test]
async fn parallel_pitch_finish_error_preserves_checkpoint_and_resume() {
    assert_pitch_cleanup_fault_preserves_checkpoint_and_resume(LifecyclePitchFault::FinishError)
        .await;
}

#[tokio::test]
async fn pitch_launch_cleanup_failures_stop_without_recovery_or_batch_mutation() {
    for stage in ["profile_create", "browser_launch", "browser_setup"] {
        let workspace = temp_root();
        let store = store_at(workspace.path());
        offline_pitch_batch(&store, "launch-cleanup-failure", &["一", "二"]);
        let before = load_batch(&store, "launch-cleanup-failure").unwrap();
        let mut driver = ScriptedPitchDriver::new(
            &store,
            "launch-cleanup-failure",
            vec![ScriptedPitchAction::Complete, ScriptedPitchAction::Complete],
        );
        driver.launch_error = Some(if stage == "profile_create" {
            super::BrowserLaunchError::workspace_creation(
                std::io::Error::other(crate::temp_workspace::WorkspaceCreationCleanupFailure {
                    original: std::io::Error::other(
                        "искусственный сбой создания временного дерева",
                    ),
                    cleanup: std::io::Error::other("принадлежащие ресурсы не удалось удалить"),
                }),
                "worker_workspace_create_failed",
            )
        } else {
            super::BrowserLaunchError::CleanupFailed {
                original: format!("{stage}: искусственный сбой запуска"),
                cleanup: "принадлежащие ресурсы не удалось удалить".into(),
            }
        });
        let mut progress = CapturedPitchProgress::default();
        let error = super::run_batch_with_driver(
            &store,
            "launch-cleanup-failure",
            "batch_run",
            &mut driver,
            &mut progress,
            offline_pitch_policy(8),
            no_pitch_interruption(),
        )
        .await
        .unwrap_err();

        assert_eq!(
            error.details["run_stop_reason"], "browser_cleanup_failed",
            "{stage}"
        );
        assert_eq!(
            load_batch(&store, "launch-cleanup-failure").unwrap(),
            before,
            "{stage}"
        );
        assert_eq!(driver.launches, 1, "{stage}");
        assert!(driver.seen.is_empty(), "{stage}");
        assert!(driver.closed.is_empty(), "сессия не была создана: {stage}");
        assert_eq!(
            driver.actions.len(),
            2,
            "после отказа новые задания не выполняются: {stage}"
        );
        assert!(progress.events.iter().all(|event|
            event.event != "browser_session_recovered" && event.recovery.is_none()
        ), "ошибка очистки не должна расходовать бюджет восстановления: {stage}");
    }
}

#[tokio::test]
async fn pitch_launch_panic_with_proven_cleanup_remains_fail_closed() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "launch-panic", &["一", "二"]);
    let before = load_batch(&store, "launch-panic").unwrap();
    let mut driver =
        ScriptedPitchDriver::new(&store, "launch-panic", vec![ScriptedPitchAction::Complete]);
    driver.launch_error = Some(super::BrowserLaunchError::Panic(
        "browser_setup_panicked: искусственная паника".into(),
    ));
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "launch-panic",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "worker_panic");
    assert_eq!(driver.launches, 1);
    assert!(driver.seen.is_empty());
    assert_eq!(load_batch(&store, "launch-panic").unwrap(), before);
    assert!(progress.events.iter().all(|event| event.recovery.is_none()));
}

#[tokio::test]
async fn pitch_launch_failure_recovers_locally_and_completes_frontier() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "launch-recovery", &["一", "二", "三"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "launch-recovery",
        vec![
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::Complete,
        ],
    );
    driver.fail_launch_on = Some(1);
    let mut progress = CapturedPitchProgress::default();
    let (batch, changed) = super::run_batch_with_driver(
        &store,
        "launch-recovery",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();

    assert!(changed);
    // Первый запуск не дал сессии, второй её заменил: испорченной сессии нет,
    // поэтому закрывается только рабочая.
    assert_eq!(driver.launches, 2);
    assert_eq!(driver.closed, vec![2]);
    assert_eq!(driver.seen.len(), 3);
    for item in &batch.items {
        assert_eq!(item.attempts.len(), 1);
    }
    let recovered = progress
        .events
        .iter()
        .filter(|event| event.event == "browser_session_recovered")
        .collect::<Vec<_>>();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].recovery, Some(1));
    assert_eq!(recovered[0].worker, Some(1));
    assert_eq!(recovered[0].reason.as_deref(), Some("session_failure"));
    assert_eq!(progress.events.last().unwrap().event, "run_finished");
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "run_stopped")
    );
}

#[tokio::test]
async fn pitch_recovery_budget_exhaustion_retires_worker_and_leaves_frontier_pending() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "launch-exhausted", &["一", "二", "三"]);
    let before = load_batch(&store, "launch-exhausted").unwrap();
    let mut driver = ScriptedPitchDriver::new(&store, "launch-exhausted", vec![]);
    // Бюджет восстановления исчерпан: каждый запуск исполнителя терпит сбой.
    driver.fail_launch_until = Some(super::PITCH_SESSION_RECOVERY_BUDGET as usize);
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "launch-exhausted",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();

    assert_eq!(
        error.details["run_stop_reason"],
        "session_recovery_exhausted"
    );
    assert_eq!(error.details["session_failure"]["code"], "browser_setup");
    assert_eq!(
        driver.launches,
        super::PITCH_SESSION_RECOVERY_BUDGET as usize
    );
    assert!(driver.seen.is_empty());
    assert!(driver.closed.is_empty());
    // Ни один элемент не получил попытку: отказ сессии не синтезирует результат.
    assert_eq!(load_batch(&store, "launch-exhausted").unwrap(), before);
    let exhausted = progress
        .events
        .iter()
        .filter(|event| event.event == "worker_recovery_exhausted")
        .collect::<Vec<_>>();
    assert_eq!(exhausted.len(), 1);
    assert_eq!(
        exhausted[0].recovery,
        Some(super::PITCH_SESSION_RECOVERY_BUDGET)
    );
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_requeued")
            .count(),
        super::PITCH_SESSION_RECOVERY_BUDGET as usize
    );
    assert!(progress.events.iter().all(|event| event.run_completed == 0));
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
    assert_eq!(
        progress.events.last().unwrap().reason.as_deref(),
        Some("session_recovery_exhausted")
    );
}

#[tokio::test]
async fn pitch_session_failure_without_outcome_requeues_without_attempt() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "session-requeue", &["一", "二", "三"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "session-requeue",
        vec![
            ScriptedPitchAction::SessionFailure {
                after_outcome: false,
            },
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::Complete,
        ],
    );
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "session-requeue",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();

    // Отказ сессии без результата не создаёт попытку, а назначенный элемент
    // возвращается в очередь и выполняется ровно один раз после восстановления.
    let requeued = progress
        .events
        .iter()
        .filter(|event| event.event == "item_requeued")
        .collect::<Vec<_>>();
    assert_eq!(requeued.len(), 1);
    assert_eq!(requeued[0].identity.as_ref().unwrap().key, "一");
    assert_eq!(requeued[0].reason.as_deref(), Some("session_failure"));
    assert_eq!(requeued[0].recovery, Some(1));
    let ended = progress
        .events
        .iter()
        .filter(|event| {
            event.event == "browser_session_ended"
                && event.reason.as_deref() == Some("session_failure")
        })
        .collect::<Vec<_>>();
    assert_eq!(ended.len(), 1);
    assert!(ended[0].session.is_some());
    assert_eq!(ended[0].session, requeued[0].session);
    assert_eq!(ended[0].worker, requeued[0].worker);
    assert_eq!(ended[0].worker_session, requeued[0].worker_session);
    assert_eq!(ended[0].identity, requeued[0].identity);
    assert_eq!(ended[0].recovery, requeued[0].recovery);
    assert_eq!(driver.seen.len(), 4);
    assert_eq!(driver.seen[0], (1, "一".into()));
    assert_eq!(driver.seen[1], (2, "二".into()));
    assert_eq!(driver.seen[3], (2, "一".into()));
    for item in &batch.items {
        assert_eq!(item.attempts.len(), 1, "элемент {}", item.identity.key);
    }
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "run_stopped")
    );
}

#[tokio::test]
async fn pitch_session_failure_after_real_outcome_checkpoints_once_then_rotates_session() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "session-after-outcome", &["一", "二", "三"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "session-after-outcome",
        vec![
            ScriptedPitchAction::SessionFailure {
                after_outcome: true,
            },
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::Complete,
        ],
    );
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "session-after-outcome",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();

    // Один настоящий результат плюс отказ сессии: результат сохраняется ровно
    // один раз, элемент не получает повторную попытку, а следующий идёт уже в новой сессии.
    assert_eq!(
        driver.seen,
        vec![(1, "一".into()), (2, "二".into()), (2, "三".into())]
    );
    assert_eq!(driver.closed, vec![1, 2]);
    assert_eq!(batch.item("一").unwrap().attempts.len(), 1);
    for item in &batch.items {
        assert_eq!(item.attempts.len(), 1, "элемент {}", item.identity.key);
    }
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_checkpointed")
            .count(),
        3
    );
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "item_requeued")
    );
    let recovered = progress
        .events
        .iter()
        .filter(|event| event.event == "browser_session_recovered")
        .collect::<Vec<_>>();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].recovery, Some(1));
    assert_eq!(recovered[0].worker_session, Some(2));
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "run_stopped")
    );
}

#[tokio::test]
async fn pitch_recovery_budget_resets_after_healthy_progress() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "recovery-reset", &["一", "二", "三"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "recovery-reset",
        vec![
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::SessionFailure {
                after_outcome: false,
            },
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::Complete,
        ],
    );
    // Два подряд сбоя запуска, затем доказанный прогресс на «一», затем новый отказ
    // сессии: серия обязана начаться заново с единицы, а не продолжиться с трёх.
    driver.fail_launch_until = Some(2);
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "recovery-reset",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();

    let recovered = progress
        .events
        .iter()
        .filter(|event| event.event == "browser_session_recovered")
        .collect::<Vec<_>>();
    assert_eq!(
        recovered
            .iter()
            .map(|event| event.recovery)
            .collect::<Vec<_>>(),
        vec![Some(2), Some(1)]
    );
    for item in &batch.items {
        assert_eq!(item.attempts.len(), 1, "элемент {}", item.identity.key);
    }
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "worker_recovery_exhausted")
    );
}

#[tokio::test]
async fn pitch_invalid_provider_report_stops_run_without_attempts() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "invalid-report", &["一", "二"]);
    let before = load_batch(&store, "invalid-report").unwrap();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "invalid-report",
        vec![ScriptedPitchAction::InvalidReport],
    );
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "invalid-report",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();

    // Нарушение инварианта протокола провайдера остаётся fail-closed и не
    // понижается до локального восстановления сессии.
    assert_eq!(error.details["run_stop_reason"], "invalid_provider_report");
    assert_eq!(load_batch(&store, "invalid-report").unwrap(), before);
    assert!(driver.seen.len() == 1);
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "browser_session_recovered")
    );
}

#[tokio::test]
async fn pitch_session_cleanup_failure_stops_run_fail_closed() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "cleanup-fail-closed", &["一", "二"]);
    let before = load_batch(&store, "cleanup-fail-closed").unwrap();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "cleanup-fail-closed",
        vec![
            ScriptedPitchAction::SessionFailure {
                after_outcome: false,
            },
            ScriptedPitchAction::Complete,
        ],
    );
    driver.fail_close_on = Some(1);
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "cleanup-fail-closed",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();

    // Неподтверждённая очистка испорченной сессии — не повод перезапустить её.
    assert_eq!(error.details["run_stop_reason"], "browser_cleanup_failed");
    assert_eq!(load_batch(&store, "cleanup-fail-closed").unwrap(), before);
    assert_eq!(driver.launches, 1);
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "browser_session_recovered")
    );
}

#[tokio::test]
async fn pitch_checkpoint_failure_is_not_hidden_by_local_session_recovery() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "checkpoint-fail-closed", &["一", "二"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "checkpoint-fail-closed",
        vec![
            ScriptedPitchAction::CorruptRuntime,
            ScriptedPitchAction::Complete,
        ],
    );
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "checkpoint-fail-closed",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap_err();

    // Граница надёжно сохранённой контрольной точки остаётся fail-closed: локальное восстановление
    // сессии не имеет права скрыть сбой хранилища.
    assert_ne!(
        error
            .details
            .get("run_stop_reason")
            .and_then(|value| value.as_str()),
        Some("session_recovery_exhausted")
    );
    assert_ne!(
        error
            .details
            .get("run_stop_reason")
            .and_then(|value| value.as_str()),
        Some("invalid_provider_report")
    );
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "browser_session_recovered"),
        "сбой сохранения не запускает обычное пересоздание сессии"
    );
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "item_checkpointed")
    );
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
}

#[tokio::test]
async fn pitch_stale_token_outcome_is_discarded_and_recovery_adds_no_duplicate() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "stale-with-recovery", &["一", "二"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "stale-with-recovery",
        vec![
            ScriptedPitchAction::ReacquireSessionFailure,
            ScriptedPitchAction::Complete,
        ],
    );
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "stale-with-recovery",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();

    // Устаревший токен отвергается CAS, и отказ сессии в том же отчёте не
    // превращает отвергнутый результат во вторую контрольную точку.
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.event == "item_discarded_stale")
    );
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_checkpointed")
            .count(),
        1
    );
    assert!(batch.item("一").unwrap().attempts.is_empty());
    assert_eq!(batch.item("二").unwrap().attempts.len(), 1);
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_requeued")
            .count(),
        0,
        "отчёт с настоящим результатом не возвращает задание в очередь"
    );
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "run_stopped")
    );
}

#[tokio::test]
async fn parallel_pitch_interruption_during_recovery_stops_dispatch_and_leaves_resume() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    offline_pitch_batch(&store, "parallel-recovery-interrupted", &["一", "二", "三"]);
    let trace = ParallelPitchTrace::default();
    let (start_a_tx, mut start_a_rx) = futures::channel::oneshot::channel();
    let (release_a_tx, release_a_rx) = futures::channel::oneshot::channel();
    let (session_end_tx, mut session_end_rx) = futures::channel::oneshot::channel();
    let (interrupt_tx, interrupt_rx) = futures::channel::oneshot::channel();
    let failing = |surface: &str| {
        (
            surface.to_owned(),
            ParallelPitchAction::SessionFailureTimes {
                failures: 5,
                started: None,
            },
        )
    };
    let mut drivers = [
        ParallelPitchDriver::new(
            1,
            trace.clone(),
            [failing("一"), failing("二"), failing("三")],
        ),
        ParallelPitchDriver::new(
            2,
            trace.clone(),
            [
                failing("一"),
                (
                    "二".into(),
                    ParallelPitchAction::WaitFailure {
                        started: Some(start_a_tx),
                        release: release_a_rx,
                    },
                ),
                failing("三"),
            ],
        ),
    ];
    let mut progress = CapturedPitchProgress::default();
    progress.session_end_signals.insert(1, session_end_tx);
    let mut run = Box::pin(super::run_batch_with_drivers(
        &store,
        "parallel-recovery-interrupted",
        "batch_run",
        &mut drivers,
        &mut progress,
        parallel_pitch_policy(8),
        pitch_signal(interrupt_rx),
    ));
    // Прерывание приходит ровно в момент, когда один исполнитель уже закрыл
    // испорченную сессию, а второй держит назначенный элемент.
    wait_for_worker_signal(&mut run, &mut start_a_rx).await;
    wait_for_worker_signal(&mut run, &mut session_end_rx).await;
    interrupt_tx.send(()).unwrap();
    release_a_tx.send(()).unwrap();
    let error = run.await.unwrap_err();

    assert_eq!(error.details["run_stop_reason"], "interrupted");
    let snapshot = trace.snapshot();
    assert_eq!(
        snapshot.finished.len(),
        2,
        "оба исполнителя обязаны завершиться"
    );
    assert_parallel_workspaces_removed(&trace, 2);

    // Незавершённые элементы остаются доступными для `batch resume`.
    let batch = load_batch(&store, "parallel-recovery-interrupted").unwrap();
    for item in &batch.items {
        assert!(
            item.attempts.is_empty(),
            "элемент {} не должен получить попытку",
            item.identity.key
        );
        assert_eq!(item.status(), PitchBatchItemStatus::Pending);
    }
    let mut resumed = ParallelPitchDriver::new(
        1,
        trace.clone(),
        [
            ("一".into(), ParallelPitchAction::Complete),
            ("二".into(), ParallelPitchAction::Complete),
            ("三".into(), ParallelPitchAction::Complete),
        ],
    );
    let (batch, _) = super::run_batch_with_drivers(
        &store,
        "parallel-recovery-interrupted",
        "batch_resume",
        std::slice::from_mut(&mut resumed),
        &mut CapturedPitchProgress::default(),
        parallel_pitch_policy(8),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
}

#[tokio::test]
async fn pitch_configuration_and_telemetry_failures_do_not_fabricate_tail_attempts() {
    for (expect_exhausted, action) in [
        (true, ScriptedPitchAction::SetupFailure),
        (
            true,
            ScriptedPitchAction::SessionFailure {
                after_outcome: false,
            },
        ),
        (
            false,
            ScriptedPitchAction::SessionFailure {
                after_outcome: true,
            },
        ),
    ] {
        let workspace = temp_root();
        let root = workspace.path();
        let store = store_at(root);
        offline_pitch_batch(&store, "session-failure", &["一", "二", "三"]);
        // Отказ сессии повторяется на каждом запросе: исполнитель либо исчерпывает
        // бюджет, либо каждый раз доказывает прогресс настоящим результатом.
        let mut driver = ScriptedPitchDriver::new(
            &store,
            "session-failure",
            (0..super::PITCH_SESSION_RECOVERY_BUDGET as usize + 2)
                .map(|_| action.clone())
                .collect(),
        );
        let mut progress = CapturedPitchProgress::default();
        let outcome = super::run_batch_with_driver(
            &store,
            "session-failure",
            "batch_run",
            &mut driver,
            &mut progress,
            offline_pitch_policy(2),
            no_pitch_interruption(),
        )
        .await;
        let batch = load_batch(&store, "session-failure").unwrap();

        if expect_exhausted {
            let error = outcome.unwrap_err();
            assert_eq!(
                error.details["run_stop_reason"],
                "session_recovery_exhausted"
            );
            // Ни один элемент не получил попытку: отказ сессии без результата не
            // синтезирует терминальный сбой.
            assert_untouched_pitch_tail(&batch, 0);
            assert_eq!(progress.events.last().unwrap().run_completed, 0);
            assert_eq!(
                progress
                    .events
                    .iter()
                    .filter(|event| event.event == "browser_session_recovered")
                    .count(),
                super::PITCH_SESSION_RECOVERY_BUDGET as usize - 1,
                "последний сбой исчерпывает бюджет и уже не пересоздаёт сессию"
            );
            assert_eq!(
                progress
                    .events
                    .iter()
                    .filter(|event| event.event == "worker_recovery_exhausted")
                    .count(),
                1
            );
        } else {
            outcome.unwrap();
            // Настоящий результат до отказа сессии сохраняется ровно один раз для
            // каждого элемента, и ни одна попытка не выдумана.
            for item in &batch.items {
                assert_eq!(item.attempts.len(), 1, "элемент {}", item.identity.key);
            }
            assert!(
                progress
                    .events
                    .iter()
                    .all(|event| event.event != "worker_recovery_exhausted")
            );
            assert!(
                progress
                    .events
                    .iter()
                    .filter(|event| event.event == "browser_session_recovered")
                    .all(|event| event.reason.as_deref() == Some("session_failure"))
            );
        }
        assert_eq!(
            progress.events.last().unwrap().event,
            if expect_exhausted {
                "run_stopped"
            } else {
                "run_finished"
            }
        );
    }
}

#[tokio::test]
async fn pitch_interruption_between_items_preserves_durable_prefix() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "interrupt-between", &["一", "二", "三"]);
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "interrupt-between",
        vec![ScriptedPitchAction::Complete],
    );
    let mut progress = CapturedPitchProgress {
        interrupt_on_checkpoint: Some(signal),
        ..Default::default()
    };
    let error = super::run_batch_with_driver(
        &store,
        "interrupt-between",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "interrupted");
    let batch = load_batch(&store, "interrupt-between").unwrap();
    assert_eq!(batch.items[0].attempts.len(), 1);
    assert_untouched_pitch_tail(&batch, 1);
    assert_eq!(driver.seen, vec![(1, "一".into())]);
    assert_eq!(driver.closed, vec![1]);
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
    assert_eq!(progress.events.last().unwrap().run_completed, 1);
}

#[tokio::test]
async fn pitch_interrupt_in_flight_keeps_current_identity_unchanged() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "interrupt-active", &["一", "二"]);
    let before = load_batch(&store, "interrupt-active").unwrap();
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "interrupt-active",
        vec![ScriptedPitchAction::InterruptInFlight(signal)],
    );
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "interrupt-active",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "interrupted");
    assert_eq!(load_batch(&store, "interrupt-active").unwrap(), before);
    assert_eq!(driver.closed, vec![1]);
    assert!(progress.events.iter().all(|event| event.run_completed == 0));
    assert!(
        !progress
            .events
            .iter()
            .any(|event| event.event == "item_checkpointed")
    );
}

#[tokio::test]
async fn pitch_ready_outcome_wins_signal_and_is_checkpointed_before_stop() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "ready-signal", &["一", "二"]);
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "ready-signal",
        vec![ScriptedPitchAction::ReadyWithSignal(signal)],
    );
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "ready-signal",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "interrupted");
    let batch = load_batch(&store, "ready-signal").unwrap();
    assert_eq!(batch.items[0].attempts.len(), 1);
    assert_untouched_pitch_tail(&batch, 1);
    assert_eq!(driver.closed, vec![1]);
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_checkpointed")
            .count(),
        1
    );
}

#[tokio::test]
async fn pitch_interruption_during_launch_waits_for_owner_then_closes_session() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "launch-interrupted", &["一", "二"]);
    let before = load_batch(&store, "launch-interrupted").unwrap();
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(&store, "launch-interrupted", vec![]);
    driver.interrupt_during_launch = Some(signal);
    let mut progress = CapturedPitchProgress::default();
    let error = super::run_batch_with_driver(
        &store,
        "launch-interrupted",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(error.details["run_stop_reason"], "interrupted");
    assert_eq!(load_batch(&store, "launch-interrupted").unwrap(), before);
    assert!(driver.seen.is_empty());
    assert_eq!(driver.closed, vec![1]);
    assert_eq!(progress.events.last().unwrap().event, "run_stopped");
}

#[tokio::test]
async fn pitch_progress_jsonl_has_live_heartbeat_and_only_durable_completed_count() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "json-progress", &["一"]);
    let state_path = store
        .root()
        .join(".runtime/batches/json-progress/state.json");
    let before = fs::read(&state_path).unwrap();
    let (permit, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "json-progress",
        vec![ScriptedPitchAction::WaitForHeartbeat(receiver)],
    );
    let mut progress = CapturedPitchProgress {
        release_on_heartbeat: Some(permit),
        state_path: Some(state_path),
        ..Default::default()
    };
    let (batch, changed) = super::run_batch_with_driver(
        &store,
        "json-progress",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert!(changed);
    assert!(!progress.heartbeat_states.is_empty());
    assert!(
        progress
            .heartbeat_states
            .iter()
            .all(|state| state == &before)
    );
    let heartbeat = progress
        .events
        .iter()
        .find(|event| event.event == "heartbeat")
        .unwrap();
    assert_eq!(heartbeat.identity.as_ref().unwrap().key, "一");
    assert_eq!(heartbeat.run_completed, 0);
    assert_eq!(heartbeat.run_total, 1);
    assert_eq!(heartbeat.attempt, Some(1));
    let checkpoint = progress
        .events
        .iter()
        .find(|event| event.event == "item_checkpointed")
        .unwrap();
    assert_eq!(checkpoint.run_completed, 1);
    assert_eq!(checkpoint.attempt, Some(1));
    assert_eq!(checkpoint.outcome.as_deref(), Some("vocabulary_not_found"));
    let response = super::render_response(
        super::batch_response(
            "batch_run",
            "needs_review",
            changed,
            Some(StoreSummary {
                path: store.root().display().to_string(),
                store_id: store.store_id().into(),
            }),
            &batch,
            Vec::new(),
            None,
        ),
        OutputFormat::Json,
        3,
    );
    let stdout: serde_json::Value = serde_json::from_str(&response.stdout).unwrap();
    assert_eq!(stdout["operation"], "batch_run");
    assert!(response.stderr.is_empty());
    let jsonl = String::from_utf8(progress.jsonl).unwrap();
    for line in jsonl.lines() {
        let event: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(event["schema_version"], 1);
        assert_eq!(event["batch_id"], "json-progress");
        assert!(event.get("round").is_none());
        assert!(event.get("round_limit").is_none());
        assert!(event.get("event").is_some());
    }
    assert!(serde_json::from_str::<serde_json::Value>(&jsonl).is_err());
}

#[tokio::test]
async fn pitch_rotation_then_interruption_resumes_tail_without_prefix_overlap() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "rotate-resume", &["一", "二", "三"]);
    let (signal, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "rotate-resume",
        vec![
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::InterruptInFlight(signal),
        ],
    );
    let mut progress = CapturedPitchProgress::default();
    super::run_batch_with_driver(
        &store,
        "rotate-resume",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(1),
        pitch_signal(receiver),
    )
    .await
    .unwrap_err();
    assert_eq!(driver.seen, vec![(1, "一".into()), (2, "二".into())]);
    assert_eq!(driver.closed, vec![1, 2]);
    assert_eq!(driver.attempts_at_start, vec![0, 1]);
    assert_untouched_pitch_tail(&load_batch(&store, "rotate-resume").unwrap(), 1);
    let mut resumed = ScriptedPitchDriver::new(
        &store,
        "rotate-resume",
        vec![ScriptedPitchAction::Complete, ScriptedPitchAction::Complete],
    );
    let mut resumed_progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "rotate-resume",
        "batch_resume",
        &mut resumed,
        &mut resumed_progress,
        offline_pitch_policy(1),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(resumed.seen, vec![(1, "二".into()), (2, "三".into())]);
    assert_eq!(resumed.closed, vec![1, 2]);
    assert_eq!(resumed.attempts_at_start, vec![1, 2]);
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
    assert!(
        resumed_progress
            .events
            .iter()
            .any(|event| event.event == "browser_session_rotated")
    );
    assert_eq!(resumed_progress.events.first().unwrap().run_total, 2);
    assert!(
        resumed_progress
            .events
            .iter()
            .all(|event| event.operation == "batch_run")
    );
    assert_eq!(resumed_progress.events.last().unwrap().run_completed, 2);
}

#[tokio::test]
async fn pitch_rotation_launch_failure_recovers_without_counting_planned_rotation() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "rotation-failure", &["一", "二", "三"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "rotation-failure",
        vec![
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::Complete,
            ScriptedPitchAction::Complete,
        ],
    );
    // Плановая ротация по лимиту записей не расходует бюджет: запуск, сменивший
    // ротацию, восстанавливается как первый в серии.
    driver.fail_launch_on = Some(2);
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "rotation-failure",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(1),
        no_pitch_interruption(),
    )
    .await
    .unwrap();

    assert_eq!(driver.launches, 4);
    assert_eq!(driver.closed, vec![1, 3, 4]);
    // Возвращённый элемент встаёт в конец очереди и выполняется последним.
    assert_eq!(
        driver.seen,
        vec![(1, "一".into()), (3, "三".into()), (4, "二".into())]
    );
    assert!(batch.items.iter().all(|item| item.attempts.len() == 1));
    let recovered = progress
        .events
        .iter()
        .filter(|event| event.event == "browser_session_recovered")
        .collect::<Vec<_>>();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].recovery, Some(1));
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.event == "browser_session_rotated"
                && event.reason.as_deref() == Some("item_limit"))
    );
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.event != "run_stopped")
    );
}

#[tokio::test]
async fn pitch_stale_cas_is_reported_without_incrementing_checkpoint_counter() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "stale-progress", &["一", "二"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "stale-progress",
        vec![
            ScriptedPitchAction::Reacquire,
            ScriptedPitchAction::Complete,
        ],
    );
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "stale-progress",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(batch.items[0].status(), PitchBatchItemStatus::Pending);
    assert!(batch.items[0].attempts.is_empty());
    assert_eq!(batch.items[1].attempts.len(), 1);
    let discarded = progress
        .events
        .iter()
        .find(|event| event.event == "item_discarded_stale")
        .unwrap();
    assert_eq!(discarded.run_completed, 0);
    assert_eq!(progress.events.last().unwrap().run_completed, 1);
}

#[tokio::test]
async fn pitch_session_age_rotates_between_items_and_production_limits_are_explicit() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "age-rotation", &["一", "二"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "age-rotation",
        vec![ScriptedPitchAction::Complete, ScriptedPitchAction::Complete],
    );
    let mut progress = CapturedPitchProgress::default();
    let mut policy = offline_pitch_policy(128);
    policy.max_age = std::time::Duration::from_nanos(1);
    super::run_batch_with_driver(
        &store,
        "age-rotation",
        "batch_run",
        &mut driver,
        &mut progress,
        policy,
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(driver.closed, vec![1, 2]);
    assert_eq!(driver.seen, vec![(1, "一".into()), (2, "二".into())]);
    assert!(
        progress
            .events
            .iter()
            .any(|event| event.event == "browser_session_rotated"
                && event.reason.as_deref() == Some("age_limit"))
    );
    assert_eq!(super::PitchRunPolicy::default().max_items, 64);
    assert_eq!(
        super::PitchRunPolicy::default().max_age,
        std::time::Duration::from_mins(20)
    );
}

#[tokio::test]
async fn pitch_manual_retry_reports_actual_attempt_and_pending_denominator() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "retry-progress", &["一", "二"]);
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "retry-progress",
        vec![
            ScriptedPitchAction::ItemFailure,
            ScriptedPitchAction::Complete,
        ],
    );
    super::run_batch_with_driver(
        &store,
        "retry-progress",
        "batch_run",
        &mut driver,
        &mut CapturedPitchProgress::default(),
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), "retry-progress").unwrap();
    let mut batch = runtime.load().unwrap().unwrap();
    assert_eq!(
        batch.items[0].status(),
        PitchBatchItemStatus::TechnicalFailure
    );
    batch
        .retry("一", "явный повтор после сетевой ошибки".into())
        .unwrap();
    runtime.save(&batch).unwrap();
    drop(runtime);
    let mut resumed = ScriptedPitchDriver::new(
        &store,
        "retry-progress",
        vec![ScriptedPitchAction::Complete],
    );
    let mut progress = CapturedPitchProgress::default();
    let (batch, _) = super::run_batch_with_driver(
        &store,
        "retry-progress",
        "batch_resume",
        &mut resumed,
        &mut progress,
        offline_pitch_policy(2),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    assert_eq!(batch.items[0].attempts.len(), 2);
    assert_eq!(batch.items[1].attempts.len(), 1);
    assert_eq!(progress.events[0].run_total, 1);
    assert_eq!(progress.events[0].batch_total, 2);
    assert!(
        progress
            .events
            .iter()
            .all(|event| event.operation == "batch_run")
    );
    let retry = progress
        .events
        .iter()
        .find(|event| event.event == "retry_started")
        .unwrap();
    assert_eq!(retry.attempt, Some(2));
    let checkpoint = progress
        .events
        .iter()
        .find(|event| event.event == "item_checkpointed")
        .unwrap();
    assert_eq!(checkpoint.attempt, Some(2));
    assert_eq!(checkpoint.run_completed, 1);
}

#[tokio::test]
async fn pitch_rotation_and_launch_heartbeat_have_explicit_session_context() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "rotation-context", &["一", "二"]);
    let (permit, receiver) = futures::channel::oneshot::channel();
    let mut driver = ScriptedPitchDriver::new(
        &store,
        "rotation-context",
        vec![ScriptedPitchAction::Complete, ScriptedPitchAction::Complete],
    );
    driver.slow_launch = Some((2, receiver));
    let mut progress = CapturedPitchProgress {
        release_on_session_heartbeat: Some((2, permit)),
        ..CapturedPitchProgress::default()
    };
    super::run_batch_with_driver(
        &store,
        "rotation-context",
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(1),
        no_pitch_interruption(),
    )
    .await
    .unwrap();
    let checkpoint = progress
        .events
        .iter()
        .position(|event| event.event == "item_checkpointed")
        .unwrap();
    let rotation = progress
        .events
        .iter()
        .position(|event| event.event == "browser_session_rotated")
        .unwrap();
    let heartbeat = progress
        .events
        .iter()
        .position(|event| event.event == "heartbeat" && event.session == Some(2))
        .unwrap();
    let next_start = progress
        .events
        .iter()
        .rposition(|event| event.event == "item_started")
        .unwrap();
    assert!(checkpoint < rotation && rotation < heartbeat && heartbeat < next_start);
    assert_eq!(progress.events[rotation].session, Some(1));
    assert_eq!(progress.events[rotation].next_session, Some(2));
    assert_eq!(progress.events[heartbeat].session, Some(2));
    for event in &progress.events {
        if let Some(identity) = event.identity.as_ref() {
            if event.session.is_some() {
                assert_eq!(
                    event.session,
                    Some(if identity.key == "一" { 1 } else { 2 }),
                    "event={} identity={} context={event:?}",
                    event.event,
                    identity.key
                );
                assert!(event.worker_session.is_some());
            } else {
                assert_eq!(event.event, "heartbeat");
                assert!(event.worker_session.is_none());
            }
            assert_eq!(event.attempt, Some(1));
            assert!(event.next_session.is_none());
        } else {
            assert!(
                event.attempt.is_none(),
                "событие {} унаследовало попытку",
                event.event
            );
        }
    }
}

#[test]
fn pitch_human_progress_localizes_protocol_values_without_changing_jsonl() {
    let cases = [
        (
            "browser_session_rotated",
            None,
            Some("item_limit"),
            "смена сессии браузера",
            "достигнут лимит записей сессии",
        ),
        (
            "run_stopped",
            None,
            Some("session_failure"),
            "получение остановлено",
            "ошибка сессии браузера",
        ),
        (
            "item_checkpointed",
            Some("technical_failure"),
            None,
            "результат записи сохранён",
            "техническая ошибка",
        ),
        (
            "item_checkpointed",
            Some("vocabulary_not_found"),
            None,
            "результат записи сохранён",
            "запись JPDB не найдена",
        ),
        (
            "item_discarded_stale",
            None,
            Some("item_token_changed"),
            "устаревший результат отброшен",
            "запись изменена другим действием",
        ),
    ];
    for (event, outcome, reason, label, detail) in cases {
        let progress = super::PitchProgressEvent {
            schema_version: 1,
            operation: "batch_run",
            event,
            batch_id: "human-progress".into(),
            elapsed_ms: 25,
            run_completed: 1,
            run_total: 2,
            batch_total: 2,
            workers: Some(2),
            in_flight: Some(0),
            identity: None,
            worker: None,
            worker_session: None,
            session: Some(1),
            next_session: None,
            attempt: None,
            recovery: None,
            outcome: outcome.map(str::to_owned),
            reason: reason.map(str::to_owned),
        };
        let mut human = Vec::new();
        super::write_pitch_progress(&progress, OutputFormat::Human, &mut human).unwrap();
        let human = String::from_utf8(human).unwrap();
        assert!(human.contains(label));
        assert!(human.contains(detail));
        assert!(!human.contains(event));
        if let Some(value) = outcome.or(reason) {
            assert!(!human.contains(value));
        }
        let mut machine = Vec::new();
        super::write_pitch_progress(&progress, OutputFormat::Json, &mut machine).unwrap();
        let machine: serde_json::Value = serde_json::from_slice(&machine).unwrap();
        assert_eq!(machine["event"], event);
        assert_eq!(machine["outcome"].as_str(), outcome);
        assert_eq!(machine["reason"].as_str(), reason);
    }
}

#[tokio::test]
async fn pitch_saved_typed_failure_is_available_in_status_and_error_json_summary() {
    for failure in [
        JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "контракт строки: has_forms=false; 日本語\nточная диагностика".into(),
        },
        JpdbPitchFailure::Telemetry {
            stage: JpdbPitchStage::SearchReadiness,
            message: "monitor_failed=true; relevant_pending=2; безопасная подробность".into(),
        },
    ] {
        let workspace = temp_root();
        let root = workspace.path();
        let store = store_at(root);
        offline_pitch_batch(&store, "typed-summary", &["一"]);
        let token = load_batch(&store, "typed-summary")
            .unwrap()
            .item_token("一")
            .unwrap();
        assert!(
            super::record_one_outcome(
                &store,
                "typed-summary",
                &token,
                JpdbPitchOutcome::Failed {
                    error: failure.clone()
                },
                crate::browser_diagnostics::BrowserItemTimer::new(
                    crate::browser_diagnostics::BrowserItemContext::new(
                        "jpdb",
                        "一",
                        1,
                        u64::from(token.generation)
                    ),
                ),
                None,
            )
            .unwrap()
        );
        let saved = load_batch(&store, "typed-summary").unwrap();
        let expected = serde_json::to_value(&failure).unwrap();
        let snapshot = serde_json::to_value(&saved).unwrap();
        assert_eq!(
            snapshot["items"][0]["attempts"][0]["outcome"]["error"],
            expected
        );
        let status = execute(cli(
            store.root().to_path_buf(),
            root.to_path_buf(),
            OutputFormat::Json,
            PitchCommand::Batch {
                command: PitchBatchCommand::Status {
                    batch_id: "typed-summary".into(),
                },
            },
        ))
        .await;
        assert_eq!(status.exit_code, 0);
        let status: serde_json::Value = serde_json::from_str(&status.stdout).unwrap();
        assert_eq!(status["items"][0]["failure"], expected);
        assert_eq!(status["items"][0]["last_outcome"]["error"], expected);
        let stopped = super::render_batch_error(
            &store,
            StoreSummary {
                path: store.root().display().to_string(),
                store_id: store.store_id().into(),
            },
            "typed-summary",
            "batch_run",
            super::pitch_recovery_exhausted(
                "session_failure",
                "получение остановлено из-за ошибки сессии браузера",
                Some(&failure),
            ),
            OutputFormat::Json,
            true,
        );
        let stopped: serde_json::Value = serde_json::from_str(&stopped.stdout).unwrap();
        assert_eq!(stopped["items"][0]["failure"], expected);
        assert_eq!(stopped["error"]["details"]["session_failure"], expected);
    }
}

#[tokio::test]
async fn json_batch_response_exposes_flushed_log_on_success_and_failure() {
    for succeeds in [true, false] {
        let workspace = temp_root();
        let root = workspace.path();
        let store = store_at(root);
        let batch_id = if succeeds {
            save_durable_candidate(&store, "diagnostic-success", "幽霊");
            "diagnostic-success"
        } else {
            "diagnostic-missing-batch"
        };
        let summary = StoreSummary {
            path: store.root().display().to_string(),
            store_id: store.store_id().to_owned(),
        };
        let output = super::run_batch_output(
            &store,
            batch_id,
            "batch_run",
            summary,
            OutputFormat::Json,
            false,
            1,
        )
        .await;
        assert_eq!(output.stderr, "");
        let response: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
        let log_path = PathBuf::from(response["diagnostic_log"].as_str().unwrap());
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
        let terminal_event = if succeeds {
            "run_finished"
        } else {
            "run_stopped"
        };
        let terminal = events
            .iter()
            .find(|event| event["fields"]["event"] == terminal_event)
            .unwrap();
        assert!(terminal["spans"].as_array().unwrap().iter().any(|span| {
            span["name"] == "pitch_batch_run"
                && span["run_id"] == terminal["fields"]["run_id"]
                && span["diagnostic_log"] == response["diagnostic_log"]
        }));
    }
}

#[tokio::test]
async fn pitch_failure_diagnostics_log_each_accepted_failure_once() {
    let cases = [
        (ScriptedPitchAction::Complete, false, 0, 0, 0),
        (
            ScriptedPitchAction::SessionFailure {
                after_outcome: false,
            },
            false,
            1,
            0,
            0,
        ),
        (ScriptedPitchAction::Complete, true, 0, 1, 0),
        (
            ScriptedPitchAction::SessionFailure {
                after_outcome: true,
            },
            false,
            1,
            0,
            0,
        ),
        (ScriptedPitchAction::ItemFailure, false, 0, 0, 1),
        (
            ScriptedPitchAction::ItemFailureWithSessionFailure,
            false,
            1,
            0,
            1,
        ),
    ];
    for (action, launch_failure, session_failures, setup_failures, item_failures) in cases {
        let workspace = temp_root();
        let store = store_at(workspace.path());
        let batch_id = "diagnostic-failure-count";
        offline_pitch_batch(&store, batch_id, &["雨"]);
        let requeued = matches!(
            action,
            ScriptedPitchAction::SessionFailure {
                after_outcome: false
            }
        );
        let mut actions = vec![action];
        if requeued {
            actions.push(ScriptedPitchAction::Complete);
        }
        let mut driver = ScriptedPitchDriver::new(&store, batch_id, actions);
        driver.fail_launch_on = launch_failure.then_some(1);
        let mut progress = CapturedPitchProgress::default();
        let path = workspace.path().join("diagnostics.jsonl");
        let log = crate::diagnostics::RunLogGuard::new(
            fs::File::create(&path).unwrap(),
            crate::diagnostics::OutputMode::Json,
        );
        super::run_batch_with_driver(
            &store,
            batch_id,
            "batch_run",
            &mut driver,
            &mut progress,
            offline_pitch_policy(8),
            no_pitch_interruption(),
        )
        .with_subscriber(log.dispatch())
        .await
        .unwrap();
        log.finish().unwrap();

        let events = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        let failures = events
            .iter()
            .filter(|event| event["fields"]["event"] == "pitch_failure")
            .map(|event| &event["fields"])
            .collect::<Vec<_>>();
        assert_eq!(
            failures.len(),
            session_failures + setup_failures + item_failures
        );
        for (code, expected, session_failure) in [
            ("session_failure", session_failures, true),
            ("browser_setup", setup_failures, true),
            ("timeout", item_failures, false),
        ] {
            let matching = failures
                .iter()
                .filter(|fields| fields["code"] == code)
                .collect::<Vec<_>>();
            assert_eq!(matching.len(), expected, "код {code}");
            for fields in matching {
                assert_eq!(fields["session_failure"], session_failure);
                assert_eq!(fields["identity"], "雨");
                assert_eq!(fields["worker"], 1);
                assert_eq!(fields["worker_session"], 1);
                assert_eq!(fields["session"], 1);
                assert_eq!(fields["attempt"], 1);
            }
        }
        assert_eq!(
            load_batch(&store, batch_id).unwrap().items[0]
                .attempts
                .len(),
            1
        );
    }
}

#[tokio::test]
async fn session_failure_log_records_checkpointed_prefix_before_recovery_exhaustion() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "diagnostic-session-failure", &["一", "二", "三"]);
    let run_log =
        crate::batch_runtime::SafeBatchRuntime::open(store.root(), "diagnostic-session-failure")
            .unwrap()
            .create_run_log()
            .unwrap();
    let run_id = run_log.run_id;
    let log_path = run_log.path.display().to_string();
    let guard =
        crate::diagnostics::RunLogGuard::new(run_log.file, crate::diagnostics::OutputMode::Json);
    // Настоящий результат для «一», затем серия отказов сессии на «二»:
    // бюджет восстановления исчерпан без фиктивных попыток для хвоста.
    let mut actions = vec![ScriptedPitchAction::Complete];
    for _ in 0..super::PITCH_SESSION_RECOVERY_BUDGET {
        actions.push(ScriptedPitchAction::SessionFailure {
            after_outcome: false,
        });
    }
    let mut driver = ScriptedPitchDriver::new(&store, "diagnostic-session-failure", actions);
    let mut progress = CapturedPitchProgress::default();
    let batch_id = "diagnostic-session-failure";
    let operation = "batch_run";
    let span_run_id = run_id.clone();
    let span_log_path = log_path.clone();
    let future = async {
        let span = tracing::info_span!(
            "pitch_batch_run",
            operation,
            batch_id,
            run_id = span_run_id,
            diagnostic_log = span_log_path,
        );
        async {
            tracing::info!(event = "run_started", operation, batch_id, run_id);
            let result = super::run_batch_with_driver(
                &store,
                batch_id,
                operation,
                &mut driver,
                &mut progress,
                offline_pitch_policy(8),
                no_pitch_interruption(),
            )
            .await;
            if let Err(error) = &result {
                tracing::error!(
                    event = "run_stopped",
                    operation,
                    batch_id,
                    run_id,
                    code = error.code.as_str(),
                    message = %crate::diagnostics::safe_message(&error.message),
                );
            }
            result
        }
        .instrument(span)
        .await
    }
    .with_subscriber(guard.dispatch());
    let error = future.await.unwrap_err();
    assert_eq!(
        error.details["run_stop_reason"],
        "session_recovery_exhausted"
    );
    guard.finish().unwrap();

    let batch = load_batch(&store, batch_id).unwrap();
    assert_eq!(batch.items[0].attempts.len(), 1);
    assert!(batch.items[1..].iter().all(|item| item.attempts.is_empty()));
    assert_eq!(
        progress
            .events
            .iter()
            .filter(|event| event.event == "item_started")
            .map(|event| event.identity.as_ref().unwrap().key.as_str())
            .collect::<Vec<_>>(),
        ["一", "二", "三", "二"]
    );

    let contents = fs::read_to_string(log_path).unwrap();
    assert!(!contents.contains('\u{1b}'));
    let events = contents
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let failures = events
        .iter()
        .filter(|event| event["fields"]["event"] == "pitch_failure")
        .collect::<Vec<_>>();
    assert_eq!(
        failures.len(),
        super::PITCH_SESSION_RECOVERY_BUDGET as usize
    );
    assert!(
        failures
            .iter()
            .all(|event| event["fields"]["code"] == "session_failure")
    );
    let failure = failures[0];
    assert_eq!(failure["fields"]["code"], "session_failure");
    assert_eq!(failure["fields"]["identity"], "二");
    assert_eq!(failure["fields"]["worker"], 1);
    assert_eq!(failure["fields"]["worker_session"], 1);
    assert_eq!(failure["fields"]["session"], 1);
    assert_eq!(failure["fields"]["attempt"], 1);
    assert!(
        failure["fields"]["message"]
            .as_str()
            .unwrap()
            .contains("автономной проверке")
    );
    assert!(events.iter().any(|event| {
        event["fields"]["event"] == "run_stopped"
            && event["spans"]
                .as_array()
                .unwrap()
                .iter()
                .any(|span| span["name"] == "pitch_batch_run" && span["run_id"] == run_id)
    }));
    // Успешный элемент не оставляет в журнале ни одного отказа.
    assert_eq!(
        events
            .iter()
            .filter(|event| {
                event["fields"]["event"] == "pitch_failure" && event["fields"]["identity"] == "一"
            })
            .count(),
        0
    );
}

/// Каждая неудачная подготовка страницы описывается в диагностике; после
/// исчерпания бюджета восстановления элемент остаётся без попытки.
#[tokio::test]
async fn pitch_setup_failure_records_diagnostics_without_fabricating_attempt() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    let batch_id = "diagnostic-setup-recovery";
    offline_pitch_batch(&store, batch_id, &["雨"]);
    let mut actions = Vec::new();
    for _ in 0..super::PITCH_SESSION_RECOVERY_BUDGET {
        actions.push(ScriptedPitchAction::SetupFailure);
    }
    let mut driver = ScriptedPitchDriver::new(&store, batch_id, actions);
    let mut progress = CapturedPitchProgress::default();
    let path = workspace.path().join("diagnostics.jsonl");
    let log = crate::diagnostics::RunLogGuard::new(
        fs::File::create(&path).unwrap(),
        crate::diagnostics::OutputMode::Json,
    );
    let error = super::run_batch_with_driver(
        &store,
        batch_id,
        "batch_run",
        &mut driver,
        &mut progress,
        offline_pitch_policy(8),
        no_pitch_interruption(),
    )
    .with_subscriber(log.dispatch())
    .await
    .unwrap_err();
    log.finish().unwrap();

    assert_eq!(
        error.details["run_stop_reason"],
        "session_recovery_exhausted"
    );
    let events = read_browser_diagnostic_events(&path);
    let items = events
        .iter()
        .filter(|fields| fields["event"] == "browser_acquisition_item")
        .collect::<Vec<_>>();
    assert_eq!(
        items.len(),
        super::PITCH_SESSION_RECOVERY_BUDGET as usize,
        "каждая неудачная сессия описывается отдельно"
    );
    for item in &items {
        assert_eq!(item["outcome"], "failure");
        assert_eq!(item["failure_code"], "browser_configuration");
        assert_eq!(item["stop_reason"], "session_failure");
        assert_eq!(item["identity"], "雨");
        assert_eq!(item["attempt"], 1);
        assert_eq!(item["worker"], 1);
    }
    assert!(
        load_batch(&store, batch_id).unwrap().items[0]
            .attempts
            .is_empty(),
        "отказ подготовки страницы не создаёт попытку"
    );
}

fn read_browser_diagnostic_events(path: &std::path::Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .filter(|event| event["fields"]["schema"] == "browser_acquisition_v1")
        .map(|event| event["fields"].clone())
        .collect()
}

#[tokio::test]
async fn pitch_worker_diagnostics_continue_into_real_checkpoint_and_keep_negative_outcomes() {
    let (signal, interrupted) = futures::channel::oneshot::channel();
    let cases = [
        (
            ScriptedPitchAction::Complete,
            "vocabulary_not_found",
            None,
            None,
        ),
        (
            ScriptedPitchAction::Reacquire,
            "discarded_stale",
            None,
            None,
        ),
        (
            ScriptedPitchAction::ItemFailure,
            "failure",
            Some("timeout"),
            None,
        ),
        (
            ScriptedPitchAction::SessionFailure {
                after_outcome: true,
            },
            "vocabulary_not_found",
            None,
            Some("session_failure"),
        ),
        (
            ScriptedPitchAction::InterruptInFlight(signal),
            "interrupted",
            Some("interrupted"),
            Some("acquisition_interrupted"),
        ),
    ];
    let mut interrupted = Some(interrupted);
    for (action, expected_outcome, failure_code, expected_stop_reason) in cases {
        let workspace = temp_root();
        let store = store_at(workspace.path());
        let batch_id = "diagnostic-lifecycle";
        offline_pitch_batch(&store, batch_id, &["雨"]);
        // Реальное ненулевое generation исключает подмену контекста provider-only.
        let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id).unwrap();
        let mut batch = runtime.load().unwrap().unwrap();
        batch
            .reacquire("雨", "новое получение для проверки контекста".into())
            .unwrap();
        runtime.save(&batch).unwrap();
        let generation = batch.item_token("雨").unwrap().generation;
        drop(runtime);
        let mut driver = ScriptedPitchDriver::new(&store, batch_id, vec![action]);
        let mut progress = CapturedPitchProgress::default();
        let path = workspace.path().join("diagnostics.jsonl");
        let log = crate::diagnostics::RunLogGuard::new(
            fs::File::create(&path).unwrap(),
            crate::diagnostics::OutputMode::Json,
        );
        let interrupt: std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>>>> =
            if expected_outcome == "interrupted" {
                Box::pin(pitch_signal(interrupted.take().unwrap()))
            } else {
                Box::pin(no_pitch_interruption())
            };
        let result = super::run_batch_with_driver(
            &store,
            batch_id,
            "batch_run",
            &mut driver,
            &mut progress,
            offline_pitch_policy(8),
            interrupt,
        )
        .with_subscriber(log.dispatch())
        .await;
        if expected_outcome == "interrupted" {
            assert!(result.is_err());
        } else {
            // Отказ сессии после настоящего результата не отменяет уже
            // сохранённую контрольную точку: запуск продолжается.
            assert!(result.is_ok());
        }
        log.finish().unwrap();
        let events = read_browser_diagnostic_events(&path);
        assert_eq!(
            events
                .iter()
                .filter(|fields| fields["event"] == "browser_acquisition_item")
                .count(),
            1
        );
        let item = events
            .iter()
            .find(|fields| fields["event"] == "browser_acquisition_item")
            .unwrap();
        assert_eq!(item["outcome"], expected_outcome);
        assert_eq!(item["failure_code"].as_str(), failure_code);
        assert_eq!(item["stop_reason"].as_str(), expected_stop_reason);
        assert_ne!(item["outcome"], "success");
        assert_eq!(events[0]["stage"], "browser_launch");
        for event in &events {
            assert_eq!(event["identity"], "雨");
            assert_eq!(event["attempt"], 1);
            assert_eq!(event["generation"], generation);
            assert_eq!(event["worker"], 1);
            assert_eq!(event["worker_session"], 1);
            assert_eq!(event["browser_session"], 1);
        }
        let checkpoint = events
            .iter()
            .position(|fields| fields["stage"] == "checkpoint");
        let item_index = events
            .iter()
            .position(|fields| fields["event"] == "browser_acquisition_item")
            .unwrap();
        if matches!(expected_outcome, "interrupted") {
            assert_eq!(checkpoint, None);
            assert!(
                load_batch(&store, batch_id).unwrap().items[0]
                    .attempts
                    .is_empty()
            );
        } else {
            let checkpoint =
                checkpoint.expect("полученный результат проходит рабочую контрольную точку CAS");
            assert!(checkpoint < item_index);
            assert!(
                events[checkpoint]["item_duration_ms"].as_u64().unwrap()
                    <= item["item_duration_ms"].as_u64().unwrap()
            );
            if expected_outcome == "discarded_stale" {
                assert_eq!(events[checkpoint]["outcome"], "discarded_stale");
                assert!(
                    load_batch(&store, batch_id).unwrap().items[0]
                        .attempts
                        .is_empty()
                );
            } else {
                assert!(
                    events
                        .iter()
                        .any(|fields| fields["stage"] == "outcome_validation")
                );
                assert_eq!(
                    load_batch(&store, batch_id).unwrap().items[0]
                        .attempts
                        .len(),
                    1
                );
            }
        }
    }
}

#[test]
fn pitch_candidate_validation_and_durable_failure_complete_the_original_item_timer() {
    use crate::browser_diagnostics::{BrowserItemContext, BrowserItemTimer};

    for case in ["acquired", "invalid_outcome", "missing_batch"] {
        let workspace = temp_root();
        let store = store_at(workspace.path());
        let batch_id = "diagnostic-final-validation";
        create_batch(
            &store,
            batch_id,
            &[PitchPlanItem {
                surface: "幽霊".into(),
                reading: Some("ゆうれい".into()),
                selection: None,
            }],
            None,
        )
        .unwrap();
        let token = load_batch(&store, batch_id)
            .unwrap()
            .item_token("幽霊")
            .unwrap();
        let path = workspace.path().join("diagnostics.jsonl");
        let log = crate::diagnostics::RunLogGuard::new(
            fs::File::create(&path).unwrap(),
            crate::diagnostics::OutputMode::Json,
        );
        let bytes = png();
        let result = log.with_default(|| {
            let timer = BrowserItemTimer::new(
                BrowserItemContext::new("jpdb", "幽霊", 2, u64::from(token.generation))
                    .with_worker(3, 4)
                    .with_browser_session(42),
            );
            timer.stage("post_capture_verification").finish_success();
            // Проверяем время между провайдером и долговременным сохранением,
            // включая промежуток после завершения этапов провайдера.
            std::thread::sleep(std::time::Duration::from_millis(5));
            super::record_one_outcome(
                &store,
                if case == "missing_batch" {
                    "missing"
                } else {
                    batch_id
                },
                &token,
                if case == "invalid_outcome" {
                    JpdbPitchOutcome::VocabularyNotFound {
                        surface: "別の語".into(),
                        reading: None,
                    }
                } else {
                    JpdbPitchOutcome::Acquired {
                        asset: Box::new(JpdbPitchAcquired {
                            bytes,
                            metadata: metadata("幽霊", "ゆうれい", 123),
                        }),
                    }
                },
                timer,
                None,
            )
        });
        log.finish().unwrap();
        let events = read_browser_diagnostic_events(&path);
        let item = events.last().unwrap();
        assert_eq!(item["event"], "browser_acquisition_item");
        assert!(item["item_duration_ms"].as_u64().unwrap() >= 5);
        assert!(
            item["item_duration_ms"].as_u64().unwrap()
                > events[0]["item_duration_ms"].as_u64().unwrap()
        );
        for event in &events {
            assert_eq!(event["identity"], "幽霊");
            assert_eq!(event["attempt"], 2);
            assert_eq!(event["generation"], token.generation);
            assert_eq!(event["worker"], 3);
            assert_eq!(event["worker_session"], 4);
            assert_eq!(event["browser_session"], 42);
        }
        let checkpoint = events
            .iter()
            .find(|fields| fields["stage"] == "checkpoint")
            .unwrap();
        if case != "acquired" {
            assert!(result.is_err());
            assert_eq!(checkpoint["outcome"], "failure");
            assert_eq!(item["outcome"], "failure");
            if case == "invalid_outcome" {
                let validation = events
                    .iter()
                    .find(|fields| fields["stage"] == "outcome_validation")
                    .unwrap();
                assert_eq!(validation["outcome"], "failure");
                assert_eq!(validation["failure_code"], "invalid_validation_evidence");
            }
            assert!(
                load_batch(&store, batch_id).unwrap().items[0]
                    .attempts
                    .is_empty()
            );
        } else {
            assert!(result.unwrap());
            let names = events
                .iter()
                .map(|fields| fields["stage"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                names,
                [
                    "post_capture_verification",
                    "candidate_validation",
                    "outcome_validation",
                    "checkpoint",
                    "item"
                ]
            );
            assert_eq!(item["outcome"], "acquired");
        }
    }
}

/// Машинный результат пакетного повтора обязан точно перечислять выбранные
/// элементы и оставаться идемпотентным при повторном запуске.
#[tokio::test]
async fn batch_retry_technical_reports_exact_selection_and_is_idempotent() {
    let workspace = temp_root();
    let store = store_at(workspace.path());
    let surfaces = ["一", "二", "三", "四", "五", "六"];
    offline_pitch_batch(&store, "cli-retry-technical", &surfaces);
    {
        let mut runtime =
            PitchAccentBatchRuntime::open(store.root(), "cli-retry-technical").unwrap();
        let mut batch = runtime.load().unwrap().unwrap();
        let timeout = || JpdbPitchOutcome::Failed {
            error: JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::DetailReadiness,
                diagnostic: Some("Истёк лимит запроса".into()),
            },
        };
        let outcomes = [
            ("一", timeout()),
            (
                "二",
                JpdbPitchOutcome::Failed {
                    error: JpdbPitchFailure::PageContract {
                        stage: JpdbPitchStage::DetailReadiness,
                        message: "Страница не соответствует контракту".into(),
                    },
                },
            ),
            (
                "三",
                JpdbPitchOutcome::VocabularyNotFound {
                    surface: "三".into(),
                    reading: None,
                },
            ),
            ("四", timeout()),
            ("五", timeout()),
            ("六", timeout()),
        ];
        for (surface, outcome) in outcomes {
            let cas = batch.item_token(surface).unwrap();
            assert!(runtime.record_outcome(&mut batch, &cas, outcome).unwrap());
        }
    }

    let store_root = store.root().to_path_buf();
    let root = workspace.path().to_path_buf();
    let retry_technical = |reason: &'static str| {
        execute(cli(
            store_root.clone(),
            root.clone(),
            OutputFormat::Json,
            PitchCommand::Batch {
                command: PitchBatchCommand::RetryTechnical {
                    batch_id: "cli-retry-technical".into(),
                    reason: reason.into(),
                },
            },
        ))
    };
    let retry_one = |surface: &'static str, reason: &'static str| {
        execute(cli(
            store_root.clone(),
            root.clone(),
            OutputFormat::Json,
            PitchCommand::Batch {
                command: PitchBatchCommand::Retry {
                    batch_id: "cli-retry-technical".into(),
                    surface: surface.into(),
                    reason: reason.into(),
                },
            },
        ))
    };

    // Адресный повтор по-прежнему перечисляет ровно один элемент.
    let single = retry_one("六", "Адресный повтор временного сбоя").await;
    assert_eq!(single.exit_code, 0, "{}", single.stdout);
    let response: serde_json::Value = serde_json::from_str(&single.stdout).unwrap();
    assert_eq!(response["operation"], "batch_retry");
    assert_eq!(response["retried_identities"], serde_json::json!(["六"]));

    // Пакетный повтор выбирает все текущие устранимые сбои в порядке пакета.
    let first = retry_technical("Пакетный повтор временных сбоев").await;
    assert_eq!(first.exit_code, 0, "{}", first.stdout);
    assert!(first.stderr.is_empty());
    let response: serde_json::Value = serde_json::from_str(&first.stdout).unwrap();
    assert_eq!(response["operation"], "batch_retry_technical");
    assert_eq!(response["changed"], true);
    assert_eq!(
        response["retried_identities"],
        serde_json::json!(["一", "四", "五"])
    );

    // Повторный запуск уже нечего переводить: выбор пуст, состояние не меняется.
    let second = retry_technical("Повтор без выбранных элементов").await;
    assert_eq!(second.exit_code, 0, "{}", second.stdout);
    let response: serde_json::Value = serde_json::from_str(&second.stdout).unwrap();
    assert_eq!(response["changed"], false);
    assert!(
        response.get("retried_identities").is_none(),
        "пустой выбор не попадает в машинный результат"
    );

    // Неустранимый сбой остаётся недоступен и адресному повтору.
    let blocked = retry_one("二", "Повтор неустранимого сбоя").await;
    assert_ne!(blocked.exit_code, 0);
    let response: serde_json::Value = serde_json::from_str(&blocked.stdout).unwrap();
    assert_eq!(response["error"]["code"], "invalid_transition");
}

#[tokio::test]
async fn batch_retry_technical_uses_a_russian_human_operation_name() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    offline_pitch_batch(&store, "human-retry-technical", &["幽霊"]);
    {
        let mut runtime =
            PitchAccentBatchRuntime::open(store.root(), "human-retry-technical").unwrap();
        let mut batch = runtime.load().unwrap().unwrap();
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
    }

    let output = execute(cli(
        store.root().to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Human,
        PitchCommand::Batch {
            command: PitchBatchCommand::RetryTechnical {
                batch_id: "human-retry-technical".into(),
                reason: "Повтор технического сбоя".into(),
            },
        },
    ))
    .await;

    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    assert!(output.stderr.is_empty());
    assert!(
        output
            .stdout
            .contains("Операция: пакетный повтор технических сбоев")
    );
    assert!(!output.stdout.contains("batch_retry_technical"));
}

#[tokio::test]
async fn batch_retry_error_persists_only_owner_reconciliation() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    let batch_id = "retry-update-reconcile-error";
    offline_pitch_batch(&store, batch_id, &["幽霊"]);
    {
        let mut runtime = PitchAccentBatchRuntime::open(&store_root, batch_id).unwrap();
        let mut batch = runtime.load().unwrap().unwrap();
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
        batch
            .items
            .iter_mut()
            .find(|item| item.identity.key == "幽霊")
            .unwrap()
            .item_revision = u64::MAX - 1;
        runtime.save(&batch).unwrap();
    }

    store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes: png(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(
                    serde_json::to_value(metadata("幽霊", "ゆうれい", 123)).unwrap(),
                ),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();

    let expected = {
        let owner =
            PitchBatchOwnerSnapshot::from_records(store.verify_integrity().unwrap()).unwrap();
        let mut runtime = PitchAccentBatchRuntime::open(&store_root, batch_id).unwrap();
        let mut batch = runtime.load().unwrap().unwrap();
        batch.reconcile_owner(&owner).unwrap();
        batch
    };
    let expected_item = expected.item("幽霊").unwrap();
    assert_eq!(expected_item.item_revision, u64::MAX);
    assert_eq!(
        expected_item.status(),
        PitchBatchItemStatus::ExistingVerified
    );

    let output = execute(cli(
        store_root.clone(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::Retry {
                batch_id: batch_id.into(),
                surface: "幽霊".into(),
                reason: "Повтор после сверки владельца".into(),
            },
        },
    ))
    .await;
    assert_ne!(
        output.exit_code, 0,
        "повтор должен отказать при переполнении"
    );
    let response: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(response["error"]["code"], "invalid_transition");

    let mut runtime = PitchAccentBatchRuntime::open(&store_root, batch_id).unwrap();
    let actual = runtime.load().unwrap().unwrap();
    assert_eq!(
        actual, expected,
        "ошибка повтора должна сохранить только сверку владельца, без частичной мутации"
    );
}

#[tokio::test]
async fn batch_retry_technical_empty_selection_reports_owner_reconciliation_change() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    offline_pitch_batch(&store, "retry-technical-owner-change", &["幽霊"]);
    let previous = {
        let mut runtime =
            PitchAccentBatchRuntime::open(&store_root, "retry-technical-owner-change").unwrap();
        let batch = runtime.load().unwrap().unwrap();
        batch.item("幽霊").unwrap().clone()
    };
    assert_eq!(previous.status(), PitchBatchItemStatus::Pending);
    drop(store);
    let owner_store = AssetStore::open_existing_with_policy(
        StoreOptions::new(&store_root),
        PitchAccentDomainPolicy,
    )
    .unwrap();
    owner_store
        .ingest_verified(
            VerifiedIngestRequest {
                identity: AssetIdentity::new("pitch_accent", "幽霊").unwrap(),
                bytes: png(),
                provenance: Provenance {
                    source_kind: "jpdb_browser_capture".into(),
                    source_name: "jpdb-vocabulary-123.png".into(),
                },
                domain_metadata: Some(
                    serde_json::to_value(metadata("幽霊", "ゆうれい", 123)).unwrap(),
                ),
                replace_expected_sha256: None,
            },
            &PitchAccentImageValidator,
        )
        .unwrap();
    drop(owner_store);

    let output = execute(cli(
        store_root.to_path_buf(),
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::RetryTechnical {
                batch_id: "retry-technical-owner-change".into(),
                reason: "Повтор после сверки владельца".into(),
            },
        },
    ))
    .await;
    assert_eq!(output.exit_code, 0, "{}", output.stderr);
    let response: serde_json::Value = serde_json::from_str(&output.stdout).unwrap();
    assert_eq!(response["changed"], true);
    assert!(response.get("retried_identities").is_none());
    assert_eq!(response["items"][0]["status"], "existing_verified");
    let after = &response["batch"]["items"][0];
    assert_eq!(after["generation"], previous.generation);
    assert_eq!(
        after["attempts"].as_array().unwrap().len(),
        previous.attempts.len()
    );

    let repeated = execute(cli(
        store_root,
        root.to_path_buf(),
        OutputFormat::Json,
        PitchCommand::Batch {
            command: PitchBatchCommand::RetryTechnical {
                batch_id: "retry-technical-owner-change".into(),
                reason: "Повтор без выбранных элементов".into(),
            },
        },
    ))
    .await;
    assert_eq!(repeated.exit_code, 0, "{}", repeated.stderr);
    let response: serde_json::Value = serde_json::from_str(&repeated.stdout).unwrap();
    assert_eq!(response["changed"], false);
    assert!(response.get("retried_identities").is_none());
}

#[tokio::test]
async fn review_export_and_apply_bind_decisions_to_snapshot_across_separate_processes() {
    let workspace = temp_root();
    let root = workspace.path();
    let store = store_at(root);
    let store_root = store.root().to_path_buf();
    let mut batch = PitchAccentBatch::new(
        "cli-review",
        vec![JpdbPitchRequest::new(JpdbPitchQuery::new(
            "幽霊",
            Some("ゆうれい".into()),
        ))],
        PitchAccentImageValidator::validator_identity(),
    )
    .unwrap();
    let mut runtime = PitchAccentBatchRuntime::create(store.root(), &batch).unwrap();
    let token = batch.item_token("幽霊").unwrap();
    assert!(
        runtime
            .record_outcome(
                &mut batch,
                &token,
                JpdbPitchOutcome::AmbiguousVocabulary {
                    surface: "幽霊".into(),
                    reading: Some("ゆうれい".into()),
                    candidates: ambiguity_candidates(),
                },
            )
            .unwrap()
    );
    drop(runtime);
    drop(store);
    let batch_command = |command| {
        execute(cli(
            store_root.clone(),
            root.to_path_buf(),
            OutputFormat::Json,
            PitchCommand::Batch { command },
        ))
    };

    // Export и apply — отдельные вызовы CLI: снимок и решения живут на диске.
    let exported = batch_command(PitchBatchCommand::ReviewExport {
        batch_id: "cli-review".into(),
    })
    .await;
    assert_eq!(exported.exit_code, 0, "{}", exported.stdout);
    let exported: serde_json::Value = serde_json::from_str(&exported.stdout).unwrap();
    assert_eq!(exported["operation"], "batch_review_export");
    let review_id = exported["review_id"].as_str().unwrap().to_owned();
    let decisions = PathBuf::from(exported["review_artifact"].as_str().unwrap());
    let text = fs::read_to_string(&decisions).unwrap();
    assert!(text.contains(&format!("@review\t1\tcli-review\t{review_id}")));
    fs::write(
        &decisions,
        text.replace(
            "\t幽霊\tambiguous_vocabulary",
            "124\t幽霊\tambiguous_vocabulary",
        ),
    )
    .unwrap();

    // Чужой review_id отклоняется, не меняя пакет.
    let foreign = batch_command(PitchBatchCommand::ReviewApply {
        batch_id: "cli-review".into(),
        review_id: "0".repeat(64),
    })
    .await;
    assert_ne!(foreign.exit_code, 0);
    let foreign: serde_json::Value = serde_json::from_str(&foreign.stdout).unwrap();
    assert_eq!(foreign["changed"], false);

    let applied = batch_command(PitchBatchCommand::ReviewApply {
        batch_id: "cli-review".into(),
        review_id: review_id.clone(),
    })
    .await;
    assert_eq!(applied.exit_code, 0, "{}", applied.stdout);
    let applied: serde_json::Value = serde_json::from_str(&applied.stdout).unwrap();
    assert_eq!(applied["changed"], true);
    assert_eq!(applied["review_decisions"]["selected"], 1);
    let saved = PitchAccentBatchRuntime::open(&store_root, "cli-review")
        .unwrap()
        .load()
        .unwrap()
        .unwrap();
    let selection = saved
        .item("幽霊")
        .unwrap()
        .request
        .selection
        .clone()
        .unwrap();
    assert_eq!(selection.vocabulary_id, 124);
    assert_eq!(
        selection.detail_url,
        "https://jpdb.io/vocabulary/124/幽霊/ゆうれい"
    );

    // Повтор того же снимка идемпотентен.
    let replay = batch_command(PitchBatchCommand::ReviewApply {
        batch_id: "cli-review".into(),
        review_id,
    })
    .await;
    assert_eq!(replay.exit_code, 0, "{}", replay.stdout);
    let replay: serde_json::Value = serde_json::from_str(&replay.stdout).unwrap();
    assert_eq!(replay["changed"], false);
    assert_eq!(replay["review_decisions"]["already_applied"], 1);
}
