//! Общий слой CLI для локального журнала операций.
//!
//! Владелец предметных команд — `kanji-assets` и `pitch-assets`; здесь живут
//! только описание подкоманды `journal`, общий формат машинного ответа, разбор
//! значения `--operation` и русское представление событий, чтобы оба CLI не
//! расходились.

use clap::Subcommand;
use serde::Serialize;

use crate::error::{AssetError, ErrorCode};
use crate::journal::{JournalCheck, JournalEvent, JournalOperation};

/// Команды локального журнала операций.
///
/// Описание общее для обоих CLI: разойдясь, они перестали бы быть одним
/// контрактом команды `journal`.
#[derive(Debug, Subcommand)]
pub enum JournalCommand {
    /// Выводит события журнала по возрастанию `seq`.
    List {
        /// Оставляет только последние N событий.
        #[arg(long)]
        limit: Option<usize>,
        /// Только события с точным ключом ресурса (`identity` события).
        #[arg(long)]
        identity: Option<String>,
        /// Только события указанной операции, например `verified_published`.
        #[arg(long)]
        operation: Option<String>,
    },
    /// Проверяет целостность журнала; ненулевой код возврата при найденной проблеме.
    Check,
    /// Сверяет незакрытые интенты с фактическим состоянием манифестов.
    Reconcile,
    /// Записывает единственное событие `baseline` со снимком текущего состояния.
    Baseline,
}

impl JournalCommand {
    /// Имя операции в машинном ответе.
    #[must_use]
    pub const fn operation_name(&self) -> &'static str {
        match self {
            Self::List { .. } => "journal_list",
            Self::Check => "journal_check",
            Self::Reconcile => "journal_reconcile",
            Self::Baseline => "journal_baseline",
        }
    }
}

/// Вид команды `journal`, определяющий русское представление ответа.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JournalKind {
    #[default]
    List,
    Check,
    Reconcile,
    Baseline,
}

/// Машиночитаемая часть ответа команд `journal`.
#[derive(Debug, Default, Serialize)]
pub struct JournalReport {
    /// Вид команды; в JSON не попадает.
    #[serde(skip)]
    pub kind: JournalKind,
    /// События в порядке возрастания `seq`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<JournalEvent>,
    /// Результат проверки целостности для `journal check`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<JournalCheck>,
    /// События, фактически дописанные сверкой.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub appended: Vec<JournalEvent>,
    /// Единственное событие `baseline`, записанное командой.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline: Option<JournalEvent>,
}

/// Русское представление всего ответа команды `journal`.
pub fn report_text(report: &JournalReport) -> String {
    let mut text = String::new();
    match report.kind {
        JournalKind::List => {
            for event in &report.events {
                text.push_str(&event_line(event));
            }
            if report.events.is_empty() {
                text.push_str("Событий нет.\n");
            }
        }
        JournalKind::Check => {
            if let Some(check) = &report.check {
                text.push_str(&check_text(check));
            }
        }
        JournalKind::Reconcile => text.push_str(&appended_text(&report.appended)),
        JournalKind::Baseline => {
            if let Some(baseline) = &report.baseline {
                text.push_str("Записан baseline:\n");
                text.push_str(&event_line(baseline));
            }
        }
    }
    text
}

/// Разбирает значение `--operation` в операцию журнала.
pub fn parse_operation(value: &str) -> Result<JournalOperation, AssetError> {
    JournalOperation::parse(value).ok_or_else(|| {
        AssetError::new(
            ErrorCode::InvalidTransition,
            format!("неизвестная операция журнала: {value}"),
        )
    })
}

/// Русское представление одного события журнала.
pub fn event_line(event: &JournalEvent) -> String {
    let mut line = format!(
        "Событие {}  {}  {}  {}:{}",
        event.seq, event.operation, event.outcome, event.namespace, event.identity
    );
    if let Some(revision) = event.manifest_revision {
        line.push_str(&format!("  revision={revision}"));
    }
    line.push_str(&format!(
        "  sha={}→{}",
        event.previous_sha256.as_deref().unwrap_or("-"),
        event.new_sha256.as_deref().unwrap_or("-")
    ));
    if let Some(verified) = event.verified {
        line.push_str(if verified {
            "  verified=да"
        } else {
            "  verified=нет"
        });
    }
    if let Some(intent_id) = &event.intent_id {
        line.push_str(&format!("  intent={intent_id}"));
    }
    if let Some(reason) = &event.reason {
        line.push_str(&format!("  причина={reason}"));
    }
    if let Some(snapshot) = &event.snapshot {
        line.push_str(&format!(
            "  снимок: assets={}, verified={}, pending={}, quarantined={}, revision={}",
            snapshot.assets,
            snapshot.verified,
            snapshot.pending,
            snapshot.quarantined,
            snapshot.manifest_revision
        ));
    }
    line.push_str(&format!("  {}\n", event.recorded_at));
    line
}

/// Русское представление результата проверки целостности журнала.
pub fn check_text(check: &JournalCheck) -> String {
    let mut text = format!(
        "Событий: {}; оборванный хвост: {}; неразобранных строк: {}\n",
        check.event_count,
        if check.torn_tail { "да" } else { "нет" },
        check.corrupt_lines.len()
    );
    if check.problems.is_empty() {
        text.push_str("Проблем не найдено.\n");
    } else {
        for problem in &check.problems {
            text.push_str(&format!("Проблема: {problem}\n"));
        }
    }
    if !check.open_intents.is_empty() {
        text.push_str("Незакрытые интенты:\n");
        for intent in &check.open_intents {
            text.push_str(&format!(
                "  seq={} {} {}:{} от {} event_id={}\n",
                intent.seq,
                intent.operation,
                intent.namespace,
                intent.identity,
                intent.recorded_at,
                intent.event_id
            ));
        }
    }
    text
}

/// Русское представление событий, дописанных сверкой.
pub fn appended_text(appended: &[JournalEvent]) -> String {
    if appended.is_empty() {
        "Сверка не дописала ни одного события.\n".to_owned()
    } else {
        let mut text = format!("Сверка дописала событий: {}.\n", appended.len());
        for event in appended {
            text.push_str(&event_line(event));
        }
        text
    }
}
