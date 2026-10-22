use std::fs;

use super::*;
use crate::temp_workspace::TempWorkspace;

fn defaults() -> LoadedConfig {
    LoadedConfig::load(&ConfigSources {
        skip_local_discovery: true,
        ..ConfigSources::default()
    })
    .unwrap()
}

/// Синтетическая рабочая копия с маркером штатного файла и локальным слоем.
fn checkout(local: &str) -> TempWorkspace {
    let temp = TempWorkspace::create("inference-config").unwrap();
    let config = temp.path().join(CONFIG_DIRECTORY);
    fs::create_dir_all(config.join("prompts")).unwrap();
    fs::write(config.join("inference.toml"), "# маркер рабочей копии\n").unwrap();
    fs::write(config.join(LOCAL_FILE_NAME), local).unwrap();
    temp
}

fn load_with_local(temp: &TempWorkspace) -> Result<LoadedConfig, AssetError> {
    LoadedConfig::load(&ConfigSources {
        repository_root: temp.path().to_path_buf(),
        ..ConfigSources::default()
    })
}

fn error_of(result: Result<LoadedConfig, AssetError>) -> AssetError {
    match result {
        Ok(_) => panic!("ожидалась ошибка конфигурации"),
        Err(error) => error,
    }
}

#[test]
fn repository_defaults_are_complete_and_report_missing_weights_as_typed_outcome() {
    let loaded = defaults();
    for task in TaskId::ALL {
        let resolved = loaded.resolve(task).unwrap();
        assert!(
            matches!(resolved.backend, ResolvedBackend::LlamaServer(ref llama) if llama.mode == LlamaMode::Managed)
        );
        let codes: Vec<_> = resolved
            .readiness()
            .iter()
            .map(|issue| issue.code)
            .collect();
        assert_eq!(
            codes,
            [
                "backend_not_configured",
                "model_not_configured",
                "projector_not_configured"
            ]
        );
        let error = resolved.require_ready().unwrap_err();
        assert_eq!(error.code, ErrorCode::InferenceUnavailable);
        assert_eq!(error.details["reason"], "backend_not_configured");
    }
    let kanji = loaded.resolve(TaskId::KanjiOcr).unwrap();
    let jpdb = loaded.resolve(TaskId::JpdbPitchReview).unwrap();
    assert_eq!(kanji.model_profile(), "kanji_vision");
    assert_eq!(jpdb.model_profile(), "jpdb_vision");
    assert_eq!(kanji.prompt.source, "repository:prompts/kanji_ocr.txt");
    assert_ne!(kanji.prompt.sha256, jpdb.prompt.sha256);
}

#[test]
fn partial_local_override_changes_only_named_keys_and_resolves_paths_from_its_directory() {
    let temp = checkout(
        r#"
[backends.llama]
binary = "bin/llama-server"

[models.jpdb_vision.llama_server]
model = "weights/jpdb.gguf"
vision = "integrated"
context_size = 8192

[tasks.jpdb_pitch_review]
prompt = "my-prompt.txt"
temperature = 0.2
"#,
    );
    let config = temp.path().join(CONFIG_DIRECTORY);
    fs::write(config.join("my-prompt.txt"), "Look carefully.").unwrap();
    let loaded = load_with_local(&temp).unwrap();
    let jpdb = loaded.resolve(TaskId::JpdbPitchReview).unwrap();
    let ResolvedModel::LlamaServer(model) = &jpdb.model else {
        panic!("ожидался профиль llama.cpp");
    };
    assert_eq!(
        model.model.as_deref(),
        Some(config.join("weights/jpdb.gguf").as_path())
    );
    assert_eq!(model.vision, VisionMode::Integrated);
    assert_eq!(model.context_size, 8192);
    // Неупомянутые ключи остаются штатными.
    assert_eq!(model.batch_size, 512);
    assert_eq!(model.gpu_layers, GpuLayers::Auto);
    assert_eq!(jpdb.prompt.text, "Look carefully.");
    assert_eq!(
        jpdb.prompt.source,
        config.join("my-prompt.txt").display().to_string()
    );
    assert_eq!(jpdb.generation.temperature, 0.2);
    assert!(jpdb.readiness().is_empty());
    // Профиль Kanji независим: его модель, промпт и параметры не изменились.
    let kanji = loaded.resolve(TaskId::KanjiOcr).unwrap();
    let ResolvedModel::LlamaServer(kanji_model) = &kanji.model else {
        panic!("ожидался профиль llama.cpp");
    };
    assert!(kanji_model.model.is_none());
    assert_eq!(kanji_model.context_size, 4096);
    assert_eq!(kanji.generation.temperature, 0.0);
    assert_eq!(kanji.prompt.source, "repository:prompts/kanji_ocr.txt");
    let described = loaded.describe(None);
    assert_eq!(
        described["overridden_keys"]["models.jpdb_vision.llama_server.context_size"]["layer"],
        "local"
    );
    assert!(
        described["overridden_keys"]
            .get("models.kanji_vision.llama_server.context_size")
            .is_none()
    );
}

#[test]
fn layers_apply_in_documented_order() {
    let temp = checkout("[models.kanji_vision.llama_server]\ncontext_size = 2048\n");
    let explicit = temp.path().join("explicit.toml");
    fs::write(
        &explicit,
        "[models.kanji_vision.llama_server]\ncontext_size = 3072\nthreads = 6\n",
    )
    .unwrap();
    let sources = |cli: Vec<(String, toml::Value)>, explicit_file: Option<PathBuf>| ConfigSources {
        repository_root: temp.path().to_path_buf(),
        explicit_file,
        cli_values: cli,
        skip_local_discovery: false,
    };
    let context = |loaded: &LoadedConfig| match loaded.resolve(TaskId::KanjiOcr).unwrap().model {
        ResolvedModel::LlamaServer(model) => (model.context_size, model.threads),
        ResolvedModel::Ollama(_) => unreachable!(),
    };
    assert_eq!(
        context(&LoadedConfig::load(&sources(vec![], None)).unwrap()),
        (2048, 0)
    );
    assert_eq!(
        context(&LoadedConfig::load(&sources(vec![], Some(explicit.clone()))).unwrap()),
        (3072, 6)
    );
    let cli = vec![parse_assignment("models.kanji_vision.llama_server.context_size=6144").unwrap()];
    let loaded = LoadedConfig::load(&sources(cli, Some(explicit))).unwrap();
    assert_eq!(context(&loaded), (6144, 6));
    let described = loaded.describe(Some(TaskId::KanjiOcr));
    assert_eq!(
        described["overridden_keys"]["models.kanji_vision.llama_server.context_size"]["layer"],
        "cli"
    );
    assert_eq!(
        described["overridden_keys"]["models.kanji_vision.llama_server.threads"]["layer"],
        "explicit"
    );
}

#[test]
fn unknown_keys_types_ranges_and_references_are_rejected_before_any_model_starts() {
    for (local, fragment) in [
        (
            "[models.kanji_vision.llama_server]\nctx = 1\n",
            "unknown field",
        ),
        ("[tasks.kanji_ocr]\nmax_tokens = 5\n", "unknown field"),
        (
            "[tasks.translate]\nmodel = \"kanji_vision\"\n",
            "missing field",
        ),
        (
            "[models.kanji_vision.llama_server]\ncontext_size = 64\n",
            "context_size",
        ),
        (
            "[models.kanji_vision.llama_server]\nubatch_size = 1024\n",
            "ubatch_size",
        ),
        (
            "[models.kanji_vision.llama_server]\ngpu_layers = \"most\"\n",
            "gpu_layers",
        ),
        ("[tasks.kanji_ocr]\ntemperature = 3.0\n", "temperature"),
        ("[tasks.kanji_ocr]\nmodel = \"absent\"\n", "absent"),
        ("[models.kanji_vision]\nbackend = \"absent\"\n", "absent"),
        (
            "[models.kanji_vision.llama_server]\nvision = \"integrated\"\nprojector = \"x.mmproj\"\n",
            "projector",
        ),
        (
            "[models.kanji_vision.llama_server]\nvision = \"none\"\n",
            "vision",
        ),
        (
            "[backends.llama]\nmode = \"external\"\nbinary = \"llama-server\"\n",
            "binary",
        ),
        (
            "[backends.llama]\nendpoint = \"http://127.0.0.1:8080\"\n",
            "endpoint",
        ),
        ("[backends.llama]\nhost = \"0.0.0.0\"\n", "loopback"),
        ("[backends.llama]\nkind = \"vllm\"\n", "unknown variant"),
        (
            "[backends.ollama]\nendpoint = \"http://192.168.1.2:11434\"\n",
            "allow_remote",
        ),
        (
            "[backends.ollama]\nendpoint = \"http://user:pass@127.0.0.1:11434\"\n",
            "credentials",
        ),
        ("[runtime]\nmax_loaded_models = 2\n", "max_loaded_models"),
        ("[models]\nkanji_vision = 1\n", "таблица"),
    ] {
        let temp = checkout(local);
        let error = error_of(load_with_local(&temp));
        assert_eq!(
            error.code,
            ErrorCode::InferenceConfigInvalid,
            "{local}: {}",
            error.message
        );
        assert!(
            error.message.contains(fragment),
            "{local}: «{}» не содержит «{fragment}»",
            error.message
        );
    }
    let temp = checkout("schema_version = 2\n");
    assert_eq!(
        error_of(load_with_local(&temp)).code,
        ErrorCode::UnsupportedSchemaVersion
    );
}

#[test]
fn remote_endpoint_needs_explicit_consent_and_dns_names_are_not_loopback() {
    let temp = checkout(
        "[backends.ollama]\nendpoint = \"http://localhost:11434\"\nallow_remote = true\n[models.kanji_vision]\nbackend = \"ollama\"\n",
    );
    let resolved = load_with_local(&temp)
        .unwrap()
        .resolve(TaskId::KanjiOcr)
        .unwrap();
    let ResolvedBackend::Ollama(backend) = &resolved.backend else {
        panic!("ожидался Ollama");
    };
    assert!(!is_loopback(&backend.endpoint));
    let temp = checkout("[backends.ollama]\nendpoint = \"http://[::1]:11434\"\n");
    assert!(load_with_local(&temp).is_ok());
}

#[test]
fn prompt_file_must_exist_and_be_text() {
    let temp = checkout("[tasks.kanji_ocr]\nprompt = \"absent.txt\"\n");
    let error = load_with_local(&temp)
        .unwrap()
        .resolve(TaskId::KanjiOcr)
        .unwrap_err();
    assert_eq!(error.details["reason"], "prompt_missing");
    let temp = checkout("[tasks.kanji_ocr]\nprompt = \"empty.txt\"\n");
    fs::write(temp.path().join(CONFIG_DIRECTORY).join("empty.txt"), "  \n").unwrap();
    assert!(
        load_with_local(&temp)
            .unwrap()
            .resolve(TaskId::KanjiOcr)
            .is_err()
    );
    // Встроенный слой не читает файлов вне встроенного набора.
    let loaded = LoadedConfig::load(&ConfigSources {
        skip_local_discovery: true,
        cli_values: vec![],
        ..ConfigSources::default()
    })
    .unwrap();
    assert!(loaded.resolve(TaskId::KanjiOcr).is_ok());
}

#[test]
fn cli_assignment_parses_toml_literals_and_falls_back_to_strings() {
    assert_eq!(
        parse_assignment("tasks.kanji_ocr.temperature=0.5")
            .unwrap()
            .1,
        toml::Value::Float(0.5)
    );
    assert_eq!(
        parse_assignment("backends.llama.windows_interop=true")
            .unwrap()
            .1,
        toml::Value::Boolean(true)
    );
    assert_eq!(
        parse_assignment("models.kanji_vision.llama_server.model=/w/model.gguf")
            .unwrap()
            .1,
        toml::Value::String("/w/model.gguf".into())
    );
    assert_eq!(
        parse_assignment("models.kanji_vision.llama_server.gpu_layers=\"all\"")
            .unwrap()
            .1,
        toml::Value::String("all".into())
    );
    for invalid in ["no-equals", "=1", "a..b=1", "a.$b=1"] {
        assert_eq!(
            parse_assignment(invalid).unwrap_err().code,
            ErrorCode::InferenceConfigInvalid
        );
    }
}

#[test]
fn windows_paths_round_trip_only_for_drive_mounts() {
    assert_eq!(
        windows_drive_path(r"C:\Users\me\model.gguf"),
        Some(PathBuf::from("/mnt/c/Users/me/model.gguf"))
    );
    assert_eq!(
        windows_drive_path("D:/models/a.gguf"),
        Some(PathBuf::from("/mnt/d/models/a.gguf"))
    );
    assert_eq!(windows_drive_path("/home/me/a.gguf"), None);
    assert_eq!(windows_drive_path(r"\\server\share"), None);
    assert_eq!(
        to_windows_path(Path::new("/mnt/c/Users/me/model.gguf")).as_deref(),
        Some(r"C:\Users\me\model.gguf")
    );
    assert_eq!(to_windows_path(Path::new("/home/me/model.gguf")), None);
    assert_eq!(to_windows_path(Path::new("/mnt/wsl/x")), None);
}

#[test]
fn documented_local_example_is_a_valid_partial_override() {
    let temp = checkout(include_str!("../config/inference.local.example.toml"));
    let loaded = load_with_local(&temp).unwrap();
    for task in TaskId::ALL {
        let resolved = loaded.resolve(task).unwrap();
        let ResolvedBackend::LlamaServer(llama) = &resolved.backend else {
            panic!("образец оставляет llama.cpp основным backend");
        };
        assert!(llama.windows_interop);
        assert!(
            resolved.readiness().is_empty(),
            "{:?}",
            resolved.readiness()
        );
    }
}
