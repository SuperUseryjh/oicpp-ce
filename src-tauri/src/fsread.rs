//! 文件读取、目录扫描、压缩包读取与文件监听（对应 Electron 版 main.js 的读侧实现）。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde_json::{json, Value};

use crate::codec;
use crate::events;
use crate::ipc::Ipc;
use crate::paths;

/// 单文件扩展名白名单
const READABLE_EXTS: [&str; 14] = [
    ".cpp", ".c", ".h", ".hpp", ".cc", ".cxx", ".py", ".txt", ".md", ".json", ".in", ".out", ".ans",
    ".pdf",
];

/// 压缩包内可读的扩展名
const ZIP_TEXT_EXTS: [&str; 6] = [".in", ".out", ".ans", ".txt", ".input", ".output"];

const MAX_ACTIVE_FILE_WATCHERS: usize = 512;
const FILE_WATCH_DEBOUNCE_MS: i64 = 120;

struct WatcherEntry {
    /// 持有 watcher 以保持监听（字段本身不被读取）
    _watcher: notify::RecommendedWatcher,
    fingerprint: String,
    last_event_ms: i64,
}

static WATCHERS: Lazy<Mutex<HashMap<String, WatcherEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

// ------------------------------------------------------------------ 工具

fn is_dir_like(path: &Path) -> bool {
    path.is_dir()
}

fn has_readable_ext(name: &str) -> bool {
    let ext = paths::extname(name);
    if ext.is_empty() {
        return true;
    }
    READABLE_EXTS.contains(&ext.as_str())
}

fn should_skip_name(name: &str) -> bool {
    name.starts_with('.') || name.to_lowercase().ends_with(".dsym")
}

fn read_dir_entries(dir: &Path) -> Result<Vec<Value>, String> {
    let entries = std::fs::read_dir(dir).map_err(|e| e.to_string())?;
    let mut folders: Vec<Value> = Vec::new();
    let mut files: Vec<Value> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if should_skip_name(&name) {
            continue;
        }
        let path = entry.path();
        let path_str = path.to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            folders.push(json!({
                "name": name,
                "type": "folder",
                "path": path_str,
                "children": []
            }));
        } else {
            if !has_readable_ext(&name) {
                continue;
            }
            files.push(json!({
                "name": name,
                "type": "file",
                "path": path_str,
                "extension": paths::extname(&name)
            }));
        }
    }

    let by_name = |a: &Value, b: &Value| {
        let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let bn = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        an.to_lowercase().cmp(&bn.to_lowercase())
    };
    folders.sort_by(by_name);
    files.sort_by(by_name);

    folders.extend(files);
    Ok(folders)
}

fn collect_files(
    dir: &Path,
    include_exts: &[String],
    exclude_globs: &[String],
    max_files: usize,
    out: &mut Vec<Value>,
) {
    if out.len() >= max_files {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= max_files {
            return;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name.to_lowercase().ends_with(".dsym") {
            continue;
        }
        let path = entry.path();
        let path_str = path.to_string_lossy().to_string();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let lower = name.to_lowercase();
            if exclude_globs.iter().any(|glob| lower.contains(&glob.to_lowercase())) {
                continue;
            }
            collect_files(&path, include_exts, exclude_globs, max_files, out);
        } else {
            let ext = paths::extname(&name);
            if !ext.is_empty() && !include_exts.iter().any(|item| item == &ext) {
                continue;
            }
            out.push(json!({ "name": name, "path": path_str, "ext": ext }));
            if out.len() >= max_files {
                return;
            }
        }
    }
}

// ------------------------------------------------------------------ 文件监听

fn watch_key(path: &Path) -> String {
    let raw = path.to_string_lossy().to_string();
    if cfg!(windows) {
        raw.to_lowercase()
    } else {
        raw
    }
}

fn file_fingerprint(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => codec::fingerprint(&String::from_utf8_lossy(&bytes)),
        Err(_) => String::new(),
    }
}

fn mtime_ms(path: &Path) -> Value {
    std::fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| json!(duration.as_millis() as i64))
        .unwrap_or(Value::Null)
}

fn start_watch(app: tauri::AppHandle, key: String, path: PathBuf) -> Result<(), String> {
    let callback_key = key.clone();
    let callback_path = path.clone();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Ok(event) = result else {
            return;
        };
        let now = paths::now_millis();
        let exists = callback_path.exists();
        let (change_type, event_type) = if !exists {
            ("deleted", "change")
        } else {
            match event.kind {
                notify::EventKind::Remove(_) => ("renamed", "rename"),
                notify::EventKind::Create(_) => ("modified", "change"),
                _ => ("modified", "change"),
            }
        };
        let fingerprint = file_fingerprint(&callback_path);

        {
            let Ok(mut guard) = WATCHERS.lock() else {
                return;
            };
            let Some(entry) = guard.get_mut(&callback_key) else {
                return;
            };
            if now - entry.last_event_ms < FILE_WATCH_DEBOUNCE_MS {
                return;
            }
            entry.last_event_ms = now;
            if !fingerprint.is_empty() && fingerprint == entry.fingerprint {
                return;
            }
            entry.fingerprint = fingerprint;
        }

        events::broadcast_one(
            &app,
            "external-file-changed",
            json!({
                "filePath": callback_path.to_string_lossy(),
                "changeType": change_type,
                "eventType": event_type,
                "exists": exists,
                "mtimeMs": mtime_ms(&callback_path),
                "timestamp": now
            }),
        );
    })
    .map_err(|e| e.to_string())?;

    // 先注册监听、再登记到表里（避免回调在持锁期间触发造成死锁）
    notify::Watcher::watch(&mut watcher, &path, notify::RecursiveMode::NonRecursive)
        .map_err(|e| e.to_string())?;

    let mut guard = WATCHERS.lock().map_err(|e| e.to_string())?;
    guard.insert(
        key,
        WatcherEntry {
            _watcher: watcher,
            fingerprint: file_fingerprint(&path),
            last_event_ms: 0,
        },
    );
    Ok(())
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "read-directory" => {
            let dir_path = ipc.string_arg(0);
            let result = read_dir_entries(Path::new(&dir_path));
            if ipc.is_invoke() {
                return match result {
                    Ok(items) => ipc.ok(Value::Array(items)),
                    Err(err) => ipc.fail(err),
                };
            }
            match result {
                Ok(items) => {
                    ipc.emit("directory-read", vec![json!(dir_path), Value::Array(items)]);
                }
                Err(err) => {
                    ipc.emit("directory-read-error", vec![json!(dir_path), json!(err)]);
                }
            }
            Some(Ok(Value::Null))
        }
        "read-file-content" => {
            let raw = ipc.string_arg(0);
            let path = paths::normalize_dropped_path(&raw);
            let target = Path::new(&path);
            if !target.exists() {
                return ipc.fail("文件不存在");
            }
            let bytes = match std::fs::read(target) {
                Ok(bytes) => bytes,
                Err(err) => return ipc.fail(err.to_string()),
            };
            if codec::is_binary(&bytes) {
                return ipc.fail("不支持的二进制文件");
            }
            ipc.ok(codec::decode_auto(&bytes))
        }
        "read-file-buffer" => {
            let raw = ipc.string_arg(0);
            if raw.trim().is_empty() {
                return ipc.fail("缺少文件路径");
            }
            let path = paths::normalize_dropped_path(&raw);
            let target = Path::new(&path);
            if !target.exists() {
                return ipc.fail("文件不存在");
            }
            match std::fs::read(target) {
                Ok(bytes) => ipc.ok(codec::b64_encode(&bytes)),
                Err(err) => ipc.fail(err.to_string()),
            }
        }
        "read-zip-text-files" => {
            let zip_path = ipc.string_arg(0);
            if zip_path.trim().is_empty() {
                return Some(Ok(json!({ "success": false, "error": "无效的压缩包路径" })));
            }
            if !Path::new(&zip_path).exists() {
                return Some(Ok(json!({ "success": false, "error": "压缩包不存在" })));
            }
            match read_zip_text_files(&zip_path) {
                Ok(files) => Some(Ok(json!({ "success": true, "files": files }))),
                Err(err) => Some(Ok(json!({ "success": false, "error": err }))),
            }
        }
        "walk-directory" => {
            let dir_path = ipc.string_arg(0);
            let options = ipc.object_arg(1);
            let include_exts: Vec<String> = options
                .get("includeExts")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| {
                    [
                        ".cpp", ".c", ".h", ".hpp", ".cc", ".cxx", ".txt", ".md", ".json", ".in",
                        ".out", ".ans", ".py",
                    ]
                    .iter()
                    .map(|s| s.to_string())
                    .collect()
                });
            let exclude_globs: Vec<String> = options
                .get("excludeGlobs")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| {
                    ["node_modules", ".git", ".oicpp", ".vscode", ".dsym"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                });
            let max_files = options
                .get("maxFiles")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(5000);

            let root = Path::new(&dir_path);
            if !is_dir_like(root) {
                return Some(Ok(json!({ "success": false, "error": "目录不存在" })));
            }
            let mut out: Vec<Value> = Vec::new();
            collect_files(root, &include_exts, &exclude_globs, max_files, &mut out);
            Some(Ok(json!({ "success": true, "files": out })))
        }
        "watch-file" => {
            let raw = ipc.string_arg(0);
            let normalized = paths::normalize_dropped_path(&raw);
            if normalized.trim().is_empty() {
                return Some(Ok(json!({ "success": false, "error": "invalid-path" })));
            }
            let resolved = std::fs::canonicalize(&normalized)
                .unwrap_or_else(|_| PathBuf::from(&normalized));
            if !resolved.exists() {
                return Some(Ok(json!({ "success": false, "error": "not-found" })));
            }
            let key = watch_key(&resolved);
            {
                let Ok(guard) = WATCHERS.lock() else {
                    return Some(Ok(json!({ "success": false, "error": "watcher-unavailable" })));
                };
                if guard.contains_key(&key) {
                    return Some(Ok(json!({ "success": true, "reason": "stopped" })));
                }
                if guard.len() >= MAX_ACTIVE_FILE_WATCHERS {
                    return Some(Ok(json!({ "success": true, "reason": "limit-reached" })));
                }
            }
            match start_watch(ipc.app.clone(), key, resolved) {
                Ok(_) => Some(Ok(json!({ "success": true, "reason": "stopped" }))),
                Err(err) => Some(Ok(json!({ "success": false, "error": err }))),
            }
        }
        "unwatch-file" => {
            let raw = ipc.string_arg(0);
            let normalized = paths::normalize_dropped_path(&raw);
            if normalized.trim().is_empty() {
                return Some(Ok(json!({ "success": false, "error": "invalid-path" })));
            }
            let resolved = std::fs::canonicalize(&normalized)
                .unwrap_or_else(|_| PathBuf::from(&normalized));
            let key = watch_key(&resolved);
            if let Ok(mut guard) = WATCHERS.lock() {
                guard.remove(&key);
            }
            Some(Ok(json!({ "success": true })))
        }
        _ => None,
    }
}

fn read_zip_text_files(zip_path: &str) -> Result<Vec<Value>, String> {
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut out: Vec<Value> = Vec::new();

    for index in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(index) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string().replace('\\', "/");
        if name.contains("__MACOSX/") {
            continue;
        }
        let base = paths::basename(&name);
        if base.starts_with('.') {
            continue;
        }
        let ext = paths::extname(&base);
        if !ZIP_TEXT_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let mut buffer = Vec::new();
        if entry.read_to_end(&mut buffer).is_err() {
            // 不支持的压缩方法等：跳过该条目
            continue;
        }
        let content = codec::decode_auto(&buffer);
        let size_bytes = entry.size();
        out.push(json!({
            "path": name,
            "content": content,
            "sizeBytes": size_bytes
        }));
    }

    Ok(out)
}
