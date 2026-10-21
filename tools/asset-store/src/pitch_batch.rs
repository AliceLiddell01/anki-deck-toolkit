//! Возобновляемое предметное состояние пакетного получения pitch-accent.
//!
//! Файловые границы и неизменяемые файлы кандидатов обслуживает [`SafeBatchRuntime`].
//! Этот модуль хранит только результаты pitch-accent, точную идентичность/SHA,
//! свидетельства проверки, выбор словарной записи и намерение публикации.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::batch_runtime::{
    MAX_RUNTIME_STATE_BYTES, RuntimeBatchState, RuntimeBlobRef, SafeBatchRuntime,
    validate_batch_id, validate_hash,
};
use crate::browser_diagnostics::BrowserItemTimer;
use crate::domain::AssetDomainPolicy;
use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::jpdb::{
    JpdbPitchAcquired, JpdbPitchFailure, JpdbPitchOutcome, JpdbPitchRequest, JpdbPitchSelection,
    JpdbVocabularyCandidate,
};
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanDecision, LifecycleState, SemanticStatus,
    ValidationRecord, ValidatorIdentity,
};
use crate::pitch_accent::{
    PITCH_ACCENT_MAX_ASSET_BYTES, PitchAccentDomainMetadata, PitchAccentDomainPolicy,
    PitchAccentImageValidator, PitchAccentResolvedForm, jpdb_readings_equivalent,
    parse_jpdb_vocabulary_route,
};
use crate::validation::SemanticValidator;

/// Версия формата предметного состояния пакета.
pub const PITCH_BATCH_SCHEMA_VERSION: u32 = 3;
/// Версия внешней схемы нормализованного плана pitch.
pub const PITCH_PLAN_SCHEMA_VERSION: u32 = 1;
/// Максимальный размер причины точечного действия.
pub const MAX_PITCH_BATCH_REASON_BYTES: usize = 4096;

/// Неизменяемая идентичность исходного плана с версией и нормализованными данными.
/// Исходные запросы сохраняются вместе с хешем, чтобы состояние могло проверить
/// хеш и не путать исходный план с изменённым текущим запросом.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchPlanIdentity {
    pub schema_version: u32,
    pub requests: Vec<JpdbPitchRequest>,
    pub sha256: String,
}

impl PitchBatchPlanIdentity {
    pub fn new(schema_version: u32, requests: Vec<JpdbPitchRequest>) -> Result<Self, AssetError> {
        if schema_version != PITCH_PLAN_SCHEMA_VERSION {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "версия идентичности плана pitch-accent не поддерживается",
            ));
        }
        if requests.is_empty() {
            return Err(invalid(
                "идентичность плана должна содержать хотя бы один запрос",
            ));
        }
        let mut surfaces = BTreeSet::new();
        for request in &requests {
            validate_request(request)?;
            if !surfaces.insert(request.query.surface.as_str()) {
                return Err(identity_conflict(
                    "исходная идентичность плана должна содержать нормализованные уникальные написания",
                ));
            }
        }
        let sha256 = plan_identity_digest(schema_version, &requests)?;
        Ok(Self {
            schema_version,
            requests,
            sha256,
        })
    }

    fn validate(&self) -> Result<(), AssetError> {
        if self.schema_version != PITCH_PLAN_SCHEMA_VERSION {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "версия сохранённой идентичности плана pitch-accent не поддерживается",
            ));
        }
        if self.requests.is_empty() {
            return Err(invalid("сохранённая идентичность плана повреждена"));
        }
        let mut surfaces = BTreeSet::new();
        for request in &self.requests {
            validate_request(request)?;
            if !surfaces.insert(request.query.surface.as_str()) {
                return Err(identity_conflict(
                    "сохранённая идентичность плана содержит повторный `surface`",
                ));
            }
        }
        validate_hash(&self.sha256)?;
        if self.sha256 != plan_identity_digest(self.schema_version, &self.requests)? {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "хеш идентичности плана не совпадает с исходными нормализованными запросами",
            ));
        }
        Ok(())
    }
}

/// Состояние среды выполнения одного пакета. Порядок запросов сохраняется для воспроизводимого вывода CLI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchAccentBatch {
    pub schema_version: u32,
    pub batch_id: String,
    pub revision: u64,
    pub validator: ValidatorIdentity,
    /// Неизменяемый контракт плана; точечные действия меняют `item.request`, но не исходную идентичность.
    pub original_plan: PitchBatchPlanIdentity,
    /// Смена ожидаемых свидетельств проверки инвалидирует кэш blob-объектов `SafeBatchRuntime`.
    pub blob_validation_context_sha256: String,
    pub items: Vec<PitchBatchItem>,
}

/// Отдельное состояние для каждой точной `surface`; идентичность предметной области не содержит `reading` или ID словарной записи.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchItem {
    pub identity: AssetIdentity,
    pub request: JpdbPitchRequest,
    /// Точечный повтор или явный выбор при неоднозначности начинает новое поколение.
    pub generation: u32,
    /// CAS-счётчик элемента. Изменение соседней идентичности не делает получение устаревшим.
    pub item_revision: u64,
    pub attempts: Vec<PitchBatchAttempt>,
    pub current_candidate_sha256: Option<String>,
    /// Точная попытка, которой принадлежит `current_candidate_sha256`; SHA может
    /// повториться между поколениями.
    pub current_candidate_attempt_index: Option<u32>,
    pub rejected_candidates: Vec<PitchBatchRejection>,
    /// Канонический точный SHA, подтверждённый снимком владельца, если он уже был.
    pub canonical_sha256: Option<String>,
    /// SHA записи текущей идентичности из снимка владельца, без вывода о доверии к ней.
    pub owner_current_sha256: Option<String>,
    /// Запись владельца, которую можно повторно использовать как текущую `VERIFIED`.
    pub existing_verified_sha256: Option<String>,
    /// SHA, подтверждённый завершённой публикацией у владельца.
    pub published_sha256: Option<String>,
    /// При явном обновлении новый кандидат должен заменить именно этот SHA.
    pub refresh_expected_sha256: Option<String>,
    /// Завершённые намерения публикации, сохранённые при следующем точечном поколении.
    pub publication_history: Vec<PitchBatchPublication>,
    pub publication: Option<PitchBatchPublication>,
    pub owner_conflict: Option<PitchBatchConflict>,
    pub last_action_reason: Option<String>,
}

/// Один результат получения с полями `query` и `selection`, действовавшими именно для этой попытки.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchAttempt {
    pub index: u32,
    pub generation: u32,
    pub request: JpdbPitchRequest,
    pub outcome: PitchBatchOutcome,
}

/// Полный типизированный результат провайдера. Полученные байты заменяются ссылкой на файл среды выполнения по SHA.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum PitchBatchOutcome {
    Acquired {
        candidate: Box<PitchBatchCandidate>,
    },
    NoPitchAccentOnSource {
        evidence: crate::jpdb::JpdbPitchAbsenceEvidence,
    },
    AmbiguousVocabulary {
        surface: String,
        reading: Option<String>,
        candidates: Vec<JpdbVocabularyCandidate>,
    },
    VocabularyNotFound {
        surface: String,
        reading: Option<String>,
    },
    Failed {
        error: JpdbPitchFailure,
    },
}

/// Точные байты PNG и результаты текущего валидатора.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchCandidate {
    pub blob: RuntimeBlobRef,
    pub sha256: String,
    pub byte_length: u64,
    pub metadata: PitchAccentDomainMetadata,
    pub validation: ValidationRecord,
    /// Контекст входит в кэш среды выполнения для blob-объектов и меняется при изменении любого свидетельства.
    pub validation_context_sha256: String,
}

/// Отказ привязан только к точным байтам; следующий SHA не наследует отказ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchRejection {
    pub candidate_sha256: String,
    pub reason: String,
    pub generation: u32,
}

/// Сохраняемое намерение публикации у владельца. `expected_previous_sha256` задаёт CAS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchPublication {
    pub candidate_sha256: String,
    /// Точная попытка, чьи метаданные и свидетельства используются при публикации.
    pub candidate_attempt_index: u32,
    pub expected_previous_sha256: Option<String>,
    pub status: PitchBatchPublicationStatus,
    pub conflict_code: Option<String>,
    pub conflict_message: Option<String>,
}

/// Итог сверки публикации с полным снимком владельца.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchBatchPublicationStatus {
    Pending,
    Published,
    Conflict,
}

/// Сохранённая причина расхождения снимка владельца с состоянием пакета.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PitchBatchConflict {
    pub code: String,
    pub message: String,
}

/// UI/CLI-состояние элемента, вычисляемое по типизированной истории и намерению владельца.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchBatchItemStatus {
    Pending,
    AcquiredVerified,
    CandidateRejected,
    NoPitchAccentOnSource,
    AmbiguousVocabulary,
    VocabularyNotFound,
    TechnicalFailure,
    PublicationPending,
    Published,
    ExistingVerified,
    Conflict,
}

impl PitchBatchItemStatus {
    pub const fn is_resolved(self) -> bool {
        matches!(
            self,
            Self::NoPitchAccentOnSource | Self::Published | Self::ExistingVerified
        )
    }
}

/// Токен CAS, получаемый вызывающей стороной перед освобождением блокировки на время работы браузера.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PitchBatchItemToken {
    pub identity: AssetIdentity,
    pub generation: u32,
    pub item_revision: u64,
    pub request_fingerprint: String,
}

/// Полный неизменяемый снимок владельца, полученный только после успешной проверки целостности store.
#[derive(Debug, Clone, Default)]
pub struct PitchBatchOwnerSnapshot {
    records: BTreeMap<AssetIdentity, AssetRecord>,
}

impl PitchBatchOwnerSnapshot {
    /// Строит снимок из успешного результата `AssetStore::verify_integrity()`.
    /// Записи из `runtime` и `quarantine` сохраняются для CAS, но не считаются подтверждёнными ресурсами.
    pub fn from_records(records: Vec<AssetRecord>) -> Result<Self, AssetError> {
        let mut indexed = BTreeMap::new();
        for record in records {
            PitchAccentDomainPolicy.validate_identity(&record.identity)?;
            if indexed.insert(record.identity.clone(), record).is_some() {
                return Err(identity_conflict(
                    "снимок владельца содержит повторяющуюся идентичность pitch-accent",
                ));
            }
        }
        Ok(Self { records: indexed })
    }

    pub fn current(&self, identity: &AssetIdentity) -> Option<&AssetRecord> {
        self.records.get(identity)
    }
}

/// Адаптер сохранённого pitch-состояния над общей безопасной средой выполнения.
#[derive(Debug)]
pub struct PitchAccentBatchRuntime {
    runtime: SafeBatchRuntime,
}

impl PitchAccentBatch {
    /// Создаёт состояние; одинаковые точные дубли схлопываются, а разные запросы для
    /// одной канонической идентичности отвергаются явно.
    pub fn new(
        batch_id: impl Into<String>,
        requests: Vec<JpdbPitchRequest>,
        validator: ValidatorIdentity,
    ) -> Result<Self, AssetError> {
        Self::new_inner(batch_id.into(), requests, validator, None)
    }

    /// Создаёт пакет с неизменяемой идентичностью плана с версией схемы, откуда пришли запросы.
    pub fn new_with_plan_identity(
        batch_id: impl Into<String>,
        requests: Vec<JpdbPitchRequest>,
        validator: ValidatorIdentity,
        original_plan: PitchBatchPlanIdentity,
    ) -> Result<Self, AssetError> {
        original_plan.validate()?;
        Self::new_inner(batch_id.into(), requests, validator, Some(original_plan))
    }

    fn new_inner(
        batch_id: String,
        requests: Vec<JpdbPitchRequest>,
        validator: ValidatorIdentity,
        original_plan: Option<PitchBatchPlanIdentity>,
    ) -> Result<Self, AssetError> {
        validate_batch_id(&batch_id)?;
        if validator != PitchAccentImageValidator::validator_identity() {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "пакет pitch-accent требует текущую версию PitchAccentImageValidator",
            ));
        }

        let mut by_surface = BTreeMap::<String, JpdbPitchRequest>::new();
        let mut order = Vec::new();
        for request in requests {
            validate_request(&request)?;
            let surface = request.query.surface.clone();
            match by_surface.get(&surface) {
                Some(previous) if previous == &request => {}
                Some(_) => {
                    return Err(identity_conflict(format!(
                        "пакет содержит несовместимые `reading`/`selection` для канонического написания `surface` {surface:?}"
                    )));
                }
                None => {
                    order.push(surface.clone());
                    by_surface.insert(surface, request);
                }
            }
        }
        if order.is_empty() {
            return Err(invalid(
                "пакет pitch-accent должен содержать хотя бы один запрос",
            ));
        }
        let items = order
            .into_iter()
            .map(|surface| {
                let request = by_surface
                    .remove(&surface)
                    .expect("каждый сохранённый `surface` имеет запрос");
                Ok(PitchBatchItem {
                    identity: AssetIdentity::new("pitch_accent", surface)
                        .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?,
                    request,
                    generation: 0,
                    item_revision: 0,
                    attempts: Vec::new(),
                    current_candidate_sha256: None,
                    current_candidate_attempt_index: None,
                    rejected_candidates: Vec::new(),
                    canonical_sha256: None,
                    owner_current_sha256: None,
                    existing_verified_sha256: None,
                    published_sha256: None,
                    refresh_expected_sha256: None,
                    publication_history: Vec::new(),
                    publication: None,
                    owner_conflict: None,
                    last_action_reason: None,
                })
            })
            .collect::<Result<Vec<_>, AssetError>>()?;
        let normalized_requests = items
            .iter()
            .map(|item| item.request.clone())
            .collect::<Vec<_>>();
        let original_plan = match original_plan {
            Some(plan) => {
                if plan.requests != normalized_requests {
                    return Err(identity_conflict(
                        "исходная идентичность плана не совпадает с начальным порядком запросов пакета",
                    ));
                }
                plan
            }
            None => PitchBatchPlanIdentity::new(PITCH_PLAN_SCHEMA_VERSION, normalized_requests)?,
        };
        let batch = Self {
            schema_version: PITCH_BATCH_SCHEMA_VERSION,
            batch_id,
            revision: 0,
            validator,
            original_plan,
            blob_validation_context_sha256: String::new(),
            items,
        };
        let mut batch = batch;
        batch.refresh_blob_validation_context()?;
        batch.validate()?;
        Ok(batch)
    }

    pub fn item(&self, surface: &str) -> Option<&PitchBatchItem> {
        self.items.iter().find(|item| item.identity.key == surface)
    }

    pub fn item_mut(&mut self, surface: &str) -> Option<&mut PitchBatchItem> {
        self.items
            .iter_mut()
            .find(|item| item.identity.key == surface)
    }

    pub fn is_resolved(&self) -> bool {
        self.items.iter().all(|item| item.status().is_resolved())
    }

    /// Возвращает токен CAS только для элемента, ожидающего получения в текущем поколении.
    pub fn item_token(&self, surface: &str) -> Result<PitchBatchItemToken, AssetError> {
        let item = self
            .item(surface)
            .ok_or_else(|| invalid("`surface` отсутствует в пакете pitch-accent"))?;
        if item.status() != PitchBatchItemStatus::Pending {
            return Err(invalid("элемент пакета pitch-accent не ожидает получения"));
        }
        Ok(item.token())
    }

    /// Сохраняет любой SHA владельца как наблюдение CAS, не объявляя его доверенным.
    /// Непроверенная или устаревшая запись блокирует работу до явного `reacquire`.
    pub fn observe_owner(&mut self, record: &AssetRecord) -> Result<(), AssetError> {
        let validator = self.validator.clone();
        let item = self
            .item_mut(&record.identity.key)
            .ok_or_else(|| invalid("идентичность владельца отсутствует в пакете pitch-accent"))?;
        if record.identity.namespace != "pitch_accent" || item.identity != record.identity {
            return Err(identity_conflict(
                "запись владельца не совпадает с идентичностью пакета pitch-accent",
            ));
        }
        validate_hash(&record.sha256)?;
        let before = owner_state(item);
        item.reconcile_owner_record(Some(record), &validator)?;
        let after = owner_state(item);
        if before == after {
            return Ok(());
        }
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер изменения элемента"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Ставит в очередь только эту идентичность для нового запроса к провайдеру.
    /// Точный наблюдавшийся SHA владельца становится новым CAS-основанием публикации.
    pub fn reacquire(&mut self, surface: &str, reason: String) -> Result<(), AssetError> {
        validate_reason(&reason)?;
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("`surface` отсутствует в пакете pitch-accent"))?;
        if item.owner_conflict.as_ref().is_some_and(|conflict| {
            conflict.code != "owner_not_current_verified"
                && conflict.code != "owner_rejected"
                && conflict.code != "refresh_owner_drift"
        }) || item
            .publication
            .as_ref()
            .is_some_and(|intent| intent.status == PitchBatchPublicationStatus::Pending)
        {
            return Err(invalid(
                "сначала разрешите конфликт владельца или завершите намерение публикации",
            ));
        }
        item.generation = item
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер поколения"))?;
        item.refresh_expected_sha256 = item.owner_current_sha256.clone();
        item.current_candidate_sha256 = None;
        item.current_candidate_attempt_index = None;
        item.owner_conflict = None;
        item.existing_verified_sha256 = None;
        item.archive_publication()?;
        item.last_action_reason = Some(reason);
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер изменения элемента"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Выбирает точный кандидат из последнего списка неоднозначных результатов.
    /// Следующий запуск провайдера обязан выполнить новый поиск JPDB.
    pub fn select_candidate(
        &mut self,
        surface: &str,
        vocabulary_id: u64,
        detail_url: impl Into<String>,
    ) -> Result<(), AssetError> {
        let detail_url = detail_url.into();
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("`surface` отсутствует в пакете pitch-accent"))?;
        if item.owner_conflict.as_ref().is_some_and(|conflict| {
            conflict.code != "owner_not_current_verified" && conflict.code != "owner_rejected"
        }) || item
            .publication
            .as_ref()
            .is_some_and(|intent| intent.status == PitchBatchPublicationStatus::Pending)
        {
            return Err(invalid(
                "сначала разрешите несовпадение идентичности владельца или намерения публикации",
            ));
        }
        let Some(PitchBatchOutcome::AmbiguousVocabulary { candidates, .. }) =
            item.current_outcome()
        else {
            return Err(invalid(
                "элемент не ожидает выбора неоднозначной словарной записи",
            ));
        };
        let selection = JpdbPitchSelection::new(vocabulary_id, detail_url.clone())
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        let selected_route = parse_jpdb_vocabulary_route(&selection.detail_url)
            .map_err(|_| invalid("маршрут словарной записи выбора невалиден"))?;
        let matches = candidates.iter().any(|candidate| {
            candidate.vocabulary_id == vocabulary_id
                && parse_jpdb_vocabulary_route(&candidate.detail_url)
                    .is_ok_and(|route| route == selected_route)
        });
        if !matches {
            return Err(identity_conflict(
                "выбранные ID словарной записи и маршрут словарной записи отсутствуют в последнем списке неоднозначных результатов",
            ));
        }
        item.request.selection = Some(selection);
        // Канонический ресурс с другим выбором словарной записи не считается доверенным для этого запроса.
        item.canonical_sha256 = None;
        item.generation = item
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер поколения"))?;
        item.current_candidate_sha256 = None;
        item.current_candidate_attempt_index = None;
        item.refresh_expected_sha256 = item.owner_current_sha256.clone();
        item.owner_conflict = None;
        item.existing_verified_sha256 = None;
        item.archive_publication()?;
        item.last_action_reason = Some("explicit_vocabulary_selection".into());
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер изменения элемента"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Запускает новое поколение только для временных сбоев провайдера.
    pub fn retry(&mut self, surface: &str, reason: String) -> Result<(), AssetError> {
        validate_reason(&reason)?;
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("`surface` отсутствует в пакете pitch-accent"))?;
        item.retry_failure(reason)?;
        self.bump_revision()?;
        self.validate()
    }

    /// Переводит в новое поколение все текущие устранимые технические сбои пакета.
    ///
    /// Возвращает словоформы, у которых поколение действительно сменилось, в порядке
    /// элементов пакета. Пустой список — допустимый результат: в пакете уже нет
    /// устранимых технических сбоев, и это не ошибка.
    ///
    /// Пакет меняется целиком или не меняется вовсе: промежуточное состояние, в
    /// котором часть сбоев уже переведена в новое поколение, не становится
    /// наблюдаемым даже при отказе на одном из элементов.
    pub fn retry_retryable_failures(&mut self, reason: String) -> Result<Vec<String>, AssetError> {
        validate_reason(&reason)?;
        let mut candidate = self.clone();
        let mut surfaces = Vec::new();
        for item in &mut candidate.items {
            if item.ensure_retryable_failure().is_ok() {
                item.apply_retry_failure(reason.clone())?;
                candidate.revision = candidate
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("превышен номер изменения пакета pitch-accent"))?;
                surfaces.push(item.identity.key.clone());
            }
        }
        if surfaces.is_empty() {
            return Ok(Vec::new());
        }
        candidate.validate()?;
        *self = candidate;
        Ok(surfaces)
    }

    /// Перечисляет словоформы, текущий результат которых — устранимый технический сбой.
    ///
    /// Отбор совпадает с условиями точечного `retry`: сохранённый технический сбой
    /// без конфликта владельца и без незавершённого намерения публикации. Наличие
    /// подходящей VERIFIED-записи владельца не отменяет явный повтор этого сбоя.
    pub fn retryable_failure_surfaces(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|item| item.ensure_retryable_failure().is_ok())
            .map(|item| item.identity.key.clone())
            .collect()
    }

    /// Отмечает ровно один SHA кандидата как отклонённый владельцем.
    pub fn reject_candidate(
        &mut self,
        surface: &str,
        candidate_sha256: &str,
        reason: String,
    ) -> Result<(), AssetError> {
        validate_hash(candidate_sha256)?;
        validate_reason(&reason)?;
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("`surface` отсутствует в пакете pitch-accent"))?;
        if item
            .publication
            .as_ref()
            .is_some_and(|intent| intent.status == PitchBatchPublicationStatus::Pending)
        {
            return Err(invalid(
                "нельзя отклонить кандидата при незавершённом намерении публикации",
            ));
        }
        if item.candidate(candidate_sha256).is_none() {
            return Err(invalid("точный SHA кандидата отсутствует в истории пакета"));
        }
        if item
            .rejected_candidates
            .iter()
            .any(|rejection| rejection.candidate_sha256 == candidate_sha256)
        {
            return Ok(());
        }
        if item
            .publication
            .as_ref()
            .is_some_and(|publication| publication.candidate_sha256 == candidate_sha256)
        {
            item.archive_publication()?;
        }
        item.rejected_candidates.push(PitchBatchRejection {
            candidate_sha256: candidate_sha256.into(),
            reason,
            generation: item.generation,
        });
        if item.current_candidate_sha256.as_deref() == Some(candidate_sha256) {
            item.current_candidate_sha256 = None;
            item.current_candidate_attempt_index = None;
        }
        if item.owner_current_sha256.as_deref() == Some(candidate_sha256) {
            item.canonical_sha256 = None;
            item.existing_verified_sha256 = None;
            item.owner_conflict = Some(PitchBatchConflict {
                code: "owner_rejected".into(),
                message: "текущая запись владельца отклонена для этого точного SHA".into(),
            });
        }
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер изменения элемента"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Сохраняет намерение публикации до побочного эффекта у владельца.
    /// Не принимает `REJECTED`, `CORRUPT`, неполные или устаревшие свидетельства validator.
    pub fn begin_publication(
        &mut self,
        surface: &str,
        candidate_sha256: &str,
        expected_previous_sha256: Option<String>,
    ) -> Result<(), AssetError> {
        validate_hash(candidate_sha256)?;
        if let Some(expected) = &expected_previous_sha256 {
            validate_hash(expected)?;
        }
        let validator = self.validator.clone();
        let item = self
            .item_mut(surface)
            .ok_or_else(|| invalid("`surface` отсутствует в пакете pitch-accent"))?;
        if item.current_candidate_sha256.as_deref() != Some(candidate_sha256) {
            return Err(identity_conflict(
                "намерение публикации относится не к текущему точному кандидату",
            ));
        }
        let candidate_attempt_index = item
            .current_candidate_attempt_index
            .ok_or_else(|| invalid("попытка текущего кандидата отсутствует"))?;
        let candidate = item
            .candidate_at(candidate_attempt_index, candidate_sha256)
            .ok_or_else(|| invalid("свидетельства текущего кандидата отсутствуют"))?;
        if candidate.validation.status != SemanticStatus::Verified
            || candidate.validation.validator != validator
            || candidate.validation.content_sha256 != candidate.sha256
        {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "публикация разрешена только для текущих свидетельств VERIFIED",
            ));
        }
        if item
            .rejected_candidates
            .iter()
            .any(|rejection| rejection.candidate_sha256 == candidate_sha256)
        {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "точный SHA кандидата был ранее отклонён",
            ));
        }
        if item.owner_current_sha256 != expected_previous_sha256
            || item.refresh_expected_sha256 != expected_previous_sha256
        {
            return Err(identity_conflict(
                "CAS публикации не совпадает с текущим снимком владельца",
            ));
        }
        let requested = PitchBatchPublication {
            candidate_sha256: candidate_sha256.into(),
            candidate_attempt_index,
            expected_previous_sha256,
            status: PitchBatchPublicationStatus::Pending,
            conflict_code: None,
            conflict_message: None,
        };
        if let Some(existing) = &item.publication {
            if existing.candidate_sha256 == requested.candidate_sha256
                && existing.candidate_attempt_index == requested.candidate_attempt_index
                && existing.expected_previous_sha256 == requested.expected_previous_sha256
            {
                return Ok(());
            }
            if existing.status == PitchBatchPublicationStatus::Pending {
                return Err(identity_conflict(
                    "для идентичности уже сохранено другое намерение публикации",
                ));
            }
        }
        item.publication = Some(requested);
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер изменения элемента"))?;
        self.bump_revision()?;
        self.validate()
    }

    /// Сверяет все сохранённые факты владельца с полным снимком. Отсутствие/ошибка снимка
    /// должна обрабатываться вызывающей стороной до вызова этого метода.
    pub fn reconcile_owner(
        &mut self,
        snapshot: &PitchBatchOwnerSnapshot,
    ) -> Result<(), AssetError> {
        let validator = self.validator.clone();
        let mut batch_changed = false;
        for item in &mut self.items {
            let before = owner_state(item);
            let current = snapshot.current(&item.identity);
            item.reconcile_owner_record(current, &validator)?;
            if owner_state(item) != before {
                item.item_revision = item
                    .item_revision
                    .checked_add(1)
                    .ok_or_else(|| invalid("превышен номер изменения элемента"))?;
                batch_changed = true;
            }
        }
        if batch_changed {
            self.bump_revision()?;
            self.validate()?;
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), AssetError> {
        validate_batch_id(&self.batch_id)?;
        if self.schema_version != PITCH_BATCH_SCHEMA_VERSION {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "версия состояния пакета pitch-accent не поддерживается",
            ));
        }
        if self.validator != PitchAccentImageValidator::validator_identity() {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "состояние пакета pitch-accent использует устаревший валидатор; автоматическое доверие не переносится",
            ));
        }
        if self.items.is_empty() {
            return Err(invalid(
                "состояние пакета pitch-accent не содержит элементов",
            ));
        }
        self.original_plan.validate()?;
        if self.original_plan.requests.len() != self.items.len()
            || self
                .original_plan
                .requests
                .iter()
                .zip(&self.items)
                .any(|(request, item)| request.query.surface != item.identity.key)
        {
            return Err(identity_conflict(
                "написания и порядок исходного плана не совпадают с идентичностями пакета",
            ));
        }
        let mut identities = BTreeSet::new();
        for item in &self.items {
            PitchAccentDomainPolicy.validate_identity(&item.identity)?;
            if !identities.insert(item.identity.clone())
                || item.request.query.surface != item.identity.key
            {
                return Err(identity_conflict(
                    "написание `surface` запроса не уникально или не совпадает с канонической идентичностью",
                ));
            }
            validate_request(&item.request)?;
            match (
                item.current_candidate_sha256.as_deref(),
                item.current_candidate_attempt_index,
            ) {
                (Some(hash), Some(attempt_index)) => {
                    validate_hash(hash)?;
                    let attempt = item
                        .attempts
                        .iter()
                        .find(|attempt| attempt.index == attempt_index)
                        .ok_or_else(|| {
                            invalid("попытка текущего кандидата отсутствует в истории")
                        })?;
                    if attempt.generation != item.generation
                        || !matches!(&attempt.outcome, PitchBatchOutcome::Acquired { candidate } if candidate.sha256 == hash)
                        || item
                            .attempts
                            .last()
                            .is_none_or(|last| last.index != attempt_index)
                    {
                        return Err(invalid(
                            "текущий кандидат не совпадает с точной последней попыткой текущего поколения",
                        ));
                    }
                }
                (None, None) => {}
                _ => {
                    return Err(invalid(
                        "SHA текущего кандидата и индекс попытки должны задаваться вместе",
                    ));
                }
            }
            if let Some(hash) = &item.canonical_sha256 {
                validate_hash(hash)?;
            }
            if let Some(hash) = &item.owner_current_sha256 {
                validate_hash(hash)?;
            }
            if let Some(hash) = &item.existing_verified_sha256 {
                validate_hash(hash)?;
                if item.canonical_sha256.as_deref() != Some(hash) {
                    return Err(invalid(
                        "SHA существующей проверенной записи не совпадает с каноническим SHA",
                    ));
                }
            }
            if let Some(hash) = &item.published_sha256 {
                validate_hash(hash)?;
                if item.candidate(hash).is_none() {
                    return Err(invalid(
                        "опубликованный SHA отсутствует в истории получения",
                    ));
                }
            }
            if let Some(hash) = &item.refresh_expected_sha256 {
                validate_hash(hash)?;
            }
            let mut previous_generation = None;
            for (index, attempt) in item.attempts.iter().enumerate() {
                if attempt.index != index as u32 + 1
                    || attempt.generation > item.generation
                    || previous_generation.is_some_and(|old| old >= attempt.generation)
                    || attempt.request.query.surface != item.identity.key
                {
                    return Err(invalid(
                        "индексы попыток, поколение или `surface` нарушают историю пакета",
                    ));
                }
                previous_generation = Some(attempt.generation);
                validate_request(&attempt.request)?;
                validate_outcome(
                    &attempt.outcome,
                    &item.identity,
                    &attempt.request,
                    &self.validator,
                )?;
                if attempt.generation == item.generation && attempt.request != item.request {
                    return Err(identity_conflict(
                        "активный запрос отличается от результата текущего поколения",
                    ));
                }
            }
            if item.current_candidate_sha256.is_none()
                && let Some(PitchBatchOutcome::Acquired { candidate }) = item.current_outcome()
                && !item
                    .rejected_candidates
                    .iter()
                    .any(|rejection| rejection.candidate_sha256 == candidate.sha256)
            {
                return Err(invalid(
                    "текущий результат получения потерял ссылку на свою точную попытку",
                ));
            }
            for rejection in &item.rejected_candidates {
                validate_hash(&rejection.candidate_sha256)?;
                validate_reason(&rejection.reason)?;
                if rejection.generation > item.generation
                    || item.candidate(&rejection.candidate_sha256).is_none()
                {
                    return Err(invalid(
                        "отклонение не относится к сохранённому точному SHA кандидата",
                    ));
                }
            }
            if let Some(publication) = &item.publication {
                validate_hash(&publication.candidate_sha256)?;
                if let Some(expected) = &publication.expected_previous_sha256 {
                    validate_hash(expected)?;
                }
                let candidate = item
                    .candidate_at(
                        publication.candidate_attempt_index,
                        &publication.candidate_sha256,
                    )
                    .ok_or_else(|| invalid("кандидат публикации отсутствует в истории"))?;
                if candidate.validation.status != SemanticStatus::Verified
                    || candidate.validation.validator != self.validator
                    || item
                        .rejected_candidates
                        .iter()
                        .any(|rejection| rejection.candidate_sha256 == candidate.sha256)
                    || publication.conflict_code.is_some() != publication.conflict_message.is_some()
                    || (publication.status == PitchBatchPublicationStatus::Pending
                        && item.current_candidate_attempt_index
                            != Some(publication.candidate_attempt_index))
                {
                    return Err(AssetError::new(
                        ErrorCode::InvalidValidationEvidence,
                        "намерение публикации не привязано к текущему кандидату VERIFIED",
                    ));
                }
            }
            for publication in &item.publication_history {
                validate_hash(&publication.candidate_sha256)?;
                if publication.status == PitchBatchPublicationStatus::Pending
                    || item
                        .candidate_at(
                            publication.candidate_attempt_index,
                            &publication.candidate_sha256,
                        )
                        .is_none()
                    || publication.conflict_code.is_some() != publication.conflict_message.is_some()
                {
                    return Err(invalid(
                        "архивированное намерение публикации повреждено или ещё не завершено",
                    ));
                }
                if let Some(expected) = &publication.expected_previous_sha256 {
                    validate_hash(expected)?;
                }
            }
            if item.owner_conflict.as_ref().is_some_and(|conflict| {
                conflict.code.trim().is_empty() || conflict.message.trim().is_empty()
            }) {
                return Err(invalid("конфликт владельца содержит пустую диагностику"));
            }
            if item.last_action_reason.as_ref().is_some_and(|reason| {
                reason.trim().is_empty() || reason.len() > MAX_PITCH_BATCH_REASON_BYTES
            }) {
                return Err(invalid("причина действия над элементом некорректна"));
            }
        }
        if self.blob_validation_context_sha256 != blob_context_digest(&self.items)? {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "контекст проверки файлов пакета pitch-accent не совпадает со свидетельствами кандидата",
            ));
        }
        Ok(())
    }

    fn bump_revision(&mut self) -> Result<(), AssetError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер изменения пакета pitch-accent"))?;
        Ok(())
    }

    fn refresh_blob_validation_context(&mut self) -> Result<(), AssetError> {
        self.blob_validation_context_sha256 = blob_context_digest(&self.items)?;
        Ok(())
    }
}

impl PitchBatchItem {
    /// Единый критерий разрешённости адресного и пакетного повтора.
    fn ensure_retryable_failure(&self) -> Result<(), AssetError> {
        if self.owner_conflict.is_some()
            || self
                .publication
                .as_ref()
                .is_some_and(|intent| intent.status == PitchBatchPublicationStatus::Pending)
        {
            return Err(invalid(
                "сначала разрешите конфликт владельца или завершите намерение публикации",
            ));
        }
        let Some(PitchBatchOutcome::Failed { error }) = self.current_outcome() else {
            return Err(invalid(
                "точечный повтор разрешён только после технического сбоя",
            ));
        };
        if !is_retryable_failure(error) {
            return Err(invalid(
                "сбой источника, входных данных или выбора нельзя исправить повтором получения",
            ));
        }
        Ok(())
    }

    /// Переводит разрешённый сбой в новое поколение без проверки всего пакета.
    fn retry_failure(&mut self, reason: String) -> Result<(), AssetError> {
        self.ensure_retryable_failure()?;
        self.apply_retry_failure(reason)
    }

    fn apply_retry_failure(&mut self, reason: String) -> Result<(), AssetError> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер поколения"))?;
        self.current_candidate_sha256 = None;
        self.current_candidate_attempt_index = None;
        self.refresh_expected_sha256 = self.owner_current_sha256.clone();
        self.existing_verified_sha256 = None;
        self.archive_publication()?;
        self.last_action_reason = Some(reason);
        self.item_revision = self
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер изменения элемента"))?;
        Ok(())
    }

    fn reconcile_owner_record(
        &mut self,
        current: Option<&AssetRecord>,
        validator: &ValidatorIdentity,
    ) -> Result<(), AssetError> {
        self.owner_current_sha256 = current.map(|record| record.sha256.clone());

        if let Some(mut publication) = self.publication.clone() {
            let candidate = self.candidate_at(
                publication.candidate_attempt_index,
                &publication.candidate_sha256,
            );
            let exact = match (current, candidate) {
                (Some(record), Some(candidate)) => {
                    record_is_verified_for(record, validator)
                        && record_matches_request(record, &self.request)?
                        && record_matches_candidate(record, candidate)?
                }
                _ => false,
            };
            if exact {
                publication.status = PitchBatchPublicationStatus::Published;
                publication.conflict_code = None;
                publication.conflict_message = None;
                self.canonical_sha256 = Some(publication.candidate_sha256.clone());
                self.existing_verified_sha256 = None;
                self.published_sha256 = Some(publication.candidate_sha256.clone());
                self.refresh_expected_sha256 = None;
                self.owner_conflict = None;
            } else if current.is_some_and(owner_record_rejected) {
                let conflict = PitchBatchConflict {
                    code: "owner_rejected".into(),
                    message: "текущая запись владельца отклонена для этого точного SHA".into(),
                };
                publication.status = PitchBatchPublicationStatus::Conflict;
                publication.conflict_code = Some(conflict.code.clone());
                publication.conflict_message = Some(conflict.message.clone());
                self.owner_conflict = Some(conflict);
                self.canonical_sha256 = None;
                self.existing_verified_sha256 = None;
                self.refresh_expected_sha256 = None;
            } else {
                let expected_unchanged =
                    match (current, publication.expected_previous_sha256.as_deref()) {
                        (None, None) => true,
                        (Some(record), Some(expected)) => record.sha256 == expected,
                        _ => false,
                    };
                if publication.status != PitchBatchPublicationStatus::Pending || !expected_unchanged
                {
                    let owner_drift = !expected_unchanged
                        || publication.conflict_code.as_deref() == Some("refresh_owner_drift");
                    let conflict = PitchBatchConflict {
                        code: if owner_drift {
                            "refresh_owner_drift".into()
                        } else {
                            "identity_conflict".into()
                        },
                        message: if owner_drift {
                            "SHA владельца изменился после фиксации CAS публикации; явно примите текущий SHA через reacquire".into()
                        } else {
                            match current {
                                Some(record) if record.sha256 != publication.candidate_sha256 => {
                                    "текущий SHA владельца не совпадает с кандидатом или ожидаемым CAS SHA".into()
                                }
                                Some(_) => {
                                    "владелец не подтверждает текущий VERIFIED для точного кандидата".into()
                                }
                                None => {
                                    "снимок владельца не содержит идентичности намерения публикации".into()
                                }
                            }
                        },
                    };
                    publication.status = PitchBatchPublicationStatus::Conflict;
                    publication.conflict_code = Some(conflict.code.clone());
                    publication.conflict_message = Some(conflict.message.clone());
                    self.owner_conflict = Some(conflict);
                    self.canonical_sha256 = None;
                    self.existing_verified_sha256 = None;
                    self.refresh_expected_sha256 = None;
                }
            }
            self.publication = Some(publication);
            return Ok(());
        }

        if let Some(record) = current {
            let refresh_matches = self.refresh_expected_sha256.as_deref() == Some(&record.sha256);
            let reusable = record_is_verified_for(record, validator)
                && record_matches_request(record, &self.request)?;
            if self.refresh_expected_sha256.is_some() && !refresh_matches {
                self.canonical_sha256 = None;
                self.existing_verified_sha256 = None;
                self.owner_conflict = Some(PitchBatchConflict {
                    code: "refresh_owner_drift".into(),
                    message: "SHA владельца изменился после фиксации CAS обновления; явно примите текущий SHA через reacquire".into(),
                });
            } else if reusable {
                self.canonical_sha256 = Some(record.sha256.clone());
                if self.refresh_expected_sha256.is_none()
                    && self.published_sha256.as_deref() != Some(record.sha256.as_str())
                {
                    self.existing_verified_sha256 = Some(record.sha256.clone());
                } else {
                    self.existing_verified_sha256 = None;
                }
                self.owner_conflict = None;
            } else if refresh_matches {
                self.canonical_sha256 = None;
                self.existing_verified_sha256 = None;
                self.owner_conflict = None;
            } else if owner_record_rejected(record) {
                self.canonical_sha256 = None;
                self.existing_verified_sha256 = None;
                self.owner_conflict = Some(PitchBatchConflict {
                    code: "owner_rejected".into(),
                    message: "текущая запись владельца отклонена для этого точного SHA".into(),
                });
            } else {
                self.canonical_sha256 = None;
                self.existing_verified_sha256 = None;
                self.owner_conflict = Some(PitchBatchConflict {
                    code: "owner_not_current_verified".into(),
                    message:
                        "идентичность владельца есть, но её нельзя переиспользовать без явного обновления"
                            .into(),
                });
            }
        } else {
            self.canonical_sha256 = None;
            self.existing_verified_sha256 = None;
            self.owner_conflict = if self.refresh_expected_sha256.is_some() {
                Some(PitchBatchConflict {
                    code: "refresh_owner_drift".into(),
                    message: "ожидаемый SHA владельца для обновления отсутствует; явно примите текущую пустую идентичность через reacquire".into(),
                })
            } else {
                None
            };
        }
        Ok(())
    }

    fn archive_publication(&mut self) -> Result<(), AssetError> {
        let Some(publication) = self.publication.take() else {
            return Ok(());
        };
        if publication.status == PitchBatchPublicationStatus::Pending {
            self.publication = Some(publication);
            return Err(invalid(
                "нельзя завершить новое поколение при незавершённом намерении публикации",
            ));
        }
        self.publication_history.push(publication);
        Ok(())
    }

    pub fn status(&self) -> PitchBatchItemStatus {
        if self.owner_conflict.is_some()
            || self.publication.as_ref().is_some_and(|publication| {
                publication.status == PitchBatchPublicationStatus::Conflict
            })
        {
            return PitchBatchItemStatus::Conflict;
        }
        if let Some(publication) = &self.publication {
            return match publication.status {
                PitchBatchPublicationStatus::Pending => PitchBatchItemStatus::PublicationPending,
                PitchBatchPublicationStatus::Published => PitchBatchItemStatus::Published,
                PitchBatchPublicationStatus::Conflict => PitchBatchItemStatus::Conflict,
            };
        }
        if self.canonical_sha256.is_some() && self.refresh_expected_sha256.is_none() {
            return PitchBatchItemStatus::ExistingVerified;
        }
        let Some(outcome) = self.current_outcome() else {
            return PitchBatchItemStatus::Pending;
        };
        match outcome {
            PitchBatchOutcome::Acquired { candidate } => {
                if self
                    .rejected_candidates
                    .iter()
                    .any(|rejection| rejection.candidate_sha256 == candidate.sha256)
                {
                    PitchBatchItemStatus::CandidateRejected
                } else if self
                    .current_candidate()
                    .is_none_or(|current| current.sha256 != candidate.sha256)
                {
                    PitchBatchItemStatus::Pending
                } else if candidate.validation.status == SemanticStatus::Verified {
                    PitchBatchItemStatus::AcquiredVerified
                } else {
                    PitchBatchItemStatus::CandidateRejected
                }
            }
            PitchBatchOutcome::NoPitchAccentOnSource { .. } => {
                PitchBatchItemStatus::NoPitchAccentOnSource
            }
            PitchBatchOutcome::AmbiguousVocabulary { .. } => {
                PitchBatchItemStatus::AmbiguousVocabulary
            }
            PitchBatchOutcome::VocabularyNotFound { .. } => {
                PitchBatchItemStatus::VocabularyNotFound
            }
            PitchBatchOutcome::Failed { .. } => PitchBatchItemStatus::TechnicalFailure,
        }
    }

    pub fn current_outcome(&self) -> Option<&PitchBatchOutcome> {
        self.attempts
            .last()
            .filter(|attempt| attempt.generation == self.generation)
            .map(|attempt| &attempt.outcome)
    }

    pub fn candidate(&self, sha256: &str) -> Option<&PitchBatchCandidate> {
        self.current_candidate()
            .filter(|candidate| candidate.sha256 == sha256)
            .or_else(|| {
                self.attempts.iter().find_map(|attempt| {
                    if let PitchBatchOutcome::Acquired { candidate } = &attempt.outcome
                        && candidate.sha256 == sha256
                    {
                        Some(candidate.as_ref())
                    } else {
                        None
                    }
                })
            })
    }

    pub fn current_candidate(&self) -> Option<&PitchBatchCandidate> {
        let attempt_index = self.current_candidate_attempt_index?;
        let sha256 = self.current_candidate_sha256.as_deref()?;
        self.candidate_at(attempt_index, sha256)
    }

    pub fn candidate_at(&self, attempt_index: u32, sha256: &str) -> Option<&PitchBatchCandidate> {
        self.attempts
            .iter()
            .find(|attempt| attempt.index == attempt_index)
            .and_then(|attempt| match &attempt.outcome {
                PitchBatchOutcome::Acquired { candidate } if candidate.sha256 == sha256 => {
                    Some(candidate.as_ref())
                }
                _ => None,
            })
    }

    pub fn token(&self) -> PitchBatchItemToken {
        PitchBatchItemToken {
            identity: self.identity.clone(),
            generation: self.generation,
            item_revision: self.item_revision,
            request_fingerprint: request_fingerprint(&self.request),
        }
    }
}

impl PitchAccentBatchRuntime {
    pub fn open(store_root: &Path, batch_id: &str) -> Result<Self, AssetError> {
        Ok(Self {
            runtime: SafeBatchRuntime::open(store_root, batch_id)?,
        })
    }

    /// Создаёт новое состояние среды выполнения и сохраняет его до возврата.
    pub fn create(store_root: &Path, batch: &PitchAccentBatch) -> Result<Self, AssetError> {
        batch.validate()?;
        let mut runtime = Self::open(store_root, &batch.batch_id)?;
        runtime.save(batch)?;
        Ok(runtime)
    }

    pub fn batch_id(&self) -> &str {
        self.runtime.batch_id()
    }

    pub fn load(&mut self) -> Result<Option<PitchAccentBatch>, AssetError> {
        self.runtime.load()
    }

    pub fn save(&mut self, batch: &PitchAccentBatch) -> Result<(), AssetError> {
        self.runtime.save(batch)
    }

    /// Сохраняет результат одного элемента по токену CAS. Возвращает `false`, если во время
    /// получения у элемента изменились поколение, запрос или ревизия элемента.
    pub fn record_outcome(
        &mut self,
        batch: &mut PitchAccentBatch,
        token: &PitchBatchItemToken,
        outcome: JpdbPitchOutcome,
    ) -> Result<bool, AssetError> {
        self.record_outcome_diagnostic(batch, token, outcome, None)
    }

    pub(crate) fn record_outcome_diagnostic(
        &mut self,
        batch: &mut PitchAccentBatch,
        token: &PitchBatchItemToken,
        outcome: JpdbPitchOutcome,
        diagnostics: Option<&BrowserItemTimer>,
    ) -> Result<bool, AssetError> {
        if batch.batch_id != self.batch_id() {
            return Err(invalid("ID пакета не совпадает с каталогом runtime"));
        }
        let Some(item) = batch
            .items
            .iter()
            .find(|item| item.identity == token.identity)
        else {
            return Ok(false);
        };
        if !token_matches(item, token) || item.status() != PitchBatchItemStatus::Pending {
            return Ok(false);
        }
        let stored = match outcome {
            JpdbPitchOutcome::Acquired { asset } => {
                self.store_acquired(item, *asset, &batch.validator, diagnostics)?
            }
            JpdbPitchOutcome::NoPitchAccentOnSource { evidence } => {
                PitchBatchOutcome::NoPitchAccentOnSource { evidence }
            }
            JpdbPitchOutcome::AmbiguousVocabulary {
                surface,
                reading,
                candidates,
            } => PitchBatchOutcome::AmbiguousVocabulary {
                surface,
                reading,
                candidates,
            },
            JpdbPitchOutcome::VocabularyNotFound { surface, reading } => {
                PitchBatchOutcome::VocabularyNotFound { surface, reading }
            }
            JpdbPitchOutcome::Failed { error } => PitchBatchOutcome::Failed { error },
        };
        let validation = diagnostics.map(|item| item.stage("outcome_validation"));
        let validated = validate_outcome(&stored, &item.identity, &item.request, &batch.validator);
        if let Some(validation) = validation {
            match &validated {
                Ok(_) => validation.finish_success(),
                Err(error) => validation.finish_failure(error.code.as_str(), false, None),
            }
        }
        validated?;
        let item = batch
            .item_mut(&token.identity.key)
            .expect("идентичность токена проверена выше");
        let attempt_index = item.attempts.len() as u32 + 1;
        item.attempts.push(PitchBatchAttempt {
            index: attempt_index,
            generation: item.generation,
            request: item.request.clone(),
            outcome: stored,
        });
        item.current_candidate_sha256 = item.attempts.last().and_then(|attempt| {
            if let PitchBatchOutcome::Acquired { candidate } = &attempt.outcome {
                Some(candidate.sha256.clone())
            } else {
                None
            }
        });
        item.current_candidate_attempt_index = item
            .current_candidate_sha256
            .as_ref()
            .map(|_| attempt_index);
        item.item_revision = item
            .item_revision
            .checked_add(1)
            .ok_or_else(|| invalid("превышен номер изменения элемента"))?;
        batch.refresh_blob_validation_context()?;
        batch.bump_revision()?;
        self.save(batch)?;
        Ok(true)
    }

    pub fn read_candidate(
        &self,
        batch: &PitchAccentBatch,
        candidate: &PitchBatchCandidate,
    ) -> Result<Vec<u8>, AssetError> {
        let referenced = batch.items.iter().any(|item| {
            item.attempts.iter().any(|attempt| {
                matches!(&attempt.outcome, PitchBatchOutcome::Acquired { candidate: stored } if stored.as_ref() == candidate)
            })
        });
        if !referenced {
            return Err(invalid("кандидат не принадлежит этому состоянию пакета"));
        }
        let bytes = self
            .runtime
            .read_blob_with_limit(&candidate.blob, PITCH_ACCENT_MAX_ASSET_BYTES)?;
        validate_candidate_bytes(candidate, &bytes, &batch.validator)?;
        Ok(bytes)
    }

    /// HTML-отчёт принадлежит `pitch_review`; среда выполнения только безопасно записывает артефакт.
    pub fn write_review(
        &self,
        batch: &PitchAccentBatch,
        owner_records: &[AssetRecord],
    ) -> Result<PathBuf, AssetError> {
        batch.validate()?;
        if batch.batch_id != self.batch_id() {
            return Err(invalid("ID пакета не совпадает с каталогом runtime"));
        }
        let html = crate::pitch_review::render(batch, owner_records, |candidate| {
            self.read_candidate(batch, candidate)
        })?;
        self.runtime
            .write_artifact("review.html", html.as_bytes(), MAX_RUNTIME_STATE_BYTES)
    }

    fn store_acquired(
        &self,
        item: &PitchBatchItem,
        acquired: JpdbPitchAcquired,
        validator_identity: &ValidatorIdentity,
        diagnostics: Option<&BrowserItemTimer>,
    ) -> Result<PitchBatchOutcome, AssetError> {
        validate_acquired_metadata(&item.identity, &item.request, &acquired.metadata).map_err(
            |message| {
                AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    format!("кандидат провайдера не соответствует запросу: {message}"),
                )
            },
        )?;
        if acquired.bytes.len() as u64 > PITCH_ACCENT_MAX_ASSET_BYTES
            || DetectedFormat::from_signature(&acquired.bytes) != DetectedFormat::Png
        {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "полученный кандидат провайдера превышает лимит или не является PNG",
            ));
        }
        let sha256 = sha256_hex(&acquired.bytes);
        let blob = self.runtime.persist_blob_with_limit(
            &acquired.bytes,
            "png",
            PITCH_ACCENT_MAX_ASSET_BYTES,
        )?;
        let stage = diagnostics.map(|item| item.stage("candidate_validation"));
        let validation = validate_candidate(
            &item.identity,
            &acquired.metadata,
            &acquired.bytes,
            validator_identity,
        );
        if let Some(stage) = stage {
            match &validation {
                Ok(_) => stage.finish_success(),
                Err(error) => stage.finish_failure(error.code.as_str(), false, None),
            }
        }
        let validation = validation?;
        let candidate = PitchBatchCandidate {
            blob,
            sha256,
            byte_length: acquired.bytes.len() as u64,
            metadata: acquired.metadata,
            validation,
            validation_context_sha256: String::new(),
        };
        let candidate = with_validation_context(candidate)?;
        Ok(PitchBatchOutcome::Acquired {
            candidate: Box::new(candidate),
        })
    }
}

impl RuntimeBatchState for PitchAccentBatch {
    fn batch_id(&self) -> &str {
        &self.batch_id
    }

    fn revision(&self) -> u64 {
        self.revision
    }

    fn validate(&self) -> Result<(), AssetError> {
        PitchAccentBatch::validate(self)
    }

    /// Состояние прежней схемы хранит свидетельства прежнего валидатора; оно не
    /// разбирается и не переносит доверие, а требует нового пакета.
    fn check_stored_compatibility(raw: &serde_json::Value) -> Result<(), AssetError> {
        if raw["schema_version"].as_u64() != Some(u64::from(PITCH_BATCH_SCHEMA_VERSION)) {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                format!(
                    "версия состояния пакета pitch-accent {} не поддерживается (текущая {PITCH_BATCH_SCHEMA_VERSION}); создайте новый пакет",
                    raw["schema_version"]
                ),
            ));
        }
        let current = PitchAccentImageValidator::validator_identity();
        if raw["validator"]["id"].as_str() != Some(current.id.as_str())
            || raw["validator"]["version"].as_str() != Some(current.version.as_str())
        {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "состояние пакета pitch-accent использует устаревший валидатор; автоматическое доверие не переносится, создайте новый пакет",
            ));
        }
        Ok(())
    }

    fn referenced_blobs(&self) -> Vec<RuntimeBlobRef> {
        self.items
            .iter()
            .flat_map(|item| item.attempts.iter())
            .filter_map(|attempt| match &attempt.outcome {
                PitchBatchOutcome::Acquired { candidate } => Some(candidate.blob.clone()),
                _ => None,
            })
            .collect()
    }

    fn maximum_blob_bytes(&self) -> u64 {
        PITCH_ACCENT_MAX_ASSET_BYTES
    }

    fn blob_validation_context(&self, blob: &RuntimeBlobRef) -> Option<&str> {
        self.items
            .iter()
            .flat_map(|item| item.attempts.iter())
            .any(|attempt| match &attempt.outcome {
                PitchBatchOutcome::Acquired { candidate } => candidate.blob == *blob,
                _ => false,
            })
            .then_some(self.blob_validation_context_sha256.as_str())
    }

    fn validate_blob_bytes(&self, blob: &RuntimeBlobRef, bytes: &[u8]) -> Result<(), AssetError> {
        let mut found = false;
        for candidate in self
            .items
            .iter()
            .flat_map(|item| item.attempts.iter())
            .filter_map(|attempt| match &attempt.outcome {
                PitchBatchOutcome::Acquired { candidate } if candidate.blob == *blob => {
                    Some(candidate)
                }
                _ => None,
            })
        {
            found = true;
            validate_candidate_bytes(candidate, bytes, &self.validator)?;
        }
        if !found {
            return Err(invalid(
                "состояние runtime ссылается на неизвестного кандидата pitch-accent",
            ));
        }
        Ok(())
    }
}

fn token_matches(item: &PitchBatchItem, token: &PitchBatchItemToken) -> bool {
    item.identity == token.identity
        && item.generation == token.generation
        && item.item_revision == token.item_revision
        && request_fingerprint(&item.request) == token.request_fingerprint
}

type OwnerItemState = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<PitchBatchConflict>,
    Option<PitchBatchPublication>,
);

fn owner_state(item: &PitchBatchItem) -> OwnerItemState {
    (
        item.owner_current_sha256.clone(),
        item.canonical_sha256.clone(),
        item.existing_verified_sha256.clone(),
        item.published_sha256.clone(),
        item.refresh_expected_sha256.clone(),
        item.owner_conflict.clone(),
        item.publication.clone(),
    )
}

fn owner_record_rejected(record: &AssetRecord) -> bool {
    record.current_human_decision() == Some(HumanDecision::Reject)
}

fn request_fingerprint(request: &JpdbPitchRequest) -> String {
    let bytes = serde_json::to_vec(request).expect("JpdbPitchRequest сериализуемый");
    let mut context = b"pitch-accent-request-v1\0".to_vec();
    context.extend_from_slice(&bytes);
    sha256_hex(&context)
}

fn plan_identity_digest(
    schema_version: u32,
    requests: &[JpdbPitchRequest],
) -> Result<String, AssetError> {
    let canonical = serde_json::to_vec(&(schema_version, requests)).map_err(|error| {
        AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            format!("не удалось сериализовать нормализованный план pitch-accent: {error}"),
        )
    })?;
    let mut digest_input = b"pitch-batch-plan-v1\0".to_vec();
    digest_input.extend_from_slice(&canonical);
    Ok(sha256_hex(&digest_input))
}

fn validate_request(request: &JpdbPitchRequest) -> Result<(), AssetError> {
    let query = &request.query;
    if query.surface.trim().is_empty()
        || query.surface != query.surface.trim()
        || query.surface.chars().any(char::is_control)
        || query.reading.as_deref().is_some_and(|reading| {
            reading.trim().is_empty() || reading.chars().any(char::is_control)
        })
    {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "`surface` должен быть точным непустым значением без краевых пробелов; `reading` — непустым без управляющих символов",
        ));
    }
    PitchAccentDomainPolicy.validate_identity(
        &AssetIdentity::new("pitch_accent", query.surface.clone())
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?,
    )?;
    if let Some(selection) = &request.selection {
        JpdbPitchSelection::new(selection.vocabulary_id, selection.detail_url.clone())
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
    }
    Ok(())
}

fn validate_outcome(
    outcome: &PitchBatchOutcome,
    identity: &AssetIdentity,
    request: &JpdbPitchRequest,
    validator: &ValidatorIdentity,
) -> Result<(), AssetError> {
    match outcome {
        PitchBatchOutcome::Acquired { candidate } => {
            validate_candidate_record(candidate, identity, request, validator)?;
        }
        PitchBatchOutcome::NoPitchAccentOnSource { evidence } => {
            if evidence.surface != identity.key
                || request
                    .query
                    .reading
                    .as_deref()
                    .is_some_and(|reading| !jpdb_readings_equivalent(&evidence.reading, reading))
                || evidence.jpdb_vocabulary_id == 0
                || evidence.source_url.trim().is_empty()
                || !evidence.base_page_contract_valid
                || evidence.pitch_section_present
                || evidence.pitch_marker_count != 0
                || !contains_form(
                    &evidence.resolved_forms,
                    &identity.key,
                    Some(&evidence.reading),
                )
            {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "свидетельства отсутствия pitch-accent противоречат точному запросу или контракту страницы",
                ));
            }
            let selection =
                JpdbPitchSelection::new(evidence.jpdb_vocabulary_id, evidence.source_url.clone())
                    .map_err(|message| {
                    AssetError::new(ErrorCode::InvalidValidationEvidence, message)
                })?;
            if let Some(request_selection) = &request.selection {
                let requested = JpdbPitchSelection::new(
                    request_selection.vocabulary_id,
                    request_selection.detail_url.clone(),
                )
                .map_err(|message| {
                    AssetError::new(ErrorCode::InvalidValidationEvidence, message)
                })?;
                let expected_route =
                    parse_jpdb_vocabulary_route(&requested.detail_url).map_err(|_| {
                        AssetError::new(
                            ErrorCode::InvalidValidationEvidence,
                            "маршрут явно выбранной словарной записи невалиден",
                        )
                    })?;
                let evidence_route =
                    parse_jpdb_vocabulary_route(&selection.detail_url).map_err(|_| {
                        AssetError::new(
                            ErrorCode::InvalidValidationEvidence,
                            "URL источника свидетельств отсутствия не является маршрутом словарной записи",
                        )
                    })?;
                if selection.vocabulary_id != requested.vocabulary_id
                    || evidence_route != expected_route
                {
                    return Err(AssetError::new(
                        ErrorCode::InvalidValidationEvidence,
                        "свидетельства отсутствия не совпадают с точным явным выбором словарной записи",
                    ));
                }
            }
        }
        PitchBatchOutcome::AmbiguousVocabulary {
            surface,
            reading,
            candidates,
        } => {
            if surface != &identity.key
                || request.query.reading.as_deref().is_some_and(|expected| {
                    reading
                        .as_deref()
                        .is_none_or(|actual| !jpdb_readings_equivalent(actual, expected))
                })
                || candidates.is_empty()
            {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "список неоднозначных результатов не соответствует точному запросу",
                ));
            }
            let mut ids = BTreeSet::new();
            for candidate in candidates {
                if candidate.vocabulary_id == 0
                    || !ids.insert(candidate.vocabulary_id)
                    || !contains_form(&candidate.resolved_forms, &identity.key, reading.as_deref())
                {
                    return Err(AssetError::new(
                        ErrorCode::InvalidValidationEvidence,
                        "список неоднозначных результатов содержит повторный ID или неподтверждённую форму",
                    ));
                }
                JpdbPitchSelection::new(candidate.vocabulary_id, candidate.detail_url.clone())
                    .map_err(|message| {
                        AssetError::new(ErrorCode::InvalidValidationEvidence, message)
                    })?;
            }
        }
        PitchBatchOutcome::VocabularyNotFound { surface, reading } => {
            if surface != &identity.key
                || request.query.reading.as_deref().is_some_and(|expected| {
                    reading
                        .as_deref()
                        .is_none_or(|actual| !jpdb_readings_equivalent(actual, expected))
                })
            {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "результат отсутствия словарной записи не соответствует точному запросу",
                ));
            }
        }
        PitchBatchOutcome::Failed { .. } => {}
    }
    if let PitchBatchOutcome::Acquired { candidate } = outcome {
        validate_acquired_metadata(identity, request, &candidate.metadata)?;
        if candidate.validation.validator != *validator {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "свидетельства кандидата используют не текущий валидатор",
            ));
        }
    }
    Ok(())
}

fn validate_candidate_record(
    candidate: &PitchBatchCandidate,
    identity: &AssetIdentity,
    request: &JpdbPitchRequest,
    validator: &ValidatorIdentity,
) -> Result<(), AssetError> {
    validate_hash(&candidate.sha256)?;
    if candidate.blob.sha256 != candidate.sha256
        || candidate.blob.storage_path != format!("candidates/{}.png", candidate.sha256)
        || candidate.byte_length == 0
        || candidate.byte_length > PITCH_ACCENT_MAX_ASSET_BYTES
        || candidate.metadata.surface != identity.key
        || candidate.validation.content_sha256 != candidate.sha256
        || candidate.validation.validator != *validator
        || !candidate.validation.is_valid_for_sha(&candidate.sha256)
        || candidate.validation_context_sha256 != candidate_context(candidate)?
    {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            "файл кандидата pitch-accent, SHA, метаданные, валидатор или контекст не согласованы",
        ));
    }
    validate_acquired_metadata(identity, request, &candidate.metadata)
}

fn validate_acquired_metadata(
    identity: &AssetIdentity,
    request: &JpdbPitchRequest,
    metadata: &PitchAccentDomainMetadata,
) -> Result<(), AssetError> {
    if metadata.surface != identity.key
        || !contains_form(
            &metadata.evidence.resolved_forms,
            &identity.key,
            request.query.reading.as_deref(),
        )
    {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            "метаданные кандидата не доказывают запрошенные `surface`/`reading`",
        ));
    }
    if let Some(selection) = &request.selection {
        let requested_route = parse_jpdb_vocabulary_route(&selection.detail_url)
            .map_err(|_| identity_conflict("маршрут выбранной в запросе записи невалиден"))?;
        let actual_route = parse_jpdb_vocabulary_route(&metadata.evidence.source_url)
            .map_err(|_| identity_conflict("маршрут источника кандидата невалиден"))?;
        if selection.vocabulary_id != metadata.jpdb_vocabulary_id || requested_route != actual_route
        {
            return Err(identity_conflict(
                "источник кандидата не совпадает с явным выбором словарной записи",
            ));
        }
    }
    Ok(())
}

fn contains_form(forms: &[PitchAccentResolvedForm], surface: &str, reading: Option<&str>) -> bool {
    forms.iter().any(|form| {
        form.surface == surface
            && reading.is_none_or(|expected| jpdb_readings_equivalent(&form.reading, expected))
    })
}

fn validate_candidate(
    identity: &AssetIdentity,
    metadata: &PitchAccentDomainMetadata,
    bytes: &[u8],
    validator_identity: &ValidatorIdentity,
) -> Result<ValidationRecord, AssetError> {
    let sha256 = sha256_hex(bytes);
    let location =
        PitchAccentDomainPolicy.canonical_location(identity, &sha256, DetectedFormat::Png)?;
    let asset =
        AssetRecord {
            identity: identity.clone(),
            storage_path: location.storage_path,
            consumer_filename: location.consumer_filename,
            sha256: sha256.clone(),
            byte_length: bytes.len() as u64,
            format: DetectedFormat::Png,
            provenance: crate::model::Provenance {
                source_kind: "jpdb_browser_render".into(),
                source_name: format!("jpdb-vocabulary-{}.png", metadata.jpdb_vocabulary_id),
            },
            lifecycle: LifecycleState::Pending,
            validation: None,
            human_attestation: None,
            domain_metadata: Some(serde_json::to_value(metadata).map_err(|error| {
                invalid(format!("не удалось сериализовать метаданные: {error}"))
            })?),
        };
    let validator = PitchAccentImageValidator;
    if validator.identity() != *validator_identity {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            "получение кандидата использует не текущий валидатор",
        ));
    }
    let decision = validator
        .validate(&asset, &mut Cursor::new(bytes))
        .map_err(|failure| {
            AssetError::new(
                ErrorCode::ValidatorFailure,
                format!(
                    "PitchAccentImageValidator завершился ошибкой: {}",
                    failure.message
                ),
            )
        })?;
    Ok(ValidationRecord {
        status: decision.status,
        validator: validator.identity(),
        content_sha256: sha256,
        evidence: decision.evidence,
    })
}

fn validate_candidate_bytes(
    candidate: &PitchBatchCandidate,
    bytes: &[u8],
    validator: &ValidatorIdentity,
) -> Result<(), AssetError> {
    if bytes.len() as u64 != candidate.byte_length
        || sha256_hex(bytes) != candidate.sha256
        || DetectedFormat::from_signature(bytes) != DetectedFormat::Png
    {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "байты кандидата pitch-accent, длина или сигнатура PNG не совпадают с состоянием",
        ));
    }
    if candidate.validation_context_sha256 != candidate_context(candidate)? {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "контекст проверки кандидата pitch-accent не совпадает с метаданными или свидетельствами",
        ));
    }
    let identity = AssetIdentity::new("pitch_accent", candidate.metadata.surface.clone())
        .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
    let fresh = validate_candidate(&identity, &candidate.metadata, bytes, validator)?;
    if fresh != candidate.validation {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "результат PitchAccentImageValidator не совпадает с сохранёнными точными свидетельствами",
        ));
    }
    Ok(())
}

fn with_validation_context(
    mut candidate: PitchBatchCandidate,
) -> Result<PitchBatchCandidate, AssetError> {
    candidate.validation_context_sha256 = candidate_context(&candidate)?;
    Ok(candidate)
}

fn candidate_context(candidate: &PitchBatchCandidate) -> Result<String, AssetError> {
    #[derive(Serialize)]
    struct Context<'a> {
        sha256: &'a str,
        byte_length: u64,
        metadata: &'a PitchAccentDomainMetadata,
        validation: &'a ValidationRecord,
    }
    let bytes = serde_json::to_vec(&Context {
        sha256: &candidate.sha256,
        byte_length: candidate.byte_length,
        metadata: &candidate.metadata,
        validation: &candidate.validation,
    })
    .map_err(|error| {
        invalid(format!(
            "не удалось сериализовать контекст кандидата: {error}"
        ))
    })?;
    Ok(sha256_hex(&bytes))
}

fn blob_context_digest(items: &[PitchBatchItem]) -> Result<String, AssetError> {
    let contexts = items
        .iter()
        .flat_map(|item| item.attempts.iter())
        .filter_map(|attempt| match &attempt.outcome {
            PitchBatchOutcome::Acquired { candidate } => Some((
                candidate.blob.sha256.clone(),
                candidate.validation_context_sha256.clone(),
            )),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let bytes = serde_json::to_vec(&contexts).map_err(|error| {
        invalid(format!(
            "не удалось сериализовать контекст файлов кандидатов: {error}"
        ))
    })?;
    Ok(sha256_hex(&bytes))
}

fn record_is_verified_for(record: &AssetRecord, validator: &ValidatorIdentity) -> bool {
    record.lifecycle == LifecycleState::Verified
        && record.format == DetectedFormat::Png
        && record.validation.as_ref().is_some_and(|validation| {
            validation.status == SemanticStatus::Verified
                && validation.validator == *validator
                && validation.content_sha256 == record.sha256
                && validation.is_valid_for_sha(&record.sha256)
        })
        && record.is_trusted_for_automated_validation(validator)
}

fn record_matches_request(
    record: &AssetRecord,
    request: &JpdbPitchRequest,
) -> Result<bool, AssetError> {
    let Some(value) = &record.domain_metadata else {
        return Ok(false);
    };
    let metadata: PitchAccentDomainMetadata =
        serde_json::from_value(value.clone()).map_err(|_| {
            AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "метаданные pitch-accent владельца имеют неизвестную форму",
            )
        })?;
    if metadata.surface != request.query.surface
        || !contains_form(
            &metadata.evidence.resolved_forms,
            &request.query.surface,
            request.query.reading.as_deref(),
        )
    {
        return Ok(false);
    }
    if let Some(selection) = &request.selection {
        let expected = parse_jpdb_vocabulary_route(&selection.detail_url)
            .map_err(|_| identity_conflict("маршрут выбранной в запросе записи невалиден"))?;
        let actual = parse_jpdb_vocabulary_route(&metadata.evidence.source_url)
            .map_err(|_| identity_conflict("маршрут источника в метаданных владельца невалиден"))?;
        return Ok(metadata.jpdb_vocabulary_id == selection.vocabulary_id && actual == expected);
    }
    Ok(true)
}

fn record_matches_candidate(
    record: &AssetRecord,
    candidate: &PitchBatchCandidate,
) -> Result<bool, AssetError> {
    if record.sha256 != candidate.sha256
        || record.validation.as_ref() != Some(&candidate.validation)
    {
        return Ok(false);
    }
    let Some(value) = &record.domain_metadata else {
        return Ok(false);
    };
    let metadata: PitchAccentDomainMetadata =
        serde_json::from_value(value.clone()).map_err(|_| {
            AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "метаданные pitch-accent владельца имеют неизвестную форму",
            )
        })?;
    Ok(metadata == candidate.metadata)
}

pub fn is_retryable_failure(error: &JpdbPitchFailure) -> bool {
    match error {
        JpdbPitchFailure::Timeout { .. }
        | JpdbPitchFailure::BrowserSetup { .. }
        | JpdbPitchFailure::Telemetry { .. }
        | JpdbPitchFailure::SessionFailure { .. }
        | JpdbPitchFailure::Screenshot { .. } => true,
        JpdbPitchFailure::Navigation { message, .. } => contains_retryable_network_error(message),
        JpdbPitchFailure::BrowserEvaluation { .. }
        | JpdbPitchFailure::BrowserConfiguration { .. }
        | JpdbPitchFailure::InvalidQuery { .. }
        | JpdbPitchFailure::PageContract { .. }
        | JpdbPitchFailure::DetailIdentityMismatch { .. }
        | JpdbPitchFailure::InvalidSelection { .. }
        | JpdbPitchFailure::ExplicitSelectionMismatch { .. }
        | JpdbPitchFailure::DarkThemeUnverified { .. }
        | JpdbPitchFailure::CaptureContract { .. }
        | JpdbPitchFailure::InvalidPng { .. } => false,
    }
}

fn contains_retryable_network_error(message: &str) -> bool {
    let upper = message.to_ascii_uppercase();
    [
        "NET::ERR_TIMED_OUT",
        "ERR_TIMED_OUT",
        "NET::ERR_CONNECTION_RESET",
        "ERR_CONNECTION_RESET",
        "NET::ERR_CONNECTION_CLOSED",
        "ERR_CONNECTION_CLOSED",
        "NET::ERR_CONNECTION_REFUSED",
        "ERR_CONNECTION_REFUSED",
        "NET::ERR_NETWORK_CHANGED",
        "ERR_NETWORK_CHANGED",
        "NET::ERR_ABORTED",
        "ERR_ABORTED",
        "NET::ERR_DNS_TIMED_OUT",
        "ERR_DNS_TIMED_OUT",
        "NET::ERR_DNS_SERVER_FAILED",
        "ERR_DNS_SERVER_FAILED",
        "NET::ERR_NAME_RESOLUTION_FAILED",
        "ERR_NAME_RESOLUTION_FAILED",
    ]
    .iter()
    .any(|token| upper.contains(token))
        || contains_retryable_http_status(&upper)
}

fn contains_retryable_http_status(message: &str) -> bool {
    let without_urls = message
        .split_whitespace()
        .filter(|part| !part.contains("://"))
        .collect::<Vec<_>>()
        .join(" ");
    let words = without_urls
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    words.iter().enumerate().any(|(index, word)| {
        let Ok(status) = word.parse::<u16>() else {
            return false;
        };
        if status != 429 && !(500..=599).contains(&status) {
            return false;
        }
        if words
            .get(index + 1)
            .is_some_and(|unit| matches!(*unit, "MS" | "MSEC" | "MILLISECONDS"))
        {
            return false;
        }
        let previous = index.checked_sub(1).and_then(|i| words.get(i));
        previous.is_some_and(|context| matches!(*context, "HTTP" | "STATUS" | "RESPONSE"))
            || (previous == Some(&"CODE")
                && index.checked_sub(2).and_then(|i| words.get(i)) == Some(&"STATUS"))
    })
}

fn validate_reason(reason: &str) -> Result<(), AssetError> {
    if reason.trim().is_empty() || reason.len() > MAX_PITCH_BATCH_REASON_BYTES {
        return Err(invalid(format!(
            "`reason` должен быть непустым и не длиннее {MAX_PITCH_BATCH_REASON_BYTES} байт"
        )));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

fn identity_conflict(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::IdentityConflict, message)
}
