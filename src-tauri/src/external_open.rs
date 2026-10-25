//! 单实例 / 外部文件打开。
//!
//! 第一次启动时解析命令行参数；之后由 `tauri-plugin-single-instance` 把第二次启动的
//! 参数转发到已有进程。这里负责过滤支持的源文件、处理文件夹、串行打开多个目标，
//! 并聚焦主窗口。渲染层通过 `workspace-path-report` 上报当前工作区，用于避免在已有
//! 工作区时因为双击外部文件而强行切换工作区。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use once_cell::sync::Lazy;
use serde_json::Value;
use tauri::{AppHandle, Manager};

use crate::{events, fsops, misc, paths, settings};

const SUPPORTED_EXTERNAL_EXTS: [&str; 6] = [".cpp", ".c", ".cc", ".cxx", ".h", ".hpp"];
const STARTUP_OPEN_DELAY_MS: u64 = 1500;
const STEP_DELAY_MS: u64 = 250;

static CURRENT_WORKSPACE: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));
/// 串行化外部打开操作，避免多个 second-instance 同时切换工作区。
static OPEN_LOCK: Lazy<tokio::sync::Mutex<()>> = Lazy::new(|| tokio::sync::Mutex::new(()));

#[derive(Debug, Default)]
struct OpenTargets {
    files: Vec<String>,
    folders: Vec<String>,
}

impl OpenTargets {
    fn is_empty(&self) -> bool {
        self.files.is_empty() && self.folders.is_empty()
    }
}

/// 渲染层上报的当前工作区。传空字符串表示工作区已关闭。
pub fn set_current_workspace(path: Option<String>) {
    let value = path
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if let Ok(mut guard) = CURRENT_WORKSPACE.lock() {
        *guard = value;
    }
}

fn current_workspace() -> Option<String> {
    CURRENT_WORKSPACE.lock().ok().and_then(|guard| guard.clone())
}

fn focus_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn is_supported_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    let ext = paths::extname(&path.to_string_lossy()).to_lowercase();
    SUPPORTED_EXTERNAL_EXTS.contains(&ext.as_str())
}

fn normalize_existing_path(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let cleaned = paths::normalize_dropped_path(raw);
    let cleaned = cleaned.trim();
    if cleaned.is_empty() || cleaned == "." {
        return None;
    }
    let candidate = PathBuf::from(cleaned);
    let resolved = if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    };
    if resolved.exists() {
        Some(resolved)
    } else {
        None
    }
}

fn extract_targets(args: &[String], cwd: &Path) -> OpenTargets {
    let mut targets = OpenTargets::default();
    let mut seen_folders: HashSet<String> = HashSet::new();
    let mut seen_files: HashSet<String> = HashSet::new();

    for raw in args {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || trimmed == "."
            || trimmed.starts_with("--")
            || trimmed.starts_with("-psn")
        {
            continue;
        }
        let Some(resolved) = normalize_existing_path(trimmed, cwd) else {
            continue;
        };
        let path_text = resolved.to_string_lossy().to_string();
        let dedupe_key = if cfg!(windows) {
            path_text.to_lowercase()
        } else {
            path_text.clone()
        };

        if resolved.is_dir() {
            if seen_folders.insert(dedupe_key) {
                targets.folders.push(path_text);
            }
        } else if is_supported_file(&resolved) && seen_files.insert(dedupe_key) {
            targets.files.push(path_text);
        }
    }

    targets
}

fn startup_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 应用启动时处理命令行参数。
pub fn handle_startup_args(app: &AppHandle) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cwd = startup_cwd();
    let targets = extract_targets(&args, &cwd);
    if targets.is_empty() {
        return;
    }

    // 文件夹参数优先决定启动工作区；只有文件参数时才沿用“已有可恢复工作区则保留”的规则。
    if let Some(folder) = targets.folders.first() {
        settings::set_startup_workspace(folder.clone());
    } else if !crate::has_restorable_workspace() {
        if let Some(file) = targets.files.first() {
            settings::set_startup_workspace(paths::dirname(file));
        }
    }

    misc::log_line(
        "info",
        &format!(
            "[外部打开] 启动参数: files={:?}, folders={:?}",
            targets.files, targets.folders
        ),
    );

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(STARTUP_OPEN_DELAY_MS)).await;
        let _guard = OPEN_LOCK.lock().await;
        // 首个文件夹已经交给 startup workspace 处理，避免重复切换。
        let folders = if targets.folders.len() > 1 {
            targets.folders[1..].to_vec()
        } else {
            Vec::new()
        };
        open_targets(&handle, folders, targets.files, false).await;
    });
}

/// `tauri-plugin-single-instance` 回调：已有实例收到第二次启动的参数。
pub fn handle_second_instance(app: &AppHandle, argv: &[String], cwd: &str) {
    let cwd = if cwd.trim().is_empty() {
        startup_cwd()
    } else {
        PathBuf::from(cwd)
    };
    let args = if argv.is_empty() {
        Vec::new()
    } else {
        argv[1..].to_vec()
    };
    let targets = extract_targets(&args, &cwd);

    focus_main_window(app);
    misc::log_line(
        "info",
        &format!(
            "[外部打开] second-instance: files={:?}, folders={:?}",
            targets.files, targets.folders
        ),
    );
    if targets.is_empty() {
        return;
    }

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _guard = OPEN_LOCK.lock().await;
        open_targets(&handle, targets.folders, targets.files, true).await;
    });
}

async fn open_targets(
    app: &AppHandle,
    folders: Vec<String>,
    files: Vec<String>,
    second_instance: bool,
) {
    for folder in folders {
        switch_workspace(app, &folder);
        tokio::time::sleep(Duration::from_millis(STEP_DELAY_MS)).await;
    }

    if files.is_empty() {
        return;
    }

    // 第二次启动且当前没有工作区时，把第一个文件所在目录设为工作区。
    if second_instance && current_workspace().is_none() {
        if let Some(parent) = Path::new(&files[0]).parent() {
            let parent = parent.to_string_lossy().to_string();
            if !parent.is_empty() {
                switch_workspace(app, &parent);
                tokio::time::sleep(Duration::from_millis(STEP_DELAY_MS)).await;
            }
        }
    }

    for file in files {
        let content = fsops::read_text_file(&file).unwrap_or_default();
        misc::log_line("info", &format!("[外部打开] 打开文件: {file}"));
        events::broadcast_one(
            app,
            "file-opened-from-args",
            serde_json::json!({
                "path": file,
                "fileName": paths::basename(&file),
                "content": content
            }),
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn switch_workspace(app: &AppHandle, folder: &str) {
    settings::set_value("lastOpen", Value::String(folder.to_string()));
    let _ = settings::save();
    set_current_workspace(Some(folder.to_string()));
    events::broadcast_one(app, "folder-opened", Value::String(folder.to_string()));
    misc::log_line("info", &format!("[外部打开] 切换工作区: {folder}"));
}
