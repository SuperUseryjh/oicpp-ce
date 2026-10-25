//! 设置持久化 + 历史记录 + 语言相关的 IPC。
//!
//! 与 Electron 版保持一致：全部设置都保存在 `~/.oicpp/settings.json` 的单个 JSON 对象里。

use std::path::PathBuf;
use std::sync::Mutex;

use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};

use crate::ipc::Ipc;
use crate::paths;

static SETTINGS: Lazy<Mutex<Value>> = Lazy::new(|| Mutex::new(default_settings()));
static STARTUP_WORKSPACE: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

/// updateSettings 白名单（对应 main.js 的 validKeys）
const VALID_KEYS: &[&str] = &[
    "compilerPath",
    "pythonInterpreterPath",
    "compilerArgs",
    "runMode",
    "testlibPath",
    "font",
    "fontSize",
    "terminalFontSize",
    "terminalStartupCommand",
    "syntaxCheckEnabled",
    "lineHeight",
    "theme",
    "syntaxColorsByTheme",
    "syntaxFontStyles",
    "unifiedPreprocessorColor",
    "syntaxColors",
    "enableAutoCompletion",
    "foldingEnabled",
    "stickyScrollEnabled",
    "fontLigaturesEnabled",
    "cppTemplate",
    "tabSize",
    "formatterIndentStyle",
    "clangFormatStyle",
    "clangFormatRaw",
    "autoSave",
    "autoSaveInterval",
    "codeSnippets",
    "windowOpacity",
    "glassEffectEnabled",
    "backgroundImage",
    "markdownMode",
    "keybindings",
    "fileHistory",
    "lastOpenTabs",
    "autoOpenLastWorkspace",
    "language",
    "autoBackupSettings",
    "receiveBetaUpdates",
    "runAllSamples",
];

/// 重置设置时保留的键（对应 main.js 的 getResettableSettingsKeys 语义）
const NON_RESETTABLE_KEYS: &[&str] = &[
    "recentFiles",
    "fileHistory",
    "lastOpenTabs",
    "lastOpen",
    "pendingUpdate",
    "lastUpdateCheck",
    "postInstallNotice",
    "account",
];

pub fn default_run_mode() -> &'static str {
    if paths::is_windows() {
        "popup"
    } else {
        "integrated-terminal"
    }
}

fn default_compile_and_run_shortcut() -> &'static str {
    if paths::is_macos() {
        "Ctrl+F11"
    } else {
        "F11"
    }
}

fn default_run_all_samples_shortcut() -> &'static str {
    if paths::is_macos() {
        "Ctrl+Shift+F11"
    } else {
        "Ctrl+F11"
    }
}

pub fn default_clang_format_style() -> Value {
    json!({
        "BasedOnStyle": "LLVM",
        "IndentWidth": 4,
        "TabWidth": 4,
        "UseTab": "Never",
        "ColumnLimit": 0,
        "BreakBeforeBraces": "Attach",
        "AllowShortIfStatementsOnASingleLine": "Never",
        "AllowShortFunctionsOnASingleLine": "Empty",
        "IndentCaseLabels": false,
        "PointerAlignment": "Left",
        "SpaceBeforeParens": "ControlStatements",
        "SortIncludes": true,
        "AlignConsecutiveAssignments": false,
        "AlignConsecutiveDeclarations": false
    })
}

pub fn default_settings() -> Value {
    json!({
        "compilerPath": "",
        "pythonInterpreterPath": "",
        "compilerArgs": "-std=c++14 -O2 -static",
        "runMode": default_run_mode(),
        "testlibPath": "",
        "font": "Consolas",
        "fontSize": 14,
        "terminalFontSize": 14,
        "terminalStartupCommand": "",
        "syntaxCheckEnabled": true,
        "lineHeight": 0,
        "theme": "dark",
        "syntaxColorsByTheme": {},
        "syntaxFontStyles": {},
        "unifiedPreprocessorColor": false,
        "syntaxColors": {
            "keyword": "#c586c0",
            "string": "#ce9178",
            "number": "#b5cea8",
            "type": "#4ec9b0",
            "function": "#dcdcaa",
            "class": "#4ec9b0",
            "comment": "#6a9955"
        },
        "tabSize": 4,
        "formatterIndentStyle": "editor",
        "clangFormatStyle": default_clang_format_style(),
        "clangFormatRaw": "",
        "fontLigaturesEnabled": true,
        "foldingEnabled": true,
        "stickyScrollEnabled": true,
        "enableAutoCompletion": true,
        "autoSave": true,
        "autoSaveInterval": 60000,
        "language": "zh-cn",
        "autoBackupSettings": false,
        "receiveBetaUpdates": false,
        "markdownMode": "split",
        "lastUpdateCheck": "1970-01-01",
        "pendingUpdate": Value::Null,
        "postInstallNotice": Value::Null,
        "lastOpen": "",
        "autoOpenLastWorkspace": true,
        "recentFiles": [],
        "fileHistory": [],
        "lastOpenTabs": [],
        "codeSnippets": [],
        "windowOpacity": 1.0,
        "glassEffectEnabled": false,
        "backgroundImage": "",
        "cppTemplate": "",
        "account": Value::Null,
        "keybindings": {
            "formatCode": "Alt+Shift+S",
            "showFunctionPicker": "Ctrl+Shift+G",
            "markdownPreview": "Ctrl+Shift+V",
            "renameSymbol": "F2",
            "deleteLine": "Ctrl+D",
            "duplicateLine": "Ctrl+E",
            "moveLineUp": "Ctrl+Shift+Up",
            "moveLineDown": "Ctrl+Shift+Down",
            "copy": "Ctrl+C",
            "paste": "Ctrl+V",
            "cut": "Ctrl+X",
            "compileCode": "F9",
            "runCode": "F10",
            "compileAndRun": default_compile_and_run_shortcut(),
            "toggleDebug": "F5",
            "debugContinue": "F6",
            "debugStepOver": "F7",
            "debugStepInto": "F8",
            "debugStepOut": "Shift+F8",
            "cloudCompile": "F12",
            "openTerminal": "Ctrl+`",
            "runAllSamples": default_run_all_samples_shortcut()
        }
    })
}

// ------------------------------------------------------------------ 基础访问

pub fn all() -> Value {
    SETTINGS.lock().unwrap().clone()
}

pub fn replace(value: Value) {
    *SETTINGS.lock().unwrap() = value;
}

pub fn update<F: FnOnce(&mut Value)>(mutator: F) {
    let mut guard = SETTINGS.lock().unwrap();
    mutator(&mut guard);
}

pub fn get_str(key: &str) -> String {
    SETTINGS
        .lock()
        .unwrap()
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn get_bool(key: &str, fallback: bool) -> bool {
    SETTINGS
        .lock()
        .unwrap()
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(fallback)
}

pub fn get_value(key: &str) -> Option<Value> {
    SETTINGS.lock().unwrap().get(key).cloned()
}

pub fn set_value(key: &str, value: Value) {
    if let Some(map) = SETTINGS.lock().unwrap().as_object_mut() {
        map.insert(key.to_string(), value);
    }
}

pub fn compiler_path() -> String {
    get_str("compilerPath")
}

pub fn testlib_path() -> String {
    get_str("testlibPath")
}

pub fn account() -> Option<Value> {
    get_value("account").filter(|v| !v.is_null())
}

pub fn run_mode() -> String {
    normalize_run_mode(&get_str("runMode"))
}

pub fn normalize_run_mode(input: &str) -> String {
    if !paths::is_windows() {
        return "integrated-terminal".to_string();
    }
    if input.eq_ignore_ascii_case("integrated-terminal") {
        "integrated-terminal".to_string()
    } else {
        "popup".to_string()
    }
}

/// 顶层白名单合并（对应 main.js 的 mergeSettings）
pub fn merge_settings(defaults: &Value, incoming: &Value) -> Value {
    let mut out = defaults.clone();
    if let (Some(default_map), Some(incoming_map)) = (out.as_object_mut(), incoming.as_object()) {
        for (key, value) in incoming_map {
            if default_map.contains_key(key) {
                default_map.insert(key.clone(), value.clone());
            }
        }
    }
    out
}

fn normalize_runtime_shape(mut settings: Value) -> Value {
    let run_mode = settings
        .get("runMode")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let normalized = normalize_run_mode(&run_mode);
    if let Some(map) = settings.as_object_mut() {
        map.insert("runMode".into(), Value::String(normalized));
        if map.get("clangFormatStyle").map(|v| v.is_null()).unwrap_or(true) {
            map.insert("clangFormatStyle".into(), default_clang_format_style());
        }
        let formatter_style = map
            .get("formatterIndentStyle")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if formatter_style.is_empty() {
            let use_tab = map
                .get("clangFormatStyle")
                .and_then(|v| v.get("UseTab"))
                .and_then(|v| v.as_str())
                .unwrap_or("Never");
            map.insert(
                "formatterIndentStyle".into(),
                Value::String(if use_tab == "Always" { "tabs" } else { "editor" }.into()),
            );
        }
    }
    settings
}

// ------------------------------------------------------------------ 持久化

#[cfg(target_os = "linux")]
fn restore_linux_settings_backup() {
    let backup = PathBuf::from("/tmp/oicpp_backup/settings.json");
    if !backup.exists() {
        return;
    }
    let target = paths::settings_path();
    let need_restore = !target.exists()
        || std::fs::metadata(&target)
            .map(|meta| meta.len() < 10)
            .unwrap_or(true);
    if need_restore {
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::copy(&backup, &target) {
            Ok(_) => crate::misc::log_line("info", "[启动] 已从临时备份恢复 settings.json"),
            Err(err) => crate::misc::log_line(
                "warn",
                &format!("[启动] 恢复 Linux 设置备份失败: {err}"),
            ),
        }
    }
    let _ = std::fs::remove_file(&backup);
}

pub fn load() {
    #[cfg(target_os = "linux")]
    restore_linux_settings_backup();

    let defaults = default_settings();
    let path = paths::settings_path();
    let loaded = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok());
    let had_settings = loaded.is_some();

    match loaded {
        Some(value) => {
            let merged = merge_settings(&defaults, &value);
            replace(normalize_runtime_shape(merged));
        }
        None => {
            replace(defaults);
        }
    }

    // 非 Windows 首次启动时自动探测系统编译器，避免用户必须先手动配置。
    if ensure_compiler_path() || !had_settings {
        let _ = save();
    }
}

/// 非 Windows 平台的编译器候选路径（按优先级排序）。
fn compiler_candidates() -> Vec<&'static str> {
    if paths::is_windows() {
        return Vec::new();
    }
    if paths::is_macos() {
        vec![
            "/usr/bin/clang++",
            "/opt/homebrew/opt/llvm/bin/clang++",
            "/usr/local/opt/llvm/bin/clang++",
            "/usr/bin/g++",
            "/usr/local/bin/g++",
        ]
    } else {
        vec!["/usr/bin/g++", "/usr/bin/clang++", "/bin/g++"]
    }
}

/// compilerPath 为空时尝试探测并写入设置。返回是否有变更。
fn ensure_compiler_path() -> bool {
    if !get_str("compilerPath").trim().is_empty() {
        return false;
    }
    let Some(path) = compiler_candidates()
        .into_iter()
        .find(|candidate| PathBuf::from(candidate).is_file())
    else {
        return false;
    };
    set_value("compilerPath", Value::String(path.to_string()));
    crate::misc::log_line("info", &format!("[设置] 自动探测编译器: {path}"));
    true
}

pub fn save() -> Result<(), String> {
    let text = {
        let guard = SETTINGS.lock().unwrap();
        serde_json::to_string_pretty(&*guard).map_err(|e| e.to_string())?
    };
    std::fs::write(paths::settings_path(), text).map_err(|e| e.to_string())
}

pub fn set_startup_workspace(path: String) {
    *STARTUP_WORKSPACE.lock().unwrap() = Some(path);
}

// ------------------------------------------------------------------ 历史记录

fn update_recent_files(target: &str) {
    let mut resolved = target.to_string();
    if std::fs::metadata(&resolved).map(|m| m.is_file()).unwrap_or(false) {
        resolved = paths::dirname(&resolved);
    }
    let name = paths::basename(&resolved);
    update(|settings| {
        let list = settings
            .get_mut("recentFiles")
            .and_then(|v| v.as_array_mut());
        if let Some(list) = list {
            list.retain(|item| item.get("path").and_then(|p| p.as_str()) != Some(resolved.as_str()));
            list.insert(
                0,
                json!({ "path": resolved, "name": name, "lastAccessed": paths::iso_now() }),
            );
            list.truncate(10);
        }
    });
}

fn recent_files() -> Value {
    let mut list = SETTINGS
        .lock()
        .unwrap()
        .get("recentFiles")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let mut changed = false;
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for item in list.drain(..) {
        let mut path = item.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if path.is_empty() {
            changed = true;
            continue;
        }
        if !std::path::Path::new(&path).exists() {
            changed = true;
            continue;
        }
        if std::fs::metadata(&path).map(|m| m.is_file()).unwrap_or(false) {
            path = paths::dirname(&path);
            changed = true;
        }
        if seen.contains(&path) {
            changed = true;
            continue;
        }
        seen.push(path.clone());
        out.push(json!({
            "path": path,
            "name": paths::basename(&path),
            "lastAccessed": item.get("lastAccessed").cloned().unwrap_or(Value::String(paths::iso_now()))
        }));
    }
    if changed {
        set_value("recentFiles", Value::Array(out.clone()));
        let _ = save();
    }
    Value::Array(out)
}

fn file_history() -> Value {
    let list = SETTINGS
        .lock()
        .unwrap()
        .get("fileHistory")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut changed = false;
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for item in list {
        let path = item.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if path.is_empty() || !std::path::Path::new(&path).exists() {
            changed = true;
            continue;
        }
        let resolved = std::fs::canonicalize(&path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.clone());
        if seen.contains(&resolved) {
            changed = true;
            continue;
        }
        seen.push(resolved.clone());
        let name = paths::basename(&resolved);
        if item.get("name").and_then(|v| v.as_str()) != Some(name.as_str()) {
            changed = true;
        }
        out.push(json!({
            "path": resolved,
            "name": name,
            "lastOpened": item.get("lastOpened").cloned().unwrap_or(Value::String(paths::iso_now()))
        }));
    }
    if changed {
        set_value("fileHistory", Value::Array(out.clone()));
        let _ = save();
    }
    Value::Array(out)
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "get-settings" | "get-all-settings" | "get-top-level-settings" => ipc.ok(all()),
        "get-language" => {
            let language = get_str("language");
            ipc.ok(if language.is_empty() {
                Value::String("zh-cn".into())
            } else {
                Value::String(language)
            })
        }
        "update-settings" => {
            let incoming = ipc.object_arg(0);
            let theme = incoming
                .get("theme")
                .and_then(|v| v.as_str())
                .map(|v| v.to_string());
            update(|settings| {
                if let Some(map) = settings.as_object_mut() {
                    for (key, value) in incoming {
                        map.insert(key, value);
                    }
                }
            });
            if SETTINGS.lock().unwrap().get("runMode").is_some() {
                let mode = run_mode();
                set_value("runMode", Value::String(mode));
            }
            normalize_and_store();
            if let Err(err) = save() {
                return ipc.fail(err);
            }
            crate::cloud::schedule_auto_settings_backup(ipc.app.clone());
            crate::misc::apply_window_visuals(ipc.app);
            let snapshot = all();
            ipc.emit_one("settings-applied", snapshot);
            if let Some(theme) = theme {
                broadcast_to_settings_windows(ipc.app, "theme-changed", Value::String(theme));
            }
            Some(Ok(json!({ "success": true })))
        }
        "updateSettings" | "update-top-level-settings" | "update-editor-settings" => {
            let incoming = ipc.object_arg(0);
            apply_whitelisted_update(&incoming);
            let has_keybindings = incoming.contains_key("keybindings");
            let language = incoming
                .get("language")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let theme = incoming
                .get("theme")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Err(err) = save() {
                return ipc.fail(err);
            }
            crate::cloud::schedule_auto_settings_backup(ipc.app.clone());
            if has_keybindings {
                crate::menu::rebuild(ipc.app);
            }
            crate::misc::apply_window_visuals(ipc.app);
            let snapshot = all();
            ipc.emit("settings-changed", vec![Value::Null, snapshot.clone()]);
            if let Some(language) = language {
                broadcast_to_settings_windows(ipc.app, "language-changed", Value::String(language));
            }
            if let Some(theme) = theme {
                broadcast_to_settings_windows(ipc.app, "theme-changed", Value::String(theme));
            }
            Some(Ok(json!({ "success": true })))
        }
        "reset-settings" => {
            let defaults = default_settings();
            let mut current = all();
            if let (Some(current_map), Some(default_map)) = (current.as_object_mut(), defaults.as_object())
            {
                for (key, value) in default_map {
                    if NON_RESETTABLE_KEYS.contains(&key.as_str()) {
                        continue;
                    }
                    current_map.insert(key.clone(), value.clone());
                }
            }
            let normalized = normalize_runtime_shape(current);
            replace(normalized.clone());
            if let Err(err) = save() {
                return ipc.fail(err);
            }
            ipc.emit_one("settings-reset", normalized.clone());
            Some(Ok(json!({ "success": true, "settings": normalized })))
        }
        "export-settings" => {
            let picked = crate::dlg::pick_save(
                ipc.app,
                Some("导出设置"),
                Some("oicpp-settings.json"),
                vec![
                    ("JSON 文件".into(), vec!["json".into()]),
                    ("所有文件".into(), vec!["*".into()]),
                ],
            )
            .await;
            let Some(path) = picked else {
                return Some(Ok(json!({ "success": false, "message": "用户取消操作" })));
            };
            let text = match serde_json::to_string_pretty(&all()) {
                Ok(text) => text,
                Err(err) => return Some(Ok(json!({ "success": false, "error": err.to_string() }))),
            };
            match std::fs::write(&path, text) {
                Ok(_) => Some(Ok(json!({
                    "success": true,
                    "filePath": path.to_string_lossy()
                }))),
                Err(err) => Some(Ok(json!({ "success": false, "error": err.to_string() }))),
            }
        }
        "import-settings" => {
            let picked = crate::dlg::pick_open(
                ipc.app,
                crate::dlg::OpenOptions {
                    title: Some("导入设置".into()),
                    filters: vec![
                        ("JSON 文件".into(), vec!["json".into()]),
                        ("所有文件".into(), vec!["*".into()]),
                    ],
                    ..Default::default()
                },
            )
            .await;
            let Some(path) = picked.into_iter().next() else {
                return Some(Ok(json!({ "success": false, "message": "用户取消操作" })));
            };
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(err) => return Some(Ok(json!({ "success": false, "error": err.to_string() }))),
            };
            let parsed: Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(err) => return Some(Ok(json!({ "success": false, "error": err.to_string() }))),
            };
            let merged = merge_settings(&default_settings(), &parsed);
            let normalized = normalize_runtime_shape(merged);
            replace(normalized.clone());
            if let Err(err) = save() {
                return Some(Ok(json!({ "success": false, "error": err })));
            }
            ipc.emit_one("settings-imported", normalized.clone());
            Some(Ok(json!({ "success": true, "settings": normalized })))
        }
        "save-setting" => {
            let key = ipc.string_arg(0);
            let value = ipc.arg(1).cloned().unwrap_or(Value::Null);
            set_value(&key, value);
            match save() {
                Ok(_) => {
                    crate::cloud::schedule_auto_settings_backup(ipc.app.clone());
                    Some(Ok(json!({ "success": true })))
                }
                Err(err) => Some(Ok(json!({ "success": false, "error": err }))),
            }
        }
        "settings-preview" => {
            let preview = ipc.arg(0).cloned().unwrap_or(Value::Null);
            ipc.emit_one("apply-settings-preview", preview);
            Some(Ok(Value::Null))
        }
        "consume-startup-workspace-to-open" => {
            let mut guard = STARTUP_WORKSPACE.lock().unwrap();
            let value = guard.take();
            Some(Ok(value.map(Value::String).unwrap_or(Value::Null)))
        }
        "get-recent-files" => ipc.ok(recent_files()),
        "open-recent-file" => {
            let input = ipc.string_arg(0);
            let mut target = input.clone();
            if target.is_empty() || !std::path::Path::new(&target).exists() {
                return ipc.ok(false);
            }
            if std::fs::metadata(&target).map(|m| m.is_file()).unwrap_or(false) {
                target = paths::dirname(&target);
            }
            update_recent_files(&target);
            let _ = save();
            ipc.emit_one("folder-opened", Value::String(target));
            ipc.ok(true)
        }
        "add-to-file-history" => {
            let input = ipc.string_arg(0);
            if input.is_empty() || !std::path::Path::new(&input).exists() {
                return ipc.ok(false);
            }
            let resolved = std::fs::canonicalize(&input)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(input);
            let name = paths::basename(&resolved);
            update(|settings| {
                if let Some(list) = settings.get_mut("fileHistory").and_then(|v| v.as_array_mut()) {
                    list.retain(|item| item.get("path").and_then(|p| p.as_str()) != Some(resolved.as_str()));
                    list.insert(
                        0,
                        json!({ "path": resolved, "name": name, "lastOpened": paths::iso_now() }),
                    );
                    list.truncate(50);
                }
            });
            let _ = save();
            ipc.ok(true)
        }
        "get-file-history" => ipc.ok(file_history()),
        "open-file-from-history" => {
            let input = ipc.string_arg(0);
            if input.is_empty() || !std::path::Path::new(&input).exists() {
                return ipc.ok(false);
            }
            let resolved = std::fs::canonicalize(&input)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(input);
            let content = crate::fsops::read_text_file(&resolved).unwrap_or_default();
            let name = paths::basename(&resolved);
            ipc.emit_one(
                "file-opened",
                json!({ "filePath": resolved, "fileName": name, "content": content }),
            );
            update(|settings| {
                if let Some(list) = settings.get_mut("fileHistory").and_then(|v| v.as_array_mut()) {
                    for item in list.iter_mut() {
                        if item.get("path").and_then(|p| p.as_str()) == Some(resolved.as_str()) {
                            if let Some(map) = item.as_object_mut() {
                                map.insert("lastOpened".into(), Value::String(paths::iso_now()));
                            }
                        }
                    }
                }
            });
            let _ = save();
            ipc.ok(true)
        }
        "save-last-open-tabs" => {
            let Some(Value::Array(tabs)) = ipc.arg(0).cloned() else {
                return ipc.ok(false);
            };
            let mut filtered: Vec<Value> = tabs
                .into_iter()
                .filter(|tab| {
                    tab.get("filePath")
                        .and_then(|v| v.as_str())
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
                })
                .collect();
            filtered.truncate(20);
            set_value("lastOpenTabs", Value::Array(filtered));
            let _ = save();
            ipc.ok(true)
        }
        "get-last-open-tabs" => {
            let tabs = SETTINGS
                .lock()
                .unwrap()
                .get("lastOpenTabs")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            let filtered: Vec<Value> = tabs
                .into_iter()
                .filter(|tab| {
                    tab.get("filePath")
                        .and_then(|v| v.as_str())
                        .map(|p| std::path::Path::new(p).exists())
                        .unwrap_or(false)
                })
                .collect();
            if filtered.len()
                != SETTINGS
                    .lock()
                    .unwrap()
                    .get("lastOpenTabs")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
            {
                set_value("lastOpenTabs", Value::Array(filtered.clone()));
                let _ = save();
            }
            ipc.ok(Value::Array(filtered))
        }
        "clear-file-history" => {
            set_value("fileHistory", Value::Array(vec![]));
            let _ = save();
            ipc.ok(true)
        }
        _ => None,
    }
}

fn normalize_and_store() {
    let current = all();
    replace(normalize_runtime_shape(current));
}

fn apply_whitelisted_update(incoming: &Map<String, Value>) {
    update(|settings| {
        if let Some(map) = settings.as_object_mut() {
            for (key, value) in incoming {
                if !VALID_KEYS.contains(&key.as_str()) {
                    continue;
                }
                if key == "runMode" {
                    let mode = value.as_str().unwrap_or("");
                    map.insert(key.clone(), Value::String(normalize_run_mode(mode)));
                    continue;
                }
                map.insert(key.clone(), value.clone());
            }
        }
    });
    normalize_and_store();
}

/// 语言 / 主题变化只广播给设置窗口（与 Electron 版一致）
fn broadcast_to_settings_windows(app: &tauri::AppHandle, channel: &str, payload: Value) {
    use tauri::{Emitter, Manager};

    for label in [
        "compiler-settings",
        "editor-settings",
        "template-settings",
        "backup-settings",
    ] {
        if let Some(window) = app.get_webview_window(label) {
            let _ = window.emit(
                crate::events::EVENT_NAME,
                json!({ "channel": channel, "args": [payload.clone()] }),
            );
        }
    }
}
