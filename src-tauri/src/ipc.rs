//! IPC 分发骨架。
//!
//! 前端 shim 把所有 `ipcRenderer.invoke/send` 统一转成两个 Tauri 命令：
//!   - `ipc_invoke(channel, args)`  -> 同步拿到返回值
//!   - `ipc_send(channel, args)`    -> 即发即忘
//!
//! 这里按模块顺序询问，谁认领了通道谁负责处理。

use std::future::Future;

use serde_json::{Map, Value};
use tauri::{AppHandle, WebviewWindow};

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
    pub window: Option<&'a WebviewWindow>,
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

    pub fn opt_string_arg(&self, idx: usize) -> Option<String> {
        match self.args.get(idx) {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        }
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

    pub fn bool_arg(&self, idx: usize) -> bool {
        crate::paths::value_truthy(self.args.get(idx))
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

/// 便于模块书写：`Value::from(x)`
pub fn obj(pairs: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (k, v) in pairs {
        map.insert(k.to_string(), v);
    }
    Value::Object(map)
}

pub trait IntoValue {
    fn into_value(self) -> Value;
}

impl IntoValue for Value {
    fn into_value(self) -> Value {
        self
    }
}

/// 分发入口
pub async fn dispatch(
    app: &AppHandle,
    window: Option<&WebviewWindow>,
    kind: Kind,
    channel: &str,
    args: &[Value],
) -> Option<Result<Value, String>> {
    let ipc = Ipc {
        app,
        kind,
        channel,
        args,
        window,
    };

    macro_rules! ask {
        ($module:path) => {
            if let Some(result) = $module::dispatch(&ipc).await {
                return Some(result);
            }
        };
    }

    ask!(crate::settings);
    ask!(crate::fsops);
    ask!(crate::fsread);
    ask!(crate::misc);
    ask!(crate::update);
    ask!(crate::browser);
    ask!(crate::compile);
    ask!(crate::toolchain);
    ask!(crate::download);
    ask!(crate::terminal);
    ask!(crate::lsp);
    ask!(crate::debugger);
    ask!(crate::cloud);

    None
}

/// 供模块内部复用：包一层便于书写
pub fn boxed<F>(future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    future
}
