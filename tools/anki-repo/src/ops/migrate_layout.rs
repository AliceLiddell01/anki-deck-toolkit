//! Контролируемая миграция раскладки CrowdAnki-экспортов репозитория.
//!
//! Репозиторий переходит от раскладки с каталогами группировки
//! (`decks/<группа>/<подгруппа>/<имя_экспорта>/`) к плоскому списку
//! `decks/output/<имя_экспорта>/`. Операция обязана сохранить байты и
//! идентичности экспортов, поэтому она:
//!
//! * сначала **доказывает** полноту источника — перечисляет все файлы,
//!   их размеры и SHA-256 и сворачивает их в один отпечаток содержимого;
//! * отказывает целиком при любой неоднозначности (коллизия имён назначения,
//!   уже занятый или непустой путь назначения, символическая ссылка, нечитаемый
//!   файл, не-UTF-8 имя, экспорт в самом корне прежней области);
//! * переносит экспорт атомарным `rename` в пределах одной файловой системы,
//!   а при её отсутствии — копированием с проверкой отпечатка до публикации;
//! * сверяет отпечаток **после** переноса и не считает работу выполненной,
//!   если он разошёлся;
//! * повторяема: завершённая миграция не находит источников и ничего не делает,
//!   а уже существующее побайтово то же назначение не перезаписывается.
//!
//! `decks/input/` — закрытая область будущего доверенного загрузчика: обход
//! прежней области не открывает её и не спускается в неё.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{
    AtFlags, CWD, Dir, FileType, Mode, OFlags, RenameFlags, mkdirat, open, openat, renameat_with,
    statat, unlinkat,
};
use serde::Serialize;
use unicode_normalization::UnicodeNormalization;

use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::layout;
use crate::loader::DECK_JSON;
use crate::paths::{canonical_ish, paths_alias};
use crate::write::ExportLock;
use asset_store::hashing::sha256_hex;

/// Префикс временного каталога незавершённой копии между файловыми системами.
const TEMP_PREFIX: &str = ".anki-repo-migrate-tmp-";
/// Предел числа путей, перечисляемых в одной диагностике.
const MAX_REPORTED_PATHS: usize = 20;
/// Единственный файл, который прежняя раскладка держала как placeholder Git.
const GITKEEP: &str = ".gitkeep";

/// Отпечаток одного файла экспорта.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileFact {
    /// Путь относительно корня экспорта, с `/` как разделителем.
    relative: String,
    /// Размер в байтах.
    bytes: u64,
    /// SHA-256 содержимого в lowercase hex.
    sha256: String,
}

/// Доказанное содержимое экспорта.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExportManifest {
    files: Vec<FileFact>,
    total_bytes: u64,
    /// SHA-256 канонического текста списка файлов: одно значение на весь экспорт.
    digest: String,
}

/// Причина, по которой экспорт нельзя перенести.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ScanFailure {
    Missing,
    NotDirectory,
    IsSymlink,
    ContainsSymlink(String),
    NonUtf8Name(String),
    Unreadable(String),
    DeckJsonMissing,
}

impl ScanFailure {
    /// Стабильный машиночитаемый подкод причины.
    const fn reason(&self) -> &'static str {
        match self {
            Self::Missing => "source_missing",
            Self::NotDirectory => "source_not_directory",
            Self::IsSymlink => "source_is_symlink",
            Self::ContainsSymlink(_) => "source_contains_symlink",
            Self::NonUtf8Name(_) => "source_non_utf8_name",
            Self::Unreadable(_) => "source_unreadable",
            Self::DeckJsonMissing => "deck_json_missing",
        }
    }

    /// Объяснение для человека.
    fn message(&self, root: &Path) -> String {
        match self {
            Self::Missing => format!("{} исчез во время планирования", root.display()),
            Self::NotDirectory => format!("{} не является каталогом", root.display()),
            Self::IsSymlink => format!(
                "{} является символической ссылкой: переносить ссылку вместо экспорта нельзя",
                root.display()
            ),
            Self::ContainsSymlink(relative) => format!(
                "внутри экспорта есть символическая ссылка {relative}: содержимое нельзя доказать \
                 побайтово"
            ),
            Self::NonUtf8Name(relative) => format!(
                "имя {relative} не является корректным UTF-8: плоское имя назначения невозможно \
                 доказать однозначно"
            ),
            Self::Unreadable(path) => format!("{path} невозможно прочитать"),
            Self::DeckJsonMissing => {
                format!("в {} нет {DECK_JSON}", root.display())
            }
        }
    }
}

/// Одна запланированная или выполненная миграция экспорта.
#[derive(Debug, Serialize)]
pub struct ExportPlan {
    /// Имя корня экспорта: оно же плоское имя назначения.
    pub name: Option<String>,
    /// Прежний путь экспорта относительно корня репозитория.
    pub source: String,
    /// Новый путь экспорта относительно корня репозитория.
    pub target: Option<String>,
    /// `move`, `already_present` или `refused`.
    pub action: &'static str,
    /// `planned`, `applied` или `refused`.
    pub state: &'static str,
    /// Число файлов в экспорте.
    pub files: u64,
    /// Суммарный размер файлов экспорта.
    pub bytes: u64,
    /// Отпечаток содержимого экспорта.
    pub content_sha256: Option<String>,
    /// Стабильный подкод отказа.
    pub reason: Option<&'static str>,
    /// Объяснение отказа или особого исхода.
    pub message: Option<String>,
    #[serde(skip)]
    source_path: PathBuf,
    #[serde(skip)]
    target_path: Option<PathBuf>,
    #[serde(skip)]
    manifest: Option<ExportManifest>,
}

/// Диагностика, не относящаяся к одному экспорту.
#[derive(Debug, Serialize)]
pub struct Diagnostic {
    /// Стабильный код диагностики.
    pub code: &'static str,
    /// Объяснение для человека.
    pub message: String,
    /// Относящиеся к диагностике пути.
    pub paths: Vec<String>,
}

impl Diagnostic {
    fn new(code: &'static str, message: impl Into<String>, paths: Vec<String>) -> Self {
        Self {
            code,
            message: message.into(),
            paths,
        }
    }
}

/// Итоговые счётчики плана.
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    /// Сколько экспортов рассмотрено.
    pub exports: usize,
    /// Сколько запланировано к переносу.
    pub planned: usize,
    /// Сколько перенесено этой командой.
    pub applied: usize,
    /// Сколько уже присутствовало в назначении побайтово.
    pub already_present: usize,
    /// Сколько отвергнуто.
    pub refused: usize,
    /// Суммарное число файлов рассмотренных экспортов.
    pub files: u64,
    /// Суммарный размер рассмотренных экспортов.
    pub bytes: u64,
}

/// Результат `migrate-layout`.
#[derive(Debug, Serialize)]
pub struct MigrationReport {
    /// Корень репозитория.
    pub root: String,
    /// Прежняя область раскладки относительно корня.
    pub legacy_area: String,
    /// Плоское назначение относительно корня.
    pub output_dir: String,
    /// Запрошена ли запись.
    pub apply: bool,
    /// Выполнена ли запись.
    pub applied: bool,
    /// Есть ли хотя бы один отказ.
    pub refused: bool,
    /// Рассмотренные экспорты.
    pub exports: Vec<ExportPlan>,
    /// Удалённые placeholder-файлы и опустевшие каталоги прежней раскладки.
    pub pruned: Vec<String>,
    /// Диагностика.
    pub diagnostics: Vec<Diagnostic>,
    /// Итоговые счётчики.
    pub summary: Summary,
}

impl MigrationReport {
    /// Код возврата команды: отказ не является успехом.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        if self.refused { 3 } else { 0 }
    }
}

/// Планирует и, при `apply`, выполняет миграцию раскладки.
///
/// # Ошибки
///
/// Возвращает [`ErrorCode::Usage`], если корень репозитория не задан и не может
/// быть найден, и [`ErrorCode::InputUnreadable`] или [`ErrorCode::WriteFailed`]
/// при отказе файловой системы. Отказ отдельного экспорта ошибкой не является:
/// он виден в [`MigrationReport::refused`] и в коде возврата.
pub fn migrate(root: Option<&Path>, apply: bool) -> Result<MigrationReport, DomainError> {
    if let Some(path) = root {
        crate::layout::guard_local_path(path, "корень репозитория")?;
    }
    let root = resolve_root(root)?;
    crate::layout::guard_local_path_for_root(&root, &root, "корень репозитория")?;
    let legacy = canonical_ish(&root.join(layout::DECKS_DIR));
    let input = layout::input_dir(&root);
    let output = layout::output_dir(&root);
    validate_output_location(&root, &output)?;

    let mut diagnostics = Vec::new();
    if !legacy.is_dir() {
        diagnostics.push(Diagnostic::new(
            "legacy_area_absent",
            "прежняя область decks/ отсутствует: мигрировать нечего",
            Vec::new(),
        ));
        return Ok(report(
            &root,
            &legacy,
            &output,
            apply,
            Vec::new(),
            Vec::new(),
            diagnostics,
        ));
    }

    let discovery = discover_export_roots(&legacy, &input, &output)?;
    diagnostics.extend(discovery.diagnostics);

    let mut entries: Vec<ExportPlan> = discovery
        .roots
        .iter()
        .map(|source| plan_entry(source, &output))
        .collect();

    if legacy.join(DECK_JSON).is_file() {
        entries.push(refused_entry(
            &legacy,
            "legacy_area_is_export",
            format!(
                "{} сам содержит {DECK_JSON}: переносить корень прежней области вместе с decks/input \
                 и decks/output нельзя",
                legacy.display()
            ),
        ));
    }

    if entries.is_empty() {
        diagnostics.push(Diagnostic::new(
            "nothing_to_migrate",
            format!(
                "в {} не найдено ни одного CrowdAnki-экспорта ({DECK_JSON})",
                legacy.display()
            ),
            Vec::new(),
        ));
    }

    mark_name_collisions(&mut entries);
    mark_existing_target_collisions(&mut entries, &output)?;
    mark_target_state(&mut entries);

    let mut pruned = Vec::new();
    if apply && !entries.is_empty() && entries.iter().all(|entry| entry.state != "refused") {
        let (removed, retained) = write_plan(&root, &legacy, &input, &output, &mut entries)?;
        pruned = removed;
        if let Some(diagnostic) = retained {
            diagnostics.push(diagnostic);
        }
    }

    Ok(report(
        &root,
        &legacy,
        &output,
        apply,
        entries,
        pruned,
        diagnostics,
    ))
}

/// Корень репозитория: явный или найденный от текущего каталога.
fn resolve_root(explicit: Option<&Path>) -> Result<PathBuf, DomainError> {
    if let Some(path) = explicit {
        let canonical = canonical_ish(path);
        if !canonical.is_dir() {
            return Err(DomainError::with_details(
                ErrorCode::InputUnreadable,
                format!("корень репозитория {} недоступен", path.display()),
                details! {
                    "path" => path.display().to_string(),
                },
            ));
        }
        return Ok(canonical);
    }

    let current = std::env::current_dir().map_err(|error| {
        DomainError::with_details(
            ErrorCode::Usage,
            format!("не удалось определить текущий каталог: {error}"),
            details! { "io_error" => error.to_string() },
        )
    })?;

    layout::repository_root_for(&current).ok_or_else(|| {
        DomainError::new(
            ErrorCode::Usage,
            "не удалось определить корень репозитория: укажите --root",
        )
    })
}

/// Собирает отчёт из рассмотренных экспортов.
fn report(
    root: &Path,
    legacy: &Path,
    output: &Path,
    apply: bool,
    mut exports: Vec<ExportPlan>,
    pruned: Vec<String>,
    mut diagnostics: Vec<Diagnostic>,
) -> MigrationReport {
    // Отчёт покидает процесс и может попасть в PR, поэтому абсолютные локальные
    // пути пользователя в нём не остаются: и поля-пути, и построенные из них
    // тексты приводятся к виду относительно корня репозитория.
    for entry in &mut exports {
        entry.source = relativize_paths(root, &entry.source);
        entry.target = entry
            .target
            .as_deref()
            .map(|path| relativize_paths(root, path));
        entry.message = entry
            .message
            .as_deref()
            .map(|text| relativize_paths(root, text));
    }
    for diagnostic in &mut diagnostics {
        diagnostic.message = relativize_paths(root, &diagnostic.message);
        for path in &mut diagnostic.paths {
            *path = relativize_paths(root, path);
        }
    }

    let mut summary = Summary {
        exports: exports.len(),
        ..Summary::default()
    };
    for entry in &exports {
        match entry.action {
            "move" if entry.state == "planned" => summary.planned += 1,
            "already_present" => summary.already_present += 1,
            "refused" => summary.refused += 1,
            _ => {}
        }
        if entry.state == "applied" {
            summary.applied += 1;
        }
        summary.files += entry.files;
        summary.bytes += entry.bytes;
    }
    let refused = summary.refused > 0;
    let applied = exports.iter().any(|entry| entry.state == "applied");
    MigrationReport {
        root: root.display().to_string(),
        legacy_area: display_relative(root, legacy),
        output_dir: display_relative(root, output),
        apply,
        applied,
        refused,
        exports,
        pruned,
        diagnostics,
        summary,
    }
}

/// Найденные корни экспортов и диагностика обхода.
struct Discovery {
    roots: Vec<PathBuf>,
    diagnostics: Vec<Diagnostic>,
}

/// Обходит прежнюю область и находит каталоги с `deck.json`.
///
/// Обход не заходит в `decks/input/` и `decks/output/` и не следует по
/// символическим ссылкам: ссылка, ведущая к экспорту, попадает в план как
/// отказ, а прочие ссылки только отмечаются диагностикой.
fn discover_export_roots(
    legacy: &Path,
    input: &Path,
    output: &Path,
) -> Result<Discovery, DomainError> {
    let mut roots = Vec::new();
    let mut diagnostics = Vec::new();
    let mut stack = vec![legacy.to_path_buf()];

    while let Some(directory) = stack.pop() {
        for path in read_dir_sorted(&directory)? {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| read_error(&path, "read_source", &error))?;
            let file_type = metadata.file_type();

            // `decks/input` и `decks/output` не просматриваются в любом виде:
            // проверка ссылки раньше этой ветки заглянула бы внутрь закрытой
            // области и превратила бы её в кандидата на перенос.
            if paths_alias(&path, input) || paths_alias(&path, output) {
                continue;
            }
            if file_type.is_symlink() {
                if path.join(DECK_JSON).is_file() {
                    roots.push(path);
                } else {
                    diagnostics.push(Diagnostic::new(
                        "symlink_skipped",
                        format!(
                            "символическая ссылка {} не просматривается и не переносится",
                            path.display()
                        ),
                        vec![path.display().to_string()],
                    ));
                }
                continue;
            }
            if !file_type.is_dir() {
                continue;
            }
            if path.join(DECK_JSON).is_file() {
                roots.push(path);
                continue;
            }
            stack.push(path);
        }
    }

    roots.sort();
    Ok(Discovery { roots, diagnostics })
}

/// Читает каталог в детерминированном порядке.
fn read_dir_sorted(directory: &Path) -> Result<Vec<PathBuf>, DomainError> {
    let mut paths = Vec::new();
    for entry in
        fs::read_dir(directory).map_err(|error| read_error(directory, "read_dir", &error))?
    {
        let entry = entry.map_err(|error| read_error(directory, "read_dir", &error))?;
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

/// Готовит план переноса одного экспорта.
fn plan_entry(source: &Path, output: &Path) -> ExportPlan {
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned);

    let mut entry = ExportPlan {
        name: name.clone(),
        source: source.display().to_string(),
        target: None,
        action: "refused",
        state: "refused",
        files: 0,
        bytes: 0,
        content_sha256: None,
        reason: None,
        message: None,
        source_path: source.to_path_buf(),
        target_path: None,
        manifest: None,
    };

    let Some(name) = name else {
        // Тот же код, что и у не-UTF-8 имени внутри экспорта: понятие одно, и
        // плоское имя назначения по нему доказать нельзя.
        return refused_entry(
            source,
            "source_non_utf8_name",
            "имя корня экспорта не является UTF-8",
        );
    };

    let target = output.join(&name);
    entry.target = Some(target.display().to_string());
    entry.target_path = Some(target);

    match scan_export(source) {
        Err(failure) => {
            entry.reason = Some(failure.reason());
            entry.message = Some(failure.message(source));
            entry
        }
        Ok(manifest) => {
            entry.files = manifest.files.len() as u64;
            entry.bytes = manifest.total_bytes;
            entry.content_sha256 = Some(manifest.digest.clone());
            entry.manifest = Some(manifest);
            entry.action = "move";
            entry.state = "planned";
            entry
        }
    }
}

/// Готовит запись об отказе без разобранного содержимого.
fn refused_entry(source: &Path, reason: &'static str, message: impl Into<String>) -> ExportPlan {
    ExportPlan {
        name: source
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned),
        source: source.display().to_string(),
        target: None,
        action: "refused",
        state: "refused",
        files: 0,
        bytes: 0,
        content_sha256: None,
        reason: Some(reason),
        message: Some(message.into()),
        source_path: source.to_path_buf(),
        target_path: None,
        manifest: None,
    }
}

/// Отмечает отказ на уже существующей записи плана.
fn refuse(entry: &mut ExportPlan, reason: &'static str, message: impl Into<String>) {
    entry.action = "refused";
    entry.state = "refused";
    entry.reason = Some(reason);
    entry.message = Some(message.into());
}

/// Ключ сравнения имён назначения: регистр и Unicode-нормализация не различаются.
fn collision_key(name: &str) -> String {
    name.nfc().collect::<String>().to_lowercase()
}

/// Отмечает экспорты, чьи плоские имена назначения сталкиваются.
fn mark_name_collisions(entries: &mut [ExportPlan]) {
    let mut by_key: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.state == "refused" {
            continue;
        }
        if let Some(name) = entry.name.as_deref() {
            by_key.entry(collision_key(name)).or_default().push(index);
        }
    }

    for indexes in by_key.values().filter(|indexes| indexes.len() > 1) {
        let mut names: Vec<String> = indexes
            .iter()
            .filter_map(|index| entries[*index].name.clone())
            .collect();
        names.sort();
        let message = format!(
            "плоские имена назначения совпадают с точностью до регистра и Unicode-нормализации: {}",
            names.join(", ")
        );
        for index in indexes {
            refuse(
                &mut entries[*index],
                "target_name_collision",
                message.clone(),
            );
        }
    }
}

/// Отмечает источники, чьё плоское имя совпадает с уже лежащим в назначении.
///
/// Сравнение не различает регистр и Unicode-нормализацию, как и у имён двух
/// источников: иначе рядом с существующим `WORDS__a` появился бы второй каталог
/// `Words__A` с той же колодой. Точное совпадение имени разбирает
/// [`mark_target_state`]: назначение может оказаться побайтовым дубликатом.
fn mark_existing_target_collisions(
    entries: &mut [ExportPlan],
    output: &Path,
) -> Result<(), DomainError> {
    let existing = match fs::read_dir(output) {
        Ok(existing) => existing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(read_error(output, "read_output", &error)),
    };
    let mut by_key: BTreeMap<String, String> = BTreeMap::new();
    for entry in existing {
        let entry = entry.map_err(|error| read_error(output, "read_output", &error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        by_key.insert(collision_key(name), name.to_owned());
    }

    for entry in entries.iter_mut().filter(|entry| entry.action == "move") {
        let Some(name) = entry.name.as_deref() else {
            continue;
        };
        let Some(existing_name) = by_key.get(&collision_key(name)) else {
            continue;
        };
        if existing_name == name {
            continue;
        }
        refuse(
            entry,
            "target_name_collision",
            format!(
                "плоское имя назначения {name} совпадает с уже существующим {existing_name} с \
                 точностью до регистра и Unicode-нормализации"
            ),
        );
    }
    Ok(())
}

/// Отмечает состояние уже существующего назначения.
fn mark_target_state(entries: &mut [ExportPlan]) {
    for entry in entries.iter_mut().filter(|entry| entry.action == "move") {
        let Some(target) = entry.target_path.clone() else {
            continue;
        };
        let metadata = match fs::symlink_metadata(&target) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                refuse(
                    entry,
                    "target_unreadable",
                    format!("{} недоступен: {error}", target.display()),
                );
                continue;
            }
        };

        if metadata.file_type().is_symlink() {
            refuse(
                entry,
                "target_is_symlink",
                format!(
                    "{} является символической ссылкой: писать в цель ссылки нельзя",
                    target.display()
                ),
            );
            continue;
        }

        if !metadata.is_dir() {
            refuse(
                entry,
                "target_conflict",
                format!("{} существует и не является каталогом", target.display()),
            );
            continue;
        }

        let expected = entry
            .manifest
            .as_ref()
            .map(|manifest| manifest.digest.clone());
        match scan_export(&target) {
            Ok(existing) if Some(existing.digest.as_str()) == expected.as_deref() => {
                entry.action = "already_present";
                entry.message = Some(format!(
                    "{} уже содержит побайтово тот же экспорт",
                    target.display()
                ));
            }
            _ => refuse(
                entry,
                "target_conflict",
                format!(
                    "{} уже существует и отличается от источника: перезапись запрещена",
                    target.display()
                ),
            ),
        }
    }
}

/// Выполняет запись плана: перенос экспортов и снятие placeholder-файлов.
///
/// Блокировка прежней области удерживается на всю запись: и перенос, и снятие
/// placeholder-файлов меняют одну и ту же область, и вторая миграция не должна
/// вклиниться между ними.
fn write_plan(
    root: &Path,
    legacy: &Path,
    input: &Path,
    output: &Path,
    entries: &mut [ExportPlan],
) -> Result<(Vec<String>, Option<Diagnostic>), DomainError> {
    let _lock = ExportLock::acquire(legacy)?;
    apply_entries(root, output, entries)?;
    if entries.iter().any(|entry| entry.state == "refused") {
        // Отказ, обнаруженный уже при записи (`target_appeared`), отменяет и
        // снятие placeholder-файлов: отвергнутый план не пишет ничего.
        return Ok((Vec::new(), None));
    }
    let (removed, retained) = prune_legacy_placeholders(legacy, input, output)?;
    let pruned = removed
        .iter()
        .map(|path| display_relative(root, path))
        .collect();
    Ok((pruned, retained))
}

/// Проверяет, что назначение раскладки остаётся обычным каталогом `decks/output`.
///
/// Проверка выполняется до чтения прежней области и повторяется перед записью:
/// канонический путь назначения не должен совпасть с `decks/input`, а ни один
/// компонент пути раскладки не должен перенаправлять миграцию через symlink.
fn validate_output_location(root: &Path, output: &Path) -> Result<(), DomainError> {
    let requested = root.join(layout::DECKS_DIR).join(layout::OUTPUT_DIR);
    layout::guard_local_path_for_root(&requested, root, "каталог назначения")?;
    layout::guard_local_path_for_root(output, root, "каталог назначения")?;

    let current_output = layout::output_dir(root);
    if current_output != output {
        return Err(layout_path_error(
            &requested,
            "layout_destination_changed",
            "путь каталога назначения изменился во время планирования",
        ));
    }

    let input = layout::input_dir(root);
    if current_output.starts_with(&input)
        || input.starts_with(&current_output)
        || current_output == input
    {
        return Err(input_area_error(&requested, root));
    }

    let decks = root.join(layout::DECKS_DIR);
    match fs::symlink_metadata(&decks) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(layout_path_error(
                &decks,
                "layout_path_is_symlink",
                "каталог decks перенаправлен символической ссылкой; миграция отказана",
            ));
        }
        Ok(metadata) if !metadata.is_dir() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(&decks, "inspect_layout_path", &error)),
    }

    let output_path = decks.join(layout::OUTPUT_DIR);
    match fs::symlink_metadata(&output_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(layout_path_error(
                &output_path,
                "layout_path_is_symlink",
                "каталог назначения перенаправлен символической ссылкой; миграция отказана",
            ));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(layout_path_error(
                &output_path,
                "layout_destination_not_directory",
                "каталог назначения существует, но не является каталогом",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(&output_path, "inspect_layout_path", &error)),
    }
    Ok(())
}

fn input_area_error(path: &Path, root: &Path) -> DomainError {
    DomainError::with_details(
        ErrorCode::InvalidRequest,
        format!(
            "каталог назначения {} пересекает закрытую область decks/input; миграция отказана",
            path.display()
        ),
        details! {
            "path" => path.display().to_string(),
            "repository_root" => root.display().to_string(),
            "area" => "decks/input",
            "reason" => "input_area_forbidden",
        },
    )
}

fn layout_path_error(path: &Path, reason: &str, message: &str) -> DomainError {
    DomainError::with_details(
        ErrorCode::InvalidRequest,
        format!("{message}: {}", path.display()),
        details! {
            "path" => path.display().to_string(),
            "reason" => reason,
        },
    )
}

/// Открывает `decks/output` без перехода по symlink на любом компоненте.
fn open_output_directory(root: &Path, output: &Path, create: bool) -> Result<File, DomainError> {
    validate_output_location(root, output)?;
    let root_directory = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error(root, "open_repository_root", &io::Error::from(error)))?;
    let decks_directory = openat(
        &root_directory,
        layout::DECKS_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| {
        io_error(
            &root.join(layout::DECKS_DIR),
            "open_decks_directory",
            &io::Error::from(error),
        )
    })?;

    if create {
        match mkdirat(
            &decks_directory,
            layout::OUTPUT_DIR,
            Mode::from_raw_mode(0o755),
        ) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::EXIST => {}
            Err(error) => {
                return Err(io_error(
                    output,
                    "create_output_dir",
                    &io::Error::from(error),
                ));
            }
        }
    }

    openat(
        &decks_directory,
        layout::OUTPUT_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error(output, "open_output_dir", &io::Error::from(error)))
}

/// Выполняет план записи: переносит экспорты, не снимая placeholder-файлы.
fn apply_entries(
    root: &Path,
    output: &Path,
    entries: &mut [ExportPlan],
) -> Result<(), DomainError> {
    validate_output_location(root, output)?;
    for entry in entries.iter_mut().filter(|entry| entry.action == "move") {
        if let Some(target) = entry.target_path.clone()
            && fs::symlink_metadata(&target).is_ok()
        {
            refuse(
                entry,
                "target_appeared",
                format!(
                    "{} появился между планированием и записью: перезапись запрещена",
                    target.display()
                ),
            );
        }
    }
    // Отказ разбирается до первой записи: отвергнутый план не создаёт даже
    // пустой каталог назначения.
    if entries.iter().any(|entry| entry.state == "refused") {
        return Ok(());
    }

    let output_directory = open_output_directory(root, output, true)?;

    let needed = bytes_to_copy(entries, &output_directory)?;
    let available = available_bytes(&output_directory, output)?;
    if available < needed {
        return Err(DomainError::with_details(
            ErrorCode::WriteFailed,
            format!(
                "недостаточно места для миграции: требуется {needed} байт, доступно {available}"
            ),
            details! {
                "required_bytes" => needed,
                "available_bytes" => available,
                "output_dir" => output.display().to_string(),
            },
        ));
    }

    for entry in entries.iter_mut() {
        match entry.action {
            "move" => move_export(root, output, entry)?,
            "already_present" => remove_duplicate_source(entry)?,
            _ => {}
        }
    }
    Ok(())
}

/// Переносит экспорт в назначение и подтверждает отпечаток.
fn move_export(root: &Path, output: &Path, entry: &mut ExportPlan) -> Result<(), DomainError> {
    let source = entry.source_path.clone();
    let target = entry
        .target_path
        .clone()
        .expect("план переноса всегда имеет назначение");
    let expected = entry
        .manifest
        .as_ref()
        .map(|manifest| manifest.digest.clone());

    // План строится до захвата блокировки, поэтому источник перепроверяется
    // непосредственно перед переносом. Расхождение — отказ до изменения
    // раскладки: иначе перенос уже состоялся бы, а команда сообщила бы об
    // ошибке при фактически выполненной работе.
    let current = scan_export(&source)
        .map_err(|failure| verification_error(&source, &failure.message(&source)))?;
    if Some(&current.digest) != expected.as_ref() {
        return Err(verification_error(
            &source,
            "источник изменился между планированием и переносом: перенос отменён",
        ));
    }

    validate_output_location(root, output)?;
    let output_directory = open_output_directory(root, output, false)?;
    let target_name = target
        .file_name()
        .ok_or_else(|| verification_error(&target, "у назначения нет имени каталога"))?;
    match renameat_with(
        CWD,
        &source,
        &output_directory,
        target_name,
        RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::EXIST => {
            refuse(
                entry,
                "target_appeared",
                format!(
                    "{} появился перед атомарной публикацией: перезапись запрещена",
                    target.display()
                ),
            );
            return Ok(());
        }
        Err(error) if io::Error::from(error).kind() == io::ErrorKind::CrossesDevices => {
            if !copy_export(root, output, &source, &target, expected.as_deref())? {
                refuse(
                    entry,
                    "target_appeared",
                    format!(
                        "{} появился перед атомарной публикацией копии: перезапись запрещена",
                        target.display()
                    ),
                );
                return Ok(());
            }
        }
        Err(error) => {
            return Err(io_error(&source, "rename_export", &io::Error::from(error)));
        }
    }

    let actual = scan_export(&target)
        .map_err(|failure| verification_error(&target, &failure.message(&target)))?;
    if Some(&actual.digest) != expected.as_ref() {
        return Err(verification_error(
            &target,
            "отпечаток содержимого после переноса разошёлся с доказанным до переноса",
        ));
    }

    entry.state = "applied";
    Ok(())
}

/// Копирует экспорт в назначение, проверяя отпечаток до публикации.
fn copy_export(
    root: &Path,
    output: &Path,
    source: &Path,
    target: &Path,
    expected: Option<&str>,
) -> Result<bool, DomainError> {
    let temporary_name = format!("{TEMP_PREFIX}{}-{}", std::process::id(), unique_suffix());
    let temporary = output.join(&temporary_name);
    let target_name = target
        .file_name()
        .ok_or_else(|| verification_error(target, "у назначения нет имени каталога"))?;
    let output_directory = open_output_directory(root, output, false)?;
    let source_directory = open(
        source,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| io_error(source, "open_copy_source", &io::Error::from(error)))?;
    mkdirat(
        &output_directory,
        temporary_name.as_str(),
        Mode::from_raw_mode(0o755),
    )
    .map_err(|error| io_error(&temporary, "create_copy_stage", &io::Error::from(error)))?;
    let temporary_directory = match openat(
        &output_directory,
        temporary_name.as_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(directory) => File::from(directory),
        Err(error) => {
            let _ = remove_tree_at(&output_directory, OsStr::new(&temporary_name));
            return Err(io_error(
                &temporary,
                "open_copy_stage",
                &io::Error::from(error),
            ));
        }
    };

    let outcome = (|| -> Result<CopyPublish, DomainError> {
        validate_output_location(root, output)?;
        copy_tree_contents(source, &source_directory, &temporary_directory)?;
        let copied = scan_export(&temporary)
            .map_err(|failure| verification_error(&temporary, &failure.message(&temporary)))?;
        if Some(copied.digest.as_str()) != expected {
            return Err(verification_error(
                &temporary,
                "скопированный экспорт не совпал с доказанным отпечатком источника",
            ));
        }
        validate_output_location(root, output)?;
        let current_output_directory = open_output_directory(root, output, false)?;
        match renameat_with(
            &current_output_directory,
            temporary_name.as_str(),
            &current_output_directory,
            target_name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::EXIST => {
                return Ok(CopyPublish::TargetAppeared);
            }
            Err(error) => {
                return Err(io_error(target, "publish_copy", &io::Error::from(error)));
            }
        }
        fs::remove_dir_all(source).map_err(|error| io_error(source, "remove_source", &error))?;
        Ok(CopyPublish::Published)
    })();

    match outcome {
        Ok(CopyPublish::Published) => Ok(true),
        Ok(CopyPublish::TargetAppeared) => {
            let _ = remove_tree_at(&output_directory, OsStr::new(&temporary_name));
            Ok(false)
        }
        Err(error) => {
            let _ = remove_tree_at(&output_directory, OsStr::new(&temporary_name));
            Err(error)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CopyPublish {
    Published,
    TargetAppeared,
}

/// Копирует файлы между открытыми каталогами, не следуя по символическим ссылкам.
fn copy_tree_contents(
    source_path: &Path,
    source_directory: &File,
    target_directory: &File,
) -> Result<(), DomainError> {
    for name in directory_names(source_directory)
        .map_err(|error| read_error(source_path, "read_source", &error))?
    {
        let path = source_path.join(&name);
        let stat = statat(source_directory, &name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| read_error(&path, "read_source", &io::Error::from(error)))?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => {
                let source_child = openat(
                    source_directory,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(|error| {
                    read_error(&path, "open_source_directory", &io::Error::from(error))
                })?;
                mkdirat(target_directory, &name, Mode::from_raw_mode(0o755)).map_err(|error| {
                    io_error(&path, "create_copy_directory", &io::Error::from(error))
                })?;
                let target_child = openat(
                    target_directory,
                    &name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(|error| io_error(&path, "open_copy_directory", &io::Error::from(error)))?;
                copy_tree_contents(&path, &source_child, &target_child)?;
            }
            FileType::RegularFile => {
                let mut source_file = openat(
                    source_directory,
                    &name,
                    OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                    Mode::empty(),
                )
                .map(File::from)
                .map_err(|error| read_error(&path, "open_source_file", &io::Error::from(error)))?;
                if !source_file
                    .metadata()
                    .map_err(|error| read_error(&path, "read_source", &error))?
                    .is_file()
                {
                    return Err(verification_error(
                        &path,
                        "источник перестал быть обычным файлом",
                    ));
                }
                let mut target_file = openat(
                    target_directory,
                    &name,
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::CLOEXEC
                        | OFlags::NOFOLLOW,
                    Mode::from_raw_mode(stat.st_mode & 0o7777),
                )
                .map(File::from)
                .map_err(|error| io_error(&path, "create_copy_file", &io::Error::from(error)))?;
                io::copy(&mut source_file, &mut target_file)
                    .map_err(|error| io_error(&path, "copy_file", &error))?;
                target_file
                    .sync_data()
                    .map_err(|error| io_error(&path, "sync_copy_file", &error))?;
            }
            FileType::Symlink => {
                return Err(verification_error(
                    &path,
                    "источник содержит символическую ссылку",
                ));
            }
            _ => {
                return Err(verification_error(
                    &path,
                    "источник содержит не обычный файл",
                ));
            }
        }
    }
    Ok(())
}

fn directory_names(directory: &File) -> io::Result<Vec<std::ffi::OsString>> {
    let mut names = Vec::new();
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(OsStr::from_bytes(bytes).to_os_string());
        }
    }
    names.sort();
    Ok(names)
}

fn remove_tree_at(parent: &File, name: &OsStr) -> io::Result<()> {
    let directory = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    let directory = File::from(directory);
    for child in directory_names(&directory)? {
        let stat = statat(&directory, &child, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            remove_tree_at(&directory, &child)?;
        } else {
            unlinkat(&directory, &child, AtFlags::empty())?;
        }
    }
    unlinkat(parent, name, AtFlags::REMOVEDIR).map_err(io::Error::from)
}

/// Убирает источник, побайтово совпадающий с уже существующим назначением.
fn remove_duplicate_source(entry: &mut ExportPlan) -> Result<(), DomainError> {
    let source = entry.source_path.clone();
    let target = entry
        .target_path
        .clone()
        .expect("дубликат всегда имеет назначение");

    let actual_source = scan_export(&source)
        .map_err(|failure| verification_error(&source, &failure.message(&source)))?;
    let actual_target = scan_export(&target)
        .map_err(|failure| verification_error(&target, &failure.message(&target)))?;
    if actual_source.digest != actual_target.digest {
        return Err(verification_error(
            &source,
            "источник перестал совпадать с назначением: удаление отменено",
        ));
    }

    fs::remove_dir_all(&source).map_err(|error| io_error(&source, "remove_duplicate", &error))?;
    entry.state = "applied";
    entry.message = Some(format!(
        "{} удалён как побайтовый дубликат уже существующего назначения",
        source.display()
    ));
    Ok(())
}

/// Снимает placeholder-файлы и опустевшие каталоги прежней раскладки.
///
/// Удаляются только пустые каталоги и нулевые `.gitkeep`: любой другой файл или
/// ссылка означают, что прежняя область ещё содержит данные, и она остаётся как есть.
fn prune_legacy_placeholders(
    legacy: &Path,
    input: &Path,
    output: &Path,
) -> Result<(Vec<PathBuf>, Option<Diagnostic>), DomainError> {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut others = Vec::new();
    let mut stack = vec![legacy.to_path_buf()];

    while let Some(directory) = stack.pop() {
        for path in read_dir_sorted(&directory)? {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| read_error(&path, "read_legacy", &error))?;
            let file_type = metadata.file_type();
            // Закрытые области не считаются содержимым прежней раскладки ни в
            // каком виде, в том числе ссылкой: иначе `decks/input` удерживал бы
            // прежнюю область как «посторонний элемент».
            if paths_alias(&path, input) || paths_alias(&path, output) {
                continue;
            }
            if file_type.is_symlink() {
                others.push(path);
                continue;
            }
            if file_type.is_dir() {
                directories.push(path.clone());
                stack.push(path);
                continue;
            }
            if path.file_name().is_some_and(|name| name == GITKEEP) && metadata.len() == 0 {
                files.push(path);
            } else {
                others.push(path);
            }
        }
    }

    if !others.is_empty() {
        others.sort();
        let mut reported: Vec<String> = others
            .iter()
            .take(MAX_REPORTED_PATHS)
            .map(|path| path.display().to_string())
            .collect();
        if others.len() > MAX_REPORTED_PATHS {
            reported.push(format!("… ещё {}", others.len() - MAX_REPORTED_PATHS));
        }
        return Ok((
            Vec::new(),
            Some(Diagnostic::new(
                "legacy_area_retained",
                format!(
                    "прежняя область сохранена: в ней осталось {} посторонних элементов",
                    others.len()
                ),
                reported,
            )),
        ));
    }

    let mut removed = Vec::new();
    for path in files {
        fs::remove_file(&path).map_err(|error| io_error(&path, "remove_placeholder", &error))?;
        removed.push(path);
    }

    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in directories {
        if fs::remove_dir(&path).is_ok() {
            removed.push(path);
        }
    }
    Ok((removed, None))
}

/// Объём, который действительно будет скопирован в назначение.
///
/// Перенос в пределах одной файловой системы выполняется одним `rename` и места
/// не требует, а уже существующее назначение только снимает источник. Поэтому
/// свободное место считается только по тем записям, которые пройдут через
/// [`copy_export`] при `CrossesDevices`, а не по полному объёму миграции.
fn bytes_to_copy(entries: &[ExportPlan], output: &File) -> Result<u64, DomainError> {
    let output_device = output
        .metadata()
        .map_err(|error| io_error(Path::new("decks/output"), "read_metadata", &error))?
        .dev();
    let mut needed = 0_u64;
    for entry in entries.iter().filter(|entry| entry.action == "move") {
        if device_of(&entry.source_path)? != output_device {
            needed = needed.saturating_add(entry.bytes);
        }
    }
    Ok(needed)
}

/// Устройство файловой системы, на которой лежит путь.
fn device_of(path: &Path) -> Result<u64, DomainError> {
    let metadata = fs::metadata(path).map_err(|error| read_error(path, "read_metadata", &error))?;
    Ok(metadata.dev())
}

/// Свободное место на файловой системе каталога.
fn available_bytes(directory: &File, path: &Path) -> Result<u64, DomainError> {
    let stat = rustix::fs::fstatvfs(directory).map_err(|error| {
        DomainError::with_details(
            ErrorCode::WriteFailed,
            format!(
                "не удалось измерить свободное место в {}: {error}",
                path.display()
            ),
            details! {
                "path" => path.display().to_string(),
                "io_error" => error.to_string(),
            },
        )
    })?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// Уникальный суффикс временного каталога.
fn unique_suffix() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

/// Считает полное доказанное содержимое экспорта.
fn scan_export(root: &Path) -> Result<ExportManifest, ScanFailure> {
    let metadata = fs::symlink_metadata(root).map_err(|error| match error.kind() {
        io::ErrorKind::NotFound => ScanFailure::Missing,
        _ => ScanFailure::Unreadable(root.display().to_string()),
    })?;
    if metadata.file_type().is_symlink() {
        return Err(ScanFailure::IsSymlink);
    }
    if !metadata.is_dir() {
        return Err(ScanFailure::NotDirectory);
    }
    if !root.join(DECK_JSON).is_file() {
        return Err(ScanFailure::DeckJsonMissing);
    }

    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory)
            .map_err(|_| ScanFailure::Unreadable(directory.display().to_string()))?;
        for entry in entries {
            let entry =
                entry.map_err(|_| ScanFailure::Unreadable(directory.display().to_string()))?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|_| ScanFailure::Unreadable(path.display().to_string()))?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(ScanFailure::ContainsSymlink(relative_name(root, &path)));
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                return Err(ScanFailure::Unreadable(path.display().to_string()));
            }
            let relative = relative_name(root, &path);
            if relative.starts_with('/') {
                return Err(ScanFailure::NonUtf8Name(relative));
            }
            let bytes =
                fs::read(&path).map_err(|_| ScanFailure::Unreadable(path.display().to_string()))?;
            files.push(FileFact {
                relative,
                bytes: bytes.len() as u64,
                sha256: sha256_hex(&bytes),
            });
        }
    }

    files.sort_by(|left, right| left.relative.cmp(&right.relative));
    let total_bytes = files.iter().map(|file| file.bytes).sum();
    let digest = manifest_digest(&files);
    Ok(ExportManifest {
        files,
        total_bytes,
        digest,
    })
}

/// Отпечаток всего экспорта по отсортированному списку файлов.
fn manifest_digest(files: &[FileFact]) -> String {
    let mut text = String::new();
    for file in files {
        let _ = writeln!(text, "{}\0{}\0{}", file.relative, file.bytes, file.sha256);
    }
    sha256_hex(text.as_bytes())
}

/// Путь относительно корня экспорта с `/` как разделителем.
///
/// Для имени, которое не является корректным UTF-8, возвращается строка с
/// ведущим `/`: это не может совпасть с настоящим относительным путём и
/// распознаётся вызывающим кодом как недоказуемое имя.
fn relative_name(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut parts = Vec::new();
    for component in relative.components() {
        match component.as_os_str().to_str() {
            Some(name) => parts.push(name.to_string()),
            None => return format!("/{}", path.display()),
        }
    }
    parts.join("/")
}

/// Путь относительно корня репозитория, если это возможно.
fn display_relative(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if !relative.as_os_str().is_empty() => relative.display().to_string(),
        _ => path.display().to_string(),
    }
}

/// Убирает абсолютный префикс корня репозитория из пути или текста отчёта.
fn relativize_paths(root: &Path, text: &str) -> String {
    let prefix = format!("{}/", root.display());
    text.replace(&prefix, "")
}

/// Готовит доменную ошибку отказа файловой системы.
fn io_error(path: &Path, operation: &str, error: &io::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::WriteFailed,
        format!("{}: {operation}: {error}", path.display()),
        details! {
            "path" => path.display().to_string(),
            "operation" => operation,
            "io_error" => error.to_string(),
        },
    )
}

/// Готовит доменную ошибку чтения входа: обход ничего не пишет, и называть такой
/// отказ `write_failed` нельзя.
fn read_error(path: &Path, operation: &str, error: &io::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::InputUnreadable,
        format!("{}: {operation}: {error}", path.display()),
        details! {
            "path" => path.display().to_string(),
            "operation" => operation,
            "io_error" => error.to_string(),
        },
    )
}

/// Готовит ошибку расхождения доказанного содержимого.
fn verification_error(path: &Path, message: &str) -> DomainError {
    DomainError::with_details(
        ErrorCode::SourceChanged,
        format!("{}: {message}", path.display()),
        details! {
            "path" => path.display().to_string(),
            "reason" => "migration_verification_failed",
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    use crate::test_support::TempDir;

    /// Постоянный `deck.json`: два экспорта с одинаковым содержимым должны
    /// давать один отпечаток независимо от их расположения.
    const DECK_JSON_BODY: &str = r#"{"__type__":"Deck","name":"Демонстрационная колода"}"#;

    /// Готовит репозиторий с признаком `.git` и пустым `decks/`.
    fn repository(label: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new(label);
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("признак репозитория");
        fs::create_dir_all(root.join("decks")).expect("каталог decks");
        (dir, root)
    }

    /// Создаёт синтетический экспорт по относительному пути.
    fn write_export(root: &Path, relative: &str, payload: &[u8]) -> PathBuf {
        let directory = root.join(relative);
        fs::create_dir_all(directory.join("media")).expect("каталог экспорта");
        fs::write(directory.join(DECK_JSON), DECK_JSON_BODY.as_bytes()).expect("deck.json");
        fs::write(directory.join("media").join("a.bin"), payload).expect("media");
        directory
    }

    #[test]
    fn dry_run_reports_the_plan_without_touching_anything() {
        let (_dir, root) = repository("layout-dry-run");
        let payload = vec![7_u8; 4096];
        write_export(&root, "decks/japanese/words/Words__A", &payload);
        write_export(&root, "decks/japanese/words/Words__B", b"b");

        let report = migrate(Some(&root), false).expect("план");

        assert_eq!(report.summary.planned, 2);
        assert!(!report.applied);
        assert!(!report.refused);
        assert_eq!(report.exit_code(), 0);
        assert!(
            root.join("decks/japanese/words/Words__A/deck.json")
                .is_file(),
            "dry-run не меняет раскладку"
        );
        assert!(
            !root.join("decks/output").exists(),
            "назначение не создаётся"
        );
        for entry in &report.exports {
            assert_eq!(entry.action, "move");
            assert_eq!(entry.state, "planned");
            assert_eq!(entry.files, 2);
            assert_eq!(
                entry.content_sha256.as_deref().map(str::len),
                Some(64),
                "отпечаток содержимого — sha256 в hex"
            );
        }
    }

    #[test]
    fn apply_moves_exports_flat_and_preserves_bytes() {
        let (_dir, root) = repository("layout-apply");
        let payload = vec![7_u8; 4096];
        write_export(&root, "decks/japanese/words/Words__A", &payload);
        write_export(&root, "decks/other/Words__B", b"b");
        fs::write(root.join("decks/japanese/words").join(GITKEEP), b"").expect("placeholder");

        let report = migrate(Some(&root), true).expect("применение");

        assert!(report.applied);
        assert_eq!(report.summary.applied, 2);
        assert_eq!(report.summary.refused, 0);
        assert!(!root.join("decks/japanese/words/Words__A").exists());
        assert_eq!(
            fs::read(root.join("decks/output/Words__A/media/a.bin")).expect("перенесённый media"),
            payload,
            "байты сохранены"
        );
        assert!(root.join("decks/output/Words__B/deck.json").is_file());
        assert!(
            !root.join("decks/japanese").exists(),
            "placeholder и опустевшие каталоги прежней раскладки сняты"
        );
        assert!(
            report.pruned.iter().any(|path| path.ends_with(GITKEEP)),
            "снятый placeholder должен быть виден в отчёте"
        );
    }

    #[test]
    fn a_second_run_is_a_no_op() {
        let (_dir, root) = repository("layout-idempotent");
        write_export(&root, "decks/words/Words__A", b"a");
        migrate(Some(&root), true).expect("первое применение");

        let before = fs::read(root.join("decks/output/Words__A/deck.json")).expect("deck.json");
        let again = migrate(Some(&root), true).expect("повтор");

        assert_eq!(again.summary.exports, 0);
        assert_eq!(again.summary.applied, 0);
        assert!(!again.applied);
        assert!(!again.refused);
        assert!(
            again
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "nothing_to_migrate"),
            "повтор объясняет, что мигрировать нечего"
        );
        assert_eq!(
            fs::read(root.join("decks/output/Words__A/deck.json")).expect("deck.json"),
            before,
            "рабочая копия не перезаписана"
        );
    }

    #[test]
    fn apply_resumes_after_a_partial_migration() {
        let (_dir, root) = repository("layout-resume");
        let payload = vec![3_u8; 1024];
        write_export(&root, "decks/words/Words__A", &payload);
        write_export(&root, "decks/output/Words__A", &payload);
        write_export(&root, "decks/words/Words__B", b"b");

        let report = migrate(Some(&root), true).expect("применение");

        assert_eq!(report.summary.already_present, 1);
        assert_eq!(report.summary.applied, 2);
        assert!(!root.join("decks/words/Words__A").exists());
        assert!(!root.join("decks/words/Words__B").exists());
        assert_eq!(
            fs::read(root.join("decks/output/Words__A/media/a.bin")).expect("media"),
            payload
        );
    }

    #[test]
    fn a_target_name_collision_refuses_the_whole_plan() {
        let (_dir, root) = repository("layout-collision");
        write_export(&root, "decks/words/Words__A", b"a");
        write_export(&root, "decks/other/WORDS__a", b"b");

        let report = migrate(Some(&root), true).expect("план");

        assert!(report.refused);
        assert_eq!(report.exit_code(), 3);
        assert_eq!(report.summary.refused, 2);
        assert_eq!(report.summary.applied, 0);
        assert!(
            root.join("decks/words/Words__A/deck.json").is_file(),
            "отказ не переносит ни одного экспорта"
        );
        assert!(root.join("decks/other/WORDS__a/deck.json").is_file());
        assert!(!root.join("decks/output").exists());
    }

    #[test]
    fn a_refused_apply_is_reported_as_a_refusal_in_human_output() {
        let (_dir, root) = repository("layout-human-refused");
        write_export(&root, "decks/words/Words__A", b"a");
        write_export(&root, "decks/other/WORDS__a", b"b");

        let report = migrate(Some(&root), true).expect("план");
        assert!(report.refused);

        let human = crate::render::human::migrate_layout(&report);
        assert!(
            human.contains("перенос отклонён целиком"),
            "отказ обязан называться отказом: {human}"
        );
        assert!(
            !human.contains("перенос не требовался"),
            "отвергнутый план — не отсутствие работы: {human}"
        );
    }

    #[test]
    fn a_source_changed_after_planning_is_refused_before_the_move() {
        let (_dir, root) = repository("layout-source-changed");
        let export = write_export(&root, "decks/words/Words__A", b"a");
        let output = root.join("decks/output");
        fs::create_dir_all(&output).expect("каталог назначения");

        let mut entry = plan_entry(&export, &output);
        assert_eq!(entry.action, "move");

        // Источник изменился между планированием и переносом: отпечаток плана
        // больше не описывает его содержимое.
        fs::write(export.join("media").join("a.bin"), b"b").expect("источник изменён");

        let error = move_export(&root, &output, &mut entry).expect_err("перенос обязан отказать");
        assert_eq!(error.code, ErrorCode::SourceChanged);
        assert!(
            export.join(DECK_JSON).is_file(),
            "отказ не переносит экспорт"
        );
        assert!(
            !output.join("Words__A").exists(),
            "отказ не создаёт назначение"
        );
    }

    #[test]
    fn a_destination_redirected_after_planning_is_rechecked_before_writing() {
        let (_dir, root) = repository("layout-output-race");
        let source = write_export(&root, "decks/words/Words__A", b"a");
        let legacy = root.join("decks");
        let input = root.join("decks/input");
        fs::create_dir_all(&input).expect("закрытая область");
        fs::write(input.join("sentinel.bin"), b"private").expect("маркер");
        let output = root.join("decks/output");
        validate_output_location(&root, &output).expect("исходное назначение безопасно");
        let mut entries = vec![plan_entry(&source, &output)];

        std::os::unix::fs::symlink(&input, &output).expect("назначение перенаправлено в input");
        let error = apply_entries(&root, &output, &mut entries)
            .expect_err("путь повторно проверяется непосредственно перед записью");

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "input_area_forbidden");
        assert!(source.join(DECK_JSON).is_file(), "источник не перенесён");
        assert_eq!(fs::read(input.join("sentinel.bin")).unwrap(), b"private");
        assert!(
            !input.join("Words__A").exists(),
            "в input ничего не записано"
        );
        assert!(legacy.is_dir());
    }

    #[test]
    fn an_empty_target_that_appears_at_rename_is_not_replaced() {
        let (_dir, root) = repository("layout-rename-no-replace");
        let source = write_export(&root, "decks/words/Words__A", b"source");
        let output = root.join("decks/output");
        fs::create_dir_all(&output).expect("каталог назначения");
        let target = output.join("Words__A");
        fs::create_dir(&target).expect("пустое назначение появилось конкурентно");
        let mut entry = plan_entry(&source, &output);

        move_export(&root, &output, &mut entry).expect("конкурентная цель — отказ в отчёте");

        assert_eq!(entry.reason, Some("target_appeared"));
        assert_eq!(entry.state, "refused");
        assert!(source.join(DECK_JSON).is_file(), "источник сохранён");
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "назначение не заменено"
        );
    }

    #[test]
    fn copy_publication_does_not_replace_a_target_that_appears() {
        let (_dir, root) = repository("layout-copy-no-replace");
        let source = write_export(&root, "decks/words/Words__A", b"source");
        let output = root.join("decks/output");
        fs::create_dir_all(&output).expect("каталог назначения");
        let target = output.join("Words__A");
        fs::create_dir(&target).expect("пустое назначение появилось конкурентно");
        let expected = scan_export(&source).expect("отпечаток").digest;

        let published = copy_export(&root, &output, &source, &target, Some(&expected))
            .expect("появившаяся цель не перезаписывается");

        assert!(!published, "публикация отказана на атомарном rename");
        assert!(source.join(DECK_JSON).is_file(), "источник сохранён");
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "назначение не заменено"
        );
        assert!(
            fs::read_dir(&output).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(TEMP_PREFIX)),
            "временная копия очищена"
        );
    }

    #[test]
    fn a_same_device_move_needs_no_free_space() {
        let (_dir, root) = repository("layout-space");
        let source = write_export(&root, "decks/words/Words__A", &vec![7_u8; 4096]);
        let output = root.join("decks/output");
        fs::create_dir_all(&output).expect("каталог назначения");

        let entry = |action: &'static str| ExportPlan {
            name: Some("Words__A".to_owned()),
            source: "decks/words/Words__A".to_owned(),
            target: Some("decks/output/Words__A".to_owned()),
            action,
            state: "planned",
            files: 2,
            bytes: 4096,
            content_sha256: None,
            reason: None,
            message: None,
            source_path: source.clone(),
            target_path: Some(output.join("Words__A")),
            manifest: None,
        };

        // Перенос внутри одной файловой системы идёт одним `rename`, а уже
        // существующее назначение только снимает источник: свободное место
        // нужно только для копирования между файловыми системами.
        let entries = vec![entry("move"), entry("already_present")];
        let output_directory = File::open(&output).expect("каталог назначения");
        assert_eq!(
            bytes_to_copy(&entries, &output_directory).expect("объём"),
            0
        );
    }

    #[test]
    fn a_differing_target_is_never_overwritten() {
        let (_dir, root) = repository("layout-conflict");
        write_export(&root, "decks/words/Words__A", b"a");
        write_export(&root, "decks/output/Words__A", "другое".as_bytes());

        let report = migrate(Some(&root), true).expect("план");

        assert!(report.refused);
        assert_eq!(report.exports[0].reason, Some("target_conflict"));
        assert_eq!(
            fs::read(root.join("decks/output/Words__A/media/a.bin")).expect("media"),
            "другое".as_bytes().to_vec()
        );
        assert!(root.join("decks/words/Words__A/deck.json").is_file());
    }

    #[test]
    fn a_symlink_inside_an_export_refuses_the_whole_plan() {
        let (_dir, root) = repository("layout-symlink");
        let export = write_export(&root, "decks/words/Words__A", b"a");
        std::os::unix::fs::symlink("/etc/hostname", export.join("media").join("link.bin"))
            .expect("ссылка");

        let report = migrate(Some(&root), true).expect("план");

        assert!(report.refused);
        assert_eq!(report.exports[0].reason, Some("source_contains_symlink"));
        assert!(export.join("deck.json").is_file());
        assert!(!root.join("decks/output").exists());
    }

    #[test]
    fn the_closed_input_area_is_never_a_candidate() {
        let (_dir, root) = repository("layout-input");
        write_export(&root, "decks/input/Hidden", b"hidden");
        write_export(&root, "decks/words/Words__A", b"a");

        let report = migrate(Some(&root), true).expect("применение");

        assert_eq!(report.summary.exports, 1);
        assert_eq!(report.summary.applied, 1);
        assert!(
            root.join("decks/input/Hidden/deck.json").is_file(),
            "закрытая область не тронута"
        );
        assert!(!root.join("decks/output/Hidden").exists());
        assert!(root.join("decks/output/Words__A/deck.json").is_file());
    }

    #[test]
    fn a_refusal_during_apply_skips_even_the_placeholder_removal() {
        let (_dir, root) = repository("layout-refusal-during-apply");
        let legacy = root.join("decks");
        let input = legacy.join("input");
        let output = legacy.join("output");
        let export = write_export(&root, "decks/words/Words__A", b"a");
        fs::write(legacy.join(".gitkeep"), b"").expect("placeholder прежней раскладки");
        let mut entries = vec![plan_entry(&export, &output)];
        // Назначение появилось между планированием и записью: план отвергается
        // уже внутри записи.
        fs::create_dir_all(output.join("Words__A")).expect("назначение появилось");

        let (pruned, retained) =
            write_plan(&root, &legacy, &input, &output, &mut entries).expect("запись");

        assert_eq!(entries[0].reason, Some("target_appeared"));
        assert!(pruned.is_empty(), "placeholder-файлы не снимаются");
        assert!(
            legacy.join(".gitkeep").is_file(),
            "отвергнутый план ничего не пишет"
        );
        assert!(
            retained.is_none(),
            "отвергнутый план не сообщает об удержании области: {retained:?}"
        );
        assert!(root.join("decks/words/Words__A/deck.json").is_file());
    }

    #[test]
    fn an_existing_target_differing_only_by_case_refuses_the_whole_plan() {
        let (_dir, root) = repository("layout-existing-case-collision");
        write_export(&root, "decks/words/Words__A", b"a");
        write_export(&root, "decks/output/WORDS__a", b"b");

        let report = migrate(Some(&root), true).expect("план с отказом");

        assert!(report.refused);
        let entry = report.exports.first().expect("экспорт в отчёте");
        assert_eq!(entry.reason, Some("target_name_collision"));
        assert!(
            root.join("decks/words/Words__A/deck.json").is_file(),
            "источник не тронут"
        );
        assert!(
            !root.join("decks/output/Words__A").exists(),
            "второй каталог с тем же именем не создан"
        );
    }

    #[test]
    fn the_report_keeps_paths_relative_to_the_repository_root() {
        let (_dir, root) = repository("layout-relative-report");
        write_export(&root, "decks/words/Words__A", b"a");

        let report = migrate(Some(&root), false).expect("план");

        let entry = report.exports.first().expect("экспорт в отчёте");
        assert_eq!(entry.source, "decks/words/Words__A");
        assert_eq!(entry.target.as_deref(), Some("decks/output/Words__A"));
        assert_eq!(report.legacy_area, "decks");
        assert_eq!(report.output_dir, "decks/output");
    }

    #[test]
    fn a_symlinked_closed_area_is_neither_a_candidate_nor_foreign_content() {
        let (_dir, root) = repository("layout-closed-symlink");
        write_export(&root, "decks/words/Words__A", b"a");

        // Закрытая область подменена ссылкой на посторонний каталог с deck.json:
        // обход не должен ни заглядывать внутрь, ни считать её содержимым
        // прежней раскладки, иначе миграция отказывает целиком.
        let outside = root.join("outside").join("input");
        fs::create_dir_all(&outside).expect("посторонний каталог");
        fs::write(outside.join(DECK_JSON), DECK_JSON_BODY.as_bytes())
            .expect("посторонний deck.json");
        std::os::unix::fs::symlink(&outside, root.join("decks").join("input")).expect("ссылка");

        let report = migrate(Some(&root), true).expect("миграция выполнена");

        assert!(
            !report.refused,
            "миграция отклонена: {:?}",
            report
                .exports
                .iter()
                .map(|entry| entry.reason.unwrap_or("ok"))
                .collect::<Vec<_>>()
        );
        let sources: Vec<&str> = report
            .exports
            .iter()
            .map(|entry| entry.source.as_str())
            .collect();
        assert!(
            !sources.iter().any(|source| source.contains("decks/input")),
            "ссылка на закрытую область не становится кандидатом: {sources:?}"
        );
        let diagnostic_codes: Vec<&str> = report
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect();
        assert!(
            !diagnostic_codes.contains(&"legacy_area_retained"),
            "закрытая область не удерживает прежнюю раскладку: {diagnostic_codes:?}"
        );
        assert!(
            root.join("decks/output/Words__A/deck.json").is_file(),
            "экспорт перенесён"
        );
    }

    #[test]
    fn migration_does_not_need_to_read_the_closed_input_area() {
        let (_dir, root) = repository("layout-input-unreadable");
        write_export(&root, "decks/input/Hidden", b"hidden");
        write_export(&root, "decks/words/Words__A", b"a");

        let input = root.join("decks").join("input");
        let mut permissions = fs::metadata(&input).expect("каталог").permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&input, permissions).expect("закрыть доступ");

        let still_readable = fs::read_dir(&input).is_ok();
        let report = migrate(Some(&root), true);

        let mut permissions = fs::metadata(&input).expect("каталог").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&input, permissions).expect("вернуть доступ");

        let report = report.expect("миграция не зависит от чтения закрытой области");
        assert_eq!(report.summary.applied, 1);
        if !still_readable {
            assert!(
                root.join("decks/input/Hidden/deck.json").is_file(),
                "содержимое закрытой области осталось нетронутым"
            );
        }
    }

    #[test]
    fn the_legacy_area_itself_is_never_moved_as_an_export() {
        let (_dir, root) = repository("layout-root-export");
        fs::write(
            root.join("decks").join(DECK_JSON),
            DECK_JSON_BODY.as_bytes(),
        )
        .expect("deck.json");
        write_export(&root, "decks/words/Words__A", b"a");

        let report = migrate(Some(&root), true).expect("план");

        assert!(report.refused);
        assert!(
            report
                .exports
                .iter()
                .any(|entry| entry.reason == Some("legacy_area_is_export"))
        );
        assert!(root.join("decks/words/Words__A/deck.json").is_file());
        assert!(!root.join("decks/output").exists());
    }

    #[test]
    fn a_root_inside_the_closed_area_is_refused() {
        let (_dir, root) = repository("layout-guard");
        let input = root.join("decks").join("input");
        fs::create_dir_all(&input).expect("закрытая область");

        let error = migrate(Some(&input), false).expect_err("отказ");

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "input_area_forbidden");
    }

    #[test]
    fn a_repository_without_the_legacy_area_reports_it_honestly() {
        let dir = TempDir::new("layout-absent");
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("признак репозитория");

        let report = migrate(Some(&root), true).expect("план");

        assert_eq!(report.summary.exports, 0);
        assert!(!report.applied);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "legacy_area_absent")
        );
    }

    #[test]
    fn foreign_content_keeps_the_legacy_area_and_is_reported() {
        let (_dir, root) = repository("layout-retained");
        write_export(&root, "decks/words/Words__A", b"a");
        let notes = root.join("decks").join("words").join("notes.txt");
        fs::create_dir_all(notes.parent().expect("родительский каталог")).expect("каталог");
        fs::write(&notes, "личные заметки".as_bytes()).expect("файл");

        let report = migrate(Some(&root), true).expect("применение");

        assert!(report.applied);
        assert!(notes.is_file(), "чужой файл не удалён");
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "legacy_area_retained")
        );
    }
}
