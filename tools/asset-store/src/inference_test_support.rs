//! Подменённый HTTP внешнего `llama-server` для тестов транспорта и задач.
//! Не доказывает работу настоящего сервера: только контракт клиента.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

pub(crate) type ChatHook = Box<dyn Fn() + Send>;

pub(crate) struct MockState {
    pub requests: Vec<(String, Value)>,
    pub alias: String,
    pub model_path: String,
    pub build_info: String,
    pub vision: bool,
    pub content: String,
    pub finish_reason: String,
    pub chat_status: u16,
    pub chat_error: String,
    pub on_chat: Option<ChatHook>,
}

pub(crate) struct MockLlama {
    pub url: String,
    pub state: Arc<Mutex<MockState>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl MockLlama {
    pub(crate) fn new(content: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(Mutex::new(MockState {
            requests: Vec::new(),
            alias: "external-model".into(),
            model_path: "/models/fixture.gguf".into(),
            build_info: "b0-fixture".into(),
            vision: true,
            content: content.into(),
            finish_reason: "stop".into(),
            chat_status: 200,
            chat_error: String::new(),
            on_chat: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));
        let (worker_state, worker_stop) = (state.clone(), stop.clone());
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
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let (path, body) = read_request(&mut stream);
                let (status, response) = {
                    let mut state = worker_state.lock().unwrap();
                    state.requests.push((path.clone(), body));
                    if path == "/v1/chat/completions"
                        && let Some(hook) = &state.on_chat
                    {
                        hook();
                    }
                    respond(&state, &path)
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
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

    pub(crate) fn chats(&self) -> Vec<Value> {
        self.state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(path, _)| path == "/v1/chat/completions")
            .map(|(_, body)| body.clone())
            .collect()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> (String, Value) {
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
                        .map(|value| value.trim().parse::<usize>().unwrap())
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
    (path, body)
}

fn respond(state: &MockState, path: &str) -> (u16, String) {
    match path {
        "/health" => (200, json!({"status": "ok"}).to_string()),
        "/props" => (
            200,
            json!({
                "model_alias": state.alias,
                "model_path": state.model_path,
                "build_info": state.build_info,
                "modalities": {"vision": state.vision},
            })
            .to_string(),
        ),
        "/v1/models" => (
            200,
            json!({"data": [{"id": state.alias, "meta": {"n_ctx": 4096}}]}).to_string(),
        ),
        "/v1/chat/completions" if state.chat_status != 200 => (
            state.chat_status,
            json!({"error": {"code": state.chat_status, "message": state.chat_error}}).to_string(),
        ),
        "/v1/chat/completions" => (
            200,
            json!({
                "model": state.alias,
                "choices": [{"finish_reason": state.finish_reason, "message": {"role": "assistant", "content": state.content}}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5},
                "timings": {"prompt_ms": 1.5, "predicted_ms": 2.5},
            })
            .to_string(),
        ),
        other => (404, json!({"error": format!("unexpected {other}")}).to_string()),
    }
}

impl Drop for MockLlama {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Файл переопределений: обе задачи через внешний сервер мока.
pub(crate) fn external_config(directory: &std::path::Path, url: &str) -> std::path::PathBuf {
    let path = directory.join("inference-test.toml");
    std::fs::write(
        &path,
        format!(
            "[backends.llama]\nmode = \"external\"\nendpoint = \"{url}\"\n\
             [tasks.kanji_ocr]\nstructured_output = \"json_schema\"\n"
        ),
    )
    .unwrap();
    path
}
