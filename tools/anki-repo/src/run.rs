//! Выбор операции и подготовка вывода: единственное место, где CLI встречается
//! с предметной логикой.
//!
//! [`execute`] полностью готовит stdout и код возврата команды, не обращаясь
//! напрямую к стандартным потокам процесса, поэтому контракт можно проверять
//! без отдельного процесса. Единственное чтение из стандартного ввода —
//! `edit --request -`.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::cli::{Cli, Command, MaintenanceAction, MatchArg};
use crate::error::{DomainError, ErrorCode};
use crate::index::ExportIndex;
use crate::loader::load_export;
use crate::ops::create as create_op;
use crate::ops::deck_select::DeckSelector;
use crate::ops::edit as edit_op;
use crate::ops::edit::{EditRequest, EditSpec, STDIN_REQUEST_SOURCE};
use crate::ops::find as find_op;
use crate::ops::find::{FindCriteria, FindQuery, MatchMode};
use crate::ops::inspect as inspect_op;
use crate::ops::migrate_layout as migrate_layout_op;
use crate::ops::migrate_media as migrate_media_op;
use crate::ops::models as models_op;
use crate::ops::models::ModelsQuery;
use crate::ops::qa as qa_op;
use crate::ops::qa::QaQuery;
use crate::ops::retire as retire_op;
use crate::ops::retire::{RetireRequest, RetireSpec};
use crate::ops::review as review_op;
use crate::ops::review::{ReviewCriteria, ReviewQuery};
use crate::ops::review_check as review_check_op;
use crate::ops::source as source_op;
use crate::ops::stats::{StatsQuery, stats as stats_op};
use crate::ops::validate::validate as validate_op;
use crate::ops::visual_report as visual_report_op;
use crate::ops::visual_report::ReportRequest;
use crate::proposal;
use crate::render::{human, json};
use repository_maintenance::care;

/// Полностью подготовленный к печати результат команды.
#[derive(Debug)]
pub struct Rendered {
    /// Имя команды для оболочки JSON.
    pub command: &'static str,
    /// Текст, который должен попасть в stdout.
    pub stdout: String,
    /// Код возврата при успешном выполнении.
    pub exit: u8,
}

/// Выполняет команду и готовит её вывод.
///
/// # Ошибки
///
/// Возвращает [`DomainError`] для любой доменной проблемы.
pub fn execute_cli(cli: &Cli) -> Result<Rendered, DomainError> {
    execute(cli)
}

/// Выполняет команду и готовит результат без обращения к стандартным потокам.
pub fn execute(cli: &Cli) -> Result<Rendered, DomainError> {
    // Граница закрытой области проверяется до предметной работы: ни одна команда
    // не должна успеть открыть `decks/input`.
    crate::layout::guard_command(&cli.command)?;

    match &cli.command {
        Command::Maintenance {
            action,
            workspace_root,
        } => {
            let root =
                repository_maintenance::policy::workspace_root(workspace_root.as_deref(), false)
                    .map_err(|e| DomainError::new(ErrorCode::InputUnreadable, e))?;
            match action {
                MaintenanceAction::Status => (),
                MaintenanceAction::Enable => {
                    care::enable(&root).map_err(|e| DomainError::new(ErrorCode::WriteFailed, e))?
                }
                MaintenanceAction::Disable => {
                    care::disable(&root).map_err(|e| DomainError::new(ErrorCode::WriteFailed, e))?
                }
            }
            let status = care::inspect(&root);
            Ok(Rendered {
                command: "maintenance",
                stdout: if cli.json {
                    json::generic_json("maintenance", &status)
                } else {
                    care::render_text(&status)
                },
                exit: 0,
            })
        }
        Command::Inspect {
            export_dir,
            verbose,
        } => {
            let loaded = load_export(export_dir)?;
            let result = inspect_op::inspect(
                &loaded.export_dir,
                &loaded.deck_json,
                &loaded.root,
                *verbose,
            );
            Ok(Rendered {
                command: "inspect",
                stdout: if cli.json {
                    json::inspect_json(&result)
                } else {
                    human::inspect(&result)
                },
                exit: 0,
            })
        }

        Command::Find {
            export_dir,
            guid,
            field,
            value,
            match_mode,
            deck,
            limit,
        } => {
            let loaded = load_export(export_dir)?;
            let index = ExportIndex::build(&loaded.root);
            let query = FindQuery {
                criteria: build_criteria(
                    guid.as_deref(),
                    field.as_deref(),
                    value.as_deref(),
                    *match_mode,
                )?,
                deck: deck.clone(),
                limit: usize::try_from(*limit).unwrap_or(usize::MAX),
            };
            let result = find_op::find(&loaded.export_dir, &index, &query)?;
            Ok(Rendered {
                command: "find",
                stdout: if cli.json {
                    json::find_json(&result)
                } else {
                    human::find(&result)
                },
                exit: 0,
            })
        }

        Command::Stats {
            export_dir,
            group_by,
            top,
        } => {
            let loaded = load_export(export_dir)?;
            let index = ExportIndex::build(&loaded.root);
            let query = StatsQuery {
                group_by: group_by.clone(),
                top: usize::try_from(top.unwrap_or(crate::cli::DEFAULT_TOP)).unwrap_or(usize::MAX),
            };
            let result = stats_op(&loaded.export_dir, &index, &query)?;
            Ok(Rendered {
                command: "stats",
                stdout: if cli.json {
                    json::stats_json(&result)
                } else {
                    human::stats(&result)
                },
                exit: 0,
            })
        }

        Command::Validate { export_dir } => {
            let result = validate_op(export_dir)?;
            Ok(Rendered {
                command: "validate",
                stdout: if cli.json {
                    json::validate_json(&result)
                } else {
                    human::validate(&result)
                },
                exit: if result.valid { 0 } else { 6 },
            })
        }

        Command::Qa {
            export_dir,
            codes,
            max_per_code,
        } => {
            let loaded = load_export(export_dir)?;
            let index = ExportIndex::build(&loaded.root);
            let query = QaQuery {
                codes: codes.clone(),
                max_per_code: to_usize(*max_per_code),
            };
            let result = qa_op::qa(&loaded.export_dir, &index, &query)?;
            Ok(Rendered {
                command: "qa",
                stdout: if cli.json {
                    json::qa_json(&result)
                } else {
                    human::qa(&result)
                },
                exit: 0,
            })
        }

        Command::Review {
            export_dir,
            all,
            guid,
            field,
            value,
            match_mode,
            qa_code,
            deck,
            offset,
            limit,
        } => {
            let loaded = load_export(export_dir)?;
            let index = ExportIndex::build(&loaded.root);
            let query = ReviewQuery {
                criteria: build_review_criteria(
                    *all,
                    guid.as_deref(),
                    field.as_deref(),
                    value.as_deref(),
                    *match_mode,
                    qa_code.as_deref(),
                )?,
                deck: deck.clone(),
                offset: to_usize(*offset),
                limit: to_usize(*limit),
            };
            let result = review_op::review(&loaded.export_dir, &index, &query)?;
            Ok(Rendered {
                command: "review",
                stdout: if cli.json {
                    json::review_json(&result)
                } else {
                    human::review(&result)
                },
                exit: 0,
            })
        }

        Command::ReviewCheck {
            export_dir,
            proposals_file,
        } => {
            let label = proposals_file.display().to_string();
            let raw = read_document(
                proposals_file,
                proposal::MAX_PROPOSAL_BYTES,
                "документ предложений",
            )?;
            let document = proposal::parse_proposal_bytes(&raw, &label)?;

            // Тот же набор проверок исходника, что и в мутирующем `edit`:
            // запрос выпускается только там, где граница записи приняла бы этот
            // экспорт. Иначе `review-check` обещал бы запрос, который `edit`
            // отклонит из-за неканонического или невалидного источника.
            let source = source_op::read_source(export_dir)?;
            let blockers = source_op::source_blockers(&source);
            let index = ExportIndex::build(&source.root);
            let result = review_check_op::review_check(export_dir, &index, &document, &blockers)?;
            Ok(Rendered {
                command: "review-check",
                stdout: if cli.json {
                    json::review_check_json(&result)
                } else {
                    human::review_check(&result)
                },
                exit: result.exit_code,
            })
        }

        Command::Models {
            export_dir,
            deck,
            deck_uuid,
            deck_preorder,
            sample_limit,
        } => {
            let query = ModelsQuery {
                deck: deck_selector(deck.clone(), deck_uuid.clone(), *deck_preorder),
                sample_limit: to_usize(*sample_limit),
            };
            let result = models_op::inspect(export_dir, &query)?;
            Ok(Rendered {
                command: "models",
                stdout: if cli.json {
                    json::models_json(&result)
                } else {
                    human::models(&result)
                },
                exit: 0,
            })
        }

        Command::Create {
            export_dir,
            request_file,
            apply,
            emit_resolved,
            create_config,
            asset_store,
            pitch_asset_store,
        } => {
            let raw = read_document(request_file, create_op::MAX_REQUEST_BYTES, "запрос")?;
            let label = request_file.display().to_string();
            let request = create_op::parse_request_bytes(&raw, &label)?;
            // Разрешённый запрос публикуется внутри самой операции и до
            // мутации экспорта: см. `create_op::create`.
            let result = create_op::create_with_options(
                export_dir,
                &request,
                *apply,
                emit_resolved.as_deref(),
                &crate::ops::create_media::MediaOptions {
                    config: create_config.clone(),
                    asset_store: asset_store.clone(),
                    pitch_asset_store: pitch_asset_store.clone(),
                },
            )?;
            Ok(Rendered {
                command: "create",
                stdout: if cli.json {
                    json::create_json(&result)
                } else {
                    human::create(&result)
                },
                exit: 0,
            })
        }

        Command::MigrateMedia {
            export_dir,
            namespace,
            key,
            from,
            apply,
            create_config,
            asset_store,
            pitch_asset_store,
        } => {
            let request = migrate_media_op::MigrationRequest {
                namespace: namespace.clone(),
                key: key.clone(),
                legacy_filename: from.clone(),
            };
            let result = migrate_media_op::migrate(
                export_dir,
                &request,
                &crate::ops::create_media::MediaOptions {
                    config: create_config.clone(),
                    asset_store: asset_store.clone(),
                    pitch_asset_store: pitch_asset_store.clone(),
                },
                *apply,
            )?;
            Ok(Rendered {
                command: "migrate-media",
                stdout: if cli.json {
                    json::migrate_media_json(&result)
                } else {
                    human::migrate_media(&result)
                },
                exit: 0,
            })
        }

        Command::Retire {
            export_dir,
            request_file,
            guid,
            tag,
            apply,
        } => {
            let request = build_retire_request(request_file.as_deref(), guid, tag.as_deref())?;
            let result = retire_op::retire(export_dir, &request, *apply)?;
            Ok(Rendered {
                command: "retire",
                stdout: if cli.json {
                    json::retire_json(&result)
                } else {
                    human::retire(&result)
                },
                exit: 0,
            })
        }

        Command::VisualReport {
            before,
            after,
            out,
            retire_tag,
            preview_limit,
        } => {
            let request = ReportRequest {
                before: before.clone(),
                after: after.clone(),
                out: out.clone(),
                retire_tag: retire_tag.clone(),
                preview_limit: to_usize(*preview_limit),
            };
            let result = visual_report_op::report(&request)?;
            Ok(Rendered {
                command: "visual-report",
                stdout: if cli.json {
                    json::visual_report_json(&result)
                } else {
                    human::visual_report(&result)
                },
                exit: 0,
            })
        }

        Command::Edit {
            export_dir,
            request_file,
            guid,
            field,
            set,
            expect,
            apply,
        } => {
            let request = build_edit_request(
                request_file.as_deref(),
                guid.as_deref(),
                field.as_deref(),
                set.as_deref(),
                expect.as_deref(),
            )?;
            let result = edit_op::edit(export_dir, &request, *apply)?;
            Ok(Rendered {
                command: "edit",
                stdout: if cli.json {
                    json::edit_json(&result)
                } else {
                    human::edit(&result)
                },
                exit: 0,
            })
        }

        Command::MigrateLayout { root, apply } => {
            let result = migrate_layout_op::migrate(root.as_deref(), *apply)?;
            Ok(Rendered {
                command: "migrate-layout",
                stdout: if cli.json {
                    json::migrate_layout_json(&result)
                } else {
                    human::migrate_layout(&result)
                },
                exit: result.exit_code(),
            })
        }
    }
}

/// Собирает селектор колоды из аргументов CLI.
///
/// Отсутствие всех трёх аргументов означает корневую колоду экспорта
/// (`preorder = 0`): у CrowdAnki-экспорта один корневой узел, и читать «модели
/// экспорта» без адреса колоды означает именно его. Запись такого умолчания не
/// имеет: `create` берёт колоду каждой заметки из самого запроса.
fn deck_selector(
    path: Option<String>,
    crowdanki_uuid: Option<String>,
    preorder: Option<usize>,
) -> DeckSelector {
    let selector = DeckSelector {
        path,
        crowdanki_uuid,
        preorder,
    };
    if selector.is_empty() {
        return DeckSelector {
            preorder: Some(0),
            ..DeckSelector::default()
        };
    }
    selector
}

/// Собирает запрос на вывод заметок из обращения из аргументов CLI.
///
/// `--request` читается целиком и разбирается; `-` означает stdin. Одиночные
/// `--guid` вместе с `--tag` дают тот же запрос, что и файл, поэтому обе формы
/// проходят одни и те же проверки владельца операции.
fn build_retire_request(
    request_file: Option<&Path>,
    guid: &[String],
    tag: Option<&str>,
) -> Result<RetireRequest, DomainError> {
    if let Some(path) = request_file {
        let label = path.display().to_string();
        let raw = read_document(path, retire_op::MAX_REQUEST_BYTES, "запрос")?;
        return retire_op::parse_request_bytes(&raw, &label);
    }

    let (Some(tag), false) = (tag, guid.is_empty()) else {
        return Err(DomainError::new(
            ErrorCode::Usage,
            "для вывода из обращения нужен либо --request, либо --tag с хотя бы одним --guid",
        ));
    };

    let request = RetireRequest {
        tag: tag.to_string(),
        notes: guid
            .iter()
            .map(|guid| RetireSpec {
                note_id: None,
                guid: guid.clone(),
            })
            .collect(),
    };
    retire_op::validate_request(&request)?;
    Ok(request)
}

/// Собирает запрос на правку из аргументов CLI.
///
/// `--request` читается целиком и разбирается; `-` означает stdin.
fn build_edit_request(
    request_file: Option<&Path>,
    guid: Option<&str>,
    field: Option<&str>,
    set: Option<&str>,
    expect: Option<&str>,
) -> Result<EditRequest, DomainError> {
    if let Some(path) = request_file {
        let label = path.display().to_string();
        let raw = read_document(path, edit_op::MAX_REQUEST_BYTES, "запрос")?;
        return edit_op::parse_request_bytes(&raw, &label);
    }

    let (Some(guid), Some(field), Some(set), Some(expect)) = (guid, field, set, expect) else {
        return Err(DomainError::new(
            ErrorCode::Usage,
            "для правки нужен либо --request, либо --guid с --field, --set и --expect",
        ));
    };

    let request = EditRequest {
        edits: vec![EditSpec {
            edit_id: None,
            guid: guid.to_string(),
            field: field.to_string(),
            expected: expect.to_string(),
            replacement: set.to_string(),
        }],
    };
    edit_op::validate_request(&request)?;
    Ok(request)
}

/// Читает документ запроса из файла или stdin.
///
/// `max_bytes` — предел размера документа, объявленный его владельцем
/// (`edit` и `review-check` используют один и тот же предел запроса правки).
fn read_document(path: &Path, max_bytes: usize, what: &str) -> Result<Vec<u8>, DomainError> {
    let label = path.display().to_string();
    let limit = document_read_limit(max_bytes);
    if label == STDIN_REQUEST_SOURCE {
        return read_bounded(std::io::stdin().lock(), limit)
            .map_err(|error| document_read_error(what, STDIN_REQUEST_SOURCE, &error));
    }

    let file =
        File::open(path).map_err(|error| document_read_error(what, label.as_str(), &error))?;
    read_bounded(file, limit).map_err(|error| document_read_error(what, label.as_str(), &error))
}

/// Предел чтения документа: максимум размера плюс один байт.
///
/// Лишний байт нужен, чтобы владелец документа отличил «ровно предел» от
/// «больше предела» по длине буфера.
const fn document_read_limit(max_bytes: usize) -> u64 {
    max_bytes as u64 + 1
}

/// Читает не более `limit` байтов.
///
/// Предел размера документа проверяется по длине буфера, поэтому неограниченное
/// чтение успело бы занять память под весь входной поток прежде, чем команда
/// сообщила бы о превышении.
fn read_bounded<R: Read>(reader: R, limit: u64) -> std::io::Result<Vec<u8>> {
    let mut raw = Vec::new();
    reader.take(limit).read_to_end(&mut raw)?;
    Ok(raw)
}

/// Готовит ошибку нечитаемого документа запроса.
fn document_read_error(what: &str, label: &str, error: &std::io::Error) -> DomainError {
    DomainError::with_details(
        ErrorCode::InputUnreadable,
        format!("не удалось прочитать {what} {label}: {error}"),
        crate::details! {
            "path" => label,
            "io_error" => error.to_string(),
        },
    )
}

fn build_criteria(
    guid: Option<&str>,
    field: Option<&str>,
    value: Option<&str>,
    match_mode: Option<MatchArg>,
) -> Result<FindCriteria, DomainError> {
    // Проверяется здесь, а не только в clap: --match осмысленен исключительно
    // вместе с --field.
    if match_mode.is_some() && field.is_none() {
        return Err(DomainError::new(
            ErrorCode::Usage,
            "аргумент --match применяется только вместе с --field",
        ));
    }
    if let Some(guid) = guid {
        return Ok(FindCriteria::Guid {
            guid: guid.to_string(),
        });
    }
    if let Some(field) = field {
        return Ok(FindCriteria::Field {
            field: field.to_string(),
            value: value.unwrap_or_default().to_string(),
            mode: match match_mode {
                Some(MatchArg::Exact) => MatchMode::Exact,
                _ => MatchMode::Contains,
            },
        });
    }
    Err(DomainError::new(
        ErrorCode::Usage,
        "не задан критерий поиска: нужен --guid или --field",
    ))
}

/// Приводит счётчик CLI к `usize` без паники.
///
/// Значения уже ограничены clap, а насыщение здесь защищает от 32-битной
/// платформы, где `u64` может не поместиться в `usize`.
const fn to_usize(value: u64) -> usize {
    if value > usize::MAX as u64 {
        usize::MAX
    } else {
        value as usize
    }
}

/// Собирает критерий выбора для `review`.
///
/// `review` и `find` принимают одинаковые критерии по полю: `--field` со
/// значением и, необязательно, `--match`. Взаимная исключительность аргументов
/// проверяется clap, а зависимость `--match` от `--field` — здесь, как и в
/// `find`.
fn build_review_criteria(
    all: bool,
    guid: Option<&str>,
    field: Option<&str>,
    value: Option<&str>,
    match_mode: Option<MatchArg>,
    qa_code: Option<&str>,
) -> Result<ReviewCriteria, DomainError> {
    if match_mode.is_some() && field.is_none() {
        return Err(DomainError::new(
            ErrorCode::Usage,
            "аргумент --match применяется только вместе с --field",
        ));
    }
    if all {
        return Ok(ReviewCriteria::All);
    }
    if let Some(guid) = guid {
        return Ok(ReviewCriteria::Guid {
            guid: guid.to_string(),
        });
    }
    if let Some(code) = qa_code {
        return Ok(ReviewCriteria::QaCode {
            code: code.to_string(),
        });
    }
    if let Some(field) = field {
        return Ok(ReviewCriteria::Field {
            field: field.to_string(),
            value: value.unwrap_or_default().to_string(),
            mode: match match_mode {
                Some(MatchArg::Exact) => MatchMode::Exact,
                _ => MatchMode::Contains,
            },
        });
    }
    Err(DomainError::new(
        ErrorCode::Usage,
        "не задан критерий отбора: нужен один из --all, --guid, --field или --qa-code",
    ))
}

/// Готовит stdout/stderr для доменной ошибки.
///
/// В режиме JSON stdout содержит только один корректный документ JSON, а stderr
/// остаётся пустым. В обычном режиме сообщение уходит в stderr, а stdout пуст.
pub fn render_error(command: &str, json_mode: bool, error: &DomainError) -> (String, String) {
    if json_mode {
        (json::error_json(command, error), String::new())
    } else {
        (
            String::new(),
            format!(
                "anki-repo: ошибка [{code}]: {message}\n",
                code = error.code.as_str(),
                message = error.message
            ),
        )
    }
}

/// Ближайший родительский каталог экспорта, который является репозиторием.
///
/// Владение экспортом определяется его расположением, а не текущим каталогом
/// процесса: иначе команда над экспортом одного репозитория работала бы с
/// границами другого.
pub fn export_repository_root(export_dir: &Path) -> Option<std::path::PathBuf> {
    let export = std::fs::canonicalize(export_dir).ok()?;
    export
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Бесконечный reader: если бы чтение не было ограничено, тест не завершился
    /// бы, поэтому это прямая проверка предела, а не косвенная.
    #[test]
    fn request_read_stops_at_the_limit() {
        let limit = document_read_limit(edit_op::MAX_REQUEST_BYTES);
        let raw = read_bounded(std::io::repeat(b'a'), limit).expect("чтение повторов");

        assert_eq!(raw.len() as u64, limit);
        assert!(
            raw.len() > edit_op::MAX_REQUEST_BYTES,
            "лишний байт нужен, чтобы отличить ровно предел от превышения"
        );
    }

    #[test]
    fn request_read_keeps_short_input_intact() {
        let raw = read_bounded(
            &b"{\"schema_version\": 1}"[..],
            document_read_limit(edit_op::MAX_REQUEST_BYTES),
        )
        .expect("чтение");

        assert_eq!(raw, b"{\"schema_version\": 1}".to_vec());
    }

    #[test]
    fn request_read_keeps_the_boundary_exactly_at_the_limit() {
        let source = vec![b' '; edit_op::MAX_REQUEST_BYTES];
        let raw = read_bounded(&source[..], document_read_limit(edit_op::MAX_REQUEST_BYTES))
            .expect("чтение");

        assert_eq!(raw.len(), edit_op::MAX_REQUEST_BYTES);
        assert!(
            raw.len() <= edit_op::MAX_REQUEST_BYTES,
            "ровно предел не считается превышением"
        );
    }
}
