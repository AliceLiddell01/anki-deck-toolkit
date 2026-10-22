//! Поддельный процесс `llama-server` для контрактов менеджера процесса.
//!
//! Тестовый бинарник перезапускает сам себя через shell-обёртку и в этом
//! режиме обслуживает HTTP-контракт llama-server. Это проверяет владение
//! процессом, аргументы и протокол клиента, но не работу настоящей модели.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

const ARGS: &str = "ASSET_STORE_FAKE_LLAMA_ARGS";
pub const MODE: &str = "ASSET_STORE_FAKE_LLAMA_MODE";
pub const LOG: &str = "ASSET_STORE_FAKE_LLAMA_LOG";
pub const CONTENT: &str = "ASSET_STORE_FAKE_LLAMA_CONTENT";

/// Shell-обёртка, которую менеджер запускает как исполняемый файл сервера.
/// Аргументы сервера остаются и в argv (после `--` это фильтры libtest, не
/// совпадающие ни с одним тестом при `--exact`): по ним, как у настоящего
/// сервера, менеджер доказывает владение процессом.
pub fn write_script(directory: &Path) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let path = directory.join("llama-server");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n{ARGS}=\"$(printf '%s\\037' \"$@\")\" exec '{}' fake_llama::fake_llama_server_process --exact --nocapture --test-threads=1 -- \"$@\" >/dev/null\n",
            exe.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// События поддельного сервера из журнала.
pub fn events(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

pub fn process_alive(pid: i64) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        !stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
    })
}

fn append(log: &Option<String>, event: Value) {
    if let Some(log) = log {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap();
        writeln!(file, "{event}").unwrap();
    }
}

#[test]
fn fake_llama_server_process() {
    let Ok(raw) = std::env::var(ARGS) else {
        return;
    };
    let args: Vec<String> = raw
        .split('\u{1f}')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    let value = |flag: &str| {
        args.windows(2)
            .find(|pair| pair[0] == flag)
            .map(|pair| pair[1].clone())
    };
    let mode = std::env::var(MODE).unwrap_or_else(|_| "ok".into());
    let log = std::env::var(LOG).ok();
    append(
        &log,
        json!({"event": "start", "pid": std::process::id(), "args": args}),
    );
    match mode.as_str() {
        "exit_mmproj" => {
            eprintln!("clip_model_loader: failed to load mmproj model");
            std::process::exit(1);
        }
        "exit_model" => {
            eprintln!("llama_model_load: error loading model: failed to load model");
            std::process::exit(1);
        }
        _ => {}
    }
    let host = value("--host").unwrap();
    let port = value("--port").unwrap();
    let alias = if mode == "foreign" {
        "someone-else".to_owned()
    } else {
        value("--alias").unwrap()
    };
    let api_key = value("--api-key");
    let model = value("--model").unwrap();
    let content = std::env::var(CONTENT).unwrap_or_else(|_| "{\"observed\":\"?\"}".into());
    let listener = TcpListener::bind(format!("{host}:{port}")).unwrap();
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let Some((path, authorization, body)) = read(&mut stream) else {
            continue;
        };
        let authorized = api_key
            .as_ref()
            .is_none_or(|key| authorization.as_deref() == Some(&format!("Bearer {key}")));
        let (status, response) = match path.as_str() {
            "/health" if mode == "hang" => (503, json!({"error": "loading"})),
            "/health" => (200, json!({"status": "ok"})),
            "/props" => (
                200,
                json!({
                    "model_alias": alias, "model_path": model, "build_info": "b0-fake",
                    "modalities": {"vision": mode != "no_vision"},
                }),
            ),
            "/v1/models" => (
                200,
                json!({"data": [{"id": alias, "meta": {"n_ctx": 4096}}]}),
            ),
            "/v1/chat/completions" if !authorized => (401, json!({"error": "unauthorized"})),
            "/v1/chat/completions" => {
                append(&log, json!({"event": "chat", "body": body}));
                if mode == "slow" {
                    std::thread::sleep(Duration::from_secs(60));
                }
                (
                    200,
                    json!({
                        "model": alias,
                        "choices": [{"finish_reason": "stop", "message": {"content": content}}],
                        "timings": {"prompt_ms": 1.0, "predicted_ms": 1.0},
                    }),
                )
            }
            _ => (404, json!({})),
        };
        let text = response.to_string();
        let _ = write!(
            stream,
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
            text.len()
        );
    }
}

fn read(stream: &mut TcpStream) -> Option<(String, Option<String>, Value)> {
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let (end, length) = loop {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
            let length = header
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map(|value| value.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            break (end + 4, length);
        }
    };
    while bytes.len() < end + length {
        let count = stream.read(&mut buffer).ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    let header = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let path = header.split_whitespace().nth(1)?.to_owned();
    let authorization = header.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.trim().to_owned())
    });
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[end..end + length]).ok()?
    };
    Some((path, authorization, body))
}
