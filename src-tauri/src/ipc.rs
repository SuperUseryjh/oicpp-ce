//! IPC 分发骨架。
//!
//! 前端 shim 把所有 `ipcRenderer.invoke/send` 统一转成两个 Tauri 命令：
//!   - `ipc_invoke(channel, args)`  -> 同步拿到返回值
//!   - `ipc_send(channel, args)`    -> 即发即忘
//!
//! 这里按模块顺序询问，谁认领了通道谁负责处理。

use serde_json::{Map, Value};
use tauri::AppHandle;

use crate::events;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Invoke,
    Send,
}

pub struct Ipc<'a> {
    pub app: &'a AppHandle,
    pub kind: Kind,
    pub channel: &'a str,
    pub args: &'a [Value],
}

impl<'a> Ipc<'a> {
    pub fn is_invoke(&self) -> bool {
        matches!(self.kind, Kind::Invoke)
    }

    pub fn arg(&self, idx: usize) -> Option<&Value> {
        self.args.get(idx)
    }

    pub fn string_arg(&self, idx: usize) -> String {
        crate::paths::value_to_string(self.args.get(idx))
    }

    /// 与 JS 的 `options.field` 语义一致：缺失返回空字符串
    pub fn object_arg(&self, idx: usize) -> Map<String, Value> {
        match self.args.get(idx) {
            Some(Value::Object(map)) => map.clone(),
            _ => Map::new(),
        }
    }

    pub fn number_arg(&self, idx: usize) -> Option<f64> {
        match self.args.get(idx) {
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => s.parse::<f64>().ok(),
            _ => None,
        }
    }

    pub fn emit(&self, channel: &str, args: Vec<Value>) {
        events::broadcast(self.app, channel, args);
    }

    pub fn emit_one(&self, channel: &str, payload: Value) {
        events::broadcast(self.app, channel, vec![payload]);
    }

    pub fn ok(&self, value: impl Into<Value>) -> Option<Result<Value, String>> {
        Some(Ok(value.into()))
    }

    pub fn fail(&self, message: impl Into<String>) -> Option<Result<Value, String>> {
        Some(Err(message.into()))
    }
}

/// 分发入口
pub async fn dispatch(
    app: &AppHandle,
    kind: Kind,
    channel: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let ipc = Ipc {
        app,
        kind,
        channel,
        args,
    };

    // 依次询问各业务模块，谁认领了通道谁负责处理。
    // 这里刻意不使用宏：`$module:path` 片段后不能再接 `::`。
    if let Some(result) = crate::settings::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::fsops::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::fsread::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::misc::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::update::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::browser::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::compile::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::toolchain::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::download::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::terminal::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::lsp::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::debugger::dispatch(&ipc).await {
        return Some(result);
    }
    if let Some(result) = crate::cloud::dispatch(&ipc).await {
        return Some(result);
    }

    None
}
