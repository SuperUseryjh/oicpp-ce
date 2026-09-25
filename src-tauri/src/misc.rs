//! 平台信息 / 窗口控制 / 剪贴板 / 对话框 / 外链 / 日志 / 内置浏览器辅助 等杂项通道。

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_opener::OpenerExt;

use crate::events;
use crate::ipc::Ipc;
use crate::paths;
use crate::settings;

static CLOSE_WAITER: Lazy<Mutex<Option<tokio::sync::oneshot::Sender<()>>>> =
    Lazy::new(|| Mutex::new(None));
static CURRENT_LOG: Lazy<Mutex<Option<PathBuf>>> = Lazy::new(|| Mutex::new(None));
static LOG_COUNTER: Lazy<Mutex<u32>> = Lazy::new(|| Mutex::new(0));

// ------------------------------------------------------------------ 日志

const LOG_FILE_LIMIT: usize = 5;

fn current_log_file() -> PathBuf {
    let mut guard = CURRENT_LOG.lock().unwrap();
    if let Some(path) = guard.as_ref() {
        return path.clone();
    }
    let dir = paths::logs_dir();
    let name = format!("{}.log", chrono::Local::now().format("%Y-%m-%d_%H-%M-%S"));
    let path = dir.join(name);
    rotate_logs(&dir);
    *guard = Some(path.clone());
    path
}

fn rotate_logs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext.eq_ignore_ascii_case("log"))
                .unwrap_or(false)
        })
        .filter_map(|e| {
            let modified = e.metadata().ok().and_then(|m| m.modified().ok())?;
            Some((modified, e.path()))
        })
        .collect();
    if files.len() <= LOG_FILE_LIMIT {
        return;
    }
    files.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in files.into_iter().skip(LOG_FILE_LIMIT) {
        let _ = std::fs::remove_file(path);
    }
}

fn write_log(level: &str, args: &Value, meta: Option<&Value>) {
    let path = current_log_file();
    let stamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f%:z");
    let mut body = serde_json::to_string(args).unwrap_or_else(|_| "[]".into());
    if let Some(meta) = meta {
        body.push_str(" ");
        body.push_str(&meta.to_string());
    }
    if body.len() > 4000 {
        body.truncate(4000);
        body.push_str("...(truncated)");
    }
    let line = format!("[{stamp}] [{}] {body}\n", level.to_uppercase());
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(line.as_bytes());
    }
    *LOG_COUNTER.lock().unwrap() += 1;
}

// ------------------------------------------------------------------ 关闭流程

fn close_main(app: &AppHandle) {
    crate::ALLOW_CLOSE.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.close();
    } else {
        app.exit(0);
    }
}

/// 写一条自定义日志（供其它模块输出诊断信息）
pub(crate) fn log_line(level: &str, message: &str) {
    write_log(level, &json!([message]), None);
}

pub(crate) fn request_save_all_and_close(app: &AppHandle) {
    events::broadcast_empty(app, "request-save-all");
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    *CLOSE_WAITER.lock().unwrap() = Some(tx);
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        let _ = tokio::time::timeout(Duration::from_millis(4000), rx).await;
        close_main(&handle);
    });
}

fn notify_save_all_complete() {
    if let Some(tx) = CLOSE_WAITER.lock().unwrap().take() {
        let _ = tx.send(());
    }
}

// ------------------------------------------------------------------ 系统信息

fn system_info() -> Value {
    let distro;
    let system_version;
    if paths::is_linux() {
        let content = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let mut pretty = String::new();
        let mut name = String::new();
        let mut version = String::new();
        for line in content.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"').to_string();
            match key {
                "PRETTY_NAME" => pretty = value,
                "NAME" => name = value,
                "VERSION" => version = value,
                _ => {}
            }
        }
        distro = if !pretty.is_empty() {
            pretty.clone()
        } else if !name.is_empty() {
            format!("{name} {version}").trim().to_string()
        } else {
            "Linux".to_string()
        };
        system_version = distro.clone();
    } else if paths::is_macos() {
        distro = "macOS".to_string();
        system_version = std::env::var("TERM_PROGRAM_VERSION").unwrap_or_default();
    } else {
        distro = std::env::var("OS").unwrap_or_else(|_| "Windows".into());
        system_version = std::env::var("PROCESSOR_ARCHITECTURE").unwrap_or_default();
    }

    json!({
        "platform": paths::platform_key(),
        "arch": std::env::consts::ARCH,
        "nodeVersion": "",
        "homedir": paths::home_dir().to_string_lossy(),
        "tmpdir": std::env::temp_dir().to_string_lossy(),
        "osRelease": std::env::consts::OS,
        "systemVersion": system_version,
        "distro": distro,
    })
}

pub(crate) fn build_info(app: &AppHandle) -> Value {
    let path = paths::resource_dir(app).join("build-info.json");
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            return value;
        }
    }
    json!({ "version": "1.5.4", "buildTime": "未知", "author": "mywwzh" })
}

fn user_icon_path(app: &AppHandle) -> String {
    let user_icon = paths::oicpp_dir().join("oicpp.ico");
    if user_icon.exists() {
        return user_icon.to_string_lossy().to_string();
    }
    paths::resource_dir(app)
        .join("oicpp.ico")
        .to_string_lossy()
        .to_string()
}

// ------------------------------------------------------------------ 内置浏览器辅助

fn resolve_browser_url(input: &str) -> String {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return trimmed.to_string();
    }
    let looks_like_url = !trimmed.chars().any(char::is_whitespace) && {
        if let Ok(candidate) = url::Url::parse(&format!("https://{trimmed}")) {
            match candidate.host_str() {
                Some(host) => {
                    let is_ipv4 = host.split('.').count() == 4
                        && host
                            .split('.')
                            .all(|seg| seg.parse::<u16>().map(|n| n <= 255).unwrap_or(false));
                    let is_ipv6 = host.contains(':');
                    let is_local = host == "localhost";
                    let is_domain = regex::Regex::new(r"^[a-zA-Z0-9-]+(?:\.[a-zA-Z0-9-]+)+$")
                        .map(|re| re.is_match(host))
                        .unwrap_or(false);
                    is_ipv4 || is_ipv6 || is_local || is_domain
                }
                None => false,
            }
        } else {
            false
        }
    };
    if looks_like_url {
        match url::Url::parse(&format!("https://{trimmed}")) {
            Ok(candidate) => candidate.to_string(),
            Err(_) => format!(
                "https://www.google.com/search?q={}",
                url_encode(trimmed)
            ),
        }
    } else {
        format!("https://www.google.com/search?q={}", url_encode(trimmed))
    }
}

fn url_encode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
            out.push(c);
        } else if c == ' ' {
            out.push_str("%20");
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn page_title_host(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(parsed) => parsed
            .host_str()
            .map(|host| host.strip_prefix("www.").unwrap_or(host).to_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

// ------------------------------------------------------------------ 窗口外观

/// 玻璃（毛玻璃 / Mica）效果：仅在 Windows / macOS 生效，需窗口透明才有完整效果
fn apply_glass_effect(window: &tauri::WebviewWindow) {
    #[cfg(windows)]
    {
        use tauri::window::{Effect, EffectsBuilder};
        let _ = window.set_effects(EffectsBuilder::new().effect(Effect::Mica).build());
    }
    #[cfg(target_os = "macos")]
    {
        use tauri::window::{Effect, EffectsBuilder};
        let _ = window.set_effects(EffectsBuilder::new().effect(Effect::HudWindow).build());
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = window;
    }
}

/// 窗口整体透明度：Windows 用分层窗口属性实现；其它平台暂不支持
#[cfg(windows)]
fn apply_window_opacity(window: &tauri::WebviewWindow, opacity: f64) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE, LWA_ALPHA,
        WS_EX_LAYERED,
    };

    let Ok(handle) = window.hwnd() else {
        return;
    };
    let hwnd = handle.0 as isize;
    let alpha = (opacity.clamp(0.05, 1.0) * 255.0).round() as u8;
    unsafe {
        let current = GetWindowLongPtrW(hwnd as _, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd as _, GWL_EXSTYLE, current | (WS_EX_LAYERED as isize));
        let _ = SetLayeredWindowAttributes(hwnd as _, 0, alpha, LWA_ALPHA);
    }
}

#[cfg(not(windows))]
fn apply_window_opacity(_window: &tauri::WebviewWindow, _opacity: f64) {}

/// 应用当前设置里的窗口透明度与玻璃效果
pub(crate) fn apply_window_visuals(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if settings::get_bool("glassEffectEnabled", false) {
        apply_glass_effect(&window);
    }
    let opacity = settings::get_value("windowOpacity")
        .and_then(|value| value.as_f64())
        .unwrap_or(1.0);
    apply_window_opacity(&window, opacity);
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "logger-log" => {
            let payload = ipc.object_arg(0);
            let level = payload
                .get("level")
                .and_then(|v| v.as_str())
                .unwrap_or("info")
                .to_string();
            let args = payload.get("args").cloned().unwrap_or(json!([]));
            let meta = payload.get("meta").cloned();
            write_log(&level, &args, meta.as_ref());
            Some(Ok(Value::Null))
        }
        "get-platform" => ipc.ok(paths::platform_name()),
        "get-user-home" | "get-home-dir" => ipc.ok(paths::home_dir().to_string_lossy().to_string()),
        "get-cpu-threads" => {
            let threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(2)
                .max(1);
            Some(Ok(json!(threads)))
        }
        "get-app-path" => {
            let path = std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|p| p.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."));
            ipc.ok(path.to_string_lossy().to_string())
        }
        "get-system-info" => ipc.ok(system_info()),
        "get-build-info" => ipc.ok(build_info(ipc.app)),
        "get-user-icon-path" => ipc.ok(user_icon_path(ipc.app)),

        // ---- 窗口控制
        "window-minimize" => {
            if let Some(window) = ipc.app.get_webview_window("main") {
                let _ = window.minimize();
            }
            Some(Ok(Value::Null))
        }
        "window-maximize" => {
            if let Some(window) = ipc.app.get_webview_window("main") {
                if window.is_maximized().unwrap_or(false) {
                    let _ = window.unmaximize();
                } else {
                    let _ = window.maximize();
                }
            }
            Some(Ok(Value::Null))
        }
        "window-unmaximize" => {
            if let Some(window) = ipc.app.get_webview_window("main") {
                let _ = window.unmaximize();
            }
            Some(Ok(Value::Null))
        }
        "window-is-maximized" => {
            let maximized = ipc
                .app
                .get_webview_window("main")
                .and_then(|w| w.is_maximized().ok())
                .unwrap_or(false);
            Some(Ok(Value::Bool(maximized)))
        }
        "window-close" | "app-close-confirmed" => {
            request_save_all_and_close(ipc.app);
            Some(Ok(Value::Null))
        }
        "window-close-discard" | "app-close-discard" => {
            close_main(ipc.app);
            Some(Ok(Value::Null))
        }
        "app-close-cancelled" => {
            *CLOSE_WAITER.lock().unwrap() = None;
            Some(Ok(Value::Null))
        }
        "save-all-complete" => {
            notify_save_all_complete();
            Some(Ok(Value::Null))
        }
        "relaunch-app" => {
            ipc.app.restart();
        }

        // ---- 剪贴板
        "clipboard-write-text" => {
            let text = ipc.string_arg(0);
            match ipc.app.clipboard().write_text(text) {
                Ok(_) => Some(Ok(json!({ "success": true }))),
                Err(err) => Some(Ok(json!({ "success": false, "error": err.to_string() }))),
            }
        }
        "clipboard-read-text" => match ipc.app.clipboard().read_text() {
            Ok(text) => Some(Ok(json!({ "success": true, "text": text }))),
            Err(err) => Some(Ok(json!({ "success": false, "error": err.to_string() }))),
        },

        // ---- 外链 / 文件
        "open-external" => {
            let url = ipc.string_arg(0);
            if url.trim().is_empty() {
                return ipc.fail("Invalid URL");
            }
            match ipc.app.opener().open_url(url, None::<String>) {
                Ok(_) => Some(Ok(json!({ "ok": true }))),
                Err(err) => ipc.fail(err.to_string()),
            }
        }
        "open-path" => {
            let raw = ipc.string_arg(0);
            let options = ipc.object_arg(1);
            let reveal = crate::paths::value_truthy(options.get("reveal"))
                || crate::paths::value_truthy(options.get("highlight"));
            if raw.trim().is_empty() {
                return Some(Ok(json!({ "success": false, "error": "路径无效" })));
            }
            let target = PathBuf::from(&raw);
            if !target.exists() {
                return Some(Ok(json!({ "success": false, "error": format!("路径不存在: {raw}") })));
            }
            let is_file = std::fs::metadata(&target)
                .map(|m| m.is_file())
                .unwrap_or(false);
            if reveal || is_file {
                match ipc.app.opener().reveal_item_in_dir(&target) {
                    Ok(_) => Some(Ok(json!({ "success": true, "action": "reveal" }))),
                    Err(err) => Some(Ok(json!({ "success": false, "error": err.to_string() }))),
                }
            } else {
                match ipc.app.opener().open_path(raw, None::<String>) {
                    Ok(_) => Some(Ok(json!({ "success": true, "action": "open" }))),
                    Err(err) => Some(Ok(json!({ "success": false, "error": err.to_string() }))),
                }
            }
        }
        "show-open-dialog" => {
            let options = ipc.object_arg(0);
            let filters = parse_filters(options.get("filters"));
            let directory = options
                .get("properties")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter().any(|item| {
                        item.as_str()
                            .map(|s| s == "openDirectory")
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false);
            let multiple = options
                .get("properties")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .any(|item| item.as_str().map(|s| s == "multiSelections").unwrap_or(false))
                })
                .unwrap_or(false);
            let picked = crate::dlg::pick_open(
                ipc.app,
                crate::dlg::OpenOptions {
                    title: options.get("title").and_then(|v| v.as_str()).map(String::from),
                    directory,
                    multiple,
                    default_path: options
                        .get("defaultPath")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    filters,
                },
            )
            .await;
            if picked.is_empty() {
                Some(Ok(json!({ "canceled": true, "filePaths": [] })))
            } else {
                let list: Vec<Value> = picked
                    .iter()
                    .map(|p| Value::String(p.to_string_lossy().to_string()))
                    .collect();
                Some(Ok(json!({ "canceled": false, "filePaths": list })))
            }
        }
        "show-save-dialog" => {
            let options = ipc.object_arg(0);
            let filters = parse_filters(options.get("filters"));
            let default_name = options
                .get("defaultPath")
                .and_then(|v| v.as_str())
                .and_then(|p| PathBuf::from(p).file_name().map(|n| n.to_string_lossy().to_string()));
            let picked = crate::dlg::pick_save(
                ipc.app,
                options.get("title").and_then(|v| v.as_str()),
                default_name.as_deref(),
                filters,
            )
            .await;
            match picked {
                Some(path) => Some(Ok(
                    json!({ "canceled": false, "filePath": path.to_string_lossy() }),
                )),
                None => Some(Ok(json!({ "canceled": true, "filePath": Value::Null }))),
            }
        }
        "show-message-box" => {
            let options = ipc.object_arg(0);
            let title = options
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("提示")
                .to_string();
            let mut message = options
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(detail) = options.get("detail").and_then(|v| v.as_str()) {
                if !detail.is_empty() {
                    message.push_str("\n\n");
                    message.push_str(detail);
                }
            }
            let buttons: Vec<String> = options
                .get("buttons")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|b| b.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_else(|| vec!["确定".into()]);
            let yes = buttons.first().cloned().unwrap_or_else(|| "确定".into());
            let no = buttons.get(1).cloned();
            let response = if let Some(no) = no {
                let ok = crate::dlg::confirm(ipc.app, &title, &message, &yes, &no).await;
                if ok { 0 } else { 1 }
            } else {
                crate::dlg::message(ipc.app, &title, &message).await;
                0
            };
            Some(Ok(json!({ "response": response, "checkboxChecked": false })))
        }

        // ---- 设置窗口
        "open-compiler-settings" => {
            crate::open_settings_window(ipc.app, "compiler-settings", "compiler.html", "");
            Some(Ok(Value::Null))
        }
        "open-editor-settings" => {
            let theme = settings::get_str("theme");
            let theme = if theme.is_empty() { "dark".to_string() } else { theme };
            crate::open_settings_window(
                ipc.app,
                "editor-settings",
                "editor.html",
                &format!("?theme={theme}"),
            );
            Some(Ok(Value::Null))
        }
        "open-template-settings" => {
            crate::open_settings_window(ipc.app, "template-settings", "templates.html", "");
            Some(Ok(Value::Null))
        }
        "open-backup-settings" => {
            let theme = settings::get_str("theme");
            let theme = if theme.is_empty() { "dark".to_string() } else { theme };
            crate::open_settings_window(
                ipc.app,
                "backup-settings",
                "backup.html",
                &format!("?theme={theme}"),
            );
            Some(Ok(Value::Null))
        }

        // ---- 更新（Tauri 版暂不内置自动更新）
        "get-update-download-status" => Some(Ok(crate::update::state_payload())),
        "check-updates-manual" => {
            let app = ipc.app.clone();
            tauri::async_runtime::spawn(async move {
                crate::update::check_updates(&app, true).await;
            });
            Some(Ok(Value::Null))
        }

        // ---- 窗口外观（透明度 / 玻璃效果）
        "apply-window-effects" => {
            apply_window_visuals(ipc.app);
            Some(Ok(Value::Null))
        }

        // ---- 工作区上报（渲染进程为唯一事实来源）
        //
        // 原实现把它存下来供「样例抓取 HTTP 服务」使用，该服务尚未移植，
        // 因此这里只保留通道以免渲染层报未注册，不保存状态。
        "workspace-path-report" => Some(Ok(Value::Null)),

        // ---- 请求转发（渲染层自行处理，这里只做日志）
        "request-new-file" | "request-open-file" | "request-save-file" => Some(Ok(Value::Null)),

        // ---- 内置浏览器
        "browser-resolve-url" => ipc.ok(resolve_browser_url(&ipc.string_arg(0))),
        "browser-get-page-title" => ipc.ok(page_title_host(&ipc.string_arg(0))),

        _ => None,
    }
}

fn parse_filters(value: Option<&Value>) -> Vec<(String, Vec<String>)> {
    let Some(Value::Array(list)) = value else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|item| {
            let name = item.get("name").and_then(|v| v.as_str())?.to_string();
            let exts: Vec<String> = item
                .get("extensions")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if exts.is_empty() {
                None
            } else {
                Some((name, exts))
            }
        })
        .collect()
}
