//! Возобновляемый пакет кандзи: получение по уровням, свидетельства и точные действия человека.
//!
//! Это состояние предметной области, а не источник канонического доверия.
//! Интегратор сначала проверяет байты владельцем ресурсов и публикует
//! семантическое решение или решение человека, затем фиксирует результат здесь.
//! Runtime-данные хранятся только под `.runtime/batches/`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub(crate) use crate::batch_runtime::{validate_batch_id, validate_hash};
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::kanji_domain::parse_kanji_character;
use crate::model::{
    AssetIdentity, DetectedFormat, SemanticDecision, SemanticStatus, ValidationEvidence,
    ValidationRecord, ValidatorIdentity,
};

#[cfg(test)]
pub(crate) use crate::batch_runtime::REFERENCED_BLOB_READS as CANDIDATE_FILE_READS;
pub use crate::batch_runtime::{
    MAX_RUNTIME_BLOB_BYTES, MAX_RUNTIME_STATE_BYTES, RuntimeBatchState, RuntimeBlobRef,
    SafeBatchRuntime,
};

pub const BATCH_SCHEMA_VERSION: u32 = 1;
pub const AGGREGATE_POLICY_VERSION: &str = "kanji-distinct-mean-v2";
pub const SINGLE_CANDIDATE_POLICY_VERSION: &str = "kanji-single-candidate-v1";
pub const MAX_ACQUISITION_ROUNDS: u32 = 5;
pub(crate) const MAX_HUMAN_REASON_BYTES: usize = 4096;
const MAX_REVIEW_HTML_BYTES: u64 = MAX_RUNTIME_STATE_BYTES;
const MAX_CANDIDATE_BYTES: u64 = crate::kanji_validator::MAX_MEDIA_BYTES as u64;
/// Пороги совпадают с текущими порогами f32 валидатора кандзи; версия правил закреплена в состоянии.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregatePolicy {
    pub version: String,
    pub validator: ValidatorIdentity,
    pub maximum_expected_distance: f64,
    pub minimum_margin: f64,
}

impl AggregatePolicy {
    pub fn single_candidate(validator: ValidatorIdentity) -> Self {
        Self {
            version: SINGLE_CANDIDATE_POLICY_VERSION.into(),
            ..Self::current(validator)
        }
    }

    pub fn current(validator: ValidatorIdentity) -> Self {
        let (maximum_expected_distance, minimum_margin) =
            crate::kanji_validator::registered_aggregate_thresholds();
        Self {
            version: AGGREGATE_POLICY_VERSION.into(),
            validator,
            maximum_expected_distance,
            minimum_margin,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricSummary {
    pub mean: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub count: u32,
}

impl MetricSummary {
    fn calculate(values: impl Iterator<Item = f64>) -> Option<Self> {
        let values: Vec<_> = values.collect();
        if values.is_empty() {
            return None;
        }
        Some(Self {
            mean: values.iter().sum::<f64>() / values.len() as f64,
            minimum: values.iter().copied().fold(f64::INFINITY, f64::min),
            maximum: values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            count: values.len() as u32,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KanjiMetrics {
    pub expected_distance: f64,
    pub margin: f64,
    pub nearest_other: Option<String>,
}

impl KanjiMetrics {
    /// Читает только фактические свидетельства текущего владельца проверки изображений.
    pub fn from_validation(record: &ValidationRecord) -> Option<Self> {
        record.evidence.iter().find_map(|evidence| {
            if evidence.kind != "pixel_reference_comparison" {
                return None;
            }
            let details = evidence.details.as_ref()?;
            let metrics = Self {
                expected_distance: details.get("expected_distance")?.as_f64()?,
                margin: details.get("nearest_margin")?.as_f64()?,
                nearest_other: details
                    .get("nearest_other")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned),
            };
            metrics.valid().then_some(metrics)
        })
    }

    fn valid(&self) -> bool {
        self.expected_distance.is_finite()
            && self.expected_distance >= 0.0
            && self.margin.is_finite()
    }
}

/// Точный кандидат. Ссылка относительна каталогу пакета и вычисляется из SHA-256.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchCandidate {
    pub sha256: String,
    pub storage_path: String,
    pub format: DetectedFormat,
    /// Текущие сохранённые байты прошли проверку целостности, формата и декодера.
    pub technically_valid: bool,
    pub automated: ValidationRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquisition: Option<Box<crate::yarxi::AcquisitionEvidence>>,
}

impl BatchCandidate {
    pub fn metrics(&self) -> Option<KanjiMetrics> {
        self.technically_valid
            .then(|| KanjiMetrics::from_validation(&self.automated))
            .flatten()
    }
}

/// Актуальные результаты технической и семантической проверки сохранённых байтов.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateRevalidation {
    pub automated: ValidationRecord,
    /// Рассчитывается заново по текущим байтам, формату и декодеру.
    pub technically_valid: bool,
}

/// Сохраняются все реальные результаты получения, включая сетевые сбои.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum BatchAttemptInput {
    Candidate { candidate: BatchCandidate },
    Failed { code: String, message: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchAttempt {
    pub index: u32,
    /// Повторное получение по запросу пользователя начинает поколение, сохраняя историю.
    pub generation: u32,
    pub round: u32,
    pub result: BatchAttemptInput,
    /// Повтор записывается для воспроизводимости, но не считается отдельным образцом.
    pub duplicate_sha256: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AggregateEvidence {
    pub policy_version: String,
    pub distinct_valid_hashes: Vec<String>,
    pub expected_distance: Option<MetricSummary>,
    pub margin: Option<MetricSummary>,
    pub accepted: bool,
    pub selected_sha256: Option<String>,
    pub selection_reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchItemStatus {
    Unresolved,
    AwaitingHuman,
    Rejected,
    Reacquire,
    AutoVerified,
    HumanVerified,
    ExistingVerified,
}

impl BatchItemStatus {
    /// Семантический результат; готовность к канонической публикации отдельно
    /// проверяет `BatchItem::is_ready`.
    pub fn semantic_resolved(self) -> bool {
        matches!(
            self,
            Self::AutoVerified | Self::HumanVerified | Self::ExistingVerified
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchTrustSource {
    Existing,
    Automated,
    Aggregate,
    Human,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanBatchAction {
    Confirm,
    Reject,
    /// Отказ без автоматического повторного получения.
    RejectAndHold,
    Reacquire,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanBatchDecision {
    pub identity: AssetIdentity,
    pub candidate_sha256: String,
    pub action: HumanBatchAction,
    /// Указывается сообщение пользователя или однозначная семантическая интерпретация.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatchItem {
    pub identity: AssetIdentity,
    pub status: BatchItemStatus,
    pub generation: u32,
    pub attempts: Vec<BatchAttempt>,
    pub aggregate: AggregateEvidence,
    pub current_sha256: Option<String>,
    pub published_sha256: Option<String>,
    pub existing_candidate: Option<BatchCandidate>,
    pub publication_source: Option<BatchTrustSource>,
    pub human_decisions: Vec<HumanBatchDecision>,
    /// Уже применённые точные отказы владельца: при возобновлении они не применяются повторно.
    #[serde(default)]
    pub observed_owner_rejections: Vec<HumanBatchDecision>,
    pub review_reason: Option<String>,
}

impl BatchItem {
    pub fn is_ready(&self) -> bool {
        self.status.semantic_resolved()
            && self.current_sha256.is_some()
            && self.published_sha256 == self.current_sha256
            && self.publication_source.is_some()
    }
}

/// Runtime-состояние не является манифестом и не должно попадать в коммит корпуса.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KanjiBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub policy: AggregatePolicy,
    pub revision: u64,
    pub items: Vec<BatchItem>,
}

/// Выбранные байты доступны через `BatchRuntime::read_candidate`, решение — через API владельца.
#[derive(Debug, Clone)]
pub struct AggregateResolution {
    pub identity: AssetIdentity,
    pub candidate: BatchCandidate,
    pub decision: SemanticDecision,
    pub validator: ValidatorIdentity,
}

impl KanjiBatch {
    pub fn new(
        batch_id: String,
        identities: Vec<AssetIdentity>,
        validator: ValidatorIdentity,
    ) -> Result<Self, AssetError> {
        validate_batch_id(&batch_id)?;
        ValidatorIdentity::new(validator.id.clone(), validator.version.clone()).map_err(invalid)?;
        let policy = AggregatePolicy::single_candidate(validator);
        let mut seen = BTreeSet::new();
        let mut items = Vec::new();
        for identity in identities {
            validate_identity(&identity)?;
            if !seen.insert(identity.clone()) {
                continue;
            }
            items.push(BatchItem {
                identity,
                status: BatchItemStatus::Unresolved,
                generation: 0,
                attempts: Vec::new(),
                aggregate: empty_aggregate(&policy),
                current_sha256: None,
                published_sha256: None,
                existing_candidate: None,
                publication_source: None,
                human_decisions: Vec::new(),
                observed_owner_rejections: Vec::new(),
                review_reason: None,
            });
        }
        if items.is_empty() {
            return Err(invalid("пакет должен содержать хотя бы один идентификатор"));
        }
        Ok(Self {
            schema_version: BATCH_SCHEMA_VERSION,
            batch_id,
            policy,
            revision: 0,
            items,
        })
    }

    /// Граница обхода не допускает повтор первого элемента до первого прохода
    /// по всем остальным. Вызывающая сторона сохраняет состояние после каждого
    /// результата, а не только после целого раунда.
    pub fn next_round(&self) -> Vec<AssetIdentity> {
        let unresolved = self.items.iter().filter(|item| {
            matches!(
                item.status,
                BatchItemStatus::Unresolved | BatchItemStatus::Reacquire
            )
        });
        let minimum = unresolved.clone().map(generation_attempt_count).min();
        let Some(minimum) = minimum else {
            return Vec::new();
        };
        if minimum >= MAX_ACQUISITION_ROUNDS {
            return Vec::new();
        }
        unresolved
            .filter(|item| generation_attempt_count(item) == minimum)
            .map(|item| item.identity.clone())
            .collect()
    }

    pub fn is_resolved(&self) -> bool {
        self.items.iter().all(BatchItem::is_ready)
    }

    pub fn review_queue(&self) -> Vec<&BatchItem> {
        self.items
            .iter()
            .filter(|item| item.status == BatchItemStatus::AwaitingHuman)
            .collect()
    }

    /// Вызывающая сторона задаёт существующее действующее доверие после
    /// `AssetStore::read_verified`.
    pub fn mark_existing_ready(
        &mut self,
        identity: &AssetIdentity,
        sha256: &str,
    ) -> Result<(), AssetError> {
        validate_hash(sha256)?;
        let item = self.item_mut(identity)?;
        if !item.human_decisions.is_empty() {
            return Err(invalid(
                "повторное использование после решения человека требует явного разрешения для точных байтов",
            ));
        }
        item.status = BatchItemStatus::ExistingVerified;
        item.current_sha256 = Some(sha256.into());
        item.published_sha256 = Some(sha256.into());
        item.publication_source = Some(BatchTrustSource::Existing);
        self.revision += 1;
        Ok(())
    }

    /// Сохраняет байты и свидетельства повторного использования канонического
    /// ресурса для последующей проверки точных байтов человеком. Это не попытка
    /// получения и не увеличивает число независимых образцов.
    pub fn retain_existing_candidate(
        &mut self,
        identity: &AssetIdentity,
        candidate: BatchCandidate,
    ) -> Result<(), AssetError> {
        validate_attempt_input(
            &BatchAttemptInput::Candidate {
                candidate: candidate.clone(),
            },
            &self.policy,
        )?;
        let item = self.item_mut(identity)?;
        if item.status != BatchItemStatus::ExistingVerified
            || item.current_sha256.as_deref() != Some(candidate.sha256.as_str())
        {
            return Err(invalid(
                "кандидат для проверки не соответствует повторно использованному каноническому SHA-256",
            ));
        }
        item.existing_candidate = Some(candidate);
        self.revision += 1;
        Ok(())
    }

    /// Вызывается после канонического коммита владельца. До этого точный
    /// кандидат уже сохранён, поэтому при возобновлении публикация повторяется
    /// без нового получения.
    pub fn mark_published_ready(
        &mut self,
        identity: &AssetIdentity,
        sha256: &str,
        source: BatchTrustSource,
    ) -> Result<(), AssetError> {
        validate_hash(sha256)?;
        let item = self.item_mut(identity)?;
        if item.current_sha256.as_deref() != Some(sha256) || !item.status.semantic_resolved() {
            return Err(invalid(
                "публикация относится к устаревшему или неразрешённому кандидату",
            ));
        }
        check_publication_source(item, source)?;
        item.published_sha256 = Some(sha256.into());
        item.publication_source = Some(source);
        self.revision += 1;
        Ok(())
    }

    /// Снимок владельца больше не подтверждает сохранённую готовность. Точный
    /// отказ владельца сохраняется отдельно и исключает этот SHA-256 из будущей
    /// автоматической агрегации.
    pub fn invalidate_owner_trust(
        &mut self,
        identity: &AssetIdentity,
        reason: String,
        owner_rejection: Option<HumanBatchDecision>,
    ) -> Result<(), AssetError> {
        if reason.trim().is_empty() || reason.len() > MAX_HUMAN_REASON_BYTES {
            return Err(invalid(
                "для сверки с владельцем требуется непустая ограниченная причина",
            ));
        }
        if let Some(decision) = &owner_rejection {
            validate_hash(&decision.candidate_sha256)?;
            if &decision.identity != identity
                || !matches!(
                    decision.action,
                    HumanBatchAction::Reject | HumanBatchAction::RejectAndHold
                )
                || decision.reason.trim().is_empty()
                || decision.reason.len() > MAX_HUMAN_REASON_BYTES
            {
                return Err(invalid(
                    "отказ владельца не является семантическим Reject для точного идентификатора и SHA-256",
                ));
            }
        }
        let policy = self.policy.clone();
        let item = self.item_mut(identity)?;
        if !item.status.semantic_resolved() {
            return Err(invalid(
                "для отзыва доверия владельца элемент должен быть ранее разрешён",
            ));
        }
        if let Some(decision) = owner_rejection {
            if !item.observed_owner_rejections.contains(&decision) {
                item.observed_owner_rejections.push(decision.clone());
            }
            if !item.human_decisions.contains(&decision) {
                item.human_decisions.push(decision);
            }
        }
        item.generation = item
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("превышен предел номера поколения"))?;
        item.status = BatchItemStatus::Reacquire;
        item.current_sha256 = None;
        item.published_sha256 = None;
        item.publication_source = None;
        item.aggregate = empty_aggregate(&policy);
        item.review_reason = Some(reason);
        self.revision += 1;
        Ok(())
    }

    /// Повторно запускает техническое получение после пяти сбоев без кандидата.
    /// Не принимает семантических решений и не заменяет проверку точного кандидата.
    pub fn retry_acquisition(
        &mut self,
        identity: &AssetIdentity,
        reason: String,
    ) -> Result<(), AssetError> {
        if reason.trim().is_empty() || reason.len() > MAX_HUMAN_REASON_BYTES {
            return Err(invalid(
                "для повторного получения требуется непустая ограниченная причина",
            ));
        }
        let policy = self.policy.clone();
        let item = self.item_mut(identity)?;
        if !matches!(
            item.status,
            BatchItemStatus::AwaitingHuman | BatchItemStatus::Rejected
        ) || item.current_sha256.is_some()
        {
            return Err(invalid(
                "повторное получение без кандидата разрешено после технических сбоев или явного отказа",
            ));
        }
        item.generation = item
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("превышен предел номера поколения"))?;
        item.status = BatchItemStatus::Reacquire;
        item.review_reason = Some(reason);
        item.aggregate = empty_aggregate(&policy);
        self.revision += 1;
        Ok(())
    }

    pub fn record_attempt(
        &mut self,
        identity: &AssetIdentity,
        result: BatchAttemptInput,
    ) -> Result<(), AssetError> {
        if !self.next_round().contains(identity) {
            return Err(invalid(
                "идентификатор отсутствует в текущей границе обхода по уровням",
            ));
        }
        validate_attempt_input(&result, &self.policy)?;
        let policy = self.policy.clone();
        let item = self.item_mut(identity)?;
        let duplicate_sha256 = match &result {
            BatchAttemptInput::Candidate { candidate } => item.attempts.iter().any(|attempt| matches!(&attempt.result, BatchAttemptInput::Candidate { candidate: old } if old.sha256 == candidate.sha256)),
            BatchAttemptInput::Failed { .. } => false,
        };
        item.attempts.push(BatchAttempt {
            index: item.attempts.len() as u32 + 1,
            generation: item.generation,
            round: generation_attempt_count(item) + 1,
            result,
            duplicate_sha256,
        });
        refresh_item(item, &policy);
        self.revision += 1;
        Ok(())
    }

    /// Сохраняет точное намерение человека до подтверждения владельцем; Confirm
    /// становится готовым только после `mark_published_ready`. Байты повторно
    /// сверяются с SHA-256.
    pub fn decide(
        &mut self,
        decision: HumanBatchDecision,
        exact_current_bytes: &[u8],
    ) -> Result<(), AssetError> {
        validate_hash(&decision.candidate_sha256)?;
        if decision.reason.trim().is_empty() || decision.reason.len() > MAX_HUMAN_REASON_BYTES {
            return Err(invalid(
                "решение человека требует непустую ограниченную причину",
            ));
        }
        if sha256_hex(exact_current_bytes) != decision.candidate_sha256 {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "байты решения человека не совпадают с точным SHA-256",
            ));
        }
        let policy = self.policy.clone();
        let item = self.item_mut(&decision.identity)?;
        if item.current_sha256.as_deref() != Some(&decision.candidate_sha256) {
            return Err(invalid(
                "решение человека относится к устаревшему или чужому кандидату",
            ));
        }
        let repeated = item.human_decisions.last() == Some(&decision);
        if repeated && decision.action != HumanBatchAction::Confirm {
            return Ok(());
        }
        if decision.action == HumanBatchAction::Confirm {
            let candidate = find_candidate(item, &decision.candidate_sha256)
                .ok_or_else(|| invalid("байты или свидетельства кандидата отсутствуют"))?;
            if !candidate.technically_valid || candidate.automated.status == SemanticStatus::Corrupt
            {
                return Err(invalid(
                    "кандидат с технической ошибкой или статусом CORRUPT нельзя подтвердить вручную",
                ));
            }
            item.status = BatchItemStatus::HumanVerified;
            // Существующая или автоматическая публикация не заменяет явное
            // подтверждение владельца человеком. При возобновлении это намерение
            // должно пройти через publish_human.
            item.published_sha256 = None;
            item.publication_source = None;
            item.review_reason = None;
        } else {
            item.status = if decision.action == HumanBatchAction::RejectAndHold {
                BatchItemStatus::Rejected
            } else {
                BatchItemStatus::Reacquire
            };
            item.generation = item
                .generation
                .checked_add(1)
                .ok_or_else(|| invalid("превышен предел номера поколения"))?;
            item.review_reason = Some(decision.reason.clone());
            item.current_sha256 = None;
            item.published_sha256 = None;
            item.publication_source = None;
            item.aggregate = empty_aggregate(&policy);
        }
        if !repeated {
            item.human_decisions.push(decision);
        }
        self.revision += 1;
        Ok(())
    }

    /// Точный хеш и версионированные свидетельства агрегации для публикации
    /// владельцем. Индивидуальный статус классификатора не переписывается.
    pub fn aggregate_decision(
        &self,
        identity: &AssetIdentity,
    ) -> Result<Option<AggregateResolution>, AssetError> {
        let item = self
            .items
            .iter()
            .find(|item| &item.identity == identity)
            .ok_or_else(|| invalid("идентификатор отсутствует в пакете"))?;
        if item.status != BatchItemStatus::AutoVerified || !item.aggregate.accepted {
            return Ok(None);
        }
        let hash = item
            .current_sha256
            .as_deref()
            .ok_or_else(|| invalid("у агрегата отсутствует точный выбранный SHA-256"))?;
        let candidate = find_candidate(item, hash)
            .ok_or_else(|| invalid("кандидат агрегата отсутствует"))?
            .clone();
        let evidence = ValidationEvidence {
            kind: "kanji_batch_aggregate".into(),
            summary: "Средние метрики разных технически допустимых SHA-256 без REJECTED удовлетворяют неизменённым порогам проверки изображений, а выбранный кандидат проходит порог отступа; точный канонический кандидат выбран детерминированно".into(),
            details: Some(serde_json::json!({
                "batch_id": self.batch_id,
                "identity": identity,
                "content_sha256": hash,
                "aggregate": item.aggregate,
                "policy": self.policy,
                "generation": item.generation,
                // Агрегат не подменяет индивидуальный автоматический статус
                // выбранного кандидата, поэтому он сохраняется рядом.
                "selected_individual_status": candidate.automated.status,
                "selected_individual_validator": candidate.automated.validator,
                "selected_individual_evidence": candidate.automated.evidence,
            })),
        };
        Ok(Some(AggregateResolution {
            identity: identity.clone(),
            candidate,
            decision: SemanticDecision::new(SemanticStatus::Verified, vec![evidence]),
            validator: self.policy.validator.clone(),
        }))
    }

    fn item_mut(&mut self, identity: &AssetIdentity) -> Result<&mut BatchItem, AssetError> {
        self.items
            .iter_mut()
            .find(|item| &item.identity == identity)
            .ok_or_else(|| invalid("идентификатор отсутствует в пакете"))
    }

    /// Явная смена политики на тех же сохранённых байтах, без сетевых попыток.
    /// Вызывающий слой повторно сверяет SHA и отдельно сообщает результат декодирования.
    pub fn revalidate_candidates(
        &mut self,
        validator: ValidatorIdentity,
        mut validate: impl FnMut(
            &AssetIdentity,
            &BatchCandidate,
        ) -> Result<CandidateRevalidation, AssetError>,
    ) -> Result<(), AssetError> {
        self.validate()?;
        let mut next = self.clone();
        next.policy = AggregatePolicy::single_candidate(validator);
        for item in &mut next.items {
            let was_human_verified = item.status == BatchItemStatus::HumanVerified;
            let human_confirmation = item
                .human_decisions
                .last()
                .filter(|decision| decision.action == HumanBatchAction::Confirm)
                .cloned();
            let published_sha256 = item.published_sha256.clone();
            let publication_source = item.publication_source;
            for attempt in &mut item.attempts {
                if let BatchAttemptInput::Candidate { candidate } = &mut attempt.result {
                    let result = validate(&item.identity, candidate)?;
                    if result.automated.content_sha256 != candidate.sha256 {
                        return Err(AssetError::new(
                            ErrorCode::IntegrityMismatch,
                            "новая проверка не привязана к точному SHA-256 кандидата",
                        ));
                    }
                    candidate.automated = result.automated;
                    candidate.technically_valid = result.technically_valid;
                }
            }
            if let Some(candidate) = &mut item.existing_candidate {
                let result = validate(&item.identity, candidate)?;
                if result.automated.content_sha256 != candidate.sha256 {
                    return Err(AssetError::new(
                        ErrorCode::IntegrityMismatch,
                        "новая проверка не привязана к точному SHA-256 кандидата",
                    ));
                }
                candidate.automated = result.automated;
                candidate.technically_valid = result.technically_valid;
            }

            if item.status == BatchItemStatus::ExistingVerified {
                // Готовность этого состояния принадлежит каноническому store;
                // CLI после миграции сверит её с новым снимком владельца.
                continue;
            }

            let keeps_human_confirmation = was_human_verified
                && human_confirmation.as_ref().is_some_and(|decision| {
                    item.current_sha256.as_deref() == Some(decision.candidate_sha256.as_str())
                        && find_candidate(item, &decision.candidate_sha256).is_some_and(
                            |candidate| {
                                candidate.technically_valid
                                    && candidate.automated.status != SemanticStatus::Corrupt
                            },
                        )
                });
            if keeps_human_confirmation {
                item.review_reason = None;
                if human_confirmation.as_ref().is_some_and(|decision| {
                    published_sha256.as_deref() != Some(decision.candidate_sha256.as_str())
                        || publication_source != Some(BatchTrustSource::Human)
                }) {
                    item.published_sha256 = None;
                    item.publication_source = None;
                }
                continue;
            }

            item.published_sha256 = None;
            item.publication_source = None;
            refresh_item(item, &next.policy);
        }
        next.revision += 1;
        next.validate()?;
        *self = next;
        Ok(())
    }

    /// Отказывает при подмене вычисленных свидетельств, неизвестной политике,
    /// версии схемы или ссылках на данные.
    pub fn validate(&self) -> Result<(), AssetError> {
        validate_batch_id(&self.batch_id)?;
        if self.schema_version != BATCH_SCHEMA_VERSION {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "версия состояния пакета не поддерживается",
            ));
        }
        ValidatorIdentity::new(
            self.policy.validator.id.clone(),
            self.policy.validator.version.clone(),
        )
        .map_err(invalid)?;
        if self.policy != AggregatePolicy::current(self.policy.validator.clone())
            && self.policy != AggregatePolicy::single_candidate(self.policy.validator.clone())
        {
            return Err(invalid(
                "политика агрегации и пороги не соответствуют зарегистрированной версии",
            ));
        }
        if self.items.is_empty() {
            return Err(invalid("состояние пакета не содержит элементов"));
        }
        let mut identities = BTreeSet::new();
        for item in &self.items {
            validate_identity(&item.identity)?;
            if let Some(candidate) = &item.existing_candidate {
                validate_attempt_input(
                    &BatchAttemptInput::Candidate {
                        candidate: candidate.clone(),
                    },
                    &self.policy,
                )?;
            }
            if !identities.insert(&item.identity) {
                return Err(invalid("повторяющийся запрошенный идентификатор"));
            }
            let mut hashes = BTreeSet::new();
            let mut generations = std::collections::BTreeMap::<u32, u32>::new();
            for (index, attempt) in item.attempts.iter().enumerate() {
                validate_attempt_input(&attempt.result, &self.policy)?;
                let count = generations.entry(attempt.generation).or_default();
                *count += 1;
                if attempt.index != index as u32 + 1
                    || attempt.generation > item.generation
                    || attempt.round != *count
                    || *count > MAX_ACQUISITION_ROUNDS
                {
                    return Err(invalid(
                        "нарушены ограничения номера попытки, раунда или поколения",
                    ));
                }
                let duplicate = match &attempt.result {
                    BatchAttemptInput::Candidate { candidate } => {
                        !hashes.insert(candidate.sha256.clone())
                    }
                    BatchAttemptInput::Failed { .. } => false,
                };
                if duplicate != attempt.duplicate_sha256 {
                    return Err(invalid(
                        "признак повтора не соответствует фактическим SHA-256",
                    ));
                }
            }
            for decision in item
                .human_decisions
                .iter()
                .chain(&item.observed_owner_rejections)
            {
                if decision.identity != item.identity
                    || decision.reason.trim().is_empty()
                    || decision.reason.len() > MAX_HUMAN_REASON_BYTES
                {
                    return Err(invalid(
                        "недопустимы идентификатор или причина решения человека",
                    ));
                }
                validate_hash(&decision.candidate_sha256)?;
            }
            if item.review_reason.as_ref().is_some_and(|reason| {
                reason.trim().is_empty() || reason.len() > MAX_HUMAN_REASON_BYTES
            }) {
                return Err(invalid(format!(
                    "причина проверки должна быть непустой и не длиннее {MAX_HUMAN_REASON_BYTES} байт"
                )));
            }
            if item.observed_owner_rejections.iter().any(|decision| {
                !matches!(
                    decision.action,
                    HumanBatchAction::Reject | HumanBatchAction::RejectAndHold
                ) || !item.human_decisions.contains(decision)
            }) {
                return Err(invalid(
                    "зафиксированный отказ владельца не имеет точного семантического свидетельства Reject",
                ));
            }
            if let Some(hash) = &item.current_sha256 {
                validate_hash(hash)?;
                if item.status != BatchItemStatus::ExistingVerified
                    && find_candidate(item, hash).is_none()
                {
                    return Err(invalid(
                        "для текущего кандидата отсутствуют сохранённые свидетельства",
                    ));
                }
            }
            if item.published_sha256.is_some() != item.publication_source.is_some() {
                return Err(invalid("SHA-256 и источник публикации не согласованы"));
            }
            if let Some(source) = item.publication_source {
                if !item.is_ready() {
                    return Err(invalid(
                        "опубликованное состояние относится к устаревшему или неразрешённому кандидату",
                    ));
                }
                check_publication_source(item, source)?;
            }
            if item.status == BatchItemStatus::ExistingVerified {
                if item.current_sha256.is_none() || !item.human_decisions.is_empty() {
                    return Err(invalid(
                        "недопустимое состояние ранее подтверждённого ресурса",
                    ));
                }
            } else if item.status == BatchItemStatus::HumanVerified {
                let decision = item.human_decisions.last().ok_or_else(|| {
                    invalid("подтверждённому человеком состоянию не хватает решения")
                })?;
                let candidate = item
                    .current_sha256
                    .as_deref()
                    .and_then(|hash| find_candidate(item, hash))
                    .ok_or_else(|| {
                        invalid("подтверждённому человеком состоянию не хватает кандидата")
                    })?;
                if decision.action != HumanBatchAction::Confirm
                    || decision.candidate_sha256 != candidate.sha256
                    || !candidate.technically_valid
                    || candidate.automated.status == SemanticStatus::Corrupt
                {
                    return Err(invalid(
                        "подтверждение человека не связано с допустимыми точными байтами",
                    ));
                }
            } else {
                let mut derived = item.clone();
                refresh_item(&mut derived, &self.policy);
                let aggregate_matches = item.aggregate == derived.aggregate
                    || (self.policy.version == SINGLE_CANDIDATE_POLICY_VERSION
                        && generation_attempt_count(item) == 0
                        && item.aggregate
                            == empty_aggregate(&AggregatePolicy::current(
                                self.policy.validator.clone(),
                            ))
                        && derived.aggregate == empty_aggregate(&self.policy));
                if !aggregate_matches
                    || item.current_sha256 != derived.current_sha256
                    || item.status != derived.status
                {
                    return Err(invalid(
                        "вычисленные статус, агрегат или свидетельства кандидата в пакете подменены",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl RuntimeBatchState for KanjiBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn validate(&self) -> Result<(), AssetError> {
        KanjiBatch::validate(self)
    }

    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        let mut blobs = Vec::new();
        for item in &self.items {
            if let Some(candidate) = &item.existing_candidate {
                blobs.push(RuntimeBlobRef {
                    sha256: candidate.sha256.clone(),
                    storage_path: candidate.storage_path.clone(),
                });
            }
            for attempt in &item.attempts {
                if let BatchAttemptInput::Candidate { candidate } = &attempt.result {
                    blobs.push(RuntimeBlobRef {
                        sha256: candidate.sha256.clone(),
                        storage_path: candidate.storage_path.clone(),
                    });
                }
            }
        }
        blobs
    }

    fn maximum_blob_bytes(&self) -> u64 {
        MAX_CANDIDATE_BYTES
    }

    fn blob_validation_context(&self, blob: &RuntimeBlobRef) -> Option<&str> {
        Some(kanji_candidate_extension_from_path(blob))
    }

    fn validate_blob_bytes(&self, blob: &RuntimeBlobRef, bytes: &[u8]) -> Result<(), AssetError> {
        let expected_format = match kanji_candidate_extension_from_path(blob) {
            "gif" => DetectedFormat::Gif,
            "png" => DetectedFormat::Png,
            _ => {
                return Err(invalid(
                    "кандидат пакета кандзи должен иметь формат GIF или PNG",
                ));
            }
        };
        if DetectedFormat::from_signature(bytes) != expected_format {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "байты кандидата не соответствуют объявленному формату",
            ));
        }
        Ok(())
    }
}

fn kanji_candidate_extension_from_path(blob: &RuntimeBlobRef) -> &'static str {
    match blob
        .storage_path
        .rsplit_once('.')
        .map(|(_, extension)| extension)
    {
        Some("gif") => "gif",
        Some("png") => "png",
        _ => "unsupported",
    }
}

fn check_publication_source(item: &BatchItem, source: BatchTrustSource) -> Result<(), AssetError> {
    let allowed = match source {
        BatchTrustSource::Existing => item.status == BatchItemStatus::ExistingVerified,
        BatchTrustSource::Human => item.status == BatchItemStatus::HumanVerified,
        BatchTrustSource::Aggregate => {
            item.status == BatchItemStatus::AutoVerified && item.aggregate.accepted
        }
        BatchTrustSource::Automated => {
            item.status == BatchItemStatus::AutoVerified
                && item
                    .current_sha256
                    .as_deref()
                    .and_then(|hash| find_candidate(item, hash))
                    .is_some_and(|candidate| {
                        candidate.technically_valid
                            && candidate.automated.status == SemanticStatus::Verified
                    })
        }
    };
    if allowed {
        Ok(())
    } else {
        Err(invalid(
            "источник доверия публикации не подтверждён свидетельствами предметной области",
        ))
    }
}

fn empty_aggregate(policy: &AggregatePolicy) -> AggregateEvidence {
    AggregateEvidence {
        policy_version: policy.version.clone(),
        distinct_valid_hashes: Vec::new(),
        expected_distance: None,
        margin: None,
        accepted: false,
        selected_sha256: None,
        selection_reason: "нет разных технически допустимых семантических образцов".into(),
    }
}

fn generation_attempt_count(item: &BatchItem) -> u32 {
    item.attempts
        .iter()
        .filter(|attempt| attempt.generation == item.generation)
        .count() as u32
}

fn find_candidate<'a>(item: &'a BatchItem, hash: &str) -> Option<&'a BatchCandidate> {
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

fn refresh_item(item: &mut BatchItem, policy: &AggregatePolicy) {
    if generation_attempt_count(item) == 0 {
        item.aggregate = empty_aggregate(policy);
        item.current_sha256 = None;
        item.status = if item.status == BatchItemStatus::Rejected
            && item
                .human_decisions
                .last()
                .is_some_and(|decision| decision.action == HumanBatchAction::RejectAndHold)
        {
            BatchItemStatus::Rejected
        } else if item.generation > 0 {
            BatchItemStatus::Reacquire
        } else {
            BatchItemStatus::Unresolved
        };
        return;
    }
    let rejected: BTreeSet<_> = item
        .human_decisions
        .iter()
        .filter(|decision| {
            matches!(
                decision.action,
                HumanBatchAction::Reject | HumanBatchAction::RejectAndHold
            )
        })
        .map(|decision| decision.candidate_sha256.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    let mut samples: Vec<(&BatchCandidate, KanjiMetrics)> = Vec::new();
    let mut display_candidates = Vec::new();
    for attempt in item
        .attempts
        .iter()
        .filter(|attempt| attempt.generation == item.generation)
    {
        let BatchAttemptInput::Candidate { candidate } = &attempt.result else {
            continue;
        };
        if rejected.contains(candidate.sha256.as_str()) {
            continue;
        }
        display_candidates.push(candidate);
        // REJECTED означает, что другой эталон Unicode заметно ближе, то есть
        // изображение, вероятно, показывает другой символ. Это отрицательное
        // свидетельство: такой кандидат не участвует в среднем и не может быть
        // выбран каноническим.
        if matches!(
            candidate.automated.status,
            SemanticStatus::Verified | SemanticStatus::Uncertain
        ) && let Some(metrics) = candidate.metrics()
            && seen.insert(&candidate.sha256)
        {
            samples.push((candidate, metrics));
        }
    }
    samples.sort_by(|(left, lm), (right, rm)| {
        lm.expected_distance
            .total_cmp(&rm.expected_distance)
            .then_with(|| rm.margin.total_cmp(&lm.margin))
            .then_with(|| left.sha256.cmp(&right.sha256))
    });
    let expected_distance =
        MetricSummary::calculate(samples.iter().map(|(_, metrics)| metrics.expected_distance));
    let margin = MetricSummary::calculate(samples.iter().map(|(_, metrics)| metrics.margin));
    let selected_aggregate = samples.first();
    let accepted = policy.version == AGGREGATE_POLICY_VERSION
        && expected_distance
            .as_ref()
            .is_some_and(|metric| metric.mean <= policy.maximum_expected_distance)
        && margin
            .as_ref()
            .is_some_and(|metric| metric.mean >= policy.minimum_margin)
        && selected_aggregate.is_some_and(|(_, metrics)| metrics.margin >= policy.minimum_margin);
    // Отдельный VERIFIED уже является достаточным свидетельством; агрегат
    // может выбрать любой отличный допустимый образец только при положительном среднем.
    let individually_verified = display_candidates
        .iter()
        .copied()
        .filter(|candidate| {
            candidate.technically_valid && candidate.automated.status == SemanticStatus::Verified
        })
        .min_by(|left, right| left.sha256.cmp(&right.sha256));
    let selected = individually_verified
        .or_else(|| selected_aggregate.map(|(candidate, _)| *candidate))
        .or_else(|| {
            // Запасной выбор нужен только для проверки: если есть другой кандидат,
            // REJECTED не становится текущим, но полностью отклонённый элемент
            // остаётся видимым и доступным для решения человека.
            let rejected_rank = |candidate: &&BatchCandidate| {
                usize::from(candidate.automated.status == SemanticStatus::Rejected)
            };
            display_candidates.iter().copied().min_by(|left, right| {
                rejected_rank(left)
                    .cmp(&rejected_rank(right))
                    .then_with(|| left.sha256.cmp(&right.sha256))
            })
        });
    let selected_hash = selected.map(|candidate| candidate.sha256.clone());
    let selected_for_human_review = selected.is_some_and(|candidate| {
        candidate.technically_valid && candidate.automated.status != SemanticStatus::Corrupt
    });
    let mut distinct_valid_hashes: Vec<_> = samples
        .iter()
        .map(|(candidate, _)| candidate.sha256.clone())
        .collect();
    distinct_valid_hashes.sort();
    item.aggregate = AggregateEvidence {
        policy_version: policy.version.clone(), distinct_valid_hashes, expected_distance, margin,
        accepted, selected_sha256: selected_hash.clone(),
        selection_reason: if individually_verified.is_some() { "отдельный статус VERIFIED; при равенстве выбирается минимальный SHA-256" } else { "минимальная ожидаемая дистанция, максимальный отступ, затем минимальный SHA-256; агрегат принимается, только если выбранный образец также проходит порог отступа; учитываются только разные допустимые SHA" }.into(),
    };
    item.current_sha256 = selected_hash;
    if individually_verified.is_some() || accepted {
        item.status = BatchItemStatus::AutoVerified;
        item.review_reason = None;
    } else if (policy.version == SINGLE_CANDIDATE_POLICY_VERSION && selected_for_human_review)
        || generation_attempt_count(item) >= MAX_ACQUISITION_ROUNDS
    {
        item.status = BatchItemStatus::AwaitingHuman;
        item.review_reason = Some(
            if policy.version == SINGLE_CANDIDATE_POLICY_VERSION && selected_for_human_review {
                "кандидат сохранён; повтор той же картинки не добавляет независимого свидетельства; требуется решение человека".into()
            } else if samples.is_empty() {
                format!(
                    "достигнут предел числа попыток получения ({MAX_ACQUISITION_ROUNDS}) без пригодных независимых семантических образцов"
                )
            } else {
                format!(
                    "достигнут предел числа попыток получения ({MAX_ACQUISITION_ROUNDS}); среднее разных образцов не достигает неизменённых порогов проверки изображений"
                )
            },
        );
    } else {
        item.status = if item.generation > 0 {
            BatchItemStatus::Reacquire
        } else {
            BatchItemStatus::Unresolved
        };
    }
}

fn validate_attempt_input(
    result: &BatchAttemptInput,
    policy: &AggregatePolicy,
) -> Result<(), AssetError> {
    match result {
        BatchAttemptInput::Failed { code, message } => {
            if code.is_empty() || code.len() > 256 || message.len() > 4096 {
                return Err(invalid(
                    "свидетельство сбоя должно быть ограниченным и содержать код",
                ));
            }
        }
        BatchAttemptInput::Candidate { candidate } => {
            validate_hash(&candidate.sha256)?;
            if candidate.storage_path != candidate_path(&candidate.sha256, candidate.format)?
                || candidate.automated.content_sha256 != candidate.sha256
                || candidate.automated.validator != policy.validator
                || candidate.automated.evidence.is_empty()
            {
                return Err(invalid(
                    "путь кандидата, SHA-256, валидатор и свидетельства не совпадают",
                ));
            }
            let encoded =
                serde_json::to_vec(candidate).map_err(|error| invalid(error.to_string()))?;
            if encoded.len() > 128 * 1024 {
                return Err(invalid(
                    "автоматические свидетельства кандидата превышают 128 КиБ",
                ));
            }
        }
    }
    Ok(())
}

fn validate_identity(identity: &AssetIdentity) -> Result<(), AssetError> {
    identity.validate().map_err(invalid)?;
    if identity.namespace != "kanji" {
        return Err(invalid("пакет поддерживает только пространство имён kanji"));
    }
    parse_kanji_character(&identity.key).map_err(invalid)?;
    Ok(())
}

fn candidate_path(hash: &str, format: DetectedFormat) -> Result<String, AssetError> {
    validate_hash(hash)?;
    let extension = kanji_candidate_extension(format)
        .ok_or_else(|| invalid("кандидат пакета кандзи должен иметь формат GIF или PNG"))?;
    Ok(format!("candidates/{hash}.{extension}"))
}

fn kanji_candidate_extension(format: DetectedFormat) -> Option<&'static str> {
    match format {
        DetectedFormat::Gif | DetectedFormat::Png => {
            Some(crate::domain::extension_for_format(format))
        }
        _ => None,
    }
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

/// Kanji-адаптер над домен-независимым безопасным runtime.
#[derive(Debug)]
pub struct BatchRuntime {
    runtime: SafeBatchRuntime,
}

impl BatchRuntime {
    pub(crate) fn artifact_directory(
        &self,
        name: &str,
    ) -> Result<crate::batch_runtime::ArtifactDirectory, AssetError> {
        self.runtime.artifact_directory(name)
    }
    pub fn open(store_root: &Path, batch_id: &str) -> Result<Self, AssetError> {
        Ok(Self {
            runtime: SafeBatchRuntime::open(store_root, batch_id)?,
        })
    }

    pub fn batch_id(&self) -> &str {
        self.runtime.batch_id()
    }

    /// Освобождает runtime lock на время browser/network ожидания.
    pub fn release_lock(&mut self) -> Result<(), AssetError> {
        self.runtime.release_lock()
    }

    /// Повторно захватывает lock после внешнего ожидания.
    pub fn reacquire_lock(&mut self) -> Result<(), AssetError> {
        self.runtime.reacquire_lock()
    }

    /// Перечитывает состояние с кэшированием уже проверенных неизменяемых
    /// candidate blobs после повторного захвата lock.
    pub fn reload_cached(&mut self) -> Result<Option<KanjiBatch>, AssetError> {
        self.runtime.reload_cached()
    }

    pub fn load(&mut self) -> Result<Option<KanjiBatch>, AssetError> {
        self.runtime.load()
    }

    pub fn save(&mut self, state: &KanjiBatch) -> Result<(), AssetError> {
        self.runtime.save(state)
    }

    /// Локальная страница проверки с экранированным содержимым. Каждый отдельный
    /// SHA кандидата проверяется до добавления относительной ссылки на изображение;
    /// GIF сохраняет полную анимацию.
    /// Страница проверки не меняет машиночитаемый источник истины.
    pub fn write_review(&self, state: &KanjiBatch) -> Result<PathBuf, AssetError> {
        state.validate()?;
        if state.batch_id != self.batch_id() {
            return Err(invalid(
                "идентификатор пакета проверки не совпадает с каталогом runtime-данных",
            ));
        }
        let mut html = String::from(
            "<!doctype html><html lang=\"ru\"><meta charset=\"utf-8\"><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src 'self' file:; style-src 'unsafe-inline'\"><title>Проверка изображений кандзи</title><style>body{font-family:system-ui;max-width:1100px;margin:auto;padding:1rem}article{border:1px solid #999;padding:1rem;margin:1rem 0}img{max-width:300px;max-height:300px}pre{white-space:pre-wrap;overflow-wrap:anywhere}code{overflow-wrap:anywhere}</style><h1>Проверка изображений кандзи</h1>",
        );
        write!(
            &mut html,
            "<p>Пакет: <code>{}</code>; версия правил: <code>{}</code>; номер изменения: {}</p>",
            html_escape(&state.batch_id),
            html_escape(&state.policy.version),
            state.revision
        )
        .expect("запись в строку не может завершиться ошибкой");
        let suggestions = crate::kanji_ocr::saved_suggestions_or_empty(self);
        let mut review_queue = state.review_queue();
        review_queue.sort_by_key(|item| {
            suggestions
                .get(&item.identity)
                .filter(|ocr| item.current_sha256.as_ref() == Some(&ocr.source_sha256))
                .map_or(3, |ocr| ocr.group())
        });
        for item in review_queue {
            write!(
                &mut html,
                "<article><h2>{} · U+{:04X}</h2><p>Состояние: {}; попыток получения: {}; поколение: {}; разных допустимых SHA-256: {}</p>",
                html_escape(&item.identity.key),
                u32::from(parse_kanji_character(&item.identity.key).map_err(invalid)?),
                review_item_status(item.status),
                item.attempts.len(),
                item.generation,
                item.aggregate.distinct_valid_hashes.len()
            )
            .expect("запись в строку не может завершиться ошибкой");
            if let Some(hash) = &item.current_sha256 {
                let candidate = find_candidate(item, hash)
                    .ok_or_else(|| invalid("кандидат для проверки отсутствует"))?;
                self.read_candidate(candidate)?;
                if let Some(ocr) = suggestions
                    .get(&item.identity)
                    .filter(|ocr| &ocr.source_sha256 == hash)
                {
                    write!(
                        &mut html,
                        "<h3>Предложение OCR (не решение человека)</h3><pre>{}</pre>",
                        html_escape(&ocr.summary())
                    )
                    .expect("запись в строку");
                }
                write!(
                    &mut html,
                    "<img alt=\"{}\" src=\"{}\"><p>Точный SHA-256: <code>{}</code>; формат: {}; автоматический результат: {}; техническая проверка пройдена: {}</p>",
                    html_escape(&item.identity.key),
                    html_escape(&candidate.storage_path),
                    candidate.sha256,
                    review_format(candidate.format),
                    candidate.automated.status.as_str(),
                    candidate.technically_valid
                )
                .expect("запись в строку не может завершиться ошибкой");
                if let Some(metrics) = candidate.metrics() {
                    write!(
                        &mut html,
                        "<p>Ближайший конкурирующий Unicode: {}</p>",
                        html_escape(metrics.nearest_other.as_deref().unwrap_or("нет"))
                    )
                    .expect("запись в строку не может завершиться ошибкой");
                }
            } else {
                html.push_str("<p>Нет доступного текущего кандидата: получение завершилось техническим отказом. Нужно запустить получение повторно; семантическое подтверждение недоступно.</p>");
            }
            write!(
                &mut html,
                "<p>Причина проверки: {}</p><h3>Среднее, минимум, максимум и количество по агрегату</h3><pre>{}</pre><h3>Все попытки получения</h3>",
                html_escape(item.review_reason.as_deref().unwrap_or("")),
                html_escape(&serde_json::to_string_pretty(&item.aggregate).map_err(|error| invalid(error.to_string()))?)
            )
            .expect("запись в строку не может завершиться ошибкой");
            let mut shown = BTreeSet::new();
            if let Some(hash) = &item.current_sha256 {
                shown.insert(hash.clone());
            }
            for attempt in &item.attempts {
                if let BatchAttemptInput::Candidate { candidate } = &attempt.result
                    && shown.insert(candidate.sha256.clone())
                {
                    self.read_candidate(candidate)?;
                    write!(
                        &mut html,
                        "<figure><img alt=\"{}\" src=\"{}\"><figcaption>Другой кандидат SHA-256: <code>{}</code>; формат: {}; автоматический результат: {}; техническая проверка пройдена: {}</figcaption></figure>",
                        html_escape(&item.identity.key),
                        html_escape(&candidate.storage_path),
                        candidate.sha256,
                        review_format(candidate.format),
                        candidate.automated.status.as_str(),
                        candidate.technically_valid
                    )
                    .expect("запись в строку не может завершиться ошибкой");
                }
            }
            for attempt in &item.attempts {
                write!(
                    &mut html,
                    "<details><summary>Попытка {} · раунд {} · поколение {} · повтор SHA-256: {}</summary>",
                    attempt.index,
                    attempt.round,
                    attempt.generation,
                    attempt.duplicate_sha256
                )
                .expect("запись в строку не может завершиться ошибкой");
                let description = serde_json::to_string_pretty(&attempt.result)
                    .map_err(|error| invalid(error.to_string()))?;
                let bounded: String = description.chars().take(6000).collect();
                write!(&mut html, "<pre>{}</pre></details>", html_escape(&bounded))
                    .expect("запись в строку не может завершиться ошибкой");
            }
            html.push_str("</article>");
        }
        html.push_str("</html>");
        if html.len() as u64 > MAX_REVIEW_HTML_BYTES {
            return Err(invalid(
                "HTML-страница проверки превышает ограничение runtime-данных; разделите пакет",
            ));
        }
        self.runtime
            .write_artifact("review.html", html.as_bytes(), MAX_REVIEW_HTML_BYTES)
    }

    /// Байты сохраняются до записи состояния: прерывание оставит безопасный
    /// осиротевший файл, который не становится доверенным и не мешает возобновить прежнее состояние.
    pub fn persist_candidate(
        &self,
        bytes: &[u8],
        automated: ValidationRecord,
        technically_valid: bool,
    ) -> Result<BatchCandidate, AssetError> {
        if bytes.len() as u64 > MAX_CANDIDATE_BYTES {
            return Err(invalid("кандидат превышает ограничение размера"));
        }
        let hash = sha256_hex(bytes);
        if automated.content_sha256 != hash {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "запись автоматической проверки не соответствует байтам кандидата",
            ));
        }
        let format = DetectedFormat::from_signature(bytes);
        let storage_path = candidate_path(&hash, format)?;
        let extension =
            kanji_candidate_extension(format).expect("candidate_path принимает только GIF и PNG");
        let blob = self
            .runtime
            .persist_blob_with_limit(bytes, extension, MAX_CANDIDATE_BYTES)?;
        if blob.sha256 != hash || blob.storage_path != storage_path {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "blob runtime не совпал с идентификатором кандидата кандзи",
            ));
        }
        Ok(BatchCandidate {
            sha256: hash,
            storage_path,
            format,
            technically_valid,
            automated,
            acquisition: None,
        })
    }

    /// Доступ к байтам GIF/PNG для проверки или подтверждения владельцем.
    /// Проверяет путь, обычный файл, ограничения размера, формат и SHA-256
    /// перед возвращением фактических байтов.
    pub fn read_candidate(&self, candidate: &BatchCandidate) -> Result<Vec<u8>, AssetError> {
        let expected = candidate_path(&candidate.sha256, candidate.format)?;
        if candidate.storage_path != expected {
            return Err(AssetError::new(
                ErrorCode::PathTraversal,
                "ссылка кандидата не совпадает с относительным путём, вычисленным из SHA-256",
            ));
        }
        let bytes = self.runtime.read_blob_with_limit(
            &RuntimeBlobRef {
                sha256: candidate.sha256.clone(),
                storage_path: expected,
            },
            MAX_CANDIDATE_BYTES,
        )?;
        if sha256_hex(&bytes) != candidate.sha256
            || DetectedFormat::from_signature(&bytes) != candidate.format
            || bytes.len() as u64 > MAX_CANDIDATE_BYTES
        {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "байты, SHA-256 или формат кандидата изменились",
            ));
        }
        Ok(bytes)
    }
}

pub(crate) fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn review_item_status(status: BatchItemStatus) -> &'static str {
    match status {
        BatchItemStatus::Unresolved => "не обработан",
        BatchItemStatus::AwaitingHuman => "ожидает решения человека",
        BatchItemStatus::Rejected => "отклонён человеком; повтор только явно",
        BatchItemStatus::Reacquire => "ожидает повторного получения",
        BatchItemStatus::AutoVerified => "проверен автоматически",
        BatchItemStatus::HumanVerified => "подтверждён человеком",
        BatchItemStatus::ExistingVerified => "уже подтверждён владельцем",
    }
}

fn review_format(format: DetectedFormat) -> &'static str {
    match format {
        DetectedFormat::Gif => "GIF",
        DetectedFormat::Png => "PNG",
        DetectedFormat::Jpeg => "JPEG",
        DetectedFormat::Webp => "WebP",
        DetectedFormat::Bmp => "BMP",
        DetectedFormat::Tiff => "TIFF",
        DetectedFormat::Unknown => "неизвестный",
    }
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
