//! Владелец собственного процесса `llama-server`.
//!
//! Жизненный цикл: эксклюзивная блокировка менеджера → сбор осиротевших
//! процессов прежних запусков по их арендам → запуск с явными аргументами без
//! shell → доказанная готовность (`/health`) и идентичность (`/props`,
//! `/v1/models`) → запросы → остановка группы процессов с проверкой, что в
//! ней никого не осталось, порт освобождён, а процесс Windows (при interop)
//! исчез.
//!
//! Свой сервер отличается от чужого случайным `--alias` и `--api-key`,
//! известными только этому запуску: сервер, который вернул другой alias, —
//! чужой, даже если слушает тот же порт и загрузил ту же модель. Сигналы
//! отправляются только группе, созданной этим запуском (`process_group(0)`),
//! и только пока её лидер не собран. Чужие процессы не завершаются никогда.
//!
//! При `windows_interop` лидер группы — прокси WSL interop, а сам сервер —
//! процесс Windows, чьими CPU/GPU/памятью он пользуется. Завершение прокси
//! сигналом завершает и процесс Windows; это подтверждается отдельно:
//! исчезновением процесса с нашим alias из списка Windows и освобождением порта.
//!
//! Аренда (`leases/<alias>.json`) переживает аварийное завершение CLI: следующий
//! запуск находит прокси/процесс по PID, времени старта и alias в командной
//! строке и останавливает его как собственный. Совпадение только PID не
//! считается доказательством владения. Сервер без надёжно записанной аренды
//! не считается запущенным: процесс останавливается, запуск отклоняется.
//!
//! Процесс Windows ищется по собственному alias в командной строке, а не по
//! имени исполняемого файла: настроенный бинарник может называться иначе, чем
//! `llama-server.exe`. Завершается он только по PID, командная строка которого
//! в момент завершения всё ещё содержит этот alias.

use std::fs;
use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use url::Url;

use crate::inference::{Cancellation, InferenceFailure};
use crate::inference_config::{LlamaBackend, LlamaModel, VisionMode};

/// Хранимый хвост вывода сервера для диагностики отказа.
const OUTPUT_TAIL_BYTES: usize = 16 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const POWERSHELL: &str = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";
const WINDOWS_PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Нижняя граница автовыбора порта: ниже — привилегированные и часто занятые порты.
const AUTO_PORT_FLOOR: u16 = 10_000;
/// Динамический диапазон портов Windows по умолчанию: при `windows_interop`
/// его занимают исходящие соединения Windows.
const WINDOWS_DYNAMIC_PORTS: (u16, u16) = (49_152, 65_535);
const AUTO_PORT_ATTEMPTS: usize = 64;

/// Аргументы и окружение запуска, вычисленные из профиля.
#[derive(Debug, Clone)]
pub(crate) struct LaunchPlan {
    pub binary: PathBuf,
    pub windows_interop: bool,
    pub host: IpAddr,
    pub port: u16,
    /// Путь модели в том виде, в каком его видит сервер (Windows при interop).
    pub model_argument: String,
    pub projector_argument: Option<String>,
    pub model: LlamaModel,
    /// Задача передаёт изображение: готовность требует подтверждённого vision.
    pub require_vision: bool,
    pub startup_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub connect_timeout: Duration,
}

impl LaunchPlan {
    pub(crate) fn new(
        backend: &LlamaBackend,
        model: &LlamaModel,
    ) -> Result<Self, InferenceFailure> {
        let binary = backend.binary.clone().ok_or_else(|| {
            InferenceFailure::new("backend_not_configured", "путь к llama-server не задан")
        })?;
        let weights = model
            .model
            .as_ref()
            .ok_or_else(|| InferenceFailure::new("model_not_configured", "путь к GGUF не задан"))?;
        let argument = |path: &Path| -> Result<String, InferenceFailure> {
            if backend.windows_interop {
                crate::inference_config::to_windows_path(path).ok_or_else(|| {
                    InferenceFailure::new(
                        "path_not_visible_to_windows",
                        format!(
                            "{} не находится на диске Windows (/mnt/<диск>/…), а сервер запускается в Windows",
                            path.display()
                        ),
                    )
                })
            } else {
                path.to_str()
                    .map(str::to_owned)
                    .ok_or_else(|| InferenceFailure::new("invalid_path", "путь модели не UTF-8"))
            }
        };
        let projector_argument = match (model.vision, &model.projector) {
            (VisionMode::Projector, Some(path)) => Some(argument(path)?),
            (VisionMode::Projector, None) => {
                return Err(InferenceFailure::new(
                    "projector_not_configured",
                    "профиль требует отдельный mmproj, но путь не задан",
                ));
            }
            _ => None,
        };
        Ok(Self {
            binary,
            windows_interop: backend.windows_interop,
            host: backend.host,
            port: backend.port,
            model_argument: argument(weights)?,
            projector_argument,
            model: model.clone(),
            require_vision: true,
            startup_timeout: Duration::from_secs(backend.startup_timeout_seconds),
            shutdown_timeout: Duration::from_secs(backend.shutdown_timeout_seconds),
            connect_timeout: Duration::from_secs(backend.connect_timeout_seconds),
        })
    }

    /// Аргументы процесса. Значения передаются отдельными элементами argv.
    pub(crate) fn arguments(&self, port: u16, alias: &str, api_key: &str) -> Vec<String> {
        let model = &self.model;
        let mut arguments = vec!["--model".to_owned(), self.model_argument.clone()];
        match &self.projector_argument {
            Some(projector) => {
                arguments.extend(["--mmproj".to_owned(), projector.clone()]);
                if !model.projector_offload {
                    arguments.push("--no-mmproj-offload".to_owned());
                }
            }
            None => arguments.push("--no-mmproj-auto".to_owned()),
        }
        arguments.extend([
            "--host".to_owned(),
            self.host.to_string(),
            "--port".to_owned(),
            port.to_string(),
            "--alias".to_owned(),
            alias.to_owned(),
            "--api-key".to_owned(),
            api_key.to_owned(),
            "--ctx-size".to_owned(),
            model.context_size.to_string(),
            "--gpu-layers".to_owned(),
            model.gpu_layers.argument(),
            "--batch-size".to_owned(),
            model.batch_size.to_string(),
            "--ubatch-size".to_owned(),
            model.ubatch_size.to_string(),
            "--parallel".to_owned(),
            model.parallel.to_string(),
            "--flash-attn".to_owned(),
            model.flash_attention.as_str().to_owned(),
            "--reasoning".to_owned(),
            model.reasoning.as_str().to_owned(),
            "--no-webui".to_owned(),
        ]);
        if model.threads > 0 {
            arguments.extend(["--threads".to_owned(), model.threads.to_string()]);
        }
        arguments
    }
}

/// Что известно о сервере после доказанной готовности.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ServerReady {
    pub pid: i32,
    pub port: u16,
    pub alias: String,
    pub build_info: Option<String>,
    pub model_path: String,
    pub vision: bool,
    pub context_size: Option<u64>,
    pub ready_ms: u128,
    pub windows: Option<WindowsProcess>,
    pub arguments: Vec<String>,
    pub orphans_stopped: Vec<OrphanStop>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct WindowsProcess {
    pub pid: u32,
    pub working_set_bytes: Option<u64>,
}

/// Состояние аренды прежнего запуска.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OrphanStop {
    pub pid: i32,
    pub alias: String,
    pub port: u16,
    pub windows_interop: bool,
    /// Файл аренды: по нему владелец находит процесс при следующем запуске.
    pub lease: String,
    /// Аренда снята: собственный процесс остановлен или исчез.
    pub stopped: bool,
    /// `stopped` — остановлен и исчез; `stale` — живого процесса нет;
    /// `process_alive` — собственный процесс Linux ещё жив;
    /// `windows_process_alive` — процесс Windows с нашим alias жив;
    /// `windows_probe_failed` — проверить процесс Windows не удалось;
    /// `invalid` — файл аренды не читается.
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub windows_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe_error: Option<String>,
}

impl OrphanStop {
    /// Человекочитаемое описание для сообщения об отказе запуска.
    fn describe(&self) -> String {
        let mut text = format!("{} (lease {})", self.alias, self.lease);
        match self.state.as_str() {
            "process_alive" => text.push_str(&format!(": процесс {} ещё жив", self.pid)),
            "windows_process_alive" => {
                text.push_str(": процесс Windows ещё жив");
                if let Some(pid) = self.windows_pid {
                    text.push_str(&format!(", PID Windows {pid}"));
                }
                if let Some(error) = &self.probe_error {
                    text.push_str(&format!(" ({error})"));
                }
            }
            "windows_probe_failed" => text.push_str(&format!(
                ": проверить процесс Windows не удалось ({})",
                self.probe_error.as_deref().unwrap_or("?")
            )),
            "invalid" => text.push_str(
                ": файл аренды не читается; проверьте его и процессы llama-server вручную",
            ),
            other => text.push_str(&format!(": {other}")),
        }
        text
    }
}

/// Доказательства остановки собственного процесса.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct StopReport {
    pub pid: i32,
    /// `exited` — сервер завершился сам; `terminated` — после SIGTERM; `killed` — после SIGKILL.
    pub method: String,
    pub leader_exit: Option<String>,
    pub group_empty: bool,
    pub port_released: bool,
    /// `Some(true)`, если процесс Windows с нашим alias больше не существует.
    pub windows_process_gone: Option<bool>,
    pub elapsed_ms: u128,
    pub lease_removed: bool,
    pub problems: Vec<String>,
}

impl StopReport {
    pub(crate) fn proven(&self) -> bool {
        self.problems.is_empty()
            && self.group_empty
            && self.port_released
            && self.windows_process_gone != Some(false)
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lease {
    pid: i32,
    start_ticks: u64,
    alias: String,
    port: u16,
    windows_interop: bool,
    created_unix_seconds: u64,
}

/// Каталог состояния менеджера: аренды и блокировка. Только локальные данные пользователя.
pub(crate) fn state_directory() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::home_dir().map(|home| home.join(".cache")))?;
    Some(base.join("anki-deck-toolkit").join("inference"))
}

/// Эксклюзивная блокировка менеджера на время владения сервером.
#[derive(Debug)]
struct ManagerLock {
    _file: fs::File,
}

impl ManagerLock {
    fn acquire(directory: &Path) -> Result<Self, InferenceFailure> {
        fs::create_dir_all(directory)
            .map_err(|error| io_failure("manager_state_unavailable", error))?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(directory.join("managed.lock"))
            .map_err(|error| io_failure("manager_state_unavailable", error))?;
        match rustix::fs::flock(
            file.as_fd(),
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        ) {
            Ok(()) => Ok(Self { _file: file }),
            Err(rustix::io::Errno::WOULDBLOCK) => Err(InferenceFailure::new(
                "managed_server_busy",
                "другая операция уже владеет управляемым llama-server; дождитесь её завершения",
            )),
            Err(error) => Err(io_failure("manager_state_unavailable", error.into())),
        }
    }
}

fn io_failure(code: &'static str, error: io::Error) -> InferenceFailure {
    InferenceFailure::new(code, crate::diagnostics::safe_message(&error.to_string()))
}

fn random_token() -> Result<String, InferenceFailure> {
    let mut bytes = [0_u8; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| io::Read::read_exact(&mut file, &mut bytes))
        .map_err(|error| io_failure("random_unavailable", error))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Время старта процесса в тактах с загрузки системы (`/proc/<pid>/stat`, поле 22).
fn process_start_ticks(pid: i32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = stat.rsplit_once(')')?.1;
    after_name.split_whitespace().nth(19)?.parse().ok()
}

fn process_command_line(pid: i32) -> Option<Vec<String>> {
    let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        bytes
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect(),
    )
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// Есть ли в группе живой (не зомби) процесс. Зомби, которого ещё не собрал
/// его родитель, ресурсов сервера уже не держит.
fn group_alive(group: Pid) -> bool {
    if matches!(test_kill_process_group(group), Err(rustix::io::Errno::SRCH)) {
        return false;
    }
    crate::browser_process::group_has_live_members(
        Path::new("/proc"),
        group.as_raw_nonzero().get(),
        std::process::id(),
    )
    .unwrap_or(true)
}

/// Проверка и завершение процессов Windows; подменяется в тестах.
pub(crate) trait WindowsProcesses {
    /// Процесс Windows с данным alias в командной строке: `Ok(None)` — такого нет.
    async fn find(&self, alias: &str) -> Result<Option<WindowsProcess>, String>;
    /// Завершает процесс `pid`, только если его командная строка всё ещё содержит alias.
    async fn terminate(&self, pid: u32, alias: &str) -> Result<(), String>;
}

/// Процессы Windows через PowerShell WSL interop.
pub(crate) struct PowerShell;

impl WindowsProcesses for PowerShell {
    async fn find(&self, alias: &str) -> Result<Option<WindowsProcess>, String> {
        windows_process(alias).await
    }

    async fn terminate(&self, pid: u32, alias: &str) -> Result<(), String> {
        terminate_windows_process(pid, alias).await
    }
}

/// Ждёт исчезновения процесса Windows с alias; по истечении `grace` завершает
/// его по PID (только пока командная строка содержит alias) и проверяет снова.
async fn stop_windows_process<W: WindowsProcesses>(
    windows: &W,
    alias: &str,
    grace: Duration,
) -> Result<(), (Option<u32>, String)> {
    let deadline = Instant::now() + grace;
    let alive = loop {
        match windows.find(alias).await {
            Ok(None) => return Ok(()),
            Ok(Some(_)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Ok(Some(process)) => break process,
            Err(error) => return Err((None, error)),
        }
    };
    if let Err(error) = windows.terminate(alive.pid, alias).await {
        return Err((
            Some(alive.pid),
            format!("windows_terminate_failed: {error}"),
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match windows.find(alias).await {
            Ok(None) => return Ok(()),
            Ok(Some(_)) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Ok(Some(process)) => {
                return Err((Some(process.pid), "windows_process_alive".into()));
            }
            Err(error) => return Err((Some(alive.pid), error)),
        }
    }
}

/// Останавливает осиротевшие собственные процессы прежних запусков.
async fn stop_orphans(directory: &Path) -> Vec<OrphanStop> {
    orphans_with(directory, &PowerShell, Duration::from_secs(15), true).await
}

/// Состояние аренд прежних запусков; при `stop` — остановка их собственных
/// процессов. Аренда удаляется только при доказанной остановке: для
/// `windows_interop` исчезновение прокси WSL не доказывает исчезновения
/// процесса Windows, поэтому он проверяется отдельно, как и при обычной
/// остановке. Без `stop` сигналы не отправляются и файлы не меняются.
async fn orphans_with<W: WindowsProcesses>(
    directory: &Path,
    windows: &W,
    windows_grace: Duration,
    stop: bool,
) -> Vec<OrphanStop> {
    let leases = directory.join("leases");
    let Ok(entries) = fs::read_dir(&leases) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    let mut reported = Vec::new();
    for path in paths {
        let lease: Option<Lease> = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let Some(lease) = lease else {
            // Нечитаемая аренда не доказывает остановку процесса: она остаётся
            // на месте и при `stop`, а запуск отклоняется через `orphan_failure`.
            reported.push(OrphanStop {
                pid: 0,
                alias: String::new(),
                port: 0,
                windows_interop: false,
                lease: path.display().to_string(),
                stopped: false,
                state: "invalid".into(),
                windows_pid: None,
                probe_error: None,
            });
            continue;
        };
        let mut orphan = OrphanStop {
            pid: lease.pid,
            alias: lease.alias.clone(),
            port: lease.port,
            windows_interop: lease.windows_interop,
            lease: path.display().to_string(),
            stopped: false,
            state: "stale".into(),
            windows_pid: None,
            probe_error: None,
        };
        let owned = process_start_ticks(lease.pid) == Some(lease.start_ticks)
            && process_command_line(lease.pid)
                .is_some_and(|argv| argv.iter().any(|argument| argument == &lease.alias));
        let mut group_gone = !owned;
        if owned
            && stop
            && let Some(group) = Pid::from_raw(lease.pid)
        {
            let _ = kill_process_group(group, Signal::TERM);
            let deadline = Instant::now() + Duration::from_secs(10);
            while group_alive(group) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if group_alive(group) {
                let _ = kill_process_group(group, Signal::KILL);
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            group_gone = !group_alive(group);
        }
        if !group_gone {
            orphan.state = "process_alive".into();
        }
        let mut windows_gone = true;
        if lease.windows_interop && group_gone {
            let result = if stop {
                stop_windows_process(windows, &lease.alias, windows_grace).await
            } else {
                match windows.find(&lease.alias).await {
                    Ok(None) => Ok(()),
                    Ok(Some(process)) => Err((Some(process.pid), "windows_process_alive".into())),
                    Err(error) => Err((None, error)),
                }
            };
            if let Err((pid, error)) = result {
                windows_gone = false;
                orphan.windows_pid = pid;
                // Известный PID — процесс жив, даже если завершить его не удалось.
                orphan.state = if pid.is_some() {
                    "windows_process_alive"
                } else {
                    "windows_probe_failed"
                }
                .into();
                orphan.probe_error = (error != "windows_process_alive").then_some(error);
            }
        }
        let gone = group_gone && windows_gone;
        if gone && stop {
            orphan.stopped = fs::remove_file(&path).is_ok();
            if owned {
                orphan.state = "stopped".into();
            }
        }
        if !stop || owned || !gone {
            if !gone {
                tracing::warn!(
                    stage = "inference_orphan",
                    pid = lease.pid,
                    state = orphan.state.as_str(),
                    lease = orphan.lease.as_str(),
                    "осиротевший llama-server прежнего запуска не остановлен; аренда сохранена"
                );
            }
            reported.push(orphan);
        }
    }
    reported
}

/// Отказ запуска из-за неостановленных сирот: живой процесс отличается от
/// недоступной проверки, сообщение называет аренду и путь восстановления.
fn orphan_failure(orphans: &[OrphanStop]) -> Option<InferenceFailure> {
    let remaining: Vec<&OrphanStop> = orphans.iter().filter(|orphan| !orphan.stopped).collect();
    if remaining.is_empty() {
        return None;
    }
    let alive = remaining
        .iter()
        .any(|orphan| orphan.state != "windows_probe_failed");
    let code = if alive {
        "orphan_server_alive"
    } else {
        "orphan_server_unverified"
    };
    let described: Vec<String> = remaining.iter().map(|orphan| orphan.describe()).collect();
    let mut failure = InferenceFailure::new(
        code,
        format!(
            "осиротевший llama-server прежнего запуска не остановлен, аренда сохранена: {}. \
             Новая модель не загружается, пока прежняя может занимать память. \
             Восстановление: `inference orphans` показывает состояние, \
             `inference orphans --stop` повторяет остановку собственного процесса; \
             файл аренды вручную не удаляйте",
            described.join("; ")
        ),
    );
    failure.orphans = orphans.to_vec();
    Some(failure)
}

/// Аренды прежних запусков под блокировкой менеджера: без `stop` — только
/// состояние, со `stop` — остановка собственных процессов, как при запуске.
pub(crate) async fn inspect_orphans(
    stop: bool,
) -> Result<(PathBuf, Vec<OrphanStop>), InferenceFailure> {
    let directory = state_directory().ok_or_else(|| {
        InferenceFailure::new(
            "manager_state_unavailable",
            "не найден домашний каталог для аренды процесса",
        )
    })?;
    let _lock = ManagerLock::acquire(&directory)?;
    let orphans = orphans_with(&directory, &PowerShell, Duration::from_secs(15), stop).await;
    Ok((directory.join("leases"), orphans))
}

/// Порт свободен, если на нём никто не отвечает. Для нативного сервера порт
/// дополнительно занимается и сразу освобождается; для процесса Windows так
/// делать нельзя: в mirrored-сети WSL порт, однажды занятый со стороны Linux,
/// какое-то время недоступен процессам Windows.
fn port_free(host: IpAddr, port: u16, timeout: Duration, windows_interop: bool) -> bool {
    let address = SocketAddr::new(host, port);
    if std::net::TcpStream::connect_timeout(&address, timeout.min(Duration::from_secs(1))).is_ok() {
        return false;
    }
    windows_interop || TcpListener::bind(address).is_ok()
}

/// Диапазон эфемерных портов Linux. В mirrored-сети WSL он зарезервирован
/// за Linux, и процесс Windows не может занять порт из него.
fn linux_ephemeral_range() -> Option<(u16, u16)> {
    let text = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").ok()?;
    let mut parts = text.split_whitespace().map(str::parse::<u16>);
    let (low, high) = (parts.next()?.ok()?, parts.next()?.ok()?);
    (low <= high).then_some((low, high))
}

/// Допустимые диапазоны автовыбора: от `AUTO_PORT_FLOOR` до конца, кроме
/// эфемерного диапазона Linux и, при `windows_interop`, динамического
/// диапазона Windows.
fn auto_port_ranges(ephemeral: Option<(u16, u16)>, windows_interop: bool) -> Vec<(u16, u16)> {
    let mut ranges = vec![(AUTO_PORT_FLOOR, u16::MAX)];
    let excluded = ephemeral
        .into_iter()
        .chain(windows_interop.then_some(WINDOWS_DYNAMIC_PORTS));
    for (low, high) in excluded {
        ranges = ranges
            .into_iter()
            .flat_map(|(start, end)| {
                if high < start || low > end {
                    return vec![(start, end)];
                }
                let mut parts = Vec::new();
                if start < low {
                    parts.push((start, low - 1));
                }
                if end > high {
                    parts.push((high + 1, end));
                }
                parts
            })
            .collect();
    }
    ranges
}

fn random_u32() -> Result<u32, InferenceFailure> {
    let mut bytes = [0_u8; 4];
    fs::File::open("/dev/urandom")
        .and_then(|mut file| io::Read::read_exact(&mut file, &mut bytes))
        .map_err(|error| io_failure("random_unavailable", error))?;
    Ok(u32::from_le_bytes(bytes))
}

/// Автовыбор порта: случайный свободный порт из допустимых диапазонов. Если
/// эфемерный диапазон Linux покрывает их целиком, нативный сервер получает
/// порт, назначенный ядром; для процесса Windows такой порт недоступен.
fn choose_port(plan: &LaunchPlan) -> Result<u16, InferenceFailure> {
    choose_port_with(plan, linux_ephemeral_range())
}

fn choose_port_with(
    plan: &LaunchPlan,
    ephemeral: Option<(u16, u16)>,
) -> Result<u16, InferenceFailure> {
    if plan.port != 0 {
        if !port_free(
            plan.host,
            plan.port,
            plan.connect_timeout,
            plan.windows_interop,
        ) {
            return Err(InferenceFailure::new(
                "port_in_use",
                format!("порт {} уже занят другим процессом", plan.port),
            ));
        }
        return Ok(plan.port);
    }
    let ranges = auto_port_ranges(ephemeral, plan.windows_interop);
    let total: u32 = ranges
        .iter()
        .map(|(low, high)| u32::from(*high) - u32::from(*low) + 1)
        .sum();
    if total > 0 {
        for _ in 0..AUTO_PORT_ATTEMPTS {
            let mut offset = random_u32()? % total;
            let port = ranges
                .iter()
                .find_map(|(low, high)| {
                    let size = u32::from(*high) - u32::from(*low) + 1;
                    if offset < size {
                        u16::try_from(u32::from(*low) + offset).ok()
                    } else {
                        offset -= size;
                        None
                    }
                })
                .expect("смещение меньше суммы размеров");
            if port_free(plan.host, port, plan.connect_timeout, plan.windows_interop) {
                return Ok(port);
            }
        }
    }
    if !plan.windows_interop
        && let Ok(listener) = TcpListener::bind(SocketAddr::new(plan.host, 0))
        && let Ok(address) = listener.local_addr()
    {
        return Ok(address.port());
    }
    Err(InferenceFailure::new(
        "port_unavailable",
        format!(
            "не удалось выбрать свободный loopback-порт: допустимые диапазоны {ranges:?} \
             (эфемерный диапазон Linux {ephemeral:?}{}); задайте port явно",
            if plan.windows_interop {
                ", динамический диапазон Windows 49152-65535"
            } else {
                ""
            }
        ),
    ))
}

/// Хвост вывода сервера: только для классификации отказа и диагностики.
#[derive(Debug, Default)]
struct OutputTail {
    bytes: Vec<u8>,
}

impl OutputTail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        if self.bytes.len() > OUTPUT_TAIL_BYTES {
            let excess = self.bytes.len() - OUTPUT_TAIL_BYTES;
            self.bytes.drain(..excess);
        }
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

fn spawn_reader<R>(mut reader: R, tail: Arc<Mutex<OutputTail>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0_u8; 4096];
        while let Ok(read) = reader.read(&mut chunk).await {
            if read == 0 {
                break;
            }
            tail.lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(&chunk[..read]);
        }
    });
}

/// Классификация преждевременного выхода по выводу сервера.
fn classify_exit(output: &str) -> &'static str {
    let lower = output.to_ascii_lowercase();
    if lower.contains("couldn't bind")
        || lower.contains("address already in use")
        || lower.contains("only one usage of each socket address")
    {
        "port_in_use"
    } else if lower.contains("mmproj") || lower.contains("multimodal") || lower.contains("clip") {
        "projector_load_failed"
    } else if lower.contains("failed to load model")
        || lower.contains("error loading model")
        || lower.contains("unable to load model")
    {
        "model_load_failed"
    } else if lower.contains("invalid argument") || lower.contains("unknown argument") {
        "invalid_server_arguments"
    } else {
        "server_exited_during_startup"
    }
}

/// Последние строки вывода без управляющих символов, для ответа CLI.
fn output_excerpt(output: &str) -> String {
    let lines: Vec<_> = output.lines().rev().take(6).collect();
    let text = lines.into_iter().rev().collect::<Vec<_>>().join(" | ");
    crate::diagnostics::safe_message(&text)
        .chars()
        .filter(|character| !character.is_control())
        .take(800)
        .collect()
}

/// Запущенный и подтверждённый собственный сервер.
#[derive(Debug)]
pub(crate) struct ManagedServer {
    child: Child,
    group: Pid,
    exited: Option<std::process::ExitStatus>,
    host: IpAddr,
    port: u16,
    alias: String,
    api_key: String,
    windows_interop: bool,
    shutdown_timeout: Duration,
    output: Arc<Mutex<OutputTail>>,
    lease: Option<PathBuf>,
    _lock: ManagerLock,
    pub ready: ServerReady,
}

impl ManagedServer {
    pub(crate) fn base_url(&self) -> Url {
        Url::parse(&format!("http://{}", SocketAddr::new(self.host, self.port)))
            .expect("адрес loopback корректен")
    }

    pub(crate) fn alias(&self) -> &str {
        &self.alias
    }

    pub(crate) fn api_key(&self) -> &str {
        &self.api_key
    }

    /// Запуск и доказательство готовности. При любом отказе собственный процесс
    /// уже остановлен.
    pub(crate) async fn start(
        plan: &LaunchPlan,
        client: &reqwest::Client,
        cancellation: &mut Cancellation,
    ) -> Result<Self, InferenceFailure> {
        // Автопорт может оказаться запрещённым на стороне Windows (исключённые
        // диапазоны Hyper-V): тогда запуск повторяется с другим портом.
        let attempts = if plan.port == 0 { 3 } else { 1 };
        let mut attempt = 0;
        loop {
            attempt += 1;
            match Self::start_once(plan, client, cancellation).await {
                Err(failure) if failure.code == "port_in_use" && attempt < attempts => {
                    tracing::warn!(
                        stage = "inference_port_retry",
                        attempt,
                        "порт отклонён сервером; повтор"
                    );
                }
                result => return result,
            }
        }
    }

    async fn start_once(
        plan: &LaunchPlan,
        client: &reqwest::Client,
        cancellation: &mut Cancellation,
    ) -> Result<Self, InferenceFailure> {
        let directory = state_directory().ok_or_else(|| {
            InferenceFailure::new(
                "manager_state_unavailable",
                "не найден домашний каталог для аренды процесса",
            )
        })?;
        let lock = ManagerLock::acquire(&directory)?;
        let orphans_stopped = stop_orphans(&directory).await;
        if let Some(failure) = orphan_failure(&orphans_stopped) {
            return Err(failure);
        }
        let metadata = fs::metadata(&plan.binary).map_err(|_| {
            InferenceFailure::new(
                "binary_not_found",
                format!("исполняемый файл {} не найден", plan.binary.display()),
            )
        })?;
        if !metadata.is_file() {
            return Err(InferenceFailure::new(
                "binary_not_found",
                format!("{} не является файлом", plan.binary.display()),
            ));
        }
        let port = choose_port(plan)?;
        let alias = format!("anki-{}", random_token()?);
        let api_key = random_token()?;
        let arguments = plan.arguments(port, &alias, &api_key);
        let mut command = Command::new(&plan.binary);
        command
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Терминальные сигналы не доходят до сервера напрямую: его
            // останавливает только этот владелец.
            .process_group(0)
            .kill_on_drop(true);
        if let Some(parent) = plan.binary.parent() {
            command.current_dir(parent);
        }
        // Переменные LLAMA_ARG_* молча меняли бы параметры, отличные от записанных.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("LLAMA_ARG_") {
                command.env_remove(name);
            }
        }
        let started = Instant::now();
        let mut child = command.spawn().map_err(|error| {
            InferenceFailure::new(
                "server_spawn_failed",
                crate::diagnostics::safe_message(&error.to_string()),
            )
        })?;
        let pid = child
            .id()
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or_else(|| InferenceFailure::new("server_spawn_failed", "PID недоступен"))?;
        let group = Pid::from_raw(pid).expect("PID положителен");
        let output = Arc::new(Mutex::new(OutputTail::default()));
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, output.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, output.clone());
        }
        let mut server = Self {
            child,
            group,
            exited: None,
            host: plan.host,
            port,
            alias: alias.clone(),
            api_key,
            windows_interop: plan.windows_interop,
            shutdown_timeout: plan.shutdown_timeout,
            output,
            lease: None,
            _lock: lock,
            ready: ServerReady {
                pid,
                port,
                alias,
                build_info: None,
                model_path: String::new(),
                vision: false,
                context_size: None,
                ready_ms: 0,
                windows: None,
                arguments: redact_arguments(&arguments),
                orphans_stopped,
            },
        };
        // Без аренды аварийно брошенный сервер не нашёл бы следующий запуск:
        // такой процесс не становится работающим managed-сервером.
        match write_lease(&directory, pid, &server.alias, port, plan.windows_interop) {
            Ok(path) => server.lease = Some(path),
            Err(failure) => {
                let report = server.stop().await;
                tracing::warn!(
                    stage = "inference_lease_failed",
                    code = failure.code,
                    stop_proven = report.proven(),
                    "аренда llama-server не записана; процесс остановлен"
                );
                return Err(failure.with_stop(report));
            }
        }
        tracing::info!(
            stage = "inference_server_spawned",
            pid,
            port,
            "llama-server запущен"
        );
        match server
            .await_ready(plan, client, cancellation, started)
            .await
        {
            Ok(()) => Ok(server),
            Err(failure) => {
                let report = server.stop().await;
                tracing::warn!(
                    stage = "inference_server_start_failed",
                    code = failure.code,
                    stop_proven = report.proven(),
                    "запуск llama-server не завершился готовностью"
                );
                Err(failure.with_stop(report))
            }
        }
    }

    async fn await_ready(
        &mut self,
        plan: &LaunchPlan,
        client: &reqwest::Client,
        cancellation: &mut Cancellation,
        started: Instant,
    ) -> Result<(), InferenceFailure> {
        let deadline = started + plan.startup_timeout;
        let base = self.base_url();
        loop {
            if cancellation.interrupted() {
                return Err(InferenceFailure::interrupted());
            }
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|error| io_failure("server_wait_failed", error))?
            {
                self.exited = Some(status);
                // Дать читателям дочитать вывод до классификации.
                tokio::time::sleep(Duration::from_millis(200)).await;
                let output = self.output_text();
                return Err(InferenceFailure::new(
                    classify_exit(&output),
                    format!(
                        "llama-server завершился при запуске ({status}): {}",
                        output_excerpt(&output)
                    ),
                ));
            }
            let health = client
                .get(base.join("/health").expect("путь корректен"))
                .timeout(plan.connect_timeout)
                .send()
                .await;
            if let Ok(response) = health
                && response.status().is_success()
            {
                break;
            }
            if Instant::now() >= deadline {
                return Err(InferenceFailure::new(
                    "startup_timeout",
                    format!(
                        "llama-server не стал готов за {} с: {}",
                        plan.startup_timeout.as_secs(),
                        output_excerpt(&self.output_text())
                    ),
                ));
            }
            tokio::select! {
                () = tokio::time::sleep(POLL_INTERVAL) => {}
                () = cancellation.wait() => return Err(InferenceFailure::interrupted()),
            }
        }
        let props = self.identity(client, plan.connect_timeout).await?;
        if props.model_path != plan.model_argument {
            return Err(InferenceFailure::new(
                "model_mismatch",
                format!(
                    "сервер загрузил {}, ожидался {}",
                    props.model_path, plan.model_argument
                ),
            ));
        }
        if plan.require_vision && !props.vision {
            return Err(InferenceFailure::new(
                "model_does_not_support_vision",
                "сервер не подтвердил vision для загруженной модели",
            ));
        }
        self.ready.build_info = props.build_info;
        self.ready.model_path = props.model_path;
        self.ready.vision = props.vision;
        self.ready.context_size = props.context_size;
        self.ready.ready_ms = started.elapsed().as_millis();
        if self.windows_interop {
            self.ready.windows = PowerShell.find(&self.alias).await.ok().flatten();
        }
        tracing::info!(
            stage = "inference_server_ready",
            pid = self.ready.pid,
            ready_ms = self.ready.ready_ms as u64,
            "llama-server готов и подтвердил модель"
        );
        Ok(())
    }

    fn output_text(&self) -> String {
        self.output
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .text()
    }

    /// Повторная сверка: жив ли собственный процесс и отвечает ли он нашим alias.
    pub(crate) async fn identity(
        &mut self,
        client: &reqwest::Client,
        timeout: Duration,
    ) -> Result<ServerProps, InferenceFailure> {
        if let Some(status) = self
            .child
            .try_wait()
            .map_err(|error| io_failure("server_wait_failed", error))?
        {
            self.exited = Some(status);
            return Err(InferenceFailure::new(
                "server_exited",
                format!(
                    "llama-server завершился ({status}): {}",
                    output_excerpt(&self.output_text())
                ),
            ));
        }
        let props = server_props(client, &self.base_url(), Some(&self.api_key), timeout).await?;
        if props.alias.as_deref() != Some(self.alias.as_str()) {
            return Err(InferenceFailure::new(
                "foreign_server",
                "на порту отвечает сервер с чужим alias: это не собственный процесс",
            ));
        }
        Ok(props)
    }

    /// Останавливает собственную группу и доказывает освобождение ресурсов.
    pub(crate) async fn stop(mut self) -> StopReport {
        let started = Instant::now();
        let mut report = StopReport {
            pid: self.ready.pid,
            ..StopReport::default()
        };
        if self.exited.is_none() {
            match self.child.try_wait() {
                Ok(Some(status)) => self.exited = Some(status),
                Ok(None) => {}
                Err(error) => report.problems.push(format!("wait: {error}")),
            }
        }
        report.method = if self.exited.is_some() {
            "exited".into()
        } else {
            let _ = kill_process_group(self.group, Signal::TERM);
            match tokio::time::timeout(self.shutdown_timeout, self.child.wait()).await {
                Ok(Ok(status)) => {
                    self.exited = Some(status);
                    "terminated".into()
                }
                _ => {
                    let _ = kill_process_group(self.group, Signal::KILL);
                    match tokio::time::timeout(Duration::from_secs(10), self.child.wait()).await {
                        Ok(Ok(status)) => self.exited = Some(status),
                        _ => report.problems.push("leader_not_reaped_after_kill".into()),
                    }
                    "killed".into()
                }
            }
        };
        report.leader_exit = self.exited.map(|status| status.to_string());
        let drain_deadline = Instant::now() + Duration::from_secs(5);
        while group_alive(self.group) && Instant::now() < drain_deadline {
            let _ = kill_process_group(self.group, Signal::KILL);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        report.group_empty = !group_alive(self.group);
        if !report.group_empty {
            report.problems.push("process_group_not_empty".into());
        }
        let port_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let address = SocketAddr::new(self.host, self.port);
            if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_err() {
                report.port_released = true;
                break;
            }
            if Instant::now() >= port_deadline {
                report.problems.push("port_still_answering".into());
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if self.windows_interop {
            match stop_windows_process(&PowerShell, &self.alias, Duration::from_secs(15)).await {
                Ok(()) => report.windows_process_gone = Some(true),
                Err((pid, error)) if error == "windows_process_alive" => {
                    report.windows_process_gone = Some(false);
                    report.problems.push(format!(
                        "windows_process_alive:{}",
                        pid.map_or_else(|| "?".to_owned(), |pid| pid.to_string())
                    ));
                }
                Err((pid, error)) => {
                    // Отказ проверки не выдаётся за доказанное исчезновение.
                    if let Some(pid) = pid {
                        report.windows_process_gone = Some(false);
                        report.problems.push(format!("windows_process_alive:{pid}"));
                    }
                    report
                        .problems
                        .push(format!("windows_probe_failed: {error}"));
                }
            }
        }
        if report.problems.is_empty()
            && let Some(lease) = self.lease.take()
        {
            report.lease_removed = fs::remove_file(lease).is_ok();
        }
        report.elapsed_ms = started.elapsed().as_millis();
        tracing::info!(
            stage = "inference_server_stopped",
            pid = report.pid,
            method = report.method.as_str(),
            proven = report.proven(),
            "llama-server остановлен"
        );
        report
    }
}

impl Drop for ManagedServer {
    fn drop(&mut self) {
        // Резервная граница для паники и отмены Future: явный `stop` уже собрал
        // лидера. Аренда остаётся, если остановка не доказана.
        if self.exited.is_none() {
            let _ = kill_process_group(self.group, Signal::KILL);
        }
    }
}

fn write_lease(
    directory: &Path,
    pid: i32,
    alias: &str,
    port: u16,
    windows_interop: bool,
) -> Result<PathBuf, InferenceFailure> {
    let start_ticks = process_start_ticks(pid).ok_or_else(|| {
        InferenceFailure::new(
            "lease_write_failed",
            format!("время старта процесса {pid} недоступно"),
        )
    })?;
    let failed = |error: io::Error| io_failure("lease_write_failed", error);
    let leases = directory.join("leases");
    fs::create_dir_all(&leases).map_err(failed)?;
    let path = leases.join(format!("{alias}.json"));
    let lease = Lease {
        pid,
        start_ticks,
        alias: alias.to_owned(),
        port,
        windows_interop,
        created_unix_seconds: unix_seconds(),
    };
    let bytes = serde_json::to_vec(&lease)
        .map_err(|error| InferenceFailure::new("lease_write_failed", error.to_string()))?;
    let temporary = leases.join(format!(".{alias}.tmp"));
    let written = fs::File::create(&temporary).and_then(|mut file| {
        file.write_all(&bytes)?;
        file.sync_all()
    });
    if let Err(error) = written.and_then(|()| fs::rename(&temporary, &path)) {
        let _ = fs::remove_file(&temporary);
        return Err(failed(error));
    }
    // Переименование доказано на диске только после синхронизации каталога.
    fs::File::open(&leases)
        .and_then(|directory| directory.sync_all())
        .map_err(failed)?;
    Ok(path)
}

/// Значение `--api-key` не попадает в свидетельства.
fn redact_arguments(arguments: &[String]) -> Vec<String> {
    let mut redacted = Vec::with_capacity(arguments.len());
    let mut hide = false;
    for argument in arguments {
        if hide {
            redacted.push("<redacted>".to_owned());
            hide = false;
        } else {
            hide = argument == "--api-key";
            redacted.push(argument.clone());
        }
    }
    redacted
}

/// Сведения `/props` и `/v1/models`, нужные для идентичности.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerProps {
    pub alias: Option<String>,
    pub model_path: String,
    pub build_info: Option<String>,
    pub vision: bool,
    pub context_size: Option<u64>,
}

pub(crate) async fn server_props(
    client: &reqwest::Client,
    base: &Url,
    api_key: Option<&str>,
    timeout: Duration,
) -> Result<ServerProps, InferenceFailure> {
    let props =
        crate::inference::get_json(client, base, "/props", api_key, timeout, "llama").await?;
    let models =
        crate::inference::get_json(client, base, "/v1/models", api_key, timeout, "llama").await?;
    let model_path = props["model_path"]
        .as_str()
        .filter(|path| !path.is_empty() && path.len() <= 4096)
        .ok_or_else(|| {
            InferenceFailure::new("identity_unavailable", "/props не сообщает model_path")
        })?
        .to_owned();
    let listed: Vec<&Value> = models["data"]
        .as_array()
        .map(|data| data.iter().collect())
        .unwrap_or_default();
    if listed.len() != 1 {
        return Err(InferenceFailure::new(
            "identity_unavailable",
            "/v1/models должен перечислять ровно одну загруженную модель",
        ));
    }
    let alias = listed[0]["id"].as_str().map(str::to_owned);
    if props["model_alias"]
        .as_str()
        .is_some_and(|value| Some(value) != alias.as_deref())
    {
        return Err(InferenceFailure::new(
            "identity_unavailable",
            "alias в /props и /v1/models расходятся",
        ));
    }
    Ok(ServerProps {
        alias,
        model_path,
        build_info: props["build_info"].as_str().map(str::to_owned),
        vision: props["modalities"]["vision"].as_bool().unwrap_or(false),
        context_size: listed[0]["meta"]["n_ctx"].as_u64(),
    })
}

/// Alias, созданный этим модулем: `anki-` и 32 шестнадцатеричные цифры. Только
/// такое значение подставляется в запрос PowerShell.
fn valid_alias(alias: &str) -> bool {
    alias.strip_prefix("anki-").is_some_and(|token| {
        token.len() == 32
            && token
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    })
}

async fn powershell(script: &str) -> Result<String, String> {
    if !Path::new(POWERSHELL).is_file() {
        return Err("powershell_unavailable".into());
    }
    let output = tokio::time::timeout(
        WINDOWS_PROBE_TIMEOUT,
        Command::new(POWERSHELL)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| "windows_probe_timeout".to_owned())?
    .map_err(|error| crate::diagnostics::safe_message(&error.to_string()))?;
    if !output.status.success() {
        return Err(format!("windows_probe_exit:{}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Процесс Windows с данным alias в командной строке: `Ok(None)` — такого нет.
/// Поиск идёт по alias, а не по имени исполняемого файла; сам PowerShell,
/// в чьей командной строке тоже есть alias, исключается по `$PID`.
pub(crate) async fn windows_process(alias: &str) -> Result<Option<WindowsProcess>, String> {
    if !valid_alias(alias) {
        return Err("invalid_alias".into());
    }
    let text = powershell(&format!(
        "Get-CimInstance Win32_Process -Filter \"CommandLine LIKE '%{alias}%' AND ProcessId <> $PID\" | Select-Object ProcessId,CommandLine,WorkingSetSize | ConvertTo-Json -Compress"
    ))
    .await?;
    parse_windows_processes(&text, alias)
}

/// Разбор ответа PowerShell: процесс, в командной строке которого alias —
/// отдельный аргумент.
fn parse_windows_processes(text: &str, alias: &str) -> Result<Option<WindowsProcess>, String> {
    if text.is_empty() {
        return Ok(None);
    }
    let value: Value =
        serde_json::from_str(text).map_err(|_| "windows_probe_invalid_json".to_owned())?;
    let entries = match value {
        Value::Array(entries) => entries,
        single => vec![single],
    };
    Ok(entries.iter().find_map(|entry| {
        let command_line = entry["CommandLine"].as_str()?;
        command_line
            .split_whitespace()
            .any(|part| part.trim_matches('"') == alias)
            .then(|| WindowsProcess {
                pid: entry["ProcessId"]
                    .as_u64()
                    .and_then(|pid| u32::try_from(pid).ok())
                    .unwrap_or(0),
                working_set_bytes: entry["WorkingSetSize"].as_u64(),
            })
    }))
}

/// Завершает процесс Windows `pid`, только если его командная строка в момент
/// завершения содержит alias отдельным аргументом: чужой процесс с тем же PID
/// не трогается.
async fn terminate_windows_process(pid: u32, alias: &str) -> Result<(), String> {
    if !valid_alias(alias) || pid == 0 {
        return Err("invalid_alias".into());
    }
    let answer = powershell(&format!(
        "$p = Get-CimInstance Win32_Process -Filter \"ProcessId = {pid}\"; \
         if ($null -eq $p) {{ 'absent' }} \
         elseif (@(($p.CommandLine -split '\\s+') | ForEach-Object {{ $_.Trim('\"') }}) -contains '{alias}') \
         {{ Stop-Process -Id {pid} -Force -ErrorAction Stop; 'stopped' }} \
         else {{ 'foreign' }}"
    ))
    .await?;
    match answer.as_str() {
        "stopped" | "absent" => Ok(()),
        "foreign" => Err("windows_process_not_owned".into()),
        _ => Err("windows_terminate_unexpected_answer".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_classification_names_typed_reasons() {
        assert_eq!(
            classify_exit("main: couldn't bind HTTP server socket"),
            "port_in_use"
        );
        assert_eq!(
            classify_exit("clip_init: failed to load mmproj model"),
            "projector_load_failed"
        );
        assert_eq!(
            classify_exit("llama_model_load: error loading model"),
            "model_load_failed"
        );
        assert_eq!(
            classify_exit("error: invalid argument: --bogus"),
            "invalid_server_arguments"
        );
        assert_eq!(classify_exit("segfault"), "server_exited_during_startup");
    }

    #[test]
    fn api_key_never_reaches_evidence() {
        let arguments = vec![
            "--alias".into(),
            "a".into(),
            "--api-key".into(),
            "secret".into(),
        ];
        let redacted = redact_arguments(&arguments);
        assert!(!redacted.iter().any(|value| value == "secret"));
        assert_eq!(redacted[3], "<redacted>");
    }

    /// Подменённый список процессов Windows: ответы по очереди, затем последний.
    struct FakeWindows {
        answers: Mutex<Vec<Result<Option<WindowsProcess>, String>>>,
        terminate: Result<(), String>,
        terminated: Mutex<Vec<(u32, String)>>,
    }

    impl FakeWindows {
        fn new(answers: Vec<Result<Option<WindowsProcess>, String>>) -> Self {
            Self {
                answers: Mutex::new(answers),
                terminate: Ok(()),
                terminated: Mutex::new(Vec::new()),
            }
        }
    }

    impl WindowsProcesses for FakeWindows {
        async fn find(&self, _alias: &str) -> Result<Option<WindowsProcess>, String> {
            let mut answers = self.answers.lock().unwrap();
            if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            }
        }

        async fn terminate(&self, pid: u32, alias: &str) -> Result<(), String> {
            self.terminated
                .lock()
                .unwrap()
                .push((pid, alias.to_owned()));
            self.terminate.clone()
        }
    }

    fn alive(pid: u32) -> Result<Option<WindowsProcess>, String> {
        Ok(Some(WindowsProcess {
            pid,
            working_set_bytes: None,
        }))
    }

    fn write_test_lease(
        directory: &Path,
        name: &str,
        alias: &str,
        windows_interop: bool,
    ) -> PathBuf {
        let leases = directory.join("leases");
        fs::create_dir_all(&leases).unwrap();
        // PID текущего процесса жив, но время старта и alias не совпадают:
        // процесс Linux не считается собственным.
        let lease = Lease {
            pid: std::process::id() as i32,
            start_ticks: 1,
            alias: alias.into(),
            port: 1,
            windows_interop,
            created_unix_seconds: 0,
        };
        let path = leases.join(name);
        fs::write(&path, serde_json::to_vec(&lease).unwrap()).unwrap();
        path
    }

    #[tokio::test]
    async fn unreadable_lease_is_kept_and_blocks_the_start_even_with_stop() {
        let directory = crate::temp_workspace::TempWorkspace::create("llama-lease-bad").unwrap();
        let leases = directory.path().join("leases");
        fs::create_dir_all(&leases).unwrap();
        let path = leases.join("bad.json");
        fs::write(&path, b"{ not a lease").unwrap();

        let windows = FakeWindows::new(vec![]);
        for stop in [false, true] {
            let orphans = orphans_with(directory.path(), &windows, Duration::ZERO, stop).await;
            assert_eq!(orphans.len(), 1);
            assert_eq!(orphans[0].state, "invalid");
            assert!(!orphans[0].stopped);
            assert!(path.exists(), "нечитаемая аренда не удаляется");
            assert!(orphan_failure(&orphans).is_some());
        }
    }

    #[tokio::test]
    async fn foreign_process_with_reused_pid_is_not_treated_as_orphan() {
        let directory = crate::temp_workspace::TempWorkspace::create("llama-lease").unwrap();
        let path = write_test_lease(directory.path(), "x.json", "anki-foreign", false);
        assert!(stop_orphans(directory.path()).await.is_empty());
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn windows_lease_survives_until_the_windows_process_is_proven_gone() {
        let directory = crate::temp_workspace::TempWorkspace::create("llama-lease-win").unwrap();
        let path = write_test_lease(directory.path(), "w.json", "anki-windows", true);

        // Осмотр без --stop: ничего не завершается и не удаляется.
        let windows = FakeWindows::new(vec![alive(7)]);
        let inspected = orphans_with(directory.path(), &windows, Duration::ZERO, false).await;
        assert_eq!(inspected[0].state, "windows_process_alive");
        assert_eq!(inspected[0].windows_pid, Some(7));
        assert!(windows.terminated.lock().unwrap().is_empty());
        assert!(path.exists());

        // Живой процесс с нашим alias завершается по PID; аренда снимается
        // только после того, как повторная проверка его не нашла.
        let windows = FakeWindows::new(vec![alive(7), Ok(None)]);
        let orphans = orphans_with(directory.path(), &windows, Duration::ZERO, true).await;
        assert_eq!(
            *windows.terminated.lock().unwrap(),
            vec![(7, "anki-windows".to_owned())]
        );
        assert_eq!(orphans.len(), 0, "процесс Linux не был собственным");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn unproven_windows_stop_keeps_the_lease_and_names_the_recovery() {
        let directory = crate::temp_workspace::TempWorkspace::create("llama-lease-keep").unwrap();
        let path = write_test_lease(directory.path(), "w.json", "anki-windows", true);

        // Завершить не удалось: процесс жив, PID известен, аренда сохранена.
        let mut windows = FakeWindows::new(vec![alive(7)]);
        windows.terminate = Err("windows_process_not_owned".into());
        let orphans = orphans_with(directory.path(), &windows, Duration::ZERO, true).await;
        assert_eq!(orphans.len(), 1);
        assert!(!orphans[0].stopped);
        assert_eq!(orphans[0].state, "windows_process_alive");
        assert_eq!(orphans[0].windows_pid, Some(7));
        assert!(path.exists(), "живой процесс Windows сохраняет аренду");
        let failure = orphan_failure(&orphans).unwrap();
        assert_eq!(failure.code, "orphan_server_alive");
        assert!(failure.message.contains("PID Windows 7"));
        assert!(failure.message.contains(&path.display().to_string()));
        assert!(failure.message.contains("inference orphans --stop"));
        assert_eq!(failure.orphans, orphans);

        // Проверка недоступна: это не «процесс жив» и не «процесс исчез».
        let windows = FakeWindows::new(vec![Err("powershell_unavailable".into())]);
        let orphans = orphans_with(directory.path(), &windows, Duration::ZERO, true).await;
        assert_eq!(orphans[0].state, "windows_probe_failed");
        assert_eq!(orphans[0].windows_pid, None);
        assert_eq!(
            orphans[0].probe_error.as_deref(),
            Some("powershell_unavailable")
        );
        assert!(windows.terminated.lock().unwrap().is_empty());
        assert!(path.exists(), "отказ проверки не доказывает исчезновение");
        let failure = orphan_failure(&orphans).unwrap();
        assert_eq!(failure.code, "orphan_server_unverified");
        assert!(failure.message.contains("powershell_unavailable"));

        // Процесс исчез: устаревшая аренда снимается без сообщения.
        let windows = FakeWindows::new(vec![Ok(None)]);
        let gone = orphans_with(directory.path(), &windows, Duration::ZERO, true).await;
        assert!(gone.is_empty());
        assert!(!path.exists());
        assert!(orphan_failure(&gone).is_none());
    }

    #[test]
    fn windows_process_is_found_by_alias_not_by_executable_name() {
        let alias = "anki-0123456789abcdef0123456789abcdef";
        assert!(valid_alias(alias));
        for invalid in [
            "anki-windows",
            "anki-0123",
            "x' OR 1=1",
            &format!("{alias}0"),
        ] {
            assert!(!valid_alias(invalid), "{invalid}");
        }
        // Переименованный бинарник с нашим alias — собственный процесс.
        let text = format!(
            r#"[{{"ProcessId":41,"CommandLine":"\"C:\\Tools\\my-llama.exe\" --model C:\\m.gguf --alias {alias}","WorkingSetSize":5}},
               {{"ProcessId":42,"CommandLine":"powershell.exe -Command Get-CimInstance -Filter \"CommandLine LIKE '%{alias}%'\""}}]"#
        );
        assert_eq!(
            parse_windows_processes(&text, alias).unwrap(),
            Some(WindowsProcess {
                pid: 41,
                working_set_bytes: Some(5)
            })
        );
        // Alias лишь как часть другого аргумента — не наш процесс.
        let other =
            format!(r#"{{"ProcessId":43,"CommandLine":"llama-server.exe --alias {alias}-other"}}"#);
        assert_eq!(parse_windows_processes(&other, alias).unwrap(), None);
        assert_eq!(parse_windows_processes("", alias).unwrap(), None);
        assert!(parse_windows_processes("not json", alias).is_err());
    }

    fn plan(port: u16, windows_interop: bool) -> LaunchPlan {
        let temp = crate::temp_workspace::TempWorkspace::create("llama-plan").unwrap();
        let config =
            crate::inference_config::LoadedConfig::load(&crate::inference_config::ConfigSources {
                cli_values: vec![
                    (
                        "backends.llama.binary".into(),
                        toml::Value::String("/bin/true".into()),
                    ),
                    (
                        "models.kanji_vision.llama_server.model".into(),
                        toml::Value::String(temp.path().join("m.gguf").display().to_string()),
                    ),
                    (
                        "models.kanji_vision.llama_server.vision".into(),
                        toml::Value::String("integrated".into()),
                    ),
                ],
                skip_local_discovery: true,
                ..crate::inference_config::ConfigSources::default()
            })
            .unwrap()
            .resolve(crate::inference_config::TaskId::KanjiOcr)
            .unwrap();
        let (
            crate::inference_config::ResolvedBackend::LlamaServer(backend),
            crate::inference_config::ResolvedModel::LlamaServer(model),
        ) = (&config.backend, &config.model)
        else {
            unreachable!()
        };
        let mut plan = LaunchPlan::new(backend, model).unwrap();
        plan.port = port;
        plan.windows_interop = windows_interop;
        plan
    }

    #[test]
    fn automatic_port_respects_the_actual_ephemeral_range() {
        // Эфемерный диапазон, перекрывающий прежние 20000..32767 целиком.
        let wide = Some((20_000, 60_999));
        assert_eq!(
            auto_port_ranges(wide, false),
            vec![(10_000, 19_999), (61_000, 65_535)]
        );
        assert_eq!(auto_port_ranges(wide, true), vec![(10_000, 19_999)]);
        assert_eq!(auto_port_ranges(None, false), vec![(10_000, 65_535)]);
        for windows_interop in [false, true] {
            for _ in 0..20 {
                let port = choose_port_with(&plan(0, windows_interop), wide).unwrap();
                assert!(!(20_000..=60_999).contains(&port), "{port}");
                if windows_interop {
                    assert!(port < 49_152, "{port}");
                }
            }
        }
        // Эфемерный диапазон покрывает всё: нативный сервер получает порт от
        // ядра, процессу Windows подходящего порта нет.
        let all = Some((1_024, 65_535));
        assert!(auto_port_ranges(all, false).is_empty());
        assert!(choose_port_with(&plan(0, false), all).unwrap() > 0);
        assert_eq!(
            choose_port_with(&plan(0, true), all).unwrap_err().code,
            "port_unavailable"
        );
        // Явный занятый порт отклоняется, свободный принимается как есть.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = listener.local_addr().unwrap().port();
        assert_eq!(
            choose_port_with(&plan(busy, false), wide).unwrap_err().code,
            "port_in_use"
        );
        drop(listener);
        assert_eq!(choose_port_with(&plan(busy, false), wide).unwrap(), busy);
    }
}
