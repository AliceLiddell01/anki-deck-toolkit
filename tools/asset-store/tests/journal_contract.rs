//! Контракт CLI команд `journal` для обоих предметных хранилищ.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use asset_store::temp_workspace::TempWorkspace;
use asset_store::{AssetStore, PitchAccentDomainPolicy, StoreOptions};

const CANDIDATE_BYTES: &[u8] = b"GIF89a journal contract bytes";

struct Fixture {
    workspace: TempWorkspace,
}

impl Fixture {
    fn new(purpose: &str) -> Self {
        Self {
            workspace: TempWorkspace::create(purpose).expect("fixture root создаётся"),
        }
    }

    fn path(&self) -> &Path {
        self.workspace.path()
    }

    fn kanji_store(&self) -> PathBuf {
        self.path().join("kanji")
    }

    fn pitch_store(&self) -> PathBuf {
        self.path().join("pitch")
    }

    fn run(&self, binary: &str, store: &Path, args: &[&str]) -> Output {
        Command::new(binary)
            .current_dir(self.path())
            .arg("--repository-root")
            .arg(self.path())
            .arg("--store")
            .arg(store)
            .args(args)
            .output()
            .expect("CLI-процесс запускается")
    }

    fn kanji(&self, args: &[&str]) -> Output {
        self.run(
            env!("CARGO_BIN_EXE_kanji-assets"),
            &self.kanji_store(),
            args,
        )
    }

    fn pitch(&self, args: &[&str]) -> Output {
        self.run(
            env!("CARGO_BIN_EXE_pitch-assets"),
            &self.pitch_store(),
            args,
        )
    }

    fn events_file(&self, store: &Path) -> PathBuf {
        store.join(".journal").join("events.jsonl")
    }
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).expect("stdout содержит JSON-контракт")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn ingest_kanji_candidate(fixture: &Fixture) -> String {
    let init = fixture.kanji(&["init"]);
    assert!(init.status.success(), "{}", stderr(&init));
    let source = fixture.path().join("claim.gif");
    fs::write(&source, CANDIDATE_BYTES).expect("записан синтетический кандидат");
    let ingest = fixture.kanji(&[
        "--output",
        "json",
        "ingest",
        "--character",
        "漢",
        "--file",
        source.to_str().unwrap(),
    ]);
    assert!(ingest.status.success(), "{}", stderr(&ingest));
    json(&ingest)["assets"][0]["sha256"]
        .as_str()
        .expect("кандидат получил SHA-256")
        .to_owned()
}

#[test]
fn kanji_journal_records_a_two_phase_transition_and_reads_it_after_restart() {
    let fixture = Fixture::new("journal-contract-kanji");
    let sha256 = ingest_kanji_candidate(&fixture);

    // Новый процесс CLI читает тот же журнал: он переживает перезапуск.
    let listed = fixture.kanji(&["--output", "json", "journal", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let listed = json(&listed);
    assert_eq!(listed["operation"], "journal_list");
    assert_eq!(listed["outcome"], "listed");
    assert_eq!(listed["changed"], false);
    let events = listed["journal"]["events"]
        .as_array()
        .expect("журнал перечислен");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["seq"], 1);
    assert_eq!(events[0]["operation"], "candidate_written");
    assert_eq!(events[0]["outcome"], "pending");
    assert_eq!(events[1]["seq"], 2);
    assert_eq!(events[1]["outcome"], "completed");
    assert_eq!(events[1]["new_sha256"], sha256);
    assert_eq!(events[1]["new_lifecycle"], "pending");
    assert_eq!(events[1]["verified"], false);
    assert_eq!(events[1]["intent_id"], events[0]["event_id"]);
    assert_ne!(events[0]["event_key"], events[1]["event_key"]);

    let filtered = fixture.kanji(&[
        "--output",
        "json",
        "journal",
        "list",
        "--limit",
        "1",
        "--identity",
        "漢",
        "--operation",
        "candidate_written",
    ]);
    assert!(filtered.status.success(), "{}", stderr(&filtered));
    let filtered = json(&filtered);
    assert_eq!(
        filtered["journal"]["events"].as_array().unwrap().len(),
        1,
        "фильтры оставляют одно событие"
    );
    assert_eq!(filtered["journal"]["events"][0]["seq"], 2);

    let unknown = fixture.kanji(&["journal", "list", "--operation", "unknown_op"]);
    assert_eq!(unknown.status.code(), Some(3));
    assert!(stderr(&unknown).contains("неизвестная операция журнала"));

    let check = fixture.kanji(&["--output", "json", "journal", "check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    let check = json(&check);
    assert_eq!(check["operation"], "journal_check");
    assert_eq!(check["outcome"], "checked");
    assert_eq!(check["journal"]["check"]["event_count"], 2);
    assert_eq!(check["journal"]["check"]["torn_tail"], false);
    assert!(
        check["journal"]["check"]["problems"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    let reconcile = fixture.kanji(&["--output", "json", "journal", "reconcile"]);
    assert!(reconcile.status.success(), "{}", stderr(&reconcile));
    let reconcile = json(&reconcile);
    assert_eq!(reconcile["outcome"], "no_op");
    assert_eq!(reconcile["changed"], false);
    assert!(reconcile["journal"]["appended"].is_null());

    let baseline = fixture.kanji(&["--output", "json", "journal", "baseline"]);
    assert!(baseline.status.success(), "{}", stderr(&baseline));
    let baseline = json(&baseline);
    assert_eq!(baseline["outcome"], "baseline_recorded");
    assert_eq!(baseline["changed"], true);
    let event = &baseline["journal"]["baseline"];
    assert_eq!(event["operation"], "baseline");
    assert_eq!(event["identity"], "*");
    assert_eq!(event["snapshot"]["assets"], 1);
    assert_eq!(event["snapshot"]["pending"], 1);

    let repeat = fixture.kanji(&["--output", "json", "journal", "baseline"]);
    assert_eq!(repeat.status.code(), Some(3));
    assert_eq!(json(&repeat)["error"]["code"], "invalid_transition");

    // NDJSON: по одной строке на событие, три события.
    let text = fs::read_to_string(fixture.events_file(&fixture.kanji_store()))
        .expect("журнал читается с диска");
    assert!(text.ends_with('\n'));
    assert_eq!(text.lines().count(), 3);
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).expect("строка NDJSON");
        assert_eq!(value["schema_version"], 1);
    }
}

#[test]
fn reconcile_refuses_to_mask_a_result_without_its_intent_link() {
    let fixture = Fixture::new("journal-contract-reconcile");
    ingest_kanji_candidate(&fixture);
    let file = fixture.events_file(&fixture.kanji_store());
    let text = fs::read_to_string(&file).expect("журнал читается");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    let mut broken = lines[1].to_owned();
    let marker = "\"intent_id\":\"";
    let value_start = broken.find(marker).expect("результат ссылается на интент");
    let quote_start = value_start + marker.len();
    let offset = broken[quote_start..].find('"').expect("конец event_id");
    broken.replace_range(value_start..quote_start + offset + 1, "\"intent_id\":null");
    fs::write(&file, format!("{}\n{broken}\n", lines[0])).expect("журнал испорчен");

    let check = fixture.kanji(&["--output", "json", "journal", "check"]);
    assert_eq!(check.status.code(), Some(3), "{}", stderr(&check));
    let check = json(&check);
    assert_eq!(check["outcome"], "problems_found");
    assert!(!check["blockers"].as_array().unwrap().is_empty());
    let open = check["journal"]["check"]["open_intents"]
        .as_array()
        .expect("незакрытые интенты перечислены");
    assert_eq!(open.len(), 1);
    assert_eq!(open[0]["operation"], "candidate_written");

    let before_reconcile = fs::read(&file).expect("байты повреждённой истории сохранены");
    let reconcile = fixture.kanji(&["--output", "json", "journal", "reconcile"]);
    assert_eq!(reconcile.status.code(), Some(4), "{}", stderr(&reconcile));
    assert_eq!(json(&reconcile)["error"]["code"], "manifest_corrupt");
    assert_eq!(
        fs::read(&file).expect("сверка не меняет файл"),
        before_reconcile,
        "структурно повреждённая история не получает новых событий"
    );

    let check = fixture.kanji(&["--output", "json", "journal", "check"]);
    assert_eq!(check.status.code(), Some(3), "{}", stderr(&check));
    assert_eq!(json(&check)["outcome"], "problems_found");
    assert!(!json(&check)["blockers"].as_array().unwrap().is_empty());
}

#[test]
fn kanji_journal_survives_deleting_the_batch_directory() {
    let fixture = Fixture::new("journal-contract-batch");
    ingest_kanji_candidate(&fixture);
    let batch = fixture
        .kanji_store()
        .join(".runtime")
        .join("batches")
        .join("review-batch");
    fs::create_dir_all(&batch).expect("создан каталог пакета");
    fs::write(batch.join("state.json"), b"{}").expect("записано состояние пакета");

    let listed = fixture.kanji(&["--output", "json", "journal", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert_eq!(
        json(&listed)["journal"]["events"].as_array().unwrap().len(),
        2
    );

    fs::remove_dir_all(&batch).expect("каталог пакета удалён");
    let after = fixture.kanji(&["--output", "json", "journal", "list"]);
    assert!(after.status.success(), "{}", stderr(&after));
    assert_eq!(
        json(&after)["journal"]["events"].as_array().unwrap().len(),
        2,
        "журнал не зависит от локального состояния пакета"
    );
    let check = fixture.kanji(&["--output", "json", "journal", "check"]);
    assert!(check.status.success(), "{}", stderr(&check));
}

#[test]
fn pitch_journal_lists_an_empty_store_and_records_a_baseline() {
    let fixture = Fixture::new("journal-contract-pitch");
    AssetStore::open_with_policy(
        StoreOptions::new(fixture.pitch_store()),
        PitchAccentDomainPolicy,
    )
    .expect("pitch store создан");

    let listed = fixture.pitch(&["--output", "json", "journal", "list"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    let listed = json(&listed);
    assert_eq!(listed["operation"], "journal_list");
    assert_eq!(listed["outcome"], "listed");
    assert!(listed["journal"].get("events").is_none());

    let check = fixture.pitch(&["--output", "json", "journal", "check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    assert_eq!(json(&check)["outcome"], "checked");

    let baseline = fixture.pitch(&["--output", "json", "journal", "baseline"]);
    assert!(baseline.status.success(), "{}", stderr(&baseline));
    let baseline = json(&baseline);
    assert_eq!(baseline["journal"]["baseline"]["identity"], "*");
    assert_eq!(baseline["journal"]["baseline"]["domain"], "pitch_accent");
    assert_eq!(baseline["journal"]["baseline"]["snapshot"]["assets"], 0);

    let repeat = fixture.pitch(&["--output", "json", "journal", "baseline"]);
    assert_eq!(repeat.status.code(), Some(3));
    assert_eq!(json(&repeat)["error"]["code"], "invalid_transition");

    let human = fixture.pitch(&["journal", "list"]);
    assert!(human.status.success(), "{}", stderr(&human));
    assert!(
        String::from_utf8_lossy(&human.stdout).contains("baseline"),
        "{}",
        String::from_utf8_lossy(&human.stdout)
    );
}
