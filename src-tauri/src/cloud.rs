//! 账号登录、云同步、日志上传等通道（对应 Electron 版 main.js 的认证与云 API 实现）。

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tauri_plugin_opener::OpenerExt;

use crate::codec;
use crate::ipc::Ipc;
use crate::paths;
use crate::settings;

type HmacSha256 = Hmac<Sha256>;

const AUTH_BASE: &str = "https://auth.mywwzh.top";
const AUTH_LOGIN_PATH: &str = "/oicpp_ide_login";
const AUTH_VERIFY_PATH: &str = "/api/verify_token";
const AUTH_SERVICE: &str = "oicpp-ide";
const IDE_LOGIN_TIMEOUT_MS: u64 = 300_000;

const CLOUD_API_BASE: &str = "https://oicpp.mywwzh.top/api";
const CLIENT_LOG_UPLOAD_URL: &str = "https://oicpp.mywwzh.top/api/uploadClientLog";

static DEVICE_INFO: Lazy<Mutex<Option<(String, String)>>> = Lazy::new(|| Mutex::new(None));
static LOGIN_BUSY: Lazy<Mutex<bool>> = Lazy::new(|| Mutex::new(false));
static AUTO_BACKUP_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static LAST_AUTO_BACKUP: Lazy<Mutex<Option<Instant>>> = Lazy::new(|| Mutex::new(None));
static HEARTBEAT_TASK: Lazy<Mutex<Option<tauri::async_runtime::JoinHandle<()>>>> =
    Lazy::new(|| Mutex::new(None));

// ------------------------------------------------------------------ 工具

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex(&hasher.finalize())
}

fn hmac_sha256_hex(secret: &str, message: &str) -> String {
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return String::new(),
    };
    mac.update(message.as_bytes());
    hex(&mac.finalize().into_bytes())
}

fn random_nonce() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..16).map(|_| rng.gen::<u8>()).collect();
    hex(&bytes)
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap_or_default()
}

fn device_info() -> (String, String) {
    if let Ok(guard) = DEVICE_INFO.lock() {
        if let Some(info) = guard.clone() {
            return info;
        }
    }
    let device_name = std::env::var("COMPUTERNAME")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "Unknown-Device".to_string());
    let cpu_id = std::env::var("PROCESSOR_IDENTIFIER")
        .ok()
        .map(|model| model.split_whitespace().collect::<String>().chars().take(50).collect::<String>())
        .filter(|v| !v.is_empty())
        .map(|model| format!("CPU-{model}"))
        .unwrap_or_else(|| "CPU-Unknown".to_string());
    let info = (device_name, cpu_id);
    if let Ok(mut guard) = DEVICE_INFO.lock() {
        *guard = Some(info.clone());
    }
    info
}

fn system_tag() -> &'static str {
    if paths::is_windows() {
        "win"
    } else if paths::is_macos() {
        "mac"
    } else {
        "linux"
    }
}

fn encoded_token(username: &str) -> String {
    let (device_name, cpu_id) = device_info();
    let raw = format!("{username}&{device_name}&{cpu_id}&{}", system_tag());
    codec::b64_encode(raw.as_bytes())
}

fn login_token() -> String {
    settings::account()
        .and_then(|account| {
            account
                .get("loginToken")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_default()
}

fn username() -> String {
    settings::account()
        .and_then(|account| {
            account
                .get("user")
                .and_then(|user| user.get("username").or_else(|| user.get("name")))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_default()
}

fn broadcast_one(app: &tauri::AppHandle, channel: &str, payload: Value) {
    crate::events::broadcast_one(app, channel, payload);
}

fn broadcast_login_state(app: &tauri::AppHandle, payload: Value) {
    broadcast_one(app, "ide-login-updated", payload);
}

fn broadcast_login_error(app: &tauri::AppHandle, message: &str) {
    broadcast_one(app, "ide-login-error", json!({ "message": message }));
}

// ------------------------------------------------------------------ 云 API 调用

async fn cloud_api(
    method: &str,
    path: &str,
    query: Option<&Map<String, Value>>,
    body: Option<&Value>,
) -> Value {
    let token = login_token();
    if token.is_empty() {
        return json!({ "ok": false, "status": 401, "error": "未登录" });
    }

    let path_part = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    let upper = method.to_uppercase();
    let signed_path = format!("/api{path_part}");

    let body_text = if upper == "GET" || upper == "HEAD" {
        String::new()
    } else {
        match body {
            Some(Value::Object(_)) => serde_json::to_string(body.unwrap()).unwrap_or_default(),
            Some(Value::Array(_)) | Some(Value::String(_)) | Some(Value::Number(_))
            | Some(Value::Bool(_)) => serde_json::to_string(&json!({ "data": body.unwrap() }))
                .unwrap_or_default(),
            _ => "{}".to_string(),
        }
    };

    let timestamp = paths::now_millis().to_string();
    let nonce = random_nonce();
    // 注意：body 为空（GET/HEAD）时，原实现用**空字符串**参与签名，而不是
    // sha256("")。若这里算成 sha256("")，每个 GET 请求（如 /cloudSync/list）
    // 的签名都会对不上，服务端返回 401，界面表现为「登录已过期」。
    let body_hash = if body_text.is_empty() {
        String::new()
    } else {
        sha256_hex(&body_text)
    };
    let signing = format!("{upper}\n{signed_path}\n{timestamp}\n{nonce}\n{body_hash}");
    let signature = hmac_sha256_hex(&token, &signing);

    let mut url = match url::Url::parse(&format!("{CLOUD_API_BASE}{path_part}")) {
        Ok(url) => url,
        Err(err) => return json!({ "ok": false, "status": 0, "error": err.to_string() }),
    };
    if let Some(query) = query {
        let sorted: BTreeMap<String, String> = query
            .iter()
            .map(|(key, value)| {
                let text = match value {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                (key.clone(), text)
            })
            .collect();
        let mut pairs = url.query_pairs_mut();
        for (key, value) in sorted {
            pairs.append_pair(&key, &value);
        }
    }

    let client = http_client();
    let mut request = client
        .request(
            reqwest::Method::from_bytes(upper.as_bytes()).unwrap_or(reqwest::Method::GET),
            url,
        )
        .header("Authorization", format!("Bearer {token}"))
        .header("X-OICPP-Token", encoded_token(&username()))
        .header("X-OICPP-Timestamp", &timestamp)
        .header("X-OICPP-Nonce", &nonce)
        .header("X-OICPP-Signature", &signature);
    if !body_text.is_empty() {
        request = request
            .header("Content-Type", "application/json")
            .body(body_text);
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status().as_u16();
            let ok = response.status().is_success();
            let text = response.text().await.unwrap_or_default();
            let data = serde_json::from_str::<Value>(&text)
                .unwrap_or_else(|_| Value::String(text));
            json!({ "ok": ok, "status": status, "data": data })
        }
        Err(err) => json!({ "ok": false, "status": 0, "error": err.to_string() }),
    }
}

/// 云 API 返回解析（对应 main.js 的 callCloudSyncApi 结果）
async fn require_cloud_data(
    method: &str,
    path: &str,
    query: Option<&Map<String, Value>>,
    body: Option<&Value>,
) -> Result<Value, String> {
    let result = cloud_api(method, path, query, body).await;
    if result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        Ok(result.get("data").cloned().unwrap_or(Value::Null))
    } else if result.get("status").and_then(|v| v.as_u64()) == Some(401) {
        Err("NOT_LOGGED_IN".to_string())
    } else {
        Err(result
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("REQUEST_FAILED")
            .to_string())
    }
}

fn sanitize_device_name(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn cloud_list_items(data: &Value) -> Vec<Value> {
    data.get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn settings_backup_pattern() -> Option<regex::Regex> {
    regex::Regex::new(r"^OICPP_user_(\d{8}_\d{6})_(.+?)_settings\.cpp$").ok()
}

fn is_settings_backup(name: &str) -> bool {
    settings_backup_pattern()
        .map(|re| re.is_match(name))
        .unwrap_or(false)
}

/// 取最新一份设置备份信息
async fn latest_backup_info() -> Result<Value, String> {
    let data = require_cloud_data("GET", "/cloudSync/list", None, None).await?;
    let items = cloud_list_items(&data);
    let mut candidates: Vec<(String, Value)> = Vec::new();
    for item in items {
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if !is_settings_backup(name) {
            continue;
        }
        let timestamp = item
            .get("timestampRaw")
            .or_else(|| item.get("updatedAt"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        candidates.push((timestamp, item));
    }
    if candidates.is_empty() {
        return Err("NO_BACKUP".to_string());
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let (timestamp_raw, item) = candidates.remove(0);
    let name = item
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let display_time = regex::Regex::new(r"^OICPP_user_(\d{8}_\d{6})_")
        .ok()
        .and_then(|re| re.captures(&name))
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_else(|| timestamp_raw.clone());
    let device_name = regex::Regex::new(r"^OICPP_user_\d{8}_\d{6}_(.+?)_settings\.cpp$")
        .ok()
        .and_then(|re| re.captures(&name))
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_default();
    let path = item
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(&name)
        .to_string();
    Ok(json!({
        "fileName": name,
        "path": path,
        "timestampRaw": timestamp_raw,
        "displayTime": display_time,
        "deviceName": device_name
    }))
}

// ------------------------------------------------------------------ 自动备份 / 心跳

/// 把当前 settings.json 备份到云空间（不含账号信息）。
pub async fn backup_settings_to_cloud() -> Value {
    if login_token().is_empty() {
        return json!({ "success": false, "error": "NOT_LOGGED_IN" });
    }
    let text = match std::fs::read_to_string(paths::settings_path()) {
        Ok(text) => text,
        Err(_) => return json!({ "success": false, "error": "NO_SETTINGS" }),
    };
    let mut payload: Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return json!({ "success": false, "error": "NO_SETTINGS" }),
    };
    if let Some(map) = payload.as_object_mut() {
        map.remove("account");
    }

    let (device_name, _) = device_info();
    let sanitized = sanitize_device_name(&device_name);
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let file_name = format!("OICPP_user_{stamp}_{sanitized}_settings.cpp");

    // 先清理旧备份
    if let Ok(data) = require_cloud_data("GET", "/cloudSync/list", None, None).await {
        for item in cloud_list_items(&data) {
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !is_settings_backup(name) {
                continue;
            }
            let path = item
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(name)
                .to_string();
            let _ = require_cloud_data(
                "POST",
                "/cloudSync/delete",
                None,
                Some(&json!({ "path": path })),
            )
            .await;
        }
    }

    let body = json!({
        "path": file_name,
        "content": serde_json::to_string(&payload).unwrap_or_default()
    });
    match require_cloud_data("POST", "/cloudSync/upload", None, Some(&body)).await {
        Ok(_) => json!({
            "success": true,
            "info": {
                "fileName": file_name,
                "path": file_name,
                "timestampRaw": stamp,
                "displayTime": stamp,
                "deviceName": device_name
            }
        }),
        Err(err) => json!({ "success": false, "error": err }),
    }
}

/// 设置变更后触发自动备份。15 秒冷却，避免连续写入设置时反复上传。
pub fn schedule_auto_settings_backup(_app: tauri::AppHandle) {
    if !settings::get_bool("autoBackupSettings", false) || login_token().is_empty() {
        return;
    }
    if AUTO_BACKUP_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Ok(guard) = LAST_AUTO_BACKUP.lock() {
        if let Some(last) = *guard {
            if last.elapsed() < Duration::from_secs(15) {
                AUTO_BACKUP_IN_FLIGHT.store(false, Ordering::SeqCst);
                return;
            }
        }
    }

    tauri::async_runtime::spawn(async move {
        let result = backup_settings_to_cloud().await;
        let success = result
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if success {
            if let Ok(mut guard) = LAST_AUTO_BACKUP.lock() {
                *guard = Some(Instant::now());
            }
            crate::misc::log_line("info", "[设置备份] 自动备份成功");
        } else {
            let error = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN");
            if error != "NOT_LOGGED_IN" {
                crate::misc::log_line(
                    "warn",
                    &format!("[设置备份] 自动备份失败: {error}"),
                );
            }
        }
        AUTO_BACKUP_IN_FLIGHT.store(false, Ordering::SeqCst);
    });
}

async fn send_heartbeat(kind: &str) {
    let logged_in_user = username();
    let token = encoded_token(&logged_in_user);
    let (device_name, cpu_id) = device_info();
    let login_token = login_token();
    let mut data = json!({
        "type": kind,
        "token": token,
        "version": env!("CARGO_PKG_VERSION")
    });
    if let Some(map) = data.as_object_mut() {
        if !login_token.is_empty() {
            map.insert("login_token".into(), Value::String(login_token));
        }
        if kind == "start" {
            map.insert("username".into(), Value::String(logged_in_user));
            map.insert("device_name".into(), Value::String(device_name));
            map.insert("cpu_id".into(), Value::String(cpu_id));
        }
    }

    let _ = http_client()
        .post(format!("{CLOUD_API_BASE}/heartbeat"))
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(10))
        .json(&data)
        .send()
        .await;
}

/// 登录后启动心跳：立即 start，之后每 30 分钟一次。
pub fn start_heartbeat(app: &tauri::AppHandle) {
    stop_heartbeat();
    if username().is_empty() && login_token().is_empty() {
        return;
    }
    let _ = app;
    let task = tauri::async_runtime::spawn(async move {
        send_heartbeat("start").await;
        loop {
            tokio::time::sleep(Duration::from_secs(30 * 60)).await;
            send_heartbeat("heartbeat").await;
        }
    });
    if let Ok(mut guard) = HEARTBEAT_TASK.lock() {
        *guard = Some(task);
    }
}

pub fn stop_heartbeat() {
    if let Ok(mut guard) = HEARTBEAT_TASK.lock() {
        if let Some(task) = guard.take() {
            task.abort();
        }
    }
}

// ------------------------------------------------------------------ 登录流程

async fn start_login(app: &tauri::AppHandle) -> Value {
    {
        let mut busy = match LOGIN_BUSY.lock() {
            Ok(guard) => guard,
            Err(_) => return json!({ "ok": false, "message": "登录流程正在进行，请稍候完成。" }),
        };
        if *busy {
            return json!({ "ok": false, "message": "登录流程正在进行，请稍候完成。" });
        }
        *busy = true;
    }

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(_) => {
            if let Ok(mut busy) = LOGIN_BUSY.lock() {
                *busy = false;
            }
            broadcast_login_error(app, "本地回调服务启动失败");
            return json!({ "ok": false, "message": "本地回调服务启动失败" });
        }
    };
    let port = listener.local_addr().map(|addr| addr.port()).unwrap_or(0);
    let state = random_nonce();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let login_url = format!(
        "{AUTH_BASE}{AUTH_LOGIN_PATH}?redirect={}&service={AUTH_SERVICE}&state={state}",
        url_encode(&redirect)
    );

    if app.opener().open_url(login_url, None::<String>).is_err() {
        if let Ok(mut busy) = LOGIN_BUSY.lock() {
            *busy = false;
        }
        broadcast_login_error(app, "打开浏览器失败");
        return json!({ "ok": false, "message": "打开浏览器失败" });
    }

    let handle = app.clone();
    tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(IDE_LOGIN_TIMEOUT_MS);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                broadcast_login_error(&handle, "登录超时，请重试");
                break;
            }
            let accepted = tokio::time::timeout(remaining, listener.accept()).await;
            let Ok(Ok((mut stream, _addr))) = accepted else {
                continue;
            };

            let target = read_request_target(&mut stream).await;
            let Some(target) = target else {
                continue;
            };
            let parsed = url::Url::parse(&format!("http://127.0.0.1{target}")).ok();
            let Some(parsed) = parsed else {
                continue;
            };
            if parsed.path() != "/callback" {
                continue;
            }
            let params: Map<String, Value> = parsed
                .query_pairs()
                .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
                .collect();
            let get = |key: &str| {
                params
                    .get(key)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };

            if get("state") != state {
                respond_html(&mut stream, "登录失败：状态校验失败").await;
                broadcast_login_error(&handle, "状态校验失败");
                continue;
            }
            if get("uid").is_empty()
                || get("username").is_empty()
                || get("timestamp").is_empty()
                || get("token").is_empty()
            {
                respond_html(&mut stream, "登录失败：回调参数不完整").await;
                broadcast_login_error(&handle, "回调参数不完整");
                continue;
            }
            if get("service") != AUTH_SERVICE {
                respond_html(&mut stream, "登录失败：服务校验失败").await;
                broadcast_login_error(&handle, "状态校验失败");
                continue;
            }

            let verify_body = json!({
                "uid": get("uid"),
                "username": get("username"),
                "timestamp": get("timestamp"),
                "token": get("token"),
                "service": get("service")
            });
            let verify = http_client()
                .post(format!("{AUTH_BASE}{AUTH_VERIFY_PATH}"))
                .json(&verify_body)
                .send()
                .await;
            let verified = match verify {
                Ok(response) => response
                    .json::<Value>()
                    .await
                    .unwrap_or(Value::Null),
                Err(_) => {
                    respond_html(&mut stream, "登录失败：验证请求失败").await;
                    broadcast_login_error(&handle, "验证请求失败");
                    continue;
                }
            };

            if verified.get("success").and_then(|v| v.as_bool()) != Some(true)
                || verified.get("user").is_none()
            {
                let message = verified
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("验证失败")
                    .to_string();
                respond_html(&mut stream, &format!("登录失败：{message}")).await;
                broadcast_login_error(&handle, &message);
                continue;
            }

            let access_token = get("access_token");
            let login_token = if !access_token.is_empty() {
                access_token
            } else {
                get("login_token")
            };
            let login_token = if login_token.is_empty() {
                get("token")
            } else {
                login_token
            };
            if login_token.is_empty() {
                respond_html(&mut stream, "登录失败：未获取到登录凭证").await;
                broadcast_login_error(&handle, "未获取到登录凭证");
                continue;
            }

            let user = verified.get("user").cloned().unwrap_or(Value::Null);
            // 与原实现一致：loggedInAt 为毫秒时间戳（渲染层会拿它做时间计算）
            let logged_in_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            settings::set_value(
                "account",
                json!({
                    "user": user,
                    "loginToken": login_token,
                    "loggedInAt": logged_in_at
                }),
            );
            let _ = settings::save();
            start_heartbeat(&handle);
            respond_html(&mut stream, "登录成功，请返回 OICPP IDE").await;
            broadcast_login_state(
                &handle,
                json!({ "loggedIn": true, "user": user, "message": "登录成功" }),
            );
            break;
        }
        if let Ok(mut busy) = LOGIN_BUSY.lock() {
            *busy = false;
        }
    });

    json!({ "ok": true })
}

fn url_encode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

async fn read_request_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut buffer = vec![0u8; 4096];
    let read = stream.read(&mut buffer).await.ok()?;
    let text = String::from_utf8_lossy(&buffer[..read]).to_string();
    let first_line = text.lines().next()?;
    let mut parts = first_line.split_whitespace();
    let _method = parts.next()?;
    let target = parts.next()?;
    Some(target.to_string())
}

async fn respond_html(stream: &mut tokio::net::TcpStream, message: &str) {
    use tokio::io::AsyncWriteExt;
    let body = format!(
        "<!DOCTYPE html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\"><title>OICPP IDE</title></head><body style=\"font-family:sans-serif;padding:40px\"><h2>{message}</h2></body></html>"
    );
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.as_bytes().len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

// ------------------------------------------------------------------ 日志目录

fn list_logs() -> Value {
    let dir = paths::logs_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return json!({ "success": true, "logs": [] });
    };
    let mut logs: Vec<Value> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_log = path
            .extension()
            .map(|ext| ext.eq_ignore_ascii_case("log"))
            .unwrap_or(false);
        if !is_log {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime_ms = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0);
        logs.push(json!({
            "name": path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            "path": path.to_string_lossy(),
            "size": meta.len(),
            "mtimeMs": mtime_ms
        }));
    }
    logs.sort_by(|a, b| {
        let am = a.get("mtimeMs").and_then(|v| v.as_i64()).unwrap_or(0);
        let bm = b.get("mtimeMs").and_then(|v| v.as_i64()).unwrap_or(0);
        bm.cmp(&am)
    });
    json!({ "success": true, "logs": logs })
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "ide-login-start" => Some(Ok(start_login(ipc.app).await)),
        "ide-login-status" => {
            let account = settings::account();
            let user = account
                .as_ref()
                .and_then(|a| a.get("user").cloned())
                .unwrap_or(Value::Null);
            let token = account
                .as_ref()
                .and_then(|a| a.get("loginToken").and_then(|v| v.as_str()).map(String::from))
                .unwrap_or_default();
            let logged_in = !user.is_null() && !token.is_empty();
            Some(Ok(json!({
                "loggedIn": logged_in,
                "user": user,
                "loginToken": token
            })))
        }
        "ide-logout" => {
            settings::set_value("account", Value::Null);
            let _ = settings::save();
            stop_heartbeat();
            broadcast_login_state(
                ipc.app,
                json!({ "loggedIn": false, "user": Value::Null, "message": "已退出登录" }),
            );
            Some(Ok(json!({ "ok": true })))
        }
        "cloud-sync-request" => {
            let payload = ipc.object_arg(0);
            let method = payload
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_string();
            let path = payload
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("/cloudSync/list")
                .to_string();
            let query = payload.get("query").and_then(|v| v.as_object()).cloned();
            let body = if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD") {
                None
            } else {
                payload.get("body").cloned()
            };
            Some(Ok(
                cloud_api(&method, &path, query.as_ref(), body.as_ref()).await,
            ))
        }
        "backup-settings-to-cloud" => Some(Ok(backup_settings_to_cloud().await)),
        "get-settings-backup-info" => match latest_backup_info().await {
            Ok(info) => Some(Ok(json!({ "success": true, "info": info }))),
            Err(err) => Some(Ok(json!({ "success": false, "error": err }))),
        },
        "sync-settings-from-cloud" => {
            let info = match latest_backup_info().await {
                Ok(info) => info,
                Err(err) => return Some(Ok(json!({ "success": false, "error": err }))),
            };
            let mut query = Map::new();
            query.insert(
                "path".to_string(),
                info.get("path").cloned().unwrap_or(Value::Null),
            );
            let data = match require_cloud_data("GET", "/cloudSync/download", Some(&query), None).await
            {
                Ok(data) => data,
                Err(err) => return Some(Ok(json!({ "success": false, "error": err }))),
            };
            let content = data
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if content.is_empty() {
                return Some(Ok(json!({ "success": false, "error": "EMPTY_BACKUP" })));
            }
            let parsed: Value = match serde_json::from_str(&content) {
                Ok(value) => value,
                Err(_) => return Some(Ok(json!({ "success": false, "error": "INVALID_BACKUP" }))),
            };
            let local_account = settings::get_value("account").unwrap_or(Value::Null);
            let mut merged = settings::merge_settings(&settings::default_settings(), &parsed);
            if let Some(map) = merged.as_object_mut() {
                map.insert("account".to_string(), local_account);
            }
            settings::replace(merged.clone());
            let _ = settings::save();
            crate::events::broadcast_one(ipc.app, "settings-imported", merged.clone());
            crate::events::broadcast_one(ipc.app, "settings-applied", merged);
            Some(Ok(json!({ "success": true, "info": info })))
        }
        // 上游 Electron 也没有实现反馈上传接口；当前 UI 直接打开 GitHub Issues，
        // 这里保留明确的“未开放”响应，避免调用方误以为已发送。
        "send-feedback" => Some(Ok(json!({
            "success": false,
            "message": "反馈通道当前版本暂未开放"
        }))),
        "list-client-logs" => Some(Ok(list_logs())),
        "upload-client-log" => {
            let file_path = ipc.string_arg(0);
            if file_path.trim().is_empty() {
                return Some(Ok(json!({ "success": false, "message": "日志路径无效" })));
            }
            if paths::extname(&file_path) != ".log" {
                return Some(Ok(json!({ "success": false, "message": "仅支持上传 .log 文件" })));
            }
            let bytes = match std::fs::read(&file_path) {
                Ok(bytes) => bytes,
                Err(_) => {
                    return Some(Ok(json!({
                        "success": false,
                        "message": "日志文件不存在或不可读取"
                    })))
                }
            };
            if bytes.is_empty() {
                return Some(Ok(json!({
                    "success": false,
                    "message": "请选择有效的日志文件"
                })));
            }
            if bytes.len() > 5 * 1024 * 1024 {
                return Some(Ok(json!({
                    "success": false,
                    "message": "日志文件超过 5MB 限制"
                })));
            }

            let (device_name, cpu_id) = device_info();
            let app_version = crate::misc::build_info(ipc.app)
                .get("version")
                .and_then(|v| v.as_str())
                .unwrap_or("1.5.4")
                .to_string();
            let file_name = paths::basename(&file_path);
            let part = reqwest::multipart::Part::bytes(bytes)
                .file_name(file_name)
                .mime_str("text/plain")
                .unwrap_or_else(|_| reqwest::multipart::Part::bytes(Vec::new()));
            let mut form = reqwest::multipart::Form::new()
                .part("file", part)
                .text("version", app_version)
                .text("system", format!("{} {}", paths::platform_name(), std::env::consts::OS))
                .text("device_name", device_name)
                .text("cpu_id", cpu_id);
            let token = login_token();
            if !token.is_empty() {
                form = form
                    .text("username", username())
                    .text("login_token", token.clone());
            }

            let mut request = http_client()
                .post(CLIENT_LOG_UPLOAD_URL)
                .header("X-OICPP-Token", encoded_token(&username()))
                .multipart(form);
            if !token.is_empty() {
                request = request
                    .header("Authorization", format!("Bearer {token}"))
                    .header("X-Login-Token", token);
            }

            match request.send().await {
                Ok(response) => {
                    let payload: Value = response.json().await.unwrap_or(Value::Null);
                    let trace_code = payload
                        .get("trace_code")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if trace_code.is_empty() {
                        return Some(Ok(json!({
                            "success": false,
                            "message": "上传成功但未返回追踪码，请稍后重试"
                        })));
                    }
                    let uploaded_at = payload
                        .get("uploaded_at")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let message = payload
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("上传成功")
                        .to_string();
                    Some(Ok(json!({
                        "success": true,
                        "traceCode": trace_code,
                        "uploadedAt": uploaded_at,
                        "message": message
                    })))
                }
                Err(err) => Some(Ok(json!({ "success": false, "message": err.to_string() }))),
            }
        }
        "get-device-info" => {
            let (device_name, cpu_id) = device_info();
            Some(Ok(json!({ "deviceName": device_name, "cpuId": cpu_id })))
        }
        "get-encoded-token" => Some(Ok(Value::String(encoded_token(&username())))),
        _ => None,
    }
}
