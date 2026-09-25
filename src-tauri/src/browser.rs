//! 内置浏览器：基于 Tauri 的多 webview（`unstable` 特性）在主窗口内嵌真实网页视图。
//!
//! 渲染层负责计算容器位置/尺寸并通过 `browser-webview-*` 系列命令同步给后端；
//! 后端把导航、加载状态、新开标签页等事件通过 `browser-webview-event` /
//! `browser-open-new-tab` 广播回渲染层。

use std::collections::HashMap;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tauri::webview::{NewWindowResponse, PageLoadEvent, WebviewBuilder};
use tauri::{LogicalPosition, LogicalSize, Manager, Webview, WebviewUrl};

use crate::events;
use crate::ipc::Ipc;
use crate::paths;

static VIEWS: Lazy<Mutex<HashMap<String, Webview>>> = Lazy::new(|| Mutex::new(HashMap::new()));

fn emit_view_event(app: &tauri::AppHandle, id: &str, event_type: &str, extra: Value) {
    let mut payload = json!({ "id": id, "type": event_type });
    if let (Some(map), Some(extra_map)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in extra_map {
            map.insert(key.clone(), value.clone());
        }
    }
    events::broadcast_one(app, "browser-webview-event", payload);
}

fn number_of(options: &serde_json::Map<String, Value>, key: &str, fallback: f64) -> f64 {
    options
        .get(key)
        .and_then(|value| match value {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse::<f64>().ok(),
            _ => None,
        })
        .unwrap_or(fallback)
}

struct Bounds {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn bounds_of(options: &serde_json::Map<String, Value>) -> Bounds {
    Bounds {
        x: number_of(options, "x", 0.0).max(0.0),
        y: number_of(options, "y", 0.0).max(0.0),
        width: number_of(options, "width", 800.0).max(1.0),
        height: number_of(options, "height", 600.0).max(1.0),
    }
}

fn id_of(options: &serde_json::Map<String, Value>) -> String {
    options
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string()
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "browser-webview-create" => Some(create(ipc).await),
        "browser-webview-set-bounds" => {
            let options = ipc.object_arg(0);
            let id = id_of(&options);
            let bounds = bounds_of(&options);
            if let Ok(guard) = VIEWS.lock() {
                if let Some(view) = guard.get(&id) {
                    let _ = view.set_position(LogicalPosition::new(bounds.x, bounds.y));
                    let _ = view.set_size(LogicalSize::new(bounds.width, bounds.height));
                }
            }
            Some(Ok(json!({ "ok": true })))
        }
        "browser-webview-navigate" => {
            let options = ipc.object_arg(0);
            let id = id_of(&options);
            let url_text = options
                .get("url")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let parsed = match url::Url::parse(&url_text) {
                Ok(url) => url,
                Err(err) => return Some(Err(format!("无效的 URL: {err}"))),
            };
            if let Ok(guard) = VIEWS.lock() {
                if let Some(view) = guard.get(&id) {
                    if let Err(err) = view.navigate(parsed) {
                        return Some(Err(err.to_string()));
                    }
                }
            }
            Some(Ok(json!({ "ok": true })))
        }
        "browser-webview-action" => {
            let options = ipc.object_arg(0);
            let id = id_of(&options);
            let action = options
                .get("action")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            let script = match action.as_str() {
                "back" => "history.back()",
                "forward" => "history.forward()",
                "reload" => "location.reload()",
                "stop" => "window.stop()",
                other => return Some(Err(format!("不支持的操作: {other}"))),
            };
            if let Ok(guard) = VIEWS.lock() {
                if let Some(view) = guard.get(&id) {
                    let _ = view.eval(script);
                }
            }
            Some(Ok(json!({ "ok": true })))
        }
        "browser-webview-set-visible" => {
            let options = ipc.object_arg(0);
            let id = id_of(&options);
            let visible = options
                .get("visible")
                .and_then(|value| value.as_bool())
                .unwrap_or(true);
            if let Ok(guard) = VIEWS.lock() {
                if let Some(view) = guard.get(&id) {
                    if visible {
                        let _ = view.show();
                    } else {
                        let _ = view.hide();
                    }
                }
            }
            Some(Ok(json!({ "ok": true })))
        }
        "browser-webview-state" => {
            let options = ipc.object_arg(0);
            let id = id_of(&options);
            let mut url_text = String::new();
            if let Ok(guard) = VIEWS.lock() {
                if let Some(view) = guard.get(&id) {
                    if let Ok(url) = view.url() {
                        url_text = url.to_string();
                    }
                }
            }
            // 注：`tauri::Webview` 没有 title() 接口，标题由渲染层自行维护
            Some(Ok(json!({ "url": url_text, "title": "" })))
        }
        "browser-webview-close" => {
            let options = ipc.object_arg(0);
            let id = id_of(&options);
            let view = VIEWS.lock().ok().and_then(|mut guard| guard.remove(&id));
            if let Some(view) = view {
                let _ = view.close();
            }
            Some(Ok(json!({ "ok": true })))
        }
        "browser-webview-list" => {
            let ids: Vec<String> = VIEWS
                .lock()
                .map(|guard| guard.keys().cloned().collect())
                .unwrap_or_default();
            Some(Ok(json!({ "ids": ids })))
        }
        _ => None,
    }
}

async fn create(ipc: &Ipc<'_>) -> Result<Value, String> {
    let options = ipc.object_arg(0);
    let id = id_of(&options);
    if id.is_empty() {
        return Err("缺少 webview id".to_string());
    }
    let url_text = options
        .get("url")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let parsed = url::Url::parse(&url_text).map_err(|err| format!("无效的 URL: {err}"))?;
    let bounds = bounds_of(&options);
    let label = format!("oicpp-browser-{id}");

    let data_dir = paths::oicpp_dir().join("browser");
    let _ = std::fs::create_dir_all(&data_dir);

    let app_for_load = ipc.app.clone();
    let id_for_load = id.clone();
    let app_for_window = ipc.app.clone();
    let id_for_window = id.clone();

    let mut builder = WebviewBuilder::new(&label, WebviewUrl::External(parsed))
        .auto_resize()
        .data_directory(data_dir)
        .on_page_load(move |_webview, payload| {
            let event_type = match payload.event() {
                PageLoadEvent::Started => "did-start-loading",
                PageLoadEvent::Finished => "did-stop-loading",
            };
            emit_view_event(
                &app_for_load,
                &id_for_load,
                event_type,
                json!({ "url": payload.url().to_string() }),
            );
        })
        .on_new_window(move |url, _features| {
            events::broadcast_one(
                &app_for_window,
                "browser-open-new-tab",
                json!({
                    "url": url.to_string(),
                    "disposition": "default",
                    "sourceWebviewId": id_for_window.clone()
                }),
            );
            NewWindowResponse::Deny
        });

    if let Some(user_agent) = options.get("userAgent").and_then(|value| value.as_str()) {
        if !user_agent.is_empty() {
            builder = builder.user_agent(user_agent);
        }
    }

    // 注意：`add_child` 定义在 `Window` 上（不是 `WebviewWindow`）
    let window = ipc
        .app
        .get_window("main")
        .ok_or_else(|| "主窗口不存在".to_string())?;
    let webview = window
        .add_child(
            builder,
            LogicalPosition::new(bounds.x, bounds.y),
            LogicalSize::new(bounds.width, bounds.height),
        )
        .map_err(|err| err.to_string())?;

    if let Ok(mut guard) = VIEWS.lock() {
        guard.insert(id, webview);
    }
    Ok(json!({ "ok": true }))
}
