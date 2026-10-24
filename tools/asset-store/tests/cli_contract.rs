use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use asset_store::hashing::sha256_hex;
use asset_store::temp_workspace::TempWorkspace;

struct TempDir {
    workspace: TempWorkspace,
}

impl TempDir {
    fn new() -> Self {
        let workspace = TempWorkspace::create("kanji-assets-cli").expect("fixture root создаётся");
        fs::create_dir_all(workspace.path().join("repository/decks"))
            .expect("создаётся корень тестового репозитория");
        Self { workspace }
    }

    fn path(&self) -> &Path {
        self.workspace.path()
    }
}

fn cli(temp: &TempDir, args: &[&str]) -> Output {
    let store = temp.path().join("owned-store");
    let repository = temp.path().join("repository");
    Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--store")
        .arg(store)
        .arg("--repository-root")
        .arg(repository)
        .arg("--output")
        .arg("json")
        .args(args)
        .output()
        .expect("процесс kanji-assets запускается")
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("stdout содержит JSON-контракт")
}

#[test]
fn cli_reports_store_unicode_selection_idempotency_and_conflicts_as_json() {
    let temp = TempDir::new();
    let init = cli(&temp, &["init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stdout)
    );
    let init_json = json(&init);
    assert_eq!(init_json["operation"], "init");
    assert_eq!(init_json["outcome"], "initialized");
    assert_eq!(init_json["changed"], true);
    assert!(init_json["store"]["store_id"].as_str().unwrap().len() >= 32);

    let init_repeat = cli(&temp, &["init"]);
    assert!(init_repeat.status.success());
    assert_eq!(json(&init_repeat)["outcome"], "already_initialized");
    assert_eq!(json(&init_repeat)["changed"], false);

    let source = temp.path().join("claim.png");
    fs::write(&source, b"not an image").expect("записываются синтетические байты");
    let source_text = source.to_str().unwrap();
    let first = cli(
        &temp,
        &["ingest", "--character", "𠮷", "--file", source_text],
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stdout)
    );
    let first_json = json(&first);
    assert_eq!(first_json["outcome"], "candidate_created");
    assert_eq!(first_json["assets"][0]["identity"]["key"], "𠮷");
    assert_eq!(
        first_json["assets"][0]["domain_metadata"]["unicode_codepoints"][0],
        "U+20BB7"
    );
    assert_eq!(first_json["assets"][0]["to_state"], "pending");
    assert_eq!(
        first_json["assets"][0]["validation_status"],
        serde_json::Value::Null
    );

    let repeat = cli(
        &temp,
        &["ingest", "--character", "𠮷", "--file", source_text],
    );
    assert!(repeat.status.success());
    assert_eq!(json(&repeat)["outcome"], "already_present");
    assert_eq!(json(&repeat)["changed"], false);

    let plan = cli(
        &temp,
        &[
            "plan",
            "--mode",
            "new",
            "--validator-id",
            "kanji-cv",
            "--validator-version",
            "future-1",
        ],
    );
    assert!(plan.status.success());
    let plan_json = json(&plan);
    assert_eq!(plan_json["mode"], "new");
    assert_eq!(plan_json["assets"].as_array().unwrap().len(), 1);
    assert_eq!(plan_json["validator_available"], false);
    assert_eq!(plan_json["blockers"].as_array().unwrap().len(), 0);

    let full = cli(
        &temp,
        &[
            "plan",
            "--mode",
            "full",
            "--validator-id",
            "kanji-cv",
            "--validator-version",
            "future-1",
        ],
    );
    assert!(full.status.success());
    assert_eq!(json(&full)["mode"], "full");
    assert_eq!(json(&full)["assets"].as_array().unwrap().len(), 1);

    let other = temp.path().join("different.bin");
    fs::write(&other, b"different").unwrap();
    let other_text = other.to_str().unwrap();
    let conflict = cli(
        &temp,
        &["ingest", "--character", "𠮷", "--file", other_text],
    );
    assert_eq!(conflict.status.code(), Some(3));
    let conflict_json = json(&conflict);
    assert_eq!(conflict_json["outcome"], "conflict");
    assert_eq!(conflict_json["conflicts"][0]["code"], "identity_conflict");
    assert!(
        conflict_json["conflicts"][0]["existing_sha256"]
            .as_str()
            .is_some()
    );
    assert!(
        conflict_json["conflicts"][0]["candidate_sha256"]
            .as_str()
            .is_some()
    );
}

#[test]
fn list_and_plan_reject_a_missing_store_without_creating_it() {
    let temp = TempDir::new();
    let missing_store = temp.path().join("owned-store");
    let list = cli(&temp, &["list"]);
    assert_eq!(list.status.code(), Some(4));
    assert_eq!(json(&list)["error"]["code"], "store_missing");
    assert!(!missing_store.exists());

    let plan = cli(
        &temp,
        &[
            "plan",
            "--mode",
            "new",
            "--validator-id",
            "kanji-cv",
            "--validator-version",
            "future-1",
        ],
    );
    assert_eq!(plan.status.code(), Some(4));
    assert_eq!(json(&plan)["error"]["code"], "store_missing");
    assert!(!missing_store.exists());
}

#[test]
fn list_does_not_initialize_an_existing_empty_directory() {
    let temp = TempDir::new();
    let store = temp.path().join("owned-store");
    fs::create_dir(&store).expect("создаётся пустой каталог store");

    let output = cli(&temp, &["list"]);

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "store_not_owned");
    assert_eq!(fs::read_dir(&store).unwrap().count(), 0);
}

#[test]
fn cli_rejects_store_under_decks_before_creating_it() {
    let temp = TempDir::new();
    let store = temp.path().join("repository/decks/kanji-assets");
    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--store")
        .arg(&store)
        .arg("--repository-root")
        .arg(temp.path().join("repository"))
        .arg("--output")
        .arg("json")
        .arg("init")
        .output()
        .expect("CLI запускается");
    assert_eq!(output.status.code(), Some(4));
    let value = json(&output);
    assert_eq!(value["error"]["code"], "boundary_violation");
    assert!(!store.exists());
}

#[test]
fn cli_rejects_explicit_source_under_decks_before_creating_the_store() {
    let temp = TempDir::new();
    let source = temp
        .path()
        .join("repository/decks/japanese/media/candidate.png");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(
        &source,
        "синтетический пользовательский media-файл".as_bytes(),
    )
    .unwrap();
    let source_text = source.to_str().unwrap();

    let output = cli(
        &temp,
        &["ingest", "--character", "漢", "--file", source_text],
    );

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "boundary_violation");
    assert!(
        !temp.path().join("owned-store").exists(),
        "защищённый source отклоняется до создания store"
    );
}

#[cfg(unix)]
#[test]
fn cli_rejects_decks_source_reached_through_a_symlink_alias() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let repository = temp.path().join("repository");
    let decks = repository.join("decks");
    fs::remove_dir(&decks).unwrap();
    let actual_decks = temp.path().join("private-decks");
    let source = actual_decks.join("media/candidate.png");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(
        &source,
        "синтетический пользовательский media-файл".as_bytes(),
    )
    .unwrap();
    symlink(&actual_decks, &decks).unwrap();
    let aliased_source = decks.join("media/candidate.png");
    let source_text = aliased_source.to_str().unwrap();

    let output = cli(
        &temp,
        &["ingest", "--character", "漢", "--file", source_text],
    );

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "boundary_violation");
    assert!(
        !temp.path().join("owned-store").exists(),
        "symlink alias не даёт прочитать source или создать store"
    );
}

#[test]
fn cli_detects_decks_boundary_even_when_repository_root_argument_is_wrong() {
    let temp = TempDir::new();
    let repository = temp.path().join("repository");
    let store = repository.join("decks/local-store");
    let unrelated = temp.path().join("unrelated");
    fs::create_dir_all(&unrelated).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--store")
        .arg(&store)
        .arg("--repository-root")
        .arg(&unrelated)
        .arg("--output")
        .arg("json")
        .arg("init")
        .current_dir(temp.path())
        .output()
        .expect("CLI запускается");
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "boundary_violation");
    assert!(!store.exists());
}

#[test]
fn cli_returns_machine_readable_invalid_input_without_creating_the_store() {
    let temp = TempDir::new();
    let invalid_character = cli(
        &temp,
        &["ingest", "--character", "", "--file", "missing.bin"],
    );
    assert_eq!(invalid_character.status.code(), Some(3));
    assert_eq!(
        json(&invalid_character)["error"]["code"],
        "invalid_identity"
    );

    let missing_source = cli(
        &temp,
        &[
            "ingest",
            "--character",
            "漢",
            "--file",
            "missing-source.bin",
        ],
    );
    assert_eq!(missing_source.status.code(), Some(3));
    assert_eq!(
        json(&missing_source)["error"]["code"],
        "source_file_missing"
    );

    let invalid_validator = cli(
        &temp,
        &[
            "plan",
            "--mode",
            "new",
            "--validator-id",
            "",
            "--validator-version",
            "1",
        ],
    );
    assert_eq!(invalid_validator.status.code(), Some(3));
    assert_eq!(
        json(&invalid_validator)["error"]["code"],
        "invalid_validator_identity"
    );
    assert!(!temp.path().join("owned-store").exists());
}

#[test]
fn cli_reports_unsupported_manifest_schema_with_a_stable_json_code() {
    let temp = TempDir::new();
    let init = cli(&temp, &["init"]);
    assert!(init.status.success());
    let manifest_path = temp.path().join("owned-store/manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["schema_version"] = serde_json::json!(500);
    manifest["future_field"] = serde_json::json!({"format": "future"});
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let output = cli(&temp, &["list"]);
    assert_eq!(output.status.code(), Some(4));
    let value = json(&output);
    assert_eq!(value["outcome"], "blocked");
    assert_eq!(value["error"]["code"], "unsupported_schema_version");
    assert_eq!(value["store"]["store_id"], serde_json::Value::Null);
}

#[test]
fn cli_json_reports_layout_migration_only_on_the_migrating_open() {
    let temp = TempDir::new();
    let init = cli(&temp, &["init"]);
    assert!(init.status.success());
    assert_eq!(json(&init)["store"]["layout_migrated_on_open"], false);

    let store = temp.path().join("owned-store");
    let manifest_path = store.join("manifest.json");
    let legacy_path = store.join("assets/漢.png");
    let bytes = b"\x89PNG\r\n\x1a\nlegacy migration fixture";
    fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
    fs::write(&legacy_path, bytes).unwrap();

    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["schema_version"] = serde_json::json!(4);
    manifest.as_object_mut().unwrap().remove("domain_id");
    manifest["assets"] = serde_json::json!([{
        "identity": { "namespace": "kanji", "key": "漢" },
        "storage_path": "assets/漢.png",
        "sha256": sha256_hex(bytes),
        "byte_length": bytes.len(),
        "format": "png",
        "provenance": { "source_kind": "fixture", "source_name": "migration.png" },
        "lifecycle": "pending"
    }]);
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let first_open = cli(&temp, &["list"]);
    assert!(
        first_open.status.success(),
        "{}",
        String::from_utf8_lossy(&first_open.stderr)
    );
    let first_json = json(&first_open);
    assert_eq!(first_json["store"]["layout_migrated_on_open"], true);
    let migrated_manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(migrated_manifest["schema_version"], 6);
    assert!(migrated_manifest["assets"].as_array().unwrap().is_empty());
    assert!(!legacy_path.exists());

    let second_open = cli(&temp, &["list"]);
    assert!(second_open.status.success());
    assert_eq!(
        json(&second_open)["store"]["layout_migrated_on_open"],
        false
    );
}

#[test]
fn cli_argument_usage_error_writes_stderr_without_json_stdout() {
    let temp = TempDir::new();
    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--output")
        .arg("json")
        .arg("invalid-command")
        .current_dir(temp.path())
        .output()
        .expect("CLI запускается");

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    assert!(
        !temp.path().join(".asset-store/kanji").exists(),
        "ошибка разбора аргументов не запускает операцию с store"
    );
}

#[test]
fn default_store_resolves_to_workspace_root_when_run_from_a_subdirectory() {
    let temp = TempDir::new();
    let repository = temp.path().join("repository");
    let nested = repository.join("tools/asset-store");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(repository.join("decks")).unwrap();
    fs::write(repository.join("Cargo.toml"), "[workspace]\n").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--repository-root")
        .arg(&repository)
        .arg("--output")
        .arg("json")
        .arg("init")
        .current_dir(&nested)
        .output()
        .expect("CLI запускается");
    assert!(output.status.success());
    let expected = repository.join(".asset-store/kanji");
    assert!(expected.exists());
    assert!(!nested.join(".asset-store").exists());
    assert_eq!(
        json(&output)["store"]["path"],
        expected.display().to_string()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn default_store_resolves_symlinked_repository_root_with_parent_components() {
    use std::os::unix::fs::symlink;

    let temp = TempDir::new();
    let repository = temp.path().join("repository");
    let nested = repository.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(repository.join("decks")).unwrap();
    fs::write(repository.join("Cargo.toml"), "[workspace]\n").unwrap();
    let alias = temp.path().join("repository-alias");
    symlink(&repository, &alias).unwrap();
    let repository_argument = alias.join("nested").join("..");

    let output = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--repository-root")
        .arg(&repository_argument)
        .arg("--output")
        .arg("json")
        .arg("init")
        .current_dir(temp.path())
        .output()
        .expect("CLI запускается");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = repository.join(".asset-store/kanji");
    assert!(expected.exists());
    assert_eq!(
        json(&output)["store"]["path"],
        expected.display().to_string()
    );
}

#[test]
fn identify_reads_sources_without_a_store_and_respects_the_decks_boundary() {
    let temp = TempDir::new();
    let outside = temp.path().join("archive");
    fs::create_dir_all(&outside).unwrap();
    let broken = outside.join("0_1.gif");
    fs::write(&broken, b"GIF89a\x01\x00").unwrap();
    let protected = temp.path().join("repository/decks/media/0_2.gif");
    fs::create_dir_all(protected.parent().unwrap()).unwrap();
    fs::write(&protected, b"GIF89a").unwrap();
    let before = fs::read(&broken).unwrap();

    let output = cli(
        &temp,
        &[
            "identify",
            broken.to_str().unwrap(),
            protected.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(3));
    let value = json(&output);
    assert_eq!(value["operation"], "identify");
    assert_eq!(value["items"][0]["sha256"], sha256_hex(&before));
    assert_eq!(value["items"][0]["byte_length"], before.len());
    assert_eq!(value["items"][0]["oversized"], false);
    assert_eq!(value["items"][0]["identification"]["status"], "corrupt");
    assert_eq!(value["items"][1]["error"]["code"], "boundary_violation");
    assert!(value["items"][1].get("sha256").is_none());
    assert_eq!(value["counts"]["corrupt"], 1);
    assert_eq!(value["counts"]["error"], 1);
    assert_eq!(fs::read(&broken).unwrap(), before);
    assert!(!temp.path().join("owned-store").exists());
}

#[test]
fn identify_does_not_report_a_prefix_hash_for_an_oversized_file() {
    let temp = TempDir::new();
    let oversized = temp.path().join("archive/0_1.gif");
    fs::create_dir_all(oversized.parent().unwrap()).unwrap();
    let mut bytes = b"GIF89a".to_vec();
    bytes.resize(8 * 1024 * 1024 + 1, 0);
    fs::write(&oversized, &bytes).unwrap();

    let output = cli(&temp, &["identify", oversized.to_str().unwrap()]);

    assert_eq!(output.status.code(), Some(0));
    let item = &json(&output)["items"][0];
    assert_eq!(item["oversized"], true);
    assert!(item["sha256"].is_null());
    assert!(item["byte_length"].is_null());
    assert_eq!(item["identification"]["status"], "corrupt");
}

#[cfg(unix)]
#[test]
fn batch_import_fails_closed_when_the_decks_boundary_cannot_be_resolved() {
    let temp = TempDir::new();
    let source = temp.path().join("staged");
    fs::create_dir_all(&source).unwrap();
    let repository = temp.path().join("repository");
    fs::create_dir_all(&repository).unwrap();
    let _ = fs::remove_dir_all(repository.join("decks"));
    // Цикл symlink: canonicalize отказывает не с NotFound, лексическая проверка молчит.
    std::os::unix::fs::symlink("decks", repository.join("decks")).unwrap();

    let output = cli(
        &temp,
        &[
            "batch",
            "import",
            "--batch-id",
            "local",
            "--source-dir",
            source.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(5));
    assert_eq!(json(&output)["error"]["code"], "io_failure");
    assert!(!temp.path().join("owned-store").exists());
}

#[test]
fn batch_import_rejects_a_source_directory_under_decks_before_opening_the_store() {
    let temp = TempDir::new();
    let protected = temp.path().join("repository/decks/media");
    fs::create_dir_all(&protected).unwrap();

    let output = cli(
        &temp,
        &[
            "batch",
            "import",
            "--batch-id",
            "local",
            "--source-dir",
            protected.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "boundary_violation");
    assert!(!temp.path().join("owned-store").exists());
}

#[cfg(unix)]
#[test]
fn batch_import_rejects_a_symlink_to_decks_before_opening_the_store() {
    let temp = TempDir::new();
    let protected = temp.path().join("repository/decks/media");
    fs::create_dir_all(&protected).unwrap();
    let link = temp.path().join("staged");
    std::os::unix::fs::symlink(&protected, &link).unwrap();

    let output = cli(
        &temp,
        &[
            "batch",
            "import",
            "--batch-id",
            "local",
            "--source-dir",
            link.to_str().unwrap(),
        ],
    );

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(json(&output)["error"]["code"], "boundary_violation");
    assert!(!temp.path().join("owned-store").exists());
}

#[test]
fn batch_import_reads_a_checked_safe_directory_through_the_cli() {
    let temp = TempDir::new();
    let source = temp.path().join("staged");
    fs::create_dir_all(&source).unwrap();
    let start = cli(&temp, &["batch", "start", "--batch-id", "local", "水"]);
    assert!(
        start.status.success(),
        "{}",
        String::from_utf8_lossy(&start.stdout)
    );

    let output = cli(
        &temp,
        &[
            "batch",
            "import",
            "--batch-id",
            "local",
            "--source-dir",
            source.to_str().unwrap(),
        ],
    );

    // Технический отказ попытки, а не отказ команды: дескриптор проверенного
    // каталога дошёл до импорта.
    assert_eq!(output.status.code(), Some(3));
    let value = json(&output);
    assert_eq!(value["outcome"], "partial_progress");
    assert_eq!(
        value["batch"]["items"][0]["attempts"][0]["result"]["code"],
        "local_source_missing"
    );
}
