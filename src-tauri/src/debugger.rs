//! GDB 调试通道。
//!
//! 与原 Electron 版一致：不使用 GDB/MI，而是把 gdb 的提示符改成一个特殊字符串，
//! 以提示符作为"一条命令响应结束"的边界来解析文本输出。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::events;
use crate::ipc::Ipc;
use crate::paths;

const GDB_PROMPT: &str = ">>>>>>oicpp_gdb:";
const SYNC_TIMEOUT_MS: u64 = 4000;

struct GdbSession {
    child: Child,
    stdin: std::process::ChildStdin,
}

static GDB: Lazy<Mutex<Option<GdbSession>>> = Lazy::new(|| Mutex::new(None));
static CMD_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));
static RESPONSES: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));
static NOTIFY: Lazy<tokio::sync::Notify> = Lazy::new(|| tokio::sync::Notify::new());
static INFERIOR_RUNNING: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));
static APP: Lazy<Mutex<Option<AppHandle>>> = Lazy::new(|| Mutex::new(None));

/// `key -> (file, line, gdb 断点号)`；gdb 断点号为 0 表示尚未分配
static BREAKPOINTS: Lazy<Mutex<Vec<(String, String, i64, i64)>>> =
    Lazy::new(|| Mutex::new(Vec::new()));
static WATCHES: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));
static CURRENT_FILE: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

fn app_handle() -> Option<AppHandle> {
    APP.lock().ok().and_then(|guard| guard.clone())
}

fn broadcast_one(channel: &str, payload: Value) {
    if let Some(app) = app_handle() {
        events::broadcast_one(&app, channel, payload);
    }
}

fn broadcast(channel: &str, args: Vec<Value>) {
    if let Some(app) = app_handle() {
        events::broadcast(&app, channel, args);
    }
}

// ------------------------------------------------------------------ 变量/调用栈解析

fn variable_node(name: &str, value: &str, var_type: &str) -> Value {
    let trimmed = value.trim();
    let is_container = trimmed.starts_with('{') || trimmed.starts_with('[');
    let element_count = if is_container {
        trimmed
            .matches(',')
            .count()
            .checked_add(1)
            .map(|count| json!(count))
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    json!({
        "name": name,
        "type": var_type,
        "value": trimmed,
        "isArray": trimmed.starts_with('['),
        "isContainer": is_container,
        "elementCount": element_count,
        "children": []
    })
}

fn parse_locals(text: &str) -> Value {
    let mut map = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("No locals") || line.starts_with("No symbol") {
            continue;
        }
        if let Some((name, value)) = line.split_once('=') {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            map.insert(name.to_string(), variable_node(name, value, ""));
        }
    }
    let mut object = serde_json::Map::new();
    let mut names: Vec<String> = map.keys().cloned().collect();
    names.sort();
    for name in names {
        if let Some(node) = map.remove(&name) {
            object.insert(name, node);
        }
    }
    Value::Object(object)
}

fn parse_backtrace(text: &str) -> Vec<Value> {
    let pattern = regex::Regex::new(
        r"^#(?P<level>\d+)\s+(?:0x[0-9a-fA-F]+\s+in\s+)?(?P<func>[^\s(]+)\s*(?:\(.*?\))?\s*(?:at\s+(?P<file>[^:\r\n]+):(?P<line>\d+))?",
    )
    .ok();
    let mut frames = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('#') {
            continue;
        }
        let Some(re) = pattern.as_ref() else {
            break;
        };
        if let Some(caps) = re.captures(line) {
            frames.push(json!({
                "level": caps.name("level").and_then(|m| m.as_str().parse::<i64>().ok()).unwrap_or(0),
                "function": caps.name("func").map(|m| m.as_str()).unwrap_or("??"),
                "file": caps.name("file").map(|m| m.as_str().trim()).unwrap_or(""),
                "line": caps.name("line").and_then(|m| m.as_str().parse::<i64>().ok()).unwrap_or(0),
                "address": ""
            }));
        }
    }
    frames
}

// ------------------------------------------------------------------ 输出解析

fn handle_response(response: &str) {
    let text = response.replace('\u{1a}', " ");

    if let Ok(mut guard) = RESPONSES.lock() {
        guard.push(text.clone());
    }
    NOTIFY.notify_waiters();

    // 断点命中
    if let Ok(re) = regex::Regex::new(r"([A-Za-z]:)?([^:\r\n]+):(\d+):\d+:[begmidl]+:(0x[0-9A-Fa-f]+)") {
        if let Some(caps) = re.captures(&text) {
            INFERIOR_RUNNING.store(false, Ordering::SeqCst);
            let file = format!(
                "{}{}",
                caps.get(1).map(|m| m.as_str()).unwrap_or(""),
                caps.get(2).map(|m| m.as_str().trim()).unwrap_or("")
            );
            let line = caps
                .get(3)
                .and_then(|m| m.as_str().parse::<i64>().ok())
                .unwrap_or(0);
            let address = caps.get(4).map(|m| m.as_str()).unwrap_or("");
            broadcast_one(
                "debug-stopped",
                json!({
                    "reason": "breakpoint-hit",
                    "file": file,
                    "line": line,
                    "function": "",
                    "address": address
                }),
            );
            broadcast_one(
                "debug-breakpoint-hit",
                json!({ "file": file, "line": line, "address": address }),
            );
            refresh_variables_soon();
        }
    }

    // 被调试程序退出
    if let Ok(re) = regex::Regex::new(r"\[Inferior \d+ \(process \d+\) exited with code (\d+)\]") {
        if let Some(caps) = re.captures(&text) {
            INFERIOR_RUNNING.store(false, Ordering::SeqCst);
            let exit_code = caps
                .get(1)
                .and_then(|m| m.as_str().parse::<i64>().ok())
                .unwrap_or(0);
            broadcast_one("debug-program-exited", json!({ "exitCode": exit_code }));
            std::thread::sleep(Duration::from_millis(100));
            broadcast_one(
                "debug-stopped",
                json!({ "reason": "program-exited", "exitCode": exit_code }),
            );
        }
    } else if text.contains("exited normally") {
        INFERIOR_RUNNING.store(false, Ordering::SeqCst);
        broadcast_one("debug-program-exited", json!({ "exitCode": 0 }));
    }
}

/// 仅当提示符之后（除空白外）没有其它内容时，才认定为一条完整响应。
/// 这样可以避免被调试程序自身输出里恰好包含 `oicpp_gdb:` 文本造成误判。
fn split_response(accumulator: &mut String) -> Option<String> {
    let mut search_from = 0usize;
    let mut chosen: Option<usize> = None;
    while let Some(found) = accumulator[search_from..].find(GDB_PROMPT) {
        let absolute = search_from + found;
        let after = &accumulator[absolute + GDB_PROMPT.len()..];
        if after.trim().is_empty() {
            chosen = Some(absolute);
            break;
        }
        search_from = absolute + 1;
    }
    let index = chosen?;
    let response = accumulator[..index].to_string();
    let rest = accumulator[index + GDB_PROMPT.len()..].to_string();
    *accumulator = rest;
    Some(response)
}

/// 判断某一行是否属于「必须交给响应解析处理」的 GDB 控制消息
fn is_critical_control_line(line: &str) -> bool {
    if line.is_empty() {
        return false;
    }
    if line.starts_with('\u{1a}') {
        return true;
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("[Inferior ")
        || trimmed.starts_with("[New Thread")
        || trimmed.starts_with("[Switching to thread")
        || trimmed.starts_with("[Thread ")
    {
        return true;
    }
    if trimmed.starts_with("Program received signal ")
        || trimmed.starts_with("Program exited")
        || trimmed.starts_with("Program terminated with signal")
        || trimmed.starts_with("During startup program exited")
    {
        return true;
    }
    if trimmed.starts_with("Thread ") && trimmed.contains(" hit ") {
        return true;
    }
    if trimmed.starts_with("Breakpoint ")
        || trimmed.starts_with("Temporary breakpoint ")
        || trimmed.starts_with("Catchpoint ")
    {
        return true;
    }
    false
}

/// 运行态下可安全作为「程序输出」转发的子串末尾（遇到第一条控制消息即停止）
fn safe_drain_end(buffer: &str) -> usize {
    let mut position = 0usize;
    let mut safe_end = 0usize;
    while position < buffer.len() {
        let Some(relative) = buffer[position..].find('\n') else {
            break;
        };
        let newline = position + relative;
        if is_critical_control_line(&buffer[position..newline]) {
            break;
        }
        safe_end = newline + 1;
        position = newline + 1;
    }
    safe_end
}

fn start_reader(stdout: std::process::ChildStdout) {
    std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut accumulator = String::new();
        let mut buffer = [0u8; 4096];
        loop {
            let read = match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            accumulator.push_str(&String::from_utf8_lossy(&buffer[..read]));

            loop {
                let Some(response) = split_response(&mut accumulator) else {
                    break;
                };
                if !response.trim().is_empty() {
                    handle_response(&response);
                }
            }

            // 没有完整提示符：运行中的程序输出需及时转发，但不能把 GDB 控制消息
            // 当作程序输出消费掉，否则 program-exited / 断点事件会丢失。
            if !accumulator.is_empty()
                && !accumulator.contains(GDB_PROMPT)
                && INFERIOR_RUNNING.load(Ordering::SeqCst)
            {
                let safe_end = safe_drain_end(&accumulator);
                if safe_end > 0 {
                    let data = accumulator[..safe_end].to_string();
                    accumulator = accumulator[safe_end..].to_string();
                    broadcast_one("debug-terminal-output", json!({ "data": data }));
                }
            }
        }
    });
}

// ------------------------------------------------------------------ 命令收发

fn write_command(command: &str) -> Result<(), String> {
    let mut guard = GDB.lock().map_err(|e| e.to_string())?;
    let session = guard.as_mut().ok_or_else(|| "调试器未运行".to_string())?;
    session
        .stdin
        .write_all(format!("{command}\n").as_bytes())
        .map_err(|e| e.to_string())?;
    session.stdin.flush().map_err(|e| e.to_string())
}

/// 发送命令并等待一条响应（以提示符为界）
async fn send_sync(command: &str) -> Result<String, String> {
    let _guard = CMD_LOCK.lock().await;
    let before = RESPONSES.lock().map(|g| g.len()).unwrap_or(0);
    write_command(command)?;

    let deadline = Instant::now() + Duration::from_millis(SYNC_TIMEOUT_MS);
    loop {
        if let Ok(mut guard) = RESPONSES.lock() {
            if guard.len() > before {
                return Ok(guard.remove(before));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("gdb 响应超时".to_string());
        }
        let _ = tokio::time::timeout(remaining, NOTIFY.notified()).await;
    }
}

fn take_response() -> Option<String> {
    RESPONSES.lock().ok().and_then(|mut guard| {
        if guard.is_empty() {
            None
        } else {
            Some(guard.remove(0))
        }
    })
}

#[allow(dead_code)]
fn unused_response_probe() {
    let _ = take_response();
}

/// 停止后自动刷新一次变量（延迟到下一次 IPC 轮询，避免与响应解析重入）
fn refresh_variables_soon() {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        let _ = collect_variables().await;
    });
}

async fn variable_type(expression: &str) -> String {
    match send_sync(&format!("whatis {expression}")).await {
        Ok(response) => response
            .lines()
            .find(|line| line.contains("type ="))
            .and_then(|line| line.split("type =").nth(1))
            .map(|value| value.trim().to_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

async fn variable_value(expression: &str) -> Option<String> {
    let response = send_sync(&format!("output {expression}")).await.ok()?;
    let value = response.trim();
    if value.is_empty() {
        return None;
    }
    Some(value.to_string())
}

/// 采集 locals + watches 并广播 `debug-variables-updated`
async fn collect_variables() -> Result<Value, String> {
    let locals_text = send_sync("info locals").await.unwrap_or_default();
    let local = parse_locals(&locals_text);

    let watch_exprs: Vec<String> = WATCHES.lock().map(|g| g.clone()).unwrap_or_default();
    let mut watches = serde_json::Map::new();
    for expression in watch_exprs {
        let value = variable_value(&expression).await.unwrap_or_default();
        let var_type = variable_type(&expression).await;
        watches.insert(expression.clone(), variable_node(&expression, &value, &var_type));
    }

    let payload = json!({
        "local": local,
        "global": {},
        "watches": Value::Object(watches)
    });
    broadcast_one("debug-variables-updated", payload.clone());
    Ok(payload)
}

fn pending_watch_payload(message: &str) -> Value {
    let watch_exprs: Vec<String> = WATCHES.lock().map(|g| g.clone()).unwrap_or_default();
    let mut watches = serde_json::Map::new();
    for expression in watch_exprs {
        watches.insert(
            expression,
            json!({
                "type": "",
                "value": message,
                "isArray": false,
                "isContainer": false,
                "elementCount": Value::Null,
                "children": []
            }),
        );
    }
    json!({ "local": {}, "global": {}, "watches": Value::Object(watches) })
}

// ------------------------------------------------------------------ 会话启动/停止

fn resolve_executable(file_path: &str) -> String {
    let lower = file_path.to_lowercase();
    let mut base = file_path.to_string();
    for ext in [".cpp", ".cxx", ".cc", ".c"] {
        if lower.ends_with(ext) {
            base = file_path[..file_path.len() - ext.len()].to_string();
            break;
        }
    }
    if paths::is_windows() {
        return format!("{base}.exe");
    }
    if PathBuf::from(&base).exists() {
        return base;
    }
    base
}

async fn start_session(file_path: &str, breakpoints: Vec<i64>) -> Result<Value, String> {
    stop_session();

    let executable = resolve_executable(file_path);
    if !PathBuf::from(&executable).exists() {
        return Err(format!(
            "可执行文件不存在: {executable}。请先编译代码（需要包含 -g 参数）。"
        ));
    }

    let mut command = Command::new("gdb");
    command
        .arg("-fullname")
        .arg("-quiet")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // 让 MinGW 自带的 gdb 能被找到
    let compiler_path = crate::settings::compiler_path();
    if !compiler_path.trim().is_empty() {
        let compiler = PathBuf::from(&compiler_path);
        let mut entries: Vec<PathBuf> = Vec::new();
        if let Some(dir) = compiler.parent() {
            entries.push(dir.to_path_buf());
            if let Some(root) = dir.parent() {
                for sub in ["bin", "mingw64/bin", "mingw32/bin"] {
                    let candidate = root.join(sub);
                    if candidate.exists() {
                        entries.push(candidate);
                    }
                }
            }
        }
        if let Some(existing) = std::env::var_os("PATH") {
            entries.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(entries) {
            command.env("PATH", joined);
        }
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("启动 GDB 失败: {e}"))?;

    let stdin = child.stdin.take().ok_or_else(|| "无法获取 GDB 输入流".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "无法获取 GDB 输出流".to_string())?;
    let pid = child.id() as i64;

    if let Ok(mut guard) = RESPONSES.lock() {
        guard.clear();
    }
    *GDB.lock().map_err(|e| e.to_string())? = Some(GdbSession { child, stdin });
    INFERIOR_RUNNING.store(false, Ordering::SeqCst);

    start_reader(stdout);

    // 初始化
    write_command(&format!("set prompt {GDB_PROMPT}"))?;
    for init in [
        "show version",
        "set confirm off",
        "set width 0",
        "set height 0",
        "set breakpoint pending on",
        "set print asm-demangle on",
        "set unwindonsignal on",
        "set print elements 200",
        "set filename-display absolute",
        "set style enabled off",
        "set print pretty on",
        "set print array-indexes on",
    ] {
        let _ = write_command(init);
    }
    let _ = send_sync("echo ready").await;

    if let Ok(mut guard) = BREAKPOINTS.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = CURRENT_FILE.lock() {
        *guard = Some(file_path.to_string());
    }

    for line in &breakpoints {
        let _ = add_breakpoint(file_path, *line).await;
    }

    let mode = crate::settings::run_mode();
    broadcast_one(
        "debug-ready-waiting",
        json!({
            "hasBreakpoints": !breakpoints.is_empty(),
            "message": "调试器已就绪，等待执行"
        }),
    );

    Ok(json!({
        "success": true,
        "file": file_path,
        "executable": executable,
        "process": pid,
        "mode": mode
    }))
}

fn stop_session() {
    INFERIOR_RUNNING.store(false, Ordering::SeqCst);
    let session = GDB.lock().ok().and_then(|mut guard| guard.take());
    if let Some(mut session) = session {
        let _ = session.stdin.write_all(b"quit\n");
        let _ = session.stdin.flush();
        std::thread::sleep(Duration::from_millis(200));
        let _ = session.child.kill();
        let _ = session.child.wait();
    }
    if let Ok(mut guard) = RESPONSES.lock() {
        guard.clear();
    }
    if let Ok(mut guard) = CURRENT_FILE.lock() {
        *guard = None;
    }
}

async fn add_breakpoint(file: &str, line: i64) -> Result<(), String> {
    let response = send_sync(&format!("break \"{file}:{line}\"")).await?;
    let number = regex::Regex::new(r"Breakpoint (\d+)")
        .ok()
        .and_then(|re| re.captures(&response))
        .and_then(|caps| caps.get(1).and_then(|m| m.as_str().parse::<i64>().ok()))
        .unwrap_or(0);
    let key = format!("{file}:{line}");
    if let Ok(mut guard) = BREAKPOINTS.lock() {
        guard.retain(|item| item.0 != key);
        guard.push((key, file.to_string(), line, number));
    }
    Ok(())
}

async fn remove_breakpoint(file: &str, line: i64) -> Result<(), String> {
    let key = format!("{file}:{line}");
    let number = BREAKPOINTS
        .lock()
        .map(|guard| {
            guard
                .iter()
                .find(|item| item.0 == key)
                .map(|item| item.3)
                .unwrap_or(0)
        })
        .unwrap_or(0);
    if number > 0 {
        let _ = send_sync(&format!("delete breakpoints {number}")).await;
    }
    if let Ok(mut guard) = BREAKPOINTS.lock() {
        guard.retain(|item| item.0 != key);
    }
    Ok(())
}

async fn expand_variable(name: &str, options: serde_json::Map<String, Value>) -> Result<Value, String> {
    let value = variable_value(name).await.unwrap_or_default();
    let var_type = variable_type(name).await;
    let scope = options
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("local")
        .to_string();
    let path = options
        .get("path")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let cache_key = options.get("cacheKey").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "name": name,
        "scope": scope,
        "path": path,
        "cacheKey": cache_key,
        "data": variable_node(name, &value, &var_type)
    }))
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "start-debug" => {
            let file_path = ipc.string_arg(0);
            let options = ipc.object_arg(1);
            if let Ok(mut guard) = APP.lock() {
                if guard.is_none() {
                    *guard = Some(ipc.app.clone());
                }
            }
            let lines: Vec<i64> = options
                .get("breakpoints")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|item| match item {
                            Value::Number(n) => n.as_i64(),
                            Value::Object(map) => map.get("line").and_then(|v| v.as_i64()),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();

            match start_session(&file_path, lines).await {
                Ok(result) => {
                    broadcast_one("debug-started", result.clone());
                    Some(Ok(result))
                }
                Err(err) => {
                    broadcast_one("debug-error", json!(err.clone()));
                    Some(Err(err))
                }
            }
        }
        "stop-debug" => {
            stop_session();
            let _ = collect_variables().await;
            broadcast_one("debug-stopped", json!({ "success": true }));
            Some(Ok(json!({ "success": true })))
        }
        "debug-step-over" | "debug-step-into" | "debug-step-out" | "debug-continue" => {
            let (command, message) = match ipc.channel {
                "debug-step-over" => ("next", "步过执行"),
                "debug-step-into" => ("step", "步入执行"),
                "debug-step-out" => ("finish", "步出执行"),
                _ => ("continue", "继续执行"),
            };
            INFERIOR_RUNNING.store(true, Ordering::SeqCst);
            broadcast_empty_local("debug-running");
            match write_command(command) {
                Ok(_) => {
                    broadcast_one("debug-output", json!({ "message": message, "type": "debug" }));
                    Some(Ok(json!({ "success": true })))
                }
                Err(err) => {
                    broadcast_one("debug-error", json!(err.clone()));
                    Some(Err(err))
                }
            }
        }
        "debug-run" => {
            INFERIOR_RUNNING.store(true, Ordering::SeqCst);
            match write_command("run") {
                Ok(_) => {
                    broadcast_one("debug-output", json!({ "message": "程序已启动", "type": "debug" }));
                    broadcast_empty_local("debug-running");
                    Some(Ok(json!({ "success": true })))
                }
                Err(err) => {
                    broadcast_one("debug-error", json!(err.clone()));
                    Some(Err(err))
                }
            }
        }
        "debug-send-input" => {
            let input = ipc.string_arg(0).replace('\r', "\n");
            if !INFERIOR_RUNNING.load(Ordering::SeqCst) {
                broadcast_one(
                    "debug-terminal-output",
                    json!({ "data": "\r\n[调试] 当前处于暂停态，请点击继续运行后再输入\r\n" }),
                );
                return Some(Ok(Value::Null));
            }
            match write_command(&input) {
                Ok(_) => Some(Ok(Value::Null)),
                Err(err) => {
                    broadcast_one("debug-error", json!(err.clone()));
                    Some(Err(err))
                }
            }
        }
        "debug-add-breakpoint" => {
            let map = ipc.object_arg(0);
            let file = map.get("file").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let line = map.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            if file.is_empty() || line <= 0 {
                return Some(Err("断点参数不完整".to_string()));
            }
            match add_breakpoint(&file, line).await {
                Ok(_) => {
                    broadcast_one(
                        "debug-output",
                        json!({ "message": format!("断点已设置: {file}:{line}"), "type": "info" }),
                    );
                    broadcast_one(
                        "debug-breakpoint-set",
                        json!({ "file": file, "line": line, "success": true }),
                    );
                    Some(Ok(json!({ "success": true })))
                }
                Err(err) => {
                    broadcast_one("debug-error", json!(err.clone()));
                    broadcast_one(
                        "debug-breakpoint-set",
                        json!({ "file": file, "line": line, "success": false, "error": err }),
                    );
                    Some(Err(err))
                }
            }
        }
        "debug-remove-breakpoint" => {
            let map = ipc.object_arg(0);
            let file = map.get("file").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let line = map.get("line").and_then(|v| v.as_i64()).unwrap_or(0);
            match remove_breakpoint(&file, line).await {
                Ok(_) => {
                    broadcast_one(
                        "debug-output",
                        json!({ "message": format!("断点已移除: {file}:{line}"), "type": "info" }),
                    );
                    broadcast_one(
                        "debug-breakpoint-removed",
                        json!({ "file": file, "line": line, "success": true }),
                    );
                    Some(Ok(json!({ "success": true })))
                }
                Err(err) => {
                    broadcast_one("debug-error", json!(err.clone()));
                    broadcast_one(
                        "debug-breakpoint-removed",
                        json!({ "file": file, "line": line, "success": false, "error": err }),
                    );
                    Some(Err(err))
                }
            }
        }
        "debug-request-variables" => match collect_variables().await {
            Ok(payload) => Some(Ok(payload)),
            Err(err) => {
                broadcast_one("debug-error", json!(err.clone()));
                Some(Err(err))
            }
        },
        "debug-request-callstack" => {
            let response = send_sync("bt 30").await.unwrap_or_default();
            Some(Ok(Value::Array(parse_backtrace(&response))))
        }
        "debug-goto-frame" => {
            let frame = ipc.arg(0).cloned().unwrap_or(Value::Null);
            broadcast_one("goto-source-location", frame);
            Some(Ok(Value::Null))
        }
        "debug-add-watch" => {
            let expression = ipc.string_arg(0).trim().to_string();
            if expression.is_empty() {
                return Some(Err("监视表达式不能为空".to_string()));
            }
            if let Ok(mut guard) = WATCHES.lock() {
                if !guard.contains(&expression) {
                    guard.push(expression.clone());
                }
            }
            broadcast_one(
                "debug-output",
                json!({ "message": format!("已添加监视变量: {expression}"), "type": "info" }),
            );
            if !INFERIOR_RUNNING.load(Ordering::SeqCst) {
                let _ = collect_variables().await;
            } else {
                let payload = pending_watch_payload("(运行中，等待暂停)");
                broadcast_one("debug-variables-updated", payload);
            }
            Some(Ok(json!({ "success": true })))
        }
        "debug-remove-watch" => {
            let expression = ipc.string_arg(0).trim().to_string();
            if expression.is_empty() {
                return Some(Ok(Value::Null));
            }
            if let Ok(mut guard) = WATCHES.lock() {
                guard.retain(|item| item != &expression);
            }
            broadcast_one(
                "debug-output",
                json!({ "message": format!("已移除监视变量: {expression}"), "type": "info" }),
            );
            let _ = collect_variables().await;
            Some(Ok(json!({ "success": true })))
        }
        "debug-refresh-variables" => {
            if GDB.lock().map(|g| g.is_none()).unwrap_or(true) {
                return Some(Err("调试器未运行".to_string()));
            }
            match collect_variables().await {
                Ok(payload) => Some(Ok(payload)),
                Err(err) => {
                    broadcast_one("debug-error", json!(err.clone()));
                    Some(Err(err))
                }
            }
        }
        "debug-expand-variable" | "debug-load-more-variable" => {
            let name = ipc.string_arg(0);
            let mut options = ipc.object_arg(1);
            let append = ipc.channel == "debug-load-more-variable";
            if append {
                options.insert("append".to_string(), Value::Bool(true));
            }
            if GDB.lock().map(|g| g.is_none()).unwrap_or(true) {
                return Some(Err("调试器未运行".to_string()));
            }
            match expand_variable(&name, options.clone()).await {
                Ok(mut result) => {
                    let message = if append {
                        format!("已加载更多: {name}")
                    } else {
                        format!("已展开变量: {name}")
                    };
                    broadcast_one("debug-output", json!({ "message": message, "type": "info" }));
                    let _ = collect_variables().await;
                    if let Some(map) = result.as_object_mut() {
                        map.insert("options".to_string(), Value::Object(options));
                        if append {
                            map.insert("append".to_string(), Value::Bool(true));
                        }
                    }
                    broadcast_one("debug-variable-expanded", result.clone());
                    Some(Ok(result))
                }
                Err(err) => {
                    broadcast_one("debug-error", json!(err.clone()));
                    Some(Err(err))
                }
            }
        }
        "debug-collapse-variable" => {
            let name = ipc.string_arg(0);
            broadcast_one(
                "debug-output",
                json!({ "message": format!("已折叠变量: {name}"), "type": "info" }),
            );
            match collect_variables().await {
                Ok(payload) => Some(Ok(payload)),
                Err(err) => {
                    broadcast_one("debug-error", json!(err.clone()));
                    Some(Err(err))
                }
            }
        }
        "get-current-file" => {
            let value = CURRENT_FILE.lock().ok().and_then(|guard| guard.clone());
            Some(Ok(value.map(Value::String).unwrap_or(Value::Null)))
        }
        "get-breakpoints" => {
            let entries: Vec<Value> = BREAKPOINTS
                .lock()
                .map(|guard| {
                    guard
                        .iter()
                        .map(|(key, file, line, _)| {
                            json!([key, { "file": file, "line": line, "enabled": true }])
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(Ok(Value::Array(entries)))
        }
        _ => None,
    }
}

fn broadcast_empty_local(channel: &str) {
    if let Some(app) = app_handle() {
        events::broadcast_empty(&app, channel);
    }
}

/// 清理入口：应用退出时终止 gdb
pub fn shutdown() {
    stop_session();
}
