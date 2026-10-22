//! Процесс Chromium, принадлежащий одному `BrowserSession`.
//!
//! `chromiumoxide` 0.9.1 запускает браузер внутри `Browser::launch` без доступа к
//! настройке дочернего процесса, поэтому Chromium попадал в foreground-группу
//! терминала и получал терминальный `SIGINT` одновременно с CLI — раньше, чем
//! координатор успевал выполнить свою семантику отмены. Здесь браузер
//! запускается в собственной группе процессов (`process_group(0)`), а CDP
//! подключается к нему через `Browser::connect_with_config`. Владелец группы —
//! только этот объект: сигнал отправляется лишь группе, созданной этим запуском.
//! До сбора лидера `wait` номер группы не может перейти к постороннему процессу;
//! после сбора `SIGKILL` потомкам отправляется, только пока проверка подтверждает
//! непустую группу, — между проверкой и сигналом остаётся узкое окно.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

/// Максимальный объём `stderr`, сохраняемый для диагностики отказа запуска.
const MAX_LAUNCH_STDERR_BYTES: usize = 16 * 1024;
/// После выхода лидера его потомки (renderer, zygote, GPU) получают время на
/// самостоятельный выход до принудительного завершения группы.
const DESCENDANT_EXIT_GRACE: Duration = Duration::from_millis(500);
const PROC_ROOT: &str = "/proc";

/// Аргументы, совпадающие со стандартным запуском `chromiumoxide` 0.9.1
/// (`DEFAULT_ARGS`, headless, без расширений, с песочницей) для настроек
/// `BrowserSession`. Отдельный порт отладки выбирает сам Chromium.
const DEFAULT_ARGS: [&str; 24] = [
    "--disable-background-networking",
    "--enable-features=NetworkService,NetworkServiceInProcess",
    "--disable-background-timer-throttling",
    "--disable-backgrounding-occluded-windows",
    "--disable-breakpad",
    "--disable-client-side-phishing-detection",
    "--disable-component-extensions-with-background-pages",
    "--disable-default-apps",
    "--disable-dev-shm-usage",
    "--disable-features=TranslateUI",
    "--disable-hang-monitor",
    "--disable-ipc-flooding-protection",
    "--disable-popup-blocking",
    "--disable-prompt-on-repost",
    "--disable-renderer-backgrounding",
    "--disable-sync",
    "--force-color-profile=srgb",
    "--metrics-recording-only",
    "--no-first-run",
    "--enable-automation",
    "--password-store=basic",
    "--use-mock-keychain",
    "--enable-blink-features=IdleDetection",
    "--lang=en_US",
];

/// Что именно нужно запустить: исполняемый файл, профиль и окружение.
#[derive(Debug, Clone)]
pub(crate) struct BrowserLaunchSpec {
    pub executable: PathBuf,
    pub user_data_dir: PathBuf,
    pub tmpdir: String,
    pub launch_timeout: Duration,
}

impl BrowserLaunchSpec {
    pub(crate) fn arguments(&self) -> Vec<String> {
        let mut arguments = DEFAULT_ARGS.map(str::to_owned).to_vec();
        arguments.extend([
            "--remote-debugging-port=0".to_owned(),
            "--disable-extensions".to_owned(),
            format!("--user-data-dir={}", self.user_data_dir.display()),
            "--headless".to_owned(),
            "--hide-scrollbars".to_owned(),
            "--mute-audio".to_owned(),
            "--incognito".to_owned(),
        ]);
        arguments
    }
}

/// Наблюдение остановки принадлежащего процесса и его группы.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessStop {
    /// Лидер завершился сам, а в группе не осталось процессов.
    Exited,
    /// Группа завершена принудительно и лидер собран.
    Killed,
}

/// Подробности последней остановки для диагностики lifecycle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProcessStopDetails {
    /// Сколько ждали самостоятельного выхода лидера.
    pub leader_wait: Duration,
    /// Сколько заняло доказательство пустоты группы после выхода лидера.
    pub group_drain: Duration,
    /// Потребовалось ли принудительно завершить оставшихся потомков.
    pub descendants_killed: bool,
}

/// Ошибка запуска, после которой принадлежащая группа уже остановлена либо
/// остановка явно недоказана.
#[derive(Debug)]
pub(crate) struct ProcessLaunchFailure {
    pub message: String,
    pub stop: Result<ProcessStop, String>,
}

/// Chromium и все его потомки в группе с номером, равным PID лидера.
#[derive(Debug)]
pub(crate) struct OwnedBrowserProcess {
    child: Child,
    group: Pid,
    /// Статус собранного лидера; до сбора номер группы не может быть переиспользован.
    exit: Option<ExitStatus>,
    details: ProcessStopDetails,
}

impl OwnedBrowserProcess {
    /// Запускает браузер в новой группе процессов и дожидается адреса CDP.
    pub(crate) async fn spawn(
        spec: &BrowserLaunchSpec,
    ) -> Result<(Self, String), ProcessLaunchFailure> {
        let not_started = |message: String| ProcessLaunchFailure {
            message,
            stop: Ok(ProcessStop::Exited),
        };
        let executable = resolve_executable(&spec.executable)
            .await
            .map_err(|error| not_started(format!("запуск браузера: {error}")))?;
        let mut command = Command::new(&executable);
        command
            .args(spec.arguments())
            .env("TMPDIR", &spec.tmpdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            // Терминальные сигналы foreground-группы CLI больше не доходят до
            // браузера напрямую: его останавливает только владелец сеанса.
            .process_group(0)
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| not_started(format!("запуск браузера: {error}")))?;
        let group = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(Pid::from_raw)
            .ok_or_else(|| not_started("запуск браузера: PID процесса недоступен".into()))?;
        let stderr = child.stderr.take();
        let mut process = Self {
            child,
            group,
            exit: None,
            details: ProcessStopDetails::default(),
        };
        let address = match stderr {
            Some(stderr) => tokio::time::timeout(
                spec.launch_timeout,
                devtools_address(stderr, &mut process.child),
            )
            .await
            .unwrap_or_else(|_| {
                Err("запуск браузера: истекло время ожидания адреса DevTools".into())
            }),
            None => Err("запуск браузера: stderr браузера недоступен".into()),
        };
        match address {
            Ok(address) => Ok((process, address)),
            Err(message) => {
                let stop = process.stop(Duration::from_secs(2)).await;
                Err(ProcessLaunchFailure { message, stop })
            }
        }
    }

    pub(crate) fn pid(&self) -> i32 {
        self.group.as_raw_nonzero().get()
    }

    pub(crate) fn stop_details(&self) -> ProcessStopDetails {
        self.details
    }

    /// Ждёт самостоятельного выхода лидера; затем доказывает пустоту группы.
    /// Возвращает `Ok(None)`, если лидер не завершился за отведённое время.
    pub(crate) async fn wait_exit(
        &mut self,
        limit: Duration,
    ) -> Result<Option<ExitStatus>, String> {
        if let Some(status) = self.exit {
            self.drain_group(limit).await?;
            return Ok(Some(status));
        }
        let started = Instant::now();
        let waited = tokio::time::timeout(limit, self.child.wait()).await;
        self.details.leader_wait = started.elapsed();
        match waited {
            Ok(Ok(status)) => {
                self.exit = Some(status);
                self.drain_group(limit).await?;
                Ok(Some(status))
            }
            Ok(Err(error)) => Err(format!(
                "browser_process_wait_failed: {}",
                crate::diagnostics::safe_message(&error.to_string())
            )),
            Err(_) => Ok(None),
        }
    }

    /// Принудительно завершает собственную группу и собирает лидера.
    pub(crate) async fn kill(&mut self, limit: Duration) -> Result<(), String> {
        if self.exit.is_none() {
            self.signal_group(Signal::KILL)
                .map_err(|error| format!("browser_kill_failed: {error}"))?;
            match tokio::time::timeout(limit, self.child.wait()).await {
                Ok(Ok(status)) => self.exit = Some(status),
                Ok(Err(error)) => {
                    return Err(format!(
                        "browser_kill_failed: {}",
                        crate::diagnostics::safe_message(&error.to_string())
                    ));
                }
                Err(_) => return Err("browser_kill_timeout".into()),
            }
        }
        self.drain_group(limit).await
    }

    /// Самостоятельный выход либо принудительная остановка; результат типизирован.
    pub(crate) async fn stop(&mut self, limit: Duration) -> Result<ProcessStop, String> {
        if self.exit.is_none() {
            self.exit = self.child.try_wait().map_err(|error| {
                format!(
                    "browser_process_wait_failed: {}",
                    crate::diagnostics::safe_message(&error.to_string())
                )
            })?;
        }
        // Уже вышедший лидер не отменяет лимит на сбор его потомков.
        if self.exit.is_some() {
            self.drain_group(limit).await?;
            return Ok(ProcessStop::Exited);
        }
        self.kill(limit).await.map(|()| ProcessStop::Killed)
    }

    /// Потомки лидера (renderer, zygote, GPU) остаются в группе и после его выхода.
    async fn drain_group(&mut self, limit: Duration) -> Result<(), String> {
        let started = Instant::now();
        let deadline = started + limit;
        let grace = started + DESCENDANT_EXIT_GRACE.min(limit);
        let result = loop {
            match self.group_has_members() {
                Ok(false) => break Ok(()),
                Ok(true) => {}
                Err(error) => break Err(format!("browser_process_group_probe_failed: {error}")),
            }
            let now = Instant::now();
            if !self.details.descendants_killed && now >= grace {
                if let Err(error) = self.signal_group(Signal::KILL) {
                    break Err(format!("browser_kill_failed: {error}"));
                }
                self.details.descendants_killed = true;
            }
            if now >= deadline {
                break Err("browser_process_group_stop_unproven".into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        self.details.group_drain = started.elapsed();
        result
    }

    fn group_has_members(&self) -> io::Result<bool> {
        match test_kill_process_group(self.group) {
            Ok(()) => {}
            Err(rustix::io::Errno::SRCH) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        // `kill(-pgid, 0)` успешен и для zombie: без reaper (например, CLI как
        // PID 1 контейнера) вышедшие потомки иначе навсегда «занимали» бы группу.
        group_has_live_members(Path::new(PROC_ROOT), self.pid(), std::process::id())
    }

    fn signal_group(&self, signal: Signal) -> io::Result<()> {
        match kill_process_group(self.group, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for OwnedBrowserProcess {
    fn drop(&mut self) {
        // Резервная граница для паники и отмены Future: явное закрытие уже собрало
        // лидера и доказало пустоту группы. `kill_on_drop` затем соберёт лидера.
        if self.exit.is_none() {
            let _ = self.signal_group(Signal::KILL);
        }
    }
}

/// Есть ли в группе процесс не в состоянии zombie/dead. Если `proc_root` не
/// принадлежит пространству PID текущего процесса или отсутствует, единственным
/// свидетельством остаётся успешный `kill(-pgid, 0)`, и группа считается непустой.
fn group_has_live_members(proc_root: &Path, group: i32, own_pid: u32) -> io::Result<bool> {
    let own = std::fs::read_link(proc_root.join("self"));
    if !own.is_ok_and(|own| own.as_os_str() == own_pid.to_string().as_str()) {
        return Ok(true);
    }
    for entry in std::fs::read_dir(proc_root)? {
        let entry = entry?;
        let name = entry.file_name();
        if name.is_empty() || !name.as_encoded_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let stat = match std::fs::read(entry.path().join("stat")) {
            Ok(stat) => stat,
            // Процесс завершился между чтением каталога и `stat`.
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        let (state, pgrp) = stat_state_and_group(&stat).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "невалидный /proc/PID/stat")
        })?;
        if pgrp == group && !matches!(state, b'Z' | b'X' | b'x') {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Состояние (поле 3) и группа процессов (поле 5) из `/proc/PID/stat`; поле
/// `comm` может содержать `)`, поэтому разбор идёт от последней скобки.
fn stat_state_and_group(stat: &[u8]) -> Option<(u8, i32)> {
    let closing = stat.iter().rposition(|byte| *byte == b')')?;
    let tail = std::str::from_utf8(&stat[closing + 1..]).ok()?;
    let mut fields = tail.split_ascii_whitespace();
    let state = fields.next().filter(|state| state.len() == 1)?.as_bytes()[0];
    let pgrp = fields.nth(1)?.parse().ok()?;
    Some((state, pgrp))
}

async fn resolve_executable(path: &Path) -> io::Result<PathBuf> {
    // Как и `chromiumoxide`: канонический путь снижает проблемы песочницы, а
    // обёртку snap запускаем по исходному абсолютному пути.
    let canonical = tokio::fs::canonicalize(path).await?;
    if canonical.to_string_lossy().ends_with("/snap") {
        return std::path::absolute(path);
    }
    Ok(canonical)
}

async fn devtools_address(
    stderr: tokio::process::ChildStderr,
    child: &mut Child,
) -> Result<String, String> {
    let mut reader = BufReader::new(stderr);
    let mut captured = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        tokio::select! {
            read = reader.read_until(b'\n', &mut line) => {
                let read = read.map_err(|error| format!("запуск браузера: чтение stderr: {error}"))?;
                if read == 0 {
                    return Err(launch_output_error("запуск браузера: stderr закрыт до адреса DevTools", &captured));
                }
                if let Some(address) = parse_devtools_address(&String::from_utf8_lossy(&line)) {
                    return Ok(address);
                }
                let room = MAX_LAUNCH_STDERR_BYTES.saturating_sub(captured.len());
                captured.extend_from_slice(&line[..line.len().min(room)]);
            }
            status = child.wait() => {
                let status = status.map_err(|error| format!("запуск браузера: {error}"))?;
                return Err(launch_output_error(&format!("запуск браузера: процесс завершился ({status}) до адреса DevTools"), &captured));
            }
        }
    }
}

fn launch_output_error(message: &str, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        message.to_owned()
    } else {
        format!(
            "{message}; stderr: {}",
            crate::diagnostics::safe_message(stderr)
        )
    }
}

fn parse_devtools_address(line: &str) -> Option<String> {
    let (_, address) = line.rsplit_once("listening on ")?;
    let address = address.trim();
    (address.starts_with("ws") && address.contains("devtools/browser")).then(|| address.to_owned())
}

#[cfg(test)]
#[path = "browser_process_tests.rs"]
mod tests;
