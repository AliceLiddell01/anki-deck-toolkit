//! Стабильные машиночитаемые ошибки общего слоя хранилища ресурсов.

use thiserror::Error;

/// Коды ошибок, не зависящие от русской диагностики.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidIdentity,
    InvalidValidatorIdentity,
    InvalidStoreRoot,
    BoundaryViolation,
    StoreMissing,
    StoreNotOwned,
    UnsupportedSchemaVersion,
    ManifestCorrupt,
    ManifestMissing,
    PathTraversal,
    UnexpectedPath,
    MissingAssetFile,
    IntegrityMismatch,
    IdentityConflict,
    SourceMissing,
    SourceNotRegular,
    InvalidTransition,
    InvalidValidationEvidence,
    IoFailure,
    JournalTooLarge,
    ValidatorFailure,
}

impl ErrorCode {
    /// Стабильное имя кода в формате `snake_case`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidIdentity => "invalid_identity",
            Self::InvalidValidatorIdentity => "invalid_validator_identity",
            Self::InvalidStoreRoot => "invalid_store_root",
            Self::BoundaryViolation => "boundary_violation",
            Self::StoreMissing => "store_missing",
            Self::StoreNotOwned => "store_not_owned",
            Self::UnsupportedSchemaVersion => "unsupported_schema_version",
            Self::ManifestCorrupt => "manifest_corrupt",
            Self::ManifestMissing => "manifest_missing",
            Self::PathTraversal => "path_traversal",
            Self::UnexpectedPath => "unexpected_path",
            Self::MissingAssetFile => "missing_asset_file",
            Self::IntegrityMismatch => "integrity_mismatch",
            Self::IdentityConflict => "identity_conflict",
            Self::SourceMissing => "source_file_missing",
            Self::SourceNotRegular => "source_not_regular",
            Self::InvalidTransition => "invalid_transition",
            Self::InvalidValidationEvidence => "invalid_validation_evidence",
            Self::IoFailure => "io_failure",
            Self::JournalTooLarge => "journal_too_large",
            Self::ValidatorFailure => "validator_failure",
        }
    }

    /// Код завершения процесса: 0 — успех или отсутствие изменений, 3 —
    /// блокировка или ошибка ввода, 4 — нарушение целостности, границы или схемы,
    /// 5 — ошибка ввода-вывода или валидатора.
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::InvalidIdentity
            | Self::InvalidValidatorIdentity
            | Self::IdentityConflict
            | Self::SourceMissing
            | Self::SourceNotRegular
            | Self::InvalidTransition
            | Self::InvalidValidationEvidence => 3,
            Self::InvalidStoreRoot
            | Self::BoundaryViolation
            | Self::StoreMissing
            | Self::StoreNotOwned
            | Self::UnsupportedSchemaVersion
            | Self::ManifestCorrupt
            | Self::ManifestMissing
            | Self::PathTraversal
            | Self::UnexpectedPath
            | Self::MissingAssetFile
            | Self::IntegrityMismatch
            | Self::JournalTooLarge => 4,
            Self::IoFailure | Self::ValidatorFailure => 5,
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ошибка хранилища с устойчивым кодом и человекочитаемым пояснением.
#[derive(Debug, Error)]
#[error("{code}: {message}")]
pub struct AssetError {
    pub code: ErrorCode,
    pub message: String,
    pub details: serde_json::Value,
}

impl AssetError {
    /// Создаёт доменную ошибку.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: serde_json::json!({}),
        }
    }

    /// Ошибка с дополнительными стабильными машиночитаемыми данными.
    pub fn with_details(
        code: ErrorCode,
        message: impl Into<String>,
        details: serde_json::Value,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            details,
        }
    }

    /// Упаковывает ошибку файловой системы с устойчивым кодом.
    pub fn io(context: impl Into<String>, error: std::io::Error) -> Self {
        Self::new(ErrorCode::IoFailure, format!("{}: {error}", context.into()))
    }

    /// Код завершения процесса для CLI.
    pub const fn exit_code(&self) -> u8 {
        self.code.exit_code()
    }
}
