//! Контракт локальных данных репозитория.
//!
//! Файл проверяет две границы, которые не принадлежат ни одной предметной
//! команде:
//!
//! * `decks/input/` — закрытая область будущего доверенного загрузчика. Ни одна
//!   команда `anki-repo` не имеет права её открыть, и отказ обязан быть
//!   объяснимым, а не «файл не найден»;
//! * `anki-repo migrate-layout` — контролируемая миграция прежней раскладки в
//!   плоский `decks/output/`, которая не должна терять байты, перезаписывать
//!   чужое назначение и повторяться идемпотентно.
//!
//! Ни один тест не знает фактического состава `decks/`: каждая фикстура
//! строится в собственном временном каталоге с синтетическим `deck.json`.

use std::fs;
use std::path::{Path, PathBuf};

use common::{TempDir, parse_json, run_cli};
use serde_json::Value;

use crate::common;
use anki_repo::error::ErrorCode;
use anki_repo::ops::edit::{EditRequest, EditSpec};

/// Тело синтетического `deck.json`: постоянное, чтобы одинаковые экспорты
/// давали один и тот же отпечаток содержимого.
const DECK_JSON_BODY: &str = r#"{"__type__":"Deck","name":"Локальная колода"}"#;

/// Репозиторий-фикстура с признаком `.git` и пустым `decks/`.
struct Repo {
    _dir: TempDir,
    root: PathBuf,
}

impl Repo {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let root = dir.path().join("repo");
        fs::create_dir_all(root.join(".git")).expect("признак репозитория");
        fs::create_dir_all(root.join("decks")).expect("каталог decks");
        Self { _dir: dir, root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// Создаёт синтетический экспорт по относительному пути.
    fn export(&self, relative: &str, payload: &[u8]) -> PathBuf {
        let directory = self.path(relative);
        fs::create_dir_all(directory.join("media")).expect("каталог экспорта");
        fs::write(directory.join("deck.json"), DECK_JSON_BODY).expect("deck.json");
        fs::write(directory.join("media").join("a.bin"), payload).expect("media");
        directory
    }
}

/// Аргументы запуска `migrate-layout` для корня репозитория.
fn migrate_args(repo: &Repo, apply: bool) -> Vec<String> {
    let mut args = vec![
        "migrate-layout".to_string(),
        "--root".to_string(),
        repo.root().display().to_string(),
    ];
    if apply {
        args.push("--apply".to_string());
    }
    args
}

/// Запускает `migrate-layout` с JSON-выводом.
fn run_migrate(repo: &Repo, apply: bool) -> (i32, Value, String) {
    let mut args = migrate_args(repo, apply);
    args.push("--json".to_string());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let (code, stdout, stderr) = run_cli(&borrowed);
    let value = if stdout.trim().is_empty() {
        Value::Null
    } else {
        parse_json(&stdout)
    };
    (code, value, stderr)
}

/// Ошибка команды в JSON-конверте.
fn error_reason(value: &Value) -> (&str, &str) {
    let error = &value["error"];
    (
        error["code"].as_str().unwrap_or(""),
        error["details"]["reason"].as_str().unwrap_or(""),
    )
}

#[test]
fn migrate_layout_dry_run_is_the_default_and_changes_nothing() {
    let repo = Repo::new("local-data-dry-run");
    let payload = vec![5_u8; 2048];
    repo.export("decks/japanese/words/Words__A", &payload);

    let (code, value, stderr) = run_migrate(&repo, false);

    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["command"], "migrate-layout");
    assert_eq!(value["schema_version"], 1);
    let result = &value["result"];
    assert_eq!(result["apply"], false);
    assert_eq!(result["applied"], false);
    assert_eq!(result["summary"]["planned"], 1);
    assert_eq!(result["refused"], false);
    assert_eq!(
        result["output_dir"], "decks/output",
        "назначение описывается относительно корня репозитория"
    );
    assert!(
        repo.path("decks/japanese/words/Words__A/deck.json")
            .is_file(),
        "dry-run не меняет раскладку"
    );
    assert!(!repo.path("decks/output").exists());
}

#[test]
fn migrate_layout_apply_moves_exports_and_preserves_bytes() {
    let repo = Repo::new("local-data-apply");
    let payload = vec![9_u8; 3072];
    repo.export("decks/japanese/words/Words__A", &payload);
    fs::write(repo.path("decks/japanese/words/.gitkeep"), b"").expect("placeholder");

    let (code, value, stderr) = run_migrate(&repo, true);

    assert_eq!(code, 0, "stderr: {stderr}");
    let result = &value["result"];
    assert_eq!(result["applied"], true);
    assert_eq!(result["summary"]["applied"], 1);
    assert_eq!(
        fs::read(repo.path("decks/output/Words__A/media/a.bin")).expect("media"),
        payload,
        "байты перенесены без изменений"
    );
    assert!(!repo.path("decks/japanese/words/Words__A").exists());
    assert!(
        !repo.path("decks/japanese").exists(),
        "placeholder и опустевшие каталоги прежней раскладки сняты"
    );

    let (again_code, again, _) = run_migrate(&repo, true);
    assert_eq!(again_code, 0);
    assert_eq!(again["result"]["summary"]["exports"], 0);
    assert_eq!(again["result"]["applied"], false);
    assert!(
        again["result"]["diagnostics"]
            .as_array()
            .expect("диагностика")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "nothing_to_migrate"),
        "повтор объясняет, что мигрировать нечего"
    );
}

#[test]
fn migrate_layout_refuses_a_name_collision_without_moving_anything() {
    let repo = Repo::new("local-data-collision");
    repo.export("decks/words/Words__A", b"a");
    repo.export("decks/other/WORDS__a", b"b");

    let (code, value, _) = run_migrate(&repo, true);

    assert_eq!(code, 3, "отказ — отдельный код возврата, а не успех");
    assert_eq!(value["result"]["refused"], true);
    assert_eq!(value["result"]["summary"]["applied"], 0);
    assert!(
        repo.path("decks/words/Words__A/deck.json").is_file(),
        "ни один экспорт не перенесён"
    );
    assert!(!repo.path("decks/output").exists());
}

#[test]
fn migrate_layout_never_touches_the_closed_input_area() {
    let repo = Repo::new("local-data-input");
    repo.export("decks/input/Hidden", b"hidden");
    repo.export("decks/words/Words__A", b"a");

    let (code, value, _) = run_migrate(&repo, true);

    assert_eq!(code, 0);
    assert_eq!(value["result"]["summary"]["exports"], 1);
    assert!(
        repo.path("decks/input/Hidden/deck.json").is_file(),
        "закрытая область не тронута"
    );
    assert!(!repo.path("decks/output/Hidden").exists());
}

#[test]
fn migrate_layout_rejects_output_redirected_into_input_before_any_write() {
    let repo = Repo::new("local-data-output-input-alias");
    let source = repo.export("decks/words/Words__A", b"source");
    let input = repo.path("decks/input");
    fs::create_dir_all(&input).expect("закрытая область");
    fs::write(input.join("sentinel.bin"), b"private").expect("маркер закрытой области");
    let placeholder = repo.path("decks/.gitkeep");
    fs::write(&placeholder, b"").expect("placeholder прежней раскладки");
    let output = repo.path("decks/output");
    std::os::unix::fs::symlink(&input, &output).expect("назначение перенаправлено в input");

    let (code, value, stderr) = run_migrate(&repo, true);

    assert_eq!(code, 3, "stderr: {stderr}");
    assert_eq!(
        error_reason(&value),
        ("invalid_request", "input_area_forbidden")
    );
    assert!(source.join("deck.json").is_file(), "источник не перенесён");
    assert!(placeholder.is_file(), "placeholder не удалён");
    assert_eq!(
        fs::read(input.join("sentinel.bin")).expect("закрытая область не изменена"),
        b"private"
    );
    assert!(
        fs::symlink_metadata(&output)
            .expect("ссылка назначения сохранена")
            .file_type()
            .is_symlink()
    );
    assert!(
        !input.join("Words__A").exists(),
        "в input ничего не записано"
    );
}

#[test]
fn migrate_layout_rejects_a_symlinked_intermediate_decks_component() {
    let repo = Repo::new("local-data-decks-intermediate-alias");
    let decks = repo.path("decks");
    let external_decks = repo.path("outside/decks");
    fs::create_dir_all(external_decks.parent().expect("родитель decks")).expect("внешний каталог");
    fs::rename(&decks, &external_decks).expect("перенести каталог для symlink-фикстуры");
    let source = external_decks.join("words/Words__A");
    fs::create_dir_all(source.join("media")).expect("экспорт");
    fs::write(source.join("deck.json"), DECK_JSON_BODY).expect("deck.json");
    fs::write(source.join("media/a.bin"), b"source").expect("media");
    let input = external_decks.join("input");
    fs::create_dir_all(&input).expect("закрытая область");
    fs::write(input.join("sentinel.bin"), b"private").expect("маркер закрытой области");
    let placeholder = external_decks.join(".gitkeep");
    fs::write(&placeholder, b"").expect("placeholder");
    std::os::unix::fs::symlink(&external_decks, &decks).expect("промежуточная ссылка decks");

    let (code, value, stderr) = run_migrate(&repo, true);

    assert_eq!(code, 3, "stderr: {stderr}");
    assert_eq!(
        error_reason(&value),
        ("invalid_request", "layout_path_is_symlink")
    );
    assert!(source.join("deck.json").is_file(), "источник не перенесён");
    assert!(placeholder.is_file(), "placeholder не удалён");
    assert_eq!(
        fs::read(input.join("sentinel.bin")).expect("внешняя область не изменена"),
        b"private"
    );
    assert!(!external_decks.join("output/Words__A").exists());
}

#[test]
fn every_export_directory_command_refuses_the_closed_input_area() {
    let repo = Repo::new("local-data-guard");
    let hidden = repo.export("decks/input/Hidden", b"hidden");
    let path = hidden.display().to_string();
    let report = repo.path("report").display().to_string();
    let proposals = repo.path("decks/input/Hidden/deck.json");
    let proposals = proposals.display().to_string();

    let cases: Vec<Vec<&str>> = vec![
        vec!["inspect", &path],
        vec!["stats", &path],
        vec!["validate", &path],
        vec!["qa", &path],
        vec!["models", &path],
        vec!["find", &path, "--guid", "g"],
        vec!["review", &path, "--all"],
        vec!["review-check", &path, "--proposals", &proposals],
        vec![
            "edit", &path, "--guid", "g", "--field", "f", "--set", "v", "--expect", "e",
        ],
        vec!["retire", &path, "--guid", "g", "--tag", "t"],
        vec!["create", &path, "--request", &proposals],
        vec![
            "migrate-media",
            &path,
            "--namespace",
            "n",
            "--key",
            "k",
            "--from",
            "f.png",
        ],
        vec![
            "visual-report",
            "--before",
            &path,
            "--after",
            &path,
            "--out",
            &report,
        ],
    ];

    for mut case in cases {
        case.push("--json");
        let args = case;
        let (code, stdout, stderr) = run_cli(&args);
        let command = args[0];
        assert_eq!(
            code, 3,
            "{command}: ожидался отказ границы, stderr: {stderr}"
        );
        let value = parse_json(&stdout);
        let (error_code, reason) = error_reason(&value);
        assert_eq!(error_code, "invalid_request", "{command}: {stdout}");
        assert_eq!(reason, "input_area_forbidden", "{command}: {stdout}");
        let message = value["error"]["message"].as_str().unwrap_or("");
        assert!(
            message.contains("decks/input"),
            "{command}: объяснение обязано называть закрытую область: {stdout}"
        );
        assert!(
            stderr.is_empty(),
            "{command}: при --json диагностика идёт в stdout, а не в stderr: {stderr}"
        );
    }

    assert!(
        hidden.join("deck.json").is_file(),
        "отказ границы ничего не изменил"
    );
    assert!(!repo.path("report").exists(), "отчёт не создан");
}

#[test]
fn a_deck_inside_output_stays_readable() {
    let repo = Repo::new("local-data-output-readable");
    let export = repo.export("decks/output/Words__A", b"a");
    let path = export.display().to_string();

    let (code, stdout, stderr) = run_cli(&["validate", &path, "--json"]);

    assert_eq!(code, 0, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(value["command"], "validate");
}

#[test]
fn the_closed_input_boundary_works_without_git_for_cli_and_library_calls() {
    let repo = Repo::new("local-data-no-git");
    fs::remove_dir_all(repo.path(".git")).expect("убрать Git-метаданные");
    let hidden = repo.export("decks/input/Hidden", b"hidden");
    fs::create_dir_all(hidden.join(".git")).expect("вложенный Git-каталог");
    let original = fs::read(hidden.join("deck.json")).expect("исходный deck.json");
    let path = hidden.display().to_string();

    let (code, stdout, stderr) = run_cli(&[
        "edit", &path, "--guid", "g", "--field", "f", "--set", "new", "--expect", "old", "--apply",
        "--json",
    ]);
    assert_eq!(code, 3, "stderr: {stderr}");
    let value = parse_json(&stdout);
    assert_eq!(
        error_reason(&value),
        ("invalid_request", "input_area_forbidden")
    );

    let load_error = anki_repo::loader::load_export(&hidden).expect_err("API чтения закрыт");
    assert_eq!(load_error.code, ErrorCode::InvalidRequest);
    assert_eq!(load_error.details["reason"], "input_area_forbidden");

    let request = EditRequest {
        edits: vec![EditSpec {
            edit_id: None,
            guid: "g".to_owned(),
            field: "f".to_owned(),
            expected: "old".to_owned(),
            replacement: "new".to_owned(),
        }],
    };
    let edit_error = anki_repo::ops::edit::edit(&hidden, &request, true)
        .expect_err("прямой API edit --apply закрыт");
    assert_eq!(edit_error.code, ErrorCode::InvalidRequest);
    assert_eq!(edit_error.details["reason"], "input_area_forbidden");
    assert_eq!(
        fs::read(hidden.join("deck.json")).expect("deck.json не изменён"),
        original
    );

    let external = repo.export("external/SomeExport", b"public");
    anki_repo::loader::load_export(&external).expect("внешний экспорт остаётся доступным");
    let external_path = external.display().to_string();
    let (code, stdout, stderr) = run_cli(&["validate", &external_path, "--json"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(parse_json(&stdout)["command"], "validate");
}

#[test]
fn aliases_to_the_closed_area_are_refused_without_git() {
    let repo = Repo::new("local-data-no-git-alias");
    fs::remove_dir_all(repo.path(".git")).expect("убрать Git-метаданные");
    let hidden = repo.export("decks/input/Hidden", b"hidden");
    let alias = repo.path("external/alias");
    fs::create_dir_all(alias.parent().expect("родитель alias")).expect("каталог alias");
    std::os::unix::fs::symlink(&hidden, &alias).expect("внешняя ссылка на закрытый каталог");

    let alias_path = alias.display().to_string();
    let (code, stdout, stderr) = run_cli(&["inspect", &alias_path, "--json"]);
    assert_eq!(code, 3, "stderr: {stderr}");
    assert_eq!(
        error_reason(&parse_json(&stdout)),
        ("invalid_request", "input_area_forbidden")
    );

    let dotdot_path = repo
        .path("decks/unused/../input/Hidden")
        .display()
        .to_string();
    let (code, stdout, stderr) = run_cli(&["inspect", &dotdot_path, "--json"]);
    assert_eq!(code, 3, "stderr: {stderr}");
    assert_eq!(
        error_reason(&parse_json(&stdout)),
        ("invalid_request", "input_area_forbidden")
    );
}

#[test]
fn a_root_inside_the_closed_area_is_refused() {
    let repo = Repo::new("local-data-root-guard");
    let input = repo.path("decks/input");
    fs::create_dir_all(&input).expect("закрытая область");
    let path = input.display().to_string();

    let (code, stdout, stderr) = run_cli(&["migrate-layout", "--root", &path, "--json"]);

    assert_eq!(code, 3, "stderr: {stderr}");
    let value = parse_json(&stdout);
    let (error_code, reason) = error_reason(&value);
    assert_eq!(error_code, "invalid_request");
    assert_eq!(reason, "input_area_forbidden");
}

#[test]
fn a_repository_without_the_legacy_area_reports_it_without_failing() {
    let repo = Repo::new("local-data-absent");
    fs::remove_dir(repo.path("decks")).expect("репозиторий без прежней области");

    let (code, value, stderr) = run_migrate(&repo, true);

    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(value["result"]["summary"]["exports"], 0);
    assert_eq!(value["result"]["applied"], false);
    assert!(
        value["result"]["diagnostics"]
            .as_array()
            .expect("диагностика")
            .iter()
            .any(|diagnostic| diagnostic["code"] == "legacy_area_absent")
    );
}
