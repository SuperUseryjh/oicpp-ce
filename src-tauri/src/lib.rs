//! OICPP IDE —— Tauri 版应用入口。
//!
//! 架构：静态前端（`frontend/`，由 `scripts/prepare-frontend.mjs` 从 `oicpp-origin` 生成）
//! + Rust 后端（本 crate）。前端通过注入的兼容层 (`inject.rs`) 用统一的
//! `ipc_invoke` / `ipc_send` 命令访问后端，后端再按通道名分发到各业务模块。

// 设置项的 `json!` 字面量层级较深，默认 128 的递归上限不够用
#![recursion_limit = "512"]

mod browser;
mod codec;
mod compile;
mod cloud;
mod debugger;
mod dlg;
mod download;
mod events;
mod fsops;
mod fsread;
mod inject;
mod ipc;
mod lsp;
mod menu;
mod misc;
mod paths;
mod settings;
mod terminal;
mod toolchain;
mod update;

use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use tauri::{AppHandle, Manager};

/// 允许窗口真正关闭（避免 CloseRequested 再次触发保存流程）
pub(crate) static ALLOW_CLOSE: AtomicBool = AtomicBool::new(false);

#[tauri::command]
async fn ipc_invoke(app: AppHandle, channel: String, args: Vec<Value>) -> Result<Value, String> {
    match ipc::dispatch(&app, ipc::Kind::Invoke, &channel, &args).await {
        Some(result) => result,
        None => Err(format!("未注册的 IPC 通道: {channel}")),
    }
}

#[tauri::command]
async fn ipc_send(app: AppHandle, channel: String, args: Vec<Value>) {
    if ipc::dispatch(&app, ipc::Kind::Send, &channel, &args)
        .await
        .is_none()
    {
        // 有些通道只实现了 invoke 语义，这里兜底再按 invoke 处理一次
        let _ = ipc::dispatch(&app, ipc::Kind::Invoke, &channel, &args).await;
    }
}

/// 打开设置窗口（compiler / editor / templates / backup）
pub(crate) fn open_settings_window(app: &AppHandle, label: &str, page: &str, query: &str) {
    if let Some(existing) = app.get_webview_window(label) {
        let _ = existing.set_focus();
        return;
    }
    let script = inject::build_script(&serde_json::json!({ "queryString": query }));
    let mut builder = tauri::WebviewWindowBuilder::new(
        app,
        label,
        tauri::WebviewUrl::App(format!("renderer/settings/{page}").into()),
    )
    .title("OICPP IDE 设置")
    .inner_size(980.0, 720.0)
    .min_inner_size(720.0, 520.0)
    .resizable(true)
    .initialization_script(script);

    if let Some(parent) = app.get_webview_window("main") {
        builder = match builder.parent(&parent) {
            Ok(builder) => builder,
            Err(err) => {
                eprintln!("设置窗口设置父窗口失败: {err}");
                return;
            }
        };
    }

    if let Err(err) = builder.build() {
        eprintln!("创建设置窗口失败: {err}");
    }
}

fn setup_main_window(app: &mut tauri::App) -> tauri::Result<()> {
    let script = inject::main_script();
    // 用 from_config 而不是 `new() + 链式方法`：`dragDropEnabled` 只存在于窗口配置里，
    // WebviewWindowBuilder 上没有对应方法。而它必须关掉 —— 开启时 Tauri 的原生层会吃掉
    // 拖放事件，渲染层依赖的 HTML5 拖拽（把标签页拖到分屏区域、拖文件进工作区）会全部失效，
    // 表现为拖动时鼠标显示「禁止」。
    // mut 只在 macOS 分支里用到（追加 titleBarStyle / hiddenTitle）
    #[allow(unused_mut)]
    let mut config_json = serde_json::json!({
        "label": "main",
        "url": "renderer/index.html",
        "title": "OICPP IDE",
        "width": 1440.0,
        "height": 900.0,
        "minWidth": 940.0,
        "minHeight": 620.0,
        "resizable": true,
        "center": true,
        "dragDropEnabled": false,
        // macOS 保留原生边框（配合 overlay 标题栏样式），Windows / Linux 无边框
        // —— 注意是 cfg! 本身而不是取反，写反会导致 Windows 下原生标题栏重新出现
        "decorations": cfg!(target_os = "macos"),
    });
    #[cfg(target_os = "macos")]
    {
        config_json["titleBarStyle"] = serde_json::json!("overlay");
        config_json["hiddenTitle"] = serde_json::json!(true);
    }

    let window_config: tauri::utils::config::WindowConfig =
        match serde_json::from_value(config_json) {
            Ok(config) => config,
            Err(err) => {
                eprintln!("主窗口配置解析失败: {err}");
                return Ok(());
            }
        };

    let window = tauri::WebviewWindowBuilder::from_config(app, &window_config)?
        .initialization_script(script)
        .build()?;

    let menu = menu::build(app.handle());
    // 原生菜单只在 macOS 生效：Windows / Linux 下 set_menu 会在窗口内画一条原生菜单栏，
    // 而本项目的菜单是渲染层用 .menu-bar 自绘的（无边框窗口需要这样）
    #[cfg(target_os = "macos")]
    let _ = window.set_menu(menu);
    #[cfg(not(target_os = "macos"))]
    let _ = menu;

    // 最大化 / 还原状态变化通知渲染进程
    let handle = app.handle().clone();
    let maximized_flag = std::sync::Arc::new(AtomicBool::new(false));
    window.on_window_event(move |event| match event {
        tauri::WindowEvent::Resized(_) => {
            if let Some(window) = handle.get_webview_window("main") {
                let maximized = window.is_maximized().unwrap_or(false);
                let previous = maximized_flag.swap(maximized, Ordering::SeqCst);
                if maximized != previous {
                    let channel = if maximized {
                        "window-maximized"
                    } else {
                        "window-unmaximized"
                    };
                    events::broadcast_empty(&handle, channel);
                }
            }
        }
        tauri::WindowEvent::CloseRequested { api, .. } => {
            if !ALLOW_CLOSE.load(Ordering::SeqCst) {
                api.prevent_close();
                events::broadcast_empty(&handle, "app-close-requested");
                misc::request_save_all_and_close(&handle);
            }
        }
        _ => {}
    });

    Ok(())
}

/// 返回可恢复的工作区路径：`lastOpen` 优先，其次最近文件列表
fn restorable_workspace() -> Option<String> {
    let last = settings::get_str("lastOpen");
    if !last.trim().is_empty() && std::path::Path::new(&last).exists() {
        return Some(last);
    }
    settings::get_value("recentFiles")
        .and_then(|value| value.as_array().cloned())
        .and_then(|list| {
            list.iter().find_map(|item| {
                let path = item
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| item.get("path").and_then(|p| p.as_str()).map(str::to_string))?;
                if std::path::Path::new(&path).exists() {
                    Some(path)
                } else {
                    None
                }
            })
        })
}

fn has_restorable_workspace() -> bool {
    restorable_workspace().is_some()
}

/// 启动时恢复上次的工作区。对应原实现 `main.js` 的 `pendingStartupWorkspaceToOpen`：
/// 渲染层启动后会取一次「待打开工作区」，主进程必须在这之前准备好，否则
/// 重启后已打开的工作区会丢失（`lastOpen` / `lastOpenTabs` 存了却没人用）。
fn restore_startup_workspace() {
    if settings::get_value("autoOpenLastWorkspace").and_then(|value| value.as_bool()) == Some(false) {
        return;
    }
    if let Some(target) = restorable_workspace() {
        settings::set_startup_workspace(target);
    }
}

/// 把资源目录内容写进日志，便于确认打包资源（clangd / ConsolePauser 等）是否就位
fn log_resource_layout(app: &AppHandle) {
    let root = paths::resource_dir(app);
    let mut entries: Vec<String> = std::fs::read_dir(&root)
        .map(|iter| {
            iter.filter_map(|entry| entry.ok())
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if entry.path().is_dir() {
                        format!("{name}/")
                    } else {
                        name
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    entries.sort();
    misc::log_line(
        "info",
        &format!("[资源] {} => {}", root.display(), entries.join(", ")),
    );
}

/// 处理命令行传入的文件 / 目录
fn handle_startup_args(app: &AppHandle) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let target = args.into_iter().find(|arg| {
        let trimmed = arg.trim();
        !trimmed.is_empty()
            && !trimmed.starts_with('-')
            && std::path::Path::new(trimmed).exists()
    });
    let Some(target) = target else {
        return;
    };
    let is_file = std::fs::metadata(&target).map(|m| m.is_file()).unwrap_or(false);
    let workspace = if is_file {
        paths::dirname(&target)
    } else {
        target.clone()
    };
    // 与原实现一致：已有可恢复的工作区时（lastOpen / 最近文件）保留它，
    // 只把文件开在当前工作区里，不因命令行参数抢占工作区。
    if !has_restorable_workspace() {
        settings::set_startup_workspace(workspace);
    }

    if is_file {
        let content = fsops::read_text_file(&target).unwrap_or_default();
        let file_name = paths::basename(&target);
        let handle = app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            events::broadcast_one(
                &handle,
                "file-opened-from-args",
                serde_json::json!({
                    "path": target,
                    "fileName": file_name,
                    "content": content
                }),
            );
        });
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![ipc_invoke, ipc_send])
        .on_menu_event(|app, event| {
            if let Some(channel) = menu::channel_of(event.id().as_ref()) {
                events::broadcast_empty(app, channel);
            }
        })
        .setup(|app| {
            paths::oicpp_dir();
            settings::load();
            // 必须在窗口创建/渲染层取「待打开工作区」之前准备好
            restore_startup_workspace();
            log_resource_layout(app.handle());
            setup_main_window(app)?;
            let handle = app.handle().clone();
            // 窗口外观（透明度 / 玻璃效果）与启动自动检查更新
            misc::apply_window_visuals(&handle);
            update::schedule_auto_check(&handle);
            handle_startup_args(&handle);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("启动 OICPP IDE 失败")
        .run(|_app, event| {
            if let tauri::RunEvent::ExitRequested { .. } = event {
                debugger::shutdown();
                let _ = settings::save();
                // Windows 下静默拉起待安装的更新包
                update::run_pending_installer_on_quit();
            }
        });
}
