//! WebView 初始化脚本注入：把兼容层和运行环境信息塞进页面。

use serde_json::{json, Value};
use tauri::AppHandle;

use crate::paths;

/// 前端兼容层源码（编译期嵌入，保证在页面任何脚本之前执行）
pub const SHIM_JS: &str = include_str!("../../frontend-src/shim/electron-api.js");

pub fn asset_origin() -> String {
    if cfg!(windows) {
        "http://asset.localhost".to_string()
    } else {
        "asset://localhost".to_string()
    }
}

/// 生成注入脚本；`params` 目前支持 `queryString`（供设置窗口读取 theme 等参数）
pub fn build_script(params: &Value) -> String {
    let env = json!({
        "platform": paths::platform_key(),
        "home": paths::home_dir().to_string_lossy(),
        "debug": cfg!(debug_assertions),
        "assetOrigin": asset_origin(),
        "queryString": params.get("queryString").and_then(|v| v.as_str()).unwrap_or(""),
        "tauriVersion": "2",
    });
    format!("window.__OICPP_ENV__ = {env};\n{SHIM_JS}")
}

/// 主窗口注入脚本
pub fn main_script() -> String {
    build_script(&json!({}))
}

/// 打包时使用的 clangd 目录（若存在）
pub fn bundled_clangd_root(app: &AppHandle) -> Option<std::path::PathBuf> {
    let dir = paths::resource_dir(app).join("clangd");
    if dir.exists() {
        Some(dir)
    } else {
        None
    }
}
