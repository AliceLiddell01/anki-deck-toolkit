//! Повторно используемая среда Chromium и техническая телеметрия CDP.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::future::Future;
use std::io::{self, Read};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chromiumoxide::{
    Browser, BrowserConfig, Page,
    cdp::browser_protocol::{
        emulation::{MediaFeature, SetDeviceMetricsOverrideParams, SetEmulatedMediaParams},
        network::{
            EnableParams as NetworkEnableParams, EventLoadingFailed, EventLoadingFinished,
            EventRequestWillBeSent, EventResponseReceived, ResourceType,
        },
        page::{EnableParams, GetFrameTreeParams},
    },
    cdp::js_protocol::runtime::{EnableParams as RuntimeEnableParams, EventExceptionThrown},
};
use futures::{FutureExt, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::time::timeout;
use url::Url;

use crate::temp_workspace::TempWorkspace;

const MAX_TRACKED_REQUESTS: usize = 256;
const MAX_NETWORK_OUTCOMES: usize = 256;
/// События ответа из кэша разделены долями миллисекунды, поэтому такой объём памяти
/// завершённых запросов с большим запасом покрывает задержку между слушателями.
const MAX_COMPLETED_REQUESTS: usize = 1024;
const MAX_NETWORK_DIAGNOSTIC_ITEMS: usize = 3;
const MAX_NETWORK_DIAGNOSTIC_PATH_CHARS: usize = 160;
/// Максимум записей каждой коллекции в диагностическом JSON.
pub const MAX_RUNTIME_DIAGNOSTIC_ITEMS: usize = 16;
const MAX_RUNTIME_REQUEST_ID_BYTES: usize = 1024;
const MAX_RUNTIME_URL_BYTES: usize = 16 * 1024;
const MAX_RUNTIME_REASON_BYTES: usize = 1024;
static NEXT_BROWSER_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Способ выбора исполняемого файла браузера без сохранения абсолютного пути.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserExecutableSource {
    ChromeBinEnvironment,
    ChromiumBinEnvironment,
    PathLookup,
    PlaywrightCache,
    ChromiumoxideDefault,
}

/// Версия среды браузера; абсолютные пути, профиль и cookie-файлы не записываются.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserRuntimeProvenance {
    pub product: String,
    pub protocol_version: String,
    pub revision: String,
    pub user_agent: String,
    pub js_version: String,
    pub executable_source: BrowserExecutableSource,
}

/// Размер области просмотра и масштаб устройства для CDP `Emulation.setDeviceMetricsOverride`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DeviceMetrics {
    pub width: i64,
    pub height: i64,
    pub device_scale_factor: f64,
    pub mobile: bool,
}

impl DeviceMetrics {
    pub fn new(width: i64, height: i64, device_scale_factor: f64) -> Result<Self, String> {
        let metrics = Self {
            width,
            height,
            device_scale_factor,
            mobile: false,
        };
        metrics.validate()?;
        Ok(metrics)
    }

    pub fn validate(self) -> Result<(), String> {
        if self.width <= 0 || self.height <= 0 {
            return Err(
                "browser_device_metrics_invalid: размеры области просмотра должны быть положительными".into(),
            );
        }
        if !self.device_scale_factor.is_finite() || self.device_scale_factor <= 0.0 {
            return Err(
                "browser_device_metrics_invalid: масштаб устройства должен быть конечным и положительным".into(),
            );
        }
        Ok(())
    }

    fn cdp_params(self) -> SetDeviceMetricsOverrideParams {
        SetDeviceMetricsOverrideParams::new(
            self.width,
            self.height,
            self.device_scale_factor,
            self.mobile,
        )
    }
}

/// Настройки изолированного сеанса Chromium. Переходы по сайтам и правила работы с DOM
/// остаются ответственностью вызывающего провайдера.
#[derive(Debug, Clone)]
pub struct BrowserRuntimeConfig {
    pub device_metrics: Option<DeviceMetrics>,
    pub prefers_color_scheme: Option<String>,
    pub launch_timeout: Duration,
    pub request_timeout: Duration,
}

impl Default for BrowserRuntimeConfig {
    fn default() -> Self {
        Self {
            device_metrics: None,
            prefers_color_scheme: None,
            launch_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(30),
        }
    }
}

impl BrowserRuntimeConfig {
    fn validate(&self) -> Result<(), String> {
        if let Some(metrics) = self.device_metrics {
            metrics.validate()?;
        }
        if self.launch_timeout.is_zero() || self.request_timeout.is_zero() {
            return Err(
                "browser_runtime_timeout_invalid: значения таймаутов должны быть положительными"
                    .into(),
            );
        }
        if self
            .prefers_color_scheme
            .as_deref()
            .is_some_and(|scheme| scheme.trim().is_empty())
        {
            return Err(
                "browser_media_preference_invalid: цветовая схема не должна быть пустой".into(),
            );
        }
        Ok(())
    }
}

/// Найденный исполняемый файл Chromium и безопасная для записи категория его происхождения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserExecutableSelection {
    path: PathBuf,
    pub source: BrowserExecutableSource,
}

/// Только созданная этим объектом подпапка может быть удалена при закрытии.
struct BrowserProfile {
    path: PathBuf,
    temp_path: PathBuf,
    workspace_path: PathBuf,
    workspace: Option<TempWorkspace>,
    closed: bool,
}

/// Ошибка запуска сохраняет доказанность очистки до владельца восстановления.
#[derive(Debug, thiserror::Error)]
pub enum BrowserLaunchError {
    #[error("{0}")]
    Setup(String),
    #[error("{original}; {cleanup}")]
    CleanupFailed { original: String, cleanup: String },
    #[error("{0}")]
    Panic(String),
}

impl BrowserLaunchError {
    pub(crate) fn workspace_creation(error: io::Error, code: &str) -> Self {
        let original = format!("{code}: {error}");
        if let Some(cleanup_failure) = error.get_ref().and_then(|error| {
            error.downcast_ref::<crate::temp_workspace::WorkspaceCreationCleanupFailure>()
        }) {
            Self::CleanupFailed {
                original: format!("{code}: {}", cleanup_failure.original),
                cleanup: format!("temp_workspace_cleanup_failed: {}", cleanup_failure.cleanup),
            }
        } else {
            Self::Setup(original)
        }
    }

    fn with_cleanup(self, cleanup: Result<(), String>) -> Self {
        match cleanup {
            Ok(()) => self,
            Err(cleanup) => Self::CleanupFailed {
                original: self.to_string(),
                cleanup: crate::diagnostics::safe_message(&cleanup),
            },
        }
    }
}

impl From<io::Error> for BrowserLaunchError {
    fn from(error: io::Error) -> Self {
        Self::workspace_creation(error, "browser_profile_create_failed")
    }
}

fn profile_creation_failure(error: io::Error, paths: &[&Path]) -> BrowserLaunchError {
    let mut cleanup = Ok(());
    for path in paths {
        let removed = remove_browser_directory(path)
            .and_then(|()| ensure_directory_removed(path))
            .map_err(|error| format!("browser_profile_cleanup_failed: {error}"));
        cleanup = combine_browser_close_results(cleanup, removed);
    }
    BrowserLaunchError::from(error).with_cleanup(cleanup)
}

impl BrowserProfile {
    fn standalone() -> Result<Self, BrowserLaunchError> {
        let workspace = TempWorkspace::create("browser-session")?;
        let parent = workspace.path().to_path_buf();
        Self::create(&parent, Some(workspace))
    }

    fn in_workspace(workspace: &Path) -> Result<Self, BrowserLaunchError> {
        Self::create(workspace, None)
    }

    fn create(parent: &Path, workspace: Option<TempWorkspace>) -> Result<Self, BrowserLaunchError> {
        let result = Self::create_directories(parent);
        match result {
            Ok(mut profile) => {
                profile.workspace = workspace;
                Ok(profile)
            }
            Err(error) => Err(error.with_cleanup(
                workspace
                    .map_or(Ok(()), TempWorkspace::close)
                    .map_err(|error| format!("browser_workspace_cleanup_failed: {error}")),
            )),
        }
    }

    fn create_directories(parent: &Path) -> Result<Self, BrowserLaunchError> {
        if !fs::symlink_metadata(parent)?.is_dir() {
            return Err(
                io::Error::other("путь для профиля браузера должен указывать на каталог").into(),
            );
        }
        let parent = fs::canonicalize(parent)?;
        let mut random = [0_u8; 18];
        for _ in 0..32 {
            File::open("/dev/urandom")?.read_exact(&mut random)?;
            let path = parent.join(format!(
                "browser-profile-{}",
                crate::hashing::encode_lower_hex(&random[..16])
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    if let Err(error) = set_private_directory(&path) {
                        return Err(profile_creation_failure(error, &[&path]));
                    }
                    let temp_path =
                        parent.join(format!("t{:x}{:02x}", random[16] & 0x0f, random[17]));
                    match fs::create_dir(&temp_path) {
                        Ok(()) => {
                            if let Err(error) = set_private_directory(&temp_path) {
                                return Err(profile_creation_failure(error, &[&temp_path, &path]));
                            }
                            return Ok(Self {
                                path,
                                temp_path,
                                workspace_path: parent,
                                workspace: None,
                                closed: false,
                            });
                        }
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                            if let Err(cleanup) = fs::remove_dir(&path) {
                                return Err(BrowserLaunchError::from(error).with_cleanup(Err(
                                    format!("browser_profile_cleanup_failed: {cleanup}"),
                                )));
                            }
                            continue;
                        }
                        Err(error) => {
                            return Err(profile_creation_failure(error, &[&path]));
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::other("не удалось создать уникальный профиль браузера").into())
    }

    fn close(mut self) -> io::Result<()> {
        self.finish_close()
    }

    async fn close_after_browser_stop(mut self) -> io::Result<()> {
        wait_for_workspace_processes(&self.workspace_path).await?;
        self.finish_close()
    }

    fn finish_close(&mut self) -> io::Result<()> {
        self.cleanup()?;
        self.closed = true;
        self.workspace.take().map_or(Ok(()), TempWorkspace::close)
    }

    fn cleanup(&self) -> io::Result<()> {
        TempWorkspace::ensure_no_live_process_references(&self.workspace_path)?;
        let profile = remove_browser_directory(&self.path);
        let temp = remove_browser_directory(&self.temp_path);
        profile.and(temp)?;
        ensure_directory_removed(&self.path)?;
        ensure_directory_removed(&self.temp_path)
    }
}

fn ensure_directory_removed(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(io::Error::other("принадлежащий браузеру каталог не удалён")),
    }
}

fn remove_browser_directory(path: &Path) -> io::Result<()> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

async fn wait_for_workspace_processes(workspace: &Path) -> io::Result<()> {
    wait_for_workspace_processes_with(
        || TempWorkspace::has_live_process_references(workspace),
        Duration::from_secs(2),
        Duration::from_millis(50),
    )
    .await
}

async fn wait_for_workspace_processes_with(
    mut has_references: impl FnMut() -> io::Result<bool>,
    wait_limit: Duration,
    poll_interval: Duration,
) -> io::Result<()> {
    let deadline = Instant::now() + wait_limit;
    loop {
        let result = has_references();
        if matches!(result, Ok(false)) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return match result {
                Ok(true) => Err(io::Error::other(
                    "процесс браузера всё ещё использует временное дерево",
                )),
                Err(error) => Err(error),
                Ok(false) => Ok(()),
            };
        }
        tokio::time::sleep(poll_interval.min(deadline.saturating_duration_since(Instant::now())))
            .await;
    }
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn build_browser_config(
    profile: &BrowserProfile,
    config: &BrowserRuntimeConfig,
    executable: Option<BrowserExecutableSelection>,
) -> Result<BrowserConfig, String> {
    let configured_temp_root =
        std::env::var_os(crate::temp_workspace::CONFIGURED_TEMP_ROOT_ENV).map(PathBuf::from);
    let current_directory = std::env::current_dir().ok();
    let temp_directory = browser_tmpdir_value(
        &profile.temp_path,
        configured_temp_root.as_deref(),
        current_directory.as_deref(),
    );
    let temp_directory = temp_directory
        .to_str()
        .ok_or("browser_tmpdir_invalid: путь TMPDIR должен быть UTF-8")?;
    let mut builder = BrowserConfig::builder()
        .user_data_dir(&profile.path)
        .env("TMPDIR", temp_directory)
        .incognito()
        .respect_https_errors()
        .launch_timeout(config.launch_timeout)
        .request_timeout(config.request_timeout);
    if let Some(executable) = executable {
        builder = builder.chrome_executable(executable.path);
    }
    builder
        .build()
        .map_err(|error| format!("настройка браузера: {error}"))
}

fn browser_tmpdir_value(
    temp_path: &Path,
    configured_root: Option<&Path>,
    current_directory: Option<&Path>,
) -> PathBuf {
    if let (Some(configured_root), Some(current_directory)) = (configured_root, current_directory)
        && let (Ok(root), Ok(current)) = (
            fs::canonicalize(configured_root),
            fs::canonicalize(current_directory),
        )
        && root == current
        && let Ok(temp_path) = fs::canonicalize(temp_path)
        && let Ok(relative) = temp_path.strip_prefix(&root)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_path_buf();
    }
    temp_path.to_path_buf()
}

fn profile_setup_failure(profile: BrowserProfile, message: String) -> BrowserLaunchError {
    match profile.close() {
        Ok(()) => BrowserLaunchError::Setup(message),
        Err(error) => {
            tracing::error!(stage = "temp_cleanup", code = "browser_profile_cleanup_failed", path_category = "browser_workspace", message = %crate::diagnostics::safe_message(&error.to_string()), "Ошибка очистки после отказа запуска браузера");
            BrowserLaunchError::Setup(message)
                .with_cleanup(Err(format!("browser_profile_cleanup_failed: {error}")))
        }
    }
}

impl Drop for BrowserProfile {
    fn drop(&mut self) {
        if !self.closed
            && let Err(error) = self.cleanup()
        {
            tracing::error!(
                stage = "temp_cleanup",
                code = "browser_profile_cleanup_failed",
                path_category = "browser_workspace",
                message = %crate::diagnostics::safe_message(&error.to_string()),
                "Не удалось безопасно удалить профиль и временный каталог браузера"
            );
        }
    }
}

/// Ресурсы создаются до асинхронной настройки: её отмена также закрывает браузер.
struct BrowserResources {
    diagnostic_session_id: u64,
    browser: Option<Browser>,
    handler_task: Option<tokio::task::JoinHandle<()>>,
    profile: Option<BrowserProfile>,
}

/// Паника остаётся отдельным исходом, пока не закрыты ресурсы настройки.
enum BrowserSetupFailure {
    Error(String),
    Panic(Box<dyn std::any::Any + Send>),
}

impl BrowserSetupFailure {
    fn launch_error(self, panic_code: &str) -> BrowserLaunchError {
        match self {
            Self::Error(message) => BrowserLaunchError::Setup(message),
            failure @ Self::Panic(_) => {
                BrowserLaunchError::Panic(failure.message_with_panic_code(panic_code))
            }
        }
    }

    fn message_with_panic_code(self, panic_code: &str) -> String {
        match self {
            Self::Error(message) => message,
            Self::Panic(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| payload.downcast_ref::<&str>().copied())
                    .unwrap_or("паника с нетекстовым содержимым");
                format!(
                    "{panic_code}: {}",
                    crate::diagnostics::safe_message(message)
                )
            }
        }
    }
}

async fn catch_browser_setup<T>(
    setup: impl Future<Output = Result<T, String>>,
) -> Result<T, BrowserSetupFailure> {
    match AssertUnwindSafe(setup).catch_unwind().await {
        Ok(result) => result.map_err(BrowserSetupFailure::Error),
        Err(payload) => Err(BrowserSetupFailure::Panic(payload)),
    }
}

async fn profile_launch_failure(
    profile: BrowserProfile,
    failure: BrowserSetupFailure,
) -> BrowserLaunchError {
    let original = failure.launch_error("browser_launch_panicked");
    match profile.close_after_browser_stop().await {
        Ok(()) => original,
        Err(error) => {
            let cleanup = crate::diagnostics::safe_message(&error.to_string());
            tracing::error!(stage = "temp_cleanup", code = "browser_profile_cleanup_failed", path_category = "browser_workspace", message = %cleanup, "Ошибка очистки после отказа или паники при запуске браузера");
            original.with_cleanup(Err(format!("browser_profile_cleanup_failed: {cleanup}")))
        }
    }
}

/// Закрытие транспорта CDP и завершение принадлежащего сессии процесса — независимые наблюдения.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserTransportStop {
    Closed,
    Detached,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserStopObservation {
    transport: BrowserTransportStop,
    process_stopped: bool,
    process_failure: Option<String>,
}

fn transport_stop_result(
    result: Result<(), chromiumoxide::error::CdpError>,
) -> BrowserTransportStop {
    match result {
        Ok(()) => BrowserTransportStop::Closed,
        // Типы означают отсутствие живого обработчика или канала ответа, а не ошибку
        // самой команды CDP. Нефатальность определяется только после очистки ресурсов.
        Err(
            chromiumoxide::error::CdpError::ChannelSendError(_)
            | chromiumoxide::error::CdpError::NoResponse,
        ) => BrowserTransportStop::Detached,
        Err(error) => BrowserTransportStop::Failed(format!(
            "browser_close_failed: {}",
            crate::diagnostics::safe_message(&error.to_string())
        )),
    }
}

async fn stop_browser(browser: &mut Browser, browser_session: u64) -> BrowserStopObservation {
    let started = Instant::now();
    let transport = match timeout(Duration::from_secs(2), browser.close()).await {
        Ok(result) => transport_stop_result(result.map(|_| ())),
        Err(_) => BrowserTransportStop::Failed("browser_close_timeout".into()),
    };
    let transport_outcome = match &transport {
        BrowserTransportStop::Closed => "closed",
        BrowserTransportStop::Detached => "detached",
        BrowserTransportStop::Failed(_) => "failed",
    };
    let transport_code = match &transport {
        BrowserTransportStop::Closed => "browser_transport_closed",
        BrowserTransportStop::Detached => "browser_transport_detached",
        BrowserTransportStop::Failed(error) if error == "browser_close_timeout" => {
            "browser_close_timeout"
        }
        BrowserTransportStop::Failed(_) => "browser_close_failed",
    };
    tracing::info!(
        event = "browser_lifecycle",
        stage = "browser_transport_stop",
        code = transport_code,
        browser_session,
        outcome = transport_outcome,
        stage_duration_ms = duration_millis(started.elapsed()),
        "Завершено наблюдение остановки транспорта CDP"
    );
    let started = Instant::now();
    let wait = timeout(Duration::from_secs(2), browser.wait()).await;
    let (process_stopped, process_failure, process_outcome) = if matches!(wait, Ok(Ok(_))) {
        (true, None, "exited")
    } else {
        match timeout(Duration::from_secs(2), browser.kill()).await {
            Ok(Some(Ok(()))) => {
                let failure = if transport == BrowserTransportStop::Detached {
                    None
                } else {
                    Some("browser_close_failed: потребовалось принудительное завершение".into())
                };
                (true, failure, "killed")
            }
            Ok(Some(Err(error))) => (
                false,
                Some(format!(
                    "browser_kill_failed: {}",
                    crate::diagnostics::safe_message(&error.to_string())
                )),
                "stop_failed",
            ),
            Ok(None) => (
                false,
                Some("browser_process_stop_unproven".into()),
                "unproven",
            ),
            Err(_) => (false, Some("browser_kill_timeout".into()), "stop_failed"),
        }
    };
    let process_code = match process_outcome {
        "exited" => "browser_process_exited",
        "killed" => "browser_process_killed",
        "unproven" => "browser_process_stop_unproven",
        _ if process_failure.as_deref() == Some("browser_kill_timeout") => "browser_kill_timeout",
        _ => "browser_kill_failed",
    };
    tracing::info!(
        event = "browser_lifecycle",
        stage = "browser_process_stop",
        code = process_code,
        browser_session,
        outcome = process_outcome,
        process_stopped,
        stage_duration_ms = duration_millis(started.elapsed()),
        "Завершена проверка остановки принадлежащего браузера"
    );
    BrowserStopObservation {
        transport,
        process_stopped,
        process_failure,
    }
}

fn classify_browser_cleanup(
    stopped: BrowserStopObservation,
    cleanup: Result<(), String>,
) -> Result<(), String> {
    let stop = if let Some(failure) = stopped.process_failure {
        Err(failure)
    } else if !stopped.process_stopped {
        Err("browser_process_stop_unproven".into())
    } else {
        match stopped.transport {
            BrowserTransportStop::Closed | BrowserTransportStop::Detached => Ok(()),
            BrowserTransportStop::Failed(error) => Err(error),
        }
    };
    combine_browser_close_results(stop, cleanup)
}

async fn shutdown_handler(handler: tokio::task::JoinHandle<()>) -> Result<(), String> {
    handler.abort();
    match handler.await {
        Ok(()) => Ok(()),
        Err(error) if error.is_cancelled() => Ok(()),
        Err(_) => Err("browser_handler_shutdown_failed: паника обработчика CDP".into()),
    }
}

impl BrowserResources {
    async fn finish_setup<T>(
        self,
        setup: Result<T, BrowserSetupFailure>,
    ) -> Result<(Self, T), BrowserLaunchError> {
        match setup {
            Ok(value) => Ok((self, value)),
            Err(failure) => {
                let original = failure.launch_error("browser_setup_panicked");
                Err(original.with_cleanup(self.close().await))
            }
        }
    }

    async fn close(mut self) -> Result<(), String> {
        let stopped = match self.browser.as_mut() {
            Some(browser) => stop_browser(browser, self.diagnostic_session_id).await,
            None => BrowserStopObservation {
                transport: BrowserTransportStop::Closed,
                process_stopped: true,
                process_failure: None,
            },
        };
        let started = Instant::now();
        let handler_result = match self.handler_task.take() {
            Some(handler) => shutdown_handler(handler).await,
            None => Ok(()),
        };
        tracing::info!(
            event = "browser_lifecycle",
            stage = "browser_handler_shutdown",
            code = if handler_result.is_ok() {
                "browser_handler_stopped"
            } else {
                "browser_handler_shutdown_failed"
            },
            browser_session = self.diagnostic_session_id,
            outcome = if handler_result.is_ok() {
                "stopped"
            } else {
                "failed"
            },
            stage_duration_ms = duration_millis(started.elapsed()),
            "Завершена остановка обработчика CDP"
        );
        drop(self.browser.take());
        let started = Instant::now();
        let cleanup = match self.profile.take() {
            Some(profile) => profile.close_after_browser_stop().await.map_err(|error| {
                format!(
                    "browser_profile_cleanup_failed: {}",
                    crate::diagnostics::safe_message(&error.to_string())
                )
            }),
            None => Ok(()),
        };
        tracing::info!(
            event = "browser_lifecycle",
            stage = "browser_profile_temp_cleanup",
            code = if cleanup.is_ok() {
                "browser_profile_temp_removed"
            } else {
                "browser_profile_cleanup_failed"
            },
            browser_session = self.diagnostic_session_id,
            outcome = if cleanup.is_ok() { "removed" } else { "failed" },
            owned_resources_removed = cleanup.is_ok(),
            stage_duration_ms = duration_millis(started.elapsed()),
            "Завершена проверка удаления профиля и временного каталога"
        );
        tracing::info!(
            event = "browser_lifecycle",
            stage = "browser_owned_resource_postconditions",
            browser_session = self.diagnostic_session_id,
            code = if stopped.process_stopped && cleanup.is_ok() {
                "browser_owned_resources_removed"
            } else {
                "browser_owned_resources_unproven"
            },
            process_stopped = stopped.process_stopped,
            profile_temp_removed = cleanup.is_ok(),
            "Проверены postconditions принадлежащих процессов и каталогов"
        );
        let result = combine_browser_close_results(
            classify_browser_cleanup(stopped, cleanup),
            handler_result,
        );
        if let Err(error) = &result {
            tracing::error!(stage = "temp_cleanup", code = "browser_cleanup_failed", path_category = "browser_workspace", message = %crate::diagnostics::safe_message(error), "Ошибка закрытия браузера или очистки профиля");
        }
        result
    }
}

fn combine_browser_close_results(
    browser_result: Result<(), String>,
    cleanup_result: Result<(), String>,
) -> Result<(), String> {
    match (browser_result, cleanup_result) {
        (Err(browser), Err(cleanup)) => Err(format!("{browser}; {cleanup}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        _ => Ok(()),
    }
}

impl Drop for BrowserResources {
    fn drop(&mut self) {
        let browser = self.browser.take();
        let handler = self.handler_task.take();
        let profile = self.profile.take();
        if browser.is_none() && handler.is_none() && profile.is_none() {
            return;
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let diagnostic_session_id = self.diagnostic_session_id;
            runtime.spawn(async move {
                let resources = BrowserResources {
                    diagnostic_session_id,
                    browser,
                    handler_task: handler,
                    profile,
                };
                let _ = resources.close().await;
            });
        } else {
            if let Some(handler) = handler {
                handler.abort();
            }
            drop(browser);
            drop(profile);
        }
    }
}

/// Изолированный Chromium-процесс, его начальная страница и CDP-монитор.
pub struct BrowserSession {
    diagnostic_session_id: u64,
    resources: Option<BrowserResources>,
    page: Page,
    provenance: BrowserRuntimeProvenance,
    telemetry: CdpRuntimeMonitor,
}

impl BrowserSession {
    pub async fn launch(config: BrowserRuntimeConfig) -> Result<Self, BrowserLaunchError> {
        config.validate().map_err(BrowserLaunchError::Setup)?;
        let profile = BrowserProfile::standalone()?;
        Self::launch_with_profile(config, profile).await
    }

    /// Профиль принадлежит сеансу; переданное временное дерево запуска не удаляется.
    pub async fn launch_in_workspace(
        config: BrowserRuntimeConfig,
        workspace: &Path,
    ) -> Result<Self, BrowserLaunchError> {
        config.validate().map_err(BrowserLaunchError::Setup)?;
        let profile = BrowserProfile::in_workspace(workspace)?;
        Self::launch_with_profile(config, profile).await
    }

    async fn launch_with_profile(
        config: BrowserRuntimeConfig,
        profile: BrowserProfile,
    ) -> Result<Self, BrowserLaunchError> {
        let executable = find_browser_executable();
        let executable_source = executable
            .as_ref()
            .map(|selection| selection.source)
            .unwrap_or(BrowserExecutableSource::ChromiumoxideDefault);
        let browser_config = match build_browser_config(&profile, &config, executable) {
            Ok(config) => config,
            Err(message) => {
                return Err(profile_setup_failure(profile, message));
            }
        };
        // Если разворачивание стека прервёт получение Browser, дочерний процесс
        // chromiumoxide применит `kill_on_drop`. `catch_browser_setup` полностью
        // уничтожит `Future` запуска; затем ждём, пока процессы перестанут
        // использовать временное дерево, и только после этого удаляем профиль.
        // Это не требует обращаться к скрытому дочернему процессу через `waitpid`.
        let launched = catch_browser_setup(async {
            Browser::launch(browser_config)
                .await
                .map_err(|error| format!("запуск браузера: {error}"))
        })
        .await;
        let (browser, mut handler) = match launched {
            Ok(launched) => launched,
            Err(failure) => {
                return Err(profile_launch_failure(profile, failure).await);
            }
        };
        let handler_task = tokio::spawn(async move {
            while let Some(event) = handler.next().await {
                if event.is_err() {
                    break;
                }
            }
        });

        let mut resources = BrowserResources {
            diagnostic_session_id: NEXT_BROWSER_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            browser: Some(browser),
            handler_task: Some(handler_task),
            profile: Some(profile),
        };
        // Перехватываем панику, пока `resources` принадлежит запуску: `Drop`
        // запускает только фоновую резервную очистку и не гарантирует её завершение.
        let setup = catch_browser_setup(async {
            let browser = resources.browser.as_mut().expect("браузер запущен");
            let version = browser
                .version()
                .await
                .map_err(|error| format!("CDP Browser.getVersion: {error}"))?;
            let provenance = BrowserRuntimeProvenance {
                product: version.product,
                protocol_version: version.protocol_version,
                revision: version.revision,
                user_agent: version.user_agent,
                js_version: version.js_version,
                executable_source,
            };
            let page = browser
                .new_page("about:blank")
                .await
                .map_err(|error| format!("создание страницы браузера: {error}"))?;
            page.execute(EnableParams::default())
                .await
                .map_err(|error| format!("CDP Page.enable: {error}"))?;
            if let Some(metrics) = config.device_metrics {
                page.execute(metrics.cdp_params())
                    .await
                    .map_err(|error| format!("CDP Emulation.setDeviceMetricsOverride: {error}"))?;
            }
            if let Some(scheme) = &config.prefers_color_scheme {
                page.execute(
                    SetEmulatedMediaParams::builder()
                        .feature(MediaFeature::new("prefers-color-scheme", scheme.clone()))
                        .build(),
                )
                .await
                .map_err(|error| format!("CDP Emulation.setEmulatedMedia: {error}"))?;
            }
            let telemetry = CdpRuntimeMonitor::start(&page).await?;
            Ok::<_, String>((page, provenance, telemetry))
        })
        .await;

        let (resources, (page, provenance, telemetry)) = resources.finish_setup(setup).await?;
        let diagnostic_session_id = resources.diagnostic_session_id;
        Ok(Self {
            diagnostic_session_id,
            resources: Some(resources),
            page,
            provenance,
            telemetry,
        })
    }

    pub fn diagnostic_session_id(&self) -> u64 {
        self.diagnostic_session_id
    }

    pub fn page(&self) -> &Page {
        &self.page
    }

    pub fn provenance(&self) -> &BrowserRuntimeProvenance {
        &self.provenance
    }

    pub fn telemetry(&self) -> &CdpRuntimeMonitor {
        &self.telemetry
    }

    pub async fn close(mut self) -> Result<(), String> {
        let started = Instant::now();
        let telemetry_result = self.telemetry.shutdown().await;
        tracing::info!(
            event = "browser_lifecycle",
            stage = "browser_telemetry_shutdown",
            code = if telemetry_result.is_ok() {
                "browser_telemetry_stopped"
            } else {
                "browser_telemetry_shutdown_timeout"
            },
            browser_session = self.diagnostic_session_id,
            outcome = if telemetry_result.is_ok() {
                "stopped"
            } else {
                "failed"
            },
            stage_duration_ms = duration_millis(started.elapsed()),
            "Завершена остановка мониторинга CDP"
        );
        let cleanup = self
            .resources
            .take()
            .expect("ресурсы сеанса доступны")
            .close()
            .await;
        combine_browser_close_results(telemetry_result, cleanup)
    }
}

impl Drop for BrowserSession {
    fn drop(&mut self) {
        self.telemetry.abort();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedRequest {
    pub request_id: String,
    pub resource_type: ResourceType,
    pub url: String,
    pub epoch: u64,
    pub is_top_level: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkOutcome {
    pub resource_type: ResourceType,
    pub url: Option<String>,
    pub failure_reason: Option<String>,
    pub request_id: String,
    pub epoch: u64,
    pub status_code: Option<u16>,
    pub is_top_level: bool,
}

/// Неинтерпретированная провайдером телеметрия CDP одного этапа получения.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeSnapshot {
    /// Эпоха, для которой снято наблюдение; начальная эпоха 0 остаётся явной.
    pub epoch: u64,
    /// Монотонный возраст реально наблюдавшихся ожидающих запросов.
    pub pending_observations: Vec<PendingRequestObservation>,
    pub pending_requests: Vec<TrackedRequest>,
    pub network_failures: Vec<NetworkOutcome>,
    pub http_errors: Vec<NetworkOutcome>,
    pub javascript_exceptions: u32,
    pub monitor_failed: bool,
    /// Свидетельства чужих эпох отделены от рабочего снимка готовности.
    pub stale_pending_requests: Vec<TrackedRequest>,
    pub stale_network_failures: Vec<NetworkOutcome>,
    pub stale_http_errors: Vec<NetworkOutcome>,
    pub current_javascript_exceptions: u32,
    pub bootstrap_javascript_exceptions: u32,
    pub stale_javascript_exceptions: u32,
}

/// Возраст запроса CDP, а не длительность server-side обработки.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRequestObservation {
    pub request_id: String,
    pub observed_ms: u64,
}

/// Ограниченные сведения для журналов: URL и сырые идентификаторы не сериализуются.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeSnapshotDiagnostic {
    pub epoch: u64,
    pub pending_request_count: usize,
    pub network_failure_count: usize,
    pub http_error_count: usize,
    pub pending_requests: Vec<RuntimeRequestDiagnostic>,
    pub network_failures: Vec<RuntimeRequestDiagnostic>,
    pub http_errors: Vec<RuntimeRequestDiagnostic>,
    pub javascript_exceptions: u32,
    pub monitor_failed: bool,
    pub stale_pending_request_count: usize,
    pub stale_network_failure_count: usize,
    pub stale_http_error_count: usize,
    pub stale_pending_requests: Vec<RuntimeRequestDiagnostic>,
    pub stale_network_failures: Vec<RuntimeRequestDiagnostic>,
    pub stale_http_errors: Vec<RuntimeRequestDiagnostic>,
    pub current_javascript_exceptions: u32,
    pub bootstrap_javascript_exceptions: u32,
    pub stale_javascript_exceptions: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeRequestDiagnostic {
    pub request_key: String,
    pub resource_type: ResourceType,
    pub epoch: u64,
    pub in_current_epoch: bool,
    pub is_top_level: bool,
    pub observed_ms: Option<u64>,
    pub status_code: Option<u16>,
    pub failure_category: Option<String>,
}

impl RuntimeSnapshot {
    pub fn diagnostic(&self) -> RuntimeSnapshotDiagnostic {
        let pending = self
            .pending_requests
            .iter()
            .filter(|request| request_in_scope(request.epoch, self.epoch));
        let network = self
            .network_failures
            .iter()
            .filter(|outcome| request_in_scope(outcome.epoch, self.epoch));
        let http = self
            .http_errors
            .iter()
            .filter(|outcome| request_in_scope(outcome.epoch, self.epoch));
        let outcome = |outcome: &NetworkOutcome| RuntimeRequestDiagnostic {
            request_key: crate::hashing::sha256_hex(&outcome.request_id),
            resource_type: outcome.resource_type.clone(),
            epoch: outcome.epoch,
            in_current_epoch: outcome.epoch == self.epoch,
            is_top_level: outcome.is_top_level,
            observed_ms: None,
            status_code: outcome.status_code,
            failure_category: outcome
                .failure_reason
                .as_deref()
                .map(sanitized_network_failure_reason),
        };
        let request = |request: &TrackedRequest| RuntimeRequestDiagnostic {
            request_key: crate::hashing::sha256_hex(&request.request_id),
            resource_type: request.resource_type.clone(),
            epoch: request.epoch,
            in_current_epoch: request.epoch == self.epoch,
            is_top_level: request.is_top_level,
            observed_ms: self
                .pending_observations
                .iter()
                .find(|observation| observation.request_id == request.request_id)
                .map(|observation| observation.observed_ms),
            status_code: None,
            failure_category: None,
        };
        // Защита от исходных тестовых данных со смешанными эпохами: чужие записи не становятся
        // текущими даже при сборке снимка вне RuntimeState.
        let stale_pending = self.stale_pending_requests.iter().chain(
            self.pending_requests
                .iter()
                .filter(|request| !request_in_scope(request.epoch, self.epoch)),
        );
        let stale_network = self.stale_network_failures.iter().chain(
            self.network_failures
                .iter()
                .filter(|outcome| !request_in_scope(outcome.epoch, self.epoch)),
        );
        let stale_http = self.stale_http_errors.iter().chain(
            self.http_errors
                .iter()
                .filter(|outcome| !request_in_scope(outcome.epoch, self.epoch)),
        );
        RuntimeSnapshotDiagnostic {
            epoch: self.epoch,
            pending_request_count: pending.clone().count(),
            network_failure_count: network.clone().count(),
            http_error_count: http.clone().count(),
            pending_requests: pending
                .take(MAX_RUNTIME_DIAGNOSTIC_ITEMS)
                .map(request)
                .collect(),
            network_failures: network
                .take(MAX_RUNTIME_DIAGNOSTIC_ITEMS)
                .map(outcome)
                .collect(),
            http_errors: http
                .take(MAX_RUNTIME_DIAGNOSTIC_ITEMS)
                .map(outcome)
                .collect(),
            javascript_exceptions: self.javascript_exceptions,
            monitor_failed: self.monitor_failed,
            stale_pending_request_count: stale_pending.clone().count(),
            stale_network_failure_count: stale_network.clone().count(),
            stale_http_error_count: stale_http.clone().count(),
            stale_pending_requests: stale_pending
                .take(MAX_RUNTIME_DIAGNOSTIC_ITEMS)
                .map(request)
                .collect(),
            stale_network_failures: stale_network
                .take(MAX_RUNTIME_DIAGNOSTIC_ITEMS)
                .map(outcome)
                .collect(),
            stale_http_errors: stale_http
                .take(MAX_RUNTIME_DIAGNOSTIC_ITEMS)
                .map(outcome)
                .collect(),
            current_javascript_exceptions: self.current_javascript_exceptions,
            bootstrap_javascript_exceptions: self.bootstrap_javascript_exceptions,
            stale_javascript_exceptions: self.stale_javascript_exceptions,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryTrigger {
    ReadinessTimeout,
    ItemTimeout,
    RuntimeFailure,
}

#[derive(Debug, Default)]
struct RuntimeState {
    relevant_requests: HashMap<String, TrackedRequest>,
    request_observed_at: HashMap<String, Instant>,
    network_failures: Vec<NetworkOutcome>,
    http_errors: Vec<NetworkOutcome>,
    javascript_exceptions: HashMap<u64, u32>,
    monitor_failed: bool,
    main_frame_id: String,
    active_epoch: u64,
    next_epoch: u64,
    // Слушатели событий CDP разных типов работают независимо, поэтому терминальное
    // событие ответа из кэша может быть обработано раньше его `requestWillBeSent`.
    completed_requests: HashSet<String>,
    completed_order: VecDeque<String>,
}

impl RuntimeState {
    fn begin_epoch(&mut self) -> u64 {
        self.next_epoch = self.next_epoch.saturating_add(1).max(1);
        self.active_epoch = self.next_epoch;
        self.active_epoch
    }

    fn record_request(
        &mut self,
        request_id: String,
        resource_type: ResourceType,
        url: String,
        frame_id: Option<&str>,
    ) {
        if !is_relevant_resource_type(&resource_type) {
            return;
        }
        if request_id.len() > MAX_RUNTIME_REQUEST_ID_BYTES || url.len() > MAX_RUNTIME_URL_BYTES {
            self.monitor_failed = true;
            return;
        }
        let is_top_level = frame_id.is_some_and(|id| id == self.main_frame_id);
        // Исход, обработанный раньше запроса, получает его URL и принадлежность кадру:
        // без них критический сбой страницы не распознаётся как критический.
        // Шаги перенаправления одного запроса приходят по порядку, поэтому сбой
        // относится к последнему из них; URL ошибки HTTP взят из самого ответа.
        for outcome in self
            .network_failures
            .iter_mut()
            .filter(|outcome| outcome.request_id == request_id)
        {
            outcome.url = Some(url.clone());
            outcome.is_top_level |= is_top_level;
        }
        for outcome in self
            .http_errors
            .iter_mut()
            .filter(|outcome| outcome.request_id == request_id)
        {
            outcome.url.get_or_insert_with(|| url.clone());
            outcome.is_top_level |= is_top_level;
        }
        if self.completed_requests.contains(&request_id) {
            return;
        }
        if !self.relevant_requests.contains_key(&request_id)
            && self.relevant_requests.len() >= MAX_TRACKED_REQUESTS
        {
            self.monitor_failed = true;
            return;
        }
        let epoch = self
            .relevant_requests
            .get(&request_id)
            .map_or(self.active_epoch, |request| request.epoch);
        self.request_observed_at
            .entry(request_id.clone())
            .or_insert_with(Instant::now);
        self.relevant_requests.insert(
            request_id.clone(),
            TrackedRequest {
                request_id,
                resource_type,
                url,
                epoch,
                is_top_level,
            },
        );
    }

    fn record_finished(&mut self, request_id: &str) {
        self.relevant_requests.remove(request_id);
        self.request_observed_at.remove(request_id);
        self.remember_completed(request_id);
    }

    /// Терминальное событие окончательно: поздний `requestWillBeSent` того же запроса
    /// (в том числе шаг перенаправления) уже не делает его ожидающим.
    fn remember_completed(&mut self, request_id: &str) {
        if request_id.len() > MAX_RUNTIME_REQUEST_ID_BYTES
            || self.completed_requests.contains(request_id)
        {
            return;
        }
        if self.completed_order.len() >= MAX_COMPLETED_REQUESTS
            && let Some(oldest) = self.completed_order.pop_front()
        {
            self.completed_requests.remove(&oldest);
        }
        self.completed_requests.insert(request_id.to_owned());
        self.completed_order.push_back(request_id.to_owned());
    }

    fn record_network_failure(
        &mut self,
        request_id: String,
        event_resource_type: ResourceType,
        failure_reason: String,
    ) {
        let tracked = self.relevant_requests.remove(&request_id);
        self.request_observed_at.remove(&request_id);
        self.remember_completed(&request_id);
        if request_id.len() > MAX_RUNTIME_REQUEST_ID_BYTES
            || failure_reason.len() > MAX_RUNTIME_REASON_BYTES
        {
            self.monitor_failed = true;
            return;
        }
        if tracked.is_none() && !is_relevant_resource_type(&event_resource_type) {
            return;
        }
        if self.network_failures.len() >= MAX_NETWORK_OUTCOMES {
            self.monitor_failed = true;
            return;
        }
        self.network_failures.push(NetworkOutcome {
            resource_type: tracked
                .as_ref()
                .map(|request| request.resource_type.clone())
                .unwrap_or(event_resource_type),
            url: tracked.as_ref().map(|request| request.url.clone()),
            failure_reason: Some(failure_reason),
            request_id,
            epoch: tracked
                .as_ref()
                .map_or(self.active_epoch, |request| request.epoch),
            status_code: None,
            is_top_level: tracked.is_some_and(|request| request.is_top_level),
        });
    }

    fn record_http_error(
        &mut self,
        request_id: String,
        resource_type: ResourceType,
        url: String,
        status_code: u16,
    ) {
        if !is_relevant_resource_type(&resource_type) || status_code < 400 {
            return;
        }
        if request_id.len() > MAX_RUNTIME_REQUEST_ID_BYTES || url.len() > MAX_RUNTIME_URL_BYTES {
            self.monitor_failed = true;
            return;
        }
        if self.http_errors.len() >= MAX_NETWORK_OUTCOMES {
            self.monitor_failed = true;
            return;
        }
        let tracked = self.relevant_requests.get(&request_id);
        self.http_errors.push(NetworkOutcome {
            resource_type,
            url: Some(url),
            failure_reason: None,
            request_id,
            epoch: tracked.map_or(self.active_epoch, |request| request.epoch),
            status_code: Some(status_code),
            is_top_level: tracked.is_some_and(|request| request.is_top_level),
        });
    }

    fn record_javascript_exception(&mut self) {
        if !self.javascript_exceptions.contains_key(&self.active_epoch)
            && self.javascript_exceptions.len() >= MAX_NETWORK_OUTCOMES
        {
            self.monitor_failed = true;
            return;
        }
        let current = self
            .javascript_exceptions
            .entry(self.active_epoch)
            .or_default();
        *current = current.saturating_add(1);
        if self.javascript_exceptions.len() > MAX_NETWORK_OUTCOMES {
            self.monitor_failed = true;
        }
    }

    fn snapshot(&self, epoch: u64) -> RuntimeSnapshot {
        self.snapshot_at(epoch, Instant::now())
    }

    fn snapshot_at(&self, epoch: u64, now: Instant) -> RuntimeSnapshot {
        let mut pending_requests = self.relevant_requests.values().cloned().collect::<Vec<_>>();
        pending_requests.sort_by(|left, right| left.request_id.cmp(&right.request_id));
        let pending_observations = pending_requests
            .iter()
            .filter_map(|request| {
                self.request_observed_at
                    .get(&request.request_id)
                    .map(|observed| PendingRequestObservation {
                        request_id: request.request_id.clone(),
                        observed_ms: duration_millis(now.saturating_duration_since(*observed)),
                    })
            })
            .collect();
        let (pending_requests, stale_pending_requests) = pending_requests
            .into_iter()
            .partition(|request| request_in_scope(request.epoch, epoch));
        RuntimeSnapshot {
            epoch,
            pending_observations,
            pending_requests,
            stale_pending_requests,
            stale_network_failures: self
                .network_failures
                .iter()
                .filter(|outcome| !request_in_scope(outcome.epoch, epoch))
                .cloned()
                .collect(),
            stale_http_errors: self
                .http_errors
                .iter()
                .filter(|outcome| !request_in_scope(outcome.epoch, epoch))
                .cloned()
                .collect(),
            current_javascript_exceptions: self
                .javascript_exceptions
                .get(&epoch)
                .copied()
                .unwrap_or(0),
            bootstrap_javascript_exceptions: if epoch == 0 {
                0
            } else {
                self.javascript_exceptions.get(&0).copied().unwrap_or(0)
            },
            stale_javascript_exceptions: self
                .javascript_exceptions
                .iter()
                .filter(|(recorded_epoch, _)| !request_in_scope(**recorded_epoch, epoch))
                .fold(0_u32, |total, (_, count)| total.saturating_add(*count)),
            network_failures: self
                .network_failures
                .iter()
                .filter(|failure| request_in_scope(failure.epoch, epoch))
                .cloned()
                .collect(),
            http_errors: self
                .http_errors
                .iter()
                .filter(|failure| request_in_scope(failure.epoch, epoch))
                .cloned()
                .collect(),
            javascript_exceptions: self
                .javascript_exceptions
                .iter()
                .filter(|(recorded_epoch, _)| request_in_scope(**recorded_epoch, epoch))
                .fold(0_u32, |total, (_, count)| total.saturating_add(*count)),
            monitor_failed: self.monitor_failed,
        }
    }
}

/// Слушает события Network и Runtime CDP; состояние ограничено по размеру, а
/// при потере события всегда сообщает об ошибке мониторинга.
#[derive(Debug, Clone)]
pub struct CdpRuntimeMonitor {
    state: Arc<Mutex<RuntimeState>>,
    tasks: Arc<Vec<tokio::task::JoinHandle<()>>>,
}

impl CdpRuntimeMonitor {
    pub async fn start(page: &Page) -> Result<Self, String> {
        let main_frame_id = page
            .execute(GetFrameTreeParams::default())
            .await
            .map_err(|error| format!("CDP Page.getFrameTree: {error}"))?
            .frame_tree
            .frame
            .id
            .as_ref()
            .to_owned();
        let request_events = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|error| format!("обработчик CDP Network.requestWillBeSent: {error}"))?;
        let finished_events = page
            .event_listener::<EventLoadingFinished>()
            .await
            .map_err(|error| format!("обработчик CDP Network.loadingFinished: {error}"))?;
        let failed_events = page
            .event_listener::<EventLoadingFailed>()
            .await
            .map_err(|error| format!("обработчик CDP Network.loadingFailed: {error}"))?;
        let response_events = page
            .event_listener::<EventResponseReceived>()
            .await
            .map_err(|error| format!("обработчик CDP Network.responseReceived: {error}"))?;
        let exception_events = page
            .event_listener::<EventExceptionThrown>()
            .await
            .map_err(|error| format!("обработчик CDP Runtime.exceptionThrown: {error}"))?;

        page.execute(NetworkEnableParams::default())
            .await
            .map_err(|error| format!("CDP Network.enable: {error}"))?;
        page.execute(RuntimeEnableParams::default())
            .await
            .map_err(|error| format!("CDP Runtime.enable: {error}"))?;

        Ok(Self::from_event_streams(
            main_frame_id,
            request_events,
            finished_events,
            failed_events,
            response_events,
            exception_events,
        ))
    }

    // Каждая BrowserSession получает собственное состояние и собственные задачи:
    // старые слушатели и их поздние события остаются у прежнего монитора.
    fn from_event_streams(
        main_frame_id: String,
        request_events: impl Stream<Item = Arc<EventRequestWillBeSent>> + Send + Unpin + 'static,
        finished_events: impl Stream<Item = Arc<EventLoadingFinished>> + Send + Unpin + 'static,
        failed_events: impl Stream<Item = Arc<EventLoadingFailed>> + Send + Unpin + 'static,
        response_events: impl Stream<Item = Arc<EventResponseReceived>> + Send + Unpin + 'static,
        exception_events: impl Stream<Item = Arc<EventExceptionThrown>> + Send + Unpin + 'static,
    ) -> Self {
        let state = Arc::new(Mutex::new(RuntimeState {
            main_frame_id,
            ..RuntimeState::default()
        }));
        let mut tasks = Vec::with_capacity(5);

        {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                let mut events = request_events;
                while let Some(event) = events.next().await {
                    let Some(resource_type) = event.r#type.as_ref() else {
                        mark_monitor_failed(&state);
                        continue;
                    };
                    lock_state(&state).record_request(
                        event.request_id.as_ref().to_owned(),
                        resource_type.clone(),
                        event.request.url.clone(),
                        event.frame_id.as_ref().map(|id| id.as_ref()),
                    );
                }
                mark_monitor_failed(&state);
            }));
        }
        {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                let mut events = finished_events;
                while let Some(event) = events.next().await {
                    lock_state(&state).record_finished(event.request_id.as_ref());
                }
                mark_monitor_failed(&state);
            }));
        }
        {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                let mut events = failed_events;
                while let Some(event) = events.next().await {
                    lock_state(&state).record_network_failure(
                        event.request_id.as_ref().to_owned(),
                        event.r#type.clone(),
                        event.error_text.clone(),
                    );
                }
                mark_monitor_failed(&state);
            }));
        }
        {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                let mut events = response_events;
                while let Some(event) = events.next().await {
                    lock_state(&state).record_http_error(
                        event.request_id.as_ref().to_owned(),
                        event.r#type.clone(),
                        event.response.url.clone(),
                        event.response.status as u16,
                    );
                }
                mark_monitor_failed(&state);
            }));
        }
        {
            let state = Arc::clone(&state);
            tasks.push(tokio::spawn(async move {
                let mut events = exception_events;
                while events.next().await.is_some() {
                    lock_state(&state).record_javascript_exception();
                }
                mark_monitor_failed(&state);
            }));
        }

        Self {
            state,
            tasks: Arc::new(tasks),
        }
    }

    pub fn begin_epoch(&self) -> u64 {
        lock_state(&self.state).begin_epoch()
    }

    pub fn snapshot(&self, epoch: u64) -> RuntimeSnapshot {
        lock_state(&self.state).snapshot(epoch)
    }

    pub fn current_snapshot(&self) -> RuntimeSnapshot {
        let state = lock_state(&self.state);
        state.snapshot(state.active_epoch)
    }

    pub fn monitor_failed(&self) -> bool {
        lock_state(&self.state).monitor_failed
    }

    pub(crate) fn retryable_acquisition(&self, epoch: u64, trigger: RetryTrigger) -> bool {
        let state = lock_state(&self.state);
        if state.monitor_failed {
            return false;
        }
        if trigger == RetryTrigger::ReadinessTimeout {
            return true;
        }

        let bootstrap_failure = state
            .network_failures
            .iter()
            .any(|failure| failure.epoch == 0)
            || state.http_errors.iter().any(|failure| failure.epoch == 0)
            || state.javascript_exceptions.get(&0).copied().unwrap_or(0) > 0;
        let has_epoch_network_failure = state
            .network_failures
            .iter()
            .any(|failure| failure.epoch == epoch);
        let has_epoch_http_error = state
            .http_errors
            .iter()
            .any(|failure| failure.epoch == epoch);
        let epoch_network_failures_are_retryable = state
            .network_failures
            .iter()
            .filter(|failure| failure.epoch == epoch)
            .all(|failure| {
                failure
                    .failure_reason
                    .as_deref()
                    .is_some_and(is_retryable_network_error)
            });
        let epoch_http_errors_are_retryable = state
            .http_errors
            .iter()
            .filter(|failure| failure.epoch == epoch)
            .all(|failure| failure.status_code.is_some_and(is_retryable_http_status));
        let has_epoch_failure = has_epoch_network_failure || has_epoch_http_error;

        !bootstrap_failure
            && state
                .javascript_exceptions
                .get(&epoch)
                .copied()
                .unwrap_or(0)
                == 0
            && epoch_network_failures_are_retryable
            && epoch_http_errors_are_retryable
            && (trigger == RetryTrigger::ItemTimeout || has_epoch_failure)
    }

    /// Удаляет только запись с совпадающими идентификатором запроса CDP, URL и причиной.
    /// Провайдер сам проверяет, что такое исключение разрешено его политикой.
    pub fn clear_exact_network_failure(
        &self,
        request_id: &str,
        url: &str,
        failure_reason: &str,
    ) -> bool {
        let mut state = lock_state(&self.state);
        let Some(index) = state.network_failures.iter().position(|failure| {
            failure.request_id == request_id
                && failure.url.as_deref() == Some(url)
                && failure
                    .failure_reason
                    .as_deref()
                    .is_some_and(|reason| reason.eq_ignore_ascii_case(failure_reason))
        }) else {
            return false;
        };
        state.network_failures.remove(index);
        true
    }

    pub fn abort(&self) {
        for task in self.tasks.iter() {
            task.abort();
        }
    }

    async fn shutdown(&self) -> Result<(), String> {
        self.abort();
        timeout(Duration::from_secs(2), async {
            while self.tasks.iter().any(|task| !task.is_finished()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| "browser_telemetry_shutdown_timeout".into())
    }

    #[cfg(test)]
    pub(crate) fn from_snapshot_for_test(snapshot: RuntimeSnapshot) -> Self {
        let mut javascript_exceptions = HashMap::new();
        if snapshot.javascript_exceptions > 0 {
            javascript_exceptions.insert(0, snapshot.javascript_exceptions);
        }
        Self {
            state: Arc::new(Mutex::new(RuntimeState {
                relevant_requests: snapshot
                    .pending_requests
                    .into_iter()
                    .chain(snapshot.stale_pending_requests)
                    .map(|request| (request.request_id.clone(), request))
                    .collect(),
                network_failures: snapshot
                    .network_failures
                    .into_iter()
                    .chain(snapshot.stale_network_failures)
                    .collect(),
                http_errors: snapshot
                    .http_errors
                    .into_iter()
                    .chain(snapshot.stale_http_errors)
                    .collect(),
                javascript_exceptions,
                monitor_failed: snapshot.monitor_failed,
                ..RuntimeState::default()
            })),
            tasks: Arc::new(Vec::new()),
        }
    }
}

pub fn is_relevant_resource_type(resource_type: &ResourceType) -> bool {
    matches!(
        resource_type,
        ResourceType::Document
            | ResourceType::Stylesheet
            | ResourceType::Image
            | ResourceType::Font
            | ResourceType::Script
            | ResourceType::Xhr
            | ResourceType::Fetch
    )
}

pub fn is_retryable_http_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

pub fn is_retryable_network_error(reason: &str) -> bool {
    matches!(
        reason.trim().to_ascii_uppercase().as_str(),
        "NET::ERR_TIMED_OUT"
            | "ERR_TIMED_OUT"
            | "NET::ERR_CONNECTION_RESET"
            | "ERR_CONNECTION_RESET"
            | "NET::ERR_CONNECTION_CLOSED"
            | "ERR_CONNECTION_CLOSED"
            | "NET::ERR_CONNECTION_REFUSED"
            | "ERR_CONNECTION_REFUSED"
            | "NET::ERR_NETWORK_CHANGED"
            | "ERR_NETWORK_CHANGED"
            | "NET::ERR_ABORTED"
            | "ERR_ABORTED"
    )
}

pub fn format_network_failure_details(failures: &[NetworkOutcome]) -> String {
    if failures.is_empty() {
        return "нет".into();
    }
    let mut details = failures
        .iter()
        .take(MAX_NETWORK_DIAGNOSTIC_ITEMS)
        .map(|failure| {
            let location = failure
                .url
                .as_deref()
                .map(sanitized_network_location)
                .unwrap_or_else(|| "<URL недоступен>".into());
            let reason = failure
                .failure_reason
                .as_deref()
                .map(sanitized_network_failure_reason)
                .unwrap_or_else(|| "не классифицированная сетевая ошибка".into());
            format!("{:?} {location} {reason}", failure.resource_type)
        })
        .collect::<Vec<_>>();
    if failures.len() > MAX_NETWORK_DIAGNOSTIC_ITEMS {
        details.push(format!(
            "ещё сетевых ошибок: {}",
            failures.len() - MAX_NETWORK_DIAGNOSTIC_ITEMS
        ));
    }
    details.join("; ")
}

fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn request_in_scope(request_epoch: u64, requested_epoch: u64) -> bool {
    request_epoch == 0 || request_epoch == requested_epoch
}

fn lock_state(state: &Mutex<RuntimeState>) -> std::sync::MutexGuard<'_, RuntimeState> {
    state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn mark_monitor_failed(state: &Mutex<RuntimeState>) {
    lock_state(state).monitor_failed = true;
}

pub fn find_browser_executable() -> Option<BrowserExecutableSelection> {
    for (variable, source) in [
        ("CHROME_BIN", BrowserExecutableSource::ChromeBinEnvironment),
        (
            "CHROMIUM_BIN",
            BrowserExecutableSource::ChromiumBinEnvironment,
        ),
    ] {
        if let Some(path) = std::env::var_os(variable).map(PathBuf::from)
            && is_executable_file(&path)
        {
            return Some(BrowserExecutableSelection { path, source });
        }
    }
    if let Some(path_entries) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path_entries) {
            for name in ["chromium", "chromium-browser", "google-chrome", "chrome"] {
                let path = directory.join(name);
                if is_executable_file(&path) {
                    return Some(BrowserExecutableSelection {
                        path,
                        source: BrowserExecutableSource::PathLookup,
                    });
                }
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    let cache = home.join(".cache/ms-playwright");
    let mut candidates = Vec::new();
    let entries = std::fs::read_dir(cache).ok()?;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("chromium-") {
            continue;
        }
        candidates.push(entry.path().join("chrome-linux64/chrome"));
        candidates.push(entry.path().join("chrome-linux/chrome"));
    }
    candidates.sort();
    candidates
        .into_iter()
        .find(|path| is_executable_file(path))
        .map(|path| BrowserExecutableSelection {
            path,
            source: BrowserExecutableSource::PlaywrightCache,
        })
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn sanitized_network_location(raw_url: &str) -> String {
    let Ok(url) = Url::parse(raw_url) else {
        return "<URL не удалось разобрать>".into();
    };
    if !matches!(url.scheme(), "http" | "https") || !url.origin().is_tuple() {
        return format!("<{} ресурс>", url.scheme());
    }
    let raw_path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    let mut path = raw_path
        .chars()
        .take(MAX_NETWORK_DIAGNOSTIC_PATH_CHARS)
        .collect::<String>();
    if raw_path.chars().count() > MAX_NETWORK_DIAGNOSTIC_PATH_CHARS {
        path.push('…');
    }
    format!("{}{path}", url.origin().ascii_serialization())
}

fn sanitized_network_failure_reason(raw_reason: &str) -> String {
    if raw_reason.len() > 128 {
        return "не классифицированная сетевая ошибка".into();
    }
    let upper = raw_reason.trim().to_ascii_uppercase();
    let Some(suffix) = upper.strip_prefix("NET::ERR_") else {
        return "не классифицированная сетевая ошибка".into();
    };
    if suffix.is_empty()
        || !suffix
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return "не классифицированная сетевая ошибка".into();
    }
    format!("net::ERR_{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    #[test]
    fn borrowed_profile_drop_removes_only_its_child() {
        let parent = TempWorkspace::create("borrowed-browser-profile-test").unwrap();
        let sentinel = parent.path().join(".runtime");
        fs::create_dir(&sentinel).unwrap();
        fs::write(sentinel.join("keep"), "sentinel").unwrap();
        let path;
        let temp_path;
        {
            let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
            path = profile.path.clone();
            temp_path = profile.temp_path.clone();
            fs::write(path.join("data"), "browser").unwrap();
            fs::write(temp_path.join("data"), "временные данные браузера").unwrap();
        }
        assert!(!path.exists());
        assert!(!temp_path.exists());
        assert_eq!(
            fs::read_to_string(sentinel.join("keep")).unwrap(),
            "sentinel"
        );
        parent.close().unwrap();
    }

    #[test]
    fn standalone_profile_close_removes_its_owned_workspace() {
        let profile = BrowserProfile::standalone().unwrap();
        let root = profile.workspace.as_ref().unwrap().path().to_path_buf();
        fs::write(profile.path.join("data"), "browser").unwrap();
        fs::write(profile.temp_path.join("data"), "временные данные браузера").unwrap();
        let profile_path = profile.path.clone();
        let temp_path = profile.temp_path.clone();
        profile.close().unwrap();
        assert!(!root.exists());
        assert!(!profile_path.exists());
        assert!(!temp_path.exists());
    }

    #[test]
    fn standalone_profile_is_cleaned_after_early_error() {
        fn failing_setup(observed_root: &mut PathBuf) -> Result<(), BrowserLaunchError> {
            let profile = BrowserProfile::standalone()?;
            *observed_root = profile.workspace.as_ref().unwrap().path().to_path_buf();
            fs::write(profile.path.join("data"), "browser")?;
            fs::write(profile.temp_path.join("data"), "временные данные браузера")?;
            Err(io::Error::other("искусственно вызванный сбой настройки").into())
        }
        let mut root = PathBuf::new();
        assert!(failing_setup(&mut root).is_err());
        assert!(!root.exists());
    }

    #[test]
    fn cleanup_runs_and_removes_browser_tree_after_kill_fallback_error() {
        let parent = TempWorkspace::create("browser-kill-fallback-cleanup-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        let profile_path = profile.path.clone();
        let temp_path = profile.temp_path.clone();
        fs::write(profile_path.join("profile-data"), b"profile").unwrap();
        fs::write(temp_path.join("temp-data"), b"temp").unwrap();

        let stop = Err("browser_close_failed: потребовалось принудительное завершение".into());
        let cleanup = profile
            .close()
            .map_err(|error| format!("browser_profile_cleanup_failed: {error}"));
        let result = combine_browser_close_results(stop, cleanup);
        assert!(result.is_err());
        assert!(!profile_path.exists());
        assert!(!temp_path.exists());
        parent.close().unwrap();
    }

    #[test]
    fn profile_cleanup_failure_is_returned_without_deleting_borrowed_parent() {
        let parent = TempWorkspace::create("browser-profile-error-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        fs::remove_dir(&profile.path).unwrap();
        fs::write(
            &profile.path,
            "искусственно вызванная ошибка файловой системы",
        )
        .unwrap();
        assert!(profile.close().is_err());
        assert!(parent.path().is_dir());
        parent.close().unwrap();
    }

    #[test]
    fn simultaneous_profiles_under_one_parent_do_not_conflict() {
        let parent = TempWorkspace::create("browser-profile-parallel-test").unwrap();
        std::thread::scope(|scope| {
            let first = scope.spawn(|| BrowserProfile::in_workspace(parent.path()).unwrap());
            let second = scope.spawn(|| BrowserProfile::in_workspace(parent.path()).unwrap());
            let first = first.join().unwrap();
            let second = second.join().unwrap();
            assert_ne!(first.path, second.path);
            assert_ne!(first.temp_path, second.temp_path);
            assert_eq!(first.path.parent(), first.temp_path.parent());
            assert_eq!(second.path.parent(), second.temp_path.parent());
            for profile in [&first, &second] {
                let socket_path = profile
                    .temp_path
                    .join("org.chromium.Chromium.XXXXXX")
                    .join("SingletonSocket");
                assert!(socket_path.as_os_str().as_bytes().len() < 108);
            }
            first.close().unwrap();
            second.close().unwrap();
        });
        parent.close().unwrap();
    }

    #[test]
    fn browser_config_scopes_tmpdir_to_profile_owner_without_global_change() {
        let parent = TempWorkspace::create("browser-scoped-tmpdir-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        let before = std::env::var_os("TMPDIR");
        let executable = BrowserExecutableSelection {
            path: PathBuf::from("/bin/true"),
            source: BrowserExecutableSource::PathLookup,
        };
        let config =
            build_browser_config(&profile, &BrowserRuntimeConfig::default(), Some(executable))
                .unwrap();
        assert_eq!(
            config.user_data_dir.as_deref(),
            Some(profile.path.as_path())
        );
        assert_eq!(
            config
                .process_envs
                .as_ref()
                .and_then(|environment| environment.get("TMPDIR"))
                .map(String::as_str),
            profile.temp_path.to_str()
        );
        assert!(profile.temp_path.starts_with(parent.path()));
        assert_eq!(std::env::var_os("TMPDIR"), before);
        assert!(profile.temp_path.as_os_str().as_bytes().len() < 108);
        profile.close().unwrap();
        parent.close().unwrap();
    }

    #[test]
    fn browser_config_preserves_unicode_tmpdir_and_rejects_non_utf8_before_launch() {
        let parent = TempWorkspace::create("browser-tmpdir-encoding-test").unwrap();
        for name in ["рабочее".as_bytes(), b"invalid-\xff".as_slice()] {
            let borrowed = parent.path().join(std::ffi::OsStr::from_bytes(name));
            fs::create_dir(&borrowed).unwrap();
            fs::write(borrowed.join("keep"), b"borrowed").unwrap();
            let profile = BrowserProfile::in_workspace(&borrowed).unwrap();
            let profile_path = profile.path.clone();
            let temp_path = profile.temp_path.clone();
            let executable = BrowserExecutableSelection {
                path: PathBuf::from("/bin/true"),
                source: BrowserExecutableSource::PathLookup,
            };
            let result =
                build_browser_config(&profile, &BrowserRuntimeConfig::default(), Some(executable));
            if let Some(expected) = temp_path.to_str() {
                let config = result.unwrap();
                assert_eq!(
                    config.process_envs.unwrap().get("TMPDIR").unwrap(),
                    expected
                );
                profile.close().unwrap();
            } else {
                let message = result.unwrap_err();
                assert_eq!(
                    message,
                    "browser_tmpdir_invalid: путь TMPDIR должен быть UTF-8"
                );
                assert!(matches!(
                    profile_setup_failure(profile, message),
                    BrowserLaunchError::Setup(_)
                ));
            }
            assert!(!profile_path.exists());
            assert!(!temp_path.exists());
            assert_eq!(fs::read(borrowed.join("keep")).unwrap(), b"borrowed");
        }
        parent.close().unwrap();
    }

    #[test]
    fn browser_tmpdir_is_relative_when_the_configured_root_is_the_process_directory() {
        let parent = TempWorkspace::create("browser-relative-tmpdir-parent-test").unwrap();
        let worker = TempWorkspace::create_under(parent.path(), "worker").unwrap();
        let temp_path = worker.path().join("t00");
        fs::create_dir(&temp_path).unwrap();

        let relative = browser_tmpdir_value(&temp_path, Some(parent.path()), Some(parent.path()));
        assert!(relative.is_relative());
        let socket_path = relative
            .join("org.chromium.Chromium.XXXXXX")
            .join("SingletonSocket");
        assert!(
            socket_path.as_os_str().as_bytes().len() < 108,
            "путь Unix-сокета занимает {} байт: {}",
            socket_path.as_os_str().as_bytes().len(),
            socket_path.display()
        );

        worker.close().unwrap();
        parent.close().unwrap();
    }

    #[tokio::test]
    async fn invalid_browser_setup_creates_no_profile_in_borrowed_workspace() {
        let parent = TempWorkspace::create("browser-invalid-setup-test").unwrap();
        let before = fs::read_dir(parent.path()).unwrap().count();
        let config = BrowserRuntimeConfig {
            launch_timeout: Duration::ZERO,
            ..Default::default()
        };
        assert!(
            BrowserSession::launch_in_workspace(config, parent.path())
                .await
                .is_err()
        );
        assert_eq!(fs::read_dir(parent.path()).unwrap().count(), before);
        parent.close().unwrap();
    }

    #[tokio::test]
    async fn launch_panic_drops_child_before_observing_process_stop_and_removing_profile() {
        struct LaunchChild(Option<futures::channel::oneshot::Sender<()>>);
        impl Drop for LaunchChild {
            fn drop(&mut self) {
                if let Some(stopped) = self.0.take() {
                    let _ = stopped.send(());
                }
            }
        }

        let parent = TempWorkspace::create("browser-launch-panic-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        let profile_path = profile.path.clone();
        let (kill_tx, kill_rx) = futures::channel::oneshot::channel();
        let launched = catch_browser_setup(async {
            let _child = LaunchChild(Some(kill_tx));
            tokio::task::yield_now().await;
            panic!("искусственная паника до получения владения браузером");
            #[allow(unreachable_code)]
            Ok::<(), String>(())
        })
        .await;
        // Аналог `kill_on_drop` уже завершился до проверки обращений к временному дереву.
        kill_rx.now_or_never().unwrap().unwrap();
        let failure = match launched {
            Err(failure) => failure,
            Ok(_) => panic!("паника при запуске должна вернуть ошибку"),
        };
        assert_eq!(
            failure.message_with_panic_code("browser_launch_panicked"),
            "browser_launch_panicked: искусственная паника до получения владения браузером"
        );
        let (checked_tx, checked_rx) = futures::channel::oneshot::channel();
        let (released_tx, mut released_rx) = futures::channel::oneshot::channel();
        let mut first_check = Some(checked_tx);
        let mut checks = 0;
        let waiting = wait_for_workspace_processes_with(
            || {
                checks += 1;
                if let Some(checked) = first_check.take() {
                    checked.send(()).unwrap();
                }
                Ok(released_rx.try_recv().unwrap().is_none())
            },
            Duration::from_secs(1),
            Duration::ZERO,
        );
        let stop = async {
            checked_rx.await.unwrap();
            // Запрос на завершение не означает, что процесс уже перестал обращаться к дереву.
            assert!(profile_path.is_dir());
            released_tx.send(()).unwrap();
        };
        let (observed, ()) = futures::join!(waiting, stop);
        observed.unwrap();
        assert!(checks >= 2);
        profile.close().unwrap();
        assert!(!profile_path.exists());
        parent.close().unwrap();
    }

    #[tokio::test]
    async fn process_stop_observation_reports_live_references_and_probe_errors_at_deadline() {
        let live = wait_for_workspace_processes_with(|| Ok(true), Duration::ZERO, Duration::ZERO)
            .await
            .unwrap_err();
        assert!(
            live.to_string()
                .contains("всё ещё использует временное дерево")
        );
        let failed = wait_for_workspace_processes_with(
            || {
                Err(io::Error::other(
                    "искусственно вызванный сбой проверки процесса",
                ))
            },
            Duration::ZERO,
            Duration::ZERO,
        )
        .await
        .unwrap_err();
        assert_eq!(
            failed.to_string(),
            "искусственно вызванный сбой проверки процесса"
        );
    }

    #[test]
    fn profile_creation_failure_preserves_cleanup_classification() {
        let parent = TempWorkspace::create("browser-profile-creation-failure-test").unwrap();
        let path = parent.path().join("owned-profile");
        fs::create_dir(&path).unwrap();
        let recoverable = profile_creation_failure(io::Error::other("сбой создания"), &[&path]);
        assert!(matches!(recoverable, BrowserLaunchError::Setup(_)));
        assert!(!path.exists());
        fs::write(&path, "каталог подменён файлом").unwrap();
        let fatal = profile_creation_failure(io::Error::other("сбой создания"), &[&path]);
        assert!(matches!(fatal, BrowserLaunchError::CleanupFailed { .. }));
        assert!(path.exists());
        parent.close().unwrap();
    }

    #[tokio::test]
    async fn launch_and_setup_errors_preserve_cleanup_classification() {
        for stage in ["config", "launch", "setup"] {
            for fail_cleanup in [false, true] {
                let parent = TempWorkspace::create("browser-launch-setup-failure-test").unwrap();
                let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
                let path = profile.path.clone();
                if fail_cleanup {
                    fs::remove_dir(&path).unwrap();
                    fs::write(&path, "каталог подменён файлом").unwrap();
                }
                let failure = BrowserSetupFailure::Error("искусственный сбой настройки".into());
                let error = if stage == "setup" {
                    let resources = BrowserResources {
                        diagnostic_session_id: 0,
                        browser: None,
                        handler_task: None,
                        profile: Some(profile),
                    };
                    match resources.finish_setup::<()>(Err(failure)).await {
                        Err(error) => error,
                        Ok(_) => panic!("ошибка настройки должна сохраниться"),
                    }
                } else if stage == "launch" {
                    profile_launch_failure(profile, failure).await
                } else {
                    profile_setup_failure(profile, "искусственный сбой настройки".into())
                };
                assert_eq!(
                    matches!(error, BrowserLaunchError::CleanupFailed { .. }),
                    fail_cleanup
                );
                assert_eq!(matches!(error, BrowserLaunchError::Setup(_)), !fail_cleanup);
                assert_eq!(path.exists(), fail_cleanup);
                parent.close().unwrap();
            }
        }
    }

    #[tokio::test]
    async fn launch_panic_failure_waits_for_profile_cleanup_and_keeps_its_error() {
        let parent = TempWorkspace::create("browser-launch-panic-cleanup-error-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        let temp_path = profile.temp_path.clone();
        fs::remove_dir(&profile.path).unwrap();
        fs::write(&profile.path, "искусственно вызванный сбой очистки").unwrap();
        let error = profile_launch_failure(
            profile,
            BrowserSetupFailure::Panic(Box::new("искусственная паника при запуске")),
        )
        .await;
        assert!(matches!(error, BrowserLaunchError::CleanupFailed { .. }));
        assert!(
            error
                .to_string()
                .starts_with("browser_launch_panicked: искусственная паника при запуске;")
        );
        assert!(
            error
                .to_string()
                .contains("browser_profile_cleanup_failed:")
        );
        assert!(!temp_path.exists());
        parent.close().unwrap();
    }

    #[tokio::test]
    async fn setup_panic_waits_for_handler_and_profile_cleanup_before_returning() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct HandlerStopped(Arc<AtomicBool>);
        impl Drop for HandlerStopped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let parent = TempWorkspace::create("browser-setup-panic-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        let profile_path = profile.path.clone();
        let temp_path = profile.temp_path.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&stopped);
        let (started_tx, started_rx) = futures::channel::oneshot::channel();
        let handler = tokio::spawn(async move {
            let _guard = HandlerStopped(observed);
            started_tx.send(()).unwrap();
            futures::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        let resources = BrowserResources {
            diagnostic_session_id: 0,
            browser: None,
            handler_task: Some(handler),
            profile: Some(profile),
        };
        let setup = catch_browser_setup(async {
            // Паника возникает при очередном `poll` после приостановки `Future`,
            // как и во время настройки CDP.
            tokio::task::yield_now().await;
            panic!("искусственная паника при настройке браузера");
            #[allow(unreachable_code)]
            Ok::<(), String>(())
        })
        .await;
        assert!(matches!(&setup, Err(BrowserSetupFailure::Panic(_))));
        let error = match resources.finish_setup(setup).await {
            Err(error) => error,
            Ok(_) => panic!("паника при настройке должна вернуть ошибку"),
        };
        assert!(matches!(error, BrowserLaunchError::Panic(_)));
        assert_eq!(
            error.to_string(),
            "browser_setup_panicked: искусственная паника при настройке браузера"
        );
        // Эти утверждения выполняются до передачи управления: фоновая очистка из `Drop` их не заменит.
        assert!(stopped.load(Ordering::SeqCst));
        assert!(!profile_path.exists());
        assert!(!temp_path.exists());
        assert!(parent.path().is_dir());
        parent.close().unwrap();
    }

    #[tokio::test]
    async fn setup_panic_keeps_original_failure_and_cleanup_error_with_safe_diagnostics() {
        let parent = TempWorkspace::create("browser-setup-panic-cleanup-error-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        let temp_path = profile.temp_path.clone();
        fs::remove_dir(&profile.path).unwrap();
        fs::write(&profile.path, "искусственно вызванный сбой очистки").unwrap();
        let resources = BrowserResources {
            diagnostic_session_id: 0,
            browser: None,
            handler_task: None,
            profile: Some(profile),
        };
        let setup = catch_browser_setup(async {
            std::panic::panic_any(String::from(
                "искусственная паника настройки token=secret-value",
            ));
            #[allow(unreachable_code)]
            Ok::<(), String>(())
        })
        .await;
        let error = match resources.finish_setup(setup).await {
            Err(error) => error,
            Ok(_) => panic!("паника при настройке должна вернуть ошибку"),
        };
        assert!(matches!(error, BrowserLaunchError::CleanupFailed { .. }));
        assert!(
            error
                .to_string()
                .contains("browser_setup_panicked: искусственная паника настройки")
        );
        assert!(
            error
                .to_string()
                .contains("browser_profile_cleanup_failed:")
        );
        assert!(!error.to_string().contains("secret-value"));
        assert!(!temp_path.exists());
        assert!(parent.path().is_dir());
        parent.close().unwrap();
    }

    #[tokio::test]
    async fn setup_panic_with_nontext_payload_has_stable_failure_code() {
        let setup = catch_browser_setup(async {
            std::panic::panic_any(17_u8);
            #[allow(unreachable_code)]
            Ok::<(), String>(())
        })
        .await;
        let failure = match setup {
            Err(failure) => failure,
            Ok(_) => panic!("паника при настройке должна вернуть ошибку"),
        };
        assert_eq!(
            failure.message_with_panic_code("browser_setup_panicked"),
            "browser_setup_panicked: паника с нетекстовым содержимым"
        );
    }

    fn outcome(
        resource_type: ResourceType,
        url: Option<&str>,
        reason: Option<&str>,
        request_id: &str,
        epoch: u64,
        status: Option<u16>,
    ) -> NetworkOutcome {
        NetworkOutcome {
            resource_type,
            url: url.map(str::to_owned),
            failure_reason: reason.map(str::to_owned),
            request_id: request_id.to_owned(),
            epoch,
            status_code: status,
            is_top_level: false,
        }
    }

    #[test]
    fn provenance_serialization_keeps_runtime_and_selection_without_local_paths() {
        let provenance = BrowserRuntimeProvenance {
            product: "Chrome/140.0.0.0".into(),
            protocol_version: "1.3".into(),
            revision: "abc123".into(),
            user_agent: "Mozilla/5.0 Chrome/140".into(),
            js_version: "14.0".into(),
            executable_source: BrowserExecutableSource::PathLookup,
        };
        let encoded = serde_json::to_value(&provenance).unwrap();
        assert_eq!(encoded["executable_source"], "path_lookup");
        assert_eq!(encoded["product"], "Chrome/140.0.0.0");
        assert_eq!(encoded.as_object().unwrap().len(), 6);
        let text = encoded.to_string();
        assert!(!text.contains("/home/"));
        assert!(!text.contains("C:\\"));
    }

    #[test]
    fn device_metrics_accept_scale_three_without_baking_in_provider_scale() {
        let metrics = DeviceMetrics::new(1280, 900, 3.0).unwrap();
        let config = BrowserRuntimeConfig {
            device_metrics: Some(metrics),
            ..BrowserRuntimeConfig::default()
        };
        config.validate().unwrap();
        assert_eq!(config.device_metrics.unwrap().device_scale_factor, 3.0);
        let params = serde_json::to_value(metrics.cdp_params()).unwrap();
        assert_eq!(params["deviceScaleFactor"].as_f64(), Some(3.0));
        assert!(DeviceMetrics::new(0, 900, 3.0).is_err());
        assert!(DeviceMetrics::new(1280, 900, f64::NAN).is_err());
    }

    #[test]
    fn raw_snapshot_tracks_pending_failures_http_javascript_and_epoch_scope() {
        let mut state = RuntimeState {
            main_frame_id: "main".into(),
            ..RuntimeState::default()
        };
        let epoch = state.begin_epoch();
        state.record_request(
            "pending".into(),
            ResourceType::Fetch,
            "https://example.test/pending".into(),
            Some("main"),
        );
        state.record_request(
            "failed".into(),
            ResourceType::Script,
            "https://example.test/script.js".into(),
            Some("main"),
        );
        state.record_network_failure(
            "failed".into(),
            ResourceType::Script,
            "net::ERR_CONNECTION_RESET".into(),
        );
        state.record_http_error(
            "pending".into(),
            ResourceType::Fetch,
            "https://example.test/pending".into(),
            503,
        );
        state.record_javascript_exception();

        let snapshot = state.snapshot(epoch);
        assert_eq!(snapshot.pending_requests.len(), 1);
        assert_eq!(snapshot.pending_requests[0].request_id, "pending");
        assert!(snapshot.pending_requests[0].is_top_level);
        assert_eq!(snapshot.network_failures.len(), 1);
        assert_eq!(snapshot.http_errors[0].status_code, Some(503));
        assert_eq!(snapshot.javascript_exceptions, 1);
        assert!(state.snapshot(epoch + 1).pending_requests.is_empty());
        assert_eq!(state.snapshot(epoch + 1).javascript_exceptions, 0);
    }

    #[test]
    fn telemetry_overflow_fails_closed() {
        let mut state = RuntimeState::default();
        for index in 0..=MAX_TRACKED_REQUESTS {
            state.record_request(
                format!("request-{index}"),
                ResourceType::Fetch,
                format!("https://example.test/{index}"),
                None,
            );
        }
        assert!(state.monitor_failed);
        assert_eq!(state.relevant_requests.len(), MAX_TRACKED_REQUESTS);

        for index in 0..=MAX_NETWORK_OUTCOMES {
            state.record_network_failure(
                format!("failed-{index}"),
                ResourceType::Fetch,
                "net::ERR_FAILED".into(),
            );
        }
        assert!(state.monitor_failed);
        assert_eq!(state.network_failures.len(), MAX_NETWORK_OUTCOMES);
    }

    #[test]
    fn retry_classification_rejects_javascript_and_permanent_failures() {
        assert!(is_retryable_network_error("net::ERR_TIMED_OUT"));
        assert!(is_retryable_network_error("net::ERR_ABORTED"));
        assert!(!is_retryable_network_error("net::ERR_CERT_DATE_INVALID"));
        assert!(!is_retryable_network_error("details: ERR_CONNECTION_RESET"));
        assert!(is_retryable_http_status(429));
        assert!(is_retryable_http_status(503));
        assert!(!is_retryable_http_status(404));

        let mut state = RuntimeState::default();
        let epoch = state.begin_epoch();
        state.network_failures.push(outcome(
            ResourceType::Fetch,
            Some("https://example.test/retry"),
            Some("net::ERR_CONNECTION_RESET"),
            "retryable",
            epoch,
            None,
        ));
        let monitor = CdpRuntimeMonitor {
            state: Arc::new(Mutex::new(state)),
            tasks: Arc::new(Vec::new()),
        };
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::RuntimeFailure));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ReadinessTimeout));

        let mut state = lock_state(&monitor.state);
        state.record_javascript_exception();
        drop(state);
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ReadinessTimeout));

        lock_state(&monitor.state).http_errors.push(outcome(
            ResourceType::Fetch,
            Some("https://example.test/permanent"),
            None,
            "permanent",
            epoch,
            Some(404),
        ));
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::RuntimeFailure));
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
    }

    #[test]
    fn retry_classification_rejects_bootstrap_failure_except_readiness_timeout() {
        let mut state = RuntimeState::default();
        let epoch = state.begin_epoch();
        state.network_failures.push(outcome(
            ResourceType::Fetch,
            Some("https://example.test/bootstrap"),
            Some("net::ERR_CONNECTION_RESET"),
            "bootstrap",
            0,
            None,
        ));
        let monitor = CdpRuntimeMonitor {
            state: Arc::new(Mutex::new(state)),
            tasks: Arc::new(Vec::new()),
        };

        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::RuntimeFailure));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ReadinessTimeout));
    }

    #[test]
    fn retry_classification_preserves_timeout_without_failures_and_rejects_failed_monitor() {
        let mut state = RuntimeState::default();
        let epoch = state.begin_epoch();
        let monitor = CdpRuntimeMonitor {
            state: Arc::new(Mutex::new(state)),
            tasks: Arc::new(Vec::new()),
        };
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ItemTimeout));
        assert!(!monitor.retryable_acquisition(epoch, RetryTrigger::RuntimeFailure));
        assert!(monitor.retryable_acquisition(epoch, RetryTrigger::ReadinessTimeout));

        lock_state(&monitor.state).monitor_failed = true;
        for trigger in [
            RetryTrigger::ItemTimeout,
            RetryTrigger::RuntimeFailure,
            RetryTrigger::ReadinessTimeout,
        ] {
            assert!(!monitor.retryable_acquisition(epoch, trigger));
        }
    }

    #[test]
    fn network_diagnostics_redact_credentials_query_fragment_and_unknown_error_text() {
        let details = format_network_failure_details(&[
            outcome(
                ResourceType::Script,
                Some("https://user:password@example.test/a.js?token=secret#fragment"),
                Some("net::ERR_CONNECTION_RESET"),
                "one",
                1,
                None,
            ),
            outcome(
                ResourceType::Fetch,
                Some("https://example.test/private?secret=hidden"),
                Some("сведения о соединении содержат секрет"),
                "two",
                1,
                None,
            ),
        ]);
        assert!(details.contains("https://example.test/a.js net::ERR_CONNECTION_RESET"));
        assert!(details.contains("не классифицированная сетевая ошибка"));
        for secret in ["user", "password", "token", "secret", "hidden", "fragment"] {
            assert!(!details.contains(secret));
        }
    }
    #[test]
    fn detached_transport_needs_process_and_directory_postconditions() {
        let stopped = BrowserStopObservation {
            transport: transport_stop_result(Err(chromiumoxide::error::CdpError::NoResponse)),
            process_stopped: true,
            process_failure: None,
        };
        assert_eq!(stopped.transport, BrowserTransportStop::Detached);
        assert!(classify_browser_cleanup(stopped.clone(), Ok(())).is_ok());
        let directory_error = "browser_profile_cleanup_failed: удаление не удалось".to_owned();
        assert_eq!(
            classify_browser_cleanup(stopped.clone(), Err(directory_error.clone())),
            Err(directory_error),
        );
        assert_eq!(
            classify_browser_cleanup(
                BrowserStopObservation {
                    process_stopped: false,
                    ..stopped
                },
                Ok(())
            ),
            Err("browser_process_stop_unproven".into())
        );
        // Похожий текст в произвольной ошибке команды CDP не означает отсоединённый канал.
        let arbitrary =
            transport_stop_result(Err(chromiumoxide::error::CdpError::msg("receiver is gone")));
        assert!(matches!(arbitrary, BrowserTransportStop::Failed(_)));
        assert!(
            classify_browser_cleanup(
                BrowserStopObservation {
                    transport: arbitrary,
                    process_stopped: true,
                    process_failure: None,
                },
                Ok(())
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn cancelled_response_channel_is_typed_detached_without_message_matching() {
        let (sender, receiver) = futures::channel::oneshot::channel::<()>();
        drop(sender);
        let error = chromiumoxide::error::CdpError::from(receiver.await.unwrap_err());
        assert_eq!(
            transport_stop_result(Err(error)),
            BrowserTransportStop::Detached
        );
    }

    #[test]
    fn detached_transport_with_actual_profile_cleanup_failure_stays_fatal() {
        let parent = TempWorkspace::create("browser-detached-real-cleanup-error-test").unwrap();
        let profile = BrowserProfile::in_workspace(parent.path()).unwrap();
        let temp_path = profile.temp_path.clone();
        fs::remove_dir(&profile.path).unwrap();
        fs::write(&profile.path, "каталог заменён файлом").unwrap();
        let cleanup = profile
            .close()
            .map_err(|error| format!("browser_profile_cleanup_failed: {error}"));
        let error = classify_browser_cleanup(
            BrowserStopObservation {
                transport: BrowserTransportStop::Detached,
                process_stopped: true,
                process_failure: None,
            },
            cleanup,
        )
        .unwrap_err();
        assert!(error.starts_with("browser_profile_cleanup_failed:"));
        assert!(!temp_path.exists());
        parent.close().unwrap();
    }

    #[test]
    fn pending_observation_preserves_request_start_across_redirects_and_filters_epochs() {
        let mut state = RuntimeState::default();
        let bootstrap_start = Instant::now();
        state.record_request(
            "bootstrap".into(),
            ResourceType::Script,
            "https://example.test/bootstrap".into(),
            None,
        );
        state
            .request_observed_at
            .insert("bootstrap".into(), bootstrap_start);
        let first_epoch = state.begin_epoch();
        state.record_request(
            "stale".into(),
            ResourceType::Image,
            "https://example.test/old".into(),
            None,
        );
        state
            .request_observed_at
            .insert("stale".into(), bootstrap_start);
        let current_epoch = state.begin_epoch();
        state.record_request(
            "current".into(),
            ResourceType::Fetch,
            "https://example.test/request".into(),
            None,
        );
        let started = bootstrap_start + Duration::from_millis(10);
        state.request_observed_at.insert("current".into(), started);
        state.record_request(
            "current".into(),
            ResourceType::Fetch,
            "https://example.test/redirect".into(),
            None,
        );
        let snapshot =
            state.snapshot_at(current_epoch, bootstrap_start + Duration::from_millis(45));
        assert_eq!(snapshot.epoch, current_epoch);
        assert_eq!(snapshot.pending_requests.len(), 2);
        assert!(
            snapshot
                .pending_requests
                .iter()
                .all(|request| request.epoch != first_epoch)
        );
        let diagnostic = snapshot.diagnostic();
        assert_eq!(diagnostic.stale_pending_request_count, 1);
        assert_eq!(diagnostic.stale_pending_requests[0].observed_ms, Some(45));
        assert_eq!(diagnostic.stale_pending_requests[0].epoch, first_epoch);
        let current = diagnostic
            .pending_requests
            .iter()
            .find(|request| request.in_current_epoch)
            .unwrap();
        assert_eq!(current.observed_ms, Some(35));
        let bootstrap = diagnostic
            .pending_requests
            .iter()
            .find(|request| !request.in_current_epoch)
            .unwrap();
        assert_eq!(bootstrap.epoch, 0);
        assert_eq!(bootstrap.observed_ms, Some(45));
        assert_eq!(
            state
                .snapshot_at(current_epoch, bootstrap_start)
                .diagnostic()
                .pending_requests
                .iter()
                .find(|request| request.in_current_epoch)
                .unwrap()
                .observed_ms,
            Some(0)
        );
        state.record_network_failure(
            "current".into(),
            ResourceType::Fetch,
            "net::ERR_TIMED_OUT".into(),
        );
        assert!(!state.request_observed_at.contains_key("current"));
    }

    #[test]
    fn safe_snapshot_bounds_all_collections_and_excludes_raw_data() {
        let mut snapshot = RuntimeSnapshot {
            epoch: 2,
            ..Default::default()
        };
        for index in 0..(MAX_RUNTIME_DIAGNOSTIC_ITEMS + 8) {
            let id = format!("private-request-id-{index}");
            snapshot.pending_requests.push(TrackedRequest {
                request_id: id.clone(),
                resource_type: ResourceType::Fetch,
                url: "https://user:password@example.test/private?token=secret#fragment".into(),
                epoch: 2,
                is_top_level: true,
            });
            snapshot.network_failures.push(outcome(
                ResourceType::Fetch,
                Some("https://example.test/private?authorization=hidden"),
                Some("сырые куки и тело содержат секрет"),
                &id,
                2,
                None,
            ));
            snapshot.http_errors.push(outcome(
                ResourceType::Fetch,
                Some("https://example.test/response-body"),
                None,
                &id,
                2,
                Some(503),
            ));
        }
        for index in 0..(MAX_RUNTIME_DIAGNOSTIC_ITEMS + 8) {
            let id = format!("stale-private-{index}");
            snapshot.stale_pending_requests.push(TrackedRequest {
                request_id: id.clone(),
                resource_type: ResourceType::Image,
                url: "https://example.test/stale?secret=value".into(),
                epoch: 1,
                is_top_level: false,
            });
            snapshot.stale_network_failures.push(outcome(
                ResourceType::Fetch,
                Some("https://example.test/stale?secret=value"),
                Some(&format!("net::ERR_{}", "X".repeat(119))),
                &id,
                1,
                None,
            ));
            snapshot.stale_http_errors.push(outcome(
                ResourceType::Fetch,
                Some("https://example.test/stale?secret=value"),
                None,
                &id,
                1,
                Some(503),
            ));
        }
        snapshot.pending_requests.push(TrackedRequest {
            request_id: "stale-request".into(),
            resource_type: ResourceType::Image,
            url: "https://example.test/stale".into(),
            epoch: 1,
            is_top_level: false,
        });
        let diagnostic = snapshot.diagnostic();
        assert_eq!(
            diagnostic.pending_request_count,
            MAX_RUNTIME_DIAGNOSTIC_ITEMS + 8
        );
        assert_eq!(
            diagnostic.network_failure_count,
            MAX_RUNTIME_DIAGNOSTIC_ITEMS + 8
        );
        assert_eq!(
            diagnostic.http_error_count,
            MAX_RUNTIME_DIAGNOSTIC_ITEMS + 8
        );
        assert_eq!(
            diagnostic.pending_requests.len(),
            MAX_RUNTIME_DIAGNOSTIC_ITEMS
        );
        assert_eq!(
            diagnostic.network_failures.len(),
            MAX_RUNTIME_DIAGNOSTIC_ITEMS
        );
        assert_eq!(diagnostic.http_errors.len(), MAX_RUNTIME_DIAGNOSTIC_ITEMS);
        assert_eq!(
            diagnostic.stale_pending_request_count,
            MAX_RUNTIME_DIAGNOSTIC_ITEMS + 9
        );
        assert_eq!(
            diagnostic.stale_network_failure_count,
            MAX_RUNTIME_DIAGNOSTIC_ITEMS + 8
        );
        assert_eq!(
            diagnostic.stale_http_error_count,
            MAX_RUNTIME_DIAGNOSTIC_ITEMS + 8
        );
        assert_eq!(
            diagnostic.stale_pending_requests.len(),
            MAX_RUNTIME_DIAGNOSTIC_ITEMS
        );
        assert_eq!(
            diagnostic.stale_network_failures.len(),
            MAX_RUNTIME_DIAGNOSTIC_ITEMS
        );
        assert_eq!(
            diagnostic.stale_http_errors.len(),
            MAX_RUNTIME_DIAGNOSTIC_ITEMS
        );
        assert!(
            diagnostic
                .stale_pending_requests
                .iter()
                .all(|request| !request.in_current_epoch)
        );
        assert!(
            diagnostic
                .pending_requests
                .iter()
                .all(|request| request.observed_ms.is_none())
        );
        let encoded = serde_json::to_string(&diagnostic).unwrap();
        assert!(encoded.len() < 32 * 1024);
        for raw in [
            "https://",
            "example.test",
            "private-request-id",
            "password",
            "secret",
            "fragment",
            "authorization",
            "cookies",
            "response-body",
            "stale-request",
        ] {
            assert!(
                !encoded.contains(raw),
                "сырые данные попали в диагностику: {raw}"
            );
        }
    }

    #[test]
    fn runtime_state_bounds_observation_strings_and_javascript_epochs() {
        let mut state = RuntimeState::default();
        for index in 0..=MAX_TRACKED_REQUESTS {
            state.record_request(
                index.to_string(),
                ResourceType::Fetch,
                "https://example.test/".into(),
                None,
            );
        }
        assert_eq!(state.request_observed_at.len(), MAX_TRACKED_REQUESTS);
        state.record_request(
            "oversized-url".into(),
            ResourceType::Fetch,
            "x".repeat(MAX_RUNTIME_URL_BYTES + 1),
            None,
        );
        assert!(!state.relevant_requests.contains_key("oversized-url"));
        state.record_network_failure(
            "long-reason".into(),
            ResourceType::Fetch,
            "x".repeat(MAX_RUNTIME_REASON_BYTES + 1),
        );
        assert!(state.network_failures.is_empty());
        for _ in 0..=MAX_NETWORK_OUTCOMES {
            state.begin_epoch();
            state.record_javascript_exception();
        }
        assert_eq!(state.javascript_exceptions.len(), MAX_NETWORK_OUTCOMES);
        assert!(state.monitor_failed);
    }

    #[test]
    fn completion_handled_before_its_request_does_not_leave_pending_request() {
        // Ответ из memory cache: `requestWillBeSent` и `loadingFinished` приходят почти
        // одновременно, а их независимые слушатели могут обработать их в обратном порядке.
        let mut state = RuntimeState {
            main_frame_id: "main".into(),
            ..RuntimeState::default()
        };
        let epoch = state.begin_epoch();
        state.record_finished("cached-style");
        state.record_request(
            "cached-style".into(),
            ResourceType::Stylesheet,
            "https://jpdb.io/static/site.css".into(),
            Some("main"),
        );
        // Шаг перенаправления после завершения тоже не воскрешает запрос.
        state.record_request(
            "cached-style".into(),
            ResourceType::Stylesheet,
            "https://jpdb.io/static/site-redirected.css".into(),
            Some("main"),
        );
        let snapshot = state.snapshot(epoch);
        assert!(snapshot.pending_requests.is_empty());
        assert!(snapshot.stale_pending_requests.is_empty());
        assert!(state.request_observed_at.is_empty());
        assert!(!snapshot.monitor_failed);

        // Обычный порядок по-прежнему отслеживает незавершённый запрос.
        state.record_request(
            "live-script".into(),
            ResourceType::Script,
            "https://jpdb.io/static/app.js".into(),
            Some("main"),
        );
        assert_eq!(state.snapshot(epoch).pending_requests.len(), 1);
        state.record_finished("live-script");
        assert!(state.snapshot(epoch).pending_requests.is_empty());
    }

    #[test]
    fn repeated_cached_navigations_do_not_exhaust_request_tracking() {
        let mut state = RuntimeState {
            main_frame_id: "main".into(),
            ..RuntimeState::default()
        };
        // Больше навигаций, чем помещается в MAX_TRACKED_REQUESTS при прежней утечке.
        for navigation in 0..MAX_TRACKED_REQUESTS {
            let epoch = state.begin_epoch();
            for resource in 0..8 {
                let id = format!("{navigation}.{resource}");
                state.record_finished(&id);
                state.record_request(
                    id,
                    ResourceType::Font,
                    format!("https://jpdb.io/static/{resource}.woff2"),
                    Some("main"),
                );
            }
            let snapshot = state.snapshot(epoch);
            assert!(snapshot.pending_requests.is_empty());
            assert!(snapshot.stale_pending_requests.is_empty());
        }
        assert!(!state.monitor_failed);
        assert_eq!(state.completed_order.len(), MAX_COMPLETED_REQUESTS);
        assert_eq!(state.completed_requests.len(), state.completed_order.len());
        assert!(!state.completed_requests.contains("0.0"));
        let newest = format!("{}.7", MAX_TRACKED_REQUESTS - 1);
        assert!(state.completed_requests.contains(&newest));
        assert_eq!(state.completed_order.back(), Some(&newest));
    }

    #[test]
    fn outcome_handled_before_its_request_gets_url_and_main_frame_scope() {
        let mut state = RuntimeState {
            main_frame_id: "main".into(),
            ..RuntimeState::default()
        };
        let epoch = state.begin_epoch();
        state.record_network_failure(
            "document".into(),
            ResourceType::Document,
            "net::ERR_CONNECTION_RESET".into(),
        );
        state.record_http_error(
            "throttled".into(),
            ResourceType::Document,
            "https://jpdb.io/search?q=x".into(),
            429,
        );
        state.record_request(
            "document".into(),
            ResourceType::Document,
            "https://jpdb.io/vocabulary/1/x".into(),
            Some("main"),
        );
        state.record_request(
            "throttled".into(),
            ResourceType::Document,
            "https://jpdb.io/search?q=x".into(),
            Some("main"),
        );
        let snapshot = state.snapshot(epoch);
        let failure = &snapshot.network_failures[0];
        assert_eq!(
            failure.url.as_deref(),
            Some("https://jpdb.io/vocabulary/1/x")
        );
        assert!(failure.is_top_level);
        assert!(snapshot.http_errors[0].is_top_level);
        // Неудачный запрос завершён, а ответ с ошибкой ещё может получить loadingFinished.
        assert_eq!(snapshot.pending_requests.len(), 1);
        assert_eq!(snapshot.pending_requests[0].request_id, "throttled");

        // Подкадр не становится главным кадром из-за позднего запроса.
        state.record_network_failure(
            "frame".into(),
            ResourceType::Document,
            "net::ERR_ABORTED".into(),
        );
        state.record_request(
            "frame".into(),
            ResourceType::Document,
            "https://ads.example/frame".into(),
            Some("child"),
        );
        assert!(!state.snapshot(epoch).network_failures[1].is_top_level);
    }

    #[test]
    fn failure_handled_before_redirect_steps_gets_the_final_step_url() {
        let mut state = RuntimeState {
            main_frame_id: "main".into(),
            ..RuntimeState::default()
        };
        let epoch = state.begin_epoch();
        state.record_network_failure(
            "document".into(),
            ResourceType::Document,
            "net::ERR_CERT_AUTHORITY_INVALID".into(),
        );
        for url in [
            "http://yarxi.ru/",
            "https://yarxi.ru/",
            "https://www.yarxi.ru/",
        ] {
            state.record_request(
                "document".into(),
                ResourceType::Document,
                url.into(),
                Some("main"),
            );
        }
        state.record_http_error(
            "throttled".into(),
            ResourceType::Document,
            "https://jpdb.io/search?q=x".into(),
            429,
        );
        state.record_request(
            "throttled".into(),
            ResourceType::Document,
            "https://jpdb.io/redirect-step".into(),
            Some("main"),
        );
        let snapshot = state.snapshot(epoch);
        assert_eq!(
            snapshot.network_failures[0].url.as_deref(),
            Some("https://www.yarxi.ru/")
        );
        assert_eq!(
            snapshot.http_errors[0].url.as_deref(),
            Some("https://jpdb.io/search?q=x")
        );

        let monitor = CdpRuntimeMonitor::from_snapshot_for_test(snapshot);
        assert!(!monitor.clear_exact_network_failure(
            "document",
            "http://yarxi.ru/",
            "net::ERR_CERT_AUTHORITY_INVALID",
        ));
        assert!(monitor.clear_exact_network_failure(
            "document",
            "https://www.yarxi.ru/",
            "net::ERR_CERT_AUTHORITY_INVALID",
        ));
        assert!(monitor.snapshot(epoch).network_failures.is_empty());
    }

    #[test]
    fn bootstrap_javascript_is_counted_once_across_epochs() {
        let mut state = RuntimeState::default();
        state.record_javascript_exception();
        assert_eq!(state.snapshot(0).javascript_exceptions, 1);
        let epoch = state.begin_epoch();
        state.record_javascript_exception();
        assert_eq!(state.snapshot(epoch).javascript_exceptions, 2);
        assert_eq!(state.snapshot(epoch).bootstrap_javascript_exceptions, 1);
        assert_eq!(state.snapshot(epoch).current_javascript_exceptions, 1);
        let next_epoch = state.begin_epoch();
        assert_eq!(state.snapshot(next_epoch).javascript_exceptions, 1);
        assert_eq!(state.snapshot(next_epoch).stale_javascript_exceptions, 1);
    }

    #[tokio::test]
    async fn session_telemetry_rotation_isolates_evidence_and_listener_ownership() {
        // Та же граница создания состояния и задач слушателей, что и в start(),
        // вызываемом BrowserSession::launch_with_profile; CDP заменён каналами.
        let (old_requests, old_request_events) = futures::channel::mpsc::unbounded();
        let old = CdpRuntimeMonitor::from_event_streams(
            "old-frame".into(),
            old_request_events,
            futures::stream::pending(),
            futures::stream::pending(),
            futures::stream::pending(),
            futures::stream::pending(),
        );
        let retained = old.clone();
        {
            let mut state = lock_state(&old.state);
            state.record_javascript_exception();
            let old_epoch = state.begin_epoch();
            state.record_javascript_exception();
            state.record_request(
                "old-pending".into(),
                ResourceType::Image,
                "https://example.test/old".into(),
                Some("old-frame"),
            );
            state.record_network_failure(
                "old-failed".into(),
                ResourceType::Fetch,
                "net::ERR_TIMED_OUT".into(),
            );
            state.record_http_error(
                "old-http".into(),
                ResourceType::Fetch,
                "https://example.test/old-http".into(),
                503,
            );
            assert_eq!(state.snapshot(old_epoch).javascript_exceptions, 2);
            state.begin_epoch();
        }
        let before_rotation = old.current_snapshot();
        assert_eq!(before_rotation.stale_pending_requests.len(), 1);
        assert_eq!(before_rotation.stale_network_failures.len(), 1);
        assert_eq!(before_rotation.stale_http_errors.len(), 1);
        assert_eq!(before_rotation.stale_javascript_exceptions, 1);

        let fresh = CdpRuntimeMonitor::from_event_streams(
            "new-frame".into(),
            futures::stream::pending(),
            futures::stream::pending(),
            futures::stream::pending(),
            futures::stream::pending(),
            futures::stream::pending(),
        );
        assert!(!Arc::ptr_eq(&old.state, &fresh.state));
        assert!(!Arc::ptr_eq(&old.tasks, &fresh.tasks));
        assert_eq!(fresh.current_snapshot(), RuntimeSnapshot::default());
        assert_eq!(fresh.begin_epoch(), 1);
        let expected = RuntimeSnapshot {
            epoch: 1,
            ..RuntimeSnapshot::default()
        };

        // Завершение старого потока после создания нового монитора должно
        // пометить отказавшимся только прежнее состояние. Ждём задачу слушателя,
        // поэтому проверка не зависит от scheduling или задержек по времени.
        drop(old_requests);
        while !old.tasks[0].is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(retained.monitor_failed());
        assert_eq!(fresh.current_snapshot(), expected);
        old.shutdown().await.unwrap();
        assert!(
            retained
                .tasks
                .iter()
                .all(tokio::task::JoinHandle::is_finished)
        );
        assert!(fresh.tasks.iter().all(|task| !task.is_finished()));
        assert_eq!(fresh.current_snapshot(), expected);

        lock_state(&fresh.state).record_request(
            "new-pending".into(),
            ResourceType::Document,
            "https://example.test/new".into(),
            Some("new-frame"),
        );
        assert!(fresh.current_snapshot().pending_requests[0].is_top_level);
        assert_eq!(retained.current_snapshot().pending_requests.len(), 0);
        assert_eq!(retained.current_snapshot().stale_pending_requests.len(), 1);
        fresh.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn telemetry_shutdown_waits_for_shared_listener_task_completion() {
        use std::sync::atomic::AtomicBool;
        struct ListenerStopped(Arc<AtomicBool>);
        impl Drop for ListenerStopped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let stopped = Arc::new(AtomicBool::new(false));
        let observed = Arc::clone(&stopped);
        let (started_tx, started_rx) = futures::channel::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = ListenerStopped(observed);
            started_tx.send(()).unwrap();
            futures::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        let monitor = CdpRuntimeMonitor {
            state: Arc::new(Mutex::new(RuntimeState::default())),
            tasks: Arc::new(vec![task]),
        };
        let shared = monitor.clone();
        monitor.shutdown().await.unwrap();
        assert!(stopped.load(Ordering::SeqCst));
        assert!(
            shared
                .tasks
                .iter()
                .all(tokio::task::JoinHandle::is_finished)
        );
    }
    #[test]
    fn snapshot_separates_current_bootstrap_and_stale_javascript_and_network_evidence() {
        let mut state = RuntimeState::default();
        state.record_javascript_exception();
        let old_epoch = state.begin_epoch();
        for _ in 0..3 {
            state.record_javascript_exception();
        }
        state.record_request(
            "old".into(),
            ResourceType::Fetch,
            "https://example.test/old".into(),
            None,
        );
        state.record_http_error(
            "old".into(),
            ResourceType::Fetch,
            "https://example.test/old".into(),
            503,
        );
        state.record_network_failure(
            "old".into(),
            ResourceType::Fetch,
            "net::ERR_TIMED_OUT".into(),
        );
        let epoch = state.begin_epoch();
        for _ in 0..2 {
            state.record_javascript_exception();
        }
        state.record_request(
            "current".into(),
            ResourceType::Fetch,
            "https://example.test/current".into(),
            None,
        );
        state.record_http_error(
            "current".into(),
            ResourceType::Fetch,
            "https://example.test/current".into(),
            404,
        );
        let snapshot = state.snapshot(epoch);
        assert!(snapshot.network_failures.is_empty());
        assert_eq!(snapshot.http_errors.len(), 1);
        assert_eq!(snapshot.javascript_exceptions, 3);
        let diagnostic = snapshot.diagnostic();
        assert_eq!(diagnostic.current_javascript_exceptions, 2);
        assert_eq!(diagnostic.bootstrap_javascript_exceptions, 1);
        assert_eq!(diagnostic.stale_javascript_exceptions, 3);
        assert_eq!(diagnostic.network_failure_count, 0);
        assert_eq!(diagnostic.http_error_count, 1);
        assert_eq!(diagnostic.stale_network_failure_count, 1);
        assert_eq!(diagnostic.stale_http_error_count, 1);
        assert_eq!(diagnostic.stale_network_failures[0].epoch, old_epoch);
        assert!(!diagnostic.stale_network_failures[0].in_current_epoch);
        assert!(diagnostic.http_errors[0].in_current_epoch);
    }
}
