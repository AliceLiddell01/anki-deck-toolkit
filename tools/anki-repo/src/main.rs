//! Тонкий адаптер CLI и процесса.
//!
//! Вся доменная работа происходит в библиотеке; здесь только разбор
//! аргументов, печать подготовленного вывода и отображение результата на
//! код завершения процесса.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

use clap::Parser;

use anki_repo::cli::{Cli, Command};
use anki_repo::error::ErrorCode;
use anki_repo::output::write_text;
use anki_repo::run::{execute_cli, render_error};
use repository_maintenance::care;

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Предложение обслуживания диска не должно срабатывать для команды, которую
    // граница закрытой области отвергает: оно читало бы репозиторий, к которому
    // команда всё равно не получит доступа.
    if anki_repo::layout::guard_command(&cli.command).is_ok() {
        maybe_offer_maintenance(&cli);
    }

    match execute_cli(&cli) {
        Ok(rendered) => {
            if stdout_write_failed(&rendered.stdout) {
                return internal_failure();
            }
            ExitCode::from(rendered.exit)
        }
        Err(error) => {
            let command = cli.command_name();
            let (stdout, stderr) = render_error(command, cli.json, &error);
            let failed = stdout_write_failed(&stdout);
            // Диагностика вторична: если её не удалось записать, основной
            // результат и exit code всё равно должны быть отданы.
            let _ = write_text(&mut io::stderr().lock(), &stderr);
            if failed {
                return internal_failure();
            }
            ExitCode::from(error.exit_code())
        }
    }
}

/// Пишет вывод команды в stdout и сообщает, была ли запись внутренней ошибкой.
///
/// Закрытый читателем pipe (`anki-repo … | head`) внутренней ошибкой не
/// считается: получатель сам перестал читать, а результат команды уже вычислен,
/// поэтому сохраняется её собственный exit code. Любой другой отказ записи
/// (нет места на диске, негодный дескриптор) — это внутренняя ошибка.
fn stdout_write_failed(text: &str) -> bool {
    match write_text(&mut io::stdout().lock(), text) {
        Ok(()) => false,
        Err(error) => error.kind() != io::ErrorKind::BrokenPipe,
    }
}

/// Код завершения внутренней ошибки: вывод не удалось записать.
fn internal_failure() -> ExitCode {
    ExitCode::from(ErrorCode::Internal.exit_code())
}

/// Корень рабочей области, к которой относится экспорт выбранной команды.
///
/// Явное указание окружением `ANKI_REPOSITORY_ROOT` старше расположения
/// экспорта: это осознанное решение оператора. Иначе репозиторий берётся из
/// самого экспорта, а не из текущего каталога процесса: иначе предложение
/// измерило бы `target` постороннего проекта, а согласие установило бы таймер,
/// который ежедневно очищает чужой `target`.
fn maintenance_workspace_root(command: &Command) -> Option<std::path::PathBuf> {
    use repository_maintenance::policy::workspace_root;
    if std::env::var_os("ANKI_REPOSITORY_ROOT").is_some() {
        return workspace_root(None, false).ok();
    }
    let export_dir = match command {
        Command::Inspect { export_dir, .. }
        | Command::Find { export_dir, .. }
        | Command::Stats { export_dir, .. }
        | Command::Validate { export_dir, .. }
        | Command::Qa { export_dir, .. }
        | Command::Models { export_dir, .. } => export_dir,
        _ => return None,
    };
    let repository = anki_repo::run::export_repository_root(export_dir)?;
    workspace_root(Some(repository.as_path()), false).ok()
}

/// Вступительное предложение — только в обычных командах с настоящим TTY.
/// Никакого чтения stdin у запросов с перенаправленными потоками и никаких баннеров в JSON.
/// Признак `CI` проверяется отдельно от TTY: некоторые CI-среды выделяют
/// псевдотерминал (`docker run -t`, util-linux `script`), и тогда вопрос
/// ожидал бы ответа без ограничения времени. README обещает отсутствие
/// вопросов в CI, поэтому проверка нужна здесь, а не только по потокам.
fn maybe_offer_maintenance(cli: &Cli) {
    if cli.json
        || std::env::var_os("CI").is_some()
        || !io::stdin().is_terminal()
        || !io::stdout().is_terminal()
        || !io::stderr().is_terminal()
        || !matches!(
            &cli.command,
            Command::Inspect { .. }
                | Command::Find { .. }
                | Command::Stats { .. }
                | Command::Validate { .. }
                | Command::Qa { .. }
                | Command::Models { .. }
        )
    {
        return;
    }
    let Some(root) = maintenance_workspace_root(&cli.command) else {
        return;
    };
    let Ok(choice) = care::choice() else { return };
    if choice == "yes" {
        return;
    }
    if choice == "no" {
        // Суточный интервал расходуется на саму проверку, а не только на
        // показанное предупреждение: иначе у пользователя, отказавшегося от
        // автоочистки и не превышающего пороги, полный обход повторялся бы при
        // каждом вызове.
        if care::claim_daily_check().unwrap_or(false) {
            let status = care::inspect(&root);
            if status
                .diagnostics
                .iter()
                .any(|d| d.code == "target_warning" || d.code == "external_cache_limit")
                && care::should_warn().unwrap_or(false)
            {
                eprintln!(
                    "Предупреждение GC: объём управляемых артефактов превысил порог; подробности: anki-repo maintenance status."
                );
            }
        }
        return;
    }
    // Не предлагаем переустановку автоматически при существующем user timer,
    // даже если он связан с другой рабочей копией. Состояния `unknown`
    // (таймер включён, принадлежность не подтверждена) и `unavailable`
    // (нет пользовательского менеджера systemd) означают, что предложение
    // нельзя выполнить: ответ `y` сохранил бы согласие и всё равно не смог
    // установить таймер.
    let timer = repository_maintenance::timer::inspect();
    if timer.installed_root.is_some() || matches!(timer.state.as_str(), "unknown" | "unavailable") {
        return;
    }
    let status = care::inspect(&root);
    eprintln!(
        "Рекомендуем включить автоматическую очистку временных файлов и артефактов сборки Anki-decks."
    );
    if let Some(bytes) = status.measurement.project_bytes {
        eprintln!(
            "Измерено управляемых данных: {bytes} байт{}.",
            if status.measurement.complete {
                ""
            } else if status
                .measurement
                .system_tmp
                .as_ref()
                .is_some_and(|tmp| !tmp.complete)
            {
                " (общий обзор системного /tmp неполон; размер системных данных не включён)"
            } else {
                " (полнота проверки не подтверждена)"
            }
        );
    }
    eprintln!("Очистка ограничена проверками владения и безопасности действующего GC.");
    eprint!("Включить автоматическую очистку? [y/N] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    // Пустой ввод на EOF (Ctrl-D, закрытие терминала) — это отсутствие ответа, а не
    // отказ: сохранять `no` в этом случае нельзя, иначе вопрос исчезнет навсегда,
    // хотя пользователь на него не отвечал.
    match io::stdin().read_line(&mut answer) {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    // Отказом считается явный `n`/`no`/`нет` и пустой ответ (умолчание `[y/N]`).
    // Нераспознанный ввод — опечатка или форма, которую инструмент не понимает, —
    // не сохраняется: иначе случайная строка навсегда закрывала бы вопрос.
    let answer = answer.trim().to_lowercase();
    if matches!(answer.as_str(), "y" | "yes" | "д" | "да") {
        match care::enable(&root) {
            Ok(()) => eprintln!("Автоочистка подтверждена: таймер установлен и активен."),
            Err(e) => {
                let consent = match care::choice() {
                    Ok(choice) if choice == "yes" => "Согласие сохранено",
                    Ok(_) => "Согласие не сохранено",
                    Err(_) => "Состояние согласия неизвестно",
                };
                eprintln!(
                    "Не удалось включить GC: {e}. {consent}; проверьте anki-repo maintenance status."
                );
            }
        }
    } else if matches!(answer.as_str(), "" | "n" | "no" | "н" | "нет") {
        if let Err(e) = care::set_choice("no") {
            eprintln!("Не удалось сохранить отказ: {e}");
        }
    } else {
        eprintln!("Ответ не распознан; выбор не сохранён, вопрос будет задан снова.");
    }
}
