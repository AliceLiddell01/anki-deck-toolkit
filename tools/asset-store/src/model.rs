//! Версионируемое состояние и доменные типы для всех потребителей ресурсов.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Версия формата постоянного манифеста.
///
/// Схема 6 разрешает домену с форматными вариантами (`kanji`) хранить для одной
/// идентичности по одной независимой записи на формат (`漢.gif` и `漢.png`).
/// Раскладка файлов и поля записи совпадают со схемой 5.
pub const MANIFEST_SCHEMA_VERSION: u32 = 6;

/// Первая схема с вложенной канонической раскладкой и закреплённым `domain_id`.
/// Схемы 3/4 до неё переносятся миграцией раскладки; схема 5 отличается от
/// текущей только правилом уникальности записей.
pub const NESTED_LAYOUT_SCHEMA_VERSION: u32 = 5;

/// Логический идентификатор ресурса; имя файла в нём не участвует.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetIdentity {
    /// Тип или пространство имён ресурса, например `kanji`.
    pub namespace: String,
    /// Стабильный ключ, заданный вызывающей предметной областью.
    pub key: String,
}

impl AssetIdentity {
    /// Создаёт и проверяет логический идентификатор.
    pub fn new(namespace: impl Into<String>, key: impl Into<String>) -> Result<Self, String> {
        let identity = Self {
            namespace: namespace.into(),
            key: key.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    /// Проверяет безопасные ограничения формата идентификатора.
    pub fn validate(&self) -> Result<(), String> {
        let namespace_ok = !self.namespace.is_empty()
            && self.namespace.len() <= 64
            && self.namespace.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
            });
        if !namespace_ok {
            return Err(
                "поле namespace должно содержать до 64 символов a-z, 0-9, '.', '_' или '-'".into(),
            );
        }
        if self.key.is_empty() || self.key.len() > 512 || self.key.chars().any(char::is_control) {
            return Err(
                "поле key должно быть непустым, не длиннее 512 байт и не содержать управляющих символов".into(),
            );
        }
        Ok(())
    }
}

impl fmt::Display for AssetIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.namespace, self.key)
    }
}

/// Формат, определённый по сигнатуре байтов независимо от расширения исходника.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectedFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    Tiff,
    Unknown,
}

impl DetectedFormat {
    /// Классифицирует распространённые бинарные форматы по сигнатуре.
    pub fn from_signature(bytes: &[u8]) -> Self {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Self::Png
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Self::Jpeg
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Self::Gif
        } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
            Self::Webp
        } else if bytes.starts_with(b"BM") {
            Self::Bmp
        } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
            Self::Tiff
        } else {
            Self::Unknown
        }
    }
}

/// Физическое положение файла в хранилище.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Pending,
    Verified,
    Quarantined,
}

impl LifecycleState {
    /// Стабильное машиночитаемое имя состояния.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Verified => "verified",
            Self::Quarantined => "quarantined",
        }
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Результат семантического решения внешнего доменного валидатора.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticStatus {
    Verified,
    Rejected,
    Uncertain,
    Corrupt,
}

impl SemanticStatus {
    /// Стабильное машиночитаемое имя результата.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Rejected => "rejected",
            Self::Uncertain => "uncertain",
            Self::Corrupt => "corrupt",
        }
    }
}

/// Идентификатор и версия семантического валидатора.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorIdentity {
    pub id: String,
    pub version: String,
}

impl ValidatorIdentity {
    /// Создаёт и проверяет явную пару «идентификатор/версия».
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> Result<Self, String> {
        let identity = Self {
            id: id.into(),
            version: version.into(),
        };
        if identity.id.is_empty()
            || identity.version.is_empty()
            || identity.id.len() > 128
            || identity.version.len() > 128
            || identity.id.chars().any(char::is_control)
            || identity.version.chars().any(char::is_control)
        {
            return Err(
                "идентификатор и версия валидатора должны быть непустыми строками до 128 байт"
                    .into(),
            );
        }
        Ok(identity)
    }
}

/// Одно проверяемое свидетельство доменного валидатора.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationEvidence {
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Ещё не сохранённый ответ валидатора; его принимает только `API` жизненного цикла.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticDecision {
    pub status: SemanticStatus,
    pub evidence: Vec<ValidationEvidence>,
}

impl SemanticDecision {
    /// Создаёт решение с явным статусом и свидетельствами.
    pub fn new(status: SemanticStatus, evidence: Vec<ValidationEvidence>) -> Self {
        Self { status, evidence }
    }
}

/// Происхождение исходных байтов.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    /// Стабильный тип источника; поддерживается `local_import`.
    pub source_kind: String,
    /// Имя явно переданного файла; абсолютный путь исходной системы не сохраняется.
    pub source_name: String,
}

/// Семантическое решение, привязанное к хешу и версии конкретного валидатора.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationRecord {
    pub status: SemanticStatus,
    pub validator: ValidatorIdentity,
    pub content_sha256: String,
    pub evidence: Vec<ValidationEvidence>,
}

impl ValidationRecord {
    /// Проверяет, что автоматическое решение содержит пригодные данные и
    /// относится к ожидаемому точному хешу.
    pub(crate) fn is_valid_for_sha(&self, sha256: &str) -> bool {
        self.content_sha256 == sha256
            && is_sha256(&self.content_sha256)
            && ValidatorIdentity::new(self.validator.id.clone(), self.validator.version.clone())
                .is_ok()
            && !self.evidence.is_empty()
            && self.evidence.iter().all(|evidence| {
                !evidence.kind.trim().is_empty() && !evidence.summary.trim().is_empty()
            })
    }
}

/// Явное семантическое решение человека для конкретных байтов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HumanDecision {
    Approve,
    Reject,
}

/// Независимое от автоматической проверки свидетельство; не обходит контроль
/// целостности и декодирование.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanAttestation {
    pub identity: AssetIdentity,
    pub content_sha256: String,
    pub decision: HumanDecision,
    /// Основание явно полученного пользовательского решения.
    pub reason: String,
}

/// Одна актуальная версия логического ресурса.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetRecord {
    pub identity: AssetIdentity,
    /// Относительный путь сохраняется явно и при чтении сверяется с состоянием и хешем.
    pub storage_path: String,
    /// Плоское имя, которое потребитель размещает в своём каталоге медиафайлов.
    /// Оно независимо от вложенного `storage_path` и сверяется с форматом байтов.
    #[serde(default)]
    pub consumer_filename: String,
    pub sha256: String,
    pub byte_length: u64,
    pub format: DetectedFormat,
    pub provenance: Provenance,
    pub lifecycle: LifecycleState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<ValidationRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_attestation: Option<HumanAttestation>,
    /// Потребитель `kanji` хранит здесь символ и его кодовые точки Unicode; общий
    /// код сохраняет дополнительные сведения о расширении без интерпретации.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_metadata: Option<serde_json::Value>,
}

impl AssetRecord {
    /// Решение человека учитывается только для точной идентичности и хеша.
    pub fn current_human_decision(&self) -> Option<HumanDecision> {
        self.human_attestation
            .as_ref()
            .filter(|attestation| {
                attestation.identity == self.identity
                    && attestation.content_sha256 == self.sha256
                    && !attestation.reason.trim().is_empty()
            })
            .map(|attestation| attestation.decision)
    }

    /// Автоматическое решение учитывается, только если его хеш и свидетельства
    /// корректны для текущих байтов.
    pub(crate) fn current_validation_status(&self) -> Option<SemanticStatus> {
        self.validation
            .as_ref()
            .filter(|decision| decision.is_valid_for_sha(&self.sha256))
            .map(|decision| decision.status)
    }

    /// Есть ли пригодные свидетельства автоматической проверки для текущего хеша.
    pub(crate) fn has_current_validation(&self) -> bool {
        self.current_validation_status().is_some()
    }

    /// Полное решение человека: точное одобрение текущих байтов, подтверждённое
    /// актуальным автоматическим свидетельством.
    pub(crate) fn has_complete_human_approval(&self) -> bool {
        self.current_human_decision() == Some(HumanDecision::Approve)
            && self.has_current_validation()
            && self.effective_status() == Some(SemanticStatus::Verified)
    }

    /// Актуальное автоматическое решение ожидаемой версии валидатора.
    pub(crate) fn has_current_automated_decision(&self, validator: &ValidatorIdentity) -> bool {
        self.validation.as_ref().is_some_and(|decision| {
            decision.is_valid_for_sha(&self.sha256) && decision.validator == *validator
        })
    }

    /// Семантический статус после решения человека. Вызывающий обязан сначала
    /// проверить физическую целостность; этот метод не читает и не декодирует байты.
    pub fn effective_status(&self) -> Option<SemanticStatus> {
        let automated = self.current_validation_status();
        if automated == Some(SemanticStatus::Corrupt) {
            return automated;
        }
        match self.current_human_decision() {
            Some(HumanDecision::Reject) => Some(SemanticStatus::Rejected),
            Some(HumanDecision::Approve) if self.has_current_validation() => {
                Some(SemanticStatus::Verified)
            }
            Some(HumanDecision::Approve) => None,
            None => automated,
        }
    }

    /// Актуальность семантического статуса для потребителя, ожидающего версию валидатора.
    pub fn is_trusted_for(&self, validator: &ValidatorIdentity) -> bool {
        self.effective_status() == Some(SemanticStatus::Verified)
            && (self.current_human_decision() == Some(HumanDecision::Approve)
                || self.validation.as_ref().is_some_and(|decision| {
                    decision.is_valid_for_sha(&self.sha256) && &decision.validator == validator
                }))
    }

    /// Домен `pitch-accent` доверяет публикации только при положительном решении
    /// ожидаемой версии автоматического валидатора. Одобрение человека не
    /// заменяет и не переписывает это решение.
    pub(crate) fn is_trusted_for_automated_validation(
        &self,
        validator: &ValidatorIdentity,
    ) -> bool {
        self.effective_status() == Some(SemanticStatus::Verified)
            && self.validation.as_ref().is_some_and(|decision| {
                decision.status == SemanticStatus::Verified
                    && decision.is_valid_for_sha(&self.sha256)
                    && &decision.validator == validator
            })
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Канонический постоянный манифест.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    /// Политика и домен, закреплённые за этим корнем хранилища.
    /// Отсутствует только в старых схемах 3/4 и заполняется при открытии с изменениями.
    #[serde(default)]
    pub domain_id: String,
    pub store_id: String,
    pub revision: u64,
    pub assets: Vec<AssetRecord>,
}

impl Manifest {
    /// Пустое состояние нового хранилища, привязанное к политике конкретного домена.
    pub fn empty_for_domain(store_id: String, domain_id: impl Into<String>) -> Self {
        Self {
            schema_version: MANIFEST_SCHEMA_VERSION,
            domain_id: domain_id.into(),
            store_id,
            revision: 0,
            assets: Vec::new(),
        }
    }
}
