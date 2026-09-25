//! 集成终端（基于 `portable-pty`）。
//!
//! 对应 Electron 版 `terminal-manager.js`：为渲染进程提供 PTY 会话的
//! 创建 / 写入 / 缩放 / 结束 / 列表 / TTY 查询能力。终端输出通过
//! `terminal-data` 事件回推、进程退出通过 `terminal-exit` 事件回推。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use serde_json::{json, Map, Value};
use tauri::AppHandle;

use crate::events;
use crate::ipc::Ipc;
use crate::paths;

/// 单个终端会话。
///
/// 注意：`Box<dyn MasterPty + Send>` 只保证 `Send`，所以整张会话表必须放在
/// `Mutex<HashMap<..>>` 里，才能让 `static` 满足 `Sync`。
struct Session {
    id: String,
    shell: String,
    cwd: String,
    cols: u16,
    rows: u16,
    name: String,
    created_at: i64,
    pid: Option<u32>,
    /// PTY 主端，用于 `resize`
    master: Box<dyn MasterPty + Send>,
    /// PTY 写入端
    writer: Box<dyn Write + Send>,
    /// 独立的结束句柄：等待线程阻塞在 `wait()` 时它仍可终止子进程
    killer: Box<dyn ChildKiller + Send + Sync>,
}

static SESSIONS: Lazy<Mutex<HashMap<String, Session>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Linux 下识别 pts / tty 设备路径
static TTY_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^/dev/(pts/\d+|tty\d+|tty)$").expect("TTY 正则表达式应当合法")
});

// ------------------------------------------------------------------ 参数解析

fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// 读取尺寸参数：非法值回退默认值，并保证不小于 `min`
fn resolve_dim(value: Option<&Value>, default: u16, min: u16) -> u16 {
    match value.and_then(number_of) {
        Some(n) if n.is_finite() => (n.floor() as i64).clamp(min as i64, u16::MAX as i64) as u16,
        _ => default,
    }
}

/// 平台默认 shell
fn default_shell() -> String {
    if paths::is_windows() {
        std::env::var("POWERSHELL_EXE")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "powershell.exe".to_string())
    } else if paths::is_macos() {
        std::env::var("SHELL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "/bin/zsh".to_string())
    } else {
        std::env::var("SHELL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| "/bin/bash".to_string())
    }
}

/// 从可能带引号 / 带参数的值里取出可执行文件路径
fn extract_executable(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return String::new();
    }
    let first = raw.as_bytes()[0];
    if (first == b'"' || first == b'\'') && raw.len() > 1 {
        if let Some(offset) = raw[1..].find(first as char) {
            let inner = raw[1..1 + offset].trim();
            if !inner.is_empty() {
                return inner.to_string();
            }
        }
    }
    match raw.find(char::is_whitespace) {
        Some(idx) => raw[..idx].trim().to_string(),
        None => raw.to_string(),
    }
}

fn push_candidate(candidates: &mut Vec<String>, value: &str) {
    let next = extract_executable(value);
    if next.is_empty() {
        return;
    }
    if paths::is_windows() {
        if !candidates.iter().any(|c| c.eq_ignore_ascii_case(&next)) {
            candidates.push(next);
        }
    } else if !candidates.contains(&next) {
        candidates.push(next);
    }
}

/// 候选 shell 链：指定 shell → 平台默认 → 通用兜底
fn build_shell_candidates(preferred: &str) -> Vec<String> {
    let mut candidates: Vec<String> = Vec::new();
    push_candidate(&mut candidates, preferred);

    if paths::is_windows() {
        if let Ok(shell) = std::env::var("SHELL") {
            push_candidate(&mut candidates, &shell);
        }
        if let Ok(powershell) = std::env::var("POWERSHELL_EXE") {
            push_candidate(&mut candidates, &powershell);
        }
        push_candidate(&mut candidates, "powershell.exe");
        push_candidate(&mut candidates, "pwsh.exe");
        push_candidate(&mut candidates, "cmd.exe");
    } else {
        if let Ok(shell) = std::env::var("SHELL") {
            push_candidate(&mut candidates, &shell);
        }
        if paths::is_macos() {
            push_candidate(&mut candidates, "/bin/zsh");
        }
        push_candidate(&mut candidates, "/bin/bash");
        push_candidate(&mut candidates, "/bin/sh");
    }

    candidates
}

/// 非 Windows 且为绝对路径时，要求文件真实存在
fn is_usable_shell(shell: &str) -> bool {
    if shell.is_empty() {
        return false;
    }
    if paths::is_windows() {
        return true;
    }
    let path = Path::new(shell);
    if !path.is_absolute() {
        return true;
    }
    path.exists()
}

/// 默认启动参数：powershell 加 `-NoLogo`
fn default_args(shell: &str) -> Vec<String> {
    if shell.to_ascii_lowercase().contains("powershell") {
        vec!["-NoLogo".to_string()]
    } else {
        Vec::new()
    }
}

/// 补几个终端常用环境变量（其余继承父进程环境）
fn apply_env(cmd: &mut CommandBuilder) {
    cmd.env("TERM", "xterm-256color");
    if paths::is_windows() {
        if std::env::var_os("LANG").is_none() {
            cmd.env("LANG", "zh_CN.UTF-8");
        }
        if std::env::var_os("LC_ALL").is_none() {
            cmd.env("LC_ALL", "C.UTF-8");
        }
        if std::env::var_os("PYTHONIOENCODING").is_none() {
            cmd.env("PYTHONIOENCODING", "utf-8");
        }
    }
    if paths::is_macos() {
        cmd.env("TERM_PROGRAM", "oicpp");
        cmd.env("TERM_PROGRAM_VERSION", "embedded");
    }
}

fn resolve_cwd(value: Option<&Value>) -> PathBuf {
    if let Some(text) = value.and_then(|v| v.as_str()) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            let candidate = PathBuf::from(trimmed);
            if candidate.is_dir() {
                return candidate;
            }
        }
    }
    paths::home_dir()
}

fn hostname() -> Option<String> {
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        if !name.trim().is_empty() {
            return Some(name.trim().to_string());
        }
    }
    if let Ok(name) = std::env::var("HOSTNAME") {
        if !name.trim().is_empty() {
            return Some(name.trim().to_string());
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|name| !name.is_empty())
}

fn resolve_name(value: Option<&Value>) -> String {
    if let Some(text) = value.and_then(|v| v.as_str()) {
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "user".to_string());
    let host = hostname().unwrap_or_else(|| "localhost".to_string());
    format!("{user}@{host}")
}

// ------------------------------------------------------------------ 会话创建

/// 真正打开 PTY 并启动 shell；成功则把会话写入全局表并返回结果 JSON
fn spawn_session(
    handle: &AppHandle,
    shell: &str,
    args: &[String],
    cwd: &Path,
    cols: u16,
    rows: u16,
    name: &str,
) -> Result<Value, String> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("打开 PTY 失败: {err}"))?;

    let mut cmd = CommandBuilder::new(shell);
    cmd.args(args.iter());
    cmd.cwd(cwd);
    apply_env(&mut cmd);

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|err| format!("启动 shell 失败: {err}"))?;

    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|err| format!("读取 PTY 失败: {err}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|err| format!("写入 PTY 失败: {err}"))?;
    let master = pair.master;

    let pid = child.process_id();
    // killer 与 Child 分离，便于在等待线程阻塞于 wait() 时结束进程
    let killer = child.clone_killer();

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = paths::now_millis();
    let cwd_text = cwd.to_string_lossy().to_string();

    // 交给等待线程的共享槽位
    let child_slot: Arc<Mutex<Option<Box<dyn Child + Send + Sync>>>> =
        Arc::new(Mutex::new(Some(child)));

    // 先入表再起线程，避免子进程瞬间退出时等待线程与主表竞争
    SESSIONS.lock().unwrap().insert(
        id.clone(),
        Session {
            id: id.clone(),
            shell: shell.to_string(),
            cwd: cwd_text.clone(),
            cols,
            rows,
            name: name.to_string(),
            created_at,
            pid,
            master,
            writer,
            killer,
        },
    );

    // 读线程：把 PTY 输出逐段广播为 terminal-data
    {
        let reader_handle = handle.clone();
        let reader_id = id.clone();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let text = String::from_utf8_lossy(&buf[..n]).to_string();
                        events::broadcast(
                            &reader_handle,
                            "terminal-data",
                            vec![json!({ "terminalId": reader_id, "data": text })],
                        );
                    }
                    Err(_) => break,
                }
            }
        });
    }

    // 等待线程：取出子进程并阻塞 wait()，结束后广播 terminal-exit 并移除会话
    {
        let waiter_handle = handle.clone();
        let waiter_id = id.clone();
        std::thread::spawn(move || {
            let child = child_slot.lock().unwrap().take();
            let exit_code = match child {
                Some(mut child) => match child.wait() {
                    Ok(status) => Value::from(status.exit_code()),
                    Err(_) => Value::Null,
                },
                None => Value::Null,
            };
            SESSIONS.lock().unwrap().remove(&waiter_id);
            events::broadcast(
                &waiter_handle,
                "terminal-exit",
                vec![json!({
                    "terminalId": waiter_id,
                    "exitCode": exit_code,
                    "signal": Value::Null
                })],
            );
        });
    }

    Ok(json!({
        "ok": true,
        "terminalId": id,
        "shell": shell,
        "cwd": cwd_text,
        "name": name,
        "cols": cols,
        "rows": rows,
        "pid": pid.map(Value::from).unwrap_or(Value::Null),
        "backend": "pty"
    }))
}

fn create_session(handle: &AppHandle, options: &Map<String, Value>) -> Value {
    let requested_shell = options
        .get("shell")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(default_shell);

    let requested_args: Option<Vec<String>> = options
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .filter(|args| !args.is_empty());

    let cols = resolve_dim(options.get("cols"), 120, 40);
    let rows = resolve_dim(options.get("rows"), 30, 8);
    let cwd = resolve_cwd(options.get("cwd"));
    let name = resolve_name(options.get("name"));

    let mut candidates = build_shell_candidates(&requested_shell);
    candidates.retain(|candidate| is_usable_shell(candidate));
    if candidates.is_empty() {
        return json!({ "ok": false, "error": "未找到可用的 shell 候选项" });
    }

    let mut last_error = String::new();
    for shell in &candidates {
        let args = requested_args
            .clone()
            .unwrap_or_else(|| default_args(shell));
        match spawn_session(handle, shell, &args, &cwd, cols, rows, &name) {
            Ok(result) => return result,
            Err(err) => last_error = format!("{shell} -> {err}"),
        }
    }

    json!({ "ok": false, "error": format!("启动 shell 失败: {last_error}") })
}

// ------------------------------------------------------------------ 会话操作

fn write_session(id: &str, data: &str) -> bool {
    let mut sessions = SESSIONS.lock().unwrap();
    let Some(session) = sessions.get_mut(id) else {
        return false;
    };
    if session.writer.write_all(data.as_bytes()).is_err() {
        return false;
    }
    session.writer.flush().is_ok()
}

fn resize_session(id: &str, cols: Option<f64>, rows: Option<f64>) -> bool {
    let mut sessions = SESSIONS.lock().unwrap();
    let Some(session) = sessions.get_mut(id) else {
        return false;
    };
    let next_cols = match cols.filter(|n| n.is_finite()) {
        Some(n) => (n.floor() as i64).clamp(20, u16::MAX as i64) as u16,
        None => session.cols,
    };
    let next_rows = match rows.filter(|n| n.is_finite()) {
        Some(n) => (n.floor() as i64).clamp(6, u16::MAX as i64) as u16,
        None => session.rows,
    };
    session.cols = next_cols;
    session.rows = next_rows;
    session
        .master
        .resize(PtySize {
            rows: next_rows,
            cols: next_cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .is_ok()
}

fn kill_session(id: &str) -> bool {
    let session = SESSIONS.lock().unwrap().remove(id);
    let Some(mut session) = session else {
        return false;
    };
    let _ = session.killer.kill();
    true
}

fn list_sessions() -> Vec<Value> {
    let sessions = SESSIONS.lock().unwrap();
    sessions
        .values()
        .map(|session| {
            json!({
                "terminalId": session.id,
                "shell": session.shell,
                "cwd": session.cwd,
                "cols": session.cols,
                "rows": session.rows,
                "pid": session.pid.map(Value::from).unwrap_or(Value::Null),
                "backend": "pty",
                "name": session.name,
                "createdAt": session.created_at
            })
        })
        .collect()
}

/// 由 pid 反查 TTY（仅 macOS / Linux）
fn resolve_tty(pid: u32) -> Result<Option<String>, String> {
    if paths::is_macos() {
        let output = std::process::Command::new("ps")
            .arg("-p")
            .arg(pid.to_string())
            .arg("-o")
            .arg("tty=")
            .output()
            .map_err(|err| err.to_string())?;
        let text = String::from_utf8_lossy(&output.stdout);
        for line in text.split('\n') {
            let tty = line.trim();
            if !tty.is_empty() && tty != "?" && tty != "-" {
                let path = if tty.starts_with("/dev/") {
                    tty.to_string()
                } else {
                    format!("/dev/{tty}")
                };
                return Ok(Some(path));
            }
        }
        return Ok(None);
    }

    if paths::is_linux() {
        let fd_dir = format!("/proc/{pid}/fd");
        let mut fds: Vec<u32> = vec![0, 1, 2];
        if let Ok(entries) = std::fs::read_dir(&fd_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(fd) = name.parse::<u32>() {
                        if !fds.contains(&fd) {
                            fds.push(fd);
                        }
                    }
                }
            }
        }
        fds.sort_unstable();
        for fd in fds {
            let link = format!("/proc/{pid}/fd/{fd}");
            if let Ok(target) = std::fs::read_link(&link) {
                let path = target.to_string_lossy().to_string();
                if TTY_RE.is_match(&path) {
                    return Ok(Some(path));
                }
            }
        }
        return Ok(None);
    }

    Ok(None)
}

fn get_tty(id: &str) -> Value {
    if paths::is_windows() {
        return json!({ "ok": false, "tty": Value::Null });
    }

    let pid = {
        let sessions = SESSIONS.lock().unwrap();
        sessions.get(id).and_then(|session| session.pid)
    };
    let Some(pid) = pid else {
        return json!({ "ok": false, "tty": Value::Null });
    };

    match resolve_tty(pid) {
        Ok(Some(tty)) => json!({ "ok": true, "tty": tty }),
        Ok(None) => json!({ "ok": false, "tty": Value::Null }),
        Err(err) => json!({ "ok": false, "tty": Value::Null, "error": err }),
    }
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "terminal-feature-status" => Some(Ok(json!({
            "available": true,
            "reason": "",
            "detail": ""
        }))),
        "terminal-create" => {
            let options = ipc.object_arg(0);
            Some(Ok(create_session(ipc.app, &options)))
        }
        "terminal-write" => {
            let id = ipc.string_arg(0);
            let data = ipc.string_arg(1);
            Some(Ok(json!({ "ok": write_session(&id, &data) })))
        }
        "terminal-resize" => {
            let id = ipc.string_arg(0);
            let cols = ipc.number_arg(1);
            let rows = ipc.number_arg(2);
            Some(Ok(json!({ "ok": resize_session(&id, cols, rows) })))
        }
        "terminal-kill" => {
            let id = ipc.string_arg(0);
            Some(Ok(json!({ "ok": kill_session(&id) })))
        }
        "terminal-list" => Some(Ok(Value::Array(list_sessions()))),
        "terminal-get-tty" => {
            let id = ipc.string_arg(0);
            Some(Ok(get_tty(&id)))
        }
        _ => None,
    }
}
