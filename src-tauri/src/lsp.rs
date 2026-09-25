//! clangd 语言服务器（JSON-RPC over stdio）。
//!
//! 对应 Electron 版 `main.js` 中的 `ClangdLspManager`：负责定位并启动 clangd、
//! 维护请求 / 响应映射（`lsp-request`）、发送通知（`lsp-notify`），以及把
//! 服务端主动发来的通知广播为 `lsp-notification` 事件。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use tauri::AppHandle;
use tokio::sync::oneshot;

use crate::events;
use crate::inject;
use crate::ipc::Ipc;
use crate::paths;

/// 单个 clangd 进程及其运行期状态
struct LspProcess {
    /// 子进程句柄：等待线程与 stop 都可能先一步回收它
    child: Arc<Mutex<Option<Child>>>,
    /// clangd stdin
    stdin: Box<dyn Write + Send>,
    /// 请求 id 自增器
    next_id: Arc<AtomicU64>,
    /// 未完成的请求（id -> 结果通道）
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    /// 世代号：每次 stop 递增
    generation: Arc<AtomicU64>,
}

static LSP: Lazy<Mutex<Option<LspProcess>>> = Lazy::new(|| Mutex::new(None));

/// 单次请求超时时间
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------ clangd 定位

/// 在目录中递归查找 clangd 可执行文件（优先 `bin/` 下）
fn find_clangd(root: &Path) -> Option<PathBuf> {
    for name in ["clangd", "clangd.exe"] {
        let direct = root.join("bin").join(name);
        if direct.is_file() {
            return Some(direct);
        }
    }

    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    let mut fallback: Option<PathBuf> = None;
    while let Some((dir, depth)) = stack.pop() {
        if depth > 6 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push((path, depth + 1));
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let lower = file_name.to_ascii_lowercase();
            if (lower == "clangd" || lower == "clangd.exe") && fallback.is_none() {
                fallback = Some(path);
            }
        }
    }
    fallback
}

/// 递归复制目录内容（保留文件权限）
fn copy_dir_contents(src: &Path, dst: &Path) {
    if std::fs::create_dir_all(dst).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(src) else {
        return;
    };
    for entry in entries.flatten() {
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            copy_dir_contents(&from, &to);
        } else if file_type.is_file() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

/// 在 PATH 中查找可执行文件
fn find_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !name.to_ascii_lowercase().ends_with(".exe") {
            let with_exe = dir.join(format!("{name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

/// 解析 clangd 可执行文件：用户目录 → 打包目录（复制到用户目录）→ PATH
fn ensure_clangd(app: &AppHandle) -> Option<PathBuf> {
    let user_root = paths::clangd_user_root();
    let direct = user_root.join("bin").join(paths::clangd_executable_name());
    if direct.is_file() {
        return Some(direct);
    }
    if let Some(found) = find_clangd(&user_root) {
        return Some(found);
    }

    if let Some(bundled) = inject::bundled_clangd_root(app) {
        if let Some(found) = find_clangd(&bundled) {
            copy_dir_contents(&bundled, &user_root);
            if direct.is_file() {
                return Some(direct);
            }
            if let Some(copied) = find_clangd(&user_root) {
                return Some(copied);
            }
            return Some(found);
        }
    }

    // 兜底：Tauri 打包资源时目录映射到哪一层难以预判（`resources/*` 这类 glob 对
    // 目录是否递归、是否保留 `resources/` 前缀随版本而异），所以直接在整个资源目录里
    // 递归找 clangd 可执行文件。找到后就地运行、不复制 —— 只复制 bin/ 会丢掉同级的
    // lib/clang 资源目录，clangd 会找不到内置头文件。
    let resource_root = paths::resource_dir(app);
    if let Some(found) = find_clangd(&resource_root) {
        crate::misc::log_line(
            "info",
            &format!("[LSP] 在资源目录中直接找到 clangd: {}", found.display()),
        );
        return Some(found);
    }

    let from_path = find_in_path(paths::clangd_executable_name());
    if from_path.is_none() {
        // 定位失败是「编辑器空白」的最常见原因（渲染层把 LSP 启动失败当致命错误），
        // 所以把查过的位置写进日志，便于区分「没打包进来」和「打包了但路径不对」
        crate::misc::log_line(
            "warn",
            &format!(
                "[LSP] 未找到 clangd。用户目录 {}（存在 {}）/ 打包目录 {}（存在 {}）/ 资源目录 {}（存在 {}）/ PATH 中的 {}",
                user_root.display(),
                user_root.is_dir(),
                resource_root.join("clangd").display(),
                resource_root.join("clangd").is_dir(),
                resource_root.display(),
                resource_root.is_dir(),
                paths::clangd_executable_name()
            ),
        );
    }
    from_path
}

// ------------------------------------------------------------------ 帧读写

/// 按 `Content-Length` 帧格式写入一条 JSON-RPC 消息
fn write_json<W: Write + ?Sized>(writer: &mut W, payload: &Value) -> std::io::Result<()> {
    let body = payload.to_string();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()
}

/// 处理一条来自 clangd 的消息：响应回填 pending，通知广播事件
fn dispatch_message(
    handle: &AppHandle,
    message: &Value,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
) {
    if let Some(id) = message.get("id").and_then(|v| v.as_u64()) {
        let sender = pending.lock().unwrap().remove(&id);
        if let Some(sender) = sender {
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| error.to_string());
                let _ = sender.send(Err(text));
            } else {
                let result = message.get("result").cloned().unwrap_or(Value::Null);
                let _ = sender.send(Ok(result));
            }
        }
        return;
    }

    if let Some(method) = message.get("method").and_then(|v| v.as_str()) {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        events::broadcast_one(
            handle,
            "lsp-notification",
            json!({ "method": method, "params": params }),
        );
    }
}

/// stdout 读取循环：解析帧、派发消息；退出时拒绝所有挂起请求并回收子进程
fn read_loop(
    handle: &AppHandle,
    stdout: std::process::ChildStdout,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>,
    child_slot: &Arc<Mutex<Option<Child>>>,
) {
    let mut reader = BufReader::new(stdout);

    'outer: loop {
        // 读取头：直到空行，取出 Content-Length
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break 'outer,
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    let lower = trimmed.to_ascii_lowercase();
                    if let Some(rest) = lower.strip_prefix("content-length:") {
                        content_length = rest.trim().parse::<usize>().ok();
                    }
                }
                Err(_) => break 'outer,
            }
        }

        let Some(length) = content_length else {
            continue;
        };
        if length == 0 {
            continue;
        }
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            break;
        }
        let Ok(text) = String::from_utf8(body) else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        dispatch_message(handle, &message, pending);
    }

    // clangd 已退出：拒绝所有挂起请求，避免调用方一直等待
    {
        let mut map = pending.lock().unwrap();
        for (_, sender) in map.drain() {
            let _ = sender.send(Err("clangd exited".to_string()));
        }
    }
    let child = child_slot.lock().unwrap().take();
    if let Some(mut child) = child {
        let _ = child.wait();
    }
}

// ------------------------------------------------------------------ 启停

fn start(app: &AppHandle, options: &Map<String, Value>) -> Value {
    {
        let guard = LSP.lock().unwrap();
        if guard.is_some() {
            return json!({ "ok": true, "alreadyRunning": true });
        }
    }

    let Some(clangd_path) = ensure_clangd(app) else {
        return json!({ "ok": false, "error": "clangd missing" });
    };

    let mut args: Vec<String> = vec![
        "--offset-encoding=utf-16".to_string(),
        "--background-index".to_string(),
        "--completion-style=detailed".to_string(),
        "--header-insertion=iwyu".to_string(),
        "--pch-storage=memory".to_string(),
    ];

    let compiler_path = options
        .get("compilerPath")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if !compiler_path.is_empty() && Path::new(&compiler_path).exists() {
        let normalized = compiler_path.replace('\\', "/");
        let dir = normalized
            .rsplit_once('/')
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_default();
        let base = paths::basename(&normalized);
        let stem = match base.rfind('.') {
            Some(idx) => base[..idx].to_string(),
            None => base.clone(),
        };
        if dir.is_empty() {
            args.push(format!("--query-driver={stem}*"));
        } else {
            args.push(format!("--query-driver={dir}/{stem}*"));
        }
    }

    let fallback_flags: Vec<String> = options
        .get("fallbackFlags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if let Some(extra) = options.get("clangdArgs").and_then(|v| v.as_array()) {
        for item in extra {
            if let Some(text) = item.as_str() {
                if !text.is_empty() {
                    args.push(text.to_string());
                }
            }
        }
    }

    let mut child = match Command::new(&clangd_path)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return json!({ "ok": false, "error": format!("启动 clangd 失败: {err}") });
        }
    };

    let Some(stdin) = child.stdin.take() else {
        let _ = child.kill();
        return json!({ "ok": false, "error": "clangd stdin 不可用" });
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return json!({ "ok": false, "error": "clangd stdout 不可用" });
    };
    let stderr = child.stderr.take();

    let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU64::new(1));
    let generation = Arc::new(AtomicU64::new(1));
    let child_slot: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(Some(child)));

    // stdout 帧解析线程
    {
        let handle = app.clone();
        let pending = pending.clone();
        let child_slot = child_slot.clone();
        std::thread::spawn(move || read_loop(&handle, stdout, &pending, &child_slot));
    }

    // stderr 排空线程（避免管道写满阻塞 clangd）
    if let Some(stderr) = stderr {
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for _line in reader.lines() {}
        });
    }

    *LSP.lock().unwrap() = Some(LspProcess {
        child: child_slot,
        stdin: Box::new(stdin),
        next_id,
        pending,
        generation,
    });

    json!({
        "ok": true,
        "clangdPath": clangd_path.to_string_lossy().to_string(),
        "args": args,
        "fallbackFlags": fallback_flags
    })
}

fn stop() {
    let taken = LSP.lock().unwrap().take();
    let Some(process) = taken else {
        return;
    };
    process.generation.fetch_add(1, Ordering::SeqCst);

    // 拒绝所有挂起请求
    {
        let mut pending = process.pending.lock().unwrap();
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err("clangd stopped".to_string()));
        }
    }

    // 结束并回收子进程
    let child = process.child.lock().unwrap().take();
    if let Some(mut child) = child {
        let _ = child.kill();
        let _ = child.wait();
    }
}

// ------------------------------------------------------------------ 请求 / 通知

async fn request(method: String, params: Value) -> Option<Result<Value, String>> {
    // 先在锁内取数据、写入 stdin、注册回调，然后立刻释放锁再 await
    let (receiver, id, pending) = {
        let mut guard = LSP.lock().unwrap();
        let Some(process) = guard.as_mut() else {
            return Some(Err("clangd not running".to_string()));
        };
        let id = process.next_id.fetch_add(1, Ordering::SeqCst);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        if let Err(err) = write_json(&mut *process.stdin, &payload) {
            return Some(Err(format!("发送 LSP 请求失败: {err}")));
        }
        let (sender, receiver) = oneshot::channel();
        process.pending.lock().unwrap().insert(id, sender);
        (receiver, id, process.pending.clone())
    };

    match tokio::time::timeout(REQUEST_TIMEOUT, receiver).await {
        Ok(Ok(Ok(result))) => Some(Ok(result)),
        Ok(Ok(Err(message))) => Some(Err(message)),
        Ok(Err(_)) => Some(Err("clangd 请求已取消".to_string())),
        Err(_) => {
            pending.lock().unwrap().remove(&id);
            Some(Err("clangd 请求超时".to_string()))
        }
    }
}

fn notify(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    let method = ipc.string_arg(0);
    let params = ipc
        .arg(1)
        .cloned()
        .filter(|v| !v.is_null())
        .unwrap_or_else(|| json!({}));

    let mut guard = LSP.lock().unwrap();
    let Some(process) = guard.as_mut() else {
        return Some(Ok(json!({ "ok": false, "error": "clangd not running" })));
    };
    let payload = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params
    });
    match write_json(&mut *process.stdin, &payload) {
        Ok(()) => Some(Ok(json!({ "ok": true }))),
        Err(err) => Some(Ok(json!({ "ok": false, "error": err.to_string() }))),
    }
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "lsp-start" => {
            let options = ipc.object_arg(0);
            Some(Ok(start(ipc.app, &options)))
        }
        "lsp-stop" => {
            stop();
            Some(Ok(json!({ "ok": true })))
        }
        "lsp-restart" => {
            let options = ipc.object_arg(0);
            stop();
            Some(Ok(start(ipc.app, &options)))
        }
        "lsp-request" => {
            let method = ipc.string_arg(0);
            let params = ipc
                .arg(1)
                .cloned()
                .filter(|v| !v.is_null())
                .unwrap_or_else(|| json!({}));
            request(method, params).await
        }
        "lsp-notify" => notify(ipc),
        _ => None,
    }
}
