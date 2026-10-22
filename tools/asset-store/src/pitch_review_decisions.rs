//! Снимок проверки и адресные решения человека для пакета pitch accent.
//!
//! Общий жизненный цикл снимка (адресация `review_id`, точные копии PNG,
//! неизменяемые колонки `decisions.tsv`, отказ при подмене) принадлежит
//! `review_snapshot`; источник истины — сохранённый `PitchAccentBatch`, а решения
//! применяются теми же переходами, что и адресные команды `select`/`reject`.
//!
//! Уровень доверия не меняется: `pitch_accent` использует
//! `TrustSemantics::AUTOMATED_VERIFIED_ONLY`. Действие `y` лишь подтверждает
//! просмотр текущего автоматически проверенного SHA и отклоняется целиком, если
//! у строки нет актуального `VERIFIED` текущего валидатора; оно не публикует и не
//! делает пригодным `UNCERTAIN`, `REJECTED` или `CORRUPT`. Действие `r` отклоняет
//! точные байты; число в строке неоднозначной записи выбирает словарную запись
//! JPDB только из перечисленных в снимке пар ID и URL.

use serde::{Deserialize, Serialize};

use crate::error::{AssetError, ErrorCode};
use crate::jpdb::JpdbPitchSelection;
use crate::model::{AssetIdentity, AssetRecord, HumanDecision, LifecycleState, SemanticStatus};
use crate::pitch_batch::{
    PitchAccentBatch, PitchAccentBatchRuntime, PitchBatchItem, PitchBatchItemStatus,
    PitchBatchOutcome, PitchBatchOwnerSnapshot,
};
use crate::review_snapshot::{ReviewBundle, ReviewCell, ReviewFormat};
use crate::store::{AssetStore, HumanAttestationRequest};

const SCHEMA: u32 = 1;

const FORMAT: ReviewFormat = ReviewFormat {
    schema: SCHEMA,
    comments: &[
        "# Проверка произношения JPDB, формат v1",
        "# Строка кандидата: y = просмотрено, текущий автоматический VERIFIED принят как есть (доверие не меняется); r = отклонить точный SHA",
        "# Строка неоднозначной записи: число = выбрать словарную запись JPDB с этим ID из колонки кандидатов",
        "# Пусто = без решения; - = пропуск. Только первая колонка; разделитель TAB. Сохраните файл перед применением.",
    ],
};

const SELECTION_REASON: &str = "explicit_vocabulary_selection";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PitchReviewKind {
    Candidate,
    AmbiguousVocabulary,
}

impl PitchReviewKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::AmbiguousVocabulary => "ambiguous_vocabulary",
        }
    }
}

/// Словарная запись JPDB, перечисленная провайдером для неоднозначного поиска.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PitchReviewVocabulary {
    pub vocabulary_id: u64,
    pub detail_url: String,
}

/// Строка привязана к точному токену элемента: поколению, ревизии, запросу и попытке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PitchReviewRow {
    pub identity: AssetIdentity,
    pub kind: PitchReviewKind,
    pub status: PitchBatchItemStatus,
    pub generation: u32,
    pub item_revision: u64,
    pub request_fingerprint: String,
    pub attempt_index: u32,
    pub sha256: Option<String>,
    pub validation_status: Option<SemanticStatus>,
    pub filename: Option<String>,
    pub vocabulary_candidates: Vec<PitchReviewVocabulary>,
    /// Необязательное предложение LLM для точного кандидата строки; не решение.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm: Option<crate::pitch_llm_review::PitchLlmSuggestion>,
}

fn status_name(status: PitchBatchItemStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl crate::review_snapshot::ReviewRow for PitchReviewRow {
    fn identity(&self) -> &AssetIdentity {
        &self.identity
    }

    fn image(&self) -> Option<(&str, &str)> {
        self.filename.as_deref().zip(self.sha256.as_deref())
    }

    fn columns(&self) -> String {
        let candidates = self
            .vocabulary_candidates
            .iter()
            .map(|candidate| format!("{}={}", candidate.vocabulary_id, candidate.detail_url))
            .collect::<Vec<_>>()
            .join(" ");
        let mut columns = format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            self.identity.key,
            self.kind.as_str(),
            status_name(self.status),
            self.generation,
            self.item_revision,
            self.attempt_index,
            self.sha256.as_deref().unwrap_or("-"),
            self.validation_status.map_or("-", SemanticStatus::as_str),
            self.filename.as_deref().unwrap_or("-"),
            if candidates.is_empty() {
                "-"
            } else {
                &candidates
            },
        );
        if let Some(llm) = &self.llm {
            columns.push_str(&format!("\t{}\t{}", llm.label(), llm.key()));
        }
        columns
    }

    fn validate(&self) -> Result<(), AssetError> {
        let consistent = match self.kind {
            PitchReviewKind::Candidate => {
                self.sha256.is_some()
                    && self.filename.is_some()
                    && self.validation_status.is_some()
                    && self.vocabulary_candidates.is_empty()
            }
            PitchReviewKind::AmbiguousVocabulary => {
                self.sha256.is_none()
                    && self.filename.is_none()
                    && self.validation_status.is_none()
                    && !self.vocabulary_candidates.is_empty()
            }
        };
        if !consistent {
            return Err(invalid(
                "строка review pitch accent внутренне несогласована",
            ));
        }
        if self.llm.as_ref().is_some_and(|llm| !llm.matches_row(self)) {
            return Err(invalid(
                "предложение LLM не принадлежит точному кандидату строки review",
            ));
        }
        Ok(())
    }
}

pub(crate) type PitchReviewSnapshot = crate::review_snapshot::ReviewSnapshot<PitchReviewRow>;

/// Сводка применения; машиночитаемая часть ответа `batch review-apply`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct PitchReviewApplied {
    pub applied: usize,
    pub selected: usize,
    pub rejected: usize,
    pub acknowledged: usize,
    pub skipped: usize,
    pub already_applied: usize,
    pub owner_rejections: usize,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PitchReviewDecision {
    Acknowledge,
    Reject,
    Select(JpdbPitchSelection),
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn rejection_reason(review_id: &str) -> String {
    format!("review:{review_id}; visual_rejection")
}

/// Строки, по которым человек может принять решение, в порядке пакета.
pub(crate) fn review_rows(batch: &PitchAccentBatch) -> Result<Vec<PitchReviewRow>, AssetError> {
    let mut ambiguous = Vec::new();
    let mut candidates = Vec::new();
    for item in &batch.items {
        let token = item.token();
        let status = item.status();
        match (status, item.current_outcome()) {
            (
                PitchBatchItemStatus::AmbiguousVocabulary,
                Some(PitchBatchOutcome::AmbiguousVocabulary {
                    candidates: vocabulary,
                    ..
                }),
            ) => {
                let attempt = item
                    .attempts
                    .last()
                    .expect("текущий исход принадлежит попытке");
                let vocabulary_candidates = vocabulary
                    .iter()
                    .map(|candidate| {
                        if candidate.detail_url.chars().any(char::is_whitespace) {
                            return Err(invalid(
                                "маршрут словарной записи JPDB содержит пробельный символ",
                            ));
                        }
                        Ok(PitchReviewVocabulary {
                            vocabulary_id: candidate.vocabulary_id,
                            detail_url: candidate.detail_url.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                ambiguous.push(PitchReviewRow {
                    identity: item.identity.clone(),
                    kind: PitchReviewKind::AmbiguousVocabulary,
                    status,
                    generation: token.generation,
                    item_revision: token.item_revision,
                    request_fingerprint: token.request_fingerprint,
                    attempt_index: attempt.index,
                    sha256: None,
                    validation_status: None,
                    filename: None,
                    vocabulary_candidates,
                    llm: None,
                });
            }
            (
                PitchBatchItemStatus::AcquiredVerified
                | PitchBatchItemStatus::Published
                | PitchBatchItemStatus::CandidateRejected,
                _,
            ) => {
                // Уже отклонённые человеком байты не предлагаются повторно.
                let Some(candidate) = item.current_candidate() else {
                    continue;
                };
                let attempt_index = item
                    .current_candidate_attempt_index
                    .expect("текущий кандидат принадлежит попытке");
                candidates.push(PitchReviewRow {
                    identity: item.identity.clone(),
                    kind: PitchReviewKind::Candidate,
                    status,
                    generation: token.generation,
                    item_revision: token.item_revision,
                    request_fingerprint: token.request_fingerprint,
                    attempt_index,
                    sha256: Some(candidate.sha256.clone()),
                    validation_status: Some(candidate.validation.status),
                    filename: Some(format!("{}-{}.png", item.identity.key, candidate.sha256)),
                    vocabulary_candidates: Vec::new(),
                    llm: None,
                });
            }
            _ => {}
        }
    }
    ambiguous.extend(candidates);
    Ok(ambiguous)
}

pub(crate) fn owner_snapshot(
    store: &AssetStore,
) -> Result<(Vec<AssetRecord>, PitchBatchOwnerSnapshot), AssetError> {
    let records = store.verify_integrity()?;
    let snapshot = PitchBatchOwnerSnapshot::from_records(records.clone())?;
    Ok((records, snapshot))
}

/// Возвращает сохранённое состояние пакета и его сверку с владельцем в памяти.
pub(crate) fn load_reconciled(
    runtime: &mut PitchAccentBatchRuntime,
    owner: &PitchBatchOwnerSnapshot,
) -> Result<(PitchAccentBatch, PitchAccentBatch), AssetError> {
    let stored = runtime
        .load()?
        .ok_or_else(|| invalid("сохранённое состояние batch не найдено"))?;
    let mut batch = stored.clone();
    batch.reconcile_owner(owner)?;
    Ok((stored, batch))
}

/// Создаёт снимок по сверенному с владельцем состоянию пакета.
pub(crate) fn export(
    store: &AssetStore,
    batch_id: &str,
) -> Result<(PitchAccentBatch, ReviewBundle), AssetError> {
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    let (_, owner) = owner_snapshot(store)?;
    let (stored, batch) = load_reconciled(&mut runtime, &owner)?;
    if batch != stored {
        runtime.save(&batch)?;
    }
    let mut rows = review_rows(&batch)?;
    let suggestions = crate::pitch_llm_review::saved_or_empty(&runtime);
    for row in &mut rows {
        row.llm = suggestions
            .get(&row.identity)
            .filter(|suggestion| suggestion.matches_row(row))
            .cloned();
    }
    if rows.is_empty() {
        return Err(invalid(
            "нет кандидатов или неоднозначных записей, ожидающих просмотра",
        ));
    }
    let snapshot = PitchReviewSnapshot {
        schema_version: SCHEMA,
        batch_id: batch.batch_id.clone(),
        validator: batch.validator.clone(),
        rows,
    };
    let (bundle, directory) = crate::review_snapshot::export(
        |name| runtime.artifact_directory(name),
        &snapshot,
        &FORMAT,
        |row| {
            let item = batch
                .items
                .iter()
                .find(|item| item.identity == row.identity)
                .ok_or_else(|| invalid("строка review не принадлежит пакету"))?;
            let candidate = item
                .candidate_at(row.attempt_index, row.sha256.as_deref().unwrap_or_default())
                .ok_or_else(|| invalid("кандидат review отсутствует или заменён"))?;
            runtime.read_candidate(&batch, candidate)
        },
    )?;
    write_preview(&directory, &snapshot)?;
    Ok((batch, bundle))
}

fn parse_decision(
    row: &PitchReviewRow,
    cell: ReviewCell,
) -> Result<Option<PitchReviewDecision>, AssetError> {
    let action = match cell {
        ReviewCell::Unspecified | ReviewCell::Skip => return Ok(None),
        ReviewCell::Action(action) => action,
    };
    match row.kind {
        PitchReviewKind::Candidate => match action.as_str() {
            "y" => {
                if row.validation_status != Some(SemanticStatus::Verified) {
                    return Err(AssetError::new(
                        ErrorCode::InvalidValidationEvidence,
                        format!(
                            "{}: подтверждение возможно только для актуального VERIFIED текущего валидатора; решение человека не делает пригодными UNCERTAIN, REJECTED или CORRUPT",
                            row.identity.key
                        ),
                    ));
                }
                Ok(Some(PitchReviewDecision::Acknowledge))
            }
            "r" => Ok(Some(PitchReviewDecision::Reject)),
            _ => Err(invalid(format!(
                "{}: решение для кандидата — y, r, - или пусто",
                row.identity.key
            ))),
        },
        PitchReviewKind::AmbiguousVocabulary => {
            let selected = action
                .parse::<u64>()
                .ok()
                .filter(|_| action.bytes().all(|byte| byte.is_ascii_digit()))
                .and_then(|id| {
                    row.vocabulary_candidates
                        .iter()
                        .find(|candidate| candidate.vocabulary_id == id)
                })
                .ok_or_else(|| {
                    AssetError::new(
                        ErrorCode::IdentityConflict,
                        format!(
                            "{}: выбор должен быть ID словарной записи JPDB из колонки кандидатов этой строки",
                            row.identity.key
                        ),
                    )
                })?;
            let selection =
                JpdbPitchSelection::new(selected.vocabulary_id, selected.detail_url.clone())
                    .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
            Ok(Some(PitchReviewDecision::Select(selection)))
        }
    }
}

/// Строка совпадает с текущим состоянием элемента: поколение, ревизия, запрос,
/// попытка и точные байты.
fn row_is_current(item: &PitchBatchItem, row: &PitchReviewRow) -> bool {
    let token = item.token();
    if token.generation != row.generation
        || token.item_revision != row.item_revision
        || token.request_fingerprint != row.request_fingerprint
    {
        return false;
    }
    match row.kind {
        PitchReviewKind::Candidate => {
            item.current_candidate_sha256.as_deref() == row.sha256.as_deref()
                && item.current_candidate_attempt_index == Some(row.attempt_index)
        }
        PitchReviewKind::AmbiguousVocabulary => {
            matches!(
                item.current_outcome(),
                Some(PitchBatchOutcome::AmbiguousVocabulary { .. })
            ) && item.attempts.last().map(|attempt| attempt.index) == Some(row.attempt_index)
        }
    }
}

/// Решение этого снимка уже сохранено: повтор применения ничего не меняет.
fn already_applied(
    item: &PitchBatchItem,
    row: &PitchReviewRow,
    decision: &PitchReviewDecision,
    review_id: &str,
) -> bool {
    match decision {
        PitchReviewDecision::Reject => item.rejected_candidates.iter().any(|rejection| {
            Some(rejection.candidate_sha256.as_str()) == row.sha256.as_deref()
                && rejection.reason == rejection_reason(review_id)
        }),
        PitchReviewDecision::Select(selection) => {
            row.generation.checked_add(1) == Some(item.generation)
                && row.item_revision.checked_add(1) == Some(item.item_revision)
                && item.request.selection.as_ref() == Some(selection)
                && item.last_action_reason.as_deref() == Some(SELECTION_REASON)
        }
        PitchReviewDecision::Acknowledge => false,
    }
}

/// Применяет снимок атомарно: все строки проверяются до единственной записи
/// пакета; отказ любой строки не меняет ни пакет, ни канонический корпус.
pub(crate) fn apply(
    store: &AssetStore,
    batch_id: &str,
    review_id: &str,
) -> Result<(PitchAccentBatch, PitchReviewApplied), AssetError> {
    // Снимок владельца берётся под блокировкой пакета: команда, которую `apply`
    // дожидается, могла за это время изменить запись владельца.
    let mut runtime = PitchAccentBatchRuntime::open(store.root(), batch_id)?;
    let (owner_records, owner) = owner_snapshot(store)?;
    // Сверка с владельцем остаётся в памяти до проверки всех строк: отклонённый
    // файл решений не меняет сохранённый пакет.
    let (stored, batch) = load_reconciled(&mut runtime, &owner)?;
    let (snapshot, cells) = crate::review_snapshot::read::<PitchReviewRow>(
        |name| runtime.artifact_directory(name),
        runtime.batch_id(),
        review_id,
        &FORMAT,
    )?;
    if snapshot.validator != batch.validator {
        return Err(invalid("валидатор review устарел"));
    }
    let mut next = batch.clone();
    let mut applied = PitchReviewApplied::default();
    let mut owner_rejections = Vec::new();
    for (row, cell) in snapshot.rows.iter().zip(cells) {
        let Some(decision) = parse_decision(row, cell)? else {
            applied.skipped += 1;
            continue;
        };
        let item = batch
            .items
            .iter()
            .find(|item| item.identity == row.identity)
            .ok_or_else(|| invalid("review ссылается на чужую идентичность"))?;
        let repeated = already_applied(item, row, &decision, review_id);
        if !repeated && !row_is_current(item, row) {
            return Err(invalid(format!(
                "{}: review устарел: поколение, ревизия, попытка или текущий SHA изменились",
                row.identity.key
            )));
        }
        // Точные байты строки сверяются с сохранённым кандидатом и текущим валидатором.
        if let Some(sha256) = row.sha256.as_deref() {
            let candidate = item
                .candidate_at(row.attempt_index, sha256)
                .ok_or_else(|| invalid("кандидат review отсутствует или заменён"))?;
            runtime.read_candidate(&batch, candidate)?;
            if decision == PitchReviewDecision::Acknowledge
                && (candidate.validation.status != SemanticStatus::Verified
                    || candidate.validation.validator != batch.validator
                    || item
                        .rejected_candidates
                        .iter()
                        .any(|rejection| rejection.candidate_sha256 == sha256))
            {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    format!(
                        "{}: подтверждение возможно только для актуального VERIFIED текущего валидатора",
                        row.identity.key
                    ),
                ));
            }
        }
        if repeated {
            applied.already_applied += 1;
        } else {
            match &decision {
                PitchReviewDecision::Acknowledge => applied.acknowledged += 1,
                PitchReviewDecision::Reject => {
                    next.reject_candidate(
                        &row.identity.key,
                        row.sha256
                            .as_deref()
                            .expect("строка кандидата содержит SHA"),
                        rejection_reason(review_id),
                    )?;
                    applied.rejected += 1;
                    applied.applied += 1;
                }
                PitchReviewDecision::Select(selection) => {
                    next.select_candidate(
                        &row.identity.key,
                        selection.vocabulary_id,
                        selection.detail_url.clone(),
                    )?;
                    applied.selected += 1;
                    applied.applied += 1;
                }
            }
        }
        // Отклонённые байты, совпадающие с записью владельца, помещаются в
        // карантин той же операцией хранилища, что и у адресного `batch reject`.
        if decision == PitchReviewDecision::Reject {
            let sha256 = row
                .sha256
                .as_deref()
                .expect("строка кандидата содержит SHA");
            if owner_records.iter().any(|record| {
                record.identity == row.identity
                    && record.sha256 == sha256
                    && !(record.lifecycle == LifecycleState::Quarantined
                        && record
                            .human_attestation
                            .as_ref()
                            .is_some_and(|attestation| {
                                attestation.decision == HumanDecision::Reject
                                    && attestation.reason == rejection_reason(review_id)
                            }))
            }) {
                owner_rejections.push((row.identity.clone(), sha256.to_owned()));
            }
        }
    }
    next.validate()?;
    // Намерения сохраняются одной атомарной записью до побочных эффектов у
    // владельца; сбой после неё повторяется тем же `review-apply`.
    if next != stored {
        runtime.save(&next)?;
    }
    let saved = next.clone();
    for (identity, sha256) in owner_rejections {
        let rejected = store.attest(HumanAttestationRequest {
            identity,
            expected_sha256: sha256,
            decision: HumanDecision::Reject,
            reason: rejection_reason(review_id),
        })?;
        if rejected.changed {
            applied.owner_rejections += 1;
        }
        next.observe_owner(&rejected.asset)?;
    }
    if next != saved {
        runtime.save(&next)?;
    }
    applied.changed = next != stored || applied.owner_rejections > 0;
    Ok((next, applied))
}

fn write_preview(
    directory: &crate::batch_runtime::ArtifactDirectory,
    snapshot: &PitchReviewSnapshot,
) -> Result<(), AssetError> {
    use crate::batch::html_escape;
    let mut html = String::from(
        "<!doctype html><html lang=\"ru\"><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self' file: data:; style-src 'unsafe-inline'\"><title>Проверка pitch accent</title><style>body{font-family:system-ui;margin:2rem}article{border:1px solid #999;padding:1rem;margin:1rem 0}img{max-width:100%;background:#fff}pre{white-space:pre-wrap;overflow-wrap:anywhere}</style><h1>Кандидаты pitch accent и неоднозначные записи JPDB</h1><p>Решения задаются только первой колонкой decisions.tsv этого снимка. Подтверждение просмотра не меняет доверие: публикуется только актуальный VERIFIED текущего валидатора.</p>",
    );
    let mut inline_budget = crate::review_snapshot::PREVIEW_INLINE_BUDGET;
    for row in &snapshot.rows {
        html.push_str(&format!(
            "<article><h2>{}</h2><pre>Тип: {}\nСтатус: {}\nПоколение: {}; ревизия: {}; попытка: {}",
            html_escape(&row.identity.key),
            row.kind.as_str(),
            status_name(row.status),
            row.generation,
            row.item_revision,
            row.attempt_index
        ));
        if let (Some(filename), Some(sha256)) = (&row.filename, &row.sha256) {
            html.push_str(&format!(
                "\nSHA-256: {sha256}\nВалидатор: {}</pre>",
                row.validation_status.map_or("-", SemanticStatus::as_str)
            ));
            let image = directory.read(filename)?;
            let source =
                crate::review_snapshot::preview_image_source(filename, &image, &mut inline_budget);
            html.push_str(&format!(
                "<img src=\"{source}\" alt=\"{}\">",
                html_escape(&row.identity.key)
            ));
            if let Some(llm) = &row.llm {
                html.push_str(&format!("<pre>{}</pre>", html_escape(&llm.summary())));
            }
        } else {
            html.push_str("</pre><ul>");
            for candidate in &row.vocabulary_candidates {
                html.push_str(&format!(
                    "<li>{} — {}</li>",
                    candidate.vocabulary_id,
                    html_escape(&candidate.detail_url)
                ));
            }
            html.push_str("</ul>");
        }
        html.push_str("</article>");
    }
    html.push_str("</html>");
    directory.write("index.html", html.as_bytes())
}

#[cfg(test)]
#[path = "pitch_review_decisions_tests.rs"]
mod tests;
