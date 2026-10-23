//! Общий локальный inference для текстовых задач и задач с изображением.
//!
//! Предметная задача (`kanji_ocr`, `jpdb_pitch_review`) готовит изображение,
//! задаёт контракт ответа (инструкцию формата, JSON Schema и нужен ли вход с
//! изображением) и сама проверяет ответ. Этот модуль отвечает только за транспорт и исполнение:
//!
//! - `llama_server` — OpenAI-совместимый `/v1/chat/completions` собственного
//!   (`managed`, см. `llama_server`) или внешнего (`external`) `llama-server`;
//! - `ollama` — собственный API Ollama `/api/chat`.
//!
//! Протоколы не имитируют друг друга: изображение, параметры генерации,
//! сведения о модели, структура ответа и ошибки разбираются для каждого
//! backend отдельно. Перед первым запросом подтверждаются идентичность
//! модели и, для задач с изображением, наличие vision; перед и после каждого
//! запроса — что модель не сменилась. Клиент HTTP — без proxy и redirect, ответы ограничены по размеру.
//!
//! Провенанс (`InferenceProvenance`) — идентичность результата: задача,
//! контракт, backend и его версия, файл весов и projector по SHA-256 (или
//! digest Ollama), промпт по SHA-256, преобразование изображения и параметры
//! генерации. Параметры, влияющие только на скорость (размещение слоёв, число
//! потоков, batch), в идентичность не входят и записываются в сведениях о
//! запуске. Сохранённый результат переиспользуется только при проверяемой
//! идентичности весов и projector (`InferenceProvenance::reusable`): внешний
//! сервер, сообщивший лишь путь, или его неизвестный projector дают новый запрос.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, Instant, UNIX_EPOCH};

use base64::Engine as _;
use futures::FutureExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::inference_config::{
    LlamaMode, ResolvedBackend, ResolvedModel, ResolvedTask, StructuredOutput, TaskId, is_loopback,
};
use crate::llama_server::{LaunchPlan, ManagedServer, OrphanStop, ServerReady, StopReport};

pub(crate) const PROVENANCE_SCHEMA: u32 = 1;
/// Машинный маркер недоказанной остановки собственного сервера.
pub(crate) const STOP_UNPROVEN_BLOCKER: &str = "inference_server_stop_unproven";
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_CONTENT_BYTES: usize = 64 * 1024;
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// Типизированный отказ inference. `code` — стабильная машинная строка.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct InferenceFailure {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Box<StopSummary>>,
    /// Аренды прежних запусков, из-за которых запуск отклонён.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub orphans: Vec<OrphanStop>,
}

/// Сводка остановки сервера после отказа запуска.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct StopSummary {
    pub method: String,
    pub proven: bool,
}

impl InferenceFailure {
    pub(crate) fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            stop: None,
            orphans: Vec::new(),
        }
    }

    pub(crate) fn interrupted() -> Self {
        Self::new("interrupted", "операция прервана сигналом")
    }

    pub(crate) fn with_stop(mut self, report: StopReport) -> Self {
        self.stop = Some(Box::new(StopSummary {
            proven: report.proven(),
            method: report.method,
        }));
        self
    }

    pub(crate) fn is_interrupted(&self) -> bool {
        self.code == "interrupted"
    }

    /// Сервер был запущен, но его остановка после отказа не доказана.
    pub(crate) fn stop_unproven(&self) -> bool {
        self.stop.as_ref().is_some_and(|stop| !stop.proven)
    }

    pub(crate) fn into_asset_error(self) -> AssetError {
        let code = if self.is_interrupted() {
            ErrorCode::Interrupted
        } else {
            ErrorCode::InferenceUnavailable
        };
        let stop_unproven = self.stop_unproven();
        let mut details = json!({"reason": self.code, "stop": self.stop});
        if !self.orphans.is_empty() {
            details["orphans"] = json!(self.orphans);
        }
        if stop_unproven {
            details["stop_unproven"] = json!(true);
            details["blockers"] = json!([STOP_UNPROVEN_BLOCKER]);
        }
        AssetError::with_details(code, format!("{}: {}", self.code, self.message), details)
    }
}

type SignalWait = futures::future::Fuse<Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>>;

/// Отказ подготовки сессии вместе с отчётом о ней.
pub(crate) type PrepareError = Box<(InferenceFailure, SessionReport)>;

/// Ожидание сигнала завершения на время владения ресурсами inference.
/// Пока область жива, `SIGINT`/`SIGTERM`/`SIGHUP` не завершают процесс сразу:
/// владелец сначала останавливает свой сервер.
pub(crate) struct Cancellation {
    _scope: Option<crate::acquisition_pool::TerminationScope>,
    signal: Option<SignalWait>,
    fired: bool,
}

impl Cancellation {
    /// Перехватывает сигналы завершения процесса.
    pub(crate) fn capture() -> Self {
        let (scope, wait) = crate::acquisition_pool::TerminationScope::enter();
        let boxed: Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>> = Box::pin(wait);
        Self {
            _scope: Some(scope),
            signal: Some(boxed.fuse()),
            fired: false,
        }
    }

    /// Без перехвата: стандартная обработка сигналов процесса.
    pub(crate) fn none() -> Self {
        Self {
            _scope: None,
            signal: None,
            fired: false,
        }
    }

    pub(crate) fn interrupted(&mut self) -> bool {
        if !self.fired
            && let Some(signal) = &mut self.signal
            && let Some(result) = signal.now_or_never()
        {
            self.fired = result.is_ok();
        }
        self.fired
    }

    pub(crate) async fn wait(&mut self) {
        if self.fired {
            return;
        }
        match &mut self.signal {
            Some(signal) => {
                if signal.await.is_ok() {
                    self.fired = true;
                } else {
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending::<()>().await,
        }
    }
}

/// Контракт ответа задачи: инструкция формата и схема принадлежат коду.
#[derive(Debug, Clone)]
pub(crate) struct ResponseContract {
    /// Имя и версия контракта, входят в идентичность результата.
    pub id: &'static str,
    /// Добавляется после пользовательского промпта.
    pub instruction: &'static str,
    pub schema: Value,
    /// Каждый запрос несёт изображение: vision подтверждается до запуска
    /// задачи. Текстовая задача vision не требует и изображение не передаёт.
    pub requires_image: bool,
}

/// Итоговый текст запроса: промпт пользователя и инструкция формата из кода.
pub(crate) fn compose_prompt(prompt: &str, contract: &ResponseContract) -> String {
    format!("{}\n{}", prompt.trim_end(), contract.instruction)
}

/// Идентичность backend в провенансе.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BackendIdentity {
    /// `llama_server` или `ollama`.
    pub kind: String,
    /// `managed`, `external` или `service` (Ollama).
    pub mode: String,
    /// Версия, сообщённая самим backend, если её удалось получить.
    pub version: Option<String>,
    /// `loopback` или `remote:<host>`.
    pub scope: String,
}

/// Идентичность модели в провенансе.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelIdentity {
    pub profile: String,
    /// Путь к GGUF или имя модели Ollama.
    pub name: String,
    /// `sha256:<hex>` файла весов, `ollama-digest:<digest>` либо
    /// `reported-path:<путь>`, если файл внешнего сервера недоступен для хеша.
    pub weights: String,
    /// `sha256:<hex>` отдельного projector, если он используется;
    /// `unverified:external` — внешний сервер может использовать projector,
    /// который API не сообщает.
    pub projector: Option<String>,
    /// `projector`, `integrated`, `none` или `backend_reported`.
    pub vision: String,
    /// Параметры загрузки, влияющие на ответ (контекст, рассуждения).
    pub load: BTreeMap<String, String>,
}

/// Провенанс результата inference; сериализация входит в ключ кэша.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InferenceProvenance {
    pub inference_schema: u32,
    pub task: TaskId,
    pub contract: String,
    pub backend: BackendIdentity,
    pub model: ModelIdentity,
    pub prompt_source: String,
    pub prompt_sha256: String,
    pub transform_version: String,
    /// Параметры генерации в каноническом текстовом виде.
    pub generation: BTreeMap<String, String>,
}

impl InferenceProvenance {
    /// Провенанс, когда подготовить backend не удалось: такая запись никогда не
    /// становится попаданием кэша и не выдаётся за результат конкретной модели.
    pub(crate) fn unavailable(
        task: &ResolvedTask,
        contract: &ResponseContract,
        transform: &str,
    ) -> Self {
        let (kind, mode) = backend_kind_mode(&task.backend);
        Self {
            inference_schema: PROVENANCE_SCHEMA,
            task: task.task,
            contract: contract.id.to_owned(),
            backend: BackendIdentity {
                kind: kind.into(),
                mode: mode.into(),
                version: None,
                scope: backend_scope(&task.backend),
            },
            model: ModelIdentity {
                profile: task.model_profile().to_owned(),
                name: model_name(&task.model),
                weights: "unavailable".into(),
                projector: None,
                vision: "unavailable".into(),
                load: BTreeMap::new(),
            },
            prompt_source: task.prompt.source.clone(),
            prompt_sha256: sha256_hex(compose_prompt(&task.prompt.text, contract).as_bytes()),
            transform_version: transform.to_owned(),
            generation: generation_parameters(task),
        }
    }
    /// Можно ли выдать сохранённый результат этой идентичности без запроса.
    /// Нужны проверяемые веса (SHA-256 файла или digest Ollama) и projector,
    /// который либо не используется, либо тоже проверен по SHA-256. Путь,
    /// лишь сообщённый внешним сервером, и его неизвестный projector не
    /// доказывают, что байты модели не сменились между запусками.
    pub(crate) fn reusable(&self) -> bool {
        (self.model.weights.starts_with("sha256:")
            || self.model.weights.starts_with("ollama-digest:"))
            && self
                .model
                .projector
                .as_deref()
                .is_none_or(|projector| projector.starts_with("sha256:"))
    }

    pub(crate) fn summary(&self) -> String {
        format!(
            "{} {} {}; модель {} ({}); prompt sha256 {}; {}",
            self.backend.kind,
            self.backend.mode,
            self.backend
                .version
                .as_deref()
                .unwrap_or("версия неизвестна"),
            self.model.name,
            short(&self.model.weights),
            short(&self.prompt_sha256),
            self.transform_version
        )
    }
}

/// Сокращает значение до 16 символов (не байтов): путь модели может быть не ASCII.
fn short(value: &str) -> String {
    let prefix =
        |text: &str| (text.chars().count() > 16).then(|| text.chars().take(16).collect::<String>());
    match value.split_once(':') {
        Some((kind, hash)) => match prefix(hash) {
            Some(head) => format!("{kind}:{head}…"),
            None => value.to_owned(),
        },
        None => prefix(value).map_or_else(|| value.to_owned(), |head| format!("{head}…")),
    }
}

fn backend_kind_mode(backend: &ResolvedBackend) -> (&'static str, &'static str) {
    match backend {
        ResolvedBackend::LlamaServer(llama) => (
            "llama_server",
            match llama.mode {
                LlamaMode::Managed => "managed",
                LlamaMode::External => "external",
            },
        ),
        ResolvedBackend::Ollama(_) => ("ollama", "service"),
    }
}

fn backend_scope(backend: &ResolvedBackend) -> String {
    let url = match backend {
        ResolvedBackend::LlamaServer(llama) if llama.mode == LlamaMode::Managed => {
            return "loopback".into();
        }
        ResolvedBackend::LlamaServer(llama) => llama.endpoint.clone(),
        ResolvedBackend::Ollama(ollama) => Some(ollama.endpoint.clone()),
    };
    match url {
        Some(url) if is_loopback(&url) => "loopback".into(),
        Some(url) => format!("remote:{}", url.host_str().unwrap_or("?")),
        None => "unconfigured".into(),
    }
}

fn model_name(model: &ResolvedModel) -> String {
    match model {
        ResolvedModel::LlamaServer(llama) => llama
            .model
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        ResolvedModel::Ollama(ollama) => ollama.model.clone().unwrap_or_default(),
    }
}

fn generation_parameters(task: &ResolvedTask) -> BTreeMap<String, String> {
    let generation = &task.generation;
    let mut parameters = BTreeMap::from([
        (
            "temperature".to_owned(),
            format!("{:?}", generation.temperature),
        ),
        (
            "max_output_tokens".to_owned(),
            generation.max_output_tokens.to_string(),
        ),
        (
            "structured_output".to_owned(),
            match generation.structured_output {
                StructuredOutput::JsonSchema => "json_schema",
                StructuredOutput::None => "none",
            }
            .to_owned(),
        ),
    ]);
    if let Some(seed) = generation.seed {
        parameters.insert("seed".into(), seed.to_string());
    }
    parameters
}

/// Сведения об одном выполненном запросе (не входят в ключ кэша).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunTimings {
    pub elapsed_ms: u64,
    pub prompt_ms: Option<u64>,
    pub generation_ms: Option<u64>,
    pub load_ms: Option<u64>,
    pub prompt_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

/// Ответ модели до предметной проверки.
#[derive(Debug, Clone)]
pub(crate) struct ModelResponse {
    pub content: String,
    pub timings: RunTimings,
}

/// Добавляет к ошибке обхода отчёт завершённой сессии и признак недоказанной
/// остановки сервера, сохраняя код и сообщение исходной ошибки.
pub(crate) fn with_session_evidence(
    mut error: AssetError,
    stop_unproven: bool,
    session: Option<&SessionReport>,
) -> AssetError {
    let Some(session) = session else {
        return error;
    };
    let mut details = match std::mem::take(&mut error.details) {
        Value::Object(details) => details,
        Value::Null => serde_json::Map::new(),
        other => serde_json::Map::from_iter([("cause".to_owned(), other)]),
    };
    details
        .entry("inference")
        .or_insert_with(|| serde_json::to_value(session).unwrap_or(Value::Null));
    details
        .entry("stop_unproven")
        .or_insert(Value::Bool(stop_unproven));
    if stop_unproven {
        let blockers = details
            .entry("blockers")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(blockers) = blockers
            && !blockers
                .iter()
                .any(|blocker| blocker == STOP_UNPROVEN_BLOCKER)
        {
            blockers.push(json!(STOP_UNPROVEN_BLOCKER));
        }
    }
    error.details = Value::Object(details);
    error
}

/// Ошибка прерывания сигналом обхода задачи: код `interrupted` (выход `130`)
/// сохраняет и число записанных результатов, и сведения об остановке сервера,
/// включая `stop_unproven` и блокер `inference_server_stop_unproven`.
pub(crate) fn interrupted_error(
    message: &str,
    saved: usize,
    stop_unproven: bool,
    session: Option<&SessionReport>,
) -> AssetError {
    with_session_evidence(
        AssetError::with_details(ErrorCode::Interrupted, message, json!({"saved": saved})),
        stop_unproven,
        Some(session.unwrap_or(&SessionReport::default())),
    )
}

/// Сведения о сессии для отчёта команды.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct SessionReport {
    pub task: Option<TaskId>,
    pub backend: Option<String>,
    pub model_profile: Option<String>,
    pub prepare_ms: u64,
    pub requests: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<ServerReady>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<InferenceFailure>,
}

enum Transport {
    Ollama {
        base: Url,
        model: String,
        digest: String,
        keep_alive_seconds: u64,
        context_size: u32,
    },
    Llama {
        base: Url,
        alias: String,
        api_key: Option<String>,
        managed: Option<Box<ManagedServer>>,
        props: crate::llama_server::ServerProps,
    },
}

/// Подготовленная сессия одной задачи: backend подтверждён, модель загружена.
pub(crate) struct InferenceSession {
    task: ResolvedTask,
    prompt: String,
    contract: ResponseContract,
    client: reqwest::Client,
    transport: Transport,
    provenance: InferenceProvenance,
    cancellation: Cancellation,
    report: SessionReport,
}

pub(crate) fn http_client(connect_timeout: Duration) -> Result<reqwest::Client, InferenceFailure> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout)
        .build()
        .map_err(|error| InferenceFailure::new("http_client_failed", error.to_string()))
}

fn network_failure(prefix: &str, error: &reqwest::Error) -> InferenceFailure {
    if error.is_timeout() {
        InferenceFailure::new(format!("{prefix}_timeout"), "истекло время ожидания ответа")
    } else {
        InferenceFailure::new(format!("{prefix}_network_error"), "сервер недоступен")
    }
}

async fn read_bounded(
    prefix: &str,
    mut response: reqwest::Response,
) -> Result<Vec<u8>, InferenceFailure> {
    if response
        .content_length()
        .is_some_and(|size| size > MAX_RESPONSE_BYTES as u64)
    {
        return Err(InferenceFailure::new(
            "response_too_large",
            "ответ превышает предел",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| network_failure(prefix, &error))?
    {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(InferenceFailure::new(
                "response_too_large",
                "ответ превышает предел",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn send_json(
    prefix: &str,
    request: reqwest::RequestBuilder,
) -> Result<(u16, Vec<u8>), InferenceFailure> {
    let response = request
        .send()
        .await
        .map_err(|error| network_failure(prefix, &error))?;
    let status = response.status().as_u16();
    let bytes = read_bounded(prefix, response).await?;
    Ok((status, bytes))
}

fn http_failure(prefix: &str, status: u16, body: &[u8]) -> InferenceFailure {
    let message = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value["error"]["message"]
                .as_str()
                .or_else(|| value["error"].as_str())
                .map(str::to_owned)
        })
        .unwrap_or_default();
    let message: String = crate::diagnostics::safe_message(&message)
        .chars()
        .filter(|character| !character.is_control())
        .take(300)
        .collect();
    let lower = message.to_ascii_lowercase();
    // Отказ грамматики/схемы — отдельный код: его исправляет настройка
    // `structured_output = "none"`, а не повтор.
    if status == 400
        && (lower.contains("grammar") || lower.contains("sampler") || lower.contains("schema"))
    {
        return InferenceFailure::new("structured_output_rejected", message);
    }
    if status == 400 && (lower.contains("context") || lower.contains("exceed")) {
        return InferenceFailure::new("context_exceeded", message);
    }
    InferenceFailure::new(format!("{prefix}_http_{status}"), message)
}

/// GET JSON с ограничением размера; ошибки HTTP типизированы.
pub(crate) async fn get_json(
    client: &reqwest::Client,
    base: &Url,
    path: &str,
    api_key: Option<&str>,
    timeout: Duration,
    prefix: &str,
) -> Result<Value, InferenceFailure> {
    let url = base
        .join(path)
        .map_err(|error| InferenceFailure::new("invalid_url", error.to_string()))?;
    let mut request = client.get(url).timeout(timeout);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let (status, bytes) = send_json(prefix, request).await?;
    if !(200..300).contains(&status) {
        return Err(http_failure(prefix, status, &bytes));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| InferenceFailure::new("invalid_response_json", "ответ не JSON"))
}

async fn post_json(
    client: &reqwest::Client,
    base: &Url,
    path: &str,
    api_key: Option<&str>,
    timeout: Duration,
    prefix: &str,
    body: &Value,
) -> Result<Value, InferenceFailure> {
    let url = base
        .join(path)
        .map_err(|error| InferenceFailure::new("invalid_url", error.to_string()))?;
    let mut request = client
        .post(url)
        .timeout(timeout)
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_vec(body)
                .map_err(|error| InferenceFailure::new("invalid_request", error.to_string()))?,
        );
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let (status, bytes) = send_json(prefix, request).await?;
    if !(200..300).contains(&status) {
        return Err(http_failure(prefix, status, &bytes));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| InferenceFailure::new("invalid_response_json", "ответ не JSON"))
}

/// Память SHA-256 файлов весов: хеш многогигабайтного GGUF считается заново
/// только при изменении размера или времени изменения файла.
fn weights_sha256(path: &Path) -> Result<String, InferenceFailure> {
    let metadata = std::fs::metadata(path).map_err(|_| {
        InferenceFailure::new(
            "model_file_missing",
            format!("файл {} не найден", path.display()),
        )
    })?;
    if !metadata.is_file() {
        return Err(InferenceFailure::new(
            "model_file_missing",
            format!("{} не является файлом", path.display()),
        ));
    }
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let memo_key = format!(
        "{}\u{1f}{}\u{1f}{modified}",
        canonical.display(),
        metadata.len()
    );
    let memo_path = crate::llama_server::state_directory()
        .map(|directory| directory.join("weights-sha256.json"));
    let mut memo: BTreeMap<String, String> = memo_path
        .as_ref()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if let Some(hash) = memo.get(&memo_key) {
        return Ok(hash.clone());
    }
    let mut file = std::fs::File::open(path).map_err(|error| {
        InferenceFailure::new(
            "model_file_unreadable",
            crate::diagnostics::safe_message(&error.to_string()),
        )
    })?;
    let hash = crate::hashing::sha256_reader(&mut file).map_err(|error| {
        InferenceFailure::new(
            "model_file_unreadable",
            crate::diagnostics::safe_message(&error.to_string()),
        )
    })?;
    if let Some(memo_path) = memo_path {
        memo.retain(|key, _| !key.starts_with(&format!("{}\u{1f}", canonical.display())));
        memo.insert(memo_key, hash.clone());
        if let Some(parent) = memo_path.parent()
            && std::fs::create_dir_all(parent).is_ok()
            && let Ok(bytes) = serde_json::to_vec(&memo)
        {
            let temporary = memo_path.with_extension(format!("tmp{}", std::process::id()));
            if std::fs::write(&temporary, bytes).is_ok() {
                let _ = std::fs::rename(&temporary, &memo_path);
            }
        }
    }
    Ok(hash)
}

impl InferenceSession {
    /// Подготавливает backend и подтверждает идентичность модели.
    pub(crate) async fn prepare(
        task: ResolvedTask,
        contract: ResponseContract,
        transform_version: &str,
    ) -> Result<Self, PrepareError> {
        let started = Instant::now();
        let mut report = SessionReport {
            task: Some(task.task),
            backend: Some(task.backend_name().to_owned()),
            model_profile: Some(task.model_profile().to_owned()),
            ..SessionReport::default()
        };
        let prompt = compose_prompt(&task.prompt.text, &contract);
        let result = Self::connect(&task, contract.requires_image, &mut report).await;
        report.prepare_ms = started.elapsed().as_millis() as u64;
        let (transport, cancellation, backend, model, client) = match result {
            Ok(prepared) => prepared,
            Err(failure) => {
                report.failure = Some(failure.clone());
                return Err(Box::new((failure, report)));
            }
        };
        let provenance = InferenceProvenance {
            inference_schema: PROVENANCE_SCHEMA,
            task: task.task,
            contract: contract.id.to_owned(),
            backend,
            model,
            prompt_source: task.prompt.source.clone(),
            prompt_sha256: sha256_hex(prompt.as_bytes()),
            transform_version: transform_version.to_owned(),
            generation: generation_parameters(&task),
        };
        Ok(Self {
            task,
            prompt,
            contract,
            client,
            transport,
            provenance,
            cancellation,
            report,
        })
    }

    #[allow(clippy::type_complexity)]
    async fn connect(
        task: &ResolvedTask,
        requires_image: bool,
        report: &mut SessionReport,
    ) -> Result<
        (
            Transport,
            Cancellation,
            BackendIdentity,
            ModelIdentity,
            reqwest::Client,
        ),
        InferenceFailure,
    > {
        task.require_ready().map_err(|error| {
            let reason = error.details["reason"]
                .as_str()
                .unwrap_or("model_not_configured")
                .to_owned();
            InferenceFailure::new(reason, error.message)
        })?;
        let (kind, mode) = backend_kind_mode(&task.backend);
        let scope = backend_scope(&task.backend);
        match (&task.backend, &task.model) {
            (ResolvedBackend::Ollama(backend), ResolvedModel::Ollama(model)) => {
                let client = http_client(Duration::from_secs(backend.connect_timeout_seconds))?;
                let timeout = Duration::from_secs(task.generation.timeout_seconds);
                let name = model.model.clone().expect("readiness проверена");
                let version = get_json(
                    &client,
                    &backend.endpoint,
                    "/api/version",
                    None,
                    timeout,
                    "ollama",
                )
                .await
                .ok()
                .and_then(|value| value["version"].as_str().map(str::to_owned));
                let digest = ollama_digest(&client, &backend.endpoint, &name, timeout).await?;
                let show = post_json(
                    &client,
                    &backend.endpoint,
                    "/api/show",
                    None,
                    timeout,
                    "ollama",
                    &json!({"model": name}),
                )
                .await?;
                if requires_image
                    && !show["capabilities"]
                        .as_array()
                        .is_some_and(|caps| caps.iter().any(|cap| cap == "vision"))
                {
                    return Err(InferenceFailure::new(
                        "model_does_not_support_vision",
                        "Ollama не сообщает vision для модели",
                    ));
                }
                // Облачные модели Ollama не являются локальным provider.
                if show
                    .get("remote_host")
                    .is_some_and(|value| !value.is_null() && value != "")
                {
                    return Err(InferenceFailure::new(
                        "cloud_model_not_allowed",
                        "модель Ollama исполняется удалённо",
                    ));
                }
                let load =
                    BTreeMap::from([("context_size".to_owned(), model.context_size.to_string())]);
                Ok((
                    Transport::Ollama {
                        base: backend.endpoint.clone(),
                        model: name.clone(),
                        digest: digest.clone(),
                        keep_alive_seconds: model.keep_alive_seconds,
                        context_size: model.context_size,
                    },
                    Cancellation::none(),
                    BackendIdentity {
                        kind: kind.into(),
                        mode: mode.into(),
                        version,
                        scope,
                    },
                    ModelIdentity {
                        profile: model.profile.clone(),
                        name,
                        weights: format!("ollama-digest:{digest}"),
                        projector: None,
                        vision: "backend_reported".into(),
                        load,
                    },
                    client,
                ))
            }
            (ResolvedBackend::LlamaServer(backend), ResolvedModel::LlamaServer(model)) => {
                let client = http_client(Duration::from_secs(backend.connect_timeout_seconds))?;
                let connect = Duration::from_secs(backend.connect_timeout_seconds);
                let mut load = BTreeMap::from([
                    ("context_size".to_owned(), model.context_size.to_string()),
                    ("reasoning".to_owned(), model.reasoning.as_str().to_owned()),
                ]);
                match backend.mode {
                    LlamaMode::Managed => {
                        let mut plan = LaunchPlan::new(backend, model)?;
                        plan.require_vision = requires_image;
                        let weights = format!(
                            "sha256:{}",
                            weights_sha256(model.model.as_ref().expect("readiness проверена"))?
                        );
                        let projector = match &model.projector {
                            Some(path) => Some(format!(
                                "sha256:{}",
                                weights_sha256(path).map_err(|failure| InferenceFailure::new(
                                    failure.code.replace("model_", "projector_"),
                                    failure.message,
                                ))?
                            )),
                            None => None,
                        };
                        let mut cancellation = Cancellation::capture();
                        let server =
                            ManagedServer::start(&plan, &client, &mut cancellation).await?;
                        report.server = Some(server.ready.clone());
                        load.insert(
                            "platform".into(),
                            if backend.windows_interop {
                                "windows_interop"
                            } else {
                                "native"
                            }
                            .into(),
                        );
                        let props = crate::llama_server::ServerProps {
                            alias: Some(server.alias().to_owned()),
                            model_path: server.ready.model_path.clone(),
                            build_info: server.ready.build_info.clone(),
                            vision: server.ready.vision,
                            context_size: server.ready.context_size,
                        };
                        Ok((
                            Transport::Llama {
                                base: server.base_url(),
                                alias: server.alias().to_owned(),
                                api_key: Some(server.api_key().to_owned()),
                                props: props.clone(),
                                managed: Some(Box::new(server)),
                            },
                            cancellation,
                            BackendIdentity {
                                kind: kind.into(),
                                mode: mode.into(),
                                version: props.build_info.clone(),
                                scope,
                            },
                            ModelIdentity {
                                profile: model.profile.clone(),
                                name: model
                                    .model
                                    .as_ref()
                                    .expect("проверено")
                                    .display()
                                    .to_string(),
                                weights,
                                projector,
                                vision: match model.vision {
                                    crate::inference_config::VisionMode::Projector => "projector",
                                    crate::inference_config::VisionMode::Integrated => "integrated",
                                    crate::inference_config::VisionMode::None => "none",
                                }
                                .into(),
                                load,
                            },
                            client,
                        ))
                    }
                    LlamaMode::External => {
                        let base = backend.endpoint.clone().expect("readiness проверена");
                        let health = client
                            .get(base.join("/health").expect("путь корректен"))
                            .timeout(connect)
                            .send()
                            .await
                            .map_err(|error| network_failure("llama", &error))?;
                        if !health.status().is_success() {
                            return Err(InferenceFailure::new(
                                "server_not_ready",
                                format!("/health вернул {}", health.status().as_u16()),
                            ));
                        }
                        let props =
                            crate::llama_server::server_props(&client, &base, None, connect)
                                .await?;
                        if requires_image && !props.vision {
                            return Err(InferenceFailure::new(
                                "model_does_not_support_vision",
                                "внешний сервер не подтвердил vision",
                            ));
                        }
                        // Ожидаемая модель, если задана, сверяется с тем, что сервер
                        // действительно загрузил; её SHA считается по локальному файлу
                        // (большой GGUF хешируется один раз на путь, размер и mtime).
                        // Без неё известен лишь путь: такой результат не переиспользуется.
                        let (name, weights) = match &model.model {
                            Some(path) => {
                                let expected = if backend.windows_interop {
                                    crate::inference_config::to_windows_path(path)
                                } else {
                                    path.to_str().map(str::to_owned)
                                };
                                if expected.as_deref() != Some(props.model_path.as_str()) {
                                    return Err(InferenceFailure::new(
                                        "model_mismatch",
                                        format!("внешний сервер загрузил {}", props.model_path),
                                    ));
                                }
                                (
                                    path.display().to_string(),
                                    format!("sha256:{}", weights_sha256(path)?),
                                )
                            }
                            None => (
                                props.model_path.clone(),
                                format!("reported-path:{}", props.model_path),
                            ),
                        };
                        let alias = props.alias.clone().ok_or_else(|| {
                            InferenceFailure::new("identity_unavailable", "нет alias модели")
                        })?;
                        if let Some(context) = props.context_size {
                            load.insert("server_context_size".into(), context.to_string());
                        }
                        Ok((
                            Transport::Llama {
                                base,
                                alias,
                                api_key: None,
                                props: props.clone(),
                                managed: None,
                            },
                            Cancellation::none(),
                            BackendIdentity {
                                kind: kind.into(),
                                mode: mode.into(),
                                version: props.build_info.clone(),
                                scope,
                            },
                            ModelIdentity {
                                profile: model.profile.clone(),
                                name,
                                weights,
                                // Отдельный projector внешнего сервера API не сообщает:
                                // он отсутствует, только если профиль явно объявил
                                // vision встроенным или ненужным.
                                projector: (model.vision
                                    == crate::inference_config::VisionMode::Projector)
                                    .then(|| "unverified:external".to_owned()),
                                vision: "backend_reported".into(),
                                load,
                            },
                            client,
                        ))
                    }
                }
            }
            _ => unreachable!("разбор связывает backend и раздел профиля одного вида"),
        }
    }

    pub(crate) fn provenance(&self) -> &InferenceProvenance {
        &self.provenance
    }
    pub(crate) fn interrupted(&mut self) -> bool {
        self.cancellation.interrupted()
    }

    /// Один запрос: с изображением PNG для задач с изображением, без него —
    /// для текстовых. Модель сверяется до и после запроса.
    pub(crate) async fn run(
        &mut self,
        png: Option<&[u8]>,
    ) -> Result<ModelResponse, InferenceFailure> {
        match (self.contract.requires_image, png) {
            (true, None) => {
                return Err(InferenceFailure::new(
                    "image_required",
                    "задача выполняется только с изображением",
                ));
            }
            (false, Some(_)) => {
                return Err(InferenceFailure::new(
                    "image_not_expected",
                    "текстовая задача не принимает изображение",
                ));
            }
            (_, Some(png)) if png.len() > MAX_IMAGE_BYTES => {
                return Err(InferenceFailure::new(
                    "image_too_large",
                    "изображение превышает предел",
                ));
            }
            _ => {}
        }
        if self.cancellation.interrupted() {
            return Err(InferenceFailure::interrupted());
        }
        self.report.requests += 1;
        let mut cancellation = std::mem::replace(&mut self.cancellation, Cancellation::none());
        let result = {
            let future = std::pin::pin!(self.request(png));
            tokio::select! {
                result = future => result,
                () = cancellation.wait() => Err(InferenceFailure::interrupted()),
            }
        };
        self.cancellation = cancellation;
        result
    }

    async fn request(&mut self, png: Option<&[u8]>) -> Result<ModelResponse, InferenceFailure> {
        let timeout = Duration::from_secs(self.task.generation.timeout_seconds);
        let generation = self.task.generation.clone();
        let schema = self.contract.schema.clone();
        let image = png.map(|png| base64::engine::general_purpose::STANDARD.encode(png));
        let started = Instant::now();
        match &mut self.transport {
            Transport::Ollama {
                base,
                model,
                digest,
                keep_alive_seconds,
                context_size,
            } => {
                if ollama_digest(&self.client, base, model, timeout).await? != *digest {
                    return Err(InferenceFailure::new(
                        "model_changed_during_scan",
                        "digest модели изменился",
                    ));
                }
                let mut options = json!({
                    "temperature": generation.temperature,
                    "num_predict": generation.max_output_tokens,
                    "num_ctx": context_size,
                });
                if let Some(seed) = generation.seed {
                    options["seed"] = json!(seed);
                }
                let mut body = json!({
                    "model": model, "stream": false, "think": false,
                    "keep_alive": format!("{keep_alive_seconds}s"),
                    "options": options,
                    "messages": [ollama_message(&self.prompt, image.as_deref())],
                });
                if generation.structured_output == StructuredOutput::JsonSchema {
                    body["format"] = schema;
                }
                let response = post_json(
                    &self.client,
                    base,
                    "/api/chat",
                    None,
                    timeout,
                    "ollama",
                    &body,
                )
                .await?;
                if response["done"] != true || response["done_reason"] != "stop" {
                    return Err(if response["done_reason"] == "length" {
                        InferenceFailure::new("token_limit", "ответ обрезан пределом токенов")
                    } else {
                        InferenceFailure::new("incomplete_response", "ответ Ollama не завершён")
                    });
                }
                let content = response["message"]["content"].as_str().ok_or_else(|| {
                    InferenceFailure::new("missing_content", "нет message.content")
                })?;
                if ollama_digest(&self.client, base, model, timeout).await? != *digest {
                    return Err(InferenceFailure::new(
                        "model_changed_during_scan",
                        "digest модели изменился",
                    ));
                }
                let nanos = |key: &str| response[key].as_u64().map(|value| value / 1_000_000);
                bounded_content(
                    content,
                    RunTimings {
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        prompt_ms: nanos("prompt_eval_duration"),
                        generation_ms: nanos("eval_duration"),
                        load_ms: nanos("load_duration"),
                        prompt_tokens: response["prompt_eval_count"].as_u64(),
                        output_tokens: response["eval_count"].as_u64(),
                    },
                )
            }
            Transport::Llama {
                base,
                alias,
                api_key,
                managed,
                props,
            } => {
                let connect = Duration::from_secs(10).min(timeout);
                let current = match managed {
                    Some(server) => server.identity(&self.client, connect).await?,
                    None => {
                        crate::llama_server::server_props(
                            &self.client,
                            base,
                            api_key.as_deref(),
                            connect,
                        )
                        .await?
                    }
                };
                if current != *props {
                    return Err(InferenceFailure::new(
                        "model_changed_during_scan",
                        "сервер сменил модель или сборку",
                    ));
                }
                let mut body = json!({
                    "model": alias,
                    "stream": false,
                    "temperature": generation.temperature,
                    "max_tokens": generation.max_output_tokens,
                    "messages": [llama_message(&self.prompt, image.as_deref())],
                });
                if let Some(seed) = generation.seed {
                    body["seed"] = json!(seed);
                }
                if generation.structured_output == StructuredOutput::JsonSchema {
                    body["response_format"] = json!({
                        "type": "json_schema",
                        "json_schema": {"name": self.contract.id, "strict": true, "schema": schema},
                    });
                }
                let response = post_json(
                    &self.client,
                    base,
                    "/v1/chat/completions",
                    api_key.as_deref(),
                    timeout,
                    "llama",
                    &body,
                )
                .await?;
                if response["model"]
                    .as_str()
                    .is_some_and(|model| model != alias.as_str())
                {
                    return Err(InferenceFailure::new(
                        "model_mismatch",
                        "ответ пришёл от другой модели",
                    ));
                }
                let choice = &response["choices"][0];
                match choice["finish_reason"].as_str() {
                    Some("stop") => {}
                    Some("length") => {
                        return Err(InferenceFailure::new(
                            "token_limit",
                            "ответ обрезан пределом токенов",
                        ));
                    }
                    _ => {
                        return Err(InferenceFailure::new(
                            "incomplete_response",
                            "ответ не завершён",
                        ));
                    }
                }
                let content = choice["message"]["content"].as_str().ok_or_else(|| {
                    InferenceFailure::new("missing_content", "нет message.content")
                })?;
                let after = match managed {
                    Some(server) => server.identity(&self.client, connect).await?,
                    None => {
                        crate::llama_server::server_props(
                            &self.client,
                            base,
                            api_key.as_deref(),
                            connect,
                        )
                        .await?
                    }
                };
                if after != *props {
                    return Err(InferenceFailure::new(
                        "model_changed_during_scan",
                        "сервер сменил модель или сборку",
                    ));
                }
                let timing = |key: &str| {
                    response["timings"][key]
                        .as_f64()
                        .map(|value| value.round() as u64)
                };
                bounded_content(
                    content,
                    RunTimings {
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        prompt_ms: timing("prompt_ms"),
                        generation_ms: timing("predicted_ms"),
                        load_ms: None,
                        prompt_tokens: response["usage"]["prompt_tokens"].as_u64(),
                        output_tokens: response["usage"]["completion_tokens"].as_u64(),
                    },
                )
            }
        }
    }

    /// Останавливает собственный сервер (если он есть) и возвращает отчёт сессии.
    pub(crate) async fn finish(self) -> SessionReport {
        let mut report = self.report;
        if let Transport::Llama {
            managed: Some(server),
            ..
        } = self.transport
        {
            report.stop = Some((*server).stop().await);
        }
        report
    }
}

/// Сообщение Ollama `/api/chat`: изображения — отдельный массив base64.
fn ollama_message(prompt: &str, image: Option<&str>) -> Value {
    match image {
        Some(image) => json!({"role": "user", "content": prompt, "images": [image]}),
        None => json!({"role": "user", "content": prompt}),
    }
}

/// Сообщение OpenAI-совместимого `/v1/chat/completions`: изображение — часть
/// содержимого типа `image_url` с data URL.
fn llama_message(prompt: &str, image: Option<&str>) -> Value {
    match image {
        Some(image) => json!({"role": "user", "content": [
            {"type": "text", "text": prompt},
            {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{image}")}},
        ]}),
        None => json!({"role": "user", "content": prompt}),
    }
}

fn bounded_content(content: &str, timings: RunTimings) -> Result<ModelResponse, InferenceFailure> {
    if content.len() > MAX_CONTENT_BYTES {
        return Err(InferenceFailure::new(
            "response_too_large",
            "ответ модели превышает предел",
        ));
    }
    Ok(ModelResponse {
        content: content.to_owned(),
        timings,
    })
}

async fn ollama_digest(
    client: &reqwest::Client,
    base: &Url,
    model: &str,
    timeout: Duration,
) -> Result<String, InferenceFailure> {
    let tags = get_json(client, base, "/api/tags", None, timeout, "ollama").await?;
    let name = if model.contains(':') {
        model.to_owned()
    } else {
        format!("{model}:latest")
    };
    tags.get("models")
        .and_then(Value::as_array)
        .and_then(|models| {
            models
                .iter()
                .find(|entry| entry["name"] == name || entry["model"] == name)
        })
        .and_then(|entry| entry.get("digest"))
        .and_then(Value::as_str)
        .filter(|digest| {
            !digest.is_empty() && digest.len() <= 256 && !digest.chars().any(char::is_control)
        })
        .map(str::to_owned)
        .ok_or_else(|| {
            InferenceFailure::new(
                "model_not_installed_or_digest_missing",
                "модель не установлена в Ollama",
            )
        })
}

/// Извлекает JSON-объект ответа. Допускается ровно одно обрамление markdown
/// (```json … ```), которое некоторые модели добавляют без structured output;
/// любой иной текст вокруг объекта — ошибка предметной проверки.
pub(crate) fn json_object_text(content: &str) -> &str {
    let trimmed = content.trim();
    if let Some(rest) = trimmed.strip_prefix("```")
        && let Some(body) = rest.strip_suffix("```")
    {
        let body = body.strip_prefix("json").unwrap_or(body);
        if let Some(body) = body.strip_prefix('\n') {
            return body.trim();
        }
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference_config::{ConfigSources, LoadedConfig};
    use crate::inference_test_support::{MockLlama, external_config};
    use crate::temp_workspace::TempWorkspace;

    #[test]
    fn short_cuts_by_characters_and_keeps_the_prefix() {
        assert_eq!(
            short("reported-path:/mnt/c/Модели/model.gguf"),
            "reported-path:/mnt/c/Модели/mo…"
        );
        assert_eq!(
            short("sha256:0123456789abcdef0123"),
            "sha256:0123456789abcdef…"
        );
        assert_eq!(short("sha256:short"), "sha256:short");
        assert_eq!(short("ПутьБезПрефиксаДлинный"), "ПутьБезПрефиксаД…");
        assert_eq!(short("unavailable"), "unavailable");
    }

    #[test]
    fn traversal_error_keeps_code_and_message_and_gains_session_evidence() {
        let session = SessionReport {
            requests: 2,
            ..SessionReport::default()
        };
        let error = AssetError::with_details(
            ErrorCode::IoFailure,
            "запись журнала не удалась",
            json!({"path": "ocr/suggestions.json"}),
        );
        let enriched = with_session_evidence(error, true, Some(&session));
        assert_eq!(enriched.code, ErrorCode::IoFailure);
        assert_eq!(enriched.message, "запись журнала не удалась");
        assert_eq!(enriched.details["path"], "ocr/suggestions.json");
        assert_eq!(enriched.details["stop_unproven"], true);
        assert_eq!(enriched.details["inference"]["requests"], 2);
        // Без сессии ошибка не меняется.
        let plain = with_session_evidence(AssetError::new(ErrorCode::IoFailure, "x"), true, None);
        assert_eq!(plain.details, json!({}));
    }

    #[test]
    fn interrupted_traversal_reports_saved_suggestions() {
        let saved = |count: u64| {
            AssetError::with_details(ErrorCode::Interrupted, "x", json!({"saved": count}))
                .saved_before_failure()
        };
        assert!(saved(3));
        assert!(!saved(0));
        assert!(!AssetError::new(ErrorCode::Interrupted, "x").saved_before_failure());
    }

    #[test]
    fn failed_start_with_unproven_stop_is_reported_as_stop_unproven() {
        let failure = InferenceFailure::new("startup_timeout", "сервер не ответил");
        assert!(!failure.stop_unproven(), "сервер не запускался");
        let proven = StopReport {
            method: "terminated".into(),
            group_empty: true,
            port_released: true,
            ..StopReport::default()
        };
        assert!(!failure.clone().with_stop(proven.clone()).stop_unproven());
        let unproven = StopReport {
            port_released: false,
            ..proven
        };
        assert!(failure.with_stop(unproven).stop_unproven());
    }

    fn task_for(url: &str, extra: &[&str]) -> (TempWorkspace, ResolvedTask) {
        let temp = TempWorkspace::create("inference-transport").unwrap();
        let path = external_config(temp.path(), url);
        let task = LoadedConfig::load(&ConfigSources {
            explicit_file: Some(path),
            cli_values: extra
                .iter()
                .map(|assignment| crate::inference_config::parse_assignment(assignment).unwrap())
                .collect(),
            skip_local_discovery: true,
            ..ConfigSources::default()
        })
        .unwrap()
        .resolve(TaskId::KanjiOcr)
        .unwrap();
        (temp, task)
    }

    async fn session_for(url: &str) -> Result<InferenceSession, InferenceFailure> {
        session(url).await
    }

    async fn session(url: &str) -> Result<InferenceSession, InferenceFailure> {
        let (_temp, task) = task_for(url, &[]);
        InferenceSession::prepare(task, crate::kanji_ocr::contract(), "t-v1")
            .await
            .map_err(|error| error.0)
    }

    #[tokio::test]
    async fn external_llama_uses_openai_protocol_and_confirms_identity_around_each_request() {
        let mock = MockLlama::new("{\"observed\":\"保\"}");
        let mut session = session(&mock.url).await.unwrap();
        let provenance = session.provenance().clone();
        assert_eq!(provenance.backend.kind, "llama_server");
        assert_eq!(provenance.backend.mode, "external");
        assert_eq!(provenance.backend.version.as_deref(), Some("b0-fixture"));
        assert_eq!(provenance.backend.scope, "loopback");
        // Файл весов внешнего сервера не указан: идентичность лишь сообщена
        // сервером, такой результат не переиспользуется из кэша.
        assert_eq!(
            provenance.model.weights,
            "reported-path:/models/fixture.gguf"
        );
        assert_eq!(
            provenance.model.projector.as_deref(),
            Some("unverified:external")
        );
        assert!(!provenance.reusable());
        let response = session.run(Some(b"png-bytes")).await.unwrap();
        assert_eq!(response.content, "{\"observed\":\"保\"}");
        assert_eq!(response.timings.prompt_ms, Some(2));
        let chat = &mock.chats()[0];
        assert_eq!(chat["model"], "external-model");
        assert_eq!(chat["temperature"], 0.0);
        assert_eq!(chat["max_tokens"], 128);
        assert_eq!(chat["response_format"]["json_schema"]["strict"], true);
        assert_eq!(chat["messages"][0]["content"][0]["type"], "text");
        assert_eq!(
            chat["messages"][0]["content"][1]["image_url"]["url"],
            "data:image/png;base64,cG5nLWJ5dGVz"
        );
        // Сервер сменил модель между запросами: результат не принимается.
        mock.state.lock().unwrap().model_path = "/models/other.gguf".into();
        assert_eq!(
            session.run(Some(b"png")).await.unwrap_err().code,
            "model_changed_during_scan"
        );
        let report = session.finish().await;
        assert!(report.stop.is_none(), "внешний сервер не останавливается");
    }

    #[tokio::test]
    async fn llama_failures_are_typed_and_never_become_answers() {
        let mock = MockLlama::new("{}");
        mock.state.lock().unwrap().vision = false;
        assert_eq!(
            session(&mock.url).await.err().unwrap().code,
            "model_does_not_support_vision"
        );
        mock.state.lock().unwrap().vision = true;
        for (finish, status, error, code) in [
            ("length", 200, "", "token_limit"),
            ("tool_calls", 200, "", "incomplete_response"),
            ("stop", 503, "loading", "llama_http_503"),
            ("stop", 429, "busy", "llama_http_429"),
            (
                "stop",
                400,
                "Failed to initialize samplers: grammar",
                "structured_output_rejected",
            ),
            (
                "stop",
                400,
                "request exceeds the available context size",
                "context_exceeded",
            ),
        ] {
            {
                let mut state = mock.state.lock().unwrap();
                state.finish_reason = finish.into();
                state.chat_status = status;
                state.chat_error = error.into();
            }
            let mut session = session(&mock.url).await.unwrap();
            assert_eq!(session.run(Some(b"png")).await.unwrap_err().code, code);
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        assert_eq!(
            session(&closed).await.err().unwrap().code,
            "llama_network_error"
        );
    }

    #[tokio::test]
    async fn configured_external_model_must_match_what_the_server_loaded() {
        let mock = MockLlama::new("{}");
        let temp = TempWorkspace::create("inference-external-model").unwrap();
        let weights = temp.path().join("model.gguf");
        std::fs::write(&weights, b"weights").unwrap();
        let assignment = format!(
            "models.kanji_vision.llama_server.model={}",
            weights.display()
        );
        let (_keep, task) = task_for(
            &mock.url,
            &[
                &assignment,
                "models.kanji_vision.llama_server.projector=\"\"",
                "models.kanji_vision.llama_server.vision=\"integrated\"",
            ],
        );
        let failure = InferenceSession::prepare(task.clone(), crate::kanji_ocr::contract(), "t")
            .await
            .err()
            .unwrap()
            .0
            .clone();
        assert_eq!(failure.code, "model_mismatch");
        mock.state.lock().unwrap().model_path = weights.display().to_string();
        let session = InferenceSession::prepare(task, crate::kanji_ocr::contract(), "t")
            .await
            .map_err(|error| error.0)
            .unwrap();
        let provenance = session.provenance();
        assert_eq!(
            provenance.model.weights,
            format!("sha256:{}", sha256_hex(b"weights"))
        );
        // Профиль объявил vision встроенным: проверяемые веса — повод для кэша.
        assert_eq!(provenance.model.projector, None);
        assert!(provenance.reusable());

        // Тот же путь, но профиль ждёт отдельный projector, который внешний
        // сервер не сообщает: кэш не используется, хотя SHA весов известен.
        let (_keep, task) = task_for(
            &mock.url,
            &[
                &assignment,
                "models.kanji_vision.llama_server.projector=\"\"",
            ],
        );
        let session = InferenceSession::prepare(task, crate::kanji_ocr::contract(), "t")
            .await
            .map_err(|error| error.0)
            .unwrap();
        assert!(session.provenance().model.weights.starts_with("sha256:"));
        assert_eq!(
            session.provenance().model.projector.as_deref(),
            Some("unverified:external")
        );
        assert!(!session.provenance().reusable());
    }

    #[test]
    fn only_verifiable_identities_are_reusable() {
        let (_temp, task) = task_for("http://127.0.0.1:9", &[]);
        let mut provenance =
            InferenceProvenance::unavailable(&task, &crate::kanji_ocr::contract(), "t");
        assert!(!provenance.reusable(), "unavailable");
        for (weights, projector, reusable) in [
            ("sha256:aa", None, true),
            ("sha256:aa", Some("sha256:bb"), true),
            ("ollama-digest:cc", None, true),
            ("sha256:aa", Some("unverified:external"), false),
            ("reported-path:/m.gguf", None, false),
            ("reported-path:/m.gguf", Some("unverified:external"), false),
        ] {
            provenance.model.weights = weights.into();
            provenance.model.projector = projector.map(str::to_owned);
            assert_eq!(provenance.reusable(), reusable, "{weights} {projector:?}");
        }
    }

    fn text_contract() -> ResponseContract {
        ResponseContract {
            id: "text-fixture-v1",
            instruction: "Return ONLY JSON.",
            schema: json!({"type": "object"}),
            requires_image: false,
        }
    }

    #[tokio::test]
    async fn text_task_needs_no_vision_and_sends_no_image() {
        let mock = MockLlama::new("{\"answer\":\"ok\"}");
        mock.state.lock().unwrap().vision = false;
        let (_temp, task) = task_for(&mock.url, &[]);
        let mut session = InferenceSession::prepare(task.clone(), text_contract(), "text-v1")
            .await
            .map_err(|error| error.0)
            .unwrap();
        assert_eq!(
            session.run(Some(b"png")).await.unwrap_err().code,
            "image_not_expected"
        );
        let response = session.run(None).await.unwrap();
        assert_eq!(response.content, "{\"answer\":\"ok\"}");
        let chat = &mock.chats()[0];
        assert!(chat["messages"][0]["content"].is_string());
        assert!(!chat.to_string().contains("image_url"));
        // Задача с изображением на той же модели без vision отклоняется заранее,
        // а запрос без изображения — до обращения к серверу.
        assert_eq!(
            InferenceSession::prepare(task, crate::kanji_ocr::contract(), "t")
                .await
                .err()
                .unwrap()
                .0
                .code,
            "model_does_not_support_vision"
        );
        mock.state.lock().unwrap().vision = true;
        let mut session = session_for(&mock.url).await.unwrap();
        assert_eq!(session.run(None).await.unwrap_err().code, "image_required");
        assert_eq!(mock.chats().len(), 1);
    }

    #[test]
    fn messages_follow_each_backend_protocol() {
        assert_eq!(
            ollama_message("p", Some("aW1n")),
            json!({"role": "user", "content": "p", "images": ["aW1n"]})
        );
        assert_eq!(
            ollama_message("p", None),
            json!({"role": "user", "content": "p"})
        );
        assert_eq!(
            llama_message("p", Some("aW1n"))["content"][1]["image_url"]["url"],
            "data:image/png;base64,aW1n"
        );
        assert_eq!(
            llama_message("p", None),
            json!({"role": "user", "content": "p"})
        );
    }

    #[test]
    fn interruption_keeps_unproven_stop_evidence() {
        let session = SessionReport {
            requests: 1,
            stop: Some(StopReport {
                method: "killed".into(),
                group_empty: true,
                port_released: false,
                ..StopReport::default()
            }),
            ..SessionReport::default()
        };
        let error = interrupted_error("прервано", 2, true, Some(&session));
        assert_eq!(error.code, ErrorCode::Interrupted);
        assert_eq!(error.exit_code(), 130);
        assert_eq!(error.details["saved"], 2);
        assert_eq!(error.details["stop_unproven"], true);
        assert_eq!(error.details["blockers"], json!([STOP_UNPROVEN_BLOCKER]));
        assert_eq!(error.details["inference"]["stop"]["method"], "killed");
        let proven = interrupted_error("прервано", 0, false, None);
        assert_eq!(proven.details["stop_unproven"], false);
        assert!(proven.details.get("blockers").is_none());
        // Отказ подготовки с недоказанной остановкой несёт тот же маркер.
        let failure = InferenceFailure::interrupted().with_stop(session.stop.unwrap());
        let error = failure.into_asset_error();
        assert_eq!(error.code, ErrorCode::Interrupted);
        assert_eq!(error.details["blockers"], json!([STOP_UNPROVEN_BLOCKER]));
    }

    #[test]
    fn single_markdown_fence_is_the_only_tolerated_wrapper() {
        assert_eq!(json_object_text("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(json_object_text("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(json_object_text("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(json_object_text("Answer: {\"a\":1}"), "Answer: {\"a\":1}");
        assert_eq!(
            json_object_text("```json{\"a\":1}```"),
            "```json{\"a\":1}```"
        );
    }

    #[test]
    fn http_failures_are_typed_by_backend() {
        assert_eq!(http_failure("llama", 503, b"{}").code, "llama_http_503");
        assert_eq!(http_failure("ollama", 429, b"").code, "ollama_http_429");
        assert_eq!(
            http_failure(
                "llama",
                400,
                br#"{"error":{"message":"Failed to initialize samplers: Unexpected empty grammar stack"}}"#
            )
            .code,
            "structured_output_rejected"
        );
    }
}
