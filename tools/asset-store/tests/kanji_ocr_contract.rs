//! Полный путь публичного CLI и реального v6; подменяется только HTTP Ollama.
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use asset_store::batch::{BatchAttemptInput, BatchRuntime, KanjiBatch};
use asset_store::hashing::sha256_hex;
use asset_store::kanji_validator::KanjiImageValidator;
use asset_store::temp_workspace::TempWorkspace;
use asset_store::{
    AssetIdentity, AssetStore, HumanAttestationRequest, HumanDecision, IngestRequest,
    SemanticStatus, StoreOptions,
};
use serde_json::{Value, json};

struct Fixture {
    temp: TempWorkspace,
    store: AssetStore,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempWorkspace::create("kanji-ocr-contract").unwrap();
        fs::create_dir_all(temp.path().join("repository/decks")).unwrap();
        let store = AssetStore::open_kanji(StoreOptions::new(temp.path().join("store"))).unwrap();
        Self { temp, store }
    }
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
            .arg("--store")
            .arg(self.store.root())
            .arg("--repository-root")
            .arg(self.temp.path().join("repository"))
            .args(["--output", "json"])
            .args(args)
            .output()
            .unwrap()
    }
    fn batch(&self, characters: &[char], bytes: &[u8]) {
        let mut runtime = BatchRuntime::open(self.store.root(), "ocr").unwrap();
        let mut state = KanjiBatch::new(
            "ocr".into(),
            characters.iter().map(|c| identity(*c)).collect(),
            KanjiImageValidator::validator_identity(),
        )
        .unwrap();
        for character in characters {
            let source = self.temp.path().join("input.png");
            fs::write(&source, bytes).unwrap();
            let record = self
                .store
                .ingest(IngestRequest {
                    identity: identity(*character),
                    source_path: source,
                    expected_source_sha256: Some(sha256_hex(bytes)),
                    domain_metadata: None,
                    replace_expected_sha256: None,
                })
                .unwrap()
                .asset;
            let validation = self
                .store
                .validate_exact(
                    &record.identity,
                    &record.sha256,
                    &KanjiImageValidator::new(),
                )
                .unwrap();
            let attempt = &validation.attempts[0];
            let record_validation = asset_store::ValidationRecord {
                validator: KanjiImageValidator::validator_identity(),
                content_sha256: record.sha256.clone(),
                status: attempt.status.unwrap(),
                evidence: attempt.evidence.clone(),
            };
            let candidate = runtime
                .persist_candidate(bytes, record_validation, true)
                .unwrap();
            state
                .record_attempt(&record.identity, BatchAttemptInput::Candidate { candidate })
                .unwrap();
        }
        runtime.save(&state).unwrap();
    }
    fn state(&self) -> KanjiBatch {
        BatchRuntime::open(self.store.root(), "ocr")
            .unwrap()
            .load()
            .unwrap()
            .unwrap()
    }
    fn ocr(&self, mock: &Mock, extra: &[&str]) -> Output {
        let mut args = vec![
            "batch",
            "ocr",
            "--batch-id",
            "ocr",
            "--model",
            "fixture:vision",
            "--endpoint",
            &mock.url,
        ];
        args.extend_from_slice(extra);
        self.cli(&args)
    }
    fn ledger(&self) -> Value {
        serde_json::from_slice(
            &fs::read(
                self.store
                    .root()
                    .join(".runtime/batches/ocr/ocr/suggestions.json"),
            )
            .unwrap(),
        )
        .unwrap()
    }
}
fn identity(character: char) -> AssetIdentity {
    AssetIdentity::new("kanji", character.to_string()).unwrap()
}
fn value(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&output.stderr)))
}
fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
fn png() -> Vec<u8> {
    let mut image = image::RgbaImage::from_pixel(64, 64, image::Rgba([255, 255, 255, 255]));
    for y in 10..54 {
        for x in 10..54 {
            if x < 15 || y < 15 || x >= 49 || y >= 49 {
                image.put_pixel(x, y, image::Rgba([0, 0, 0, 255]));
            }
        }
    }
    let mut bytes = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, image::ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}

struct ServerState {
    requests: Vec<(String, Value)>,
    chat: String,
    digest: String,
    installed: bool,
    vision: bool,
    delay: Duration,
    http_status: u16,
    response_gate: Option<Arc<AtomicBool>>,
}
struct Mock {
    url: String,
    state: Arc<Mutex<ServerState>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Mock {
    fn new(observed: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(ServerState { requests: Vec::new(), chat: json!({"done":true,"done_reason":"stop","message":{"content":json!({"observed":observed}).to_string()}}).to_string(), digest: "fixture-digest-1".into(), installed:true, vision:true, delay:Duration::ZERO, http_status:200, response_gate:None }));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0; 8192];
                let (header_end, body_len) = loop {
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&bytes[..end]);
                        let len = header
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|s| s.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        break (end + 4, len);
                    }
                };
                while bytes.len() < header_end + body_len {
                    let count = stream.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let path = String::from_utf8_lossy(&bytes[..header_end])
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let body = if body_len == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[header_end..header_end + body_len]).unwrap()
                };
                let (response, delay, status, gate) = {
                    let mut state = worker_state.lock().unwrap();
                    state.requests.push((path.clone(), body));
                    let response=match path.as_str() {
                        "/api/tags" => json!({"models":if state.installed {vec![json!({"name":"fixture:vision","digest":state.digest})]} else {vec![]}}).to_string(),
                        "/api/show" => json!({"capabilities":if state.vision {vec!["vision"]}else{vec!["completion"]}}).to_string(),
                        "/api/chat" => state.chat.clone(), other=>panic!("неожиданный endpoint {other}"),
                    };
                    (
                        response,
                        if path == "/api/chat" {
                            state.delay
                        } else {
                            Duration::ZERO
                        },
                        if path == "/api/chat" {
                            state.http_status
                        } else {
                            200
                        },
                        if path == "/api/chat" {
                            state.response_gate.clone()
                        } else {
                            None
                        },
                    )
                };
                if let Some(gate) = gate {
                    while !gate.load(Ordering::Acquire) && !worker_stop.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(2));
                    }
                }
                thread::sleep(delay);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                );
            }
        });
        Self {
            url,
            state,
            stop,
            worker: Some(worker),
        }
    }
    fn chats(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(path, _)| path == "/api/chat")
            .count()
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn blind_saved_sha_cache_review_and_explicit_group_confirmation_use_public_cli() {
    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &png());
    let before = fixture.state();
    assert_eq!(before.review_queue().len(), 1);
    let mock = Mock::new("𠀀");
    assert_ok(&fixture.cli(&["batch", "status", "--batch-id", "ocr"]));
    assert_ok(&fixture.cli(&["batch", "review", "--batch-id", "ocr"]));
    assert!(mock.state.lock().unwrap().requests.is_empty());
    assert_ok(&fixture.ocr(&mock, &[]));
    assert_eq!(mock.chats(), 1);
    assert_eq!(fixture.state(), before);
    let request = mock
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .find(|(path, _)| path == "/api/chat")
        .unwrap()
        .1
        .clone();
    assert_eq!(request["think"], false);
    assert_eq!(request["stream"], false);
    assert_eq!(request["model"], "fixture:vision");
    assert_eq!(
        request["options"],
        json!({"temperature":0,"num_predict":128,"num_ctx":4096})
    );
    assert_eq!(request["format"]["additionalProperties"], false);
    assert_eq!(request["format"]["required"], json!(["observed"]));
    assert!(!request.to_string().contains('𠀀'));
    assert!(!request.to_string().contains("input.png"));
    assert_ok(&fixture.ocr(&mock, &[]));
    assert_eq!(mock.chats(), 1);
    assert_ok(&fixture.ocr(&mock, &["--rescan"]));
    assert_eq!(mock.chats(), 2);
    let exported = fixture.cli(&["batch", "review-export", "--batch-id", "ocr"]);
    assert_ok(&exported);
    let data = value(&exported);
    let review_id = data["review_id"].as_str().unwrap();
    let decisions = Path::new(data["review_artifact"].as_str().unwrap());
    let text = fs::read_to_string(decisions).unwrap();
    assert!(text.lines().any(|line| line.starts_with("\t𠀀")));
    assert!(!text.lines().any(|line| line.starts_with("y\t")));
    let html = fs::read_to_string(decisions.parent().unwrap().join("index.html")).unwrap();
    assert!(html.contains("observed=𠀀"));
    assert!(html.contains("Совпадения OCR"));
    // Mock намеренно возвращает неверную метку для видимого рисунка.
    // Совпадение не публикует ресурс: требуется явная команда человека.
    assert!(fixture.state().items[0].human_decisions.is_empty());
    let refusal = fixture.cli(&[
        "batch",
        "review-confirm-matches",
        "--batch-id",
        "ocr",
        "--review-id",
        review_id,
    ]);
    assert!(!refusal.status.success());
    assert_eq!(fixture.state(), before);

    fs::write(decisions, text.replacen("\t𠀀", "-\t𠀀", 1)).unwrap();
    assert_ok(&fixture.cli(&[
        "batch",
        "review-confirm-matches",
        "--batch-id",
        "ocr",
        "--review-id",
        review_id,
        "--confirm-reviewed",
    ]));
    assert_eq!(fixture.state(), before);
    assert!(fixture.state().items[0].human_decisions.is_empty());

    fs::write(decisions, &text).unwrap();
    let confirm = fixture.cli(&[
        "batch",
        "review-confirm-matches",
        "--batch-id",
        "ocr",
        "--review-id",
        review_id,
        "--confirm-reviewed",
    ]);
    assert_ok(&confirm);
    let after = fixture.state();
    assert!(after.is_resolved());
    assert_eq!(after.items[0].human_decisions.len(), 1);
    assert_ok(&fixture.cli(&[
        "batch",
        "review-confirm-matches",
        "--batch-id",
        "ocr",
        "--review-id",
        review_id,
        "--confirm-reviewed",
    ]));
    assert_eq!(fixture.state().items[0].human_decisions.len(), 1);
    assert_ok(&fixture.ocr(&mock, &[]));
    assert_eq!(mock.chats(), 2);
}

#[test]
fn invalid_unknown_truncated_network_http_and_nonvision_responses_keep_candidates() {
    let responses=[
        ("not json".to_owned(),"invalid_response_json"),
        (json!({"done":true,"done_reason":"length","message":{"content":"{\"observed\":\"𠀀\"}"}}).to_string(),"token_limit"),
        (json!({"done":true,"done_reason":"stop","message":{"content":"{\"observed\":\"𠀀𠀁\"}"}}).to_string(),"observed_not_single_han"),
        (json!({"done":true,"done_reason":"stop","message":{"content":"{\"observed\":\"𠀀\",\"extra\":true}"}}).to_string(),"invalid_observed_json"),
        (json!({"done":true,"done_reason":"stop","message":{"content":"explanation"}}).to_string(),"invalid_observed_json"),
        ("x".repeat(1024*1024+1),"response_too_large"),
    ];
    for (response, failure) in responses {
        let fixture = Fixture::new();
        fixture.batch(&['𠀀'], &png());
        let before = fixture.state();
        let mock = Mock::new("?");
        mock.state.lock().unwrap().chat = response;
        let output = fixture.ocr(&mock, &[]);
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(fixture.state(), before);
        assert!(fixture.ledger().to_string().contains(failure));
        assert_ok(&fixture.cli(&["batch", "review-export", "--batch-id", "ocr"]));
        assert_eq!(mock.chats(), 1);
        assert_eq!(fixture.ocr(&mock, &[]).status.code(), Some(3));
        assert_eq!(mock.chats(), 1);
    }
    for (observed, matches) in [("?", false), ("保", false)] {
        let fixture = Fixture::new();
        fixture.batch(&['𠀀'], &png());
        let mock = Mock::new(observed);
        assert_ok(&fixture.ocr(&mock, &[]));
        let ledger = fixture.ledger();
        let record = ledger["records"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(record["matches_expected"], matches);
        assert!(record["failure"].is_null());
        assert!(fixture.state().items[0].human_decisions.is_empty());
    }
    for (vision, status, delay, failure) in [
        (false, 200, 0, "model_does_not_support_vision"),
        (true, 503, 0, "ollama_http_503"),
        (true, 200, 1200, "ollama_timeout"),
    ] {
        let fixture = Fixture::new();
        fixture.batch(&['𠀀'], &png());
        let before = fixture.state();
        let mock = Mock::new("𠀀");
        {
            let mut server = mock.state.lock().unwrap();
            server.vision = vision;
            server.http_status = status;
            server.delay = Duration::from_millis(delay);
        }
        assert_eq!(
            fixture
                .ocr(&mock, &["--timeout-seconds", "1"])
                .status
                .code(),
            Some(3)
        );
        assert_eq!(fixture.state(), before);
        assert!(fixture.ledger().to_string().contains(failure));
    }
    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &png());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let output = fixture.cli(&[
        "batch",
        "ocr",
        "--batch-id",
        "ocr",
        "--model",
        "absent",
        "--endpoint",
        &url,
        "--timeout-seconds",
        "1",
    ]);
    assert_eq!(output.status.code(), Some(3));
    assert!(
        fixture
            .ledger()
            .to_string()
            .contains("ollama_network_error")
    );
    assert_eq!(fixture.state().review_queue().len(), 1);
}

#[test]
fn model_digest_change_makes_a_new_proposal_and_stale_generation_cannot_be_confirmed() {
    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &png());
    let mock = Mock::new("𠀀");
    assert_ok(&fixture.ocr(&mock, &[]));
    let exported = fixture.cli(&["batch", "review-export", "--batch-id", "ocr"]);
    assert_ok(&exported);
    let data = value(&exported);
    let review = data["review_id"].as_str().unwrap();
    mock.state.lock().unwrap().digest = "fixture-digest-2".into();
    assert_ok(&fixture.ocr(&mock, &[]));
    assert_eq!(mock.chats(), 2);
    assert_eq!(fixture.ledger()["records"].as_object().unwrap().len(), 2);
    let before = fixture.state();
    let sha = before.items[0].current_sha256.as_ref().unwrap();
    assert_ok(&fixture.cli(&[
        "batch",
        "retry",
        "--batch-id",
        "ocr",
        "--character",
        "𠀀",
        "--sha256",
        sha,
        "--reason",
        "explicit retry",
    ]));
    let current = fixture.state();
    let output = fixture.cli(&[
        "batch",
        "review-confirm-matches",
        "--batch-id",
        "ocr",
        "--review-id",
        review,
        "--confirm-reviewed",
    ]);
    assert!(!output.status.success());
    assert_eq!(fixture.state(), current);
}

#[test]
fn unavailable_model_digest_does_not_reuse_a_failure_from_another_environment() {
    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &png());
    let mock = Mock::new("𠀀");

    mock.state.lock().unwrap().installed = false;
    assert_eq!(fixture.ocr(&mock, &[]).status.code(), Some(3));
    let first = fixture.ledger();
    let first = first["records"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(first["config"]["model_digest"], "unavailable");
    assert_eq!(first["failure"], "model_not_installed_or_digest_missing");
    assert_eq!(mock.chats(), 0);

    {
        let mut server = mock.state.lock().unwrap();
        server.installed = true;
        server.vision = false;
    }
    assert_eq!(fixture.ocr(&mock, &[]).status.code(), Some(3));
    let second = fixture.ledger();
    let records = second["records"].as_object().unwrap();
    assert_eq!(records.len(), 1);
    let second = records.values().next().unwrap();
    assert_eq!(second["config"]["model_digest"], "unavailable");
    assert_eq!(second["failure"], "model_does_not_support_vision");
    assert_eq!(mock.chats(), 0);
}

#[test]
fn confirmed_canonical_resources_need_zero_provider_or_browser_requests() {
    let fixture = Fixture::new();
    let characters: Vec<_> = (0x20000..0x20040)
        .map(|code| char::from_u32(code).unwrap())
        .collect();
    fixture.batch(&characters, &png());
    let state = fixture.state();
    for item in &state.items {
        fixture
            .store
            .attest(HumanAttestationRequest {
                identity: item.identity.clone(),
                expected_sha256: item.current_sha256.clone().unwrap(),
                decision: HumanDecision::Approve,
                reason: "synthetic explicit review".into(),
            })
            .unwrap();
    }
    let mock = Mock::new("?");
    assert_ok(&fixture.ocr(&mock, &[]));
    assert!(fixture.state().is_resolved());
    assert!(mock.state.lock().unwrap().requests.is_empty());
    let names: Vec<_> = characters.iter().map(char::to_string).collect();
    let mut args = vec!["batch", "start", "--batch-id", "reuse"];
    args.extend(names.iter().map(String::as_str));
    assert_ok(&fixture.cli(&args));
    assert_ok(&fixture.cli(&["batch", "run", "--batch-id", "reuse"]));
    assert_ok(&fixture.cli(&[
        "batch",
        "ocr",
        "--batch-id",
        "reuse",
        "--model",
        "fixture:vision",
        "--endpoint",
        &mock.url,
    ]));
    let state = BatchRuntime::open(fixture.store.root(), "reuse")
        .unwrap()
        .load()
        .unwrap()
        .unwrap();
    assert!(state.is_resolved());
    assert!(state.items.iter().all(|item| item.attempts.is_empty()));
    assert!(mock.state.lock().unwrap().requests.is_empty());
}

fn reference_png(character: char) -> Vec<u8> {
    // Только генератор входного fixture из закреплённых масок, без собственного
    // валидатора или классификатора. Статус ниже определяет настоящий v6.
    let mut bytes = Vec::new();
    flate2::read::ZlibDecoder::new(
        &include_bytes!("../src/data/kanjivg-r20250816.maskdb.zlib")[..],
    )
    .read_to_end(&mut bytes)
    .unwrap();
    let entry = bytes[8..]
        .as_chunks::<516>()
        .0
        .iter()
        .find(|entry| u32::from_le_bytes(entry[..4].try_into().unwrap()) == u32::from(character))
        .unwrap();
    let mut image = image::RgbaImage::from_pixel(128, 128, image::Rgba([255, 255, 255, 255]));
    for y in 0..64 {
        for x in 0..64 {
            let index = y * 64 + x;
            if entry[4 + index / 8] & (1 << (index % 8)) != 0 {
                for dy in 0..2 {
                    for dx in 0..2 {
                        image.put_pixel(
                            (x * 2 + dx) as u32,
                            (y * 2 + dy) as u32,
                            image::Rgba([0, 0, 0, 255]),
                        );
                    }
                }
            }
        }
    }
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut png, image::ImageFormat::Png)
        .unwrap();
    png.into_inner()
}

#[test]
fn v6_verified_and_rejected_candidates_do_not_call_ocr() {
    for (label, status) in [
        ('保', SemanticStatus::Verified),
        ('倉', SemanticStatus::Rejected),
    ] {
        let fixture = Fixture::new();
        fixture.batch(&[label], &reference_png('保'));
        match &fixture.state().items[0].attempts[0].result {
            BatchAttemptInput::Candidate { candidate } => {
                assert_eq!(candidate.automated.status, status)
            }
            _ => panic!("нет кандидата"),
        }
        let mock = Mock::new("?");
        assert_ok(&fixture.ocr(&mock, &[]));
        assert!(mock.state.lock().unwrap().requests.is_empty());
    }
}

#[test]
fn gif_delta_frames_are_composited_and_the_v6_frame_is_sent_as_scaled_png() {
    let mut bytes = Vec::new();
    {
        let mut encoder = gif::Encoder::new(&mut bytes, 64, 64, &[255, 255, 255, 0, 0, 0]).unwrap();
        let mut first = vec![0u8; 64 * 64];
        for y in 10..50 {
            for x in 10..14 {
                first[y * 64 + x] = 1;
            }
        }
        encoder
            .write_frame(&gif::Frame {
                width: 64,
                height: 64,
                buffer: first.into(),
                dispose: gif::DisposalMethod::Keep,
                ..Default::default()
            })
            .unwrap();
        encoder
            .write_frame(&gif::Frame {
                left: 45,
                top: 10,
                width: 4,
                height: 40,
                buffer: vec![1u8; 4 * 40].into(),
                dispose: gif::DisposalMethod::Keep,
                ..Default::default()
            })
            .unwrap();
    }
    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &bytes);
    let mock = Mock::new("?");
    assert_ok(&fixture.ocr(&mock, &[]));
    let ledger = fixture.ledger();
    let record = ledger["records"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert_eq!(record["format"], "gif");
    assert_eq!(record["representative_frame"], 1);
    assert_eq!(record["dimensions"], json!([128, 128]));
    let server = mock.state.lock().unwrap();
    let request = &server
        .requests
        .iter()
        .find(|(path, _)| path == "/api/chat")
        .unwrap()
        .1;
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD
        .decode(request["messages"][0]["images"][0].as_str().unwrap())
        .unwrap();
    assert_eq!(record["prepared_sha256"], sha256_hex(&png));
    let image = image::load_from_memory(&png).unwrap().to_rgba8();
    assert_eq!(image.dimensions(), (128, 128));
    assert!(
        image.get_pixel(23, 60)[0] < 30,
        "сохранён штрих из первого кадра"
    );
    assert!(
        image.get_pixel(93, 60)[0] < 30,
        "сохранён штрих из delta-frame"
    );
}

#[test]
fn interrupted_process_resumes_completed_cache_and_discards_stale_network_result() {
    use std::process::Stdio;
    use std::time::Instant;
    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &png());
    let mock = Mock::new("𠀀");
    assert_ok(&fixture.ocr(&mock, &[]));
    let interrupted_gate = Arc::new(AtomicBool::new(false));
    mock.state.lock().unwrap().response_gate = Some(interrupted_gate.clone());
    let command = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kanji-assets"));
        command
            .arg("--store")
            .arg(fixture.store.root())
            .arg("--repository-root")
            .arg(fixture.temp.path().join("repository"))
            .args([
                "--output",
                "json",
                "batch",
                "ocr",
                "--batch-id",
                "ocr",
                "--model",
                "fixture:vision",
                "--endpoint",
                &mock.url,
                "--rescan",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };
    let mut interrupted = command().spawn().unwrap();
    let started = Instant::now();
    while mock.chats() < 2 {
        assert!(started.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(5));
    }
    interrupted.kill().unwrap();
    interrupted.wait().unwrap();
    interrupted_gate.store(true, Ordering::Release);
    let before = fixture.state();
    assert_eq!(before.review_queue().len(), 1);
    // Завершённая запись пережила остановку процесса; повтор без rescan не вызывает chat.
    assert_ok(&fixture.ocr(&mock, &[]));
    assert_eq!(mock.chats(), 2);
    let ledger = fixture.ledger();
    let gate = Arc::new(AtomicBool::new(false));
    {
        let mut server = mock.state.lock().unwrap();
        server.response_gate = Some(gate.clone());
        server.delay = Duration::ZERO;
    }
    let current = command().spawn().unwrap();
    let started = Instant::now();
    while mock.chats() < 3 {
        assert!(started.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(5));
    }
    let sha = before.items[0].current_sha256.as_ref().unwrap();
    assert_ok(&fixture.cli(&[
        "batch",
        "retry",
        "--batch-id",
        "ocr",
        "--character",
        "𠀀",
        "--sha256",
        sha,
        "--reason",
        "changed while OCR was waiting",
    ]));
    gate.store(true, Ordering::Release);
    let output = current.wait_with_output().unwrap();
    assert_ok(&output);
    assert_eq!(value(&output)["ocr_summary"]["saved"], 0);
    assert_eq!(fixture.ledger(), ledger);
    assert!(fixture.state().items[0].generation > before.items[0].generation);
}

#[test]
fn stale_provider_failure_is_not_reported_or_saved_after_candidate_changes() {
    use std::process::Stdio;
    use std::time::Instant;

    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &png());
    let before = fixture.state();
    let mock = Mock::new("𠀀");
    let gate = Arc::new(AtomicBool::new(false));
    {
        let mut server = mock.state.lock().unwrap();
        server.http_status = 503;
        server.response_gate = Some(gate.clone());
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_kanji-assets"));
    command
        .arg("--store")
        .arg(fixture.store.root())
        .arg("--repository-root")
        .arg(fixture.temp.path().join("repository"))
        .args([
            "--output",
            "json",
            "batch",
            "ocr",
            "--batch-id",
            "ocr",
            "--model",
            "fixture:vision",
            "--endpoint",
            &mock.url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().unwrap();
    let started = Instant::now();
    while mock.chats() == 0 {
        assert!(started.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(5));
    }
    let sha = before.items[0].current_sha256.as_ref().unwrap();
    assert_ok(&fixture.cli(&[
        "batch",
        "retry",
        "--batch-id",
        "ocr",
        "--character",
        "𠀀",
        "--sha256",
        sha,
        "--reason",
        "candidate changed while OCR was waiting",
    ]));
    gate.store(true, Ordering::Release);
    let output = child.wait_with_output().unwrap();
    assert_ok(&output);
    let response = value(&output);
    assert_eq!(response["issues"], json!([]));
    assert_eq!(response["ocr_summary"]["saved"], 0);
    assert_eq!(response["ocr_summary"]["chat_requests"], 1);
    assert!(
        !fixture
            .store
            .root()
            .join(".runtime/batches/ocr/ocr/suggestions.json")
            .exists()
    );
    assert!(fixture.state().items[0].generation > before.items[0].generation);
}

#[test]
fn changed_sha_does_not_inherit_the_old_proposal_or_snapshot() {
    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &png());
    let mock = Mock::new("𠀀");
    assert_ok(&fixture.ocr(&mock, &[]));
    let old = fixture.cli(&["batch", "review-export", "--batch-id", "ocr"]);
    assert_ok(&old);
    let data = value(&old);
    let review = data["review_id"].as_str().unwrap();
    let before = fixture.state();
    let sha = before.items[0].current_sha256.as_ref().unwrap();
    assert_ok(&fixture.cli(&[
        "batch",
        "retry",
        "--batch-id",
        "ocr",
        "--character",
        "𠀀",
        "--sha256",
        sha,
        "--reason",
        "explicit new candidate",
    ]));
    let mut image = image::load_from_memory(&png()).unwrap().to_rgba8();
    image.put_pixel(30, 30, image::Rgba([0, 0, 0, 255]));
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();
    let bytes = encoded.into_inner();
    let mut runtime = BatchRuntime::open(fixture.store.root(), "ocr").unwrap();
    let mut state = runtime.load().unwrap().unwrap();
    let record = asset_store::AssetRecord {
        identity: identity('𠀀'),
        storage_path: "fixture".into(),
        consumer_filename: "fixture".into(),
        sha256: sha256_hex(&bytes),
        byte_length: bytes.len() as u64,
        format: asset_store::DetectedFormat::Png,
        provenance: asset_store::Provenance {
            source_kind: "fixture".into(),
            source_name: "replacement.png".into(),
        },
        lifecycle: asset_store::LifecycleState::Pending,
        validation: None,
        human_attestation: None,
        domain_metadata: None,
    };
    use asset_store::SemanticValidator;
    let decision = KanjiImageValidator::new()
        .validate(&record, &mut Cursor::new(&bytes))
        .unwrap();
    assert_eq!(decision.status, SemanticStatus::Uncertain);
    let candidate = runtime
        .persist_candidate(
            &bytes,
            asset_store::ValidationRecord {
                validator: KanjiImageValidator::validator_identity(),
                content_sha256: record.sha256.clone(),
                status: decision.status,
                evidence: decision.evidence,
            },
            true,
        )
        .unwrap();
    state
        .record_attempt(&identity('𠀀'), BatchAttemptInput::Candidate { candidate })
        .unwrap();
    runtime.save(&state).unwrap();
    drop(runtime);
    let without_scan = fixture.cli(&["batch", "review-export", "--batch-id", "ocr"]);
    assert_ok(&without_scan);
    let current = value(&without_scan);
    let file = fs::read_to_string(current["review_artifact"].as_str().unwrap()).unwrap();
    assert!(!file.contains("fixture-digest"));
    let snapshot: Value = serde_json::from_slice(
        &fs::read(
            Path::new(current["review_artifact"].as_str().unwrap())
                .parent()
                .unwrap()
                .join("snapshot.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(snapshot["rows"][0].get("ocr").is_none());
    let stale = fixture.cli(&[
        "batch",
        "review-confirm-matches",
        "--batch-id",
        "ocr",
        "--review-id",
        review,
        "--confirm-reviewed",
    ]);
    assert!(!stale.status.success());
    assert_ok(&fixture.ocr(&mock, &[]));
    assert_eq!(mock.chats(), 2);
    assert_eq!(fixture.ledger()["records"].as_object().unwrap().len(), 2);
    assert!(
        fixture.state().items[0]
            .human_decisions
            .iter()
            .all(|decision| decision.action != asset_store::batch::HumanBatchAction::Confirm)
    );
}

#[test]
fn model_replacement_during_inference_is_saved_as_failure_and_can_resume() {
    use std::process::Stdio;
    use std::time::Instant;
    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &png());
    let before = fixture.state();
    let mock = Mock::new("𠀀");
    let gate = Arc::new(AtomicBool::new(false));
    mock.state.lock().unwrap().response_gate = Some(gate.clone());
    let child = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
        .arg("--store")
        .arg(fixture.store.root())
        .arg("--repository-root")
        .arg(fixture.temp.path().join("repository"))
        .args([
            "--output",
            "json",
            "batch",
            "ocr",
            "--batch-id",
            "ocr",
            "--model",
            "fixture:vision",
            "--endpoint",
            &mock.url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while mock.chats() == 0 {
        assert!(start.elapsed() < Duration::from_secs(10));
        thread::sleep(Duration::from_millis(5));
    }
    mock.state.lock().unwrap().digest = "replaced-digest".into();
    gate.store(true, Ordering::Release);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert_eq!(fixture.state(), before);
    assert!(
        fixture
            .ledger()
            .to_string()
            .contains("model_changed_during_scan")
    );
    let ledger = fixture.ledger();
    let record = ledger["records"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap();
    assert!(record["observed"].is_null());
    assert_eq!(record["matches_expected"], false);
    assert_ok(&fixture.ocr(&mock, &[]));
    assert_eq!(mock.chats(), 2);
    assert_eq!(fixture.state(), before);
}

#[test]
fn real_cli_headless_and_json_review_never_launch_the_configured_program() {
    let fixture = Fixture::new();
    fixture.batch(&['𠀀'], &png());
    let before = fixture.state();
    let editor = fixture.temp.path().join("editor-that-must-not-run");
    use std::os::unix::fs::PermissionsExt;
    fs::write(&editor, b"#!/bin/sh\ntouch \"$0.launched\"\n").unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o700)).unwrap();
    for output in ["human", "json"] {
        let result = Command::new(env!("CARGO_BIN_EXE_kanji-assets"))
            .arg("--store")
            .arg(fixture.store.root())
            .arg("--repository-root")
            .arg(fixture.temp.path().join("repository"))
            .args([
                "--output",
                output,
                "batch",
                "review-interactive",
                "--batch-id",
                "ocr",
            ])
            .env("GIT_EDITOR", &editor)
            .env("TERM", "xterm")
            .env_remove("CI")
            .env_remove("CODEX_NONINTERACTIVE")
            .env_remove("GITHUB_ACTIONS")
            .output()
            .unwrap();
        assert!(!result.status.success());
        assert_eq!(fixture.state(), before);
    }
    assert!(
        !fixture
            .temp
            .path()
            .join("editor-that-must-not-run.launched")
            .exists()
    );
}
