//! Файловое хранилище ресурсов, принадлежащих программе, с атомарным манифестом версий.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{
    AtFlags, FlockOperation, Mode, OFlags, flock, linkat, mkdirat, open, openat, renameat, unlinkat,
};
use rustix::io::Errno;
use sha2::{Digest, Sha256};

use crate::domain::{AssetDomainPolicy, GenericDomainPolicy, KanjiDomainPolicy};
use crate::error::{AssetError, ErrorCode};
use crate::hashing::{encode_lower_hex, sha256_hex};
use crate::journal::{
    JOURNAL_DIR, Journal, JournalCheck, JournalDraft, JournalEvent, JournalOperation,
    JournalOutcome, STORE_WIDE_IDENTITY, check, inspect, revision_bound, snapshot,
};
use crate::model::{
    AssetIdentity, AssetRecord, DetectedFormat, HumanAttestation, HumanDecision, LifecycleState,
    MANIFEST_SCHEMA_VERSION, Manifest, Provenance, SemanticDecision, SemanticStatus,
    ValidationRecord, ValidatorIdentity,
};
use crate::selection::SelectionMode;
use crate::validation::{SemanticValidator, ValidationAttempt, ValidationReport, ValidatorFailure};

const MANIFEST_FILE: &str = "manifest.json";
const OWNER_FILE: &str = ".owner.json";
const LOCK_FILE: &str = ".lock";
const ASSETS_DIR: &str = "assets";
const TEMP_DIR: &str = ".tmp";
const RUNTIME_DIR: &str = ".runtime";
// Локальное состояние пакетной обработки без привязки к домену. Его содержимым
// владеет соответствующий пакет; общий механизм проверяет только отдельную
// границу каталога.
const BATCHES_DIR: &str = "batches";
const REMOVAL_MARKER: &str = "removal.json";
const REMOVAL_BACKUP: &str = "removal.backup";
const TRANSITION_MARKER: &str = "transition.json";
const LAYOUT_MIGRATION_MARKER: &str = "layout-migration.json";
const OWNER_SCHEMA_VERSION: u32 = 1;
const MAX_HUMAN_APPROVED_BYTES: u64 = 8 * 1024 * 1024;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static FAIL_AFTER_REMOVAL_MANIFEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static FAIL_AFTER_CANONICAL_TRANSITION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Имитирует смерть процесса между зафиксированным изменением состояния и
    /// закрывающим событием журнала: интент остаётся висящим до сверки.
    static FAIL_BEFORE_JOURNAL_CLOSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Проверенная запись и те же байты, для которых сверена контрольная сумма.
#[derive(Debug, Clone)]
pub struct VerifiedAssetBytes {
    pub record: AssetRecord,
    pub bytes: Vec<u8>,
}

/// Параметры открытия хранилища и защищённых от пересечения каталогов.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub root: PathBuf,
    protected_roots: Vec<PathBuf>,
}

impl StoreOptions {
    /// Открывает отдельное хранилище без дополнительных защищённых корневых каталогов.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            protected_roots: Vec::new(),
        }
    }

    /// Запрещает хранилищу пересекаться с указанным пользовательским деревом.
    pub fn protect_from(mut self, path: impl Into<PathBuf>) -> Self {
        self.protected_roots.push(path.into());
        self
    }
}

/// Открытое проверяемое хранилище ресурсов.
#[derive(Debug)]
pub struct AssetStore {
    /// Канонический путь для вывода пользователю.
    root: PathBuf,
    /// Открытый дескриптор каталога: все операции ввода-вывода хранилища остаются
    /// привязаны к этому `inode`.
    root_handle: File,
    /// Локальное хранилище записей `Pending` и `Quarantined`; оно целиком исключено из `Git`.
    runtime_handle: File,
    store_id: String,
    policy: Arc<dyn AssetDomainPolicy>,
    initialized_on_open: bool,
    layout_migrated_on_open: bool,
    /// Долговременный журнал операций: дополняется только под эксклюзивной
    /// блокировкой store и никогда из worker'ов пакета.
    journal: std::sync::Mutex<Journal>,
    #[cfg(test)]
    fail_next_manifest_write: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    lock_test_hooks: std::sync::Mutex<Option<std::sync::Arc<LockTestHooks>>>,
}

#[cfg(test)]
struct LockTestHooks {
    before_lock: std::sync::Arc<dyn Fn(&File) + Send + Sync>,
    after_lock: std::sync::Arc<dyn Fn(&File) + Send + Sync>,
}

#[cfg(test)]
impl std::fmt::Debug for LockTestHooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LockTestHooks")
    }
}

/// Фактическое состояние записи в момент закрытия интента журнала.
#[derive(Debug, Clone, Default)]
struct JournalStateFacts {
    revision: Option<u64>,
    previous_sha256: Option<String>,
    next_sha256: Option<String>,
    previous_lifecycle: Option<LifecycleState>,
    next_lifecycle: Option<LifecycleState>,
    verified: Option<bool>,
}

impl JournalStateFacts {
    /// Состояние по фактическим записям: до изменения и после него.
    fn from_records(
        previous: Option<&AssetRecord>,
        next: Option<&AssetRecord>,
        revision: Option<u64>,
    ) -> Self {
        Self {
            revision,
            previous_sha256: previous.map(|record| record.sha256.clone()),
            next_sha256: next.map(|record| record.sha256.clone()),
            previous_lifecycle: previous.map(|record| record.lifecycle),
            next_lifecycle: next.map(|record| record.lifecycle),
            // Отсутствующая запись не находится в состоянии `verified`.
            verified: Some(next.is_some_and(|record| record.lifecycle == LifecycleState::Verified)),
        }
    }

    /// Состояние отказа до фиксации: прежнее состояние осталось неизменным.
    fn unchanged(intent: &JournalEvent) -> Self {
        let lifecycle = lifecycle_from_str(intent.previous_lifecycle.as_deref());
        Self {
            revision: intent.manifest_revision,
            previous_sha256: intent.previous_sha256.clone(),
            next_sha256: intent.previous_sha256.clone(),
            previous_lifecycle: lifecycle,
            next_lifecycle: lifecycle,
            verified: Some(lifecycle == Some(LifecycleState::Verified)),
        }
    }
}

/// След фактически завершённого восстановления после сбоя.
///
/// Восстановление выполняется в свободных функциях, у которых нет доступа к
/// журналу, поэтому они возвращают следы наверх: событие `Recovered` пишет
/// вызывающий метод под уже удерживаемой эксклюзивной блокировкой store.
#[derive(Debug, Clone)]
struct RecoveryFact {
    identity: AssetIdentity,
    previous_sha256: Option<String>,
    next_sha256: Option<String>,
    next_lifecycle: Option<LifecycleState>,
    reason: String,
}

impl RecoveryFact {
    fn new(
        identity: AssetIdentity,
        previous_sha256: Option<String>,
        next_sha256: Option<String>,
        next_lifecycle: Option<LifecycleState>,
        reason: &str,
    ) -> Self {
        Self {
            identity,
            previous_sha256,
            next_sha256,
            next_lifecycle,
            reason: reason.to_owned(),
        }
    }
}

fn lifecycle_from_str(value: Option<&str>) -> Option<LifecycleState> {
    match value? {
        "pending" => Some(LifecycleState::Pending),
        "verified" => Some(LifecycleState::Verified),
        "quarantined" => Some(LifecycleState::Quarantined),
        _ => None,
    }
}

/// Запрос на явный вызов `ingest` для одного указанного локального файла.
#[derive(Debug, Clone)]
pub struct IngestRequest {
    pub identity: AssetIdentity,
    pub source_path: PathBuf,
    /// Проверка исходника по CAS перед записью: байты, проверенные при ревью,
    /// не подменяются отложенным импортом.
    pub expected_source_sha256: Option<String>,
    /// Доменное расширение идентичности, которое общий слой сохраняет без
    /// интерпретации (например, символ и его кодовые точки Unicode для kanji).
    pub domain_metadata: Option<serde_json::Value>,
    /// Для явной замены требуется хеш версии, которую ожидает вызывающая сторона.
    pub replace_expected_sha256: Option<String>,
}

/// Запрос на получение, проверку и публикацию: новый объект остаётся во временной
/// области подготовки, пока семантический валидатор не вернул `verified`.
#[derive(Debug, Clone)]
pub struct VerifiedIngestRequest {
    pub identity: AssetIdentity,
    pub bytes: Vec<u8>,
    pub provenance: Provenance,
    pub domain_metadata: Option<serde_json::Value>,
    pub replace_expected_sha256: Option<String>,
}

/// Явное пользовательское решение, защищённое CAS текущего хеша.
#[derive(Debug, Clone)]
pub struct HumanAttestationRequest {
    pub identity: AssetIdentity,
    pub expected_sha256: String,
    pub decision: HumanDecision,
    pub reason: String,
}

/// Итог явного импорта.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestOutcome {
    pub asset: AssetRecord,
    pub previous: Option<AssetRecord>,
    pub changed: bool,
}

/// Итог атомарной публикации байтов, прошедших семантическую проверку.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedIngestOutcome {
    pub asset: Option<AssetRecord>,
    pub status: SemanticStatus,
    pub evidence: Vec<crate::model::ValidationEvidence>,
    pub sha256: String,
    pub byte_length: u64,
    pub changed: bool,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerMarker {
    schema_version: u32,
    store_id: String,
}

#[derive(Debug)]
struct TempArtifact {
    directory: File,
    name: OsString,
}

impl Drop for TempArtifact {
    fn drop(&mut self) {
        let _ = unlinkat(&self.directory, &self.name, AtFlags::empty());
    }
}

#[derive(Debug)]
struct StagedObject {
    artifact: TempArtifact,
    sha256: String,
    byte_length: u64,
    format: DetectedFormat,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationObject {
    storage_path: String,
    sha256: String,
    format: DetectedFormat,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PublicationTransaction {
    schema_version: u32,
    identity: AssetIdentity,
    previous: Option<PublicationObject>,
    next: PublicationObject,
    staged_name: String,
    backup_name: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RemovalTransaction {
    schema_version: u32,
    record: AssetRecord,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionTransaction {
    schema_version: u32,
    identity: AssetIdentity,
    target: TransitionTarget,
    sha256: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutMigrationTransaction {
    schema_version: u32,
    store_id: String,
    domain_id: String,
    source_schema_version: u32,
    entries: Vec<LayoutMigrationEntry>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LayoutMigrationEntry {
    identity: AssetIdentity,
    sha256: String,
    format: DetectedFormat,
    old_path: String,
    new_path: String,
    consumer_filename: String,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransitionTarget {
    Canonical,
    Runtime,
}

#[derive(Debug)]
struct PendingPublication {
    marker_name: OsString,
    transaction: PublicationTransaction,
}

#[derive(Debug)]
struct StoreLock {
    directory: File,
    lock_file: File,
}

impl StoreLock {
    fn unlock(self) -> Result<(), AssetError> {
        flock(&self.lock_file, FlockOperation::Unlock).map_err(|error| {
            AssetError::io("не удалось снять lock store", std::io::Error::from(error))
        })?;
        flock(&self.directory, FlockOperation::Unlock).map_err(|error| {
            AssetError::io(
                "не удалось снять directory lock store",
                std::io::Error::from(error),
            )
        })
    }
}

impl AssetStore {
    /// Открывает существующее или создаёт новое хранилище после проверки границ.
    pub fn open(options: StoreOptions) -> Result<Self, AssetError> {
        Self::open_with_policy(options, GenericDomainPolicy)
    }

    /// Открывает существующее хранилище, принадлежащее `asset-store`, не создавая новый корень.
    /// При открытии инициализирует отсутствующий `.runtime` и переносит туда
    /// прежние записи `Pending` и `Quarantined`, чтобы восстановить границу жизненного цикла.
    pub fn open_existing(options: StoreOptions) -> Result<Self, AssetError> {
        Self::open_existing_with_policy(options, GenericDomainPolicy)
    }

    /// Открывает хранилище с явной политикой домена и размещения.
    pub fn open_with_policy<P: AssetDomainPolicy + 'static>(
        options: StoreOptions,
        policy: P,
    ) -> Result<Self, AssetError> {
        Self::open_with_creation(options, Arc::new(policy), true)
    }

    /// Открывает существующее хранилище с явной политикой домена и размещения.
    pub fn open_existing_with_policy<P: AssetDomainPolicy + 'static>(
        options: StoreOptions,
        policy: P,
    ) -> Result<Self, AssetError> {
        Self::open_with_creation(options, Arc::new(policy), false)
    }

    /// Открывает или создаёт хранилище `kanji` с закреплённой политикой `kanji`.
    pub fn open_kanji(options: StoreOptions) -> Result<Self, AssetError> {
        Self::open_with_policy(options, KanjiDomainPolicy)
    }

    /// Открывает существующее хранилище `kanji`, не создавая его.
    pub fn open_kanji_existing(options: StoreOptions) -> Result<Self, AssetError> {
        Self::open_existing_with_policy(options, KanjiDomainPolicy)
    }

    fn open_with_creation(
        options: StoreOptions,
        policy: Arc<dyn AssetDomainPolicy>,
        create_if_missing: bool,
    ) -> Result<Self, AssetError> {
        let requested_root = resolve_store_root(&options.root)?;
        for protected in &options.protected_roots {
            for protected in resolve_protected_paths(protected)? {
                if paths_overlap(&requested_root, &protected) {
                    return Err(AssetError::new(
                        ErrorCode::BoundaryViolation,
                        format!(
                            "store root {} пересекается с защищённым каталогом {}",
                            requested_root.display(),
                            protected.display()
                        ),
                    ));
                }
            }
        }

        let (root_handle, root_created) = if create_if_missing {
            open_or_create_store_root(&requested_root, |_| {})?
        } else {
            (open_existing_store_root(&requested_root)?, false)
        };
        let canonical_root = fd_canonical_path(&root_handle)?;
        if canonical_root != requested_root {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "store root изменился во время открытия",
            ));
        }

        for protected in &options.protected_roots {
            for protected in resolve_protected_paths(protected)? {
                if paths_overlap(&canonical_root, &protected) {
                    return Err(AssetError::new(
                        ErrorCode::BoundaryViolation,
                        format!(
                            "store root {} пересекается с защищённым каталогом {}",
                            canonical_root.display(),
                            protected.display()
                        ),
                    ));
                }
            }
        }

        // Блокировка каталога через `flock` сериализует первоначальную проверку
        // и настройку, не оставляя файл блокировки в уже существующем корневом
        // каталоге, который хранилищу не принадлежит.
        flock(&root_handle, FlockOperation::LockExclusive).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать store root",
                std::io::Error::from(error),
            )
        })?;
        let state = preflight_root_ownership(&root_handle, root_created)?;
        if !create_if_missing && state != RootState::Owned {
            return Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                "store root не содержит инициализированный asset store",
            ));
        }
        // Для существующего хранилища сначала проверяем совместимость
        // канонического манифеста с запрошенной политикой. До этой проверки
        // нельзя создавать файл блокировки, `.runtime` или восстанавливать
        // транзакции: открытие чужого хранилища не должно менять его состояние.
        if state == RootState::Owned {
            let manifest = load_manifest(&root_handle)?;
            ensure_store_domain(&manifest, policy.as_ref())?;
        }
        let lock = match state {
            RootState::NewlyCreated | RootState::ExistingEmpty => create_lock_file(&root_handle)?,
            RootState::Owned => open_lock_file(&root_handle, true)?,
        };
        flock(&lock, FlockOperation::LockExclusive).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать store",
                std::io::Error::from(error),
            )
        })?;
        let initialized_on_open = if create_if_missing {
            initialize_or_load(&root_handle, state, policy.domain_id())?
        } else {
            ensure_dir_entry(&root_handle, TEMP_DIR)?;
            false
        };

        let canonical_manifest = load_manifest(&root_handle)?;
        let store_id = canonical_manifest.store_id.clone();
        let runtime_handle = open_or_initialize_runtime(&root_handle, &store_id, policy.as_ref())?;
        // Журнал открывается до восстановления: завершённое восстановление
        // фиксируется событием `Recovered`, а перенос раскладки — `LayoutMigrated`.
        let journal = std::sync::Mutex::new(Journal::open(&root_handle)?);
        let mut recovered = recover_publications(&root_handle, policy.as_ref())?;
        recovered.extend(recover_publications(&runtime_handle, policy.as_ref())?);
        // Восстановление уже изменило байты и сняло маркеры: следы пишутся до
        // шагов, которые могут отказать, иначе повторный запуск не восстановит
        // их и журнал потеряет факт навсегда.
        write_recovery_facts(
            &journal,
            policy.domain_id(),
            &root_handle,
            &runtime_handle,
            &store_id,
            &recovered,
        )?;
        recover_layout_migration(&root_handle, policy.as_ref())?;
        recover_layout_migration(&runtime_handle, policy.as_ref())?;
        let mut manifest = load_manifest(&root_handle)?;
        let mut runtime_manifest = load_runtime_manifest(&runtime_handle, &store_id)?;
        ensure_store_domain(&manifest, policy.as_ref())?;
        ensure_store_domain(&runtime_manifest, policy.as_ref())?;
        let mut layout_migrated_on_open = false;
        if manifest.schema_version < MANIFEST_SCHEMA_VERSION {
            migrate_layout(&root_handle, &mut manifest, policy.as_ref())?;
            layout_migrated_on_open = true;
        }
        if runtime_manifest.schema_version < MANIFEST_SCHEMA_VERSION {
            migrate_layout(&runtime_handle, &mut runtime_manifest, policy.as_ref())?;
            layout_migrated_on_open = true;
        }
        if layout_migrated_on_open {
            journal_append(
                &journal,
                JournalDraft::store_wide(
                    policy.domain_id(),
                    policy.domain_id(),
                    STORE_WIDE_IDENTITY,
                    JournalOperation::LayoutMigrated,
                    JournalOutcome::Completed,
                )
                .revision(Some(journal_revision_of(
                    &root_handle,
                    &runtime_handle,
                    &store_id,
                )?)),
            )?;
        }
        manifest = load_manifest(&root_handle)?;
        runtime_manifest = load_runtime_manifest(&runtime_handle, &store_id)?;
        let reconciled = reconcile_runtime_boundary(
            &root_handle,
            &runtime_handle,
            &manifest,
            &runtime_manifest,
            policy.as_ref(),
        )?;
        write_recovery_facts(
            &journal,
            policy.domain_id(),
            &root_handle,
            &runtime_handle,
            &store_id,
            &reconciled,
        )?;
        manifest = load_manifest(&root_handle)?;
        runtime_manifest = load_runtime_manifest(&runtime_handle, &store_id)?;
        validate_verified_manifest(&root_handle, &manifest, policy.as_ref())?;
        validate_runtime_manifest(
            &runtime_handle,
            &runtime_manifest,
            &store_id,
            policy.as_ref(),
        )?;

        let store = Self {
            root: canonical_root,
            root_handle,
            runtime_handle,
            journal,
            store_id,
            policy,
            initialized_on_open,
            layout_migrated_on_open,
            #[cfg(test)]
            fail_next_manifest_write: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            lock_test_hooks: std::sync::Mutex::new(None),
        };
        flock(&lock, FlockOperation::Unlock).map_err(|error| {
            AssetError::io("не удалось снять lock store", std::io::Error::from(error))
        })?;
        flock(&store.root_handle, FlockOperation::Unlock).map_err(|error| {
            AssetError::io(
                "не удалось снять lock store root",
                std::io::Error::from(error),
            )
        })?;
        Ok(store)
    }

    /// Канонический абсолютный корневой каталог, принадлежащий программе.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Постоянный идентификатор хранилища.
    pub fn store_id(&self) -> &str {
        &self.store_id
    }

    /// Истина, если этот вызов `open` создал хранилище или завершил его первичную
    /// инициализацию.
    pub const fn initialized_on_open(&self) -> bool {
        self.initialized_on_open
    }

    /// Истина, если открытие с изменением состояния завершило перенос схемы
    /// или расположения ресурсов.
    pub const fn layout_migrated_on_open(&self) -> bool {
        self.layout_migrated_on_open
    }

    /// Истина, если открытие создало хранилище или перенесло ресурсы из старой схемы.
    pub const fn did_mutate_on_open(&self) -> bool {
        self.initialized_on_open || self.layout_migrated_on_open
    }

    /// Проверяет манифест и каждый файл, на который он ссылается.
    pub fn verify_integrity(&self) -> Result<Vec<AssetRecord>, AssetError> {
        let lock = self.lock_shared()?;
        let manifest = load_manifest(&self.root_handle)?;
        validate_verified_manifest(&self.root_handle, &manifest, self.policy.as_ref())?;
        let runtime_manifest = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        validate_runtime_manifest(
            &self.runtime_handle,
            &runtime_manifest,
            &self.store_id,
            self.policy.as_ref(),
        )?;
        let mut assets = manifest.assets;
        assets.extend(runtime_manifest.assets);
        assets.sort_by(|left, right| left.identity.cmp(&right.identity));
        lock.unlock()?;
        Ok(assets)
    }

    /// Совместимый клиент только для чтения хранилища `kanji`. Для других доменов
    /// вызывайте `read_verified_with_policy` с их явной политикой.
    pub fn read_verified(
        root: impl AsRef<Path>,
        identities: &[AssetIdentity],
        expected_validator: &ValidatorIdentity,
    ) -> Result<Vec<VerifiedAssetBytes>, AssetError> {
        Self::read_verified_with_policy(root, identities, expected_validator, &KanjiDomainPolicy)
    }

    /// Вариант только для чтения с явно указанным доменом. Он не изменяет
    /// манифест и файловую систему; хранилища v3/v4 читаются по правилам
    /// расположения старой схемы, заданным политикой домена.
    pub fn read_verified_with_policy(
        root: impl AsRef<Path>,
        identities: &[AssetIdentity],
        expected_validator: &ValidatorIdentity,
        policy: &dyn AssetDomainPolicy,
    ) -> Result<Vec<VerifiedAssetBytes>, AssetError> {
        Self::read_verified_snapshot(
            root.as_ref(),
            identities,
            expected_validator,
            policy,
            |_| {},
        )
    }

    fn read_verified_snapshot(
        root: &Path,
        identities: &[AssetIdentity],
        expected_validator: &ValidatorIdentity,
        policy: &dyn AssetDomainPolicy,
        mut before_read: impl FnMut(&AssetRecord),
    ) -> Result<Vec<VerifiedAssetBytes>, AssetError> {
        validate_validator_identity(expected_validator)?;
        let requested = resolve_store_root(root)?;
        let directory = open_existing_store_root(&requested)?;
        flock(&directory, FlockOperation::LockShared)
            .map_err(|e| AssetError::io("блокировка чтения набора изображений", e.into()))?;
        let owner = read_owner_marker(&directory)?;
        let manifest = load_manifest(&directory)?;
        if owner.store_id != manifest.store_id {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "store_id отличается от owner",
            ));
        }
        ensure_store_domain(&manifest, policy)?;
        if manifest.schema_version < MANIFEST_SCHEMA_VERSION {
            validate_legacy_verified_manifest(&directory, &manifest, policy)?;
        } else {
            validate_verified_manifest(&directory, &manifest, policy)?;
        }
        let mut result = Vec::new();
        for identity in identities {
            policy.validate_identity(identity)?;
            let record = manifest
                .assets
                .iter()
                .find(|a| &a.identity == identity)
                .ok_or_else(|| {
                    AssetError::with_details(
                        ErrorCode::MissingAssetFile,
                        "в каноническом хранилище нет проверенного изображения",
                        serde_json::json!({"identity": identity}),
                    )
                })?;
            if !is_trusted_for_policy(record, expected_validator, policy) {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "ресурс не имеет доверенного решения `verified` от ожидаемой версии валидатора",
                ));
            }
            let mut record = record.clone();
            if record.consumer_filename.is_empty() {
                record.consumer_filename = policy
                    .legacy_location(&record.identity, &record.sha256, record.format)
                    .ok_or_else(|| {
                        AssetError::new(
                            ErrorCode::UnsupportedSchemaVersion,
                            "legacy manifest не имеет consumer filename для этой domain policy",
                        )
                    })?
                    .consumer_filename;
            }
            validate_record_consumer_filename(&record)?;
            before_read(&record);
            let human_approved = record.current_human_decision() == Some(HumanDecision::Approve);
            let maximum = policy.max_asset_bytes().unwrap_or(MAX_HUMAN_APPROVED_BYTES);
            if human_approved && record.byte_length > maximum {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "изображение, одобренное человеком, превышает установленный предел размера",
                ));
            }
            let file = if manifest.schema_version < MANIFEST_SCHEMA_VERSION {
                checked_legacy_asset_file(&directory, &record, policy)?
            } else {
                checked_asset_file(&directory, &record, policy)?
            };
            let bytes = if human_approved {
                read_bounded_asset_bytes(file, maximum as usize, "чтение проверенных байтов")?
            } else {
                let mut bytes = Vec::new();
                file.take(record.byte_length.saturating_add(1))
                    .read_to_end(&mut bytes)
                    .map_err(|error| AssetError::io("чтение проверенных байтов", error))?;
                bytes
            };
            let format = DetectedFormat::from_signature(&bytes);
            if format != record.format
                || bytes.len() as u64 != record.byte_length
                || sha256_hex(&bytes) != record.sha256
            {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "прочитанные байты не совпадают с проверенной записью",
                ));
            }
            if human_approved {
                validate_image_decode(&bytes)?;
            }
            result.push(VerifiedAssetBytes { record, bytes });
        }
        Ok(result)
    }

    /// Проверяет публикуемое в `Git` представление корпуса кандзи без создания,
    /// восстановления или изменения файлов. Отсутствующий корпус допустим.
    pub fn verify_publishable_corpus(
        root: impl AsRef<Path>,
        expected_validator: &ValidatorIdentity,
    ) -> Result<(), AssetError> {
        Self::verify_publishable_corpus_with_policy(root, &KanjiDomainPolicy, expected_validator)
    }

    /// Проверяет публикуемый корпус выбранного домена без создания и изменения файлов.
    /// Отсутствующий корпус допустим.
    pub fn verify_publishable_corpus_with_policy(
        root: impl AsRef<Path>,
        policy: &dyn AssetDomainPolicy,
        expected_validator: &ValidatorIdentity,
    ) -> Result<(), AssetError> {
        validate_validator_identity(expected_validator)?;
        let requested_root = resolve_store_root(root.as_ref())?;
        let root_handle = match open_existing_store_root(&requested_root) {
            Ok(root) => root,
            Err(error) if error.code == ErrorCode::StoreMissing => return Ok(()),
            Err(error) => return Err(error),
        };
        // Все изменяющие хранилище операции сначала берут исключительную
        // блокировку каталога через `flock`. Эта проверка только для чтения берёт
        // совместную блокировку того же `inode`; `.lock` игнорируется `Git` и может
        // отсутствовать в чистой копии репозитория.
        let directory_lock = open_directory_at(&root_handle, ".").map_err(|error| {
            AssetError::io("не удалось открыть каталог публикуемого корпуса", error)
        })?;
        flock(&directory_lock, FlockOperation::LockShared).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать каталог публикуемого корпуса для проверки",
                std::io::Error::from(error),
            )
        })?;
        let result = (|| {
            let names = inspect_top_level(&root_handle, ErrorCode::UnexpectedPath)?;
            if !names.contains(OWNER_FILE) || !names.contains(MANIFEST_FILE) {
                return Err(AssetError::new(
                    ErrorCode::StoreNotOwned,
                    "в публикуемом корпусе отсутствует файл `.owner.json` или `manifest.json`",
                ));
            }
            let owner = read_owner_marker(&root_handle)?;
            let manifest = load_manifest(&root_handle)?;
            if manifest.store_id != owner.store_id {
                return Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    "значение `store_id` в публикуемом манифесте не совпадает с маркером владельца",
                ));
            }
            ensure_store_domain(&manifest, policy)?;
            validate_publishable_manifest(&root_handle, &manifest, expected_validator, policy)?;
            ensure_directory_empty(&root_handle, TEMP_DIR)?;
            if names.contains(RUNTIME_DIR) {
                let runtime = open_directory_at(&root_handle, RUNTIME_DIR)
                    .map_err(|error| directory_entry_error(RUNTIME_DIR, error))?;
                let runtime_manifest = load_runtime_manifest(&runtime, &manifest.store_id)?;
                validate_runtime_manifest(&runtime, &runtime_manifest, &manifest.store_id, policy)?;
                let published: BTreeSet<_> = manifest
                    .assets
                    .iter()
                    .map(|asset| &asset.identity)
                    .collect();
                if runtime_manifest
                    .assets
                    .iter()
                    .any(|asset| published.contains(&asset.identity))
                {
                    return Err(AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        "локальный кандидат пересекается с опубликованной идентичностью",
                    ));
                }
            }
            Ok(())
        })();
        let unlock = flock(&directory_lock, FlockOperation::Unlock).map_err(|error| {
            AssetError::io(
                "не удалось снять блокировку каталога публикуемого корпуса",
                std::io::Error::from(error),
            )
        });
        result?;
        unlock?;
        Ok(())
    }

    /// Явно импортирует один файл, вычисляя `SHA-256` по скопированным байтам.
    /// Повтор для той же идентичности и хеша не меняет состояние; другой хеш
    /// требует указать ожидаемый хеш.
    pub fn ingest(&self, request: IngestRequest) -> Result<IngestOutcome, AssetError> {
        self.policy.validate_identity(&request.identity)?;
        let source = open_source_file(&request.source_path)?;
        self.ingest_from_file(request, source)
    }

    /// Проверяет байты до публикации и добавляет в манифест только записи со
    /// статусом `verified`. Ошибка валидатора и любой другой статус оставляют
    /// каноническое состояние неизменным. Повтор для доверенных байтов с тем же
    /// SHA-256 и версией валидатора является успешным no-op, если не меняются
    /// `domain_metadata` и `provenance`. Если policy требует CAS для такого
    /// обновления, передайте текущий SHA-256 в `replace_expected_sha256`;
    /// иначе команда вернёт `IdentityConflict`.
    pub fn ingest_verified<V: SemanticValidator>(
        &self,
        request: VerifiedIngestRequest,
        validator: &V,
    ) -> Result<VerifiedIngestOutcome, AssetError> {
        self.policy.validate_identity(&request.identity)?;
        if request.provenance.source_kind.trim().is_empty()
            || request.provenance.source_name.trim().is_empty()
            || request.provenance.source_name.contains(['/', '\\'])
        {
            return Err(AssetError::new(
                ErrorCode::InvalidIdentity,
                "provenance должен содержать тип и имя источника без path",
            ));
        }
        let validator_id = validator.identity();
        validate_validator_identity(&validator_id)?;
        let staged = stage_bytes(&self.root_handle, &request.bytes)?;
        let lock = self.lock_exclusive()?;
        let mut recovered = recover_publications(&self.root_handle, self.policy.as_ref())?;
        recovered.extend(recover_publications(
            &self.runtime_handle,
            self.policy.as_ref(),
        )?);
        // Восстановление уже изменило байты и сняло свои маркеры: событие о нём
        // пишется до шагов, которые могут отказать, иначе повторный запуск не
        // восстановит эти следы и журнал потеряет факт навсегда.
        self.record_recovery_facts(&recovered)?;
        let mut manifest = load_manifest(&self.root_handle)?;
        let mut runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        let reconciled = reconcile_runtime_boundary(
            &self.root_handle,
            &self.runtime_handle,
            &manifest,
            &runtime_manifest,
            self.policy.as_ref(),
        )?;
        self.record_recovery_facts(&reconciled)?;
        manifest = load_manifest(&self.root_handle)?;
        runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        validate_verified_manifest(&self.root_handle, &manifest, self.policy.as_ref())?;
        validate_runtime_manifest(
            &self.runtime_handle,
            &runtime_manifest,
            &self.store_id,
            self.policy.as_ref(),
        )?;
        let existing = manifest
            .assets
            .iter()
            .find(|asset| asset.identity == request.identity)
            .or_else(|| {
                runtime_manifest
                    .assets
                    .iter()
                    .find(|asset| asset.identity == request.identity)
            })
            .cloned();
        if let Some(current) = &existing {
            if current.sha256 == staged.sha256
                && current.current_human_decision() == Some(HumanDecision::Reject)
            {
                return Err(AssetError::new(
                    ErrorCode::InvalidTransition,
                    "эти bytes явно отклонены человеком; требуется новое решение или другой hash",
                ));
            }
            let semantics = self.policy.trust_semantics();
            let same_sha_metadata_refresh = current.sha256 == staged.sha256
                && semantics.metadata_change_requires_explicit_cas()
                && (current.domain_metadata != request.domain_metadata
                    || current.provenance != request.provenance);
            if current.sha256 == staged.sha256
                && !same_sha_metadata_refresh
                && is_trusted_for_policy(current, &validator_id, self.policy.as_ref())
            {
                let outcome = VerifiedIngestOutcome {
                    asset: Some(current.clone()),
                    status: SemanticStatus::Verified,
                    evidence: current
                        .validation
                        .as_ref()
                        .map(|record| record.evidence.clone())
                        .unwrap_or_default(),
                    sha256: current.sha256.clone(),
                    byte_length: current.byte_length,
                    changed: false,
                };
                // Повторное использование уже доверенного ресурса ничего не
                // меняет, поэтому пишется одно событие без интента.
                self.record_journal(
                    JournalDraft::new(
                        self.policy.domain_id(),
                        &current.identity,
                        JournalOperation::VerifiedReused,
                        JournalOutcome::Completed,
                    )
                    .revision(Some(self.journal_revision()?))
                    .sha(Some(current.sha256.as_str()), Some(current.sha256.as_str()))
                    .lifecycle(Some(current.lifecycle), Some(current.lifecycle))
                    .verified(Some(current.lifecycle == LifecycleState::Verified))
                    .provenance(Some(&current.provenance)),
                )?;
                drop(staged);
                lock.unlock()?;
                return Ok(outcome);
            }
            if (current.sha256 != staged.sha256 || same_sha_metadata_refresh)
                && request.replace_expected_sha256.as_deref() != Some(current.sha256.as_str())
            {
                return Err(AssetError::with_details(
                    ErrorCode::IdentityConflict,
                    if same_sha_metadata_refresh && current.sha256 == staged.sha256 {
                        format!(
                            "метаданные или сведения об источнике ресурса {} изменились; требуется точный ожидаемый SHA для обновления",
                            current.identity
                        )
                    } else {
                        format!("identity {} уже привязана к другому hash", current.identity)
                    },
                    serde_json::json!({
                        "identity": current.identity,
                        "existing_sha256": current.sha256,
                        "candidate_sha256": staged.sha256,
                        "same_sha_metadata_refresh": same_sha_metadata_refresh,
                    }),
                ));
            }
        } else if request.replace_expected_sha256.is_some() {
            return Err(AssetError::new(
                ErrorCode::IdentityConflict,
                "ожидаемый hash замены указан для отсутствующей identity",
            ));
        }

        self.policy.validate_identity(&request.identity)?;
        let location =
            self.policy
                .canonical_location(&request.identity, &staged.sha256, staged.format)?;
        let mut record = AssetRecord {
            identity: request.identity,
            storage_path: location.storage_path,
            consumer_filename: location.consumer_filename,
            sha256: staged.sha256.clone(),
            byte_length: staged.byte_length,
            format: staged.format,
            provenance: request.provenance,
            lifecycle: LifecycleState::Pending,
            validation: None,
            human_attestation: None,
            domain_metadata: request.domain_metadata,
        };
        record.human_attestation = existing
            .as_ref()
            .filter(|current| current.sha256 == record.sha256)
            .and_then(|current| current.human_attestation.clone());
        let mut candidate = std::io::Cursor::new(request.bytes.as_slice());
        let decision = validator
            .validate(&record, &mut candidate)
            .map_err(|failure| {
                let blocker = stable_failure_code(&failure);
                AssetError::with_details(
                    ErrorCode::ValidatorFailure,
                    failure.message,
                    serde_json::json!({ "blocker": blocker }),
                )
            })?;
        validate_decision(&decision)?;
        if decision.status != SemanticStatus::Verified {
            let outcome = VerifiedIngestOutcome {
                asset: None,
                status: decision.status,
                evidence: decision.evidence.clone(),
                sha256: staged.sha256.clone(),
                byte_length: staged.byte_length,
                changed: false,
            };
            drop(staged);
            lock.unlock()?;
            return Ok(outcome);
        }
        if !self.policy.allows_verified_format(staged.format) {
            return Err(AssetError::new(
                ErrorCode::InvalidTransition,
                format!(
                    "формат {:?} не может получить verified lifecycle в domain {}",
                    staged.format,
                    self.policy.domain_id()
                ),
            ));
        }
        record.lifecycle = LifecycleState::Verified;
        record.validation = Some(ValidationRecord {
            status: SemanticStatus::Verified,
            validator: validator_id,
            content_sha256: staged.sha256.clone(),
            evidence: decision.evidence.clone(),
        });
        let previous_canonical = manifest
            .assets
            .iter()
            .find(|asset| asset.identity == record.identity)
            .cloned();
        let previous_runtime = runtime_manifest
            .assets
            .iter()
            .find(|asset| asset.identity == record.identity)
            .cloned();
        let published_identity = record.identity.clone();
        let previous_state = previous_canonical
            .clone()
            .or_else(|| previous_runtime.clone());
        let published_record = record.clone();
        let intent = JournalDraft::new(
            self.policy.domain_id(),
            &published_identity,
            JournalOperation::VerifiedPublished,
            JournalOutcome::Pending,
        )
        .revision(Some(self.journal_revision()?))
        .sha(
            previous_state.as_ref().map(|asset| asset.sha256.as_str()),
            Some(record.sha256.as_str()),
        )
        .lifecycle(
            previous_state.as_ref().map(|asset| asset.lifecycle),
            Some(LifecycleState::Verified),
        )
        .verified(Some(true))
        .provenance(Some(&record.provenance));
        self.journal_transition(
            intent,
            JournalOperation::VerifiedPublished,
            &published_identity,
            || {
                if previous_runtime.is_some() {
                    begin_transition(
                        &self.root_handle,
                        &record.identity,
                        TransitionTarget::Canonical,
                        &record.sha256,
                    )?;
                }
                commit_asset_record(
                    &self.root_handle,
                    &staged,
                    previous_canonical.as_ref(),
                    &record,
                    &mut manifest,
                    self.policy.as_ref(),
                    |root, manifest| {
                        #[cfg(test)]
                        {
                            save_manifest_with_test_hook(
                                root,
                                manifest,
                                false,
                                &self.fail_next_manifest_write,
                            )
                        }
                        #[cfg(not(test))]
                        {
                            save_manifest(root, manifest, false)
                        }
                    },
                )?;
                if let Some(candidate) = &previous_runtime {
                    #[cfg(test)]
                    if FAIL_AFTER_CANONICAL_TRANSITION.with(|hook| hook.replace(false)) {
                        return Err(AssetError::new(
                            ErrorCode::IoFailure,
                            "тестовый сбой после canonical commit",
                        ));
                    }
                    remove_record_from_area(
                        &self.runtime_handle,
                        &mut runtime_manifest,
                        candidate,
                        self.policy.as_ref(),
                    )?;
                    clear_transition(&self.root_handle)?;
                    // Запись переместилась из `.runtime` в канонический манифест:
                    // она покинула локальную область.
                    self.record_removal(
                        candidate,
                        "локальная запись снята после публикации в канонический манифест",
                    )?;
                }
                Ok((
                    (),
                    JournalStateFacts::from_records(
                        previous_state.as_ref(),
                        Some(&published_record),
                        Some(self.journal_revision()?),
                    ),
                ))
            },
        )?;
        let sha256 = staged.sha256.clone();
        let byte_length = staged.byte_length;
        drop(staged);
        lock.unlock()?;
        Ok(VerifiedIngestOutcome {
            asset: Some(record),
            status: SemanticStatus::Verified,
            evidence: decision.evidence,
            sha256,
            byte_length,
            changed: true,
        })
    }

    /// Импортирует уже открытый и проверенный дескриптор исходного файла через `CLI`.
    pub(crate) fn ingest_from_file(
        &self,
        request: IngestRequest,
        source: File,
    ) -> Result<IngestOutcome, AssetError> {
        self.policy.validate_identity(&request.identity)?;
        if let Some(expected) = &request.expected_source_sha256 {
            validate_hash(expected)?;
        }
        let staged = stage_source(&self.runtime_handle, source)?;
        if request
            .expected_source_sha256
            .as_ref()
            .is_some_and(|expected| expected != &staged.sha256)
        {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "source bytes изменились после exact candidate review",
            ));
        }
        let lock = self.lock_exclusive()?;
        let mut recovered = recover_publications(&self.root_handle, self.policy.as_ref())?;
        recovered.extend(recover_publications(
            &self.runtime_handle,
            self.policy.as_ref(),
        )?);
        // Восстановление уже изменило байты и сняло свои маркеры: событие о нём
        // пишется до шагов, которые могут отказать, иначе повторный запуск не
        // восстановит эти следы и журнал потеряет факт навсегда.
        self.record_recovery_facts(&recovered)?;
        let mut manifest = load_manifest(&self.root_handle)?;
        let mut runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        let reconciled = reconcile_runtime_boundary(
            &self.root_handle,
            &self.runtime_handle,
            &manifest,
            &runtime_manifest,
            self.policy.as_ref(),
        )?;
        self.record_recovery_facts(&reconciled)?;
        manifest = load_manifest(&self.root_handle)?;
        runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        validate_verified_manifest(&self.root_handle, &manifest, self.policy.as_ref())?;
        validate_runtime_manifest(
            &self.runtime_handle,
            &runtime_manifest,
            &self.store_id,
            self.policy.as_ref(),
        )?;
        let source_name = request
            .source_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("unnamed")
            .to_owned();

        let existing_runtime = runtime_manifest
            .assets
            .iter()
            .find(|asset| asset.identity == request.identity)
            .cloned();
        let existing = existing_runtime.clone().or_else(|| {
            manifest
                .assets
                .iter()
                .find(|asset| asset.identity == request.identity)
                .cloned()
        });

        if let Some(existing) = &existing {
            if existing.sha256 == staged.sha256 {
                drop(staged);
                lock.unlock()?;
                return Ok(IngestOutcome {
                    asset: existing.clone(),
                    previous: Some(existing.clone()),
                    changed: false,
                });
            }
            if request.replace_expected_sha256.as_deref() != Some(existing.sha256.as_str()) {
                return Err(AssetError::with_details(
                    ErrorCode::IdentityConflict,
                    format!(
                        "identity {} уже привязана к {}; для явной замены укажите этот hash",
                        existing.identity, existing.sha256
                    ),
                    serde_json::json!({
                        "identity": existing.identity,
                        "existing_sha256": existing.sha256,
                        "candidate_sha256": staged.sha256,
                    }),
                ));
            }
        } else if request.replace_expected_sha256.is_some() {
            return Err(AssetError::with_details(
                ErrorCode::IdentityConflict,
                "ожидаемый hash замены указан, но identity ещё отсутствует",
                serde_json::json!({
                    "identity": request.identity,
                    "existing_sha256": serde_json::Value::Null,
                    "candidate_sha256": staged.sha256,
                }),
            ));
        }

        let location =
            self.policy
                .canonical_location(&request.identity, &staged.sha256, staged.format)?;
        let record = AssetRecord {
            identity: request.identity,
            storage_path: location.storage_path,
            consumer_filename: location.consumer_filename,
            sha256: staged.sha256.clone(),
            byte_length: staged.byte_length,
            format: staged.format,
            provenance: Provenance {
                source_kind: "local_import".to_owned(),
                source_name,
            },
            lifecycle: LifecycleState::Pending,
            validation: None,
            human_attestation: None,
            domain_metadata: request.domain_metadata,
        };
        let previous_canonical = manifest
            .assets
            .iter()
            .find(|asset| asset.identity == record.identity)
            .cloned();
        let candidate_identity = record.identity.clone();
        let candidate_record = record.clone();
        let intent = JournalDraft::new(
            self.policy.domain_id(),
            &candidate_identity,
            JournalOperation::CandidateWritten,
            JournalOutcome::Pending,
        )
        .revision(Some(self.journal_revision()?))
        .sha(
            existing.as_ref().map(|asset| asset.sha256.as_str()),
            Some(record.sha256.as_str()),
        )
        .lifecycle(
            existing.as_ref().map(|asset| asset.lifecycle),
            Some(LifecycleState::Pending),
        )
        .verified(Some(false))
        .provenance(Some(&record.provenance));
        let previous_state = existing.clone();
        self.journal_transition(
            intent,
            JournalOperation::CandidateWritten,
            &candidate_identity,
            || {
                if previous_canonical.is_some() {
                    begin_transition(
                        &self.root_handle,
                        &record.identity,
                        TransitionTarget::Runtime,
                        &record.sha256,
                    )?;
                }
                commit_asset_record(
                    &self.runtime_handle,
                    &staged,
                    existing_runtime.as_ref(),
                    &record,
                    &mut runtime_manifest,
                    self.policy.as_ref(),
                    |root, manifest| {
                        #[cfg(test)]
                        {
                            save_manifest_with_test_hook(
                                root,
                                manifest,
                                false,
                                &self.fail_next_manifest_write,
                            )
                        }
                        #[cfg(not(test))]
                        {
                            save_manifest(root, manifest, false)
                        }
                    },
                )?;
                if let Some(canonical) = &previous_canonical {
                    remove_record_from_area(
                        &self.root_handle,
                        &mut manifest,
                        canonical,
                        self.policy.as_ref(),
                    )?;
                    clear_transition(&self.root_handle)?;
                    // Вытесненная кандидатом опубликованная запись покинула
                    // канонический манифест: её байты в хранилище больше не
                    // зарегистрированы.
                    self.record_removal(
                        canonical,
                        "опубликованная запись вытеснена новым кандидатом",
                    )?;
                }
                Ok((
                    (),
                    JournalStateFacts::from_records(
                        previous_state.as_ref(),
                        Some(&candidate_record),
                        Some(self.journal_revision()?),
                    ),
                ))
            },
        )?;
        validate_verified_manifest(&self.root_handle, &manifest, self.policy.as_ref())?;
        validate_runtime_manifest(
            &self.runtime_handle,
            &runtime_manifest,
            &self.store_id,
            self.policy.as_ref(),
        )?;
        drop(staged);
        lock.unlock()?;
        Ok(IngestOutcome {
            asset: record,
            previous: existing,
            changed: true,
        })
    }

    /// Выбирает `new` или `full` из общего манифеста без запуска валидатора.
    pub fn select(
        &self,
        mode: SelectionMode,
        validator: &ValidatorIdentity,
    ) -> Result<Vec<AssetRecord>, AssetError> {
        validate_validator_identity(validator)?;
        let lock = self.lock_shared()?;
        let manifest = load_manifest(&self.root_handle)?;
        validate_verified_manifest(&self.root_handle, &manifest, self.policy.as_ref())?;
        let runtime_manifest = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        validate_runtime_manifest(
            &self.runtime_handle,
            &runtime_manifest,
            &self.store_id,
            self.policy.as_ref(),
        )?;
        let mut records = manifest.assets;
        records.extend(runtime_manifest.assets);
        let assets = select_assets_for_policy(&records, mode, validator, self.policy.as_ref())
            .into_iter()
            .cloned()
            .collect();
        lock.unlock()?;
        Ok(assets)
    }

    /// Запускает переданный семантический валидатор для выбранных ресурсов.
    /// Каждое изменённое решение публикуется отдельной атомарной записью:
    /// сбой поздней записи не откатывает уже сохранённые решения. При
    /// техническом отказе конкретный ресурс остаётся без нового решения и не
    /// получает статус `verified`; успешные решения других ресурсов могут
    /// сохраниться.
    pub fn validate<V: SemanticValidator>(
        &self,
        mode: SelectionMode,
        validator: &V,
    ) -> Result<ValidationReport, AssetError> {
        self.validate_selection(mode, validator, None)
    }

    /// Проверяет одну идентичность и хеш и сохраняет исходные свидетельства
    /// автоматической проверки для одной записи. Область действия не включает
    /// неразрешённые варианты из других пакетов.
    pub fn validate_exact<V: SemanticValidator>(
        &self,
        identity: &AssetIdentity,
        expected_sha256: &str,
        validator: &V,
    ) -> Result<ValidationReport, AssetError> {
        identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::InvalidIdentity, message))?;
        validate_hash(expected_sha256)?;
        self.validate_selection(
            SelectionMode::Full,
            validator,
            Some((identity, expected_sha256)),
        )
    }

    fn validate_selection<V: SemanticValidator>(
        &self,
        mode: SelectionMode,
        validator: &V,
        exact: Option<(&AssetIdentity, &str)>,
    ) -> Result<ValidationReport, AssetError> {
        let validator_id = validator.identity();
        validate_validator_identity(&validator_id)?;
        let lock = self.lock_exclusive()?;
        let mut recovered = recover_publications(&self.root_handle, self.policy.as_ref())?;
        recovered.extend(recover_publications(
            &self.runtime_handle,
            self.policy.as_ref(),
        )?);
        // Восстановление уже изменило байты и сняло свои маркеры: событие о нём
        // пишется до шагов, которые могут отказать, иначе повторный запуск не
        // восстановит эти следы и журнал потеряет факт навсегда.
        self.record_recovery_facts(&recovered)?;
        let mut manifest = load_manifest(&self.root_handle)?;
        let mut runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        let reconciled = reconcile_runtime_boundary(
            &self.root_handle,
            &self.runtime_handle,
            &manifest,
            &runtime_manifest,
            self.policy.as_ref(),
        )?;
        self.record_recovery_facts(&reconciled)?;
        manifest = load_manifest(&self.root_handle)?;
        runtime_manifest = load_runtime_manifest(&self.runtime_handle, &manifest.store_id)?;
        validate_verified_manifest(&self.root_handle, &manifest, self.policy.as_ref())?;
        validate_runtime_manifest(
            &self.runtime_handle,
            &runtime_manifest,
            &self.store_id,
            self.policy.as_ref(),
        )?;
        let mut records = manifest.assets.clone();
        records.extend(runtime_manifest.assets.clone());
        let selected: Vec<_> =
            select_assets_for_policy(&records, mode, &validator_id, self.policy.as_ref())
                .into_iter()
                .cloned()
                .collect();
        let selected = if let Some((identity, expected_sha256)) = exact {
            let record = selected
                .into_iter()
                .find(|record| &record.identity == identity)
                .ok_or_else(|| {
                    AssetError::new(
                        ErrorCode::MissingAssetFile,
                        "exact validation identity отсутствует",
                    )
                })?;
            if record.sha256 != expected_sha256 {
                return Err(AssetError::new(
                    ErrorCode::IdentityConflict,
                    "exact validation candidate изменился",
                ));
            }
            vec![record]
        } else {
            selected
        };
        let mut report = ValidationReport::new(mode, validator_id.clone());
        report.considered = selected.len();

        // Сначала выполняются все вызовы предметной проверки и проверяются
        // доказательства (`evidence`). До этого места манифесты и опубликованные данные не меняются.
        let mut decisions: Vec<(AssetRecord, SemanticDecision)> = Vec::new();
        for record in selected {
            let area = if record.lifecycle == LifecycleState::Verified {
                &self.root_handle
            } else {
                &self.runtime_handle
            };
            let mut file = checked_asset_file(area, &record, self.policy.as_ref())?;
            let original_state = record.lifecycle;
            match validator.validate(&record, &mut file) {
                Ok(decision) => {
                    validate_decision(&decision)?;
                    decisions.push((record, decision));
                }
                Err(failure) => {
                    let code = stable_failure_code(&failure);
                    report.blockers.push(code.clone());
                    report.attempts.push(ValidationAttempt {
                        identity: record.identity,
                        from_state: original_state,
                        to_state: original_state,
                        content_sha256: record.sha256,
                        status: None,
                        evidence: Vec::new(),
                        changed: false,
                        blocker: Some(code),
                        blocker_message: Some(failure.message),
                    });
                }
            }
        }

        for (old_record, decision) in decisions {
            let validation = ValidationRecord {
                status: decision.status,
                validator: validator_id.clone(),
                content_sha256: old_record.sha256.clone(),
                evidence: decision.evidence.clone(),
            };
            let mut new_record = old_record.clone();
            new_record.validation = Some(validation);
            let lifecycle = if new_record.effective_status() == Some(SemanticStatus::Verified) {
                LifecycleState::Verified
            } else {
                LifecycleState::Quarantined
            };
            new_record.lifecycle = lifecycle;
            let changed = new_record.lifecycle != old_record.lifecycle
                || new_record.validation != old_record.validation;
            if changed {
                let operation = if new_record.lifecycle == LifecycleState::Quarantined {
                    JournalOperation::Quarantined
                } else {
                    JournalOperation::Revalidated
                };
                let identity = new_record.identity.clone();
                let intent = JournalDraft::new(
                    self.policy.domain_id(),
                    &identity,
                    operation,
                    JournalOutcome::Pending,
                )
                .revision(Some(self.journal_revision()?))
                .sha(
                    Some(old_record.sha256.as_str()),
                    Some(new_record.sha256.as_str()),
                )
                .lifecycle(Some(old_record.lifecycle), Some(new_record.lifecycle))
                .verified(Some(new_record.lifecycle == LifecycleState::Verified))
                .reason(&format!("решение валидатора {}", decision.status.as_str()));
                self.journal_transition(intent, operation, &identity, || {
                    self.commit_lifecycle_change(
                        &old_record,
                        &new_record,
                        &mut manifest,
                        &mut runtime_manifest,
                    )?;
                    Ok((
                        (),
                        JournalStateFacts::from_records(
                            Some(&old_record),
                            Some(&new_record),
                            Some(self.journal_revision()?),
                        ),
                    ))
                })?;
            }
            report.attempts.push(ValidationAttempt {
                identity: old_record.identity,
                from_state: old_record.lifecycle,
                to_state: lifecycle,
                content_sha256: old_record.sha256,
                status: Some(decision.status),
                evidence: decision.evidence,
                changed,
                blocker: None,
                blocker_message: None,
            });
        }

        report
            .attempts
            .sort_by(|left, right| left.identity.cmp(&right.identity));
        report.blockers.sort();
        report.blockers.dedup();
        report.changed = report
            .attempts
            .iter()
            .filter(|attempt| attempt.changed)
            .count();
        validate_verified_manifest(
            &self.root_handle,
            &load_manifest(&self.root_handle)?,
            self.policy.as_ref(),
        )?;
        validate_runtime_manifest(
            &self.runtime_handle,
            &runtime_manifest,
            &self.store_id,
            self.policy.as_ref(),
        )?;
        lock.unlock()?;
        Ok(report)
    }

    /// Применяет явное решение человека к текущим точным байтам. Подтверждение
    /// требует полной проверки декодером, затем использует тот же атомарный
    /// переход жизненного цикла, что автоматическая проверка. Её данные сохраняются.
    pub fn attest(&self, request: HumanAttestationRequest) -> Result<IngestOutcome, AssetError> {
        self.policy.validate_identity(&request.identity)?;
        validate_hash(&request.expected_sha256)?;
        if request.reason.trim().is_empty() {
            return Err(AssetError::new(
                ErrorCode::InvalidValidationEvidence,
                "решение человека требует явного основания",
            ));
        }
        let lock = self.lock_exclusive()?;
        let mut recovered = recover_publications(&self.root_handle, self.policy.as_ref())?;
        recovered.extend(recover_publications(
            &self.runtime_handle,
            self.policy.as_ref(),
        )?);
        // Восстановление уже изменило байты и сняло свои маркеры: событие о нём
        // пишется до шагов, которые могут отказать, иначе повторный запуск не
        // восстановит эти следы и журнал потеряет факт навсегда.
        self.record_recovery_facts(&recovered)?;
        let mut manifest = load_manifest(&self.root_handle)?;
        let mut runtime_manifest = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        let reconciled = reconcile_runtime_boundary(
            &self.root_handle,
            &self.runtime_handle,
            &manifest,
            &runtime_manifest,
            self.policy.as_ref(),
        )?;
        self.record_recovery_facts(&reconciled)?;
        manifest = load_manifest(&self.root_handle)?;
        runtime_manifest = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        validate_verified_manifest(&self.root_handle, &manifest, self.policy.as_ref())?;
        validate_runtime_manifest(
            &self.runtime_handle,
            &runtime_manifest,
            &self.store_id,
            self.policy.as_ref(),
        )?;
        let previous = manifest
            .assets
            .iter()
            .chain(&runtime_manifest.assets)
            .find(|asset| asset.identity == request.identity)
            .cloned()
            .ok_or_else(|| {
                AssetError::new(ErrorCode::MissingAssetFile, "identity отсутствует в store")
            })?;
        if previous.sha256 != request.expected_sha256 {
            return Err(AssetError::new(
                ErrorCode::IdentityConflict,
                "candidate изменился после review; требуется решение для текущего hash",
            ));
        }
        if request.decision == HumanDecision::Approve {
            let Some(automated_status) = previous.current_validation_status() else {
                return Err(AssetError::new(
                    ErrorCode::InvalidValidationEvidence,
                    "подтверждение человеком требует автоматическую запись ValidationRecord для текущего SHA-256",
                ));
            };
            if automated_status == SemanticStatus::Corrupt {
                return Err(AssetError::new(
                    ErrorCode::InvalidTransition,
                    "подтверждение человеком не может отменить статус CORRUPT; требуется исправленный кандидат",
                ));
            }
            let maximum = self
                .policy
                .max_asset_bytes()
                .unwrap_or(MAX_HUMAN_APPROVED_BYTES);
            if previous.byte_length > maximum {
                return Err(AssetError::new(
                    ErrorCode::InvalidTransition,
                    "подтверждение человеком превышает установленный предел размера изображения",
                ));
            }
            let area = if previous.lifecycle == LifecycleState::Verified {
                &self.root_handle
            } else {
                &self.runtime_handle
            };
            let bytes = read_bounded_asset_bytes(
                checked_asset_file(area, &previous, self.policy.as_ref())?,
                maximum as usize,
                "чтение кандидата для подтверждения человеком",
            )?;
            if sha256_hex(&bytes) != previous.sha256
                || bytes.len() as u64 != previous.byte_length
                || DetectedFormat::from_signature(&bytes) != previous.format
            {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "candidate изменился при чтении",
                ));
            }
            validate_image_decode(&bytes)?;
        }
        let mut record = previous.clone();
        record.human_attestation = Some(HumanAttestation {
            identity: request.identity,
            content_sha256: request.expected_sha256,
            decision: request.decision,
            reason: request.reason,
        });
        record.lifecycle = if record.effective_status() == Some(SemanticStatus::Verified) {
            LifecycleState::Verified
        } else {
            LifecycleState::Quarantined
        };
        if record.lifecycle == LifecycleState::Verified
            && !self.policy.allows_verified_format(record.format)
        {
            return Err(AssetError::new(
                ErrorCode::InvalidTransition,
                format!(
                    "формат {:?} не может получить verified lifecycle в domain {}",
                    record.format,
                    self.policy.domain_id()
                ),
            ));
        }
        let changed = record != previous;
        if changed {
            let operation = match request.decision {
                HumanDecision::Approve => JournalOperation::TrustGranted,
                HumanDecision::Reject => JournalOperation::TrustRevoked,
            };
            let identity = record.identity.clone();
            let reason = record
                .human_attestation
                .as_ref()
                .map(|attestation| attestation.reason.clone())
                .unwrap_or_default();
            let previous_record = previous.clone();
            let next_record = record.clone();
            let intent = JournalDraft::new(
                self.policy.domain_id(),
                &identity,
                operation,
                JournalOutcome::Pending,
            )
            .revision(Some(self.journal_revision()?))
            .sha(
                Some(previous_record.sha256.as_str()),
                Some(next_record.sha256.as_str()),
            )
            .lifecycle(Some(previous_record.lifecycle), Some(next_record.lifecycle))
            .verified(Some(next_record.lifecycle == LifecycleState::Verified))
            .reason(&reason);
            self.journal_transition(intent, operation, &identity, || {
                self.commit_lifecycle_change(
                    &previous_record,
                    &next_record,
                    &mut manifest,
                    &mut runtime_manifest,
                )?;
                Ok((
                    (),
                    JournalStateFacts::from_records(
                        Some(&previous_record),
                        Some(&next_record),
                        Some(self.journal_revision()?),
                    ),
                ))
            })?;
        }
        lock.unlock()?;
        Ok(IngestOutcome {
            asset: record,
            previous: Some(previous),
            changed,
        })
    }

    fn commit_lifecycle_change(
        &self,
        old_record: &AssetRecord,
        new_record: &AssetRecord,
        manifest: &mut Manifest,
        runtime_manifest: &mut Manifest,
    ) -> Result<(), AssetError> {
        if new_record.lifecycle == LifecycleState::Verified
            && !self.policy.allows_verified_format(new_record.format)
        {
            return Err(AssetError::new(
                ErrorCode::InvalidTransition,
                format!(
                    "формат {:?} не может получить verified lifecycle в domain {}",
                    new_record.format,
                    self.policy.domain_id()
                ),
            ));
        }
        manifest.schema_version = MANIFEST_SCHEMA_VERSION;
        runtime_manifest.schema_version = MANIFEST_SCHEMA_VERSION;
        match (old_record.lifecycle, new_record.lifecycle) {
            (LifecycleState::Verified, LifecycleState::Verified) => {
                replace_manifest_record(manifest, new_record)?;
                save_manifest_revision(
                    &self.root_handle,
                    manifest,
                    #[cfg(test)]
                    Some(&self.fail_next_manifest_write),
                    #[cfg(not(test))]
                    None,
                )?;
            }
            (LifecycleState::Verified, LifecycleState::Quarantined) => {
                let source =
                    checked_asset_file(&self.root_handle, old_record, self.policy.as_ref())?;
                let staged = stage_source(&self.runtime_handle, source)?;
                begin_transition(
                    &self.root_handle,
                    &new_record.identity,
                    TransitionTarget::Runtime,
                    &new_record.sha256,
                )?;
                let previous_runtime = runtime_manifest
                    .assets
                    .iter()
                    .find(|asset| asset.identity == new_record.identity)
                    .cloned();
                commit_asset_record(
                    &self.runtime_handle,
                    &staged,
                    previous_runtime.as_ref(),
                    new_record,
                    runtime_manifest,
                    self.policy.as_ref(),
                    |root, manifest| save_manifest(root, manifest, false),
                )?;
                remove_record_from_area(
                    &self.root_handle,
                    manifest,
                    old_record,
                    self.policy.as_ref(),
                )?;
                clear_transition(&self.root_handle)?;
                self.record_removal(
                    old_record,
                    "опубликованная запись переведена в локальную область",
                )?;
            }
            (LifecycleState::Pending | LifecycleState::Quarantined, LifecycleState::Verified) => {
                let source =
                    checked_asset_file(&self.runtime_handle, old_record, self.policy.as_ref())?;
                let staged = stage_source(&self.root_handle, source)?;
                begin_transition(
                    &self.root_handle,
                    &new_record.identity,
                    TransitionTarget::Canonical,
                    &new_record.sha256,
                )?;
                let previous = manifest
                    .assets
                    .iter()
                    .find(|asset| asset.identity == new_record.identity)
                    .cloned();
                commit_asset_record(
                    &self.root_handle,
                    &staged,
                    previous.as_ref(),
                    new_record,
                    manifest,
                    self.policy.as_ref(),
                    |root, manifest| {
                        #[cfg(test)]
                        {
                            save_manifest_with_test_hook(
                                root,
                                manifest,
                                false,
                                &self.fail_next_manifest_write,
                            )
                        }
                        #[cfg(not(test))]
                        {
                            save_manifest(root, manifest, false)
                        }
                    },
                )?;
                #[cfg(test)]
                if FAIL_AFTER_CANONICAL_TRANSITION.with(|hook| hook.replace(false)) {
                    return Err(AssetError::new(
                        ErrorCode::IoFailure,
                        "тестовый сбой после canonical commit",
                    ));
                }
                remove_record_from_area(
                    &self.runtime_handle,
                    runtime_manifest,
                    old_record,
                    self.policy.as_ref(),
                )?;
                clear_transition(&self.root_handle)?;
                self.record_removal(
                    old_record,
                    "локальная запись опубликована в канонический манифест",
                )?;
            }
            (
                LifecycleState::Pending | LifecycleState::Quarantined,
                LifecycleState::Quarantined,
            ) => {
                replace_manifest_record(runtime_manifest, new_record)?;
                save_manifest_revision(&self.runtime_handle, runtime_manifest, None)?;
            }
            _ => {
                return Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    "недопустимый переход состояния жизненного цикла при проверке",
                ));
            }
        }
        Ok(())
    }

    fn lock_shared(&self) -> Result<StoreLock, AssetError> {
        self.lock(false)
    }

    fn lock_exclusive(&self) -> Result<StoreLock, AssetError> {
        self.lock(true)
    }

    fn lock(&self, exclusive: bool) -> Result<StoreLock, AssetError> {
        let directory = open_directory_at(&self.root_handle, ".")
            .map_err(|error| AssetError::io("не удалось открыть store directory handle", error))?;
        #[cfg(test)]
        let hooks = if exclusive {
            self.lock_test_hooks
                .lock()
                .expect("мьютекс тестового перехватчика блокировки не повреждён")
                .clone()
        } else {
            None
        };
        #[cfg(test)]
        if let Some(hooks) = &hooks {
            (hooks.before_lock)(&directory);
        }
        let directory_operation = if exclusive {
            FlockOperation::LockExclusive
        } else {
            FlockOperation::LockShared
        };
        flock(&directory, directory_operation).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать store directory",
                std::io::Error::from(error),
            )
        })?;
        let lock_file = open_lock_file(&self.root_handle, false)?;
        let lock_operation = if exclusive {
            FlockOperation::LockExclusive
        } else {
            FlockOperation::LockShared
        };
        flock(&lock_file, lock_operation).map_err(|error| {
            AssetError::io(
                "не удалось заблокировать store",
                std::io::Error::from(error),
            )
        })?;
        #[cfg(test)]
        if let Some(hooks) = &hooks {
            (hooks.after_lock)(&directory);
        }
        Ok(StoreLock {
            directory,
            lock_file,
        })
    }

    #[cfg(test)]
    pub(crate) fn fail_next_manifest_write(&self) {
        self.fail_next_manifest_write.store(true, Ordering::SeqCst);
    }

    /// Задаёт контекст запуска для последующих событий журнала.
    ///
    /// Контекст не является частью состояния хранилища: он только помечает
    /// события, записанные этим запуском, и не влияет на идемпотентность, потому
    /// что в ключ события не входит.
    pub fn set_journal_context(&self, run_id: Option<&str>, batch_id: Option<&str>) {
        self.journal_lock()
            .set_context(run_id.map(str::to_owned), batch_id.map(str::to_owned));
    }

    /// Блокирует журнал операций.
    ///
    /// Повреждённый мьютекс не отменяет операцию: разобранное состояние журнала
    /// сверяется с файлом по размеру и времени изменения, поэтому отдельное
    /// восстановление не требуется.
    fn journal_lock(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Дописывает событие журнала под уже удерживаемой эксклюзивной блокировкой
    /// store. Дубликат по `event_key` не является ошибкой операции.
    fn record_journal(&self, draft: JournalDraft) -> Result<(), AssetError> {
        self.journal_lock().append(&draft).map(|_| ())
    }

    /// Дописывает событие и возвращает его: `event_id` интента нужен, чтобы
    /// закрыть интент событием результата.
    fn record_journal_event(&self, draft: JournalDraft) -> Result<JournalEvent, AssetError> {
        self.journal_lock().append_and_return(&draft)
    }

    /// Пишет событие `Removed` для записи, покинувшей область хранилища.
    fn record_removal(&self, record: &AssetRecord, reason: &str) -> Result<(), AssetError> {
        let revision = self.journal_revision()?;
        self.record_journal(
            JournalDraft::new(
                self.policy.domain_id(),
                &record.identity,
                JournalOperation::Removed,
                JournalOutcome::Completed,
            )
            .revision(Some(revision))
            .sha(Some(record.sha256.as_str()), None)
            .lifecycle(Some(record.lifecycle), None)
            .verified(Some(false))
            .reason(reason),
        )
    }

    /// Фактическая суммарная ревизия канонического и `.runtime` манифестов.
    ///
    /// Сумма строго возрастает при любом изменении состояния и всегда не меньше
    /// ревизии каждой отдельной области, поэтому ею помечаются и события уровня
    /// всего хранилища.
    fn journal_revision(&self) -> Result<u64, AssetError> {
        journal_revision_of(&self.root_handle, &self.runtime_handle, &self.store_id)
    }

    /// Пишет события `Recovered` о фактически завершённом восстановлении.
    fn record_recovery_facts(&self, facts: &[RecoveryFact]) -> Result<(), AssetError> {
        write_recovery_facts(
            &self.journal,
            self.policy.domain_id(),
            &self.root_handle,
            &self.runtime_handle,
            &self.store_id,
            facts,
        )
    }

    /// Закрывает интент фактическим состоянием записи.
    fn close_journal_intent(
        &self,
        intent: &JournalEvent,
        operation: JournalOperation,
        identity: &AssetIdentity,
        outcome: JournalOutcome,
        facts: &JournalStateFacts,
        reason: Option<&str>,
    ) -> Result<(), AssetError> {
        let mut draft = JournalDraft::new(self.policy.domain_id(), identity, operation, outcome)
            .closing(&intent.event_id)
            .revision(facts.revision)
            .sha(
                facts.previous_sha256.as_deref(),
                facts.next_sha256.as_deref(),
            )
            .lifecycle(facts.previous_lifecycle, facts.next_lifecycle)
            .verified(facts.verified)
            .provenance_text(intent.provenance.as_deref());
        if let Some(reason) = reason {
            draft = draft.reason(reason);
        }
        self.record_journal(draft)
    }

    /// Совпадает ли фактическое состояние записи с прежним состоянием интента.
    ///
    /// Тело перехода может отказать уже после фиксации манифеста, поэтому исход
    /// определяется фактическими записями, а не самим фактом ошибки. Проверяются
    /// обе области: запись, оставшаяся в `.runtime` или уже попавшая в
    /// канонический манифест, — это изменение состояния, а не прежнее состояние.
    ///
    /// Переход, который не меняет ни SHA-256, ни жизненный цикл, по манифесту
    /// неотличим от отсутствия перехода: так выглядят решения валидатора и
    /// доверие человека, меняющие только `validation` и `human_attestation`.
    /// Утверждать «отказ» в этом случае нельзя — интент остаётся открытым и его
    /// разбирает [`Self::reconcile_journal`].
    fn intent_state_unchanged(
        &self,
        identity: &AssetIdentity,
        intent: &JournalEvent,
    ) -> Result<bool, AssetError> {
        if intent.previous_sha256 == intent.new_sha256
            && intent.previous_lifecycle == intent.new_lifecycle
        {
            return Ok(false);
        }
        let canonical = load_manifest(&self.root_handle)?;
        let runtime = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        let mut present = false;
        for record in canonical.assets.iter().chain(runtime.assets.iter()) {
            if record.identity.namespace != identity.namespace
                || record.identity.key != identity.key
            {
                continue;
            }
            present = true;
            if Some(record.sha256.as_str()) != intent.previous_sha256.as_deref()
                || Some(record.lifecycle.as_str()) != intent.previous_lifecycle.as_deref()
            {
                return Ok(false);
            }
        }
        if present {
            return Ok(true);
        }
        // Записи нет вовсе: это прежнее состояние только тогда, когда его и
        // описывал интент.
        Ok(intent.previous_sha256.is_none() && intent.previous_lifecycle.is_none())
    }

    /// Обрамляет изменение состояния двумя событиями журнала: интентом до него и
    /// результатом после.
    ///
    /// Штатный отказ закрывает интент событием `Refused`, поэтому обычная работа
    /// висящих интентов не оставляет. Отказ тела перехода после уже выполненной
    /// фиксации — не отказ: интент остаётся открытым, и его разбирает
    /// [`Self::reconcile_journal`]. Так же разбирается интент, оставшийся после
    /// смерти процесса между фиксацией и закрывающим событием.
    fn journal_transition<T>(
        &self,
        intent: JournalDraft,
        operation: JournalOperation,
        identity: &AssetIdentity,
        body: impl FnOnce() -> Result<(T, JournalStateFacts), AssetError>,
    ) -> Result<T, AssetError> {
        let intent_event = self.record_journal_event(intent)?;
        let result = body();
        #[cfg(test)]
        if FAIL_BEFORE_JOURNAL_CLOSE.with(|hook| hook.replace(false)) {
            return result.map(|(value, _)| value);
        }
        match result {
            Ok((value, facts)) => {
                self.close_journal_intent(
                    &intent_event,
                    operation,
                    identity,
                    JournalOutcome::Completed,
                    &facts,
                    None,
                )?;
                Ok(value)
            }
            Err(error) => {
                // Тело перехода могло отказать уже после фиксации манифеста:
                // исход определяется фактическим состоянием, а не самим фактом
                // ошибки. Отказ с прежними фактами записывается только тогда,
                // когда прежнее состояние действительно сохранилось; иначе
                // интент остаётся открытым для сверки. Ошибка чтения манифеста
                // тоже оставляет интент открытым — неизвестное состояние не
                // выдаётся за отказ.
                //
                // Ошибка записи самого журнала не подменяет исходную ошибку
                // операции: незакрытый интент разберёт сверка.
                if self
                    .intent_state_unchanged(identity, &intent_event)
                    .unwrap_or(false)
                {
                    let _ = self.close_journal_intent(
                        &intent_event,
                        operation,
                        identity,
                        JournalOutcome::Refused,
                        &JournalStateFacts::unchanged(&intent_event),
                        Some(&error.message),
                    );
                }
                Err(error)
            }
        }
    }

    /// События журнала по возрастанию `seq`.
    ///
    /// `limit` оставляет хвост журнала: последние события, а не первые.
    pub fn journal_events(
        &self,
        limit: Option<usize>,
        identity: Option<&str>,
        operation: Option<JournalOperation>,
    ) -> Result<Vec<JournalEvent>, AssetError> {
        let lock = self.lock_shared()?;
        let read = self.journal_lock().read()?;
        lock.unlock()?;
        let mut events: Vec<JournalEvent> = read
            .events
            .into_iter()
            .filter(|event| {
                identity.is_none_or(|identity| event.identity == identity)
                    && operation.is_none_or(|operation| event.operation == operation)
            })
            .collect();
        if let Some(limit) = limit {
            let skip = events.len().saturating_sub(limit);
            events.drain(..skip);
        }
        Ok(events)
    }

    /// Проверяет целостность журнала по фактическим ревизиям обоих манифестов.
    pub fn journal_check(&self) -> Result<JournalCheck, AssetError> {
        let lock = self.lock_shared()?;
        let read = self.journal_lock().read()?;
        let canonical = load_manifest(&self.root_handle)?;
        let runtime = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        lock.unlock()?;
        Ok(check(&read, revision_bound(&canonical, &runtime)))
    }

    /// Сверяет незакрытые интенты журнала с фактическим состоянием манифестов.
    ///
    /// Совпадение с ожидаемым `new_*` закрывается как `Recovered`, совпадение с
    /// прежним `previous_*` — как `Aborted`, всё остальное — как `Unknown`.
    /// Метод идемпотентен: повторный вызов не дописывает событий. Возвращается
    /// список фактически дописанных событий.
    pub fn reconcile_journal(&self) -> Result<Vec<JournalEvent>, AssetError> {
        let lock = self.lock_exclusive()?;
        let canonical = load_manifest(&self.root_handle)?;
        let runtime = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        let revision = revision_bound(&canonical, &runtime);
        let mut records: Vec<AssetRecord> = canonical.assets.clone();
        records.extend(runtime.assets.iter().cloned());
        let read = self.journal_lock().read()?;
        let inspection = inspect(&read, revision);
        if !inspection.structurally_valid {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "журнал содержит структурно некорректную историю; сверка не дописала события",
            ));
        }
        let mut appended = Vec::new();
        for intent in inspection.check.open_intents {
            let Some(event) = read
                .events
                .iter()
                .find(|event| event.event_id == intent.event_id)
            else {
                continue;
            };
            let actual = records.iter().find(|record| {
                record.identity.namespace == event.namespace
                    && record.identity.key == event.identity
            });
            let actual_sha256 = actual.map(|record| record.sha256.clone());
            let actual_lifecycle = actual.map(|record| record.lifecycle);
            let actual_lifecycle_text = actual_lifecycle.map(|state| state.as_str().to_owned());
            // Решение валидатора и доверие человека не меняют ни SHA-256, ни
            // жизненный цикл: манифест не различает применённое и неприменённое
            // изменение, поэтому исход честно остаётся неизвестным, а не
            // выдаётся за восстановленный.
            let indistinguishable = event.previous_sha256 == event.new_sha256
                && event.previous_lifecycle == event.new_lifecycle;
            let outcome = if indistinguishable {
                JournalOutcome::Unknown
            } else if actual_sha256 == event.new_sha256
                && actual_lifecycle_text == event.new_lifecycle
            {
                JournalOutcome::Recovered
            } else if actual_sha256 == event.previous_sha256
                && actual_lifecycle_text == event.previous_lifecycle
            {
                JournalOutcome::Aborted
            } else {
                JournalOutcome::Unknown
            };
            let mut draft = JournalDraft::new(
                self.policy.domain_id(),
                &AssetIdentity {
                    namespace: event.namespace.clone(),
                    key: event.identity.clone(),
                },
                event.operation,
                outcome,
            )
            .closing(&event.event_id)
            .revision(Some(revision))
            .sha(event.previous_sha256.as_deref(), actual_sha256.as_deref())
            .lifecycle(
                lifecycle_from_str(event.previous_lifecycle.as_deref()),
                actual_lifecycle,
            )
            .verified(Some(actual_lifecycle == Some(LifecycleState::Verified)));
            if outcome == JournalOutcome::Unknown {
                draft = draft.reason(&if indistinguishable {
                    format!(
                        "переход {} не меняет ни SHA-256, ни жизненный цикл: манифест не \
                         различает применённое и неприменённое изменение",
                        intent.identity
                    )
                } else {
                    format!(
                        "фактическое состояние {} не совпало ни с ожидаемым, ни с прежним",
                        intent.identity
                    )
                });
            }
            if read
                .events
                .iter()
                .any(|existing| existing.event_key == draft.event_key())
            {
                continue;
            }
            appended.push(self.record_journal_event(draft)?);
        }
        lock.unlock()?;
        Ok(appended)
    }

    /// Записывает единственное явно обозначенное событие `Baseline` со снимком
    /// фактического состояния хранилища.
    ///
    /// Повторная запись запрещена: baseline отмечает начало доверенной истории и
    /// не должен переписываться.
    pub fn journal_baseline(&self) -> Result<JournalEvent, AssetError> {
        let lock = self.lock_exclusive()?;
        let canonical = load_manifest(&self.root_handle)?;
        let runtime = load_runtime_manifest(&self.runtime_handle, &self.store_id)?;
        let read = self.journal_lock().read()?;
        if read
            .events
            .iter()
            .any(|event| event.operation == JournalOperation::Baseline)
        {
            return Err(AssetError::new(
                ErrorCode::InvalidTransition,
                "baseline журнала уже записан; повторная запись запрещена",
            ));
        }
        let draft = JournalDraft::store_wide(
            self.policy.domain_id(),
            self.policy.domain_id(),
            STORE_WIDE_IDENTITY,
            JournalOperation::Baseline,
            JournalOutcome::Completed,
        )
        .revision(Some(revision_bound(&canonical, &runtime)))
        .snapshot(snapshot(&canonical, &runtime));
        let event = self.record_journal_event(draft)?;
        lock.unlock()?;
        Ok(event)
    }
}

fn validate_validator_identity(identity: &ValidatorIdentity) -> Result<(), AssetError> {
    ValidatorIdentity::new(identity.id.clone(), identity.version.clone())
        .map(|_| ())
        .map_err(|message| AssetError::new(ErrorCode::InvalidValidatorIdentity, message))
}

fn validate_decision(decision: &SemanticDecision) -> Result<(), AssetError> {
    if decision.evidence.is_empty()
        || decision
            .evidence
            .iter()
            .any(|evidence| evidence.kind.trim().is_empty() || evidence.summary.trim().is_empty())
    {
        return Err(AssetError::new(
            ErrorCode::InvalidValidationEvidence,
            "semantic decision обязан содержать непустые kind и summary evidence",
        ));
    }
    Ok(())
}

fn stable_failure_code(failure: &ValidatorFailure) -> String {
    let code = failure.code.trim();
    if code.is_empty()
        || code.len() > 128
        || !code.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
    {
        "validator_failure".to_owned()
    } else {
        code.to_owned()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootState {
    NewlyCreated,
    ExistingEmpty,
    Owned,
}

fn initialize_or_load(root: &File, state: RootState, domain_id: &str) -> Result<bool, AssetError> {
    match state {
        RootState::NewlyCreated | RootState::ExistingEmpty => {
            // Инициализировать безопасно только пустой корневой каталог,
            // не принадлежащий хранилищу. Удерживаются обе блокировки:
            // для начальной настройки каталога и самого хранилища.
            let _assets = ensure_dir_entry(root, ASSETS_DIR)?;
            let _temporary = ensure_dir_entry(root, TEMP_DIR)?;
            let store_id = new_store_id();
            let manifest = Manifest::empty_for_domain(store_id.clone(), domain_id);
            write_owner_marker(root, &store_id)?;
            save_manifest(root, &manifest, true)?;
            Ok(true)
        }
        RootState::Owned => {
            // Внутренние каталоги можно восстановить только после проверки
            // канонического манифеста для запрошенного домена.
            ensure_dir_entry(root, ASSETS_DIR)?;
            ensure_dir_entry(root, TEMP_DIR)?;
            Ok(false)
        }
    }
}

fn open_or_initialize_runtime(
    root: &File,
    store_id: &str,
    policy: &dyn AssetDomainPolicy,
) -> Result<File, AssetError> {
    let runtime = ensure_dir_entry(root, RUNTIME_DIR)?;
    let names = inspect_area_top_level(&runtime, ErrorCode::StoreNotOwned, true)?;
    if names.is_empty() {
        ensure_dir_entry(&runtime, ASSETS_DIR)?;
        ensure_dir_entry(&runtime, TEMP_DIR)?;
        write_owner_marker(&runtime, store_id)?;
        save_manifest(
            &runtime,
            &Manifest::empty_for_domain(store_id.to_owned(), policy.domain_id()),
            true,
        )?;
    } else {
        if !names.contains(OWNER_FILE) || !names.contains(MANIFEST_FILE) {
            return Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                "непустой корневой каталог `.runtime` без файлов `.owner.json` и `manifest.json` не принадлежит `asset-store`",
            ));
        }
        let owner = read_owner_marker(&runtime)?;
        if owner.store_id != store_id {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "значение `store_id` в маркере владельца `.runtime` не совпадает со значением в каноническом манифесте",
            ));
        }
        let manifest = load_runtime_manifest(&runtime, store_id)?;
        check_schema(&manifest)?;
        ensure_store_domain(&manifest, policy)?;
        ensure_dir_entry(&runtime, ASSETS_DIR)?;
        ensure_dir_entry(&runtime, TEMP_DIR)?;
    }
    inspect_area_top_level(&runtime, ErrorCode::UnexpectedPath, true)?;
    Ok(runtime)
}

fn ensure_store_domain(
    manifest: &Manifest,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    if manifest.schema_version < MANIFEST_SCHEMA_VERSION && !policy.supports_legacy_schema() {
        return Err(AssetError::with_details(
            ErrorCode::UnsupportedSchemaVersion,
            format!(
                "domain {} не может открывать legacy schema {}",
                policy.domain_id(),
                manifest.schema_version
            ),
            serde_json::json!({
                "store_schema_version": manifest.schema_version,
                "requested_domain": policy.domain_id(),
            }),
        ));
    }
    if manifest.schema_version >= MANIFEST_SCHEMA_VERSION
        && manifest.domain_id != policy.domain_id()
    {
        return Err(AssetError::with_details(
            ErrorCode::ManifestCorrupt,
            format!(
                "store domain `{}` нельзя открыть с policy `{}`",
                manifest.domain_id,
                policy.domain_id()
            ),
            serde_json::json!({
                "store_domain": manifest.domain_id,
                "requested_domain": policy.domain_id(),
            }),
        ));
    }
    if !manifest.domain_id.is_empty() && manifest.domain_id != policy.domain_id() {
        return Err(AssetError::with_details(
            ErrorCode::ManifestCorrupt,
            "legacy store уже связан с другим domain",
            serde_json::json!({
                "store_domain": manifest.domain_id,
                "requested_domain": policy.domain_id(),
            }),
        ));
    }
    for record in &manifest.assets {
        policy.validate_identity(&record.identity)?;
    }
    Ok(())
}

/// Переносит старые записи из плоской схемы при открытии хранилища с изменениями
/// под эксклюзивной блокировкой. Старые байты сначала связываются жёсткими
/// ссылками с новым размещением и проверяются по `SHA-256`; затем одной атомарной
/// заменой манифеста фиксируются схема, путь и имя. После фиксации старые ссылки
/// удаляются. Маркер завершает или откатывает прерванный переход.
fn migrate_layout(
    root: &File,
    manifest: &mut Manifest,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    if manifest.schema_version >= MANIFEST_SCHEMA_VERSION {
        ensure_store_domain(manifest, policy)?;
        return Ok(());
    }
    if !matches!(manifest.schema_version, 3 | 4) {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            "для миграции поддерживаются только manifest schema 3 и 4",
        ));
    }
    ensure_store_domain(manifest, policy)?;
    validate_manifest(root, manifest, policy)?;

    let mut entries = Vec::with_capacity(manifest.assets.len());
    let mut target_paths = BTreeSet::new();
    let mut consumer_names = BTreeSet::new();
    for record in &manifest.assets {
        let legacy = policy
            .legacy_location(&record.identity, &record.sha256, record.format)
            .ok_or_else(|| {
                AssetError::new(
                    ErrorCode::UnsupportedSchemaVersion,
                    format!(
                        "domain {} не имеет legacy layout для {}",
                        policy.domain_id(),
                        record.identity
                    ),
                )
            })?;
        let next = policy.canonical_location(&record.identity, &record.sha256, record.format)?;
        crate::domain::validate_safe_storage_path(&legacy.storage_path)?;
        crate::domain::validate_safe_storage_path(&next.storage_path)?;
        crate::domain::validate_safe_consumer_filename(&next.consumer_filename, record.format)?;
        if record.storage_path != legacy.storage_path {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "storage_path старой схемы не совпадает с расположением, заданным политикой для {}",
                    record.identity
                ),
            ));
        }
        if !target_paths.insert(next.storage_path.clone())
            || !consumer_names.insert(next.consumer_filename.clone())
        {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "миграция обнаружила повторяющийся канонический путь или consumer filename",
            ));
        }
        entries.push(LayoutMigrationEntry {
            identity: record.identity.clone(),
            sha256: record.sha256.clone(),
            format: record.format,
            old_path: legacy.storage_path,
            new_path: next.storage_path,
            consumer_filename: next.consumer_filename,
        });
    }

    let transaction = LayoutMigrationTransaction {
        schema_version: 1,
        store_id: manifest.store_id.clone(),
        domain_id: policy.domain_id().to_owned(),
        source_schema_version: manifest.schema_version,
        entries,
    };
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let bytes = serde_json::to_vec_pretty(&transaction)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    write_transaction_marker(&temporary, LAYOUT_MIGRATION_MARKER, &bytes)?;

    for entry in &transaction.entries {
        if entry.old_path == entry.new_path {
            continue;
        }
        let old = open_storage_file(root, &entry.old_path, false)?;
        if hash_file(old)?.0 != entry.sha256 {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "ресурс прежней схемы изменился до переноса в новое расположение",
            ));
        }
        let (new_parent, new_name) = open_storage_parent(root, &entry.new_path, true)?;
        match open_regular_at(&new_parent, &new_name, ErrorCode::MissingAssetFile) {
            Ok(existing) => {
                if hash_file(existing)?.0 != entry.sha256 {
                    return Err(AssetError::new(
                        ErrorCode::IdentityConflict,
                        "целевой канонический путь уже занят другими байтами при миграции",
                    ));
                }
            }
            Err(error) if error.code == ErrorCode::MissingAssetFile => {
                let (old_parent, old_name) = open_storage_parent(root, &entry.old_path, false)?;
                match linkat(
                    &old_parent,
                    &old_name,
                    &new_parent,
                    &new_name,
                    AtFlags::empty(),
                ) {
                    Ok(()) => {}
                    Err(link_error)
                        if std::io::Error::from(link_error).kind()
                            == std::io::ErrorKind::AlreadyExists =>
                    {
                        let existing =
                            open_regular_at(&new_parent, &new_name, ErrorCode::MissingAssetFile)?;
                        if hash_file(existing)?.0 != entry.sha256 {
                            return Err(AssetError::new(
                                ErrorCode::IdentityConflict,
                                "целевой канонический путь изменился во время миграции",
                            ));
                        }
                    }
                    Err(link_error) => {
                        return Err(AssetError::io(
                            "не удалось подготовить вложенный путь ресурса для миграции",
                            std::io::Error::from(link_error),
                        ));
                    }
                }
                sync_directory(&new_parent)?;
            }
            Err(error) => return Err(error),
        }
    }

    let mut migrated = manifest.clone();
    for (record, entry) in migrated.assets.iter_mut().zip(&transaction.entries) {
        record.storage_path = entry.new_path.clone();
        record.consumer_filename = entry.consumer_filename.clone();
    }
    migrated.domain_id = policy.domain_id().to_owned();
    migrated.schema_version = MANIFEST_SCHEMA_VERSION;
    migrated.revision = migrated.revision.checked_add(1).ok_or_else(|| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            "счётчик revision в manifest переполнен при миграции",
        )
    })?;
    save_manifest(root, &migrated, false)?;
    *manifest = migrated;
    recover_layout_migration(root, policy)
}

fn recover_layout_migration(root: &File, policy: &dyn AssetDomainPolicy) -> Result<(), AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let marker = match open_regular_at(
        &temporary,
        LAYOUT_MIGRATION_MARKER,
        ErrorCode::MissingAssetFile,
    ) {
        Ok(file) => file,
        Err(error) if error.code == ErrorCode::MissingAssetFile => return Ok(()),
        Err(error) => return Err(error),
    };
    let transaction: LayoutMigrationTransaction =
        serde_json::from_reader(marker).map_err(|error| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("маркер переноса расположения повреждён: {error}"),
            )
        })?;
    if transaction.schema_version != 1
        || !matches!(transaction.source_schema_version, 3 | 4)
        || transaction.domain_id != policy.domain_id()
        || transaction.store_id.is_empty()
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "маркер переноса расположения содержит неизвестную schema/domain",
        ));
    }
    let manifest = load_owned_manifest(root)?;
    if manifest.store_id != transaction.store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "маркер переноса расположения указывает на другой store_id",
        ));
    }
    let committed = manifest.schema_version >= MANIFEST_SCHEMA_VERSION
        && manifest.domain_id == transaction.domain_id;
    let rolling_back = manifest.schema_version == transaction.source_schema_version
        && (manifest.domain_id.is_empty() || manifest.domain_id == transaction.domain_id);
    if !committed && !rolling_back {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "маркер переноса расположения не согласован с текущим manifest",
        ));
    }
    let mut seen_new_paths = BTreeSet::new();
    for entry in &transaction.entries {
        entry
            .identity
            .validate()
            .map_err(|message| AssetError::new(ErrorCode::ManifestCorrupt, message))?;
        policy.validate_identity(&entry.identity)?;
        validate_hash(&entry.sha256)?;
        let legacy = policy
            .legacy_location(&entry.identity, &entry.sha256, entry.format)
            .ok_or_else(|| {
                AssetError::new(ErrorCode::ManifestCorrupt, "legacy path policy отсутствует")
            })?;
        let expected = policy.canonical_location(&entry.identity, &entry.sha256, entry.format)?;
        if entry.old_path != legacy.storage_path
            || entry.new_path != expected.storage_path
            || entry.consumer_filename != expected.consumer_filename
            || !seen_new_paths.insert(entry.new_path.clone())
        {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "маркер миграции содержит путь или имя, не вычисляемые из domain policy",
            ));
        }
        crate::domain::validate_safe_consumer_filename(&entry.consumer_filename, entry.format)?;
        if entry.old_path != entry.new_path {
            if committed {
                let new_file = open_storage_file(root, &entry.new_path, false)?;
                if hash_file(new_file)?.0 != entry.sha256 {
                    return Err(AssetError::new(
                        ErrorCode::IntegrityMismatch,
                        "после manifest commit отсутствует migrated asset bytes",
                    ));
                }
                remove_storage_path_if_hash(
                    root,
                    &entry.old_path,
                    &entry.sha256,
                    "ресурс прежней схемы",
                )?;
            } else {
                remove_storage_path_if_hash(
                    root,
                    &entry.new_path,
                    &entry.sha256,
                    "целевой ресурс миграции",
                )?;
                prune_empty_storage_directories(root, &entry.new_path)?;
            }
        }
    }
    sync_directory(&temporary)?;
    unlink_if_exists(&temporary, &OsString::from(LAYOUT_MIGRATION_MARKER))?;
    sync_directory(&temporary)
}

/// Определяет состояние корня, не создавая файлы и каталоги.
fn preflight_root_ownership(root: &File, root_created: bool) -> Result<RootState, AssetError> {
    let names = inspect_top_level(root, ErrorCode::StoreNotOwned)?;
    if names.is_empty() {
        return Ok(if root_created {
            RootState::NewlyCreated
        } else {
            RootState::ExistingEmpty
        });
    }
    if !names.contains(OWNER_FILE) {
        return Err(AssetError::new(
            ErrorCode::StoreNotOwned,
            "непустой store root без owner marker не принадлежит asset store",
        ));
    }
    if !names.contains(MANIFEST_FILE) {
        return Err(AssetError::new(
            ErrorCode::ManifestMissing,
            "owner marker существует, но canonical manifest отсутствует",
        ));
    }

    let owner = read_owner_marker(root)?;
    let manifest = read_manifest_file(root)?;
    check_schema(&manifest)?;
    if manifest.store_id != owner.store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "store_id manifest не совпадает с owner marker",
        ));
    }
    Ok(RootState::Owned)
}

fn inspect_top_level(root: &File, unknown_code: ErrorCode) -> Result<BTreeSet<String>, AssetError> {
    inspect_area_top_level(root, unknown_code, false)
}

fn inspect_area_top_level(
    root: &File,
    unknown_code: ErrorCode,
    runtime_extensions: bool,
) -> Result<BTreeSet<String>, AssetError> {
    let root_path = fd_path(root);
    let mut names = BTreeSet::new();
    for entry in fs::read_dir(&root_path)
        .map_err(|error| AssetError::io("не удалось прочитать store root", error))?
    {
        let entry =
            entry.map_err(|error| AssetError::io("не удалось прочитать store entry", error))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(matches!(
            name.as_str(),
            LOCK_FILE
                | OWNER_FILE
                | MANIFEST_FILE
                | ASSETS_DIR
                | TEMP_DIR
                | RUNTIME_DIR
                | JOURNAL_DIR
        ) || runtime_extensions && name == BATCHES_DIR)
        {
            return Err(AssetError::new(
                unknown_code,
                format!("неожиданный файл в store root: {name}"),
            ));
        }
        if runtime_extensions && name == BATCHES_DIR {
            // Открываем относительно закреплённого файлового дескриптора каталога `.runtime`
            // с `DIRECTORY|NOFOLLOW`: метаданных пути недостаточно при его
            // конкурентной подмене.
            open_directory_at(root, BATCHES_DIR)
                .map_err(|error| directory_entry_error(BATCHES_DIR, error))?;
        }
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| AssetError::io("не удалось проверить store entry", error))?;
        if metadata.file_type().is_symlink() {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                format!("symlink запрещён в store root: {name}"),
            ));
        }
        if matches!(
            name.as_str(),
            ASSETS_DIR | TEMP_DIR | RUNTIME_DIR | JOURNAL_DIR
        ) && !metadata.is_dir()
        {
            return Err(AssetError::new(
                unknown_code,
                format!("{name} существует, но не является каталогом"),
            ));
        }
        if matches!(name.as_str(), LOCK_FILE | OWNER_FILE | MANIFEST_FILE) && !metadata.is_file() {
            return Err(AssetError::new(
                unknown_code,
                format!("{name} существует, но не является обычным файлом"),
            ));
        }
        names.insert(name);
    }
    Ok(names)
}

fn ensure_top_level(root: &File) -> Result<(), AssetError> {
    inspect_top_level(root, ErrorCode::UnexpectedPath).map(|_| ())
}

pub(crate) fn open_directory_at(
    parent: &File,
    name: impl rustix::path::Arg,
) -> std::io::Result<File> {
    let fd = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    Ok(File::from(fd))
}

pub(crate) fn ensure_dir_entry(root: &File, name: &str) -> Result<File, AssetError> {
    match open_directory_at(root, name) {
        Ok(directory) => Ok(directory),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match mkdirat(root, name, Mode::from_raw_mode(0o755)) {
                Ok(()) => open_directory_at(root, name)
                    .map_err(|error| directory_entry_error(name, error)),
                Err(create_error)
                    if std::io::Error::from(create_error).kind()
                        == std::io::ErrorKind::AlreadyExists =>
                {
                    open_directory_at(root, name)
                        .map_err(|error| directory_entry_error(name, error))
                }
                Err(create_error) => Err(AssetError::io(
                    format!("не удалось создать {name}"),
                    std::io::Error::from(create_error),
                )),
            }
        }
        Err(error) => Err(directory_entry_error(name, error)),
    }
}

pub(crate) fn directory_entry_error(name: &str, error: std::io::Error) -> AssetError {
    if is_symlink_error(&error) || error.kind() == std::io::ErrorKind::NotADirectory {
        AssetError::new(
            ErrorCode::BoundaryViolation,
            format!("{name} должен быть обычным каталогом без symlink"),
        )
    } else {
        AssetError::io(format!("не удалось проверить {name}"), error)
    }
}

fn create_lock_file(root: &File) -> Result<File, AssetError> {
    let lock = openat(
        root,
        LOCK_FILE,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::from_raw_mode(0o600),
    )
    .map_err(|error| {
        let error = std::io::Error::from(error);
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            AssetError::new(
                ErrorCode::StoreNotOwned,
                "lock появился в root до подтверждения program ownership",
            )
        } else {
            AssetError::io("не удалось создать lock store", error)
        }
    })?;
    let lock = File::from(lock);
    if !lock
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить lock store", error))?
        .is_file()
    {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "lock path должен быть обычным файлом",
        ));
    }
    Ok(lock)
}

fn open_lock_file(root: &File, create_if_missing: bool) -> Result<File, AssetError> {
    match openat(
        root,
        LOCK_FILE,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => {
            let lock = File::from(fd);
            if !lock
                .metadata()
                .map_err(|error| AssetError::io("не удалось проверить lock store", error))?
                .is_file()
            {
                return Err(AssetError::new(
                    ErrorCode::BoundaryViolation,
                    "lock path должен быть обычным файлом без symlink",
                ));
            }
            Ok(lock)
        }
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => {
            if create_if_missing {
                create_lock_file(root)
            } else {
                Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    "lock отсутствует в program-owned store",
                ))
            }
        }
        Err(error) => {
            let error = std::io::Error::from(error);
            if is_symlink_error(&error) {
                Err(AssetError::new(
                    ErrorCode::BoundaryViolation,
                    "lock path должен быть обычным файлом без symlink",
                ))
            } else {
                Err(AssetError::io("не удалось открыть lock store", error))
            }
        }
    }
}

fn ensure_directory_empty(root: &File, name: &str) -> Result<(), AssetError> {
    let directory = match open_directory_at(root, name) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(directory_entry_error(name, error)),
    };
    let mut entries = fs::read_dir(fd_path(&directory))
        .map_err(|error| AssetError::io(format!("не удалось прочитать {name}"), error))?;
    if entries.next().is_some() {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!("в публикуемом хранилище остались незавершённые временные файлы в {name}"),
        ));
    }
    Ok(())
}

fn write_owner_marker(root: &File, store_id: &str) -> Result<(), AssetError> {
    let marker = OwnerMarker {
        schema_version: OWNER_SCHEMA_VERSION,
        store_id: store_id.to_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&marker)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    atomic_write(root, OWNER_FILE, &bytes, true, None)
}

fn load_manifest(root: &File) -> Result<Manifest, AssetError> {
    ensure_top_level(root)?;
    load_owned_manifest(root)
}

// Восстановление работает с уже проверенным дескриптором канонического каталога
// или каталога `.runtime`. Имена расширений проверяет загрузчик для нужной области,
// а владельца и схему повторно проверяют при каждом чтении, включая восстановление
// транзакций.
fn load_owned_manifest(root: &File) -> Result<Manifest, AssetError> {
    let manifest = read_manifest_file(root)?;
    check_schema(&manifest)?;
    let owner = read_owner_marker(root)?;
    if owner.store_id != manifest.store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "store_id manifest не совпадает с owner marker",
        ));
    }
    Ok(manifest)
}

fn load_runtime_manifest(root: &File, store_id: &str) -> Result<Manifest, AssetError> {
    inspect_area_top_level(root, ErrorCode::UnexpectedPath, true)?;
    let owner = read_owner_marker(root)?;
    if owner.store_id != store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "значение `store_id` в маркере владельца не совпадает со значением в каноническом манифесте",
        ));
    }
    let manifest = read_manifest_file(root)?;
    check_schema(&manifest)?;
    if manifest.store_id != store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "значение `store_id` в локальном манифесте не совпадает со значением в каноническом манифесте",
        ));
    }
    Ok(manifest)
}

fn read_manifest_file(root: &File) -> Result<Manifest, AssetError> {
    let mut file = open_regular_at(root, MANIFEST_FILE, ErrorCode::ManifestMissing)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| AssetError::io("не удалось прочитать manifest", error))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    let schema_version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                "manifest schema_version отсутствует или не является целым числом",
            )
        })?;
    if !matches!(schema_version, 3 | 4) && schema_version != u64::from(MANIFEST_SCHEMA_VERSION) {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            format!(
                "manifest schema_version {schema_version} не поддерживается (ожидается {MANIFEST_SCHEMA_VERSION})"
            ),
        ));
    }
    serde_json::from_value(value)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))
}

fn read_owner_marker(root: &File) -> Result<OwnerMarker, AssetError> {
    let mut file = open_regular_at(root, OWNER_FILE, ErrorCode::StoreNotOwned)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| AssetError::io("не удалось прочитать owner marker", error))?;
    let marker: OwnerMarker = serde_json::from_slice(&bytes).map_err(|error| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!("owner marker невалиден: {error}"),
        )
    })?;
    if marker.schema_version != OWNER_SCHEMA_VERSION {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            format!(
                "неподдерживаемая версия owner marker {}",
                marker.schema_version
            ),
        ));
    }
    if marker.store_id.is_empty() {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "owner marker store_id пуст",
        ));
    }
    Ok(marker)
}

fn check_schema(manifest: &Manifest) -> Result<(), AssetError> {
    if !matches!(manifest.schema_version, 3 | 4)
        && manifest.schema_version != MANIFEST_SCHEMA_VERSION
    {
        return Err(AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            format!(
                "manifest schema_version {} не поддерживается (ожидается {})",
                manifest.schema_version, MANIFEST_SCHEMA_VERSION
            ),
        ));
    }
    if manifest.schema_version == 3
        && manifest
            .assets
            .iter()
            .any(|asset| asset.human_attestation.is_some())
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "schema v3 не поддерживает human attestation",
        ));
    }
    Ok(())
}

fn validate_manifest(
    root: &File,
    manifest: &Manifest,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    check_schema(manifest)?;
    if manifest.store_id.is_empty() || manifest.revision > i64::MAX as u64 {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "store_id или revision manifest некорректны",
        ));
    }
    let mut previous_identity: Option<&AssetIdentity> = None;
    if manifest.schema_version >= MANIFEST_SCHEMA_VERSION
        && manifest.domain_id != policy.domain_id()
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "manifest domain_id не совпадает с активной policy",
        ));
    }
    let mut registered_paths = BTreeSet::new();
    let mut consumer_names = BTreeSet::new();
    for record in &manifest.assets {
        policy
            .validate_identity(&record.identity)
            .map_err(|error| {
                AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    format!(
                        "identity {} не принадлежит domain policy: {}",
                        record.identity, error.message
                    ),
                )
            })?;
        if previous_identity.is_some_and(|previous| previous >= &record.identity) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "assets manifest должны быть уникальны и отсортированы по identity",
            ));
        }
        previous_identity = Some(&record.identity);
        validate_hash(&record.sha256)?;
        if record.byte_length == 0 && record.format != DetectedFormat::Unknown {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "пустой файл должен иметь format unknown",
            ));
        }
        let expected_location = if manifest.schema_version < MANIFEST_SCHEMA_VERSION {
            policy.legacy_location(&record.identity, &record.sha256, record.format)
        } else {
            Some(policy.canonical_location(&record.identity, &record.sha256, record.format)?)
        }
        .ok_or_else(|| {
            AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                format!(
                    "domain {} не имеет layout для {}",
                    policy.domain_id(),
                    record.identity
                ),
            )
        })?;
        validate_relative_path(&record.storage_path, &expected_location.storage_path)?;
        crate::domain::validate_safe_storage_path(&record.storage_path)?;
        if manifest.schema_version >= MANIFEST_SCHEMA_VERSION {
            crate::domain::validate_safe_consumer_filename(
                &record.consumer_filename,
                record.format,
            )?;
            if record.consumer_filename != expected_location.consumer_filename {
                return Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    format!(
                        "consumer_filename не совпадает с domain policy для {}",
                        record.identity
                    ),
                ));
            }
        }
        if !consumer_names.insert(expected_location.consumer_filename.clone()) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "manifest содержит повторяющиеся consumer_filename",
            ));
        }
        if let Some(validation) = &record.validation {
            validate_validation_record(record, validation)?;
        }
        if let Some(attestation) = &record.human_attestation {
            validate_hash(&attestation.content_sha256)?;
            if attestation.identity != record.identity
                || attestation.content_sha256 != record.sha256
                || attestation.reason.trim().is_empty()
            {
                return Err(AssetError::new(
                    ErrorCode::ManifestCorrupt,
                    "human attestation не привязана к текущим identity/hash или не содержит основания",
                ));
            }
            if attestation.decision == HumanDecision::Approve {
                if !record.has_current_validation() {
                    return Err(AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        format!(
                            "подтверждение человеком не содержит автоматическую запись ValidationRecord для текущего SHA-256: {}",
                            record.identity
                        ),
                    ));
                }
                if record.byte_length > policy.max_asset_bytes().unwrap_or(MAX_HUMAN_APPROVED_BYTES)
                {
                    return Err(AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        format!(
                            "изображение, одобренное человеком, превышает установленный предел размера: {}",
                            record.identity
                        ),
                    ));
                }
            }
        }
        let expected_lifecycle = match record.effective_status() {
            Some(SemanticStatus::Verified) => LifecycleState::Verified,
            Some(_) => LifecycleState::Quarantined,
            None => LifecycleState::Pending,
        };
        if record.lifecycle != expected_lifecycle {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "lifecycle и effective semantic decision не согласованы для {}",
                    record.identity
                ),
            ));
        }
        if record.provenance.source_kind.is_empty() || record.provenance.source_name.is_empty() {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("provenance отсутствует для {}", record.identity),
            ));
        }
        if record.provenance.source_name.contains(['/', '\\']) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "provenance source_name не должен содержать path",
            ));
        }
        validate_asset(
            root,
            record,
            policy,
            manifest.schema_version < MANIFEST_SCHEMA_VERSION,
        )?;
        if !registered_paths.insert(record.storage_path.clone()) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "manifest содержит повторяющийся storage_path: {}",
                    record.storage_path
                ),
            ));
        }
    }
    validate_asset_directory(root, &registered_paths, policy)?;
    Ok(())
}

fn validate_verified_manifest(
    root: &File,
    manifest: &Manifest,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    validate_manifest(root, manifest, policy)?;
    if manifest
        .assets
        .iter()
        .any(|asset| !policy.allows_verified_format(asset.format))
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!(
                "canonical manifest содержит формат, запрещённый для verified lifecycle в domain {}",
                policy.domain_id()
            ),
        ));
    }
    if manifest.assets.iter().any(|asset| {
        asset.lifecycle != LifecycleState::Verified
            || asset.effective_status() != Some(SemanticStatus::Verified)
    }) {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "канонический манифест допускает только effective verified записи",
        ));
    }
    Ok(())
}

fn validate_runtime_manifest(
    root: &File,
    manifest: &Manifest,
    store_id: &str,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    if manifest.store_id != store_id {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "значение `store_id` в локальном манифесте не совпадает со значением в каноническом манифесте",
        ));
    }
    validate_manifest(root, manifest, policy)?;
    if manifest
        .assets
        .iter()
        .any(|asset| asset.lifecycle == LifecycleState::Verified)
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "локальный манифест не может содержать записи со статусом `verified`",
        ));
    }
    Ok(())
}

fn validate_publishable_manifest(
    root: &File,
    manifest: &Manifest,
    expected_validator: &ValidatorIdentity,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    validate_verified_manifest(root, manifest, policy)?;
    for asset in &manifest.assets {
        if !is_trusted_for_policy(asset, expected_validator, policy) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "ресурс {} не имеет доверенного решения `verified` от ожидаемой версии валидатора",
                    asset.identity
                ),
            ));
        }
        policy.validate_publishable_record(asset)?;
        if !policy.is_publishable_format(asset.format) {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!(
                    "публикуемый корпус domain {} содержит неподдерживаемый формат для {}",
                    policy.domain_id(),
                    asset.identity,
                ),
            ));
        }
    }
    validate_asset_directory_exact(root, manifest, policy)?;
    Ok(())
}

fn is_trusted_for_policy(
    asset: &AssetRecord,
    expected_validator: &ValidatorIdentity,
    policy: &dyn AssetDomainPolicy,
) -> bool {
    policy
        .trust_semantics()
        .confers_trust(asset, expected_validator)
}

fn select_assets_for_policy<'a>(
    assets: &'a [AssetRecord],
    mode: SelectionMode,
    validator: &ValidatorIdentity,
    policy: &dyn AssetDomainPolicy,
) -> Vec<&'a AssetRecord> {
    let semantics = policy.trust_semantics();
    let mut selected: Vec<_> = assets
        .iter()
        .filter(|asset| match mode {
            SelectionMode::Full => true,
            SelectionMode::New => semantics.requires_new_validation(asset, validator),
        })
        .collect();
    selected.sort_by(|left, right| left.identity.cmp(&right.identity));
    selected
}

fn validate_legacy_verified_manifest(
    root: &File,
    manifest: &Manifest,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    if manifest.schema_version >= MANIFEST_SCHEMA_VERSION {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "legacy validator вызван для актуальной schema",
        ));
    }
    validate_verified_manifest(root, manifest, policy)
}

/// Дописывает событие в уже открытый журнал хранилища.
///
/// Используется там, где `AssetStore` ещё не собран (открытие хранилища).
fn journal_append(
    journal: &std::sync::Mutex<Journal>,
    draft: JournalDraft,
) -> Result<(), AssetError> {
    journal
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .append(&draft)
        .map(|_| ())
}

/// Фактическая суммарная ревизия манифестов двух областей хранилища.
fn journal_revision_of(root: &File, runtime: &File, store_id: &str) -> Result<u64, AssetError> {
    let canonical = load_manifest(root)?;
    let local = load_runtime_manifest(runtime, store_id)?;
    Ok(revision_bound(&canonical, &local))
}

/// Пишет события `Recovered` для следов, оставленных завершённым восстановлением.
fn write_recovery_facts(
    journal: &std::sync::Mutex<Journal>,
    domain: &str,
    root: &File,
    runtime: &File,
    store_id: &str,
    facts: &[RecoveryFact],
) -> Result<(), AssetError> {
    if facts.is_empty() {
        return Ok(());
    }
    let revision = journal_revision_of(root, runtime, store_id)?;
    for fact in facts {
        journal_append(
            journal,
            JournalDraft::new(
                domain,
                &fact.identity,
                JournalOperation::Recovered,
                JournalOutcome::Completed,
            )
            .revision(Some(revision))
            .sha(fact.previous_sha256.as_deref(), fact.next_sha256.as_deref())
            .lifecycle(None, fact.next_lifecycle)
            .verified(Some(fact.next_lifecycle == Some(LifecycleState::Verified)))
            .reason(&fact.reason),
        )?;
    }
    Ok(())
}

fn reconcile_runtime_boundary(
    root: &File,
    runtime: &File,
    canonical_manifest: &Manifest,
    runtime_manifest: &Manifest,
    policy: &dyn AssetDomainPolicy,
) -> Result<Vec<RecoveryFact>, AssetError> {
    let mut canonical = canonical_manifest.clone();
    let mut local = runtime_manifest.clone();
    let transition = load_transition(root)?;
    let mut facts = Vec::new();

    // Старые версии хранили все состояния жизненного цикла в публикуемом
    // манифесте. Сначала переносим байты в игнорируемую `.runtime`-область,
    // затем удаляем прежние записи.
    let legacy_candidates: Vec<_> = canonical
        .assets
        .iter()
        .filter(|asset| asset.lifecycle != LifecycleState::Verified)
        .cloned()
        .collect();
    // Идентичности, перенесённые этим циклом, описываются одним событием:
    // удаление их прежней записи ниже — часть того же переноса, а не отдельный
    // прерванный переход.
    let mut migrated_legacy: BTreeSet<AssetIdentity> = BTreeSet::new();
    for candidate in legacy_candidates {
        if local
            .assets
            .iter()
            .any(|asset| asset.identity == candidate.identity)
        {
            continue;
        }
        let source = checked_asset_file(root, &candidate, policy)?;
        let staged = stage_source(runtime, source)?;
        commit_asset_record(
            runtime,
            &staged,
            None,
            &candidate,
            &mut local,
            policy,
            |root, manifest| save_manifest(root, manifest, false),
        )?;
        migrated_legacy.insert(candidate.identity.clone());
        facts.push(RecoveryFact::new(
            candidate.identity.clone(),
            Some(candidate.sha256.clone()),
            Some(candidate.sha256.clone()),
            Some(candidate.lifecycle),
            "устаревшая запись перенесена из публикуемого манифеста в .runtime",
        ));
    }

    // Совпадающая идентичность в `.runtime` и каноническом манифесте означает,
    // что перенос в карантин или замена кандидатом прервались. Сохраняем
    // локальное состояние и завершаем удаление старой опубликованной записи.
    // Проверка публикации отклоняет такое совпадение до завершения восстановления.
    let overlaps: Vec<_> = canonical
        .assets
        .iter()
        .filter(|asset| {
            local
                .assets
                .iter()
                .any(|candidate| candidate.identity == asset.identity)
        })
        .cloned()
        .collect();
    for previous in overlaps {
        if let Some(marker) = &transition
            && marker.identity == previous.identity
            && matches!(marker.target, TransitionTarget::Canonical)
        {
            let survivor = canonical
                .assets
                .iter()
                .find(|asset| asset.identity == marker.identity)
                .cloned();
            if survivor
                .as_ref()
                .is_none_or(|asset| asset.sha256 != marker.sha256)
            {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "transition marker не совпадает с canonical asset",
                ));
            }
            let survivor = survivor.expect("совпадение проверено выше");
            let previous_runtime = local
                .assets
                .iter()
                .find(|asset| asset.identity == previous.identity)
                .cloned()
                .ok_or_else(|| {
                    AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        "runtime asset исчез при восстановлении перехода",
                    )
                })?;
            remove_record_from_area(runtime, &mut local, &previous_runtime, policy)?;
            // Идентичность остаётся живой записью в каноническом манифесте,
            // поэтому событие описывает её фактическое состояние, а не удаление.
            facts.push(RecoveryFact::new(
                previous_runtime.identity.clone(),
                Some(previous_runtime.sha256.clone()),
                Some(survivor.sha256.clone()),
                Some(survivor.lifecycle),
                "прерванный переход к канонической записи завершён: локальная запись удалена",
            ));
        } else {
            remove_record_from_area(root, &mut canonical, &previous, policy)?;
            if !migrated_legacy.contains(&previous.identity) {
                // Запись остаётся в `.runtime`, поэтому событие описывает её
                // фактическое состояние в другой области.
                let survivor = local
                    .assets
                    .iter()
                    .find(|asset| asset.identity == previous.identity);
                facts.push(RecoveryFact::new(
                    previous.identity.clone(),
                    Some(previous.sha256.clone()),
                    survivor.map(|asset| asset.sha256.clone()),
                    survivor.map(|asset| asset.lifecycle),
                    "прерванный переход к локальной записи завершён: опубликованная запись удалена",
                ));
            }
        }
    }

    if let Some(marker) = &transition {
        let target = match marker.target {
            TransitionTarget::Canonical => &canonical,
            TransitionTarget::Runtime => &local,
        };
        if let Some(asset) = target
            .assets
            .iter()
            .find(|asset| asset.identity == marker.identity)
            && asset.sha256 != marker.sha256
        {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "transition marker не совпадает с целевым asset",
            ));
        }
        clear_transition(root)?;
    }

    cleanup_matching_orphans(root, &canonical, &local)?;
    cleanup_matching_orphans(runtime, &local, &canonical)?;
    validate_verified_manifest(root, &canonical, policy)?;
    validate_runtime_manifest(runtime, &local, &canonical.store_id, policy)?;
    Ok(facts)
}

fn cleanup_matching_orphans(
    area: &File,
    current: &Manifest,
    other: &Manifest,
) -> Result<(), AssetError> {
    let registered: BTreeSet<_> = current
        .assets
        .iter()
        .map(|asset| asset.storage_path.as_str())
        .collect();
    for record in &other.assets {
        if registered.contains(record.storage_path.as_str()) {
            continue;
        }
        remove_storage_path_if_hash(
            area,
            &record.storage_path,
            &record.sha256,
            "запись о незавершённом переносе актива",
        )?;
    }
    Ok(())
}

fn replace_manifest_record(
    manifest: &mut Manifest,
    record: &AssetRecord,
) -> Result<(), AssetError> {
    let target = manifest
        .assets
        .iter_mut()
        .find(|asset| asset.identity == record.identity)
        .ok_or_else(|| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("манифест потерял запись {}", record.identity),
            )
        })?;
    *target = record.clone();
    sort_assets(&mut manifest.assets);
    Ok(())
}

fn save_manifest_revision(
    root: &File,
    manifest: &mut Manifest,
    #[cfg(test)] fail_before_commit: Option<&std::sync::atomic::AtomicBool>,
    #[cfg(not(test))] _fail_before_commit: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(), AssetError> {
    manifest.revision = manifest.revision.checked_add(1).ok_or_else(|| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            "поле `revision` в манифесте переполнено",
        )
    })?;
    #[cfg(test)]
    if let Some(fail_before_commit) = fail_before_commit {
        return save_manifest_with_test_hook(root, manifest, false, fail_before_commit);
    }
    save_manifest(root, manifest, false)
}

fn remove_record_from_area(
    root: &File,
    manifest: &mut Manifest,
    record: &AssetRecord,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    let Some(index) = manifest
        .assets
        .iter()
        .position(|asset| asset.identity == record.identity)
    else {
        return Ok(());
    };
    if manifest.assets[index].sha256 != record.sha256 {
        return Err(AssetError::new(
            ErrorCode::IdentityConflict,
            format!(
                "идентичность {} изменилась во время удаления записи жизненного цикла",
                record.identity
            ),
        ));
    }
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let (asset_parent, name) = open_storage_parent(root, &record.storage_path, false)?;
    let location = policy.canonical_location(&record.identity, &record.sha256, record.format)?;
    if record.storage_path != location.storage_path
        || record.consumer_filename != location.consumer_filename
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "удаляемая запись не соответствует canonical domain location",
        ));
    }
    if !file_matches_hash(&asset_parent, &name, &record.sha256)? {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "байты удаляемой записи не совпадают с manifest",
        ));
    }
    linkat(
        &asset_parent,
        name.as_str(),
        &temporary,
        REMOVAL_BACKUP,
        AtFlags::empty(),
    )
    .map_err(|error| {
        AssetError::io(
            "не удалось сохранить bytes перед удалением записи",
            std::io::Error::from(error),
        )
    })?;
    sync_directory(&temporary)?;
    let marker = RemovalTransaction {
        schema_version: 1,
        record: record.clone(),
    };
    let bytes = serde_json::to_vec(&marker)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    write_transaction_marker(&temporary, REMOVAL_MARKER, &bytes)?;
    manifest.assets.remove(index);
    manifest.revision = manifest.revision.checked_add(1).ok_or_else(|| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            "поле `revision` в манифесте переполнено",
        )
    })?;
    save_manifest(root, manifest, false)?;
    #[cfg(test)]
    if FAIL_AFTER_REMOVAL_MANIFEST.with(|hook| hook.replace(false)) {
        return Err(AssetError::new(
            ErrorCode::IoFailure,
            "тестовый сбой после записи manifest удаления",
        ));
    }
    remove_if_hash(
        &asset_parent,
        name.as_str(),
        &record.sha256,
        "байты удаляемой записи жизненного цикла",
    )?;
    sync_directory(&asset_parent)?;
    prune_empty_storage_directories(root, &record.storage_path)?;
    // Это штатное завершение удаления, а не восстановление после сбоя: след
    // восстановления здесь не публикуется.
    let _ = recover_removal(root, policy)?;
    Ok(())
}

fn validate_validation_record(
    record: &AssetRecord,
    validation: &ValidationRecord,
) -> Result<(), AssetError> {
    validate_hash(&validation.content_sha256)?;
    if validation.content_sha256 != record.sha256 || validation.evidence.is_empty() {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!(
                "semantic decision не привязан к текущему hash для {}",
                record.identity
            ),
        ));
    }
    validate_validator_identity(&validation.validator)?;
    if validation
        .evidence
        .iter()
        .any(|evidence| evidence.kind.trim().is_empty() || evidence.summary.trim().is_empty())
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            format!("evidence некорректен для {}", record.identity),
        ));
    }
    Ok(())
}

/// Полностью декодирует все кадры GIF или PNG; сигнатуры байтов недостаточно для
/// одобрения пользователем. Ограничения декодера сдерживают объём памяти для
/// одного кадра.
pub(crate) fn validate_image_decode(bytes: &[u8]) -> Result<(), AssetError> {
    use image::{AnimationDecoder, ImageDecoder};
    let failure = |message: String| {
        AssetError::new(
            ErrorCode::InvalidTransition,
            format!(
                "подтверждение человеком требует полной проверки изображения декодером: {message}"
            ),
        )
    };
    match DetectedFormat::from_signature(bytes) {
        DetectedFormat::Gif => {
            let mut decoder = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes))
                .map_err(|error| failure(error.to_string()))?;
            decoder
                .set_limits(image::Limits::default())
                .map_err(|error| failure(error.to_string()))?;
            let mut count = 0;
            for frame in decoder.into_frames() {
                frame.map_err(|error| failure(error.to_string()))?;
                count += 1;
            }
            if count == 0 {
                return Err(failure("GIF не содержит frames".into()));
            }
        }
        DetectedFormat::Png => {
            let decoder = image::codecs::png::PngDecoder::with_limits(
                std::io::Cursor::new(bytes),
                image::Limits::default(),
            )
            .map_err(|error| failure(error.to_string()))?;
            if decoder
                .is_apng()
                .map_err(|error| failure(error.to_string()))?
            {
                let mut count = 0;
                for frame in decoder
                    .apng()
                    .map_err(|error| failure(error.to_string()))?
                    .into_frames()
                {
                    frame.map_err(|error| failure(error.to_string()))?;
                    count += 1;
                }
                if count == 0 {
                    return Err(failure("APNG не содержит frames".into()));
                }
            } else {
                image::DynamicImage::from_decoder(decoder)
                    .map_err(|error| failure(error.to_string()))?;
            }
        }
        _ => return Err(failure("поддерживаются GIF и PNG".into())),
    }
    Ok(())
}

fn validate_asset(
    root: &File,
    record: &AssetRecord,
    policy: &dyn AssetDomainPolicy,
    legacy: bool,
) -> Result<(), AssetError> {
    let file = if legacy {
        checked_legacy_asset_file(root, record, policy)?
    } else {
        checked_asset_file(root, record, policy)?
    };
    let (sha256, byte_length, format) = hash_file(file)?;
    if sha256 != record.sha256 || byte_length != record.byte_length || format != record.format {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            format!("файл asset {} не совпадает с manifest", record.identity),
        ));
    }
    // `attest(Approve)` декодирует байты при принятии одобрения. При чтении
    // `read_verified` повторно декодирует данные только для записей с одобрением
    // человека; остальные проходят потоковую проверку хеша, размера и сигнатуры.
    Ok(())
}

fn validate_asset_directory(
    root: &File,
    registered_paths: &BTreeSet<String>,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    validate_asset_directory_impl(root, registered_paths, false, policy)
}

fn validate_asset_directory_exact(
    root: &File,
    manifest: &Manifest,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    let registered_paths = manifest
        .assets
        .iter()
        .map(|asset| asset.storage_path.clone())
        .collect();
    validate_asset_directory_impl(root, &registered_paths, true, policy)
}

fn validate_asset_directory_impl(
    root: &File,
    registered_paths: &BTreeSet<String>,
    reject_orphans: bool,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    let assets = match open_directory_at(root, ASSETS_DIR) {
        Ok(assets) => assets,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound && registered_paths.is_empty() =>
        {
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "каталог assets отсутствует в program-owned store",
            ));
        }
        Err(error) => {
            return Err(AssetError::io(
                "не удалось открыть canonical asset store",
                error,
            ));
        }
    };
    let mut observed_paths = BTreeSet::new();
    scan_asset_directory(
        &assets,
        ASSETS_DIR,
        registered_paths,
        &mut observed_paths,
        reject_orphans,
        policy,
    )?;
    if !registered_paths.is_subset(&observed_paths) {
        return Err(AssetError::new(
            ErrorCode::MissingAssetFile,
            "manifest ссылается на отсутствующий canonical asset",
        ));
    }
    if reject_orphans && !observed_paths.is_subset(registered_paths) {
        return Err(AssetError::new(
            ErrorCode::UnexpectedPath,
            "в публикуемом корпусе есть незарегистрированные байты",
        ));
    }
    Ok(())
}

fn scan_asset_directory(
    directory: &File,
    relative_path: &str,
    registered_paths: &BTreeSet<String>,
    observed_paths: &mut BTreeSet<String>,
    reject_orphans: bool,
    policy: &dyn AssetDomainPolicy,
) -> Result<(), AssetError> {
    for entry in fs::read_dir(fd_path(directory))
        .map_err(|error| AssetError::io("не удалось прочитать canonical asset directory", error))?
    {
        let entry =
            entry.map_err(|error| AssetError::io("не удалось прочитать asset entry", error))?;
        let name = entry.file_name();
        let component = name.to_string_lossy();
        let path = format!("{relative_path}/{component}");
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| AssetError::io("не удалось проверить asset entry", error))?;
        if metadata.file_type().is_symlink() {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                format!("symlink запрещён внутри canonical assets: {path}"),
            ));
        }
        if metadata.is_dir() {
            if !registered_paths
                .iter()
                .any(|registered| registered.starts_with(&format!("{path}/")))
            {
                if !reject_orphans {
                    let child = open_directory_at(directory, &name)
                        .map_err(|error| directory_entry_error(&path, error))?;
                    if is_directory_empty(&child, &path)? {
                        continue;
                    }
                }
                return Err(AssetError::new(
                    ErrorCode::UnexpectedPath,
                    format!("неожиданный каталог внутри canonical assets: {path}"),
                ));
            }
            let child = open_directory_at(directory, &name)
                .map_err(|error| directory_entry_error(&path, error))?;
            scan_asset_directory(
                &child,
                &path,
                registered_paths,
                observed_paths,
                reject_orphans,
                policy,
            )?;
            continue;
        }

        let file = open_regular_at(directory, &name, ErrorCode::MissingAssetFile)?;
        let (actual_hash, _, format) = hash_file(file)?;
        if registered_paths.contains(&path) {
            if crate::domain::extension_for_format(format)
                != component.rsplit('.').next().unwrap_or("")
            {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    format!("asset {path} не соответствует формату в manifest"),
                ));
            }
            observed_paths.insert(path);
            continue;
        }
        if reject_orphans || !policy.content_addressed_storage() {
            return Err(AssetError::new(
                ErrorCode::UnexpectedPath,
                format!("незарегистрированный файл в canonical asset store: {path}"),
            ));
        }
        if path.matches('/').count() != 1 {
            return Err(AssetError::new(
                ErrorCode::UnexpectedPath,
                format!("hash-addressed orphan должен быть непосредственно в assets/: {path}"),
            ));
        }
        let Some(hash) = embedded_content_hash(&component) else {
            return Err(AssetError::new(
                ErrorCode::UnexpectedPath,
                format!("неизвестный файл в canonical asset store: {path}"),
            ));
        };
        validate_hash(hash)?;
        if actual_hash != hash
            || crate::domain::extension_for_format(format)
                != component.rsplit('.').next().unwrap_or("")
        {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                format!("asset {path} не соответствует SHA-256/формату в имени"),
            ));
        }
        observed_paths.insert(path);
    }
    Ok(())
}

fn is_directory_empty(directory: &File, path: &str) -> Result<bool, AssetError> {
    let mut entries = fs::read_dir(fd_path(directory)).map_err(|error| {
        AssetError::io(
            format!("не удалось прочитать каталог assets: {path}"),
            error,
        )
    })?;
    match entries.next() {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(error)) => Err(AssetError::io(
            format!("не удалось прочитать каталог assets: {path}"),
            error,
        )),
    }
}

fn validate_relative_path(actual: &str, expected: &str) -> Result<(), AssetError> {
    let path = Path::new(actual);
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(AssetError::new(
            ErrorCode::PathTraversal,
            "storage_path содержит выход за store root",
        ));
    }
    if actual != expected {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "storage_path не совпадает с canonical asset layout",
        ));
    }
    Ok(())
}

/// Открывает родительский каталог канонического пути хранения через
/// последовательные файловые дескрипторы каталогов (`dirfd`) и `NOFOLLOW`;
/// промежуточная символическая ссылка (`symlink`) никогда не передаётся в
/// `openat` вместе со следующим компонентом пути.
fn open_storage_parent(
    root: &File,
    storage_path: &str,
    create_parents: bool,
) -> Result<(File, String), AssetError> {
    crate::domain::validate_safe_storage_path(storage_path)?;
    let relative = storage_path.strip_prefix("assets/").ok_or_else(|| {
        AssetError::new(ErrorCode::PathTraversal, "storage_path вне каталога assets")
    })?;
    let components: Vec<_> = relative.split('/').collect();
    let (leaf, directories) = components
        .split_last()
        .ok_or_else(|| AssetError::new(ErrorCode::PathTraversal, "storage_path пуст"))?;
    let mut directory = open_directory_at(root, ASSETS_DIR)
        .map_err(|error| directory_entry_error(ASSETS_DIR, error))?;
    for component in directories {
        directory = if create_parents {
            ensure_dir_entry(&directory, component)?
        } else {
            match open_directory_at(&directory, *component) {
                Ok(directory) => directory,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(AssetError::new(
                        ErrorCode::MissingAssetFile,
                        "промежуточный каталог canonical asset отсутствует",
                    ));
                }
                Err(error) => return Err(directory_entry_error(component, error)),
            }
        };
    }
    Ok((directory, (*leaf).to_owned()))
}

fn open_storage_file(
    root: &File,
    storage_path: &str,
    create_parents: bool,
) -> Result<File, AssetError> {
    let (parent, leaf) = open_storage_parent(root, storage_path, create_parents)?;
    open_regular_at(&parent, &leaf, ErrorCode::MissingAssetFile)
}

fn remove_storage_path_if_hash(
    root: &File,
    storage_path: &str,
    expected_hash: &str,
    description: &str,
) -> Result<(), AssetError> {
    let (parent, leaf) = match open_storage_parent(root, storage_path, false) {
        Ok(value) => value,
        Err(error) if error.code == ErrorCode::MissingAssetFile => return Ok(()),
        Err(error) => return Err(error),
    };
    match open_regular_at(&parent, &leaf, ErrorCode::MissingAssetFile) {
        Ok(file) => {
            if hash_file(file)?.0 != expected_hash {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    format!("{description} bytes не совпадают с ожидаемым SHA-256"),
                ));
            }
            unlinkat(&parent, leaf.as_str(), AtFlags::empty()).map_err(|error| {
                AssetError::io(
                    format!("не удалось удалить {description}"),
                    std::io::Error::from(error),
                )
            })?;
            sync_directory(&parent)?;
        }
        Err(error) if error.code == ErrorCode::MissingAssetFile => return Ok(()),
        Err(error) => return Err(error),
    }
    prune_empty_storage_directories(root, storage_path)
}

fn prune_empty_storage_directories(root: &File, storage_path: &str) -> Result<(), AssetError> {
    crate::domain::validate_safe_storage_path(storage_path)?;
    let relative = storage_path.strip_prefix("assets/").ok_or_else(|| {
        AssetError::new(ErrorCode::PathTraversal, "storage_path вне каталога assets")
    })?;
    let components: Vec<_> = relative.split('/').collect();
    if components.len() < 2 {
        return Ok(());
    }
    for depth in (1..components.len()).rev() {
        let directory_path = format!("assets/{}", components[..depth].join("/"));
        let (parent, leaf) = open_storage_parent(root, &directory_path, false)?;
        match unlinkat(&parent, leaf.as_str(), AtFlags::REMOVEDIR) {
            Ok(()) => sync_directory(&parent)?,
            Err(error)
                if std::io::Error::from(error).kind() == std::io::ErrorKind::DirectoryNotEmpty => {}
            Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(AssetError::io(
                    "не удалось удалить пустой каталог canonical assets",
                    std::io::Error::from(error),
                ));
            }
        }
    }
    Ok(())
}

fn checked_asset_file(
    root: &File,
    record: &AssetRecord,
    policy: &dyn AssetDomainPolicy,
) -> Result<File, AssetError> {
    policy.validate_identity(&record.identity)?;
    let expected = policy.canonical_location(&record.identity, &record.sha256, record.format)?;
    validate_relative_path(&record.storage_path, &expected.storage_path)?;
    validate_record_consumer_filename(record)?;
    if record.consumer_filename != expected.consumer_filename {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "consumer_filename не совпадает с canonical domain location",
        ));
    }
    open_storage_file(root, &record.storage_path, false).map_err(|error| {
        if error.code == ErrorCode::MissingAssetFile {
            AssetError::with_details(
                error.code,
                error.message,
                serde_json::json!({
                    "identity": record.identity,
                    "storage_path": record.storage_path,
                }),
            )
        } else {
            error
        }
    })
}

fn checked_legacy_asset_file(
    root: &File,
    record: &AssetRecord,
    policy: &dyn AssetDomainPolicy,
) -> Result<File, AssetError> {
    policy.validate_identity(&record.identity)?;
    let expected = policy
        .legacy_location(&record.identity, &record.sha256, record.format)
        .ok_or_else(|| {
            AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                "domain policy не поддерживает legacy layout",
            )
        })?;
    validate_relative_path(&record.storage_path, &expected.storage_path)?;
    open_storage_file(root, &record.storage_path, false).map_err(|error| {
        if error.code == ErrorCode::MissingAssetFile {
            AssetError::with_details(
                error.code,
                error.message,
                serde_json::json!({
                    "identity": record.identity,
                    "storage_path": record.storage_path,
                }),
            )
        } else {
            error
        }
    })
}

fn validate_record_consumer_filename(record: &AssetRecord) -> Result<(), AssetError> {
    crate::domain::validate_safe_consumer_filename(&record.consumer_filename, record.format)
}

fn read_bounded_asset_bytes(
    file: File,
    maximum: usize,
    operation: &str,
) -> Result<Vec<u8>, AssetError> {
    let mut bytes = Vec::new();
    file.take((maximum as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| AssetError::io(operation, error))?;
    if bytes.len() > maximum {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            format!("{operation}: изображение превышает предел {maximum} байт"),
        ));
    }
    Ok(bytes)
}

fn embedded_content_hash(filename: &str) -> Option<&str> {
    let (stem, _) = filename.rsplit_once('.')?;
    let (_, hash) = stem.rsplit_once('-')?;
    (hash.len() == 64).then_some(hash)
}

fn validate_hash(hash: &str) -> Result<(), AssetError> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "SHA-256 должен состоять из 64 строчных hex-символов",
        ));
    }
    Ok(())
}

fn stage_source(root: &File, mut input: File) -> Result<StagedObject, AssetError> {
    if !input
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить source handle", error))?
        .is_file()
    {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "explicit source должен быть обычным файлом без symlink",
        ));
    }
    let (artifact, mut output) = create_temp_file(root, "ingest")?;
    let mut digest = Sha256::new();
    let mut byte_length = 0_u64;
    let mut signature = Vec::with_capacity(12);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| AssetError::io("не удалось прочитать source file", error))?;
        if count == 0 {
            break;
        }
        byte_length = byte_length.checked_add(count as u64).ok_or_else(|| {
            AssetError::new(ErrorCode::IoFailure, "размер source file переполнен")
        })?;
        if signature.len() < 12 {
            let take = (12 - signature.len()).min(count);
            signature.extend_from_slice(&buffer[..take]);
        }
        digest.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|error| AssetError::io("не удалось записать staging file", error))?;
    }
    output
        .flush()
        .and_then(|()| output.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать staging file", error))?;
    let sha256 = encode_lower_hex(digest.finalize());
    Ok(StagedObject {
        artifact,
        sha256,
        byte_length,
        format: DetectedFormat::from_signature(&signature),
    })
}

fn stage_bytes(root: &File, bytes: &[u8]) -> Result<StagedObject, AssetError> {
    let (artifact, mut output) = create_temp_file(root, "candidate")?;
    output
        .write_all(bytes)
        .and_then(|()| output.flush())
        .and_then(|()| output.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать candidate staging", error))?;
    let digest = Sha256::digest(bytes);
    Ok(StagedObject {
        artifact,
        sha256: encode_lower_hex(digest),
        byte_length: bytes.len() as u64,
        format: DetectedFormat::from_signature(&bytes[..bytes.len().min(12)]),
    })
}

fn publish_object(
    staged: &StagedObject,
    root: &File,
    storage_path: &str,
) -> Result<(), AssetError> {
    let (parent, name) = open_storage_parent(root, storage_path, true)?;
    match open_regular_at(&parent, &name, ErrorCode::MissingAssetFile) {
        Ok(existing) => {
            verify_staged_file(staged, existing)?;
            sync_directory(&parent)
        }
        Err(error) if error.code == ErrorCode::MissingAssetFile => {
            match linkat(
                &staged.artifact.directory,
                &staged.artifact.name,
                &parent,
                name.as_str(),
                AtFlags::empty(),
            ) {
                Ok(()) => {
                    let asset = open_regular_at(&parent, &name, ErrorCode::MissingAssetFile)?;
                    verify_staged_file(staged, asset)?;
                    sync_directory(&parent)
                }
                Err(link_error)
                    if std::io::Error::from(link_error).kind()
                        == std::io::ErrorKind::AlreadyExists =>
                {
                    let existing = open_regular_at(&parent, &name, ErrorCode::MissingAssetFile)?;
                    verify_staged_file(staged, existing)
                }
                Err(link_error) => Err(AssetError::io(
                    "не удалось опубликовать canonical asset",
                    std::io::Error::from(link_error),
                )),
            }
        }
        Err(error) => Err(error),
    }
}

fn verify_staged_file(staged: &StagedObject, file: File) -> Result<(), AssetError> {
    let (hash, length, format) = hash_file(
        file.try_clone()
            .map_err(|error| AssetError::io("не удалось открыть опубликованный object", error))?,
    )?;
    if hash != staged.sha256 || length != staged.byte_length || format != staged.format {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "canonical asset не совпадает с вычисленным hash",
        ));
    }
    make_readonly(&file)
}

fn make_readonly(file: &File) -> Result<(), AssetError> {
    let metadata = file
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить object permissions", error))?;
    if !metadata.is_file() {
        return Err(AssetError::new(
            ErrorCode::BoundaryViolation,
            "object должен быть обычным файлом",
        ));
    }
    let mut permissions = metadata.permissions();
    permissions.set_readonly(true);
    file.set_permissions(permissions)
        .map_err(|error| AssetError::io("не удалось защитить object от случайной записи", error))?;
    file.sync_all()
        .map_err(|error| AssetError::io("не удалось синхронизировать object metadata", error))
}

fn save_manifest(root: &File, manifest: &Manifest, initial: bool) -> Result<(), AssetError> {
    let mut current = manifest.clone();
    current.schema_version = MANIFEST_SCHEMA_VERSION;
    let bytes = serde_json::to_vec_pretty(&current)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    atomic_write(root, MANIFEST_FILE, &bytes, initial, None)
}

#[cfg(test)]
fn save_manifest_with_test_hook(
    root: &File,
    manifest: &Manifest,
    initial: bool,
    fail_before_commit: &std::sync::atomic::AtomicBool,
) -> Result<(), AssetError> {
    let mut current = manifest.clone();
    current.schema_version = MANIFEST_SCHEMA_VERSION;
    let bytes = serde_json::to_vec_pretty(&current)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    atomic_write(
        root,
        MANIFEST_FILE,
        &bytes,
        initial,
        Some(fail_before_commit),
    )
}

fn atomic_write(
    root: &File,
    destination: &str,
    bytes: &[u8],
    initial: bool,
    #[allow(unused_variables)] fail_before_commit: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(), AssetError> {
    let (temporary, mut file) = create_temp_file(root, "state")?;
    file.write_all(bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать state file", error))?;
    drop(file);

    #[cfg(test)]
    if fail_before_commit.is_some_and(|hook| hook.swap(false, Ordering::SeqCst)) {
        return Err(AssetError::new(
            ErrorCode::IoFailure,
            "тестовая ошибка после подготовки temporary manifest и до canonical commit",
        ));
    }

    let published = if initial {
        linkat(
            &temporary.directory,
            &temporary.name,
            root,
            destination,
            AtFlags::empty(),
        )
    } else {
        renameat(&temporary.directory, &temporary.name, root, destination)
    };
    match published {
        Ok(()) => sync_directory(root),
        Err(error)
            if initial
                && std::io::Error::from(error).kind() == std::io::ErrorKind::AlreadyExists =>
        {
            Err(AssetError::new(
                ErrorCode::StoreNotOwned,
                "state file уже существует при инициализации нового store",
            ))
        }
        Err(error) => Err(AssetError::io(
            "не удалось атомарно опубликовать state file",
            std::io::Error::from(error),
        )),
    }
}

fn create_temp_file(root: &File, prefix: &str) -> Result<(TempArtifact, File), AssetError> {
    let directory = open_directory_at(root, TEMP_DIR)
        .map_err(|error| AssetError::io("не удалось открыть каталог temporary files", error))?;
    create_temp_in_directory(&directory, prefix)
}

fn create_temp_in_directory(
    directory: &File,
    prefix: &str,
) -> Result<(TempArtifact, File), AssetError> {
    for _ in 0..128 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!("{prefix}-{}-{counter}.tmp", std::process::id()));
        match openat(
            directory,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(file) => {
                return Ok((
                    TempArtifact {
                        directory: directory.try_clone().map_err(|error| {
                            AssetError::io("не удалось клонировать temp dir", error)
                        })?,
                        name,
                    },
                    File::from(file),
                ));
            }
            Err(error)
                if std::io::Error::from(error).kind() == std::io::ErrorKind::AlreadyExists =>
            {
                continue;
            }
            Err(error) => {
                return Err(AssetError::io(
                    "не удалось создать временный файл",
                    std::io::Error::from(error),
                ));
            }
        }
    }
    Err(AssetError::new(
        ErrorCode::IoFailure,
        "не удалось выбрать свободное имя временного файла",
    ))
}

fn hash_file(mut file: File) -> Result<(String, u64, DetectedFormat), AssetError> {
    let mut digest = Sha256::new();
    let mut byte_length = 0_u64;
    let mut signature = Vec::with_capacity(12);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| AssetError::io("не удалось прочитать object", error))?;
        if count == 0 {
            break;
        }
        byte_length = byte_length.checked_add(count as u64).ok_or_else(|| {
            AssetError::new(ErrorCode::IntegrityMismatch, "размер object переполнен")
        })?;
        if signature.len() < 12 {
            let take = (12 - signature.len()).min(count);
            signature.extend_from_slice(&buffer[..take]);
        }
        digest.update(&buffer[..count]);
    }
    Ok((
        encode_lower_hex(digest.finalize()),
        byte_length,
        DetectedFormat::from_signature(&signature),
    ))
}

fn commit_asset_record<F>(
    root: &File,
    staged: &StagedObject,
    previous: Option<&AssetRecord>,
    record: &AssetRecord,
    manifest: &mut Manifest,
    policy: &dyn AssetDomainPolicy,
    save: F,
) -> Result<(), AssetError>
where
    F: FnOnce(&File, &Manifest) -> Result<(), AssetError>,
{
    ensure_unique_consumer_filename(manifest, record)?;
    // Внутренние восстановления относятся к уже идущей операции: её событие
    // журнала закрывается отдельно, поэтому следы здесь не публикуются.
    let _ = recover_publications(root, policy)?;
    let next_revision = manifest.revision.checked_add(1).ok_or_else(|| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            "поле `revision` в манифесте переполнено",
        )
    })?;
    let needs_stable_publication = !policy.content_addressed_storage()
        && previous.is_none_or(|asset| {
            asset.sha256 != record.sha256 || asset.storage_path != record.storage_path
        });
    let publication = if needs_stable_publication {
        Some(prepare_publication(root, staged, previous, record, policy)?)
    } else {
        None
    };

    let result = (|| {
        if let Some(publication) = &publication {
            apply_publication(root, staged, publication)?;
        } else {
            publish_object(staged, root, &record.storage_path)?;
        }
        validate_asset(root, record, policy, false)?;
        if let Some(slot) = manifest
            .assets
            .iter_mut()
            .find(|asset| asset.identity == record.identity)
        {
            *slot = record.clone();
        } else {
            manifest.assets.push(record.clone());
        }
        sort_assets(&mut manifest.assets);
        manifest.revision = next_revision;
        save(root, manifest)
    })();

    if let Err(error) = result {
        if let Some(publication) = &publication {
            let _ = recover_publication(root, &publication.marker_name, policy)?;
        }
        return Err(error);
    }
    if let Some(publication) = publication {
        let _ = recover_publication(root, &publication.marker_name, policy)?;
    }
    Ok(())
}

fn ensure_unique_consumer_filename(
    manifest: &Manifest,
    record: &AssetRecord,
) -> Result<(), AssetError> {
    if manifest.assets.iter().any(|asset| {
        asset.identity != record.identity && asset.consumer_filename == record.consumer_filename
    }) {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "manifest содержит повторяющиеся consumer_filename",
        ));
    }
    Ok(())
}

fn prepare_publication(
    root: &File,
    staged: &StagedObject,
    previous: Option<&AssetRecord>,
    next: &AssetRecord,
    policy: &dyn AssetDomainPolicy,
) -> Result<PendingPublication, AssetError> {
    if policy.content_addressed_storage() {
        return Err(AssetError::new(
            ErrorCode::InvalidIdentity,
            "stable publication запрещена для content-addressed domain",
        ));
    }
    policy.validate_identity(&next.identity)?;
    let expected = policy.canonical_location(&next.identity, &next.sha256, next.format)?;
    if expected.storage_path != next.storage_path
        || expected.consumer_filename != next.consumer_filename
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "stable publication record не соответствует domain policy",
        ));
    }
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| AssetError::io("не удалось открыть каталог temporary files", error))?;
    let (next_parent, next_name) = open_storage_parent(root, &next.storage_path, true)?;
    let previous_object = previous.map(|asset| PublicationObject {
        storage_path: asset.storage_path.clone(),
        sha256: asset.sha256.clone(),
        format: asset.format,
    });
    if let Some(previous) = previous {
        let (previous_parent, previous_name) =
            open_storage_parent(root, &previous.storage_path, false)?;
        if !file_matches_hash(&previous_parent, &previous_name, &previous.sha256)? {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "предыдущий asset изменился до compare-and-swap публикации",
            ));
        }
        if previous.storage_path != next.storage_path {
            ensure_asset_path_absent(&next_parent, &next_name, &next.storage_path)?;
        }
    } else {
        ensure_asset_path_absent(&next_parent, &next_name, &next.storage_path)?;
    }

    for _ in 0..128 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let id = format!("{}-{counter}", std::process::id());
        let marker_name = OsString::from(format!("publication-{id}.json"));
        let backup_name = previous_object
            .as_ref()
            .filter(|previous| previous.storage_path == next.storage_path)
            .map(|_| format!("publication-{id}.backup"));
        let transaction = PublicationTransaction {
            schema_version: 1,
            identity: next.identity.clone(),
            previous: previous_object.clone(),
            next: PublicationObject {
                storage_path: next.storage_path.clone(),
                sha256: next.sha256.clone(),
                format: next.format,
            },
            staged_name: staged.artifact.name.to_string_lossy().into_owned(),
            backup_name: backup_name.clone(),
        };
        if !write_publication_marker(&temporary, &marker_name, &transaction)? {
            continue;
        }
        let pending = PendingPublication {
            marker_name,
            transaction,
        };
        if let (Some(previous), Some(backup_name)) = (previous, backup_name) {
            let (previous_parent, previous_name) =
                open_storage_parent(root, &previous.storage_path, false)?;
            if let Err(error) = linkat(
                &previous_parent,
                previous_name.as_str(),
                &temporary,
                backup_name.as_str(),
                AtFlags::empty(),
            ) {
                let _ = recover_publication(root, &pending.marker_name, policy)?;
                return Err(AssetError::io(
                    "не удалось сохранить предыдущий asset для CAS",
                    std::io::Error::from(error),
                ));
            }
            if !file_matches_hash(&temporary, backup_name.as_str(), &previous.sha256)? {
                let _ = recover_publication(root, &pending.marker_name, policy)?;
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "backup предыдущего asset не прошёл SHA-256 проверку",
                ));
            }
            sync_directory(&temporary)?;
        }
        return Ok(pending);
    }
    Err(AssetError::new(
        ErrorCode::IoFailure,
        "не удалось выбрать свободное имя publication transaction",
    ))
}

fn write_publication_marker(
    temporary: &File,
    marker_name: &OsString,
    transaction: &PublicationTransaction,
) -> Result<bool, AssetError> {
    let bytes = serde_json::to_vec_pretty(transaction)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    let (artifact, mut file) = create_temp_in_directory(temporary, "publication-state")?;
    file.write_all(&bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("не удалось синхронизировать publication marker", error))?;
    drop(file);
    match linkat(
        &artifact.directory,
        &artifact.name,
        temporary,
        marker_name,
        AtFlags::empty(),
    ) {
        Ok(()) => {
            if let Err(error) = sync_directory(temporary) {
                let _ = unlink_if_exists(temporary, marker_name);
                let _ = sync_directory(temporary);
                Err(error)
            } else {
                Ok(true)
            }
        }
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::AlreadyExists => {
            Ok(false)
        }
        Err(error) => Err(AssetError::io(
            "не удалось атомарно сохранить publication marker",
            std::io::Error::from(error),
        )),
    }
}

fn apply_publication(
    root: &File,
    staged: &StagedObject,
    publication: &PendingPublication,
) -> Result<(), AssetError> {
    let (asset_parent, next_name) =
        open_storage_parent(root, &publication.transaction.next.storage_path, true)?;
    let staged_file = open_regular_at(
        &staged.artifact.directory,
        &staged.artifact.name,
        ErrorCode::MissingAssetFile,
    )?;
    verify_staged_contents(staged, staged_file)?;
    renameat(
        &staged.artifact.directory,
        &staged.artifact.name,
        &asset_parent,
        next_name.as_str(),
    )
    .map_err(|error| {
        AssetError::io(
            "не удалось атомарно опубликовать stable asset",
            std::io::Error::from(error),
        )
    })?;
    sync_directory(&staged.artifact.directory)?;
    sync_directory(&asset_parent)?;
    let published = open_regular_at(&asset_parent, &next_name, ErrorCode::MissingAssetFile)?;
    verify_staged_file(staged, published)?;
    sync_directory(&asset_parent)
}

fn verify_staged_contents(staged: &StagedObject, file: File) -> Result<(), AssetError> {
    let (hash, length, format) = hash_file(file)?;
    if hash != staged.sha256 || length != staged.byte_length || format != staged.format {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "staged asset не совпадает с вычисленным SHA-256",
        ));
    }
    Ok(())
}

fn recover_publications(
    root: &File,
    policy: &dyn AssetDomainPolicy,
) -> Result<Vec<RecoveryFact>, AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| AssetError::io("не удалось открыть каталог temporary files", error))?;
    let mut markers = Vec::new();
    for entry in fs::read_dir(fd_path(&temporary))
        .map_err(|error| AssetError::io("не удалось прочитать transaction directory", error))?
    {
        let entry = entry
            .map_err(|error| AssetError::io("не удалось прочитать transaction entry", error))?;
        let name = entry.file_name();
        let text = name.to_string_lossy();
        if text.starts_with("publication-") && text.ends_with(".json") {
            markers.push(name);
        }
    }
    markers.sort();
    let mut facts = Vec::new();
    for marker in markers {
        if let Some(fact) = recover_publication(root, &marker, policy)? {
            facts.push(fact);
        }
    }
    if let Some(fact) = recover_removal(root, policy)? {
        facts.push(fact);
    }
    Ok(facts)
}

fn write_transaction_marker(directory: &File, name: &str, bytes: &[u8]) -> Result<(), AssetError> {
    let (artifact, mut file) = create_temp_in_directory(directory, "transaction-state")?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|error| AssetError::io("не удалось сохранить transaction marker", error))?;
    drop(file);
    renameat(directory, &artifact.name, directory, name).map_err(|error| {
        AssetError::io(
            "не удалось опубликовать transaction marker",
            std::io::Error::from(error),
        )
    })?;
    sync_directory(directory)
}

fn begin_transition(
    root: &File,
    identity: &AssetIdentity,
    target: TransitionTarget,
    sha256: &str,
) -> Result<(), AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let marker = TransitionTransaction {
        schema_version: 1,
        identity: identity.clone(),
        target,
        sha256: sha256.to_owned(),
    };
    let bytes = serde_json::to_vec(&marker)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    write_transaction_marker(&temporary, TRANSITION_MARKER, &bytes)
}

fn load_transition(root: &File) -> Result<Option<TransitionTransaction>, AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let file = match open_regular_at(&temporary, TRANSITION_MARKER, ErrorCode::MissingAssetFile) {
        Ok(file) => file,
        Err(error) if error.code == ErrorCode::MissingAssetFile => return Ok(None),
        Err(error) => return Err(error),
    };
    let marker: TransitionTransaction = serde_json::from_reader(file)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    if marker.schema_version != 1 || marker.identity.validate().is_err() {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "transition marker повреждён",
        ));
    }
    validate_hash(&marker.sha256)?;
    Ok(Some(marker))
}

fn clear_transition(root: &File) -> Result<(), AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    unlink_if_exists(&temporary, &OsString::from(TRANSITION_MARKER))?;
    sync_directory(&temporary)
}

fn recover_removal(
    root: &File,
    policy: &dyn AssetDomainPolicy,
) -> Result<Option<RecoveryFact>, AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| directory_entry_error(TEMP_DIR, error))?;
    let marker_file = match open_regular_at(&temporary, REMOVAL_MARKER, ErrorCode::MissingAssetFile)
    {
        Ok(file) => file,
        Err(error) if error.code == ErrorCode::MissingAssetFile => {
            unlink_if_exists(&temporary, &OsString::from(REMOVAL_BACKUP))?;
            sync_directory(&temporary)?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let transaction: RemovalTransaction = serde_json::from_reader(marker_file)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    if transaction.schema_version != 1 {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "removal marker повреждён",
        ));
    }
    validate_hash(&transaction.record.sha256)?;
    policy.validate_identity(&transaction.record.identity)?;
    let manifest = load_owned_manifest(root)?;
    ensure_store_domain(&manifest, policy)?;
    let expected = if manifest.schema_version < MANIFEST_SCHEMA_VERSION {
        policy.legacy_location(
            &transaction.record.identity,
            &transaction.record.sha256,
            transaction.record.format,
        )
    } else {
        Some(policy.canonical_location(
            &transaction.record.identity,
            &transaction.record.sha256,
            transaction.record.format,
        )?)
    }
    .ok_or_else(|| {
        AssetError::new(
            ErrorCode::UnsupportedSchemaVersion,
            "legacy location отсутствует",
        )
    })?;
    validate_relative_path(&transaction.record.storage_path, &expected.storage_path)?;
    if transaction.record.consumer_filename != expected.consumer_filename
        && !(manifest.schema_version < MANIFEST_SCHEMA_VERSION
            && transaction.record.consumer_filename.is_empty())
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "removal marker consumer_filename не совпадает с domain policy",
        ));
    }
    let (asset_parent, name) = open_storage_parent(root, &transaction.record.storage_path, true)?;
    let fact = match manifest
        .assets
        .iter()
        .find(|asset| asset.identity == transaction.record.identity)
    {
        Some(current)
            if current.sha256 == transaction.record.sha256
                && current.storage_path == transaction.record.storage_path =>
        {
            match open_regular_at(&asset_parent, &name, ErrorCode::MissingAssetFile) {
                Ok(file) => {
                    if hash_file(file)?.0 != current.sha256 {
                        return Err(AssetError::new(
                            ErrorCode::IntegrityMismatch,
                            "байты удаляемого asset изменились во время восстановления",
                        ));
                    }
                }
                Err(error) if error.code == ErrorCode::MissingAssetFile => {
                    if !file_matches_hash(&temporary, REMOVAL_BACKUP, &current.sha256)? {
                        return Err(AssetError::new(
                            ErrorCode::IntegrityMismatch,
                            "backup удаляемого asset отсутствует",
                        ));
                    }
                    let (restore_parent, restore_name) =
                        open_storage_parent(root, &transaction.record.storage_path, true)?;
                    renameat(
                        &temporary,
                        REMOVAL_BACKUP,
                        &restore_parent,
                        restore_name.as_str(),
                    )
                    .map_err(|error| {
                        AssetError::io(
                            "не удалось восстановить удаляемый asset",
                            std::io::Error::from(error),
                        )
                    })?;
                    sync_directory(&restore_parent)?;
                }
                Err(error) => return Err(error),
            }
            RecoveryFact::new(
                transaction.record.identity.clone(),
                Some(current.sha256.clone()),
                Some(current.sha256.clone()),
                Some(current.lifecycle),
                "удаление записи откатано: manifest сохранил запись",
            )
        }
        None => {
            remove_storage_path_if_hash(
                root,
                &transaction.record.storage_path,
                &transaction.record.sha256,
                "удаляемый asset",
            )?;
            RecoveryFact::new(
                transaction.record.identity.clone(),
                Some(transaction.record.sha256.clone()),
                None,
                None,
                "удаление записи завершено после сбоя: bytes удалены",
            )
        }
        Some(_) => {
            return Err(AssetError::new(
                ErrorCode::IdentityConflict,
                "identity изменилась при восстановлении удаления",
            ));
        }
    };
    remove_if_hash(
        &temporary,
        REMOVAL_BACKUP,
        &transaction.record.sha256,
        "removal backup",
    )?;
    unlink_if_exists(&temporary, &OsString::from(REMOVAL_MARKER))?;
    sync_directory(&temporary)?;
    prune_empty_storage_directories(root, &transaction.record.storage_path)?;
    Ok(Some(fact))
}

fn recover_publication(
    root: &File,
    marker_name: &OsString,
    policy: &dyn AssetDomainPolicy,
) -> Result<Option<RecoveryFact>, AssetError> {
    let temporary = open_directory_at(root, TEMP_DIR)
        .map_err(|error| AssetError::io("не удалось открыть каталог temporary files", error))?;
    validate_publication_names(marker_name, None, None)?;
    let marker_file = open_regular_at(&temporary, marker_name, ErrorCode::MissingAssetFile)?;
    let transaction: PublicationTransaction = serde_json::from_reader(marker_file)
        .map_err(|error| AssetError::new(ErrorCode::ManifestCorrupt, error.to_string()))?;
    let manifest = load_owned_manifest(root)?;
    ensure_store_domain(&manifest, policy)?;
    validate_publication_transaction(
        marker_name,
        &transaction,
        policy,
        manifest.schema_version < MANIFEST_SCHEMA_VERSION,
    )?;
    let current = manifest
        .assets
        .iter()
        .find(|asset| asset.identity == transaction.identity);
    let committed = current.is_some_and(|asset| {
        asset.sha256 == transaction.next.sha256
            && asset.storage_path == transaction.next.storage_path
    });
    let previous_state = match (&transaction.previous, current) {
        (Some(previous), Some(asset)) => {
            asset.sha256 == previous.sha256 && asset.storage_path == previous.storage_path
        }
        (None, None) => true,
        _ => false,
    };
    let fact = if committed {
        if !storage_file_matches_hash(
            root,
            &transaction.next.storage_path,
            &transaction.next.sha256,
        )? {
            return Err(AssetError::new(
                ErrorCode::IntegrityMismatch,
                "manifest commit ссылается на не опубликованный asset",
            ));
        }
        if let Some(previous) = &transaction.previous
            && previous.storage_path != transaction.next.storage_path
        {
            remove_storage_path_if_hash(
                root,
                &previous.storage_path,
                &previous.sha256,
                "предыдущий asset",
            )?;
        }
        RecoveryFact::new(
            transaction.identity.clone(),
            transaction
                .previous
                .as_ref()
                .map(|previous| previous.sha256.clone()),
            Some(transaction.next.sha256.clone()),
            current.map(|asset| asset.lifecycle),
            "публикация завершена после сбоя: manifest уже содержал новый asset",
        )
    } else if previous_state {
        if let Some(previous) = &transaction.previous {
            let (previous_parent, previous_name) =
                open_storage_parent(root, &previous.storage_path, true)?;
            if previous.storage_path == transaction.next.storage_path
                && !file_matches_hash(&previous_parent, &previous_name, &previous.sha256)?
            {
                let backup_name = transaction.backup_name.as_deref().ok_or_else(|| {
                    AssetError::new(
                        ErrorCode::ManifestCorrupt,
                        "publication transaction не содержит backup для in-place CAS",
                    )
                })?;
                if !file_matches_hash(&temporary, backup_name, &previous.sha256)? {
                    return Err(AssetError::new(
                        ErrorCode::IntegrityMismatch,
                        "не удалось восстановить предыдущие asset bytes после сбоя публикации",
                    ));
                }
                renameat(
                    &temporary,
                    backup_name,
                    &previous_parent,
                    previous_name.as_str(),
                )
                .map_err(|error| {
                    AssetError::io(
                        "не удалось откатить атомарную замену asset",
                        std::io::Error::from(error),
                    )
                })?;
                sync_directory(&temporary)?;
                sync_directory(&previous_parent)?;
                if !file_matches_hash(&previous_parent, &previous_name, &previous.sha256)? {
                    return Err(AssetError::new(
                        ErrorCode::IntegrityMismatch,
                        "восстановленный asset не совпадает с прежним SHA-256",
                    ));
                }
            } else if !file_matches_hash(&previous_parent, &previous_name, &previous.sha256)? {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    "предыдущий asset отсутствует после незавершённой публикации",
                ));
            }
        }
        if transaction
            .previous
            .as_ref()
            .is_none_or(|previous| previous.storage_path != transaction.next.storage_path)
        {
            remove_storage_path_if_hash(
                root,
                &transaction.next.storage_path,
                &transaction.next.sha256,
                "неподтверждённый asset",
            )?;
        }
        let previous_sha256 = transaction
            .previous
            .as_ref()
            .map(|previous| previous.sha256.clone());
        RecoveryFact::new(
            transaction.identity.clone(),
            previous_sha256.clone(),
            previous_sha256,
            current.map(|asset| asset.lifecycle),
            "незавершённая публикация откатана к прежнему состоянию",
        )
    } else {
        return Err(AssetError::new(
            ErrorCode::IntegrityMismatch,
            "publication marker не соответствует текущему manifest",
        ));
    };

    remove_if_exists(&temporary, OsString::from(transaction.staged_name.as_str()))?;
    if let (Some(backup_name), Some(previous)) = (
        transaction.backup_name.as_deref(),
        transaction.previous.as_ref(),
    ) {
        remove_if_hash(
            &temporary,
            backup_name,
            &previous.sha256,
            "publication backup",
        )?;
    }
    unlink_if_exists(&temporary, marker_name)?;
    if let Some(previous) = &transaction.previous {
        prune_empty_storage_directories(root, &previous.storage_path)?;
    }
    prune_empty_storage_directories(root, &transaction.next.storage_path)?;
    sync_directory(&temporary)?;
    Ok(Some(fact))
}

fn validate_publication_transaction(
    marker_name: &OsString,
    transaction: &PublicationTransaction,
    policy: &dyn AssetDomainPolicy,
    legacy: bool,
) -> Result<(), AssetError> {
    if transaction.schema_version != 1 {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит неизвестную schema",
        ));
    }
    policy.validate_identity(&transaction.identity)?;
    validate_hash(&transaction.next.sha256)?;
    let expected_next = if legacy {
        policy.legacy_location(
            &transaction.identity,
            &transaction.next.sha256,
            transaction.next.format,
        )
    } else {
        Some(policy.canonical_location(
            &transaction.identity,
            &transaction.next.sha256,
            transaction.next.format,
        )?)
    }
    .ok_or_else(|| {
        AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication legacy location отсутствует",
        )
    })?;
    if transaction.next.storage_path != expected_next.storage_path {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит неверный новый storage_path",
        ));
    }
    if let Some(previous) = &transaction.previous {
        validate_hash(&previous.sha256)?;
        let expected_previous = if legacy {
            policy.legacy_location(&transaction.identity, &previous.sha256, previous.format)
        } else {
            Some(policy.canonical_location(
                &transaction.identity,
                &previous.sha256,
                previous.format,
            )?)
        }
        .ok_or_else(|| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                "publication legacy location отсутствует",
            )
        })?;
        if previous.storage_path != expected_previous.storage_path {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "publication marker содержит неверный предыдущий storage_path",
            ));
        }
        if previous.sha256 == transaction.next.sha256
            && previous.storage_path == transaction.next.storage_path
        {
            return Err(AssetError::new(
                ErrorCode::ManifestCorrupt,
                "publication marker не описывает изменение bytes/path",
            ));
        }
    }
    let marker = marker_name.to_string_lossy();
    let Some(id) = marker
        .strip_prefix("publication-")
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "неверное имя publication marker",
        ));
    };
    if !valid_numeric_id(id) || !is_temp_artifact_name(&transaction.staged_name) {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит небезопасное имя temporary file",
        ));
    }
    let expected_backup = transaction
        .previous
        .as_ref()
        .filter(|previous| previous.storage_path == transaction.next.storage_path)
        .map(|_| format!("publication-{id}.backup"));
    if transaction.backup_name != expected_backup {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит неверный backup name",
        ));
    }
    Ok(())
}

fn validate_publication_names(
    marker_name: &OsString,
    staged_name: Option<&str>,
    backup_name: Option<&str>,
) -> Result<(), AssetError> {
    let marker = marker_name.to_string_lossy();
    let Some(id) = marker
        .strip_prefix("publication-")
        .and_then(|name| name.strip_suffix(".json"))
    else {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "неверное имя publication marker",
        ));
    };
    if !valid_numeric_id(id)
        || staged_name.is_some_and(|name| !is_temp_artifact_name(name))
        || backup_name.is_some_and(|name| name != format!("publication-{id}.backup"))
    {
        return Err(AssetError::new(
            ErrorCode::ManifestCorrupt,
            "publication marker содержит небезопасное имя temporary file",
        ));
    }
    Ok(())
}

fn valid_numeric_id(id: &str) -> bool {
    let Some((pid, counter)) = id.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !counter.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && counter.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_temp_artifact_name(name: &str) -> bool {
    ["candidate-", "ingest-"].iter().any(|prefix| {
        name.strip_prefix(prefix)
            .and_then(|suffix| suffix.strip_suffix(".tmp"))
            .is_some_and(valid_numeric_id)
    })
}

fn ensure_asset_path_absent(
    directory: &File,
    name: &str,
    storage_path: &str,
) -> Result<(), AssetError> {
    match open_regular_at(directory, name, ErrorCode::MissingAssetFile) {
        Ok(_) => Err(AssetError::new(
            ErrorCode::UnexpectedPath,
            format!("путь нового asset уже занят: {storage_path}"),
        )),
        Err(error) if error.code == ErrorCode::MissingAssetFile => Ok(()),
        Err(error) => Err(error),
    }
}

fn file_matches_hash(
    directory: &File,
    name: &str,
    expected_hash: &str,
) -> Result<bool, AssetError> {
    match open_regular_at(directory, name, ErrorCode::MissingAssetFile) {
        Ok(file) => Ok(hash_file(file)?.0 == expected_hash),
        Err(error) if error.code == ErrorCode::MissingAssetFile => Ok(false),
        Err(error) => Err(error),
    }
}

fn storage_file_matches_hash(
    root: &File,
    storage_path: &str,
    expected_hash: &str,
) -> Result<bool, AssetError> {
    let (parent, leaf) = match open_storage_parent(root, storage_path, false) {
        Ok(value) => value,
        Err(error) if error.code == ErrorCode::MissingAssetFile => return Ok(false),
        Err(error) => return Err(error),
    };
    file_matches_hash(&parent, &leaf, expected_hash)
}

fn remove_if_hash(
    directory: &File,
    name: &str,
    expected_hash: &str,
    label: &str,
) -> Result<(), AssetError> {
    match open_regular_at(directory, name, ErrorCode::MissingAssetFile) {
        Ok(file) => {
            if hash_file(file)?.0 != expected_hash {
                return Err(AssetError::new(
                    ErrorCode::IntegrityMismatch,
                    format!("{label} не совпадает с transaction SHA-256"),
                ));
            }
            let owned_name = OsString::from(name);
            unlink_if_exists(directory, &owned_name)
        }
        Err(error) if error.code == ErrorCode::MissingAssetFile => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_if_exists(directory: &File, name: OsString) -> Result<(), AssetError> {
    unlink_if_exists(directory, &name)
}

fn unlink_if_exists(directory: &File, name: &OsString) -> Result<(), AssetError> {
    match unlinkat(directory, name, AtFlags::empty()) {
        Ok(()) => Ok(()),
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AssetError::io(
            "не удалось удалить завершённый publication artifact",
            std::io::Error::from(error),
        )),
    }
}

pub(crate) fn open_source_file(path: &Path) -> Result<File, AssetError> {
    let file = open(
        path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| {
        let error = std::io::Error::from(error);
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(
                ErrorCode::SourceMissing,
                format!("explicit source file отсутствует: {}", path.display()),
            )
        } else if is_symlink_error(&error) {
            AssetError::new(
                ErrorCode::SourceNotRegular,
                "explicit source должен быть обычным файлом без symlink",
            )
        } else {
            AssetError::io("не удалось открыть explicit source", error)
        }
    })?;
    let file = File::from(file);
    if !file
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить explicit source", error))?
        .is_file()
    {
        return Err(AssetError::new(
            ErrorCode::SourceNotRegular,
            "explicit source должен быть обычным файлом без symlink",
        ));
    }
    Ok(file)
}

fn open_regular_at(
    directory: &File,
    name: impl rustix::path::Arg,
    missing_code: ErrorCode,
) -> Result<File, AssetError> {
    let file = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| {
        let error = std::io::Error::from(error);
        if error.kind() == std::io::ErrorKind::NotFound {
            AssetError::new(missing_code, "файл отсутствует в program-owned store")
        } else if is_symlink_error(&error) {
            AssetError::new(
                ErrorCode::BoundaryViolation,
                "symlink запрещён внутри asset store",
            )
        } else {
            AssetError::io("не удалось открыть файл store", error)
        }
    })?;
    let file = File::from(file);
    if !file
        .metadata()
        .map_err(|error| AssetError::io("не удалось проверить файл store", error))?
        .is_file()
    {
        return Err(AssetError::new(
            ErrorCode::UnexpectedPath,
            "вместо обычного файла найден другой filesystem object",
        ));
    }
    Ok(file)
}

fn resolve_store_root(root: &Path) -> Result<PathBuf, AssetError> {
    let absolute = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| AssetError::io("не удалось определить cwd", error))?
            .join(root)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(AssetError::new(
                    ErrorCode::InvalidStoreRoot,
                    "store root не должен содержать компонент '..'",
                ));
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

/// Открывает каждый компонент относительно уже открытого родительского каталога,
/// не переходя по символическим ссылкам; отсутствующие компоненты создаёт через
/// тот же дескриптор каталога.
fn open_or_create_store_root(
    path: &Path,
    after_component: impl FnMut(&Path),
) -> Result<(File, bool), AssetError> {
    open_store_root(path, true, after_component)
}

fn open_existing_store_root(path: &Path) -> Result<File, AssetError> {
    open_store_root(path, false, |_| {}).map(|(root, _)| root)
}

fn open_store_root(
    path: &Path,
    create_missing: bool,
    mut after_component: impl FnMut(&Path),
) -> Result<(File, bool), AssetError> {
    let mut current = File::open("/")
        .map_err(|error| AssetError::io("не удалось открыть filesystem root", error))?;
    let parts: Vec<OsString> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect();
    let mut root_created = false;
    let mut expected_parent = PathBuf::from("/");

    for (index, part) in parts.iter().enumerate() {
        if fd_canonical_path(&current)? != expected_parent {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                format!(
                    "компонент store root изменился во время открытия: {}",
                    path.display()
                ),
            ));
        }
        let checked_path = expected_parent.join(part);
        match open_directory_at(&current, part) {
            Ok(directory) => current = directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create_missing {
                    return Err(AssetError::new(
                        ErrorCode::StoreMissing,
                        "store root не существует",
                    ));
                }
                match mkdirat(&current, part, Mode::from_raw_mode(0o755)) {
                    Ok(()) => {
                        if index + 1 == parts.len() {
                            root_created = true;
                        }
                    }
                    Err(create_error)
                        if std::io::Error::from(create_error).kind()
                            == std::io::ErrorKind::AlreadyExists => {}
                    Err(create_error) => {
                        return Err(AssetError::io(
                            "не удалось создать компонент store root",
                            std::io::Error::from(create_error),
                        ));
                    }
                }
                current = open_directory_at(&current, part).map_err(|open_error| {
                    if is_symlink_error(&open_error)
                        || (open_error.kind() == std::io::ErrorKind::NotADirectory
                            && fs::symlink_metadata(&checked_path)
                                .is_ok_and(|metadata| metadata.file_type().is_symlink()))
                    {
                        AssetError::new(
                            ErrorCode::BoundaryViolation,
                            format!("store root проходит через symlink: {}", path.display()),
                        )
                    } else if open_error.kind() == std::io::ErrorKind::NotADirectory {
                        AssetError::new(
                            ErrorCode::InvalidStoreRoot,
                            format!(
                                "компонент store root не является каталогом: {}",
                                path.display()
                            ),
                        )
                    } else {
                        AssetError::io("не удалось открыть компонент store root", open_error)
                    }
                })?;
            }
            Err(error) if is_symlink_error(&error) => {
                return Err(AssetError::new(
                    ErrorCode::BoundaryViolation,
                    format!("store root проходит через symlink: {}", path.display()),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
                if fs::symlink_metadata(&checked_path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(AssetError::new(
                        ErrorCode::BoundaryViolation,
                        format!("store root проходит через symlink: {}", path.display()),
                    ));
                }
                return Err(AssetError::new(
                    ErrorCode::InvalidStoreRoot,
                    format!(
                        "компонент store root не является каталогом: {}",
                        path.display()
                    ),
                ));
            }
            Err(error) => {
                return Err(AssetError::io("не удалось открыть store root", error));
            }
        }
        expected_parent = checked_path;
        after_component(&expected_parent);
    }

    Ok((current, root_created))
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

pub(crate) fn fd_canonical_path(file: &File) -> Result<PathBuf, AssetError> {
    fs::canonicalize(fd_path(file))
        .map_err(|error| AssetError::io("не удалось разрешить открытый filesystem object", error))
}

fn is_symlink_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(Errno::LOOP.raw_os_error())
}

fn resolve_protected_paths(path: &Path) -> Result<Vec<PathBuf>, AssetError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| AssetError::io("не удалось определить cwd", error))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    let mut paths = vec![normalized.clone()];
    match fs::canonicalize(&normalized) {
        Ok(canonical) if canonical != normalized => paths.push(canonical),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(AssetError::io("не удалось разрешить protected root", error));
        }
    }
    Ok(paths)
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn sort_assets(assets: &mut [AssetRecord]) {
    assets.sort_by(|left, right| left.identity.cmp(&right.identity));
}

fn new_store_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let material = format!("{}:{now}:{counter}", std::process::id());
    sha256_hex(material.as_bytes())
}

pub(crate) fn sync_directory(directory: &File) -> Result<(), AssetError> {
    directory
        .sync_all()
        .map_err(|error| AssetError::io("не удалось синхронизировать каталог store", error))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread;

    use super::*;
    use crate::model::{SemanticDecision, ValidationEvidence};

    use crate::temp_workspace::TempWorkspace;

    struct TempDir {
        workspace: TempWorkspace,
    }

    impl TempDir {
        fn new() -> Self {
            Self {
                workspace: TempWorkspace::create("asset-store-store-tests")
                    .expect("создаётся временный корневой каталог теста"),
            }
        }

        fn path(&self) -> &Path {
            self.workspace.path()
        }
    }

    struct VerifiedValidator;

    impl SemanticValidator for VerifiedValidator {
        fn identity(&self) -> ValidatorIdentity {
            ValidatorIdentity {
                id: "unit-test".to_owned(),
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

    #[test]
    fn verified_snapshot_is_read_only_and_rechecks_the_returned_bytes() {
        let temp = TempDir::new();
        let root = temp.path().join("store");
        let store = AssetStore::open_kanji(StoreOptions::new(&root)).unwrap();
        let identity = AssetIdentity::new("kanji", "一").unwrap();
        store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: identity.clone(),
                    bytes: b"GIF89a-synthetic".to_vec(),
                    provenance: Provenance {
                        source_kind: "local_import".into(),
                        source_name: "fixture".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &VerifiedValidator,
            )
            .unwrap();
        drop(store);
        fs::remove_dir_all(root.join(RUNTIME_DIR)).unwrap();
        fs::remove_file(root.join(LOCK_FILE)).unwrap();
        let before = fs::read(root.join(MANIFEST_FILE)).unwrap();
        let read = AssetStore::read_verified_with_policy(
            &root,
            std::slice::from_ref(&identity),
            &VerifiedValidator.identity(),
            &KanjiDomainPolicy,
        )
        .unwrap();
        assert_eq!(read[0].bytes, b"GIF89a-synthetic");
        assert_eq!(fs::read(root.join(MANIFEST_FILE)).unwrap(), before);
        assert!(!root.join(RUNTIME_DIR).exists());
        assert!(!root.join(LOCK_FILE).exists());
        let error = AssetStore::read_verified_snapshot(
            &root,
            std::slice::from_ref(&identity),
            &VerifiedValidator.identity(),
            &KanjiDomainPolicy,
            |record| {
                let path = root.join(&record.storage_path);
                fs::remove_file(&path).unwrap();
                fs::write(&path, b"GIF89a-swapped").unwrap();
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::IntegrityMismatch);
    }

    #[test]
    fn verified_snapshot_rejects_symlink_replacement_after_validation() {
        let temp = TempDir::new();
        let root = temp.path().join("store");
        let store = AssetStore::open_kanji(StoreOptions::new(&root)).unwrap();
        let identity = AssetIdentity::new("kanji", "二").unwrap();
        store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: identity.clone(),
                    bytes: b"GIF89a-original".to_vec(),
                    provenance: Provenance {
                        source_kind: "local_import".into(),
                        source_name: "fixture".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &VerifiedValidator,
            )
            .unwrap();
        let outside = temp.path().join("outside.gif");
        fs::write(&outside, b"GIF89a-original").unwrap();
        let error = AssetStore::read_verified_snapshot(
            &root,
            &[identity],
            &VerifiedValidator.identity(),
            &KanjiDomainPolicy,
            |record| {
                let path = root.join(&record.storage_path);
                fs::remove_file(&path).unwrap();
                std::os::unix::fs::symlink(&outside, path).unwrap();
            },
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::BoundaryViolation);
    }

    #[test]
    fn layout_migration_recovery_rolls_back_or_finishes_by_manifest_commit() {
        let temp = TempDir::new();
        let root = temp.path().join("store");
        let store = AssetStore::open_kanji(StoreOptions::new(&root)).unwrap();
        let identity = AssetIdentity::new("kanji", "元").unwrap();
        let record = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity,
                    bytes: b"GIF89a migration fixture".to_vec(),
                    provenance: Provenance {
                        source_kind: "fixture".into(),
                        source_name: "migration.gif".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &VerifiedValidator,
            )
            .unwrap()
            .asset
            .unwrap();
        drop(store);

        let old_path = root.join("assets/元.gif");
        let new_path = root.join("assets/gif/元.gif");
        fs::rename(&new_path, &old_path).unwrap();
        fs::remove_dir(root.join("assets/gif")).unwrap();
        let manifest_path = root.join(MANIFEST_FILE);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        legacy["schema_version"] = 4.into();
        legacy.as_object_mut().unwrap().remove("domain_id");
        legacy["assets"][0]["storage_path"] = "assets/元.gif".into();
        legacy["assets"][0]
            .as_object_mut()
            .unwrap()
            .remove("consumer_filename");
        fs::write(&manifest_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let transaction = LayoutMigrationTransaction {
            schema_version: 1,
            store_id: legacy["store_id"].as_str().unwrap().to_owned(),
            domain_id: "kanji".into(),
            source_schema_version: 4,
            entries: vec![LayoutMigrationEntry {
                identity: record.identity.clone(),
                sha256: record.sha256.clone(),
                format: record.format,
                old_path: "assets/元.gif".into(),
                new_path: "assets/gif/元.gif".into(),
                consumer_filename: "元.gif".into(),
            }],
        };
        let root_handle = File::open(&root).unwrap();
        let write_marker = || {
            let temporary = open_directory_at(&root_handle, TEMP_DIR).unwrap();
            let bytes = serde_json::to_vec(&transaction).unwrap();
            write_transaction_marker(&temporary, LAYOUT_MIGRATION_MARKER, &bytes).unwrap();
        };

        fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        fs::hard_link(&old_path, &new_path).unwrap();
        write_marker();
        recover_layout_migration(&root_handle, &KanjiDomainPolicy).unwrap();
        assert!(old_path.exists());
        assert!(!new_path.exists());
        assert!(!root.join(TEMP_DIR).join(LAYOUT_MIGRATION_MARKER).exists());

        fs::create_dir_all(new_path.parent().unwrap()).unwrap();
        fs::hard_link(&old_path, &new_path).unwrap();
        let mut committed: Manifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        committed.schema_version = MANIFEST_SCHEMA_VERSION;
        committed.domain_id = "kanji".into();
        committed.revision += 1;
        committed.assets[0].storage_path = "assets/gif/元.gif".into();
        committed.assets[0].consumer_filename = "元.gif".into();
        save_manifest(&root_handle, &committed, false).unwrap();
        write_marker();
        recover_layout_migration(&root_handle, &KanjiDomainPolicy).unwrap();

        assert!(!old_path.exists());
        assert!(new_path.exists());
        assert_eq!(fs::read(&new_path).unwrap(), b"GIF89a migration fixture");
        assert!(!root.join(TEMP_DIR).join(LAYOUT_MIGRATION_MARKER).exists());
        let reopened = AssetStore::open_kanji_existing(StoreOptions::new(&root)).unwrap();
        assert_eq!(
            reopened.verify_integrity().unwrap()[0].sha256,
            record.sha256
        );
    }

    #[test]
    fn failed_manifest_publication_cannot_leave_false_verified_state() {
        let temp = TempDir::new();
        let root = temp.path().join("store");
        let store =
            AssetStore::open(StoreOptions::new(&root)).expect("пустое хранилище открывается");
        let source = temp.path().join("source.bin");
        fs::write(&source, b"publication failure fixture").expect("исходный файл записан");
        store
            .ingest(IngestRequest {
                identity: AssetIdentity::new("generic", "one").unwrap(),
                source_path: source,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
            .expect("объект импортирован");

        let previous_manifest = fs::read(root.join(MANIFEST_FILE)).expect("манифест существует");
        store.fail_next_manifest_write();
        let error = store
            .validate(SelectionMode::Full, &VerifiedValidator)
            .expect_err("смоделированная ошибка публикации возвращена");
        assert_eq!(error.code, ErrorCode::IoFailure);
        drop(store);
        assert_eq!(
            fs::read(root.join(MANIFEST_FILE)).expect("канонический манифест доступен для чтения"),
            previous_manifest,
            "ошибка до переименования сохраняет прежние канонические байты"
        );
        assert!(root.join(TEMP_DIR).join(TRANSITION_MARKER).exists());

        let reopened =
            AssetStore::open(StoreOptions::new(&root)).expect("хранилище доступно для чтения");
        let records = reopened
            .verify_integrity()
            .expect("каноническое состояние остаётся корректным");
        assert!(!root.join(TEMP_DIR).join(TRANSITION_MARKER).exists());
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].lifecycle, LifecycleState::Pending);
        assert!(records[0].validation.is_none());
    }

    #[test]
    fn removal_recovers_after_manifest_commit_before_byte_deletion() {
        let temp = TempDir::new();
        let root = temp.path().join("store");
        let store = AssetStore::open_kanji(StoreOptions::new(&root)).unwrap();
        let record = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", "日").unwrap(),
                    bytes: b"GIF89a durable removal fixture".to_vec(),
                    provenance: Provenance {
                        source_kind: "fixture".into(),
                        source_name: "fixture.gif".into(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &VerifiedValidator,
            )
            .unwrap()
            .asset
            .unwrap();
        let lock = store.lock_exclusive().unwrap();
        let mut manifest = load_manifest(&store.root_handle).unwrap();
        FAIL_AFTER_REMOVAL_MANIFEST.with(|hook| hook.set(true));
        assert_eq!(
            remove_record_from_area(
                &store.root_handle,
                &mut manifest,
                &record,
                store.policy.as_ref(),
            )
            .unwrap_err()
            .code,
            ErrorCode::IoFailure
        );
        assert!(root.join(&record.storage_path).exists());
        assert!(root.join(TEMP_DIR).join(REMOVAL_MARKER).exists());
        lock.unlock().unwrap();
        drop(store);

        let reopened = AssetStore::open_kanji_existing(StoreOptions::new(&root)).unwrap();
        assert!(reopened.verify_integrity().unwrap().is_empty());
        assert!(!root.join(&record.storage_path).exists());
        assert!(!root.join(TEMP_DIR).join(REMOVAL_MARKER).exists());
        assert!(!root.join(TEMP_DIR).join(REMOVAL_BACKUP).exists());
    }

    #[test]
    fn canonical_transition_recovery_keeps_verified_result() {
        for through_validation in [false, true] {
            let temp = TempDir::new();
            let root = temp.path().join("store");
            let store = AssetStore::open_kanji(StoreOptions::new(&root)).unwrap();
            let source = temp.path().join("candidate.gif");
            let bytes = b"GIF89a pending to verified fixture";
            fs::write(&source, bytes).unwrap();
            let identity = AssetIdentity::new("kanji", "日").unwrap();
            store
                .ingest(IngestRequest {
                    identity: identity.clone(),
                    source_path: source,
                    expected_source_sha256: None,
                    domain_metadata: None,
                    replace_expected_sha256: None,
                })
                .unwrap();
            FAIL_AFTER_CANONICAL_TRANSITION.with(|hook| hook.set(true));
            let error = if through_validation {
                store
                    .validate(SelectionMode::Full, &VerifiedValidator)
                    .unwrap_err()
            } else {
                store
                    .ingest_verified(
                        VerifiedIngestRequest {
                            identity,
                            bytes: bytes.to_vec(),
                            provenance: Provenance {
                                source_kind: "fixture".into(),
                                source_name: "candidate.gif".into(),
                            },
                            domain_metadata: None,
                            replace_expected_sha256: None,
                        },
                        &VerifiedValidator,
                    )
                    .unwrap_err()
            };
            assert_eq!(error.code, ErrorCode::IoFailure);
            assert!(root.join(TEMP_DIR).join(TRANSITION_MARKER).exists());
            drop(store);

            let reopened = AssetStore::open_kanji_existing(StoreOptions::new(&root)).unwrap();
            let records = reopened.verify_integrity().unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].lifecycle, LifecycleState::Verified);
            assert!(!root.join(TEMP_DIR).join(TRANSITION_MARKER).exists());
        }
    }

    #[test]
    fn failed_kanji_cas_manifest_write_restores_previous_stable_bytes() {
        let temp = TempDir::new();
        let root = temp.path().join("store");
        let store =
            AssetStore::open_kanji(StoreOptions::new(&root)).expect("пустое хранилище открывается");
        let validator = VerifiedValidator;
        let first_bytes = b"GIF89a previous fixture".to_vec();
        let first = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", "元").unwrap(),
                    bytes: first_bytes.clone(),
                    provenance: Provenance {
                        source_kind: "fixture".to_owned(),
                        source_name: "previous.gif".to_owned(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &validator,
            )
            .expect("первые проверенные байты опубликованы");
        let previous = first.asset.expect("первый ресурс существует");
        assert_eq!(previous.storage_path, "assets/gif/元.gif");

        store.fail_next_manifest_write();
        let error = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: previous.identity.clone(),
                    bytes: b"GIF89a replacement fixture".to_vec(),
                    provenance: Provenance {
                        source_kind: "fixture".to_owned(),
                        source_name: "replacement.gif".to_owned(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: Some(previous.sha256.clone()),
                },
                &validator,
            )
            .expect_err("смоделированный отказ записи манифеста прерывает публикацию по CAS");
        assert_eq!(error.code, ErrorCode::IoFailure);
        assert_eq!(
            fs::read(root.join("assets/gif/元.gif")).expect("прежний стабильный путь восстановлен"),
            first_bytes
        );
        assert_eq!(
            fs::read_dir(root.join(TEMP_DIR))
                .expect("временный каталог существует")
                .count(),
            0,
            "откат удаляет маркер транзакции, резервную копию и подготовленный объект"
        );

        let reopened = AssetStore::open_kanji(StoreOptions::new(&root))
            .expect("хранилище доступно для чтения");
        let records = reopened
            .verify_integrity()
            .expect("старый манифест по-прежнему соответствует байтам");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sha256, previous.sha256);
        assert_eq!(records[0].lifecycle, LifecycleState::Verified);
    }

    #[test]
    fn reopening_after_interrupted_kanji_cas_restores_previous_verified_bytes() {
        let temp = TempDir::new();
        let root = temp.path().join("store");
        let store =
            AssetStore::open_kanji(StoreOptions::new(&root)).expect("пустое хранилище открывается");
        let previous_bytes = b"GIF89a durable previous fixture".to_vec();
        let first = store
            .ingest_verified(
                VerifiedIngestRequest {
                    identity: AssetIdentity::new("kanji", "元").unwrap(),
                    bytes: previous_bytes.clone(),
                    provenance: Provenance {
                        source_kind: "fixture".to_owned(),
                        source_name: "previous.gif".to_owned(),
                    },
                    domain_metadata: None,
                    replace_expected_sha256: None,
                },
                &VerifiedValidator,
            )
            .expect("первые проверенные байты опубликованы");
        let previous = first.asset.expect("первый ресурс существует");

        let lock = store.lock_exclusive().expect("блокировка CAS получена");
        let replacement_bytes = b"GIF89a interrupted replacement";
        let staged =
            stage_bytes(&store.root_handle, replacement_bytes).expect("новые байты подготовлены");
        let mut next = previous.clone();
        next.sha256 = staged.sha256.clone();
        next.byte_length = staged.byte_length;
        next.format = staged.format;
        let location = store
            .policy
            .canonical_location(&next.identity, &next.sha256, next.format)
            .unwrap();
        next.storage_path = location.storage_path;
        next.consumer_filename = location.consumer_filename;
        next.lifecycle = LifecycleState::Pending;
        next.validation = None;

        let publication = prepare_publication(
            &store.root_handle,
            &staged,
            Some(&previous),
            &next,
            store.policy.as_ref(),
        )
        .expect("транзакция CAS для того же пути сохранена до публикации");
        let marker_name = publication.marker_name.clone();
        let backup_name = publication
            .transaction
            .backup_name
            .clone()
            .expect("замена того же пути сохраняет резервную копию прежних байтов");
        apply_publication(&store.root_handle, &staged, &publication)
            .expect("новые байты атомарно заменяют файл по стабильному пути");
        assert_eq!(
            fs::read(root.join("assets/gif/元.gif")).expect("замена опубликована"),
            replacement_bytes
        );
        assert!(root.join(TEMP_DIR).join(&marker_name).exists());
        assert!(root.join(TEMP_DIR).join(&backup_name).exists());

        drop(staged);
        lock.unlock().expect("тест освобождает блокировку CAS");
        drop(store);

        let reopened = AssetStore::open_kanji(StoreOptions::new(&root))
            .expect("открытие восстанавливает прерванную замену на месте до проверки");
        let records = reopened
            .verify_integrity()
            .expect("прежний проверенный манифест соответствует байтам");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].sha256, previous.sha256);
        assert_eq!(records[0].lifecycle, LifecycleState::Verified);
        assert_eq!(
            fs::read(root.join("assets/gif/元.gif"))
                .expect("прежние стабильные байты восстановлены"),
            previous_bytes
        );
        assert_eq!(
            fs::read_dir(root.join(TEMP_DIR))
                .expect("временный каталог существует")
                .count(),
            0,
            "восстановление удаляет маркер, резервную копию и подготовленный объект"
        );
    }

    fn run_serialization_probe(contested_identity: bool) {
        let temp = TempDir::new();
        let root = temp.path().join("store");
        let store =
            Arc::new(AssetStore::open(StoreOptions::new(&root)).expect("хранилище открывается"));
        let before_count = Arc::new(AtomicUsize::new(0));
        let after_count = Arc::new(AtomicUsize::new(0));
        let (first_locked_tx, first_locked_rx) = mpsc::sync_channel(1);
        let (release_first_tx, release_first_rx) = mpsc::sync_channel(1);
        let (second_probe_tx, second_probe_rx) = mpsc::sync_channel(1);
        let (second_locked_tx, second_locked_rx) = mpsc::sync_channel(1);
        let release_first_rx = std::sync::Mutex::new(release_first_rx);

        let before_count_hook = Arc::clone(&before_count);
        let before_lock = Arc::new(move |lock: &File| {
            if before_count_hook.fetch_add(1, Ordering::SeqCst) == 1 {
                let probe = flock(lock, FlockOperation::NonBlockingLockExclusive);
                let blocked = probe
                    .as_ref()
                    .is_err_and(|error| *error == Errno::WOULDBLOCK);
                if probe.is_ok() {
                    flock(lock, FlockOperation::Unlock)
                        .expect("блокировка после проверки освобождена");
                }
                second_probe_tx
                    .send(blocked)
                    .expect("основной тест получил сигнал второй записи");
            }
        });

        let after_count_hook = Arc::clone(&after_count);
        let after_lock = Arc::new(move |_lock: &File| {
            if after_count_hook.fetch_add(1, Ordering::SeqCst) == 0 {
                first_locked_tx
                    .send(())
                    .expect("основной тест проверил, что первая запись удерживает блокировку");
                release_first_rx
                    .lock()
                    .expect("мьютекс получателя сигнала освобождения исправен")
                    .recv()
                    .expect("основной тест разблокировал первую запись");
            } else {
                second_locked_tx.send(()).expect(
                    "основной тест получил подтверждение захвата блокировки второй записью",
                );
            }
        });
        *store
            .lock_test_hooks
            .lock()
            .expect("мьютекс тестового перехватчика исправен") = Some(Arc::new(LockTestHooks {
            before_lock,
            after_lock,
        }));

        let first_source = temp.path().join("first.bin");
        let second_source = temp.path().join("second.bin");
        fs::write(&first_source, b"first writer bytes").expect("первый исходный файл записан");
        fs::write(&second_source, b"second writer bytes").expect("второй исходный файл записан");
        let first_store = Arc::clone(&store);
        let first = thread::spawn(move || {
            first_store.ingest(IngestRequest {
                identity: AssetIdentity::new(
                    "generic",
                    if contested_identity { "same" } else { "first" },
                )
                .unwrap(),
                source_path: first_source,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
        });
        first_locked_rx
            .recv()
            .expect("первая запись вошла в секцию с эксклюзивной блокировкой");

        let second_store = Arc::clone(&store);
        let second = thread::spawn(move || {
            second_store.ingest(IngestRequest {
                identity: AssetIdentity::new(
                    "generic",
                    if contested_identity { "same" } else { "second" },
                )
                .unwrap(),
                source_path: second_source,
                expected_source_sha256: None,
                domain_metadata: None,
                replace_expected_sha256: None,
            })
        });
        let second_was_blocked = second_probe_rx
            .recv()
            .expect("вторая запись проверяет блокировку, пока первая её удерживает");
        release_first_tx
            .send(())
            .expect("сигнал первой записи отправлен");
        let first_result = first.join().expect("поток первой записи завершился");
        let second_result = second.join().expect("поток второй записи завершился");
        second_locked_rx
            .recv()
            .expect("вторая запись получила блокировку после её освобождения");

        assert!(
            second_was_blocked,
            "вторая запись должна увидеть, что первая удерживает эксклюзивную блокировку"
        );
        assert!(first_result.is_ok());
        if contested_identity {
            assert_eq!(second_result.unwrap_err().code, ErrorCode::IdentityConflict);
            assert_eq!(store.verify_integrity().unwrap().len(), 1);
        } else {
            second_result.expect("запись с другой идентичностью завершается успешно");
            assert_eq!(store.verify_integrity().unwrap().len(), 2);
        }
    }

    #[test]
    fn concurrent_writers_are_serialized_inside_the_mutation_lock() {
        run_serialization_probe(false);
    }

    #[test]
    fn same_identity_writers_have_one_winner_inside_the_mutation_lock() {
        run_serialization_probe(true);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn store_root_component_replacement_is_detected_before_nested_creation() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new();
        let parent = temp.path().join("requested-parent");
        let moved_parent = temp.path().join("moved-parent");
        let outside = temp.path().join("outside");
        fs::create_dir(&parent).expect("запрошенный родительский каталог существует");
        fs::create_dir(&outside).expect("каталог — цель символической ссылки — существует");
        let requested_root = parent.join("store");
        let mut replaced = false;

        let error = open_or_create_store_root(&requested_root, |opened_component| {
            if !replaced && opened_component == parent {
                fs::rename(&parent, &moved_parent)
                    .expect("открытый родительский каталог перемещён");
                symlink(&outside, &parent).expect("исходный путь заменён символической ссылкой");
                replaced = true;
            }
        })
        .expect_err(
            "изменённый дескриптор родительского каталога отклонён до создания дочернего каталога",
        );

        assert!(replaced, "тест заменил компонент после его открытия");
        assert_eq!(error.code, ErrorCode::BoundaryViolation);
        assert!(
            !requested_root.exists(),
            "состояние хранилища не создаётся через новую символическую ссылку"
        );
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "внешний целевой каталог не изменён"
        );
        assert!(
            fs::read_dir(&moved_parent).unwrap().next().is_none(),
            "закреплённый исходный каталог не изменён"
        );
    }
}

#[cfg(test)]
#[path = "trust_tests.rs"]
mod trust_tests;

#[cfg(test)]
#[path = "journal_tests.rs"]
mod journal_tests;
