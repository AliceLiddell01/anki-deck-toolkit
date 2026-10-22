//! Провайдер браузера для динамических статей Yarxi и изображений слева от них.

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::acquisition_pool::{
    AssignmentError, AssignmentLedger, CancellationListener, CancellationReason, PoolCancellation,
    RecoveryBudget,
};
use crate::browser_diagnostics::{BrowserItemContext, BrowserItemTimer, runtime_snapshot_json};
#[cfg(test)]
use crate::browser_runtime::TrackedRequest;
use crate::browser_runtime::{
    self, BrowserRuntimeConfig, BrowserSession, CdpRuntimeMonitor, DeviceMetrics, NetworkOutcome,
    RetryTrigger, RuntimeSnapshot,
};
pub use crate::browser_runtime::{BrowserExecutableSource, BrowserRuntimeProvenance};
use crate::error::AssetError;
use crate::temp_workspace::TempWorkspace;
use base64::Engine as _;
use chromiumoxide::{
    Page,
    cdp::browser_protocol::network::ResourceType,
    cdp::browser_protocol::page::{
        CaptureScreenshotFormat, GetResourceContentParams, GetResourceTreeParams,
    },
};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use image::{GenericImageView, RgbaImage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::panic::AssertUnwindSafe;
use tokio::sync::{mpsc, oneshot};
use tokio::time::{Instant, sleep, timeout_at};
use url::Url;

const PROVIDER_ID: &str = "yarxi-suu-browser";
const PROVIDER_VERSION: &str = "7";
const SITE_URL: &str = "https://www.yarxi.su/";
const SITE_HOST: &str = "www.yarxi.su";
const OPERATION_TIMEOUT: Duration = Duration::from_mins(20);
const SESSION_SETUP_READINESS_TIMEOUT: Duration = Duration::from_secs(20);
const SESSION_SETUP_READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_SESSION_SETUP_RECREATIONS: u8 = 2;
const BROWSER_SETUP_TIMEOUT_MESSAGE: &str =
    "browser_setup_timeout: истёк срок подготовки сеанса браузера";
const SESSION_SETUP_RUNTIME_FAILURE_PREFIX: &str = "browser_session_setup_runtime_unhealthy:";

fn next_session_setup_recreation(current: u8) -> Option<u8> {
    (current < MAX_SESSION_SETUP_RECREATIONS).then(|| current + 1)
}
const ITEM_TIMEOUT: Duration = Duration::from_secs(90);
const PROGRESS_HEARTBEAT: Duration = Duration::from_secs(15);
const TLS_EVIDENCE_TIMEOUT: Duration = Duration::from_secs(2);
const BATCH_PACING: Duration = Duration::from_millis(900);
const RETRY_BACKOFF: Duration = Duration::from_millis(300);
const MAX_ACQUISITION_ATTEMPTS: u8 = 2;
const CAPTURE_VIEWPORT_WIDTH: i64 = 1280;
const CAPTURE_VIEWPORT_HEIGHT: i64 = 900;
const CAPTURE_DEVICE_SCALE_FACTOR: f64 = 2.175;
const CAPTURE_MIN_EDGE_PX: u32 = 140;
const CAPTURE_MAX_EDGE_PX: u32 = 220;
const CAPTURE_LIGHT_GLYPH_MIN_LUMINANCE: f64 = 0.60;
const YARXI_DARK_THEME_STYLE_ID: &str = "asset-store-yarxi-dark-theme";
const YARXI_DARK_THEME_STYLE: &str = ":root { color-scheme: dark !important; } #app { color: rgb(var(--w-base-color-rgb)) !important; }";
type CtrlCListener = tokio::task::JoinHandle<std::io::Result<()>>;

#[derive(Debug, thiserror::Error)]
#[error("parallel coordinator cancellation: {0:?}")]
struct ParallelCancellation(CancellationReason);
const PARALLEL_COORDINATOR_CANCELLED: &str = "browser_worker_cancelled: coordinator_failure";

fn is_parallel_coordinator_cancellation(error: &AcquisitionStreamError) -> bool {
    matches!(error, AcquisitionStreamError::Provider(message) if message == PARALLEL_COORDINATOR_CANCELLED)
}

/// Источник байтов и ограниченный набор свидетельств при работе браузера.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectionResult {
    PrimaryGif,
    LeftmostPngFallback,
    RenderedFontSamplePng,
}

/// Явный выбор источника. `PreferredSource` используется по умолчанию;
/// `RenderedFontSamplePng` предназначен для отдельной приёмки рендеринга в браузере.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionTarget {
    #[default]
    PreferredSource,
    RenderedFontSamplePng,
}

/// Полученный исходный бинарный файл.
#[derive(Debug, Clone, PartialEq)]
pub struct AcquiredMedia {
    pub character: String,
    pub source_url: String,
    pub article_unicode: String,
    pub selection: SelectionResult,
    pub bytes: Vec<u8>,
    pub evidence: AcquisitionEvidence,
}

/// Сохраняемые свидетельства выбора источника без состояния браузера и сеанса.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcquisitionEvidence {
    pub provider: String,
    pub provider_version: String,
    pub article_unicode: String,
    pub article_number: Option<u32>,
    pub frequency_index: Option<u32>,
    pub selection: SelectionResult,
    #[serde(default)]
    pub target: AcquisitionTarget,
    pub fallback_absence_proof: Option<String>,
    pub rendered_font_sample: Option<RenderedFontSampleEvidence>,
    pub source_url: String,
    #[serde(default)]
    pub browser_runtime: Option<BrowserRuntimeProvenance>,
    #[serde(default)]
    pub tls_exception: Option<TlsExceptionProvenance>,
    #[serde(default = "one_acquisition_attempt")]
    pub acquisition_attempts: u8,
}

fn one_acquisition_attempt() -> u8 {
    1
}

/// След использования TLS-исключения, допускаемого политикой только для точного
/// имени узла Yarxi.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsExceptionProvenance {
    pub host: String,
    pub error_code: String,
}

#[derive(Debug, Clone)]
struct ApprovedTlsException {
    request_id: String,
    blocked_url: String,
    provenance: TlsExceptionProvenance,
}

/// Свидетельства для PNG, созданного рендерингом видимого элемента DOM `font-sample` Yarxi.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedFontSampleEvidence {
    pub index: usize,
    pub selector: String,
    pub class_name: String,
    pub title: String,
    pub text: String,
    pub font_family: String,
    pub font_size: String,
    pub capture: String,
    pub css_rect: CssRect,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub device_scale_factor: f64,
    pub page_scale_factor: f64,
    pub prefers_color_scheme_dark: bool,
    pub dark_environment: String,
    pub document_theme: String,
    pub foreground_color: String,
    pub tile_background_color: String,
    pub page_background_color: String,
    pub effective_background_color: String,
    pub border_color: String,
    pub border_style: String,
    pub border_width: String,
    pub border_visible: bool,
    pub box_shadow: String,
    pub inline_style_unchanged: bool,
    pub dark_background_pixels: u32,
    pub light_glyph_pixels: u32,
    pub frame_pixels: u32,
    pub measured_dark_background_rgb: String,
    pub measured_light_glyph_rgb: String,
    pub measured_frame_rgb: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CssRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Чёткое состояние основного GIF для проверки политики и имитационных тестов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrimaryGifState {
    PresentLoaded { url: String },
    AbsentConfirmed { proof: String },
    Unknown { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImageElementState {
    url: String,
    complete: bool,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RuntimeReadiness {
    pending_relevant_requests: usize,
    network_failures: u32,
    http_errors: u32,
    javascript_exceptions: u32,
    monitor_failed: bool,
}

fn is_expected_yarxi_tls_url(raw_url: &str) -> bool {
    Url::parse(raw_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(SITE_HOST))
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn is_relevant_font_sample_resource(
    resource_type: &ResourceType,
    url: Option<&str>,
    selected_image_urls: &HashSet<String>,
) -> bool {
    resource_type != &ResourceType::Image
        || url.is_none_or(|url| url.is_empty() || selected_image_urls.contains(url))
}

#[derive(Debug, Clone)]
struct BrowserEvidenceMonitor {
    telemetry: CdpRuntimeMonitor,
}

impl BrowserEvidenceMonitor {
    fn new(telemetry: CdpRuntimeMonitor) -> Self {
        Self { telemetry }
    }

    fn begin_acquisition(&self) -> u64 {
        self.telemetry.begin_epoch()
    }

    fn readiness(&self, epoch: u64, excluded_primary_gif: Option<&str>) -> RuntimeReadiness {
        let snapshot = self.telemetry.snapshot(epoch);
        readiness_for_snapshot(&snapshot, |resource_type, url| {
            excluded_primary_gif.is_some_and(|excluded| {
                resource_type == &ResourceType::Image && url == Some(excluded)
            })
        })
    }

    fn font_sample_readiness(
        &self,
        epoch: u64,
        selected_image_urls: &HashSet<String>,
    ) -> RuntimeReadiness {
        let snapshot = self.telemetry.snapshot(epoch);
        readiness_for_snapshot(&snapshot, |resource_type, url| {
            !is_relevant_font_sample_resource(resource_type, url, selected_image_urls)
        })
    }

    fn tls_navigation_failure(&self) -> Option<NetworkOutcome> {
        self.telemetry
            .snapshot(0)
            .network_failures
            .into_iter()
            .rev()
            .find(|failure| {
                failure.resource_type == ResourceType::Document
                    && failure.is_top_level
                    && failure
                        .url
                        .as_deref()
                        .is_some_and(is_expected_yarxi_tls_url)
                    && failure.failure_reason.as_deref().is_some_and(|reason| {
                        reason.eq_ignore_ascii_case("net::ERR_CERT_AUTHORITY_INVALID")
                    })
            })
    }

    fn clear_explicitly_approved_tls_interstitial_failure(
        &self,
        request_id: &str,
        blocked_url: &str,
    ) {
        let exact_failure_is_approved =
            self.telemetry
                .snapshot(0)
                .network_failures
                .iter()
                .any(|failure| {
                    failure.resource_type == ResourceType::Document
                        && failure.is_top_level
                        && failure.request_id == request_id
                        && failure.url.as_deref() == Some(blocked_url)
                        && failure.failure_reason.as_deref().is_some_and(|reason| {
                            reason.eq_ignore_ascii_case("net::ERR_CERT_AUTHORITY_INVALID")
                        })
                });
        if exact_failure_is_approved {
            self.telemetry.clear_exact_network_failure(
                request_id,
                blocked_url,
                "net::ERR_CERT_AUTHORITY_INVALID",
            );
        }
    }

    fn retryable_acquisition(&self, epoch: u64, trigger: RetryTrigger) -> bool {
        self.telemetry.retryable_acquisition(epoch, trigger)
    }

    fn network_failure_details(&self, epoch: u64, excluded_primary_gif: Option<&str>) -> String {
        let failures: Vec<_> = self
            .telemetry
            .snapshot(epoch)
            .network_failures
            .into_iter()
            .filter(|failure| {
                !excluded_primary_gif.is_some_and(|excluded| {
                    failure.resource_type == ResourceType::Image
                        && failure.url.as_deref() == Some(excluded)
                })
            })
            .collect();
        browser_runtime::format_network_failure_details(&failures)
    }

    fn font_sample_network_failure_details(
        &self,
        epoch: u64,
        selected_image_urls: &HashSet<String>,
    ) -> String {
        let failures: Vec<_> = self
            .telemetry
            .snapshot(epoch)
            .network_failures
            .into_iter()
            .filter(|failure| {
                is_relevant_font_sample_resource(
                    &failure.resource_type,
                    failure.url.as_deref(),
                    selected_image_urls,
                )
            })
            .collect();
        browser_runtime::format_network_failure_details(&failures)
    }

    fn abort(&self) {
        self.telemetry.abort();
    }
}

fn readiness_for_snapshot(
    snapshot: &RuntimeSnapshot,
    mut excluded: impl FnMut(&ResourceType, Option<&str>) -> bool,
) -> RuntimeReadiness {
    RuntimeReadiness {
        pending_relevant_requests: snapshot
            .pending_requests
            .iter()
            .filter(|request| !excluded(&request.resource_type, Some(&request.url)))
            .count(),
        network_failures: snapshot
            .network_failures
            .iter()
            .filter(|failure| !excluded(&failure.resource_type, failure.url.as_deref()))
            .count() as u32,
        http_errors: snapshot
            .http_errors
            .iter()
            .filter(|failure| !excluded(&failure.resource_type, failure.url.as_deref()))
            .count() as u32,
        javascript_exceptions: snapshot.javascript_exceptions,
        monitor_failed: snapshot.monitor_failed,
    }
}

fn classify_primary_state(
    primary: Option<&ImageElementState>,
    information_area_ready: bool,
    dom_stable: bool,
    all_images_complete: bool,
    runtime: RuntimeReadiness,
) -> PrimaryGifState {
    if let Some(image) = primary {
        if image.complete && image.width > 0 && image.height > 0 && !image.url.is_empty() {
            return PrimaryGifState::PresentLoaded {
                url: image.url.clone(),
            };
        }
        return PrimaryGifState::Unknown {
            reason: if image.complete {
                "элемент основного GIF есть, но загрузка завершилась без естественных размеров"
                    .into()
            } else {
                "элемент основного GIF существует, загрузка ещё не завершена".into()
            },
        };
    }
    if !runtime_is_clean(runtime) {
        return PrimaryGifState::Unknown {
            reason: runtime_failure_reason(runtime),
        };
    }
    if information_area_ready && dom_stable && all_images_complete {
        PrimaryGifState::AbsentConfirmed {
            proof: format!(
                "область информации готова; Unicode статьи совпадает; все DOM-изображения загружены; DOM стабилен 2 секунды; img.kakijun-gif отсутствует; CDP Network/Runtime чисты (ожидают={}, ошибок сети={}, HTTP-ошибок={}, JS-исключений={})",
                runtime.pending_relevant_requests,
                runtime.network_failures,
                runtime.http_errors,
                runtime.javascript_exceptions
            ),
        }
    } else {
        PrimaryGifState::Unknown {
            reason: "область информации или медиа ещё не завершила загрузку и не стабилизировалась"
                .into(),
        }
    }
}

fn runtime_is_clean(runtime: RuntimeReadiness) -> bool {
    !runtime.monitor_failed
        && runtime.pending_relevant_requests == 0
        && runtime.network_failures == 0
        && runtime.http_errors == 0
        && runtime.javascript_exceptions == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionSetupRuntimeState {
    Clean,
    PendingOnly,
    Failed,
}

fn session_setup_runtime_state(runtime: RuntimeReadiness) -> SessionSetupRuntimeState {
    if runtime_is_clean(runtime) {
        SessionSetupRuntimeState::Clean
    } else if runtime.pending_relevant_requests > 0
        && runtime.network_failures == 0
        && runtime.http_errors == 0
        && runtime.javascript_exceptions == 0
        && !runtime.monitor_failed
    {
        SessionSetupRuntimeState::PendingOnly
    } else {
        SessionSetupRuntimeState::Failed
    }
}

async fn wait_for_session_setup_readiness(
    evidence_monitor: &BrowserEvidenceMonitor,
) -> Result<(), String> {
    let deadline = Instant::now() + SESSION_SETUP_READINESS_TIMEOUT;
    loop {
        let readiness = evidence_monitor.readiness(0, None);
        match session_setup_runtime_state(readiness) {
            SessionSetupRuntimeState::Clean => return Ok(()),
            SessionSetupRuntimeState::Failed => {
                return Err(format!(
                    "{SESSION_SETUP_RUNTIME_FAILURE_PREFIX} {}",
                    runtime_failure_reason(readiness)
                ));
            }
            SessionSetupRuntimeState::PendingOnly if Instant::now() >= deadline => {
                return Err(format!(
                    "{SESSION_SETUP_RUNTIME_FAILURE_PREFIX} истёк срок ожидания чистого состояния среды выполнения при запуске: {}",
                    runtime_failure_reason(readiness)
                ));
            }
            SessionSetupRuntimeState::PendingOnly => {
                sleep(SESSION_SETUP_READINESS_POLL_INTERVAL).await;
            }
        }
    }
}

fn runtime_failure_reason(runtime: RuntimeReadiness) -> String {
    format!(
        "сетевая среда выполнения не чиста (monitor_failed={}, ожидают={}, сетевых ошибок={}, HTTP-ошибок={}, JS-исключений={})",
        runtime.monitor_failed,
        runtime.pending_relevant_requests,
        runtime.network_failures,
        runtime.http_errors,
        runtime.javascript_exceptions
    )
}

#[derive(Debug, Clone, PartialEq)]
enum MediaSourceChoice {
    Url {
        selection: SelectionResult,
        url: String,
        absence_proof: Option<String>,
    },
    RenderedFontSample {
        sample: FontSampleState,
        absence_proof: Option<String>,
    },
}

/// Выбирает байты исходного изображения или PNG, отрендеренный из точного видимого `font-sample`.
/// URL Kakijun справа никогда не используется как источник.
#[cfg(test)]
fn choose_media_source(
    primary: PrimaryGifState,
    font_samples: &[FontSampleState],
    expected_character: &str,
) -> Result<MediaSourceChoice, String> {
    choose_media_source_for_target(
        primary,
        font_samples,
        expected_character,
        AcquisitionTarget::PreferredSource,
    )
}

fn choose_media_source_for_target(
    primary: PrimaryGifState,
    font_samples: &[FontSampleState],
    expected_character: &str,
    target: AcquisitionTarget,
) -> Result<MediaSourceChoice, String> {
    if target == AcquisitionTarget::RenderedFontSamplePng {
        return Ok(MediaSourceChoice::RenderedFontSample {
            sample: choose_font_sample(font_samples, expected_character)?,
            absence_proof: None,
        });
    }
    match primary {
        PrimaryGifState::PresentLoaded { url } if target == AcquisitionTarget::PreferredSource => {
            Ok(MediaSourceChoice::Url {
                selection: SelectionResult::PrimaryGif,
                url,
                absence_proof: None,
            })
        }
        PrimaryGifState::PresentLoaded { .. } => {
            unreachable!("ветка для основного GIF обработана выше")
        }
        PrimaryGifState::AbsentConfirmed { proof } => {
            let sample = choose_font_sample(font_samples, expected_character)?;
            if let Some(image) = sample.images.first() {
                if !image.complete || image.width == 0 || image.height == 0 || image.url.is_empty()
                {
                    return Err(
                        "fallback_font_sample_image_unavailable: левое изображение образца не загрузилось"
                            .into(),
                    );
                }
                let parsed = Url::parse(&image.url).map_err(
                    |_| "fallback_url_invalid: URL крайнего левого изображения некорректен",
                )?;
                if parsed.host_str().is_some_and(is_kakijun_host) {
                    return Err(
                        "kakijun_source_forbidden: kakijun.jp не является резервным источником PNG"
                            .into(),
                    );
                }
                return Ok(MediaSourceChoice::Url {
                    selection: SelectionResult::LeftmostPngFallback,
                    url: image.url.clone(),
                    absence_proof: Some(proof),
                });
            }
            Ok(MediaSourceChoice::RenderedFontSample {
                sample,
                absence_proof: Some(proof),
            })
        }
        PrimaryGifState::Unknown { reason } => Err(format!(
            "gif_unknown: нельзя выбрать запасное изображение или образец шрифта: {reason}"
        )),
    }
}

fn choose_font_sample(
    font_samples: &[FontSampleState],
    expected_character: &str,
) -> Result<FontSampleState, String> {
    font_samples
        .iter()
        .filter(|sample| {
            sample.visible
                && !sample
                    .class_name
                    .split_whitespace()
                    .any(|class| class.starts_with("stroke-order"))
                && sample.width > 0.0
                && sample.height > 0.0
                && (sample.text.trim() == expected_character
                    || (!sample.images.is_empty() && sample.text.trim().is_empty()))
        })
        .min_by(|left, right| {
            left.left
                .total_cmp(&right.left)
                .then_with(|| left.top.total_cmp(&right.top))
        })
        .cloned()
        .ok_or_else(|| {
            "fallback_font_sample_missing: нет видимого font-sample с точным текстом кандзи"
                .to_owned()
        })
}

/// Выполняет изолированный пакет браузера. Параметр `allow_insecure_tls` разрешает
/// пройти страницу-предупреждение только для точного имени узла Yarxi; CLI включает
/// это разрешение по умолчанию.
pub fn acquire_many(
    characters: &[String],
    allow_insecure_tls: bool,
) -> Result<Vec<Result<AcquiredMedia, String>>, String> {
    acquire_many_with_target(
        characters,
        allow_insecure_tls,
        AcquisitionTarget::PreferredSource,
    )
}

/// Получает медиа с явно выбранной политикой источника. Инструменты приёмки
/// могут запросить PNG из `font-sample`; обычные вызовы используют [`acquire_many`]
/// с предпочтением основного GIF.
pub fn acquire_many_with_target(
    characters: &[String],
    allow_insecure_tls: bool,
    target: AcquisitionTarget,
) -> Result<Vec<Result<AcquiredMedia, String>>, String> {
    if characters.is_empty() {
        return Ok(Vec::new());
    }
    let mut outcomes = vec![None; characters.len()];
    acquire_many_stream_with_target(characters, allow_insecure_tls, target, |event| {
        if let AcquisitionEvent::ItemCompleted {
            index,
            outcome,
            diagnostics,
        } = event
        {
            let slot = outcomes.get_mut(index).ok_or_else(|| {
                AssetError::new(
                    crate::error::ErrorCode::InvalidTransition,
                    "провайдер выдал индекс вне исходного набора",
                )
            })?;
            if slot.is_some() {
                return Err(AssetError::new(
                    crate::error::ErrorCode::InvalidTransition,
                    "провайдер повторно выдал идентичность в одном наборе",
                ));
            }
            if let Some(diagnostics) = diagnostics {
                diagnostics.finish_acquisition(&outcome);
            }
            *slot = Some(*outcome);
        }
        Ok(())
    })
    .map_err(AcquisitionStreamError::into_message)?;
    outcomes
        .into_iter()
        .map(|outcome| {
            outcome.ok_or_else(|| {
                "provider_stream_incomplete: браузер завершил работу до обработки всего набора"
                    .to_owned()
            })
        })
        .collect()
}

#[derive(Debug)]
pub enum AcquisitionEvent {
    SessionStarted {
        session: u32,
    },
    SessionEnded {
        session: u32,
        processed: usize,
        stop_reason: Option<SessionStopReason>,
    },
    SessionRotated {
        next_session: u32,
        reason: SessionStopReason,
    },
    ItemStarted {
        index: usize,
    },
    RetryStarted {
        index: usize,
        attempt: u8,
    },
    RetryRecoveryStarted {
        index: usize,
        attempt: u8,
    },
    Heartbeat {
        index: usize,
        attempt: u8,
    },
    ItemCompleted {
        index: usize,
        outcome: Box<Result<AcquiredMedia, String>>,
        diagnostics: Option<AcquisitionItemDiagnostics>,
    },
}

/// Контекст единственного назначения; попытка поставщика отличается от попытки владельца.
#[derive(Debug)]
pub struct AcquisitionWorkerEvent {
    pub worker_id: usize,
    pub index: Option<usize>,
    pub generation: Option<u32>,
    pub owner_attempt: Option<usize>,
    pub session: u32,
    pub browser_session: Option<u64>,
    pub attempt: u8,
    pub event: AcquisitionWorkerEventKind,
}

#[derive(Debug)]
pub enum AcquisitionWorkerEventKind {
    Assigned,
    SessionBootstrapStarted,
    Provider(AcquisitionEvent),
    Deferred {
        reason: String,
    },
    Requeued {
        reason: String,
    },
    WorkerRetired {
        consecutive_failures: u8,
        reason: String,
    },
    WorkerStopped {
        reason: String,
    },
}

fn yarxi_workspace(purpose: &str) -> std::io::Result<TempWorkspace> {
    TempWorkspace::create(purpose)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParallelJob {
    index: usize,
    character: String,
    generation: u32,
    owner_attempt: usize,
}

#[derive(Clone, Default)]
struct ParallelWorkerContext {
    worker_id: usize,
    session: u32,
    browser_session: Option<u64>,
    attempt: u8,
    job: Option<ParallelJob>,
}

impl ParallelWorkerContext {
    fn event(&self, event: AcquisitionWorkerEventKind) -> AcquisitionWorkerEvent {
        AcquisitionWorkerEvent {
            worker_id: self.worker_id,
            index: self.job.as_ref().map(|job| job.index),
            generation: self.job.as_ref().map(|job| job.generation),
            owner_attempt: self.job.as_ref().map(|job| job.owner_attempt),
            session: self.session,
            browser_session: self.browser_session,
            attempt: self.attempt,
            event,
        }
    }
}

enum ParallelMessage {
    Event(AcquisitionWorkerEvent, Option<oneshot::Sender<()>>),
    Ready(usize),
}

fn parallel_protocol_error(message: &str) -> AcquisitionStreamError {
    AcquisitionStreamError::Provider(format!("browser_worker_protocol_violation: {message}"))
}

fn send_parallel_event(
    sender: &mpsc::UnboundedSender<ParallelMessage>,
    context: &ParallelWorkerContext,
    event: AcquisitionWorkerEventKind,
) -> Result<(), AcquisitionStreamError> {
    sender
        .send(ParallelMessage::Event(context.event(event), None))
        .map_err(|_| parallel_protocol_error("канал координатора закрыт"))
}

fn relay_parallel_item_event(
    sender: &mpsc::UnboundedSender<ParallelMessage>,
    context: &mut ParallelWorkerContext,
    index: usize,
    event: AcquisitionEvent,
) -> Result<(), AssetError> {
    let event = match event {
        // Координатор сообщает о всём назначении, включая подготовку сессии.
        AcquisitionEvent::Heartbeat { .. } => return Ok(()),
        AcquisitionEvent::RetryStarted { attempt, .. } => {
            context.attempt = attempt;
            AcquisitionEvent::RetryStarted { index, attempt }
        }
        AcquisitionEvent::RetryRecoveryStarted { attempt, .. } => {
            context.attempt = attempt;
            AcquisitionEvent::RetryRecoveryStarted { index, attempt }
        }
        event => event,
    };
    send_parallel_event(sender, context, AcquisitionWorkerEventKind::Provider(event)).map_err(
        |error| {
            AssetError::new(
                crate::error::ErrorCode::InvalidTransition,
                error.into_message(),
            )
        },
    )
}

async fn acknowledge_parallel_event(
    sender: &mpsc::UnboundedSender<ParallelMessage>,
    context: &ParallelWorkerContext,
    event: AcquisitionWorkerEventKind,
) -> Result<(), AcquisitionStreamError> {
    let (ack, received) = oneshot::channel();
    sender
        .send(ParallelMessage::Event(context.event(event), Some(ack)))
        .map_err(|_| parallel_protocol_error("канал координатора закрыт"))?;
    received
        .await
        .map_err(|_| parallel_protocol_error("результат не подтверждён координатором"))
}

struct ParallelAcquired {
    outcome: Option<Result<AcquiredMedia, String>>,
    diagnostics: Option<AcquisitionItemDiagnostics>,
    stop: Option<SessionStopReason>,
}

/// Узкая подмена только браузера; координатор и ack остаются настоящими в тестах.
trait ParallelDriver {
    async fn acquire(
        &mut self,
        job: &ParallelJob,
        context: &mut ParallelWorkerContext,
        sender: &mpsc::UnboundedSender<ParallelMessage>,
    ) -> Result<ParallelAcquired, AcquisitionStreamError>;
    async fn after_checkpoint(
        &mut self,
    ) -> Result<Option<SessionStopReason>, AcquisitionStreamError>;
    async fn rotate(&mut self) -> Result<(), AcquisitionStreamError>;
    fn finish(&mut self) -> Result<(), AcquisitionStreamError>;
}

struct ParallelBrowserDriver {
    workspace: Option<TempWorkspace>,
    session: Option<BrowserSession>,
    monitor: Option<BrowserEvidenceMonitor>,
    tls_exception: Option<TlsExceptionProvenance>,
    deadline: Instant,
    session_items: usize,
    interrupt: CtrlCListener,
    allow_insecure_tls: bool,
    acceptance_rotate_after_items: Option<usize>,
}

impl ParallelBrowserDriver {
    async fn start(
        &mut self,
        context: &mut ParallelWorkerContext,
        sender: &mpsc::UnboundedSender<ParallelMessage>,
    ) -> Result<(), AcquisitionStreamError> {
        if self.workspace.is_none() {
            self.workspace = Some(yarxi_workspace("yarxi-browser-worker").map_err(|error| {
                AcquisitionStreamError::Provider(format!("worker_workspace_create_failed: {error}"))
            })?);
        }
        context.session += 1;
        context.browser_session = None;
        self.deadline = Instant::now() + OPERATION_TIMEOUT;
        let config = BrowserRuntimeConfig {
            device_metrics: Some(
                DeviceMetrics::new(
                    CAPTURE_VIEWPORT_WIDTH,
                    CAPTURE_VIEWPORT_HEIGHT,
                    CAPTURE_DEVICE_SCALE_FACTOR,
                )
                .map_err(AcquisitionStreamError::Provider)?,
            ),
            prefers_color_scheme: Some("dark".into()),
            ..BrowserRuntimeConfig::default()
        };
        // Не отменяем launch: после получения владельца отмена проходит через явный close.
        let session = BrowserSession::launch_in_workspace(
            config,
            self.workspace.as_ref().expect("worker workspace").path(),
        )
        .await
        .map_err(|error| AcquisitionStreamError::Provider(error.to_string()))?;
        context.browser_session = Some(session.diagnostic_session_id());
        self.monitor = Some(BrowserEvidenceMonitor::new(session.telemetry().clone()));
        self.session = Some(session);
        send_parallel_event(
            sender,
            context,
            AcquisitionWorkerEventKind::SessionBootstrapStarted,
        )?;
        let setup = run_setup_before_deadline(
            self.deadline,
            prepare_yarxi_session(
                self.session.as_ref().expect("session owned"),
                self.monitor.as_ref().expect("monitor owned"),
                self.allow_insecure_tls,
                AcquisitionTarget::PreferredSource,
            ),
        );
        let setup = tokio::select! {
            biased;
            setup = setup => setup.map_err(AcquisitionStreamError::Provider),
            signal = &mut self.interrupt => return Err(signal_error(signal)),
        }?;
        self.tls_exception = setup.map(|approved| approved.provenance);
        self.session_items = 0;
        send_parallel_event(
            sender,
            context,
            AcquisitionWorkerEventKind::Provider(AcquisitionEvent::SessionStarted {
                session: context.session,
            }),
        )
    }
}

impl ParallelDriver for ParallelBrowserDriver {
    async fn acquire(
        &mut self,
        job: &ParallelJob,
        context: &mut ParallelWorkerContext,
        sender: &mpsc::UnboundedSender<ParallelMessage>,
    ) -> Result<ParallelAcquired, AcquisitionStreamError> {
        if self.session.is_none() {
            self.start(context, sender).await?;
        }
        let generations = [job.generation];
        let session = self.session.as_ref().expect("session owned");
        let monitor = self.monitor.as_ref().expect("monitor owned");
        let mut driver = BrowserSessionItemDriver {
            context: RetryAcquisitionContext {
                page: session.page(),
                target: AcquisitionTarget::PreferredSource,
                evidence_monitor: monitor,
                browser_runtime: session.provenance(),
                tls_exception: self.tls_exception.clone(),
                session_deadline: self.deadline,
                session: context.session,
                browser_session: session.diagnostic_session_id(),
                interrupt: &mut self.interrupt,
            },
            session: context.session,
            generations: Some(&generations),
            worker: Some((context.worker_id as u64, context.session as u64)),
            diagnostics: None,
        };
        if let Some(stop) = driver
            .before_item(usize::from(self.session_items > 0))
            .await?
        {
            return Ok(ParallelAcquired {
                outcome: None,
                diagnostics: None,
                stop: Some(stop),
            });
        }
        context.attempt = 1;
        send_parallel_event(
            sender,
            context,
            AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemStarted {
                index: job.index,
            }),
        )?;
        // Индекс 0 относится к локальному массиву поколений, внешний индекс — к исходной границе обхода.
        let mut events = |event| relay_parallel_item_event(sender, context, job.index, event);
        let (outcome, stop) = driver.acquire(0, &job.character, &mut events).await?;
        let diagnostics = driver.take_diagnostics();
        // Неисправный CDP или возраст сессии до доказанного результата не создаёт попытку владельца.
        let deferred = outcome.is_err()
            && matches!(
                stop,
                Some(SessionStopReason::Deadline | SessionStopReason::RuntimeFailure(_))
            );
        if !deferred {
            self.session_items += 1;
        }
        #[cfg(feature = "session-rotation-acceptance")]
        let stop = stop.or_else(|| {
            self.acceptance_rotate_after_items
                .is_some_and(|limit| self.session_items >= limit)
                .then_some(SessionStopReason::ItemLimitReached)
        });
        #[cfg(not(feature = "session-rotation-acceptance"))]
        let _ = self.acceptance_rotate_after_items;
        Ok(ParallelAcquired {
            outcome: (!deferred).then_some(outcome),
            diagnostics,
            stop,
        })
    }

    async fn after_checkpoint(
        &mut self,
    ) -> Result<Option<SessionStopReason>, AcquisitionStreamError> {
        if self.interrupt.is_finished() {
            return Err(signal_error((&mut self.interrupt).await));
        }
        let monitor = self.monitor.as_ref().ok_or_else(|| {
            parallel_protocol_error("после сохранения результата отсутствует монитор сессии")
        })?;
        Ok(monitor_stop_reason(monitor))
    }

    async fn rotate(&mut self) -> Result<(), AcquisitionStreamError> {
        if let Some(monitor) = self.monitor.take() {
            monitor.abort();
        }
        if let Some(session) = self.session.take() {
            session.close().await.map_err(|error| {
                AcquisitionStreamError::Provider(format!("browser_worker_cleanup_failed: {error}"))
            })?;
        }
        self.tls_exception = None;
        Ok(())
    }

    fn finish(&mut self) -> Result<(), AcquisitionStreamError> {
        self.interrupt.abort();
        self.workspace.take().map_or(Ok(()), |workspace| {
            workspace.close().map_err(|error| {
                AcquisitionStreamError::Provider(format!(
                    "worker_workspace_cleanup_failed: {error}"
                ))
            })
        })
    }
}

const PARALLEL_RECOVERY_BUDGET: u32 = MAX_SESSION_SETUP_RECREATIONS as u32 + 1;

fn parallel_recoverable(error: &AcquisitionStreamError) -> bool {
    matches!(error, AcquisitionStreamError::Provider(message) if message.starts_with(SESSION_SETUP_RUNTIME_FAILURE_PREFIX))
}

fn parallel_session_fault(stop: &SessionStopReason) -> bool {
    matches!(
        stop,
        SessionStopReason::RuntimeFailure(_)
            | SessionStopReason::RetryRecoveryFailed(_)
            | SessionStopReason::ItemTimeout
    )
}

async fn run_parallel_worker(
    mut driver: impl ParallelDriver,
    worker_id: usize,
    mut commands: mpsc::UnboundedReceiver<ParallelJob>,
    sender: mpsc::UnboundedSender<ParallelMessage>,
    mut cancellation: CancellationListener,
) -> (usize, Result<(), AcquisitionStreamError>) {
    let mut context = ParallelWorkerContext {
        worker_id,
        ..ParallelWorkerContext::default()
    };
    let mut recovery = RecoveryBudget::new(PARALLEL_RECOVERY_BUDGET);
    let mut session_processed = 0usize;
    // Последующий здоровый checkpoint доказывает восстановление. Без него
    // завершение очереди должно сохранить отказ последнего монитора.
    let mut checkpoint_fault = None;
    let result = AssertUnwindSafe(async {
        loop {
            sender
                .send(ParallelMessage::Ready(worker_id))
                .map_err(|_| parallel_protocol_error("канал координатора закрыт"))?;
            let job = tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                job = commands.recv() => match job {
                    Some(job) => job,
                    None => {
                        if let Some(message) = checkpoint_fault.take() {
                            return Err(AcquisitionStreamError::Provider(message));
                        }
                        break;
                    },
                },
            };
            if cancellation.reason().is_some() {
                break;
            }
            context.job = Some(job.clone());
            context.attempt = 1;
            send_parallel_event(&sender, &context, AcquisitionWorkerEventKind::Assigned)?;
            let acquired = driver.acquire(&job, &mut context, &sender).await;
            let mut stop;
            match acquired {
                Ok(mut acquired) => {
                    stop = acquired.stop.take();
                    if let Some(outcome) = acquired.outcome.take() {
                        let successful = outcome.is_ok();
                        session_processed += 1;
                        acknowledge_parallel_event(
                            &sender,
                            &context,
                            AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                                index: job.index,
                                outcome: Box::new(outcome),
                                diagnostics: acquired.diagnostics.take(),
                            }),
                        )
                        .await?;
                        // Проверяем здоровье после подтверждённой записи, включая последний
                        // элемент. Отказ сессии не отменяет результат и не создаёт новую попытку.
                        let health_stop = driver.after_checkpoint().await?;
                        checkpoint_fault = match &health_stop {
                            Some(SessionStopReason::RuntimeFailure(message)) => {
                                Some(message.clone())
                            }
                            _ => None,
                        };
                        stop = health_stop.or(stop);
                        if successful || !stop.as_ref().is_some_and(parallel_session_fault) {
                            recovery.record_success();
                        }
                    } else {
                        if let Some(diagnostics) = acquired.diagnostics.take() {
                            diagnostics.timing.interrupt("session_fault_deferred", None);
                        }
                        let reason = stop
                            .as_ref()
                            .map_or_else(|| "session_fault".into(), SessionStopReason::summary);
                        acknowledge_parallel_event(
                            &sender,
                            &context,
                            AcquisitionWorkerEventKind::Deferred { reason },
                        )
                        .await?;
                    }
                    if stop.as_ref().is_some_and(parallel_session_fault) {
                        recovery.record_failure();
                    }
                }
                Err(AcquisitionStreamError::Interrupted) => {
                    acknowledge_parallel_event(
                        &sender,
                        &context,
                        AcquisitionWorkerEventKind::Deferred {
                            reason: "interrupted".into(),
                        },
                    )
                    .await?;
                    break;
                }
                Err(error) if is_parallel_coordinator_cancellation(&error) => {
                    acknowledge_parallel_event(
                        &sender,
                        &context,
                        AcquisitionWorkerEventKind::Deferred {
                            reason: CancellationReason::CoordinatorFailure.label().into(),
                        },
                    )
                    .await?;
                    break;
                }
                Err(error) if parallel_recoverable(&error) => {
                    recovery.record_failure();
                    acknowledge_parallel_event(
                        &sender,
                        &context,
                        AcquisitionWorkerEventKind::Deferred {
                            reason: error.into_message(),
                        },
                    )
                    .await?;
                    stop = Some(SessionStopReason::RuntimeFailure(
                        "session_setup_failed".into(),
                    ));
                }
                Err(error) => return Err(error),
            }
            if let Some(reason) = stop {
                driver.rotate().await?;
                send_parallel_event(
                    &sender,
                    &context,
                    AcquisitionWorkerEventKind::Provider(AcquisitionEvent::SessionEnded {
                        session: context.session,
                        processed: session_processed,
                        stop_reason: Some(reason.clone()),
                    }),
                )?;
                session_processed = 0;
                context.browser_session = None;
                if recovery.exhausted() {
                    send_parallel_event(
                        &sender,
                        &context,
                        AcquisitionWorkerEventKind::WorkerRetired {
                            consecutive_failures: u8::try_from(recovery.consecutive_failures())
                                .unwrap_or(u8::MAX),
                            reason: reason.summary(),
                        },
                    )?;
                    if commands.is_closed()
                        && let Some(message) = checkpoint_fault.take()
                    {
                        return Err(AcquisitionStreamError::Provider(message));
                    }
                    break;
                }
                send_parallel_event(
                    &sender,
                    &context,
                    AcquisitionWorkerEventKind::Provider(AcquisitionEvent::SessionRotated {
                        next_session: context.session + 1,
                        reason,
                    }),
                )?;
                context.browser_session = None;
            }
            context.job = None;
        }
        Ok(())
    })
    .catch_unwind()
    .await;
    let mut result = result.unwrap_or_else(|_| {
        Err(AcquisitionStreamError::Provider(
            "browser_worker_panicked".into(),
        ))
    });
    let cleanup = AssertUnwindSafe(driver.rotate())
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            Err(AcquisitionStreamError::Provider(
                "browser_worker_cleanup_panicked".into(),
            ))
        });
    if cleanup.is_ok() && context.browser_session.is_some() {
        let cancellation_reason = cancellation.reason();
        let stop_reason = (cancellation_reason == Some(CancellationReason::Interrupted))
            .then_some(SessionStopReason::Interrupted);
        if cancellation_reason == Some(CancellationReason::CoordinatorFailure) {
            tracing::warn!(
                worker = context.worker_id,
                session = context.session,
                stage = "worker_cancel",
                code = "browser_worker_cancelled",
                cancellation_reason = "coordinator_failure",
                "Обработчик остановлен отказом координатора"
            );
        }
        if let Err(error) = send_parallel_event(
            &sender,
            &context,
            AcquisitionWorkerEventKind::Provider(AcquisitionEvent::SessionEnded {
                session: context.session,
                processed: session_processed,
                stop_reason,
            }),
        ) {
            result = Err(error);
        }
    }
    if let Err(cleanup) = cleanup {
        result = Err(AcquisitionStreamError::Provider(format!(
            "{}; {}",
            result.err().map_or_else(
                || "обработчик остановлен".into(),
                AcquisitionStreamError::into_message
            ),
            cleanup.into_message()
        )));
    }
    let finish =
        std::panic::catch_unwind(AssertUnwindSafe(|| driver.finish())).unwrap_or_else(|_| {
            Err(AcquisitionStreamError::Provider(
                "worker_workspace_cleanup_panicked".into(),
            ))
        });
    if let Err(cleanup) = finish {
        result = Err(AcquisitionStreamError::Provider(format!(
            "{}; {}",
            result.err().map_or_else(
                || "обработчик остановлен".into(),
                AcquisitionStreamError::into_message
            ),
            cleanup.into_message()
        )));
    }
    let reason = result
        .as_ref()
        .err()
        .map_or_else(|| "finished".into(), AcquisitionStreamError::message);
    let _ = send_parallel_event(
        &sender,
        &context,
        AcquisitionWorkerEventKind::WorkerStopped { reason },
    );
    (worker_id, result)
}

type ParallelAssignments = AssignmentLedger<ParallelJob>;

fn parallel_assignment_error(error: AssignmentError) -> AcquisitionStreamError {
    parallel_protocol_error(match error {
        AssignmentError::UnknownWorker => "неизвестный обработчик",
        AssignmentError::NoAssignment => "результат без назначения",
        AssignmentError::Mismatch => {
            "identity/generation/attempt результата не совпали с назначением"
        }
        AssignmentError::WorkerUnavailable => "обработчик не принимает назначение",
    })
}

/// Результат принимается только с точными индексом, поколением и попыткой назначения.
fn accept_parallel_result(
    state: &mut ParallelAssignments,
    event: &AcquisitionWorkerEvent,
) -> Result<ParallelJob, AcquisitionStreamError> {
    state
        .accept(event.worker_id, |expected| {
            event.index == Some(expected.index)
                && event.generation == Some(expected.generation)
                && event.owner_attempt == Some(expected.owner_attempt)
        })
        .map_err(parallel_assignment_error)
}

fn stopped_parallel_failure(event: &AcquisitionWorkerEvent) -> Option<AcquisitionStreamError> {
    match &event.event {
        AcquisitionWorkerEventKind::WorkerStopped { reason } if reason != "finished" => Some(
            AcquisitionStreamError::Provider(format!("browser_worker_stopped_fatal: {reason}")),
        ),
        _ => None,
    }
}

fn deliver_parallel_event(
    on_event: &mut impl FnMut(AcquisitionWorkerEvent) -> Result<(), AssetError>,
    event: AcquisitionWorkerEvent,
) -> Result<(), AcquisitionStreamError> {
    std::panic::catch_unwind(AssertUnwindSafe(|| on_event(event)))
        .map_err(|_| AcquisitionStreamError::Provider("browser_worker_consumer_panicked".into()))?
        .map_err(AcquisitionStreamError::Consumer)
}

async fn coordinate_parallel<D: ParallelDriver>(
    drivers: Vec<D>,
    jobs: VecDeque<ParallelJob>,
    interrupt: impl Future<Output = AcquisitionStreamError>,
    cancellation: PoolCancellation,
    on_event: &mut impl FnMut(AcquisitionWorkerEvent) -> Result<(), AssetError>,
) -> Result<AcquisitionSummary, AcquisitionStreamError> {
    let count = drivers.len();
    let (sender, mut messages) = mpsc::unbounded_channel();
    let mut futures = FuturesUnordered::new();
    let mut commands = Vec::new();
    for (slot, driver) in drivers.into_iter().enumerate() {
        let (command, received) = mpsc::unbounded_channel();
        commands.push(command);
        futures.push(run_parallel_worker(
            driver,
            slot + 1,
            received,
            sender.clone(),
            cancellation.subscribe(),
        ));
    }
    drop(sender);
    let mut state = ParallelAssignments::new(jobs, commands);
    tokio::pin!(interrupt);
    let mut error = None;
    let mut interrupted = false;
    let mut processed = 0;
    let mut sessions = 0;
    let mut closed = false;
    let mut contexts = (1..=count)
        .map(|worker_id| ParallelWorkerContext {
            worker_id,
            ..ParallelWorkerContext::default()
        })
        .collect::<Vec<_>>();
    let mut heartbeat = tokio::time::interval(PROGRESS_HEARTBEAT);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    while !futures.is_empty() || !closed {
        tokio::select! {
            biased;
            message = messages.recv(), if !closed => {
                let Some(message) = message else { closed = true; continue; };
                let handled = (|| -> Result<(), AcquisitionStreamError> {
                    match message {
                        ParallelMessage::Ready(worker) => {
                            state.mark_ready(worker).map_err(parallel_assignment_error)?;
                        },
                        ParallelMessage::Event(event, ack) => {
                            let worker = event.worker_id;
                            let slot = worker.checked_sub(1).filter(|slot| *slot < count)
                                .ok_or_else(|| parallel_protocol_error("неизвестный обработчик события"))?;
                            contexts[slot] = ParallelWorkerContext {
                                worker_id: worker, session: event.session,
                                browser_session: event.browser_session, attempt: event.attempt,
                                job: state.active(worker).cloned(),
                            };
                            let terminal = matches!(event.event, AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted { .. }));
                            let deferred = matches!(event.event, AcquisitionWorkerEventKind::Deferred { .. });
                            if terminal || deferred {
                                let job = accept_parallel_result(&mut state, &event)?;
                                if deferred { state.requeue(job); }
                                else { processed += 1; }
                            }
                            if matches!(event.event, AcquisitionWorkerEventKind::Provider(AcquisitionEvent::SessionStarted { .. })) { sessions += 1; }
                            if matches!(event.event, AcquisitionWorkerEventKind::WorkerRetired { .. } | AcquisitionWorkerEventKind::WorkerStopped { .. }) { state.retire(worker).map_err(parallel_assignment_error)?; }
                            // После отказа потребителя принятые результаты соседей продолжают сохраняться.
                            // Не ждём ошибки завершения задачи: сосед уже может прислать Ready.
                            // Неисправимое завершение прекращает назначения при обработке этого же сообщения.
                            let stopped_failure = stopped_parallel_failure(&event);
                            let requeue_reason = match &event.event {
                                AcquisitionWorkerEventKind::Deferred { reason } => reason.clone(),
                                _ => String::new(),
                            };
                            let requeued = deferred.then(|| AcquisitionWorkerEvent {
                                worker_id: event.worker_id, index: event.index,
                                generation: event.generation, owner_attempt: event.owner_attempt,
                                session: event.session, browser_session: event.browser_session,
                                attempt: event.attempt,
                                event: AcquisitionWorkerEventKind::Requeued { reason: requeue_reason },
                            });
                            deliver_parallel_event(on_event, event)?;
                            if let Some(requeued) = requeued { deliver_parallel_event(on_event, requeued)?; }
                            if let Some(ack) = ack { let _ = ack.send(()); }
                            if let Some(stopped_failure) = stopped_failure { return Err(stopped_failure); }
                        },
                    }
                    Ok(())
                })();
                if let Err(next) = handled {
                    if error.is_none() { error = Some(next); }
                    cancellation.request(CancellationReason::CoordinatorFailure);
                }
                // Сигнал, полученный при записи контрольной точки, проверяется до нового назначения.
                if !interrupted && let Some(signal) = interrupt.as_mut().now_or_never() {
                    interrupted = true;
                    let cancellation_reason = if matches!(&signal, AcquisitionStreamError::Interrupted) {
                        CancellationReason::Interrupted
                    } else {
                        CancellationReason::CoordinatorFailure
                    };
                    if error.is_none() { error = Some(signal); }
                    cancellation.request(cancellation_reason);
                    state.stop_dispatch();
                }
                if error.is_none()
                    && !interrupted
                    && let Err(next) = state.dispatch().map_err(parallel_assignment_error)
                {
                    error = Some(next);
                    cancellation.request(CancellationReason::CoordinatorFailure);
                }
                state.close_if_drained();
            },
            signal = &mut interrupt, if !interrupted => {
                interrupted = true;
                let cancellation_reason = if matches!(&signal, AcquisitionStreamError::Interrupted) {
                    CancellationReason::Interrupted
                } else {
                    CancellationReason::CoordinatorFailure
                };
                if error.is_none() { error = Some(signal); }
                cancellation.request(cancellation_reason);
                state.stop_dispatch();
            },
            _ = heartbeat.tick() => {
                let mut heartbeat_failed = false;
                for (worker, job) in state.active_jobs() {
                    let context = &contexts[worker - 1];
                    if context.job.as_ref() == Some(job) {
                        let event = context.event(AcquisitionWorkerEventKind::Provider(AcquisitionEvent::Heartbeat {
                            index: job.index, attempt: context.attempt,
                        }));
                        if let Err(next) = deliver_parallel_event(on_event, event) {
                            if error.is_none() { error = Some(next); }
                            heartbeat_failed = true;
                            break;
                        }
                    }
                }
                if heartbeat_failed {
                    cancellation.request(CancellationReason::CoordinatorFailure);
                    state.stop_dispatch();
                }
            },
            worker = futures.next(), if !futures.is_empty() => {
                if let Some((worker, result)) = worker {
                    let _ = state.retire(worker);
                    if let Err(next) = result {
                        if is_parallel_coordinator_cancellation(&next) {
                            continue;
                        }
                        error = Some(match error.take() {
                            Some(original @ (AcquisitionStreamError::Consumer(_) | AcquisitionStreamError::Interrupted)) => {
                                tracing::warn!(
                                    stage = "worker_stop",
                                    message = %crate::diagnostics::safe_message(&next.into_message()),
                                    "Вторичная ошибка обработчика после первичной"
                                );
                                original
                            }
                            Some(original) => AcquisitionStreamError::Provider(format!("{}; {}", original.into_message(), next.into_message())),
                            None => next,
                        });
                        cancellation.request(CancellationReason::CoordinatorFailure);
                        state.stop_dispatch();
                    }
                }
            },
        }
    }
    if let Some(error) = error {
        return Err(error);
    }
    let mut pending_indices = state.unfinished().map(|job| job.index).collect::<Vec<_>>();
    pending_indices.sort_unstable();
    if !pending_indices.is_empty() {
        return Err(AcquisitionStreamError::WorkersExhausted { pending_indices });
    }
    Ok(AcquisitionSummary {
        processed,
        sessions,
    })
}

impl AcquisitionRun {
    /// Получает одну границу обхода: раунды и CAS остаются у единственного потребителя.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn acquire_with_generations_parallel<F>(
        &mut self,
        characters: &[String],
        allow_insecure_tls: bool,
        generations: &[u32],
        owner_attempts: &[usize],
        workers: usize,
        mut on_event: F,
    ) -> Result<AcquisitionSummary, AcquisitionStreamError>
    where
        F: FnMut(AcquisitionWorkerEvent) -> Result<(), AssetError>,
    {
        if !(2..=4).contains(&workers)
            || characters.len() != generations.len()
            || characters.len() != owner_attempts.len()
            || owner_attempts.contains(&0)
        {
            return Err(parallel_protocol_error(
                "некорректное число обработчиков или токены владельца",
            ));
        }
        let mut unique = HashSet::new();
        let mut jobs = VecDeque::new();
        for (index, character) in characters.iter().enumerate() {
            crate::kanji_domain::parse_kanji_character(character)
                .map_err(AcquisitionStreamError::Provider)?;
            if !unique.insert(character) {
                return Err(parallel_protocol_error("повторная identity frontier"));
            }
            jobs.push_back(ParallelJob {
                index,
                character: character.clone(),
                generation: generations[index],
                owner_attempt: owner_attempts[index],
            });
        }
        if self.stopped {
            return Err(AcquisitionStreamError::Interrupted);
        }
        let result = self.runtime.block_on(async {
            if self.interrupt.is_finished() {
                return Err(signal_error((&mut self.interrupt).await));
            }
            let cancel = PoolCancellation::default();
            let drivers = (0..workers)
                .map(|_| {
                    let mut listener = cancel.subscribe();
                    let interrupt = tokio::spawn(async move {
                        match listener.cancelled().await {
                            CancellationReason::Interrupted => Ok(()),
                            reason @ CancellationReason::CoordinatorFailure => {
                                Err(std::io::Error::other(ParallelCancellation(reason)))
                            }
                        }
                    });
                    ParallelBrowserDriver {
                        workspace: None,
                        session: None,
                        monitor: None,
                        tls_exception: None,
                        deadline: Instant::now(),
                        session_items: 0,
                        interrupt,
                        allow_insecure_tls,
                        acceptance_rotate_after_items: self.acceptance_rotate_after_items,
                    }
                })
                .collect();
            coordinate_parallel(
                drivers,
                jobs,
                async { signal_error((&mut self.interrupt).await) },
                cancel,
                &mut on_event,
            )
            .await
        });
        if matches!(result, Err(AcquisitionStreamError::Interrupted))
            || self.interrupt.is_finished()
        {
            self.stopped = true;
        }
        result
    }
}

/// Передаёт единственный таймер элемента от браузера к потребителю результата.
/// Пакет завершает его после проверки и надёжной контрольной точки; самостоятельный
/// потребитель — после приёма результата получения.
pub struct AcquisitionItemDiagnostics {
    pub(crate) timing: BrowserItemTimer,
    acquisition_failure: Option<(String, bool)>,
}

impl std::fmt::Debug for AcquisitionItemDiagnostics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AcquisitionItemDiagnostics")
    }
}

impl AcquisitionItemDiagnostics {
    pub(crate) fn new(timing: BrowserItemTimer, failure: Option<(String, bool)>) -> Self {
        Self {
            timing,
            acquisition_failure: failure,
        }
    }

    pub fn finish_acquisition(self, outcome: &Result<AcquiredMedia, String>) {
        match outcome {
            Ok(_) => self.timing.finish_success(),
            Err(error) => self.finish_recorded_failure(acquisition_failure_code(error)),
        }
    }

    pub fn stage_timings(&self) -> Vec<crate::browser_diagnostics::BrowserStageTiming> {
        self.timing.stage_timings()
    }

    pub(crate) fn finish_recorded_failure(self, code: &str) {
        if let Some((failure, retryable)) = self.acquisition_failure {
            self.timing.finish_failure(&failure, retryable, None);
        } else {
            self.timing.finish_failure(code, false, None);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionStopReason {
    Deadline,
    ItemTimeout,
    RetryRecoveryFailed(String),
    #[cfg(feature = "session-rotation-acceptance")]
    ItemLimitReached,
    Interrupted,
    RuntimeFailure(String),
}

impl SessionStopReason {
    pub fn summary(&self) -> String {
        match self {
            Self::Deadline => "истёк срок сеанса браузера".into(),
            Self::ItemTimeout => {
                "превышен срок обработки элемента; сеанс браузера остановлен".into()
            }
            Self::RetryRecoveryFailed(error) => {
                format!("не удалось восстановить страницу: {error}")
            }
            #[cfg(feature = "session-rotation-acceptance")]
            Self::ItemLimitReached => {
                "достигнут предел элементов в приёмочном сеансе браузера".into()
            }
            Self::Interrupted => "получен Ctrl+C; сеанс браузера закрыт".into(),
            Self::RuntimeFailure(message) => format!("отказ среды сеанса браузера: {message}"),
        }
    }
}

fn session_stop_code(reason: &SessionStopReason) -> &'static str {
    match reason {
        SessionStopReason::Deadline => "session_deadline",
        SessionStopReason::ItemTimeout => "item_timeout",
        SessionStopReason::RetryRecoveryFailed(_) => "retry_recovery_failed",
        #[cfg(feature = "session-rotation-acceptance")]
        SessionStopReason::ItemLimitReached => "session_item_limit",
        SessionStopReason::Interrupted => "ctrl_c",
        SessionStopReason::RuntimeFailure(_) => "runtime_failure",
    }
}

fn terminal_failure_code<'a>(error: &'a str, stop: Option<&SessionStopReason>) -> &'a str {
    match stop {
        Some(SessionStopReason::Deadline) => "browser_session_deadline",
        Some(SessionStopReason::ItemTimeout) => "browser_item_timeout",
        Some(SessionStopReason::RetryRecoveryFailed(_)) => "browser_retry_recovery_failed",
        #[cfg(feature = "session-rotation-acceptance")]
        Some(SessionStopReason::ItemLimitReached) => "browser_session_item_limit",
        Some(SessionStopReason::Interrupted) => "acquisition_interrupted",
        Some(SessionStopReason::RuntimeFailure(_)) => "browser_network_runtime_failure",
        None => acquisition_failure_code(error),
    }
}

#[derive(Debug)]
pub enum AcquisitionStreamError {
    Provider(String),
    Consumer(AssetError),
    Interrupted,
    WorkersExhausted { pending_indices: Vec<usize> },
}

impl AcquisitionStreamError {
    fn message(&self) -> String {
        match self {
            Self::Provider(message) => message.clone(),
            Self::Consumer(error) => format!("progress_consumer_failed: {error}"),
            Self::Interrupted => "acquisition_interrupted: получен Ctrl+C".into(),
            Self::WorkersExhausted { pending_indices } => {
                format!("browser_workers_exhausted: незавершённые индексы {pending_indices:?}")
            }
        }
    }

    fn into_message(self) -> String {
        match self {
            Self::Provider(message) => message,
            error => error.message(),
        }
    }
}

impl From<AssetError> for AcquisitionStreamError {
    fn from(error: AssetError) -> Self {
        Self::Consumer(error)
    }
}

fn signal_error(
    signal: Result<std::io::Result<()>, tokio::task::JoinError>,
) -> AcquisitionStreamError {
    match signal {
        Ok(Ok(())) => accepted_ctrl_c(),
        Ok(Err(error)) => {
            if let Some(cancellation) = error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<ParallelCancellation>())
            {
                parallel_cancellation_error(cancellation.0)
            } else {
                AcquisitionStreamError::Provider(format!("ctrl_c_listener_failed: {error}"))
            }
        }
        Err(error) => AcquisitionStreamError::Provider(format!("ctrl_c_listener_failed: {error}")),
    }
}

fn accepted_ctrl_c() -> AcquisitionStreamError {
    tracing::info!(
        stage = "interrupt",
        code = "acquisition_interrupted",
        "Принятый Ctrl+C подтверждён на границе получения ресурса"
    );
    AcquisitionStreamError::Interrupted
}

fn parallel_cancellation_error(reason: CancellationReason) -> AcquisitionStreamError {
    match reason {
        CancellationReason::Interrupted => accepted_ctrl_c(),
        CancellationReason::CoordinatorFailure => {
            AcquisitionStreamError::Provider(PARALLEL_COORDINATOR_CANCELLED.into())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcquisitionSummary {
    pub processed: usize,
    pub sessions: u32,
}

/// Потоково получает медиа, закрывая BrowserSession на границе её срока.
/// Незатронутый хвост продолжается в новом сеансе без фиктивных результатов.
/// Отказ монитора останавливает запуск и оставляет хвост для продолжения.
pub fn acquire_many_stream<F>(
    characters: &[String],
    allow_insecure_tls: bool,
    on_event: F,
) -> Result<AcquisitionSummary, AcquisitionStreamError>
where
    F: FnMut(AcquisitionEvent) -> Result<(), AssetError>,
{
    acquire_many_stream_with_target(
        characters,
        allow_insecure_tls,
        AcquisitionTarget::PreferredSource,
        on_event,
    )
}

pub fn acquire_many_stream_with_target<F>(
    characters: &[String],
    allow_insecure_tls: bool,
    target: AcquisitionTarget,
    on_event: F,
) -> Result<AcquisitionSummary, AcquisitionStreamError>
where
    F: FnMut(AcquisitionEvent) -> Result<(), AssetError>,
{
    let mut run = AcquisitionRun::new()?;
    let result = run.acquire_with_target(characters, allow_insecure_tls, target, on_event);
    finish_acquisition_run(result, run.close())
}

/// Заимствует единое временное дерево acceptance, сохраняя исходную границу Ctrl+C.
pub fn acquire_many_stream_with_target_in_workspace<F>(
    characters: &[String],
    allow_insecure_tls: bool,
    target: AcquisitionTarget,
    workspace: &Path,
    on_event: F,
) -> Result<AcquisitionSummary, AcquisitionStreamError>
where
    F: FnMut(AcquisitionEvent) -> Result<(), AssetError>,
{
    let mut run = AcquisitionRun::new_in_workspace(workspace)?;
    let result = run.acquire_with_target(characters, allow_insecure_tls, target, on_event);
    finish_acquisition_run(result, run.close())
}

fn finish_acquisition_run(
    result: Result<AcquisitionSummary, AcquisitionStreamError>,
    cleanup: Result<(), AcquisitionStreamError>,
) -> Result<AcquisitionSummary, AcquisitionStreamError> {
    match cleanup {
        Ok(()) => result,
        Err(error) => Err(AcquisitionStreamError::Provider(match result {
            Ok(_) => error.into_message(),
            Err(original) => format!("{}; {}", original.into_message(), error.into_message()),
        })),
    }
}

/// Сохраняет один обработчик Ctrl+C на весь запуск, включая несколько раундов.
/// Принятый сигнал остаётся остановкой до уничтожения этого объекта.
pub struct AcquisitionRun {
    runtime: tokio::runtime::Runtime,
    interrupt: CtrlCListener,
    stopped: bool,
    workspace: PathBuf,
    owned_workspace: Option<TempWorkspace>,
    acceptance_rotate_after_items: Option<usize>,
    #[cfg(test)]
    test_session_attempts: Option<VecDeque<ScriptedSessionAttempt>>,
}

impl AcquisitionRun {
    pub fn new() -> Result<Self, AcquisitionStreamError> {
        let workspace = yarxi_workspace("yarxi-acquisition").map_err(|error| {
            AcquisitionStreamError::Provider(format!("temp_workspace_create_failed: {error}"))
        })?;
        Self::with_workspace(workspace.path().to_path_buf(), Some(workspace))
    }

    pub fn new_in_workspace(workspace: &Path) -> Result<Self, AcquisitionStreamError> {
        Self::with_workspace(workspace.to_path_buf(), None)
    }

    #[cfg(feature = "session-rotation-acceptance")]
    pub(crate) fn new_for_acceptance_rotation(
        max_items: usize,
    ) -> Result<Self, AcquisitionStreamError> {
        let workspace = yarxi_workspace("yarxi-acquisition").map_err(|error| {
            AcquisitionStreamError::Provider(format!("temp_workspace_create_failed: {error}"))
        })?;
        Self::with_workspace_and_rotation_limit(
            workspace.path().to_path_buf(),
            Some(workspace),
            Some(max_items),
        )
    }

    fn with_workspace(
        workspace: PathBuf,
        owned_workspace: Option<TempWorkspace>,
    ) -> Result<Self, AcquisitionStreamError> {
        Self::with_workspace_and_rotation_limit(workspace, owned_workspace, None)
    }

    fn with_workspace_and_rotation_limit(
        workspace: PathBuf,
        owned_workspace: Option<TempWorkspace>,
        acceptance_rotate_after_items: Option<usize>,
    ) -> Result<Self, AcquisitionStreamError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| {
                AcquisitionStreamError::Provider(format!("среда браузера: {error}"))
            })?;
        let interrupt = runtime.spawn(crate::acquisition_pool::termination_signal());
        Ok(Self {
            runtime,
            interrupt,
            stopped: false,
            workspace,
            owned_workspace,
            acceptance_rotate_after_items,
            #[cfg(test)]
            test_session_attempts: None,
        })
    }

    /// Все сессии уже закрыты при получении; заимствованный владелец остаётся у вызывающего кода.
    pub fn close(mut self) -> Result<(), AcquisitionStreamError> {
        self.interrupt.abort();
        self.owned_workspace.take().map_or(Ok(()), TempWorkspace::close).map_err(|error| {
            tracing::error!(stage = "temp_cleanup", code = "temp_workspace_cleanup_failed", message = %crate::diagnostics::safe_message(&error.to_string()), "Не удалось удалить временное дерево Yarxi");
            AcquisitionStreamError::Provider(format!("temp_workspace_cleanup_failed: {error}"))
        })
    }

    pub fn is_stopped(&self) -> bool {
        self.stopped || self.interrupt.is_finished()
    }

    pub fn acquire<F>(
        &mut self,
        characters: &[String],
        allow_insecure_tls: bool,
        on_event: F,
    ) -> Result<AcquisitionSummary, AcquisitionStreamError>
    where
        F: FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    {
        self.acquire_with_target(
            characters,
            allow_insecure_tls,
            AcquisitionTarget::PreferredSource,
            on_event,
        )
    }

    fn acquire_with_target<F>(
        &mut self,
        characters: &[String],
        allow_insecure_tls: bool,
        target: AcquisitionTarget,
        on_event: F,
    ) -> Result<AcquisitionSummary, AcquisitionStreamError>
    where
        F: FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    {
        self.acquire_with_target_and_generations(
            characters,
            allow_insecure_tls,
            target,
            None,
            on_event,
        )
    }

    /// Контекст владельца пакета сопровождает идентичность при смене сеансов,
    /// чтобы диагностика элемента сохраняла точное поколение полученного CAS.
    pub(crate) fn acquire_with_generations<F>(
        &mut self,
        characters: &[String],
        allow_insecure_tls: bool,
        generations: &[u32],
        on_event: F,
    ) -> Result<AcquisitionSummary, AcquisitionStreamError>
    where
        F: FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    {
        self.acquire_with_target_and_generations(
            characters,
            allow_insecure_tls,
            AcquisitionTarget::PreferredSource,
            Some(generations),
            on_event,
        )
    }

    fn acquire_with_target_and_generations<F>(
        &mut self,
        characters: &[String],
        allow_insecure_tls: bool,
        target: AcquisitionTarget,
        generations: Option<&[u32]>,
        mut on_event: F,
    ) -> Result<AcquisitionSummary, AcquisitionStreamError>
    where
        F: FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    {
        if self.stopped {
            return Err(AcquisitionStreamError::Interrupted);
        }
        if generations.is_some_and(|values| values.len() != characters.len()) {
            return Err(AcquisitionStreamError::Provider(
                "browser_item_context_mismatch: число generation не совпадает с очередью".into(),
            ));
        }
        for character in characters {
            crate::kanji_domain::parse_kanji_character(character).map_err(|error| {
                AcquisitionStreamError::Provider(format!("invalid_kanji_identity: {error}"))
            })?;
        }
        let runtime = &self.runtime;
        let workspace = self.workspace.as_path();
        let acceptance_rotate_after_items = self.acceptance_rotate_after_items;
        let interrupt = &mut self.interrupt;
        #[cfg(test)]
        let test_session_attempts = &mut self.test_session_attempts;
        let result = runtime.block_on(async {
            if interrupt.is_finished() {
                return Err(signal_error((&mut *interrupt).await));
            }
            let mut offset = 0;
            let mut sessions = 0_u32;
            let mut setup_recreations = 0_u8;
            while offset < characters.len() {
                let session_start = offset;
                sessions = sessions.saturating_add(1);
                let session_config = SessionAcquisitionConfig {
                    characters: &characters[offset..],
                    base_index: offset,
                    session_number: sessions,
                    acceptance_rotate_after_items,
                    allow_insecure_tls,
                    target,
                    workspace,
                    generations,
                };
                #[cfg(test)]
                let session_result = match test_session_attempts.as_mut() {
                    Some(attempts) => run_scripted_session_attempt(
                        attempts,
                        &session_config,
                        &mut on_event,
                    ),
                    None => {
                        acquire_one_session(session_config, &mut on_event, interrupt).await
                    }
                };
                #[cfg(not(test))]
                let session_result =
                    acquire_one_session(session_config, &mut on_event, interrupt).await;

                let session_result = match session_result {
                    Ok(summary) => {
                        setup_recreations = 0;
                        summary
                    }
                    Err(AcquisitionStreamError::Provider(message))
                        if message.starts_with(SESSION_SETUP_RUNTIME_FAILURE_PREFIX) =>
                    {
                        let Some(next_recreation) =
                            next_session_setup_recreation(setup_recreations)
                        else {
                            return Err(AcquisitionStreamError::Provider(format!(
                                "browser_session_setup_recovery_exhausted: после {MAX_SESSION_SETUP_RECREATIONS} пересозданий незатронутая часть очереди не изменилась; последняя причина: {message}"
                            )));
                        };
                        setup_recreations = next_recreation;
                        tracing::warn!(
                            session = sessions,
                            recreation = setup_recreations,
                            max_recreations = MAX_SESSION_SETUP_RECREATIONS,
                            code = "browser_session_setup_recreated",
                            reason = %crate::diagnostics::safe_message(&message),
                            pending = characters.len() - offset,
                            "Новая сессия браузера не прошла проверку готовности; очередь сохранена для повторной подготовки"
                        );
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                tracing::info!(session = sessions, processed = session_result.processed, pending = characters.len() - offset - session_result.processed, stage = "session_stop", code = "browser_session_ended", reason = %session_result.stop_reason.as_ref().map_or_else(|| "complete".to_owned(), |reason| crate::diagnostics::safe_message(&reason.summary())), "Сеанс завершён; счётчики отражают только реальные результаты");
                on_event(AcquisitionEvent::SessionEnded {
                    session: sessions,
                    processed: session_result.processed,
                    stop_reason: session_result.stop_reason.clone(),
                })
                .map_err(AcquisitionStreamError::Consumer)?;
                match session_action(session_start, characters.len(), &session_result)? {
                    SessionAction::Complete => {
                        offset = characters.len();
                        break;
                    }
                    SessionAction::Interrupted { .. } => return Err(AcquisitionStreamError::Interrupted),
                    SessionAction::Rotate { next_offset, reason } => {
                        offset = next_offset;
                        tracing::info!(session = sessions, next_session = sessions.saturating_add(1), processed = offset, stage = "session_rotation", code = "browser_session_rotated", reason = %crate::diagnostics::safe_message(&reason.summary()), "Продолжение очереди в новом сеансе");
                        on_event(AcquisitionEvent::SessionRotated {
                            next_session: sessions.saturating_add(1), reason,
                        })
                        .map_err(AcquisitionStreamError::Consumer)?;
                    }
                }
            }
            if interrupt.is_finished() {
                return Err(signal_error((&mut *interrupt).await));
            }
            Ok(AcquisitionSummary { processed: offset, sessions })
        });
        // После завершения обработчика сигнала stop сохраняется для следующих раундов.
        if matches!(result, Err(AcquisitionStreamError::Interrupted))
            || self.interrupt.is_finished()
        {
            self.stopped = true;
        }
        result
    }
}

impl Drop for AcquisitionRun {
    fn drop(&mut self) {
        self.interrupt.abort();
    }
}

struct SessionAcquisitionSummary {
    processed: usize,
    stop_reason: Option<SessionStopReason>,
}

struct SessionAcquisitionConfig<'a> {
    characters: &'a [String],
    base_index: usize,
    session_number: u32,
    acceptance_rotate_after_items: Option<usize>,
    allow_insecure_tls: bool,
    target: AcquisitionTarget,
    workspace: &'a Path,
    generations: Option<&'a [u32]>,
}

#[cfg(test)]
enum ScriptedSessionAttempt {
    SetupFailure(String),
    Processed {
        count: usize,
        stop_reason: Option<SessionStopReason>,
    },
}

#[cfg(test)]
fn run_scripted_session_attempt(
    attempts: &mut VecDeque<ScriptedSessionAttempt>,
    config: &SessionAcquisitionConfig<'_>,
    on_event: &mut impl FnMut(AcquisitionEvent) -> Result<(), AssetError>,
) -> Result<SessionAcquisitionSummary, AcquisitionStreamError> {
    match attempts
        .pop_front()
        .ok_or_else(|| AcquisitionStreamError::Provider("test_session_attempts_exhausted".into()))?
    {
        ScriptedSessionAttempt::SetupFailure(message) => {
            Err(AcquisitionStreamError::Provider(message))
        }
        ScriptedSessionAttempt::Processed { count, stop_reason } => {
            if count > config.characters.len() {
                return Err(AcquisitionStreamError::Provider(
                    "test_session_processed_count_exceeds_frontier".into(),
                ));
            }
            on_event(AcquisitionEvent::SessionStarted {
                session: config.session_number,
            })?;
            for offset in 0..count {
                let index = config.base_index + offset;
                on_event(AcquisitionEvent::ItemStarted { index })?;
                on_event(AcquisitionEvent::ItemCompleted {
                    index,
                    outcome: Box::new(Err("gif_unknown: тестовый исход".into())),
                    diagnostics: None,
                })?;
            }
            Ok(SessionAcquisitionSummary {
                processed: count,
                stop_reason,
            })
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SessionAction {
    Complete,
    Interrupted {
        next_offset: usize,
    },
    Rotate {
        next_offset: usize,
        reason: SessionStopReason,
    },
}

fn session_action(
    offset: usize,
    total: usize,
    summary: &SessionAcquisitionSummary,
) -> Result<SessionAction, AcquisitionStreamError> {
    let next_offset = offset
        .checked_add(summary.processed)
        .filter(|next| *next <= total)
        .ok_or_else(|| {
            AcquisitionStreamError::Provider(
                "browser_session_invalid_progress: число обработанных элементов превышает очередь"
                    .into(),
            )
        })?;
    if summary.stop_reason == Some(SessionStopReason::Interrupted) {
        return Ok(SessionAction::Interrupted { next_offset });
    }
    if let Some(SessionStopReason::RuntimeFailure(message)) = &summary.stop_reason {
        tracing::error!(processed = summary.processed, pending = total - next_offset, stage = "session_stop", code = "browser_session_runtime_failure", message = %crate::diagnostics::safe_message(message), "Отказ сеанса; незапущенный хвост остаётся без попыток");
        return Err(AcquisitionStreamError::Provider(message.clone()));
    }
    if next_offset == total {
        return Ok(SessionAction::Complete);
    }
    match (&summary.stop_reason, summary.processed) {
        (Some(reason), 1..) => Ok(SessionAction::Rotate {
            next_offset,
            reason: reason.clone(),
        }),
        (Some(reason), 0) => Err(AcquisitionStreamError::Provider(format!(
            "browser_session_no_progress: {reason:?}"
        ))),
        (None, _) => Err(AcquisitionStreamError::Provider(
            "browser_session_incomplete: сеанс завершился без результата для хвоста".into(),
        )),
    }
}

/// Обе границы запуска используют один TLS/readiness barrier.
async fn prepare_yarxi_session(
    session: &BrowserSession,
    evidence_monitor: &BrowserEvidenceMonitor,
    allow_insecure_tls: bool,
    target: AcquisitionTarget,
) -> Result<Option<ApprovedTlsException>, String> {
    let page = session.page();
    let tls_exception = match page.goto(SITE_URL).await {
        Err(error) => Some(
            async_error_or_tls_interstitial(page, evidence_monitor, allow_insecure_tls, error)
                .await?,
        ),
        Ok(_) => {
            let tls_probe: Value = page
            .evaluate("() => ({ code: document.querySelector('#error-code')?.textContent?.trim() || '', proceed: Boolean(document.querySelector('#proceed-link')) })")
            .await
            .map_err(|error| format!("проверка TLS-страницы: {error}"))?
            .into_value()
            .map_err(|error| format!("ответ проверки TLS-страницы: {error}"))?;
            if !tls_probe["code"].as_str().unwrap_or_default().is_empty() {
                Some(
                    async_error_or_tls_interstitial(
                        page,
                        evidence_monitor,
                        allow_insecure_tls,
                        tls_probe["code"].as_str().unwrap_or_default(),
                    )
                    .await?,
                )
            } else {
                None
            }
        }
    };
    let site_host: bool = page
        .evaluate(format!("location.hostname === {SITE_HOST:?}"))
        .await
        .map_err(|error| format!("проверка узла Yarxi: {error}"))?
        .into_value()
        .map_err(|error| format!("ответ проверки узла Yarxi: {error}"))?;
    if !site_host {
        return Err("provider_host_mismatch: загрузка ушла с www.yarxi.su".into());
    }
    wait_until(page, Duration::from_secs(20), || {
        "Boolean([...document.querySelectorAll('.kanji-search-form input[placeholder=\"Чтение\"]')].find(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden'))"
    })
    .await?;
    if let Some(approved) = &tls_exception {
        evidence_monitor.clear_explicitly_approved_tls_interstitial_failure(
            &approved.request_id,
            &approved.blocked_url,
        );
    }
    if target == AcquisitionTarget::RenderedFontSamplePng {
        apply_dark_theme(page).await?;
    }
    wait_for_session_setup_readiness(evidence_monitor).await?;
    Ok::<_, String>(tls_exception)
}

async fn acquire_one_session(
    config: SessionAcquisitionConfig<'_>,
    on_event: &mut impl FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    interrupt: &mut CtrlCListener,
) -> Result<SessionAcquisitionSummary, AcquisitionStreamError> {
    let SessionAcquisitionConfig {
        characters,
        base_index,
        session_number,
        acceptance_rotate_after_items,
        allow_insecure_tls,
        target,
        workspace,
        generations,
    } = config;
    let session_deadline = Instant::now() + OPERATION_TIMEOUT;
    let device_metrics = DeviceMetrics::new(
        CAPTURE_VIEWPORT_WIDTH,
        CAPTURE_VIEWPORT_HEIGHT,
        CAPTURE_DEVICE_SCALE_FACTOR,
    )
    .map_err(|error| {
        AcquisitionStreamError::Provider(format!("настройка области просмотра Yarxi: {error}"))
    })?;
    let runtime_config = BrowserRuntimeConfig {
        device_metrics: Some(device_metrics),
        prefers_color_scheme: Some("dark".into()),
        ..BrowserRuntimeConfig::default()
    };
    let session_started_at = Instant::now();
    tracing::info!(
        session = session_number,
        stage = "browser_launch",
        code = "browser_session_starting",
        "Запуск и подготовка браузера Yarxi"
    );
    // Запуск имеет собственные таймауты; получаем владельца до проверки срока,
    // чтобы отмена не опередила явное завершение процесса и уборку профиля.
    let session = match BrowserSession::launch_in_workspace(runtime_config, workspace).await {
        Ok(session) => session,
        Err(error) => {
            let message = error.to_string();
            tracing::error!(session = session_number, stage = "browser_launch", code = "browser_session_launch_failed", message = %crate::diagnostics::safe_message(&message), elapsed_ms = session_started_at.elapsed().as_millis() as u64, "Не удалось запустить браузер");
            return finish_session_setup(
                Err(AcquisitionStreamError::Provider(message)),
                session_number,
                on_event,
            );
        }
    };
    let evidence_monitor = BrowserEvidenceMonitor::new(session.telemetry().clone());
    let browser_runtime = session.provenance().clone();

    let setup_started_at = Instant::now();
    let setup = run_setup_before_deadline(session_deadline, async {
        prepare_yarxi_session(&session, &evidence_monitor, allow_insecure_tls, target).await
    });
    let setup_result = tokio::select! {
        result = setup => Ok(result),
        signal = &mut *interrupt => Err(signal),
    };
    let setup_result = match setup_result {
        Ok(Ok(setup)) => Ok(setup),
        Ok(Err(message)) => {
            tracing::error!(
                provider = PROVIDER_ID,
                session = session_number,
                browser_session = session.diagnostic_session_id(),
                stage = "browser_setup",
                code = "browser_session_setup_failed",
                failure_code = acquisition_failure_code(&message),
                outcome = "failure",
                retryable = message.starts_with(SESSION_SETUP_RUNTIME_FAILURE_PREFIX),
                stage_duration_ms = setup_started_at.elapsed().as_millis() as u64,
                runtime_snapshot = %runtime_snapshot_json(&evidence_monitor.telemetry.snapshot(0)),
                "Не удалось подготовить страницу Yarxi"
            );
            Err(AcquisitionStreamError::Provider(message))
        }
        Err(signal) => {
            tracing::info!(
                provider = PROVIDER_ID,
                session = session_number,
                browser_session = session.diagnostic_session_id(),
                stage = "browser_setup",
                failure_code = "acquisition_interrupted",
                outcome = "interrupted",
                retryable = false,
                stop_reason = "ctrl_c",
                stage_duration_ms = setup_started_at.elapsed().as_millis() as u64,
                runtime_snapshot = %runtime_snapshot_json(&evidence_monitor.telemetry.snapshot(0)),
                "Подготовка страницы Yarxi прервана"
            );
            Err(signal_error(signal))
        }
    };
    let tls_exception = match finish_session_setup(setup_result, session_number, on_event) {
        Ok(setup) => setup,
        Err(error) => {
            let cleanup = session.close().await;
            match &cleanup {
                Ok(()) => tracing::info!(
                    session = session_number,
                    stage = "browser_close",
                    code = "browser_session_closed",
                    elapsed_ms = session_started_at.elapsed().as_millis() as u64,
                    "Браузер закрыт после отказа подготовки"
                ),
                Err(cleanup_error) => tracing::error!(
                    session = session_number,
                    stage = "browser_close",
                    code = "browser_session_close_failed",
                    outcome = "failure",
                    error = %crate::diagnostics::safe_message(cleanup_error),
                    elapsed_ms = session_started_at.elapsed().as_millis() as u64,
                    "Не удалось закрыть браузер после отказа подготовки"
                ),
            }
            return Err(match cleanup {
                Ok(()) => error,
                Err(cleanup) => {
                    let message = error.into_message();
                    if message.starts_with(SESSION_SETUP_RUNTIME_FAILURE_PREFIX) {
                        AcquisitionStreamError::Provider(format!(
                            "browser_session_setup_cleanup_failed: {message}; {cleanup}"
                        ))
                    } else {
                        AcquisitionStreamError::Provider(format!("{message}; {cleanup}"))
                    }
                }
            });
        }
    };
    tracing::info!(
        session = session_number,
        stage = "browser_setup",
        code = "browser_session_started",
        stage_duration_ms = setup_started_at.elapsed().as_millis() as u64,
        elapsed_ms = session_started_at.elapsed().as_millis() as u64,
        "Браузер и страница Yarxi подготовлены"
    );

    let mut driver = BrowserSessionItemDriver {
        context: RetryAcquisitionContext {
            page: session.page(),
            target,
            evidence_monitor: &evidence_monitor,
            browser_runtime: &browser_runtime,
            tls_exception: tls_exception.map(|approved| approved.provenance),
            session_deadline,
            session: session_number,
            browser_session: session.diagnostic_session_id(),
            interrupt,
        },
        session: session_number,
        generations,
        worker: None,
        diagnostics: None,
    };
    let result = process_session_items(
        characters,
        base_index,
        acceptance_rotate_after_items,
        &mut driver,
        on_event,
    )
    .await;
    evidence_monitor.abort();
    let cleanup = session.close().await;
    match &cleanup {
        Ok(()) => tracing::info!(
            session = session_number,
            stage = "browser_close",
            code = "browser_session_closed",
            elapsed_ms = session_started_at.elapsed().as_millis() as u64,
            "Браузер Yarxi закрыт"
        ),
        Err(cleanup_error) => tracing::error!(
            session = session_number,
            stage = "browser_close",
            code = "browser_session_close_failed",
            outcome = "failure",
            error = %crate::diagnostics::safe_message(cleanup_error),
            elapsed_ms = session_started_at.elapsed().as_millis() as u64,
            "Не удалось закрыть браузер Yarxi"
        ),
    }
    match cleanup {
        Ok(()) => result,
        Err(cleanup) => Err(AcquisitionStreamError::Provider(match result {
            Ok(_) => cleanup,
            Err(error) => format!("{}; {cleanup}", error.into_message()),
        })),
    }
}

fn finish_session_setup<T>(
    result: Result<T, AcquisitionStreamError>,
    session: u32,
    on_event: &mut impl FnMut(AcquisitionEvent) -> Result<(), AssetError>,
) -> Result<T, AcquisitionStreamError> {
    let setup = result?;
    on_event(AcquisitionEvent::SessionStarted { session })?;
    Ok(setup)
}

/// Узкая граница браузера позволяет проверять очередь без сети и CDP.
trait SessionItemDriver {
    async fn before_item(
        &mut self,
        index: usize,
    ) -> Result<Option<SessionStopReason>, AcquisitionStreamError>;
    async fn acquire(
        &mut self,
        index: usize,
        character: &str,
        on_event: &mut impl FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    ) -> Result<(Result<AcquiredMedia, String>, Option<SessionStopReason>), AcquisitionStreamError>;
    fn take_diagnostics(&mut self) -> Option<AcquisitionItemDiagnostics> {
        None
    }
    async fn after_checkpoint(
        &mut self,
    ) -> Result<Option<SessionStopReason>, AcquisitionStreamError>;
}

async fn process_session_items(
    characters: &[String],
    base_index: usize,
    acceptance_rotate_after_items: Option<usize>,
    driver: &mut impl SessionItemDriver,
    on_event: &mut impl FnMut(AcquisitionEvent) -> Result<(), AssetError>,
) -> Result<SessionAcquisitionSummary, AcquisitionStreamError> {
    #[cfg(not(feature = "session-rotation-acceptance"))]
    let _ = acceptance_rotate_after_items;
    let mut processed = 0;
    let mut stop_reason = None;
    let result = async {
        for (index, character) in characters.iter().enumerate() {
            if let Some(reason) = driver.before_item(index).await? {
                stop_reason = Some(reason);
                break;
            }
            let absolute_index = base_index + index;
            on_event(AcquisitionEvent::ItemStarted {
                index: absolute_index,
            })?;
            let (outcome, item_stop_reason) =
                driver.acquire(absolute_index, character, on_event).await?;
            on_event(AcquisitionEvent::ItemCompleted {
                index: absolute_index,
                outcome: Box::new(outcome),
                diagnostics: driver.take_diagnostics(),
            })?;
            processed += 1;
            // Сначала потребитель фиксирует готовый результат, затем признаём stop.
            let checkpoint_stop = driver.after_checkpoint().await?;
            if checkpoint_stop.is_some() || item_stop_reason.is_some() {
                stop_reason = checkpoint_stop.or(item_stop_reason);
                break;
            }
            #[cfg(feature = "session-rotation-acceptance")]
            if index + 1 < characters.len()
                && acceptance_rotate_after_items.is_some_and(|limit| processed >= limit)
            {
                stop_reason = Some(SessionStopReason::ItemLimitReached);
                break;
            }
        }
        Ok::<_, AcquisitionStreamError>(())
    }
    .await;
    match result {
        Ok(()) => Ok(SessionAcquisitionSummary {
            processed,
            stop_reason,
        }),
        Err(AcquisitionStreamError::Interrupted) => Ok(SessionAcquisitionSummary {
            processed,
            stop_reason: Some(SessionStopReason::Interrupted),
        }),
        Err(error) => Err(error),
    }
}

struct BrowserSessionItemDriver<'a> {
    context: RetryAcquisitionContext<'a>,
    session: u32,
    generations: Option<&'a [u32]>,
    worker: Option<(u64, u64)>,
    diagnostics: Option<AcquisitionItemDiagnostics>,
}

/// Только смысловые границы текущего пути вызовов Yarxi.
#[derive(Debug, Clone, Copy)]
enum YarxiStage {
    SearchSubmission,
    InformationTab,
    ArticleVerification,
    MediaReadiness,
    MediaSelection,
    DarkTheme,
    ResourceRead,
    FontSampleCapture,
    RetryBackoff,
    RetryRecovery,
}

impl YarxiStage {
    fn name(self) -> &'static str {
        match self {
            Self::SearchSubmission => "search_submission",
            Self::InformationTab => "information_tab",
            Self::ArticleVerification => "article_verification",
            Self::MediaReadiness => "media_readiness",
            Self::MediaSelection => "media_selection",
            Self::DarkTheme => "dark_theme",
            Self::ResourceRead => "resource_read",
            Self::FontSampleCapture => "font_sample_capture",
            Self::RetryBackoff => "retry_backoff",
            Self::RetryRecovery => "retry_recovery",
        }
    }
}

async fn diagnose_yarxi_stage<T>(
    item: &BrowserItemTimer,
    stage: YarxiStage,
    monitor: &BrowserEvidenceMonitor,
    epoch: u64,
    operation: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    let timing = item.stage(stage.name());
    let result = operation.await;
    match &result {
        Ok(_) => timing.finish_success(),
        Err(error) => timing.finish_failure(
            acquisition_failure_code(error),
            is_retryable_acquisition_error(error, monitor, epoch),
            Some(&monitor.telemetry.snapshot(epoch)),
        ),
    }
    result
}

struct YarxiAttemptDiagnostics<'a> {
    timing: &'a BrowserItemTimer,
    monitor: &'a BrowserEvidenceMonitor,
    epoch: u64,
}

impl SessionItemDriver for BrowserSessionItemDriver<'_> {
    async fn before_item(
        &mut self,
        index: usize,
    ) -> Result<Option<SessionStopReason>, AcquisitionStreamError> {
        if let Some(reason) = self.after_checkpoint().await? {
            return Ok(Some(reason));
        }
        if index > 0 {
            tokio::select! {
                result = timeout_at(self.context.session_deadline, sleep(BATCH_PACING)) => {
                    if result.is_err() { return Ok(Some(SessionStopReason::Deadline)); }
                }
                signal = &mut *self.context.interrupt => return Err(signal_error(signal)),
            }
        }
        if Instant::now() >= self.context.session_deadline {
            return Ok(Some(SessionStopReason::Deadline));
        }
        self.after_checkpoint().await
    }

    async fn acquire(
        &mut self,
        index: usize,
        character: &str,
        on_event: &mut impl FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    ) -> Result<(Result<AcquiredMedia, String>, Option<SessionStopReason>), AcquisitionStreamError>
    {
        let owner_generation = self
            .generations
            .and_then(|generations| generations.get(index))
            .copied();
        let item_context = match owner_generation {
            Some(generation) => {
                BrowserItemContext::new(PROVIDER_ID, character, 1, u64::from(generation))
            }
            None => BrowserItemContext::without_generation(PROVIDER_ID, character, 1),
        }
        .with_browser_session(self.context.browser_session);
        let item_context = match self.worker {
            Some((worker, session)) => item_context.with_worker(worker, session),
            None => item_context,
        };
        let timing = BrowserItemTimer::new(item_context);
        tracing::info!(
            session = self.session,
            identity = character,
            index,
            attempt = 1,
            stage = "acquisition",
            code = "item_started",
            "Получение изображения кандзи"
        );
        let result = acquire_one_with_retries(
            RetryAcquisitionContext {
                page: self.context.page,
                target: self.context.target,
                evidence_monitor: self.context.evidence_monitor,
                browser_runtime: self.context.browser_runtime,
                tls_exception: self.context.tls_exception.clone(),
                session_deadline: self.context.session_deadline,
                session: self.session,
                browser_session: self.context.browser_session,
                interrupt: &mut *self.context.interrupt,
            },
            index,
            character,
            on_event,
            &timing,
        )
        .await;
        match &result {
            Ok((Ok(_), _)) => {
                self.diagnostics = Some(AcquisitionItemDiagnostics::new(timing, None));
            }
            Ok((Err(error), stop)) => {
                if let Some(reason) = stop {
                    timing.set_stop_reason(session_stop_code(reason));
                }
                let snapshot = self.context.evidence_monitor.telemetry.current_snapshot();
                let retryable = stop.is_none() || *stop == Some(SessionStopReason::ItemTimeout);
                timing.record_runtime_snapshot(&snapshot);
                let failure = (
                    terminal_failure_code(error, stop.as_ref()).to_owned(),
                    retryable
                        && is_retryable_acquisition_error(
                            error,
                            self.context.evidence_monitor,
                            snapshot.epoch,
                        ),
                );
                self.diagnostics = Some(AcquisitionItemDiagnostics::new(timing, Some(failure)));
            }
            Err(AcquisitionStreamError::Interrupted) => timing.interrupt("ctrl_c", None),
            Err(AcquisitionStreamError::Provider(error))
                if error == PARALLEL_COORDINATOR_CANCELLED =>
            {
                timing.interrupt("coordinator_failure", None)
            }
            Err(AcquisitionStreamError::Consumer(_)) => {
                timing.interrupt("progress_consumer_failed", None)
            }
            Err(AcquisitionStreamError::Provider(error)) => {
                timing.finish_failure(acquisition_failure_code(error), false, None)
            }
            Err(AcquisitionStreamError::WorkersExhausted { .. }) => {
                timing.finish_failure("browser_workers_exhausted", false, None)
            }
        }
        result
    }

    fn take_diagnostics(&mut self) -> Option<AcquisitionItemDiagnostics> {
        self.diagnostics.take()
    }

    async fn after_checkpoint(
        &mut self,
    ) -> Result<Option<SessionStopReason>, AcquisitionStreamError> {
        if self.context.interrupt.is_finished() {
            return Err(signal_error((&mut *self.context.interrupt).await));
        }
        Ok(monitor_stop_reason(self.context.evidence_monitor))
    }
}

fn monitor_stop_reason(monitor: &BrowserEvidenceMonitor) -> Option<SessionStopReason> {
    monitor.telemetry.monitor_failed().then(|| {
        let message = format!(
            "browser_network_runtime_failure: {}",
            runtime_failure_reason(monitor.readiness(0, None))
        );
        SessionStopReason::RuntimeFailure(message)
    })
}

struct RetryAcquisitionContext<'a> {
    page: &'a Page,
    target: AcquisitionTarget,
    evidence_monitor: &'a BrowserEvidenceMonitor,
    browser_runtime: &'a BrowserRuntimeProvenance,
    tls_exception: Option<TlsExceptionProvenance>,
    session_deadline: Instant,
    session: u32,
    browser_session: u64,
    interrupt: &'a mut CtrlCListener,
}

#[tracing::instrument(skip_all, fields(session = context.session, identity = character, index, stage = "acquisition"))]
async fn acquire_one_with_retries(
    context: RetryAcquisitionContext<'_>,
    index: usize,
    character: &str,
    on_event: &mut impl FnMut(AcquisitionEvent) -> Result<(), AssetError>,
    timing: &BrowserItemTimer,
) -> Result<(Result<AcquiredMedia, String>, Option<SessionStopReason>), AcquisitionStreamError> {
    for attempt in 1..=MAX_ACQUISITION_ATTEMPTS {
        timing.set_attempt(u64::from(attempt));
        if let Some(reason) = monitor_stop_reason(context.evidence_monitor) {
            timing.record_runtime_snapshot(&context.evidence_monitor.telemetry.snapshot(0));
            return Ok((Err(reason.summary()), Some(reason)));
        }
        if Instant::now() >= context.session_deadline {
            return Ok((
                Err("browser_session_deadline: общий срок пакета истёк".into()),
                Some(SessionStopReason::Deadline),
            ));
        }
        let epoch = context.evidence_monitor.begin_acquisition();
        timing.record_runtime_snapshot(&context.evidence_monitor.telemetry.snapshot(epoch));
        let item_deadline = (Instant::now() + ITEM_TIMEOUT).min(context.session_deadline);
        let (result, timed_out) = {
            let acquisition = acquire_one(
                context.page,
                character,
                context.target,
                context.browser_runtime,
                context.tls_exception.clone(),
                YarxiAttemptDiagnostics {
                    timing,
                    monitor: context.evidence_monitor,
                    epoch,
                },
            );
            tokio::pin!(acquisition);
            let item_timeout = timeout_at(item_deadline, &mut acquisition);
            tokio::pin!(item_timeout);
            let mut heartbeat = tokio::time::interval(PROGRESS_HEARTBEAT);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            heartbeat.tick().await;
            let acquisition = loop {
                tokio::select! {
                    biased;
                    result = &mut item_timeout => break result,
                    _ = heartbeat.tick() => {
                        if let Err(error) = on_event(AcquisitionEvent::Heartbeat { index, attempt }) {
                            timing.record_runtime_snapshot(&context.evidence_monitor.telemetry.snapshot(epoch));
                            timing.set_interruption("progress_consumer_failed", "progress_consumer_failed", false);
                            return Err(AcquisitionStreamError::Consumer(error));
                        }
                    },
                    signal = &mut *context.interrupt => {
                        timing.record_runtime_snapshot(&context.evidence_monitor.telemetry.snapshot(epoch));
                        timing.set_interruption("ctrl_c", "acquisition_interrupted", false);
                        return Err(signal_error(signal));
                    },
                }
            };
            timing.record_runtime_snapshot(&context.evidence_monitor.telemetry.snapshot(epoch));
            match acquisition {
                Ok(result) => (result, false),
                Err(_) if Instant::now() >= context.session_deadline => {
                    timing.set_interruption("session_deadline", "browser_session_deadline", false);
                    return Ok((
                        Err("browser_session_deadline: общий срок пакета истёк".into()),
                        Some(SessionStopReason::Deadline),
                    ));
                }
                Err(_) => {
                    timing.set_interruption(
                        "item_timeout",
                        "browser_item_timeout",
                        context
                            .evidence_monitor
                            .retryable_acquisition(epoch, RetryTrigger::ItemTimeout),
                    );
                    (
                        Err(
                            "browser_item_timeout: превышен ограниченный срок обработки символа"
                                .into(),
                        ),
                        true,
                    )
                }
            }
        };

        match result {
            Ok(mut media) => {
                media.evidence.acquisition_attempts = attempt;
                return Ok((Ok(media), None));
            }
            Err(error) => {
                let stop = monitor_stop_reason(context.evidence_monitor);
                let retry_allowed = stop.is_none()
                    && should_retry_acquisition(&error, context.evidence_monitor, epoch, attempt);
                tracing::warn!(
                    session = context.session,
                    identity = character,
                    index,
                    attempt,
                    stage = "retry_decision",
                    code = acquisition_failure_code(&error),
                    retry_allowed,
                    monitor_failed = stop.is_some(),
                    "Ошибка получения; решение о повторной попытке"
                );
                if let Some(reason) = stop {
                    return Ok((Err(error), Some(reason)));
                }
                if retry_allowed {
                    timing.set_attempt(u64::from(attempt + 1));
                    on_event(AcquisitionEvent::RetryStarted {
                        index,
                        attempt: attempt + 1,
                    })?;
                    let backoff_timing = timing.stage(YarxiStage::RetryBackoff.name());
                    let backoff_complete = tokio::select! {
                        result = timeout_at(context.session_deadline, sleep(RETRY_BACKOFF)) => result.is_ok(),
                        signal = &mut *context.interrupt => {
                            backoff_timing.interrupt("ctrl_c", Some(&context.evidence_monitor.telemetry.snapshot(epoch)));
                            return Err(signal_error(signal));
                        },
                    };
                    if !backoff_complete {
                        backoff_timing.finish_failure(
                            "browser_session_deadline",
                            false,
                            Some(&context.evidence_monitor.telemetry.snapshot(epoch)),
                        );
                        return Ok((
                            Err("browser_session_deadline: общий срок пакета истёк".into()),
                            Some(SessionStopReason::Deadline),
                        ));
                    }
                    backoff_timing.finish_success();
                    let recovery_deadline = (Instant::now()
                        + ITEM_TIMEOUT.min(Duration::from_secs(30)))
                    .min(context.session_deadline);
                    tracing::info!(
                        session = context.session,
                        identity = character,
                        index,
                        attempt = attempt + 1,
                        stage = "retry_recovery",
                        code = "retry_recovery_started",
                        "Восстановление страницы перед повторной попыткой"
                    );
                    on_event(AcquisitionEvent::RetryRecoveryStarted {
                        index,
                        attempt: attempt + 1,
                    })?;
                    let recovery_timing = timing.stage(YarxiStage::RetryRecovery.name());
                    // Та же граница begin_epoch, что ранее находилась внутри восстановления;
                    // владелец сохраняет её номер для точных свидетельств при отмене.
                    let recovery_epoch = context.evidence_monitor.begin_acquisition();
                    timing.record_runtime_snapshot(
                        &context.evidence_monitor.telemetry.snapshot(recovery_epoch),
                    );
                    let recovery = tokio::select! {
                        result = timeout_at(
                            recovery_deadline,
                            recover_page_for_retry(
                                context.page,
                                context.target,
                                context.evidence_monitor,
                                recovery_deadline,
                                recovery_epoch,
                            ),
                        ) => result,
                        signal = &mut *context.interrupt => {
                            recovery_timing.interrupt("ctrl_c", Some(&context.evidence_monitor.telemetry.snapshot(recovery_epoch)));
                            return Err(signal_error(signal));
                        },
                    };
                    let recovery_snapshot =
                        context.evidence_monitor.telemetry.snapshot(recovery_epoch);
                    match recovery {
                        Ok(Ok(())) => recovery_timing.finish_success(),
                        Err(_) if Instant::now() >= context.session_deadline => {
                            recovery_timing.finish_failure(
                                "browser_session_deadline",
                                false,
                                Some(&recovery_snapshot),
                            );
                            return Ok((
                                Err("browser_session_deadline: общий срок пакета истёк при восстановлении страницы".into()),
                                Some(SessionStopReason::Deadline),
                            ));
                        }
                        Ok(Err(recovery_error)) => {
                            recovery_timing.finish_failure(
                                acquisition_failure_code(&recovery_error),
                                false,
                                Some(&recovery_snapshot),
                            );
                            return Ok((
                                Err(format!(
                                    "{error}; browser_retry_recovery_failed: {recovery_error}"
                                )),
                                Some(SessionStopReason::RetryRecoveryFailed(recovery_error)),
                            ));
                        }
                        Err(_) => {
                            recovery_timing.finish_failure(
                                "browser_retry_recovery_timeout",
                                false,
                                Some(&recovery_snapshot),
                            );
                            let error = "восстановление страницы превысило ограниченный срок";
                            return Ok((
                                Err(format!("{error}; browser_retry_recovery_failed")),
                                Some(SessionStopReason::RetryRecoveryFailed(error.to_owned())),
                            ));
                        }
                    }
                    continue;
                }
                if timed_out {
                    return Ok((Err(error), Some(SessionStopReason::ItemTimeout)));
                }
                return Ok((Err(error), None));
            }
        }
    }
    Ok((
        Err("browser_retry_exhausted: лимит попыток получения исчерпан".into()),
        None,
    ))
}

fn acquisition_failure_code(error: &str) -> &str {
    error
        .split_once(':')
        .map_or("acquisition_failed", |(code, _)| {
            if !code.is_empty()
                && code.len() <= 80
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
            {
                code
            } else {
                "acquisition_failed"
            }
        })
}

fn is_retryable_acquisition_error(
    error: &str,
    evidence_monitor: &BrowserEvidenceMonitor,
    epoch: u64,
) -> bool {
    let trigger = if error.starts_with("browser_item_timeout:") {
        RetryTrigger::ItemTimeout
    } else if error.starts_with("browser_readiness_timeout:") {
        RetryTrigger::ReadinessTimeout
    } else if error.starts_with("browser_network_runtime_failure:") {
        RetryTrigger::RuntimeFailure
    } else {
        return false;
    };
    evidence_monitor.retryable_acquisition(epoch, trigger)
}

fn should_retry_acquisition(
    error: &str,
    evidence_monitor: &BrowserEvidenceMonitor,
    epoch: u64,
    attempt: u8,
) -> bool {
    attempt < MAX_ACQUISITION_ATTEMPTS
        && is_retryable_acquisition_error(error, evidence_monitor, epoch)
}

async fn recover_page_for_retry(
    page: &Page,
    target: AcquisitionTarget,
    evidence_monitor: &BrowserEvidenceMonitor,
    recovery_deadline: Instant,
    recovery_epoch: u64,
) -> Result<(), String> {
    page.goto("about:blank")
        .await
        .map_err(|error| format!("browser_retry_recovery_blank: {error}"))?;
    page.goto(SITE_URL)
        .await
        .map_err(|error| format!("browser_retry_recovery_navigation: {error}"))?;
    let site_host: bool = page
        .evaluate(format!("location.hostname === {SITE_HOST:?}"))
        .await
        .map_err(|error| format!("browser_retry_recovery_host_check: {error}"))?
        .into_value()
        .map_err(|error| format!("browser_retry_recovery_host_result: {error}"))?;
    if !site_host {
        return Err("browser_retry_recovery_host_mismatch: загрузка ушла с www.yarxi.su".into());
    }
    wait_until(page, Duration::from_secs(20), || {
        "Boolean([...document.querySelectorAll('.kanji-search-form input[placeholder=\"Чтение\"]')].find(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden'))"
    })
    .await
    .map_err(|error| format!("browser_retry_recovery_form: {error}"))?;
    if target == AcquisitionTarget::RenderedFontSamplePng {
        apply_dark_theme(page)
            .await
            .map_err(|error| format!("browser_retry_recovery_dark_theme: {error}"))?;
    }

    loop {
        if Instant::now() >= recovery_deadline {
            return Err(
                "browser_retry_recovery_timeout: истёк срок восстановления страницы".into(),
            );
        }
        let runtime = evidence_monitor.readiness(recovery_epoch, None);
        if runtime.monitor_failed
            || runtime.network_failures > 0
            || runtime.http_errors > 0
            || runtime.javascript_exceptions > 0
        {
            return Err(format!(
                "browser_retry_recovery_runtime_failure: {}",
                runtime_failure_reason(runtime)
            ));
        }
        if runtime.pending_relevant_requests == 0 {
            return Ok(());
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn apply_dark_theme(page: &Page) -> Result<(), String> {
    let style_id = serde_json::to_string(YARXI_DARK_THEME_STYLE_ID)
        .map_err(|error| format!("сериализация ID стиля тёмной темы: {error}"))?;
    let style_text = serde_json::to_string(YARXI_DARK_THEME_STYLE)
        .map_err(|error| format!("сериализация stylesheet тёмной темы: {error}"))?;
    let script = format!(
        r#"() => {{
            const root = document.documentElement;
            if (!root) return {{ error: 'корневой элемент document отсутствует' }};
            root.dataset.theme = 'dark';
            const styleId = {style_id};
            const styleText = {style_text};
            let style = document.getElementById(styleId);
            if (style && style.textContent !== styleText)
                return {{ error: 'идентификатор stylesheet тёмной темы уже занят' }};
            if (!style) {{
                style = document.createElement('style');
                style.id = styleId;
                style.textContent = styleText;
                (document.head || root).appendChild(style);
            }}
            const luminance = value => {{
                const components = (String(value).match(/[0-9]+(?:\.[0-9]+)?/g) || [])
                    .slice(0, 3).map(Number);
                if (components.length !== 3) return null;
                const linear = components.map(channel => {{
                    const normalized = channel / 255;
                    return normalized <= 0.04045 ? normalized / 12.92
                        : ((normalized + 0.055) / 1.055) ** 2.4;
                }});
                return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
            }};
            const rootStyle = getComputedStyle(root);
            const app = document.querySelector('#app');
            const appStyle = app && getComputedStyle(app);
            return {{
                theme: root.dataset.theme || '',
                prefers_dark: matchMedia('(prefers-color-scheme: dark)').matches,
                color_scheme: rootStyle.colorScheme,
                page_background: rootStyle.backgroundColor,
                page_background_luminance: luminance(rootStyle.backgroundColor),
                app_foreground: appStyle?.color || '',
                app_foreground_luminance: appStyle ? luminance(appStyle.color) : null,
                style_id: style.id,
                style_text: style.textContent,
            }};
        }}"#
    );
    let state: Value = page
        .evaluate(script)
        .await
        .map_err(|error| format!("yarxi_dark_theme_setup: {error}"))?
        .into_value()
        .map_err(|error| format!("yarxi_dark_theme_setup_result: {error}"))?;
    let dark_state = state["theme"].as_str() == Some("dark")
        && state["prefers_dark"].as_bool() == Some(true)
        && state["color_scheme"]
            .as_str()
            .is_some_and(|value| value.contains("dark"))
        && state["page_background_luminance"]
            .as_f64()
            .is_some_and(|value| value < 0.20)
        && state["app_foreground_luminance"]
            .as_f64()
            .is_some_and(|value| value > 0.75)
        && state["style_id"].as_str() == Some(YARXI_DARK_THEME_STYLE_ID)
        && state["style_text"].as_str() == Some(YARXI_DARK_THEME_STYLE);
    if !dark_state {
        return Err(format!(
            "yarxi_dark_theme_unavailable: палитра Yarxi или наследование цвета текста страницы не перешли в проверенное тёмное состояние: {state}"
        ));
    }
    Ok(())
}

async fn async_error_or_tls_interstitial(
    page: &Page,
    evidence_monitor: &BrowserEvidenceMonitor,
    allow_insecure_tls: bool,
    original: impl std::fmt::Display,
) -> Result<ApprovedTlsException, String> {
    if !allow_insecure_tls {
        return Err(format!(
            "TLS-сертификат Yarxi отклонён браузером: {original}; TLS-исключение отключено через --allow-insecure-tls=false"
        ));
    }
    let interstitial: Value = page
        .evaluate(
            "() => ({ code: document.querySelector('#error-code')?.textContent?.trim(), proceed: Boolean(document.querySelector('#proceed-link')) })",
        )
        .await
        .map_err(|error| format!("не удалось проверить страницу-предупреждение TLS: {error}"))?
        .into_value()
        .map_err(|error| format!("страница-предупреждение TLS вернула некорректный ответ: {error}"))?;
    let code = interstitial["code"].as_str().unwrap_or_default();
    let failed_request = wait_for_tls_navigation_failure(evidence_monitor).await?;
    let blocked_url = failed_request.url.as_deref().ok_or_else(|| {
        "страница-предупреждение TLS не связана с URL ошибочного запроса Document".to_owned()
    })?;
    if let Err(reason) = check_tls_exception(
        allow_insecure_tls,
        blocked_url,
        code,
        &original.to_string(),
        interstitial["proceed"].as_bool().unwrap_or(false),
    ) {
        return Err(format!(
            "исключение TLS отклонено: {reason}; исходная ошибка={original}; страница-предупреждение={interstitial}"
        ));
    }
    // Переход по странице-предупреждению Chrome действует только для origin; глобальный
    // ignore-certificate-errors для процесса браузера не включён.
    page.evaluate("() => document.querySelector('#proceed-link').click()")
        .await
        .map_err(|error| format!("не удалось принять исключение Yarxi: {error}"))?;
    wait_until(page, Duration::from_secs(20), || {
        "location.hostname === 'www.yarxi.su' && Boolean(document.querySelector('.kanji-search-form input[placeholder=\"Чтение\"]'))"
    })
    .await?;
    Ok(ApprovedTlsException {
        request_id: failed_request.request_id,
        blocked_url: blocked_url.to_owned(),
        provenance: TlsExceptionProvenance {
            host: SITE_HOST.to_owned(),
            error_code: "NET::ERR_CERT_AUTHORITY_INVALID".to_owned(),
        },
    })
}

async fn wait_for_tls_navigation_failure(
    evidence_monitor: &BrowserEvidenceMonitor,
) -> Result<NetworkOutcome, String> {
    let deadline = Instant::now() + TLS_EVIDENCE_TIMEOUT;
    loop {
        if let Some(failure) = evidence_monitor.tls_navigation_failure() {
            return Ok(failure);
        }
        if Instant::now() >= deadline {
            return Err(
                "страница-предупреждение TLS не связана с ошибкой верхнеуровневого события Document Yarxi".into(),
            );
        }
        sleep(Duration::from_millis(20)).await;
    }
}

fn check_tls_exception(
    allowed_by_policy: bool,
    blocked_url: &str,
    interstitial_code: &str,
    original_error: &str,
    proceed_link_present: bool,
) -> Result<(), String> {
    if !allowed_by_policy {
        return Err("TLS-исключение Yarxi отключено через --allow-insecure-tls=false".into());
    }
    let parsed_blocked_url =
        Url::parse(blocked_url).map_err(|_| "URL ошибочного Document некорректен")?;
    if !is_expected_yarxi_tls_url(blocked_url) {
        return Err(format!(
            "неожиданный URL ошибочного Document {parsed_blocked_url}"
        ));
    }
    if !interstitial_code.eq_ignore_ascii_case("NET::ERR_CERT_AUTHORITY_INVALID")
        || !original_error
            .to_ascii_uppercase()
            .contains("ERR_CERT_AUTHORITY_INVALID")
    {
        return Err("разрешена только ERR_CERT_AUTHORITY_INVALID".into());
    }
    if !proceed_link_present {
        return Err("страница-предупреждение Chrome не предоставляет ссылку перехода".into());
    }
    Ok(())
}

async fn acquire_one(
    page: &Page,
    character: &str,
    target: AcquisitionTarget,
    browser_runtime: &BrowserRuntimeProvenance,
    tls_exception: Option<TlsExceptionProvenance>,
    diagnostics: YarxiAttemptDiagnostics<'_>,
) -> Result<AcquiredMedia, String> {
    let YarxiAttemptDiagnostics {
        timing,
        monitor: evidence_monitor,
        epoch,
    } = diagnostics;
    crate::kanji_domain::parse_kanji_character(character)
        .map_err(|error| format!("invalid_kanji_identity: {error}"))?;
    let character_json = serde_json::to_string(character)
        .map_err(|error| format!("сериализация символа: {error}"))?;
    let search_script = format!(
        "() => {{ const visible = node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden' && getComputedStyle(node).display !== 'none'; const container = [...document.querySelectorAll('.kanji-search-form')].find(visible); const input = container && [...container.querySelectorAll('input[placeholder=\\\"Чтение\\\"]')].find(visible); if (!input) return false; const set = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set; set.call(input, {character_json}); input.dispatchEvent(new Event('input', {{ bubbles: true }})); input.dispatchEvent(new Event('change', {{ bubbles: true }})); const button = [...container.querySelectorAll('button,[role=button],input[type=submit],input[type=button],a')].find(node => visible(node) && (node.innerText || node.value || node.getAttribute('aria-label') || '').trim() === 'Найти' && !node.disabled); if (button) {{ button.click(); return true; }} input.dispatchEvent(new KeyboardEvent('keydown', {{ key: 'Enter', code: 'Enter', keyCode: 13, which: 13, bubbles: true }})); return true; }}"
    );
    diagnose_yarxi_stage(
        timing,
        YarxiStage::SearchSubmission,
        evidence_monitor,
        epoch,
        async {
            let submitted: bool = page
                .evaluate(search_script)
                .await
                .map_err(|error| format!("yarxi_search_ui: {error}"))?
                .into_value()
                .map_err(|error| format!("yarxi_search_ui_result: {error}"))?;
            if !submitted {
                return Err("yarxi_search_ui: поле «Чтение» или кнопка «Найти» отсутствует".into());
            }
            Ok(())
        },
    )
    .await?;
    let expected_code = character
        .chars()
        .next()
        .map(|ch| format!("{:X}", u32::from(ch)))
        .expect("выше проверен один символ");
    diagnose_yarxi_stage(timing, YarxiStage::InformationTab, evidence_monitor, epoch, async {
    wait_until(page, Duration::from_secs(20), || {
        "Boolean([...document.querySelectorAll('button,[role=tab]')].find(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden' && node.innerText.trim() === 'Информация'))"
    })
    .await?;
    let info_tab: bool = page
        .evaluate(
            "() => { const tab = [...document.querySelectorAll('button,[role=tab]')].find(node => node.getClientRects().length > 0 && getComputedStyle(node).visibility !== 'hidden' && node.innerText.trim() === 'Информация'); if (!tab) return false; tab.click(); return true; }",
        )
        .await
        .map_err(|error| format!("yarxi_information_tab: {error}"))?
        .into_value()
        .map_err(|error| format!("yarxi_information_tab_result: {error}"))?;
    if !info_tab {
        return Err("yarxi_information_tab: вкладка «Информация» отсутствует".into());
    }
    Ok(())
    }).await?;
    let article = diagnose_yarxi_stage(
        timing,
        YarxiStage::ArticleVerification,
        evidence_monitor,
        epoch,
        wait_for_article(page, &expected_code),
    )
    .await?;

    let mut snapshot = diagnose_yarxi_stage(
        timing,
        YarxiStage::MediaReadiness,
        evidence_monitor,
        epoch,
        wait_for_media_snapshot(
            page,
            character,
            &expected_code,
            epoch,
            evidence_monitor,
            target,
        ),
    )
    .await?;
    let mut choice = diagnose_yarxi_stage(
        timing,
        YarxiStage::MediaSelection,
        evidence_monitor,
        epoch,
        async {
            choose_media_source_for_target(
                snapshot.primary.clone(),
                &snapshot.font_samples,
                character,
                target,
            )
        },
    )
    .await?;
    if target == AcquisitionTarget::PreferredSource
        && matches!(choice, MediaSourceChoice::RenderedFontSample { .. })
    {
        diagnose_yarxi_stage(
            timing,
            YarxiStage::DarkTheme,
            evidence_monitor,
            epoch,
            apply_dark_theme(page),
        )
        .await?;
        snapshot = diagnose_yarxi_stage(
            timing,
            YarxiStage::MediaReadiness,
            evidence_monitor,
            epoch,
            wait_for_media_snapshot(
                page,
                character,
                &expected_code,
                epoch,
                evidence_monitor,
                target,
            ),
        )
        .await?;
        choice = diagnose_yarxi_stage(
            timing,
            YarxiStage::MediaSelection,
            evidence_monitor,
            epoch,
            async {
                choose_media_source_for_target(
                    snapshot.primary.clone(),
                    &snapshot.font_samples,
                    character,
                    target,
                )
            },
        )
        .await?;
    }
    let (selection, source_url, absence_proof, rendered_font_sample, bytes) = match choice {
        MediaSourceChoice::Url {
            selection,
            url,
            absence_proof,
        } => {
            let bytes = diagnose_yarxi_stage(
                timing,
                YarxiStage::ResourceRead,
                evidence_monitor,
                epoch,
                resource_bytes(page, &url),
            )
            .await?;
            (selection, url, absence_proof, None, bytes)
        }
        MediaSourceChoice::RenderedFontSample {
            sample,
            absence_proof,
        } => {
            let (bytes, evidence) = diagnose_yarxi_stage(
                timing,
                YarxiStage::FontSampleCapture,
                evidence_monitor,
                epoch,
                render_font_sample_png(page, &sample, character, &expected_code),
            )
            .await?;
            (
                SelectionResult::RenderedFontSamplePng,
                SITE_URL.to_owned(),
                absence_proof,
                Some(evidence),
                bytes,
            )
        }
    };
    if bytes.is_empty() {
        return Err("media_empty: браузер вернул пустые байты".into());
    }
    let evidence = AcquisitionEvidence {
        provider: PROVIDER_ID.to_owned(),
        provider_version: PROVIDER_VERSION.to_owned(),
        article_unicode: article.unicode.clone(),
        article_number: article.article_number,
        frequency_index: article.frequency_index,
        selection,
        target,
        fallback_absence_proof: absence_proof,
        rendered_font_sample,
        source_url: source_url.clone(),
        browser_runtime: Some(browser_runtime.clone()),
        tls_exception,
        acquisition_attempts: 1,
    };
    Ok(AcquiredMedia {
        character: character.to_owned(),
        source_url,
        article_unicode: article.unicode,
        selection,
        bytes,
        evidence,
    })
}

#[derive(Debug)]
struct MediaSnapshot {
    primary: PrimaryGifState,
    font_samples: Vec<FontSampleState>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FontSampleState {
    index: usize,
    visible: bool,
    text: String,
    class_name: String,
    title: String,
    images: Vec<ImageElementState>,
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ArticleEvidence {
    unicode: String,
    article_number: Option<u32>,
    frequency_index: Option<u32>,
}

async fn wait_for_article(page: &Page, expected_code: &str) -> Result<ArticleEvidence, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let state: Value = page
            .evaluate(
                "() => { const text = document.body?.innerText || ''; return { text, unicodeCount: (text.match(/Unicode:/g) || []).length }; }",
            )
            .await
            .map_err(|error| format!("yarxi_article_read: {error}"))?
            .into_value()
            .map_err(|error| format!("yarxi_article_read_result: {error}"))?;
        let text = state["text"].as_str().unwrap_or_default();
        if let Some(found) = article_unicode(text)
            && found.eq_ignore_ascii_case(expected_code)
        {
            let count = state["unicodeCount"].as_u64().unwrap_or(0);
            if count > 1 {
                return Err(format!(
                    "ambiguous_article: найдено {count} результатов для U+{expected_code}"
                ));
            }
            return Ok(ArticleEvidence {
                unicode: found,
                article_number: article_number(text),
                frequency_index: frequency_index(text),
            });
        }
        if tokio::time::Instant::now() >= deadline {
            let body = text.chars().take(200).collect::<String>();
            return Err(format!(
                "article_identity_mismatch: нет Unicode U+{expected_code}; article_text={body}"
            ));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

fn article_unicode(text: &str) -> Option<String> {
    let marker = text.find("Unicode:")?;
    text[marker + "Unicode:".len()..]
        .trim_start()
        .split(|ch: char| !ch.is_ascii_hexdigit())
        .next()
        .filter(|digits| !digits.is_empty())
        .map(str::to_owned)
}

fn article_number(text: &str) -> Option<u32> {
    let heading = text.lines().find(|line| line.contains('№'))?;
    let number = heading.split_once('№')?.1.trim_start();
    let digits: String = number.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn frequency_index(text: &str) -> Option<u32> {
    let marker = text.find("Частотность")?;
    let value = text[marker + "Частотность".len()..].trim_start();
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

async fn wait_for_media_snapshot(
    page: &Page,
    expected_character: &str,
    expected_code: &str,
    epoch: u64,
    evidence_monitor: &BrowserEvidenceMonitor,
    target: AcquisitionTarget,
) -> Result<MediaSnapshot, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let mut previous_stable: Option<(String, std::time::Instant)> = None;
    loop {
        let state: Value = page
            .evaluate(
                r#"() => {
                    const text = document.body?.innerText || '';
                    const visible = node => node.getClientRects().length > 0
                        && getComputedStyle(node).visibility !== 'hidden'
                        && getComputedStyle(node).display !== 'none';
                    const infoTab = [...document.querySelectorAll('button,[role=tab]')]
                        .find(node => visible(node) && node.innerText.trim() === 'Информация');
                    const samples = [...document.querySelectorAll('.font-sample')].map((node,index) => {
                        const rect = node.getBoundingClientRect();
                        const images = [...node.querySelectorAll('img')].map(img => ({
                            url: img.currentSrc || img.src,
                            complete: img.complete,
                            width: img.naturalWidth,
                            height: img.naturalHeight,
                        }));
                        return {
                            index,
                            visible: visible(node),
                            text: (node.textContent || '').trim(),
                            class_name: typeof node.className === 'string' ? node.className : '',
                            title: node.getAttribute('title') || '',
                            images,
                            left: rect.left,
                            top: rect.top,
                            width: rect.width,
                            height: rect.height,
                        };
                    });
                    const row = [...document.querySelectorAll('.font-sample')].filter(visible);
                    const primary = document.querySelector('img.kakijun-gif');
                    const primaryUrl = primary ? (primary.currentSrc || primary.src) : '';
                    return {
                        text,
                        ready: document.readyState === 'complete' && document.fonts.status === 'loaded' && Boolean(infoTab) && Boolean(row.length),
                        samples,
                        primary: primary ? {
                            url: primaryUrl,
                            complete: primary.complete,
                            width: primary.naturalWidth,
                            height: primary.naturalHeight,
                        } : null,
                        pending: [...document.images].some(img => !img.complete),
                    };
                }"#,
            )
            .await
            .map_err(|error| format!("yarxi_media_dom: {error}"))?
            .into_value()
            .map_err(|error| format!("yarxi_media_dom_result: {error}"))?;
        let current_code = article_unicode(state["text"].as_str().unwrap_or_default());
        if current_code
            .as_deref()
            .is_none_or(|value| !value.eq_ignore_ascii_case(expected_code))
        {
            return Err(format!(
                "article_identity_changed: media относится не к U+{expected_code}"
            ));
        }
        let ready = state["ready"].as_bool().unwrap_or(false);
        let pending = state["pending"].as_bool().unwrap_or(true);
        let font_samples: Vec<FontSampleState> =
            serde_json::from_value(state["samples"].clone())
                .map_err(|error| format!("yarxi_font_samples_result: {error}"))?;
        let primary = state["primary"].as_object().map(|node| ImageElementState {
            url: node["url"].as_str().unwrap_or_default().to_owned(),
            complete: node["complete"].as_bool().unwrap_or(false),
            width: node["width"].as_u64().unwrap_or(0) as u32,
            height: node["height"].as_u64().unwrap_or(0) as u32,
        });
        let stable_key = serde_json::to_string(&(&font_samples, &current_code))
            .expect("сериализация DOM-снимка не должна завершаться ошибкой");
        let selected_font_sample = (target == AcquisitionTarget::RenderedFontSamplePng)
            .then(|| choose_font_sample(&font_samples, expected_character).ok())
            .flatten();
        let selected_sample_image_urls: HashSet<String> = selected_font_sample
            .as_ref()
            .into_iter()
            .flat_map(|sample| sample.images.iter().map(|image| image.url.clone()))
            .collect();
        let runtime = if target == AcquisitionTarget::RenderedFontSamplePng {
            evidence_monitor.font_sample_readiness(epoch, &selected_sample_image_urls)
        } else {
            evidence_monitor.readiness(epoch, None)
        };
        if runtime.monitor_failed || runtime.network_failures > 0 || runtime.http_errors > 0 {
            let network_details = if target == AcquisitionTarget::RenderedFontSamplePng {
                evidence_monitor
                    .font_sample_network_failure_details(epoch, &selected_sample_image_urls)
            } else {
                evidence_monitor.network_failure_details(epoch, None)
            };
            tracing::warn!(identity = expected_character, stage = "media_readiness", code = "browser_network_runtime_failure", monitor_failed = runtime.monitor_failed, pending_relevant_requests = runtime.pending_relevant_requests, network_failures = runtime.network_failures, http_errors = runtime.http_errors, javascript_exceptions = runtime.javascript_exceptions, details = %crate::diagnostics::safe_message(&network_details), "CDP не подтверждает готовность источника изображения");
            return Err(format!(
                "browser_network_runtime_failure: {}; network_failures=[{network_details}]",
                runtime_failure_reason(runtime),
            ));
        }
        if runtime.javascript_exceptions > 0 {
            return Err(format!(
                "browser_javascript_failure: {}",
                runtime_failure_reason(runtime)
            ));
        }
        let relevant_images_complete = if target == AcquisitionTarget::RenderedFontSamplePng {
            selected_font_sample.as_ref().is_some_and(|sample| {
                sample.images.iter().all(|image| {
                    image.complete && image.width > 0 && image.height > 0 && !image.url.is_empty()
                })
            })
        } else {
            !pending
        };
        let dom_stable =
            if ready && relevant_images_complete && runtime.pending_relevant_requests == 0 {
                match &previous_stable {
                    Some((previous, observed_at)) if previous == &stable_key => {
                        observed_at.elapsed() >= Duration::from_secs(2)
                    }
                    _ => {
                        previous_stable = Some((stable_key, std::time::Instant::now()));
                        false
                    }
                }
            } else {
                previous_stable = None;
                false
            };
        let primary_state = classify_primary_state(
            primary.as_ref(),
            ready,
            dom_stable,
            !pending,
            evidence_monitor.readiness(epoch, None),
        );
        if target == AcquisitionTarget::RenderedFontSamplePng
            && ready
            && relevant_images_complete
            && runtime.pending_relevant_requests == 0
            && dom_stable
        {
            return Ok(MediaSnapshot {
                primary: primary_state,
                font_samples,
            });
        }
        match &primary_state {
            PrimaryGifState::PresentLoaded { .. }
                if ready && !pending && runtime.pending_relevant_requests == 0 =>
            {
                return Ok(MediaSnapshot {
                    primary: primary_state,
                    font_samples,
                });
            }
            PrimaryGifState::PresentLoaded { .. } => {}
            PrimaryGifState::AbsentConfirmed { .. } => {
                return Ok(MediaSnapshot {
                    primary: primary_state,
                    font_samples,
                });
            }
            PrimaryGifState::Unknown { reason } if reason.contains("без естественных размеров") =>
            {
                return Err(format!("gif_present_but_failed: {reason}"));
            }
            PrimaryGifState::Unknown { .. } => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let reason = runtime_failure_reason(evidence_monitor.readiness(epoch, None));
            if primary.as_ref().is_some_and(|image| !image.complete) {
                return Err(format!(
                    "gif_pending: загрузка основного GIF не завершилась; {reason}"
                ));
            }
            return Err(format!(
                "gif_unknown: область медиа не достигла завершённого и стабильного состояния; {reason}"
            ));
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn resource_bytes(page: &Page, resource_url: &str) -> Result<Vec<u8>, String> {
    if resource_url.starts_with("data:") || resource_url.starts_with("blob:") {
        let url_json = serde_json::to_string(resource_url)
            .map_err(|error| format!("сериализация URL ресурса: {error}"))?;
        let script = format!(
            "async () => {{ const response = await fetch({url_json}); if (!response.ok) throw new Error('HTTP ' + response.status); const bytes = new Uint8Array(await response.arrayBuffer()); let binary = ''; for (let i = 0; i < bytes.length; i += 0x8000) binary += String.fromCharCode(...bytes.subarray(i, i + 0x8000)); return btoa(binary); }}"
        );
        let encoded: String = page
            .evaluate(script)
            .await
            .map_err(|error| {
                format!("не удалось получить ресурс, обслуживаемый браузером: {error}")
            })?
            .into_value()
            .map_err(|error| {
                format!("обслуживаемый браузером ресурс вернул некорректные bytes: {error}")
            })?;
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| format!("декодирование base64 для ресурса браузера: {error}"));
    }
    let url =
        Url::parse(resource_url).map_err(|error| format!("некорректный URL ресурса: {error}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.host_str().is_some_and(is_kakijun_host)
    {
        return Err("source_url_rejected: ожидается HTTPS-ресурс, не связанный с Kakijun".into());
    }
    let tree = page
        .execute(GetResourceTreeParams::default())
        .await
        .map_err(|error| format!("CDP Page.getResourceTree: {error}"))?;
    let frame_id = tree.result.frame_tree.frame.id.clone();
    let content = page
        .execute(GetResourceContentParams::new(frame_id, resource_url))
        .await
        .map_err(|error| format!("CDP Page.getResourceContent: {error}"))?;
    if !content.result.base64_encoded {
        return Err("media_response_not_binary: браузер не сохранил исходные байты".into());
    }
    base64::engine::general_purpose::STANDARD
        .decode(content.result.content)
        .map_err(|error| format!("декодирование media из base64: {error}"))
}

async fn render_font_sample_png(
    page: &Page,
    sample: &FontSampleState,
    expected_character: &str,
    expected_code: &str,
) -> Result<(Vec<u8>, RenderedFontSampleEvidence), String> {
    let expected_json = serde_json::to_string(expected_character)
        .map_err(|error| format!("сериализация символа font-sample: {error}"))?;
    let expected_code_json = serde_json::to_string(expected_code)
        .map_err(|error| format!("сериализация образца шрифта Unicode: {error}"))?;
    let expected_class_json = serde_json::to_string(&sample.class_name)
        .map_err(|error| format!("сериализация класса font-sample: {error}"))?;
    let expected_title_json = serde_json::to_string(&sample.title)
        .map_err(|error| format!("сериализация заголовка образца шрифта: {error}"))?;
    let expected_text_json = serde_json::to_string(&sample.text)
        .map_err(|error| format!("сериализация текста font-sample: {error}"))?;
    let script = r#"() => {
        const expected = __EXPECTED__;
        const expectedCode = __EXPECTED_CODE__.toUpperCase();
        const expectedIndex = __INDEX__;
        const expectedClass = __CLASS__;
        const expectedTitle = __TITLE__;
        const expectedText = __TEXT__;
        const body = document.body?.innerText || '';
        const unicodeMatches = [...body.matchAll(/Unicode:\s*([0-9a-f]+)/ig)];
        if (unicodeMatches.length !== 1 || unicodeMatches[0][1].toUpperCase() !== expectedCode)
            return { error: 'Unicode статьи изменился или неоднозначен' };
        const visible = node => node.getClientRects().length > 0
            && getComputedStyle(node).visibility !== 'hidden'
            && getComputedStyle(node).display !== 'none';
        const all = [...document.querySelectorAll('.font-sample')];
        const candidates = all.map((node, index) => {
            const rect = node.getBoundingClientRect();
            const className = typeof node.className === 'string' ? node.className : '';
            const text = (node.textContent || '').trim();
            const images = [...node.querySelectorAll('img')];
            const validClass = !className.split(/\s+/).some(token => token.startsWith('stroke-order'));
            const identity = text === expected || (images.length > 0 && text === '');
            return { node, index, rect, className, text, validClass,
                identity, visible: visible(node) };
        }).filter(item => item.validClass && item.identity && item.visible
            && item.rect.width > 0 && item.rect.height > 0)
          .sort((left, right) => left.rect.left - right.rect.left
            || left.rect.top - right.rect.top || left.index - right.index);
        const selected = candidates[0];
        if (!selected) return { error: 'нет видимого font-sample с точным символом' };
        if (selected.index !== expectedIndex || selected.className !== expectedClass
            || selected.text !== expectedText
            || (selected.node.getAttribute('title') || '') !== expectedTitle)
            return { error: 'идентичность крайнего левого font-sample изменилась до capture' };
        const parseColor = value => {
            const parts = (value.match(/[0-9]+(?:\.[0-9]+)?/g) || []).slice(0, 4).map(Number);
            return parts.length >= 3 ? [parts[0], parts[1], parts[2], parts.length > 3 ? parts[3] : 1] : null;
        };
        const composite = node => {
            const chain = [];
            for (let current = node; current instanceof Element; current = current.parentElement)
                chain.unshift(current);
            let result = [255, 255, 255];
            for (const current of chain) {
                const color = parseColor(getComputedStyle(current).backgroundColor);
                if (!color) continue;
                const alpha = Math.max(0, Math.min(1, color[3]));
                result = [0, 1, 2].map(channel => color[channel] * alpha + result[channel] * (1 - alpha));
            }
            return `rgb(${result.map(Math.round).join(', ')})`;
        };
        const luminance = value => {
            const color = parseColor(value);
            if (!color) return null;
            const linear = color.slice(0, 3).map(channel => {
                const normalized = channel / 255;
                return normalized <= 0.04045 ? normalized / 12.92 : ((normalized + 0.055) / 1.055) ** 2.4;
            });
            return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2];
        };
        const root = document.documentElement;
        const bodyNode = document.body || root;
        const style = getComputedStyle(selected.node);
        const pageBackground = composite(bodyNode);
        const tileBackground = composite(selected.node);
        const foreground = style.color;
        const borderWidth = style.borderTopWidth;
        const borderStyle = style.borderTopStyle;
        const borderColor = style.borderTopColor;
        const outlineWidth = style.outlineWidth;
        const outlineStyle = style.outlineStyle;
        const outlineColor = style.outlineColor;
        const hasPaint = value => (parseColor(value)?.[3] ?? 0) > 0;
        const borderVisible = (Number.parseFloat(borderWidth) > 0 && borderStyle !== 'none'
            && hasPaint(borderColor)) || (Number.parseFloat(outlineWidth) > 0
            && outlineStyle !== 'none' && hasPaint(outlineColor));
        const visibleBorderWidth = Number.parseFloat(borderWidth) > 0 ? borderWidth : outlineWidth;
        const visibleBorderStyle = borderStyle !== 'none' && Number.parseFloat(borderWidth) > 0
            ? borderStyle : outlineStyle;
        const visibleBorderColor = borderStyle !== 'none' && Number.parseFloat(borderWidth) > 0
            ? borderColor : outlineColor;
        const styleAttribute = selected.node.getAttribute('style');
        if (styleAttribute && styleAttribute.length > 4096)
            return { error: 'inline style font-sample превышает ограничение evidence' };
        if (root.dataset.theme !== 'dark' || !matchMedia('(prefers-color-scheme: dark)').matches
            || luminance(pageBackground) === null || luminance(pageBackground) >= 0.20
            || luminance(tileBackground) === null || luminance(tileBackground) >= 0.20
            || luminance(foreground) === null || luminance(foreground) <= 0.75 || !borderVisible)
            return { error: `не удалось подготовить плитку к рендерингу (theme_class=${String(root.className).slice(0, 64)}; prefers_dark=${matchMedia('(prefers-color-scheme: dark)').matches}; page_bg=${String(pageBackground).slice(0, 64)}; tile_bg=${String(tileBackground).slice(0, 64)}; foreground=${String(foreground).slice(0, 64)}; border=${String(visibleBorderWidth).slice(0, 32)} ${String(visibleBorderStyle).slice(0, 32)} ${String(visibleBorderColor).slice(0, 64)}; border_visible=${borderVisible})` };
        const bounded = (value, limit) => String(value || '').slice(0, limit);
        const fingerprint = value => {
            const text = value === null ? '<null>' : value;
            let hash = 2166136261;
            for (let index = 0; index < text.length; index++) {
                hash ^= text.charCodeAt(index);
                hash = Math.imul(hash, 16777619);
            }
            return (hash >>> 0).toString(16).padStart(8, '0');
        };
        const rect = selected.rect;
        return {
            index: selected.index,
            selector: '.font-sample (leftmost visible exact-character sample)',
            class_name: bounded(selected.className, 200),
            title: bounded(selected.node.getAttribute('title') || '', 200),
            text: bounded(selected.text, 16),
            font_family: bounded(style.fontFamily, 256),
            font_size: bounded(style.fontSize, 32),
            css_rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
            viewport_width: window.innerWidth,
            viewport_height: window.innerHeight,
            device_scale_factor: window.devicePixelRatio,
            page_scale_factor: window.visualViewport?.scale || 1,
            prefers_color_scheme_dark: matchMedia('(prefers-color-scheme: dark)').matches,
            document_theme: bounded(root.dataset.theme || '', 32),
            foreground_color: bounded(foreground, 64),
            tile_background_color: bounded(style.backgroundColor, 64),
            page_background_color: bounded(pageBackground, 64),
            effective_background_color: bounded(tileBackground, 64),
            border_color: bounded(visibleBorderColor, 64),
            border_style: bounded(visibleBorderStyle, 32),
            border_width: bounded(visibleBorderWidth, 32),
            border_visible: borderVisible,
            box_shadow: bounded(style.boxShadow, 256),
            inline_style_json: styleAttribute,
        };
    }"#
        .replace("__EXPECTED__", &expected_json)
        .replace("__EXPECTED_CODE__", &expected_code_json)
        .replace("__INDEX__", &sample.index.to_string())
        .replace("__CLASS__", &expected_class_json)
        .replace("__TITLE__", &expected_title_json)
        .replace("__TEXT__", &expected_text_json);
    let elements = page
        .find_elements(".font-sample")
        .await
        .map_err(|error| format!("font_sample_query: {error}"))?;
    let element = elements
        .get(sample.index)
        .ok_or_else(|| "font_sample_changed: выбранный элемент больше не существует".to_owned())?;
    element
        .scroll_into_view()
        .await
        .map_err(|error| format!("font_sample_scroll: {error}"))?;
    let initial: Value = page
        .evaluate(script)
        .await
        .map_err(|error| format!("font_sample_evidence: {error}"))?
        .into_value()
        .map_err(|error| format!("font_sample_evidence_result: {error}"))?;
    if let Some(error) = initial["error"].as_str() {
        return Err(format!("font_sample_capture_contract: {error}"));
    }
    validate_capture_environment(
        initial["viewport_width"].as_u64().unwrap_or(0) as u32,
        initial["viewport_height"].as_u64().unwrap_or(0) as u32,
        initial["device_scale_factor"].as_f64().unwrap_or(0.0),
        initial["page_scale_factor"].as_f64().unwrap_or(0.0),
    )?;
    let bytes = element
        .screenshot(CaptureScreenshotFormat::Png)
        .await
        .map_err(|error| format!("font_sample_png_capture: {error}"))?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err("font_sample_png_invalid: снимок элемента не вернул байты PNG".into());
    }
    let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
        .map_err(|error| format!("font_sample_png_decode: {error}"))?;
    let (pixel_width, pixel_height) = decoded.dimensions();
    validate_capture_dimensions(pixel_width, pixel_height)?;
    let pixel_evidence = analyze_capture_pixels(&decoded.to_rgba8())?;
    let style_json = serde_json::to_string(&initial["inline_style_json"].as_str())
        .map_err(|error| format!("сериализация стиля font-sample: {error}"))?;
    let document_theme_json = serde_json::to_string(&initial["document_theme"].as_str())
        .map_err(|error| format!("сериализация темы document: {error}"))?;
    let style_id_json = serde_json::to_string(YARXI_DARK_THEME_STYLE_ID)
        .map_err(|error| format!("сериализация ID стиля тёмной темы: {error}"))?;
    let style_text_json = serde_json::to_string(YARXI_DARK_THEME_STYLE)
        .map_err(|error| format!("сериализация stylesheet тёмной темы: {error}"))?;
    let post_capture_script = format!(
        "() => {{ const node=[...document.querySelectorAll('.font-sample')][{}]; const root=document.documentElement; const style=document.getElementById({style_id_json}); return Boolean(node && node.getAttribute('style') === {style_json} && root.dataset.theme === {document_theme_json} && root.dataset.theme === 'dark' && style && style.textContent === {style_text_json} && matchMedia('(prefers-color-scheme: dark)').matches); }}",
        sample.index
    );
    let inline_style_unchanged: bool = page
        .evaluate(post_capture_script)
        .await
        .map_err(|error| format!("font_sample_post_capture_check: {error}"))?
        .into_value()
        .map_err(|error| format!("font_sample_post_capture_check_result: {error}"))?;
    if !inline_style_unchanged {
        return Err(
            "font_sample_style_changed: стиль выбранной плитки или тёмная тема изменились во время снимка"
                .into(),
        );
    }
    let evidence = RenderedFontSampleEvidence {
        index: sample.index,
        selector: initial["selector"].as_str().unwrap_or_default().to_owned(),
        class_name: initial["class_name"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        title: initial["title"].as_str().unwrap_or_default().to_owned(),
        text: initial["text"].as_str().unwrap_or_default().to_owned(),
        font_family: initial["font_family"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        font_size: initial["font_size"].as_str().unwrap_or_default().to_owned(),
        capture: "Снимок Chromium элемента .font-sample без изменений при Yarxi data-theme=dark".into(),
        css_rect: serde_json::from_value(initial["css_rect"].clone())
            .map_err(|error| format!("font_sample_css_rect: {error}"))?,
        pixel_width,
        pixel_height,
        viewport_width: initial["viewport_width"].as_u64().unwrap_or(0) as u32,
        viewport_height: initial["viewport_height"].as_u64().unwrap_or(0) as u32,
        device_scale_factor: initial["device_scale_factor"].as_f64().unwrap_or(0.0),
        page_scale_factor: initial["page_scale_factor"].as_f64().unwrap_or(0.0),
        prefers_color_scheme_dark: initial["prefers_color_scheme_dark"]
            .as_bool()
            .unwrap_or(false),
        dark_environment: "Yarxi data-theme=dark; color-scheme=dark; цвет #app наследуется от --w-base-color-rgb на уровне страницы".into(),
        document_theme: initial["document_theme"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        foreground_color: initial["foreground_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        tile_background_color: initial["tile_background_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        page_background_color: initial["page_background_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        effective_background_color: initial["effective_background_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        border_color: initial["border_color"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        border_style: initial["border_style"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        border_width: initial["border_width"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        border_visible: initial["border_visible"].as_bool().unwrap_or(false),
        box_shadow: initial["box_shadow"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        inline_style_unchanged,
        dark_background_pixels: pixel_evidence.dark_background_pixels,
        light_glyph_pixels: pixel_evidence.light_glyph_pixels,
        frame_pixels: pixel_evidence.frame_pixels,
        measured_dark_background_rgb: pixel_evidence.dark_background_rgb,
        measured_light_glyph_rgb: pixel_evidence.light_glyph_rgb,
        measured_frame_rgb: pixel_evidence.frame_rgb,
    };
    Ok((bytes, evidence))
}

fn validate_capture_dimensions(width: u32, height: u32) -> Result<(), String> {
    if (CAPTURE_MIN_EDGE_PX..=CAPTURE_MAX_EDGE_PX).contains(&width)
        && (CAPTURE_MIN_EDGE_PX..=CAPTURE_MAX_EDGE_PX).contains(&height)
    {
        Ok(())
    } else {
        Err(format!(
            "font_sample_scale_out_of_bounds: размер снимка {width}×{height} px вне диапазона {CAPTURE_MIN_EDGE_PX}–{CAPTURE_MAX_EDGE_PX} px по каждой стороне"
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TilePixelEvidence {
    dark_background_pixels: u32,
    light_glyph_pixels: u32,
    frame_pixels: u32,
    dark_background_rgb: String,
    light_glyph_rgb: String,
    frame_rgb: String,
}

#[derive(Debug, Default)]
struct PixelColorSummary {
    count: u32,
    red: u64,
    green: u64,
    blue: u64,
}

impl PixelColorSummary {
    fn add(&mut self, pixel: image::Rgba<u8>) {
        self.count = self.count.saturating_add(1);
        self.red += u64::from(pixel[0]);
        self.green += u64::from(pixel[1]);
        self.blue += u64::from(pixel[2]);
    }

    fn average_rgb(&self) -> String {
        if self.count == 0 {
            return "unavailable".into();
        }
        format!(
            "rgb({}, {}, {})",
            self.red / u64::from(self.count),
            self.green / u64::from(self.count),
            self.blue / u64::from(self.count)
        )
    }
}

fn analyze_capture_pixels(image: &RgbaImage) -> Result<TilePixelEvidence, String> {
    let (width, height) = image.dimensions();
    validate_capture_dimensions(width, height)?;
    let min_edge = width.min(height);
    let inset = (min_edge / 12).max(1);
    let frame_band = (min_edge / 20).max(1);
    let mut dark = PixelColorSummary::default();
    let mut glyph = PixelColorSummary::default();
    let mut frame = PixelColorSummary::default();

    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel[3] < 240 {
            continue;
        }
        let channels = [pixel[0], pixel[1], pixel[2]].map(|channel| {
            let normalized = f64::from(channel) / 255.0;
            if normalized <= 0.04045 {
                normalized / 12.92
            } else {
                ((normalized + 0.055) / 1.055).powf(2.4)
            }
        });
        let luminance = 0.2126 * channels[0] + 0.7152 * channels[1] + 0.0722 * channels[2];
        let inside = x >= inset && x < width - inset && y >= inset && y < height - inset;
        let at_frame =
            x < frame_band || x >= width - frame_band || y < frame_band || y >= height - frame_band;
        if luminance <= 0.10 {
            dark.add(*pixel);
        }
        if inside && luminance >= CAPTURE_LIGHT_GLYPH_MIN_LUMINANCE {
            glyph.add(*pixel);
        }
        if at_frame && (0.10..0.75).contains(&luminance) {
            frame.add(*pixel);
        }
    }

    let total_pixels = width.saturating_mul(height);
    if dark.count < total_pixels / 4 {
        return Err(format!(
            "font_sample_pixel_contract: недостаточно тёмных пикселей плитки ({}/{total_pixels})",
            dark.count
        ));
    }
    if glyph.count < 16 {
        return Err(format!(
            "font_sample_pixel_contract: недостаточно светлых пикселей glyph ({})",
            glyph.count
        ));
    }
    if frame.count < 8 {
        return Err(format!(
            "font_sample_pixel_contract: недостаточно видимых пикселей рамки среднего тона ({})",
            frame.count
        ));
    }
    Ok(TilePixelEvidence {
        dark_background_pixels: dark.count,
        light_glyph_pixels: glyph.count,
        frame_pixels: frame.count,
        dark_background_rgb: dark.average_rgb(),
        light_glyph_rgb: glyph.average_rgb(),
        frame_rgb: frame.average_rgb(),
    })
}

fn validate_capture_environment(
    viewport_width: u32,
    viewport_height: u32,
    device_scale_factor: f64,
    page_scale_factor: f64,
) -> Result<(), String> {
    if viewport_width == CAPTURE_VIEWPORT_WIDTH as u32
        && viewport_height == CAPTURE_VIEWPORT_HEIGHT as u32
        && (device_scale_factor - CAPTURE_DEVICE_SCALE_FACTOR).abs() < 0.01
        && (page_scale_factor - 1.0).abs() < 0.01
    {
        Ok(())
    } else {
        Err(format!(
            "font_sample_capture_environment_mismatch: viewport={viewport_width}x{viewport_height}, device_scale_factor={device_scale_factor}, page_scale_factor={page_scale_factor}"
        ))
    }
}

fn is_kakijun_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("kakijun.jp") || host.to_ascii_lowercase().ends_with(".kakijun.jp")
}

async fn wait_until(
    page: &Page,
    timeout_after: Duration,
    expression: impl Fn() -> &'static str,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout_after;
    loop {
        let ready: bool = page
            .evaluate(expression())
            .await
            .map_err(|error| format!("проверка готовности браузера: {error}"))?
            .into_value()
            .map_err(|error| format!("ответ проверки готовности браузера: {error}"))?;
        if ready {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(
                "browser_readiness_timeout: истёк срок ожидания готовности страницы".into(),
            );
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn run_setup_before_deadline<T>(
    deadline: Instant,
    setup: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    timeout_at(deadline, setup)
        .await
        .unwrap_or_else(|_| Err(BROWSER_SETUP_TIMEOUT_MESSAGE.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct DiagnosticBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for DiagnosticBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for DiagnosticBuffer {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn browser_diagnostic_events(operation: impl FnOnce()) -> Vec<Value> {
        let buffer = DiagnosticBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_max_level(tracing::Level::TRACE)
            .without_time()
            .with_writer(buffer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, operation);
        let bytes = buffer.0.lock().unwrap().clone();
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn browser_stage_failure_preserves_identity_stage_and_current_epoch_without_dom() {
        let monitor = BrowserEvidenceMonitor::new(CdpRuntimeMonitor::from_snapshot_for_test(
            RuntimeSnapshot {
                pending_requests: vec![
                    TrackedRequest {
                        request_id: "current".into(),
                        url: "https://example.invalid/private?secret=value".into(),
                        resource_type: ResourceType::Image,
                        epoch: 2,
                        is_top_level: false,
                    },
                    TrackedRequest {
                        request_id: "old".into(),
                        url: "https://old.invalid/secret".into(),
                        resource_type: ResourceType::Script,
                        epoch: 1,
                        is_top_level: false,
                    },
                ],
                ..RuntimeSnapshot::default()
            },
        ));
        let events = browser_diagnostic_events(|| {
            let item = BrowserItemTimer::new(
                BrowserItemContext::new(PROVIDER_ID, "漢", 1, 0).with_browser_session(7),
            );
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let result = runtime.block_on(diagnose_yarxi_stage(
                &item,
                YarxiStage::ArticleVerification,
                &monitor,
                2,
                async {
                    Err::<(), _>("article_identity_mismatch: article_text=PRIVATE_DOM".into())
                },
            ));
            assert!(result.is_err());
            item.finish_failure("article_identity_mismatch", false, None);
        });
        assert_eq!(events.len(), 2);
        let stage = &events[0]["fields"];
        assert_eq!(stage["stage"], "article_verification");
        assert_eq!(stage["provider"], PROVIDER_ID);
        assert_eq!(stage["identity"], "漢");
        assert_eq!(stage["browser_session"], 7);
        assert_eq!(stage["failure_code"], "article_identity_mismatch");
        assert_eq!(stage["retryable"], false);
        assert!(
            stage["item_duration_ms"].as_u64().unwrap()
                >= stage["stage_duration_ms"].as_u64().unwrap()
        );
        let snapshot: Value =
            serde_json::from_str(stage["runtime_snapshot"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["epoch"], 2);
        assert_eq!(snapshot["pending_request_count"], 1);
        assert_eq!(snapshot["pending_requests"][0]["in_current_epoch"], true);
        let serialized = serde_json::to_string(&events).unwrap();
        for secret in [
            "PRIVATE_DOM",
            "example.invalid",
            "old.invalid",
            "secret=value",
        ] {
            assert!(!serialized.contains(secret));
        }
    }

    #[test]
    fn browser_outer_timeout_records_active_yarxi_stage_before_next_attempt() {
        let monitor = monitor_with_failure(false);
        let events = browser_diagnostic_events(|| {
            let item = BrowserItemTimer::new(
                BrowserItemContext::new(PROVIDER_ID, "漢", 1, 0).with_browser_session(8),
            );
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                // Первый опрос входит в настоящую оболочку провайдера; затем
                // воспроизводим границу тайм-аута без ожидания и браузера.
                let acquisition = diagnose_yarxi_stage(
                    &item,
                    YarxiStage::MediaReadiness,
                    &monitor,
                    1,
                    std::future::pending::<Result<(), String>>(),
                );
                tokio::pin!(acquisition);
                tokio::select! {
                    biased;
                    _ = &mut acquisition => unreachable!(),
                    _ = std::future::ready(()) => {}
                }
                item.record_runtime_snapshot(&monitor.telemetry.snapshot(1));
                item.set_interruption("item_timeout", "browser_item_timeout", true);
            });
            item.set_attempt(2);
            item.stage(YarxiStage::SearchSubmission.name())
                .finish_success();
            item.finish_success();
        });
        assert_eq!(events.len(), 3);
        let failure = &events[0]["fields"];
        assert_eq!(failure["stage"], "media_readiness");
        assert_eq!(failure["attempt"], 1);
        assert_eq!(failure["failure_code"], "browser_item_timeout");
        assert_eq!(failure["retryable"], true);
        assert_eq!(failure["stop_reason"], "item_timeout");
        assert!(failure["runtime_snapshot"].is_string());
        let completed = &events[2]["fields"];
        assert_eq!(completed["attempt"], 2);
        assert_eq!(completed["outcome"], "success");
        assert!(completed.get("stop_reason").is_none());
    }

    #[test]
    fn browser_terminal_failure_code_keeps_session_stop_cause() {
        assert_eq!(
            terminal_failure_code("gif_unknown: pending", Some(&SessionStopReason::Deadline)),
            "browser_session_deadline"
        );
        assert_eq!(
            terminal_failure_code(
                "gif_unknown: pending",
                Some(&SessionStopReason::RetryRecoveryFailed(
                    "закрытые сведения".into()
                ))
            ),
            "browser_retry_recovery_failed"
        );
        assert_eq!(
            terminal_failure_code("gif_unknown: pending", None),
            "gif_unknown"
        );
    }

    #[test]
    fn borrowed_acquisition_workspace_survives_early_error_and_close() {
        let parent = TempWorkspace::create("yarxi-borrowed-workspace-test").unwrap();
        let runtime_dir = parent.path().join(".runtime");
        std::fs::create_dir(&runtime_dir).unwrap();
        let mut events = 0;
        let result = acquire_many_stream_with_target_in_workspace(
            &["invalid".into()],
            false,
            AcquisitionTarget::PreferredSource,
            parent.path(),
            |_| {
                events += 1;
                Ok(())
            },
        );
        assert!(
            matches!(result, Err(AcquisitionStreamError::Provider(message)) if message.starts_with("invalid_kanji_identity:"))
        );
        assert_eq!(events, 0);
        assert!(runtime_dir.is_dir());
        assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 2);
        parent.close().unwrap();
    }

    #[test]
    fn standalone_acquisition_close_removes_one_run_workspace() {
        let mut run = AcquisitionRun::new().unwrap();
        let path = run.workspace.clone();
        let summary = run.acquire(&[], false, |_| Ok(())).unwrap();
        assert_eq!(summary.processed, 0);
        run.close().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn dark_theme_setup_changes_page_environment_without_restyling_font_sample() {
        assert!(YARXI_DARK_THEME_STYLE.contains(":root { color-scheme: dark"));
        assert!(YARXI_DARK_THEME_STYLE.contains("#app { color: rgb(var(--w-base-color-rgb))"));
        assert!(!YARXI_DARK_THEME_STYLE.contains(".font-sample"));
        assert!(!YARXI_DARK_THEME_STYLE.contains("background"));
    }

    #[test]
    fn present_primary_wins_and_unknown_never_falls_back() {
        assert_eq!(
            choose_media_source(
                PrimaryGifState::PresentLoaded {
                    url: "https://yosida.com/left.gif".into(),
                },
                &[],
                "柘"
            )
            .unwrap(),
            MediaSourceChoice::Url {
                selection: SelectionResult::PrimaryGif,
                url: "https://yosida.com/left.gif".into(),
                absence_proof: None,
            }
        );
        assert!(
            choose_media_source(
                PrimaryGifState::Unknown {
                    reason: "запрос ещё выполняется".into(),
                },
                &[font_sample(0, 0.0, "柘", "font-sample serif", false)],
                "柘"
            )
            .unwrap_err()
            .contains("gif_unknown")
        );
        assert_eq!(
            choose_media_source(
                PrimaryGifState::PresentLoaded {
                    url: "https://yosida.com/left.gif".into(),
                },
                &[],
                "柘"
            )
            .unwrap(),
            MediaSourceChoice::Url {
                selection: SelectionResult::PrimaryGif,
                url: "https://yosida.com/left.gif".into(),
                absence_proof: None,
            },
            "ссылка Kakijun справа не должна заменять GIF слева"
        );
    }

    fn font_sample(
        index: usize,
        left: f64,
        text: &str,
        class_name: &str,
        stroke_order: bool,
    ) -> FontSampleState {
        FontSampleState {
            index,
            visible: true,
            text: text.into(),
            class_name: if stroke_order {
                format!("{class_name} stroke-orders")
            } else {
                class_name.into()
            },
            title: "Шрифт с засечками".into(),
            images: Vec::new(),
            left,
            top: 0.0,
            width: 80.0,
            height: 100.0,
        }
    }

    fn clean_runtime() -> RuntimeReadiness {
        RuntimeReadiness {
            pending_relevant_requests: 0,
            network_failures: 0,
            http_errors: 0,
            javascript_exceptions: 0,
            monitor_failed: false,
        }
    }

    fn test_outcome(
        resource_type: ResourceType,
        url: Option<&str>,
        failure_reason: Option<&str>,
        request_id: &str,
        epoch: u64,
        status_code: Option<u16>,
    ) -> NetworkOutcome {
        NetworkOutcome {
            resource_type,
            url: url.map(str::to_owned),
            failure_reason: failure_reason.map(str::to_owned),
            request_id: request_id.to_owned(),
            epoch,
            status_code,
            is_top_level: false,
        }
    }

    fn tracked_request(
        request_id: &str,
        resource_type: ResourceType,
        url: &str,
        epoch: u64,
    ) -> TrackedRequest {
        TrackedRequest {
            request_id: request_id.to_owned(),
            resource_type,
            url: url.to_owned(),
            epoch,
            is_top_level: false,
        }
    }

    #[test]
    fn acquisition_retry_requires_transient_runtime_evidence_and_stops_at_attempt_limit() {
        let epoch = 1;
        let telemetry = CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
            network_failures: vec![test_outcome(
                ResourceType::Fetch,
                Some("https://example.test/retry"),
                Some("net::ERR_CONNECTION_RESET"),
                "retryable",
                epoch,
                None,
            )],
            ..RuntimeSnapshot::default()
        });
        let monitor = BrowserEvidenceMonitor::new(telemetry);
        assert!(is_retryable_acquisition_error(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
        ));
        assert!(should_retry_acquisition(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
            1,
        ));
        assert!(!should_retry_acquisition(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
            2,
        ));
        assert!(!should_retry_acquisition(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
            2,
        ));
    }

    #[test]
    fn acquisition_retry_preserves_trigger_specific_evidence_rules() {
        let epoch = 1;
        let monitor = BrowserEvidenceMonitor::new(CdpRuntimeMonitor::from_snapshot_for_test(
            RuntimeSnapshot {
                http_errors: vec![test_outcome(
                    ResourceType::Fetch,
                    Some("https://example.test/permanent"),
                    None,
                    "permanent",
                    epoch,
                    Some(404),
                )],
                ..RuntimeSnapshot::default()
            },
        ));

        assert!(!is_retryable_acquisition_error(
            "browser_item_timeout: ограниченный срок истёк",
            &monitor,
            epoch,
        ));
        assert!(is_retryable_acquisition_error(
            "browser_readiness_timeout: не готова страница",
            &monitor,
            epoch,
        ));
        assert!(!is_retryable_acquisition_error(
            "browser_network_runtime_failure: постоянная HTTP-ошибка",
            &monitor,
            epoch,
        ));
        assert!(!is_retryable_acquisition_error(
            "browser_network_runtime_failure: нет сетевой ошибки",
            &monitor,
            epoch + 1,
        ));
        assert!(!is_retryable_acquisition_error(
            "неповторяемая ошибка",
            &monitor,
            epoch,
        ));

        let clean_monitor = BrowserEvidenceMonitor::new(CdpRuntimeMonitor::from_snapshot_for_test(
            RuntimeSnapshot::default(),
        ));
        assert!(is_retryable_acquisition_error(
            "browser_item_timeout: ограниченный срок истёк",
            &clean_monitor,
            epoch,
        ));
    }

    #[tokio::test]
    async fn setup_deadline_rejects_work_after_expiry() {
        let deadline = Instant::now() - Duration::from_millis(1);
        let result =
            run_setup_before_deadline(deadline, std::future::pending::<Result<(), String>>()).await;

        assert_eq!(result, Err(BROWSER_SETUP_TIMEOUT_MESSAGE.into()));
    }

    #[tokio::test]
    async fn setup_deadline_accepts_work_completed_before_expiry() {
        let result = run_setup_before_deadline(Instant::now() + Duration::from_secs(1), async {
            Ok::<_, String>("готово")
        })
        .await;

        assert_eq!(result, Ok("готово"));
    }

    #[test]
    fn session_stop_reasons_are_reported_without_tail_outcomes() {
        assert!(
            SessionStopReason::Deadline
                .summary()
                .contains("срок сеанса браузера")
        );
        assert!(
            SessionStopReason::ItemTimeout
                .summary()
                .contains("срок обработки элемента")
        );
        assert!(
            SessionStopReason::RetryRecoveryFailed("сеть недоступна".into())
                .summary()
                .contains("сеть недоступна")
        );
    }

    #[test]
    fn session_frontier_rotates_only_after_a_durable_prefix() {
        let rotate = session_action(
            10,
            30,
            &SessionAcquisitionSummary {
                processed: 7,
                stop_reason: Some(SessionStopReason::Deadline),
            },
        )
        .unwrap();
        assert_eq!(
            rotate,
            SessionAction::Rotate {
                next_offset: 17,
                reason: SessionStopReason::Deadline,
            }
        );

        let interrupted = session_action(
            10,
            30,
            &SessionAcquisitionSummary {
                processed: 7,
                stop_reason: Some(SessionStopReason::Interrupted),
            },
        )
        .unwrap();
        assert_eq!(interrupted, SessionAction::Interrupted { next_offset: 17 });

        let no_progress = session_action(
            10,
            30,
            &SessionAcquisitionSummary {
                processed: 0,
                stop_reason: Some(SessionStopReason::Deadline),
            },
        );
        assert!(matches!(
            no_progress,
            Err(AcquisitionStreamError::Provider(message))
                if message.contains("browser_session_no_progress")
        ));

        let complete = session_action(
            10,
            17,
            &SessionAcquisitionSummary {
                processed: 7,
                stop_reason: Some(SessionStopReason::Interrupted),
            },
        )
        .unwrap();
        assert_eq!(complete, SessionAction::Interrupted { next_offset: 17 });
    }

    struct FaultInjectedSession {
        monitor: BrowserEvidenceMonitor,
        fail_after_item: bool,
        acquisitions: usize,
        stop_at_checkpoint: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }

    fn monitor_with_failure(failed: bool) -> BrowserEvidenceMonitor {
        BrowserEvidenceMonitor::new(CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
            monitor_failed: failed,
            ..RuntimeSnapshot::default()
        }))
    }

    impl SessionItemDriver for FaultInjectedSession {
        async fn before_item(
            &mut self,
            _: usize,
        ) -> Result<Option<SessionStopReason>, AcquisitionStreamError> {
            Ok(monitor_stop_reason(&self.monitor))
        }

        async fn acquire(
            &mut self,
            _: usize,
            _: &str,
            _: &mut impl FnMut(AcquisitionEvent) -> Result<(), AssetError>,
        ) -> Result<
            (Result<AcquiredMedia, String>, Option<SessionStopReason>),
            AcquisitionStreamError,
        > {
            self.acquisitions += 1;
            if self.fail_after_item {
                self.monitor = monitor_with_failure(true);
                assert!(!should_retry_acquisition(
                    "browser_network_runtime_failure: отказ CDP",
                    &self.monitor,
                    0,
                    1
                ));
            }
            Ok((
                Err("browser_network_runtime_failure: реальный отказ первого элемента".into()),
                monitor_stop_reason(&self.monitor),
            ))
        }

        async fn after_checkpoint(
            &mut self,
        ) -> Result<Option<SessionStopReason>, AcquisitionStreamError> {
            if self
                .stop_at_checkpoint
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
            {
                return Err(AcquisitionStreamError::Interrupted);
            }
            Ok(monitor_stop_reason(&self.monitor))
        }
    }

    #[tokio::test]
    async fn monitor_failure_before_first_request_preserves_whole_pending_frontier() {
        let characters = vec!["漢".into(), "字".into(), "語".into()];
        let mut driver = FaultInjectedSession {
            monitor: monitor_with_failure(true),
            fail_after_item: false,
            acquisitions: 0,
            stop_at_checkpoint: None,
        };
        let mut events = Vec::new();
        let summary = process_session_items(&characters, 0, None, &mut driver, &mut |event| {
            events.push(event);
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(summary.processed, 0);
        assert_eq!(driver.acquisitions, 0);
        assert!(events.is_empty());
        assert!(
            matches!(session_action(0, characters.len(), &summary), Err(AcquisitionStreamError::Provider(message)) if message.contains("browser_network_runtime_failure"))
        );
    }

    #[tokio::test]
    async fn monitor_failure_checkpoints_only_started_prefix_and_resume_starts_first_pending() {
        let characters = vec!["漢".into(), "字".into(), "語".into()];
        let mut attempts = [0_usize; 3];
        let mut started = Vec::new();
        let mut driver = FaultInjectedSession {
            monitor: monitor_with_failure(false),
            fail_after_item: true,
            acquisitions: 0,
            stop_at_checkpoint: None,
        };
        let summary = process_session_items(&characters, 0, None, &mut driver, &mut |event| {
            match event {
                AcquisitionEvent::ItemStarted { index } => started.push(index),
                AcquisitionEvent::ItemCompleted { index, outcome, .. } => {
                    assert!(outcome.is_err());
                    attempts[index] += 1;
                }
                _ => {}
            }
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(summary.processed, 1);
        assert_eq!(started, vec![0]);
        assert_eq!(attempts, [1, 0, 0]);
        assert_eq!(driver.acquisitions, 1);
        assert!(matches!(
            session_action(0, characters.len(), &summary),
            Err(AcquisitionStreamError::Provider(_))
        ));
        // Продолжение использует ту же границу обработанного префикса, что и очередь CLI.
        let first_pending = attempts.iter().position(|attempt| *attempt == 0).unwrap();
        let mut resumed = FaultInjectedSession {
            monitor: monitor_with_failure(false),
            fail_after_item: false,
            acquisitions: 0,
            stop_at_checkpoint: None,
        };
        let resumed_summary = process_session_items(
            &characters[first_pending..],
            first_pending,
            None,
            &mut resumed,
            &mut |event| {
                match event {
                    AcquisitionEvent::ItemStarted { index } => started.push(index),
                    AcquisitionEvent::ItemCompleted { index, .. } => attempts[index] += 1,
                    _ => {}
                }
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(resumed_summary.processed, 2);
        assert_eq!(started, vec![0, 1, 2]);
        assert_eq!(attempts, [1, 1, 1]);
    }

    #[cfg(feature = "session-rotation-acceptance")]
    struct ItemFailureSession {
        monitor: BrowserEvidenceMonitor,
        acquisitions: usize,
    }

    #[cfg(feature = "session-rotation-acceptance")]
    impl SessionItemDriver for ItemFailureSession {
        async fn before_item(
            &mut self,
            _: usize,
        ) -> Result<Option<SessionStopReason>, AcquisitionStreamError> {
            Ok(monitor_stop_reason(&self.monitor))
        }

        async fn acquire(
            &mut self,
            _: usize,
            _: &str,
            _: &mut impl FnMut(AcquisitionEvent) -> Result<(), AssetError>,
        ) -> Result<
            (Result<AcquiredMedia, String>, Option<SessionStopReason>),
            AcquisitionStreamError,
        > {
            self.acquisitions += 1;
            Ok((
                Err("gif_unknown: незавершённый ресурс элемента".into()),
                None,
            ))
        }

        async fn after_checkpoint(
            &mut self,
        ) -> Result<Option<SessionStopReason>, AcquisitionStreamError> {
            Ok(monitor_stop_reason(&self.monitor))
        }
    }

    #[cfg(feature = "session-rotation-acceptance")]
    #[tokio::test]
    async fn acceptance_rotation_checkpoints_each_prefix_and_keeps_item_failures_local() {
        let characters = [
            "漢".into(),
            "字".into(),
            "語".into(),
            "音".into(),
            "鳥".into(),
        ];
        let mut checkpointed = Vec::new();
        let mut started = Vec::new();
        let mut offset = 0;
        let mut session_count = 0;
        while offset < characters.len() {
            session_count += 1;
            let mut driver = ItemFailureSession {
                monitor: monitor_with_failure(false),
                acquisitions: 0,
            };
            let summary = process_session_items(
                &characters[offset..],
                offset,
                Some(2),
                &mut driver,
                &mut |event| {
                    match event {
                        AcquisitionEvent::ItemStarted { index } => started.push(index),
                        AcquisitionEvent::ItemCompleted { index, outcome, .. } => {
                            assert!(matches!(outcome.as_ref(), Err(message) if message.starts_with("gif_unknown:")));
                            checkpointed.push(index);
                        }
                        _ => {}
                    }
                    Ok(())
                },
            )
            .await
            .unwrap();
            assert_eq!(summary.processed, driver.acquisitions);

            match session_action(offset, characters.len(), &summary).unwrap() {
                SessionAction::Rotate {
                    next_offset,
                    reason: SessionStopReason::ItemLimitReached,
                } => {
                    assert_eq!(next_offset, offset + 2);
                    assert_eq!(checkpointed.len(), next_offset);
                    offset = next_offset;
                }
                SessionAction::Complete => {
                    assert_eq!(summary.processed, characters.len() - offset);
                    offset = characters.len();
                }
                other => panic!("неожиданный результат принудительной ротации: {other:?}"),
            }
        }

        assert_eq!(session_count, 3);
        assert_eq!(started, (0..characters.len()).collect::<Vec<_>>());
        assert_eq!(checkpointed, (0..characters.len()).collect::<Vec<_>>());
    }

    #[cfg(feature = "session-rotation-acceptance")]
    #[test]
    fn acceptance_rotation_is_opt_in_and_keeps_production_limits() {
        let ordinary = AcquisitionRun::new().unwrap();
        assert_eq!(ordinary.acceptance_rotate_after_items, None);
        let forced = AcquisitionRun::new_for_acceptance_rotation(2).unwrap();
        assert_eq!(forced.acceptance_rotate_after_items, Some(2));
        assert_eq!(OPERATION_TIMEOUT, Duration::from_mins(20));
        assert_eq!(ITEM_TIMEOUT, Duration::from_secs(90));
        assert_eq!(BATCH_PACING, Duration::from_millis(900));
        assert_eq!(MAX_ACQUISITION_ATTEMPTS, 2);
    }

    #[test]
    fn final_item_monitor_failure_cannot_be_reported_as_success() {
        let result = session_action(
            0,
            1,
            &SessionAcquisitionSummary {
                processed: 1,
                stop_reason: monitor_stop_reason(&monitor_with_failure(true)),
            },
        );
        assert!(
            matches!(result, Err(AcquisitionStreamError::Provider(message)) if message.contains("browser_network_runtime_failure"))
        );
    }

    #[test]
    fn launch_or_setup_failure_never_emits_session_started() {
        for message in [
            "launch_failure: нет браузера",
            "setup_failure: отказ страницы",
            "browser_session_setup_runtime_unhealthy: ожидает Image",
        ] {
            let mut events = Vec::new();
            let result = finish_session_setup::<()>(
                Err(AcquisitionStreamError::Provider(message.into())),
                1,
                &mut |event| {
                    events.push(event);
                    Ok(())
                },
            );
            assert!(matches!(result, Err(AcquisitionStreamError::Provider(_))));
            assert!(events.is_empty());
        }
        let mut events = Vec::new();
        finish_session_setup(Ok(()), 2, &mut |event| {
            events.push(event);
            Ok(())
        })
        .unwrap();
        assert!(matches!(
            events.as_slice(),
            [AcquisitionEvent::SessionStarted { session: 2 }]
        ));
    }

    #[test]
    fn setup_readiness_recreation_is_bounded() {
        assert_eq!(next_session_setup_recreation(0), Some(1));
        assert_eq!(next_session_setup_recreation(1), Some(2));
        assert_eq!(next_session_setup_recreation(2), None);
    }

    #[test]
    fn setup_failures_keep_frontier_until_recreated_session_processes_it() {
        let characters = vec!["漢".into(), "字".into(), "語".into()];
        let mut run = AcquisitionRun::new().unwrap();
        run.test_session_attempts = Some(VecDeque::from([
            ScriptedSessionAttempt::SetupFailure(format!(
                "{SESSION_SETUP_RUNTIME_FAILURE_PREFIX} ожидающий запрос типа Image"
            )),
            ScriptedSessionAttempt::SetupFailure(format!(
                "{SESSION_SETUP_RUNTIME_FAILURE_PREFIX} ожидающий запрос типа Image"
            )),
            ScriptedSessionAttempt::Processed {
                count: 1,
                stop_reason: Some(SessionStopReason::Deadline),
            },
            ScriptedSessionAttempt::Processed {
                count: 2,
                stop_reason: None,
            },
        ]));
        let mut started_sessions = Vec::new();
        let mut ended_sessions = Vec::new();
        let mut rotated_to = Vec::new();
        let mut started_items = Vec::new();
        let mut checkpointed_items = Vec::new();

        let summary = run
            .acquire(&characters, false, |event| {
                match event {
                    AcquisitionEvent::SessionStarted { session } => {
                        started_sessions.push(session);
                    }
                    AcquisitionEvent::SessionEnded {
                        session,
                        processed,
                        stop_reason,
                    } => ended_sessions.push((session, processed, stop_reason)),
                    AcquisitionEvent::SessionRotated { next_session, .. } => {
                        rotated_to.push(next_session);
                    }
                    AcquisitionEvent::ItemStarted { index } => started_items.push(index),
                    AcquisitionEvent::ItemCompleted { index, outcome, .. } => {
                        assert!(matches!(outcome.as_ref(), Err(message) if message.starts_with("gif_unknown:")));
                        checkpointed_items.push(index);
                    }
                    _ => {}
                }
                Ok(())
            })
            .unwrap();

        assert_eq!(
            summary,
            AcquisitionSummary {
                processed: 3,
                sessions: 4
            }
        );
        assert_eq!(started_sessions, [3, 4]);
        assert_eq!(ended_sessions.len(), 2);
        assert_eq!(ended_sessions[0].0, 3);
        assert_eq!(ended_sessions[0].1, 1);
        assert_eq!(ended_sessions[0].2, Some(SessionStopReason::Deadline));
        assert_eq!(ended_sessions[1].0, 4);
        assert_eq!(ended_sessions[1].1, 2);
        assert_eq!(ended_sessions[1].2, None);
        assert_eq!(rotated_to, [4]);
        assert_eq!(started_items, [0, 1, 2]);
        assert_eq!(checkpointed_items, [0, 1, 2]);
        assert!(run.test_session_attempts.as_ref().unwrap().is_empty());
        run.close().unwrap();
    }

    #[test]
    fn setup_cleanup_failure_is_not_retried_and_unhealthy_setup_exhaustion_keeps_tail_unstarted() {
        let characters = vec!["漢".into()];

        let mut cleanup_failure = AcquisitionRun::new().unwrap();
        cleanup_failure.test_session_attempts = Some(VecDeque::from([
            ScriptedSessionAttempt::SetupFailure(
                "browser_session_setup_cleanup_failed: ошибка очистки сессии".into(),
            ),
            ScriptedSessionAttempt::Processed {
                count: 1,
                stop_reason: None,
            },
        ]));
        let mut cleanup_events = Vec::new();
        let cleanup_result = cleanup_failure.acquire(&characters, false, |event| {
            cleanup_events.push(event);
            Ok(())
        });
        assert!(matches!(
            cleanup_result,
            Err(AcquisitionStreamError::Provider(message))
                if message.starts_with("browser_session_setup_cleanup_failed:")
        ));
        assert!(cleanup_events.is_empty());
        assert_eq!(
            cleanup_failure
                .test_session_attempts
                .as_ref()
                .unwrap()
                .len(),
            1
        );
        cleanup_failure.close().unwrap();

        let mut exhausted = AcquisitionRun::new().unwrap();
        exhausted.test_session_attempts = Some(VecDeque::from([
            ScriptedSessionAttempt::SetupFailure(format!(
                "{SESSION_SETUP_RUNTIME_FAILURE_PREFIX} ожидающий запрос типа Image"
            )),
            ScriptedSessionAttempt::SetupFailure(format!(
                "{SESSION_SETUP_RUNTIME_FAILURE_PREFIX} ожидающий запрос типа Image"
            )),
            ScriptedSessionAttempt::SetupFailure(format!(
                "{SESSION_SETUP_RUNTIME_FAILURE_PREFIX} ожидающий запрос типа Image"
            )),
        ]));
        let mut exhausted_events = Vec::new();
        let exhausted_result = exhausted.acquire(&characters, false, |event| {
            exhausted_events.push(event);
            Ok(())
        });
        assert!(matches!(
            exhausted_result,
            Err(AcquisitionStreamError::Provider(message))
                if message.starts_with("browser_session_setup_recovery_exhausted:")
        ));
        assert!(exhausted_events.is_empty());
        assert!(exhausted.test_session_attempts.as_ref().unwrap().is_empty());
        exhausted.close().unwrap();
    }

    #[tokio::test]
    async fn interrupt_during_last_checkpoint_is_acknowledged_after_durable_outcome() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let characters = vec!["漢".into()];
        let mut checkpoints = 0;
        let mut driver = FaultInjectedSession {
            monitor: monitor_with_failure(false),
            fail_after_item: false,
            acquisitions: 0,
            stop_at_checkpoint: Some(stop.clone()),
        };
        let summary = process_session_items(&characters, 0, None, &mut driver, &mut |event| {
            if matches!(event, AcquisitionEvent::ItemCompleted { .. }) {
                checkpoints += 1;
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(checkpoints, 1);
        assert_eq!(summary.processed, 1);
        assert_eq!(summary.stop_reason, Some(SessionStopReason::Interrupted));
        assert_eq!(
            session_action(0, 1, &summary).unwrap(),
            SessionAction::Interrupted { next_offset: 1 }
        );
    }

    #[test]
    fn acquisition_run_keeps_accepted_signal_across_rounds() {
        let mut run = AcquisitionRun::new().unwrap();
        run.interrupt.abort();
        run.interrupt = run.runtime.spawn(async { Ok(()) });
        run.runtime.block_on(async {
            while !run.interrupt.is_finished() {
                tokio::task::yield_now().await;
            }
        });
        let mut events = 0;
        for _ in 0..2 {
            let result = run.acquire(&["漢".into()], false, |_| {
                events += 1;
                Ok(())
            });
            assert!(matches!(result, Err(AcquisitionStreamError::Interrupted)));
            assert!(run.is_stopped());
        }
        assert_eq!(events, 0);
    }

    #[test]
    fn browser_runtime_provenance_keeps_selection_method_without_local_paths() {
        let provenance = BrowserRuntimeProvenance {
            product: "Chrome/140.0.0.0".into(),
            protocol_version: "1.3".into(),
            revision: "abc123".into(),
            user_agent: "Mozilla/5.0 Chrome/140".into(),
            js_version: "14.0".into(),
            executable_source: BrowserExecutableSource::PathLookup,
        };
        let encoded = serde_json::to_string(&provenance).unwrap();
        assert!(encoded.contains("path_lookup"));
        assert!(encoded.contains("Chrome/140.0.0.0"));
        assert!(!encoded.contains("/home/"));
        assert!(!encoded.contains("C:\\"));
    }

    #[test]
    fn delayed_primary_and_unready_media_area_are_unknown() {
        let delayed = ImageElementState {
            url: "https://yosida.com/images/kanji/5143.gif".into(),
            complete: false,
            width: 0,
            height: 0,
        };
        let state = classify_primary_state(Some(&delayed), true, true, false, clean_runtime());
        assert!(matches!(state, PrimaryGifState::Unknown { .. }));
        assert!(choose_media_source(state, &[], "柘").is_err());

        let unready_absence = classify_primary_state(None, false, false, false, clean_runtime());
        assert!(matches!(unready_absence, PrimaryGifState::Unknown { .. }));
        assert!(choose_media_source(unready_absence, &[], "柘").is_err());
    }

    #[test]
    fn failed_present_gif_is_not_absence_and_kakijun_is_never_an_admissible_source() {
        let failed = ImageElementState {
            url: "https://yosida.com/images/kanji/5143.gif".into(),
            complete: true,
            width: 0,
            height: 0,
        };
        let state = classify_primary_state(Some(&failed), true, true, true, clean_runtime());
        assert!(matches!(state, PrimaryGifState::Unknown { .. }));
        assert!(choose_media_source(state, &[], "柘").is_err());
        assert!(is_kakijun_host("kakijun.jp"));
        assert!(is_kakijun_host("www.kakijun.jp"));
        assert!(!is_kakijun_host("yosida.com"));
    }

    #[test]
    fn fallback_uses_the_leftmost_visual_sample_and_never_kakijun() {
        let later_image = FontSampleState {
            images: vec![ImageElementState {
                url: "https://example.test/second.png".into(),
                complete: true,
                width: 80,
                height: 100,
            }],
            ..font_sample(1, 20.0, "", "font-sample sans", false)
        };
        let left_text = font_sample(0, 10.0, "柘", "font-sample serif", false);
        assert_eq!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "стабильный и полностью загруженный DOM".into(),
                },
                &[left_text.clone(), later_image],
                "柘",
            )
            .unwrap(),
            MediaSourceChoice::RenderedFontSample {
                sample: left_text,
                absence_proof: Some("стабильный и полностью загруженный DOM".into()),
            },
            "PNG в плитке справа не должен заменять крайний левый font-sample"
        );

        let left_image = FontSampleState {
            images: vec![ImageElementState {
                url: "https://example.test/first.png".into(),
                complete: true,
                width: 80,
                height: 100,
            }],
            ..font_sample(0, 10.0, "柘", "font-sample serif", false)
        };
        let later_text = font_sample(1, 20.0, "柘", "font-sample sans", false);
        assert_eq!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "стабильный и полностью загруженный DOM".into(),
                },
                &[left_image, later_text],
                "柘",
            )
            .unwrap(),
            MediaSourceChoice::Url {
                selection: SelectionResult::LeftmostPngFallback,
                url: "https://example.test/first.png".into(),
                absence_proof: Some("стабильный и полностью загруженный DOM".into()),
            }
        );

        let kakijun_sample = FontSampleState {
            images: vec![ImageElementState {
                url: "https://kakijun.jp/gif/right.png".into(),
                complete: true,
                width: 80,
                height: 100,
            }],
            ..font_sample(0, 10.0, "柘", "font-sample serif", false)
        };
        assert!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "стабильный и полностью загруженный DOM".into(),
                },
                &[kakijun_sample],
                "柘",
            )
            .unwrap_err()
            .contains("kakijun_source_forbidden")
        );
    }

    #[test]
    fn absent_primary_renders_leftmost_exact_font_sample_when_no_png_file_exists() {
        let samples = [
            font_sample(0, 30.0, "柘", "font-sample sans", false),
            font_sample(1, 10.0, "柘", "font-sample serif", false),
            font_sample(2, 0.0, "柘", "font-sample", true),
            font_sample(3, 5.0, "栃", "font-sample serif", false),
        ];
        assert_eq!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "стабильный и полностью загруженный DOM".into(),
                },
                &samples,
                "柘",
            )
            .unwrap(),
            MediaSourceChoice::RenderedFontSample {
                sample: samples[1].clone(),
                absence_proof: Some("стабильный и полностью загруженный DOM".into()),
            }
        );
        assert!(
            choose_media_source(
                PrimaryGifState::AbsentConfirmed {
                    proof: "стабильный и полностью загруженный DOM".into(),
                },
                &[],
                "柘",
            )
            .unwrap_err()
            .contains("fallback_font_sample_missing")
        );
    }

    #[test]
    fn explicit_png_target_uses_the_leftmost_tile_even_when_primary_gif_is_loaded() {
        let samples = [
            font_sample(0, 24.0, "柘", "font-sample sans", false),
            font_sample(1, 10.0, "柘", "font-sample serif", false),
            font_sample(2, 0.0, "柘", "font-sample stroke-orders", false),
        ];
        let primary = PrimaryGifState::PresentLoaded {
            url: "https://example.test/primary.gif".into(),
        };
        assert!(matches!(
            choose_media_source(primary.clone(), &samples, "柘").unwrap(),
            MediaSourceChoice::Url {
                selection: SelectionResult::PrimaryGif,
                ..
            }
        ));
        assert_eq!(
            choose_media_source_for_target(
                primary,
                &samples,
                "柘",
                AcquisitionTarget::RenderedFontSamplePng,
            )
            .unwrap(),
            MediaSourceChoice::RenderedFontSample {
                sample: samples[1].clone(),
                absence_proof: None,
            }
        );
    }

    #[test]
    fn explicit_png_target_can_ignore_primary_gif_only_by_exact_image_url() {
        let snapshot = RuntimeSnapshot {
            pending_requests: vec![
                tracked_request(
                    "gif",
                    ResourceType::Image,
                    "https://example.test/primary.gif",
                    0,
                ),
                tracked_request(
                    "script",
                    ResourceType::Script,
                    "https://example.test/article.js",
                    0,
                ),
            ],
            network_failures: vec![test_outcome(
                ResourceType::Image,
                Some("https://example.test/primary.gif"),
                Some("net::ERR_FAILED"),
                "gif",
                0,
                None,
            )],
            http_errors: vec![test_outcome(
                ResourceType::Image,
                Some("https://example.test/primary.gif"),
                None,
                "gif",
                0,
                Some(404),
            )],
            ..RuntimeSnapshot::default()
        };
        let all = readiness_for_snapshot(&snapshot, |_, _| false);
        assert_eq!(all.pending_relevant_requests, 2);
        let explicit = readiness_for_snapshot(&snapshot, |resource_type, url| {
            resource_type == &ResourceType::Image && url == Some("https://example.test/primary.gif")
        });
        assert_eq!(explicit.pending_relevant_requests, 1);
        assert_eq!(explicit.network_failures, 0);
        assert_eq!(explicit.http_errors, 0);
        let other_primary = readiness_for_snapshot(&snapshot, |resource_type, url| {
            resource_type == &ResourceType::Image && url == Some("https://example.test/other.gif")
        });
        assert_eq!(other_primary.network_failures, 1);
    }

    #[test]
    fn rendered_sample_readiness_tracks_its_image_but_ignores_sibling_images() {
        let selected_url = "https://example.test/sample.png";
        let sibling_url = "https://yosida.example/stroke-order.gif";
        let selected_images = HashSet::from([selected_url.to_owned()]);
        let snapshot = RuntimeSnapshot {
            pending_requests: vec![
                tracked_request("selected-image", ResourceType::Image, selected_url, 1),
                tracked_request("sibling-image", ResourceType::Image, sibling_url, 1),
                tracked_request(
                    "script",
                    ResourceType::Script,
                    "https://example.test/article.js",
                    1,
                ),
            ],
            network_failures: vec![
                test_outcome(
                    ResourceType::Image,
                    Some(sibling_url),
                    Some("net::ERR_ABORTED"),
                    "sibling-image",
                    1,
                    None,
                ),
                test_outcome(
                    ResourceType::Image,
                    Some(selected_url),
                    Some("net::ERR_FAILED"),
                    "selected-image",
                    1,
                    None,
                ),
                test_outcome(
                    ResourceType::Script,
                    Some("https://example.test/article.js"),
                    Some("net::ERR_FAILED"),
                    "script",
                    1,
                    None,
                ),
            ],
            ..RuntimeSnapshot::default()
        };

        let readiness = readiness_for_snapshot(&snapshot, |resource_type, url| {
            !is_relevant_font_sample_resource(resource_type, url, &selected_images)
        });
        assert_eq!(readiness.pending_relevant_requests, 2);
        assert_eq!(readiness.network_failures, 2);
        assert_eq!(readiness.http_errors, 0);
    }

    #[test]
    fn tls_exception_clears_only_the_approved_exact_document_failure() {
        let mut approved_failure = test_outcome(
            ResourceType::Document,
            Some(SITE_URL),
            Some("net::ERR_CERT_AUTHORITY_INVALID"),
            "approved-request",
            0,
            None,
        );
        approved_failure.is_top_level = true;
        let mut attacker_failure = test_outcome(
            ResourceType::Document,
            Some("https://attacker.example/"),
            Some("net::ERR_CERT_AUTHORITY_INVALID"),
            "attacker-request",
            0,
            None,
        );
        attacker_failure.is_top_level = true;
        let telemetry = CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
            network_failures: vec![
                approved_failure,
                attacker_failure,
                test_outcome(
                    ResourceType::Document,
                    Some(SITE_URL),
                    Some("net::ERR_CERT_AUTHORITY_INVALID"),
                    "iframe-request",
                    0,
                    None,
                ),
                test_outcome(
                    ResourceType::Document,
                    Some(SITE_URL),
                    Some("net::ERR_CERT_DATE_INVALID"),
                    "different-error",
                    0,
                    None,
                ),
            ],
            ..RuntimeSnapshot::default()
        });
        let monitor = BrowserEvidenceMonitor::new(telemetry);
        assert_eq!(
            monitor
                .tls_navigation_failure()
                .map(|failure| failure.request_id),
            Some("approved-request".into())
        );
        monitor.clear_explicitly_approved_tls_interstitial_failure("approved-request", SITE_URL);
        let failures = monitor.telemetry.snapshot(0).network_failures;
        assert_eq!(failures.len(), 3);
        assert!(
            failures
                .iter()
                .any(|failure| { failure.url.as_deref() == Some("https://attacker.example/") })
        );
        assert!(failures.iter().any(|failure| {
            failure.failure_reason.as_deref() == Some("net::ERR_CERT_DATE_INVALID")
        }));
    }

    #[test]
    fn network_failure_diagnostics_strip_query_fragment_and_credentials() {
        let telemetry = CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
            network_failures: vec![
                test_outcome(
                    ResourceType::Script,
                    Some("https://user:password@example.test/assets/app.js?token=secret#fragment"),
                    Some("net::ERR_CONNECTION_RESET"),
                    "script",
                    1,
                    None,
                ),
                test_outcome(
                    ResourceType::Image,
                    Some("https://example.test/primary.gif?cache=1"),
                    Some("net::ERR_FAILED"),
                    "primary-image",
                    1,
                    None,
                ),
            ],
            ..RuntimeSnapshot::default()
        });
        let monitor = BrowserEvidenceMonitor::new(telemetry);
        let details = monitor.network_failure_details(1, None);
        assert!(
            details.contains("Script https://example.test/assets/app.js net::ERR_CONNECTION_RESET")
        );
        assert!(!details.contains("user"));
        assert!(!details.contains("password"));
        assert!(!details.contains("token"));
        assert!(!details.contains("secret"));
        assert!(!details.contains("fragment"));

        let exact_primary =
            monitor.network_failure_details(1, Some("https://example.test/primary.gif?cache=1"));
        assert!(!exact_primary.contains("primary.gif"));
        assert!(exact_primary.contains("ERR_CONNECTION_RESET"));
        let other_primary = monitor
            .network_failure_details(1, Some("https://example.test/primary.gif?cache=other"));
        assert!(other_primary.contains("primary.gif"));
    }

    #[test]
    fn network_failure_diagnostics_limit_entries_and_redact_unrecognized_reason_text() {
        let failures = (0..5)
            .map(|index| NetworkOutcome {
                resource_type: ResourceType::Fetch,
                url: Some(format!("https://example.test/api/{index}?secret=hidden")),
                failure_reason: Some(if index == 0 {
                    "сведения о соединении содержат секрет".into()
                } else {
                    "net::ERR_FAILED".into()
                }),
                request_id: format!("request-{index}"),
                epoch: 1,
                status_code: None,
                is_top_level: false,
            })
            .collect();
        let monitor = BrowserEvidenceMonitor::new(CdpRuntimeMonitor::from_snapshot_for_test(
            RuntimeSnapshot {
                network_failures: failures,
                ..RuntimeSnapshot::default()
            },
        ));
        let details = monitor.network_failure_details(1, None);
        assert_eq!(details.matches("Fetch ").count(), 3);
        assert!(details.contains("ещё сетевых ошибок: 2"));
        assert!(details.contains("не классифицированная сетевая ошибка"));
        assert!(!details.contains("secret"));
        assert!(!details.contains("hidden"));
        assert!(!details.contains("сведения о соединении"));
    }

    #[test]
    fn pending_network_js_or_monitor_failure_never_confirms_gif_absence() {
        let dirty_states = [
            RuntimeReadiness {
                pending_relevant_requests: 1,
                ..clean_runtime()
            },
            RuntimeReadiness {
                network_failures: 1,
                ..clean_runtime()
            },
            RuntimeReadiness {
                http_errors: 1,
                ..clean_runtime()
            },
            RuntimeReadiness {
                javascript_exceptions: 1,
                ..clean_runtime()
            },
            RuntimeReadiness {
                monitor_failed: true,
                ..clean_runtime()
            },
        ];
        for runtime in dirty_states {
            assert_ne!(
                session_setup_runtime_state(runtime),
                SessionSetupRuntimeState::Clean
            );
            let state = classify_primary_state(None, true, true, true, runtime);
            assert!(matches!(state, PrimaryGifState::Unknown { .. }));
            assert!(choose_media_source(state, &[], "柘").is_err());
        }
        assert_eq!(
            session_setup_runtime_state(RuntimeReadiness {
                pending_relevant_requests: 1,
                ..clean_runtime()
            }),
            SessionSetupRuntimeState::PendingOnly
        );
        assert_eq!(
            session_setup_runtime_state(RuntimeReadiness {
                network_failures: 1,
                ..clean_runtime()
            }),
            SessionSetupRuntimeState::Failed
        );
        assert_eq!(
            session_setup_runtime_state(clean_runtime()),
            SessionSetupRuntimeState::Clean
        );
        assert!(matches!(
            classify_primary_state(None, true, true, true, clean_runtime()),
            PrimaryGifState::AbsentConfirmed { .. }
        ));
    }

    #[test]
    fn capture_environment_and_dimensions_stay_near_reference_scale() {
        assert!(validate_capture_environment(1280, 900, 2.175, 1.0).is_ok());
        assert!(validate_capture_environment(1280, 900, 2.2, 1.0).is_err());
        assert!(validate_capture_environment(1280, 900, 1.0, 1.0).is_err());
        assert!(validate_capture_environment(1024, 768, 2.0, 1.0).is_err());
        assert!(validate_capture_dimensions(174, 174).is_ok());
        assert!(validate_capture_dimensions(140, 220).is_ok());
        assert!(validate_capture_dimensions(139, 175).is_err());
        assert!(validate_capture_dimensions(173, 500).is_err());
    }

    #[test]
    fn decoded_png_evidence_requires_dark_background_light_glyph_and_frame_pixels() {
        let mut tile = RgbaImage::from_pixel(160, 160, image::Rgba([36, 39, 41, 255]));
        for y in 0..160 {
            for x in 0..160 {
                if !(3..157).contains(&x) || !(3..157).contains(&y) {
                    tile.put_pixel(x, y, image::Rgba([126, 137, 141, 255]));
                }
            }
        }
        for coordinate in 38..122 {
            tile.put_pixel(80, coordinate, image::Rgba([245, 245, 245, 255]));
            tile.put_pixel(coordinate, 80, image::Rgba([245, 245, 245, 255]));
        }
        let evidence = analyze_capture_pixels(&tile).unwrap();
        assert!(evidence.dark_background_pixels > 10_000);
        assert!(evidence.light_glyph_pixels > 100);
        assert!(evidence.frame_pixels > 100);
        assert_eq!(evidence.dark_background_rgb, "rgb(36, 39, 41)");
        assert_eq!(evidence.frame_rgb, "rgb(126, 137, 141)");

        let mut chromium_dark_tile =
            RgbaImage::from_pixel(160, 160, image::Rgba([36, 39, 41, 255]));
        for y in 0..160 {
            for x in 0..160 {
                if !(3..157).contains(&x) || !(3..157).contains(&y) {
                    chromium_dark_tile.put_pixel(x, y, image::Rgba([126, 137, 141, 255]));
                }
            }
        }
        for coordinate in 38..122 {
            chromium_dark_tile.put_pixel(80, coordinate, image::Rgba([195, 214, 236, 255]));
            chromium_dark_tile.put_pixel(coordinate, 80, image::Rgba([195, 214, 236, 255]));
        }
        let chromium_dark_evidence = analyze_capture_pixels(&chromium_dark_tile).unwrap();
        assert!(chromium_dark_evidence.light_glyph_pixels > 100);
        assert_eq!(chromium_dark_evidence.light_glyph_rgb, "rgb(195, 214, 236)");

        let blank_dark = RgbaImage::from_pixel(160, 160, image::Rgba([36, 39, 41, 255]));
        assert!(
            analyze_capture_pixels(&blank_dark)
                .unwrap_err()
                .contains("светлых пикселей glyph")
        );
        let white = RgbaImage::from_pixel(160, 160, image::Rgba([255, 255, 255, 255]));
        assert!(
            analyze_capture_pixels(&white)
                .unwrap_err()
                .contains("тёмных пикселей плитки")
        );
    }

    #[test]
    fn article_unicode_parser_does_not_accept_a_different_article() {
        let text = "Статья №773\nUnicode: 5143\nЧастотность\n192";
        assert_eq!(article_unicode(text), Some("5143".into()));
        assert_ne!(article_unicode(text).as_deref(), Some("672A"));
        assert_eq!(article_unicode("нет метаданных статьи"), None);
        assert_eq!(article_number(text), Some(773));
        assert_eq!(frequency_index(text), Some(192));
        assert_eq!(article_number("№773\nUnicode: 5143"), Some(773));
        assert_eq!(frequency_index("Частотность\n192 / JLPT 2"), Some(192));
        assert_eq!(frequency_index("Частотность\nJLPT 2"), None);
    }

    #[test]
    fn tls_exception_respects_strict_mode_and_requires_exact_failed_url_and_error() {
        let expected_error = "net::ERR_CERT_AUTHORITY_INVALID";
        assert!(
            check_tls_exception(
                true,
                SITE_URL,
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_ok()
        );
        assert!(
            check_tls_exception(
                false,
                SITE_URL,
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                "https://attacker.example/",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                "https://www.yarxi.su.evil.example/",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                "https://www.yarxi.su:444/",
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                SITE_URL,
                "NET::ERR_CERT_DATE_INVALID",
                "net::ERR_CERT_DATE_INVALID",
                true,
            )
            .is_err()
        );
        assert!(
            check_tls_exception(
                true,
                SITE_URL,
                "NET::ERR_CERT_AUTHORITY_INVALID",
                expected_error,
                false,
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod parallel_pool_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::sync::Notify;

    #[derive(Clone, Copy)]
    enum SyntheticMode {
        Terminal,
        Fault,
        SetupFailure(&'static str),
        CleanupFailure,
        TerminalSessionFault,
        AlternatingHealthy,
        FaultCleanupFailure,
        Panic,
        PostCheckpointFault,
        PostCheckpointFaultOnce,
        PostCheckpointInterrupt,
    }

    struct SyntheticDriver {
        mode: SyntheticMode,
        cancellation: CancellationListener,
        gate: Option<Arc<Notify>>,
        wait_index: Option<usize>,
        cleanup: Arc<Mutex<Vec<&'static str>>>,
        acquisitions: usize,
    }

    impl ParallelDriver for SyntheticDriver {
        async fn acquire(
            &mut self,
            job: &ParallelJob,
            context: &mut ParallelWorkerContext,
            sender: &mpsc::UnboundedSender<ParallelMessage>,
        ) -> Result<ParallelAcquired, AcquisitionStreamError> {
            self.acquisitions += 1;
            context.session += 1;
            context.browser_session =
                Some(context.worker_id as u64 * 100 + u64::from(context.session));
            send_parallel_event(
                sender,
                context,
                AcquisitionWorkerEventKind::SessionBootstrapStarted,
            )?;
            send_parallel_event(
                sender,
                context,
                AcquisitionWorkerEventKind::Provider(AcquisitionEvent::SessionStarted {
                    session: context.session,
                }),
            )?;
            if self.wait_index == Some(job.index) {
                let gate = self.gate.as_ref().expect("test gate");
                tokio::select! {
                    biased;
                    _ = gate.notified() => {},
                    reason = self.cancellation.cancelled() => {
                        return Err(parallel_cancellation_error(reason));
                    },
                }
            }
            match self.mode {
                SyntheticMode::Fault => Err(AcquisitionStreamError::Provider(format!(
                    "{SESSION_SETUP_RUNTIME_FAILURE_PREFIX} injected"
                ))),
                SyntheticMode::SetupFailure(message) => {
                    Err(AcquisitionStreamError::Provider(message.into()))
                }
                SyntheticMode::Panic => panic!("injected worker panic"),
                mode => Ok(ParallelAcquired {
                    outcome: Some(Err("gif_unknown: synthetic terminal failure".into())),
                    diagnostics: None,
                    stop: (matches!(
                        mode,
                        SyntheticMode::TerminalSessionFault | SyntheticMode::FaultCleanupFailure
                    ) || (matches!(mode, SyntheticMode::AlternatingHealthy)
                        && !self.acquisitions.is_multiple_of(3)))
                    .then_some(SessionStopReason::ItemTimeout),
                }),
            }
        }
        async fn after_checkpoint(
            &mut self,
        ) -> Result<Option<SessionStopReason>, AcquisitionStreamError> {
            self.cleanup.lock().unwrap().push("health_checked");
            match self.mode {
                SyntheticMode::PostCheckpointFault | SyntheticMode::PostCheckpointFaultOnce
                    if matches!(self.mode, SyntheticMode::PostCheckpointFault)
                        || self.acquisitions == 1 =>
                {
                    Ok(monitor_stop_reason(&BrowserEvidenceMonitor::new(
                        CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
                            monitor_failed: true,
                            ..RuntimeSnapshot::default()
                        }),
                    )))
                }
                SyntheticMode::PostCheckpointInterrupt => Err(AcquisitionStreamError::Interrupted),
                _ => Ok(None),
            }
        }
        async fn rotate(&mut self) -> Result<(), AcquisitionStreamError> {
            self.cleanup.lock().unwrap().push("session_closed");
            if matches!(
                self.mode,
                SyntheticMode::CleanupFailure | SyntheticMode::FaultCleanupFailure
            ) {
                Err(AcquisitionStreamError::Provider(
                    "browser_worker_cleanup_failed: injected".into(),
                ))
            } else {
                Ok(())
            }
        }
        fn finish(&mut self) -> Result<(), AcquisitionStreamError> {
            self.cleanup.lock().unwrap().push("workspace_closed");
            Ok(())
        }
    }

    fn jobs(count: usize) -> VecDeque<ParallelJob> {
        (0..count)
            .map(|index| ParallelJob {
                index,
                character: char::from_u32(0x4e00 + index as u32).unwrap().to_string(),
                generation: 7,
                owner_attempt: 2,
            })
            .collect()
    }
    fn driver(
        mode: SyntheticMode,
        cancellation: &PoolCancellation,
        cleanup: &Arc<Mutex<Vec<&'static str>>>,
    ) -> SyntheticDriver {
        SyntheticDriver {
            mode,
            cancellation: cancellation.subscribe(),
            gate: None,
            wait_index: None,
            cleanup: cleanup.clone(),
            acquisitions: 0,
        }
    }
    fn completion(event: &AcquisitionWorkerEvent) -> bool {
        matches!(
            event.event,
            AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted { .. })
        )
    }

    #[test]
    fn item_relay_suppresses_heartbeats_and_preserves_retry_context() {
        let (sender, mut messages) = mpsc::unbounded_channel();
        let job = jobs(8).pop_back().unwrap();
        let mut context = ParallelWorkerContext {
            worker_id: 2,
            session: 3,
            browser_session: Some(203),
            attempt: 1,
            job: Some(job.clone()),
        };
        for attempt in 2..=3 {
            relay_parallel_item_event(
                &sender,
                &mut context,
                job.index,
                AcquisitionEvent::RetryStarted { index: 0, attempt },
            )
            .unwrap();
            relay_parallel_item_event(
                &sender,
                &mut context,
                job.index,
                AcquisitionEvent::Heartbeat { index: 0, attempt },
            )
            .unwrap();
            relay_parallel_item_event(
                &sender,
                &mut context,
                job.index,
                AcquisitionEvent::RetryRecoveryStarted { index: 0, attempt },
            )
            .unwrap();
            for recovery in [false, true] {
                let ParallelMessage::Event(event, ack) = messages.try_recv().unwrap() else {
                    panic!("ожидалось событие попытки");
                };
                assert!(ack.is_none());
                assert_eq!(
                    (event.worker_id, event.session, event.browser_session),
                    (2, 3, Some(203))
                );
                assert_eq!(
                    (
                        event.index,
                        event.generation,
                        event.owner_attempt,
                        event.attempt
                    ),
                    (Some(7), Some(7), Some(2), attempt)
                );
                match event.event {
                    AcquisitionWorkerEventKind::Provider(AcquisitionEvent::RetryStarted {
                        index,
                        attempt: observed,
                    }) if !recovery => assert_eq!((index, observed), (7, attempt)),
                    AcquisitionWorkerEventKind::Provider(
                        AcquisitionEvent::RetryRecoveryStarted {
                            index,
                            attempt: observed,
                        },
                    ) if recovery => assert_eq!((index, observed), (7, attempt)),
                    unexpected => panic!("лишнее или неверное событие: {unexpected:?}"),
                }
            }
            assert!(matches!(
                messages.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        drop(messages);
        assert!(
            relay_parallel_item_event(
                &sender,
                &mut context,
                job.index,
                AcquisitionEvent::RetryStarted {
                    index: 0,
                    attempt: 4
                }
            )
            .is_err()
        );
    }

    #[test]
    fn assignment_rejects_wrong_token_and_duplicate_completion() {
        let job = jobs(1).pop_front().unwrap();
        let (command, _received) = mpsc::unbounded_channel();
        let mut assignments = ParallelAssignments::new([job.clone()], vec![command]);
        assignments.mark_ready(1).unwrap();
        assignments.dispatch().unwrap();
        let context = ParallelWorkerContext {
            worker_id: 1,
            job: Some(job),
            ..ParallelWorkerContext::default()
        };
        for invalid in [
            "generation",
            "attempt",
            "index",
            "zero-worker",
            "outside-worker",
        ] {
            let mut stale = context.event(AcquisitionWorkerEventKind::Assigned);
            match invalid {
                "generation" => stale.generation = Some(8),
                "attempt" => stale.owner_attempt = Some(3),
                "index" => stale.index = Some(1),
                "zero-worker" => stale.worker_id = 0,
                "outside-worker" => stale.worker_id = 2,
                _ => unreachable!(),
            }
            assert!(
                accept_parallel_result(&mut assignments, &stale).is_err(),
                "{invalid}"
            );
            assert!(assignments.active(1).is_some());
        }
        let current = context.event(AcquisitionWorkerEventKind::Assigned);
        assert!(accept_parallel_result(&mut assignments, &current).is_ok());
        assert!(accept_parallel_result(&mut assignments, &current).is_err());
    }

    #[tokio::test]
    async fn checkpoint_health_barrier_reports_last_and_intermediate_fault_without_reassignment() {
        for (mode, count) in [
            (SyntheticMode::PostCheckpointFault, 1),
            (SyntheticMode::PostCheckpointFault, 2),
            (SyntheticMode::PostCheckpointFault, 3),
            (SyntheticMode::PostCheckpointFaultOnce, 2),
            (SyntheticMode::Terminal, 1),
            (SyntheticMode::PostCheckpointInterrupt, 1),
        ] {
            let cancel = PoolCancellation::default();
            let cleanup = Arc::new(Mutex::new(Vec::new()));
            let mut completed = Vec::new();
            let mut ended = Vec::new();
            let mut stopped_reason = None;
            let mut deferred = 0;
            let result = coordinate_parallel(
                vec![driver(mode, &cancel, &cleanup)],
                jobs(count),
                futures::future::pending(),
                cancel,
                &mut |event| {
                    if let AcquisitionWorkerEventKind::WorkerStopped { reason } = &event.event {
                        stopped_reason = Some(reason.clone());
                    }
                    if completion(&event) {
                        completed.push((
                            event.index.unwrap(),
                            event.generation,
                            event.owner_attempt,
                        ));
                        cleanup.lock().unwrap().push("checkpoint_saved");
                    }
                    if let AcquisitionWorkerEventKind::Provider(AcquisitionEvent::SessionEnded {
                        stop_reason,
                        ..
                    }) = event.event
                    {
                        ended.push(stop_reason);
                    } else if matches!(event.event, AcquisitionWorkerEventKind::Deferred { .. }) {
                        deferred += 1;
                    }
                    Ok(())
                },
            )
            .await;
            assert_eq!(
                completed,
                (0..count)
                    .map(|index| (index, Some(7), Some(2)))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                deferred, 0,
                "сохранённый результат нельзя возвращать в очередь"
            );
            if matches!(mode, SyntheticMode::PostCheckpointInterrupt) {
                assert!(matches!(result, Err(AcquisitionStreamError::Interrupted)));
                assert_eq!(
                    stopped_reason.as_deref(),
                    Some("acquisition_interrupted: получен Ctrl+C")
                );
            } else if matches!(mode, SyntheticMode::PostCheckpointFault) {
                assert!(
                    matches!(result, Err(AcquisitionStreamError::Provider(ref message))
                    if message.contains("browser_network_runtime_failure")),
                    "{result:?}"
                );
                assert_eq!(ended.len(), count);
                assert!(ended.iter().all(
                    |reason| matches!(reason, Some(SessionStopReason::RuntimeFailure(message))
                    if message.starts_with("browser_network_runtime_failure:"))
                ));
            } else {
                assert_eq!(result.unwrap().processed, count);
                if matches!(mode, SyntheticMode::PostCheckpointFaultOnce) {
                    assert!(matches!(
                        &ended[0],
                        Some(SessionStopReason::RuntimeFailure(_))
                    ));
                    assert_eq!(ended[1], None);
                } else {
                    assert_eq!(ended, [None]);
                }
            }
            let trace = cleanup.lock().unwrap();
            assert_eq!(
                trace
                    .iter()
                    .filter(|stage| **stage == "health_checked")
                    .count(),
                count
            );
            for (index, stage) in trace.iter().enumerate() {
                if *stage == "health_checked" {
                    assert_eq!(trace[index - 1], "checkpoint_saved");
                }
            }
            assert_eq!(trace.last(), Some(&"workspace_closed"));
        }
    }

    #[tokio::test]
    async fn pool_checkpoints_out_of_order_before_slow_assignment_finishes() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Notify::new());
        let drivers = (0..2)
            .map(|_| {
                let mut driver = driver(SyntheticMode::Terminal, &cancel, &cleanup);
                driver.gate = Some(gate.clone());
                driver.wait_index = Some(0);
                driver
            })
            .collect();
        let mut completed = Vec::new();
        let summary = coordinate_parallel(
            drivers,
            jobs(4),
            futures::future::pending(),
            cancel,
            &mut |event| {
                if completion(&event) {
                    completed.push(event.index.unwrap());
                    if event.index == Some(1) {
                        gate.notify_one();
                    }
                }
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(summary.processed, 4);
        assert_eq!(completed[0], 1);
        completed.sort_unstable();
        assert_eq!(completed, vec![0, 1, 2, 3]);
        assert_eq!(
            cleanup
                .lock()
                .unwrap()
                .iter()
                .filter(|value| **value == "workspace_closed")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn rotated_sessions_keep_worker_local_browser_context() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let drivers = vec![
            driver(SyntheticMode::TerminalSessionFault, &cancel, &cleanup),
            driver(SyntheticMode::TerminalSessionFault, &cancel, &cleanup),
        ];
        let mut bootstraps = std::collections::BTreeMap::<usize, Vec<(u32, u64)>>::new();
        let mut rotations = 0;
        let mut retired = 0;
        let summary = coordinate_parallel(
            drivers,
            jobs(6),
            futures::future::pending(),
            cancel,
            &mut |event| {
                if matches!(
                    event.event,
                    AcquisitionWorkerEventKind::SessionBootstrapStarted
                ) {
                    bootstraps.entry(event.worker_id).or_default().push((
                        event.session,
                        event
                            .browser_session
                            .expect("bootstrap уже владеет session ID"),
                    ));
                }
                if matches!(
                    event.event,
                    AcquisitionWorkerEventKind::Provider(AcquisitionEvent::SessionRotated { .. })
                ) {
                    rotations += 1;
                }
                if matches!(
                    event.event,
                    AcquisitionWorkerEventKind::WorkerRetired { .. }
                ) {
                    retired += 1;
                }
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(summary.processed, 6);
        assert_eq!(
            rotations, 4,
            "каждый обработчик меняет сессию дважды до исчерпания бюджета восстановления"
        );
        assert_eq!(retired, 2);
        assert_eq!(bootstraps[&1], [(1, 101), (2, 102), (3, 103)]);
        assert_eq!(bootstraps[&2], [(1, 201), (2, 202), (3, 203)]);
        assert!(
            bootstraps[&1]
                .iter()
                .all(|(_, browser)| !bootstraps[&2].iter().any(|(_, other)| browser == other)),
            "идентификаторы сессий сохраняют привязку к обработчику при смене сессии"
        );
    }

    #[tokio::test]
    async fn sick_worker_requeues_without_terminal_attempt_and_healthy_neighbor_finishes() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let drivers = vec![
            driver(SyntheticMode::Fault, &cancel, &cleanup),
            driver(SyntheticMode::Terminal, &cancel, &cleanup),
        ];
        let mut completed = Vec::new();
        let mut deferred = 0;
        let summary = coordinate_parallel(
            drivers,
            jobs(12),
            futures::future::pending(),
            cancel,
            &mut |event| {
                if completion(&event) {
                    completed.push(event.index.unwrap());
                }
                if matches!(event.event, AcquisitionWorkerEventKind::Deferred { .. }) {
                    deferred += 1;
                }
                assert!(event.owner_attempt.is_none_or(|attempt| attempt == 2));
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(summary.processed, 12);
        assert!(deferred > 0);
        completed.sort_unstable();
        assert_eq!(completed, (0..12).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn permanent_setup_errors_preserve_cause_without_requeue_or_pool_exhaustion() {
        for message in [
            "запуск браузера: исполняемый файл не найден",
            "tls_interstitial: строгий режим запрещает исключение TLS",
            "provider_host_mismatch: загрузка ушла с www.yarxi.su",
            "browser_worker_session_fault: постоянный отказ запуска",
            "browser_worker_cleanup_failed: ошибка очистки",
        ] {
            let cancel = PoolCancellation::default();
            let cleanup = Arc::new(Mutex::new(Vec::new()));
            let mut assigned = 0;
            let mut deferred = 0;
            let mut rotated = 0;
            let mut retired = 0;
            let mut terminal = 0;
            let mut stopped_reason = None;
            let error = coordinate_parallel(
                vec![driver(
                    SyntheticMode::SetupFailure(message),
                    &cancel,
                    &cleanup,
                )],
                jobs(3),
                futures::future::pending(),
                cancel,
                &mut |event| {
                    match event.event {
                        AcquisitionWorkerEventKind::Assigned => assigned += 1,
                        AcquisitionWorkerEventKind::Deferred { .. }
                        | AcquisitionWorkerEventKind::Requeued { .. } => deferred += 1,
                        AcquisitionWorkerEventKind::WorkerRetired { .. } => retired += 1,
                        AcquisitionWorkerEventKind::WorkerStopped { reason } => {
                            stopped_reason = Some(reason)
                        }
                        AcquisitionWorkerEventKind::Provider(
                            AcquisitionEvent::SessionRotated { .. },
                        ) => rotated += 1,
                        AcquisitionWorkerEventKind::Provider(AcquisitionEvent::ItemCompleted {
                            ..
                        }) => terminal += 1,
                        _ => {}
                    }
                    Ok(())
                },
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, AcquisitionStreamError::Provider(ref cause) if cause == message),
                "{error:?}"
            );
            assert_eq!(assigned, 1);
            assert_eq!(stopped_reason.as_deref(), Some(message));
            assert_eq!((deferred, rotated, retired, terminal), (0, 0, 0, 0));
            assert_eq!(
                *cleanup.lock().unwrap(),
                ["session_closed", "workspace_closed"]
            );
        }
    }

    #[tokio::test]
    async fn exhaustion_returns_exact_frontier_without_synthetic_outcomes() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let drivers = vec![
            driver(SyntheticMode::Fault, &cancel, &cleanup),
            driver(SyntheticMode::Fault, &cancel, &cleanup),
        ];
        let mut terminal = 0;
        let mut retired = 0;
        let error = coordinate_parallel(
            drivers,
            jobs(7),
            futures::future::pending(),
            cancel,
            &mut |event| {
                if completion(&event) {
                    terminal += 1;
                }
                if matches!(
                    event.event,
                    AcquisitionWorkerEventKind::WorkerRetired { .. }
                ) {
                    retired += 1;
                }
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, AcquisitionStreamError::WorkersExhausted { pending_indices } if pending_indices == (0..7).collect::<Vec<_>>())
        );
        assert_eq!(terminal, 0);
        assert_eq!(retired, 2);
        assert_eq!(
            cleanup
                .lock()
                .unwrap()
                .iter()
                .filter(|value| **value == "workspace_closed")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn healthy_terminal_progress_resets_recovery_budget() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let drivers = vec![
            driver(SyntheticMode::AlternatingHealthy, &cancel, &cleanup),
            driver(SyntheticMode::AlternatingHealthy, &cancel, &cleanup),
        ];
        let mut retired = 0;
        let summary = coordinate_parallel(
            drivers,
            jobs(12),
            futures::future::pending(),
            cancel,
            &mut |event| {
                if matches!(
                    event.event,
                    AcquisitionWorkerEventKind::WorkerRetired { .. }
                ) {
                    retired += 1;
                }
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(summary.processed, 12);
        assert_eq!(retired, 0);
    }

    #[tokio::test]
    async fn cancellation_after_checkpoint_preserves_completed_and_defers_running_job() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Notify::new());
        let drivers = (0..2)
            .map(|_| {
                let mut driver = driver(SyntheticMode::Terminal, &cancel, &cleanup);
                driver.gate = Some(gate.clone());
                driver.wait_index = Some(0);
                driver
            })
            .collect();
        let (interrupt, received) = oneshot::channel();
        let mut interrupt = Some(interrupt);
        let mut completed = Vec::new();
        let mut deferred = Vec::new();
        let result = coordinate_parallel(
            drivers,
            jobs(5),
            async {
                let _ = received.await;
                AcquisitionStreamError::Interrupted
            },
            cancel,
            &mut |event| {
                if completion(&event) {
                    completed.push(event.index.unwrap());
                    if let Some(interrupt) = interrupt.take() {
                        let _ = interrupt.send(());
                    }
                }
                if matches!(event.event, AcquisitionWorkerEventKind::Deferred { .. }) {
                    deferred.push(event.index.unwrap());
                }
                Ok(())
            },
        )
        .await;
        assert!(matches!(result, Err(AcquisitionStreamError::Interrupted)));
        assert!(completed.contains(&1));
        assert!(!completed.contains(&0));
        assert!(deferred.contains(&0));
        assert_eq!(
            cleanup
                .lock()
                .unwrap()
                .iter()
                .filter(|value| **value == "workspace_closed")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn callback_error_and_worker_panic_both_wait_for_all_cleanup() {
        for mode in [SyntheticMode::Terminal, SyntheticMode::Panic] {
            let cancel = PoolCancellation::default();
            let cleanup = Arc::new(Mutex::new(Vec::new()));
            let drivers = vec![
                driver(mode, &cancel, &cleanup),
                driver(SyntheticMode::Terminal, &cancel, &cleanup),
            ];
            let mut callback_failed = false;
            let result = coordinate_parallel(
                drivers,
                jobs(5),
                futures::future::pending(),
                cancel,
                &mut |event| {
                    if completion(&event) && !callback_failed {
                        callback_failed = true;
                        return Err(AssetError::new(
                            crate::error::ErrorCode::InvalidTransition,
                            "injected checkpoint error",
                        ));
                    }
                    Ok(())
                },
            )
            .await;
            let error = result.unwrap_err();
            if matches!(mode, SyntheticMode::Terminal) {
                assert!(matches!(
                    error,
                    AcquisitionStreamError::Consumer(ref error)
                        if error.code == crate::error::ErrorCode::InvalidTransition
                ));
            } else {
                assert!(!matches!(error, AcquisitionStreamError::Interrupted));
            }
            assert_eq!(
                cleanup
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|value| **value == "workspace_closed")
                    .count(),
                2
            );
        }
    }

    #[tokio::test]
    async fn coordinator_failure_cancellation_is_deferred_with_its_real_reason() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Notify::new());
        let mut waiting = driver(SyntheticMode::Terminal, &cancel, &cleanup);
        waiting.gate = Some(gate);
        waiting.wait_index = Some(0);
        let drivers = vec![waiting, driver(SyntheticMode::Terminal, &cancel, &cleanup)];
        let mut deferred = Vec::new();
        let result = coordinate_parallel(
            drivers,
            jobs(4),
            futures::future::pending(),
            cancel,
            &mut |event| {
                if completion(&event) && event.index == Some(1) {
                    return Err(AssetError::new(
                        crate::error::ErrorCode::InvalidTransition,
                        "injected checkpoint error",
                    ));
                }
                if let AcquisitionWorkerEventKind::Deferred { reason } = &event.event {
                    deferred.push((event.index.unwrap(), reason.clone()));
                }
                Ok(())
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(AcquisitionStreamError::Consumer(ref error))
                if error.code == crate::error::ErrorCode::InvalidTransition
        ));
        assert!(deferred.contains(&(0, "coordinator_failure".into())));
        assert!(!deferred.iter().any(|(_, reason)| reason == "interrupted"));
        assert!(matches!(
            parallel_cancellation_error(CancellationReason::CoordinatorFailure),
            AcquisitionStreamError::Provider(message) if message == PARALLEL_COORDINATOR_CANCELLED
        ));
        assert!(matches!(
            parallel_cancellation_error(CancellationReason::Interrupted),
            AcquisitionStreamError::Interrupted
        ));
        assert!(matches!(
            signal_error(Ok(Err(std::io::Error::other(ParallelCancellation(
                CancellationReason::CoordinatorFailure,
            ))))),
            AcquisitionStreamError::Provider(message) if message == PARALLEL_COORDINATOR_CANCELLED
        ));
        assert!(matches!(
            signal_error(Ok(Ok(()))),
            AcquisitionStreamError::Interrupted
        ));
        assert_eq!(
            cleanup
                .lock()
                .unwrap()
                .iter()
                .filter(|value| **value == "workspace_closed")
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn cleanup_failure_is_fatal_after_accepted_terminal_outcome() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let drivers = vec![
            driver(SyntheticMode::CleanupFailure, &cancel, &cleanup),
            driver(SyntheticMode::Terminal, &cancel, &cleanup),
        ];
        let mut terminal = 0;
        let error = coordinate_parallel(
            drivers,
            jobs(2),
            futures::future::pending(),
            cancel,
            &mut |event| {
                if completion(&event) {
                    terminal += 1;
                }
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, AcquisitionStreamError::Provider(message) if message.contains("cleanup_failed"))
        );
        assert_eq!(terminal, 2);
    }
    #[tokio::test]
    async fn repeated_terminal_timeouts_retire_workers_after_durable_results() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let drivers = vec![
            driver(SyntheticMode::TerminalSessionFault, &cancel, &cleanup),
            driver(SyntheticMode::TerminalSessionFault, &cancel, &cleanup),
        ];
        let mut completed = Vec::new();
        let mut retired = Vec::new();
        let error = coordinate_parallel(
            drivers,
            jobs(12),
            futures::future::pending(),
            cancel,
            &mut |event| {
                if completion(&event) {
                    completed.push(event.index.unwrap());
                }
                if let AcquisitionWorkerEventKind::WorkerRetired {
                    consecutive_failures,
                    ..
                } = event.event
                {
                    retired.push(consecutive_failures);
                }
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert_eq!(completed.len(), PARALLEL_RECOVERY_BUDGET as usize * 2);
        assert_eq!(retired, vec![PARALLEL_RECOVERY_BUDGET as u8; 2]);
        let AcquisitionStreamError::WorkersExhausted { pending_indices } = error else {
            panic!("expected pool exhaustion")
        };
        completed.extend(pending_indices);
        completed.sort_unstable();
        assert_eq!(completed, (0..12).collect::<Vec<_>>());
    }

    #[test]
    fn failed_stop_is_fatal_before_join_and_normal_retirement_is_not() {
        let context = ParallelWorkerContext {
            worker_id: 1,
            ..ParallelWorkerContext::default()
        };
        let failed = context.event(AcquisitionWorkerEventKind::WorkerStopped {
            reason: "browser_worker_cleanup_failed".into(),
        });
        assert!(stopped_parallel_failure(&failed).is_some());
        let finished = context.event(AcquisitionWorkerEventKind::WorkerStopped {
            reason: "finished".into(),
        });
        assert!(stopped_parallel_failure(&finished).is_none());
    }

    #[tokio::test]
    async fn failed_stop_prevents_refill_while_healthy_ready_and_join_can_race() {
        let cancel = PoolCancellation::default();
        let cleanup = Arc::new(Mutex::new(Vec::new()));
        let drivers = vec![
            driver(SyntheticMode::FaultCleanupFailure, &cancel, &cleanup),
            driver(SyntheticMode::Terminal, &cancel, &cleanup),
        ];
        let mut fatal_seen = false;
        let mut completed = Vec::new();
        let error = coordinate_parallel(
            drivers,
            jobs(20),
            futures::future::pending(),
            cancel,
            &mut |event| {
                if matches!(event.event, AcquisitionWorkerEventKind::Assigned) {
                    assert!(!fatal_seen, "assignment after known fatal stop");
                }
                if completion(&event) {
                    completed.push(event.index.unwrap());
                }
                if stopped_parallel_failure(&event).is_some() {
                    fatal_seen = true;
                }
                Ok(())
            },
        )
        .await
        .unwrap_err();
        assert!(fatal_seen);
        assert!(
            matches!(error, AcquisitionStreamError::Provider(message) if message.contains("cleanup_failed"))
        );
        assert!(!completed.is_empty());
        completed.sort_unstable();
        completed.dedup();
        assert!(completed.len() < 20);
        assert_eq!(
            cleanup
                .lock()
                .unwrap()
                .iter()
                .filter(|value| **value == "workspace_closed")
                .count(),
            2
        );
    }
}
