//! Долговременный локальный журнал операций с ресурсами.
//!
//! Журнал живёт в `<store-root>/.journal/events.jsonl` рядом с `manifest.json`, но
//! вне `.runtime/batches/**`: он переживает удаление локального состояния одного
//! пакета. Каждая строка — один JSON-объект; файл только дополняется.
//!
//! Журнал — история переходов, а не второй манифест. Источником истины о текущем
//! `VERIFIED`-корпусе остаётся `manifest.json`; журнал лишь объясняет, как это
//! состояние получилось, и позволяет обнаружить изменение, зафиксированное в
//! манифесте, но не закрытое событием результата.
//!
//! Запись выполняется только под уже удерживаемой эксклюзивной блокировкой store:
//! ни один worker пакета не обращается к журналу, поэтому многоворкерное получение
//! не замедляется.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{Mode, OFlags, openat};
use serde::{Deserialize, Serialize};

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::model::{AssetIdentity, LifecycleState, Manifest, Provenance};
use crate::store::{directory_entry_error, ensure_dir_entry, open_directory_at, sync_directory};

/// Каталог журнала в корне хранилища.
pub const JOURNAL_DIR: &str = ".journal";
/// Единственный файл журнала: `NDJSON`, только дополнение.
pub const JOURNAL_FILE: &str = "events.jsonl";
/// Версия формата события.
pub const JOURNAL_SCHEMA_VERSION: u32 = 1;
/// Идентичность события уровня всего хранилища, а не отдельного ресурса.
pub const STORE_WIDE_IDENTITY: &str = "*";

/// Верхняя граница читаемого журнала: защита от неограниченного чтения.
///
/// Предел ограничивает только полное чтение событий. Дописыванию нужны лишь
/// ключи уже записанных событий и номер последнего, поэтому журнал больше
/// предела продолжает принимать события, а команды, читающие историю целиком,
/// сообщают `journal_too_large`.
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
/// Верхняя граница сохраняемой технической причины.
const MAX_REASON_CHARS: usize = 400;
/// Разделитель компонентов ключа идемпотентности: управляющий символ не встречается
/// ни в одном доменном значении.
const FIELD_SEPARATOR: char = '\u{1f}';

#[cfg(test)]
thread_local! {
    /// Пониженный предел чтения для проверки поведения на большом журнале.
    static MAX_JOURNAL_BYTES_OVERRIDE: std::cell::Cell<Option<u64>> =
        const { std::cell::Cell::new(None) };
}

/// Предел полного чтения журнала.
fn max_journal_bytes() -> u64 {
    #[cfg(test)]
    if let Some(limit) = MAX_JOURNAL_BYTES_OVERRIDE.with(std::cell::Cell::get) {
        return limit;
    }
    MAX_JOURNAL_BYTES
}

/// Понижает предел чтения журнала на время теста.
#[cfg(test)]
pub(crate) fn set_max_journal_bytes_for_test(limit: Option<u64>) {
    MAX_JOURNAL_BYTES_OVERRIDE.with(|cell| cell.set(limit));
}

static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Операция, о которой сообщает событие журнала.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOperation {
    CandidateWritten,
    VerifiedPublished,
    VerifiedReused,
    TrustGranted,
    TrustRevoked,
    Revalidated,
    Quarantined,
    Removed,
    Recovered,
    LayoutMigrated,
    Baseline,
}

impl JournalOperation {
    /// Стабильное имя операции в формате `snake_case`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CandidateWritten => "candidate_written",
            Self::VerifiedPublished => "verified_published",
            Self::VerifiedReused => "verified_reused",
            Self::TrustGranted => "trust_granted",
            Self::TrustRevoked => "trust_revoked",
            Self::Revalidated => "revalidated",
            Self::Quarantined => "quarantined",
            Self::Removed => "removed",
            Self::Recovered => "recovered",
            Self::LayoutMigrated => "layout_migrated",
            Self::Baseline => "baseline",
        }
    }

    /// Разбирает имя операции для фильтра `journal list --operation`.
    pub fn parse(value: &str) -> Option<Self> {
        const ALL: [JournalOperation; 11] = [
            JournalOperation::CandidateWritten,
            JournalOperation::VerifiedPublished,
            JournalOperation::VerifiedReused,
            JournalOperation::TrustGranted,
            JournalOperation::TrustRevoked,
            JournalOperation::Revalidated,
            JournalOperation::Quarantined,
            JournalOperation::Removed,
            JournalOperation::Recovered,
            JournalOperation::LayoutMigrated,
            JournalOperation::Baseline,
        ];
        ALL.into_iter()
            .find(|operation| operation.as_str() == value)
    }
}

impl std::fmt::Display for JournalOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Итог операции, зафиксированный событием.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalOutcome {
    /// Намерение записано до изменения состояния.
    Pending,
    /// Изменение состояния зафиксировано.
    Completed,
    /// Операция отказала до фиксации; состояние не изменилось.
    Refused,
    /// Незакрытое намерение сверено с фактическим состоянием: изменение состоялось.
    Recovered,
    /// Незакрытое намерение сверено с фактическим состоянием: изменения не было.
    Aborted,
    /// Незакрытое намерение не совпало ни с ожидаемым, ни с прежним состоянием.
    Unknown,
}

impl JournalOutcome {
    /// Стабильное имя итога в формате `snake_case`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Refused => "refused",
            Self::Recovered => "recovered",
            Self::Aborted => "aborted",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for JournalOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Снимок фактически проверенного состояния хранилища на момент `baseline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalSnapshot {
    pub assets: u64,
    pub verified: u64,
    pub pending: u64,
    pub quarantined: u64,
    pub manifest_revision: u64,
}

/// Одно событие журнала: одна строка `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEvent {
    pub schema_version: u32,
    pub seq: u64,
    pub event_id: String,
    pub event_key: String,
    pub recorded_at: String,
    pub domain: String,
    pub namespace: String,
    pub identity: String,
    pub operation: JournalOperation,
    pub outcome: JournalOutcome,
    pub intent_id: Option<String>,
    pub manifest_revision: Option<u64>,
    pub previous_sha256: Option<String>,
    pub new_sha256: Option<String>,
    pub previous_lifecycle: Option<String>,
    pub new_lifecycle: Option<String>,
    /// Состояние записи после события: `true` — `verified`, `false` — не
    /// `verified` или записи нет, `null` — событие не относится к одной записи.
    pub verified: Option<bool>,
    pub run_id: Option<String>,
    pub batch_id: Option<String>,
    pub provenance: Option<String>,
    pub reason: Option<String>,
    pub snapshot: Option<JournalSnapshot>,
}

impl JournalEvent {
    /// Истина, если событие закрывает интент и потому само интентом не является.
    pub const fn closes_intent(&self) -> bool {
        self.intent_id.is_some()
    }

    /// Истина, если это незакрытое намерение.
    pub const fn is_pending_intent(&self) -> bool {
        matches!(self.outcome, JournalOutcome::Pending)
    }
}

/// Незакрытый интент, перечисляемый проверкой журнала.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct JournalOpenIntent {
    pub event_id: String,
    pub seq: u64,
    pub identity: String,
    pub namespace: String,
    pub operation: JournalOperation,
    pub recorded_at: String,
}

/// Готовое к записи событие без вычисляемых полей.
#[derive(Debug, Clone)]
pub struct JournalDraft {
    domain: String,
    namespace: String,
    identity: String,
    operation: JournalOperation,
    outcome: JournalOutcome,
    intent_id: Option<String>,
    manifest_revision: Option<u64>,
    previous_sha256: Option<String>,
    new_sha256: Option<String>,
    previous_lifecycle: Option<String>,
    new_lifecycle: Option<String>,
    verified: Option<bool>,
    run_id: Option<String>,
    batch_id: Option<String>,
    provenance: Option<String>,
    reason: Option<String>,
    snapshot: Option<JournalSnapshot>,
}

impl JournalDraft {
    /// Создаёт событие для одного ресурса.
    pub fn new(
        domain: &str,
        identity: &AssetIdentity,
        operation: JournalOperation,
        outcome: JournalOutcome,
    ) -> Self {
        Self::store_wide(
            domain,
            &identity.namespace,
            &identity.key,
            operation,
            outcome,
        )
    }

    /// Создаёт событие уровня всего хранилища, не привязанное к одному ресурсу.
    pub fn store_wide(
        domain: &str,
        namespace: &str,
        identity: &str,
        operation: JournalOperation,
        outcome: JournalOutcome,
    ) -> Self {
        Self {
            domain: domain.to_owned(),
            namespace: namespace.to_owned(),
            identity: identity.to_owned(),
            operation,
            outcome,
            intent_id: None,
            manifest_revision: None,
            previous_sha256: None,
            new_sha256: None,
            previous_lifecycle: None,
            new_lifecycle: None,
            verified: None,
            run_id: None,
            batch_id: None,
            provenance: None,
            reason: None,
            snapshot: None,
        }
    }

    /// Указывает интент, который закрывает это событие.
    #[must_use]
    pub fn closing(mut self, intent_id: &str) -> Self {
        self.intent_id = Some(intent_id.to_owned());
        self
    }

    /// Указывает ревизию манифеста, к которой относится событие.
    #[must_use]
    pub fn revision(mut self, revision: Option<u64>) -> Self {
        self.manifest_revision = revision;
        self
    }

    /// Указывает прежний и новый `SHA-256`.
    #[must_use]
    pub fn sha(mut self, previous: Option<&str>, next: Option<&str>) -> Self {
        self.previous_sha256 = previous.map(str::to_owned);
        self.new_sha256 = next.map(str::to_owned);
        self
    }

    /// Указывает прежнее и новое состояние жизненного цикла.
    #[must_use]
    pub fn lifecycle(
        mut self,
        previous: Option<LifecycleState>,
        next: Option<LifecycleState>,
    ) -> Self {
        self.previous_lifecycle = previous.map(|state| state.as_str().to_owned());
        self.new_lifecycle = next.map(|state| state.as_str().to_owned());
        self
    }

    /// Указывает, получила ли запись реально состояние `verified`.
    #[must_use]
    pub fn verified(mut self, verified: Option<bool>) -> Self {
        self.verified = verified;
        self
    }

    /// Указывает сведения об источнике в форме `source_kind:source_name`.
    #[must_use]
    pub fn provenance(mut self, provenance: Option<&Provenance>) -> Self {
        self.provenance = provenance
            .map(|provenance| format!("{}:{}", provenance.source_kind, provenance.source_name));
        self
    }

    /// Указывает сведения об источнике уже готовой строкой `source_kind:source_name`.
    ///
    /// Закрывающее событие переносит источник из своего интента, чтобы у обоих
    /// событий одного перехода он совпадал.
    #[must_use]
    pub fn provenance_text(mut self, provenance: Option<&str>) -> Self {
        self.provenance = provenance.map(str::to_owned);
        self
    }

    /// Указывает краткую безопасную техническую причину.
    #[must_use]
    pub fn reason(mut self, reason: &str) -> Self {
        self.reason = Some(sanitize_reason(reason));
        self
    }

    /// Прикладывает снимок состояния для события `baseline`.
    #[must_use]
    pub fn snapshot(mut self, snapshot: JournalSnapshot) -> Self {
        self.snapshot = Some(snapshot);
        self
    }

    /// Указывает запуск, которому принадлежит операция.
    #[must_use]
    pub fn run_id(mut self, run_id: Option<String>) -> Self {
        self.run_id = run_id;
        self
    }

    /// Указывает пакет, которому принадлежит операция.
    #[must_use]
    pub fn batch_id(mut self, batch_id: Option<String>) -> Self {
        self.batch_id = batch_id;
        self
    }

    /// Ключ идемпотентности события.
    ///
    /// В материал входят `outcome` и `intent_id`: интент и закрывающее его
    /// событие результата описывают один и тот же переход одной и той же записи,
    /// и при неизменном состоянии (отказ до фиксации) их ключи иначе совпали бы,
    /// а закрывающее событие было бы отброшено как дубликат интента. `intent_id`
    /// разделяет повторные попытки одного перехода: после отказа следующая
    /// попытка — это новый интент со своим результатом, а не повтор прежнего.
    pub fn event_key(&self) -> String {
        event_key_for_fields(EventKeyFields {
            domain: &self.domain,
            namespace: &self.namespace,
            identity: &self.identity,
            operation: self.operation,
            outcome: self.outcome,
            previous_sha256: self.previous_sha256.as_deref(),
            new_sha256: self.new_sha256.as_deref(),
            manifest_revision: self.manifest_revision,
            intent_id: self.intent_id.as_deref(),
        })
    }
}

struct EventKeyFields<'a> {
    domain: &'a str,
    namespace: &'a str,
    identity: &'a str,
    operation: JournalOperation,
    outcome: JournalOutcome,
    previous_sha256: Option<&'a str>,
    new_sha256: Option<&'a str>,
    manifest_revision: Option<u64>,
    intent_id: Option<&'a str>,
}

fn event_key_for_fields(fields: EventKeyFields<'_>) -> String {
    let material = format!(
        "{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}",
        fields.domain,
        fields.namespace,
        fields.identity,
        fields.operation.as_str(),
        fields.outcome.as_str(),
        fields.previous_sha256.unwrap_or("-"),
        fields.new_sha256.unwrap_or("-"),
        fields
            .manifest_revision
            .map_or_else(|| "-".to_owned(), |revision| revision.to_string()),
        fields.intent_id.unwrap_or("-"),
        sep = FIELD_SEPARATOR,
    );
    sha256_hex(material.as_bytes())
}

fn expected_event_key(event: &JournalEvent) -> String {
    event_key_for_fields(EventKeyFields {
        domain: &event.domain,
        namespace: &event.namespace,
        identity: &event.identity,
        operation: event.operation,
        outcome: event.outcome,
        previous_sha256: event.previous_sha256.as_deref(),
        new_sha256: event.new_sha256.as_deref(),
        manifest_revision: event.manifest_revision,
        intent_id: event.intent_id.as_deref(),
    })
}

/// Проверяет ключи схемы 1, созданные до включения `intent_id` в хеш.
/// Версия схемы тогда не менялась, поэтому старые корректные журналы могут
/// содержать как прежние, так и текущие ключи.
fn legacy_event_key(event: &JournalEvent) -> String {
    let material = format!(
        "{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}{sep}{}",
        event.domain,
        event.namespace,
        event.identity,
        event.operation.as_str(),
        event.outcome.as_str(),
        event.previous_sha256.as_deref().unwrap_or("-"),
        event.new_sha256.as_deref().unwrap_or("-"),
        event
            .manifest_revision
            .map_or_else(|| "-".to_owned(), |revision| revision.to_string()),
        sep = FIELD_SEPARATOR,
    );
    sha256_hex(material.as_bytes())
}

/// Разобранный журнал и диагностика чтения.
#[derive(Debug, Clone, Default)]
pub struct JournalRead {
    pub events: Vec<JournalEvent>,
    pub torn_tail: bool,
    pub corrupt_lines: Vec<u64>,
}

/// Результат проверки целостности журнала.
#[derive(Debug, Clone, Serialize)]
pub struct JournalCheck {
    pub event_count: u64,
    pub torn_tail: bool,
    pub corrupt_lines: Vec<u64>,
    pub open_intents: Vec<JournalOpenIntent>,
    pub problems: Vec<String>,
}

impl JournalCheck {
    /// Истина, если проверка не нашла ни одной проблемы.
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

#[derive(Debug)]
struct JournalState {
    events: Vec<JournalEvent>,
    keys: BTreeSet<String>,
    torn_tail: bool,
    corrupt_lines: Vec<u64>,
    last_seq: u64,
    file_size: u64,
    file_modified: Option<SystemTime>,
    /// Журнал больше предела полного чтения: события не удержаны в памяти, но
    /// ключи, номер последнего события и оборванный хвост посчитаны.
    events_omitted: bool,
}

/// Открытый журнал одного хранилища.
///
/// Дескриптор каталога и разобранное состояние кешируются в памяти: файл
/// перечитывается только тогда, когда его размер или время изменения отличаются
/// от последнего чтения. Это не мешает другому процессу: он пишет под той же
/// эксклюзивной блокировкой store.
#[derive(Debug)]
pub struct Journal {
    root: File,
    directory: Option<File>,
    state: Option<JournalState>,
    run_id: Option<String>,
    batch_id: Option<String>,
}

impl Journal {
    /// Открывает журнал, не создавая каталог и не читая файл.
    pub fn open(root: &File) -> Result<Self, AssetError> {
        let root = root.try_clone().map_err(|error| {
            AssetError::io("не удалось клонировать дескриптор store root", error)
        })?;
        Ok(Self {
            root,
            directory: None,
            state: None,
            run_id: None,
            batch_id: None,
        })
    }

    /// Запоминает запуск и пакет, к которым относятся последующие события.
    pub fn set_context(&mut self, run_id: Option<String>, batch_id: Option<String>) {
        self.run_id = run_id;
        self.batch_id = batch_id;
    }

    /// Разбирает журнал и возвращает события вместе с диагностикой чтения.
    pub fn read(&mut self) -> Result<JournalRead, AssetError> {
        self.refresh()?;
        match &self.state {
            Some(state) if state.events_omitted => Err(journal_too_large_error(state.file_size)),
            Some(state) => Ok(JournalRead {
                events: state.events.clone(),
                torn_tail: state.torn_tail,
                corrupt_lines: state.corrupt_lines.clone(),
            }),
            None => Ok(JournalRead::default()),
        }
    }

    /// Дописывает событие; `Ok(false)` — событие с таким `event_key` уже записано.
    pub fn append(&mut self, draft: &JournalDraft) -> Result<bool, AssetError> {
        Ok(self.append_inner(draft)?.1)
    }

    /// Дописывает событие и возвращает его; для дубликата возвращается уже
    /// записанное событие, чтобы вызывающий код мог сослаться на его `event_id`.
    pub fn append_and_return(&mut self, draft: &JournalDraft) -> Result<JournalEvent, AssetError> {
        match self.append_inner(draft)? {
            (Some(event), _) => Ok(event),
            // Сюда попадает только дубликат интента в журнале больше предела
            // чтения: интенты дубликатами не считаются, поэтому их `event_id`
            // всегда доступен. Состояние диагностируется явно, а не выдуманным
            // событием.
            (None, _) => Err(AssetError::new(
                ErrorCode::JournalTooLarge,
                "событие уже записано, но журнал превышает предел чтения: его event_id недоступен",
            )),
        }
    }

    fn append_inner(
        &mut self,
        draft: &JournalDraft,
    ) -> Result<(Option<JournalEvent>, bool), AssetError> {
        self.refresh()?;
        let event_key = draft.event_key();
        // Интенты не считаются дубликатами: повторная попытка того же перехода —
        // это новое событие, и закрывать своим результатом чужой интент нельзя.
        if draft.outcome != JournalOutcome::Pending
            && self
                .state
                .as_ref()
                .is_some_and(|state| state.keys.contains(&event_key))
        {
            // `state.keys` уже хранит ключи всех событий: линейный поиск по списку
            // нужен только тогда, когда дубликат действительно есть. В журнале
            // больше предела чтения события не удержаны, и повтор просто
            // пропускается: он уже записан.
            let existing = self.state.as_ref().and_then(|state| {
                state
                    .events
                    .iter()
                    .find(|event| event.event_key == event_key)
                    .cloned()
            });
            return Ok((existing, false));
        }
        let last_seq = self.state.as_ref().map_or(0, |state| state.last_seq);
        let seq = last_seq.checked_add(1).ok_or_else(|| {
            AssetError::new(ErrorCode::ManifestCorrupt, "счётчик seq журнала переполнен")
        })?;
        let torn_tail = self.state.as_ref().is_some_and(|state| state.torn_tail);
        let event = self.build_event(draft, seq, event_key)?;
        let mut line = Vec::new();
        if torn_tail {
            // Оборванный хвост отделяется, чтобы не склеиться с новой строкой.
            line.push(b'\n');
        }
        serde_json::to_writer(&mut line, &event).map_err(|error| {
            AssetError::new(
                ErrorCode::ManifestCorrupt,
                format!("не удалось сериализовать событие журнала: {error}"),
            )
        })?;
        line.push(b'\n');

        let directory = self
            .ensure_directory(true)?
            .ok_or_else(|| AssetError::new(ErrorCode::IoFailure, "каталог журнала недоступен"))?;
        let (fd, created) = match openat(
            &directory,
            JOURNAL_FILE,
            OFlags::WRONLY
                | OFlags::CREATE
                | OFlags::EXCL
                | OFlags::APPEND
                | OFlags::CLOEXEC
                | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => (fd, true),
            Err(error) if error == rustix::io::Errno::EXIST => (
                openat(
                    &directory,
                    JOURNAL_FILE,
                    OFlags::WRONLY | OFlags::APPEND | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map_err(|error| {
                    AssetError::io(
                        "не удалось открыть журнал на дополнение",
                        std::io::Error::from(error),
                    )
                })?,
                false,
            ),
            Err(error) => {
                return Err(AssetError::io(
                    "не удалось создать журнал на дополнение",
                    std::io::Error::from(error),
                ));
            }
        };
        let mut file = File::from(fd);
        if !file
            .metadata()
            .map_err(|error| AssetError::io("не удалось проверить файл журнала", error))?
            .is_file()
        {
            return Err(AssetError::new(
                ErrorCode::BoundaryViolation,
                "журнал должен быть обычным файлом без symlink",
            ));
        }
        file.write_all(&line)
            .map_err(|error| AssetError::io("не удалось записать событие журнала", error))?;
        sync_event_append(&file, &directory, created, File::sync_data, sync_directory)?;
        let metadata = file
            .metadata()
            .map_err(|error| AssetError::io("не удалось проверить журнал после записи", error))?;
        let file_size = metadata.len();
        let file_modified = metadata.modified().ok();

        if torn_tail {
            // Разобранное состояние больше не соответствует файлу: строка обрыва
            // стала отдельной и должна быть перечитана как повреждённая.
            self.state = None;
        } else if let Some(state) = self.state.as_mut() {
            state.keys.insert(event.event_key.clone());
            state.last_seq = event.seq;
            state.torn_tail = false;
            state.file_size = file_size;
            state.file_modified = file_modified;
            if !state.events_omitted {
                state.events.push(event.clone());
            }
        }
        Ok((Some(event), true))
    }

    fn build_event(
        &self,
        draft: &JournalDraft,
        seq: u64,
        event_key: String,
    ) -> Result<JournalEvent, AssetError> {
        let recorded_at = format_rfc3339(SystemTime::now());
        let counter = EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        let material = format!(
            "{seq}{sep}{event_key}{sep}{recorded_at}{sep}{}{sep}{counter}",
            std::process::id(),
            sep = FIELD_SEPARATOR,
        );
        Ok(JournalEvent {
            schema_version: JOURNAL_SCHEMA_VERSION,
            seq,
            event_id: sha256_hex(material.as_bytes()),
            event_key,
            recorded_at,
            domain: draft.domain.clone(),
            namespace: draft.namespace.clone(),
            identity: draft.identity.clone(),
            operation: draft.operation,
            outcome: draft.outcome,
            intent_id: draft.intent_id.clone(),
            manifest_revision: draft.manifest_revision,
            previous_sha256: draft.previous_sha256.clone(),
            new_sha256: draft.new_sha256.clone(),
            previous_lifecycle: draft.previous_lifecycle.clone(),
            new_lifecycle: draft.new_lifecycle.clone(),
            verified: draft.verified,
            run_id: draft.run_id.clone().or_else(|| self.run_id.clone()),
            batch_id: draft.batch_id.clone().or_else(|| self.batch_id.clone()),
            provenance: draft.provenance.clone(),
            reason: draft.reason.clone(),
            snapshot: draft.snapshot.clone(),
        })
    }

    fn ensure_directory(&mut self, create: bool) -> Result<Option<File>, AssetError> {
        if let Some(directory) = &self.directory {
            let clone = directory.try_clone().map_err(|error| {
                AssetError::io("не удалось клонировать дескриптор журнала", error)
            })?;
            return Ok(Some(clone));
        }
        match open_directory_at(&self.root, JOURNAL_DIR) {
            Ok(directory) => {
                let clone = directory.try_clone().map_err(|error| {
                    AssetError::io("не удалось клонировать дескриптор журнала", error)
                })?;
                self.directory = Some(clone);
                Ok(Some(directory))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(None);
                }
                let directory = ensure_dir_entry(&self.root, JOURNAL_DIR)?;
                sync_directory(&self.root)?;
                let clone = directory.try_clone().map_err(|error| {
                    AssetError::io("не удалось клонировать дескриптор журнала", error)
                })?;
                self.directory = Some(clone);
                Ok(Some(directory))
            }
            Err(error) => Err(directory_entry_error(JOURNAL_DIR, error)),
        }
    }

    fn open_events_file(&mut self) -> Result<Option<File>, AssetError> {
        let Some(directory) = self.ensure_directory(false)? else {
            return Ok(None);
        };
        match openat(
            &directory,
            JOURNAL_FILE,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let file = File::from(fd);
                if !file
                    .metadata()
                    .map_err(|error| AssetError::io("не удалось проверить файл журнала", error))?
                    .is_file()
                {
                    return Err(AssetError::new(
                        ErrorCode::BoundaryViolation,
                        "журнал должен быть обычным файлом без symlink",
                    ));
                }
                Ok(Some(file))
            }
            Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(AssetError::io(
                "не удалось открыть файл журнала",
                std::io::Error::from(error),
            )),
        }
    }

    fn refresh(&mut self) -> Result<(), AssetError> {
        let file = self.open_events_file()?;
        let (size, modified) = match &file {
            Some(file) => {
                let metadata = file.metadata().map_err(|error| {
                    AssetError::io("не удалось проверить размер журнала", error)
                })?;
                (metadata.len(), metadata.modified().ok())
            }
            None => (0, None),
        };
        if let Some(state) = &self.state
            && state.file_size == size
            && state.file_modified == modified
        {
            return Ok(());
        }
        if size > max_journal_bytes() {
            // Полное чтение удержало бы в памяти сотни тысяч событий, поэтому
            // журнал обходится потоково: для дописывания достаточно ключей уже
            // записанных событий и номера последнего. Команды, читающие историю
            // целиком, в этом состоянии сообщают `journal_too_large`.
            let Some(mut file) = file else {
                return Err(journal_too_large_error(size));
            };
            let scan = scan_events(&mut file)?;
            self.state = Some(JournalState {
                events: Vec::new(),
                keys: scan.keys,
                torn_tail: scan.torn_tail,
                corrupt_lines: Vec::new(),
                last_seq: scan.last_seq,
                file_size: size,
                file_modified: modified,
                events_omitted: true,
            });
            return Ok(());
        }
        let bytes = match file {
            Some(mut file) => {
                let mut bytes = Vec::with_capacity(size as usize);
                file.read_to_end(&mut bytes)
                    .map_err(|error| AssetError::io("не удалось прочитать журнал", error))?;
                bytes
            }
            None => Vec::new(),
        };
        let (events, torn_tail, corrupt_lines) = parse_events(&bytes);
        let last_seq = events.iter().map(|event| event.seq).max().unwrap_or(0);
        let keys = events
            .iter()
            .map(|event| event.event_key.clone())
            .collect::<BTreeSet<_>>();
        self.state = Some(JournalState {
            events,
            keys,
            torn_tail,
            corrupt_lines,
            last_seq,
            file_size: size,
            file_modified: modified,
            events_omitted: false,
        });
        Ok(())
    }
}

/// Сохраняет данные события и, при первом создании файла, его имя в каталоге.
fn sync_event_append(
    file: &File,
    directory: &File,
    created: bool,
    sync_file: impl FnOnce(&File) -> std::io::Result<()>,
    sync_dir: impl FnOnce(&File) -> Result<(), AssetError>,
) -> Result<(), AssetError> {
    sync_file(file).map_err(|error| AssetError::io("не удалось синхронизировать журнал", error))?;
    if created {
        sync_dir(directory)?;
    }
    Ok(())
}

/// Проверяет целостность разобранного журнала.
///
/// `manifest_revision_max` — наибольшая ревизия среди фактически присутствующих
/// манифестов: канонического и `.runtime`. Событие не может ссылаться на ревизию,
/// которой в хранилище ещё не было.
pub fn check(read: &JournalRead, manifest_revision_max: u64) -> JournalCheck {
    inspect(read, manifest_revision_max).check
}

/// Проверяет структуру истории отдельно от допустимого незакрытого Pending.
/// Сверка может восстановить только последний случай: повреждённые связи нельзя
/// исправить дописыванием новых событий.
pub(crate) fn inspect(read: &JournalRead, manifest_revision_max: u64) -> JournalInspection {
    let mut problems = Vec::new();
    let mut structurally_valid = true;
    if read.torn_tail {
        structurally_valid = false;
        problems.push(
            "журнал оканчивается оборванной строкой без завершающего перевода строки".to_owned(),
        );
    }
    for line in &read.corrupt_lines {
        structurally_valid = false;
        problems.push(format!("строка {line} журнала не разбирается"));
    }

    let mut ids: BTreeMap<&str, Vec<(usize, &JournalEvent)>> = BTreeMap::new();
    for (index, event) in read.events.iter().enumerate() {
        ids.entry(event.event_id.as_str())
            .or_default()
            .push((index, event));
    }
    let mut seen_ids = BTreeSet::new();
    for (index, event) in read.events.iter().enumerate() {
        let expected = index as u64 + 1;
        if event.seq != expected {
            structurally_valid = false;
            problems.push(format!(
                "нарушена нумерация seq: ожидалось {expected}, получено {}",
                event.seq
            ));
        }
        if !seen_ids.insert(event.event_id.clone()) {
            structurally_valid = false;
            problems.push(format!("event_id {} повторяется", event.event_id));
        }
        if event.event_key != expected_event_key(event)
            && event.event_key != legacy_event_key(event)
        {
            structurally_valid = false;
            problems.push(format!(
                "event_key события seq={} не совпадает с его полями",
                event.seq
            ));
        }
        if let Some(revision) = event.manifest_revision
            && revision > manifest_revision_max
        {
            structurally_valid = false;
            problems.push(format!(
                "событие seq={} ссылается на ревизию {revision} больше текущей {manifest_revision_max}",
                event.seq
            ));
        }
    }

    let mut closings: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    let mut closed = BTreeSet::new();
    for (index, event) in read.events.iter().enumerate() {
        if event.operation == JournalOperation::Baseline {
            if event.outcome != JournalOutcome::Completed
                || event.intent_id.is_some()
                || event.snapshot.is_none()
            {
                structurally_valid = false;
                problems.push(format!(
                    "baseline seq={} должен быть однофазным completed-событием со снимком",
                    event.seq
                ));
            }
        } else if event.snapshot.is_some() {
            structurally_valid = false;
            problems.push(format!(
                "снимок состояния разрешён только для baseline, seq={}",
                event.seq
            ));
        }

        if event.outcome == JournalOutcome::Pending {
            if event.intent_id.is_some() || !supports_pending_intent(event.operation) {
                structurally_valid = false;
                problems.push(format!(
                    "событие seq={} не может быть Pending-интентом операции {}",
                    event.seq, event.operation
                ));
            }
            continue;
        }

        let Some(intent_id) = event.intent_id.as_deref() else {
            if !is_one_phase_event(event) {
                structurally_valid = false;
                problems.push(format!(
                    "событие результата seq={} операции {} не ссылается на Pending-интент",
                    event.seq, event.operation
                ));
            }
            continue;
        };

        let Some(matches) = ids.get(intent_id) else {
            structurally_valid = false;
            problems.push(format!(
                "событие seq={} ссылается на отсутствующий intent_id {intent_id}",
                event.seq
            ));
            continue;
        };
        if matches.len() != 1 {
            structurally_valid = false;
            problems.push(format!(
                "событие seq={} ссылается на неоднозначный intent_id {intent_id}",
                event.seq
            ));
            continue;
        }

        let (intent_index, intent) = matches[0];
        if intent_index >= index || intent.seq >= event.seq {
            structurally_valid = false;
            problems.push(format!(
                "событие seq={} ссылается не на предшествующий intent_id {intent_id}",
                event.seq
            ));
            continue;
        }
        if intent.outcome != JournalOutcome::Pending {
            structurally_valid = false;
            problems.push(format!(
                "событие seq={} ссылается на событие seq={}, которое не является Pending-интентом",
                event.seq, intent.seq
            ));
            continue;
        }
        if intent.domain != event.domain
            || intent.namespace != event.namespace
            || intent.identity != event.identity
            || intent.operation != event.operation
        {
            structurally_valid = false;
            problems.push(format!(
                "событие seq={} не совпадает с областью, идентичностью и операцией intent_id {intent_id}",
                event.seq
            ));
            continue;
        }
        closings.entry(intent_id).or_default().push(event.seq);
        closed.insert(intent_id);
    }

    // Интент закрывается ровно одним подходящим событием результата.
    for event in read.events.iter().filter(|event| event.is_pending_intent()) {
        if let Some(seqs) = closings.get(event.event_id.as_str())
            && seqs.len() > 1
        {
            structurally_valid = false;
            let listed = seqs
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            problems.push(format!(
                "интент seq={} закрыт более чем одним событием результата: seq {listed}",
                event.seq
            ));
        }
    }
    let open_intents: Vec<JournalOpenIntent> = read
        .events
        .iter()
        .filter(|event| event.is_pending_intent() && !closed.contains(event.event_id.as_str()))
        .map(|event| JournalOpenIntent {
            event_id: event.event_id.clone(),
            seq: event.seq,
            identity: event.identity.clone(),
            namespace: event.namespace.clone(),
            operation: event.operation,
            recorded_at: event.recorded_at.clone(),
        })
        .collect();
    for intent in &open_intents {
        problems.push(format!(
            "интент seq={} ({}, {}) не закрыт событием результата",
            intent.seq, intent.operation, intent.identity
        ));
    }

    JournalInspection {
        check: JournalCheck {
            event_count: read.events.len() as u64,
            torn_tail: read.torn_tail,
            corrupt_lines: read.corrupt_lines.clone(),
            open_intents,
            problems,
        },
        structurally_valid,
    }
}

#[derive(Debug)]
pub(crate) struct JournalInspection {
    pub check: JournalCheck,
    pub structurally_valid: bool,
}

const fn supports_pending_intent(operation: JournalOperation) -> bool {
    matches!(
        operation,
        JournalOperation::CandidateWritten
            | JournalOperation::VerifiedPublished
            | JournalOperation::TrustGranted
            | JournalOperation::TrustRevoked
            | JournalOperation::Revalidated
            | JournalOperation::Quarantined
    )
}

fn is_one_phase_event(event: &JournalEvent) -> bool {
    event.outcome == JournalOutcome::Completed
        && matches!(
            event.operation,
            JournalOperation::VerifiedReused
                | JournalOperation::Removed
                | JournalOperation::Recovered
                | JournalOperation::LayoutMigrated
                | JournalOperation::Baseline
        )
}

/// Снимок текущего состояния обоих манифестов хранилища.
pub fn snapshot(canonical: &Manifest, runtime: &Manifest) -> JournalSnapshot {
    let mut verified = 0_u64;
    let mut pending = 0_u64;
    let mut quarantined = 0_u64;
    let mut assets = 0_u64;
    for record in canonical.assets.iter().chain(runtime.assets.iter()) {
        assets = assets.saturating_add(1);
        match record.lifecycle {
            LifecycleState::Verified => verified = verified.saturating_add(1),
            LifecycleState::Pending => pending = pending.saturating_add(1),
            LifecycleState::Quarantined => quarantined = quarantined.saturating_add(1),
        }
    }
    JournalSnapshot {
        assets,
        verified,
        pending,
        quarantined,
        manifest_revision: canonical.revision,
    }
}

/// Суммарная ревизия обеих областей хранилища.
///
/// Событие уровня всего хранилища (`baseline`, `layout_migrated`) не принадлежит
/// одной области, поэтому помечается суммой: она строго возрастает при любой
/// миграции раскладки и всегда не меньше ревизии каждой отдельной области.
pub fn revision_bound(canonical: &Manifest, runtime: &Manifest) -> u64 {
    canonical.revision.saturating_add(runtime.revision)
}

/// Итог потокового обхода журнала.
#[derive(Debug, Default)]
struct JournalScan {
    keys: BTreeSet<String>,
    last_seq: u64,
    torn_tail: bool,
}

/// Обходит журнал построчно, удерживая только ключи событий.
fn scan_events(file: &mut File) -> Result<JournalScan, AssetError> {
    let mut reader = BufReader::new(file);
    let mut scan = JournalScan::default();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read = reader
            .read_until(b'\n', &mut buffer)
            .map_err(|error| AssetError::io("не удалось прочитать журнал", error))?;
        if read == 0 {
            break;
        }
        let terminated = buffer.last() == Some(&b'\n');
        if !terminated {
            scan.torn_tail = true;
        }
        let line = if terminated {
            &buffer[..buffer.len() - 1]
        } else {
            &buffer[..]
        };
        if let Some(event) = parse_event(line) {
            scan.last_seq = scan.last_seq.max(event.seq);
            scan.keys.insert(event.event_key);
        }
        if !terminated {
            break;
        }
    }
    Ok(scan)
}

/// Разбирает одну строку журнала в событие.
fn parse_event(line: &[u8]) -> Option<JournalEvent> {
    match serde_json::from_slice::<JournalEvent>(line) {
        Ok(event) if event.schema_version == JOURNAL_SCHEMA_VERSION && event.seq > 0 => Some(event),
        _ => None,
    }
}

fn parse_events(bytes: &[u8]) -> (Vec<JournalEvent>, bool, Vec<u64>) {
    if bytes.is_empty() {
        return (Vec::new(), false, Vec::new());
    }
    let ends_with_newline = bytes.last() == Some(&b'\n');
    let mut lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    // Последний элемент — либо пустой хвост после завершающего перевода строки,
    // либо оборванная запись, которая не считается событием.
    let tail = lines.pop().unwrap_or_default();
    let torn_tail = !ends_with_newline;
    let mut events = Vec::new();
    let mut corrupt_lines = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let number = index as u64 + 1;
        if line.is_empty() {
            corrupt_lines.push(number);
            continue;
        }
        match parse_event(line) {
            Some(event) => events.push(event),
            None => corrupt_lines.push(number),
        }
    }
    if torn_tail && !tail.is_empty() {
        // Запись без завершающего перевода строки, но целиком разобранная,
        // остаётся событием: иначе следующее событие получило бы тот же `seq`,
        // а журнал навсегда остался бы с нарушенной нумерацией.
        if let Some(event) = parse_event(tail) {
            events.push(event);
        }
    }
    (events, torn_tail, corrupt_lines)
}

/// Ошибка полного чтения журнала, превысившего предел.
fn journal_too_large_error(size: u64) -> AssetError {
    AssetError::with_details(
        ErrorCode::JournalTooLarge,
        format!(
            "журнал занимает {size} байт и превышает предел полного чтения {} байт: \
             история доступна только дописыванием, а для полного чтения журнал нужно \
             заархивировать",
            max_journal_bytes()
        ),
        serde_json::json!({
            "journal_bytes": size,
            "max_journal_bytes": max_journal_bytes(),
        }),
    )
}

fn sanitize_reason(reason: &str) -> String {
    reason
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_REASON_CHARS)
        .collect()
}

/// Форматирует момент времени как `RFC3339` в UTC с суффиксом `Z`.
pub fn format_rfc3339(time: SystemTime) -> String {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = duration.as_secs();
    let milliseconds = duration.subsec_millis();
    let days = (seconds / 86_400) as i64;
    let second_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{milliseconds:03}Z",
        second_of_day / 3_600,
        (second_of_day % 3_600) / 60,
        second_of_day % 60,
    )
}

/// Календарная дата из числа дней от `1970-01-01` (алгоритм Хауарда Хиннанта).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = (shifted - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::temp_workspace::TempWorkspace;

    struct Fixture {
        workspace: TempWorkspace,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                workspace: TempWorkspace::create("asset-store-journal-tests")
                    .expect("создаётся временный каталог теста"),
            }
        }

        fn path(&self) -> &Path {
            self.workspace.path()
        }

        fn journal(&self) -> Journal {
            let root = File::open(self.path()).expect("открывается корневой каталог теста");
            Journal::open(&root).expect("открывается журнал")
        }

        fn events_file(&self) -> PathBuf {
            self.path().join(JOURNAL_DIR).join(JOURNAL_FILE)
        }

        fn read_events(&self) -> Vec<JournalEvent> {
            let text = fs::read_to_string(self.events_file()).expect("журнал читается с диска");
            text.lines()
                .map(|line| serde_json::from_str::<JournalEvent>(line).expect("строка журнала"))
                .collect()
        }
    }

    fn draft(operation: JournalOperation, outcome: JournalOutcome) -> JournalDraft {
        JournalDraft::store_wide("kanji", "kanji", "日", operation, outcome)
    }

    #[test]
    fn append_assigns_sequence_identity_and_key_and_survives_reopen() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let first = journal
            .append_and_return(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Pending,
            ))
            .expect("первое событие записано");
        let second = journal
            .append_and_return(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Completed,
            ))
            .expect("второе событие записано");

        assert_eq!(first.seq, 1);
        assert_eq!(second.seq, 2);
        assert_eq!(first.schema_version, JOURNAL_SCHEMA_VERSION);
        assert_ne!(first.event_id, second.event_id);
        assert_ne!(first.event_key, second.event_key);
        assert!(first.is_pending_intent());
        assert!(!first.closes_intent());
        assert!(!second.is_pending_intent());

        // Повторное открытие журнала другим дескриптором — это перезапуск CLI.
        let mut reopened = fixture.journal();
        let read = reopened.read().expect("журнал перечитывается");
        assert!(!read.torn_tail);
        assert!(read.corrupt_lines.is_empty());
        assert_eq!(read.events, vec![first, second]);
    }

    #[test]
    fn first_event_syncs_file_data_before_the_new_directory_entry() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        journal
            .append(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Pending,
            ))
            .expect("первое событие создано");
        let directory = File::open(fixture.path().join(JOURNAL_DIR)).expect("каталог журнала");
        let file = File::open(fixture.events_file()).expect("файл журнала");
        let order = std::cell::RefCell::new(Vec::new());

        sync_event_append(
            &file,
            &directory,
            true,
            |_| {
                order.borrow_mut().push("file");
                Ok(())
            },
            |_| {
                order.borrow_mut().push("directory");
                Ok(())
            },
        )
        .expect("синхронизация завершена");

        assert_eq!(*order.borrow(), ["file", "directory"]);

        let reused_order = std::cell::RefCell::new(Vec::new());
        sync_event_append(
            &file,
            &directory,
            false,
            |_| {
                reused_order.borrow_mut().push("file");
                Ok(())
            },
            |_| {
                reused_order.borrow_mut().push("directory");
                Ok(())
            },
        )
        .expect("повторная запись синхронизирована");
        assert_eq!(*reused_order.borrow(), ["file"]);
    }

    #[test]
    fn a_journal_over_the_read_limit_still_accepts_events() {
        let fixture = Fixture::new();
        let completed = draft(JournalOperation::VerifiedReused, JournalOutcome::Completed);
        let mut journal = fixture.journal();
        assert!(
            journal.append(&completed).expect("первое событие записано"),
            "первое событие новое"
        );
        drop(journal);

        // Предел ниже одного события, а перезапуск CLI — это новое чтение:
        // история целиком недоступна, но дописывание обязано работать.
        set_max_journal_bytes_for_test(Some(1));
        let mut reopened = fixture.journal();
        let duplicate = reopened.append(&completed).expect("дубликат обработан");
        assert!(
            !duplicate,
            "повтор события распознаётся по ключу без удержания событий"
        );
        // Интент не считается дубликатом: его event_id нужен закрывающему событию.
        let intent = reopened
            .append_and_return(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Pending,
            ))
            .expect("интент записан");
        assert_eq!(intent.seq, 2);
        assert_eq!(fixture.read_events().len(), 2);

        let error = reopened.read().expect_err("история целиком недоступна");
        assert_eq!(error.code, ErrorCode::JournalTooLarge);
        assert!(error.details["journal_bytes"].as_u64().is_some());
        assert_eq!(error.details["max_journal_bytes"].as_u64(), Some(1));
        set_max_journal_bytes_for_test(None);
    }

    #[test]
    fn a_complete_event_without_the_final_newline_stays_an_event() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        journal
            .append_and_return(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Pending,
            ))
            .expect("интент записан");

        // Сбой записи потерял только завершающий перевод строки: событие записано
        // целиком и обязано остаться событием, иначе следующее получит тот же seq.
        let mut bytes = fs::read(fixture.events_file()).expect("журнал прочитан");
        assert_eq!(bytes.pop(), Some(b'\n'));
        fs::write(fixture.events_file(), &bytes).expect("хвост без перевода строки");

        let mut reopened = fixture.journal();
        let read = reopened.read().expect("журнал перечитывается");
        assert!(read.torn_tail, "оборванный хвост диагностируется");
        assert_eq!(read.events.len(), 1, "разобранная запись остаётся событием");
        assert!(read.corrupt_lines.is_empty());

        let next = journal
            .append(&draft(
                JournalOperation::VerifiedReused,
                JournalOutcome::Completed,
            ))
            .expect("следующее событие записано");
        assert!(next);
        let seqs: Vec<u64> = fixture
            .read_events()
            .iter()
            .map(|event| event.seq)
            .collect();
        assert_eq!(seqs, vec![1, 2], "нумерация продолжается, а не повторяется");
    }

    #[test]
    fn repeated_event_key_is_not_appended_twice() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let draft = draft(JournalOperation::VerifiedReused, JournalOutcome::Completed);
        assert!(
            journal.append(&draft).expect("первое событие записано"),
            "первое событие журнала новое"
        );
        assert!(
            !journal.append(&draft).expect("дубликат обработан"),
            "событие с тем же event_key не записывается повторно"
        );
        assert_eq!(fixture.read_events().len(), 1);
    }

    #[test]
    fn intent_and_refusal_are_distinct_events_even_when_state_is_unchanged() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        let intent = journal
            .append_and_return(
                &draft(JournalOperation::Revalidated, JournalOutcome::Pending)
                    .revision(Some(7))
                    .sha(Some("aa"), Some("aa"))
                    .verified(Some(false)),
            )
            .expect("интент записан");
        let refused = journal
            .append_and_return(
                &draft(JournalOperation::Revalidated, JournalOutcome::Refused)
                    .closing(&intent.event_id)
                    .revision(Some(7))
                    .sha(Some("aa"), Some("aa"))
                    .verified(Some(false))
                    .reason("валидатор отказал"),
            )
            .expect("отказ записан");
        assert_ne!(intent.event_id, refused.event_id);
        assert_eq!(fixture.read_events().len(), 2);
    }

    #[test]
    fn torn_tail_is_reported_and_does_not_block_the_next_append() {
        let fixture = Fixture::new();
        let mut journal = fixture.journal();
        journal
            .append(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Pending,
            ))
            .expect("первое событие записано");
        journal
            .append(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Completed,
            ))
            .expect("второе событие записано");
        drop(journal);

        // Обрываем последнюю строку: файл без завершающего перевода строки.
        let path = fixture.events_file();
        let text = fs::read_to_string(&path).expect("журнал читается");
        let truncated = text.trim_end_matches('\n');
        fs::write(&path, &truncated[..truncated.len() - 8]).expect("хвост журнала оборван");

        let mut journal = fixture.journal();
        let read = journal.read().expect("оборванный журнал разбирается");
        assert!(read.torn_tail, "оборванный хвост распознан");
        assert_eq!(read.events.len(), 1);
        assert_eq!(read.events[0].seq, 1);

        let appended = journal
            .append_and_return(&draft(
                JournalOperation::CandidateWritten,
                JournalOutcome::Completed,
            ))
            .expect("запись после оборванного хвоста продолжается");
        assert_eq!(appended.seq, 2);

        // После отделения оборванной строки файл снова разбирается, а оборванная
        // запись честно помечается повреждённой строкой 2.
        let mut reopened = fixture.journal();
        let read = reopened.read().expect("журнал перечитывается");
        assert!(!read.torn_tail);
        assert_eq!(read.corrupt_lines, vec![2]);
        assert_eq!(read.events.len(), 2);
        assert_eq!(read.events[1].seq, 2);
    }

    #[test]
    fn check_reports_sequence_gaps_duplicates_and_dangling_intents() {
        let events = vec![
            event(1, "id-1", None, JournalOutcome::Pending),
            event(3, "id-3", None, JournalOutcome::Completed),
            event(4, "id-3", Some("missing"), JournalOutcome::Completed),
        ];
        let read = JournalRead {
            events,
            torn_tail: false,
            corrupt_lines: vec![9],
        };
        let check = check(&read, 5);
        assert!(!check.is_clean());
        assert_eq!(check.event_count, 3);
        assert_eq!(check.corrupt_lines, vec![9]);
        assert!(
            check
                .problems
                .iter()
                .any(|problem| problem.contains("нарушена нумерация seq")),
            "пропуск seq обнаружен: {:?}",
            check.problems
        );
        assert!(
            check
                .problems
                .iter()
                .any(|problem| problem.contains("повторяется")),
            "дубль event_id обнаружен: {:?}",
            check.problems
        );
        assert!(
            check
                .problems
                .iter()
                .any(|problem| problem.contains("отсутствующий intent_id")),
            "висячая ссылка обнаружена: {:?}",
            check.problems
        );
        assert!(
            check
                .problems
                .iter()
                .any(|problem| problem.contains("не закрыт событием результата")),
            "незакрытый интент обнаружен: {:?}",
            check.problems
        );
        assert_eq!(check.open_intents.len(), 1);
        assert_eq!(check.open_intents[0].seq, 1);
    }

    #[test]
    fn check_reports_an_intent_with_two_closing_events() {
        let read = JournalRead {
            events: vec![
                event(1, "id-1", None, JournalOutcome::Pending),
                event(2, "id-2", Some("id-1"), JournalOutcome::Refused),
                event(3, "id-3", Some("id-1"), JournalOutcome::Completed),
            ],
            torn_tail: false,
            corrupt_lines: Vec::new(),
        };
        let check = check(&read, 5);
        assert!(!check.is_clean());
        assert!(
            check
                .problems
                .iter()
                .any(|problem| problem.contains("более чем одним событием результата")),
            "двойное закрытие обнаружено: {:?}",
            check.problems
        );
        assert!(check.open_intents.is_empty(), "интент считается закрытым");
    }

    #[test]
    fn check_accepts_valid_open_intents_and_supported_one_phase_events() {
        let mut events = vec![event(1, "pending", None, JournalOutcome::Pending)];
        let operations = [
            JournalOperation::VerifiedReused,
            JournalOperation::Removed,
            JournalOperation::Recovered,
            JournalOperation::LayoutMigrated,
            JournalOperation::Baseline,
        ];
        for (offset, operation) in operations.into_iter().enumerate() {
            let mut one_phase = event(
                offset as u64 + 2,
                &format!("one-phase-{offset}"),
                None,
                JournalOutcome::Completed,
            );
            one_phase.operation = operation;
            if operation == JournalOperation::Baseline {
                one_phase.snapshot = Some(JournalSnapshot {
                    assets: 1,
                    verified: 1,
                    pending: 0,
                    quarantined: 0,
                    manifest_revision: 1,
                });
            }
            one_phase.event_key = expected_event_key(&one_phase);
            events.push(one_phase);
        }

        let inspection = inspect(
            &JournalRead {
                events,
                torn_tail: false,
                corrupt_lines: Vec::new(),
            },
            5,
        );
        assert!(inspection.structurally_valid);
        assert_eq!(inspection.check.open_intents.len(), 1);
        assert!(!inspection.check.is_clean());
    }

    #[test]
    fn check_accepts_valid_schema_one_keys_from_before_intent_ids_were_hashed() {
        let mut pending = event(1, "pending", None, JournalOutcome::Pending);
        pending.event_key = legacy_event_key(&pending);
        let mut result = event(2, "result", Some("pending"), JournalOutcome::Completed);
        result.event_key = legacy_event_key(&result);
        let mut reused = event(3, "reused", None, JournalOutcome::Completed);
        reused.event_key = legacy_event_key(&reused);

        let inspection = inspect(
            &JournalRead {
                events: vec![pending, result, reused],
                torn_tail: false,
                corrupt_lines: Vec::new(),
            },
            5,
        );
        assert!(inspection.structurally_valid);
        assert!(
            inspection.check.is_clean(),
            "{:?}",
            inspection.check.problems
        );
    }

    #[test]
    fn check_rejects_redirected_and_malformed_intent_links() {
        let pending = |seq, id, identity: &str| {
            let mut intent = event(seq, id, None, JournalOutcome::Pending);
            intent.identity = identity.to_owned();
            intent.event_key = expected_event_key(&intent);
            intent
        };
        let mut redirected = event(
            3,
            "result",
            Some("pending-other"),
            JournalOutcome::Completed,
        );
        redirected.identity = "日".to_owned();
        redirected.event_key = expected_event_key(&redirected);
        let redirected_inspection = inspect(
            &JournalRead {
                events: vec![
                    pending(1, "pending-right", "日"),
                    pending(2, "pending-other", "月"),
                    redirected,
                ],
                torn_tail: false,
                corrupt_lines: Vec::new(),
            },
            5,
        );
        assert!(!redirected_inspection.structurally_valid);
        assert!(redirected_inspection.check.problems.iter().any(|problem| {
            problem.contains("не совпадает с областью, идентичностью и операцией")
        }));

        let mut wrong_operation = event(
            2,
            "result-operation",
            Some("pending"),
            JournalOutcome::Completed,
        );
        wrong_operation.operation = JournalOperation::CandidateWritten;
        wrong_operation.event_key = expected_event_key(&wrong_operation);
        let operation_inspection = inspect(
            &JournalRead {
                events: vec![pending(1, "pending", "日"), wrong_operation],
                torn_tail: false,
                corrupt_lines: Vec::new(),
            },
            5,
        );
        assert!(!operation_inspection.structurally_valid);
        assert!(operation_inspection.check.problems.iter().any(|problem| {
            problem.contains("не совпадает с областью, идентичностью и операцией")
        }));

        let mut points_to_fact = event(2, "result", Some("fact"), JournalOutcome::Completed);
        points_to_fact.operation = JournalOperation::Revalidated;
        points_to_fact.event_key = expected_event_key(&points_to_fact);
        let non_intent_inspection = inspect(
            &JournalRead {
                events: vec![
                    event(1, "fact", None, JournalOutcome::Completed),
                    points_to_fact,
                ],
                torn_tail: false,
                corrupt_lines: Vec::new(),
            },
            5,
        );
        assert!(!non_intent_inspection.structurally_valid);
        assert!(
            non_intent_inspection.check.problems.iter().any(|problem| {
                problem.contains("не является Pending-интентом")
            })
        );

        let mut missing_link = event(2, "result", None, JournalOutcome::Completed);
        missing_link.operation = JournalOperation::Revalidated;
        missing_link.event_key = expected_event_key(&missing_link);
        let missing_link_inspection = inspect(
            &JournalRead {
                events: vec![pending(1, "pending", "日"), missing_link],
                torn_tail: false,
                corrupt_lines: Vec::new(),
            },
            5,
        );
        assert!(!missing_link_inspection.structurally_valid);
        assert!(
            missing_link_inspection
                .check
                .problems
                .iter()
                .any(|problem| {
                    problem.contains("не ссылается на Pending-интент")
                })
        );
    }

    #[test]
    fn check_rejects_tampered_event_keys_and_cross_scope_closures() {
        let intent = event(1, "pending", None, JournalOutcome::Pending);
        let mut wrong_identity = event(2, "result", Some("pending"), JournalOutcome::Completed);
        wrong_identity.identity = "月".to_owned();
        wrong_identity.event_key = expected_event_key(&wrong_identity);
        let identity_inspection = inspect(
            &JournalRead {
                events: vec![intent.clone(), wrong_identity],
                torn_tail: false,
                corrupt_lines: Vec::new(),
            },
            5,
        );
        assert!(!identity_inspection.structurally_valid);

        let mut wrong_domain = event(
            2,
            "result-domain",
            Some("pending"),
            JournalOutcome::Completed,
        );
        wrong_domain.domain = "pitch".to_owned();
        wrong_domain.event_key = expected_event_key(&wrong_domain);
        let domain_inspection = inspect(
            &JournalRead {
                events: vec![intent.clone(), wrong_domain],
                torn_tail: false,
                corrupt_lines: Vec::new(),
            },
            5,
        );
        assert!(!domain_inspection.structurally_valid);

        let mut tampered = event(1, "one-phase", None, JournalOutcome::Completed);
        tampered.event_key = "not-the-derived-key".to_owned();
        let key_inspection = inspect(
            &JournalRead {
                events: vec![tampered],
                torn_tail: false,
                corrupt_lines: Vec::new(),
            },
            5,
        );
        assert!(!key_inspection.structurally_valid);
        assert!(
            key_inspection
                .check
                .problems
                .iter()
                .any(|problem| problem.contains("event_key") && problem.contains("не совпадает"))
        );
    }

    #[test]
    fn check_rejects_revision_beyond_the_manifest() {
        let read = JournalRead {
            events: vec![event(1, "id-1", None, JournalOutcome::Completed)],
            torn_tail: false,
            corrupt_lines: Vec::new(),
        };
        let clean = check(&read, 5);
        assert!(clean.is_clean(), "{:?}", clean.problems);

        let mut future = read.clone();
        future.events[0].manifest_revision = Some(6);
        let stale = check(&future, 5);
        assert!(
            stale
                .problems
                .iter()
                .any(|problem| problem.contains("больше текущей")),
            "ревизия сверх манифеста обнаружена: {:?}",
            stale.problems
        );
    }

    #[test]
    fn timestamps_are_rfc3339_utc() {
        let formatted = format_rfc3339(UNIX_EPOCH);
        assert_eq!(formatted, "1970-01-01T00:00:00.000Z");
        let formatted =
            format_rfc3339(UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123));
        assert_eq!(formatted, "2023-11-14T22:13:20.123Z");
    }

    fn event(
        seq: u64,
        event_id: &str,
        intent_id: Option<&str>,
        outcome: JournalOutcome,
    ) -> JournalEvent {
        let operation = if intent_id.is_some() || outcome == JournalOutcome::Pending {
            JournalOperation::Revalidated
        } else {
            JournalOperation::VerifiedReused
        };
        let mut event = JournalEvent {
            schema_version: JOURNAL_SCHEMA_VERSION,
            seq,
            event_id: event_id.to_owned(),
            event_key: String::new(),
            recorded_at: "2024-01-01T00:00:00.000Z".to_owned(),
            domain: "kanji".to_owned(),
            namespace: "kanji".to_owned(),
            identity: "日".to_owned(),
            operation,
            outcome,
            intent_id: intent_id.map(str::to_owned),
            manifest_revision: Some(1),
            previous_sha256: None,
            new_sha256: None,
            previous_lifecycle: None,
            new_lifecycle: None,
            verified: None,
            run_id: None,
            batch_id: None,
            provenance: None,
            reason: None,
            snapshot: None,
        };
        event.event_key = expected_event_key(&event);
        event
    }
}
