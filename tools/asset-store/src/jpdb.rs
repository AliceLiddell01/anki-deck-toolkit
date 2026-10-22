//! Разрешение словарных записей JPDB и получение графиков акцентуации из браузерного рендеринга.
//!
//! Источником истины служат страницы JPDB, отрисованные на английском языке.
//! Модуль не вызывает закрытые API и не реконструирует графики акцентуации.
//! Элементы одного пакета последовательно используют изолированную `BrowserSession`.

use std::future::Future;
use std::path::Path;
use std::time::Duration;

use chromiumoxide::{
    Page,
    cdp::browser_protocol::{
        emulation::{
            MediaFeature, ResetPageScaleFactorParams, SetDeviceMetricsOverrideParams,
            SetEmulatedMediaParams, SetLocaleOverrideParams,
        },
        page::{CaptureScreenshotFormat, CaptureScreenshotParams, Viewport},
    },
};
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::{Instant, sleep, timeout};
use tracing::Instrument;
use url::Url;

use crate::browser_diagnostics::{BrowserItemContext, BrowserItemTimer, BrowserStageTimer};
use crate::diagnostics::{safe_message, safe_route};

use crate::browser_runtime::{
    BrowserRuntimeConfig, BrowserSession, CdpRuntimeMonitor, DeviceMetrics, RuntimeSnapshot,
    is_relevant_resource_type,
};
use crate::hashing::sha256_hex;
use crate::pitch_accent::{
    PITCH_ACCENT_CAPTURE_GEOMETRY_TOLERANCE_CSS_PX, PITCH_ACCENT_CAPTURE_PADDING_CSS_PX,
    PITCH_ACCENT_MAX_VARIANT_COUNT, PitchAccentCaptureRect, PitchAccentCoordinateSpace,
    PitchAccentDarkThemeProof, PitchAccentDomainMetadata, PitchAccentEvidence, PitchAccentProvider,
    PitchAccentRenderEvidence, PitchAccentRenderKind, PitchAccentResolvedForm,
    PitchAccentVariantEvidence, PitchAccentVariantSelection, PitchAccentVariantSelectionBasis,
    jpdb_readings_equivalent, parse_jpdb_used_in_route, parse_jpdb_vocabulary_route,
    pitch_accent_capture_rect_for, pitch_accent_variant_selection_score,
    relative_luminance as pitch_accent_relative_luminance, select_pitch_accent_variant,
    validate_capture_background as validate_evidence_capture_background,
    validate_capture_geometry as validate_evidence_capture_geometry,
    validate_capture_margins as validate_evidence_capture_margins,
};

const JPDB_ORIGIN: &str = "https://jpdb.io";
const JPDB_HOST: &str = "jpdb.io";
const CAPTURE_VIEWPORT_WIDTH: i64 = 1280;
const CAPTURE_VIEWPORT_HEIGHT: i64 = 1200;
const CAPTURE_DEVICE_SCALE_FACTOR: f64 = 3.0;
const ITEM_TIMEOUT: Duration = Duration::from_secs(90);
const READINESS_TIMEOUT: Duration = Duration::from_secs(20);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const GRAPH_STABILITY_INTERVAL: Duration = Duration::from_millis(250);
const MAX_SEARCH_RESULTS: usize = 128;

/// Единые настройки браузера для получения графиков JPDB.
pub fn pitch_browser_runtime_config() -> BrowserRuntimeConfig {
    BrowserRuntimeConfig {
        device_metrics: Some(
            DeviceMetrics::new(
                CAPTURE_VIEWPORT_WIDTH,
                CAPTURE_VIEWPORT_HEIGHT,
                CAPTURE_DEVICE_SCALE_FACTOR,
            )
            .expect("Заданные метрики захвата JPDB должны быть корректными"),
        ),
        prefers_color_scheme: Some("dark".into()),
        ..BrowserRuntimeConfig::default()
    }
}

/// Форма написания и необязательное чтение для поиска в JPDB.
/// Чтения в хирагане и катакане считаются эквивалентными; написание сравнивается точно.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbPitchQuery {
    pub surface: String,
    pub reading: Option<String>,
}

impl JpdbPitchQuery {
    pub fn new(surface: impl Into<String>, reading: Option<String>) -> Self {
        Self {
            surface: surface.into(),
            reading,
        }
    }

    fn validate(&self) -> Result<(), String> {
        if self.surface.trim().is_empty() {
            return Err("Поле surface не должно быть пустым".into());
        }
        if self
            .reading
            .as_deref()
            .is_some_and(|reading| reading.trim().is_empty())
        {
            return Err("Поле reading должно отсутствовать или содержать текст".into());
        }
        Ok(())
    }
}

/// Запрос на получение с необязательным явным выбором словарной записи.
///
/// Выбор не обходит поиск: провайдер заново получает список JPDB-кандидатов и
/// принимает выбор только при совпадении ID и полного маршрута одного из них.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbPitchRequest {
    pub query: JpdbPitchQuery,
    pub selection: Option<JpdbPitchSelection>,
}

impl JpdbPitchRequest {
    pub fn new(query: JpdbPitchQuery) -> Self {
        Self {
            query,
            selection: None,
        }
    }

    pub fn with_selection(query: JpdbPitchQuery, selection: JpdbPitchSelection) -> Self {
        Self {
            query,
            selection: Some(selection),
        }
    }
}

/// Выбор словарной записи, связанный с проверенным маршрутом JPDB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbPitchSelection {
    pub vocabulary_id: u64,
    pub detail_url: String,
}

impl JpdbPitchSelection {
    /// Создаёт явный выбор и сверяет маршрут с ID словарной записи.
    pub fn new(vocabulary_id: u64, detail_url: impl Into<String>) -> Result<Self, String> {
        let selection = Self {
            vocabulary_id,
            detail_url: detail_url.into(),
        };
        selection.validate()?;
        Ok(selection)
    }

    fn validate(&self) -> Result<crate::pitch_accent::JpdbVocabularyRoute, String> {
        let route = parse_jpdb_vocabulary_route(&self.detail_url).map_err(|_| {
            "Ссылка выбора не является допустимым маршрутом словарной записи JPDB".to_owned()
        })?;
        if self.vocabulary_id == 0 || route.vocabulary_id != self.vocabulary_id {
            return Err("ID выбора не совпадает с ID в маршруте словарной записи JPDB".into());
        }
        Ok(route)
    }
}

/// Результат получения из браузера для одного запроса.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum JpdbPitchOutcome {
    Acquired {
        asset: Box<JpdbPitchAcquired>,
    },
    NoPitchAccentOnSource {
        evidence: JpdbPitchAbsenceEvidence,
    },
    AmbiguousVocabulary {
        surface: String,
        reading: Option<String>,
        candidates: Vec<JpdbVocabularyCandidate>,
    },
    VocabularyNotFound {
        surface: String,
        reading: Option<String>,
    },
    Failed {
        error: JpdbPitchFailure,
    },
}

/// Обработанный префикс пакета и необязательная причина остановки сессии.
///
/// `outcomes` содержит только запросы, обработка которых началась. Ошибки запуска
/// и настройки браузера не создают результаты элементов; после потери телеметрии
/// необработанный хвост очереди также отсутствует.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbPitchAcquisitionReport {
    pub outcomes: Vec<JpdbPitchOutcome>,
    pub session_failure: Option<JpdbPitchFailure>,
}

impl JpdbPitchAcquisitionReport {
    fn from_session_failure(error: JpdbPitchFailure) -> Self {
        trace_failure(&error);
        Self {
            outcomes: Vec::new(),
            session_failure: Some(error),
        }
    }

    fn stop_if_monitor_failed(&mut self, stage: JpdbPitchStage, monitor_failed: bool) -> bool {
        if monitor_failed {
            tracing::error!(
                stage = ?stage,
                monitor_failed,
                processed_count = self.outcomes.len(),
                "Очередь JPDB остановлена после отказа монитора CDP"
            );
            self.session_failure = Some(JpdbPitchFailure::SessionFailure {
                stage,
                message: "Монитор телеметрии CDP завершился с ошибкой; очередь остановлена".into(),
            });
        }
        self.session_failure.is_some()
    }

    fn stop_with_session_failure(&mut self, stage: JpdbPitchStage, message: String) {
        let failure = JpdbPitchFailure::SessionFailure { stage, message };
        trace_failure(&failure);
        self.session_failure = Some(failure);
    }

    fn record_processed_outcome(
        &mut self,
        outcome: JpdbPitchOutcome,
        stage: JpdbPitchStage,
        monitor_failed: bool,
    ) -> bool {
        if let JpdbPitchOutcome::Failed { error } = &outcome {
            trace_failure(error);
        }
        self.outcomes.push(outcome);
        self.stop_if_monitor_failed(stage, monitor_failed)
    }
}

/// Байты PNG и метаданные акцентуации, готовые к проверке.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbPitchAcquired {
    /// Неизменённые байты единственного нативного снимка CDP области PNG.
    pub bytes: Vec<u8>,
    pub metadata: PitchAccentDomainMetadata,
}

/// Положительное подтверждение идентичности страницы без секции акцентуации.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbPitchAbsenceEvidence {
    pub surface: String,
    pub reading: String,
    pub jpdb_vocabulary_id: u64,
    pub source_url: String,
    pub resolved_forms: Vec<PitchAccentResolvedForm>,
    /// Наблюдённые названия секций, нужные для подтверждения отсутствия графика.
    pub section_inventory: Vec<String>,
    pub base_page_contract_valid: bool,
    pub pitch_section_present: bool,
    pub pitch_marker_count: u32,
    pub browser: crate::browser_runtime::BrowserRuntimeProvenance,
}

/// Результат поиска, прошедший точную проверку написания и необязательного чтения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JpdbVocabularyCandidate {
    pub vocabulary_id: u64,
    pub surface_forms: Vec<String>,
    pub readings: Vec<String>,
    /// Связанные пары написания и чтения, реально наблюдённые в JPDB.
    pub resolved_forms: Vec<PitchAccentResolvedForm>,
    pub part_of_speech: Vec<String>,
    pub meanings: Vec<String>,
    /// Фактическая ссылка на словарную запись из результата поиска JPDB.
    pub detail_url: String,
}

/// Типизированная техническая ошибка или нарушение контракта источника для одного элемента.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum JpdbPitchFailure {
    InvalidQuery {
        stage: JpdbPitchStage,
        message: String,
    },
    BrowserSetup {
        stage: JpdbPitchStage,
        message: String,
    },
    BrowserConfiguration {
        stage: JpdbPitchStage,
        message: String,
    },
    Navigation {
        stage: JpdbPitchStage,
        message: String,
    },
    BrowserEvaluation {
        stage: JpdbPitchStage,
        message: String,
    },
    Timeout {
        stage: JpdbPitchStage,
        diagnostic: Option<String>,
    },
    Telemetry {
        stage: JpdbPitchStage,
        message: String,
    },
    PageContract {
        stage: JpdbPitchStage,
        message: String,
    },
    DetailIdentityMismatch {
        stage: JpdbPitchStage,
        expected_surface: String,
        expected_reading: Option<String>,
        vocabulary_id: Option<u64>,
        observed_surface_forms: Vec<String>,
        observed_readings: Vec<String>,
    },
    InvalidSelection {
        stage: JpdbPitchStage,
        message: String,
    },
    ExplicitSelectionMismatch {
        stage: JpdbPitchStage,
        vocabulary_id: u64,
        detail_url: String,
        message: String,
    },
    SessionFailure {
        stage: JpdbPitchStage,
        message: String,
    },
    DarkThemeUnverified {
        stage: JpdbPitchStage,
        message: String,
    },
    CaptureContract {
        stage: JpdbPitchStage,
        message: String,
    },
    Screenshot {
        stage: JpdbPitchStage,
        message: String,
    },
    InvalidPng {
        stage: JpdbPitchStage,
        message: String,
    },
}

/// Этап получения, указываемый в технических ошибках.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JpdbPitchStage {
    ConfigureBrowser,
    SearchNavigation,
    SearchReadiness,
    SearchResolution,
    DetailNavigation,
    DetailReadiness,
    DetailVerification,
    PitchInspection,
    Capture,
    PostCaptureVerification,
}

impl JpdbPitchStage {
    fn diagnostic_name(self) -> &'static str {
        match self {
            Self::ConfigureBrowser => "configure_browser",
            Self::SearchNavigation => "search_navigation",
            Self::SearchReadiness => "search_readiness",
            Self::SearchResolution => "search_resolution",
            Self::DetailNavigation => "detail_navigation",
            Self::DetailReadiness => "detail_readiness",
            Self::DetailVerification => "detail_verification",
            Self::PitchInspection => "pitch_inspection",
            Self::Capture => "capture",
            Self::PostCaptureVerification => "post_capture_verification",
        }
    }
}

fn request_diagnostic_timer(request: &JpdbPitchRequest) -> BrowserItemTimer {
    BrowserItemTimer::new(BrowserItemContext::without_generation(
        "jpdb",
        &request.query.surface,
        1,
    ))
}

/// Провайдер завершает собственный таймер; пакет продолжает заимствованный
/// таймер до итоговой проверки и надёжной контрольной точки в координаторе.
enum JpdbDiagnosticTimer<'a> {
    Owned(BrowserItemTimer),
    Continued(&'a BrowserItemTimer),
}

impl std::ops::Deref for JpdbDiagnosticTimer<'_> {
    type Target = BrowserItemTimer;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(item) => item,
            Self::Continued(item) => item,
        }
    }
}

/// Владеет только диагностикой: отмена Future сохраняет последний этап и свежий
/// ограниченный снимок, не принимая предметных решений и не записывая контрольную точку.
struct JpdbItemDiagnostics<'a> {
    telemetry: &'a CdpRuntimeMonitor,
    item: Option<JpdbDiagnosticTimer<'a>>,
    active: Option<BrowserStageTimer>,
    stage: JpdbPitchStage,
    epoch: Option<u64>,
}

impl<'a> JpdbItemDiagnostics<'a> {
    fn new(session: &'a BrowserSession, item: BrowserItemTimer) -> Self {
        item.set_browser_session(session.diagnostic_session_id());
        Self::from_monitor(session.telemetry(), item)
    }

    fn from_monitor(telemetry: &'a CdpRuntimeMonitor, item: BrowserItemTimer) -> Self {
        Self::with_timer(telemetry, JpdbDiagnosticTimer::Owned(item))
    }

    fn continued(
        telemetry: &'a CdpRuntimeMonitor,
        browser_session: u64,
        item: &'a BrowserItemTimer,
    ) -> Self {
        item.set_browser_session(browser_session);
        Self::with_timer(telemetry, JpdbDiagnosticTimer::Continued(item))
    }

    fn with_timer(telemetry: &'a CdpRuntimeMonitor, item: JpdbDiagnosticTimer<'a>) -> Self {
        let mut diagnostics = Self::unconfigured(telemetry, item);
        diagnostics.stage = JpdbPitchStage::ConfigureBrowser;
        diagnostics.active = Some(
            diagnostics
                .item
                .as_ref()
                .expect("диагностика элемента активна")
                .stage(JpdbPitchStage::ConfigureBrowser.diagnostic_name()),
        );
        diagnostics
    }

    fn unconfigured(telemetry: &'a CdpRuntimeMonitor, item: JpdbDiagnosticTimer<'a>) -> Self {
        // До первого перехода элемент ещё не владеет эпохой. Начальные данные
        // допустимы; данные предыдущего элемента не приписываем новому.
        item.record_runtime_snapshot(&telemetry.snapshot(0));
        Self {
            telemetry,
            item: Some(item),
            active: None,
            stage: JpdbPitchStage::SearchResolution,
            epoch: None,
        }
    }

    fn snapshot(&self) -> RuntimeSnapshot {
        self.telemetry.snapshot(self.epoch.unwrap_or(0))
    }

    fn set_epoch(&mut self, epoch: u64) {
        self.epoch = Some(epoch);
        self.item
            .as_ref()
            .expect("диагностика элемента активна")
            .record_runtime_snapshot(&self.snapshot());
    }

    fn record_current_snapshot(&self) {
        self.item
            .as_ref()
            .expect("диагностика элемента активна")
            .record_runtime_snapshot(&self.snapshot());
    }

    fn transition(&mut self, stage: JpdbPitchStage) {
        self.transition_named(stage, stage.diagnostic_name());
    }

    fn transition_named(&mut self, stage: JpdbPitchStage, name: &str) {
        if let Some(active) = self.active.take() {
            active.finish_success();
        }
        self.stage = stage;
        let item = self.item.as_ref().expect("диагностика элемента активна");
        item.record_runtime_snapshot(&self.snapshot());
        self.active = Some(item.stage(name));
    }

    fn finish_failure(mut self, failure: &JpdbPitchFailure) {
        let value = serde_json::to_value(failure).expect("ошибка JPDB сериализуется");
        let code = value["code"].as_str().unwrap_or("unknown");
        let retryable = crate::pitch_batch::is_retryable_failure(failure);
        let snapshot = self.snapshot();
        if let Some(active) = self.active.take() {
            active.finish_failure(code, retryable, Some(&snapshot));
        }
        if let Some(JpdbDiagnosticTimer::Owned(item)) = self.item.take() {
            item.finish_failure(code, retryable, Some(&snapshot));
        }
    }

    fn finish_outcome(mut self, outcome: &JpdbPitchOutcome) {
        if let JpdbPitchOutcome::Failed { error } = outcome {
            self.finish_failure(error);
            return;
        }
        let code = match outcome {
            JpdbPitchOutcome::Acquired { .. } => "acquired",
            JpdbPitchOutcome::NoPitchAccentOnSource { .. } => "no_pitch_accent_on_source",
            JpdbPitchOutcome::AmbiguousVocabulary { .. } => "ambiguous_vocabulary",
            JpdbPitchOutcome::VocabularyNotFound { .. } => "vocabulary_not_found",
            JpdbPitchOutcome::Failed { .. } => unreachable!(),
        };
        if let Some(active) = self.active.take() {
            active.finish_outcome(code);
        }
        if let Some(JpdbDiagnosticTimer::Owned(item)) = self.item.take() {
            item.finish_outcome(code);
        }
    }

    fn interrupt(mut self, reason: &str) {
        let snapshot = self.snapshot();
        if let Some(active) = self.active.take() {
            active.interrupt(reason, Some(&snapshot));
        }
        if let Some(JpdbDiagnosticTimer::Owned(item)) = self.item.take() {
            item.interrupt(reason, Some(&snapshot));
        }
    }
}

impl Drop for JpdbItemDiagnostics<'_> {
    fn drop(&mut self) {
        if self.item.is_some() {
            let snapshot = self.snapshot();
            if std::thread::panicking() {
                self.item
                    .as_ref()
                    .expect("диагностика элемента активна")
                    .set_stop_reason("worker_panic");
                if let Some(active) = self.active.take() {
                    active.finish_failure("worker_panic", false, Some(&snapshot));
                }
                if let Some(JpdbDiagnosticTimer::Owned(item)) = self.item.take() {
                    item.finish_failure("worker_panic", false, Some(&snapshot));
                }
            } else {
                if let Some(active) = self.active.take() {
                    active.interrupt("acquisition_interrupted", Some(&snapshot));
                }
                if let Some(JpdbDiagnosticTimer::Owned(item)) = self.item.take() {
                    item.interrupt("acquisition_interrupted", Some(&snapshot));
                }
            }
        }
    }
}

/// Провайдер для получения данных с публичных страниц словарных записей JPDB.
#[derive(Debug, Clone, Copy, Default)]
pub struct JpdbPitchProvider;

impl JpdbPitchProvider {
    /// Разрешает запрос и получает данные в новой изолированной сессии Chromium.
    pub async fn acquire(query: &JpdbPitchQuery) -> JpdbPitchOutcome {
        single_item_outcome(Self::acquire_many(std::slice::from_ref(query)).await)
    }

    /// Последовательно разрешает запросы в одной изолированной сессии браузера.
    /// Ошибка элемента не останавливает очередь; ошибка сессии оставляет только обработанный префикс.
    pub async fn acquire_many(queries: &[JpdbPitchQuery]) -> JpdbPitchAcquisitionReport {
        let requests = queries
            .iter()
            .cloned()
            .map(JpdbPitchRequest::new)
            .collect::<Vec<_>>();
        Self::acquire_requests(&requests).await
    }

    /// Последовательно разрешает запросы; явный выбор сверяется с новым поиском.
    pub async fn acquire_requests(requests: &[JpdbPitchRequest]) -> JpdbPitchAcquisitionReport {
        if requests.is_empty() {
            return JpdbPitchAcquisitionReport::default();
        }

        Self::acquire_requests_owned(requests, None, false).await
    }

    /// Все временные профили браузера находятся внутри workspace проверочного запуска.
    pub async fn acquire_requests_in_workspace(
        requests: &[JpdbPitchRequest],
        workspace: &Path,
    ) -> JpdbPitchAcquisitionReport {
        if requests.is_empty() {
            return JpdbPitchAcquisitionReport::default();
        }
        Self::acquire_requests_owned(requests, Some(workspace), true).await
    }

    async fn acquire_requests_owned(
        requests: &[JpdbPitchRequest],
        workspace: Option<&Path>,
        keep_late_interrupt_handler: bool,
    ) -> JpdbPitchAcquisitionReport {
        let mut interrupt = JpdbInterrupt::listen(keep_late_interrupt_handler);
        let runtime = pitch_browser_runtime_config();
        let item = request_diagnostic_timer(&requests[0]);
        let configuration = item.stage("browser_launch");
        // Не отменяем запуск по SIGINT: полученная Session явно закрывается владельцем.
        let launched = match workspace {
            Some(workspace) => BrowserSession::launch_in_workspace(runtime, workspace).await,
            None => BrowserSession::launch(runtime).await,
        };
        let session = match launched {
            Ok(session) => session,
            Err(message) => {
                let failure = JpdbPitchFailure::BrowserSetup {
                    stage: JpdbPitchStage::ConfigureBrowser,
                    message: message.to_string(),
                };
                configuration.finish_failure("browser_setup", false, None);
                item.finish_failure("browser_setup", false, None);
                let mut report = JpdbPitchAcquisitionReport::from_session_failure(failure);
                record_pending_interrupt(
                    &mut interrupt,
                    &mut report,
                    JpdbPitchStage::ConfigureBrowser,
                );
                return report;
            }
        };
        item.set_browser_session(session.diagnostic_session_id());
        item.record_runtime_snapshot(&session.telemetry().current_snapshot());
        configuration.finish_success();
        let diagnostics = JpdbItemDiagnostics::new(&session, item);
        let mut report = match wait_jpdb_operation(configure_page(session.page()), &mut interrupt)
            .await
        {
            Ok(Ok(())) => {
                process_requests_with_interrupt(
                    &session,
                    requests,
                    Some(&mut interrupt),
                    Some(diagnostics),
                )
                .await
            }
            Ok(Err(error)) => {
                diagnostics.finish_failure(&error);
                JpdbPitchAcquisitionReport::from_session_failure(error)
            }
            Err(message) => {
                diagnostics.interrupt(if interrupt.signal_observed {
                    "acquisition_interrupted"
                } else {
                    "ctrl_c_listener_failed"
                });
                JpdbPitchAcquisitionReport::from_session_failure(JpdbPitchFailure::SessionFailure {
                    stage: JpdbPitchStage::ConfigureBrowser,
                    message,
                })
            }
        };
        if let Err(message) = session.close().await {
            match &mut report.session_failure {
                Some(
                    JpdbPitchFailure::SessionFailure {
                        message: original, ..
                    }
                    | JpdbPitchFailure::BrowserSetup {
                        message: original, ..
                    }
                    | JpdbPitchFailure::BrowserConfiguration {
                        message: original, ..
                    },
                ) => {
                    original.push_str("; ");
                    original.push_str(&message);
                    trace_failure(report.session_failure.as_ref().expect("ошибка сохранена"));
                }
                Some(_) => {
                    // Предыдущая типизированная ошибка остаётся в отчёте; результат очистки
                    // отдельно записан BrowserSession в структурированный диагностический лог.
                }
                None => report.stop_with_session_failure(JpdbPitchStage::ConfigureBrowser, message),
            }
        }
        record_pending_interrupt(
            &mut interrupt,
            &mut report,
            JpdbPitchStage::ConfigureBrowser,
        );
        report
    }

    /// Использует переданную `BrowserSession`, но для каждого элемента запускает новый поиск.
    /// Провайдер задаёт для страницы масштаб 3× и эмуляцию тёмной цветовой схемы.
    pub async fn acquire_many_in_session(
        session: &BrowserSession,
        queries: &[JpdbPitchQuery],
    ) -> JpdbPitchAcquisitionReport {
        let requests = queries
            .iter()
            .cloned()
            .map(JpdbPitchRequest::new)
            .collect::<Vec<_>>();
        Self::acquire_requests_in_session(session, &requests).await
    }

    /// Обрабатывает запрос и явный выбор каждого элемента в переданной сессии браузера.
    pub async fn acquire_requests_in_session(
        session: &BrowserSession,
        requests: &[JpdbPitchRequest],
    ) -> JpdbPitchAcquisitionReport {
        if requests.is_empty() {
            return JpdbPitchAcquisitionReport::default();
        }
        let diagnostics = JpdbItemDiagnostics::new(session, request_diagnostic_timer(&requests[0]));
        Self::acquire_with_diagnostics(session, requests, diagnostics).await
    }

    /// Продолжает общий таймер исполнителя, включая уже измеренный запуск браузера.
    pub(crate) async fn acquire_requests_in_session_diagnostic(
        session: &BrowserSession,
        requests: &[JpdbPitchRequest],
        item: &BrowserItemTimer,
    ) -> JpdbPitchAcquisitionReport {
        if requests.is_empty() {
            return JpdbPitchAcquisitionReport::default();
        }
        let diagnostics = JpdbItemDiagnostics::continued(
            session.telemetry(),
            session.diagnostic_session_id(),
            item,
        );
        Self::acquire_with_diagnostics(session, requests, diagnostics).await
    }

    async fn acquire_with_diagnostics(
        session: &BrowserSession,
        requests: &[JpdbPitchRequest],
        diagnostics: JpdbItemDiagnostics<'_>,
    ) -> JpdbPitchAcquisitionReport {
        if let Err(error) = configure_page(session.page()).await {
            diagnostics.finish_failure(&error);
            return JpdbPitchAcquisitionReport::from_session_failure(error);
        }
        process_requests_with_interrupt(session, requests, None, Some(diagnostics)).await
    }
}

struct JpdbInterrupt {
    first_signal: tokio::sync::oneshot::Receiver<std::io::Result<()>>,
    listener: tokio::task::JoinHandle<()>,
    signal_observed: bool,
    keep_late_interrupt_handler: bool,
}

impl JpdbInterrupt {
    fn listen(keep_late_interrupt_handler: bool) -> Self {
        let (sender, first_signal) = tokio::sync::oneshot::channel();
        let listener = tokio::spawn(async move {
            let mut sender = Some(sender);
            loop {
                match tokio::signal::ctrl_c().await {
                    Ok(()) => {
                        if let Some(sender) = sender.take() {
                            if sender.send(Ok(())).is_err() && keep_late_interrupt_handler {
                                std::process::exit(130);
                            }
                        } else if keep_late_interrupt_handler {
                            std::process::exit(130);
                        } else {
                            return;
                        }
                    }
                    Err(error) => {
                        if let Some(sender) = sender.take() {
                            let _ = sender.send(Err(error));
                        }
                        return;
                    }
                }
            }
        });
        Self {
            listener,
            first_signal,
            signal_observed: false,
            keep_late_interrupt_handler,
        }
    }

    #[cfg(test)]
    fn injected(listener: impl Future<Output = std::io::Result<()>> + Send + 'static) -> Self {
        let (sender, first_signal) = tokio::sync::oneshot::channel();
        Self {
            listener: tokio::spawn(async move {
                let _ = sender.send(listener.await);
            }),
            first_signal,
            signal_observed: false,
            keep_late_interrupt_handler: false,
        }
    }

    fn observe_pending_signal(&mut self) -> bool {
        if self.signal_observed {
            return false;
        }
        if matches!(self.first_signal.try_recv(), Ok(Ok(()))) {
            self.signal_observed = true;
            true
        } else {
            false
        }
    }
}

impl Drop for JpdbInterrupt {
    fn drop(&mut self) {
        if self.keep_late_interrupt_handler {
            // tokio::signal::ctrl_c заменяет стандартный обработчик процесса.
            // Сохраняем зарегистрированный обработчик до завершения публикации ZIP.
            if !self.signal_observed {
                self.first_signal.close();
                if matches!(self.first_signal.try_recv(), Ok(Ok(()))) {
                    std::process::exit(130);
                }
            }
        } else {
            self.listener.abort();
        }
    }
}

fn record_pending_interrupt(
    interrupt: &mut JpdbInterrupt,
    report: &mut JpdbPitchAcquisitionReport,
    stage: JpdbPitchStage,
) {
    if !interrupt.observe_pending_signal() {
        return;
    }

    let previous_failure = report.session_failure.take().map(|failure| {
        let value = serde_json::to_value(&failure).expect("ошибка JPDB сериализуется");
        let code = value["code"].as_str().unwrap_or("unknown");
        let failure_stage = value["stage"].as_str().unwrap_or("unknown");
        let message = value
            .get("message")
            .or_else(|| value.get("diagnostic"))
            .and_then(serde_json::Value::as_str)
            .map(safe_message)
            .unwrap_or_else(|| "подробности отсутствуют".into());
        format!("{code} на этапе {failure_stage}: {message}")
    });
    let message = previous_failure.map_or_else(
        || "acquisition_interrupted: получен Ctrl+C".to_owned(),
        |previous| {
            format!("acquisition_interrupted: получен Ctrl+C; предшествующая ошибка: {previous}")
        },
    );
    report.stop_with_session_failure(stage, message);
}

async fn wait_jpdb_operation<T>(
    operation: impl Future<Output = T>,
    interrupt: &mut JpdbInterrupt,
) -> Result<T, String> {
    tokio::select! {
        biased;
        signal = &mut interrupt.first_signal => match signal {
            Ok(Ok(())) => {
                interrupt.signal_observed = true;
                Err("acquisition_interrupted: получен Ctrl+C".into())
            }
            Ok(Err(error)) => Err(format!("ctrl_c_listener_failed: {error}")),
            Err(error) => Err(format!("ctrl_c_listener_failed: {error}")),
        },
        result = operation => Ok(result),
    }
}

async fn process_requests_with_interrupt(
    session: &BrowserSession,
    requests: &[JpdbPitchRequest],
    mut interrupt: Option<&mut JpdbInterrupt>,
    mut first_diagnostics: Option<JpdbItemDiagnostics<'_>>,
) -> JpdbPitchAcquisitionReport {
    let mut report = JpdbPitchAcquisitionReport {
        outcomes: Vec::with_capacity(requests.len()),
        session_failure: None,
    };
    for request in requests {
        if report.stop_if_monitor_failed(
            JpdbPitchStage::SearchNavigation,
            session.telemetry().monitor_failed(),
        ) {
            if let Some(mut diagnostics) = first_diagnostics.take() {
                diagnostics.transition(JpdbPitchStage::SearchNavigation);
                diagnostics
                    .item
                    .as_ref()
                    .expect("диагностика элемента активна")
                    .set_stop_reason("session_failure");
                diagnostics
                    .finish_failure(report.session_failure.as_ref().expect("ошибка сохранена"));
            }
            break;
        }
        let mut diagnostics = first_diagnostics.take().unwrap_or_else(|| {
            let item = request_diagnostic_timer(request);
            item.set_browser_session(session.diagnostic_session_id());
            JpdbItemDiagnostics::unconfigured(session.telemetry(), JpdbDiagnosticTimer::Owned(item))
        });
        let item_span =
            tracing::info_span!("jpdb_item", surface = %safe_message(&request.query.surface));
        let operation = timeout(
            ITEM_TIMEOUT,
            acquire_one_in_session(
                session.page(),
                session.telemetry(),
                session.provenance(),
                request,
                &mut diagnostics,
            )
            .instrument(item_span.clone()),
        );
        let result = match interrupt.as_deref_mut() {
            Some(interrupt) => wait_jpdb_operation(operation, interrupt).await,
            None => Ok(operation.await),
        };
        let outcome = match result {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => item_timeout(diagnostics.stage),
            Err(message) => {
                let stage = diagnostics.stage;
                diagnostics.interrupt(if message.starts_with("acquisition_interrupted:") {
                    "acquisition_interrupted"
                } else {
                    "ctrl_c_listener_failed"
                });
                report.stop_with_session_failure(stage, message);
                break;
            }
        };
        let stage = diagnostics.stage;
        if session.telemetry().monitor_failed() {
            diagnostics
                .item
                .as_ref()
                .expect("диагностика элемента активна")
                .set_stop_reason("session_failure");
            diagnostics.record_current_snapshot();
        }
        diagnostics.finish_outcome(&outcome);
        if item_span.in_scope(|| {
            report.record_processed_outcome(outcome, stage, session.telemetry().monitor_failed())
        }) {
            break;
        }
    }
    report
}

fn single_item_outcome(mut report: JpdbPitchAcquisitionReport) -> JpdbPitchOutcome {
    match (report.outcomes.pop(), report.session_failure) {
        (Some(outcome), None) => outcome,
        (
            Some(outcome),
            Some(JpdbPitchFailure::SessionFailure {
                stage: JpdbPitchStage::ConfigureBrowser,
                ..
            }),
        ) => outcome,
        (_, Some(error)) => failed(error),
        (None, None) => failed(JpdbPitchFailure::BrowserSetup {
            stage: JpdbPitchStage::ConfigureBrowser,
            message: "Провайдер не вернул результат для элемента".into(),
        }),
    }
}

async fn configure_page(page: &Page) -> Result<(), JpdbPitchFailure> {
    page.emulate_locale(SetLocaleOverrideParams::builder().locale("en_US").build())
        .await
        .map_err(|error| JpdbPitchFailure::BrowserConfiguration {
            stage: JpdbPitchStage::ConfigureBrowser,
            message: format!("ошибка CDP Emulation.setLocaleOverride: {error}"),
        })?;
    page.execute(SetDeviceMetricsOverrideParams::new(
        CAPTURE_VIEWPORT_WIDTH,
        CAPTURE_VIEWPORT_HEIGHT,
        CAPTURE_DEVICE_SCALE_FACTOR,
        false,
    ))
    .await
    .map_err(|error| JpdbPitchFailure::BrowserConfiguration {
        stage: JpdbPitchStage::ConfigureBrowser,
        message: format!("ошибка CDP Emulation.setDeviceMetricsOverride: {error}"),
    })?;
    page.execute(
        SetEmulatedMediaParams::builder()
            .feature(MediaFeature::new("prefers-color-scheme", "dark"))
            .build(),
    )
    .await
    .map_err(|error| JpdbPitchFailure::BrowserConfiguration {
        stage: JpdbPitchStage::ConfigureBrowser,
        message: format!("ошибка CDP Emulation.setEmulatedMedia: {error}"),
    })?;
    page.execute(ResetPageScaleFactorParams::default())
        .await
        .map_err(|error| JpdbPitchFailure::BrowserConfiguration {
            stage: JpdbPitchStage::ConfigureBrowser,
            message: format!("ошибка CDP Emulation.resetPageScaleFactor: {error}"),
        })?;
    Ok(())
}

async fn acquire_one_in_session(
    page: &Page,
    telemetry: &CdpRuntimeMonitor,
    browser: &crate::browser_runtime::BrowserRuntimeProvenance,
    request: &JpdbPitchRequest,
    diagnostics: &mut JpdbItemDiagnostics<'_>,
) -> JpdbPitchOutcome {
    let query = &request.query;
    diagnostics.transition_named(JpdbPitchStage::SearchResolution, "request_validation");
    if let Err(message) = query.validate() {
        return failed(JpdbPitchFailure::InvalidQuery {
            stage: JpdbPitchStage::SearchResolution,
            message,
        });
    }

    let selection_route = match request.selection.as_ref().map(JpdbPitchSelection::validate) {
        Some(Ok(route)) => Some(route),
        Some(Err(message)) => {
            return failed(JpdbPitchFailure::InvalidSelection {
                stage: JpdbPitchStage::SearchResolution,
                message,
            });
        }
        None => None,
    };
    let search_url = match build_search_url(query.surface.trim()) {
        Ok(url) => url,
        Err(message) => {
            return failed(JpdbPitchFailure::InvalidQuery {
                stage: JpdbPitchStage::SearchResolution,
                message,
            });
        }
    };
    diagnostics.transition(JpdbPitchStage::SearchNavigation);
    let search_epoch = telemetry.begin_epoch();
    diagnostics.set_epoch(search_epoch);
    if let Err(error) = navigate(page, &search_url, diagnostics.stage).await {
        return failed(error);
    }
    diagnostics.transition(JpdbPitchStage::SearchReadiness);
    if let Err(error) =
        wait_for_critical_readiness(page, telemetry, search_epoch, diagnostics.stage).await
    {
        return failed(error);
    }
    diagnostics.transition(JpdbPitchStage::SearchResolution);
    let page_url = match current_url(page, diagnostics.stage).await {
        Ok(url) => url,
        Err(error) => return failed(error),
    };
    let (snapshot, detail_epoch) = if is_search_url(&page_url) {
        let search_results = match wait_for_search_candidates(page, telemetry, search_epoch).await {
            Ok(results) => results,
            Err(error) => return failed(error),
        };
        let matching = match select_candidates(&search_results.candidates, query) {
            Ok(matching) => matching,
            Err(error) => return failed(error),
        };
        let candidate = if let (Some(selection), Some(selection_route)) =
            (request.selection.as_ref(), selection_route.as_ref())
        {
            match select_explicit_candidate(&matching, selection, selection_route) {
                Ok(candidate) => candidate,
                Err(error) => return failed(error),
            }
        } else {
            match matching.len() {
                0 => return vocabulary_not_found(query),
                1 => matching[0].clone(),
                _ => {
                    return JpdbPitchOutcome::AmbiguousVocabulary {
                        surface: query.surface.clone(),
                        reading: query.reading.clone(),
                        candidates: matching,
                    };
                }
            }
        };
        let Some(candidate_route) = parse_detail_route(&candidate.detail_url) else {
            return failed(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: "Результат JPDB содержит недопустимую ссылку на словарную запись".into(),
            });
        };
        if candidate_route.vocabulary_id != candidate.vocabulary_id
            || !route_matches_resolved_forms(&candidate_route, &candidate.resolved_forms)
        {
            return failed(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message:
                    "ID или пара написания и чтения в маршруте JPDB не подтверждены формами результата"
                        .into(),
            });
        }

        diagnostics.transition(JpdbPitchStage::DetailNavigation);
        let detail_epoch = telemetry.begin_epoch();
        diagnostics.set_epoch(detail_epoch);
        if let Err(error) = navigate(page, &candidate.detail_url, diagnostics.stage).await {
            return failed(error);
        }
        diagnostics.transition(JpdbPitchStage::DetailReadiness);
        if let Err(error) =
            wait_for_critical_readiness(page, telemetry, detail_epoch, diagnostics.stage).await
        {
            return failed(error);
        }
        diagnostics.transition(JpdbPitchStage::DetailVerification);
        let snapshot = match wait_for_detail_snapshot(page, telemetry, detail_epoch).await {
            Ok(snapshot) => snapshot,
            Err(error) => return failed(error),
        };
        if let Err(error) =
            validate_navigated_detail_identity(&candidate, &candidate_route, &snapshot, query)
        {
            return failed(error);
        }
        (snapshot, detail_epoch)
    } else if let Some(redirected_route) = parse_detail_route(&page_url) {
        diagnostics.transition(JpdbPitchStage::DetailReadiness);
        if let Err(error) =
            wait_for_critical_readiness(page, telemetry, search_epoch, diagnostics.stage).await
        {
            return failed(error);
        }
        diagnostics.transition(JpdbPitchStage::DetailVerification);
        let snapshot = match wait_for_detail_snapshot(page, telemetry, search_epoch).await {
            Ok(snapshot) => snapshot,
            Err(error) => return failed(error),
        };
        if let Err(error) = validate_redirected_detail_identity(
            &redirected_route,
            &snapshot,
            query,
            request.selection.as_ref(),
            selection_route.as_ref(),
        ) {
            return failed(error);
        }
        (snapshot, search_epoch)
    } else {
        return failed(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: format!(
                "После поиска JPDB открыл неожиданный маршрут: {}",
                safe_route(&page_url)
            ),
        });
    };
    match inspect_and_capture(
        InspectionContext {
            page,
            telemetry,
            browser,
            query,
            vocabulary_id: snapshot.vocabulary_id,
            network_epoch: detail_epoch,
        },
        snapshot,
        diagnostics,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => failed(error),
    }
}

async fn navigate(page: &Page, url: &str, stage: JpdbPitchStage) -> Result<(), JpdbPitchFailure> {
    tracing::debug!(stage = ?stage, route = %safe_route(url), "Переход на страницу JPDB");
    timeout(READINESS_TIMEOUT, page.goto(url.to_owned()))
        .await
        .map_err(|_| JpdbPitchFailure::Timeout {
            stage,
            diagnostic: Some("Истёк лимит перехода к странице JPDB".into()),
        })?
        .map_err(|error| navigation_failure(stage, error))?;
    Ok(())
}

fn navigation_failure(
    stage: JpdbPitchStage,
    error: chromiumoxide::error::CdpError,
) -> JpdbPitchFailure {
    match error {
        chromiumoxide::error::CdpError::Timeout => JpdbPitchFailure::Timeout {
            stage,
            diagnostic: Some("Истёк лимит перехода к странице JPDB".into()),
        },
        error => JpdbPitchFailure::Navigation {
            stage,
            message: format!("Не удалось перейти на страницу JPDB: {error}"),
        },
    }
}

fn browser_evaluation_failure(
    stage: JpdbPitchStage,
    action: &str,
    error: impl std::fmt::Display,
) -> JpdbPitchFailure {
    JpdbPitchFailure::BrowserEvaluation {
        stage,
        message: format!(
            "Сбой выполнения JavaScript через CDP при {action}: {}",
            safe_message(&error.to_string())
        ),
    }
}

async fn current_url(page: &Page, stage: JpdbPitchStage) -> Result<String, JpdbPitchFailure> {
    page.evaluate("() => location.href")
        .await
        .map_err(|error| browser_evaluation_failure(stage, "чтении текущего URL", error))?
        .into_value()
        .map_err(|error| browser_evaluation_failure(stage, "декодировании текущего URL", error))
}

async fn wait_for_critical_readiness(
    page: &Page,
    telemetry: &CdpRuntimeMonitor,
    epoch: u64,
    stage: JpdbPitchStage,
) -> Result<(), JpdbPitchFailure> {
    let deadline = Instant::now() + READINESS_TIMEOUT;
    loop {
        let url = current_url(page, stage).await?;
        if !is_jpdb_url(&url) {
            return Err(JpdbPitchFailure::PageContract {
                stage,
                message: format!("переход покинул домен jpdb.io: {}", safe_route(&url)),
            });
        }
        let snapshot = telemetry.snapshot(epoch);
        if let Some(message) = critical_telemetry_failure(&snapshot) {
            return Err(JpdbPitchFailure::Telemetry { stage, message });
        }
        let ready_state: String = page
            .evaluate("() => document.readyState")
            .await
            .map_err(|error| {
                browser_evaluation_failure(stage, "чтении готовности документа", error)
            })?
            .into_value()
            .map_err(|error| {
                browser_evaluation_failure(stage, "декодировании готовности документа", error)
            })?;
        tracing::trace!(
            stage = ?stage,
            route = %safe_route(&url),
            ready = matches!(ready_state.as_str(), "interactive" | "complete"),
            pending_request_count = snapshot.pending_requests.len(),
            "Проверка готовности страницы JPDB"
        );
        if matches!(ready_state.as_str(), "interactive" | "complete") {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(JpdbPitchFailure::Timeout {
                stage,
                diagnostic: Some(
                    "Свойство document.readyState не перешло в состояние interactive или complete"
                        .into(),
                ),
            });
        }
        sleep(READINESS_POLL_INTERVAL).await;
    }
}

async fn wait_for_critical_network_idle(
    telemetry: &CdpRuntimeMonitor,
    epoch: u64,
    stage: JpdbPitchStage,
) -> Result<(), JpdbPitchFailure> {
    let deadline = Instant::now() + READINESS_TIMEOUT;
    let mut consecutive_idle_observations = 0;
    loop {
        let snapshot = telemetry.snapshot(epoch);
        if let Some(message) = critical_telemetry_failure(&snapshot) {
            return Err(JpdbPitchFailure::Telemetry { stage, message });
        }
        let has_pending_source_request = snapshot.pending_requests.iter().any(|request| {
            is_critical_resource(
                &request.resource_type,
                Some(&request.url),
                request.is_top_level,
            )
        });
        tracing::trace!(
            stage = ?stage,
            pending_request_count = snapshot.pending_requests.len(),
            has_pending_source_request,
            consecutive_idle_observations,
            "Проверка завершения критических запросов JPDB"
        );
        if has_pending_source_request {
            consecutive_idle_observations = 0;
        } else {
            consecutive_idle_observations += 1;
            if consecutive_idle_observations >= 2 {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            return Err(JpdbPitchFailure::Timeout {
                stage,
                diagnostic: Some(
                    "JPDB не достиг состояния без незавершённых критических запросов".into(),
                ),
            });
        }
        sleep(READINESS_POLL_INTERVAL).await;
    }
}

fn critical_telemetry_failure(snapshot: &RuntimeSnapshot) -> Option<String> {
    if snapshot.monitor_failed
        || snapshot.javascript_exceptions > 0
        || !snapshot.network_failures.is_empty()
        || !snapshot.http_errors.is_empty()
    {
        tracing::debug!(
            monitor_failed = snapshot.monitor_failed,
            javascript_exception_count = snapshot.javascript_exceptions,
            network_failure_count = snapshot.network_failures.len(),
            http_error_count = snapshot.http_errors.len(),
            pending_request_count = snapshot.pending_requests.len(),
            "Телеметрия JPDB содержит ошибки"
        );
    }
    tracing::trace!(
        monitor_failed = snapshot.monitor_failed,
        javascript_exception_count = snapshot.javascript_exceptions,
        network_failure_count = snapshot.network_failures.len(),
        http_error_count = snapshot.http_errors.len(),
        pending_request_count = snapshot.pending_requests.len(),
        "Проверка критической телеметрии JPDB"
    );
    if snapshot.monitor_failed {
        return Some("Монитор среды CDP сообщил о потере или неполноте телеметрии".into());
    }
    if snapshot.javascript_exceptions > 0 {
        return Some(format!(
            "Страница вызвала исключений JavaScript: {}",
            snapshot.javascript_exceptions
        ));
    }
    if let Some(outcome) = snapshot.network_failures.iter().find(|outcome| {
        is_critical_resource(
            &outcome.resource_type,
            outcome.url.as_deref(),
            outcome.is_top_level,
        )
    }) {
        return Some(format!(
            "Критический запрос завершился ошибкой: {} ({})",
            safe_route(outcome.url.as_deref().unwrap_or("неизвестный URL")),
            safe_message(
                outcome
                    .failure_reason
                    .as_deref()
                    .unwrap_or("неизвестная сетевая ошибка")
            )
        ));
    }
    if let Some(outcome) = snapshot.http_errors.iter().find(|outcome| {
        is_critical_resource(
            &outcome.resource_type,
            outcome.url.as_deref(),
            outcome.is_top_level,
        )
    }) {
        return Some(format!(
            "Критический запрос вернул HTTP {}: {}",
            outcome.status_code.unwrap_or_default(),
            safe_route(outcome.url.as_deref().unwrap_or("неизвестный URL"))
        ));
    }
    None
}

fn is_critical_resource(
    resource_type: &chromiumoxide::cdp::browser_protocol::network::ResourceType,
    raw_url: Option<&str>,
    is_top_level: bool,
) -> bool {
    if !is_relevant_resource_type(resource_type) {
        return false;
    }
    if is_top_level {
        return true;
    }
    if resource_type == &chromiumoxide::cdp::browser_protocol::network::ResourceType::Document {
        return false;
    }
    let is_jpdb_origin = raw_url
        .and_then(|raw| Url::parse(raw).ok())
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host == JPDB_HOST || host.ends_with(".jpdb.io"));
    is_jpdb_origin
        && matches!(
            resource_type,
            chromiumoxide::cdp::browser_protocol::network::ResourceType::Stylesheet
                | chromiumoxide::cdp::browser_protocol::network::ResourceType::Script
                | chromiumoxide::cdp::browser_protocol::network::ResourceType::Font
        )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JpdbVocabularyForm {
    surface: String,
    reading: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetailSnapshot {
    url: String,
    vocabulary_id: u64,
    forms: Vec<JpdbVocabularyForm>,
    part_of_speech: Vec<String>,
    section_inventory: Vec<String>,
    base_page_contract_valid: bool,
    pitch_section_present: bool,
    pitch_label: Option<String>,
    pitch_marker_count: usize,
    graph_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVocabularyCandidate {
    detail_url: Option<String>,
    forms: Vec<JpdbVocabularyForm>,
    part_of_speech: Vec<String>,
    meanings: Vec<String>,
    /// Ссылки `Used in` строки: свидетельство ID и написания, но не маршрут карточки.
    #[serde(default)]
    used_in_urls: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSearchSnapshot {
    ready: bool,
    no_results_found: bool,
    result_count: usize,
    truncated: bool,
    candidates: Vec<RawVocabularyCandidate>,
    /// Все `link[rel=canonical]` документа поиска в порядке DOM.
    #[serde(default)]
    canonical_urls: Vec<String>,
    /// JPDB показал единственную запись в режиме подробностей (`.results.details`).
    #[serde(default)]
    details_view: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCandidate {
    public: JpdbVocabularyCandidate,
    forms: Vec<JpdbVocabularyForm>,
    part_of_speech: Vec<String>,
    meanings: Vec<String>,
}

async fn wait_for_detail_snapshot(
    page: &Page,
    telemetry: &CdpRuntimeMonitor,
    epoch: u64,
) -> Result<DetailSnapshot, JpdbPitchFailure> {
    let deadline = Instant::now() + READINESS_TIMEOUT;
    let mut last_diagnostic = None;
    loop {
        if let Some(message) = critical_telemetry_failure(&telemetry.snapshot(epoch)) {
            return Err(JpdbPitchFailure::Telemetry {
                stage: JpdbPitchStage::DetailReadiness,
                message,
            });
        }
        let url = current_url(page, JpdbPitchStage::DetailReadiness).await?;
        let Some(route) = parse_detail_route(&url) else {
            if Instant::now() >= deadline {
                return Err(JpdbPitchFailure::Timeout {
                    stage: JpdbPitchStage::DetailReadiness,
                    diagnostic: last_diagnostic,
                });
            }
            sleep(READINESS_POLL_INTERVAL).await;
            continue;
        };
        match read_detail_snapshot(page, route.vocabulary_id, JpdbPitchStage::DetailReadiness).await
        {
            Ok(snapshot) => return Ok(snapshot),
            Err(JpdbPitchFailure::Timeout { diagnostic, .. }) if Instant::now() < deadline => {
                last_diagnostic = diagnostic;
                sleep(READINESS_POLL_INTERVAL).await;
            }
            Err(JpdbPitchFailure::Timeout { diagnostic, .. }) => {
                return Err(JpdbPitchFailure::Timeout {
                    stage: JpdbPitchStage::DetailReadiness,
                    diagnostic: diagnostic.or(last_diagnostic),
                });
            }
            Err(error) => return Err(error),
        }
    }
}

async fn read_detail_snapshot(
    page: &Page,
    expected_vocabulary_id: u64,
    stage: JpdbPitchStage,
) -> Result<DetailSnapshot, JpdbPitchFailure> {
    let raw: Value = page
        .evaluate(DETAIL_SNAPSHOT_SCRIPT)
        .await
        .map_err(|error| {
            browser_evaluation_failure(
                stage,
                "чтении сведений страницы словарной записи JPDB",
                error,
            )
        })?
        .into_value()
        .map_err(|error| {
            browser_evaluation_failure(
                stage,
                "декодировании сведений страницы словарной записи JPDB",
                error,
            )
        })?;
    tracing::debug!(
        stage = ?stage,
        route = %safe_route(raw["url"].as_str().unwrap_or_default()),
        pending = raw["pending"].as_bool().unwrap_or(false),
        base_page_contract_valid = raw["base_page_contract_valid"].as_bool().unwrap_or(false),
        english_language = raw["search_language"].as_str() == Some("english"),
        form_count = raw["forms"].as_array().map_or(0, Vec::len),
        part_of_speech_count = raw["part_of_speech"].as_array().map_or(0, Vec::len),
        graph_count = raw["graph_count"].as_u64().unwrap_or(0),
        "Наблюдённые условия словарной страницы JPDB"
    );
    if raw["pending"].as_bool() == Some(true) {
        return Err(JpdbPitchFailure::Timeout {
            stage,
            diagnostic: raw["error"].as_str().map(str::to_owned),
        });
    }
    if let Some(message) = raw["error"].as_str() {
        return Err(JpdbPitchFailure::PageContract {
            stage,
            message: message.to_owned(),
        });
    }
    let url = raw["url"].as_str().unwrap_or_default().to_owned();
    let Some(route) = parse_detail_route(&url) else {
        return Err(JpdbPitchFailure::PageContract {
            stage,
            message: "страница не соответствует точному маршруту словарной записи JPDB".into(),
        });
    };
    if route.vocabulary_id != expected_vocabulary_id {
        return Err(JpdbPitchFailure::PageContract {
            stage,
            message: "Маршрут словарной записи JPDB изменился во время проверки DOM".into(),
        });
    }
    if raw["search_language"].as_str() != Some("english") {
        return Err(JpdbPitchFailure::PageContract {
            stage,
            message: "поле `#search-bar-lang` не содержит точное значение `english`".into(),
        });
    }
    let mut forms: Vec<JpdbVocabularyForm> =
        serde_json::from_value(raw["forms"].clone()).map_err(|error| {
            JpdbPitchFailure::PageContract {
                stage,
                message: format!("некорректные наблюдённые формы словарной записи: {error}"),
            }
        })?;
    forms.retain(|form| !form.surface.trim().is_empty() && !form.reading.trim().is_empty());
    if let Some(reading) = route.reading.as_deref() {
        let route_form = JpdbVocabularyForm {
            surface: route.surface.clone(),
            reading: reading.to_owned(),
        };
        if !forms.contains(&route_form) {
            forms.push(route_form);
        }
    }
    if !route_matches_forms(&route, &forms) {
        return Err(JpdbPitchFailure::DetailIdentityMismatch {
            stage,
            expected_surface: route.surface.clone(),
            expected_reading: route.reading.clone(),
            vocabulary_id: Some(route.vocabulary_id),
            observed_surface_forms: unique_forms(&forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&forms),
        });
    }
    let part_of_speech: Vec<String> = serde_json::from_value(raw["part_of_speech"].clone())
        .map_err(|error| JpdbPitchFailure::PageContract {
            stage,
            message: format!(
                "некорректные данные о частях речи страницы словарной записи: {error}"
            ),
        })?;
    let section_inventory: Vec<String> = serde_json::from_value(raw["section_inventory"].clone())
        .map_err(|error| JpdbPitchFailure::PageContract {
        stage,
        message: format!("некорректный список секций страницы словарной записи: {error}"),
    })?;
    let base_page_contract_valid = raw["base_page_contract_valid"].as_bool().unwrap_or(false);
    if forms.is_empty() || part_of_speech.is_empty() || !base_page_contract_valid {
        return Err(JpdbPitchFailure::Timeout {
            stage,
            diagnostic: Some(
                "DOM страницы словарной записи JPDB пока не содержит проверяемых форм, частей речи и блока значений".into(),
            ),
        });
    }
    let pitch_section_present = raw["pitch_section_present"].as_bool().unwrap_or(false);
    let pitch_label = raw["pitch_label"].as_str().map(str::to_owned);
    let pitch_marker_count = raw["pitch_marker_count"].as_u64().unwrap_or(0) as usize;
    let graph_count = raw["graph_count"].as_u64().unwrap_or(0) as usize;
    Ok(DetailSnapshot {
        url,
        vocabulary_id: route.vocabulary_id,
        forms,
        part_of_speech,
        section_inventory,
        base_page_contract_valid,
        pitch_section_present,
        pitch_label,
        pitch_marker_count,
        graph_count,
    })
}

async fn read_search_candidates(page: &Page) -> Result<RawSearchSnapshot, JpdbPitchFailure> {
    let raw: Value = page
        .evaluate(SEARCH_CANDIDATES_SCRIPT)
        .await
        .map_err(|error| {
            browser_evaluation_failure(
                JpdbPitchStage::SearchResolution,
                "чтении результатов поиска JPDB",
                error,
            )
        })?
        .into_value()
        .map_err(|error| {
            browser_evaluation_failure(
                JpdbPitchStage::SearchResolution,
                "декодировании результатов поиска JPDB",
                error,
            )
        })?;
    decode_search_snapshot(raw)
}

fn decode_search_snapshot(raw: Value) -> Result<RawSearchSnapshot, JpdbPitchFailure> {
    tracing::debug!(
        stage = ?JpdbPitchStage::SearchResolution,
        ready = raw["ready"].as_bool().unwrap_or(false),
        no_results_found = raw["no_results_found"].as_bool().unwrap_or(false),
        result_count = raw["result_count"].as_u64().unwrap_or(0),
        vocabulary_row_count = raw["vocabulary_row_count"].as_u64().unwrap_or(0),
        candidate_count = raw["candidate_count"]
            .as_u64()
            .or_else(|| raw["candidates"].as_array().map(|rows| rows.len() as u64))
            .unwrap_or(0),
        truncated = raw["truncated"].as_bool().unwrap_or(false),
        parser_error = raw["error"].is_string(),
        invalid_row_index = raw["invalid_row_index"].as_u64(),
        form_count = raw["form_count"].as_u64(),
        has_forms = raw["has_forms"].as_bool(),
        part_of_speech_count = raw["part_of_speech_count"].as_u64(),
        has_part_of_speech = raw["has_part_of_speech"].as_bool(),
        meaning_count = raw["meaning_count"].as_u64(),
        has_meanings = raw["has_meanings"].as_bool(),
        has_detail_url = raw["has_detail_url"].as_bool(),
        has_detail_route = raw["has_detail_route"].as_bool(),
        detail_route_parse_succeeded = raw["detail_route_parse_succeeded"].as_bool(),
        "Наблюдённые условия поиска JPDB"
    );
    if let Some(message) = raw["error"].as_str() {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: message.to_owned(),
        });
    }
    let mut raw = raw;
    if let Some(object) = raw.as_object_mut() {
        object.remove("vocabulary_row_count");
    }
    let snapshot: RawSearchSnapshot =
        serde_json::from_value(raw).map_err(|error| JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: format!("некорректный список результатов поиска JPDB: {error}"),
        })?;
    if !snapshot.ready {
        return Err(JpdbPitchFailure::Timeout {
            stage: JpdbPitchStage::SearchReadiness,
            diagnostic: Some("JPDB ещё не показал стабильный список результатов".into()),
        });
    }
    validate_search_inventory(&snapshot)?;
    let mut snapshot = snapshot;
    resolve_single_entry_canonical_route(&mut snapshot);
    Ok(snapshot)
}

/// Подставляет canonical-маршрут страницы в единственную строку без ссылки на карточку.
///
/// Одиночная запись JPDB без альтернативных написаний содержит в строке только
/// ссылку `Used in`, а маршрут самой карточки есть лишь в `link[rel=canonical]`.
/// Canonical принимается, только если все свидетельства одного снимка DOM
/// указывают на одну запись: режим подробностей с ровно одной строкой и одним
/// результатом, ровно один допустимый canonical-маршрут, каждая ссылка `Used in`
/// строки с тем же ID и написанием и наблюдённая в заголовке строки пара
/// написания и чтения маршрута. Иначе строка остаётся без маршрута, и
/// `select_candidates` отказывает, как прежде.
fn resolve_single_entry_canonical_route(snapshot: &mut RawSearchSnapshot) {
    let Some(route) = single_entry_canonical_route(snapshot) else {
        return;
    };
    tracing::debug!(
        stage = ?JpdbPitchStage::SearchResolution,
        route_source = "canonical",
        vocabulary_id = route.0,
        "Маршрут одиночной записи JPDB подтверждён canonical-ссылкой страницы"
    );
    snapshot.candidates[0].detail_url = Some(route.1);
}

/// Непустой текст только из хираганы и катаканы, включая `ー` и метки дакутэна;
/// те же диапазоны, что у `isKana` в скриптах страниц JPDB.
fn is_kana_text(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|character| matches!(character, '\u{3040}'..='\u{309F}' | '\u{30A0}'..='\u{30FF}'))
}

fn single_entry_canonical_route(snapshot: &RawSearchSnapshot) -> Option<(u64, String)> {
    let [row] = snapshot.candidates.as_slice() else {
        return None;
    };
    if !snapshot.details_view
        || snapshot.result_count != 1
        || snapshot.no_results_found
        || snapshot.truncated
        || row.detail_url.is_some()
    {
        return None;
    }
    let [canonical_url] = snapshot.canonical_urls.as_slice() else {
        return None;
    };
    let route = parse_detail_route(canonical_url)?;
    if row.used_in_urls.is_empty()
        || !row.used_in_urls.iter().all(|url| {
            parse_jpdb_used_in_route(url).is_ok_and(|used_in| {
                used_in.vocabulary_id == route.vocabulary_id && used_in.surface == route.surface
            })
        })
    {
        return None;
    }
    // Чтение маршрута должно быть наблюдено в строке, а не перенесено из canonical.
    // Маршрут без чтения JPDB даёт записи, написанной каной: тогда чтение — само написание.
    let expected_reading = match route.reading.as_deref() {
        Some(reading) => reading,
        None if is_kana_text(&route.surface) => route.surface.as_str(),
        None => return None,
    };
    if !row
        .forms
        .iter()
        .any(|form| form.surface == route.surface && form.reading == expected_reading)
    {
        return None;
    }
    Some((route.vocabulary_id, canonical_url.clone()))
}

async fn wait_for_search_candidates(
    page: &Page,
    telemetry: &CdpRuntimeMonitor,
    epoch: u64,
) -> Result<RawSearchSnapshot, JpdbPitchFailure> {
    let deadline = Instant::now() + READINESS_TIMEOUT;
    let mut last_diagnostic = None;
    loop {
        if let Some(message) = critical_telemetry_failure(&telemetry.snapshot(epoch)) {
            return Err(JpdbPitchFailure::Telemetry {
                stage: JpdbPitchStage::SearchReadiness,
                message,
            });
        }
        match read_search_candidates(page).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(JpdbPitchFailure::Timeout { diagnostic, .. }) if Instant::now() < deadline => {
                last_diagnostic = diagnostic;
                sleep(READINESS_POLL_INTERVAL).await
            }
            Err(JpdbPitchFailure::Timeout { diagnostic, .. }) => {
                return Err(JpdbPitchFailure::Timeout {
                    stage: JpdbPitchStage::SearchReadiness,
                    diagnostic: diagnostic.or(last_diagnostic),
                });
            }
            Err(error) => {
                if let JpdbPitchFailure::PageContract { stage, message } = &error {
                    let route = current_url(page, *stage)
                        .await
                        .map(|url| safe_route(&url))
                        .unwrap_or_else(|_| "[маршрут недоступен]".into());
                    tracing::error!(
                        stage = ?stage,
                        code = "page_contract",
                        route = %route,
                        message = %safe_message(message),
                        "Нарушение контракта списка результатов JPDB"
                    );
                }
                return Err(error);
            }
        }
    }
}

fn validate_search_inventory(snapshot: &RawSearchSnapshot) -> Result<(), JpdbPitchFailure> {
    tracing::debug!(
        stage = ?JpdbPitchStage::SearchResolution,
        ready = snapshot.ready,
        result_count = snapshot.result_count,
        candidate_count = snapshot.candidates.len(),
        truncated = snapshot.truncated,
        no_results_found = snapshot.no_results_found,
        "Проверка целостности списка результатов JPDB"
    );
    if !snapshot.ready {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "список результатов JPDB ещё не готов".into(),
        });
    }
    if snapshot.truncated {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "число результатов JPDB превысило ограничение извлечения провайдера".into(),
        });
    }
    if snapshot.candidates.len() > snapshot.result_count {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "число строк словаря JPDB превышает объявленное число результатов поиска"
                .into(),
        });
    }
    if snapshot.no_results_found && snapshot.result_count != 0 {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "JPDB сообщил об отсутствии результатов, хотя строки словаря присутствуют"
                .into(),
        });
    }
    if !snapshot.no_results_found && snapshot.result_count == 0 {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "DOM поиска JPDB пуст, но явного маркера отсутствия результатов нет".into(),
        });
    }
    if snapshot.no_results_found && !snapshot.candidates.is_empty() {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "маркер отсутствия результатов JPDB конфликтует со строками словаря".into(),
        });
    }
    Ok(())
}

fn select_candidates(
    raw_candidates: &[RawVocabularyCandidate],
    query: &JpdbPitchQuery,
) -> Result<Vec<JpdbVocabularyCandidate>, JpdbPitchFailure> {
    if raw_candidates.len() > MAX_SEARCH_RESULTS {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::SearchResolution,
            message: "JPDB вернул больше кандидатов, чем допускает ограничение провайдера".into(),
        });
    }
    let mut by_id: Vec<ResolvedCandidate> = Vec::new();
    for (row_index, raw) in raw_candidates.iter().enumerate() {
        tracing::debug!(
            stage = ?JpdbPitchStage::SearchResolution,
            row_index,
            form_count = raw.forms.len(),
            part_of_speech_count = raw.part_of_speech.len(),
            meaning_count = raw.meanings.len(),
            has_detail_url = raw.detail_url.is_some(),
            route_valid = raw.detail_url.as_deref().and_then(parse_detail_route).is_some(),
            "Проверка строки поиска JPDB"
        );
        if raw.part_of_speech.is_empty() || raw.forms.is_empty() || raw.meanings.is_empty() {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: "результат JPDB не содержит наблюдённые формы, часть речи или значения"
                    .into(),
            });
        }
        let Some(detail_url) = raw.detail_url.as_deref() else {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
            message: "в распознанной строке словаря нет фактической ссылки `href` на словарную запись".into(),
            });
        };
        let Some(route) = parse_detail_route(detail_url) else {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: "результат JPDB содержит недопустимую ссылку на словарную запись".into(),
            });
        };
        if raw
            .part_of_speech
            .iter()
            .any(|pos| pos.trim().eq_ignore_ascii_case("name"))
        {
            continue;
        }
        let mut forms = raw
            .forms
            .iter()
            .filter(|form| !form.surface.trim().is_empty() && !form.reading.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>();
        if let Some(reading) = route.reading.as_deref() {
            // Конкретная ссылка JPDB подтверждает только пару из сегментов своего пути;
            // это не даёт права переносить чтение маршрута на остальные написания.
            let route_form = JpdbVocabularyForm {
                surface: route.surface.clone(),
                reading: reading.to_owned(),
            };
            if !forms.contains(&route_form) {
                forms.push(route_form);
            }
        }
        if forms.is_empty() {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: "результат JPDB не содержит подтверждённой пары написания и чтения".into(),
            });
        }
        if !route_matches_forms(&route, &forms) || !forms_match_query(&forms, query) {
            continue;
        }
        let forms = sorted_unique_forms(&forms);
        let part_of_speech = sorted_unique_strings(&raw.part_of_speech);
        let meanings = sorted_unique_strings(&raw.meanings);
        let public = JpdbVocabularyCandidate {
            vocabulary_id: route.vocabulary_id,
            surface_forms: forms.iter().map(|form| form.surface.clone()).collect(),
            readings: forms.iter().map(|form| form.reading.clone()).collect(),
            resolved_forms: forms
                .iter()
                .map(|form| PitchAccentResolvedForm {
                    surface: form.surface.clone(),
                    reading: form.reading.clone(),
                })
                .collect(),
            part_of_speech: part_of_speech.clone(),
            meanings: meanings.clone(),
            detail_url: detail_url.to_owned(),
        };
        if let Some(existing) = by_id
            .iter_mut()
            .find(|candidate| candidate.public.vocabulary_id == route.vocabulary_id)
        {
            existing.forms = sorted_unique_forms(&merge_forms(&existing.forms, &forms));
            existing.part_of_speech = sorted_unique_strings(
                &existing
                    .part_of_speech
                    .iter()
                    .chain(part_of_speech.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            existing.meanings = sorted_unique_strings(
                &existing
                    .meanings
                    .iter()
                    .chain(meanings.iter())
                    .cloned()
                    .collect::<Vec<_>>(),
            );
            if detail_url < existing.public.detail_url.as_str() {
                existing.public.detail_url = detail_url.to_owned();
            }
            populate_public_candidate(existing);
        } else {
            by_id.push(ResolvedCandidate {
                public,
                forms,
                part_of_speech,
                meanings,
            });
        }
    }
    by_id.sort_by(|left, right| {
        left.public
            .vocabulary_id
            .cmp(&right.public.vocabulary_id)
            .then_with(|| left.public.detail_url.cmp(&right.public.detail_url))
    });
    Ok(by_id
        .into_iter()
        .map(|candidate| candidate.public)
        .collect())
}

fn forms_match_query(forms: &[JpdbVocabularyForm], query: &JpdbPitchQuery) -> bool {
    let surface = query.surface.trim();
    let reading = query.reading.as_deref().map(str::trim);
    forms.iter().any(|form| {
        form.surface == surface
            && reading.is_none_or(|reading| jpdb_readings_equivalent(&form.reading, reading))
    })
}

fn resolve_query_reading(forms: &[JpdbVocabularyForm], query: &JpdbPitchQuery) -> Option<String> {
    let readings = forms
        .iter()
        .find(|form| {
            form.surface == query.surface.trim()
                && query
                    .reading
                    .as_deref()
                    .map(str::trim)
                    .is_none_or(|reading| jpdb_readings_equivalent(&form.reading, reading))
                && !form.reading.trim().is_empty()
        })
        .map(|form| form.reading.clone());
    if query.reading.is_some() {
        return readings;
    }
    let matching_readings = forms
        .iter()
        .filter(|form| form.surface == query.surface.trim() && !form.reading.trim().is_empty())
        .map(|form| form.reading.clone())
        .collect::<std::collections::BTreeSet<_>>();
    (matching_readings.len() == 1)
        .then(|| matching_readings.into_iter().next())
        .flatten()
}

fn detail_matches_query(detail: &DetailSnapshot, query: &JpdbPitchQuery) -> bool {
    parse_detail_route(&detail.url).is_some_and(|route| {
        route.vocabulary_id == detail.vocabulary_id && route_matches_forms(&route, &detail.forms)
    }) && !detail
        .part_of_speech
        .iter()
        .any(|pos| pos.trim().eq_ignore_ascii_case("name"))
        && forms_match_query(&detail.forms, query)
}

/// Сверяет открытую карточку с кандидатом поиска, по маршруту которого был переход.
fn validate_navigated_detail_identity(
    candidate: &JpdbVocabularyCandidate,
    candidate_route: &crate::pitch_accent::JpdbVocabularyRoute,
    detail: &DetailSnapshot,
    query: &JpdbPitchQuery,
) -> Result<(), JpdbPitchFailure> {
    validate_detail_matches_route(candidate_route, candidate.vocabulary_id, detail, query)
}

/// Общая сверка открытой карточки с маршрутом и ожидаемым ID для обоих путей перехода.
fn validate_detail_matches_route(
    route: &crate::pitch_accent::JpdbVocabularyRoute,
    vocabulary_id: u64,
    detail: &DetailSnapshot,
    query: &JpdbPitchQuery,
) -> Result<(), JpdbPitchFailure> {
    if !same_route_identity(&parse_detail_route(&detail.url), route)
        || detail.vocabulary_id != vocabulary_id
        || !route_matches_forms(route, &unique_forms(&detail.forms))
        || !detail_matches_query(detail, query)
    {
        return Err(JpdbPitchFailure::DetailIdentityMismatch {
            stage: JpdbPitchStage::DetailVerification,
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: unique_forms(&detail.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&detail.forms),
        });
    }
    Ok(())
}

fn validate_redirected_detail_identity(
    route: &crate::pitch_accent::JpdbVocabularyRoute,
    detail: &DetailSnapshot,
    query: &JpdbPitchQuery,
    selection: Option<&JpdbPitchSelection>,
    selection_route: Option<&crate::pitch_accent::JpdbVocabularyRoute>,
) -> Result<(), JpdbPitchFailure> {
    if let (Some(selection), Some(selection_route)) = (selection, selection_route)
        && (selection.vocabulary_id != route.vocabulary_id
            || !same_route_identity(&Some(route.clone()), selection_route))
    {
        return Err(JpdbPitchFailure::ExplicitSelectionMismatch {
            stage: JpdbPitchStage::SearchResolution,
            vocabulary_id: selection.vocabulary_id,
            detail_url: selection.detail_url.clone(),
            message:
                "страница, открытая JPDB после поиска, не совпадает с явным выбором ID и маршрута"
                    .into(),
        });
    }
    validate_detail_matches_route(route, route.vocabulary_id, detail, query)
}

fn select_explicit_candidate(
    candidates: &[JpdbVocabularyCandidate],
    selection: &JpdbPitchSelection,
    selection_route: &crate::pitch_accent::JpdbVocabularyRoute,
) -> Result<JpdbVocabularyCandidate, JpdbPitchFailure> {
    candidates
        .iter()
        .find(|candidate| {
            candidate.vocabulary_id == selection.vocabulary_id
                && same_route_identity(&parse_detail_route(&candidate.detail_url), selection_route)
        })
        .cloned()
        .ok_or_else(|| JpdbPitchFailure::ExplicitSelectionMismatch {
            stage: JpdbPitchStage::SearchResolution,
            vocabulary_id: selection.vocabulary_id,
            detail_url: selection.detail_url.clone(),
            message: "ID выбора и маршрут JPDB не совпали ни с одним кандидатом нового поиска"
                .into(),
        })
}

fn same_route_identity(
    left: &Option<crate::pitch_accent::JpdbVocabularyRoute>,
    right: &crate::pitch_accent::JpdbVocabularyRoute,
) -> bool {
    left.as_ref().is_some_and(|left| {
        left.vocabulary_id == right.vocabulary_id
            && left.surface == right.surface
            && left.reading == right.reading
    })
}

fn route_matches_forms(
    route: &crate::pitch_accent::JpdbVocabularyRoute,
    forms: &[JpdbVocabularyForm],
) -> bool {
    forms.iter().any(|form| {
        form.surface == route.surface
            && route
                .reading
                .as_deref()
                .is_none_or(|reading| form.reading == reading)
    })
}

fn route_matches_resolved_forms(
    route: &crate::pitch_accent::JpdbVocabularyRoute,
    forms: &[PitchAccentResolvedForm],
) -> bool {
    forms.iter().any(|form| {
        form.surface == route.surface
            && route
                .reading
                .as_deref()
                .is_none_or(|reading| form.reading == reading)
    })
}

fn populate_public_candidate(candidate: &mut ResolvedCandidate) {
    candidate.public.surface_forms = sorted_unique_strings(
        &candidate
            .forms
            .iter()
            .map(|form| form.surface.clone())
            .collect::<Vec<_>>(),
    );
    candidate.public.readings = sorted_unique_strings(
        &candidate
            .forms
            .iter()
            .map(|form| form.reading.clone())
            .collect::<Vec<_>>(),
    );
    candidate.public.resolved_forms = candidate
        .forms
        .iter()
        .map(|form| PitchAccentResolvedForm {
            surface: form.surface.clone(),
            reading: form.reading.clone(),
        })
        .collect();
    candidate.public.part_of_speech = candidate.part_of_speech.clone();
    candidate.public.meanings = candidate.meanings.clone();
}

fn merge_forms(
    existing: &[JpdbVocabularyForm],
    additional: &[JpdbVocabularyForm],
) -> Vec<JpdbVocabularyForm> {
    let mut forms = existing.to_vec();
    for form in additional {
        if !forms.contains(form) {
            forms.push(form.clone());
        }
    }
    forms
}

fn unique_forms(forms: &[JpdbVocabularyForm]) -> Vec<JpdbVocabularyForm> {
    let mut unique = Vec::new();
    for form in forms {
        if !form.surface.is_empty()
            && !unique
                .iter()
                .any(|existing: &JpdbVocabularyForm| existing == form)
        {
            unique.push(form.clone());
        }
    }
    unique
}

fn unique_readings(forms: &[JpdbVocabularyForm]) -> Vec<String> {
    let mut readings = Vec::new();
    for form in unique_forms(forms) {
        if !form.reading.is_empty() && !readings.contains(&form.reading) {
            readings.push(form.reading);
        }
    }
    readings
}

fn sorted_unique_forms(forms: &[JpdbVocabularyForm]) -> Vec<JpdbVocabularyForm> {
    let mut forms = unique_forms(forms);
    forms.sort_by(|left, right| {
        left.surface
            .cmp(&right.surface)
            .then_with(|| left.reading.cmp(&right.reading))
    });
    forms
}

fn sorted_unique_strings(values: &[String]) -> Vec<String> {
    let mut values = values
        .iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    values.sort();
    values.dedup();
    values
}

struct InspectionContext<'a> {
    page: &'a Page,
    telemetry: &'a CdpRuntimeMonitor,
    browser: &'a crate::browser_runtime::BrowserRuntimeProvenance,
    query: &'a JpdbPitchQuery,
    vocabulary_id: u64,
    network_epoch: u64,
}

async fn inspect_and_capture(
    context: InspectionContext<'_>,
    mut detail: DetailSnapshot,
    diagnostics: &mut JpdbItemDiagnostics<'_>,
) -> Result<JpdbPitchOutcome, JpdbPitchFailure> {
    let InspectionContext {
        page,
        telemetry,
        browser,
        query,
        vocabulary_id,
        network_epoch: inspection_epoch,
    } = context;
    if !is_jpdb_url(&detail.url)
        || detail.vocabulary_id != vocabulary_id
        || !detail_matches_query(&detail, query)
    {
        return Err(JpdbPitchFailure::DetailIdentityMismatch {
            stage: diagnostics.stage,
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: unique_forms(&detail.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&detail.forms),
        });
    }
    let resolved = sorted_unique_forms(&detail.forms);
    let resolved_reading = resolve_query_reading(&resolved, query).ok_or_else(|| {
        JpdbPitchFailure::DetailIdentityMismatch {
            stage: diagnostics.stage,
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: resolved.iter().map(|form| form.surface.clone()).collect(),
            observed_readings: unique_readings(&resolved),
        }
    })?;
    diagnostics.transition(JpdbPitchStage::PitchInspection);
    wait_for_critical_readiness(page, telemetry, inspection_epoch, diagnostics.stage).await?;
    match classify_pitch_state(&detail)? {
        PitchDomState::Absent => {
            wait_for_critical_network_idle(telemetry, inspection_epoch, diagnostics.stage).await?;
            let first_absence_snapshot = detail.clone();
            detail =
                read_detail_snapshot(page, vocabulary_id, JpdbPitchStage::PitchInspection).await?;
            if detail != first_absence_snapshot {
                return Err(JpdbPitchFailure::PageContract {
                    stage: diagnostics.stage,
                    message: "список секций страницы без графика акцентуации изменился после ожидания сетевого покоя".into(),
                });
            }
            if let Some(message) = critical_telemetry_failure(&telemetry.snapshot(inspection_epoch))
            {
                return Err(JpdbPitchFailure::Telemetry {
                    stage: diagnostics.stage,
                    message,
                });
            }
            return Ok(JpdbPitchOutcome::NoPitchAccentOnSource {
                evidence: JpdbPitchAbsenceEvidence {
                    surface: query.surface.trim().to_owned(),
                    reading: resolved_reading,
                    jpdb_vocabulary_id: vocabulary_id,
                    source_url: detail.url,
                    resolved_forms: resolved
                        .iter()
                        .map(|form| PitchAccentResolvedForm {
                            surface: form.surface.clone(),
                            reading: form.reading.clone(),
                        })
                        .collect(),
                    section_inventory: detail.section_inventory.clone(),
                    base_page_contract_valid: detail.base_page_contract_valid,
                    pitch_section_present: detail.pitch_section_present,
                    pitch_marker_count: detail.pitch_marker_count as u32,
                    browser: browser.clone(),
                },
            });
        }
        PitchDomState::Present { graph_count } if graph_count == detail.graph_count => {}
        PitchDomState::Present { .. } => {
            return Err(JpdbPitchFailure::PageContract {
                stage: diagnostics.stage,
                message: "число распознанных графиков акцентуации изменилось при классификации"
                    .into(),
            });
        }
    }
    if detail.graph_count > PITCH_ACCENT_MAX_VARIANT_COUNT {
        return Err(JpdbPitchFailure::PageContract {
            stage: diagnostics.stage,
            message: "число графиков акцентуации превышает ограничение провайдера на снимок".into(),
        });
    }

    diagnostics.transition(JpdbPitchStage::Capture);
    let inventory = read_variant_page(page, diagnostics.stage).await?;
    if inventory.variants.len() != detail.graph_count {
        return Err(JpdbPitchFailure::PageContract {
            stage: diagnostics.stage,
            message: "число распознанных вариантов графика не совпадает с числом графиков словарной записи".into(),
        });
    }
    let inventory_markup = inventory.markup_sha256();
    let selected_index =
        select_pitch_accent_variant(vocabulary_id, inventory_markup.iter().map(String::as_str))
            .ok_or_else(|| JpdbPitchFailure::CaptureContract {
                stage: diagnostics.stage,
                message: "у словарной записи нет вариантов графика для выбора".into(),
            })?;
    activate_dark_mode(page, diagnostics.stage, selected_index).await?;
    let first = capture_snapshot(page, diagnostics.stage, selected_index).await?;
    sleep(GRAPH_STABILITY_INTERVAL).await;
    let second = capture_snapshot(page, diagnostics.stage, selected_index).await?;
    if first != second || second.page.markup_sha256() != inventory_markup {
        return Err(JpdbPitchFailure::CaptureContract {
            stage: diagnostics.stage,
            message:
                "набор вариантов, DOM графиков, их геометрия или фактически отрисованная тема изменились до снимка"
                    .into(),
        });
    }
    detail = read_detail_snapshot(page, vocabulary_id, JpdbPitchStage::Capture).await?;
    if !detail_matches_query(&detail, query)
        || detail.graph_count != second.page.variants.len()
        || detail.pitch_label.as_deref() != Some("Pitch accent")
    {
        return Err(JpdbPitchFailure::DetailIdentityMismatch {
            stage: diagnostics.stage,
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: unique_forms(&detail.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&detail.forms),
        });
    }

    let clip = Viewport::builder()
        .x(second.capture_rect.x)
        .y(second.capture_rect.y)
        .width(second.capture_rect.width)
        .height(second.capture_rect.height)
        .scale(1.0)
        .build()
        .map_err(|message| JpdbPitchFailure::CaptureContract {
            stage: diagnostics.stage,
            message,
        })?;
    let params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .clip(clip)
        .capture_beyond_viewport(true)
        .build();
    let bytes = page
        .screenshot(params)
        .await
        .map_err(|error| JpdbPitchFailure::Screenshot {
            stage: JpdbPitchStage::Capture,
            message: format!("Не удалось получить снимок страницы средствами браузера: {error}"),
        })?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(JpdbPitchFailure::InvalidPng {
            stage: JpdbPitchStage::Capture,
            message: "Нативный снимок браузера вернул данные без сигнатуры PNG".into(),
        });
    }
    let image =
        image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).map_err(|error| {
            JpdbPitchFailure::InvalidPng {
                stage: JpdbPitchStage::Capture,
                message: format!("Нативный снимок браузера не декодируется как PNG: {error}"),
            }
        })?;
    let (pixel_width, pixel_height) = image.dimensions();
    if pixel_width == 0 || pixel_height == 0 {
        return Err(JpdbPitchFailure::InvalidPng {
            stage: JpdbPitchStage::Capture,
            message: "Нативный снимок браузера имеет нулевой размер".into(),
        });
    }
    diagnostics.transition(JpdbPitchStage::PostCaptureVerification);
    let post_capture = capture_snapshot(page, diagnostics.stage, selected_index).await?;
    if post_capture != second {
        return Err(JpdbPitchFailure::DarkThemeUnverified {
            stage: diagnostics.stage,
            message: "Фактически отрисованная тема, DOM графиков или геометрия снимка изменились во время снимка экрана".into(),
        });
    }
    let post_capture_detail =
        read_detail_snapshot(page, vocabulary_id, JpdbPitchStage::PostCaptureVerification).await?;
    if post_capture_detail.url != detail.url
        || post_capture_detail.vocabulary_id != vocabulary_id
        || !detail_matches_query(&post_capture_detail, query)
        || sorted_unique_forms(&post_capture_detail.forms) != resolved
    {
        return Err(JpdbPitchFailure::DetailIdentityMismatch {
            stage: diagnostics.stage,
            expected_surface: query.surface.clone(),
            expected_reading: query.reading.clone(),
            vocabulary_id: Some(vocabulary_id),
            observed_surface_forms: unique_forms(&post_capture_detail.forms)
                .into_iter()
                .map(|form| form.surface)
                .collect(),
            observed_readings: unique_readings(&post_capture_detail.forms),
        });
    }
    if let Some(message) = critical_telemetry_failure(&telemetry.snapshot(inspection_epoch)) {
        return Err(JpdbPitchFailure::Telemetry {
            stage: JpdbPitchStage::PostCaptureVerification,
            message,
        });
    }
    let resolved_forms: Vec<PitchAccentResolvedForm> = resolved
        .iter()
        .map(|form| PitchAccentResolvedForm {
            surface: form.surface.clone(),
            reading: form.reading.clone(),
        })
        .collect();
    let variants = second
        .page
        .variants
        .iter()
        .zip(&inventory_markup)
        .enumerate()
        .map(
            |(index, (variant, markup_sha256))| PitchAccentVariantEvidence {
                index: index as u32,
                selector: variant.selector.clone(),
                text: variant.text.clone(),
                markup_sha256: markup_sha256.clone(),
                selection_score: pitch_accent_variant_selection_score(vocabulary_id, markup_sha256),
                viewport_rect: variant.viewport_rect,
                document_rect: variant.document_rect,
                paint_rect: variant.paint_rect,
            },
        )
        .collect::<Vec<_>>();
    let selected = &variants[selected_index];
    let selected_paint_rect = selected.paint_rect;
    let render = PitchAccentRenderEvidence {
        kind: PitchAccentRenderKind::BrowserRegionScreenshot,
        selector: selected.selector.clone(),
        captured_graph_count: 1,
        coordinate_space: PitchAccentCoordinateSpace::Document,
        viewport_width: second.page.viewport_width,
        viewport_height: second.page.viewport_height,
        document_width: second.page.document_width,
        document_height: second.page.document_height,
        scroll_x: second.page.scroll_x,
        scroll_y: second.page.scroll_y,
        pixel_width,
        pixel_height,
        device_scale_factor: CAPTURE_DEVICE_SCALE_FACTOR,
        page_scale_factor: second.page.page_scale_factor,
        dark_theme: second.dark_theme,
        padding_css_px: PITCH_ACCENT_CAPTURE_PADDING_CSS_PX,
        capture_rect: second.capture_rect,
    };
    let metadata = PitchAccentDomainMetadata {
        surface: query.surface.trim().to_owned(),
        reading: resolved_reading,
        jpdb_vocabulary_id: vocabulary_id,
        evidence: PitchAccentEvidence {
            provider: PitchAccentProvider::Jpdb,
            source_url: detail.url,
            resolved_forms,
            variant_selection: PitchAccentVariantSelection {
                basis: PitchAccentVariantSelectionBasis::SeededHashWithoutSourcePriority,
                detected_variant_count: variants.len() as u32,
                selected_index: selected_index as u32,
                variants,
            },
            render,
            browser: browser.clone(),
        },
    };
    let evidence = &metadata.evidence;
    validate_evidence_capture_geometry(&evidence.render, &evidence.variant_selection).map_err(
        |failure| JpdbPitchFailure::CaptureContract {
            stage: diagnostics.stage,
            message: failure.into_message(),
        },
    )?;
    validate_evidence_capture_background(&evidence.render.dark_theme, &image).map_err(
        |failure| JpdbPitchFailure::DarkThemeUnverified {
            stage: diagnostics.stage,
            message: failure.into_message(),
        },
    )?;
    validate_evidence_capture_margins(&evidence.render, selected_paint_rect, &image).map_err(
        |failure| JpdbPitchFailure::CaptureContract {
            stage: diagnostics.stage,
            message: failure.into_message(),
        },
    )?;
    Ok(JpdbPitchOutcome::Acquired {
        asset: Box::new(JpdbPitchAcquired { bytes, metadata }),
    })
}

async fn activate_dark_mode(
    page: &Page,
    stage: JpdbPitchStage,
    selected_index: usize,
) -> Result<(), JpdbPitchFailure> {
    page.evaluate("() => { document.documentElement.classList.add('dark-mode'); return true; }")
        .await
        .map_err(|error| {
            browser_evaluation_failure(stage, "включении класса html.dark-mode", error)
        })?
        .into_value::<bool>()
        .map_err(|error| {
            browser_evaluation_failure(
                stage,
                "декодировании результата включения тёмной темы",
                error,
            )
        })?;
    let state = capture_snapshot(page, stage, selected_index).await?;
    if !state
        .dark_theme
        .document_element_classes
        .iter()
        .any(|class| class == "dark-mode")
        || state.dark_theme.prefers_color_scheme != "dark"
        || state.dark_theme.computed_color_scheme.trim().is_empty()
        || pitch_accent_relative_luminance(state.dark_theme.background_rgb) > 0.20
    {
        return Err(JpdbPitchFailure::DarkThemeUnverified {
            stage,
            message: format!(
                "Наблюдённый отрисованный фон не подтверждает тёмную тему: {:?}",
                state.dark_theme
            ),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureVariant {
    selector: String,
    text: String,
    markup: String,
    viewport_rect: PitchAccentCaptureRect,
    document_rect: PitchAccentCaptureRect,
    paint_rect: PitchAccentCaptureRect,
}

/// Варианты графика и метрики страницы, наблюдённые одним вызовом скрипта.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct VariantPage {
    variants: Vec<CaptureVariant>,
    viewport_width: u32,
    viewport_height: u32,
    document_width: u32,
    document_height: u32,
    scroll_x: f64,
    scroll_y: f64,
    page_scale_factor: f64,
    device_pixel_ratio: f64,
}

impl VariantPage {
    fn markup_sha256(&self) -> Vec<String> {
        self.variants
            .iter()
            .map(|variant| sha256_hex(variant.markup.as_bytes()))
            .collect()
    }
}

/// Страница и подготовленная к нативному снимку область выбранного варианта.
#[derive(Debug, Clone, PartialEq)]
struct CaptureSnapshot {
    page: VariantPage,
    selected_index: usize,
    capture_rect: PitchAccentCaptureRect,
    dark_theme: PitchAccentDarkThemeProof,
}

fn capture_script_call(selected_index: Option<usize>) -> String {
    let index = selected_index.map_or_else(|| "null".to_owned(), |index| index.to_string());
    format!("() => ({CAPTURE_SNAPSHOT_SCRIPT})({index}, {PITCH_ACCENT_CAPTURE_PADDING_CSS_PX:?})")
}

async fn evaluate_capture_script(
    page: &Page,
    stage: JpdbPitchStage,
    selected_index: Option<usize>,
) -> Result<Value, JpdbPitchFailure> {
    let raw: Value = page
        .evaluate(capture_script_call(selected_index))
        .await
        .map_err(|error| browser_evaluation_failure(stage, "получении геометрии графиков", error))?
        .into_value()
        .map_err(|error| {
            browser_evaluation_failure(stage, "декодировании геометрии графиков", error)
        })?;
    if let Some(message) = raw["error"].as_str() {
        if let Some(diagnostic) = message.strip_prefix("dark-theme: ") {
            return Err(JpdbPitchFailure::DarkThemeUnverified {
                stage,
                message: diagnostic.to_owned(),
            });
        }
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message: message.to_owned(),
        });
    }
    Ok(raw)
}

fn decode_variant_page(raw: Value, stage: JpdbPitchStage) -> Result<VariantPage, JpdbPitchFailure> {
    let page: VariantPage =
        serde_json::from_value(raw).map_err(|error| JpdbPitchFailure::CaptureContract {
            stage,
            message: format!("снимок DOM вернул неполные геометрические данные: {error}"),
        })?;
    tracing::debug!(
        stage = ?stage,
        variant_count = page.variants.len(),
        viewport_width = page.viewport_width,
        viewport_height = page.viewport_height,
        document_width = page.document_width,
        document_height = page.document_height,
        page_scale_factor = page.page_scale_factor,
        device_pixel_ratio = page.device_pixel_ratio,
        "Проверка вариантов графика JPDB"
    );
    validate_variant_page(&page, stage)?;
    Ok(page)
}

/// Читает все варианты графика выбранной словарной записи без подготовки снимка.
async fn read_variant_page(
    page: &Page,
    stage: JpdbPitchStage,
) -> Result<VariantPage, JpdbPitchFailure> {
    let raw = evaluate_capture_script(page, stage, None).await?;
    decode_variant_page(raw, stage)
}

async fn capture_snapshot(
    page: &Page,
    stage: JpdbPitchStage,
    selected_index: usize,
) -> Result<CaptureSnapshot, JpdbPitchFailure> {
    let mut raw = evaluate_capture_script(page, stage, Some(selected_index)).await?;
    let Some(object) = raw.as_object_mut() else {
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message: "снимок DOM вернул не объект".into(),
        });
    };
    let observed_index = object.remove("selected_index");
    let capture_rect = object.remove("capture_rect");
    let dark_theme = object.remove("dark_theme");
    let dark_theme: PitchAccentDarkThemeProof =
        serde_json::from_value(dark_theme.unwrap_or(Value::Null)).map_err(|error| {
            JpdbPitchFailure::DarkThemeUnverified {
                stage,
                message: format!(
                    "JPDB не предоставил проверяемые данные об отрисованном фоне: {error}"
                ),
            }
        })?;
    if dark_theme.background_selector.trim().is_empty()
        || pitch_accent_relative_luminance(dark_theme.background_rgb) > 0.20
    {
        return Err(JpdbPitchFailure::DarkThemeUnverified {
            stage,
            message: format!(
                "наблюдённый фон {} {:?} не доказывает тёмную тему",
                dark_theme.background_selector, dark_theme.background_rgb
            ),
        });
    }
    let capture_rect: PitchAccentCaptureRect =
        serde_json::from_value(capture_rect.unwrap_or(Value::Null)).map_err(|error| {
            JpdbPitchFailure::CaptureContract {
                stage,
                message: format!("снимок DOM не вернул область захвата: {error}"),
            }
        })?;
    if observed_index.and_then(|value| value.as_u64()) != Some(selected_index as u64) {
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message: "снимок DOM подготовлен не для выбранного варианта графика".into(),
        });
    }
    let snapshot = CaptureSnapshot {
        page: decode_variant_page(raw, stage)?,
        selected_index,
        capture_rect,
        dark_theme,
    };
    validate_capture_region(&snapshot, stage)?;
    Ok(snapshot)
}

fn validate_variant_page(
    page: &VariantPage,
    stage: JpdbPitchStage,
) -> Result<(), JpdbPitchFailure> {
    let document_width = f64::from(page.document_width);
    let document_height = f64::from(page.document_height);
    if page.viewport_width == 0
        || page.viewport_height == 0
        || page.document_width == 0
        || page.document_height == 0
        || page.document_width < page.viewport_width
        || page.document_height < page.viewport_height
        || !page.scroll_x.is_finite()
        || !page.scroll_y.is_finite()
        || page.scroll_x < 0.0
        || page.scroll_y < 0.0
        || page.variants.is_empty()
        || page.variants.len() > PITCH_ACCENT_MAX_VARIANT_COUNT
    {
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message:
                "размеры документа, области просмотра, прокрутки или число вариантов графика недопустимы"
                    .into(),
        });
    }
    if (page.page_scale_factor - 1.0).abs() > f64::EPSILON {
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message: format!(
                "масштаб страницы равен {}, ожидалось значение 1.0",
                page.page_scale_factor
            ),
        });
    }
    if (page.device_pixel_ratio - CAPTURE_DEVICE_SCALE_FACTOR).abs() > 0.001 {
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message: format!(
                "window.devicePixelRatio равен {}, ожидалось {}",
                page.device_pixel_ratio, CAPTURE_DEVICE_SCALE_FACTOR
            ),
        });
    }
    let tolerance = PITCH_ACCENT_CAPTURE_GEOMETRY_TOLERANCE_CSS_PX;
    for variant in &page.variants {
        let viewport = variant.viewport_rect;
        let document = variant.document_rect;
        let paint = variant.paint_rect;
        if variant.selector.trim().is_empty()
            || variant.text.trim().is_empty()
            || variant.markup.trim().is_empty()
            || !valid_rect(viewport, false)
            || viewport.x < -tolerance
            || viewport.y < -tolerance
            || viewport.x + viewport.width > f64::from(page.viewport_width) + tolerance
            || viewport.y + viewport.height > f64::from(page.viewport_height) + tolerance
            || !valid_rect(document, true)
            || document.x + document.width > document_width + tolerance
            || document.y + document.height > document_height + tolerance
            || (document.x - (viewport.x + page.scroll_x)).abs() > tolerance
            || (document.y - (viewport.y + page.scroll_y)).abs() > tolerance
            || (document.width - viewport.width).abs() > tolerance
            || (document.height - viewport.height).abs() > tolerance
            || !valid_rect(paint, false)
            || paint.x > document.x + tolerance
            || paint.y > document.y + tolerance
            || paint.x + paint.width < document.x + document.width - tolerance
            || paint.y + paint.height < document.y + document.height - tolerance
        {
            return Err(JpdbPitchFailure::CaptureContract {
                stage,
                message: "прямоугольники варианта графика не согласованы между областью просмотра, координатами документа и областью отрисовки".into(),
            });
        }
    }
    Ok(())
}

/// Проверяет подготовленную область до нативного снимка: она обязана совпасть с
/// отрисовкой выбранного варианта, расширенной на поле, остаться внутри документа
/// и не задеть отрисовку соседних вариантов.
fn validate_capture_region(
    snapshot: &CaptureSnapshot,
    stage: JpdbPitchStage,
) -> Result<(), JpdbPitchFailure> {
    let Some(selected) = snapshot.page.variants.get(snapshot.selected_index) else {
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message: "выбранный вариант графика отсутствует в снимке DOM".into(),
        });
    };
    let expected =
        pitch_accent_capture_rect_for(selected.paint_rect, PITCH_ACCENT_CAPTURE_PADDING_CSS_PX);
    let capture = snapshot.capture_rect;
    if capture != expected {
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message:
                "область захвата не совпадает с отрисовкой выбранного графика, расширенной на поле"
                    .into(),
        });
    }
    if !valid_rect(capture, true)
        || capture.x + capture.width > f64::from(snapshot.page.document_width)
        || capture.y + capture.height > f64::from(snapshot.page.document_height)
    {
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message: "область захвата с полями выходит за пределы документа".into(),
        });
    }
    if snapshot
        .page
        .variants
        .iter()
        .enumerate()
        .any(|(index, variant)| {
            index != snapshot.selected_index && rects_overlap(capture, variant.paint_rect)
        })
    {
        return Err(JpdbPitchFailure::CaptureContract {
            stage,
            message: "область захвата с полями задевает отрисовку соседнего варианта графика"
                .into(),
        });
    }
    Ok(())
}

fn rects_overlap(left: PitchAccentCaptureRect, right: PitchAccentCaptureRect) -> bool {
    left.x < right.x + right.width
        && right.x < left.x + left.width
        && left.y < right.y + right.height
        && right.y < left.y + left.height
}

fn valid_rect(rect: PitchAccentCaptureRect, require_nonnegative: bool) -> bool {
    [rect.x, rect.y, rect.width, rect.height]
        .into_iter()
        .all(f64::is_finite)
        && (!require_nonnegative || (rect.x >= 0.0 && rect.y >= 0.0))
        && rect.width > 0.0
        && rect.height > 0.0
}

fn build_search_url(surface: &str) -> Result<String, String> {
    let surface = surface.trim();
    if surface.is_empty() {
        return Err("Поле surface не должно быть пустым".into());
    }
    let mut url = Url::parse(&format!("{JPDB_ORIGIN}/search"))
        .map_err(|error| format!("неверный origin поиска JPDB: {error}"))?;
    url.query_pairs_mut()
        .append_pair("q", surface)
        .append_pair("lang", "english");
    Ok(url.to_string())
}

fn is_jpdb_url(raw_url: &str) -> bool {
    Url::parse(raw_url).is_ok_and(|url| {
        url.scheme() == "https"
            && url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(JPDB_HOST))
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
    })
}

fn is_search_url(raw_url: &str) -> bool {
    Url::parse(raw_url).is_ok_and(|url| is_jpdb_url(url.as_str()) && url.path() == "/search")
}

fn parse_detail_route(raw_url: &str) -> Option<crate::pitch_accent::JpdbVocabularyRoute> {
    parse_jpdb_vocabulary_route(raw_url).ok()
}

fn vocabulary_not_found(query: &JpdbPitchQuery) -> JpdbPitchOutcome {
    JpdbPitchOutcome::VocabularyNotFound {
        surface: query.surface.clone(),
        reading: query.reading.clone(),
    }
}

/// Записывает только тип и безопасный текст ошибки; исходные DOM/события CDP не экспортируются.
fn trace_failure(error: &JpdbPitchFailure) {
    let fields = serde_json::to_value(error).unwrap_or_default();
    tracing::warn!(
        stage = fields["stage"].as_str().unwrap_or("unknown"),
        code = fields["code"].as_str().unwrap_or("unknown"),
        message = %safe_message(fields["message"].as_str().unwrap_or_default()),
        diagnostic = %safe_message(fields["diagnostic"].as_str().unwrap_or_default()),
        "Получение графика JPDB завершилось технической ошибкой"
    );
}

fn failed(error: JpdbPitchFailure) -> JpdbPitchOutcome {
    JpdbPitchOutcome::Failed { error }
}

fn item_timeout(stage: JpdbPitchStage) -> JpdbPitchOutcome {
    failed(JpdbPitchFailure::Timeout {
        stage,
        diagnostic: Some("Истёк общий лимит времени для элемента".into()),
    })
}

const SEARCH_CANDIDATES_SCRIPT: &str = r#"() => {
  const lang = document.querySelector('#search-bar-lang');
  const results = document.querySelector('.results');
  if (!lang || !results) return { ready: false, no_results_found: false, result_count: 0, truncated: false, candidates: [] };
  if (lang.value !== 'english') return { error: 'Интерфейс JPDB должен быть на английском языке' };
  const compact = value => value.replace(/\s+/gu, '');
  const isKana = value => /^[\u3040-\u309f\u30a0-\u30ffー゙゚]+$/u.test(value);
  const parseDetailHref = href => {
    try {
      const url = new URL(href, location.href);
      const pieces = url.pathname.split('/').filter(Boolean);
      if (url.origin !== 'https://jpdb.io' || pieces.length < 3 || pieces.length > 4 || pieces[0] !== 'vocabulary') return null;
      if (!/^[1-9][0-9]*$/u.test(pieces[1])) return null;
      const surface = decodeURIComponent(pieces[2]);
      const reading = pieces.length === 4 ? decodeURIComponent(pieces[3]) : null;
      if (!surface.trim() || surface.toLowerCase() === 'used-in' || (reading !== null && (!reading.trim() || reading.toLowerCase() === 'used-in'))) return null;
      return { href: url.href, id: Number(pieces[1]), surface, reading };
    } catch (_) { return null; }
  };
  const rubyForm = root => {
    if (!root) return [];
    let surface = '';
    let reading = '';
    const baseText = node => {
      if (node.nodeType === Node.ELEMENT_NODE && node.tagName === 'RT') return '';
      if (node.nodeType === Node.TEXT_NODE) return node.textContent || '';
      if (node.nodeType === Node.ELEMENT_NODE && node.matches('.property-text')) return '';
      return [...node.childNodes].map(baseText).join('');
    };
    const visit = node => {
      if (node.nodeType === Node.TEXT_NODE) {
        const text = compact(node.textContent || '');
        surface += text;
        if (isKana(text)) reading += text;
        return;
      }
      if (!(node instanceof Element) || node.matches('.property-text')) return;
      if (node.tagName === 'RT') { reading += compact(node.textContent || ''); return; }
      if (node.tagName === 'RUBY') {
        let pendingBase = '';
        for (const child of node.childNodes) {
          if (child instanceof Element && child.tagName === 'RT') {
            const base = compact(pendingBase);
            const annotation = compact(child.textContent || '');
            surface += base;
            reading += annotation || (isKana(base) ? base : '');
            pendingBase = '';
          } else pendingBase += baseText(child);
        }
        const trailingBase = compact(pendingBase);
        surface += trailingBase;
        if (isKana(trailingBase)) reading += trailingBase;
        return;
      }
      for (const child of node.childNodes) visit(child);
    };
    for (const child of root.childNodes) visit(child);
    if (!surface) surface = compact(root.innerText || root.textContent || '');
    return surface ? [{ surface, reading }] : [];
  };
  const append = (forms, candidate) => {
    if (!candidate || !candidate.surface.trim() || !candidate.reading.trim()) return;
    if (!forms.some(form => form.surface === candidate.surface && form.reading === candidate.reading)) forms.push(candidate);
  };
  const hrefForRow = (row, id) => {
    const links = [...row.querySelectorAll('a.view-conjugations-link[href], .subsection-other-spellings .alt-spelling a.plain[href]')];
    return links.map(link => parseDetailHref(link.href)).find(route => route && route.id === id) || null;
  };
  const vocabularyRows = [...results.querySelectorAll('div.result.vocabulary')];
  const resultRows = [...results.querySelectorAll('div[id^="result-"]')];
  const resultCount = Math.max(resultRows.length, vocabularyRows.length);
  const noResultsFound = /\bNo results found\./i.test(results.innerText || '');
  if (noResultsFound && resultCount) return { error: 'JPDB одновременно сообщил об отсутствии результатов и показал строки поиска' };
  if (!noResultsFound && resultCount === 0) return { ready: false, no_results_found: false, result_count: 0, truncated: false, candidates: [] };
  const candidates = [];
  for (const row of vocabularyRows.slice(0, 128)) {
    const primary = row.querySelector('.subsection-headword .primary-spelling');
    const links = [...row.querySelectorAll('a.view-conjugations-link[href], .subsection-other-spellings .alt-spelling a.plain[href]')];
    const route = links.map(link => parseDetailHref(link.href)).find(candidate => candidate !== null);
    const forms = [];
    for (const form of rubyForm(primary)) append(forms, form);
    for (const alternate of row.querySelectorAll('.subsection-other-spellings .alt-spelling a.plain[href]')) {
      const alternateRoute = parseDetailHref(alternate.href);
      if (!alternateRoute || (route && alternateRoute.id !== route.id)) continue;
      const parsed = rubyForm(alternate)[0];
      if (!parsed || parsed.surface !== alternateRoute.surface) continue;
      if (parsed.reading && (!alternateRoute.reading || parsed.reading === alternateRoute.reading)) append(forms, parsed);
      else if (alternateRoute.reading) append(forms, { surface: alternateRoute.surface, reading: alternateRoute.reading });
    }
    const detailRoute = hrefForRow(row, route?.id);
    if (detailRoute?.reading) append(forms, { surface: detailRoute.surface, reading: detailRoute.reading });
    const partOfSpeech = [...row.querySelectorAll('.subsection-meanings .part-of-speech')]
      .map(node => (node.innerText || node.textContent || '').trim()).filter(Boolean);
    const meanings = [...row.querySelectorAll('.subsection-meanings .description')]
      .map(node => (node.innerText || node.textContent || '').replace(/\s+/gu, ' ').trim()).filter(Boolean);
    if (!forms.length || !partOfSpeech.length || !meanings.length) {
      return { error: 'Строка JPDB не содержит подтверждённые формы, часть речи и значения', result_count: resultCount, vocabulary_row_count: vocabularyRows.length, candidate_count: candidates.length, no_results_found: noResultsFound, truncated: resultCount > 128 || vocabularyRows.length > 128, invalid_row_index: candidates.length, form_count: forms.length, has_forms: forms.length > 0, part_of_speech_count: partOfSpeech.length, has_part_of_speech: partOfSpeech.length > 0, meaning_count: meanings.length, has_meanings: meanings.length > 0, has_detail_url: !!detailRoute, has_detail_route: !!detailRoute, detail_route_parse_succeeded: !!route };
    }
    // Ссылки `Used in` передаются как есть: допустимость и смысл проверяет Rust.
    const usedInUrls = [...row.querySelectorAll('a.view-conjugations-link[href]')]
      .map(link => link.href).filter(href => {
        try { return /\/used-in$/u.test(new URL(href, location.href).pathname); }
        catch (_) { return false; }
      });
    candidates.push({ detail_url: detailRoute ? detailRoute.href : null, forms, part_of_speech: partOfSpeech, meanings, used_in_urls: usedInUrls });
  }
  return {
    ready: true,
    no_results_found: noResultsFound,
    result_count: resultCount,
    vocabulary_row_count: vocabularyRows.length,
    truncated: resultCount > 128 || vocabularyRows.length > 128,
    candidates,
    canonical_urls: [...document.querySelectorAll('link[rel~="canonical"][href]')].map(link => link.href),
    details_view: results.classList.contains('details'),
  };
}"#;
const DETAIL_SNAPSHOT_SCRIPT: &str = r#"() => {
  const lang = document.querySelector('#search-bar-lang');
  const meanings = document.querySelector('.subsection-meanings');
  const primary = document.querySelector('.subsection-headword .primary-spelling');
  const compact = value => value.replace(/\s+/gu, '');
  const isKana = value => /^[\u3040-\u309f\u30a0-\u30ffー゙゚]+$/u.test(value);
  const parseDetailHref = href => {
    try {
      const url = new URL(href, location.href);
      const pieces = url.pathname.split('/').filter(Boolean);
      if (url.origin !== 'https://jpdb.io' || pieces.length < 3 || pieces.length > 4 || pieces[0] !== 'vocabulary') return null;
      if (!/^[1-9][0-9]*$/u.test(pieces[1])) return null;
      const surface = decodeURIComponent(pieces[2]);
      const reading = pieces.length === 4 ? decodeURIComponent(pieces[3]) : null;
      if (!surface.trim() || surface.toLowerCase() === 'used-in' || (reading !== null && (!reading.trim() || reading.toLowerCase() === 'used-in'))) return null;
      return { id: Number(pieces[1]), surface, reading };
    } catch (_) { return null; }
  };
  const rubyForm = root => {
    if (!root) return [];
    let surface = '';
    let reading = '';
    const baseText = node => {
      if (node.nodeType === Node.ELEMENT_NODE && node.tagName === 'RT') return '';
      if (node.nodeType === Node.TEXT_NODE) return node.textContent || '';
      if (node.nodeType === Node.ELEMENT_NODE && node.matches('.property-text')) return '';
      return [...node.childNodes].map(baseText).join('');
    };
    const visit = node => {
      if (node.nodeType === Node.TEXT_NODE) {
        const text = compact(node.textContent || '');
        surface += text;
        if (isKana(text)) reading += text;
        return;
      }
      if (!(node instanceof Element) || node.matches('.property-text')) return;
      if (node.tagName === 'RT') { reading += compact(node.textContent || ''); return; }
      if (node.tagName === 'RUBY') {
        let pendingBase = '';
        for (const child of node.childNodes) {
          if (child instanceof Element && child.tagName === 'RT') {
            const base = compact(pendingBase);
            const annotation = compact(child.textContent || '');
            surface += base;
            reading += annotation || (isKana(base) ? base : '');
            pendingBase = '';
          } else pendingBase += baseText(child);
        }
        const trailingBase = compact(pendingBase);
        surface += trailingBase;
        if (isKana(trailingBase)) reading += trailingBase;
        return;
      }
      for (const child of node.childNodes) visit(child);
    };
    for (const child of root.childNodes) visit(child);
    if (!surface) surface = compact(root.innerText || root.textContent || '');
    return surface ? [{ surface, reading }] : [];
  };
  const route = parseDetailHref(location.href);
  if (!lang || !primary || !meanings || !route) return { pending: true, error: 'DOM страницы словарной записи JPDB пока не готов к проверке' };
  const forms = [];
  const append = form => {
    if (!form || !form.surface.trim() || !form.reading.trim()) return;
    if (!forms.some(item => item.surface === form.surface && item.reading === form.reading)) forms.push(form);
  };
  for (const form of rubyForm(primary)) append(form);
  for (const alternate of document.querySelectorAll('.subsection-other-spellings .alt-spelling a.plain[href]')) {
    const alternateRoute = parseDetailHref(alternate.href);
    if (!alternateRoute || alternateRoute.id !== route.id) continue;
    const parsed = rubyForm(alternate)[0];
    if (!parsed || parsed.surface !== alternateRoute.surface) continue;
    if (parsed.reading && (!alternateRoute.reading || parsed.reading === alternateRoute.reading)) append(parsed);
    else if (alternateRoute.reading) append({ surface: alternateRoute.surface, reading: alternateRoute.reading });
  }
  const section = document.querySelector('.subsection-pitch-accent');
  const label = section?.querySelector('.subsection-label')?.innerText?.trim() || null;
  const sectionInventory = [...document.querySelectorAll('h6, .subsection-label')]
    .map(node => (node.innerText || node.textContent || '').trim()).filter(Boolean);
  const pitchMarkerCount = sectionInventory.filter(text => /pitch|accent/i.test(text)).length;
  let graphCount = 0;
  if (section) {
    const subsection = section.querySelector('.subsection');
    const column = subsection?.firstElementChild;
    if (column) {
      graphCount = [...column.children].reduce((count, row) => count + [...row.children].filter(node => {
        if (!(node instanceof HTMLElement) || !node.matches('div[style*="word-break: keep-all"]')) return false;
        return [...node.querySelectorAll(':scope > div')].some(piece => (piece.getAttribute('style') || '').includes('--pitch-'));
      }).length, 0);
    }
  }
  const partOfSpeech = [...meanings.querySelectorAll('.part-of-speech')]
    .map(node => (node.innerText || node.textContent || '').trim()).filter(Boolean);
  const meaningsLabel = meanings.querySelector('.subsection-label')?.innerText?.trim() || '';
  const hasMeaningDescription = Boolean(meanings.querySelector('.description'));
  return {
    url: location.href,
    search_language: lang.value,
    forms,
    part_of_speech: partOfSpeech,
    section_inventory: sectionInventory,
    base_page_contract_valid: meaningsLabel === 'Meanings' && partOfSpeech.length > 0 && hasMeaningDescription,
    pitch_section_present: Boolean(section),
    pitch_label: label,
    pitch_marker_count: pitchMarkerCount,
    graph_count: graphCount,
  };
}"#;
const CAPTURE_SNAPSHOT_SCRIPT: &str = r#"(selectedIndex, padding) => {
  const section = document.querySelector('.subsection-pitch-accent');
  const subsection = section?.querySelector('.subsection');
  const column = subsection?.firstElementChild;
  const lang = document.querySelector('#search-bar-lang');
  if (!section || !column || !lang || lang.value !== 'english') return { error: 'Секция графиков акцентуации, контейнер или английский интерфейс недоступны' };
  section.scrollIntoView({ block: 'center', inline: 'nearest', behavior: 'instant' });
  const scrollX = window.scrollX;
  const scrollY = window.scrollY;
  const nodes = [];
  const variants = [];
  for (const [rowIndex, row] of [...column.children].entries()) {
    let rowGraphs = 0;
    for (const [nodeIndex, node] of [...row.children].entries()) {
      if (node instanceof HTMLAnchorElement && node.matches('a.vocabulary-audio')) continue;
      if (!(node instanceof HTMLElement) || !node.matches('div[style*="word-break: keep-all"]')) {
        return { error: `Строка ${rowIndex + 1} секции графиков содержит нераспознанный элемент; сведения о варианте ударения не проверены` };
      }
      const pitchPieces = [...node.querySelectorAll(':scope > div')].filter(piece => (piece.getAttribute('style') || '').includes('--pitch-'));
      if (!pitchPieces.length) return { error: `Узел графика в строке ${rowIndex + 1} не содержит сегментов ударения` };
      rowGraphs += 1;
      const rect = node.getBoundingClientRect();
      let left = rect.left;
      let top = rect.top;
      let right = rect.right;
      let bottom = rect.bottom;
      for (const child of node.querySelectorAll('*')) {
        const childRect = child.getBoundingClientRect();
        if (childRect.width <= 0 && childRect.height <= 0) continue;
        left = Math.min(left, childRect.left);
        top = Math.min(top, childRect.top);
        right = Math.max(right, childRect.right);
        bottom = Math.max(bottom, childRect.bottom);
      }
      nodes.push(node);
      variants.push({
        selector: `.subsection-pitch-accent .subsection > div > div:nth-child(${rowIndex + 1}) > div:nth-child(${nodeIndex + 1})[style*="word-break: keep-all"]`,
        text: (node.textContent || '').replace(/\s+/gu, ''),
        markup: node.outerHTML,
        viewport_rect: { x: rect.left, y: rect.top, width: rect.width, height: rect.height },
        document_rect: { x: rect.left + scrollX, y: rect.top + scrollY, width: rect.width, height: rect.height },
        paint_rect: { x: left + scrollX, y: top + scrollY, width: right - left, height: bottom - top },
      });
    }
    if (rowGraphs !== 1) return { error: `Строка ${rowIndex + 1} секции графиков должна содержать ровно один график, найдено ${rowGraphs}` };
  }
  if (!variants.length) return { error: 'В секции графиков акцентуации не распознаны узлы графиков' };
  const documentWidth = Math.max(document.documentElement.scrollWidth, document.documentElement.clientWidth, document.body?.scrollWidth || 0);
  const documentHeight = Math.max(document.documentElement.scrollHeight, document.documentElement.clientHeight, document.body?.scrollHeight || 0);
  const pageState = {
    variants,
    viewport_width: window.innerWidth,
    viewport_height: window.innerHeight,
    document_width: documentWidth,
    document_height: documentHeight,
    scroll_x: scrollX,
    scroll_y: scrollY,
    page_scale_factor: window.visualViewport?.scale ?? NaN,
    device_pixel_ratio: window.devicePixelRatio,
  };
  if (selectedIndex === null) return pageState;
  const selected = variants[selectedIndex];
  const selectedNode = nodes[selectedIndex];
  if (!selected || !selectedNode) return { error: 'Выбранный вариант графика отсутствует на странице' };
  const paint = selected.paint_rect;
  const captureLeft = Math.floor(paint.x - padding);
  const captureTop = Math.floor(paint.y - padding);
  const captureRect = {
    x: captureLeft,
    y: captureTop,
    width: Math.ceil(paint.x + paint.width + padding) - captureLeft,
    height: Math.ceil(paint.y + paint.height + padding) - captureTop,
  };
  const clipViewport = {
    left: captureRect.x - scrollX,
    top: captureRect.y - scrollY,
    right: captureRect.x + captureRect.width - scrollX,
    bottom: captureRect.y + captureRect.height - scrollY,
  };
  const paintViewport = {
    left: paint.x - scrollX,
    top: paint.y - scrollY,
    right: paint.x + paint.width - scrollX,
    bottom: paint.y + paint.height - scrollY,
  };
  for (let node = selectedNode.parentElement; node instanceof HTMLElement; node = node.parentElement) {
    const style = getComputedStyle(node);
    if (style.overflowX === 'visible' && style.overflowY === 'visible' && style.clipPath === 'none') continue;
    const rect = node.getBoundingClientRect();
    if (rect.left > paintViewport.left || rect.top > paintViewport.top || rect.right < paintViewport.right || rect.bottom < paintViewport.bottom) {
      return { error: 'Предок графика обрезает его отрисовку свойствами overflow или clip-path' };
    }
  }
  const opaqueRgb = value => {
    const match = value.match(/^rgba?\(([^)]+)\)$/u);
    if (!match) return null;
    const channels = match[1].split(/[\s,\/]+/u).filter(Boolean).map(Number);
    if ((channels.length !== 3 && channels.length !== 4) || channels.some(channel => !Number.isFinite(channel))) return null;
    const alpha = channels.length === 4 ? channels[3] : 1;
    if (alpha < 0.999) return null;
    const rgb = channels.slice(0, 3).map(channel => Math.max(0, Math.min(255, Math.round(channel))));
    return rgb.length === 3 ? rgb : null;
  };
  const selectorFor = node => {
    if (node === document.documentElement) return 'html';
    if (node === document.body) return 'body';
    if (node.id) return `#${CSS.escape(node.id)}`;
    const classes = [...node.classList].slice(0, 3).map(name => `.${CSS.escape(name)}`).join('');
    return `${node.tagName.toLowerCase()}${classes}`;
  };
  let background = null;
  for (let node = section; node instanceof HTMLElement; node = node.parentElement) {
    const rect = node.getBoundingClientRect();
    const style = getComputedStyle(node);
    const rgb = style.backgroundImage === 'none' ? opaqueRgb(style.backgroundColor) : null;
    const covers = rect.left <= clipViewport.left + 0.5 && rect.top <= clipViewport.top + 0.5
      && rect.right >= clipViewport.right - 0.5 && rect.bottom >= clipViewport.bottom - 0.5;
    if (rgb && covers) {
      background = { selector: selectorFor(node), rgb };
      break;
    }
  }
  if (!background) return { error: 'dark-theme: Не найден непрозрачный фон предка, покрывающий всю область снимка с полями' };
  const root = document.documentElement;
  const rootStyle = getComputedStyle(root);
  return {
    ...pageState,
    selected_index: selectedIndex,
    capture_rect: captureRect,
    dark_theme: {
      document_element_classes: [...root.classList],
      prefers_color_scheme: matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light',
      computed_color_scheme: rootStyle.colorScheme,
      background_selector: background.selector,
      background_rgb: background.rgb,
    },
  };
}"#;
#[cfg(test)]
mod tests {
    use super::*;

    fn diagnostic_events(operation: impl FnOnce()) -> Vec<Value> {
        let workspace =
            crate::temp_workspace::TempWorkspace::create("jpdb-diagnostics-test").unwrap();
        let path = workspace.path().join("diagnostics.jsonl");
        let log = crate::diagnostics::RunLogGuard::new(
            std::fs::File::create(&path).unwrap(),
            crate::diagnostics::OutputMode::Json,
        );
        log.with_default(operation);
        log.finish().unwrap();
        let events = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        workspace.close().unwrap();
        events
    }

    fn diagnostic_monitor(epoch: u64) -> CdpRuntimeMonitor {
        use chromiumoxide::cdp::browser_protocol::network::ResourceType;
        CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
            pending_requests: vec![crate::browser_runtime::TrackedRequest {
                request_id: "secret-request-id".into(),
                resource_type: ResourceType::Image,
                url: "https://example.invalid/image?cookie=secret-query".into(),
                epoch,
                is_top_level: false,
            }],
            ..RuntimeSnapshot::default()
        })
    }

    #[test]
    fn main_document_http_error_is_critical_and_subframe_document_is_not() {
        use chromiumoxide::cdp::browser_protocol::network::ResourceType;

        let snapshot = |is_top_level| RuntimeSnapshot {
            epoch: 3,
            http_errors: vec![crate::browser_runtime::NetworkOutcome {
                resource_type: ResourceType::Document,
                url: Some("https://jpdb.io/search?q=x".into()),
                failure_reason: None,
                request_id: "document".into(),
                epoch: 3,
                status_code: Some(429),
                is_top_level,
            }],
            ..RuntimeSnapshot::default()
        };
        let failure = critical_telemetry_failure(&snapshot(true)).expect("главный документ");
        assert!(failure.contains("HTTP 429"));
        assert_eq!(critical_telemetry_failure(&snapshot(false)), None);
    }

    #[test]
    fn provider_only_request_timer_uses_unknown_generation() {
        let events = diagnostic_events(|| {
            request_diagnostic_timer(&JpdbPitchRequest::new(JpdbPitchQuery::new("雨", None)))
                .finish_success();
        });
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"].get("generation"), Some(&Value::Null));
        assert_eq!(events[0]["fields"]["identity"], "雨");
        assert_eq!(events[0]["fields"]["attempt"], 1);
    }

    #[test]
    fn next_request_does_not_emit_a_configuration_stage_without_configure_page() {
        let monitor = diagnostic_monitor(7);
        let events = diagnostic_events(|| {
            let item =
                request_diagnostic_timer(&JpdbPitchRequest::new(JpdbPitchQuery::new("雨", None)));
            let mut diagnostics =
                JpdbItemDiagnostics::unconfigured(&monitor, JpdbDiagnosticTimer::Owned(item));
            diagnostics.transition_named(JpdbPitchStage::SearchResolution, "request_validation");
            diagnostics.finish_outcome(&JpdbPitchOutcome::VocabularyNotFound {
                surface: "雨".into(),
                reading: None,
            });
        });
        assert!(
            !events
                .iter()
                .any(|event| event["fields"]["stage"] == "configure_browser")
        );
        assert_eq!(events[0]["fields"]["stage"], "request_validation");
    }

    #[test]
    fn cdp_navigation_timeout_keeps_its_type_and_other_errors_do_not_gain_retryability() {
        use chromiumoxide::error::CdpError;

        for stage in [
            JpdbPitchStage::SearchNavigation,
            JpdbPitchStage::DetailNavigation,
        ] {
            let failure = navigation_failure(stage, CdpError::Timeout);
            assert!(
                matches!(failure, JpdbPitchFailure::Timeout { stage: observed, .. } if observed == stage)
            );
            assert!(crate::pitch_batch::is_retryable_failure(&failure));
            for error in [
                CdpError::NoResponse,
                CdpError::NotFound,
                CdpError::msg("request timed out"),
                CdpError::msg("Request timed out"),
                CdpError::msg("произвольный сбой навигации"),
            ] {
                let failure = navigation_failure(stage, error);
                assert!(matches!(failure, JpdbPitchFailure::Navigation { .. }));
                assert!(!crate::pitch_batch::is_retryable_failure(&failure));
            }
        }
    }

    #[test]
    fn continued_provider_stages_keep_the_owner_timer_alive_through_checkpoint() {
        let monitor = diagnostic_monitor(7);
        let events = diagnostic_events(|| {
            let item = BrowserItemTimer::new(
                BrowserItemContext::new("jpdb", "雨", 3, 8)
                    .with_worker(2, 5)
                    .with_browser_session(41),
            );
            item.stage("browser_launch").finish_success();
            let mut diagnostics = JpdbItemDiagnostics::continued(&monitor, 41, &item);
            diagnostics.transition_named(JpdbPitchStage::SearchResolution, "request_validation");
            for stage in [
                JpdbPitchStage::SearchNavigation,
                JpdbPitchStage::SearchReadiness,
                JpdbPitchStage::SearchResolution,
                JpdbPitchStage::DetailNavigation,
                JpdbPitchStage::DetailReadiness,
                JpdbPitchStage::DetailVerification,
                JpdbPitchStage::PitchInspection,
                JpdbPitchStage::Capture,
                JpdbPitchStage::PostCaptureVerification,
            ] {
                diagnostics.transition(stage);
            }
            diagnostics.finish_outcome(&JpdbPitchOutcome::VocabularyNotFound {
                surface: "雨".into(),
                reading: None,
            });
            std::thread::sleep(std::time::Duration::from_millis(5));
            item.stage("checkpoint").finish_success();
            item.finish_outcome("vocabulary_not_found");
        });
        let diagnostic = events
            .iter()
            .map(|event| &event["fields"])
            .filter(|fields| fields["schema"] == "browser_acquisition_v1")
            .collect::<Vec<_>>();
        let names = diagnostic
            .iter()
            .map(|fields| fields["stage"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "browser_launch",
                "configure_browser",
                "request_validation",
                "search_navigation",
                "search_readiness",
                "search_resolution",
                "detail_navigation",
                "detail_readiness",
                "detail_verification",
                "pitch_inspection",
                "capture",
                "post_capture_verification",
                "checkpoint",
                "item"
            ]
        );
        assert_eq!(
            diagnostic
                .iter()
                .filter(|fields| fields["event"] == "browser_acquisition_item")
                .count(),
            1
        );
        for fields in &diagnostic {
            assert_eq!(fields["identity"], "雨");
            assert_eq!(fields["attempt"], 3);
            assert_eq!(fields["generation"], 8);
            assert_eq!(fields["worker"], 2);
            assert_eq!(fields["worker_session"], 5);
            assert_eq!(fields["browser_session"], 41);
        }
        let item = diagnostic.last().unwrap();
        assert!(item["item_duration_ms"].as_u64().unwrap() >= 5);
        let provider_end = diagnostic
            .iter()
            .find(|fields| fields["stage"] == "post_capture_verification")
            .unwrap();
        assert!(
            item["item_duration_ms"].as_u64().unwrap()
                > provider_end["item_duration_ms"].as_u64().unwrap()
        );
    }

    #[test]
    fn pitch_item_timeout_reports_exact_stage_and_scoped_runtime_evidence() {
        let monitor = diagnostic_monitor(7);
        let events = diagnostic_events(|| {
            let timer = BrowserItemTimer::new(
                BrowserItemContext::new("jpdb", "雨", 2, 3)
                    .with_worker(4, 5)
                    .with_browser_session(6),
            );
            let mut diagnostics = JpdbItemDiagnostics::from_monitor(&monitor, timer);
            diagnostics.transition(JpdbPitchStage::PitchInspection);
            diagnostics.set_epoch(7);
            diagnostics.finish_outcome(&item_timeout(JpdbPitchStage::PitchInspection));
        });
        let failures = events
            .iter()
            .filter(|event| event["fields"]["outcome"] == "failure")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0]["fields"]["stage"], "pitch_inspection");
        for event in failures {
            let fields = &event["fields"];
            assert_eq!(fields["failure_code"], "timeout");
            assert_eq!(fields["retryable"], true);
            assert_eq!(fields["attempt"], 2);
            assert_eq!(fields["generation"], 3);
            assert_eq!(fields["worker"], 4);
            assert_eq!(fields["worker_session"], 5);
            assert_eq!(fields["browser_session"], 6);
            assert!(
                fields["stage_duration_ms"].as_u64().unwrap()
                    <= fields["item_duration_ms"].as_u64().unwrap()
            );
            let snapshot: Value =
                serde_json::from_str(fields["runtime_snapshot"].as_str().unwrap()).unwrap();
            assert_eq!(snapshot["epoch"], 7);
            assert_eq!(snapshot["pending_request_count"], 1);
            assert_eq!(snapshot["pending_requests"][0]["in_current_epoch"], true);
            assert!(!fields.to_string().contains("secret-query"));
            assert!(!fields.to_string().contains("secret-request-id"));
        }
    }

    #[test]
    fn completed_pitch_item_links_monitor_failure_to_fresh_runtime_snapshot() {
        use chromiumoxide::cdp::browser_protocol::network::ResourceType;

        let monitor = CdpRuntimeMonitor::from_snapshot_for_test(RuntimeSnapshot {
            epoch: 7,
            pending_requests: vec![crate::browser_runtime::TrackedRequest {
                request_id: "secret-request-id".into(),
                resource_type: ResourceType::Image,
                url: "https://example.invalid/image?cookie=secret-query".into(),
                epoch: 7,
                is_top_level: false,
            }],
            monitor_failed: true,
            ..RuntimeSnapshot::default()
        });
        let events = diagnostic_events(|| {
            let mut diagnostics = JpdbItemDiagnostics::from_monitor(
                &monitor,
                BrowserItemTimer::new(
                    BrowserItemContext::new("jpdb", "雨", 1, 0).with_browser_session(6),
                ),
            );
            diagnostics.transition(JpdbPitchStage::PitchInspection);
            diagnostics.set_epoch(7);
            diagnostics
                .item
                .as_ref()
                .expect("диагностика элемента активна")
                .set_stop_reason("session_failure");
            diagnostics.record_current_snapshot();
            diagnostics.finish_outcome(&JpdbPitchOutcome::VocabularyNotFound {
                surface: "雨".into(),
                reading: None,
            });
        });

        let completed = events
            .iter()
            .filter(|event| event["fields"]["outcome"] == "vocabulary_not_found")
            .collect::<Vec<_>>();
        assert_eq!(completed.len(), 2);
        for event in completed {
            let fields = &event["fields"];
            assert_eq!(fields["outcome"], "vocabulary_not_found");
            assert_eq!(fields["stop_reason"], "session_failure");
            let snapshot: Value =
                serde_json::from_str(fields["runtime_snapshot"].as_str().unwrap()).unwrap();
            assert_eq!(snapshot["epoch"], 7);
            assert_eq!(snapshot["monitor_failed"], true);
            assert_eq!(snapshot["pending_request_count"], 1);
            assert!(!fields.to_string().contains("secret-query"));
            assert!(!fields.to_string().contains("secret-request-id"));
        }
    }

    #[test]
    fn dropping_pitch_acquisition_preserves_interrupted_stage_and_item() {
        let monitor = diagnostic_monitor(7);
        let events = diagnostic_events(|| {
            let mut diagnostics = JpdbItemDiagnostics::from_monitor(
                &monitor,
                request_diagnostic_timer(&JpdbPitchRequest::new(JpdbPitchQuery::new("雨", None))),
            );
            diagnostics.transition(JpdbPitchStage::SearchNavigation);
            diagnostics.set_epoch(7);
            drop(diagnostics);
        });
        let interrupted = events
            .iter()
            .filter(|event| event["fields"]["outcome"] == "interrupted")
            .collect::<Vec<_>>();
        assert_eq!(interrupted.len(), 2);
        assert_eq!(interrupted[0]["fields"]["stage"], "search_navigation");
        for event in interrupted {
            let fields = &event["fields"];
            assert_eq!(fields["stop_reason"], "acquisition_interrupted");
            let snapshot: Value =
                serde_json::from_str(fields["runtime_snapshot"].as_str().unwrap()).unwrap();
            assert_eq!(snapshot["epoch"], 7);
            assert_eq!(snapshot["pending_request_count"], 1);
        }
    }

    #[test]
    fn pitch_acquisition_panic_reports_failure_instead_of_cancellation() {
        let monitor = diagnostic_monitor(7);
        let events = diagnostic_events(|| {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut diagnostics = JpdbItemDiagnostics::from_monitor(
                    &monitor,
                    request_diagnostic_timer(&JpdbPitchRequest::new(JpdbPitchQuery::new(
                        "雨", None,
                    ))),
                );
                diagnostics.transition(JpdbPitchStage::Capture);
                diagnostics.set_epoch(7);
                panic!("искусственная паника");
            }));
            assert!(result.is_err());
        });
        let failures = events
            .iter()
            .filter(|event| event["fields"]["outcome"] == "failure")
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0]["fields"]["stage"], "capture");
        for event in failures {
            assert_eq!(event["fields"]["failure_code"], "worker_panic");
            assert_eq!(event["fields"]["stop_reason"], "worker_panic");
            assert_eq!(event["fields"]["retryable"], false);
        }
    }

    #[test]
    fn pitch_configuration_failure_does_not_attribute_previous_item_epoch() {
        let monitor = diagnostic_monitor(9);
        let events = diagnostic_events(|| {
            let diagnostics = JpdbItemDiagnostics::from_monitor(
                &monitor,
                request_diagnostic_timer(&JpdbPitchRequest::new(JpdbPitchQuery::new("雨", None))),
            );
            diagnostics.finish_failure(&JpdbPitchFailure::BrowserConfiguration {
                stage: JpdbPitchStage::ConfigureBrowser,
                message: "искусственный сбой настройки".into(),
            });
        });
        assert_eq!(events.len(), 2);
        for event in events {
            let fields = &event["fields"];
            assert_eq!(fields["failure_code"], "browser_configuration");
            let snapshot: Value =
                serde_json::from_str(fields["runtime_snapshot"].as_str().unwrap()).unwrap();
            assert_eq!(snapshot["epoch"], 0);
            assert_eq!(snapshot["pending_request_count"], 0);
        }
    }

    #[tokio::test]
    async fn pending_interrupt_after_launch_failure_is_preserved_for_acceptance_cleanup() {
        let mut interrupt = JpdbInterrupt::injected(async { Ok(()) });
        (&mut interrupt.listener).await.unwrap();
        let mut report =
            JpdbPitchAcquisitionReport::from_session_failure(JpdbPitchFailure::BrowserSetup {
                stage: JpdbPitchStage::ConfigureBrowser,
                message: "искусственный сбой запуска".into(),
            });

        record_pending_interrupt(
            &mut interrupt,
            &mut report,
            JpdbPitchStage::ConfigureBrowser,
        );

        assert!(interrupt.signal_observed);
        assert!(matches!(
            report.session_failure,
            Some(JpdbPitchFailure::SessionFailure { message, .. })
                if message.starts_with("acquisition_interrupted:")
                    && message.contains("browser_setup на этапе configure_browser")
                    && message.contains("искусственный сбой запуска")
        ));
    }

    #[tokio::test]
    async fn pending_interrupt_after_close_keeps_processed_outcomes_for_acceptance_cleanup() {
        let mut interrupt = JpdbInterrupt::injected(async { Ok(()) });
        (&mut interrupt.listener).await.unwrap();
        let mut report = JpdbPitchAcquisitionReport::default();
        report.outcomes.push(JpdbPitchOutcome::VocabularyNotFound {
            surface: "猫".into(),
            reading: None,
        });

        record_pending_interrupt(
            &mut interrupt,
            &mut report,
            JpdbPitchStage::ConfigureBrowser,
        );

        assert!(interrupt.signal_observed);
        assert_eq!(report.outcomes.len(), 1);
        assert!(matches!(
            report.session_failure,
            Some(JpdbPitchFailure::SessionFailure { message, .. })
                if message.starts_with("acquisition_interrupted:")
        ));
    }

    #[tokio::test]
    async fn injected_interrupt_keeps_processed_prefix_without_tail_outcomes() {
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let mut interrupt = JpdbInterrupt::injected(async {
            cancelled.await.map_err(std::io::Error::other)?;
            Ok(())
        });
        let mut report = JpdbPitchAcquisitionReport::default();
        let first = JpdbPitchOutcome::VocabularyNotFound {
            surface: "猫".into(),
            reading: None,
        };
        let completed = wait_jpdb_operation(std::future::ready(first.clone()), &mut interrupt)
            .await
            .unwrap();
        report.record_processed_outcome(completed, JpdbPitchStage::SearchResolution, false);
        cancel.send(()).unwrap();
        let interrupted =
            wait_jpdb_operation(std::future::pending::<JpdbPitchOutcome>(), &mut interrupt)
                .await
                .unwrap_err();
        report.stop_with_session_failure(JpdbPitchStage::SearchNavigation, interrupted);
        assert_eq!(report.outcomes, vec![first]);
        assert!(
            matches!(report.session_failure, Some(JpdbPitchFailure::SessionFailure { stage: JpdbPitchStage::SearchNavigation, message }) if message.starts_with("acquisition_interrupted:"))
        );
    }

    #[tokio::test]
    async fn failed_interrupt_listener_is_a_typed_session_failure() {
        let mut interrupt = JpdbInterrupt::injected(async {
            Err(std::io::Error::other("искусственная ошибка сигнала"))
        });
        let message = wait_jpdb_operation(std::future::pending::<()>(), &mut interrupt)
            .await
            .unwrap_err();
        let mut report = JpdbPitchAcquisitionReport::default();
        report.stop_with_session_failure(JpdbPitchStage::ConfigureBrowser, message);
        assert!(report.outcomes.is_empty());
        let serialized = serde_json::to_value(report.session_failure.unwrap()).unwrap();
        assert_eq!(serialized["code"], "session_failure");
        assert!(
            serialized["message"]
                .as_str()
                .unwrap()
                .starts_with("ctrl_c_listener_failed:")
        );
    }

    #[test]
    fn single_item_result_survives_browser_close_failure() {
        let outcome = JpdbPitchOutcome::VocabularyNotFound {
            surface: "猫".into(),
            reading: Some("ねこ".into()),
        };
        let report = JpdbPitchAcquisitionReport {
            outcomes: vec![outcome.clone()],
            session_failure: Some(JpdbPitchFailure::SessionFailure {
                stage: JpdbPitchStage::ConfigureBrowser,
                message: "ошибка закрытия сессии".into(),
            }),
        };
        assert_eq!(single_item_outcome(report), outcome);
    }

    #[test]
    fn single_item_result_does_not_hide_acquisition_session_failure() {
        let outcome = JpdbPitchOutcome::VocabularyNotFound {
            surface: "猫".into(),
            reading: Some("ねこ".into()),
        };
        let report = JpdbPitchAcquisitionReport {
            outcomes: vec![outcome],
            session_failure: Some(JpdbPitchFailure::SessionFailure {
                stage: JpdbPitchStage::PostCaptureVerification,
                message: "монитор остановлен".into(),
            }),
        };
        assert!(matches!(
            single_item_outcome(report),
            JpdbPitchOutcome::Failed {
                error: JpdbPitchFailure::SessionFailure {
                    stage: JpdbPitchStage::PostCaptureVerification,
                    ..
                }
            }
        ));
    }

    #[test]
    fn startup_and_configuration_failures_have_no_item_outcomes() {
        for error in [
            JpdbPitchFailure::BrowserSetup {
                stage: JpdbPitchStage::ConfigureBrowser,
                message: "Chromium не запущен".into(),
            },
            JpdbPitchFailure::BrowserConfiguration {
                stage: JpdbPitchStage::ConfigureBrowser,
                message: "Эмуляция страницы не настроена".into(),
            },
        ] {
            let report = JpdbPitchAcquisitionReport::from_session_failure(error.clone());
            assert!(report.outcomes.is_empty());
            assert_eq!(report.session_failure, Some(error));
        }
    }

    #[test]
    fn failed_monitor_before_first_request_leaves_the_entire_queue_unprocessed() {
        let mut report = JpdbPitchAcquisitionReport::default();
        assert!(report.stop_if_monitor_failed(JpdbPitchStage::SearchNavigation, true));
        assert!(report.outcomes.is_empty());
        assert!(matches!(
            report.session_failure,
            Some(JpdbPitchFailure::SessionFailure {
                stage: JpdbPitchStage::SearchNavigation,
                ..
            })
        ));
    }

    #[test]
    fn failed_monitor_preserves_processed_prefix_and_does_not_fabricate_tail() {
        let queue = [
            JpdbPitchOutcome::VocabularyNotFound {
                surface: "先頭".into(),
                reading: None,
            },
            failed(JpdbPitchFailure::Telemetry {
                stage: JpdbPitchStage::DetailReadiness,
                message: "Телеметрия потеряна во время запроса".into(),
            }),
            JpdbPitchOutcome::VocabularyNotFound {
                surface: "未処理".into(),
                reading: None,
            },
        ];
        let mut report = JpdbPitchAcquisitionReport::default();
        let mut started_items = 0;
        for (index, outcome) in queue.iter().cloned().enumerate() {
            started_items += 1;
            if report.record_processed_outcome(outcome, JpdbPitchStage::DetailReadiness, index == 1)
            {
                break;
            }
        }
        assert_eq!(started_items, 2);
        assert_eq!(report.outcomes, queue[..2]);
        assert!(matches!(
            report.session_failure,
            Some(JpdbPitchFailure::SessionFailure {
                stage: JpdbPitchStage::DetailReadiness,
                ..
            })
        ));
        assert!(report.stop_if_monitor_failed(JpdbPitchStage::SearchNavigation, false));
        assert_eq!(report.outcomes.len(), 2);
    }

    #[test]
    fn item_local_failures_keep_the_session_and_remaining_queue_available() {
        let queue = [
            failed(JpdbPitchFailure::Timeout {
                stage: JpdbPitchStage::SearchReadiness,
                diagnostic: Some("Истёк лимит элемента".into()),
            }),
            failed(JpdbPitchFailure::Telemetry {
                stage: JpdbPitchStage::DetailReadiness,
                message: "Ошибка критического ресурса страницы".into(),
            }),
            JpdbPitchOutcome::VocabularyNotFound {
                surface: "次".into(),
                reading: None,
            },
        ];
        let mut report = JpdbPitchAcquisitionReport::default();
        for outcome in queue.iter().cloned() {
            assert!(!report.record_processed_outcome(
                outcome,
                JpdbPitchStage::DetailReadiness,
                false,
            ));
        }
        assert_eq!(report.outcomes, queue);
        assert!(report.session_failure.is_none());
    }

    fn form(surface: &str, reading: &str) -> JpdbVocabularyForm {
        JpdbVocabularyForm {
            surface: surface.into(),
            reading: reading.into(),
        }
    }

    fn raw_candidate(
        detail_url: &str,
        forms: Vec<JpdbVocabularyForm>,
        part_of_speech: &[&str],
        meanings: &[&str],
    ) -> RawVocabularyCandidate {
        RawVocabularyCandidate {
            detail_url: Some(detail_url.into()),
            forms,
            part_of_speech: part_of_speech.iter().map(|value| (*value).into()).collect(),
            meanings: meanings.iter().map(|value| (*value).into()).collect(),
            used_in_urls: Vec::new(),
        }
    }

    fn capture_variant(
        row: usize,
        markup: &str,
        document_y: f64,
        paint_width: f64,
    ) -> CaptureVariant {
        let document_rect = PitchAccentCaptureRect {
            x: 100.25,
            y: document_y,
            width: 74.0,
            height: 27.25,
        };
        CaptureVariant {
            selector: format!(
                ".subsection-pitch-accent .subsection > div > div:nth-child({row}) > div:nth-child(2)[style*=\"word-break: keep-all\"]"
            ),
            text: "たのしみ".into(),
            markup: markup.into(),
            viewport_rect: PitchAccentCaptureRect {
                y: document_y - 1100.0,
                ..document_rect
            },
            document_rect,
            paint_rect: PitchAccentCaptureRect {
                width: paint_width,
                ..document_rect
            },
        }
    }

    /// Два варианта в отдельных строках на прокрученной странице; у первого
    /// отрисовка выходит на 2 CSS px правее узла, как у сегментов JPDB.
    fn capture_snapshot_for_test() -> CaptureSnapshot {
        let page = VariantPage {
            variants: vec![
                capture_variant(1, "<div>a</div>", 1450.5, 76.0),
                capture_variant(2, "<div>b</div>", 1500.5, 74.0),
            ],
            viewport_width: 1280,
            viewport_height: 1200,
            document_width: 1280,
            document_height: 2400,
            scroll_x: 0.0,
            scroll_y: 1100.0,
            page_scale_factor: 1.0,
            device_pixel_ratio: 3.0,
        };
        snapshot_selecting(page, 0)
    }

    fn snapshot_selecting(page: VariantPage, selected_index: usize) -> CaptureSnapshot {
        let capture_rect = pitch_accent_capture_rect_for(
            page.variants[selected_index].paint_rect,
            PITCH_ACCENT_CAPTURE_PADDING_CSS_PX,
        );
        CaptureSnapshot {
            page,
            selected_index,
            capture_rect,
            dark_theme: PitchAccentDarkThemeProof {
                document_element_classes: vec!["dark-mode".into()],
                prefers_color_scheme: "dark".into(),
                computed_color_scheme: "normal".into(),
                background_selector: ".subsection-pitch-accent".into(),
                background_rgb: [24, 36, 48],
            },
        }
    }

    #[test]
    fn search_url_trims_only_edges_encodes_text_and_sets_english_mode() {
        let url = Url::parse(&build_search_url("  元気 & 元気?  ").unwrap()).unwrap();
        assert_eq!(url.origin().ascii_serialization(), JPDB_ORIGIN);
        assert_eq!(url.path(), "/search");
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "q").unwrap().1,
            "元気 & 元気?"
        );
        assert_eq!(
            url.query_pairs().find(|(key, _)| key == "lang").unwrap().1,
            "english"
        );
        assert!(url.as_str().contains("%E5%85%83%E6%B0%97"));
        assert!(url.as_str().contains("%26"));
    }

    #[test]
    fn jpdb_route_parser_matches_the_validator_route_contract() {
        for value in [
            "https://jpdb.io/vocabulary/1540590/%E5%B9%BD%E9%9C%8A/%E3%82%86%E3%81%86%E3%82%8C%E3%81%84#a",
            "https://jpdb.io/vocabulary/1540590/幽霊",
            "https://jpdb.io/vocabulary/0/test",
            "https://jpdb.io/vocabulary/1540590/幽霊/used-in",
            "http://jpdb.io/vocabulary/1540590/test",
            "https://example.com/vocabulary/1540590/test",
            "https://jpdb.io/vocabulary/01540590/幽霊/ゆうれい",
        ] {
            assert_eq!(
                parse_detail_route(value),
                parse_jpdb_vocabulary_route(value).ok(),
                "разбор маршрута provider-ом и validator-ом разошёлся для {value}"
            );
        }
        let route = parse_detail_route(
            "https://jpdb.io/vocabulary/1540590/%E5%B9幽%E9%9C%8A/%E3%82%86%E3%81%86%E3%82%8C%E3%81%84",
        );
        assert!(route.is_none());
        let route = parse_detail_route("https://jpdb.io/vocabulary/1540590/%E5%B9%BD%E9%9C%8A/%E3%82%86%E3%81%86%E3%82%8C%E3%81%84").unwrap();
        assert_eq!(route.vocabulary_id, 1_540_590);
        assert_eq!(route.surface, "幽霊");
        assert_eq!(route.reading.as_deref(), Some("ゆうれい"));
    }

    #[test]
    fn candidates_match_an_exact_surface_reading_pair_and_keep_their_evidence() {
        let rows = vec![
            raw_candidate(
                "https://jpdb.io/vocabulary/10/元気/げんき",
                vec![form("元気", "げんき"), form("ゲンキ", "げんき")],
                &["Adjective (な)"],
                &["healthy", "energetic"],
            ),
            raw_candidate(
                "https://jpdb.io/vocabulary/11/元気/げんき",
                vec![form("元気", "げんき")],
                &["Name"],
                &["given name"],
            ),
            raw_candidate(
                "https://jpdb.io/vocabulary/12/元気/もとき",
                vec![form("元気", "もとき")],
                &["Noun"],
                &["origin"],
            ),
            raw_candidate(
                "https://jpdb.io/vocabulary/13/元気屋/げんきや",
                vec![form("元気屋", "げんきや")],
                &["Noun"],
                &["shop"],
            ),
            raw_candidate(
                "https://jpdb.io/vocabulary/14/猫/ねこ",
                vec![form("猫", "ねこ"), form("ネコ", "ねこ")],
                &["Noun"],
                &["cat"],
            ),
        ];
        let matched =
            select_candidates(&rows, &JpdbPitchQuery::new(" 元気 ", Some("げんき".into())))
                .unwrap();
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].vocabulary_id, 10);
        assert_eq!(matched[0].resolved_forms.len(), 2);
        assert_eq!(matched[0].part_of_speech, ["Adjective (な)"]);
        assert_eq!(matched[0].meanings, ["energetic", "healthy"]);
        assert_eq!(
            matched[0].detail_url,
            "https://jpdb.io/vocabulary/10/元気/げんき"
        );

        let ambiguous = select_candidates(&rows, &JpdbPitchQuery::new("元気", None)).unwrap();
        assert_eq!(ambiguous.len(), 2);
        assert_eq!(ambiguous[0].vocabulary_id, 10);
        assert_eq!(ambiguous[1].vocabulary_id, 12);
        assert!(
            select_candidates(&rows, &JpdbPitchQuery::new("げんき", None))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            select_candidates(&rows, &JpdbPitchQuery::new("ネコ", Some("ねこ".into()))).unwrap()[0]
                .vocabulary_id,
            14
        );
    }

    #[test]
    fn concrete_detail_href_supports_only_its_own_surface_reading_pair() {
        let rows = [raw_candidate(
            "https://jpdb.io/vocabulary/14/猫/ねこ",
            vec![form("猫", "ねこ")],
            &["Noun"],
            &["cat"],
        )];
        let route_pair =
            select_candidates(&rows, &JpdbPitchQuery::new("猫", Some("ねこ".into()))).unwrap();
        assert_eq!(route_pair.len(), 1);
        assert!(
            route_pair[0]
                .resolved_forms
                .contains(&PitchAccentResolvedForm {
                    surface: "猫".into(),
                    reading: "ねこ".into(),
                })
        );

        let unrelated_pair =
            select_candidates(&rows, &JpdbPitchQuery::new("ネコ", Some("ねこ".into()))).unwrap();
        assert!(unrelated_pair.is_empty());
    }

    #[test]
    fn candidate_order_does_not_change_inventory_or_resolution() {
        let rows = vec![
            raw_candidate(
                "https://jpdb.io/vocabulary/14/猫/ねこ",
                vec![form("猫", "ねこ"), form("ネコ", "ねこ")],
                &["Noun"],
                &["cat", "feline"],
            ),
            raw_candidate(
                "https://jpdb.io/vocabulary/12/元気/もとき",
                vec![form("元気", "もとき")],
                &["Noun"],
                &["vigor"],
            ),
            raw_candidate(
                "https://jpdb.io/vocabulary/10/元気/げんき",
                vec![form("元気", "げんき")],
                &["Adjective (な)"],
                &["healthy"],
            ),
        ];
        let query = JpdbPitchQuery::new("元気", None);
        let forward = select_candidates(&rows, &query).unwrap();
        let reverse =
            select_candidates(&rows.iter().cloned().rev().collect::<Vec<_>>(), &query).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(
            forward
                .iter()
                .map(|item| item.vocabulary_id)
                .collect::<Vec<_>>(),
            [10, 12]
        );
    }

    #[test]
    fn explicit_selection_requires_candidate_id_and_full_detail_route_match() {
        let query = JpdbPitchQuery::new("元気", None);
        let candidates = select_candidates(
            &[
                raw_candidate(
                    "https://jpdb.io/vocabulary/12/元気/もとき",
                    vec![form("元気", "もとき")],
                    &["Noun"],
                    &["origin"],
                ),
                raw_candidate(
                    "https://jpdb.io/vocabulary/10/元気/げんき",
                    vec![form("元気", "げんき")],
                    &["Noun"],
                    &["health"],
                ),
            ],
            &query,
        )
        .unwrap();
        let selection = JpdbPitchSelection::new(
            12,
            "https://jpdb.io/vocabulary/12/元気/もとき?from=search#entry",
        )
        .unwrap();
        let route = selection.validate().unwrap();
        assert_eq!(
            select_explicit_candidate(&candidates, &selection, &route)
                .unwrap()
                .vocabulary_id,
            12
        );

        let wrong_route =
            JpdbPitchSelection::new(12, "https://jpdb.io/vocabulary/12/元気/げんき").unwrap();
        let wrong_route_parsed = wrong_route.validate().unwrap();
        assert!(matches!(
            select_explicit_candidate(&candidates, &wrong_route, &wrong_route_parsed),
            Err(JpdbPitchFailure::ExplicitSelectionMismatch { .. })
        ));
        assert!(
            JpdbPitchSelection::new(10, "https://example.com/vocabulary/10/元気/げんき",).is_err()
        );
        assert!(
            JpdbPitchSelection {
                vocabulary_id: 99,
                detail_url: "https://jpdb.io/vocabulary/12/元気/もとき".into(),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn candidate_rows_require_a_real_detail_href() {
        let rows = vec![raw_candidate(
            "https://jpdb.io/vocabulary/12/元気/used-in",
            vec![form("元気", "げんき")],
            &["Noun"],
            &["health"],
        )];
        assert!(matches!(
            select_candidates(&rows, &JpdbPitchQuery::new("元気", None)),
            Err(JpdbPitchFailure::PageContract { .. })
        ));
    }

    #[test]
    fn malformed_unrelated_search_row_is_a_contract_failure_in_either_order() {
        let valid = raw_candidate(
            "https://jpdb.io/vocabulary/12/元気/げんき",
            vec![form("元気", "げんき")],
            &["Noun"],
            &["health"],
        );
        let malformed = raw_candidate(
            "https://jpdb.io/vocabulary/13/別語/べつご",
            vec![form("別語", "べつご")],
            &["Noun"],
            &[],
        );
        let query = JpdbPitchQuery::new("元気", None);
        assert_eq!(
            select_candidates(std::slice::from_ref(&valid), &query)
                .unwrap()
                .len(),
            1
        );
        for rows in [
            vec![valid.clone(), malformed.clone()],
            vec![malformed, valid],
        ] {
            assert!(
                matches!(
                    select_candidates(&rows, &query),
                    Err(JpdbPitchFailure::PageContract {
                        stage: JpdbPitchStage::SearchResolution,
                        ..
                    })
                ),
                "Неполная строка не подтверждает целостность всего списка поиска"
            );
        }
    }

    #[test]
    fn script_reported_incomplete_row_is_not_converted_to_vocabulary_not_found() {
        let message = "Строка JPDB не содержит подтверждённые формы, часть речи, значения и фактическую ссылку на словарную запись";
        let failure = decode_search_snapshot(serde_json::json!({
            "error": message,
            "result_count": 2,
            "invalid_row_index": 1,
            "form_count": 0,
            "part_of_speech_count": 1,
            "meaning_count": 1,
            "has_detail_url": true
        }))
        .unwrap_err();
        assert_eq!(
            failure,
            JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::SearchResolution,
                message: message.into()
            }
        );
        assert!(matches!(
            decode_search_snapshot(serde_json::json!({
                "ready": false, "no_results_found": false, "result_count": 0,
                "truncated": false, "candidates": []
            })),
            Err(JpdbPitchFailure::Timeout { .. })
        ));
        assert!(
            decode_search_snapshot(serde_json::json!({
                "ready": true, "no_results_found": true, "result_count": 0,
                "truncated": false, "candidates": []
            }))
            .is_ok()
        );
    }

    #[test]
    fn reading_must_belong_to_the_same_surface_form_pair() {
        let forms = vec![
            form("ネコ", "ネコ"),
            form("猫", "ねこ"),
            form("ねこ", "ねこ"),
        ];
        assert!(forms_match_query(
            &forms,
            &JpdbPitchQuery::new("ネコ", Some("ねこ".into()))
        ));
        assert!(!forms_match_query(
            &forms,
            &JpdbPitchQuery::new("ネコ", Some("しょうじょ".into()))
        ));
        assert!(forms_match_query(
            &forms,
            &JpdbPitchQuery::new(" ネコ ", Some(" ネコ ".into()))
        ));
        assert_eq!(
            resolve_query_reading(&forms, &JpdbPitchQuery::new("ネコ", Some("ねこ".into())))
                .as_deref(),
            Some("ネコ"),
            "сохраняется фактическая запись чтения из связанной формы JPDB"
        );
        assert_eq!(
            resolve_query_reading(&forms, &JpdbPitchQuery::new("ネコ", None)).as_deref(),
            Some("ネコ")
        );

        let otome_forms = vec![form("乙女", "おとめ"), form("少女", "しょうじょ")];
        assert!(forms_match_query(
            &otome_forms,
            &JpdbPitchQuery::new("乙女", Some("おとめ".into()))
        ));
        assert!(!forms_match_query(
            &otome_forms,
            &JpdbPitchQuery::new("乙女", Some("しょうじょ".into()))
        ));
        let multiple = vec![form("生", "せい"), form("生", "なま")];
        assert!(resolve_query_reading(&multiple, &JpdbPitchQuery::new("生", None)).is_none());
        assert_eq!(
            resolve_query_reading(&multiple, &JpdbPitchQuery::new("生", Some("なま".into())))
                .as_deref(),
            Some("なま")
        );
    }

    /// Снимок поиска, который `SEARCH_CANDIDATES_SCRIPT` отдаёт для одиночной
    /// записи JPDB в режиме подробностей: строка без ссылки на карточку.
    fn single_entry_search(
        row_form: (&str, &str),
        used_in_urls: &[&str],
        canonical_urls: &[&str],
    ) -> serde_json::Value {
        serde_json::json!({
            "ready": true,
            "no_results_found": false,
            "result_count": 1,
            "vocabulary_row_count": 1,
            "truncated": false,
            "details_view": true,
            "canonical_urls": canonical_urls,
            "candidates": [{
                "detail_url": null,
                "forms": [{ "surface": row_form.0, "reading": row_form.1 }],
                "part_of_speech": ["Noun"],
                "meanings": ["meaning"],
                "used_in_urls": used_in_urls,
            }],
        })
    }

    fn detail_snapshot(
        url: &str,
        vocabulary_id: u64,
        forms: Vec<JpdbVocabularyForm>,
    ) -> DetailSnapshot {
        DetailSnapshot {
            url: url.into(),
            vocabulary_id,
            forms,
            part_of_speech: vec!["Noun".into()],
            section_inventory: vec!["Meanings".into(), "Pitch accent".into()],
            base_page_contract_valid: true,
            pitch_section_present: true,
            pitch_label: Some("Pitch accent".into()),
            pitch_marker_count: 1,
            graph_count: 1,
        }
    }

    /// Повторяет проверки `acquire_one_in_session` между поиском и переходом.
    fn resolve_single(
        raw: serde_json::Value,
        query: &JpdbPitchQuery,
    ) -> Result<Vec<JpdbVocabularyCandidate>, JpdbPitchFailure> {
        let snapshot = decode_search_snapshot(raw)?;
        let candidates = select_candidates(&snapshot.candidates, query)?;
        for candidate in &candidates {
            let route = parse_detail_route(&candidate.detail_url).unwrap();
            assert_eq!(route.vocabulary_id, candidate.vocabulary_id);
            assert!(route_matches_resolved_forms(
                &route,
                &candidate.resolved_forms
            ));
        }
        Ok(candidates)
    }

    #[test]
    fn single_entry_without_row_link_resolves_only_through_corroborated_canonical() {
        for (id, surface, reading, canonical_reading) in [
            (1_192_150, "家族", "かぞく", Some("かぞく")),
            (1_544_590, "余裕", "よゆう", Some("よゆう")),
            (1_421_520, "地雷", "じらい", Some("じらい")),
            // Запись каной: canonical JPDB не содержит сегмента чтения.
            (1_061_520, "シャツ", "シャツ", None),
        ] {
            let canonical = match canonical_reading {
                Some(reading) => {
                    format!("https://jpdb.io/vocabulary/{id}/{surface}/{reading}?lang=english")
                }
                None => format!("https://jpdb.io/vocabulary/{id}/{surface}?lang=english"),
            };
            let used_in = format!("https://jpdb.io/vocabulary/{id}/{surface}/used-in");
            let raw = single_entry_search((surface, reading), &[&used_in], &[&canonical]);

            for query in [
                JpdbPitchQuery::new(surface, None),
                JpdbPitchQuery::new(surface, Some(reading.into())),
            ] {
                let candidates = resolve_single(raw.clone(), &query).unwrap();
                assert_eq!(candidates.len(), 1, "{surface}");
                let candidate = &candidates[0];
                assert_eq!(candidate.vocabulary_id, id);
                assert_eq!(candidate.detail_url, canonical);
                assert_eq!(
                    candidate.resolved_forms,
                    vec![PitchAccentResolvedForm {
                        surface: surface.into(),
                        reading: reading.into(),
                    }],
                    "canonical не добавляет форм, которых нет в строке"
                );

                // Переход по canonical-маршруту открывает ту же карточку.
                let route = parse_detail_route(&candidate.detail_url).unwrap();
                let detail = detail_snapshot(&canonical, id, vec![form(surface, reading)]);
                assert!(
                    validate_navigated_detail_identity(candidate, &route, &detail, &query).is_ok()
                );

                let selection = JpdbPitchSelection::new(id, canonical.clone()).unwrap();
                let selection_route = selection.validate().unwrap();
                assert_eq!(
                    select_explicit_candidate(&candidates, &selection, &selection_route).unwrap(),
                    *candidate
                );
                let other = JpdbPitchSelection::new(
                    id + 1,
                    format!("https://jpdb.io/vocabulary/{}/{surface}", id + 1),
                )
                .unwrap();
                let other_route = other.validate().unwrap();
                assert!(matches!(
                    select_explicit_candidate(&candidates, &other, &other_route),
                    Err(JpdbPitchFailure::ExplicitSelectionMismatch { .. })
                ));
            }

            // Canonical подтверждает запись, но не чужое чтение или написание запроса.
            for query in [
                JpdbPitchQuery::new(surface, Some("べつのよみ".into())),
                JpdbPitchQuery::new("別語", None),
            ] {
                assert_eq!(resolve_single(raw.clone(), &query).unwrap(), Vec::new());
            }
        }
    }

    #[test]
    fn canonical_fallback_refuses_without_complete_consistent_identity_evidence() {
        let used_in = "https://jpdb.io/vocabulary/1192150/家族/used-in";
        let canonical = "https://jpdb.io/vocabulary/1192150/家族/かぞく?lang=english";
        let row = ("家族", "かぞく");
        let query = JpdbPitchQuery::new("家族", None);
        assert_eq!(
            resolve_single(single_entry_search(row, &[used_in], &[canonical]), &query)
                .unwrap()
                .len(),
            1
        );

        let mut cases: Vec<(&str, serde_json::Value)> = vec![
            ("нет canonical", single_entry_search(row, &[used_in], &[])),
            (
                "два canonical",
                single_entry_search(row, &[used_in], &[canonical, canonical]),
            ),
            (
                "нет ссылки Used in",
                single_entry_search(row, &[], &[canonical]),
            ),
            (
                "чтение canonical не наблюдено в строке",
                single_entry_search(("家族", "いえぞく"), &[used_in], &[canonical]),
            ),
            (
                "написание canonical не наблюдено в строке",
                single_entry_search(("家", "いえ"), &[used_in], &[canonical]),
            ),
            (
                "canonical без чтения для записи с кандзи",
                single_entry_search(
                    row,
                    &[used_in],
                    &["https://jpdb.io/vocabulary/1192150/家族?lang=english"],
                ),
            ),
            (
                "ID canonical не совпадает со строкой",
                single_entry_search(
                    row,
                    &[used_in],
                    &["https://jpdb.io/vocabulary/1192151/家族/かぞく"],
                ),
            ),
            (
                "написание canonical не совпадает с Used in",
                single_entry_search(
                    row,
                    &["https://jpdb.io/vocabulary/1192150/家/used-in"],
                    &[canonical],
                ),
            ),
            (
                "одна из ссылок Used in противоречит canonical",
                single_entry_search(
                    row,
                    &[used_in, "https://jpdb.io/vocabulary/1405770/族/used-in"],
                    &[canonical],
                ),
            ),
            (
                "Used in на другом origin",
                single_entry_search(
                    row,
                    &["https://evil.example/vocabulary/1192150/家族/used-in"],
                    &[canonical],
                ),
            ),
        ];
        for bad in [
            "https://evil.example/vocabulary/1192150/家族/かぞく",
            "https://jpdb.io.evil.example/vocabulary/1192150/家族/かぞく",
            "http://jpdb.io/vocabulary/1192150/家族/かぞく",
            "https://jpdb.io/vocabulary/1192150/家族/used-in",
            "https://jpdb.io/vocabulary/1192150",
            "https://jpdb.io/search?q=家族&lang=english",
            "https://jpdb.io/kanji/家",
            "/vocabulary/1192150/家族/かぞく",
            "",
        ] {
            cases.push((
                "недопустимый canonical",
                single_entry_search(row, &[used_in], &[bad]),
            ));
        }
        let mut not_details = single_entry_search(row, &[used_in], &[canonical]);
        not_details["details_view"] = false.into();
        cases.push(("не режим подробностей", not_details));
        let mut more_results = single_entry_search(row, &[used_in], &[canonical]);
        more_results["result_count"] = 2.into();
        cases.push(("результатов больше одной строки", more_results));
        let mut second_row = single_entry_search(row, &[used_in], &[canonical]);
        let copy = second_row["candidates"][0].clone();
        second_row["candidates"].as_array_mut().unwrap().push(copy);
        second_row["result_count"] = 2.into();
        cases.push(("несколько строк", second_row));

        for (name, raw) in cases {
            assert!(
                matches!(
                    resolve_single(raw, &query),
                    Err(JpdbPitchFailure::PageContract {
                        stage: JpdbPitchStage::SearchResolution,
                        ..
                    })
                ),
                "canonical не должен подтверждать запись: {name}"
            );
        }
    }

    #[test]
    fn canonical_without_reading_is_accepted_only_for_kana_surfaces() {
        let query_for = |surface: &str| JpdbPitchQuery::new(surface, None);
        for (id, surface) in [
            (1_061_520, "シャツ"),
            (2_008_730, "それにしても"),
            (1_000_001, "ゔぁー"),
        ] {
            let raw = single_entry_search(
                (surface, surface),
                &[&format!(
                    "https://jpdb.io/vocabulary/{id}/{surface}/used-in"
                )],
                &[&format!(
                    "https://jpdb.io/vocabulary/{id}/{surface}?lang=english"
                )],
            );
            assert_eq!(
                resolve_single(raw, &query_for(surface)).unwrap().len(),
                1,
                "{surface}"
            );
        }
        // Чтение, совпавшее с написанием, не делает запись с кандзи или латиницей записью каной:
        // например, если `<rt>` повторяет основу.
        for (id, surface) in [
            (1_192_150, "家族"),
            (1_000_002, "お茶"),
            (1_000_003, "Tシャツ"),
            (1_000_004, "シャツ1"),
        ] {
            let raw = single_entry_search(
                (surface, surface),
                &[&format!(
                    "https://jpdb.io/vocabulary/{id}/{surface}/used-in"
                )],
                &[&format!(
                    "https://jpdb.io/vocabulary/{id}/{surface}?lang=english"
                )],
            );
            assert!(
                matches!(
                    resolve_single(raw, &query_for(surface)),
                    Err(JpdbPitchFailure::PageContract { .. })
                ),
                "canonical без чтения не подтверждает запись не каной: {surface}"
            );
        }
    }

    #[test]
    fn kana_text_matches_the_page_script_ranges() {
        for text in ["かな", "カナ", "ー", "ゔ", "\u{3099}", "ヿ"] {
            assert!(is_kana_text(text), "{text}");
        }
        for text in ["", "家", "お茶", "Tシャツ", "ｶﾅ", "かな ", "々"] {
            assert!(!is_kana_text(text), "{text:?}");
        }
    }

    #[test]
    fn row_detail_link_keeps_priority_over_canonical() {
        let row_link = "https://jpdb.io/vocabulary/1358280/食べる/たべる?lang=english#a";
        for canonical in [
            "https://jpdb.io/vocabulary/1358280/食べる/たべる?lang=english",
            "https://jpdb.io/vocabulary/999/別語/べつご",
        ] {
            let raw = serde_json::json!({
                "ready": true, "no_results_found": false, "result_count": 1,
                "truncated": false, "details_view": true, "canonical_urls": [canonical],
                "candidates": [{
                    "detail_url": row_link,
                    "forms": [
                        { "surface": "食べる", "reading": "たべる" },
                        { "surface": "喰べる", "reading": "たべる" },
                    ],
                    "part_of_speech": ["Ichidan verb", "Transitive verb"],
                    "meanings": ["to eat"],
                    "used_in_urls": ["https://jpdb.io/vocabulary/1358280/食べる/used-in"],
                }],
            });
            let candidates = resolve_single(raw, &JpdbPitchQuery::new("食べる", None)).unwrap();
            assert_eq!(candidates.len(), 1);
            assert_eq!(candidates[0].vocabulary_id, 1_358_280);
            assert_eq!(candidates[0].detail_url, row_link);
        }
    }

    #[test]
    fn ambiguous_search_is_never_narrowed_by_canonical() {
        let row = |id: u64, surface: &str, reading: &str| {
            serde_json::json!({
                "detail_url": format!("https://jpdb.io/vocabulary/{id}/{surface}/{reading}?lang=english#a"),
                "forms": [{ "surface": surface, "reading": reading }],
                "part_of_speech": ["Noun"],
                "meanings": ["meaning"],
                "used_in_urls": [format!("https://jpdb.io/vocabulary/{id}/{surface}/used-in")],
            })
        };
        // Одинаковое написание, разные чтения и ID: canonical одного из них не выбирает его.
        for canonical in [
            Vec::<String>::new(),
            vec!["https://jpdb.io/vocabulary/1580210/上手/じょうず".into()],
        ] {
            let raw = serde_json::json!({
                "ready": true, "no_results_found": false, "result_count": 2,
                "truncated": false, "details_view": false, "canonical_urls": canonical,
                "candidates": [row(1_580_210, "上手", "じょうず"), row(1_580_230, "上手", "うわて")],
            });
            let candidates =
                resolve_single(raw.clone(), &JpdbPitchQuery::new("上手", None)).unwrap();
            assert_eq!(
                candidates
                    .iter()
                    .map(|candidate| candidate.vocabulary_id)
                    .collect::<Vec<_>>(),
                vec![1_580_210, 1_580_230]
            );
            let narrowed =
                resolve_single(raw, &JpdbPitchQuery::new("上手", Some("うわて".into()))).unwrap();
            assert_eq!(narrowed.len(), 1);
            assert_eq!(narrowed[0].vocabulary_id, 1_580_230);
        }

        // Неоднозначная выдача с одной строкой без ссылки отказывает целиком.
        let raw = serde_json::json!({
            "ready": true, "no_results_found": false, "result_count": 2,
            "truncated": false, "details_view": false,
            "canonical_urls": ["https://jpdb.io/vocabulary/1589150/神/かみ"],
            "candidates": [
                row(1_589_150, "神", "かみ"),
                {
                    "detail_url": null,
                    "forms": [{ "surface": "紙", "reading": "かみ" }],
                    "part_of_speech": ["Noun"],
                    "meanings": ["paper"],
                    "used_in_urls": ["https://jpdb.io/vocabulary/1589160/紙/used-in"],
                },
            ],
        });
        assert!(matches!(
            resolve_single(raw, &JpdbPitchQuery::new("紙", None)),
            Err(JpdbPitchFailure::PageContract { .. })
        ));
    }

    #[test]
    fn navigated_detail_must_keep_the_identity_resolved_from_search() {
        let canonical = "https://jpdb.io/vocabulary/1192150/家族/かぞく?lang=english";
        let raw = single_entry_search(
            ("家族", "かぞく"),
            &["https://jpdb.io/vocabulary/1192150/家族/used-in"],
            &[canonical],
        );
        let query = JpdbPitchQuery::new("家族", None);
        let candidate = resolve_single(raw, &query).unwrap().remove(0);
        let route = parse_detail_route(&candidate.detail_url).unwrap();
        let mut named = detail_snapshot(canonical, 1_192_150, vec![form("家族", "かぞく")]);
        named.part_of_speech.push("Name".into());

        for (name, detail) in [
            (
                "другой ID в маршруте",
                detail_snapshot(
                    "https://jpdb.io/vocabulary/1192151/家族/かぞく",
                    1_192_151,
                    vec![form("家族", "かぞく")],
                ),
            ),
            (
                "ID страницы расходится с кандидатом",
                detail_snapshot(canonical, 1_192_151, vec![form("家族", "かぞく")]),
            ),
            (
                "другое чтение в маршруте",
                detail_snapshot(
                    "https://jpdb.io/vocabulary/1192150/家族/いえぞく",
                    1_192_150,
                    vec![form("家族", "いえぞく")],
                ),
            ),
            (
                "маршрут без чтения вместо подтверждённого",
                detail_snapshot(
                    "https://jpdb.io/vocabulary/1192150/家族",
                    1_192_150,
                    vec![form("家族", "かぞく")],
                ),
            ),
            (
                "формы страницы другого слова",
                detail_snapshot(canonical, 1_192_150, vec![form("家", "いえ")]),
            ),
            ("имя собственное", named),
        ] {
            assert!(
                matches!(
                    validate_navigated_detail_identity(&candidate, &route, &detail, &query),
                    Err(JpdbPitchFailure::DetailIdentityMismatch { .. })
                ),
                "подмена записи после перехода должна отвергаться: {name}"
            );
        }
        let detail = detail_snapshot(canonical, 1_192_150, vec![form("家族", "かぞく")]);
        assert!(matches!(
            validate_navigated_detail_identity(
                &candidate,
                &route,
                &detail,
                &JpdbPitchQuery::new("家族", Some("いえぞく".into()))
            ),
            Err(JpdbPitchFailure::DetailIdentityMismatch { .. })
        ));
    }

    #[test]
    fn not_found_requires_a_valid_ready_empty_search_inventory() {
        let explicit_empty = RawSearchSnapshot {
            ready: true,
            no_results_found: true,
            result_count: 0,
            truncated: false,
            candidates: Vec::new(),
            canonical_urls: Vec::new(),
            details_view: false,
        };
        assert!(validate_search_inventory(&explicit_empty).is_ok());
        let unresolved_empty = RawSearchSnapshot {
            ready: false,
            no_results_found: false,
            result_count: 0,
            truncated: false,
            candidates: Vec::new(),
            canonical_urls: Vec::new(),
            details_view: false,
        };
        assert!(matches!(
            validate_search_inventory(&unresolved_empty),
            Err(JpdbPitchFailure::PageContract { .. })
        ));
        let capped = RawSearchSnapshot {
            ready: true,
            no_results_found: false,
            result_count: 129,
            truncated: true,
            candidates: Vec::new(),
            canonical_urls: Vec::new(),
            details_view: false,
        };
        assert!(matches!(
            validate_search_inventory(&capped),
            Err(JpdbPitchFailure::PageContract { .. })
        ));
    }

    #[test]
    fn redirected_detail_landing_must_match_query_and_explicit_selection() {
        let detail = DetailSnapshot {
            url: "https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい?from=search".into(),
            vocabulary_id: 1_540_590,
            forms: vec![form("幽霊", "ゆうれい")],
            part_of_speech: vec!["Noun".into()],
            section_inventory: vec!["Meanings".into()],
            base_page_contract_valid: true,
            pitch_section_present: false,
            pitch_label: None,
            pitch_marker_count: 0,
            graph_count: 0,
        };
        let route = parse_detail_route(&detail.url).unwrap();
        let query = JpdbPitchQuery::new("幽霊", Some("ゆうれい".into()));
        let selection = JpdbPitchSelection::new(
            route.vocabulary_id,
            "https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい",
        )
        .unwrap();
        let selection_route = selection.validate().unwrap();

        assert!(
            validate_redirected_detail_identity(
                &route,
                &detail,
                &query,
                Some(&selection),
                Some(&selection_route),
            )
            .is_ok()
        );

        let other_selection = JpdbPitchSelection::new(
            1_540_591,
            "https://jpdb.io/vocabulary/1540591/幽霊/ゆうれい",
        )
        .unwrap();
        let other_selection_route = other_selection.validate().unwrap();
        assert!(matches!(
            validate_redirected_detail_identity(
                &route,
                &detail,
                &query,
                Some(&other_selection),
                Some(&other_selection_route),
            ),
            Err(JpdbPitchFailure::ExplicitSelectionMismatch { .. })
        ));

        assert!(matches!(
            validate_redirected_detail_identity(
                &route,
                &detail,
                &JpdbPitchQuery::new("幽霊", Some("しょうじょ".into())),
                None,
                None,
            ),
            Err(JpdbPitchFailure::DetailIdentityMismatch { .. })
        ));
    }

    #[test]
    fn no_pitch_requires_a_verified_detail_and_missing_section() {
        let detail = DetailSnapshot {
            url: "https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい".into(),
            vocabulary_id: 1_540_590,
            forms: vec![form("幽霊", "ゆうれい")],
            part_of_speech: vec!["Noun".into()],
            section_inventory: vec!["Meanings".into(), "Kanji used".into()],
            base_page_contract_valid: true,
            pitch_section_present: false,
            pitch_label: None,
            pitch_marker_count: 0,
            graph_count: 0,
        };
        assert!(detail_matches_query(
            &detail,
            &JpdbPitchQuery::new("幽霊", None)
        ));
        assert_eq!(classify_pitch_state(&detail), Ok(PitchDomState::Absent));

        let mut route_slug_not_in_forms = detail.clone();
        route_slug_not_in_forms.url = "https://jpdb.io/vocabulary/1540590/幽霊/レイ".into();
        assert!(!detail_matches_query(
            &route_slug_not_in_forms,
            &JpdbPitchQuery::new("幽霊", None)
        ));

        let mut unexpected_empty_section = detail.clone();
        unexpected_empty_section.pitch_section_present = true;
        unexpected_empty_section.pitch_label = Some("Pitch accent".into());
        unexpected_empty_section.pitch_marker_count = 1;
        assert!(matches!(
            classify_pitch_state(&unexpected_empty_section),
            Err(JpdbPitchFailure::PageContract { .. })
        ));

        let mut selector_drift = detail.clone();
        selector_drift.pitch_marker_count = 1;
        selector_drift.section_inventory.push("Pitch accent".into());
        assert!(matches!(
            classify_pitch_state(&selector_drift),
            Err(JpdbPitchFailure::PageContract { .. })
        ));

        let mut incomplete_base_contract = detail.clone();
        incomplete_base_contract.base_page_contract_valid = false;
        assert!(matches!(
            classify_pitch_state(&incomplete_base_contract),
            Err(JpdbPitchFailure::PageContract { .. })
        ));

        let mut wrong_identity = detail;
        wrong_identity.vocabulary_id = 99;
        assert!(!detail_matches_query(
            &wrong_identity,
            &JpdbPitchQuery::new("幽霊", None)
        ));
    }

    #[test]
    fn search_and_detail_routes_are_recognized_only_on_jpdb() {
        assert!(is_search_url("https://jpdb.io/search?q=%E5%B9%BD%E9%9C%8A"));
        assert!(!is_search_url(
            "https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい"
        ));
        assert!(parse_detail_route("https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい").is_some());
        assert!(parse_detail_route("https://jpdb.io/vocabulary/1540590/幽霊").is_some());
        assert!(
            parse_detail_route("https://jpdb.io/vocabulary/1540590/幽霊/ゆうれい?x=1#a").is_some()
        );
        assert!(!is_search_url("https://example.com/search?q=幽霊"));
    }

    #[test]
    fn readiness_tracks_main_navigation_and_relevant_jpdb_resources() {
        use chromiumoxide::cdp::browser_protocol::network::ResourceType;
        assert!(is_critical_resource(
            &ResourceType::Document,
            Some("https://analytics.example/frame"),
            true
        ));
        assert!(!is_critical_resource(
            &ResourceType::Document,
            Some("https://analytics.example/frame"),
            false
        ));
        assert!(!is_critical_resource(
            &ResourceType::Script,
            Some("https://analytics.example/widget.js"),
            false
        ));
        assert!(is_critical_resource(
            &ResourceType::Script,
            Some("https://jpdb.io/assets/application.js"),
            false
        ));
        assert!(!is_critical_resource(
            &ResourceType::Image,
            Some("https://jpdb.io/images/background.png"),
            false
        ));
    }

    #[test]
    fn capture_region_is_the_padded_paint_of_one_variant_inside_the_document() {
        let snapshot = capture_snapshot_for_test();
        assert!(snapshot.page.scroll_y > 0.0);
        assert!(validate_variant_page(&snapshot.page, JpdbPitchStage::Capture).is_ok());
        assert!(validate_capture_region(&snapshot, JpdbPitchStage::Capture).is_ok());
        assert_eq!(
            snapshot.capture_rect,
            PitchAccentCaptureRect {
                x: 97.0,
                y: 1447.0,
                width: 83.0,
                height: 34.0,
            },
            "субпиксельная отрисовка с выступом расширяется на 3 CSS px и до целых CSS px"
        );

        let mut wrong_offset = snapshot.clone();
        wrong_offset.page.variants[1].document_rect.y += 5.0;
        assert!(validate_variant_page(&wrong_offset.page, JpdbPitchStage::Capture).is_err());
        let mut viewport_mismatch = snapshot.clone();
        viewport_mismatch.page.variants[0].viewport_rect.y += 1.0;
        assert!(validate_variant_page(&viewport_mismatch.page, JpdbPitchStage::Capture).is_err());
        let mut paint_inside_node = snapshot.clone();
        paint_inside_node.page.variants[0].paint_rect.width = 50.0;
        assert!(validate_variant_page(&paint_inside_node.page, JpdbPitchStage::Capture).is_err());

        let mut unpadded = snapshot.clone();
        unpadded.capture_rect = unpadded.page.variants[0].paint_rect;
        assert!(validate_capture_region(&unpadded, JpdbPitchStage::Capture).is_err());
        let mut node_only = snapshot.clone();
        node_only.capture_rect = pitch_accent_capture_rect_for(
            node_only.page.variants[0].document_rect,
            PITCH_ACCENT_CAPTURE_PADDING_CSS_PX,
        );
        assert!(
            validate_capture_region(&node_only, JpdbPitchStage::Capture).is_err(),
            "область от узла без выступающих сегментов обрезала бы рисунок"
        );

        let mut close_neighbour = snapshot.page.clone();
        close_neighbour.variants[1].document_rect.y = 1479.0;
        close_neighbour.variants[1].viewport_rect.y = 379.0;
        close_neighbour.variants[1].paint_rect.y = 1479.0;
        let close_neighbour = snapshot_selecting(close_neighbour, 0);
        assert!(validate_variant_page(&close_neighbour.page, JpdbPitchStage::Capture).is_ok());
        assert!(
            validate_capture_region(&close_neighbour, JpdbPitchStage::Capture).is_err(),
            "поле не должно задевать соседний вариант"
        );

        let mut at_edge = snapshot.page.clone();
        for variant in &mut at_edge.variants {
            variant.document_rect.x = 1.0;
            variant.viewport_rect.x = 1.0;
            variant.paint_rect.x = 1.0;
        }
        let at_edge = snapshot_selecting(at_edge, 0);
        assert!(validate_variant_page(&at_edge.page, JpdbPitchStage::Capture).is_ok());
        assert!(
            validate_capture_region(&at_edge, JpdbPitchStage::Capture).is_err(),
            "поле за пределами документа не публикуется"
        );
    }

    #[test]
    fn capture_script_marks_variant_rows_and_receives_selection_and_padding() {
        assert!(CAPTURE_SNAPSHOT_SCRIPT.contains("nodeIndex + 1"));
        assert!(CAPTURE_SNAPSHOT_SCRIPT.contains("rowIndex + 1"));
        assert!(CAPTURE_SNAPSHOT_SCRIPT.contains("rowGraphs !== 1"));
        assert!(CAPTURE_SNAPSHOT_SCRIPT.contains("Math.floor(paint.x - padding)"));
        assert!(!CAPTURE_SNAPSHOT_SCRIPT.contains("graphUnion"));
        assert_eq!(
            capture_script_call(None),
            format!("() => ({CAPTURE_SNAPSHOT_SCRIPT})(null, 3.0)")
        );
        assert!(capture_script_call(Some(2)).ends_with(")(2, 3.0)"));
    }

    #[test]
    fn item_timeout_keeps_the_stage_that_was_running() {
        let outcome = item_timeout(JpdbPitchStage::PostCaptureVerification);
        assert!(matches!(
            outcome,
            JpdbPitchOutcome::Failed {
                error: JpdbPitchFailure::Timeout {
                    stage: JpdbPitchStage::PostCaptureVerification,
                    ..
                }
            }
        ));
    }

    #[test]
    fn browser_evaluation_failures_keep_the_stage_and_use_a_technical_outcome() {
        let failure = browser_evaluation_failure(
            JpdbPitchStage::SearchResolution,
            "чтении результатов поиска",
            "цель закрыта",
        );
        let value = serde_json::to_value(failure).unwrap();
        assert_eq!(value["code"], "browser_evaluation");
        assert_eq!(value["stage"], "search_resolution");
        assert!(value["message"].as_str().unwrap().contains("цель закрыта"));
        assert_ne!(value["code"], "page_contract");
    }

    /// Строка варианта в разметке JPDB: кнопка аудио и график из сегментов. Второй
    /// сегмент с `margin-right: -2px` выходит за прямоугольник узла графика.
    const AUDIO_BUTTON: &str = r##"<a class="icon-link vocabulary-audio" href="#" style="display:inline-block;width:28px;height:42px;background:#2a3a4a"></a>"##;

    fn jpdb_like_row(color: &str, extra: &str) -> String {
        format!(
            r##"<div style="display:flex;column-gap:1rem;align-items:center">{AUDIO_BUTTON}<div style="word-break: keep-all; display: flex;{extra}"><div style="display:flex;--pitch-low:1;padding-bottom:2px;margin-top:2px;margin-right:-2px;padding-right:2px;background:{color}"><div style="width:20.3px;height:24.4px;overflow:hidden;background:#182430">た</div></div><div style="display:flex;--pitch-high:1;padding-top:2px;margin-bottom:2px;margin-right:-2px;padding-left:2px;padding-right:2px;background:{color}"><div style="width:40.6px;height:24.4px;overflow:hidden;background:#182430">のしみ</div></div></div></div>"##
        )
    }

    fn jpdb_like_page(rows: &[String], gap: &str, section_style: &str) -> String {
        format!(
            r#"<!doctype html><html class="dark-mode"><body style="margin:0;background:#182430;color:white"><select id="search-bar-lang"><option value="english" selected>English</option></select><div style="height:1800.4px"></div><div class="subsection-pitch-accent" style="padding:20.5px 0 0 7.3px;width:300px;{section_style}"><h6 class="subsection-label">Pitch accent</h6><div class="subsection"><div style="display:flex;flex-direction:column;gap:{gap}">{}</div></div></div><div style="height:1200px"></div></body></html>"#,
            rows.concat()
        )
    }

    /// Браузерные регрессии захвата запускают Chromium по очереди.
    static BROWSER_CAPTURE_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn browser_session_with(content: &str) -> BrowserSession {
        let runtime = BrowserRuntimeConfig {
            device_metrics: Some(DeviceMetrics::new(1280, 1200, 3.0).unwrap()),
            prefers_color_scheme: Some("dark".into()),
            ..BrowserRuntimeConfig::default()
        };
        let session = BrowserSession::launch(runtime).await.unwrap();
        configure_page(session.page()).await.unwrap();
        session.page().set_content(content).await.unwrap();
        session
    }

    /// Закрытием сессии владеет `browser_runtime` со своими проверками; здесь
    /// предмет проверки — захват, поэтому сбой закрытия только сообщается.
    async fn close_capture_session(session: BrowserSession) {
        if let Err(error) = session.close().await {
            eprintln!("сессия браузера закрыта с ошибкой: {error}");
        }
    }

    /// Страница поиска со структурой разметки JPDB; `<base>` задаёт origin ссылок.
    fn jpdb_like_search_page(
        canonical: Option<&str>,
        results_class: &str,
        rows: &[String],
    ) -> String {
        let canonical = canonical
            .map(|href| format!(r#"<link rel="canonical" href="{href}" />"#))
            .unwrap_or_default();
        let rows = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                format!(
                    r#"<div id="result-{index}"><div class="result vocabulary">{row}</div></div>"#
                )
            })
            .collect::<String>();
        format!(
            r#"<!doctype html><html><head><base href="https://jpdb.io/">{canonical}</head><body><select id="search-bar-lang"><option value="english" selected>English</option></select><div class="{results_class}">{rows}</div></body></html>"#
        )
    }

    fn jpdb_like_search_row(headword: &str, links: &str, alternates: &str) -> String {
        format!(
            r#"<div class="subsection-headword"><div class="primary-spelling"><div class="spelling"><div>{headword}</div></div></div></div><div class="subsection-meanings"><h6 class="subsection-label">Meanings</h6><div class="subsection"><div class="part-of-speech"><div>Noun</div></div><div class="description">1. meaning</div></div></div>{alternates}<div class="hbox">{links}</div>"#
        )
    }

    #[tokio::test]
    #[ignore = "требует установленного Chromium; запустить: cargo test -p asset-store jpdb::tests::browser_search -- --ignored"]
    async fn browser_search_script_reports_canonical_evidence_of_single_entries() {
        let _serial = BROWSER_CAPTURE_TESTS.lock().await;
        let single = jpdb_like_search_page(
            Some("https://jpdb.io/vocabulary/1192150/家族/かぞく?lang=english"),
            "results details",
            &[jpdb_like_search_row(
                r#"<ruby class="v">家<rt>か</rt>族<rt>ぞく</rt></ruby>"#,
                r#"<a class="view-conjugations-link" href="http://[">Broken</a><a class="view-conjugations-link" href="/vocabulary/1192150/家族/used-in">Used in: 5493</a>"#,
                "",
            )],
        );
        let with_alternates = jpdb_like_search_page(
            Some("https://jpdb.io/vocabulary/1358280/食べる/たべる?lang=english"),
            "results details",
            &[jpdb_like_search_row(
                r#"<ruby class="v">食<rt>た</rt></ruby><ruby>べ</ruby><ruby>る</ruby>"#,
                r#"<a class="view-conjugations-link" href="/conjugate/1358280/食べる/たべる">Conjugations...</a><a class="view-conjugations-link" href="/vocabulary/1358280/食べる/used-in">Used in: 6198</a>"#,
                r#"<div class="subsection-other-spellings"><div class="alt-spelling"><a class="plain" href="/vocabulary/1358280/喰べる/たべる?lang=english#a"><ruby>喰<rt>た</rt></ruby><ruby>べ</ruby><ruby>る</ruby></a></div></div>"#,
            )],
        );
        let list = jpdb_like_search_page(
            None,
            "results search",
            &[
                jpdb_like_search_row(
                    r#"<ruby>髪<rt>かみ</rt></ruby>"#,
                    "",
                    r#"<div class="subsection-other-spellings"><div class="alt-spelling"><a class="plain" href="/vocabulary/1203850/かみ?lang=english#a">かみ</a></div></div>"#,
                ),
                jpdb_like_search_row(
                    r#"<ruby>紙<rt>かみ</rt></ruby>"#,
                    "",
                    r#"<div class="subsection-other-spellings"><div class="alt-spelling"><a class="plain" href="/vocabulary/1589150/かみ?lang=english#a">かみ</a></div></div>"#,
                ),
            ],
        );

        let session = browser_session_with(&single).await;
        let snapshot = read_search_candidates(session.page()).await.unwrap();
        let resolved = select_candidates(&snapshot.candidates, &JpdbPitchQuery::new("家族", None));
        session.page().set_content(with_alternates).await.unwrap();
        let alternates = read_search_candidates(session.page()).await.unwrap();
        session.page().set_content(list).await.unwrap();
        let ambiguous = read_search_candidates(session.page()).await.unwrap();
        close_capture_session(session).await;

        assert!(snapshot.details_view);
        assert_eq!(
            snapshot.candidates[0].used_in_urls,
            ["https://jpdb.io/vocabulary/1192150/%E5%AE%B6%E6%97%8F/used-in"]
        );
        let resolved = resolved.unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].vocabulary_id, 1_192_150);
        assert_eq!(
            parse_detail_route(&resolved[0].detail_url).unwrap(),
            crate::pitch_accent::JpdbVocabularyRoute {
                vocabulary_id: 1_192_150,
                surface: "家族".into(),
                reading: Some("かぞく".into()),
            }
        );

        let candidates =
            select_candidates(&alternates.candidates, &JpdbPitchQuery::new("食べる", None))
                .unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(
            parse_detail_route(&candidates[0].detail_url)
                .unwrap()
                .surface,
            "喰べる",
            "ссылка строки остаётся основным источником маршрута"
        );

        assert!(!ambiguous.details_view);
        assert!(ambiguous.canonical_urls.is_empty());
        let candidates =
            select_candidates(&ambiguous.candidates, &JpdbPitchQuery::new("かみ", None)).unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.vocabulary_id)
                .collect::<Vec<_>>(),
            vec![1_203_850, 1_589_150]
        );
    }

    #[tokio::test]
    #[ignore = "требует установленного Chromium; запустить: cargo test -p asset-store jpdb::tests::browser_capture -- --ignored"]
    async fn browser_capture_takes_one_padded_variant_with_overflowing_paint() {
        let _serial = BROWSER_CAPTURE_TESTS.lock().await;
        let colors = ["#d02020", "#20b050", "#3060e0"];
        let rows: Vec<String> = colors
            .iter()
            .map(|color| jpdb_like_row(color, ""))
            .collect();
        let session = browser_session_with(&jpdb_like_page(&rows, "0.5rem", "")).await;
        let page = session.page();

        let inventory = read_variant_page(page, JpdbPitchStage::Capture)
            .await
            .unwrap();
        assert_eq!(inventory.variants.len(), 3);
        assert!(inventory.scroll_y > 0.0);
        for variant in &inventory.variants {
            assert!(
                variant.paint_rect.x + variant.paint_rect.width
                    > variant.document_rect.x + variant.document_rect.width + 1.0,
                "сегмент с отрицательным полем должен выходить за узел: {variant:?}"
            );
        }
        let markup = inventory.markup_sha256();
        // Разметка вариантов различается только цветом, поэтому выбор не обязан быть первым.
        let selected = select_pitch_accent_variant(1, markup.iter().map(String::as_str)).unwrap();
        let snapshot = capture_snapshot(page, JpdbPitchStage::Capture, selected)
            .await
            .unwrap();
        assert_eq!(snapshot.page.markup_sha256(), markup);
        assert_eq!(snapshot.capture_rect.x.fract(), 0.0);
        assert_eq!(snapshot.capture_rect.y.fract(), 0.0);

        let clip = Viewport::builder()
            .x(snapshot.capture_rect.x)
            .y(snapshot.capture_rect.y)
            .width(snapshot.capture_rect.width)
            .height(snapshot.capture_rect.height)
            .scale(1.0)
            .build()
            .unwrap();
        let bytes = page
            .screenshot(
                CaptureScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .clip(clip)
                    .capture_beyond_viewport(true)
                    .build(),
            )
            .await
            .unwrap();
        let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png).unwrap();
        assert_eq!(
            (image.width(), image.height()),
            (
                (snapshot.capture_rect.width * 3.0) as u32,
                (snapshot.capture_rect.height * 3.0) as u32
            ),
            "целочисленный clip при DSF 3 даёт PNG точного размера"
        );

        let selected_paint = snapshot.page.variants[selected].paint_rect;
        let render = PitchAccentRenderEvidence {
            kind: PitchAccentRenderKind::BrowserRegionScreenshot,
            selector: snapshot.page.variants[selected].selector.clone(),
            captured_graph_count: 1,
            coordinate_space: PitchAccentCoordinateSpace::Document,
            viewport_width: snapshot.page.viewport_width,
            viewport_height: snapshot.page.viewport_height,
            document_width: snapshot.page.document_width,
            document_height: snapshot.page.document_height,
            scroll_x: snapshot.page.scroll_x,
            scroll_y: snapshot.page.scroll_y,
            pixel_width: image.width(),
            pixel_height: image.height(),
            device_scale_factor: 3.0,
            page_scale_factor: 1.0,
            dark_theme: snapshot.dark_theme.clone(),
            padding_css_px: PITCH_ACCENT_CAPTURE_PADDING_CSS_PX,
            capture_rect: snapshot.capture_rect,
        };
        assert!(
            validate_evidence_capture_margins(&render, selected_paint, &image).is_ok(),
            "все поля фактического PNG — подлинный фон"
        );
        let to_rgb = |hex: &str| {
            let value = u32::from_str_radix(&hex[1..], 16).unwrap();
            [(value >> 16) as u8, (value >> 8) as u8, value as u8]
        };
        let present: Vec<bool> = colors
            .iter()
            .map(|color| {
                let rgb = to_rgb(color);
                image.pixels().any(|(_, _, pixel)| pixel.0[..3] == rgb)
            })
            .collect();
        assert_eq!(
            present,
            (0..3).map(|index| index == selected).collect::<Vec<_>>(),
            "в PNG только выбранный вариант"
        );
        let overflow_x = ((selected_paint.x + selected_paint.width - 1.0 - snapshot.capture_rect.x)
            * 3.0) as u32;
        let overflow_y = ((selected_paint.y + selected_paint.height / 2.0
            - snapshot.capture_rect.y)
            * 3.0) as u32;
        assert_eq!(
            image.get_pixel(overflow_x, overflow_y).0[..3],
            to_rgb(colors[selected]),
            "выступающий за узел сегмент присутствует в PNG"
        );
        close_capture_session(session).await;
    }

    #[tokio::test]
    #[ignore = "требует установленного Chromium; запустить: cargo test -p asset-store jpdb::tests::browser_capture -- --ignored"]
    async fn browser_capture_refuses_unsafe_regions_with_typed_failures() {
        let _serial = BROWSER_CAPTURE_TESTS.lock().await;
        let two = |gap: &str, section_style: &str, extra: &str| {
            jpdb_like_page(
                &[
                    jpdb_like_row("#d02020", extra),
                    jpdb_like_row("#20b050", extra),
                ],
                gap,
                section_style,
            )
        };
        let cases = [
            (
                two("1px", "", "").replace("height:42px", "height:10px"),
                "соседнего варианта",
            ),
            (
                two("0.5rem", "overflow:hidden;max-width:70px;", ""),
                "обрезает",
            ),
            (
                // За левой границей документа нет предка с подтверждаемым фоном:
                // скрипт отказывает раньше проверки границ на стороне Rust.
                two("0.5rem", "position:absolute;left:-6.3px;top:1800px;", "")
                    .replace(AUDIO_BUTTON, ""),
                "покрывающий всю область снимка с полями",
            ),
        ];
        for (content, expected) in cases {
            let session = browser_session_with(&content).await;
            let error = async {
                let inventory = read_variant_page(session.page(), JpdbPitchStage::Capture).await?;
                for index in 0..inventory.variants.len() {
                    capture_snapshot(session.page(), JpdbPitchStage::Capture, index).await?;
                }
                Ok::<_, JpdbPitchFailure>(())
            }
            .await
            .expect_err(expected);
            let message = match &error {
                JpdbPitchFailure::CaptureContract { message, .. }
                | JpdbPitchFailure::DarkThemeUnverified { message, .. } => message.clone(),
                other => panic!("ожидался типизированный отказ «{expected}», получено {other:?}"),
            };
            assert!(message.contains(expected), "{expected}: {message}");
            close_capture_session(session).await;
        }

        let unknown = jpdb_like_page(
            &[jpdb_like_row("#d02020", "")
                .replace("</a>", "</a><span class=\"pitch-frequency\">72%</span>")],
            "0.5rem",
            "",
        );
        let session = browser_session_with(&unknown).await;
        let error = read_variant_page(session.page(), JpdbPitchStage::Capture)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, JpdbPitchFailure::CaptureContract { message, .. } if message.contains("нераспознанный элемент")),
            "неизвестные сведения о варианте не игнорируются: {error:?}"
        );
        close_capture_session(session).await;

        let translucent = two("0.5rem", "", "").replace(
            "<body style=\"margin:0;background:#182430;",
            "<body style=\"margin:0;background:rgba(24,36,48,0.5);",
        );
        let session = browser_session_with(&translucent).await;
        let error = capture_snapshot(session.page(), JpdbPitchStage::Capture, 0)
            .await
            .unwrap_err();
        assert!(
            matches!(error, JpdbPitchFailure::DarkThemeUnverified { .. }),
            "без непрозрачного фона снимок не готовится: {error:?}"
        );
        close_capture_session(session).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PitchDomState {
    Absent,
    Present { graph_count: usize },
}

fn classify_pitch_state(detail: &DetailSnapshot) -> Result<PitchDomState, JpdbPitchFailure> {
    tracing::debug!(
        stage = ?JpdbPitchStage::PitchInspection,
        route = %safe_route(&detail.url),
        base_page_contract_valid = detail.base_page_contract_valid,
        pitch_section_present = detail.pitch_section_present,
        pitch_label_valid = detail.pitch_label.as_deref() == Some("Pitch accent"),
        pitch_marker_count = detail.pitch_marker_count,
        graph_count = detail.graph_count,
        section_count = detail.section_inventory.len(),
        "Проверка присутствия графиков акцентуации JPDB"
    );
    if detail.vocabulary_id == 0
        || detail.forms.is_empty()
        || detail.part_of_speech.is_empty()
        || !detail.base_page_contract_valid
        || !detail
            .section_inventory
            .iter()
            .any(|label| label == "Meanings")
        || parse_detail_route(&detail.url)
            .is_none_or(|route| route.vocabulary_id != detail.vocabulary_id)
        || detail
            .part_of_speech
            .iter()
            .any(|pos| pos.trim().eq_ignore_ascii_case("name"))
    {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::DetailVerification,
            message: "идентичность записи и базовый контракт словарной страницы не подтверждены"
                .into(),
        });
    }
    if !detail.pitch_section_present {
        if detail.pitch_marker_count != 0 {
            return Err(JpdbPitchFailure::PageContract {
                stage: JpdbPitchStage::PitchInspection,
                message: "текст о графике акцентуации есть вне распознанного контейнера графиков"
                    .into(),
            });
        }
        return Ok(PitchDomState::Absent);
    }
    if detail.pitch_label.as_deref() != Some("Pitch accent")
        || detail.pitch_marker_count != 1
        || detail.graph_count == 0
    {
        return Err(JpdbPitchFailure::PageContract {
            stage: JpdbPitchStage::PitchInspection,
            message: "секция акцентуации найдена, но её точная метка или DOM графиков отсутствует"
                .into(),
        });
    }
    Ok(PitchDomState::Present {
        graph_count: detail.graph_count,
    })
}
