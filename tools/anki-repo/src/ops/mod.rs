//! Предметные операции: `inspect`, `find`, `stats`, `validate`, `edit`, `qa`,
//! `review`, `review-check`, `models`, `create`, `retire`, `migrate-media`,
//! `migrate-layout`, `visual-report`.
//!
//! Каждая операция возвращает собственный результат. Текстовый и JSON-форматы
//! — два представления одного и того же результата.

pub mod create;
pub mod create_media;
pub mod deck_select;
pub mod edit;
pub mod find;
pub mod inspect;
pub mod migrate_layout;
pub mod migrate_media;
pub mod models;
pub mod publish;
pub mod qa;
pub mod retire;
pub mod review;
pub mod review_check;
pub mod source;
pub mod stats;
pub mod structural;
pub mod validate;
pub mod visual_report;

use crate::index::{ExportIndex, NoteRef, resolve_named_fields};
use crate::media::MediaReport;
use crate::model::FieldValue;

/// Именованное значение поля заметки в порядке `ord` модели.
#[derive(Debug, Clone)]
pub struct NamedField {
    /// Имя поля из определения модели.
    pub name: String,
    /// Позиция поля; `None`, если `ord` в модели malformed.
    pub ord: Option<i64>,
    /// Значение поля; `None`, если значения по этой позиции нет.
    pub value: Option<String>,
}

/// Компактная сводка заметки, одинаковая для всех читающих команд.
///
/// Разрешение модели и полей выполняется здесь ровно один раз: `find` и
/// `review` не должны строить эту сводку каждая по-своему.
#[derive(Debug)]
pub struct NoteSummary {
    /// Идентификатор заметки.
    pub guid: Option<String>,
    /// Путь колоды заметки.
    pub deck_path: String,
    /// Имя модели заметки.
    pub note_model_name: Option<String>,
    /// Идентичность модели заметки.
    pub note_model_uuid: Option<String>,
    /// Теги заметки.
    pub tags: Vec<String>,
    /// Поля заметки в порядке `ord` модели.
    pub fields: Vec<NamedField>,
}

impl NoteSummary {
    /// Строит сводку по заметке в индексах экспорта.
    #[must_use]
    pub fn build(index: &ExportIndex<'_>, entry: &NoteRef<'_>) -> Self {
        let model = entry
            .note
            .note_model_uuid
            .as_deref()
            .and_then(|uuid| index.model_by_uuid(uuid));

        let fields = model.map_or_else(Vec::new, |model| {
            resolve_named_fields(entry.note, model)
                .into_iter()
                .map(|field| NamedField {
                    name: field.name.to_string(),
                    ord: field.ord.value(),
                    value: field.value.map(FieldValue::rendered),
                })
                .collect()
        });

        Self {
            guid: entry.note.guid.clone(),
            deck_path: index.note_deck_path(entry).to_string(),
            note_model_name: model.and_then(|model| model.name.clone()),
            note_model_uuid: entry.note.note_model_uuid.clone(),
            tags: entry.note.tags.clone(),
            fields,
        }
    }
}

/// Пара «ключ — количество» для детерминированных распределений.
#[derive(Debug, Clone, PartialEq)]
pub struct CountEntry {
    /// Ключ распределения.
    pub key: String,
    /// Количество.
    pub count: usize,
}

/// Компактные media-счётчики.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaCounters {
    /// Сколько имён объявлено суммарно.
    pub declared_total: usize,
    /// Сколько уникальных имён объявлено.
    pub declared_unique: usize,
    /// Сколько объявленных имён повторяются.
    pub duplicate_declared: usize,
    /// Существует ли каталог `media/`.
    pub dir_present: bool,
    /// Сколько файлов физически лежит в `media/`.
    pub physical_total: usize,
    /// Сколько объявленных имён не найдено физически.
    pub missing_physical: usize,
    /// Сколько физических файлов не объявлено.
    pub undeclared_physical: usize,
}

impl MediaCounters {
    /// Считает счётчики по подробной сводке.
    pub fn from_report(report: &MediaReport) -> Self {
        Self {
            declared_total: report.declared_total,
            declared_unique: report.declared.len(),
            duplicate_declared: report.duplicate_declared.len(),
            dir_present: report.dir_present,
            physical_total: report.physical.len(),
            missing_physical: report.declared.difference(&report.physical).count(),
            undeclared_physical: report.physical.difference(&report.declared).count(),
        }
    }
}

/// Ограничение размера диагностических выборок в verbose-выводе.
pub const SAMPLE_LIMIT: usize = 5;
