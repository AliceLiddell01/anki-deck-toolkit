//! Общие аргументы конфигурации inference и команды `inference` обоих CLI.
//!
//! `kanji-assets` и `pitch-assets` подключают одни и те же аргументы и один
//! исполнитель команд: конфигурация описана один раз и не дублируется.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{Value, json};

use crate::error::{AssetError, ErrorCode};
use crate::hashing::sha256_hex;
use crate::inference::{InferenceSession, SessionReport};
use crate::inference_config::{
    ConfigSources, LoadedConfig, ResolvedBackend, ResolvedTask, TaskId, parse_assignment,
};

const MAX_PROBE_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_PROBE_IMAGES: usize = 2000;

/// Источники и точечные переопределения конфигурации inference.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct InferenceArgs {
    /// Дополнительный файл переопределений поверх штатного и локального
    /// `tools/asset-store/config/inference.local.toml`.
    #[arg(long, value_name = "PATH")]
    pub inference_config: Option<PathBuf>,
    /// Переопределение одного ключа, например
    /// `models.kanji_vision.llama_server.context_size=8192`; можно повторять.
    #[arg(long = "inference-set", value_name = "KEY=VALUE")]
    pub inference_set: Vec<String>,
    /// Имя backend для профиля модели задачи (`llama`, `ollama` или свой).
    #[arg(long, value_name = "NAME")]
    pub backend: Option<String>,
    /// Модель профиля задачи: путь к GGUF для llama.cpp или имя модели Ollama.
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,
    /// Базовый URL backend (Ollama или внешний llama-server).
    #[arg(long, value_name = "URL")]
    pub endpoint: Option<String>,
    /// Разрешает отправку изображений адресу вне литерального loopback.
    #[arg(long)]
    pub allow_remote: bool,
    /// Предел одного запроса к модели, секунды.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
    pub timeout_seconds: Option<u64>,
    /// Корень рабочей копии; заполняет CLI из `--repository-root`.
    #[arg(skip)]
    pub repository_root: PathBuf,
}

impl InferenceArgs {
    fn has_shortcuts(&self) -> bool {
        self.backend.is_some()
            || self.model.is_some()
            || self.endpoint.is_some()
            || self.allow_remote
            || self.timeout_seconds.is_some()
    }

    /// Синтаксическая проверка до открытия хранилища.
    pub(crate) fn validate_syntax(&self) -> Result<(), AssetError> {
        for assignment in &self.inference_set {
            parse_assignment(assignment)?;
        }
        Ok(())
    }

    fn base_sources(&self) -> Result<ConfigSources, AssetError> {
        Ok(ConfigSources {
            repository_root: self.repository_root.clone(),
            explicit_file: self.inference_config.clone(),
            cli_values: self
                .inference_set
                .iter()
                .map(|assignment| parse_assignment(assignment))
                .collect::<Result<_, _>>()?,
            skip_local_discovery: false,
        })
    }

    /// Загружает слои; удобные флаги применяются к профилю и backend задачи.
    pub(crate) fn load(&self, task: Option<TaskId>) -> Result<LoadedConfig, AssetError> {
        let mut sources = self.base_sources()?;
        if !self.has_shortcuts() {
            return LoadedConfig::load(&sources);
        }
        let task = task.ok_or_else(|| {
            AssetError::new(
                ErrorCode::InferenceConfigInvalid,
                "--backend/--model/--endpoint/--allow-remote/--timeout-seconds требуют задачу (--task)",
            )
        })?;
        let initial = LoadedConfig::load(&sources)?.resolve(task)?;
        let profile = initial.model_profile().to_owned();
        if let Some(backend) = &self.backend {
            sources.cli_values.push((
                format!("models.{profile}.backend"),
                toml::Value::String(backend.clone()),
            ));
        }
        let resolved = LoadedConfig::load(&sources)?.resolve(task)?;
        let backend = resolved.backend_name().to_owned();
        let section = match resolved.backend {
            ResolvedBackend::LlamaServer(_) => "llama_server",
            ResolvedBackend::Ollama(_) => "ollama",
        };
        if let Some(model) = &self.model {
            sources.cli_values.push((
                format!("models.{profile}.{section}.model"),
                toml::Value::String(model.clone()),
            ));
        }
        if let Some(endpoint) = &self.endpoint {
            sources.cli_values.push((
                format!("backends.{backend}.endpoint"),
                toml::Value::String(endpoint.clone()),
            ));
        }
        if self.allow_remote {
            sources.cli_values.push((
                format!("backends.{backend}.allow_remote"),
                toml::Value::Boolean(true),
            ));
        }
        if let Some(timeout) = self.timeout_seconds {
            sources.cli_values.push((
                format!("tasks.{}.timeout_seconds", task.as_str()),
                toml::Value::Integer(timeout as i64),
            ));
        }
        LoadedConfig::load(&sources)
    }

    pub(crate) fn resolve(&self, task: TaskId) -> Result<ResolvedTask, AssetError> {
        self.load(Some(task))?.resolve(task)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TaskArg {
    KanjiOcr,
    JpdbPitchReview,
}

impl From<TaskArg> for TaskId {
    fn from(value: TaskArg) -> Self {
        match value {
            TaskArg::KanjiOcr => Self::KanjiOcr,
            TaskArg::JpdbPitchReview => Self::JpdbPitchReview,
        }
    }
}

/// Команды диагностики и проверки локального inference.
#[derive(Debug, Clone, Subcommand)]
pub enum InferenceCommand {
    /// Итоговые настройки: слои, источники ключей, задачи, backend и модели.
    Show {
        #[arg(long, value_enum)]
        task: Option<TaskArg>,
        #[command(flatten)]
        inference: InferenceArgs,
    },
    /// Запускает backend задачи, подтверждает модель и vision, затем корректно останавливает.
    Check {
        #[arg(long, value_enum)]
        task: TaskArg,
        #[command(flatten)]
        inference: InferenceArgs,
    },
    /// Выполняет задачу над явно указанными локальными изображениями без записи
    /// в хранилище, пакет или review. Для диагностики модели и промпта.
    Probe {
        #[arg(long, value_enum)]
        task: TaskArg,
        /// Локальный файл изображения; можно повторять.
        #[arg(long = "image", value_name = "PATH", required = true)]
        images: Vec<PathBuf>,
        #[command(flatten)]
        inference: InferenceArgs,
    },
}

impl InferenceCommand {
    pub(crate) fn operation(&self) -> &'static str {
        match self {
            Self::Show { .. } => "inference_show",
            Self::Check { .. } => "inference_check",
            Self::Probe { .. } => "inference_probe",
        }
    }

    pub(crate) fn set_repository_root(&mut self, root: &Path) {
        let inference = match self {
            Self::Show { inference, .. }
            | Self::Check { inference, .. }
            | Self::Probe { inference, .. } => inference,
        };
        inference.repository_root = root.to_path_buf();
    }
}

/// Результат команды `inference` для рендеринга в CLI.
pub(crate) struct InferenceOutput {
    pub value: Value,
    pub exit_code: u8,
}

fn error_value(operation: &str, error: &AssetError) -> Value {
    json!({
        "schema_version": 1,
        "operation": operation,
        "outcome": "error",
        "error": {"code": error.code.as_str(), "message": error.message, "details": error.details},
    })
}

/// Исполняет команду `inference`; JSON-ответ общий для обоих CLI.
pub(crate) fn execute(command: &InferenceCommand) -> InferenceOutput {
    let operation = command.operation();
    let result = match command {
        InferenceCommand::Show { task, inference } => show(task.map(TaskId::from), inference),
        InferenceCommand::Check { task, inference } => {
            run_blocking(check((*task).into(), inference))
        }
        InferenceCommand::Probe {
            task,
            images,
            inference,
        } => run_blocking(probe((*task).into(), images, inference)),
    };
    match result {
        Ok(output) => output,
        Err(error) => InferenceOutput {
            exit_code: error.exit_code(),
            value: error_value(operation, &error),
        },
    }
}

/// Команды `inference` исполняются в собственной однопоточной среде tokio;
/// если вызывающий уже внутри среды выполнения — в отдельном потоке.
fn run_blocking<F>(future: F) -> Result<InferenceOutput, AssetError>
where
    F: std::future::Future<Output = Result<InferenceOutput, AssetError>> + Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| AssetError::io("среда выполнения inference", error))?
                    .block_on(future)
            })
            .join()
            .unwrap_or_else(|_| Err(AssetError::new(ErrorCode::IoFailure, "паника в inference")))
    })
}

fn show(task: Option<TaskId>, inference: &InferenceArgs) -> Result<InferenceOutput, AssetError> {
    let loaded = inference.load(task)?;
    let mut value = loaded.describe(task);
    value["operation"] = json!("inference_show");
    value["outcome"] = json!("ok");
    Ok(InferenceOutput {
        value,
        exit_code: 0,
    })
}

fn session_value(report: &SessionReport) -> Value {
    serde_json::to_value(report).unwrap_or(Value::Null)
}

async fn check(task: TaskId, inference: &InferenceArgs) -> Result<InferenceOutput, AssetError> {
    let resolved = inference.resolve(task)?;
    resolved.require_ready()?;
    let (contract, transform) = task_contract(task);
    match InferenceSession::prepare(resolved, contract, transform).await {
        Ok(session) => {
            let provenance = session.provenance().clone();
            let report = session.finish().await;
            let proven = report.stop.as_ref().is_none_or(|stop| stop.proven());
            Ok(InferenceOutput {
                exit_code: if proven { 0 } else { 3 },
                value: json!({
                    "schema_version": 1,
                    "operation": "inference_check",
                    "outcome": if proven { "ready" } else { "stop_unproven" },
                    "task": task,
                    "provenance": provenance,
                    "session": session_value(&report),
                }),
            })
        }
        Err(error) => {
            let (failure, report) = *error;
            Ok(InferenceOutput {
                exit_code: if failure.is_interrupted() { 130 } else { 3 },
                value: json!({
                    "schema_version": 1,
                    "operation": "inference_check",
                    "outcome": "unavailable",
                    "task": task,
                    "failure": failure,
                    "session": session_value(&report),
                }),
            })
        }
    }
}

fn task_contract(task: TaskId) -> (crate::inference::ResponseContract, &'static str) {
    match task {
        TaskId::KanjiOcr => (
            crate::kanji_ocr::contract(),
            crate::kanji_ocr::TRANSFORM_VERSION,
        ),
        TaskId::JpdbPitchReview => (
            crate::pitch_llm_review::contract(),
            crate::pitch_llm_review::TRANSFORM_VERSION,
        ),
    }
}

/// Подготовленное изображение и предметная интерпретация ответа для probe.
fn prepare_probe(task: TaskId, bytes: &[u8]) -> Result<(Vec<u8>, Value), String> {
    match task {
        TaskId::KanjiOcr => crate::kanji_ocr::prepare_for_probe(bytes),
        TaskId::JpdbPitchReview => crate::pitch_llm_review::prepare_image(bytes)
            .map(|prepared| (prepared.png, json!({"dimensions": prepared.dimensions}))),
    }
}

fn interpret_probe(task: TaskId, content: &str) -> Result<Value, String> {
    match task {
        TaskId::KanjiOcr => {
            crate::kanji_ocr::interpret(content).map(|observed| json!({"observed": observed}))
        }
        TaskId::JpdbPitchReview => crate::pitch_llm_review::interpret(content)
            .map(|assessment| serde_json::to_value(assessment).unwrap_or(Value::Null)),
    }
}

/// Канонические каталоги `decks/` среди предков корня рабочей копии, текущего
/// каталога и каждого изображения (в указанном и каноническом виде): граница
/// не зависит от того, найдена ли рабочая копия по `repository_root`.
fn protected_decks_roots(repository_root: &Path, images: &[PathBuf]) -> Vec<PathBuf> {
    let mut starts = vec![repository_root.to_path_buf()];
    starts.extend(std::env::current_dir().ok());
    for image in images {
        starts.extend(std::path::absolute(image).ok());
        starts.extend(std::fs::canonicalize(image).ok());
    }
    let mut roots = std::collections::BTreeSet::new();
    for start in starts
        .iter()
        .filter_map(|start| std::path::absolute(start).ok())
    {
        for ancestor in start.ancestors() {
            if let Ok(decks) = std::fs::canonicalize(ancestor.join("decks"))
                && decks.is_dir()
            {
                roots.insert(decks);
            }
        }
    }
    roots.into_iter().collect()
}

fn read_probe_image(path: &Path, protected: &[PathBuf]) -> Result<Vec<u8>, String> {
    let canonical =
        std::fs::canonicalize(path).map_err(|error| format!("image_unreadable: {error}"))?;
    if protected.iter().any(|decks| canonical.starts_with(decks)) {
        return Err("boundary_violation: изображения из decks/ не читаются".into());
    }
    let metadata =
        std::fs::metadata(&canonical).map_err(|error| format!("image_unreadable: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_PROBE_IMAGE_BYTES {
        return Err("image_invalid: нужен обычный файл не больше 8 МиБ".into());
    }
    std::fs::read(&canonical).map_err(|error| format!("image_unreadable: {error}"))
}

async fn probe(
    task: TaskId,
    images: &[PathBuf],
    inference: &InferenceArgs,
) -> Result<InferenceOutput, AssetError> {
    if images.len() > MAX_PROBE_IMAGES {
        return Err(AssetError::new(
            ErrorCode::InvalidTransition,
            format!("не больше {MAX_PROBE_IMAGES} изображений за запуск"),
        ));
    }
    let resolved = inference.resolve(task)?;
    resolved.require_ready()?;
    let protected = protected_decks_roots(&inference.repository_root, images);
    // Все входы читаются и проверяются до запуска модели.
    let mut prepared = Vec::with_capacity(images.len());
    for path in images {
        let entry = read_probe_image(path, &protected).and_then(|bytes| {
            let source = sha256_hex(&bytes);
            prepare_probe(task, &bytes).map(|(png, details)| (source, png, details))
        });
        prepared.push((path, entry));
    }
    let (contract, transform) = task_contract(task);
    let mut session = match InferenceSession::prepare(resolved, contract, transform).await {
        Ok(session) => session,
        Err(error) => {
            let (failure, report) = *error;
            return Ok(InferenceOutput {
                exit_code: if failure.is_interrupted() { 130 } else { 3 },
                value: json!({
                    "schema_version": 1,
                    "operation": "inference_probe",
                    "outcome": "unavailable",
                    "task": task,
                    "failure": failure,
                    "session": session_value(&report),
                }),
            });
        }
    };
    let provenance = session.provenance().clone();
    let mut results = Vec::with_capacity(prepared.len());
    let mut failures = 0;
    let mut interrupted = false;
    for (path, entry) in prepared {
        let (source, png, details) = match entry {
            Ok(entry) => entry,
            Err(error) => {
                failures += 1;
                results.push(json!({"image": path, "failure": error}));
                continue;
            }
        };
        if session.interrupted() {
            interrupted = true;
            break;
        }
        let prepared_sha256 = sha256_hex(&png);
        match session.run(&png).await {
            Ok(response) => {
                let interpreted = interpret_probe(task, &response.content);
                if interpreted.is_err() {
                    failures += 1;
                }
                let raw: String = response.content.chars().take(2000).collect();
                results.push(json!({
                    "image": path,
                    "source_sha256": source,
                    "prepared_sha256": prepared_sha256,
                    "preparation": details,
                    "raw_response": raw,
                    "result": interpreted.as_ref().ok(),
                    "failure": interpreted.err(),
                    "timings": response.timings,
                }));
            }
            Err(failure) if failure.is_interrupted() => {
                interrupted = true;
                break;
            }
            Err(failure) => {
                failures += 1;
                results.push(json!({
                    "image": path,
                    "source_sha256": source,
                    "prepared_sha256": prepared_sha256,
                    "failure": failure.code,
                    "message": failure.message,
                }));
            }
        }
    }
    let report = session.finish().await;
    let proven = report.stop.as_ref().is_none_or(|stop| stop.proven());
    let exit_code = if interrupted {
        130
    } else if failures > 0 || !proven {
        3
    } else {
        0
    };
    Ok(InferenceOutput {
        exit_code,
        value: json!({
            "schema_version": 1,
            "operation": "inference_probe",
            "outcome": if interrupted { "interrupted" } else if failures > 0 { "completed_with_failures" } else { "completed" },
            "task": task,
            "provenance": provenance,
            "results": results,
            "session": session_value(&report),
        }),
    })
}

/// Человекочитаемая сводка ответа команды `inference`.
/// Пара `(stdout, stderr)` для обоих CLI: человекочитаемая ошибка, как и у
/// остальных команд, идёт в `stderr`, а `stdout` остаётся пустым.
pub(crate) fn render(output: &InferenceOutput, json: bool) -> (String, String) {
    if json {
        let text = serde_json::to_string_pretty(&output.value).unwrap_or_else(|_| "{}".into());
        return (format!("{text}\n"), String::new());
    }
    let summary = human_summary(&output.value);
    if output.value.get("error").is_some() {
        (String::new(), summary)
    } else {
        (summary, String::new())
    }
}

pub(crate) fn human_summary(value: &Value) -> String {
    let mut text = format!(
        "{}: {}\n",
        value["operation"].as_str().unwrap_or("inference"),
        value["outcome"].as_str().unwrap_or("?")
    );
    if let Some(error) = value.get("error") {
        text.push_str(&format!(
            "ошибка {}: {}\n",
            error["code"].as_str().unwrap_or("?"),
            error["message"].as_str().unwrap_or("")
        ));
    }
    if let Some(tasks) = value["tasks"].as_object() {
        for (name, task) in tasks {
            text.push_str(&format!(
                "задача {name}: {}; backend {}; профиль {}; промпт {}\n",
                task["status"].as_str().unwrap_or("?"),
                task["backend_name"].as_str().unwrap_or("?"),
                task["model_profile"].as_str().unwrap_or("?"),
                task["prompt"]["source"].as_str().unwrap_or("?"),
            ));
            for issue in task["issues"].as_array().into_iter().flatten() {
                text.push_str(&format!(
                    "  {} ({}): {}\n",
                    issue["code"].as_str().unwrap_or("?"),
                    issue["key"].as_str().unwrap_or("?"),
                    issue["hint"].as_str().unwrap_or("")
                ));
            }
        }
    }
    if let Some(failure) = value.get("failure").filter(|failure| !failure.is_null()) {
        text.push_str(&format!(
            "отказ {}: {}\n",
            failure["code"].as_str().unwrap_or("?"),
            failure["message"].as_str().unwrap_or("")
        ));
    }
    if let Some(provenance) = value.get("provenance") {
        text.push_str(&format!(
            "модель {} ({}); backend {} {}\n",
            provenance["model"]["name"].as_str().unwrap_or("?"),
            provenance["model"]["weights"].as_str().unwrap_or("?"),
            provenance["backend"]["kind"].as_str().unwrap_or("?"),
            provenance["backend"]["version"].as_str().unwrap_or("?"),
        ));
    }
    if let Some(results) = value["results"].as_array() {
        for result in results {
            text.push_str(&format!(
                "{}: {}\n",
                result["image"].as_str().unwrap_or("?"),
                if result["failure"].is_null() {
                    result["result"].to_string()
                } else {
                    format!("отказ {}", result["failure"])
                }
            ));
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::temp_workspace::TempWorkspace;

    #[test]
    fn human_error_goes_to_stderr_and_success_to_stdout() {
        let failed = InferenceOutput {
            exit_code: 3,
            value: json!({
                "operation": "inference_check",
                "outcome": "failed",
                "error": {"code": "inference_config_invalid", "message": "неизвестный ключ"},
            }),
        };
        let (stdout, stderr) = render(&failed, false);
        assert!(stdout.is_empty());
        assert!(stderr.contains("ошибка inference_config_invalid: неизвестный ключ"));
        let (stdout, stderr) = render(&failed, true);
        assert!(stderr.is_empty());
        assert_eq!(
            serde_json::from_str::<Value>(&stdout).unwrap()["outcome"],
            "failed"
        );
        let ready = InferenceOutput {
            exit_code: 0,
            value: json!({"operation": "inference_check", "outcome": "ready"}),
        };
        let (stdout, stderr) = render(&ready, false);
        assert_eq!(stdout, "inference_check: ready\n");
        assert!(stderr.is_empty());
    }

    #[test]
    fn probe_rejects_decks_images_outside_the_configured_checkout() {
        let workspace = TempWorkspace::create("inference-probe-boundary").unwrap();
        let checkout = workspace.path().join("other-checkout");
        let media = checkout.join("decks/output/Words/media");
        std::fs::create_dir_all(&media).unwrap();
        let image = media.join("kanji.gif");
        std::fs::write(&image, b"GIF89a").unwrap();
        let outside = workspace.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let link = outside.join("link.gif");
        std::os::unix::fs::symlink(&image, &link).unwrap();
        let plain = outside.join("plain.gif");
        std::fs::write(&plain, b"GIF89a").unwrap();
        // `repository_root` указывает вне рабочей копии с изображениями.
        let unrelated = workspace.path().join("unrelated");
        std::fs::create_dir_all(&unrelated).unwrap();
        let images = vec![image.clone(), link.clone(), plain.clone()];
        let protected = protected_decks_roots(&unrelated, &images);
        for (label, path) in [("decks", &image), ("symlink", &link)] {
            let error = read_probe_image(path, &protected).unwrap_err();
            assert!(error.starts_with("boundary_violation"), "{label}");
        }
        assert_eq!(read_probe_image(&plain, &protected).unwrap(), b"GIF89a");
    }
}
