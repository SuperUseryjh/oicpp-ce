//! 主进程 -> 渲染进程的事件通道。
//!
//! Tauri 侧统一使用一个事件名 `oicpp`，payload 为 `{ channel, args }`；
//! 前端 shim 收到后再按 channel 分发给 `ipcRenderer.on` 注册的监听器。

use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

pub const EVENT_NAME: &str = "oicpp";

/// 广播到所有窗口
pub fn broadcast(app: &AppHandle, channel: &str, args: Vec<Value>) {
    let _ = app.emit(EVENT_NAME, json!({ "channel": channel, "args": args }));
}

/// 便捷：只带一个 payload
pub fn broadcast_one(app: &AppHandle, channel: &str, payload: Value) {
    broadcast(app, channel, vec![payload]);
}

/// 无 payload
pub fn broadcast_empty(app: &AppHandle, channel: &str) {
    broadcast(app, channel, vec![]);
}
