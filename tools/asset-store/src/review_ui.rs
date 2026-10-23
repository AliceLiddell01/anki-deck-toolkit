//! Единственная граница внешних приложений редакторской проверки.
//! Диспетчер допускает её только по явному выбору пользователя и после
//! завершения текущего этапа обхода по ширине.
//!
//! Продукт — прежде всего Linux-приложение: обязательной зависимости от Windows,
//! WSL или `notepad.exe` нет. Приложение выбирается в прозрачном порядке:
//!
//! 1. **явно выбранный текстовый редактор** — `GIT_EDITOR`, `core.editor`,
//!    `VISUAL`, `EDITOR`. Значение обязано быть программой с аргументами без
//!    shell-операторов и не может быть самой оболочкой: наличие `GIT_EDITOR`
//!    не является разрешением исполнить shell-код. Редактор открывает только
//!    `decisions.tsv`, но не каталог;
//! 2. **системная ассоциация Linux** — каталог открывает механизм MIME
//!    (`xdg-open`), а `decisions.tsv` — только ассоциация `text/plain`
//!    (`xdg-mime query default text/plain` и десктопный запуск `gtk-launch`).
//!    Общая ассоциация самого файла намеренно не используется: она может
//!    оказаться табличным редактором, который перепишет формат решений;
//! 3. **WSL2** — Windows Explorer и резервный Notepad остаются **дополнительными**
//!    путём и выбираются только при доказанном межсистемном взаимодействии:
//!    включённый ядром флаг `binfmt_misc/WSLInterop*`, а для каталога также
//!    доступный `explorer.exe` в `PATH` или проверенный путь к Windows Explorer через `wslpath`. Одного `WSL_DISTRO_NAME` недостаточно. Путь
//!    передаётся в формате Windows через `wslpath -w`; при отказе остаются
//!    Linux-ассоциация, настроенный редактор или ручной способ;
//! 4. **резервный безопасный путь** — печать путей каталога и `decisions.tsv` и
//!    готовых указаний `review-export`/`review-apply`. Отсутствие графической
//!    среды (серверный Linux без `DISPLAY`/Wayland) не является фатальной
//!    ошибкой команды: пользователь получает пути и инструкцию.
//!
//! Все программы собираются как program + argv и запускаются напрямую, без
//! `sh -c`, `bash -c` и `cmd /c`: пути и текст решений никогда не являются кодом.
//! Ожидание закрытия графического приложения не является протоколом применения:
//! после `open()` следует отдельный вопрос о применении и повторное чтение TSV.
use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output};

use crate::{AssetError, ErrorCode};

pub(super) trait ReviewInteraction {
    fn eligible(&self) -> bool;
    fn ask(&mut self, message: &str) -> Result<bool, AssetError>;
    fn open(&mut self, request: &ReviewOpen<'_>) -> Result<(), AssetError>;
}

/// Что открыть: каталог пакета ручной проверки и его `decisions.tsv`.
/// Идентификаторы нужны только для печати готовых указаний ручного резервного пути.
pub(super) struct ReviewOpen<'a> {
    pub batch_id: &'a str,
    pub review_id: &'a str,
    pub folder: &'a Path,
    pub decisions: &'a Path,
}

pub(super) struct SystemReviewInteraction;

impl ReviewInteraction for SystemReviewInteraction {
    fn eligible(&self) -> bool {
        allows_interaction(
            cfg!(test) || test_ancestor(),
            [
                io::stdin().is_terminal(),
                io::stdout().is_terminal(),
                io::stderr().is_terminal(),
            ],
            ["CI", "GITHUB_ACTIONS", "CODEX_NONINTERACTIVE"]
                .iter()
                .any(|key| std::env::var_os(key).is_some()),
            std::env::var("TERM").ok().as_deref(),
        ) && std::fs::read_to_string("/proc/self/stat").is_ok_and(|stat| foreground_process(&stat))
    }

    fn ask(&mut self, message: &str) -> Result<bool, AssetError> {
        if !self.eligible() {
            return Err(invalid(
                "интерактивное окружение недоступно: вопрос не задаётся",
            ));
        }
        let mut stderr = io::stderr().lock();
        write!(stderr, "{message} [y/N] ")
            .and_then(|_| stderr.flush())
            .map_err(|error| AssetError::io("приглашение проверки", error))?;
        let mut line = String::new();
        let count = io::stdin()
            .read_line(&mut line)
            .map_err(|error| AssetError::io("ответ проверки", error))?;
        Ok(count > 0 && line.trim().eq_ignore_ascii_case("y"))
    }

    fn open(&mut self, request: &ReviewOpen<'_>) -> Result<(), AssetError> {
        // Повторная защита самой границы: ни тест, ни неполный TTY, ни CI, ни
        // `TERM=dumb` не запускают внешние приложения, даже если `open` вызван
        // напрямую, минуя диспетчер.
        if !self.eligible() {
            return Err(invalid(
                "интерактивное окружение недоступно: внешние приложения не открываются",
            ));
        }
        let host = SystemHost;
        let environment = Environment::detect(&host)?;
        let mut diagnostics = io::stderr().lock();
        open_with(&host, &environment, request, &mut diagnostics)
    }
}

/// Запуск процессов и пробы окружения границы проверки.
///
/// Продакшн-реализация запускает настоящие программы; подменяемая реализация
/// позволяет проверить собранную команду, выбранную ветку и обработку отказа
/// запуска без единого реального `xdg-open`, `explorer.exe` или редактора.
pub(super) trait Host {
    /// Запускает программу напрямую, без shell, и ждёт её завершения.
    fn launch(&self, program: &OsStr, args: &[OsString]) -> io::Result<ExitStatus>;
    /// Запускает программу и возвращает её вывод (`git config`, `wslpath`, `xdg-mime`).
    fn probe(&self, program: &OsStr, args: &[OsString]) -> io::Result<Output>;
    /// Проверяет разрешимость программы в `PATH`.
    fn resolve(&self, program: &str) -> Option<PathBuf>;
    /// Читает файл-признак окружения; отсутствующий файл — `None`.
    fn read(&self, path: &Path) -> Option<Vec<u8>>;
}

/// Продакшн-хост: настоящие процессы и файлы.
struct SystemHost;

impl Host for SystemHost {
    fn launch(&self, program: &OsStr, args: &[OsString]) -> io::Result<ExitStatus> {
        Command::new(program).args(args).status()
    }

    fn probe(&self, program: &OsStr, args: &[OsString]) -> io::Result<Output> {
        Command::new(program).args(args).output()
    }

    fn resolve(&self, program: &str) -> Option<PathBuf> {
        let requested = Path::new(program);
        let candidates: Vec<_> = if requested.is_absolute() {
            vec![requested.to_path_buf()]
        } else {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .map(|directory| directory.join(program))
                .collect()
        };
        candidates.into_iter().find(|candidate| {
            use std::os::unix::fs::PermissionsExt;
            candidate.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    }

    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }
}

const KERNEL_RELEASE: &str = "/proc/sys/kernel/osrelease";
const INTEROP_FLAGS: [&str; 2] = [
    "/proc/sys/fs/binfmt_misc/WSLInterop",
    "/proc/sys/fs/binfmt_misc/WSLInterop-late",
];

/// Снимок окружения, от которого зависит выбор приложения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Environment {
    /// Явно выбранный текстовый редактор: `GIT_EDITOR` → `core.editor` →
    /// `VISUAL` → `EDITOR`.
    pub editor: Option<String>,
    /// Объявлена графическая среда Linux (`DISPLAY` или `WAYLAND_DISPLAY`).
    pub desktop: bool,
    /// Сессия WSL: имя дистрибутива или ядро Microsoft.
    pub wsl: bool,
    /// Ядро включило межсистемное взаимодействие WSL2 (binfmt-флаг).
    pub wsl_interop: bool,
    /// Десктопная ассоциация `text/plain`, если она объявлена.
    pub text_association: Option<String>,
}

impl Environment {
    /// Снимок окружения процесса через инъецируемые пробы.
    fn detect(host: &dyn Host) -> Result<Self, AssetError> {
        Ok(Self {
            editor: editor_setting(host)?,
            desktop: desktop_available(
                std::env::var_os("DISPLAY").as_deref(),
                std::env::var_os("WAYLAND_DISPLAY").as_deref(),
            ),
            wsl: wsl_session(host, std::env::var_os("WSL_DISTRO_NAME").as_deref()),
            wsl_interop: interop_registered(host),
            text_association: text_association(host),
        })
    }
}

/// Графическая среда Linux объявлена непустым `DISPLAY` или `WAYLAND_DISPLAY`.
fn desktop_available(display: Option<&OsStr>, wayland: Option<&OsStr>) -> bool {
    [display, wayland]
        .into_iter()
        .flatten()
        .any(|value| !value.is_empty())
}

/// Сессия WSL: имя дистрибутива или ядро Microsoft. Это признак сессии,
/// а не доказательство доступности `explorer.exe`.
fn wsl_session(host: &dyn Host, distro: Option<&OsStr>) -> bool {
    distro.is_some_and(|value| !value.is_empty())
        || host.read(Path::new(KERNEL_RELEASE)).is_some_and(|bytes| {
            String::from_utf8_lossy(&bytes)
                .to_ascii_lowercase()
                .contains("microsoft")
        })
}

/// Межсистемное взаимодействие объявлено ядром: binfmt-флаг включён.
fn interop_registered(host: &dyn Host) -> bool {
    INTEROP_FLAGS.iter().any(|path| {
        host.read(Path::new(path))
            .is_some_and(|bytes| bytes.split(|byte| *byte == b'\n').next() == Some(b"enabled"))
    })
}

/// Десктопная ассоциация именно `text/plain`: предсказуемое **текстовое**
/// открытие файла решений. Общая ассоциация `.tsv` не запрашивается — она
/// может оказаться табличным редактором, который перепишет формат решений.
fn text_association(host: &dyn Host) -> Option<String> {
    let output = host
        .probe(
            OsStr::new("xdg-mime"),
            &[
                OsString::from("query"),
                OsString::from("default"),
                OsString::from("text/plain"),
            ],
        )
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let desktop = String::from_utf8(output.stdout).ok()?;
    let desktop = desktop.trim();
    (desktop.ends_with(".desktop")
        && !desktop.starts_with('-')
        && !desktop.contains(['/', '\\'])
        && !desktop.chars().any(char::is_whitespace))
    .then(|| desktop.to_owned())
}

/// Явно выбранный текстовый редактор в порядке приоритета.
fn editor_setting(host: &dyn Host) -> Result<Option<String>, AssetError> {
    if let Some(editor) =
        first_editor_setting(editor_env("GIT_EDITOR")?.as_deref(), None, None, None)
    {
        return Ok(Some(editor));
    }
    let core_editor = host
        .probe(
            OsStr::new("git"),
            &[
                OsString::from("config"),
                OsString::from("--get"),
                OsString::from("core.editor"),
            ],
        )
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            String::from_utf8(output.stdout).map_err(|_| invalid("core.editor должен быть UTF-8"))
        })
        .transpose()?;
    if let Some(editor) = first_editor_setting(None, core_editor.as_deref(), None, None) {
        return Ok(Some(editor));
    }
    if let Some(editor) = first_editor_setting(None, None, editor_env("VISUAL")?.as_deref(), None) {
        return Ok(Some(editor));
    }
    Ok(first_editor_setting(
        None,
        None,
        None,
        editor_env("EDITOR")?.as_deref(),
    ))
}

fn editor_env(key: &str) -> Result<Option<String>, AssetError> {
    match std::env::var(key) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(invalid(format!(
            "{key} должен быть UTF-8; настройка не проигнорирована"
        ))),
    }
}

/// Первое непустое значение приоритета `GIT_EDITOR` → `core.editor` →
/// `VISUAL` → `EDITOR`.
fn first_editor_setting(
    git_editor: Option<&str>,
    core_editor: Option<&str>,
    visual: Option<&str>,
    editor: Option<&str>,
) -> Option<String> {
    [git_editor, core_editor, visual, editor]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Разобранный явно выбранный редактор: программа и её собственные аргументы.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EditorCommand {
    program: OsString,
    args: Vec<OsString>,
}

/// Разбирает настройку редактора и отвергает оболочки: `GIT_EDITOR` не является
/// разрешением исполнить shell-код.
fn editor_command(text: &str) -> Result<EditorCommand, AssetError> {
    let mut parts = editor_arguments(text)?;
    let program = parts.remove(0);
    if program.is_empty() || is_shell_program(&program) {
        return Err(invalid(
            "настроенный редактор является оболочкой: shell-команды не поддерживаются",
        ));
    }
    Ok(EditorCommand {
        program: OsString::from(program),
        args: parts.into_iter().map(OsString::from).collect(),
    })
}

/// Оболочки, запуск которых означал бы исполнение shell-кода из настройки.
fn is_shell_program(program: &str) -> bool {
    const SHELLS: [&str; 16] = [
        "env",
        "busybox",
        "sh",
        "bash",
        "dash",
        "zsh",
        "ksh",
        "fish",
        "csh",
        "tcsh",
        "cmd",
        "cmd.exe",
        "powershell",
        "powershell.exe",
        "pwsh",
        "pwsh.exe",
    ];
    // Windows-разделители остаются литералами при запуске в Linux.
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    SHELLS.iter().any(|shell| name.eq_ignore_ascii_case(shell))
}

/// Один шаг открытия. Кандидаты каталога адресуют каталог, кандидаты файла
/// решений — `decisions.tsv`; планировщик их не смешивает.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Candidate {
    /// Windows Explorer (WSL2): путь каталога преобразуется `wslpath -w`.
    Explorer,
    /// Резервный текстовый редактор Windows при отсутствии редактора Linux.
    Notepad,
    /// Системная ассоциация Linux по MIME для каталога.
    XdgOpen,
    /// Десктопная ассоциация `text/plain` для файла решений.
    TextAssociation { desktop: String },
    /// Явно выбранный текстовый редактор; каталог им не открывается.
    Editor {
        program: OsString,
        args: Vec<OsString>,
    },
}

impl Candidate {
    /// Имя шага для диагностики отказа.
    fn label(&self) -> &'static str {
        match self {
            Self::Explorer => "explorer.exe",
            Self::Notepad => "notepad.exe",
            Self::XdgOpen => "xdg-open",
            Self::TextAssociation { .. } => "ассоциация text/plain",
            Self::Editor { .. } => "настроенный редактор",
        }
    }
}

/// План открытия: кандидаты каталога и файла решений в порядке приоритета.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OpenPlan {
    folder: Vec<Candidate>,
    decisions: Vec<Candidate>,
}

/// Приоритет: явно выбранный текстовый редактор → системная ассоциация →
/// (если открывать нечем) диагностируемый безопасный резервный путь в `guidance`.
fn plan_open(environment: &Environment) -> Result<OpenPlan, AssetError> {
    let editor = environment
        .editor
        .as_deref()
        .map(editor_command)
        .transpose()?;
    let mut plan = OpenPlan::default();
    if environment.desktop {
        plan.folder.push(Candidate::XdgOpen);
    }
    if environment.wsl && environment.wsl_interop {
        plan.folder.push(Candidate::Explorer);
    }
    if let Some(editor) = editor {
        plan.decisions.push(Candidate::Editor {
            program: editor.program,
            args: editor.args,
        });
    }
    if environment.desktop
        && let Some(desktop) = &environment.text_association
    {
        plan.decisions.push(Candidate::TextAssociation {
            desktop: desktop.clone(),
        });
    }
    // Windows-редактор не является обязательной зависимостью Linux.
    // Не подменяем им явно заданный редактор пользователя.
    if environment.wsl && environment.wsl_interop && environment.editor.is_none() {
        plan.decisions.push(Candidate::Notepad);
    }
    Ok(plan)
}

/// Готовая к запуску команда: программа и argv без shell.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    program: OsString,
    args: Vec<OsString>,
    /// Explorer при передаче папки работающему процессу возвращает также код 1.
    tolerate_exit_one: bool,
}

/// На WSL Explorer может отсутствовать в PATH даже при включённом interop.
/// Дополнительный путь преобразуется через wslpath, чтобы учитывать нестандартный
/// каталог монтирования Windows, и проверяется как исполняемый файл.
fn windows_system_program(host: &dyn Host, name: &str, windows_location: &str) -> Option<PathBuf> {
    if host.resolve(name).is_some() {
        // Сохраняем прежний запуск по имени, если PATH уже настроен.
        return Some(PathBuf::from(name));
    }
    let converted = convert_path(host, "-u", Path::new(windows_location)).ok()?;
    let path = PathBuf::from(converted);
    if path.is_absolute() {
        host.resolve(path.to_str()?)
    } else {
        None
    }
}

fn explorer_program(host: &dyn Host) -> Option<PathBuf> {
    windows_system_program(host, "explorer.exe", r"C:\Windows\explorer.exe")
}

/// Собирает команду шага. `path` — путь той цели, которой адресован кандидат.
fn invocation(
    host: &dyn Host,
    environment: &Environment,
    path: &Path,
    candidate: &Candidate,
) -> Result<Invocation, AssetError> {
    match candidate {
        Candidate::Explorer => {
            let path = windows_path(host, path)?;
            let program = explorer_program(host)
                .ok_or_else(|| invalid("explorer.exe не найдена в PATH или каталоге Windows"))?;
            Ok(Invocation {
                program: program.into_os_string(),
                args: vec![path],
                tolerate_exit_one: true,
            })
        }
        Candidate::Notepad => {
            let path = windows_path(host, path)?;
            // Notepad может не поддерживать путь длиннее MAX_PATH даже при
            // успешном запуске процесса: не выдаём такое окно за успех.
            if path.to_string_lossy().encode_utf16().count() >= 260 {
                return Err(invalid(concat!(
                    "путь к decisions.tsv слишком длинный для Windows Notepad;",
                    " используйте Linux-редактор или короткий путь к хранилищу"
                )));
            }
            let program =
                windows_system_program(host, "notepad.exe", r"C:\Windows\System32\notepad.exe")
                    .ok_or_else(|| invalid("notepad.exe недоступен в Windows"))?;
            Ok(Invocation {
                program: program.into_os_string(),
                args: vec![path],
                tolerate_exit_one: false,
            })
        }
        Candidate::XdgOpen => Ok(Invocation {
            program: OsString::from("xdg-open"),
            args: vec![path.as_os_str().to_owned()],
            tolerate_exit_one: false,
        }),
        Candidate::TextAssociation { desktop } => {
            if host.resolve("gtk-launch").is_none() && host.resolve("gio").is_some() {
                let user_data = std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|home| PathBuf::from(home).join(".local/share"))
                    });
                let system_data = std::env::var_os("XDG_DATA_DIRS")
                    .unwrap_or_else(|| OsString::from("/usr/local/share:/usr/share"));
                let desktop_path = user_data
                    .into_iter()
                    .chain(std::env::split_paths(&system_data))
                    .filter(|directory| directory.is_absolute())
                    .map(|directory| directory.join("applications").join(desktop))
                    .find(|file| host.read(file).is_some())
                    .ok_or_else(|| {
                        invalid("файл ассоциации text/plain не найден; настройте EDITOR")
                    })?;
                return Ok(Invocation {
                    program: OsString::from("gio"),
                    args: vec![
                        OsString::from("launch"),
                        desktop_path.into_os_string(),
                        path.as_os_str().to_owned(),
                    ],
                    tolerate_exit_one: false,
                });
            }
            Ok(Invocation {
                program: OsString::from("gtk-launch"),
                args: vec![OsString::from(desktop), path.as_os_str().to_owned()],
                tolerate_exit_one: false,
            })
        }
        Candidate::Editor { program, args } => {
            let mut argv = args.clone();
            // Файл решений передаётся отдельным аргументом; его текст не является
            // ни программой, ни аргументом.
            argv.push(editor_path(host, environment, program, path)?);
            Ok(Invocation {
                program: editor_program(host, environment, program)?,
                args: argv,
                tolerate_exit_one: false,
            })
        }
    }
}

/// Открытие пакета: кандидаты пробуются по порядку, отказы не теряются.
fn open_with(
    host: &dyn Host,
    environment: &Environment,
    request: &ReviewOpen<'_>,
    diagnostics: &mut dyn Write,
) -> Result<(), AssetError> {
    let plan = match plan_open(environment) {
        Ok(plan) => plan,
        Err(error) => {
            guidance(diagnostics, request);
            return Err(error);
        }
    };
    let mut failures = Vec::new();
    let mut launched = 0usize;
    let mut unopenable = 0usize;
    for (path, candidates) in [
        (request.folder, &plan.folder),
        (request.decisions, &plan.decisions),
    ] {
        if candidates.is_empty() {
            // Открывать нечем: пользователь обязан получить пути и инструкцию.
            unopenable += 1;
            let _ = writeln!(
                diagnostics,
                "Нет программы для открытия: {}",
                path.display()
            );
            continue;
        }
        if launch_first(host, environment, path, candidates, &mut failures) {
            launched += 1;
        } else {
            unopenable += 1;
            let _ = writeln!(diagnostics, "Не удалось открыть: {}", path.display());
        }
    }
    for failure in &failures {
        let _ = writeln!(diagnostics, "Внешнее приложение не открылось: {failure}");
    }
    if launched == 0 {
        guidance(diagnostics, request);
        if let Some(failure) = failures.first() {
            // Ошибка запуска не выдаётся за успех и не теряется.
            return Err(invalid(format!(
                "внешнее приложение проверки не запустилось: {failure}"
            )));
        }
    } else if unopenable > 0 {
        guidance(diagnostics, request);
    }
    Ok(())
}

/// Пробует кандидатов по порядку, пока один не запустится; отказы собираются
/// в `failures` и печатаются как диагностика, а не выдаются за успех.
fn launch_first(
    host: &dyn Host,
    environment: &Environment,
    path: &Path,
    candidates: &[Candidate],
    failures: &mut Vec<String>,
) -> bool {
    for candidate in candidates {
        match invocation(host, environment, path, candidate).and_then(|step| run(host, &step)) {
            Ok(()) => return true,
            Err(error) => failures.push(format!("{}: {}", candidate.label(), error.message)),
        }
    }
    false
}

/// Запускает собранную команду и проверяет её завершение.
fn run(host: &dyn Host, step: &Invocation) -> Result<(), AssetError> {
    let program = step.program.to_string_lossy();
    let status = host
        .launch(&step.program, &step.args)
        .map_err(|error| invalid(format!("{program} не запустилась: {error}")))?;
    if status.success() || (step.tolerate_exit_one && status.code() == Some(1)) {
        Ok(())
    } else {
        Err(invalid(format!("{program} завершилась с {status}")))
    }
}

/// Безопасный резервный путь: пути пакета и готовые указания вместо обязательного GUI.
fn guidance(diagnostics: &mut dyn Write, request: &ReviewOpen<'_>) {
    let _ = writeln!(
        diagnostics,
        "Открытие обеих целей не подтверждено. Пути пакета ручной проверки:"
    );
    let _ = writeln!(diagnostics, "  каталог: {}", request.folder.display());
    let _ = writeln!(diagnostics, "  решения: {}", request.decisions.display());
    let _ = writeln!(
        diagnostics,
        "  проверенные копии изображений лежат в каталоге рядом с файлом решений"
    );
    let _ = writeln!(
        diagnostics,
        "Откройте decisions.tsv текстовым редактором, измените только первую колонку и сохраните UTF-8 TSV."
    );
    let _ = writeln!(
        diagnostics,
        "Без интерактивного режима пакет применяется существующими командами:"
    );
    let _ = writeln!(
        diagnostics,
        "  kanji-assets --output json batch review-export --batch-id {}",
        request.batch_id
    );
    let _ = writeln!(
        diagnostics,
        "  kanji-assets --output json batch review-apply --batch-id {} --review-id {}",
        request.batch_id, request.review_id
    );
}

/// Путь файла решений для редактора: для Windows-программы в WSL — формат Windows.
fn editor_path(
    host: &dyn Host,
    environment: &Environment,
    program: &OsStr,
    decisions: &Path,
) -> Result<OsString, AssetError> {
    if environment.wsl && windows_editor(program) {
        if !environment.wsl_interop {
            return Err(invalid(
                "Windows-редактор недоступен: WSL interop не подтверждён",
            ));
        }
        windows_path(host, decisions)
    } else {
        Ok(decisions.as_os_str().to_owned())
    }
}

/// Программа редактора: Windows-путь преобразуется `wslpath -u`, потому что WSL
/// не запускает `C:\...` напрямую.
fn editor_program(
    host: &dyn Host,
    environment: &Environment,
    program: &OsStr,
) -> Result<OsString, AssetError> {
    if environment.wsl && is_windows_program(program) {
        convert_path(host, "-u", Path::new(program))
    } else {
        Ok(program.to_owned())
    }
}

/// Признак пути Windows: буква диска и двоеточие (`C:\...` или `C:/...`).
fn is_windows_program(program: &OsStr) -> bool {
    let bytes = program.as_encoded_bytes();
    bytes.get(1) == Some(&b':') && bytes.first().is_some_and(u8::is_ascii_alphabetic)
}

fn windows_editor(program: &OsStr) -> bool {
    is_windows_program(program)
        || program
            .as_encoded_bytes()
            .rsplit(|byte| *byte == b'.')
            .next()
            .is_some_and(|extension| extension.eq_ignore_ascii_case(b"exe"))
}

fn windows_path(host: &dyn Host, path: &Path) -> Result<OsString, AssetError> {
    let translated = convert_path(host, "-w", path)?;
    let short = translated
        .to_str()
        .and_then(|path| path.strip_prefix(r"\\wsl.localhost\"))
        .map(|suffix| format!("\\\\wsl$\\{suffix}"));
    // Оба UNC-варианта поддерживаются Windows; короткий адрес уменьшает
    // риск MAX_PATH у Windows-редакторов, не копируя файл решений.
    Ok(short.map_or(translated, OsString::from))
}

fn convert_path(host: &dyn Host, direction: &str, path: &Path) -> Result<OsString, AssetError> {
    let output = host
        .probe(
            OsStr::new("wslpath"),
            &[OsString::from(direction), path.as_os_str().to_owned()],
        )
        .map_err(|error| invalid(format!("wslpath не запустился: {error}")))?;
    if !output.status.success() {
        return Err(invalid("wslpath не преобразовал путь"));
    }
    let value = String::from_utf8(output.stdout).map_err(|_| invalid("wslpath вернул не UTF-8"))?;
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.contains(['\r', '\n', '\0']) {
        return Err(invalid("wslpath вернул пустой или неоднозначный путь"));
    }
    Ok(value.into())
}

// Полный TTY у фоновой job сохраняется, но ввод ей не принадлежит. /proc
// позволяет проверить foreground process group без небезопасного FFI.
fn foreground_process(stat: &str) -> bool {
    let Some((_, fields)) = stat.rsplit_once(')') else {
        return false;
    };
    let fields: Vec<_> = fields.split_whitespace().take(6).collect();
    let pgrp = fields.get(2).and_then(|value| value.parse::<i64>().ok());
    let tpgid = fields.get(5).and_then(|value| value.parse::<i64>().ok());
    matches!((pgrp, tpgid), (Some(group), Some(foreground)) if group > 0 && group == foreground)
}

fn allows_interaction(
    test_process: bool,
    terminals: [bool; 3],
    ci: bool,
    term: Option<&str>,
) -> bool {
    !test_process
        && terminals.into_iter().all(|terminal| terminal)
        && !ci
        && term.is_some_and(|value| !value.is_empty() && value != "dumb")
}

// В том числе бинарник, запущенный integration-тестом через PTY: его
// собственный cfg(test) выключен, но родительский Cargo/test harness виден в /proc.
fn test_ancestor() -> bool {
    let mut pid = std::process::id();
    for _ in 0..64 {
        let root = format!("/proc/{pid}");
        let Ok(cmdline) = std::fs::read(format!("{root}/cmdline")) else {
            return true;
        };
        let arguments: Vec<_> = cmdline
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .collect();
        if let Some(program) = arguments.first() {
            let program = std::path::Path::new(
                <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(program),
            );
            if program
                .parent()
                .is_some_and(|parent| parent.file_name().is_some_and(|name| name == "deps"))
                || (program.file_name().is_some_and(|name| name == "cargo")
                    && cargo_test_arguments(&arguments))
            {
                return true;
            }
        }
        let Ok(status) = std::fs::read_to_string(format!("{root}/status")) else {
            return true;
        };
        let Some(parent) = status.lines().find_map(|line| {
            line.strip_prefix("PPid:")
                .and_then(|value| value.trim().parse::<u32>().ok())
        }) else {
            return true;
        };
        if parent == 0 {
            return false;
        }
        pid = parent;
    }
    true
}

fn cargo_test_arguments(arguments: &[&[u8]]) -> bool {
    arguments
        .iter()
        .skip(1)
        .take_while(|argument| **argument != b"--")
        .any(|argument| *argument == b"test")
}

fn invalid(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InvalidTransition, message)
}

/// Поддерживает кавычки и пробелы в пути/аргументах редактора, но намеренно
/// не реализует shell, подстановки, pipeline или выполнение составных команд.
/// Настроенная программа является доверенным кодом пользователя; проверка
/// аргументов не изолирует редактор и не является песочницей.
fn editor_arguments(text: &str) -> Result<Vec<String>, AssetError> {
    if text.contains(['\n', '\r', '\0']) {
        return Err(invalid("настройка редактора содержит управляющие символы"));
    }
    let mut result = Vec::new();
    let mut word = String::new();
    let mut word_started = false;
    let mut quote = None;
    let mut escaped = false;
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if escaped {
            word.push(character);
            word_started = true;
            escaped = false;
            continue;
        }
        if character == '\\' && quote != Some('\'') {
            word_started = true;
            if characters.peek().is_some_and(|next| {
                matches!(next, '\\' | '\'' | '"') || (quote.is_none() && next.is_whitespace())
            }) {
                escaped = true;
            } else {
                word.push(character);
            }
            continue;
        }
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                word.push(character);
            }
        } else if matches!(character, '\'' | '"') {
            word_started = true;
            quote = Some(character);
        } else if character.is_whitespace() {
            if word_started {
                result.push(std::mem::take(&mut word));
                word_started = false;
            }
        } else if matches!(
            character,
            ';' | '|' | '&' | '<' | '>' | '`' | '$' | '\n' | '\r'
        ) {
            return Err(invalid(
                "редактор должен быть программой с аргументами без shell-операторов",
            ));
        } else {
            word.push(character);
            word_started = true;
        }
    }
    if quote.is_some() || escaped {
        return Err(invalid("незакрытые кавычки или escape редактора"));
    }
    if word_started {
        result.push(word);
    }
    if result.is_empty() {
        return Err(invalid("пустая команда редактора"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn tests_cannot_enable_system_apps_even_with_inherited_tty() {
        assert!(!SystemReviewInteraction.eligible());
        assert!(test_ancestor());
    }

    #[test]
    fn every_stream_ci_terminal_and_test_guard_is_required() {
        assert!(allows_interaction(false, [true; 3], false, Some("xterm")));
        for stream in 0..3 {
            let mut terminals = [true; 3];
            terminals[stream] = false;
            assert!(!allows_interaction(false, terminals, false, Some("xterm")));
        }
        assert!(!allows_interaction(true, [true; 3], false, Some("xterm")));
        assert!(!allows_interaction(false, [true; 3], true, Some("xterm")));
        for term in [None, Some(""), Some("dumb")] {
            assert!(!allows_interaction(false, [true; 3], false, term));
        }
    }

    #[test]
    fn background_job_and_missing_foreground_evidence_are_ineligible() {
        assert!(foreground_process(
            "10 (job with ) name) S 1 20 20 123 20 0"
        ));
        assert!(!foreground_process("10 (job) S 1 20 20 123 30 0"));
        assert!(!foreground_process("10 (job) S 1 20 20 0 -1 0"));
        assert!(!foreground_process("invalid"));
    }

    #[test]
    fn cargo_run_user_arguments_are_not_test_harness_markers() {
        assert!(!cargo_test_arguments(&[
            b"cargo",
            b"run",
            b"--",
            b"batch",
            b"run",
            b"--batch-id",
            b"test"
        ]));
        assert!(cargo_test_arguments(&[b"cargo", b"test", b"--", b"filter"]));
    }

    /// Тест процесса не открывает внешние приложения даже при прямом вызове
    /// `open`: граница проверяет `eligible()` повторно.
    #[test]
    fn test_process_never_opens_external_applications() {
        let folder = Path::new("/nonexistent/review/pkg");
        let decisions = Path::new("/nonexistent/review/pkg/decisions.tsv");
        let request = ReviewOpen {
            batch_id: "batch",
            review_id: "review",
            folder,
            decisions,
        };
        let mut interaction = SystemReviewInteraction;
        let error = interaction.open(&request).unwrap_err();
        assert!(error.message.contains("интерактивное окружение"));
        assert!(interaction.ask("не должен печататься").is_err());
    }

    // --- Инъекция процессов: подменяемый хост без единого реального GUI ---

    type Call = (OsString, Vec<OsString>);

    #[derive(Clone, Copy)]
    enum FakeLaunch {
        Success,
        Code(i32),
        Missing,
    }

    /// Хост, который ничего не запускает: записывает вызовы и отдаёт заранее
    /// заданные ответы проб. `xdg-open`, `explorer.exe`, `gtk-launch` и
    /// редакторы в тестах не запускаются.
    #[derive(Default)]
    struct FakeHost {
        launches: RefCell<Vec<Call>>,
        probes: RefCell<Vec<Call>>,
        launch_codes: RefCell<HashMap<OsString, FakeLaunch>>,
        probe_replies: RefCell<HashMap<String, (bool, String)>>,
        resolved: RefCell<Vec<String>>,
        files: RefCell<HashMap<PathBuf, Vec<u8>>>,
    }

    impl FakeHost {
        fn missing(&self, program: &str) {
            self.launch_codes
                .borrow_mut()
                .insert(OsString::from(program), FakeLaunch::Missing);
        }

        fn exits(&self, program: &str, code: i32) {
            self.launch_codes
                .borrow_mut()
                .insert(OsString::from(program), FakeLaunch::Code(code));
        }

        fn replies(&self, program: &str, args: &[&str], success: bool, stdout: &str) {
            self.probe_replies
                .borrow_mut()
                .insert(probe_key(program, args), (success, stdout.to_owned()));
        }

        fn resolves(&self, program: &str) {
            self.resolved.borrow_mut().push(program.to_owned());
        }

        fn has_file(&self, path: &str, content: &str) {
            self.files
                .borrow_mut()
                .insert(PathBuf::from(path), content.as_bytes().to_vec());
        }

        fn programs(&self) -> Vec<String> {
            self.launches
                .borrow()
                .iter()
                .map(|(program, _)| program.to_string_lossy().into_owned())
                .collect()
        }

        fn argv(&self, index: usize) -> Vec<String> {
            self.launches.borrow()[index]
                .1
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect()
        }

        fn probed(&self, program: &str) -> bool {
            self.probes.borrow().iter().any(|(name, _)| name == program)
        }
    }

    impl Host for FakeHost {
        fn launch(&self, program: &OsStr, args: &[OsString]) -> io::Result<ExitStatus> {
            self.launches
                .borrow_mut()
                .push((program.to_owned(), args.to_vec()));
            match self
                .launch_codes
                .borrow()
                .get(program)
                .copied()
                .unwrap_or(FakeLaunch::Success)
            {
                FakeLaunch::Success => Ok(ExitStatus::from_raw(0)),
                FakeLaunch::Code(code) => Ok(ExitStatus::from_raw(code << 8)),
                FakeLaunch::Missing => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "программа не найдена",
                )),
            }
        }

        fn probe(&self, program: &OsStr, args: &[OsString]) -> io::Result<Output> {
            self.probes
                .borrow_mut()
                .push((program.to_owned(), args.to_vec()));
            let (success, stdout) = self
                .probe_replies
                .borrow()
                .get(&key(program, args))
                .cloned()
                .unwrap_or((false, String::new()));
            Ok(Output {
                status: ExitStatus::from_raw(if success { 0 } else { 1 << 8 }),
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
            })
        }

        fn resolve(&self, program: &str) -> Option<PathBuf> {
            if self.resolved.borrow().iter().any(|name| name == program) {
                let path = PathBuf::from(program);
                Some(if path.is_absolute() {
                    path
                } else {
                    PathBuf::from(format!("/usr/bin/{program}"))
                })
            } else {
                None
            }
        }

        fn read(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.borrow().get(path).cloned()
        }
    }

    /// Ключ вызова: программа и argv через разделитель, не встречающийся в путях.
    fn key(program: &OsStr, args: &[OsString]) -> String {
        let mut parts = vec![program.to_string_lossy().into_owned()];
        parts.extend(
            args.iter()
                .map(|argument| argument.to_string_lossy().into_owned()),
        );
        parts.join("\u{1f}")
    }

    fn probe_key(program: &str, args: &[&str]) -> String {
        let mut parts = vec![program.to_owned()];
        parts.extend(args.iter().map(|argument| (*argument).to_owned()));
        parts.join("\u{1f}")
    }

    fn request<'a>(folder: &'a Path, decisions: &'a Path) -> ReviewOpen<'a> {
        ReviewOpen {
            batch_id: "batch-id",
            review_id: "review-id",
            folder,
            decisions,
        }
    }

    fn plain_linux(editor: Option<&str>, association: Option<&str>) -> Environment {
        Environment {
            editor: editor.map(str::to_owned),
            desktop: true,
            wsl: false,
            wsl_interop: false,
            text_association: association.map(str::to_owned),
        }
    }

    fn text(output: &[u8]) -> String {
        String::from_utf8(output.to_vec()).unwrap()
    }

    // --- Приоритет редактора и безопасный разбор настройки ---

    #[test]
    fn editor_priority_is_git_editor_then_core_then_visual_then_editor() {
        let cases = [
            (
                Some("git-editor"),
                Some("core"),
                Some("visual"),
                Some("editor"),
                Some("git-editor"),
            ),
            (
                Some("   "),
                Some("core"),
                Some("visual"),
                Some("editor"),
                Some("core"),
            ),
            (
                None,
                Some("core"),
                Some("visual"),
                Some("editor"),
                Some("core"),
            ),
            (None, None, Some("visual"), Some("editor"), Some("visual")),
            (None, None, None, Some("editor"), Some("editor")),
            (Some(""), Some("  "), Some(""), Some("nano"), Some("nano")),
            (None, None, None, None, None),
        ];
        for (git_editor, core_editor, visual, editor, expected) in cases {
            assert_eq!(
                first_editor_setting(git_editor, core_editor, visual, editor).as_deref(),
                expected,
                "{git_editor:?} {core_editor:?} {visual:?} {editor:?}"
            );
        }
    }

    #[test]
    fn configured_editor_precedes_text_association_and_folder_never_uses_editor() {
        let environment = plain_linux(Some("nano --wait"), Some("org.gnome.TextEditor.desktop"));
        let plan = plan_open(&environment).unwrap();
        assert_eq!(
            plan.decisions,
            vec![
                Candidate::Editor {
                    program: OsString::from("nano"),
                    args: vec![OsString::from("--wait")],
                },
                Candidate::TextAssociation {
                    desktop: "org.gnome.TextEditor.desktop".into(),
                },
            ]
        );
        assert_eq!(plan.folder, vec![Candidate::XdgOpen]);
    }

    #[test]
    fn editor_setting_refuses_shell_programs_and_shell_operators() {
        for setting in [
            "",
            "editor; other",
            "editor $(other)",
            "editor | other",
            "editor && other",
            "editor `other`",
            "editor > file",
            "sh -c 'rm -rf /'",
            "bash -c 'rm -rf /'",
            "/bin/sh -c x",
            "cmd /c dir",
            "cmd.exe /c dir",
            "powershell -Command x",
            "pwsh -c x",
            "env bash -c x",
            "busybox sh -c x",
            "'' nano",
            "nano\nother",
            r#""C:\Windows\System32\cmd.exe" /c dir"#,
            r#"'C:\Windows\powershell.exe' -Command x"#,
            "'editor",
        ] {
            assert!(editor_command(setting).is_err(), "принято: {setting}");
        }
        for setting in ["vim", "nano --wait", "code --wait", "/usr/bin/editor"] {
            assert!(editor_command(setting).is_ok(), "отвергнуто: {setting}");
        }
    }

    #[test]
    fn editor_arguments_keep_quotes_spaces_and_shell_free_literals() {
        assert_eq!(
            editor_command("'/mnt/c/Program Files/Editor/edit.exe' --wait").unwrap(),
            EditorCommand {
                program: OsString::from("/mnt/c/Program Files/Editor/edit.exe"),
                args: vec![OsString::from("--wait")],
            }
        );
        assert_eq!(
            editor_command(r#""C:\Program Files\Editor\edit.exe" --wait"#).unwrap(),
            EditorCommand {
                program: OsString::from(r"C:\Program Files\Editor\edit.exe"),
                args: vec![OsString::from("--wait")],
            }
        );
        // Кавычки делают `$` и `>` литеральными аргументами: shell не участвует.
        assert_eq!(
            editor_arguments("vim 'literal $HOME > file'").unwrap(),
            ["vim", "literal $HOME > file"]
        );
        assert_eq!(
            editor_arguments("vim --title ''").unwrap(),
            ["vim", "--title", ""]
        );
    }

    // --- argv: пробелы, кавычки, отдельные аргументы, отсутствие shell ---

    #[test]
    fn editor_receives_decisions_as_separate_argument_without_shell() {
        let host = FakeHost::default();
        let environment = Environment {
            editor: Some("'/mnt/c/Program Files/Editor/edit.exe' --wait".into()),
            desktop: false,
            wsl: false,
            wsl_interop: false,
            text_association: None,
        };
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/review 1/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["/mnt/c/Program Files/Editor/edit.exe"]);
        assert_eq!(
            host.argv(0),
            ["--wait", "/tmp/review-fixture/review 1/decisions.tsv"]
        );
        // Ни программа, ни аргументы не содержат текста решений: разделители TSV
        // и переводы строк в argv не попадают, shell не используется.
        for (program, args) in host.launches.borrow().iter() {
            for value in std::iter::once(program).chain(args.iter()) {
                let value = value.to_string_lossy();
                assert!(!value.contains('\t'), "{value}");
                assert!(!value.contains('\n'), "{value}");
            }
        }
        assert!(host.launches.borrow().iter().all(|(program, _)| {
            !matches!(program.to_string_lossy().as_ref(), "sh" | "bash" | "cmd")
        }));
    }

    // --- Ветки: обычный Linux, WSL с interop и без, headless ---

    #[test]
    fn real_system_launcher_passes_literal_argv_to_a_safe_fixture_editor() {
        use std::os::unix::ffi::OsStringExt;
        use std::os::unix::fs::PermissionsExt;
        let workspace =
            crate::temp_workspace::TempWorkspace::create("review-safe-launcher").unwrap();
        let editor = workspace.path().join("fixture editor");
        std::fs::write(&editor, b"#!/bin/sh\nprintf '%s\\0' \"$@\" > \"$0.argv\"\n").unwrap();
        std::fs::set_permissions(&editor, std::fs::Permissions::from_mode(0o700)).unwrap();
        let editor_setting = format!("\"{}\" --wait", editor.display());
        let mut environment = plain_linux(Some(&editor_setting), None);
        environment.desktop = false;
        let decisions = workspace.path().join(OsString::from_vec(
            b"decisions with $() and \xff.tsv".to_vec(),
        ));
        let mut diagnostics = Vec::new();
        open_with(
            &SystemHost,
            &environment,
            &request(workspace.path(), &decisions),
            &mut diagnostics,
        )
        .unwrap();
        let argv = std::fs::read(workspace.path().join("fixture editor.argv")).unwrap();
        let arguments: Vec<_> = argv
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .collect();
        assert_eq!(
            arguments,
            [
                b"--wait".as_slice(),
                decisions.as_os_str().as_encoded_bytes()
            ]
        );
    }

    #[test]
    fn gio_launches_text_association_when_gtk_launcher_is_missing() {
        let host = FakeHost::default();
        host.resolves("gio");
        host.has_file(
            "/usr/share/applications/fixture-editor.desktop",
            "[Desktop Entry]\nType=Application\nExec=fixture-editor %f",
        );
        let environment = plain_linux(None, Some("fixture-editor.desktop"));
        let path = Path::new("/safe path/decisions.tsv");
        let command = invocation(
            &host,
            &environment,
            path,
            &Candidate::TextAssociation {
                desktop: "fixture-editor.desktop".into(),
            },
        )
        .unwrap();
        assert_eq!(command.program, "gio");
        assert_eq!(
            command.args,
            vec![
                OsString::from("launch"),
                OsString::from("/usr/share/applications/fixture-editor.desktop"),
                path.as_os_str().to_owned()
            ]
        );
    }

    #[test]
    fn folder_success_with_failed_editor_is_explicit_and_does_not_apply() {
        let host = FakeHost::default();
        host.missing("fixture-editor");
        let environment = plain_linux(Some("fixture-editor"), None);
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(Path::new("/folder"), Path::new("/folder/decisions.tsv")),
            &mut diagnostics,
        )
        .unwrap();
        let text = String::from_utf8(diagnostics).unwrap();
        assert!(text.contains("Не удалось открыть: /folder/decisions.tsv"));
        assert!(text.contains("review-apply"));
    }

    #[test]
    fn real_proc_foreground_parser_reads_the_actual_process_and_fails_closed() {
        let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
        let fields: Vec<_> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let group: i64 = fields[2].parse().unwrap();
        let foreground: i64 = fields[5].parse().unwrap();
        assert_eq!(foreground_process(&stat), group > 0 && group == foreground);
        assert!(!foreground_process("0 (broken)"));
        assert!(!foreground_process("0 (broken) S 1 NaN 2 3 4"));
    }

    #[test]
    fn plain_linux_uses_mime_association_for_folder_and_text_for_decisions() {
        let host = FakeHost::default();
        let environment = plain_linux(None, Some("org.gnome.TextEditor.desktop"));
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["xdg-open", "gtk-launch"]);
        // Каталог и файл решений передаются отдельными аргументами.
        assert_eq!(host.argv(0), ["/tmp/review-fixture"]);
        assert_eq!(
            host.argv(1),
            [
                "org.gnome.TextEditor.desktop",
                "/tmp/review-fixture/decisions.tsv"
            ]
        );
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn plain_linux_without_text_association_never_uses_generic_file_association() {
        let host = FakeHost::default();
        let environment = plain_linux(None, None);
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        // Табличный редактор, назначенный на `.tsv`, не получает файл решений.
        assert_eq!(host.programs(), ["xdg-open"]);
        let text = text(&diagnostics);
        assert!(text.contains("/tmp/review-fixture/decisions.tsv"), "{text}");
        assert!(text.contains("review-export"), "{text}");
        assert!(text.contains("review-apply"), "{text}");
    }

    #[test]
    fn wsl_with_proven_interop_uses_explorer_and_windows_path() {
        let host = FakeHost::default();
        host.resolves("explorer.exe");
        host.exits("explorer.exe", 1);
        host.replies(
            "wslpath",
            &["-w", "/tmp/review-fixture"],
            true,
            "C:\\Users\\review-user\\pkg\r\n",
        );
        let environment = Environment {
            editor: None,
            desktop: false,
            wsl: true,
            wsl_interop: true,
            text_association: None,
        };
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["explorer.exe"]);
        assert_eq!(host.argv(0), ["C:\\Users\\review-user\\pkg"]);
        // Файл решений не открывается обязательным notepad.exe: печатаются пути.
        let text = text(&diagnostics);
        assert!(text.contains("/tmp/review-fixture/decisions.tsv"), "{text}");
        assert!(!host.programs().iter().any(|name| name == "notepad.exe"));
    }

    #[test]
    fn wsl_without_windows_in_path_discovers_explorer_through_wslpath() {
        let host = FakeHost::default();
        host.replies(
            "wslpath",
            &["-u", r"C:\Windows\explorer.exe"],
            true,
            "/mnt/c/Windows/explorer.exe\n",
        );
        host.resolves("/mnt/c/Windows/explorer.exe");
        host.exits("/mnt/c/Windows/explorer.exe", 1);
        host.replies(
            "wslpath",
            &["-w", "/tmp/review-fixture"],
            true,
            "\\\\wsl.localhost\\archlinux\\tmp\\review-fixture\n",
        );
        let environment = Environment {
            editor: None,
            desktop: false,
            wsl: true,
            wsl_interop: true,
            text_association: None,
        };
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(
                Path::new("/tmp/review-fixture"),
                Path::new("/tmp/review-fixture/decisions.tsv"),
            ),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["/mnt/c/Windows/explorer.exe"]);
        assert_eq!(host.argv(0), [r"\\wsl$\archlinux\tmp\review-fixture"]);
    }

    #[test]
    fn wsl_without_linux_editor_opens_windows_notepad_as_fallback() {
        let host = FakeHost::default();
        host.replies(
            "wslpath",
            &["-u", r"C:\Windows\System32\notepad.exe"],
            true,
            "/mnt/c/Windows/System32/notepad.exe\n",
        );
        host.resolves("/mnt/c/Windows/System32/notepad.exe");
        host.replies(
            "wslpath",
            &["-w", "/tmp/review/decisions.tsv"],
            true,
            "\\\\wsl.localhost\\archlinux\\tmp\\review\\decisions.tsv\n",
        );
        let environment = Environment {
            editor: None,
            desktop: false,
            wsl: true,
            wsl_interop: true,
            text_association: None,
        };
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(
                Path::new("/tmp/review"),
                Path::new("/tmp/review/decisions.tsv"),
            ),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["/mnt/c/Windows/System32/notepad.exe"]);
        assert_eq!(host.argv(0), [r"\\wsl$\archlinux\tmp\review\decisions.tsv"]);
    }

    #[test]
    fn windows_notepad_rejects_path_above_max_path_before_launch() {
        let host = FakeHost::default();
        host.replies(
            "wslpath",
            &["-w", "/tmp/review/decisions.tsv"],
            true,
            &format!("\\\\wsl.localhost\\archlinux\\{}\n", "x".repeat(270)),
        );
        let environment = Environment {
            editor: None,
            desktop: false,
            wsl: true,
            wsl_interop: true,
            text_association: None,
        };
        let error = invocation(
            &host,
            &environment,
            Path::new("/tmp/review/decisions.tsv"),
            &Candidate::Notepad,
        )
        .unwrap_err();
        assert!(error.message.contains("слишком длинный"));
        assert!(host.launches.borrow().is_empty());
    }

    #[test]
    fn windows_path_shortens_only_wsl_localhost_unc() {
        let host = FakeHost::default();
        host.replies(
            "wslpath",
            &["-w", "/tmp/日本/decisions.tsv"],
            true,
            "\\\\wsl.localhost\\archlinux\\tmp\\日本\\decisions.tsv\n",
        );
        assert_eq!(
            windows_path(&host, Path::new("/tmp/日本/decisions.tsv")).unwrap(),
            OsString::from(r"\\wsl$\archlinux\tmp\日本\decisions.tsv")
        );
        host.replies(
            "wslpath",
            &["-w", "/mnt/c/review/decisions.tsv"],
            true,
            "C:\\review\\decisions.tsv\n",
        );
        assert_eq!(
            windows_path(&host, Path::new("/mnt/c/review/decisions.tsv")).unwrap(),
            OsString::from(r"C:\review\decisions.tsv")
        );
    }

    #[test]
    fn wsl_without_proven_interop_falls_back_to_linux_association() {
        let host = FakeHost::default();
        host.exits("explorer.exe", 0);
        let environment = Environment {
            editor: None,
            desktop: true,
            wsl: true,
            wsl_interop: false,
            text_association: Some("org.gnome.TextEditor.desktop".into()),
        };
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["xdg-open", "gtk-launch"]);
        assert!(!host.probed("wslpath"));
        assert!(diagnostics.is_empty());
    }

    #[test]
    fn headless_without_display_prints_paths_and_instructions_without_failure() {
        let host = FakeHost::default();
        let environment = Environment {
            editor: None,
            desktop: false,
            wsl: false,
            wsl_interop: false,
            text_association: None,
        };
        let folder = Path::new("/srv/anki/pkg");
        let decisions = Path::new("/srv/anki/pkg/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        assert!(host.launches.borrow().is_empty());
        assert!(host.probes.borrow().is_empty());
        let text = text(&diagnostics);
        assert!(text.contains("/srv/anki/pkg"), "{text}");
        assert!(text.contains("/srv/anki/pkg/decisions.tsv"), "{text}");
        assert!(
            text.contains("batch review-export --batch-id batch-id"),
            "{text}"
        );
        assert!(
            text.contains("batch review-apply --batch-id batch-id --review-id review-id"),
            "{text}"
        );
    }

    #[test]
    fn headless_with_configured_terminal_editor_opens_only_the_editor() {
        let host = FakeHost::default();
        let environment = Environment {
            editor: Some("nano".into()),
            desktop: false,
            wsl: false,
            wsl_interop: false,
            text_association: None,
        };
        let folder = Path::new("/srv/anki/pkg");
        let decisions = Path::new("/srv/anki/pkg/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["nano"]);
        assert_eq!(host.argv(0), ["/srv/anki/pkg/decisions.tsv"]);
        // Пользователь всё равно видит каталог с проверенными копиями изображений.
        assert!(text(&diagnostics).contains("/srv/anki/pkg"));
    }

    // --- Отказ запуска не теряется и не выдаётся за успех ---

    #[test]
    fn missing_program_is_reported_and_never_success() {
        let host = FakeHost::default();
        host.missing("xdg-open");
        let environment = plain_linux(None, None);
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        let error = open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap_err();
        assert!(error.message.contains("xdg-open"), "{}", error.message);
        assert!(error.message.contains("не найдена"), "{}", error.message);
        assert_eq!(host.programs(), ["xdg-open"]);
        let text = text(&diagnostics);
        assert!(text.contains("/tmp/review-fixture/decisions.tsv"), "{text}");
        assert!(text.contains("review-apply"), "{text}");
    }

    #[test]
    fn missing_editor_is_reported_and_never_success() {
        let host = FakeHost::default();
        host.missing("not-installed-editor");
        let environment = Environment {
            editor: Some("not-installed-editor".into()),
            desktop: false,
            wsl: false,
            wsl_interop: false,
            text_association: None,
        };
        let folder = Path::new("/srv/anki/pkg");
        let decisions = Path::new("/srv/anki/pkg/decisions.tsv");
        let mut diagnostics = Vec::new();
        let error = open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap_err();
        assert!(
            error.message.contains("настроенный редактор"),
            "{}",
            error.message
        );
        assert_eq!(host.programs(), ["not-installed-editor"]);
    }

    #[test]
    fn failing_editor_exit_is_not_success_and_next_candidate_is_tried() {
        let host = FakeHost::default();
        host.exits("nano", 3);
        let environment = plain_linux(Some("nano"), Some("org.gnome.TextEditor.desktop"));
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["xdg-open", "nano", "gtk-launch"]);
        let text = text(&diagnostics);
        assert!(text.contains("настроенный редактор"), "{text}");
    }

    #[test]
    fn broken_editor_setting_refuses_without_launching_anything() {
        let host = FakeHost::default();
        let environment = plain_linux(Some("nano; other"), Some("org.gnome.TextEditor.desktop"));
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        let error = open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap_err();
        assert!(
            error.message.contains("shell-операторов"),
            "{}",
            error.message
        );
        assert!(host.launches.borrow().is_empty());
        assert!(text(&diagnostics).contains("/tmp/review-fixture/decisions.tsv"));
    }

    // --- WSL: межсистемное взаимодействие и преобразование путей ---

    #[test]
    fn interop_requires_enabled_binfmt_flag() {
        let host = FakeHost::default();
        assert!(!interop_registered(&host));
        host.has_file("/proc/sys/fs/binfmt_misc/WSLInterop", "enabled\n");
        assert!(interop_registered(&host));
        host.has_file("/proc/sys/fs/binfmt_misc/WSLInterop", "");
        assert!(!interop_registered(&host));
        host.has_file("/proc/sys/fs/binfmt_misc/WSLInterop", "disabled\n");
        assert!(!interop_registered(&host));
        host.has_file("/proc/sys/fs/binfmt_misc/WSLInterop-late", "enabled\n");
        assert!(interop_registered(&host));
    }

    #[test]
    fn distro_name_alone_is_a_session_marker_not_an_interop_proof() {
        let host = FakeHost::default();
        assert!(wsl_session(&host, Some(OsStr::new("Ubuntu"))));
        assert!(!wsl_session(&host, None));
        host.has_file(
            "/proc/sys/kernel/osrelease",
            "5.15.90.1-microsoft-standard-WSL2\n",
        );
        assert!(wsl_session(&host, None));
    }

    #[test]
    fn desktop_requires_non_empty_display_or_wayland() {
        assert!(!desktop_available(None, None));
        assert!(!desktop_available(
            Some(OsStr::new("")),
            Some(OsStr::new(""))
        ));
        assert!(desktop_available(Some(OsStr::new(":0")), None));
        assert!(desktop_available(None, Some(OsStr::new("wayland-0"))));
    }

    #[test]
    fn windows_editor_program_and_decisions_path_are_converted_in_wsl() {
        let host = FakeHost::default();
        host.replies(
            "wslpath",
            &["-u", r"C:\Program Files\Editor\edit.exe"],
            true,
            "/mnt/c/Program Files/Editor/edit.exe\n",
        );
        host.replies(
            "wslpath",
            &["-w", "/tmp/review-fixture/decisions.tsv"],
            true,
            "C:\\Users\\review-user\\decisions.tsv\r\n",
        );
        let environment = Environment {
            editor: Some(r#""C:\Program Files\Editor\edit.exe" --wait"#.into()),
            desktop: false,
            wsl: true,
            wsl_interop: true,
            text_association: None,
        };
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["/mnt/c/Program Files/Editor/edit.exe"]);
        assert_eq!(
            host.argv(0),
            ["--wait", "C:\\Users\\review-user\\decisions.tsv"]
        );
    }

    #[test]
    fn linux_editor_in_wsl_keeps_linux_paths_without_conversion() {
        let host = FakeHost::default();
        let environment = Environment {
            editor: Some("nano".into()),
            desktop: false,
            wsl: true,
            wsl_interop: false,
            text_association: None,
        };
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["nano"]);
        assert_eq!(host.argv(0), ["/tmp/review-fixture/decisions.tsv"]);
        assert!(!host.probed("wslpath"));
    }

    #[test]
    fn wslpath_failure_is_reported_and_not_success() {
        let host = FakeHost::default();
        host.replies("wslpath", &["-w", "/tmp/review-fixture"], false, "");
        let environment = Environment {
            editor: None,
            desktop: false,
            wsl: true,
            wsl_interop: true,
            text_association: None,
        };
        let folder = Path::new("/tmp/review-fixture");
        let decisions = Path::new("/tmp/review-fixture/decisions.tsv");
        let mut diagnostics = Vec::new();
        let error = open_with(
            &host,
            &environment,
            &request(folder, decisions),
            &mut diagnostics,
        )
        .unwrap_err();
        assert!(error.message.contains("wslpath"), "{}", error.message);
        assert!(host.launches.borrow().is_empty());
    }

    // --- Ассоциация text/plain ---

    #[test]
    fn text_association_requires_declared_desktop_entry() {
        let host = FakeHost::default();
        assert_eq!(text_association(&host), None);
        host.replies(
            "xdg-mime",
            &["query", "default", "text/plain"],
            true,
            "org.gnome.TextEditor.desktop\n",
        );
        assert_eq!(
            text_association(&host).as_deref(),
            Some("org.gnome.TextEditor.desktop")
        );
        host.replies(
            "xdg-mime",
            &["query", "default", "text/plain"],
            true,
            "libreoffice --calc\n",
        );
        assert_eq!(text_association(&host), None);
        host.replies("xdg-mime", &["query", "default", "text/plain"], false, "");
        assert_eq!(text_association(&host), None);
    }

    #[test]
    fn windows_program_detection_requires_drive_letter() {
        assert!(is_windows_program(OsStr::new(r"C:\Program Files\edit.exe")));
        assert!(is_windows_program(OsStr::new("C:/edit.exe")));
        assert!(is_windows_program(OsStr::new("c:edit.exe")));
        assert!(!is_windows_program(OsStr::new("/mnt/c/edit.exe")));
        assert!(!is_windows_program(OsStr::new("notepad.exe")));
        assert!(!is_windows_program(OsStr::new("")));
    }

    #[test]
    fn wsl_desktop_prefers_linux_folder_and_falls_back_to_explorer_on_failure() {
        let host = FakeHost::default();
        host.missing("xdg-open");
        host.resolves("explorer.exe");
        host.replies("wslpath", &["-w", "/tmp/review"], true, "C:\\review\n");
        let mut environment = plain_linux(Some("nano"), None);
        environment.wsl = true;
        environment.wsl_interop = true;
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &environment,
            &request(
                Path::new("/tmp/review"),
                Path::new("/tmp/review/decisions.tsv"),
            ),
            &mut diagnostics,
        )
        .unwrap();
        assert_eq!(host.programs(), ["xdg-open", "explorer.exe", "nano"]);
        assert!(text(&diagnostics).contains("xdg-open"));
    }

    #[test]
    fn wsl_exe_editors_convert_file_path_and_require_interop() {
        for editor in ["edit.exe --wait", "'/mnt/c/Program Files/edit.exe' --wait"] {
            let host = FakeHost::default();
            let mut environment = plain_linux(Some(editor), None);
            environment.desktop = false;
            environment.wsl = true;
            let request = request(
                Path::new("/tmp/review"),
                Path::new("/tmp/review/decisions.tsv"),
            );
            let mut diagnostics = Vec::new();
            assert!(open_with(&host, &environment, &request, &mut diagnostics).is_err());
            assert!(host.launches.borrow().is_empty());
            host.replies(
                "wslpath",
                &["-w", "/tmp/review/decisions.tsv"],
                true,
                "C:\\review\\decisions.tsv\n",
            );
            environment.wsl_interop = true;
            // Отсутствие Explorer не мешает явно выбранному Windows-редактору.
            open_with(&host, &environment, &request, &mut diagnostics).unwrap();
            assert_eq!(host.launches.borrow().len(), 1);
            assert_eq!(host.argv(0), ["--wait", "C:\\review\\decisions.tsv"]);
        }
    }

    #[test]
    fn failed_folder_launch_still_prints_paths_after_successful_editor() {
        let host = FakeHost::default();
        host.missing("xdg-open");
        let mut diagnostics = Vec::new();
        open_with(
            &host,
            &plain_linux(Some("nano"), None),
            &request(
                Path::new("/tmp/review"),
                Path::new("/tmp/review/decisions.tsv"),
            ),
            &mut diagnostics,
        )
        .unwrap();
        let message = text(&diagnostics);
        assert!(message.contains("каталог: /tmp/review"));
        assert!(message.contains("review-apply"));
    }

    #[test]
    fn linux_paths_preserve_non_utf8_bytes_and_literal_shell_characters() {
        use std::os::unix::ffi::OsStringExt;
        let host = FakeHost::default();
        let path = PathBuf::from(OsString::from_vec(
            b"/tmp/review \xff '$(echo injected)'/decisions.tsv".to_vec(),
        ));
        let step = invocation(
            &host,
            &plain_linux(Some("nano"), None),
            &path,
            &Candidate::Editor {
                program: "nano".into(),
                args: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(step.args, vec![path.as_os_str().to_owned()]);
        assert_eq!(step.program, "nano");
    }

    #[test]
    fn converted_paths_must_be_nonempty_and_unambiguous() {
        for value in ["", "\n", "C:\\one\nC:\\two\n", "C:\\bad\0path\n"] {
            let host = FakeHost::default();
            host.replies("wslpath", &["-w", "/tmp/review"], true, value);
            assert!(windows_path(&host, Path::new("/tmp/review")).is_err());
            assert!(host.launches.borrow().is_empty());
        }
    }
}
