//! 外部题目导入服务：SampleTester API（20030）与 Competitive Companion（10043）。
//!
//! 原 Electron 版在 main.js 里用 Node http 起两个本地服务。Tauri 侧没有 Node，
//! 这里用 tokio TcpListener 实现一个最小的 HTTP/1.1 JSON 服务：
//!   - `POST /createNewProblem`（20030）接收 SampleTester API 的题目；
//!   - `POST /`、`/competitive-companion`、`/add`、`/receive`、`/companion`（10043）
//!     接收 Competitive Companion 的题目。
//! 收到合法题目后广播 `sample-tester-create-problem`，由渲染层创建题目文件。

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::{json, Map, Value};
use tauri::AppHandle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::events;
use crate::misc;

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
const READ_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy)]
enum ApiKind {
    SampleTester,
    CompetitiveCompanion,
}

impl ApiKind {
    fn label(self) -> &'static str {
        match self {
            Self::SampleTester => "SampleTesterAPI",
            Self::CompetitiveCompanion => "CompetitiveCompanion",
        }
    }

    fn matches_path(self, path: &str) -> bool {
        match self {
            Self::SampleTester => path == "/createNewProblem",
            Self::CompetitiveCompanion => {
                matches!(
                    path,
                    "/"
                        | "/competitive-companion"
                        | "/add"
                        | "/receive"
                        | "/companion"
                )
            }
        }
    }
}

static SERVER_TASKS: Lazy<Mutex<Vec<tauri::async_runtime::JoinHandle<()>>>> =
    Lazy::new(|| Mutex::new(Vec::new()));

static PLACEHOLDER_SAMPLE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:见|参见|参阅)\s*附\s*件|附\s*件\s*中|see\s+(?:the\s+)?attach|refer(?:s|red)?\s+to\s+(?:the\s+)?attach",
    )
    .expect("无效的 Competitive Companion 占位样例正则")
});

/// 启动两个本地服务。端口被占用时只记录日志，不影响应用启动。
pub fn start(app: &AppHandle) {
    start_server(app.clone(), 20030, ApiKind::SampleTester);
    start_server(app.clone(), 10043, ApiKind::CompetitiveCompanion);
}

/// 终止监听任务（应用退出时调用）。已建立的连接会随进程退出结束。
pub fn shutdown() {
    if let Ok(mut tasks) = SERVER_TASKS.lock() {
        for task in tasks.drain(..) {
            task.abort();
        }
    }
}

fn start_server(app: AppHandle, port: u16, kind: ApiKind) {
    let task = tauri::async_runtime::spawn(async move {
        let listener = match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => listener,
            Err(err) => {
                let message = if err.kind() == std::io::ErrorKind::AddrInUse {
                    format!(
                        "[{}] 端口 {} 被占用，可能被其他工具占用（如 cph）。可在 Competitive Companion 中添加 http://127.0.0.1:{}/ 作为自定义端口。",
                        kind.label(),
                        port,
                        port
                    )
                } else {
                    format!("[{}] 监听 127.0.0.1:{} 失败: {err}", kind.label(), port)
                };
                misc::log_line("warn", &message);
                return;
            }
        };

        misc::log_line(
            "info",
            &format!(
                "[{}] 服务已启动 http://127.0.0.1:{}",
                kind.label(),
                port
            ),
        );

        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(err) = handle_connection(app, stream, kind).await {
                            misc::log_line(
                                "warn",
                                &format!("[{}] 处理来自 {addr} 的请求失败: {err}", kind.label()),
                            );
                        }
                    });
                }
                Err(err) => {
                    misc::log_line(
                        "warn",
                        &format!("[{}] accept 失败: {err}", kind.label()),
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }
        }
    });

    if let Ok(mut tasks) = SERVER_TASKS.lock() {
        tasks.push(task);
    }
}

struct HttpRequest {
    method: String,
    path: String,
    body: Vec<u8>,
}

struct ReadError {
    status: u16,
    message: String,
}

impl ReadError {
    fn bad(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            message: message.into(),
        }
    }

    fn too_large(message: impl Into<String>) -> Self {
        Self {
            status: 413,
            message: message.into(),
        }
    }
}

async fn read_with_timeout(
    stream: &mut TcpStream,
    buffer: &mut [u8],
) -> Result<usize, ReadError> {
    match tokio::time::timeout(READ_TIMEOUT, stream.read(buffer)).await {
        Ok(Ok(read)) => Ok(read),
        Ok(Err(err)) => Err(ReadError::bad(format!("读取请求失败: {err}"))),
        Err(_) => Err(ReadError::bad("读取请求超时")),
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn normalize_path(path: &str) -> String {
    let mut path = path.to_string();
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    if path.is_empty() {
        "/".to_string()
    } else {
        path
    }
}

async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, ReadError> {
    let mut buffer = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];

    let header_end = loop {
        if let Some(index) = find_header_end(&buffer) {
            break index;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return Err(ReadError::bad("请求头过大"));
        }
        let read = read_with_timeout(stream, &mut chunk).await?;
        if read == 0 {
            return Err(ReadError::bad("请求不完整"));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let header_text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or_else(|| ReadError::bad("缺少请求行"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| ReadError::bad("缺少请求方法"))?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .ok_or_else(|| ReadError::bad("缺少请求路径"))?;
    let path = normalize_path(target.split('?').next().unwrap_or("/"));

    let mut content_length = 0usize;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .trim()
                .parse::<usize>()
                .map_err(|_| ReadError::bad("Content-Length 无效"))?;
        } else if name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }

    if chunked {
        return Err(ReadError::bad("暂不支持 chunked 请求体"));
    }
    if content_length > MAX_BODY_BYTES {
        return Err(ReadError::too_large("请求体超过 2MB 限制"));
    }

    let body_start = header_end + 4;
    let mut body = if body_start < buffer.len() {
        buffer[body_start..].to_vec()
    } else {
        Vec::new()
    };
    while body.len() < content_length {
        let read = read_with_timeout(stream, &mut chunk).await?;
        if read == 0 {
            return Err(ReadError::bad("请求体不完整"));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);

    Ok(HttpRequest { method, path, body })
}

async fn write_json(
    stream: &mut TcpStream,
    status: u16,
    payload: &Value,
) -> Result<(), String> {
    let body = payload.to_string();
    let response = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status_text(status),
        body.as_bytes().len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|err| err.to_string())?;
    stream.flush().await.map_err(|err| err.to_string())
}

async fn write_no_content(stream: &mut TcpStream) -> Result<(), String> {
    let response = "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: Content-Type\r\nAccess-Control-Allow-Methods: POST, OPTIONS\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream
        .write_all(response.as_bytes())
        .await
        .map_err(|err| err.to_string())?;
    stream.flush().await.map_err(|err| err.to_string())
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Internal Server Error",
    }
}

async fn handle_connection(
    app: AppHandle,
    mut stream: TcpStream,
    kind: ApiKind,
) -> Result<(), String> {
    let request = match read_request(&mut stream).await {
        Ok(request) => request,
        Err(err) => {
            let _ = write_json(
                &mut stream,
                err.status,
                &json!({ "code": err.status, "message": err.message }),
            )
            .await;
            return Ok(());
        }
    };

    if request.method == "OPTIONS" {
        return write_no_content(&mut stream).await;
    }
    if request.method != "POST" || !kind.matches_path(&request.path) {
        return write_json(
            &mut stream,
            404,
            &json!({ "code": 404, "message": "Not Found" }),
        )
        .await;
    }

    let data: Value = match serde_json::from_slice::<Value>(&request.body) {
        Ok(value) if value.is_object() => value,
        _ => {
            return write_json(
                &mut stream,
                400,
                &json!({ "code": 400, "message": "Invalid JSON body" }),
            )
            .await;
        }
    };

    let payload = match kind {
        ApiKind::SampleTester => data,
        ApiKind::CompetitiveCompanion => match normalize_competition_companion_payload(&data) {
            Ok(payload) => payload,
            Err(err) => {
                return write_json(
                    &mut stream,
                    400,
                    &json!({
                        "code": 400,
                        "message": format!("Invalid parameters: {}", err.message),
                        "invalidField": err.field
                    }),
                )
                .await;
            }
        },
    };

    if let Err(err) = validate_sample_payload(&payload) {
        return write_json(
            &mut stream,
            400,
            &json!({
                "code": 400,
                "message": format!("Invalid parameters: {}", err.message),
                "invalidField": err.field
            }),
        )
        .await;
    }

    let problem_name = payload
        .get("problemName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let sample_count = payload
        .get("samples")
        .and_then(Value::as_array)
        .map(|samples| samples.len())
        .unwrap_or(0);
    misc::log_line(
        "info",
        &format!(
            "[{}] 收到题目: {} 样例数: {}",
            kind.label(),
            problem_name,
            sample_count
        ),
    );
    events::broadcast_one(&app, "sample-tester-create-problem", payload);

    write_json(
        &mut stream,
        200,
        &json!({ "code": 200, "message": "Problem created successfully" }),
    )
    .await
}

// ------------------------------------------------------------------ 校验 / CC 转换

#[derive(Debug)]
struct ValidationError {
    message: String,
    field: String,
}

impl ValidationError {
    fn new(message: impl Into<String>, field: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            field: field.into(),
        }
    }
}

fn validate_sample_payload(data: &Value) -> Result<(), ValidationError> {
    let object = data
        .as_object()
        .ok_or_else(|| ValidationError::new("body must be JSON object", "body"))?;

    let problem_name = object
        .get("problemName")
        .and_then(Value::as_str)
        .unwrap_or("");
    if problem_name.is_empty() {
        return Err(ValidationError::new(
            "problemName missing.",
            "problemName",
        ));
    }

    let samples = object
        .get("samples")
        .and_then(Value::as_array)
        .ok_or_else(|| ValidationError::new("samples must not be empty.", "samples"))?;
    if samples.is_empty() {
        return Err(ValidationError::new(
            "samples must not be empty.",
            "samples",
        ));
    }

    let mut ids = HashSet::new();
    for sample in samples {
        let sample = sample
            .as_object()
            .ok_or_else(|| ValidationError::new("sample must be object", "samples"))?;
        let id = sample
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| ValidationError::new("sample id invalid", "id"))?;
        if id <= 0 {
            return Err(ValidationError::new("sample id invalid", "id"));
        }
        if !ids.insert(id) {
            return Err(ValidationError::new("duplicate sample id", "id"));
        }
        if !sample.get("input").map(Value::is_string).unwrap_or(false) {
            return Err(ValidationError::new("input must be string", "input"));
        }
        if !sample.get("output").map(Value::is_string).unwrap_or(false) {
            return Err(ValidationError::new("output must be string", "output"));
        }
        if let Some(time_limit) = sample.get("timeLimit") {
            match time_limit.as_i64() {
                Some(value) if value > 0 => {}
                _ => {
                    return Err(ValidationError::new(
                        "timeLimit must be positive integer",
                        "timeLimit",
                    ))
                }
            }
        }
    }

    Ok(())
}

fn first_non_empty_string(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        let value = object
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn host_from_url(value: &str) -> Option<String> {
    let parsed = url::Url::parse(value.trim()).ok()?;
    parsed
        .host_str()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(str::to_string)
}

fn is_placeholder_sample(input: &str, output: &str) -> bool {
    PLACEHOLDER_SAMPLE_RE.is_match(&format!("{input}\n{output}"))
}

fn normalize_competition_companion_payload(
    data: &Value,
) -> Result<Value, ValidationError> {
    let object = data
        .as_object()
        .ok_or_else(|| ValidationError::new("body must be JSON object", "body"))?;

    let title = first_non_empty_string(object, &["title", "name", "problemName"])
        .ok_or_else(|| ValidationError::new("problem title missing", "title"))?;

    let tests = object
        .get("tests")
        .and_then(Value::as_array)
        .ok_or_else(|| ValidationError::new("tests must not be empty", "tests"))?;
    if tests.is_empty() {
        return Err(ValidationError::new("tests must not be empty", "tests"));
    }

    let oj = object
        .get("group")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            object
                .get("source")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            object
                .get("url")
                .and_then(Value::as_str)
                .and_then(host_from_url)
        })
        .unwrap_or_else(|| "Competitive Companion".to_string());

    let time_limit_ms = object
        .get("timeLimit")
        .and_then(Value::as_f64)
        .filter(|value| *value > 0.0)
        .map(|value| {
            if value <= 50.0 {
                (value * 1000.0).round() as i64
            } else {
                value.round() as i64
            }
        });

    let raw_samples: Vec<(String, String)> = tests
        .iter()
        .map(|test| {
            let input = test
                .get("input")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let output = test
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            (input, output)
        })
        .collect();

    let filtered: Vec<(String, String)> = raw_samples
        .iter()
        .filter(|(input, output)| !is_placeholder_sample(input, output))
        .cloned()
        .collect();
    let usable = if filtered.is_empty() {
        raw_samples
    } else {
        if filtered.len() != tests.len() {
            misc::log_line(
                "info",
                &format!(
                    "[CompetitiveCompanion] 已自动过滤 {} 个附件引用占位样例",
                    tests.len() - filtered.len()
                ),
            );
        }
        filtered
    };

    let samples: Vec<Value> = usable
        .into_iter()
        .enumerate()
        .map(|(index, (input, output))| {
            let mut sample = Map::new();
            sample.insert("id".to_string(), json!(index as i64 + 1));
            sample.insert("input".to_string(), Value::String(input));
            sample.insert("output".to_string(), Value::String(output));
            if let Some(time_limit) = time_limit_ms {
                sample.insert("timeLimit".to_string(), json!(time_limit));
            }
            Value::Object(sample)
        })
        .collect();

    Ok(json!({
        "OJ": oj,
        "problemName": title,
        "samples": Value::Array(samples)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_placeholder_samples() {
        let payload = normalize_competition_companion_payload(&json!({
            "title": "Test",
            "group": "OJ",
            "tests": [
                { "input": "见附件中的 zoo.in", "output": "见附件中的 zoo.out" },
                { "input": "1 2", "output": "3" }
            ]
        }))
        .expect("normalize");
        let samples = payload["samples"].as_array().expect("samples");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0]["input"], "1 2");
    }

    #[test]
    fn keeps_raw_samples_when_all_are_placeholders() {
        let payload = normalize_competition_companion_payload(&json!({
            "title": "Test",
            "tests": [
                { "input": "见附件中的 zoo.in", "output": "see attach" }
            ]
        }))
        .expect("normalize");
        assert_eq!(payload["samples"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn validates_duplicate_ids() {
        let result = validate_sample_payload(&json!({
            "problemName": "Test",
            "samples": [
                { "id": 1, "input": "", "output": "" },
                { "id": 1, "input": "", "output": "" }
            ]
        }));
        assert!(result.is_err());
    }
}
