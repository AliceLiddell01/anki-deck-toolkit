//! Человекочитаемый вывод: компактный текст на русском языке.
//!
//! Модуль вывода не выполняет доменную работу: он только печатает уже готовый
//! доменный результат — включая адресуемость заметок и состав групп, которые команды
//! уже вычислили. Значения полей в человекочитаемом режиме приводятся к одной строке и
//! ограничиваются по длине.
//!
//! Сообщение об усечении обязано называть реальную причину и реальный способ
//! получить остаток. `--json` сериализует ровно тот же вектор, поэтому
//! «полный список доступен в --json» запрещено: это обещание, которого команда
//! не выполняет.

use crate::ops::NamedField;
use crate::ops::create::CreateResult;
use crate::ops::edit::{EditResult, EditStatus};
use crate::ops::find::FindResult;
use crate::ops::inspect::InspectResult;
use crate::ops::migrate_layout::{ExportPlan, MigrationReport};
use crate::ops::migrate_media::MigrationResult;
use crate::ops::models::ModelsResult;
use crate::ops::qa::QaResult;
use crate::ops::retire::RetireResult;
use crate::ops::review::ReviewResult;
use crate::ops::review_check::{ReviewCheckResult, ReviewOutcome};
use crate::ops::stats::StatsResult;
use crate::ops::validate::{Severity, ValidateResult};
use crate::ops::visual_report::{NoteChangeKind, VisualReportResult};

/// Предел длины значения поля в человекочитаемом режиме.
pub const VALUE_LIMIT: usize = 300;

#[derive(Default)]
struct Out {
    buffer: String,
}

impl Out {
    fn line(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.buffer.push_str(text.as_ref());
        self.buffer.push('\n');
        self
    }

    fn blank(&mut self) -> &mut Self {
        self.buffer.push('\n');
        self
    }

    fn finish(mut self) -> String {
        while self.buffer.ends_with("\n\n") {
            self.buffer.pop();
        }
        self.buffer
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "да" } else { "нет" }
}

fn optional(value: Option<&str>) -> &str {
    value.unwrap_or("—")
}

/// Приводит значение поля к одной строке с ограничением длины.
fn compact_value(value: &str) -> String {
    let single: String = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut result: String = single.chars().take(VALUE_LIMIT).collect();
    if single.chars().count() > VALUE_LIMIT {
        result.push('…');
    }
    result
}

/// Печатает результат `inspect`.
pub fn inspect(result: &InspectResult) -> String {
    let mut out = Out::default();
    let verbose = result.verbose.is_some();

    out.line(format!("Экспорт: {}", result.export_dir));
    if let Some(details) = &result.verbose {
        out.line(format!("deck.json: {}", details.deck_json));
    }
    out.line(format!("Корневая колода: {}", result.root_deck_name));
    out.line(format!("Узлов колоды: {}", result.deck_nodes));
    out.line(format!("Заметок всего: {}", result.notes_total));
    out.blank();

    out.line(format!("Модели заметок ({}):", result.models.len()));
    if result.models.is_empty() {
        out.line("  —");
    }
    for model in &result.models {
        out.line(format!("  {}", model.name));
        if verbose {
            out.line(format!(
                "    crowdanki_uuid: {}",
                optional(model.crowdanki_uuid.as_deref())
            ));
        }
        out.line(format!(
            "    полей: {}, шаблонов: {}, заметок: {}",
            model.field_count, model.template_count, model.used_notes
        ));
        for field in &model.fields {
            out.line(format!(
                "    ord={}  {}",
                field
                    .ord
                    .map_or_else(|| "?".to_string(), |ord| ord.to_string()),
                field.name
            ));
        }
    }
    out.blank();

    out.line(format!("Конфигурации колод ({}):", result.configs.len()));
    if result.configs.is_empty() {
        out.line("  —");
    }
    for config in &result.configs {
        out.line(format!("  {}", config.name));
        if verbose {
            out.line(format!(
                "    crowdanki_uuid: {}",
                optional(config.crowdanki_uuid.as_deref())
            ));
        }
        out.line(format!("    узлов: {}", config.nodes_using));
    }
    out.blank();

    let media = &result.media;
    out.line("Media:");
    out.line(format!(
        "  объявлено имён: {} (уникальных: {}, повторов: {})",
        media.declared_total, media.declared_unique, media.duplicate_declared
    ));
    out.line(format!(
        "  каталог media/: {}",
        if media.dir_present {
            "есть"
        } else {
            "нет"
        }
    ));
    out.line(format!("  файлов в media/: {}", media.physical_total));
    out.line(format!(
        "  объявлено, но нет физически: {}",
        media.missing_physical
    ));
    out.line(format!(
        "  есть физически, но не объявлено: {}",
        media.undeclared_physical
    ));

    if let Some(details) = &result.verbose {
        push_sample(
            &mut out,
            "  образец отсутствующих физически",
            &details.media_missing_physical_sample,
        );
        push_sample(
            &mut out,
            "  образец необъявленных физически",
            &details.media_undeclared_physical_sample,
        );
        push_sample(
            &mut out,
            "  образец повторяющихся объявлений",
            &details.media_duplicate_declared_sample,
        );
    }
    out.blank();

    out.line("Заметки по узлам колоды:");
    if result.notes_by_deck.is_empty() {
        out.line("  —");
    }
    for entry in &result.notes_by_deck {
        out.line(format!("  {}: {}", entry.key, entry.count));
    }

    if let Some(details) = &result.verbose {
        out.blank();
        out.line("Дерево колод (preorder):");
        for node in &details.nodes {
            out.line(format!(
                "  {indent}{path}  [заметок: {notes}, uuid: {uuid}]",
                indent = "  ".repeat(node.depth),
                path = node.path,
                notes = node.notes,
                uuid = optional(node.crowdanki_uuid.as_deref()),
            ));
        }

        out.blank();
        out.line("Шаблоны карточек:");
        if details.model_templates.is_empty() {
            out.line("  —");
        }
        for model in &details.model_templates {
            out.line(format!("  {}", model.model_name));
            for template in &model.templates {
                out.line(format!(
                    "    ord={}  {}",
                    template
                        .ord
                        .map_or_else(|| "?".to_string(), |ord| ord.to_string()),
                    optional(template.name.as_deref())
                ));
            }
        }

        out.blank();
        out.line("Идентификаторы заметок:");
        out.line(format!("  уникальных guid: {}", details.guids_unique));
        out.line(format!("  guid с повторами: {}", details.guid_duplicates));

        out.blank();
        out.line("Выборка заметок:");
        if details.notes_sample.is_empty() {
            out.line("  —");
        }
        for sample in &details.notes_sample {
            out.line(format!(
                "  guid={} колода={} полей={}",
                optional(sample.guid.as_deref()),
                sample.deck_path,
                sample.field_count
            ));
        }
    }

    out.finish()
}

fn push_sample(out: &mut Out, title: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    out.line(format!("{title}: {}", values.join(", ")));
}

/// Печатает результат `find`.
pub fn find(result: &FindResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("Критерий: {}", describe_criteria(result)));
    out.line(format!(
        "Найдено заметок: {}, возвращено: {}, усечено: {}",
        result.matched_total,
        result.returned,
        yes_no(result.truncated)
    ));

    for (position, note) in result.notes.iter().enumerate() {
        out.blank();
        out.line(format!(
            "[{}] guid: {}",
            position + 1,
            optional(note.guid.as_deref())
        ));
        out.line(format!("    колода: {}", note.deck_path));
        out.line(format!(
            "    модель: {}",
            optional(note.note_model_name.as_deref())
        ));
        out.line(format!(
            "    uuid модели: {}",
            optional(note.note_model_uuid.as_deref())
        ));
        out.line(format!(
            "    теги: {}",
            if note.tags.is_empty() {
                "—".to_string()
            } else {
                note.tags.join(", ")
            }
        ));
        for field in &note.fields {
            out.line(format!("    {}: {}", field.name, render_field(field)));
        }
    }

    out.finish()
}

fn render_field(field: &NamedField) -> String {
    match &field.value {
        Some(value) => compact_value(value),
        None => "(значение отсутствует)".to_string(),
    }
}

fn describe_criteria(result: &FindResult) -> String {
    let mut text = match (
        result.criteria.field.as_deref(),
        result.criteria.guid.as_deref(),
    ) {
        (Some(field), _) => format!(
            "поле {:?} {mode} {:?}",
            field,
            result.criteria.value.as_deref().unwrap_or(""),
            mode = match result.criteria.match_mode {
                Some("exact") => "точно равно",
                _ => "содержит",
            }
        ),
        (None, Some(guid)) => format!("guid {guid:?}"),
        (None, None) => "не задан".to_string(),
    };
    if let Some(deck) = &result.criteria.deck {
        text.push_str(&format!(", колода {deck:?}"));
    }
    text
}

/// Печатает результат `stats`.
pub fn stats(result: &StatsResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("Узлов колоды: {}", result.deck_nodes));
    out.line(format!("Заметок всего: {}", result.notes_total));
    out.line(format!(
        "Заметок с неразрешённой моделью: {}",
        result.unresolved_model_notes
    ));

    out.blank();
    out.line("Заметки по колодам:");
    if result.notes_by_deck.is_empty() {
        out.line("  —");
    }
    for entry in &result.notes_by_deck {
        out.line(format!("  {}: {}", entry.key, entry.count));
    }

    out.blank();
    out.line("Заметки по моделям:");
    if result.notes_by_model.is_empty() {
        out.line("  —");
    }
    for entry in &result.notes_by_model {
        out.line(format!("  {}: {}", entry.key, entry.count));
    }

    out.blank();
    out.line("Использование конфигураций (по узлам колод):");
    if result.config_usage.is_empty() {
        out.line("  —");
    }
    for entry in &result.config_usage {
        out.line(format!("  {}: {}", entry.key, entry.count));
    }

    out.blank();
    out.line("Поля:");
    if result.fields.is_empty() {
        out.line("  —");
    }
    for field in &result.fields {
        out.line(format!(
            "  {}: всего {}, непустых {}, пустых {}",
            field.name, field.total, field.nonempty, field.empty
        ));
    }

    let media = &result.media;
    out.blank();
    out.line("Media:");
    out.line(format!(
        "  объявлено имён: {} (уникальных: {}, повторов: {})",
        media.declared_total, media.declared_unique, media.duplicate_declared
    ));
    out.line(format!("  файлов в media/: {}", media.physical_total));
    out.line(format!(
        "  объявлено, но нет физически: {}",
        media.missing_physical
    ));
    out.line(format!(
        "  есть физически, но не объявлено: {}",
        media.undeclared_physical
    ));

    if let Some(group) = &result.group_by {
        out.blank();
        out.line(format!(
            "Распределение по полю {:?} (различных значений: {}, заметок без поля: {}, усечено: {}):",
            group.field,
            group.distinct_values,
            group.notes_without_field,
            yes_no(group.truncated)
        ));
        if group.buckets.is_empty() {
            out.line("  —");
        }
        for bucket in &group.buckets {
            let value = if bucket.key.is_empty() {
                "(пусто)".to_string()
            } else {
                compact_value(&bucket.key)
            };
            out.line(format!("  {:>6}  {}", bucket.count, value));
        }
    }

    out.finish()
}

/// Печатает результат `validate`.
pub fn validate(result: &ValidateResult) -> String {
    let mut out = Out::default();

    out.line(format!("Валиден: {}", yes_no(result.valid)));
    out.line(format!(
        "ERROR: {}, WARNING: {}, INFO: {}",
        result.summary.errors, result.summary.warnings, result.summary.info
    ));

    for severity in [Severity::Error, Severity::Warning, Severity::Info] {
        let issues: Vec<_> = result
            .issues
            .iter()
            .filter(|issue| issue.severity == severity)
            .collect();
        if issues.is_empty() {
            continue;
        }
        out.blank();
        out.line(format!(
            "{} ({}):",
            severity.as_str().to_uppercase(),
            issues.len()
        ));
        for issue in issues {
            let scope = describe_scope(issue.deck_path.as_deref(), &issue.location);
            if scope.is_empty() {
                out.line(format!("  {} — {}", issue.code, issue.message));
            } else {
                out.line(format!("  {} {} — {}", issue.code, scope, issue.message));
            }
        }
    }

    out.finish()
}

fn describe_scope(deck_path: Option<&str>, location: &str) -> String {
    match (deck_path, location.is_empty()) {
        (Some(path), false) => format!("{path} {location}"),
        (Some(path), true) => path.to_string(),
        (None, false) => location.to_string(),
        (None, true) => String::new(),
    }
}

/// Печатает результат `edit`.
pub fn edit(result: &EditResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir.display()));
    out.line(format!("deck.json: {}", result.deck_json.display()));
    out.line(if result.applied {
        "Режим: --apply, deck.json заменён атомарно".to_string()
    } else if result.dry_run {
        "Режим: dry-run, deck.json не изменён".to_string()
    } else {
        "Режим: --apply, запись не требовалась".to_string()
    });
    out.line(format!("Правок в запросе: {}", result.edits_total));
    out.line(format!("Эффективных правок: {}", result.effective_edits));
    out.line(format!(
        "Статусы: применено {}, dry-run {}, без изменений {}, уже применено {}",
        result.summaries.applied,
        result.summaries.dry_run,
        result.summaries.noop_identical,
        result.summaries.already_applied
    ));
    match result.first_changed_line {
        Some(line) => out.line(format!(
            "Изменённых строк: {} (первая — {line})",
            result.changed_lines
        )),
        None => out.line("Изменённых строк: 0"),
    };
    out.line(format!(
        "Байт: {} → {} ({:+})",
        result.source_bytes, result.candidate_bytes, result.byte_delta
    ));

    out.blank();
    out.line(format!("Правки ({}):", result.outcomes.len()));
    if result.outcomes.is_empty() {
        out.line("  —");
    }
    for outcome in &result.outcomes {
        let edit_id = outcome
            .edit_id
            .as_deref()
            .map_or(String::new(), |id| format!(" [{id}]"));
        out.line(format!(
            "  #{} {}{} — {} (ord {}, заметка {}, колода {})",
            outcome.edit_index,
            outcome.status.as_str(),
            edit_id,
            format_args!("{:?}/{:?}", outcome.guid, outcome.field),
            outcome.field_ord,
            outcome.note_index,
            outcome.deck_path
        ));
        if outcome.status != EditStatus::NoopIdentical {
            out.line(format!(
                "      было: {}",
                compact_value(&outcome.old_sample)
            ));
            out.line(format!(
                "      стало: {}",
                compact_value(&outcome.new_sample)
            ));
        }
    }
    if result.outcomes_truncated {
        out.line(format!(
            "  … в отчёт попало только {} правок из {}; остальные не показаны ни здесь, ни в --json",
            result.outcomes.len(),
            result.edits_total
        ));
    }

    out.blank();
    out.line("Проверки:");
    for (name, value) in [
        ("source_canonical", result.checks.source_canonical),
        ("candidate_reparsed", result.checks.candidate_reparsed),
        (
            "semantic_targets_verified",
            result.checks.semantic_targets_verified,
        ),
        (
            "diff_shape_is_exactly_requested",
            result.checks.diff_shape_is_exactly_requested,
        ),
        (
            "byte_delta_matches_token_delta",
            result.checks.byte_delta_matches_token_delta,
        ),
    ] {
        out.line(format!("  {name}: {}", yes_no(value)));
    }

    out.blank();
    out.line(format!(
        "Валидация до: ERROR {}, WARNING {}, INFO {}",
        result.validation.before.errors,
        result.validation.before.warnings,
        result.validation.before.info
    ));
    out.line(format!(
        "Валидация после: ERROR {}, WARNING {}, INFO {}",
        result.validation.after.errors,
        result.validation.after.warnings,
        result.validation.after.info
    ));
    out.line(format!(
        "Новые ERROR: {}",
        list_or_none(&result.validation.new_error_codes)
    ));
    out.line(format!(
        "Новые WARNING: {}",
        list_or_none(&result.validation.new_warning_codes)
    ));

    out.finish()
}

/// Печатает список кодов или «нет».
fn list_or_none(codes: &[String]) -> String {
    if codes.is_empty() {
        "нет".to_string()
    } else {
        codes.join(", ")
    }
}

/// Печатает результат `qa`.
pub fn qa(result: &QaResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("Заметок всего: {}", result.notes_total));
    out.line(format!(
        "Findings: всего {}, показано {}, усечено: {} (предел {} на код)",
        result.findings_total,
        result.findings_returned,
        yes_no(result.truncated),
        result.max_per_code
    ));
    if result.unaddressable_findings > 0 {
        out.line(format!(
            "Неадресуемых findings: {} (нет guid, guid не уникален или модель не разрешается; \
             это диагностика содержимого, структурные причины — в validate)",
            result.unaddressable_findings
        ));
    }
    out.line(format!(
        "Коды: {}",
        if result.codes.is_empty() {
            "все правила".to_string()
        } else {
            result.codes.join(", ")
        }
    ));

    out.blank();
    out.line("По кодам:");
    if result.by_code.is_empty() {
        out.line("  —");
    }
    for entry in &result.by_code {
        out.line(format!(
            "  {} [{}]: {}",
            entry.code,
            entry.severity.as_str(),
            entry.count
        ));
    }

    out.blank();
    out.line("Findings:");
    if result.findings.is_empty() {
        out.line("  —");
    }
    for finding in &result.findings {
        out.line(format!(
            "  [{}] {} — заметка #{} ({})",
            finding.severity.as_str(),
            finding.code,
            finding.note_index,
            optional(finding.guid.as_deref())
        ));
        out.line(format!(
            "      колода: {}, модель: {}",
            finding.deck_path,
            optional(finding.note_model.as_deref())
        ));
        if let Some(field) = &finding.field {
            out.line(format!(
                "      поле: {} (ord {})",
                field,
                finding
                    .field_ord
                    .map_or_else(|| "?".to_string(), |ord| ord.to_string())
            ));
        }
        out.line(format!("      {}", finding.message));
        if !finding.addressable {
            out.line(
                "        адресуемость: нет (guid отсутствует, пуст или повторяется, \
                 либо модель заметки не разрешается) — предложение по этой заметке \
                 неисполнимо, структурные причины смотрите в validate",
            );
        }
        if finding.group_size.is_some() {
            out.line(format!(
                "        участники группы ({}): {}",
                finding
                    .group_size
                    .map_or_else(|| "?".to_string(), |size| size.to_string()),
                describe_related(&finding.related_note_indices, &finding.related_guids)
            ));
            if finding.related_truncated {
                out.line("        … показаны не все участники группы");
            }
        }
    }
    if result.truncated {
        out.line(format!(
            "  … показано не более {} findings на код: остальные не выводятся ни здесь, ни в --json; \
             увеличьте --max-per-code (или сузьте выборку через --code)",
            result.max_per_code
        ));
    }

    out.blank();
    out.line(format!("Правила ({}):", result.rules.len()));
    for rule in &result.rules {
        out.line(format!(
            "  {} [{}] применимо: {}, findings: {}",
            rule.code,
            rule.severity.as_str(),
            yes_no(rule.applicable),
            rule.findings
        ));
        out.line(format!("      {}", rule.description));
    }

    out.finish()
}

/// Печатает результат `review`.
pub fn review(result: &ReviewResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("Критерий: {}", describe_selection(result)));
    out.line(format!(
        "Выбрано заметок: {} из {}, страница: offset {}, limit {}, возвращено {}, усечено: {}",
        result.total_selected,
        result.notes_total,
        result.offset,
        result.limit,
        result.returned,
        yes_no(result.truncated)
    ));
    match result.next_offset {
        Some(next) => out.line(format!("Следующая страница: --offset {next}")),
        None => out.line("Следующая страница: нет"),
    };
    if result.excluded_unaddressable > 0 {
        out.line(format!(
            "Исключено неадресуемых заметок: {} (нет guid, guid не уникален или модель не \
             разрешается; предложение по ним неисполнимо, структурные причины — в validate)",
            result.excluded_unaddressable
        ));
    }

    for item in &result.items {
        out.blank();
        out.line(format!(
            "[#{}] guid: {}",
            item.note_index,
            optional(item.note.guid.as_deref())
        ));
        out.line(format!("    колода: {}", item.note.deck_path));
        out.line(format!(
            "    модель: {}",
            optional(item.note.note_model_name.as_deref())
        ));
        out.line(format!(
            "    теги: {}",
            if item.note.tags.is_empty() {
                "—".to_string()
            } else {
                item.note.tags.join(", ")
            }
        ));
        for field in &item.note.fields {
            out.line(format!("    {}: {}", field.name, render_field(field)));
        }
        for membership in &item.group_membership {
            out.line(format!(
                "    группа {}: участник группы заметки #{} ({}), всего участников {}",
                membership.code,
                membership.owner_note_index,
                optional(membership.owner_guid.as_deref()),
                membership.group_size
            ));
        }
        if item.qa_findings.is_empty() {
            out.line("    QA: —");
        } else {
            out.line(format!("    QA ({}):", item.qa_findings.len()));
            for finding in &item.qa_findings {
                let field = finding
                    .field
                    .as_deref()
                    .map_or(String::new(), |field| format!(", поле {field}"));
                out.line(format!(
                    "      [{}] {}{} — {}",
                    finding.severity.as_str(),
                    finding.code,
                    field,
                    finding.message
                ));
                if finding.group_size.is_some() {
                    out.line(format!(
                        "        группа: участников {}, кроме этой заметки: {}",
                        finding
                            .group_size
                            .map_or_else(|| "?".to_string(), |size| size.to_string()),
                        describe_related_notes(&finding.related)
                    ));
                    if finding.related_truncated {
                        out.line("        … показаны не все участники группы");
                    }
                }
            }
            if item.qa_findings_truncated {
                out.line("      … список findings обрезан");
            }
        }
    }

    out.finish()
}

/// Печатает результат `review-check`.
pub fn review_check(result: &ReviewCheckResult) -> String {
    let blockers: Vec<&str> = result
        .source_blockers
        .iter()
        .map(|blocker| blocker.code)
        .collect();
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir));
    out.line(format!("deck.json: {}", result.deck_json));
    out.line(format!("Итог: {}", result.outcome.as_str()));
    out.line(format!(
        "Предложений в документе: {}",
        result.proposals_total
    ));
    out.line(format!(
        "Статусы: valid {}, already_correct {}, already_applied {}, conflict {}, invalid {}",
        result.counts.valid,
        result.counts.already_correct,
        result.counts.already_applied,
        result.counts.conflict,
        result.counts.invalid
    ));
    out.line(format!(
        "Эффективных предложений: {}",
        result.effective_proposals
    ));
    out.line(format!(
        "Готовый запрос для edit: {}",
        match &result.edit_request {
            Some(request) => format!("да, правок {}", request.edits.len()),
            None if !result.source_blockers.is_empty() => {
                format!("нет: исходник нельзя править ({})", blockers.join(", "))
            }
            None if result.outcome == ReviewOutcome::Ok => "нет: нечего применять".to_string(),
            None => "нет: отчёт не ok".to_string(),
        }
    ));
    if !result.source_blockers.is_empty() {
        out.line(format!(
            "Исходник: править нельзя ({}); отчёт по предложениям всё равно показан ниже",
            blockers.join(", ")
        ));
        for blocker in &result.source_blockers {
            out.line(format!("  {} — {}", blocker.code, blocker.message));
        }
    }

    out.blank();
    out.line(format!("Предложения ({}):", result.proposals.len()));
    if result.proposals.is_empty() {
        out.line("  —");
    }
    for proposal in &result.proposals {
        out.line(format!(
            "  #{} {} — {}",
            proposal.proposal_index,
            proposal.status.as_str(),
            format_args!("{:?}/{:?}", proposal.guid, proposal.field)
        ));
        if let (Some(current), Some(_)) = (proposal.current_sample.as_deref(), proposal.note_index)
        {
            out.line(format!("      сейчас: {}", compact_value(current)));
        }
        out.line(format!(
            "      ожидалось: {}",
            compact_value(&proposal.expected_sample)
        ));
        out.line(format!(
            "      замена: {}",
            compact_value(&proposal.replacement_sample)
        ));
        if let Some(reason) = &proposal.reason {
            out.line(format!("      пояснение агента: {}", compact_value(reason)));
        }
        if let Some(message) = &proposal.message {
            out.line(format!(
                "      проблема [{}]: {message}",
                proposal.problem.unwrap_or("problem")
            ));
        }
    }
    if result.proposals_truncated {
        out.line(format!(
            "  … в отчёт попало только {} предложений из {}; счётчики выше считают все, \
             а остальные предложения не показаны ни здесь, ни в --json",
            result.proposals.len(),
            result.proposals_total
        ));
    }

    out.finish()
}

/// Печатает участников группы в форме `note_index (guid)`.
fn describe_related(positions: &[usize], guids: &[String]) -> String {
    if positions.is_empty() {
        return "—".to_string();
    }
    positions
        .iter()
        .enumerate()
        .map(|(offset, position)| match guids.get(offset) {
            Some(guid) => format!("#{position} ({guid})"),
            None => format!("#{position} (без guid)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Печатает участников группы набора в форме `note_index (guid)`.
fn describe_related_notes(related: &[crate::ops::review::RelatedNote]) -> String {
    if related.is_empty() {
        return "—".to_string();
    }
    related
        .iter()
        .map(|note| match &note.guid {
            Some(guid) => format!("#{} ({guid})", note.note_index),
            None => format!("#{} (без guid)", note.note_index),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Описывает критерий выбора `review`.
fn describe_selection(result: &ReviewResult) -> String {
    let selection = &result.selection;
    let mut parts = vec![selection.kind.to_string()];
    if let Some(guid) = &selection.guid {
        parts.push(format!("guid {guid:?}"));
    }
    if let Some(field) = &selection.field {
        parts.push(format!(
            "поле {field:?}, значение {:?}, режим {}",
            selection.value.as_deref().unwrap_or_default(),
            selection.match_mode.unwrap_or("contains")
        ));
    }
    if let Some(code) = &selection.qa_code {
        parts.push(format!("QA-код {code}"));
    }
    if let Some(deck) = &selection.deck {
        parts.push(format!("колода {deck:?}"));
    }
    parts.join("; ")
}

/// Человекочитаемое представление `models`.
pub fn models(result: &ModelsResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir.display()));
    out.line(format!(
        "Колода: {} (preorder {}, uuid {})",
        result.deck.path,
        result.deck.preorder,
        optional(result.deck.crowdanki_uuid.as_deref())
    ));
    out.line(format!("Заметок в узле: {}", result.deck.notes_in_deck));
    out.line(format!("Предел примеров на поле: {}", result.sample_limit));

    if result.models.is_empty() {
        out.blank();
        out.line("Модели заметок в этой колоде не найдены.");
        return out.finish();
    }

    for model in &result.models {
        out.blank();
        out.line(format!(
            "Модель: {} ({}, {}, {})",
            model.name,
            model.crowdanki_uuid,
            model_kind_name(model.model_kind),
            if model.declared_in_deck {
                "объявлена в колоде"
            } else {
                "объявлена вне колоды"
            }
        ));
        out.line(format!(
            "  type: {}; заметок в узле {}, в поддереве {}",
            model
                .model_type
                .map_or_else(|| "—".to_string(), |value| value.to_string()),
            model.notes_in_deck,
            model.notes_in_subtree
        ));

        out.line(format!("  Поля ({}):", model.fields.len()));
        for field in &model.fields {
            out.line(format!(
                "    ord {} {} — пусто у {} заметок колоды",
                field.ord, field.name, field.empty_in_deck
            ));
            if let Some(description) = field.description.as_deref()
                && !description.trim().is_empty()
            {
                out.line(format!("        описание: {}", compact_value(description)));
            }
            for sample in &field.samples {
                out.line(format!(
                    "        пример [{}]: {}",
                    optional(sample.guid.as_deref()),
                    compact_value(&sample.value)
                ));
            }
        }

        out.line(format!("  Шаблоны ({}):", model.templates.len()));
        for template in &model.templates {
            out.line(format!(
                "    ord {} {} — поля: {}; special: {}",
                template.ord,
                optional(template.name.as_deref()),
                if template.fields.is_empty() {
                    "—".to_string()
                } else {
                    template.fields.join(", ")
                },
                if template.specials.is_empty() {
                    "—".to_string()
                } else {
                    template.specials.join(", ")
                }
            ));
            for unsupported in &template.unsupported {
                out.line(format!(
                    "        не поддержано превью: {} — {}",
                    unsupported.construct, unsupported.reason
                ));
            }
        }

        if !model.schema_problems.is_empty() {
            out.line("  Схема полей непригодна для сборки значений:");
            for problem in &model.schema_problems {
                out.line(format!("    {problem}"));
            }
        }
    }

    out.finish()
}

/// Человекочитаемое представление `create`.
pub fn create(result: &CreateResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir.display()));
    out.line(format!("deck.json: {}", result.deck_json.display()));
    if result.media.mutations > 0 && !result.applied {
        out.line("Режим: --apply, восстановлены медиафайлы");
    } else {
        out.line(mode_line(result.applied, result.dry_run));
    }
    out.line(format!("Заметок в запросе: {}", result.notes_total));
    out.line(format!("Создано: {}", result.notes_created));
    out.line(format!("Уже было: {}", result.notes_already_applied));
    out.line(format!(
        "Байт: {} → {} ({:+})",
        result.source_bytes, result.candidate_bytes, result.byte_delta
    ));

    out.line(format!(
        "План медиафайлов: ссылок={}, проверенных файлов={}, новых объявлений media_files={}",
        result.media.references.len(),
        result.media.assets_total(),
        result.media.declarations_added.len()
    ));
    out.blank();
    out.line(format!(
        "Затронутые колоды ({}):",
        result.decks_touched.len()
    ));
    if result.decks_touched.is_empty() {
        out.line("  —");
    }
    for touch in &result.decks_touched {
        out.line(format!(
            "  {} ({}) — было заметок {}, добавлено {}",
            touch.deck_path, touch.deck_uuid, touch.notes_before, touch.notes_added
        ));
    }

    out.blank();
    out.line(format!("Заметки ({}):", result.outcomes.len()));
    if result.outcomes.is_empty() {
        out.line("  —");
    }
    for outcome in &result.outcomes {
        let note_id = outcome
            .note_id
            .as_deref()
            .map_or(String::new(), |id| format!(" [{id}]"));
        let guid = if outcome.guid_generated {
            format!("{:?} (сгенерирован)", outcome.guid)
        } else {
            format!("{:?}", outcome.guid)
        };
        out.line(format!(
            "  #{} {}{} — guid {}, колода {}, модель {} ({})",
            outcome.note_index,
            outcome.status.as_str(),
            note_id,
            guid,
            outcome.deck_path,
            outcome.model_name,
            outcome.model_uuid
        ));
        out.line(format!(
            "      поля с обработчиком: {}",
            outcome.processor_fields.join(", ")
        ));
        out.line(format!(
            "      свидетельство модели: {}",
            outcome.model_evidence
        ));
        out.line(format!(
            "      поля ({}): {}",
            outcome.fields_total,
            outcome.field_names.join(", ")
        ));
        out.line(format!(
            "      теги: {}; ссылок на медиа в новых значениях: {}",
            if outcome.tags.is_empty() {
                "—".to_string()
            } else {
                outcome.tags.join(", ")
            },
            outcome.media_references
        ));
    }
    if result.outcomes_truncated {
        out.line(format!(
            "  … в отчёт попало только {} заметок из {}",
            result.outcomes.len(),
            result.notes_total
        ));
    }

    out.blank();
    out.line("Проверки:");
    for (name, value) in [
        ("source_canonical", result.checks.source_canonical),
        ("candidate_reparsed", result.checks.candidate_reparsed),
        (
            "model_resolution_evidenced",
            result.checks.model_resolution_evidenced,
        ),
        (
            "media_references_absent",
            result.checks.media_references_absent,
        ),
        (
            "only_notes_and_media_files_appended",
            result.checks.only_notes_and_media_files_appended,
        ),
        ("media_assets_verified", result.checks.media_assets_verified),
        (
            "guids_resolved_without_conflict",
            result.checks.guids_resolved_without_conflict,
        ),
        ("only_notes_appended", result.checks.only_notes_appended),
        (
            "appended_notes_verified",
            result.checks.appended_notes_verified,
        ),
    ] {
        out.line(format!("  {name}: {}", yes_no(value)));
    }

    push_validation(&mut out, &result.validation);
    out.finish()
}

/// Человекочитаемое представление `retire`.
pub fn retire(result: &RetireResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir.display()));
    out.line(format!("deck.json: {}", result.deck_json.display()));
    out.line(mode_line(result.applied, result.dry_run));
    out.line(format!("Тег вывода из обращения: {:?}", result.tag));
    out.line(format!("Заметок в запросе: {}", result.notes_total));
    out.line(format!("Помечено: {}", result.notes_retired));
    out.line(format!(
        "Уже было помечено: {}",
        result.notes_already_retired
    ));
    out.line(format!(
        "Байт: {} → {} ({:+})",
        result.source_bytes, result.candidate_bytes, result.byte_delta
    ));

    out.blank();
    out.line(format!("Заметки ({}):", result.outcomes.len()));
    if result.outcomes.is_empty() {
        out.line("  —");
    }
    for outcome in &result.outcomes {
        let note_id = outcome
            .note_id
            .as_deref()
            .map_or(String::new(), |id| format!(" [{id}]"));
        out.line(format!(
            "  #{} {}{} — guid {:?}, колода {}, заметка {}",
            outcome.note_index,
            outcome.status.as_str(),
            note_id,
            outcome.guid,
            outcome.deck_path,
            outcome.note_position
        ));
        out.line(format!(
            "      теги: {} → {}",
            if outcome.previous_tags.is_empty() {
                "—".to_string()
            } else {
                outcome.previous_tags.join(", ")
            },
            if outcome.tags.is_empty() {
                "—".to_string()
            } else {
                outcome.tags.join(", ")
            }
        ));
    }
    if result.outcomes_truncated {
        out.line(format!(
            "  … в отчёт попало только {} заметок из {}",
            result.outcomes.len(),
            result.notes_total
        ));
    }

    out.blank();
    out.line("Проверки:");
    for (name, value) in [
        ("source_canonical", result.checks.source_canonical),
        ("candidate_reparsed", result.checks.candidate_reparsed),
        ("only_tags_appended", result.checks.only_tags_appended),
        (
            "tags_appended_verified",
            result.checks.tags_appended_verified,
        ),
        (
            "retired_notes_still_resolvable",
            result.checks.retired_notes_still_resolvable,
        ),
    ] {
        out.line(format!("  {name}: {}", yes_no(value)));
    }

    push_validation(&mut out, &result.validation);
    out.finish()
}

/// Человекочитаемое представление `visual-report`.
pub fn visual_report(result: &VisualReportResult) -> String {
    let mut out = Out::default();

    out.line(format!("До: {}", result.before.export_dir.display()));
    out.line(format!("После: {}", result.after.export_dir.display()));
    out.line(format!("Отчёт: {}", result.index_html.display()));
    out.line(format!(
        "Заметок: {} → {}",
        result.before.notes, result.after.notes
    ));
    out.line(format!(
        "Файлов превью: {} (в списке {})",
        result.card_files_total,
        result.card_files.len()
    ));
    if let Some(tag) = &result.retire_tag {
        out.line(format!("Тег вывода из обращения: {tag:?}"));
    }

    out.blank();
    out.line("Изменения:");
    for (name, value) in [
        ("created", result.counts.created),
        ("changed", result.counts.changed),
        ("retired", result.counts.retired),
        ("removed", result.counts.removed),
        ("unchanged", result.counts.unchanged),
        ("ambiguous", result.counts.ambiguous),
    ] {
        out.line(format!("  {name}: {value}"));
    }

    // Заметки без изменений в список не попадают: их число уже названо выше, а
    // список из сотен одинаковых строк не читается человеком. В `--json` они
    // остаются, чтобы машинный потребитель видел полный состав.
    let listed: Vec<_> = result
        .outcomes
        .iter()
        .filter(|outcome| outcome.kind != NoteChangeKind::Unchanged)
        .collect();

    out.blank();
    out.line(format!(
        "Заметки в отчёте (показано {}, без изменений пропущено {}):",
        listed.len(),
        result.counts.unchanged
    ));
    if listed.is_empty() {
        out.line("  —");
    }
    {
        for outcome in &listed {
            let fields = if outcome.changed_fields.is_empty() {
                String::new()
            } else {
                format!("; поля: {}", outcome.changed_fields.join(", "))
            };
            out.line(format!(
                "  {} guid {:?} — колода {}{fields}",
                outcome.kind.as_str(),
                outcome.guid,
                if outcome.deck_path.is_empty() {
                    "—"
                } else {
                    &outcome.deck_path
                }
            ));
        }
    }
    if result.outcomes_truncated {
        // Знаменатель — число классифицированных заметок, а не сумма размеров
        // двух состояний: заметка, присутствующая в обоих, считалась бы дважды.
        let classified = result.counts.created
            + result.counts.changed
            + result.counts.retired
            + result.counts.removed
            + result.counts.unchanged
            + result.counts.ambiguous;
        out.line(format!(
            "  … в JSON-результат попало только {} заметок из {classified}",
            result.outcomes.len()
        ));
    }

    out.blank();
    out.line(format!("Диагностика ({}):", result.diagnostics.len()));
    if result.diagnostics.is_empty() {
        out.line("  —");
    }
    for diagnostic in &result.diagnostics {
        out.line(format!(
            "  [{}] {} — {}",
            diagnostic.severity, diagnostic.code, diagnostic.message
        ));
    }

    out.blank();
    out.line(format!(
        "Media: скопировано {}, отсутствует {}, вне экспорта {}, символических ссылок {}, слишком больших {}",
        result.media.copied,
        result.media.missing.len(),
        result.media.remote.len(),
        result.media.symlinks.len(),
        result.media.oversized.len()
    ));

    if !result.unsupported_constructs.is_empty() {
        out.blank();
        out.line(format!(
            "Неподдержанные конструкции затронутых моделей ({}):",
            result.unsupported_constructs.len()
        ));
        for item in &result.unsupported_constructs {
            out.line(format!("  {} — {}", item.construct, item.reason));
        }
    }

    out.blank();
    out.line("Ограничения отчёта:");
    for limitation in &result.limitations {
        out.line(format!("  — {limitation}"));
    }

    out.blank();
    out.line("Проверки:");
    for (name, value) in [
        ("before_parsed", result.checks.before_parsed),
        ("after_parsed", result.checks.after_parsed),
        ("every_note_classified", result.checks.every_note_classified),
        ("out_dir_outside_decks", result.checks.out_dir_outside_decks),
        (
            "all_files_inside_out_dir",
            result.checks.all_files_inside_out_dir,
        ),
        (
            "every_generated_page_offline",
            result.checks.every_generated_page_offline,
        ),
        (
            "index_without_external_assets",
            result.checks.index_without_external_assets,
        ),
        (
            "media_confined_to_out_dir",
            result.checks.media_confined_to_out_dir,
        ),
    ] {
        out.line(format!("  {name}: {}", yes_no(value)));
    }

    out.finish()
}

/// Печатает строку режима записи для мутирующих команд.
fn mode_line(applied: bool, dry_run: bool) -> &'static str {
    if applied {
        "Режим: --apply, deck.json заменён атомарно"
    } else if dry_run {
        "Режим: dry-run, deck.json не изменён"
    } else {
        "Режим: --apply, запись не требовалась"
    }
}

/// Печатает сравнение валидации до и после.
fn push_validation(out: &mut Out, validation: &crate::ops::source::ValidationDelta) {
    out.blank();
    out.line(format!(
        "Валидация: ошибок {} → {}, предупреждений {} → {}, info {} → {}",
        validation.before.errors,
        validation.after.errors,
        validation.before.warnings,
        validation.after.warnings,
        validation.before.info,
        validation.after.info
    ));
    if !validation.new_error_codes.is_empty() {
        out.line(format!(
            "  новые коды ошибок: {}",
            validation.new_error_codes.join(", ")
        ));
    }
    if !validation.new_warning_codes.is_empty() {
        out.line(format!(
            "  новые коды предупреждений: {}",
            validation.new_warning_codes.join(", ")
        ));
    }
}

/// Имя вида модели для человекочитаемого вывода.
fn model_kind_name(kind: crate::template::ModelKind) -> &'static str {
    match kind {
        crate::template::ModelKind::Standard => "standard",
        crate::template::ModelKind::Cloze => "cloze",
    }
}

/// Человекочитаемое представление `migrate-media`.
pub fn migrate_media(result: &MigrationResult) -> String {
    let mut out = Out::default();

    out.line(format!("Экспорт: {}", result.export_dir.display()));
    out.line(format!("deck.json: {}", result.deck_json.display()));
    out.line(mode_line(result.applied, result.dry_run));
    out.line(format!(
        "Идентичность: {} / {}",
        result.identity.namespace, result.identity.key
    ));
    out.line(format!(
        "Имя файла: {} → {} (SHA-256 канонических байтов {}, размещение {})",
        result.legacy_filename,
        result.canonical_filename,
        result.canonical_sha256,
        result.canonical_action
    ));
    out.line(format!("Доказанных ссылок: {}", result.references_total));
    if result.references_truncated {
        out.line(format!("  показаны первые {}", result.references.len()));
    }
    for reference in &result.references {
        out.line(format!(
            "  guid {} — поле {} (ord {}), модель {} ({})",
            reference.guid.as_deref().unwrap_or("—"),
            reference.field,
            reference.field_ord,
            reference.model_name.as_deref().unwrap_or("—"),
            reference.model_uuid
        ));
    }
    out.blank();
    out.line(format!(
        "media_files: добавлено {}, снято {}",
        result.media_files_added.len(),
        result.media_files_removed.len()
    ));
    for name in &result.media_files_added {
        out.line(format!("  + {name}"));
    }
    for name in &result.media_files_removed {
        out.line(format!("  - {name}"));
    }
    match &result.legacy_media {
        Some(digest) => {
            out.line(format!(
                "Прежний файл до миграции: media/{} — {} байт, sha256 {}",
                digest.filename, digest.byte_length, digest.sha256
            ));
        }
        None => {
            out.line(format!(
                "Прежний файл до миграции: media/{} физически отсутствовал",
                result.legacy_filename
            ));
        }
    }
    if result.legacy_released {
        out.line("После применения legacy-имя освобождено");
    } else if result.legacy_media_exists_after {
        out.line("После завершения команды legacy-файл существует");
    } else {
        out.line("После завершения команды legacy-файл отсутствует");
    }
    if !result.changed {
        out.line("Мигрировать нечего: повторный прогон ничего не меняет");
    }

    push_validation(&mut out, &result.validation);
    out.finish()
}

/// Человекочитаемое представление `migrate-layout`.
pub fn migrate_layout(result: &MigrationReport) -> String {
    let mut out = Out::default();

    out.line(format!("Корень репозитория: {}", result.root));
    out.line(format!(
        "Прежняя область: {} → плоское назначение: {}",
        result.legacy_area, result.output_dir
    ));
    out.line(if result.apply {
        if result.refused {
            "Режим: --apply, перенос отклонён целиком".to_string()
        } else if result.applied {
            "Режим: --apply, перенос выполнен".to_string()
        } else {
            "Режим: --apply, перенос не требовался".to_string()
        }
    } else {
        "Режим: dry-run, файлы не изменялись".to_string()
    });

    if result.exports.is_empty() {
        out.line("Экспортов прежней раскладки не найдено");
    } else {
        out.line(format!("Экспортов рассмотрено: {}", result.summary.exports));
        out.blank();
        for entry in &result.exports {
            let name = entry.name.as_deref().unwrap_or("—");
            out.line(format!("{name}: {} → {}", entry.source, action_line(entry)));
            if entry.state == "applied" || entry.action == "already_present" {
                out.line(format!(
                    "  файлов {}, байт {}, sha256 {}",
                    entry.files,
                    entry.bytes,
                    optional(entry.content_sha256.as_deref())
                ));
            }
            if let Some(reason) = entry.reason {
                out.line(format!(
                    "  отказ {reason}: {}",
                    optional(entry.message.as_deref())
                ));
            } else if let Some(message) = entry.message.as_deref() {
                out.line(format!("  {message}"));
            }
        }
    }

    if !result.pruned.is_empty() {
        out.blank();
        out.line(format!(
            "Снято элементов прежней раскладки: {}",
            result.pruned.len()
        ));
        for path in &result.pruned {
            out.line(format!("  - {path}"));
        }
    }

    for diagnostic in &result.diagnostics {
        out.blank();
        out.line(format!("{}: {}", diagnostic.code, diagnostic.message));
        for path in &diagnostic.paths {
            out.line(format!("  {path}"));
        }
    }

    out.blank();
    out.line(format!(
        "Итог: к переносу {}, перенесено {}, уже на месте {}, отказов {}",
        result.summary.planned,
        result.summary.applied,
        result.summary.already_present,
        result.summary.refused
    ));
    out.finish()
}

/// Строка исхода одного экспорта для человекочитаемого режима.
fn action_line(entry: &ExportPlan) -> String {
    let target = optional(entry.target.as_deref());
    match (entry.action, entry.state) {
        ("move", "applied") => format!("{target} (перенесён)"),
        ("move", _) => format!("{target} (к переносу)"),
        ("already_present", "applied") => {
            format!("{target} (уже на месте, дубликат источника снят)")
        }
        ("already_present", _) => format!("{target} (уже на месте)"),
        _ => "перенос невозможен".to_string(),
    }
}
