//! Тонкий CLI-слой для управления пакетами кандзи и общим владельцем ресурсов.

use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Cursor, Read, Write};
#[cfg(feature = "session-rotation-acceptance")]
use std::num::NonZeroUsize;
use std::time::Instant;

use crate::batch::{
    AggregateResolution, BatchAttemptInput, BatchCandidate, BatchItemStatus, BatchRuntime,
    BatchTrustSource, CandidateRevalidation, HumanBatchAction, HumanBatchDecision, KanjiBatch,
    MAX_ACQUISITION_ROUNDS, MAX_HUMAN_REASON_BYTES,
};
use crate::batch_runtime::SafeBatchRuntime;
use crate::diagnostics::{OutputMode as DiagnosticOutputMode, RunLogGuard, safe_message};
use crate::domain::{AssetDomainPolicy, KanjiDomainPolicy};
use crate::model::{HumanDecision, Provenance, SemanticDecision, ValidationRecord};
use crate::store::{HumanAttestationRequest, validate_image_decode};
use crate::validation::ValidatorFailure;
use crate::yarxi::{
    AcquisitionEvent, AcquisitionRun, AcquisitionStreamError, AcquisitionWorkerEvent,
    AcquisitionWorkerEventKind,
};

#[derive(Debug, Subcommand)]
pub enum BatchCommand {
    /// Создаёт сохранённый пакет; повторное использование доверенных ресурсов проверяется без нового получения.
    Start {
        #[arg(long)]
        batch_id: Option<String>,
        #[arg(required = true, num_args = 1..)]
        characters: Vec<String>,
    },
    /// Продолжает пакет по уровням, не более указанного числа раундов.
    Run {
        #[arg(long)]
        batch_id: String,
        #[arg(long, default_value_t = MAX_ACQUISITION_ROUNDS)]
        rounds: u32,
        /// Число изолированных браузерных обработчиков; значение по умолчанию выбрано по сетевому замеру производительности.
        #[arg(long, default_value_t = 4, value_parser = clap::value_parser!(u8).range(1..=4))]
        workers: u8,
        /// После получения предложить ручную проверку; только полный TTY, без JSON/CI.
        #[arg(long)]
        interactive_review: bool,
        /// Только в приёмочной сборке: менять сессию браузера после N сохранённых элементов.
        #[cfg(feature = "session-rotation-acceptance")]
        #[arg(long, value_name = "N")]
        acceptance_rotate_after_items: Option<NonZeroUsize>,
    },
    /// Возвращает состояние и свидетельства, не запускает получение или публикацию.
    Status {
        #[arg(long)]
        batch_id: String,
    },
    /// Создаёт локальную HTML-страницу с сохранёнными неразрешёнными кандидатами.
    Review {
        #[arg(long)]
        batch_id: String,
    },
    /// Повторно проверяет сохранённые байты текущим валидатором без получения.
    Revalidate {
        #[arg(long)]
        batch_id: String,
    },
    /// Экспортирует UTF-8 TSV и проверенные копии текущих кандидатов; не запускает приложения.
    ReviewExport {
        #[arg(long)]
        batch_id: String,
    },
    /// Слепой OCR сохранённых Uncertain; предложения не меняют доверие.
    Ocr {
        #[arg(long)]
        batch_id: String,
        #[command(flatten)]
        options: crate::kanji_ocr::OcrOptions,
    },
    /// Подтверждает просмотренные совпадения OCR из точного review-снимка.
    ReviewConfirmMatches {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        review_id: String,
        /// Явное заявление человека: изображения группы просмотрены.
        #[arg(long, required = true)]
        confirm_reviewed: bool,
    },
    /// Проверяет целиком и применяет только decisions.tsv указанного снимка.
    ReviewApply {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        review_id: String,
    },
    /// Явно предлагает открыть редактор и папку сохранённых кандидатов.
    ReviewInteractive {
        #[arg(long)]
        batch_id: String,
    },
    /// Структурированное решение человека для точных текущих байтов.
    Decide {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        character: String,
        #[arg(long)]
        sha256: String,
        #[arg(long, value_enum)]
        action: BatchActionArg,
        #[arg(long)]
        reason: String,
    },
    /// Повторно запускает получение для одного элемента. При наличии кандидата требуется точный SHA-256.
    Retry {
        #[arg(long)]
        batch_id: String,
        #[arg(long)]
        character: String,
        #[arg(long)]
        sha256: Option<String>,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BatchActionArg {
    Confirm,
    Reject,
    Reacquire,
}

impl From<BatchActionArg> for HumanBatchAction {
    fn from(action: BatchActionArg) -> Self {
        match action {
            BatchActionArg::Confirm => Self::Confirm,
            BatchActionArg::Reject => Self::Reject,
            BatchActionArg::Reacquire => Self::Reacquire,
        }
    }
}

impl BatchCommand {
    pub(super) fn operation(&self) -> &'static str {
        match self {
            Self::Start { .. } => "batch_start",
            Self::Run { .. } => "batch_run",
            Self::Status { .. } => "batch_status",
            Self::Review { .. } => "batch_review",
            Self::Revalidate { .. } => "batch_revalidate",
            Self::ReviewExport { .. } => "batch_review_export",
            Self::Ocr { .. } => "batch_ocr",
            Self::ReviewConfirmMatches { .. } => "batch_review_confirm_matches",
            Self::ReviewApply { .. } => "batch_review_apply",
            Self::ReviewInteractive { .. } => "batch_review_interactive",
            Self::Decide { .. } => "batch_decide",
            Self::Retry { .. } => "batch_retry",
        }
    }

    fn id(&self) -> Option<&str> {
        match self {
            Self::Start { batch_id, .. } => batch_id.as_deref(),
            Self::Run { batch_id, .. }
            | Self::Status { batch_id }
            | Self::Review { batch_id }
            | Self::Revalidate { batch_id }
            | Self::ReviewExport { batch_id }
            | Self::Ocr { batch_id, .. }
            | Self::ReviewConfirmMatches { batch_id, .. }
            | Self::ReviewApply { batch_id, .. }
            | Self::ReviewInteractive { batch_id }
            | Self::Decide { batch_id, .. }
            | Self::Retry { batch_id, .. } => Some(batch_id),
        }
    }
}

pub(super) fn prevalidate(command: &BatchCommand) -> Result<(), AssetError> {
    if let Some(id) = command.id() {
        crate::batch::validate_batch_id(id)?;
    }
    match command {
        BatchCommand::ReviewApply { review_id, .. }
        | BatchCommand::ReviewConfirmMatches { review_id, .. } => {
            crate::batch::validate_hash(review_id)?
        }
        BatchCommand::Ocr { options, .. } => {
            options.validate()?;
        }
        BatchCommand::Start { characters, .. } => {
            for character in characters {
                parse_character(character)?;
            }
            if characters.is_empty() {
                return Err(invalid("пакет не может быть пустым"));
            }
        }
        BatchCommand::Decide {
            character,
            sha256,
            reason,
            ..
        } => {
            parse_character(character)?;
            crate::batch::validate_hash(sha256)?;
            validate_reason(reason)?;
        }
        BatchCommand::Retry {
            character,
            sha256,
            reason,
            ..
        } => {
            parse_character(character)?;
            if let Some(hash) = sha256 {
                crate::batch::validate_hash(hash)?;
            }
            validate_reason(reason)?;
        }
        BatchCommand::Run { rounds, .. } if !(1..=MAX_ACQUISITION_ROUNDS).contains(rounds) => {
            return Err(invalid(format!(
                "число раундов должно быть от 1 до {MAX_ACQUISITION_ROUNDS}"
            )));
        }
        BatchCommand::Run { workers, .. } if !(1..=4).contains(workers) => {
            return Err(invalid("число обработчиков должно быть от 1 до 4"));
        }
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct BatchIssue {
    identity: AssetIdentity,
    code: String,
    message: String,
}

#[derive(Debug, Serialize)]
struct BatchCounts {
    requested: usize,
    effective_verified: usize,
    auto_verified: usize,
    human_verified: usize,
    existing_verified: usize,
    awaiting_human: usize,
    rejected: usize,
    scheduled_for_reacquire: usize,
    pending_publication: usize,
    unresolved: usize,
}

#[derive(Debug, Serialize)]
struct BatchItemOutcome {
    identity: AssetIdentity,
    state: BatchItemStatus,
    effective_verified: bool,
    candidate_sha256: Option<String>,
    published_sha256: Option<String>,
    publication_source: Option<BatchTrustSource>,
    acquisition_attempts: usize,
    generation: u32,
    distinct_valid_hashes: usize,
    item_outcome: &'static str,
}

#[derive(Debug, Serialize)]
struct BatchResponse {
    schema_version: u32,
    operation: String,
    store: StoreSummary,
    batch_id: String,
    outcome: &'static str,
    changed: bool,
    counts: BatchCounts,
    items: Vec<BatchItemOutcome>,
    issues: Vec<BatchIssue>,
    blockers: Vec<String>,
    review_artifact: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    review_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    review_decisions: Option<ReviewApplied>,
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostic_log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ocr_summary: Option<crate::kanji_ocr::OcrScanReport>,
    /// Источник истины предметной области остаётся в сохранённом состоянии владельца.
    batch: KanjiBatch,
}

#[derive(Debug, Clone, Serialize)]
struct BatchProgressEvent {
    schema_version: u32,
    operation: &'static str,
    event: &'static str,
    batch_id: String,
    elapsed_ms: u128,
    round: Option<u32>,
    round_limit: u32,
    round_completed: usize,
    round_total: usize,
    run_completed: usize,
    batch_total: usize,
    workers: u8,
    in_flight: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity: Option<AssetIdentity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worker_id: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    browser_session_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attempt: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner_attempt: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_elapsed_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stage_timings: Option<Vec<crate::browser_diagnostics::BrowserStageTiming>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

trait BatchProgressSink {
    fn emit(&mut self, event: BatchProgressEvent) -> Result<(), AssetError>;
}

struct StderrProgressSink {
    output: OutputFormat,
}

impl BatchProgressSink for StderrProgressSink {
    fn emit(&mut self, event: BatchProgressEvent) -> Result<(), AssetError> {
        write_progress_event(&event, self.output, &mut io::stderr().lock())
    }
}

#[cfg(test)]
#[derive(Default)]
struct NullProgressSink;

#[cfg(test)]
impl BatchProgressSink for NullProgressSink {
    fn emit(&mut self, _event: BatchProgressEvent) -> Result<(), AssetError> {
        Ok(())
    }
}

struct BatchProgressReporter<'a> {
    sink: &'a mut dyn BatchProgressSink,
    batch_id: String,
    started: Instant,
    round: Option<u32>,
    round_limit: u32,
    round_completed: usize,
    round_total: usize,
    run_completed: usize,
    batch_total: usize,
    workers: u8,
    in_flight: BTreeSet<String>,
    item_contexts: BTreeMap<String, BatchProgressItemContext>,
    last_identity: Option<AssetIdentity>,
    // Контекст подробного журнала: публичные события прогресса сохраняют прежние поля.
    browser_session: Option<u32>,
    acquisition_attempt: Option<u8>,
    generation: Option<u32>,
    owner_attempt: Option<usize>,
    item_pending_checkpoint: bool,
    last_checkpointed_identity: Option<AssetIdentity>,
    last_checkpoint_revision: Option<u64>,
    run_stop_reason: Option<&'static str>,
}

struct BatchProgressItemContext {
    worker_id: Option<usize>,
    browser_session_id: Option<u64>,
    session: Option<u32>,
    generation: u32,
    owner_attempt: usize,
    acquisition_attempt: Option<u8>,
    started: Instant,
}

impl BatchProgressReporter<'_> {
    fn begin_round(&mut self, round: u32, total: usize) {
        self.round = Some(round);
        self.round_completed = 0;
        self.round_total = total;
    }

    fn begin_item(
        &mut self,
        identity: &AssetIdentity,
        generation: u32,
        prior_attempts: usize,
        worker_id: Option<usize>,
        session: Option<u32>,
        browser_session_id: Option<u64>,
    ) {
        self.acquisition_attempt = Some(1);
        self.generation = Some(generation);
        self.owner_attempt = Some(prior_attempts + 1);
        let key = identity.key.clone();
        self.item_contexts
            .entry(key.clone())
            .and_modify(|context| {
                context.worker_id = worker_id.or(context.worker_id);
                context.browser_session_id = browser_session_id.or(context.browser_session_id);
                context.session = session.or(context.session);
                context.acquisition_attempt = Some(1);
            })
            .or_insert_with(|| BatchProgressItemContext {
                worker_id,
                browser_session_id,
                session,
                generation,
                owner_attempt: prior_attempts + 1,
                acquisition_attempt: Some(1),
                started: Instant::now(),
            });
        self.in_flight.insert(key);
        self.item_pending_checkpoint = true;
    }

    fn update_worker_context(
        &mut self,
        identity: &AssetIdentity,
        session: Option<u32>,
        worker_id: usize,
        browser_session_id: Option<u64>,
    ) {
        if let Some(context) = self.item_contexts.get_mut(&identity.key) {
            context.worker_id = Some(worker_id);
            context.browser_session_id = browser_session_id.or(context.browser_session_id);
            context.session = session.or(context.session);
        }
    }

    fn checkpointed(&mut self, identity: &AssetIdentity, revision: u64) {
        self.round_completed += 1;
        self.run_completed += 1;
        self.last_checkpointed_identity = Some(identity.clone());
        self.last_checkpoint_revision = Some(revision);
        self.in_flight.remove(&identity.key);
        self.item_pending_checkpoint = !self.in_flight.is_empty();
    }

    fn finish_item(&mut self, identity: &AssetIdentity) {
        self.in_flight.remove(&identity.key);
        self.item_contexts.remove(&identity.key);
        self.item_pending_checkpoint = !self.in_flight.is_empty();
    }

    fn emit(
        &mut self,
        event: &'static str,
        identity: Option<&AssetIdentity>,
        session: Option<u32>,
        attempt: Option<u8>,
        outcome: Option<String>,
        reason: Option<String>,
    ) -> Result<(), AssetError> {
        self.emit_with_context(
            event, identity, session, attempt, outcome, reason, None, None, None, None, None, None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_with_context(
        &mut self,
        event: &'static str,
        identity: Option<&AssetIdentity>,
        session: Option<u32>,
        attempt: Option<u8>,
        outcome: Option<String>,
        reason: Option<String>,
        worker_id: Option<usize>,
        browser_session_id: Option<u64>,
        generation: Option<u32>,
        owner_attempt: Option<usize>,
        item_elapsed_ms: Option<u128>,
        stage_timings: Option<Vec<crate::browser_diagnostics::BrowserStageTiming>>,
    ) -> Result<(), AssetError> {
        if identity.is_some() {
            self.last_identity = identity.cloned();
        }
        if let Some(session) = session {
            self.browser_session = Some(session);
        }
        if let Some(attempt) = attempt {
            self.acquisition_attempt = Some(attempt);
        }
        if let Some(identity) = identity
            && let Some(context) = self.item_contexts.get_mut(&identity.key)
        {
            context.worker_id = worker_id.or(context.worker_id);
            context.browser_session_id = browser_session_id.or(context.browser_session_id);
            context.session = session.or(context.session);
            context.generation = generation.unwrap_or(context.generation);
            context.owner_attempt = owner_attempt.unwrap_or(context.owner_attempt);
            context.acquisition_attempt = attempt.or(context.acquisition_attempt);
        }
        let context = identity.and_then(|identity| self.item_contexts.get(&identity.key));
        let worker_id = worker_id.or_else(|| context.and_then(|context| context.worker_id));
        let browser_session_id =
            browser_session_id.or_else(|| context.and_then(|context| context.browser_session_id));
        let generation = generation
            .or_else(|| context.map(|context| context.generation))
            .or_else(|| worker_id.is_none().then_some(self.generation).flatten());
        let owner_attempt = owner_attempt
            .or_else(|| context.map(|context| context.owner_attempt))
            .or_else(|| worker_id.is_none().then_some(self.owner_attempt).flatten());
        let item_elapsed_ms = item_elapsed_ms
            .or_else(|| context.map(|context| context.started.elapsed().as_millis()));
        let session = context.and_then(|context| context.session).or(session);
        let reason = reason.map(|reason| safe_message(&reason));
        tracing::info!(
            diagnostic_schema = "kanji_batch_progress_v1",
            provider = "yarxi", domain = "kanji",
            stage = event, batch_id = %self.batch_id,
            identity = identity.map(|identity| identity.key.as_str()), session, attempt,
            browser_session = self.browser_session,
            acquisition_attempt = self.acquisition_attempt,
            worker_id, browser_session_id,
            generation, owner_attempt,
            run_stop_reason = self.run_stop_reason,
            last_checkpointed_identity = self.last_checkpointed_identity.as_ref().map(|identity| identity.key.as_str()),
            last_checkpoint_revision = self.last_checkpoint_revision,
            checkpointed_items = self.run_completed,
            item_pending_checkpoint = self.item_pending_checkpoint,
            outcome = ?outcome, reason = ?reason,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            round = ?self.round, round_completed = self.round_completed,
            round_total = self.round_total, run_completed = self.run_completed,
            batch_total = self.batch_total, workers = self.workers,
            in_flight = self.in_flight.len(), item_elapsed_ms,
            stage_timings = ?stage_timings,
            "Прогресс обработки пакета"
        );
        self.sink.emit(BatchProgressEvent {
            schema_version: 1,
            operation: "batch_run",
            event,
            batch_id: self.batch_id.clone(),
            elapsed_ms: self.started.elapsed().as_millis(),
            round: self.round,
            round_limit: self.round_limit,
            round_completed: self.round_completed,
            round_total: self.round_total,
            run_completed: self.run_completed,
            batch_total: self.batch_total,
            workers: self.workers,
            in_flight: self.in_flight.len(),
            identity: identity.cloned(),
            session,
            worker_id,
            browser_session_id,
            attempt,
            generation,
            owner_attempt,
            item_elapsed_ms,
            stage_timings,
            outcome,
            reason,
        })
    }
}

fn write_progress_event(
    event: &BatchProgressEvent,
    output: OutputFormat,
    writer: &mut impl Write,
) -> Result<(), AssetError> {
    let line = match output {
        OutputFormat::Json => serde_json::to_string(event)
            .map_err(|error| invalid(format!("не удалось сериализовать прогресс: {error}")))?,
        OutputFormat::Human => {
            let identity = event
                .identity
                .as_ref()
                .map(|identity| identity.key.as_str())
                .unwrap_or("-");
            let outcome = event
                .outcome
                .as_deref()
                .map(progress_outcome_label)
                .unwrap_or("-");
            let reason = match event.reason.as_deref().unwrap_or("") {
                "finished" => "работа завершена",
                reason => reason,
            };
            let mut context = String::new();
            for (label, value) in [
                ("обработчик", event.worker_id.map(|value| value as u64)),
                ("сессия", event.session.map(u64::from)),
                ("браузер", event.browser_session_id),
                ("поколение", event.generation.map(u64::from)),
                (
                    "попытка владельца",
                    event.owner_attempt.map(|value| value as u64),
                ),
            ] {
                if let Some(value) = value {
                    context.push_str(&format!(" · {label}={value}"));
                }
            }
            format!(
                "Прогресс: {} · раунд {}/{} · запуск {} · всего {} · {:.1} с · символ={} · результат={}{} {}",
                progress_event_label(event.event),
                event.round_completed,
                event.round_total,
                event.run_completed,
                event.batch_total,
                event.elapsed_ms as f64 / 1000.0,
                identity,
                outcome,
                context,
                reason
            )
        }
    };
    writer
        .write_all(line.as_bytes())
        .and_then(|()| writer.write_all(b"\n"))
        .and_then(|()| writer.flush())
        .map_err(|error| AssetError::io("запись прогресса пакета в stderr", error))
}

fn progress_event_label(event: &str) -> &'static str {
    match event {
        "run_started" => "пакет запущен",
        "round_started" => "раунд начат",
        "browser_session_started" => "сессия браузера запущена",
        "browser_session_rotated" => "смена сессии браузера",
        "browser_session_ended" => "сессия браузера завершена",
        "item_started" => "получение начато",
        "retry_started" => "повтор получения",
        "retry_recovery_started" => "восстановление страницы",
        "heartbeat" => "операция выполняется",
        "item_checkpointed" => "результат сохранён",
        "item_discarded_stale" => "устаревший результат отброшен",
        "round_finished" => "раунд завершён",
        "run_stopped" => "пакет остановлен",
        "run_finished" => "пакет завершён",
        "worker_assigned" => "элемент назначен обработчику",
        "browser_session_bootstrap_started" => "подготовка сессии браузера",
        "browser_session_recovery_started" => "восстановление сессии браузера",
        "item_deferred" => "элемент отложен",
        "item_requeued" => "элемент возвращён в очередь",
        "worker_retired" => "обработчик выведен из пула",
        "worker_stopped" => "обработчик остановлен",
        "worker_exhausted" => "пул обработчиков исчерпан",
        _ => "событие прогресса",
    }
}

fn progress_outcome_label(outcome: &str) -> &str {
    match outcome {
        "candidate_recorded" => "кандидат сохранён",
        "acquisition_failed" => "ошибка получения",
        "source_identity_mismatch" => "источник не подтвердил символ",
        "media_format_mismatch" => "формат изображения не совпал",
        "invalid_validation_evidence" => "недопустимые доказательства проверки",
        "io_failure" => "ошибка ввода-вывода",
        "validator_failure" => "ошибка валидатора",
        "integrity_mismatch" => "нарушение целостности",
        other => localized_outcome(other),
    }
}

pub(super) fn execute(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    output: OutputFormat,
    allow_insecure_tls: bool,
) -> CliOutput {
    // События журнала, записанные путём проверки и публикации этого пакета,
    // помечаются его идентификатором. Сами события пишутся только из методов
    // store под эксклюзивной блокировкой, не из worker'ов пакета.
    store.set_journal_context(None, command.id());
    let mut progress = StderrProgressSink { output };
    execute_with_progress(
        store,
        summary,
        command,
        output,
        allow_insecure_tls,
        &mut progress,
    )
}

fn execute_with_progress(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    output: OutputFormat,
    allow_insecure_tls: bool,
    progress: &mut dyn BatchProgressSink,
) -> CliOutput {
    execute_with_progress_and_interaction(
        store,
        summary,
        command,
        output,
        allow_insecure_tls,
        progress,
        &mut super::review_ui::SystemReviewInteraction,
    )
}

fn interactive_requested(command: &BatchCommand) -> bool {
    matches!(
        command,
        BatchCommand::Run {
            interactive_review: true,
            ..
        } | BatchCommand::ReviewInteractive { .. }
    )
}

pub(super) fn prevalidate_output(
    command: &BatchCommand,
    output: OutputFormat,
) -> Result<(), AssetError> {
    if interactive_requested(command) && output == OutputFormat::Json {
        return Err(invalid(
            "интерактивная проверка несовместима с --output json",
        ));
    }
    Ok(())
}

fn execute_with_progress_and_interaction(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    output: OutputFormat,
    allow_insecure_tls: bool,
    progress: &mut dyn BatchProgressSink,
    interaction: &mut dyn super::review_ui::ReviewInteraction,
) -> CliOutput {
    let eligibility = prevalidate_output(command, output).and_then(|()| {
        if interactive_requested(command) && !interaction.eligible() {
            Err(invalid("интерактивная проверка требует полного TTY и отключённого CI; используйте review-export/review-apply"))
        } else { Ok(()) }
    });
    if let Err(error) = eligibility {
        return render_error(
            command.operation().into(),
            None,
            error,
            summary,
            output,
            false,
        );
    }
    let mut diagnostic_log = None;
    let mut diagnostic_guard = None;
    let mut log_creation_error = None;
    if let BatchCommand::Run { batch_id, .. } = command {
        match SafeBatchRuntime::open(store.root(), batch_id)
            .and_then(|runtime| runtime.create_run_log())
        {
            Ok(run_log) => {
                let log_path = run_log.path.display().to_string();
                let run_id = run_log.run_id;
                let mode = match output {
                    OutputFormat::Human => DiagnosticOutputMode::Human,
                    OutputFormat::Json => DiagnosticOutputMode::Json,
                };
                diagnostic_guard = Some((run_id, RunLogGuard::new(run_log.file, mode)));
                diagnostic_log = Some(log_path);
            }
            Err(error) => log_creation_error = Some(error),
        }
    }
    let initial_revision = if matches!(command, BatchCommand::Run { .. }) {
        command
            .id()
            .and_then(|batch_id| saved_batch(store, batch_id).ok())
            .map(|batch| batch.revision)
    } else {
        None
    };
    if let Some(error) = log_creation_error {
        return render_batch_error(
            store,
            summary,
            output,
            BatchRunFailure {
                batch_id: command.id().expect("команда запуска содержит batch_id"),
                operation: command.operation(),
                error,
                initial_revision,
                diagnostic_log: None,
            },
        );
    }
    let execution = if let Some((run_id, guard)) = diagnostic_guard {
        let batch_id = command.id().expect("команда запуска содержит batch_id");
        let path = diagnostic_log
            .as_deref()
            .expect("runtime вернул путь к диагностическому файлу");
        if matches!(output, OutputFormat::Human)
            && let Err(error) = io::stderr()
                .lock()
                .write_all(format!("Диагностический журнал: {path}\n").as_bytes())
        {
            let original = AssetError::io("вывод пути диагностического файла", error);
            let error = match guard.finish() {
                Ok(()) => original,
                Err(logging_error) => diagnostic_write_error(Some(original), logging_error),
            };
            return render_batch_error(
                store,
                summary,
                output,
                BatchRunFailure {
                    batch_id,
                    operation: command.operation(),
                    error,
                    initial_revision,
                    diagnostic_log,
                },
            );
        }
        let result = guard.with_default(|| {
            let span = tracing::info_span!(
                "kanji_batch_run",
                operation = command.operation(),
                batch_id,
                run_id,
                diagnostic_log = path,
            );
            let _entered = span.enter();
            tracing::info!(
                event = "run_started",
                stage = "run",
                code = "batch_run_started",
                batch_id,
                run_id,
                "Начата пакетная обработка"
            );
            let result = execute_command_with_progress(
                store,
                summary.clone(),
                command,
                allow_insecure_tls,
                progress,
            );
            match &result {
                Ok((response, _)) => tracing::info!(
                    event = "run_finished",
                    stage = "run",
                    code = response.outcome,
                    batch_id,
                    run_id,
                    changed = response.changed,
                    "Пакетная обработка завершена"
                ),
                Err(error) => tracing::error!(
                    event = "run_stopped",
                    stage = "run",
                    code = error.code.as_str(),
                    message = %safe_message(&error.message),
                    batch_id,
                    run_id,
                    "Пакетная обработка остановлена"
                ),
            }
            result
        });
        let flush = guard.finish();
        match (result, flush) {
            (result, Ok(())) => result,
            (result, Err(logging_error)) => {
                Err(diagnostic_write_error(result.err(), logging_error))
            }
        }
    } else {
        execute_command_with_progress(
            store,
            summary.clone(),
            command,
            allow_insecure_tls,
            progress,
        )
    };
    let execution = execution.and_then(|(mut response, mut exit_code)| {
        if interactive_requested(command) {
            finish_interactive_review(store, &mut response, interaction)?;
            if !response.issues.is_empty() {
                exit_code = 3;
            }
            if matches!(command, BatchCommand::Run { .. }) {
                exit_code = if response.batch.is_resolved() { 0 } else { 3 };
            }
        }
        Ok((response, exit_code))
    });
    match execution {
        Ok((response, exit_code)) => {
            render_batch_response(response, output, exit_code, diagnostic_log)
        }
        Err(error) if matches!(command, BatchCommand::Run { .. }) => render_batch_error(
            store,
            summary,
            output,
            BatchRunFailure {
                batch_id: command.id().expect("команда запуска содержит batch_id"),
                operation: command.operation(),
                error,
                initial_revision,
                diagnostic_log,
            },
        ),
        Err(error) => super::render_error(
            command.operation().into(),
            None,
            error,
            summary,
            output,
            store.did_mutate_on_open(),
        ),
    }
}

fn render_batch_response(
    mut response: BatchResponse,
    output: OutputFormat,
    exit_code: u8,
    diagnostic_log: Option<String>,
) -> CliOutput {
    match output {
        OutputFormat::Json => {
            response.diagnostic_log = diagnostic_log;
            CliOutput {
                stdout: format!(
                    "{}\n",
                    serde_json::to_string_pretty(&response)
                        .expect("ответ пакета содержит конечные сериализуемые значения")
                ),
                stderr: String::new(),
                exit_code,
            }
        }
        OutputFormat::Human => {
            let mut text = format!(
                "Операция: {}; результат: {}; пакет: {}\n",
                localized_operation(&response.operation),
                localized_outcome(response.outcome),
                response.batch_id
            );
            for item in &response.items {
                text.push_str(&format!(
                    "{}  {}  {}  попыток_получения={} разных_кандидатов_SHA-256={}\n",
                    item.identity,
                    localized_item_outcome(item.item_outcome),
                    item.candidate_sha256.as_deref().unwrap_or("-"),
                    item.acquisition_attempts,
                    item.distinct_valid_hashes
                ));
            }
            if let Some(id) = &response.review_id {
                text.push_str(&format!("Идентификатор снимка проверки: {id}\n"));
            }
            if let Some(applied) = &response.review_decisions {
                text.push_str(&format!(
                    "Решения: подтверждено={}, отклонено={}, пропущено={}, уже применено={}\n",
                    applied.confirmed, applied.rejected, applied.skipped, applied.already_applied
                ));
            }
            if let Some(path) = response.review_artifact {
                text.push_str(&format!("Файл проверки: {path}\n"));
            }
            if let Some(path) = diagnostic_log {
                text.push_str(&format!("Диагностический журнал: {path}\n"));
            }
            for issue in &response.issues {
                text.push_str(&format!(
                    "{}: {}: {}\n",
                    issue.identity, issue.code, issue.message
                ));
            }
            CliOutput {
                stdout: text,
                stderr: String::new(),
                exit_code,
            }
        }
    }
}

fn saved_batch(store: &AssetStore, batch_id: &str) -> Result<KanjiBatch, AssetError> {
    let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
    load_required(&mut runtime)
}

struct BatchRunFailure<'a> {
    batch_id: &'a str,
    operation: &'a str,
    error: AssetError,
    initial_revision: Option<u64>,
    diagnostic_log: Option<String>,
}

fn render_batch_error(
    store: &AssetStore,
    summary: StoreSummary,
    output: OutputFormat,
    failure: BatchRunFailure<'_>,
) -> CliOutput {
    let BatchRunFailure {
        batch_id,
        operation,
        error,
        initial_revision,
        diagnostic_log,
    } = failure;
    let saved = saved_batch(store, batch_id).ok();
    let changed = store.did_mutate_on_open()
        || saved.as_ref().map(|batch| batch.revision) != initial_revision;
    tracing::error!(stage = "run", code = error.code.as_str(), message = %safe_message(&error.message),
        batch_id, changed, saved_revision = ?saved.as_ref().map(|batch| batch.revision),
        "Обработка пакета завершилась ошибкой");
    if matches!(output, OutputFormat::Human) {
        let mut rendered =
            super::render_error(operation.into(), None, error, summary, output, changed);
        if let Some(batch) = saved {
            let attempts: usize = batch.items.iter().map(|item| item.attempts.len()).sum();
            rendered.stderr.push_str(&format!(
                "Пакет: {batch_id}; сохранено попыток: {attempts}; ревизия: {}\n",
                batch.revision
            ));
        }
        if let Some(path) = diagnostic_log {
            rendered
                .stderr
                .push_str(&format!("Диагностический журнал: {path}\n"));
        }
        return rendered;
    }
    let exit_code = error.exit_code();
    let mut response = if let Some(batch) = saved {
        serde_json::to_value(batch_response(
            operation,
            summary,
            batch,
            changed,
            Vec::new(),
            None,
            "failed",
        ))
        .expect("ответ пакета содержит сериализуемые значения")
    } else {
        serde_json::json!({
            "schema_version": 1, "operation": operation, "store": summary,
            "batch_id": batch_id, "outcome": "failed", "changed": changed,
        })
    };
    response["error"] = serde_json::json!({
        "code": error.code.as_str(),
        "category": if error.code.exit_code() == 3 || error.code.exit_code() == 4 { "domain_blocker" } else { "io_failure" },
        "message": error.message, "details": error.details,
    });
    if let Some(path) = diagnostic_log {
        response["diagnostic_log"] = serde_json::Value::String(path);
    }
    CliOutput {
        stdout: format!(
            "{}\n",
            serde_json::to_string_pretty(&response).expect("ответ ошибки сериализуем")
        ),
        stderr: String::new(),
        exit_code,
    }
}

fn diagnostic_write_error(original: Option<AssetError>, logging_error: String) -> AssetError {
    let (message, cause) = match original {
        Some(error) => (
            format!(
                "{}; дополнительно не удалось записать диагностический журнал: {logging_error}",
                error.message
            ),
            serde_json::json!({
                "code": error.code.as_str(),
                "message": safe_message(&error.message),
                "details": error.details,
            }),
        ),
        None => (
            format!("не удалось записать диагностический журнал: {logging_error}"),
            serde_json::Value::Null,
        ),
    };
    AssetError::with_details(
        ErrorCode::IoFailure,
        message,
        serde_json::json!({
            "diagnostic_log_error": logging_error,
            "original_error": cause,
        }),
    )
}

fn localized_operation(operation: &str) -> &str {
    match operation {
        "batch_start" => "создание пакета",
        "batch_run" => "обработка пакета",
        "batch_status" => "просмотр состояния пакета",
        "batch_review" => "создание страницы проверки",
        "batch_review_export" => "экспорт файла решений",
        "batch_review_apply" => "применение файла решений",
        "batch_review_interactive" => "интерактивная проверка",
        "batch_revalidate" => "повторная проверка сохранённых байтов",
        "batch_ocr" => "OCR сохранённых кандидатов",
        "batch_review_confirm_matches" => "подтверждение просмотренных совпадений OCR",
        "batch_decide" => "решение человека",
        "batch_retry" => "повторное получение",
        other => other,
    }
}

fn localized_outcome(outcome: &str) -> &str {
    match outcome {
        "started" => "создан",
        "already_started" => "уже существовал",
        "resolved" => "разрешён",
        "partial_progress" => "есть частичный прогресс",
        "awaiting_human" => "ожидает решения человека",
        "decision_applied" => "решение применено",
        "publication_blocked" => "публикация заблокирована",
        "reacquire_scheduled" => "повторное получение запланировано",
        "ok" => "успешно",
        "revalidated" => "повторная проверка завершена",
        "ocr_suggestions_saved" => "предложения OCR сохранены",
        other => other,
    }
}

fn localized_item_outcome(outcome: &str) -> &str {
    match outcome {
        "effective_verified" => "подтверждён владельцем",
        "pending_publication" => "ожидает публикации",
        "awaiting_human" => "ожидает решения человека",
        "reacquire_scheduled" => "ожидает повторного получения",
        "rejected" => "отклонён человеком; повтор только явно",
        "unresolved" => "не разрешён",
        other => other,
    }
}

/// Один раз проверенный полный снимок корпуса. Индексированные записи повторно
/// используются на всей границе; изменения обновляют этот локальный вид по
/// точным результатам владельца.
struct OwnerSnapshot {
    records: BTreeMap<AssetIdentity, AssetRecord>,
    error: Option<AssetError>,
}

impl OwnerSnapshot {
    fn from_result(result: Result<Vec<AssetRecord>, AssetError>) -> Self {
        match result {
            Ok(records) => Self {
                records: records
                    .into_iter()
                    .map(|record| (record.identity.clone(), record))
                    .collect(),
                error: None,
            },
            Err(error) => Self {
                records: BTreeMap::new(),
                error: Some(error),
            },
        }
    }
    fn checked(&self) -> Result<(), AssetError> {
        if let Some(error) = &self.error {
            return Err(copy_error(error));
        }
        Ok(())
    }
    fn current(&self, identity: &AssetIdentity) -> Option<&AssetRecord> {
        self.records.get(identity)
    }
    fn committed(&mut self, record: AssetRecord) {
        self.records.insert(record.identity.clone(), record);
    }
}

trait OwnerSnapshotReader {
    fn capture(&mut self, store: &AssetStore) -> OwnerSnapshot;
}
struct StoreSnapshotReader;
impl OwnerSnapshotReader for StoreSnapshotReader {
    fn capture(&mut self, store: &AssetStore) -> OwnerSnapshot {
        OwnerSnapshot::from_result(store.verify_integrity())
    }
}
fn copy_error(error: &AssetError) -> AssetError {
    AssetError::with_details(error.code, error.message.clone(), error.details.clone())
}

#[cfg(test)]
fn execute_command(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    allow_insecure_tls: bool,
) -> Result<(BatchResponse, u8), AssetError> {
    let mut progress = NullProgressSink;
    execute_command_with_progress(store, summary, command, allow_insecure_tls, &mut progress)
}

fn execute_command_with_progress(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    allow_insecure_tls: bool,
    progress: &mut dyn BatchProgressSink,
) -> Result<(BatchResponse, u8), AssetError> {
    execute_command_with_snapshots_and_progress(
        store,
        summary,
        command,
        allow_insecure_tls,
        &mut StoreSnapshotReader,
        progress,
    )
}

#[cfg(test)]
fn execute_command_with_snapshots(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    allow_insecure_tls: bool,
    reader: &mut impl OwnerSnapshotReader,
) -> Result<(BatchResponse, u8), AssetError> {
    let mut progress = NullProgressSink;
    execute_command_with_snapshots_and_progress(
        store,
        summary,
        command,
        allow_insecure_tls,
        reader,
        &mut progress,
    )
}

fn execute_command_with_snapshots_and_progress(
    store: &AssetStore,
    summary: StoreSummary,
    command: &BatchCommand,
    allow_insecure_tls: bool,
    reader: &mut impl OwnerSnapshotReader,
    progress: &mut dyn BatchProgressSink,
) -> Result<(BatchResponse, u8), AssetError> {
    prevalidate(command)?;
    let operation = command.operation();
    match command {
        BatchCommand::Start {
            batch_id,
            characters,
        } => {
            let identities: Vec<_> = characters
                .iter()
                .map(|character| parse_character(character).map(|character| character.identity()))
                .collect::<Result<_, _>>()?;
            let batch_id = match batch_id {
                Some(id) => id.clone(),
                None => generated_batch_id()?,
            };
            let mut runtime = BatchRuntime::open(store.root(), &batch_id)?;
            if let Some(mut state) = runtime.load()? {
                let requested: BTreeSet<_> = identities.into_iter().collect();
                let existing: BTreeSet<_> = state
                    .items
                    .iter()
                    .map(|item| item.identity.clone())
                    .collect();
                if requested != existing
                    || state.policy.validator != KanjiImageValidator::validator_identity()
                {
                    return Err(invalid(
                        "batch_id уже связан с другим набором символов или валидатором",
                    ));
                }
                let revision = state.revision;
                let mut issues = Vec::new();
                let snapshot = reader.capture(store);
                reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
                if state.revision != revision {
                    runtime.save(&state)?;
                }
                let changed = state.revision != revision;
                return Ok((
                    batch_response(
                        operation,
                        summary,
                        state,
                        changed,
                        issues,
                        None,
                        "already_started",
                    ),
                    0,
                ));
            }
            let mut state = KanjiBatch::new(
                batch_id,
                identities,
                KanjiImageValidator::validator_identity(),
            )?;
            let snapshot = reader.capture(store);
            reuse_existing(&snapshot, &mut state)?;
            runtime.save(&state)?;
            Ok((
                batch_response(operation, summary, state, true, Vec::new(), None, "started"),
                0,
            ))
        }
        BatchCommand::Run {
            batch_id,
            rounds,
            workers,
            interactive_review: _,
            #[cfg(feature = "session-rotation-acceptance")]
            acceptance_rotate_after_items,
        } => {
            let selected_workers = usize::from(*workers);
            #[cfg(feature = "session-rotation-acceptance")]
            let acceptance_rotate_after_items = *acceptance_rotate_after_items;
            let mut acquisition_run = None;
            let result = run_batch_with_worker_stream_and_progress(
                store,
                batch_id,
                *rounds,
                selected_workers,
                |characters, generations, owner_attempts, worker_count, on_event| {
                    if acquisition_run.is_none() {
                        #[cfg(feature = "session-rotation-acceptance")]
                        let new_run = match acceptance_rotate_after_items {
                            Some(limit) => {
                                AcquisitionRun::new_for_acceptance_rotation(limit.get())?
                            }
                            None => AcquisitionRun::new()?,
                        };
                        #[cfg(not(feature = "session-rotation-acceptance"))]
                        let new_run = AcquisitionRun::new()?;
                        acquisition_run = Some(new_run);
                    }
                    let run = acquisition_run.as_mut().expect("среда получения создана");
                    if worker_count == 1 {
                        let mut adapt = |event| on_event(BatchAcquisitionEvent::Sequential(event));
                        run.acquire_with_generations(
                            characters,
                            allow_insecure_tls,
                            generations,
                            &mut adapt,
                        )
                        .map(|_| ())
                    } else {
                        run.acquire_with_generations_parallel(
                            characters,
                            allow_insecure_tls,
                            generations,
                            owner_attempts,
                            worker_count,
                            |event| on_event(BatchAcquisitionEvent::Worker(event)),
                        )
                        .map(|_| ())
                    }
                },
                reader,
                progress,
            );
            // Сессии закрыты после получения элементов; владелец всего временного
            // дерева запуска закрывается при успехе, ошибке потребителя/поставщика и обработанном Ctrl+C.
            let cleanup = acquisition_run.take().map_or(Ok(()), AcquisitionRun::close);
            if let Err(error) = cleanup {
                let message = match error {
                    AcquisitionStreamError::Provider(message) => message,
                    AcquisitionStreamError::Consumer(error) => error.to_string(),
                    AcquisitionStreamError::Interrupted => {
                        "acquisition_interrupted: получен Ctrl+C".into()
                    }
                    AcquisitionStreamError::WorkersExhausted { pending_indices } => {
                        format!(
                            "browser_workers_exhausted: незавершённые индексы {pending_indices:?}"
                        )
                    }
                };
                tracing::error!(stage = "temp_cleanup", code = "temp_workspace_cleanup_failed", message = %safe_message(&message), "Не удалось закрыть временное дерево получения кандзи");
                return Err(AssetError::with_details(
                    ErrorCode::IoFailure,
                    format!("temp_workspace_cleanup_failed: {message}"),
                    serde_json::json!({
                        "cleanup_failure": message,
                        "original_error": result.err().map(|original| serde_json::json!({
                            "code": original.code.as_str(),
                            "message": original.message,
                            "details": original.details,
                        })),
                    }),
                ));
            }
            let (state, changed, issues) = result?;
            let resolved = state.is_resolved();
            let outcome = if resolved {
                "resolved"
            } else if state.review_queue().is_empty() {
                "partial_progress"
            } else {
                "awaiting_human"
            };
            let artifact = if state.review_queue().is_empty() {
                None
            } else {
                let runtime = BatchRuntime::open(store.root(), batch_id)?;
                Some(runtime.write_review(&state)?.display().to_string())
            };
            Ok((
                batch_response(
                    operation, summary, state, changed, issues, artifact, outcome,
                ),
                if resolved { 0 } else { 3 },
            ))
        }
        BatchCommand::Status { batch_id }
        | BatchCommand::Review { batch_id }
        | BatchCommand::ReviewInteractive { batch_id } => {
            let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
            let mut state = load_required(&mut runtime)?;
            let revision = state.revision;
            let mut issues = Vec::new();
            let snapshot = reader.capture(store);
            reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
            if state.revision != revision {
                runtime.save(&state)?;
            }
            let changed = state.revision != revision;
            let artifact = if matches!(command, BatchCommand::Review { .. }) {
                Some(runtime.write_review(&state)?.display().to_string())
            } else {
                None
            };
            Ok((
                batch_response(operation, summary, state, changed, issues, artifact, "ok"),
                0,
            ))
        }
        BatchCommand::Revalidate { batch_id } => {
            let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
            let mut state = load_required(&mut runtime)?;
            let validator = KanjiImageValidator::new();
            let mut cache = BTreeMap::<(AssetIdentity, String), CandidateRevalidation>::new();
            state.revalidate_candidates(validator.identity(), |identity, candidate| {
                let key = (identity.clone(), candidate.sha256.clone());
                if let Some(result) = cache.get(&key) {
                    return Ok(result.clone());
                }
                let bytes = runtime.read_candidate(candidate)?;
                let asset =
                    candidate_asset_record(identity, &bytes, candidate.acquisition.as_deref())?;
                let decision = validator
                    .validate(&asset, &mut Cursor::new(&bytes))
                    .map_err(|failure| {
                        AssetError::new(ErrorCode::ValidatorFailure, failure.message)
                    })?;
                let record = ValidationRecord {
                    validator: validator.identity(),
                    content_sha256: candidate.sha256.clone(),
                    status: decision.status,
                    evidence: decision.evidence,
                };
                let result = CandidateRevalidation {
                    automated: record,
                    technically_valid: validate_image_decode(&bytes).is_ok(),
                };
                cache.insert(key, result.clone());
                Ok(result)
            })?;
            let mut issues = Vec::new();
            let owner = reader.capture(store);
            reconcile_owner_trust(&owner, &mut state, &mut issues)?;
            runtime.save(&state)?;
            let code = if issues.is_empty() { 0 } else { 3 };
            Ok((
                batch_response(operation, summary, state, true, issues, None, "revalidated"),
                code,
            ))
        }
        BatchCommand::ReviewExport { batch_id } => {
            let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
            let state = load_required(&mut runtime)?;
            check_current_validator(&state)?;
            let bundle = crate::kanji_review::prepare(&runtime, &state)?;
            let mut response = batch_response(
                operation,
                summary,
                state,
                false,
                Vec::new(),
                Some(bundle.decisions.display().to_string()),
                "ok",
            );
            response.review_id = Some(bundle.id);
            Ok((response, 0))
        }
        BatchCommand::Ocr { batch_id, options } => {
            let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
            let mut state = load_required(&mut runtime)?;
            check_current_validator(&state)?;
            let initial_revision = state.revision;
            let owner = reader.capture(store);
            let mut issues = Vec::new();
            reconcile_owner_trust(&owner, &mut state, &mut issues)?;
            reuse_existing(&owner, &mut state)?;
            if state.revision != initial_revision {
                runtime.save(&state)?;
            }
            let mut report = crate::kanji_ocr::scan(&mut runtime, &mut state, options)?;
            issues.extend(std::mem::take(&mut report.failures).into_iter().map(
                |(identity, message)| BatchIssue {
                    identity,
                    code: "ocr_provider_failure".into(),
                    message,
                },
            ));
            let artifact = runtime.write_review(&state)?.display().to_string();
            let code = if issues.is_empty() { 0 } else { 3 };
            let changed = state.revision != initial_revision || report.saved > 0;
            let mut response = batch_response(
                operation,
                summary,
                state,
                changed,
                issues,
                Some(artifact),
                "ocr_suggestions_saved",
            );
            response.ocr_summary = Some(report);
            Ok((response, code))
        }
        BatchCommand::ReviewConfirmMatches {
            batch_id,
            review_id,
            confirm_reviewed,
        } => {
            if !confirm_reviewed {
                return Err(invalid(
                    "нужно явное --confirm-reviewed после просмотра группы",
                ));
            }
            let (state, issues, applied) =
                apply_review_with_matches(store, batch_id, review_id, true)?;
            let mut response = batch_response(
                operation,
                summary,
                state,
                applied.changed,
                issues,
                None,
                "ok",
            );
            response.review_id = Some(review_id.clone());
            response.review_decisions = Some(applied);
            let code = if response.issues.is_empty() { 0 } else { 3 };
            Ok((response, code))
        }
        BatchCommand::ReviewApply {
            batch_id,
            review_id,
        } => {
            let (state, issues, applied) = apply_review(store, batch_id, review_id)?;
            let changed = applied.changed;
            let mut response =
                batch_response(operation, summary, state, changed, issues, None, "ok");
            response.review_id = Some(review_id.clone());
            response.review_decisions = Some(applied);
            let code = if response.issues.is_empty() { 0 } else { 3 };
            Ok((response, code))
        }
        BatchCommand::Decide {
            batch_id,
            character,
            sha256,
            action,
            reason,
        } => {
            let decision = HumanBatchDecision {
                identity: parse_character(character)?.identity(),
                candidate_sha256: sha256.clone(),
                action: (*action).into(),
                reason: reason.clone(),
            };
            let (state, issues) = decide_exact(store, batch_id, decision)?;
            let code = if issues.is_empty() { 0 } else { 3 };
            Ok((
                batch_response(
                    operation,
                    summary,
                    state,
                    true,
                    issues,
                    None,
                    if code == 0 {
                        "decision_applied"
                    } else {
                        "publication_blocked"
                    },
                ),
                code,
            ))
        }
        BatchCommand::Retry {
            batch_id,
            character,
            sha256,
            reason,
        } => {
            let identity = parse_character(character)?.identity();
            let (state, issues) = if let Some(sha256) = sha256 {
                decide_exact(
                    store,
                    batch_id,
                    HumanBatchDecision {
                        identity,
                        candidate_sha256: sha256.clone(),
                        action: HumanBatchAction::Reacquire,
                        reason: reason.clone(),
                    },
                )?
            } else {
                let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
                let mut state = load_required(&mut runtime)?;
                check_current_validator(&state)?;
                state.retry_acquisition(&identity, reason.clone())?;
                runtime.save(&state)?;
                (state, Vec::new())
            };
            let code = if issues.is_empty() { 0 } else { 3 };
            Ok((
                batch_response(
                    operation,
                    summary,
                    state,
                    true,
                    issues,
                    None,
                    "reacquire_scheduled",
                ),
                code,
            ))
        }
    }
}

fn load_required(runtime: &mut BatchRuntime) -> Result<KanjiBatch, AssetError> {
    runtime.load()?.ok_or_else(|| {
        AssetError::new(
            ErrorCode::MissingAssetFile,
            "пакет отсутствует; сначала выполните команду batch start",
        )
    })
}

#[cfg(test)]
fn run_batch<F>(
    store: &AssetStore,
    batch_id: &str,
    rounds: u32,
    acquire: F,
) -> Result<(KanjiBatch, bool, Vec<BatchIssue>), AssetError>
where
    F: FnMut(&[String]) -> Result<Vec<Result<AcquiredMedia, String>>, String>,
{
    run_batch_with_snapshots(store, batch_id, rounds, acquire, &mut StoreSnapshotReader)
}

#[cfg(test)]
fn run_batch_with_snapshots<F>(
    store: &AssetStore,
    batch_id: &str,
    rounds: u32,
    mut acquire: F,
    reader: &mut impl OwnerSnapshotReader,
) -> Result<(KanjiBatch, bool, Vec<BatchIssue>), AssetError>
where
    F: FnMut(&[String]) -> Result<Vec<Result<AcquiredMedia, String>>, String>,
{
    let mut progress = NullProgressSink;
    run_batch_with_stream_and_progress(
        store,
        batch_id,
        rounds,
        |characters, _generations, on_event| {
            let outcomes = acquire(characters).map_err(AcquisitionStreamError::Provider)?;
            for (index, outcome) in outcomes.into_iter().enumerate() {
                on_event(AcquisitionEvent::ItemStarted { index })
                    .map_err(AcquisitionStreamError::Consumer)?;
                on_event(AcquisitionEvent::ItemCompleted {
                    index,
                    outcome: Box::new(outcome),
                    diagnostics: None,
                })
                .map_err(AcquisitionStreamError::Consumer)?;
            }
            Ok(())
        },
        reader,
        &mut progress,
    )
}

enum BatchAcquisitionEvent {
    Sequential(AcquisitionEvent),
    Worker(AcquisitionWorkerEvent),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FrontierItemState {
    Pending,
    Active { worker_id: usize },
    Terminal,
}

fn validate_worker_token(
    event_index: Option<usize>,
    event_generation: Option<u32>,
    event_owner_attempt: Option<usize>,
    frontier: &[AssetIdentity],
    generations: &[(u32, usize)],
) -> Result<usize, AssetError> {
    let index =
        event_index.ok_or_else(|| invalid("событие обработчика не содержит индекс назначения"))?;
    let Some((generation, previous_attempts)) = generations.get(index) else {
        return Err(invalid(
            "обработчик сообщил индекс вне текущей границы обхода",
        ));
    };
    if event_generation != Some(*generation) || event_owner_attempt != Some(previous_attempts + 1) {
        return Err(invalid(format!(
            "обработчик сообщил устаревшее сочетание generation/attempt для {}",
            frontier[index].key
        )));
    }
    Ok(index)
}

fn is_acquisition_session_fault(reason: &crate::yarxi::SessionStopReason) -> bool {
    matches!(
        reason,
        crate::yarxi::SessionStopReason::RuntimeFailure(_)
            | crate::yarxi::SessionStopReason::RetryRecoveryFailed(_)
            | crate::yarxi::SessionStopReason::ItemTimeout
    )
}

#[allow(clippy::too_many_arguments)]
fn checkpoint_batch_acquisition_item(
    runtime: &mut BatchRuntime,
    state: &mut KanjiBatch,
    identity: &AssetIdentity,
    generation_attempts: (u32, usize),
    outcome: Result<AcquiredMedia, String>,
    diagnostics: Option<crate::yarxi::AcquisitionItemDiagnostics>,
    issues: &mut Vec<BatchIssue>,
    progress: &mut BatchProgressReporter<'_>,
    worker_id: Option<usize>,
    session: Option<u32>,
    browser_session_id: Option<u64>,
    attempt: Option<u8>,
) -> Result<(), AssetError> {
    if let Some(worker_id) = worker_id {
        progress.update_worker_context(identity, session, worker_id, browser_session_id);
    }
    let stage_timings = diagnostics
        .as_ref()
        .map(crate::yarxi::AcquisitionItemDiagnostics::stage_timings);
    let owner_attempt = generation_attempts.1 + 1;
    let (latest, saved, outcome_code) = checkpoint_acquisition_outcome(
        runtime,
        identity,
        generation_attempts,
        outcome,
        diagnostics,
        issues,
    )?;
    *state = latest;
    let event_result = if saved {
        progress.checkpointed(identity, state.revision);
        progress.emit_with_context(
            "item_checkpointed",
            Some(identity),
            session,
            attempt,
            outcome_code,
            None,
            worker_id,
            browser_session_id,
            Some(generation_attempts.0),
            Some(owner_attempt),
            None,
            stage_timings,
        )
    } else {
        progress.emit_with_context(
            "item_discarded_stale",
            Some(identity),
            session,
            attempt,
            None,
            Some("состояние символа изменилось во время получения".into()),
            worker_id,
            browser_session_id,
            Some(generation_attempts.0),
            Some(owner_attempt),
            None,
            stage_timings,
        )
    };
    progress.finish_item(identity);
    event_result
}

#[cfg(test)]
fn run_batch_with_stream_and_progress<F>(
    store: &AssetStore,
    batch_id: &str,
    rounds: u32,
    mut acquire: F,
    reader: &mut impl OwnerSnapshotReader,
    progress_sink: &mut dyn BatchProgressSink,
) -> Result<(KanjiBatch, bool, Vec<BatchIssue>), AssetError>
where
    F: FnMut(
        &[String],
        &[u32],
        &mut dyn FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    ) -> Result<(), AcquisitionStreamError>,
{
    run_batch_with_worker_stream_and_progress(
        store,
        batch_id,
        rounds,
        1,
        |characters, generations, _owner_attempts, _workers, on_event| {
            let mut adapt = |event| on_event(BatchAcquisitionEvent::Sequential(event));
            acquire(characters, generations, &mut adapt)
        },
        reader,
        progress_sink,
    )
}

fn run_batch_with_worker_stream_and_progress<F>(
    store: &AssetStore,
    batch_id: &str,
    rounds: u32,
    workers: usize,
    mut acquire: F,
    reader: &mut impl OwnerSnapshotReader,
    progress_sink: &mut dyn BatchProgressSink,
) -> Result<(KanjiBatch, bool, Vec<BatchIssue>), AssetError>
where
    F: FnMut(
        &[String],
        &[u32],
        &[usize],
        usize,
        &mut dyn FnMut(BatchAcquisitionEvent) -> Result<(), AssetError>,
    ) -> Result<(), AcquisitionStreamError>,
{
    let _run_span = tracing::info_span!(
        "kanji_batch_run",
        domain = "kanji",
        source = "yarxi",
        operation = "batch_run",
        batch_id,
        rounds
    )
    .entered();
    if !(1..=MAX_ACQUISITION_ROUNDS).contains(&rounds) {
        return Err(invalid(format!(
            "число раундов должно быть от 1 до {MAX_ACQUISITION_ROUNDS}"
        )));
    }
    if !(1..=4).contains(&workers) {
        return Err(invalid("число обработчиков должно быть от 1 до 4"));
    }
    let mut issues = Vec::new();
    let (initial_revision, batch_total) = {
        let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
        let mut state = load_required(&mut runtime)?;
        check_current_validator(&state)?;
        let initial_revision = state.revision;
        let batch_total = state.items.len();
        let mut snapshot = reader.capture(store);
        reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
        runtime.save(&state)?;
        snapshot.checked()?;
        // Сначала возобновляются сохранённые намерения; точные результаты
        // изменений у владельца обновляют снимок без повторного обхода соседних файлов.
        publish_pending(store, &mut runtime, &mut state, &mut issues, &mut snapshot)?;
        reuse_existing(&snapshot, &mut state)?;
        runtime.save(&state)?;
        (initial_revision, batch_total)
    };
    let run_started = Instant::now();
    let mut progress = BatchProgressReporter {
        sink: progress_sink,
        batch_id: batch_id.to_owned(),
        started: run_started,
        round: None,
        round_limit: rounds,
        round_completed: 0,
        round_total: 0,
        run_completed: 0,
        batch_total,
        workers: workers as u8,
        in_flight: BTreeSet::new(),
        item_contexts: BTreeMap::new(),
        last_identity: None,
        browser_session: None,
        acquisition_attempt: None,
        generation: None,
        owner_attempt: None,
        item_pending_checkpoint: false,
        last_checkpointed_identity: None,
        last_checkpoint_revision: None,
        run_stop_reason: None,
    };
    progress.emit("run_started", None, None, None, None, None)?;

    for round in 1..=rounds {
        let _round_span = tracing::info_span!("kanji_batch_round", round).entered();
        let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
        let mut state = load_required(&mut runtime)?;
        let frontier = state.next_round();
        if frontier.is_empty() {
            break;
        }
        let generations: Vec<_> = frontier
            .iter()
            .map(|identity| {
                let item = state
                    .items
                    .iter()
                    .find(|item| &item.identity == identity)
                    .expect("элемент границы обхода присутствует в состоянии");
                (item.generation, item.attempts.len())
            })
            .collect();
        let owner_generations: Vec<_> = generations
            .iter()
            .map(|(generation, _)| *generation)
            .collect();
        let owner_attempts: Vec<_> = generations
            .iter()
            .map(|(_, previous_attempts)| previous_attempts + 1)
            .collect();
        let characters: Vec<_> = frontier
            .iter()
            .map(|identity| identity.key.clone())
            .collect();
        let mut frontier_state = vec![FrontierItemState::Pending; frontier.len()];
        let mut active_workers = BTreeMap::<usize, usize>::new();
        progress.begin_round(round, frontier.len());
        progress.emit("round_started", None, None, None, None, None)?;
        // Кандидаты уже проверены загрузкой. Объект сохраняет кэш проверок,
        // но исключительная блокировка не удерживается во время ожидания сети и браузера.
        runtime.release_lock()?;
        tracing::debug!(
            stage = "lock_released",
            "Блокировка пакета отпущена перед получением"
        );
        let acquisition = {
            let mut on_acquisition_event = |event: BatchAcquisitionEvent| match event {
                BatchAcquisitionEvent::Sequential(event) => match event {
                    AcquisitionEvent::SessionStarted { session } => progress.emit(
                        "browser_session_started",
                        None,
                        Some(session),
                        None,
                        None,
                        None,
                    ),
                    AcquisitionEvent::SessionEnded {
                        session,
                        processed,
                        stop_reason,
                    } => progress.emit(
                        "browser_session_ended",
                        None,
                        Some(session),
                        None,
                        Some(format!("{processed} обработано")),
                        stop_reason.map(|reason| reason.summary()),
                    ),
                    AcquisitionEvent::SessionRotated {
                        next_session,
                        reason,
                    } => {
                        let summary = reason.summary();
                        if is_acquisition_session_fault(&reason) {
                            progress.emit(
                                "browser_session_recovery_started",
                                None,
                                Some(next_session),
                                None,
                                None,
                                Some(summary.clone()),
                            )?;
                        }
                        progress.emit(
                            "browser_session_rotated",
                            None,
                            Some(next_session),
                            None,
                            None,
                            Some(summary),
                        )
                    }
                    AcquisitionEvent::ItemStarted { index } => {
                        let identity = frontier.get(index).ok_or_else(|| {
                            invalid("поставщик сообщил индекс символа вне очереди раунда")
                        })?;
                        if frontier_state[index] != FrontierItemState::Pending
                            || active_workers.contains_key(&0)
                        {
                            return Err(invalid(
                                "поставщик повторно назначил или повторно запустил элемент границы обхода",
                            ));
                        }
                        frontier_state[index] = FrontierItemState::Active { worker_id: 0 };
                        active_workers.insert(0, index);
                        progress.begin_item(
                            identity,
                            generations[index].0,
                            generations[index].1,
                            None,
                            None,
                            None,
                        );
                        progress.emit("item_started", Some(identity), None, None, None, None)
                    }
                    AcquisitionEvent::RetryStarted { index, attempt } => {
                        let identity = frontier.get(index).ok_or_else(|| {
                            invalid("поставщик сообщил повтор вне очереди раунда")
                        })?;
                        if frontier_state[index] != (FrontierItemState::Active { worker_id: 0 }) {
                            return Err(invalid("повтор получен без активного назначения"));
                        }
                        progress.emit(
                            "retry_started",
                            Some(identity),
                            None,
                            Some(attempt),
                            None,
                            None,
                        )
                    }
                    AcquisitionEvent::RetryRecoveryStarted { index, attempt } => {
                        let identity = frontier.get(index).ok_or_else(|| {
                            invalid("поставщик сообщил восстановление вне очереди раунда")
                        })?;
                        if frontier_state[index] != (FrontierItemState::Active { worker_id: 0 }) {
                            return Err(invalid(
                                "восстановление получено без активного назначения",
                            ));
                        }
                        progress.emit(
                            "retry_recovery_started",
                            Some(identity),
                            None,
                            Some(attempt),
                            None,
                            None,
                        )
                    }
                    AcquisitionEvent::Heartbeat { index, attempt } => {
                        let identity = frontier.get(index).ok_or_else(|| {
                            invalid("поставщик сообщил ожидание вне очереди раунда")
                        })?;
                        if frontier_state[index] != (FrontierItemState::Active { worker_id: 0 }) {
                            return Err(invalid("heartbeat получен без активного назначения"));
                        }
                        progress.emit("heartbeat", Some(identity), None, Some(attempt), None, None)
                    }
                    AcquisitionEvent::ItemCompleted {
                        index,
                        outcome,
                        diagnostics,
                    } => {
                        let identity = frontier.get(index).ok_or_else(|| {
                            invalid("поставщик сообщил результат вне очереди раунда")
                        })?;
                        if frontier_state[index] != (FrontierItemState::Active { worker_id: 0 }) {
                            return Err(invalid(
                                "результат получен без единственного активного назначения",
                            ));
                        }
                        checkpoint_batch_acquisition_item(
                            &mut runtime,
                            &mut state,
                            identity,
                            generations[index],
                            *outcome,
                            diagnostics,
                            &mut issues,
                            &mut progress,
                            None,
                            None,
                            None,
                            None,
                        )?;
                        frontier_state[index] = FrontierItemState::Terminal;
                        active_workers.remove(&0);
                        Ok(())
                    }
                },
                BatchAcquisitionEvent::Worker(event) => {
                    if event.worker_id == 0 || event.worker_id > workers {
                        return Err(invalid("worker сообщил неизвестный worker_id"));
                    }
                    match event.event {
                        AcquisitionWorkerEventKind::Assigned => {
                            let index = validate_worker_token(
                                event.index,
                                event.generation,
                                event.owner_attempt,
                                &frontier,
                                &generations,
                            )?;
                            let identity = &frontier[index];
                            if frontier_state[index] != FrontierItemState::Pending
                                || active_workers.contains_key(&event.worker_id)
                            {
                                return Err(invalid(
                                    "worker получил повторное или параллельное назначение",
                                ));
                            }
                            frontier_state[index] = FrontierItemState::Active {
                                worker_id: event.worker_id,
                            };
                            active_workers.insert(event.worker_id, index);
                            progress.begin_item(
                                identity,
                                generations[index].0,
                                generations[index].1,
                                Some(event.worker_id),
                                Some(event.session),
                                event.browser_session,
                            );
                            progress.emit_with_context(
                                "worker_assigned",
                                Some(identity),
                                Some(event.session),
                                Some(event.attempt),
                                None,
                                None,
                                Some(event.worker_id),
                                event.browser_session,
                                event.generation,
                                event.owner_attempt,
                                None,
                                None,
                            )
                        }
                        AcquisitionWorkerEventKind::SessionBootstrapStarted => {
                            let index = validate_worker_token(
                                event.index,
                                event.generation,
                                event.owner_attempt,
                                &frontier,
                                &generations,
                            )?;
                            if frontier_state[index]
                                != (FrontierItemState::Active {
                                    worker_id: event.worker_id,
                                })
                            {
                                return Err(invalid(
                                    "bootstrap не соответствует активному assignment token",
                                ));
                            }
                            let identity = &frontier[index];
                            progress.update_worker_context(
                                identity,
                                Some(event.session),
                                event.worker_id,
                                event.browser_session,
                            );
                            progress.emit_with_context(
                                "browser_session_bootstrap_started",
                                Some(identity),
                                Some(event.session),
                                Some(event.attempt),
                                None,
                                None,
                                Some(event.worker_id),
                                event.browser_session,
                                event.generation,
                                event.owner_attempt,
                                None,
                                None,
                            )
                        }
                        AcquisitionWorkerEventKind::Provider(provider_event) => {
                            let item_index = match &provider_event {
                                AcquisitionEvent::ItemStarted { index }
                                | AcquisitionEvent::RetryStarted { index, .. }
                                | AcquisitionEvent::RetryRecoveryStarted { index, .. }
                                | AcquisitionEvent::Heartbeat { index, .. }
                                | AcquisitionEvent::ItemCompleted { index, .. } => Some(*index),
                                _ => None,
                            };
                            if let Some(index) = item_index {
                                let token_index = validate_worker_token(
                                    event.index,
                                    event.generation,
                                    event.owner_attempt,
                                    &frontier,
                                    &generations,
                                )?;
                                if index != token_index
                                    || frontier_state[index]
                                        != (FrontierItemState::Active {
                                            worker_id: event.worker_id,
                                        })
                                {
                                    return Err(invalid(
                                        "provider report не соответствует активному assignment token",
                                    ));
                                }
                            }
                            match provider_event {
                                AcquisitionEvent::SessionStarted { session } => {
                                    let active_index =
                                        active_workers.get(&event.worker_id).copied();
                                    let identity = active_index.map(|index| &frontier[index]);
                                    if let Some(identity) = identity {
                                        progress.update_worker_context(
                                            identity,
                                            Some(session),
                                            event.worker_id,
                                            event.browser_session,
                                        );
                                    }
                                    progress.emit_with_context(
                                        "browser_session_started",
                                        identity,
                                        Some(session),
                                        Some(event.attempt),
                                        None,
                                        None,
                                        Some(event.worker_id),
                                        event.browser_session,
                                        active_index.map(|index| generations[index].0),
                                        active_index.map(|index| generations[index].1 + 1),
                                        None,
                                        None,
                                    )
                                }
                                AcquisitionEvent::SessionEnded {
                                    session,
                                    processed,
                                    stop_reason,
                                } => progress.emit_with_context(
                                    "browser_session_ended",
                                    None,
                                    Some(session),
                                    Some(event.attempt),
                                    Some(format!("{processed} обработано")),
                                    stop_reason.map(|reason| reason.summary()),
                                    Some(event.worker_id),
                                    event.browser_session,
                                    None,
                                    None,
                                    None,
                                    None,
                                ),
                                AcquisitionEvent::SessionRotated {
                                    next_session,
                                    reason,
                                } => {
                                    let summary = reason.summary();
                                    if is_acquisition_session_fault(&reason) {
                                        progress.emit_with_context(
                                            "browser_session_recovery_started",
                                            None,
                                            Some(next_session),
                                            Some(event.attempt),
                                            None,
                                            Some(summary.clone()),
                                            Some(event.worker_id),
                                            event.browser_session,
                                            None,
                                            None,
                                            None,
                                            None,
                                        )?;
                                    }
                                    progress.emit_with_context(
                                        "browser_session_rotated",
                                        None,
                                        Some(next_session),
                                        Some(event.attempt),
                                        None,
                                        Some(summary),
                                        Some(event.worker_id),
                                        event.browser_session,
                                        None,
                                        None,
                                        None,
                                        None,
                                    )
                                }
                                AcquisitionEvent::ItemStarted { index } => {
                                    let identity = &frontier[index];
                                    progress.update_worker_context(
                                        identity,
                                        Some(event.session),
                                        event.worker_id,
                                        event.browser_session,
                                    );
                                    progress.emit_with_context(
                                        "item_started",
                                        Some(identity),
                                        Some(event.session),
                                        Some(event.attempt),
                                        None,
                                        None,
                                        Some(event.worker_id),
                                        event.browser_session,
                                        event.generation,
                                        event.owner_attempt,
                                        None,
                                        None,
                                    )
                                }
                                AcquisitionEvent::RetryStarted { index, attempt } => {
                                    let identity = &frontier[index];
                                    progress.emit_with_context(
                                        "retry_started",
                                        Some(identity),
                                        Some(event.session),
                                        Some(attempt),
                                        None,
                                        None,
                                        Some(event.worker_id),
                                        event.browser_session,
                                        event.generation,
                                        event.owner_attempt,
                                        None,
                                        None,
                                    )
                                }
                                AcquisitionEvent::RetryRecoveryStarted { index, attempt } => {
                                    let identity = &frontier[index];
                                    progress.emit_with_context(
                                        "retry_recovery_started",
                                        Some(identity),
                                        Some(event.session),
                                        Some(attempt),
                                        None,
                                        None,
                                        Some(event.worker_id),
                                        event.browser_session,
                                        event.generation,
                                        event.owner_attempt,
                                        None,
                                        None,
                                    )
                                }
                                AcquisitionEvent::Heartbeat { index, attempt } => {
                                    let identity = &frontier[index];
                                    progress.emit_with_context(
                                        "heartbeat",
                                        Some(identity),
                                        Some(event.session),
                                        Some(attempt),
                                        None,
                                        None,
                                        Some(event.worker_id),
                                        event.browser_session,
                                        event.generation,
                                        event.owner_attempt,
                                        None,
                                        None,
                                    )
                                }
                                AcquisitionEvent::ItemCompleted {
                                    index,
                                    outcome,
                                    diagnostics,
                                } => {
                                    let identity = &frontier[index];
                                    checkpoint_batch_acquisition_item(
                                        &mut runtime,
                                        &mut state,
                                        identity,
                                        generations[index],
                                        *outcome,
                                        diagnostics,
                                        &mut issues,
                                        &mut progress,
                                        Some(event.worker_id),
                                        Some(event.session),
                                        event.browser_session,
                                        Some(event.attempt),
                                    )?;
                                    frontier_state[index] = FrontierItemState::Terminal;
                                    active_workers.remove(&event.worker_id);
                                    Ok(())
                                }
                            }
                        }
                        AcquisitionWorkerEventKind::Deferred { reason } => {
                            let index = validate_worker_token(
                                event.index,
                                event.generation,
                                event.owner_attempt,
                                &frontier,
                                &generations,
                            )?;
                            let identity = &frontier[index];
                            if frontier_state[index]
                                != (FrontierItemState::Active {
                                    worker_id: event.worker_id,
                                })
                                || active_workers.get(&event.worker_id) != Some(&index)
                            {
                                return Err(invalid(
                                    "worker попытался отложить неактивное назначение",
                                ));
                            }
                            frontier_state[index] = FrontierItemState::Pending;
                            active_workers.remove(&event.worker_id);
                            let result = progress.emit_with_context(
                                "item_deferred",
                                Some(identity),
                                Some(event.session),
                                Some(event.attempt),
                                None,
                                Some(reason),
                                Some(event.worker_id),
                                event.browser_session,
                                event.generation,
                                event.owner_attempt,
                                None,
                                None,
                            );
                            progress.finish_item(identity);
                            result
                        }
                        AcquisitionWorkerEventKind::Requeued { reason } => {
                            let index = validate_worker_token(
                                event.index,
                                event.generation,
                                event.owner_attempt,
                                &frontier,
                                &generations,
                            )?;
                            if frontier_state[index] != FrontierItemState::Pending {
                                return Err(invalid(
                                    "worker сообщил requeue без незавершённого pending элемента",
                                ));
                            }
                            progress.emit_with_context(
                                "item_requeued",
                                Some(&frontier[index]),
                                Some(event.session),
                                Some(event.attempt),
                                None,
                                Some(reason),
                                Some(event.worker_id),
                                event.browser_session,
                                event.generation,
                                event.owner_attempt,
                                None,
                                None,
                            )
                        }
                        AcquisitionWorkerEventKind::WorkerRetired {
                            consecutive_failures,
                            reason,
                        } => {
                            if let Some(index) = active_workers.remove(&event.worker_id)
                                && frontier_state[index]
                                    == (FrontierItemState::Active {
                                        worker_id: event.worker_id,
                                    })
                            {
                                frontier_state[index] = FrontierItemState::Pending;
                                let identity = &frontier[index];
                                let _ = progress.emit_with_context(
                                    "item_deferred",
                                    Some(identity),
                                    Some(event.session),
                                    Some(event.attempt),
                                    None,
                                    Some(reason.clone()),
                                    Some(event.worker_id),
                                    event.browser_session,
                                    Some(generations[index].0),
                                    Some(generations[index].1 + 1),
                                    None,
                                    None,
                                );
                                progress.finish_item(identity);
                            }
                            progress.emit_with_context(
                                "worker_retired",
                                None,
                                Some(event.session),
                                Some(event.attempt),
                                Some(format!("consecutive_failures={consecutive_failures}")),
                                Some(reason),
                                Some(event.worker_id),
                                event.browser_session,
                                None,
                                None,
                                None,
                                None,
                            )
                        }
                        AcquisitionWorkerEventKind::WorkerStopped { reason } => {
                            if let Some(index) = active_workers.remove(&event.worker_id)
                                && frontier_state[index]
                                    == (FrontierItemState::Active {
                                        worker_id: event.worker_id,
                                    })
                            {
                                frontier_state[index] = FrontierItemState::Pending;
                                let identity = &frontier[index];
                                let _ = progress.emit_with_context(
                                    "item_deferred",
                                    Some(identity),
                                    Some(event.session),
                                    Some(event.attempt),
                                    None,
                                    Some(reason.clone()),
                                    Some(event.worker_id),
                                    event.browser_session,
                                    Some(generations[index].0),
                                    Some(generations[index].1 + 1),
                                    None,
                                    None,
                                );
                                progress.finish_item(identity);
                            }
                            progress.emit_with_context(
                                "worker_stopped",
                                None,
                                Some(event.session),
                                Some(event.attempt),
                                None,
                                Some(reason),
                                Some(event.worker_id),
                                event.browser_session,
                                None,
                                None,
                                None,
                                None,
                            )
                        }
                    }
                }
            };
            acquire(
                &characters,
                &owner_generations,
                &owner_attempts,
                workers,
                &mut on_acquisition_event,
            )
        };
        if let Err(error) = acquisition {
            progress.run_stop_reason = Some(match &error {
                AcquisitionStreamError::Provider(_) => "provider_failure",
                AcquisitionStreamError::Consumer(_) => "consumer_failure",
                AcquisitionStreamError::Interrupted => "interrupted",
                AcquisitionStreamError::WorkersExhausted { .. } => "workers_exhausted",
            });
            let reason = match &error {
                AcquisitionStreamError::Provider(message) => message.clone(),
                AcquisitionStreamError::Consumer(error) => error.to_string(),
                AcquisitionStreamError::Interrupted => "получен Ctrl+C".into(),
                AcquisitionStreamError::WorkersExhausted { pending_indices } => {
                    let pending = pending_indices
                        .iter()
                        .filter_map(|index| frontier.get(*index))
                        .map(|identity| identity.key.as_str())
                        .collect::<Vec<_>>();
                    format!("browser_workers_exhausted: pending={pending:?}")
                }
            };
            if matches!(&error, AcquisitionStreamError::WorkersExhausted { .. }) {
                progress.emit(
                    "worker_exhausted",
                    None,
                    None,
                    None,
                    None,
                    Some(reason.clone()),
                )?;
            }
            let last_identity = progress.last_identity.clone();
            let _ = progress.emit(
                "run_stopped",
                last_identity.as_ref(),
                None,
                None,
                None,
                Some(reason.clone()),
            );
            tracing::warn!(stage = "run_stopped", reason = %safe_message(&reason),
                checkpointed_items = progress.run_completed, remaining_without_checkpoint = frontier.len().saturating_sub(progress.round_completed),
                "Получение остановлено; незапущенный хвост сохранён без попыток");
            return Err(match error {
                AcquisitionStreamError::Provider(message) => {
                    AssetError::new(crate::error::ErrorCode::IoFailure, message)
                }
                AcquisitionStreamError::Consumer(error) => error,
                AcquisitionStreamError::Interrupted => AssetError::new(
                    crate::error::ErrorCode::InvalidTransition,
                    "batch_interrupted: получен Ctrl+C; сохранённый прогресс доступен для возобновления",
                ),
                AcquisitionStreamError::WorkersExhausted { pending_indices } => {
                    let pending = pending_indices
                        .iter()
                        .filter_map(|index| frontier.get(*index))
                        .map(|identity| identity.key.as_str())
                        .collect::<Vec<_>>();
                    AssetError::new(
                        crate::error::ErrorCode::InvalidTransition,
                        format!(
                            "browser_workers_exhausted: сохранённый прогресс доступен для возобновления; pending={pending:?}"
                        ),
                    )
                }
            });
        }
        let unfinished = frontier_state
            .iter()
            .enumerate()
            .filter_map(|(index, item)| (*item != FrontierItemState::Terminal).then_some(index))
            .collect::<Vec<_>>();
        if !unfinished.is_empty() {
            progress.run_stop_reason = Some("frontier_protocol_incomplete");
            for index in &unfinished {
                if let FrontierItemState::Active { worker_id } = frontier_state[*index] {
                    let identity = &frontier[*index];
                    let _ = progress.emit_with_context(
                        "item_deferred",
                        Some(identity),
                        None,
                        None,
                        None,
                        Some("поставщик завершился без окончательного результата".into()),
                        (worker_id != 0).then_some(worker_id),
                        None,
                        Some(generations[*index].0),
                        Some(generations[*index].1 + 1),
                        None,
                        None,
                    );
                    progress.finish_item(identity);
                }
            }
            let identities = unfinished
                .iter()
                .map(|index| frontier[*index].key.as_str())
                .collect::<Vec<_>>();
            let reason = format!(
                "batch_frontier_incomplete: поставщик вернул успех до terminal checkpoint: {identities:?}"
            );
            let _ = progress.emit("run_stopped", None, None, None, None, Some(reason.clone()));
            return Err(invalid(reason));
        }
        let mut snapshot = reader.capture(store);
        runtime.reacquire_lock()?;
        tracing::debug!(
            stage = "lock_reacquired",
            "Блокировка пакета получена после сети"
        );
        state = load_required_cached(&mut runtime)?;
        tracing::debug!(
            stage = "state_reloaded",
            revision = state.revision,
            "Сохранённое состояние перечитано"
        );
        reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
        runtime.save(&state)?;
        snapshot.checked()?;
        reuse_existing(&snapshot, &mut state)?;
        runtime.save(&state)?;
        publish_pending(store, &mut runtime, &mut state, &mut issues, &mut snapshot)?;
        runtime.save(&state)?;
        progress.emit("round_finished", None, None, None, None, None)?;
    }
    let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
    let mut state = load_required(&mut runtime)?;
    let snapshot = reader.capture(store);
    reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
    runtime.save(&state)?;
    let changed = state.revision != initial_revision;
    progress.emit(
        "run_finished",
        None,
        None,
        None,
        Some(if state.is_resolved() {
            "resolved".into()
        } else {
            "partial_progress".into()
        }),
        None,
    )?;
    Ok((state, changed, issues))
}

fn load_required_cached(runtime: &mut BatchRuntime) -> Result<KanjiBatch, AssetError> {
    runtime.reload_cached()?.ok_or_else(|| {
        AssetError::new(
            crate::error::ErrorCode::MissingAssetFile,
            "пакет отсутствует; сначала выполните команду batch start",
        )
    })
}

fn checkpoint_acquisition_outcome(
    runtime: &mut BatchRuntime,
    identity: &AssetIdentity,
    expected: (u32, usize),
    outcome: Result<AcquiredMedia, String>,
    diagnostics: Option<crate::yarxi::AcquisitionItemDiagnostics>,
    issues: &mut Vec<BatchIssue>,
) -> Result<(KanjiBatch, bool, Option<String>), AssetError> {
    let timing = diagnostics.as_ref().map(|diagnostics| &diagnostics.timing);
    let checkpoint_timing = timing.map(|item| item.stage("checkpoint"));
    let _checkpoint_span = tracing::info_span!("kanji_checkpoint", identity = %identity.key,
        expected_generation = expected.0, expected_attempts = expected.1)
    .entered();
    tracing::info!(stage = "checkpoint_begin", "Начато сохранение результата");
    let mut candidate_diagnostic_outcome = None;
    let checkpoint = (|| {
        runtime.reacquire_lock()?;
        tracing::debug!(
            stage = "lock_reacquired",
            "Блокировка получена для сохранения результата"
        );
        let mut state = load_required_cached(runtime)?;
        tracing::debug!(
            stage = "state_reloaded",
            revision = state.revision,
            "Состояние перечитано перед CAS"
        );
        let item = state
            .items
            .iter()
            .find(|item| &item.identity == identity)
            .ok_or_else(|| invalid("символ отсутствует в текущем состоянии пакета"))?;
        if (item.generation, item.attempts.len()) != expected
            || !state.next_round().contains(identity)
        {
            tracing::warn!(
                stage = "cas_stale_discard",
                code = "stale_result",
                observed_generation = item.generation,
                observed_attempts = item.attempts.len(),
                "Устаревший результат получения отброшен"
            );
            return Ok((state, false, None));
        }
        let input = match outcome {
            Ok(media) => {
                let validation = timing.map(|item| item.stage("final_validation"));
                let result = validated_candidate(runtime, identity, &media, timing);
                if let Some(validation) = validation {
                    match &result {
                        Ok(BatchAttemptInput::Candidate { candidate }) => {
                            let outcome = candidate_validation_outcome(candidate);
                            match outcome {
                                CandidateDiagnosticOutcome::Success => validation.finish_success(),
                                CandidateDiagnosticOutcome::Uncertain => {
                                    validation.finish_outcome("uncertain")
                                }
                                CandidateDiagnosticOutcome::Failure(code) => {
                                    validation.finish_failure(code, false, None)
                                }
                            }
                        }
                        Ok(BatchAttemptInput::Failed { code, .. }) => {
                            validation.finish_failure(code, false, None)
                        }
                        Err(error) => validation.finish_failure(error.code.as_str(), false, None),
                    }
                }
                result
            }
            Err(message) => {
                tracing::warn!(stage = "acquisition", code = "acquisition_failed", message = %safe_message(&message),
                    "Поставщик вернул ошибку получения");
                Ok(failure("acquisition_failed", &message))
            }
        };
        let input = match input {
            Ok(input) => input,
            Err(error) if error.code.exit_code() != 4 => {
                issues.push(issue(identity, &error));
                failure(error.code.as_str(), &error.message)
            }
            Err(error) => return Err(error),
        };
        let outcome_code = match &input {
            BatchAttemptInput::Candidate { candidate } => {
                candidate_diagnostic_outcome = Some(candidate_validation_outcome(candidate));
                "candidate_recorded".to_owned()
            }
            BatchAttemptInput::Failed { code, .. } => code.clone(),
        };
        state.record_attempt(identity, input)?;
        runtime.save(&state)?;
        tracing::info!(stage = "checkpoint_success", revision = state.revision,
            outcome = %outcome_code,
            "Результат надёжно сохранён");
        Ok((state, true, Some(outcome_code)))
    })();
    let release = runtime.release_lock();
    tracing::debug!(
        stage = "lock_released",
        released = release.is_ok(),
        "Завершено освобождение блокировки после сохранения"
    );
    let result = match checkpoint {
        Ok(value) => release.map(|()| value),
        Err(error) => {
            tracing::error!(stage = "checkpoint_failure", code = error.code.as_str(), message = %safe_message(&error.message),
                "Не удалось сохранить результат");
            let _ = release;
            Err(error)
        }
    };
    if let Some(checkpoint) = checkpoint_timing {
        match &result {
            Ok((_, false, _)) => checkpoint.finish_outcome("discarded_stale"),
            Ok((_, true, Some(code))) if code == "candidate_recorded" => {
                checkpoint.finish_success()
            }
            Ok(_) => checkpoint.finish_outcome("failure_recorded"),
            Err(error) => checkpoint.finish_failure(error.code.as_str(), false, None),
        }
    }
    if let Some(diagnostics) = diagnostics {
        match &result {
            Ok((_, false, _)) => diagnostics.timing.finish_outcome("discarded_stale"),
            Ok((_, true, Some(code))) if code == "candidate_recorded" => {
                match candidate_diagnostic_outcome.unwrap_or(CandidateDiagnosticOutcome::Failure(
                    "checkpoint_outcome_invalid",
                )) {
                    CandidateDiagnosticOutcome::Success => diagnostics.timing.finish_success(),
                    CandidateDiagnosticOutcome::Uncertain => {
                        diagnostics.timing.finish_outcome("uncertain")
                    }
                    CandidateDiagnosticOutcome::Failure(code) => {
                        diagnostics.timing.finish_failure(code, false, None)
                    }
                }
            }
            Ok((_, true, Some(code))) => diagnostics.finish_recorded_failure(code),
            Ok(_) => diagnostics
                .timing
                .finish_failure("checkpoint_outcome_invalid", false, None),
            Err(error) => diagnostics
                .timing
                .finish_failure(error.code.as_str(), false, None),
        }
    }
    result
}

enum CandidateDiagnosticOutcome {
    Success,
    Uncertain,
    Failure(&'static str),
}

fn candidate_validation_outcome(candidate: &BatchCandidate) -> CandidateDiagnosticOutcome {
    if !candidate.technically_valid {
        CandidateDiagnosticOutcome::Failure("media_validation_failed")
    } else {
        match candidate.automated.status {
            SemanticStatus::Verified => CandidateDiagnosticOutcome::Success,
            SemanticStatus::Uncertain => CandidateDiagnosticOutcome::Uncertain,
            SemanticStatus::Rejected => {
                CandidateDiagnosticOutcome::Failure("semantic_validation_rejected")
            }
            SemanticStatus::Corrupt => {
                CandidateDiagnosticOutcome::Failure("media_validation_failed")
            }
        }
    }
}

fn validated_candidate(
    runtime: &BatchRuntime,
    identity: &AssetIdentity,
    media: &AcquiredMedia,
    timing: Option<&crate::browser_diagnostics::BrowserItemTimer>,
) -> Result<BatchAttemptInput, AssetError> {
    // Свидетельства поставщика хранят Unicode статьи как шестнадцатеричную
    // кодовую точку, а идентичность — сам символ. Сравнивается точная кодовая точка
    // запрошенного символа.
    let expected_code = match parse_kanji_character(&identity.key) {
        Ok(character) => format!("{:X}", u32::from(character)),
        Err(message) => return Ok(failure("source_identity_mismatch", &message)),
    };
    if media.character != identity.key
        || !media.article_unicode.eq_ignore_ascii_case(&expected_code)
        || !media
            .evidence
            .article_unicode
            .eq_ignore_ascii_case(&expected_code)
    {
        return Ok(failure(
            "source_identity_mismatch",
            "символ или Unicode статьи Yarxi не совпадает с запрошенной identity",
        ));
    }
    if let Err(message) = validate_selected_format(media.selection, &media.bytes) {
        return Ok(failure("media_format_mismatch", &message));
    }
    let sha256 = sha256_hex(&media.bytes);
    let record = candidate_asset_record(identity, &media.bytes, Some(&media.evidence))?;
    let validator = KanjiImageValidator::new();
    let semantic_validation = timing.map(|item| item.stage("semantic_validation"));
    let decision = validator
        .validate(&record, &mut Cursor::new(media.bytes.as_slice()))
        .map_err(|failure| AssetError::new(ErrorCode::ValidatorFailure, failure.message));
    if let Some(validation) = semantic_validation {
        match &decision {
            Ok(decision) => match decision.status {
                SemanticStatus::Verified => validation.finish_success(),
                SemanticStatus::Uncertain => validation.finish_outcome("uncertain"),
                SemanticStatus::Rejected => {
                    validation.finish_failure("semantic_validation_rejected", false, None)
                }
                SemanticStatus::Corrupt => {
                    validation.finish_failure("media_validation_failed", false, None)
                }
            },
            Err(error) => validation.finish_failure(error.code.as_str(), false, None),
        }
    }
    let decision = decision?;
    let technically_valid = validate_image_decode(&media.bytes).is_ok();
    let automated = ValidationRecord {
        status: decision.status,
        validator: validator.identity(),
        content_sha256: sha256,
        evidence: decision.evidence,
    };
    let mut candidate = runtime.persist_candidate(&media.bytes, automated, technically_valid)?;
    candidate.acquisition = Some(Box::new(media.evidence.clone()));
    Ok(BatchAttemptInput::Candidate { candidate })
}

fn reuse_existing(snapshot: &OwnerSnapshot, state: &mut KanjiBatch) -> Result<(), AssetError> {
    snapshot.checked()?;
    for item in state.items.clone() {
        if item.is_ready() || item.status.semantic_resolved() || !item.human_decisions.is_empty() {
            continue;
        }
        let Some(record) = snapshot.current(&item.identity).filter(|record| {
            record.lifecycle == LifecycleState::Verified
                && record.is_trusted_for(&state.policy.validator)
        }) else {
            continue;
        };
        state.mark_existing_ready(&item.identity, &record.sha256)?;
    }
    Ok(())
}

/// Целостность снимка владелец проверил один раз. Готовность определяется по
/// индексированной точной идентичности, SHA-256 и текущему доверию без полного
/// перечитывания манифеста для каждого элемента.
fn reconcile_owner_trust(
    snapshot: &OwnerSnapshot,
    state: &mut KanjiBatch,
    issues: &mut Vec<BatchIssue>,
) -> Result<(), AssetError> {
    // Ошибка полной проверки не показывает, какая идентичность отсутствует;
    // не сбрасываем доверие всех элементов по частичному/пустому снимку.
    snapshot.checked()?;
    for item in state.items.clone() {
        let current = snapshot.current(&item.identity);
        let owner_rejection = current
            .filter(|record| record.current_human_decision() == Some(HumanDecision::Reject))
            .map(|record| HumanBatchDecision {
                identity: record.identity.clone(),
                candidate_sha256: record.sha256.clone(),
                action: HumanBatchAction::Reject,
                reason: record
                    .human_attestation
                    .as_ref()
                    .expect("текущее решение человека присутствует в состоянии")
                    .reason
                    .chars()
                    .take(900)
                    .collect(),
            });
        let rejected_auto_candidate = item.status == BatchItemStatus::AutoVerified
            && owner_rejection.as_ref().is_some_and(|decision| {
                item.current_sha256.as_deref() == Some(decision.candidate_sha256.as_str())
            });
        if !item.is_ready() && !rejected_auto_candidate {
            continue;
        }
        if current.is_some_and(|record| {
            record.lifecycle == LifecycleState::Verified
                && record.is_trusted_for(&state.policy.validator)
                && item.published_sha256.as_deref() == Some(record.sha256.as_str())
        }) && !rejected_auto_candidate
        {
            continue;
        }
        let error = if let Some(record) = current {
            if record.sha256 != item.published_sha256.as_deref().unwrap_or("") {
                AssetError::new(
                    ErrorCode::IdentityConflict,
                    "сохранённый готовый SHA-256 отличается от точных текущих байтов владельца",
                )
            } else {
                AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "владелец не подтверждает ожидаемое каноническое доверие",
                )
            }
        } else {
            AssetError::new(
                ErrorCode::MissingAssetFile,
                "готовая identity отсутствует в проверенном снимке владельца",
            )
        };
        state.invalidate_owner_trust(
            &item.identity,
            format!("доверие владельца утрачено: {}", error.code.as_str()),
            owner_rejection,
        )?;
        issues.push(issue(&item.identity, &error));
    }
    Ok(())
}

fn publish_pending(
    store: &AssetStore,
    runtime: &mut BatchRuntime,
    state: &mut KanjiBatch,
    issues: &mut Vec<BatchIssue>,
    snapshot: &mut OwnerSnapshot,
) -> Result<(), AssetError> {
    snapshot.checked()?;
    for item in state.items.clone() {
        if let Some(decision) = item.human_decisions.last()
            && matches!(
                decision.action,
                HumanBatchAction::Reject | HumanBatchAction::RejectAndHold
            )
            && !item.observed_owner_rejections.contains(decision)
        {
            let current = snapshot.current(&item.identity);
            let already_rejected = current.is_some_and(|record| {
                record.sha256 == decision.candidate_sha256
                    && record.current_human_decision() == Some(HumanDecision::Reject)
            });
            let may_apply = current.is_none_or(|record| record.sha256 == decision.candidate_sha256);
            if !already_rejected
                && may_apply
                && let Some(candidate) = candidate_by_hash(&item, &decision.candidate_sha256)
            {
                let bytes = runtime.read_candidate(candidate)?;
                if let Err(error) = apply_rejection(
                    store,
                    runtime,
                    &item.identity,
                    candidate,
                    &bytes,
                    &decision.reason,
                    snapshot,
                ) {
                    issues.push(issue(&item.identity, &error));
                }
            }
        }
        if item.is_ready() {
            continue;
        }
        let result = match item.status {
            BatchItemStatus::AutoVerified => {
                publish_automated(store, runtime, state, &item.identity, snapshot)
            }
            BatchItemStatus::HumanVerified => publish_human(store, runtime, &item, snapshot),
            _ => continue,
        };
        match result {
            Ok((sha256, source)) => {
                state.mark_published_ready(&item.identity, &sha256, source)?;
                runtime.save(state)?;
            }
            Err(error) if error.code.exit_code() != 4 => issues.push(issue(&item.identity, &error)),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn publish_automated(
    store: &AssetStore,
    runtime: &BatchRuntime,
    state: &KanjiBatch,
    identity: &AssetIdentity,
    snapshot: &mut OwnerSnapshot,
) -> Result<(String, BatchTrustSource), AssetError> {
    let item = state
        .items
        .iter()
        .find(|item| &item.identity == identity)
        .expect("идентификатор пакета присутствует в загруженном состоянии");
    let candidate = current_candidate(item)?;
    let bytes = runtime.read_candidate(candidate)?;
    let request = verified_request(snapshot, identity, bytes, candidate)?;
    let (outcome, source) = if candidate.automated.status == SemanticStatus::Verified {
        (
            store.ingest_verified(request, &KanjiImageValidator::new())?,
            BatchTrustSource::Automated,
        )
    } else {
        let resolution = state.aggregate_decision(identity)?.ok_or_else(|| {
            invalid("автоматический кандидат не имеет положительного свидетельства агрегации")
        })?;
        (
            store.ingest_verified(request, &AggregateValidator { resolution })?,
            BatchTrustSource::Aggregate,
        )
    };
    let record = outcome
        .asset
        .ok_or_else(|| invalid("владелец не опубликовал кандидата пакета"))?;
    if outcome.status != SemanticStatus::Verified
        || outcome.sha256 != candidate.sha256
        || record.identity != *identity
        || record.sha256 != candidate.sha256
        || record.lifecycle != LifecycleState::Verified
        || !record.is_trusted_for(&state.policy.validator)
    {
        return Err(invalid(
            "точный коммит владельца не подтверждает доверие к кандидату пакета",
        ));
    }
    // Владелец только что проверил, подготовил или зафиксировал эти точные байты
    // с помощью CAS. Итоговый снимок границы учитывает последующие конкурентные
    // изменения; повторное полное чтение для каждого элемента не требуется.
    snapshot.committed(record);
    Ok((candidate.sha256.clone(), source))
}

fn publish_human(
    store: &AssetStore,
    runtime: &BatchRuntime,
    item: &crate::batch::BatchItem,
    snapshot: &mut OwnerSnapshot,
) -> Result<(String, BatchTrustSource), AssetError> {
    let decision = item
        .human_decisions
        .last()
        .ok_or_else(|| invalid("для кандидата человека отсутствует решение"))?;
    let candidate = current_candidate(item)?;
    if decision.action != HumanBatchAction::Confirm || decision.candidate_sha256 != candidate.sha256
    {
        return Err(invalid(
            "намерение человека относится к устаревшему кандидату",
        ));
    }
    let bytes = runtime.read_candidate(candidate)?;
    materialize_candidate(
        store,
        runtime,
        &item.identity,
        candidate,
        &bytes,
        true,
        snapshot,
    )?;
    let outcome = store.attest(HumanAttestationRequest {
        identity: item.identity.clone(),
        expected_sha256: candidate.sha256.clone(),
        decision: HumanDecision::Approve,
        reason: decision.reason.clone(),
    })?;
    if outcome.asset.identity != item.identity
        || outcome.asset.sha256 != candidate.sha256
        || outcome.asset.current_human_decision() != Some(HumanDecision::Approve)
        || outcome.asset.lifecycle != LifecycleState::Verified
        || !outcome
            .asset
            .is_trusted_for(&KanjiImageValidator::validator_identity())
    {
        return Err(invalid(
            "точный коммит владельца после решения человека не подтверждает доверие к кандидату",
        ));
    }
    snapshot.committed(outcome.asset);
    Ok((candidate.sha256.clone(), BatchTrustSource::Human))
}

fn decide_exact(
    store: &AssetStore,
    batch_id: &str,
    decision: HumanBatchDecision,
) -> Result<(KanjiBatch, Vec<BatchIssue>), AssetError> {
    let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
    let mut state = load_required(&mut runtime)?;
    // Решение публикует соседние элементы текущим валидатором, поэтому закреплённый
    // валидатор проверяется до любых изменений, а не после них.
    check_current_validator(&state)?;
    let mut snapshot = StoreSnapshotReader.capture(store);
    snapshot.checked()?;
    let identity = decision.identity.clone();
    let item = state
        .items
        .iter()
        .find(|item| item.identity == identity)
        .ok_or_else(|| invalid("identity отсутствует в пакете"))?;
    let candidate = if item.status == BatchItemStatus::ExistingVerified {
        // Для повторно использованного канонического элемента нет попытки получения.
        // Закрепляем байты владельца и добавляем локальные свидетельства, чтобы
        // явный отказ всё ещё можно было проверить.
        let verified = AssetStore::read_verified_with_policy(
            store.root(),
            std::slice::from_ref(&identity),
            &state.policy.validator,
            &crate::domain::KanjiDomainPolicy,
        )?;
        let record = &verified[0].record;
        if record.sha256 != decision.candidate_sha256 {
            return Err(invalid(
                "существующий канонический кандидат изменился после проверки",
            ));
        }
        // Одобрение человеком сохраняет доверие между версиями валидатора. Перед
        // записью этих байтов в пакет обновляем автоматическое свидетельство.
        let validation_is_current = record.validation.as_ref().is_some_and(|validation| {
            validation.is_valid_for_sha(&record.sha256)
                && validation.validator == state.policy.validator
        });
        if !validation_is_current {
            validate_exact_snapshot(store, &identity, &record.sha256, &mut snapshot)?;
        }
        let automated = snapshot
            .current(&identity)
            .filter(|current| current.sha256 == decision.candidate_sha256)
            .and_then(|current| current.validation.clone())
            .ok_or_else(|| {
                invalid("для существующего канонического кандидата отсутствуют актуальные автоматические свидетельства")
            })?;
        runtime.persist_candidate(&verified[0].bytes, automated, true)?
    } else {
        current_candidate(item)?.clone()
    };
    if state
        .items
        .iter()
        .any(|item| item.identity == identity && item.status == BatchItemStatus::ExistingVerified)
    {
        state.retain_existing_candidate(&identity, candidate.clone())?;
    }
    let bytes = runtime.read_candidate(&candidate)?;
    // Здесь не выводится решение человека: проверяется только явно заданное действие.
    state.decide(decision.clone(), &bytes)?;
    runtime.save(&state)?;
    let mut issues = Vec::new();
    if decision.action == HumanBatchAction::Confirm {
        publish_pending(store, &mut runtime, &mut state, &mut issues, &mut snapshot)?;
    } else if matches!(
        decision.action,
        HumanBatchAction::Reject | HumanBatchAction::RejectAndHold
    ) {
        // Сохраняем у общего владельца и семантические свидетельства, и отказ человека.
        if let Err(error) = apply_rejection(
            store,
            &runtime,
            &identity,
            &candidate,
            &bytes,
            &decision.reason,
            &mut snapshot,
        ) {
            issues.push(issue(&identity, &error));
        }
    }
    let snapshot = StoreSnapshotReader.capture(store);
    reconcile_owner_trust(&snapshot, &mut state, &mut issues)?;
    runtime.save(&state)?;
    Ok((state, issues))
}

fn materialize_candidate(
    store: &AssetStore,
    runtime: &BatchRuntime,
    identity: &AssetIdentity,
    candidate: &BatchCandidate,
    bytes: &[u8],
    attach_automated: bool,
    snapshot: &mut OwnerSnapshot,
) -> Result<(), AssetError> {
    snapshot.checked()?;
    if sha256_hex(bytes) != candidate.sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "байты, подготовленные к публикации, не совпадают с SHA-256",
        ));
    }
    let current = snapshot.current(identity).cloned();
    if current
        .as_ref()
        .is_some_and(|record| record.sha256 == candidate.sha256 && record.validation.is_some())
    {
        return Ok(());
    }
    let path = store
        .root()
        .join(".runtime/batches")
        .join(runtime.batch_id())
        .join(&candidate.storage_path);
    let outcome = store.ingest(IngestRequest {
        identity: identity.clone(),
        source_path: path,
        expected_source_sha256: Some(candidate.sha256.clone()),
        domain_metadata: Some(candidate_metadata(identity, candidate)),
        replace_expected_sha256: current.map(|record| record.sha256),
    })?;
    snapshot.committed(outcome.asset);
    if attach_automated {
        validate_exact_snapshot(store, identity, &candidate.sha256, snapshot)?;
    }
    Ok(())
}

fn validate_exact_snapshot(
    store: &AssetStore,
    identity: &AssetIdentity,
    sha256: &str,
    snapshot: &mut OwnerSnapshot,
) -> Result<(), AssetError> {
    let report = store.validate_exact(identity, sha256, &KanjiImageValidator::new())?;
    if !report.blockers.is_empty() {
        return Err(AssetError::new(
            ErrorCode::ValidatorFailure,
            "точная автоматическая проверка владельца завершилась техническим отказом",
        ));
    }
    let attempt = report
        .attempts
        .iter()
        .find(|attempt| &attempt.identity == identity && attempt.content_sha256 == sha256)
        .ok_or_else(|| invalid("точная проверка владельца не вернула ожидаемый результат"))?;
    let status = attempt
        .status
        .ok_or_else(|| invalid("в точной проверке владельца отсутствует семантический статус"))?;
    let mut record = snapshot
        .current(identity)
        .filter(|record| record.sha256 == sha256)
        .cloned()
        .ok_or_else(|| invalid("в снимке отсутствует запись с точным результатом проверки"))?;
    record.validation = Some(ValidationRecord {
        status,
        validator: report.validator,
        content_sha256: sha256.into(),
        evidence: attempt.evidence.clone(),
    });
    record.lifecycle = attempt.to_state;
    snapshot.committed(record);
    Ok(())
}

fn apply_rejection(
    store: &AssetStore,
    runtime: &BatchRuntime,
    identity: &AssetIdentity,
    candidate: &BatchCandidate,
    bytes: &[u8],
    reason: &str,
    snapshot: &mut OwnerSnapshot,
) -> Result<(), AssetError> {
    snapshot.checked()?;
    if sha256_hex(bytes) != candidate.sha256 {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "байты отклоняемого ресурса не совпадают с SHA-256",
        ));
    }
    let current = snapshot.current(identity).cloned();
    if current
        .as_ref()
        .is_some_and(|record| record.sha256 != candidate.sha256)
    {
        return Ok(());
    }
    let attach_automated = current
        .as_ref()
        .is_none_or(|record| record.validation.is_none());
    if current.is_none() {
        // CAS=None по-прежнему защищает от промежуточного изменения ресурса другим владельцем.
        let outcome = store.ingest(IngestRequest {
            identity: identity.clone(),
            source_path: store
                .root()
                .join(".runtime/batches")
                .join(runtime.batch_id())
                .join(&candidate.storage_path),
            expected_source_sha256: Some(candidate.sha256.clone()),
            domain_metadata: Some(candidate_metadata(identity, candidate)),
            replace_expected_sha256: None,
        })?;
        snapshot.committed(outcome.asset);
    }
    let outcome = store.attest(HumanAttestationRequest {
        identity: identity.clone(),
        expected_sha256: candidate.sha256.clone(),
        decision: HumanDecision::Reject,
        reason: reason.into(),
    })?;
    snapshot.committed(outcome.asset);
    if attach_automated {
        validate_exact_snapshot(store, identity, &candidate.sha256, snapshot)?;
    }
    Ok(())
}

fn verified_request(
    snapshot: &OwnerSnapshot,
    identity: &AssetIdentity,
    bytes: Vec<u8>,
    candidate: &BatchCandidate,
) -> Result<VerifiedIngestRequest, AssetError> {
    snapshot.checked()?;
    Ok(VerifiedIngestRequest {
        identity: identity.clone(),
        bytes,
        provenance: candidate_provenance(candidate),
        domain_metadata: Some(candidate_metadata(identity, candidate)),
        replace_expected_sha256: snapshot
            .current(identity)
            .map(|record| record.sha256.clone()),
    })
}

fn candidate_provenance(candidate: &BatchCandidate) -> Provenance {
    Provenance {
        source_kind: candidate
            .acquisition
            .as_ref()
            .map_or("kanji_batch_runtime", |evidence| evidence.provider.as_str())
            .into(),
        source_name: match candidate.format {
            DetectedFormat::Gif => "batch-candidate.gif",
            _ => "batch-candidate.png",
        }
        .into(),
    }
}

fn candidate_metadata(identity: &AssetIdentity, candidate: &BatchCandidate) -> serde_json::Value {
    let mut metadata = KanjiCharacter(identity.key.clone()).metadata();
    if let Some(evidence) = &candidate.acquisition {
        metadata["yarxi"] = serde_json::to_value(evidence)
            .expect("типизированные свидетельства получения сериализуются");
    }
    metadata
}

fn candidate_asset_record(
    identity: &AssetIdentity,
    bytes: &[u8],
    acquisition: Option<&crate::yarxi::AcquisitionEvidence>,
) -> Result<AssetRecord, AssetError> {
    let sha256 = sha256_hex(bytes);
    let format = DetectedFormat::from_signature(bytes);
    let location = KanjiDomainPolicy.canonical_location(identity, &sha256, format)?;
    Ok(AssetRecord {
        identity: identity.clone(),
        storage_path: location.storage_path,
        consumer_filename: location.consumer_filename,
        sha256,
        byte_length: bytes.len() as u64,
        format,
        provenance: Provenance {
            source_kind: acquisition
                .map_or("kanji_batch_runtime", |evidence| evidence.provider.as_str())
                .into(),
            source_name: "candidate".into(),
        },
        lifecycle: LifecycleState::Pending,
        validation: None,
        human_attestation: None,
        domain_metadata: Some(KanjiCharacter(identity.key.clone()).metadata()),
    })
}

struct AggregateValidator {
    resolution: AggregateResolution,
}

impl SemanticValidator for AggregateValidator {
    fn identity(&self) -> ValidatorIdentity {
        self.resolution.validator.clone()
    }

    fn validate(
        &self,
        asset: &AssetRecord,
        bytes: &mut dyn Read,
    ) -> Result<SemanticDecision, ValidatorFailure> {
        let mut actual = Vec::new();
        bytes
            .take(crate::kanji_validator::MAX_MEDIA_BYTES as u64 + 1)
            .read_to_end(&mut actual)
            .map_err(|error| ValidatorFailure::new("aggregate_read_failure", error.to_string()))?;
        if asset.identity != self.resolution.identity
            || asset.sha256 != self.resolution.candidate.sha256
            || sha256_hex(&actual) != self.resolution.candidate.sha256
            || self.identity() != KanjiImageValidator::validator_identity()
        {
            return Err(ValidatorFailure::new(
                "aggregate_candidate_mismatch",
                "решение агрегации не соответствует точным identity, SHA-256 и текущему валидатору",
            ));
        }
        validate_image_decode(&actual)
            .map_err(|error| ValidatorFailure::new("aggregate_decode_failure", error.message))?;
        let selected = KanjiImageValidator::new().validate(asset, &mut Cursor::new(actual))?;
        if selected.status == SemanticStatus::Corrupt {
            return Err(ValidatorFailure::new(
                "aggregate_corrupt_candidate",
                "кандидат CORRUPT не может получить доверие на основании агрегации",
            ));
        }
        if selected.status == SemanticStatus::Rejected {
            return Err(ValidatorFailure::new(
                "aggregate_rejected_candidate",
                "кандидат REJECTED не может получить доверие на основании агрегации",
            ));
        }
        let mut decision = self.resolution.decision.clone();
        // Сохраняем автоматические свидетельства одного кандидата рядом с явной политикой агрегации.
        decision.evidence.extend(selected.evidence);
        Ok(decision)
    }
}

fn candidate_by_hash<'a>(
    item: &'a crate::batch::BatchItem,
    hash: &str,
) -> Option<&'a BatchCandidate> {
    item.attempts
        .iter()
        .rev()
        .find_map(|attempt| match &attempt.result {
            BatchAttemptInput::Candidate { candidate } if candidate.sha256 == hash => {
                Some(candidate)
            }
            _ => None,
        })
        .or_else(|| {
            item.existing_candidate
                .as_ref()
                .filter(|candidate| candidate.sha256 == hash)
        })
}

fn current_candidate(item: &crate::batch::BatchItem) -> Result<&BatchCandidate, AssetError> {
    let hash = item
        .current_sha256
        .as_deref()
        .ok_or_else(|| invalid("у элемента нет текущего кандидата"))?;
    candidate_by_hash(item, hash)
        .ok_or_else(|| invalid("отсутствуют точные свидетельства кандидата"))
}

fn batch_response(
    operation: &str,
    store: StoreSummary,
    batch: KanjiBatch,
    changed: bool,
    issues: Vec<BatchIssue>,
    artifact: Option<String>,
    outcome: &'static str,
) -> BatchResponse {
    let ready = |status| {
        batch
            .items
            .iter()
            .filter(|item| item.status == status && item.is_ready())
            .count()
    };
    let counts = BatchCounts {
        requested: batch.items.len(),
        effective_verified: batch.items.iter().filter(|item| item.is_ready()).count(),
        auto_verified: ready(BatchItemStatus::AutoVerified),
        human_verified: ready(BatchItemStatus::HumanVerified),
        existing_verified: ready(BatchItemStatus::ExistingVerified),
        awaiting_human: batch.review_queue().len(),
        rejected: batch
            .items
            .iter()
            .filter(|item| item.status == BatchItemStatus::Rejected)
            .count(),
        scheduled_for_reacquire: batch
            .items
            .iter()
            .filter(|item| item.status == BatchItemStatus::Reacquire)
            .count(),
        pending_publication: batch
            .items
            .iter()
            .filter(|item| item.status.semantic_resolved() && !item.is_ready())
            .count(),
        unresolved: batch
            .items
            .iter()
            .filter(|item| !item.status.semantic_resolved())
            .count(),
    };
    let items = batch
        .items
        .iter()
        .map(|item| BatchItemOutcome {
            identity: item.identity.clone(),
            state: item.status,
            effective_verified: item.is_ready(),
            candidate_sha256: item.current_sha256.clone(),
            published_sha256: item.published_sha256.clone(),
            publication_source: item.publication_source,
            acquisition_attempts: item.attempts.len(),
            generation: item.generation,
            distinct_valid_hashes: item.aggregate.distinct_valid_hashes.len(),
            item_outcome: if item.is_ready() {
                "effective_verified"
            } else if item.status.semantic_resolved() {
                "pending_publication"
            } else if item.status == BatchItemStatus::AwaitingHuman {
                "awaiting_human"
            } else if item.status == BatchItemStatus::Rejected {
                "rejected"
            } else if item.status == BatchItemStatus::Reacquire {
                "reacquire_scheduled"
            } else {
                "unresolved"
            },
        })
        .collect();
    let blockers = issues
        .iter()
        .map(|issue| format!("{}:{}", issue.identity, issue.code))
        .collect();
    BatchResponse {
        schema_version: 1,
        operation: operation.into(),
        store,
        batch_id: batch.batch_id.clone(),
        outcome,
        changed,
        counts,
        items,
        issues,
        blockers,
        review_artifact: artifact,
        review_id: None,
        review_decisions: None,
        diagnostic_log: None,
        ocr_summary: None,
        batch,
    }
}

fn generated_batch_id() -> Result<String, AssetError> {
    let mut entropy = [0_u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut entropy))
        .map_err(|error| {
            AssetError::io(
                "не удалось получить случайные данные для идентификатора пакета",
                error,
            )
        })?;
    Ok(format!("kanji-{}", &sha256_hex(entropy)[..32]))
}

fn check_current_validator(state: &KanjiBatch) -> Result<(), AssetError> {
    if state.policy.validator != KanjiImageValidator::validator_identity() {
        return Err(AssetError::new(
            ErrorCode::InvalidValidatorIdentity,
            "закреплённый в пакете валидатор отличается от текущего; выполните batch revalidate на сохранённых байтах",
        ));
    }
    Ok(())
}
fn parse_character(value: &str) -> Result<KanjiCharacter, AssetError> {
    value
        .parse()
        .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))
}
fn validate_reason(reason: &str) -> Result<(), AssetError> {
    if reason.trim().is_empty() || reason.len() > MAX_HUMAN_REASON_BYTES {
        Err(invalid(format!(
            "причина должна быть непустой и не длиннее {MAX_HUMAN_REASON_BYTES} байт"
        )))
    } else {
        Ok(())
    }
}
fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}
fn issue(identity: &AssetIdentity, error: &AssetError) -> BatchIssue {
    BatchIssue {
        identity: identity.clone(),
        code: error.code.as_str().into(),
        message: error.message.chars().take(1000).collect(),
    }
}
fn failure(code: &str, message: &str) -> BatchAttemptInput {
    BatchAttemptInput::Failed {
        code: code.chars().take(256).collect(),
        message: message.chars().take(1000).collect(),
    }
}

#[cfg(test)]
#[path = "batch_cli_tests.rs"]
mod tests;

#[cfg(test)]
mod snapshot_cost_tests {
    use super::*;
    use crate::temp_workspace::TempWorkspace;

    struct CountedSnapshots {
        records: Vec<AssetRecord>,
        captures: usize,
    }
    impl OwnerSnapshotReader for CountedSnapshots {
        fn capture(&mut self, _store: &AssetStore) -> OwnerSnapshot {
            self.captures += 1;
            OwnerSnapshot::from_result(Ok(self.records.clone()))
        }
    }

    /// Проверяет рабочие пути команд и раундов, отдельно считая обращения
    /// к проверенному снимку владельца. Физический корпус пуст, поэтому
    /// повторное `read_verified` для каждого элемента не сможет незаметно пройти.
    #[test]
    fn thousand_ready_identities_use_bounded_full_owner_snapshots() {
        let workspace = TempWorkspace::create("asset-store-batch-snapshot-cost-test").unwrap();
        let directory = workspace.path();
        let store = AssetStore::open_kanji(StoreOptions::new(directory.join("corpus"))).unwrap();
        let summary = StoreSummary {
            path: store.root().display().to_string(),
            store_id: Some(store.store_id().into()),
            layout_migrated_on_open: store.layout_migrated_on_open(),
        };
        let characters: Vec<_> = (0x4e00..0x4e00 + 1000)
            .map(|code| char::from_u32(code).unwrap().to_string())
            .collect();
        let records = characters
            .iter()
            .map(|character| {
                let identity = parse_character(character).unwrap().identity();
                let consumer_filename = format!("{}.png", identity.key);
                let sha256 = sha256_hex(character.as_bytes());
                AssetRecord {
                    identity,
                    storage_path: format!("assets/png/{consumer_filename}"),
                    consumer_filename,
                    sha256: sha256.clone(),
                    byte_length: 1,
                    format: DetectedFormat::Png,
                    provenance: Provenance {
                        source_kind: "local_import".into(),
                        source_name: "synthetic.png".into(),
                    },
                    lifecycle: LifecycleState::Verified,
                    validation: Some(ValidationRecord {
                        status: SemanticStatus::Verified,
                        validator: KanjiImageValidator::validator_identity(),
                        content_sha256: sha256,
                        evidence: vec![crate::model::ValidationEvidence {
                            kind: "snapshot_test_evidence".into(),
                            summary: "синтетическое свидетельство для проверки снимка".into(),
                            details: None,
                        }],
                    }),
                    human_attestation: None,
                    domain_metadata: None,
                }
            })
            .collect();
        let mut reader = CountedSnapshots {
            records,
            captures: 0,
        };
        let start = BatchCommand::Start {
            batch_id: Some("thousand-ready".into()),
            characters,
        };
        let (response, _) =
            execute_command_with_snapshots(&store, summary.clone(), &start, false, &mut reader)
                .unwrap();
        assert_eq!(response.counts.effective_verified, 1000);
        assert_eq!(
            reader.captures, 1,
            "при запуске используется один полный снимок"
        );
        for command in [
            start,
            BatchCommand::Status {
                batch_id: "thousand-ready".into(),
            },
            BatchCommand::Review {
                batch_id: "thousand-ready".into(),
            },
        ] {
            reader.captures = 0;
            let (response, _) = execute_command_with_snapshots(
                &store,
                summary.clone(),
                &command,
                false,
                &mut reader,
            )
            .unwrap();
            assert_eq!(response.counts.effective_verified, 1000);
            assert_eq!(
                reader.captures, 1,
                "повторный запуск, просмотр состояния и проверка используют один полный снимок"
            );
        }
        reader.captures = 0;
        let (state, changed, issues) = run_batch_with_snapshots(
            &store,
            "thousand-ready",
            5,
            |_| panic!("доверенный корпус не должен запускать получение"),
            &mut reader,
        )
        .unwrap();
        assert_eq!(
            state.items.iter().filter(|item| item.is_ready()).count(),
            1000
        );
        assert!(!changed);
        assert!(issues.is_empty());
        assert_eq!(
            reader.captures, 2,
            "для готового запуска используются начальный и итоговый снимки"
        );
        // Для границы получения также делается один снимок на раунд,
        // независимо от числа результатов элементов, сохранённых в раунде.
        reader.records.clear();
        reader.captures = 0;
        execute_command_with_snapshots(
            &store,
            summary,
            &BatchCommand::Start {
                batch_id: Some("unresolved-frontier".into()),
                characters: (0x4e00..0x4e00 + 12)
                    .map(|code| char::from_u32(code).unwrap().to_string())
                    .collect(),
            },
            false,
            &mut reader,
        )
        .unwrap();
        assert_eq!(reader.captures, 1);
        reader.captures = 0;
        let (state, _, _) = run_batch_with_snapshots(
            &store,
            "unresolved-frontier",
            1,
            |characters| {
                Ok(characters
                    .iter()
                    .map(|_| Err("синтетическая ошибка".into()))
                    .collect())
            },
            &mut reader,
        )
        .unwrap();
        assert_eq!(state.items.len(), 12);
        assert!(state.items.iter().all(|item| item.attempts.len() == 1));
        assert_eq!(
            reader.captures, 3,
            "начальный снимок + один снимок границы обработки + итоговый снимок"
        );
    }
}

#[cfg(test)]
mod candidate_cost_tests {
    use super::*;
    use crate::batch::CANDIDATE_FILE_READS;
    use crate::temp_workspace::TempWorkspace;

    /// Цикл сохранения проверяет состояние после каждого элемента, поэтому
    /// повторная проверка всех файлов кандидатов давала бы квадратичный обход.
    /// Фикстура хранит по одному настоящему кандидату на символ и падает, если
    /// число чтений снова станет пропорционально квадрату числа элементов.
    #[test]
    fn frontier_round_reads_each_candidate_a_bounded_number_of_times() {
        const ITEMS: u32 = 200;
        let workspace = TempWorkspace::create("asset-store-batch-candidate-cost-test").unwrap();
        let directory = workspace.path();
        let store = AssetStore::open_kanji(StoreOptions::new(directory.join("corpus"))).unwrap();
        let summary = StoreSummary {
            path: store.root().display().to_string(),
            store_id: Some(store.store_id().into()),
            layout_migrated_on_open: store.layout_migrated_on_open(),
        };
        let characters: Vec<_> = (0x4e00..0x4e00 + ITEMS)
            .map(|code| char::from_u32(code).unwrap().to_string())
            .collect();
        execute_command(
            &store,
            summary.clone(),
            &BatchCommand::Start {
                batch_id: Some("candidate-cost".into()),
                characters: characters.clone(),
            },
            false,
        )
        .unwrap();
        let mut runtime = BatchRuntime::open(store.root(), "candidate-cost").unwrap();
        let mut state = runtime.load().unwrap().unwrap();
        // Агрегатная политика оставляет `Uncertain` в границе обхода,
        // поэтому запуск проходит через checkpoint каждого кандидата.
        state.policy = crate::batch::AggregatePolicy::current(state.policy.validator.clone());
        for (index, character) in characters.iter().enumerate() {
            let identity = parse_character(character).unwrap().identity();
            let bytes = synthetic_png(index as u8);
            let sha256 = sha256_hex(&bytes);
            let record = ValidationRecord {
                status: SemanticStatus::Uncertain,
                validator: KanjiImageValidator::validator_identity(),
                content_sha256: sha256.clone(),
                evidence: vec![ValidationEvidence {
                    kind: "pixel_reference_comparison".into(),
                    summary: "синтетические независимые метрики".into(),
                    details: Some(serde_json::json!({
                        "expected_distance": 0.5,
                        "nearest_margin": -0.5,
                        "nearest_other": "漠",
                    })),
                }],
            };
            let candidate = runtime.persist_candidate(&bytes, record, true).unwrap();
            state
                .record_attempt(&identity, BatchAttemptInput::Candidate { candidate })
                .unwrap();
        }
        runtime.save(&state).unwrap();
        drop(runtime);
        CANDIDATE_FILE_READS.with(|reads| reads.set(0));
        let (state, _, _) = run_batch(&store, "candidate-cost", 1, |characters| {
            Ok(characters
                .iter()
                .map(|_| Err("синтетическая ошибка".into()))
                .collect())
        })
        .unwrap();
        let reads = CANDIDATE_FILE_READS.with(std::cell::Cell::get);
        assert_eq!(state.items.len(), ITEMS as usize);
        assert!(state.items.iter().all(|item| item.attempts.len() == 2));
        // Линейный обход требует несколько чтений на символ; прежний
        // квадратичный путь давал ITEMS^2 = 40000.
        assert!(
            reads <= 8 * ITEMS as usize,
            "candidate-файлы перечитаны {reads} раз при {ITEMS} identity"
        );
    }

    fn synthetic_png(index: u8) -> Vec<u8> {
        let mut image = image::RgbaImage::new(8, 8);
        for (x, _, pixel) in image.enumerate_pixels_mut() {
            *pixel = image::Rgba([index, x as u8, 0, 255]);
        }
        let mut encoded = Cursor::new(Vec::new());
        image
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        encoded.into_inner()
    }
}

#[derive(Debug, Default, Serialize)]
struct ReviewApplied {
    applied: usize,
    confirmed: usize,
    rejected: usize,
    skipped: usize,
    already_applied: usize,
    changed: bool,
}

fn apply_review(
    store: &AssetStore,
    batch_id: &str,
    review_id: &str,
) -> Result<(KanjiBatch, Vec<BatchIssue>, ReviewApplied), AssetError> {
    apply_review_with_matches(store, batch_id, review_id, false)
}

fn apply_review_with_matches(
    store: &AssetStore,
    batch_id: &str,
    review_id: &str,
    confirm_matches: bool,
) -> Result<(KanjiBatch, Vec<BatchIssue>, ReviewApplied), AssetError> {
    let mut runtime = BatchRuntime::open(store.root(), batch_id)?;
    let mut state = load_required(&mut runtime)?;
    let initial_revision = state.revision;
    check_current_validator(&state)?;
    let (review, mut decisions) = crate::kanji_review::read(&runtime, review_id)?;
    if confirm_matches {
        for (row, decision) in review.rows.iter().zip(&mut decisions) {
            if matches!(decision, crate::kanji_review::ParsedDecision::Unspecified)
                && row.ocr.as_ref().is_some_and(|ocr| ocr.group() == 0)
            {
                *decision = crate::kanji_review::ParsedDecision::Apply(HumanBatchDecision {
                    identity: row.identity.clone(),
                    candidate_sha256: row.sha256.clone(),
                    action: HumanBatchAction::Confirm,
                    reason: "explicitly_reviewed_ocr_matching_group".into(),
                });
            }
        }
        if !review
            .rows
            .iter()
            .any(|row| row.ocr.as_ref().is_some_and(|ocr| ocr.group() == 0))
        {
            return Err(invalid("в снимке нет совпавших предложений OCR"));
        }
    }
    if review.validator != state.policy.validator {
        return Err(invalid("валидатор review устарел"));
    }
    let mut owner = StoreSnapshotReader.capture(store);
    owner.checked()?;
    let mut next = state.clone();
    let mut applied = ReviewApplied::default();
    // Вся синтаксическая и предметная проверка, включая декодирование,
    // заканчивается до единственной атомарной записи всех намерений.
    for (row, parsed_decision) in review.rows.iter().zip(decisions) {
        let decision = match parsed_decision {
            crate::kanji_review::ParsedDecision::Apply(decision) => Some(decision),
            crate::kanji_review::ParsedDecision::Unspecified
            | crate::kanji_review::ParsedDecision::Skip => None,
        };
        let item = state
            .items
            .iter()
            .find(|item| item.identity == row.identity)
            .ok_or_else(|| invalid("review ссылается на чужой Unicode"))?;
        let candidate = crate::kanji_review::candidate(&state, row)?;
        let bytes = runtime.read_candidate(candidate)?;
        let repeated = decision.as_ref().is_some_and(|decision| {
            item.human_decisions.last() == Some(decision)
                && item.attempts.len() == row.attempts
                && if decision.action == HumanBatchAction::Confirm {
                    item.generation == row.generation
                        && item.current_sha256.as_ref() == Some(&row.sha256)
                        && item.status == BatchItemStatus::HumanVerified
                } else {
                    row.generation.checked_add(1) == Some(item.generation)
                        && item.current_sha256.is_none()
                        && item.status == BatchItemStatus::Rejected
                }
        });
        if repeated {
            applied.already_applied += 1;
            continue;
        }
        if item.status != BatchItemStatus::AwaitingHuman
            || item.generation != row.generation
            || item.attempts.len() != row.attempts
            || item.current_sha256.as_ref() != Some(&row.sha256)
        {
            return Err(invalid(
                "review устарел: статус, поколение, попытка или текущий SHA изменились",
            ));
        }
        if let Some(decision) = decision {
            if decision.action == HumanBatchAction::Confirm {
                validate_image_decode(&bytes)?;
                applied.confirmed += 1;
            } else {
                applied.rejected += 1;
            }
            next.decide(decision, &bytes)?;
            applied.applied += 1;
        } else {
            applied.skipped += 1;
        }
    }
    next.validate()?;
    if applied.applied > 0 {
        runtime.save(&next)?;
    }
    state = next;
    let mut issues = Vec::new();
    // KanjiBatch.human_decisions — сохраняемый журнал. Сбой после save
    // возобновляется здесь или через batch run; чужие байты не подтверждаются.
    publish_pending(store, &mut runtime, &mut state, &mut issues, &mut owner)?;
    let owner = StoreSnapshotReader.capture(store);
    reconcile_owner_trust(&owner, &mut state, &mut issues)?;
    runtime.save(&state)?;
    applied.changed = state.revision != initial_revision;
    Ok((state, issues, applied))
}

fn finish_interactive_review(
    store: &AssetStore,
    response: &mut BatchResponse,
    interaction: &mut dyn super::review_ui::ReviewInteraction,
) -> Result<(), AssetError> {
    if !interaction.eligible() {
        return Err(invalid("интерактивное окружение больше недоступно"));
    }
    if response
        .batch
        .review_queue()
        .iter()
        .all(|item| item.current_sha256.is_none())
    {
        return Ok(());
    }
    let message = format!(
        "Получение завершено; подтверждено: {}/{}; ожидают решения: {}. Открыть пакет ручной проверки?",
        response.counts.effective_verified,
        response.counts.requested,
        response.counts.awaiting_human
    );
    if !interaction.ask(&message)? {
        return Ok(());
    }
    let bundle = {
        let mut runtime = BatchRuntime::open(store.root(), &response.batch_id)?;
        let state = load_required(&mut runtime)?;
        check_current_validator(&state)?;
        crate::kanji_review::prepare(&runtime, &state)?
    }; // Блокировка освобождена до редактора и ожидания пользователя.
    response.review_id = Some(bundle.id.clone());
    response.review_artifact = Some(bundle.decisions.display().to_string());
    interaction.open(&super::review_ui::ReviewOpen {
        batch_id: &response.batch_id,
        review_id: &bundle.id,
        folder: &bundle.directory,
        decisions: &bundle.decisions,
    })?;
    if !interaction
        .ask("Сохраните decisions.tsv. Применить сохранённый файл? (y для применения)")?
    {
        return Ok(());
    }
    let (state, apply_issues, applied) = apply_review(store, &response.batch_id, &bundle.id)?;
    let mut issues = std::mem::take(&mut response.issues);
    issues.retain(|previous| {
        state
            .items
            .iter()
            .find(|item| item.identity == previous.identity)
            .is_none_or(|item| !item.is_ready())
    });
    for new_issue in apply_issues {
        if !issues.iter().any(|previous| {
            previous.identity == new_issue.identity
                && previous.code == new_issue.code
                && previous.message == new_issue.message
        }) {
            issues.push(new_issue);
        }
    }
    let outcome = if response.operation == "batch_run" {
        if state.is_resolved() {
            "resolved"
        } else if state.review_queue().is_empty() {
            "partial_progress"
        } else {
            "awaiting_human"
        }
    } else {
        "ok"
    };
    let mut updated = batch_response(
        &response.operation,
        response.store.clone(),
        state,
        response.changed || applied.changed,
        issues,
        response.review_artifact.clone(),
        outcome,
    );
    updated.review_id = Some(bundle.id);
    updated.review_decisions = Some(applied);
    *response = updated;
    Ok(())
}
