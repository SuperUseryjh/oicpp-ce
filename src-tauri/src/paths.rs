//! 路径与平台相关工具（对应 Electron 版本 main.js 中的 os/path 逻辑）

use std::path::{Path, PathBuf};

use serde_json::Value;
use tauri::{AppHandle, Manager};

/// `~`
pub fn home_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home);
        }
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        if !profile.is_empty() {
            return PathBuf::from(profile);
        }
    }
    if cfg!(windows) {
        if let (Ok(drive), Ok(path)) = (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH")) {
            return PathBuf::from(format!("{drive}{path}"));
        }
    }
    PathBuf::from(".")
}

/// `~/.oicpp`（确保存在）
pub fn oicpp_dir() -> PathBuf {
    let dir = home_dir().join(".oicpp");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn ensure(dir: PathBuf) -> PathBuf {
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn code_temp_dir() -> PathBuf {
    ensure(oicpp_dir().join("codeTemp"))
}

pub fn compilers_dir() -> PathBuf {
    ensure(oicpp_dir().join("Compilers"))
}

pub fn testlibs_dir() -> PathBuf {
    ensure(oicpp_dir().join("Testlibs"))
}

pub fn settings_path() -> PathBuf {
    oicpp_dir().join("settings.json")
}

pub fn logs_dir() -> PathBuf {
    ensure(oicpp_dir().join("logs"))
}

pub fn console_pauser_path() -> PathBuf {
    oicpp_dir().join("consolepauser.exe")
}

/// 用户自定义 clangd 目录：`~/.oicpp/LSP`
pub fn clangd_user_root() -> PathBuf {
    oicpp_dir().join("LSP")
}

pub fn clangd_executable_name() -> &'static str {
    if cfg!(windows) {
        "clangd.exe"
    } else {
        "clangd"
    }
}

pub fn resource_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .resource_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// `win32` / `darwin` / `linux`
pub fn platform_key() -> &'static str {
    if cfg!(windows) {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

/// `windows` / `macos` / `linux`（与 Electron 进程 get-platform 返回一致）
pub fn platform_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

pub fn is_windows() -> bool {
    cfg!(windows)
}

pub fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

pub fn is_linux() -> bool {
    cfg!(target_os = "linux")
}

/// 可执行文件后缀
pub fn exe_suffix() -> &'static str {
    if cfg!(windows) {
        ".exe"
    } else {
        ""
    }
}

pub fn basename(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

pub fn dirname(path: &str) -> String {
    Path::new(path)
        .parent()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 与 path.extname 一致（小写、含点、无扩展名返回空串）
pub fn extname(path: &str) -> String {
    Path::new(path)
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy().to_lowercase()))
        .unwrap_or_default()
}

pub fn basename_without_ext(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 归一化拖拽 / 粘贴进来的路径（对应 main.js 的 normalizeDroppedPath）
pub fn normalize_dropped_path(input: &str) -> String {
    let mut value = input.trim().to_string();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
        {
            value = value[1..value.len() - 1].to_string();
        }
    }
    if let Some(rest) = value.strip_prefix("file://") {
        value = rest.to_string();
        if cfg!(windows) && value.len() > 2 {
            let bytes = value.as_bytes();
            if bytes[0] == b'/' && bytes[2] == b':' {
                value = value[1..].to_string();
            }
        }
        value = percent_decode(&value);
    }
    value = value.replace('\0', "");
    if cfg!(windows) {
        value = value.replace('/', "\\");
    }
    value
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &input[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// 文件名合法性校验（对应 main.js 的 validateFileName）
pub fn validate_file_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("名称不能为空".into());
    }
    if cfg!(windows) {
        if name.chars().any(|c| "<>:\"/\\|?*".contains(c)) {
            return Err("名称包含非法字符".into());
        }
        let upper = name.to_uppercase();
        let stem = upper.split('.').next().unwrap_or("");
        const RESERVED: [&str; 22] = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        if RESERVED.contains(&stem) {
            return Err("该名称为系统保留名称，不能使用".into());
        }
        if name.ends_with(' ') || name.ends_with('.') {
            return Err("文件名不能以空格或句点结尾".into());
        }
    } else if name.contains('/') {
        return Err("名称包含非法字符".into());
    }
    Ok(())
}

/// `base (n).ext` 去重（对应 main.js 的 getUniquePath）
pub fn unique_path(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let ext = extname(file_name);
    let stem = if ext.is_empty() {
        file_name.to_string()
    } else {
        file_name[..file_name.len() - ext.len()].to_string()
    };
    for i in 1..10_000 {
        let next = dir.join(format!("{stem} ({i}){ext}"));
        if !next.exists() {
            return next;
        }
    }
    dir.join(format!("{stem} ({}){ext}", now_millis()))
}

pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn iso_now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 字符串型参数读取
pub fn value_to_string(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

/// 宽松布尔判断（对应 JS 的 `!!x`）
pub fn value_truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) | None => false,
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}
