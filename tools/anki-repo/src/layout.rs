//! Раскладка локальных пользовательских данных и граница закрытой области.
//!
//! Пользовательские данные репозитория живут вне Git: рабочие колоды — в
//! `decks/output/`, доверенные ресурсы — в `.asset-store/`, а `decks/input/` —
//! закрытая область будущего доверенного загрузчика. Ни одна команда
//! инструмента не адресует `decks/input`: ни как источник, ни как назначение.
//!
//! Путь проверяется по факту, а не по написанию: `decks/input`, `./decks/../decks/input`
//! и символическая ссылка на тот же каталог обязаны дать один ответ. Для этого
//! используется [`crate::paths::canonical_ish`], который разбирает `.`/`..` и
//! раскрывает ссылки в существующей части пути.
//!
//! Это **граница API**, а не изоляция прав ОС. Тот же UID по-прежнему может
//! прочитать файл другими средствами; инструмент отвечает только за то, что его
//! собственные операции не открывают закрытую область.

use std::path::{Component, Path, PathBuf};

use crate::cli::Command;
use crate::details;
use crate::error::{DomainError, ErrorCode};
use crate::paths::canonical_ish;

/// Каталог локальных данных репозитория.
pub const DECKS_DIR: &str = "decks";
/// Закрытая область будущего доверенного загрузчика.
pub const INPUT_DIR: &str = "input";
/// Плоский список рабочих локальных колод.
pub const OUTPUT_DIR: &str = "output";
/// Локальное доверенное хранилище ресурсов.
pub const ASSET_STORE_DIR: &str = ".asset-store";

/// Корень репозитория, которому принадлежит путь.
///
/// Владение определяется расположением самого пути, а не текущим каталогом
/// процесса: иначе команда над данными одного репозитория применяла бы границы
/// другого. Функция работает и для ещё не существующего пути (`--out` отчёта,
/// каталог, который только будет создан), поэтому вместо [`std::fs::canonicalize`]
/// используется [`canonical_ish`].
#[must_use]
pub fn repository_root_for(path: &Path) -> Option<PathBuf> {
    canonical_ish(path)
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Каталог закрытой области внутри репозитория.
#[must_use]
pub fn input_dir(repository_root: &Path) -> PathBuf {
    canonical_ish(&repository_root.join(DECKS_DIR).join(INPUT_DIR))
}

/// Плоский каталог рабочих колод внутри репозитория.
#[must_use]
pub fn output_dir(repository_root: &Path) -> PathBuf {
    canonical_ish(&repository_root.join(DECKS_DIR).join(OUTPUT_DIR))
}

/// Адресует ли путь закрытую область `decks/input` этого репозитория.
///
/// Сравнение идёт по [`canonical_ish`], поэтому покрывает относительность,
/// `.`/`..` внутри пути и символические ссылки — как в самом пути, так и в
/// компоненте `decks/input` репозитория.
#[must_use]
pub fn is_inside_input(path: &Path, repository_root: &Path) -> bool {
    let input = input_dir(repository_root);
    let canonical = canonical_ish(path);
    canonical == input || canonical.starts_with(&input)
}

/// Корни репозиториев, чьи границы обязан учитывать этот путь.
///
/// Кроме репозитория самого пути учитывается текущий каталог процесса: закрытая
/// область может быть достигнута не только через путь внутри репозитория, но и
/// через символическую ссылку `decks/input`, ведущую наружу, — тогда путь к цели
/// ссылки уже не содержит `.git` и сам по себе корень не выдаёт. Явное указание
/// окружением `ANKI_REPOSITORY_ROOT` учитывается первым как осознанное решение
/// оператора.
fn candidate_repository_roots(path: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    if let Some(explicit) = std::env::var_os("ANKI_REPOSITORY_ROOT") {
        roots.push(canonical_ish(Path::new(&explicit)));
    }
    if let Ok(current) = std::env::current_dir() {
        roots.extend(repository_roots_for(&current));
    }
    roots.extend(repository_roots_for(path));

    roots.sort();
    roots.dedup();
    roots
}

/// Все корни Git-репозиториев по пути, от ближайшего до внешнего.
///
/// Вложенный `.git` внутри закрытой области не отменяет границу внешнего
/// репозитория. Проверяем также написание пути до раскрытия symlink, чтобы
/// сохранить исходную связь с родительским каталогом.
fn repository_roots_for(path: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for candidate in [lexical_absolute(path), canonical_ish(path)] {
        for directory in candidate.ancestors() {
            if directory.join(".git").exists() {
                roots.push(canonical_ish(directory));
            }
        }
    }
    roots
}

/// Абсолютный путь с разобранными `.` и `..`, без раскрытия символических ссылок.
fn lexical_absolute(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(name) => normalized.push(name),
        }
    }
    normalized
}

/// Находит закрытую область по самому пути, даже если Git-метаданные недоступны.
fn input_area_from_path(path: &Path) -> Option<(PathBuf, PathBuf)> {
    for candidate in [lexical_absolute(path), canonical_ish(path)] {
        for input in candidate.ancestors() {
            if input.file_name().is_some_and(|name| name == INPUT_DIR)
                && input
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == DECKS_DIR)
                && let Some(root) = input.parent().and_then(Path::parent)
            {
                return Some((root.to_path_buf(), input.to_path_buf()));
            }
        }
    }
    None
}

/// Отказывает, если путь адресует закрытую область `decks/input`.
///
/// `role` — назначение пути в команде (`каталог экспорта`, `каталог отчёта`),
/// чтобы сообщение объясняло, что именно отвергнуто.
///
/// # Ошибки
///
/// Возвращает [`ErrorCode::InvalidRequest`], если путь ведёт внутрь `decks/input`
/// любого репозитория, чья граница применима к этому пути.
pub fn guard_local_path(path: &Path, role: &str) -> Result<(), DomainError> {
    if let Some((root, _input)) = input_area_from_path(path) {
        return Err(input_area_error(path, role, &root));
    }
    for root in candidate_repository_roots(path) {
        if is_inside_input(path, &root) {
            return Err(input_area_error(path, role, &root));
        }
    }
    Ok(())
}

/// Проверяет путь относительно известного корня, даже если у корня нет `.git`.
pub(crate) fn guard_local_path_for_root(
    path: &Path,
    trusted_root: &Path,
    role: &str,
) -> Result<(), DomainError> {
    if let Some((root, _input)) = input_area_from_path(path) {
        return Err(input_area_error(path, role, &root));
    }
    if is_inside_input(path, trusted_root) {
        return Err(input_area_error(path, role, trusted_root));
    }
    guard_local_path(path, role)
}

fn input_area_error(path: &Path, role: &str, root: &Path) -> DomainError {
    DomainError::with_details(
        ErrorCode::InvalidRequest,
        format!(
            "{role} {} адресует закрытую область decks/input: инструмент её не читает и не \
             изменяет; для работы с колодами используйте decks/output",
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

/// Проверяет все пути выбранной команды до начала её работы.
///
/// Единственная точка входа границы: разбор аргументов и предметная логика не
/// дублируют проверку, поэтому новая команда не может её «забыть», если её
/// аргументы перечислены здесь.
///
/// # Ошибки
///
/// Возвращает первую ошибку [`guard_local_path`].
pub fn guard_command(command: &Command) -> Result<(), DomainError> {
    for (role, path) in command_paths(command) {
        guard_local_path(path, role)?;
    }
    Ok(())
}

/// Пути, которые команда получает от пользователя.
fn command_paths(command: &Command) -> Vec<(&'static str, &Path)> {
    let mut paths: Vec<(&'static str, &Path)> = Vec::new();

    match command {
        Command::Maintenance { workspace_root, .. } => {
            paths.extend(
                workspace_root
                    .as_deref()
                    .map(|root| ("корень рабочей области", root)),
            );
        }
        Command::Inspect { export_dir, .. }
        | Command::Find { export_dir, .. }
        | Command::Stats { export_dir, .. }
        | Command::Validate { export_dir, .. }
        | Command::Qa { export_dir, .. }
        | Command::Review { export_dir, .. }
        | Command::Models { export_dir, .. } => paths.push(("каталог экспорта", export_dir)),
        Command::ReviewCheck {
            export_dir,
            proposals_file,
        } => {
            paths.push(("каталог экспорта", export_dir));
            paths.push(("документ предложений", proposals_file));
        }
        Command::Create {
            export_dir,
            request_file,
            emit_resolved,
            create_config,
            asset_store,
            pitch_asset_store,
            ..
        } => {
            paths.push(("каталог экспорта", export_dir));
            paths.push(("документ запроса", request_file));
            paths.extend(emit_resolved.as_deref().map(|path| ("файл запроса", path)));
            paths.extend(
                create_config
                    .as_deref()
                    .map(|path| ("правила создания", path)),
            );
            paths.extend(
                asset_store
                    .as_deref()
                    .map(|path| ("хранилище изображений кандзи", path)),
            );
            paths.extend(
                pitch_asset_store
                    .as_deref()
                    .map(|path| ("хранилище pitch-accent", path)),
            );
        }
        Command::Retire {
            export_dir,
            request_file,
            ..
        } => {
            paths.push(("каталог экспорта", export_dir));
            paths.extend(
                request_file
                    .as_deref()
                    .map(|path| ("документ запроса", path)),
            );
        }
        Command::Edit {
            export_dir,
            request_file,
            ..
        } => {
            paths.push(("каталог экспорта", export_dir));
            paths.extend(
                request_file
                    .as_deref()
                    .map(|path| ("документ запроса", path)),
            );
        }
        Command::MigrateMedia {
            export_dir,
            create_config,
            asset_store,
            pitch_asset_store,
            ..
        } => {
            paths.push(("каталог экспорта", export_dir));
            paths.extend(
                create_config
                    .as_deref()
                    .map(|path| ("правила создания", path)),
            );
            paths.extend(
                asset_store
                    .as_deref()
                    .map(|path| ("хранилище изображений кандзи", path)),
            );
            paths.extend(
                pitch_asset_store
                    .as_deref()
                    .map(|path| ("хранилище pitch-accent", path)),
            );
        }
        Command::VisualReport {
            before, after, out, ..
        } => {
            paths.push(("каталог экспорта «до»", before));
            paths.push(("каталог экспорта «после»", after));
            paths.push(("каталог отчёта", out));
        }
        Command::MigrateLayout { root, .. } => {
            paths.extend(root.as_deref().map(|path| ("корень репозитория", path)));
        }
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_support::TempDir;

    /// Создаёт каталог репозитория с признаком `.git` и закрытой областью.
    fn repository(dir: &TempDir) -> PathBuf {
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join(".git")).expect("признак репозитория");
        std::fs::create_dir_all(root.join("decks").join("input").join("hidden"))
            .expect("закрытая область");
        root
    }

    #[test]
    fn input_is_recognised_through_dot_segments_and_absolute_paths() {
        let dir = TempDir::new("layout-input");
        let root = repository(&dir);
        let inside = root.join("decks").join("input").join("hidden");

        assert!(is_inside_input(&inside, &root));
        assert!(is_inside_input(&root.join("decks/input"), &root));
        assert!(is_inside_input(
            &root.join("decks/output/../input/hidden"),
            &root
        ));
        assert!(is_inside_input(&root.join("decks/input/не_создано"), &root));
    }

    #[test]
    fn output_and_neighbours_are_not_the_closed_area() {
        let dir = TempDir::new("layout-output");
        let root = repository(&dir);

        assert!(!is_inside_input(
            &root.join("decks/output/Words__N3"),
            &root
        ));
        // `inputX` не является компонентом `input`.
        assert!(!is_inside_input(&root.join("decks/inputX"), &root));
        assert!(!is_inside_input(&root.join("decks"), &root));
        assert!(!is_inside_input(&root, &root));
    }

    #[test]
    fn a_symlinked_closed_area_is_still_the_closed_area() {
        let dir = TempDir::new("layout-symlink");
        let root = repository(&dir);
        let outside = dir.path().join("private");
        std::fs::create_dir_all(&outside).expect("внешний каталог");
        std::fs::remove_dir_all(root.join("decks").join("input")).expect("убрать каталог");
        std::os::unix::fs::symlink(&outside, root.join("decks").join("input")).expect("ссылка");

        assert!(is_inside_input(
            &root.join("decks").join("input").join("x"),
            &root
        ));
        // Путь к цели ссылки — тот же каталог, и он тоже отвергается.
        assert!(is_inside_input(&outside.join("x"), &root));
    }

    #[test]
    fn repository_root_follows_the_path_not_the_process() {
        let dir = TempDir::new("layout-root");
        let root = repository(&dir);
        let detached = dir.path().join("detached");
        std::fs::create_dir_all(&detached).expect("посторонний каталог");

        assert_eq!(
            repository_root_for(&root.join("decks/output")),
            Some(root.clone())
        );
        assert_eq!(repository_root_for(&detached), None);
    }

    #[test]
    fn the_reserved_input_layout_is_closed_without_git_or_inside_nested_git() {
        let dir = TempDir::new("layout-no-git");
        let root = repository(&dir);
        std::fs::remove_dir_all(root.join(".git")).expect("убрать Git-метаданные");
        std::fs::create_dir_all(root.join("decks/input/hidden/.git"))
            .expect("вложенный репозиторий");

        for path in [
            root.join("decks/input/hidden"),
            root.join("decks/unused/../input/hidden"),
        ] {
            let error = guard_local_path(&path, "каталог экспорта")
                .expect_err("закрытая область не зависит от Git-метаданных");
            assert_eq!(error.details["reason"], "input_area_forbidden");
        }
    }

    #[test]
    fn aliases_to_and_from_the_reserved_input_layout_are_closed() {
        let dir = TempDir::new("layout-input-alias");
        let root = repository(&dir);
        std::fs::remove_dir_all(root.join(".git")).expect("убрать Git-метаданные");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("внешний каталог");

        let input = root.join("decks/input");
        std::fs::remove_dir_all(&input).expect("убрать каталог input");
        std::os::unix::fs::symlink(&outside, &input).expect("ссылка input наружу");
        assert_eq!(
            guard_local_path(&input.join("Hidden"), "каталог экспорта")
                .expect_err("путь через закрытый alias отвергается")
                .details["reason"],
            "input_area_forbidden"
        );

        std::fs::remove_file(&input).expect("убрать ссылку input");
        std::fs::create_dir_all(&input).expect("вернуть закрытый каталог");
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(root.join("decks/input"), &alias)
            .expect("внешняя ссылка в закрытую область");
        assert_eq!(
            guard_local_path(&alias.join("Hidden"), "каталог экспорта")
                .expect_err("канонический путь через alias отвергается")
                .details["reason"],
            "input_area_forbidden"
        );
    }

    #[test]
    fn unrelated_external_paths_remain_allowed_without_git() {
        let dir = TempDir::new("layout-external");
        let root = dir.path().join("external");
        std::fs::create_dir_all(&root).expect("внешний каталог");

        guard_local_path(&root.join("SomeExport"), "каталог экспорта")
            .expect("обычный внешний экспорт разрешён");
        guard_local_path(&root.join("decks/output/SomeExport"), "каталог экспорта")
            .expect("внешняя рабочая раскладка не совпадает с закрытым input");
    }

    #[test]
    fn guard_reports_the_forbidden_area_with_a_stable_reason() {
        let dir = TempDir::new("layout-guard");
        let root = repository(&dir);
        let error = guard_local_path(&root.join("decks/input/x"), "каталог экспорта")
            .expect_err("отказ обязателен");

        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert_eq!(error.details["reason"], "input_area_forbidden");
        assert_eq!(error.details["area"], "decks/input");
    }
}
