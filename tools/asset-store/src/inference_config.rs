//! Публичная конфигурация локального inference.
//!
//! Слои применяются в фиксированном порядке:
//!
//! 1. версионируемый `tools/asset-store/config/inference.toml`, встроенный в
//!    бинарник вместе со штатными промптами (не зависит от текущего каталога);
//! 2. игнорируемый Git `tools/asset-store/config/inference.local.toml` рабочей
//!    копии, найденной вверх от `--repository-root`;
//! 3. файл `--inference-config`;
//! 4. значения `--inference-set` и удобные флаги команды.
//!
//! Таблицы сливаются по ключам, скаляры последнего слоя заменяют прежние; после
//! слияния весь документ строго разбирается (`deny_unknown_fields`) и
//! проверяется до запуска модели. Для каждого листового ключа сохраняется слой,
//! который его задал: от него зависит база относительного пути. Подстановки
//! переменных среды и исполнения команд нет; единственное расширение пути —
//! префикс `~/` (домашний каталог).
//!
//! Конфигурация не задаёт формат ответа, его проверку и правила доверия: ими
//! владеет код предметной задачи.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::{Host, Url};

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;

pub(crate) const SCHEMA_VERSION: u32 = 1;
const REPOSITORY_CONFIG: &str = include_str!("../config/inference.toml");
/// Штатные файлы, на которые может ссылаться встроенный слой.
const EMBEDDED_FILES: &[(&str, &str)] = &[
    (
        "prompts/kanji_ocr.txt",
        include_str!("../config/prompts/kanji_ocr.txt"),
    ),
    (
        "prompts/jpdb_pitch_review.txt",
        include_str!("../config/prompts/jpdb_pitch_review.txt"),
    ),
];
/// Расположение штатного файла и локального переопределения в рабочей копии.
pub(crate) const CONFIG_DIRECTORY: &str = "tools/asset-store/config";
pub(crate) const LOCAL_FILE_NAME: &str = "inference.local.toml";
const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_PROMPT_BYTES: u64 = 32 * 1024;

/// Именованные задачи публичной схемы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TaskId {
    KanjiOcr,
    JpdbPitchReview,
}

impl TaskId {
    pub(crate) const ALL: [Self; 2] = [Self::KanjiOcr, Self::JpdbPitchReview];

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::KanjiOcr => "kanji_ocr",
            Self::JpdbPitchReview => "jpdb_pitch_review",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|task| task.as_str() == value)
    }
}

// ---------------------------------------------------------------------------
// Строгая схема файла после слияния слоёв.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    schema_version: u32,
    runtime: RawRuntime,
    backends: BTreeMap<String, RawBackend>,
    models: BTreeMap<String, RawModel>,
    tasks: BTreeMap<String, RawTask>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRuntime {
    max_loaded_models: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RawBackend {
    LlamaServer(RawLlamaBackend),
    Ollama(RawOllamaBackend),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlamaBackend {
    mode: LlamaMode,
    binary: String,
    windows_interop: bool,
    host: String,
    port: u16,
    endpoint: String,
    allow_remote: bool,
    connect_timeout_seconds: u64,
    startup_timeout_seconds: u64,
    shutdown_timeout_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOllamaBackend {
    endpoint: String,
    allow_remote: bool,
    connect_timeout_seconds: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LlamaMode {
    Managed,
    External,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModel {
    backend: String,
    llama_server: Option<RawLlamaModel>,
    ollama: Option<RawOllamaModel>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLlamaModel {
    model: String,
    vision: VisionMode,
    projector: String,
    context_size: u32,
    gpu_layers: GpuLayers,
    threads: u32,
    batch_size: u32,
    ubatch_size: u32,
    parallel: u32,
    projector_offload: bool,
    flash_attention: Switch,
    reasoning: Switch,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOllamaModel {
    model: String,
    context_size: u32,
    keep_alive_seconds: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTask {
    model: String,
    prompt: String,
    temperature: f64,
    max_output_tokens: u32,
    structured_output: StructuredOutput,
    timeout_seconds: u64,
    seed: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum VisionMode {
    /// Отдельный mmproj-файл обязателен.
    Projector,
    /// Vision встроен в GGUF; отдельный projector не нужен.
    Integrated,
    /// Модель без vision.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Switch {
    Auto,
    On,
    Off,
}

impl Switch {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StructuredOutput {
    JsonSchema,
    None,
}

/// `"auto"`, `"all"` либо точное число слоёв.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GpuLayers {
    Auto,
    All,
    Count(u32),
}

impl GpuLayers {
    pub(crate) fn argument(self) -> String {
        match self {
            Self::Auto => "auto".into(),
            Self::All => "all".into(),
            Self::Count(count) => count.to_string(),
        }
    }
}

impl Serialize for GpuLayers {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Count(count) => serializer.serialize_u32(*count),
            other => serializer.serialize_str(&other.argument()),
        }
    }
}

impl<'de> Deserialize<'de> for GpuLayers {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Count(u32),
            Keyword(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Count(count) if count <= 1024 => Ok(Self::Count(count)),
            Raw::Count(_) => Err(serde::de::Error::custom(
                "gpu_layers: число слоёв должно быть от 0 до 1024",
            )),
            Raw::Keyword(keyword) => match keyword.as_str() {
                "auto" => Ok(Self::Auto),
                "all" => Ok(Self::All),
                _ => Err(serde::de::Error::custom(
                    "gpu_layers: допустимы \"auto\", \"all\" или число",
                )),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Итоговые значения задачи.
// ---------------------------------------------------------------------------

/// Как задача подключается к backend и запускает его.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ResolvedBackend {
    LlamaServer(LlamaBackend),
    Ollama(OllamaBackend),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LlamaBackend {
    pub name: String,
    pub mode: LlamaMode,
    pub binary: Option<PathBuf>,
    pub windows_interop: bool,
    pub host: IpAddr,
    pub port: u16,
    pub endpoint: Option<Url>,
    pub allow_remote: bool,
    pub connect_timeout_seconds: u64,
    pub startup_timeout_seconds: u64,
    pub shutdown_timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OllamaBackend {
    pub name: String,
    pub endpoint: Url,
    pub allow_remote: bool,
    pub connect_timeout_seconds: u64,
}

/// Веса, vision-компоненты и потребление ресурсов профиля модели.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ResolvedModel {
    LlamaServer(LlamaModel),
    Ollama(OllamaModel),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct LlamaModel {
    pub profile: String,
    pub model: Option<PathBuf>,
    pub vision: VisionMode,
    pub projector: Option<PathBuf>,
    pub context_size: u32,
    pub gpu_layers: GpuLayers,
    pub threads: u32,
    pub batch_size: u32,
    pub ubatch_size: u32,
    pub parallel: u32,
    pub projector_offload: bool,
    pub flash_attention: Switch,
    pub reasoning: Switch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct OllamaModel {
    pub profile: String,
    pub model: Option<String>,
    pub context_size: u32,
    pub keep_alive_seconds: u64,
}

/// Текст промпта и его происхождение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ResolvedPrompt {
    /// `repository:<путь>` для встроенного файла или абсолютный путь пользователя.
    pub source: String,
    #[serde(skip)]
    pub text: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Generation {
    pub temperature: f64,
    pub max_output_tokens: u32,
    pub seed: Option<u64>,
    pub structured_output: StructuredOutput,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ResolvedTask {
    pub task: TaskId,
    pub backend: ResolvedBackend,
    pub model: ResolvedModel,
    pub prompt: ResolvedPrompt,
    pub generation: Generation,
}

/// Чего не хватает, чтобы задача могла выполняться.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct ReadinessIssue {
    pub code: &'static str,
    pub key: String,
    pub hint: String,
}

impl ResolvedTask {
    pub(crate) fn model_profile(&self) -> &str {
        match &self.model {
            ResolvedModel::LlamaServer(model) => &model.profile,
            ResolvedModel::Ollama(model) => &model.profile,
        }
    }

    pub(crate) fn backend_name(&self) -> &str {
        match &self.backend {
            ResolvedBackend::LlamaServer(backend) => &backend.name,
            ResolvedBackend::Ollama(backend) => &backend.name,
        }
    }

    /// Отсутствующие обязательные значения; пустой список — задача настроена.
    pub(crate) fn readiness(&self) -> Vec<ReadinessIssue> {
        let profile = self.model_profile();
        let mut issues = Vec::new();
        match (&self.backend, &self.model) {
            (ResolvedBackend::LlamaServer(backend), ResolvedModel::LlamaServer(model)) => {
                if backend.mode == LlamaMode::Managed {
                    if backend.binary.is_none() {
                        issues.push(ReadinessIssue {
                            code: "backend_not_configured",
                            key: format!("backends.{}.binary", backend.name),
                            hint: "укажите путь к llama-server в inference.local.toml".into(),
                        });
                    }
                    if model.model.is_none() {
                        issues.push(ReadinessIssue {
                            code: "model_not_configured",
                            key: format!("models.{profile}.llama_server.model"),
                            hint: "укажите путь к локальному GGUF в inference.local.toml".into(),
                        });
                    }
                    if model.vision == VisionMode::Projector && model.projector.is_none() {
                        issues.push(ReadinessIssue {
                            code: "projector_not_configured",
                            key: format!("models.{profile}.llama_server.projector"),
                            hint:
                                "укажите путь к mmproj или vision = \"integrated\", если он встроен"
                                    .into(),
                        });
                    }
                } else if backend.endpoint.is_none() {
                    issues.push(ReadinessIssue {
                        code: "backend_not_configured",
                        key: format!("backends.{}.endpoint", backend.name),
                        hint: "укажите URL внешнего llama-server".into(),
                    });
                }
            }
            (ResolvedBackend::Ollama(_), ResolvedModel::Ollama(model)) => {
                if model.model.is_none() {
                    issues.push(ReadinessIssue {
                        code: "model_not_configured",
                        key: format!("models.{profile}.ollama.model"),
                        hint: "укажите имя установленной vision-модели Ollama".into(),
                    });
                }
            }
            _ => unreachable!("разбор связывает backend и раздел профиля одного вида"),
        }
        issues
    }

    /// Ошибка `inference_unavailable`, если задача не настроена.
    pub(crate) fn require_ready(&self) -> Result<(), AssetError> {
        let issues = self.readiness();
        let Some(first) = issues.first() else {
            return Ok(());
        };
        Err(AssetError::with_details(
            ErrorCode::InferenceUnavailable,
            format!(
                "{}: задача {} не настроена ({}): {}",
                first.code,
                self.task.as_str(),
                first.key,
                first.hint
            ),
            json!({"reason": first.code, "task": self.task.as_str(), "issues": issues}),
        ))
    }
}

// ---------------------------------------------------------------------------
// Слои.
// ---------------------------------------------------------------------------

/// Вид слоя, задавшего значение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LayerKind {
    Repository,
    Local,
    Explicit,
    Cli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Layer {
    pub kind: LayerKind,
    /// Файл слоя; для CLI — отсутствует.
    pub path: Option<PathBuf>,
    /// Применён ли слой (локальный файл может отсутствовать).
    pub present: bool,
}

impl Layer {
    /// База относительных путей. `None` у встроенного слоя: его пути указывают
    /// на встроенные файлы.
    fn base_directory(&self) -> Result<Option<PathBuf>, AssetError> {
        match self.kind {
            LayerKind::Repository => Ok(None),
            LayerKind::Local | LayerKind::Explicit => Ok(self
                .path
                .as_ref()
                .and_then(|path| path.parent())
                .map(Path::to_path_buf)),
            LayerKind::Cli => std::env::current_dir()
                .map(Some)
                .map_err(|error| AssetError::io("текущий каталог для пути CLI", error)),
        }
    }
}

/// Источники конфигурации одной команды.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConfigSources {
    pub repository_root: PathBuf,
    pub explicit_file: Option<PathBuf>,
    /// Пары `ключ.через.точки` → значение в порядке аргументов.
    pub cli_values: Vec<(String, toml::Value)>,
    /// Не искать локальный файл рабочей копии (используется тестами модулей).
    pub skip_local_discovery: bool,
}

/// Результат слияния слоёв и строгого разбора.
#[derive(Debug, Clone)]
pub(crate) struct LoadedConfig {
    raw: RawConfig,
    layers: Vec<Layer>,
    /// Ключ листа → индекс слоя в `layers`.
    origins: BTreeMap<String, usize>,
}

fn config_error(message: impl Into<String>) -> AssetError {
    AssetError::new(ErrorCode::InferenceConfigInvalid, message)
}

/// Разбирает `ключ=значение` аргумента `--inference-set`. Значение читается как
/// TOML-литерал; текст, не являющийся TOML-литералом, считается строкой.
pub(crate) fn parse_assignment(argument: &str) -> Result<(String, toml::Value), AssetError> {
    let (key, value) = argument
        .split_once('=')
        .ok_or_else(|| config_error("--inference-set ожидает ключ=значение"))?;
    validate_key(key)?;
    let value = match toml::from_str::<toml::Table>(&format!("v = {value}")) {
        Ok(mut table) => table.remove("v").expect("ключ задан выше"),
        Err(_) => toml::Value::String(value.to_owned()),
    };
    Ok((key.to_owned(), value))
}

fn validate_key(key: &str) -> Result<(), AssetError> {
    if key.is_empty()
        || key.len() > 256
        || key.split('.').any(|segment| {
            segment.is_empty()
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        })
    {
        return Err(config_error(format!(
            "недопустимый ключ конфигурации `{key}`"
        )));
    }
    Ok(())
}

/// Рабочая копия, содержащая штатный файл конфигурации, вверх от `start`.
pub(crate) fn find_checkout(start: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(start).ok()?;
    canonical.ancestors().find_map(|ancestor| {
        ancestor
            .join(CONFIG_DIRECTORY)
            .join("inference.toml")
            .is_file()
            .then(|| ancestor.to_path_buf())
    })
}

fn read_layer_file(path: &Path) -> Result<toml::Table, AssetError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| AssetError::io(format!("файл конфигурации {}", path.display()), error))?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(config_error(format!(
            "{}: нужен обычный файл не больше {MAX_CONFIG_BYTES} байт",
            path.display()
        )));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| AssetError::io(format!("файл конфигурации {}", path.display()), error))?;
    toml::from_str(&text).map_err(|error| config_error(format!("{}: {error}", path.display())))
}

/// Сливает `overlay` в `base`, запоминая слой каждого листа.
fn merge(
    base: &mut toml::Table,
    overlay: toml::Table,
    layer: usize,
    prefix: &str,
    origins: &mut BTreeMap<String, usize>,
) -> Result<(), AssetError> {
    for (key, value) in overlay {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(table)) => {
                merge(existing, table, layer, &path, origins)?;
            }
            (Some(toml::Value::Table(_)), _) => {
                return Err(config_error(format!(
                    "`{path}` — таблица; заменить её скаляром нельзя"
                )));
            }
            (_, toml::Value::Table(table)) => {
                let mut fresh = toml::Table::new();
                merge(&mut fresh, table, layer, &path, origins)?;
                base.insert(key, toml::Value::Table(fresh));
            }
            (_, value) => {
                origins.retain(|existing, _| {
                    existing != &path && !existing.starts_with(&format!("{path}."))
                });
                origins.insert(path, layer);
                base.insert(key, value);
            }
        }
    }
    Ok(())
}

fn record_origins(
    table: &toml::Table,
    layer: usize,
    prefix: &str,
    origins: &mut BTreeMap<String, usize>,
) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::Table(inner) => record_origins(inner, layer, &path, origins),
            _ => {
                origins.insert(path, layer);
            }
        }
    }
}

fn assignment_table(key: &str, value: toml::Value) -> toml::Table {
    let mut segments: Vec<&str> = key.split('.').collect();
    let last = segments.pop().expect("ключ проверен");
    let mut table = toml::Table::new();
    table.insert(last.to_owned(), value);
    for segment in segments.into_iter().rev() {
        let mut outer = toml::Table::new();
        outer.insert(segment.to_owned(), toml::Value::Table(table));
        table = outer;
    }
    table
}

impl LoadedConfig {
    pub(crate) fn load(sources: &ConfigSources) -> Result<Self, AssetError> {
        let mut merged: toml::Table = toml::from_str(REPOSITORY_CONFIG)
            .map_err(|error| config_error(format!("встроенная конфигурация: {error}")))?;
        let mut layers = vec![Layer {
            kind: LayerKind::Repository,
            path: Some(PathBuf::from(CONFIG_DIRECTORY).join("inference.toml")),
            present: true,
        }];
        let mut origins = BTreeMap::new();
        record_origins(&merged, 0, "", &mut origins);
        if !sources.skip_local_discovery {
            let local = find_checkout(&sources.repository_root)
                .map(|root| root.join(CONFIG_DIRECTORY).join(LOCAL_FILE_NAME));
            let present = local.as_ref().is_some_and(|path| path.is_file());
            layers.push(Layer {
                kind: LayerKind::Local,
                path: local.clone(),
                present,
            });
            if present {
                let table = read_layer_file(local.as_ref().expect("present"))?;
                merge(&mut merged, table, layers.len() - 1, "", &mut origins)?;
            }
        }
        if let Some(path) = &sources.explicit_file {
            let path = std::path::absolute(path)
                .map_err(|error| AssetError::io("путь --inference-config", error))?;
            let table = read_layer_file(&path)?;
            layers.push(Layer {
                kind: LayerKind::Explicit,
                path: Some(path),
                present: true,
            });
            merge(&mut merged, table, layers.len() - 1, "", &mut origins)?;
        }
        if !sources.cli_values.is_empty() {
            layers.push(Layer {
                kind: LayerKind::Cli,
                path: None,
                present: true,
            });
            let index = layers.len() - 1;
            for (key, value) in &sources.cli_values {
                validate_key(key)?;
                merge(
                    &mut merged,
                    assignment_table(key, value.clone()),
                    index,
                    "",
                    &mut origins,
                )?;
            }
        }
        let raw: RawConfig = toml::Value::Table(merged)
            .try_into()
            .map_err(|error: toml::de::Error| config_error(format!("схема inference: {error}")))?;
        let loaded = Self {
            raw,
            layers,
            origins,
        };
        loaded.validate()?;
        Ok(loaded)
    }
    fn origin(&self, key: &str) -> Option<&Layer> {
        self.origins.get(key).map(|index| &self.layers[*index])
    }

    fn validate(&self) -> Result<(), AssetError> {
        let raw = &self.raw;
        if raw.schema_version != SCHEMA_VERSION {
            return Err(AssetError::new(
                ErrorCode::UnsupportedSchemaVersion,
                format!(
                    "схема inference {} не поддерживается; ожидается {SCHEMA_VERSION}",
                    raw.schema_version
                ),
            ));
        }
        if raw.runtime.max_loaded_models != 1 {
            return Err(config_error(
                "runtime.max_loaded_models: поддерживается только 1 (модели загружаются последовательно)",
            ));
        }
        for (name, backend) in &raw.backends {
            validate_name(name)?;
            match backend {
                RawBackend::LlamaServer(llama) => validate_llama_backend(name, llama)?,
                RawBackend::Ollama(ollama) => {
                    in_range(
                        &format!("backends.{name}.connect_timeout_seconds"),
                        ollama.connect_timeout_seconds,
                        1,
                        120,
                    )?;
                    endpoint(
                        &ollama.endpoint,
                        ollama.allow_remote,
                        &format!("backends.{name}"),
                    )?;
                }
            }
        }
        for (name, model) in &raw.models {
            validate_name(name)?;
            let backend = raw.backends.get(&model.backend).ok_or_else(|| {
                config_error(format!(
                    "models.{name}.backend ссылается на отсутствующий backend `{}`",
                    model.backend
                ))
            })?;
            if let Some(llama) = &model.llama_server {
                validate_llama_model(name, llama)?;
            }
            if let Some(ollama) = &model.ollama {
                in_range(
                    &format!("models.{name}.ollama.context_size"),
                    ollama.context_size,
                    256,
                    1_048_576,
                )?;
                in_range(
                    &format!("models.{name}.ollama.keep_alive_seconds"),
                    ollama.keep_alive_seconds,
                    0,
                    86_400,
                )?;
                validate_text(&format!("models.{name}.ollama.model"), &ollama.model)?;
            }
            let section_present = match backend {
                RawBackend::LlamaServer(_) => model.llama_server.is_some(),
                RawBackend::Ollama(_) => model.ollama.is_some(),
            };
            if !section_present {
                return Err(config_error(format!(
                    "models.{name}: для backend `{}` нужен раздел [models.{name}.{}]",
                    model.backend,
                    match backend {
                        RawBackend::LlamaServer(_) => "llama_server",
                        RawBackend::Ollama(_) => "ollama",
                    }
                )));
            }
        }
        for (name, task) in &raw.tasks {
            if TaskId::parse(name).is_none() {
                return Err(config_error(format!(
                    "неизвестная задача `tasks.{name}`; поддерживаются kanji_ocr и jpdb_pitch_review"
                )));
            }
            let model = raw.models.get(&task.model).ok_or_else(|| {
                config_error(format!(
                    "tasks.{name}.model ссылается на отсутствующий профиль `{}`",
                    task.model
                ))
            })?;
            if model
                .llama_server
                .as_ref()
                .is_some_and(|llama| llama.vision == VisionMode::None)
                && matches!(
                    raw.backends.get(&model.backend),
                    Some(RawBackend::LlamaServer(_))
                )
            {
                return Err(config_error(format!(
                    "tasks.{name}: задаче нужен vision, а профиль `{}` объявлен без него",
                    task.model
                )));
            }
            if !task.temperature.is_finite() || !(0.0..=2.0).contains(&task.temperature) {
                return Err(config_error(format!(
                    "tasks.{name}.temperature должна быть от 0 до 2"
                )));
            }
            in_range(
                &format!("tasks.{name}.max_output_tokens"),
                task.max_output_tokens,
                16,
                8192,
            )?;
            in_range(
                &format!("tasks.{name}.timeout_seconds"),
                task.timeout_seconds,
                1,
                3600,
            )?;
            if task.prompt.trim().is_empty() {
                return Err(config_error(format!("tasks.{name}.prompt не задан")));
            }
        }
        for task in TaskId::ALL {
            if !raw.tasks.contains_key(task.as_str()) {
                return Err(config_error(format!(
                    "отсутствует раздел tasks.{}",
                    task.as_str()
                )));
            }
        }
        Ok(())
    }

    /// Итоговые настройки одной задачи. Промпт читается и проверяется здесь.
    pub(crate) fn resolve(&self, task: TaskId) -> Result<ResolvedTask, AssetError> {
        let raw_task = &self.raw.tasks[task.as_str()];
        let model_name = &raw_task.model;
        let raw_model = &self.raw.models[model_name];
        let backend_name = &raw_model.backend;
        let (backend, model) = match &self.raw.backends[backend_name] {
            RawBackend::LlamaServer(raw) => {
                let host: IpAddr = raw.host.parse().expect("проверено при загрузке");
                let llama = raw_model
                    .llama_server
                    .as_ref()
                    .expect("раздел проверен при загрузке");
                let backend = LlamaBackend {
                    name: backend_name.clone(),
                    mode: raw.mode,
                    binary: self.path(&format!("backends.{backend_name}.binary"), &raw.binary)?,
                    windows_interop: raw.windows_interop,
                    host,
                    port: raw.port,
                    endpoint: (!raw.endpoint.is_empty())
                        .then(|| endpoint(&raw.endpoint, raw.allow_remote, "endpoint"))
                        .transpose()?,
                    allow_remote: raw.allow_remote,
                    connect_timeout_seconds: raw.connect_timeout_seconds,
                    startup_timeout_seconds: raw.startup_timeout_seconds,
                    shutdown_timeout_seconds: raw.shutdown_timeout_seconds,
                };
                let prefix = format!("models.{model_name}.llama_server");
                let model = LlamaModel {
                    profile: model_name.clone(),
                    model: self.path(&format!("{prefix}.model"), &llama.model)?,
                    vision: llama.vision,
                    projector: self.path(&format!("{prefix}.projector"), &llama.projector)?,
                    context_size: llama.context_size,
                    gpu_layers: llama.gpu_layers,
                    threads: llama.threads,
                    batch_size: llama.batch_size,
                    ubatch_size: llama.ubatch_size,
                    parallel: llama.parallel,
                    projector_offload: llama.projector_offload,
                    flash_attention: llama.flash_attention,
                    reasoning: llama.reasoning,
                };
                (
                    ResolvedBackend::LlamaServer(backend),
                    ResolvedModel::LlamaServer(model),
                )
            }
            RawBackend::Ollama(raw) => {
                let ollama = raw_model.ollama.as_ref().expect("раздел проверен");
                (
                    ResolvedBackend::Ollama(OllamaBackend {
                        name: backend_name.clone(),
                        endpoint: endpoint(&raw.endpoint, raw.allow_remote, "endpoint")?,
                        allow_remote: raw.allow_remote,
                        connect_timeout_seconds: raw.connect_timeout_seconds,
                    }),
                    ResolvedModel::Ollama(OllamaModel {
                        profile: model_name.clone(),
                        model: (!ollama.model.is_empty()).then(|| ollama.model.clone()),
                        context_size: ollama.context_size,
                        keep_alive_seconds: ollama.keep_alive_seconds,
                    }),
                )
            }
        };
        Ok(ResolvedTask {
            task,
            backend,
            model,
            prompt: self.prompt(task)?,
            generation: Generation {
                temperature: raw_task.temperature,
                max_output_tokens: raw_task.max_output_tokens,
                seed: raw_task.seed,
                structured_output: raw_task.structured_output,
                timeout_seconds: raw_task.timeout_seconds,
            },
        })
    }

    /// Разрешает путь листа по базе слоя, который его задал. Пустая строка — не задано.
    fn path(&self, key: &str, value: &str) -> Result<Option<PathBuf>, AssetError> {
        if value.is_empty() {
            return Ok(None);
        }
        validate_text(key, value)?;
        let layer = self
            .origin(key)
            .ok_or_else(|| config_error(format!("неизвестный источник ключа {key}")))?;
        let path = if let Some(rest) = value.strip_prefix("~/") {
            std::env::home_dir()
                .ok_or_else(|| config_error(format!("{key}: домашний каталог неизвестен")))?
                .join(rest)
        } else if let Some(windows) = windows_drive_path(value) {
            windows
        } else {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                path
            } else {
                match layer.base_directory()? {
                    Some(base) => base.join(path),
                    None => {
                        return Err(config_error(format!(
                            "{key}: во встроенном слое допустимы только абсолютные пути"
                        )));
                    }
                }
            }
        };
        Ok(Some(path))
    }

    fn prompt(&self, task: TaskId) -> Result<ResolvedPrompt, AssetError> {
        let key = format!("tasks.{}.prompt", task.as_str());
        let value = &self.raw.tasks[task.as_str()].prompt;
        let layer = self
            .origin(&key)
            .ok_or_else(|| config_error(format!("неизвестный источник ключа {key}")))?;
        let (source, text) = if layer.kind == LayerKind::Repository {
            let text = EMBEDDED_FILES
                .iter()
                .find(|(name, _)| *name == value)
                .map(|(_, text)| (*text).to_owned())
                .ok_or_else(|| config_error(format!("{key}: встроенного промпта `{value}` нет")))?;
            (format!("repository:{value}"), text)
        } else {
            let path = self.path(&key, value)?.expect("непустой промпт проверен");
            let metadata = std::fs::metadata(&path).map_err(|error| {
                AssetError::with_details(
                    ErrorCode::InferenceConfigInvalid,
                    format!("{key}: файл промпта {} недоступен: {error}", path.display()),
                    json!({"reason": "prompt_missing"}),
                )
            })?;
            if !metadata.is_file() || metadata.len() > MAX_PROMPT_BYTES {
                return Err(config_error(format!(
                    "{key}: промпт должен быть обычным файлом не больше {MAX_PROMPT_BYTES} байт"
                )));
            }
            let bytes = std::fs::read(&path)
                .map_err(|error| AssetError::io(format!("промпт {}", path.display()), error))?;
            let text = String::from_utf8(bytes)
                .map_err(|_| config_error(format!("{key}: промпт должен быть UTF-8")))?;
            (path.display().to_string(), text)
        };
        if text.trim().is_empty() || text.contains('\0') {
            return Err(config_error(format!("{key}: промпт пуст или содержит NUL")));
        }
        Ok(ResolvedPrompt {
            source,
            sha256: sha256_hex(text.as_bytes()),
            text,
        })
    }

    /// Безопасная сводка: слои, источники ключей и итог каждой задачи.
    /// Секретов схема не содержит; пути и адреса — локальные данные пользователя.
    pub(crate) fn describe(&self, only: Option<TaskId>) -> Value {
        let mut tasks = serde_json::Map::new();
        for task in TaskId::ALL {
            if only.is_some_and(|only| only != task) {
                continue;
            }
            let entry = match self.resolve(task) {
                Ok(resolved) => {
                    let issues = resolved.readiness();
                    json!({
                        "status": if issues.is_empty() { "configured" } else { "not_configured" },
                        "backend_name": resolved.backend_name(),
                        "model_profile": resolved.model_profile(),
                        "backend": resolved.backend,
                        "model": resolved.model,
                        "prompt": resolved.prompt,
                        "generation": resolved.generation,
                        "issues": issues,
                    })
                }
                Err(error) => json!({
                    "status": "invalid",
                    "error": {"code": error.code.as_str(), "message": error.message},
                }),
            };
            tasks.insert(task.as_str().to_owned(), entry);
        }
        let origins: BTreeMap<_, _> = self
            .origins
            .iter()
            .filter(|(_, index)| self.layers[**index].kind != LayerKind::Repository)
            .map(|(key, index)| {
                (
                    key.clone(),
                    json!({
                        "layer": self.layers[*index].kind,
                        "path": self.layers[*index].path,
                    }),
                )
            })
            .collect();
        json!({
            "schema_version": SCHEMA_VERSION,
            "layers": self.layers,
            "overridden_keys": origins,
            "tasks": tasks,
        })
    }
}

fn validate_name(name: &str) -> Result<(), AssetError> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(config_error(format!("недопустимое имя профиля `{name}`")));
    }
    Ok(())
}

fn validate_text(key: &str, value: &str) -> Result<(), AssetError> {
    if value.len() > 4096 || value.chars().any(char::is_control) || value.trim() != value {
        return Err(config_error(format!(
            "{key}: значение содержит управляющие символы, пробелы по краям или слишком длинное"
        )));
    }
    Ok(())
}

fn in_range<T: PartialOrd + std::fmt::Display>(
    key: &str,
    value: T,
    min: T,
    max: T,
) -> Result<(), AssetError> {
    if value < min || value > max {
        return Err(config_error(format!(
            "{key} = {value} вне допустимого диапазона {min}..={max}"
        )));
    }
    Ok(())
}

fn validate_llama_backend(name: &str, raw: &RawLlamaBackend) -> Result<(), AssetError> {
    let prefix = format!("backends.{name}");
    in_range(
        &format!("{prefix}.connect_timeout_seconds"),
        raw.connect_timeout_seconds,
        1,
        120,
    )?;
    in_range(
        &format!("{prefix}.startup_timeout_seconds"),
        raw.startup_timeout_seconds,
        1,
        3600,
    )?;
    in_range(
        &format!("{prefix}.shutdown_timeout_seconds"),
        raw.shutdown_timeout_seconds,
        1,
        600,
    )?;
    validate_text(&format!("{prefix}.binary"), &raw.binary)?;
    let host: IpAddr = raw
        .host
        .parse()
        .map_err(|_| config_error(format!("{prefix}.host: нужен литеральный IP-адрес")))?;
    match raw.mode {
        LlamaMode::Managed => {
            if !host.is_loopback() {
                return Err(config_error(format!(
                    "{prefix}.host: управляемый сервер слушает только loopback"
                )));
            }
            if raw.port != 0 && raw.port < 1024 {
                return Err(config_error(format!(
                    "{prefix}.port: 0 (автовыбор) или 1024..=65535"
                )));
            }
            if !raw.endpoint.is_empty() {
                return Err(config_error(format!(
                    "{prefix}.endpoint задан, но mode = \"managed\": адрес выбирает менеджер"
                )));
            }
        }
        LlamaMode::External => {
            if !raw.binary.is_empty() {
                return Err(config_error(format!(
                    "{prefix}.binary задан, но mode = \"external\": внешний сервер не запускается"
                )));
            }
            if !raw.endpoint.is_empty() {
                endpoint(&raw.endpoint, raw.allow_remote, &prefix)?;
            }
        }
    }
    Ok(())
}

fn validate_llama_model(name: &str, raw: &RawLlamaModel) -> Result<(), AssetError> {
    let prefix = format!("models.{name}.llama_server");
    validate_text(&format!("{prefix}.model"), &raw.model)?;
    validate_text(&format!("{prefix}.projector"), &raw.projector)?;
    in_range(
        &format!("{prefix}.context_size"),
        raw.context_size,
        256,
        1_048_576,
    )?;
    in_range(&format!("{prefix}.threads"), raw.threads, 0, 512)?;
    in_range(&format!("{prefix}.batch_size"), raw.batch_size, 32, 65_536)?;
    in_range(
        &format!("{prefix}.ubatch_size"),
        raw.ubatch_size,
        32,
        65_536,
    )?;
    in_range(&format!("{prefix}.parallel"), raw.parallel, 1, 64)?;
    if raw.ubatch_size > raw.batch_size {
        return Err(config_error(format!(
            "{prefix}: ubatch_size не может превышать batch_size"
        )));
    }
    match raw.vision {
        VisionMode::Projector => {}
        VisionMode::Integrated | VisionMode::None if !raw.projector.is_empty() => {
            return Err(config_error(format!(
                "{prefix}.projector задан, но vision = \"{}\": projector используется только при vision = \"projector\"",
                serde_json::to_value(raw.vision)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default()
            )));
        }
        VisionMode::Integrated | VisionMode::None => {}
    }
    Ok(())
}

/// Базовый HTTP URL без credentials, пути, query и fragment; вне литерального
/// loopback — только с явным разрешением.
pub(crate) fn endpoint(value: &str, allow_remote: bool, key: &str) -> Result<Url, AssetError> {
    let url = Url::parse(value).map_err(|_| config_error(format!("{key}: неверный URL")))?;
    if url.scheme() != "http"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || !matches!(url.path(), "" | "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(config_error(format!(
            "{key}: нужен базовый HTTP URL без credentials, пути, query и fragment"
        )));
    }
    if !is_loopback(&url) && !allow_remote {
        return Err(config_error(format!(
            "{key}: адрес вне loopback требует allow_remote (--allow-remote): изображения будут отправлены этому серверу"
        )));
    }
    Ok(url)
}

/// Имена DNS не считаются loopback: DNS/hosts могут направить их наружу.
pub(crate) fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    }
}

/// `C:\dir\file` или `C:/dir/file` → `/mnt/c/dir/file` (стандартное
/// монтирование дисков WSL). Иные формы путей Windows не поддерживаются.
fn windows_drive_path(value: &str) -> Option<PathBuf> {
    let bytes = value.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || !matches!(bytes[2], b'\\' | b'/')
    {
        return None;
    }
    let rest = value[3..].replace('\\', "/");
    Some(
        PathBuf::from(format!("/mnt/{}", (bytes[0] as char).to_ascii_lowercase()))
            .join(rest.trim_start_matches('/')),
    )
}

/// Обратное преобразование для аргументов процесса Windows: `/mnt/c/a/b` → `C:\a\b`.
pub(crate) fn to_windows_path(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let rest = text.strip_prefix("/mnt/")?;
    let mut parts = rest.splitn(2, '/');
    let drive = parts.next()?;
    if drive.len() != 1 || !drive.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    let tail = parts.next().unwrap_or("");
    if tail.contains('\\') {
        return None;
    }
    Some(format!(
        "{}:\\{}",
        drive.to_ascii_uppercase(),
        tail.replace('/', "\\")
    ))
}

#[cfg(test)]
#[path = "inference_config_tests.rs"]
mod tests;
