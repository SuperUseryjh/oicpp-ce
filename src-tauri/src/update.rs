//! 更新检查 / 后台下载 / 退出时安装。
//!
//! 完全复刻 Electron 版 `checkForUpdates` 流程：走 `oicpp.mywwzh.top/api/checkUpdate`
//! 与 `/api/getUpdateFilelist`，把安装包下载到 `~/.oicpp/`，并写入
//! `settings.pendingUpdate`（Windows 会在退出时静默拉起安装程序）。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use tauri::AppHandle;

use crate::events;
use crate::ipc::Ipc;
use crate::paths;
use crate::settings;

const UPDATE_API_BASE: &str = "https://oicpp.mywwzh.top/api";

#[derive(Debug, Default, Clone)]
struct UpdateState {
    auto_checking: bool,
    downloading: bool,
    version: String,
    progress: i64,
}

static STATE: Lazy<Mutex<UpdateState>> = Lazy::new(|| Mutex::new(UpdateState::default()));

// ------------------------------------------------------------------ 状态

fn pending_update() -> Option<Value> {
    settings::get_value("pendingUpdate").filter(|value| !value.is_null())
}

fn pending_install() -> bool {
    pending_update().is_some()
}

fn pending_version() -> String {
    pending_update()
        .and_then(|value| {
            value
                .get("version")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .unwrap_or_default()
}

pub fn state_payload() -> Value {
    let state = STATE.lock().map(|guard| guard.clone()).unwrap_or_default();
    json!({
        "autoChecking": state.auto_checking,
        "downloading": state.downloading,
        "version": state.version,
        "progress": state.progress,
        "pendingInstall": pending_install(),
        "pendingVersion": pending_version()
    })
}

fn broadcast_state(app: &AppHandle) {
    events::broadcast_one(app, "update-download-status", state_payload());
}

fn mutate_state<F: FnOnce(&mut UpdateState)>(mutator: F) {
    if let Ok(mut guard) = STATE.lock() {
        mutator(&mut guard);
    }
}

fn toast(app: &AppHandle, level: &str, message: String) {
    events::broadcast_one(
        app,
        "app-toast",
        json!({ "type": level, "message": message }),
    );
}

// ------------------------------------------------------------------ 版本比较

/// 解析形如 `1.5.4` / `v1.5.4-beta2` 的版本号
fn parse_version(input: &str) -> (Vec<i64>, bool) {
    let trimmed = input.trim().trim_start_matches(['v', 'V']);
    let (main, pre) = match trimmed.split_once('-') {
        Some((main, pre)) => (main, pre),
        None => (trimmed, ""),
    };
    let numbers = main
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<i64>()
                .unwrap_or(0)
        })
        .collect::<Vec<i64>>();
    (numbers, !pre.is_empty())
}

/// 远端版本是否比当前版本更新（`allow_beta` 为 false 时忽略预发布版本）
pub fn has_update(current: &str, latest: &str, allow_beta: bool) -> bool {
    let (latest_numbers, latest_is_pre) = parse_version(latest);
    if latest_is_pre && !allow_beta {
        return false;
    }
    let (current_numbers, _) = parse_version(current);
    let length = latest_numbers.len().max(current_numbers.len());
    for index in 0..length {
        let left = *latest_numbers.get(index).unwrap_or(&0);
        let right = *current_numbers.get(index).unwrap_or(&0);
        if left != right {
            return left > right;
        }
    }
    false
}

// ------------------------------------------------------------------ 网络

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Value, String> {
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP error! status: {}", response.status().as_u16()));
    }
    response.json::<Value>().await.map_err(|e| e.to_string())
}

fn system_candidates() -> Vec<&'static str> {
    if paths::is_windows() {
        vec!["win"]
    } else if paths::is_macos() {
        vec!["mac", "macos", "darwin"]
    } else {
        vec!["linux"]
    }
}

fn pick_installer(files: &[Value]) -> Option<Value> {
    let matches = |file: &Value, suffix: &str| {
        file.get("name")
            .and_then(|v| v.as_str())
            .map(|name| name.to_lowercase().ends_with(suffix))
            .unwrap_or(false)
    };
    let candidates: Vec<&str> = if paths::is_windows() {
        vec![".exe"]
    } else if paths::is_linux() {
        vec![".deb", ".rpm"]
    } else {
        vec![".dmg", ".pkg", ".zip"]
    };
    for suffix in candidates {
        if let Some(file) = files.iter().find(|file| matches(file, suffix)) {
            if file
                .get("downloadUrl")
                .and_then(|v| v.as_str())
                .map(|url| !url.is_empty())
                .unwrap_or(false)
            {
                return Some(file.clone());
            }
        }
    }
    None
}

/// 清理 `~/.oicpp` 下的旧安装包，保留正在等待安装的那一个。
fn cleanup_old_installers(keep: Option<&str>) {
    let dir = paths::oicpp_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(keep) = keep {
            if path.to_string_lossy() == keep {
                continue;
            }
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let lower = name.to_lowercase();
        let is_installer = (lower.starts_with("oicpp") && lower.contains("setup") && lower.ends_with(".exe"))
            || (lower.starts_with("oicpp") && (lower.ends_with(".deb") || lower.ends_with(".rpm") || lower.ends_with(".dmg") || lower.ends_with(".pkg")));
        if is_installer {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn pending_installer_path() -> Option<String> {
    pending_update()
        .and_then(|value| value.get("installerPath").and_then(|v| v.as_str()).map(String::from))
}

async fn download_installer(app: &AppHandle, url: &str, dest: &Path, version: &str) -> Result<(), String> {
    let client = http_client()?;
    let response = client.get(url).send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP error! status: {}", response.status().as_u16()));
    }
    let total = response.content_length().unwrap_or(0);

    mutate_state(|state| {
        state.downloading = true;
        state.version = version.to_string();
        state.progress = 0;
    });
    broadcast_state(app);

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_reported: i64 = -1;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .map_err(|e| e.to_string())?;
        downloaded += chunk.len() as u64;
        if total > 0 {
            let progress = ((downloaded.min(total) * 100) / total) as i64;
            if progress != last_reported {
                last_reported = progress;
                mutate_state(|state| state.progress = progress);
                broadcast_state(app);
            }
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file)
        .await
        .map_err(|e| e.to_string())?;

    mutate_state(|state| {
        state.downloading = false;
        state.version = String::new();
        state.progress = 0;
    });
    broadcast_state(app);
    Ok(())
}

// ------------------------------------------------------------------ 主流程

pub async fn check_updates(app: &AppHandle, is_manual: bool) {
    if prune_stale_pending_update() {
        broadcast_state(app);
    }
    if pending_install() {
        if is_manual {
            crate::dlg::message(
                app,
                "检查更新",
                "已有更新等待安装\n\n请先退出 OICPP 完成当前更新安装，安装完成后再检查更新。",
            )
            .await;
        }
        return;
    }
    {
        let state = STATE.lock().map(|guard| guard.clone()).unwrap_or_default();
        if state.downloading {
            if is_manual {
                crate::dlg::message(
                    app,
                    "检查更新",
                    &format!(
                        "更新下载进行中\n\n当前正在后台下载更新 ({}), 进度 {}%。",
                        state.version, state.progress
                    ),
                )
                .await;
            }
            return;
        }
    }

    let client = match http_client() {
        Ok(client) => client,
        Err(err) => {
            if is_manual {
                crate::dlg::message(app, "更新失败", &err).await;
            }
            return;
        }
    };

    let info = match fetch_json(&client, &format!("{UPDATE_API_BASE}/checkUpdate")).await {
        Ok(info) => info,
        Err(err) => {
            if is_manual {
                crate::dlg::message(app, "更新失败", &format!("获取更新信息失败: {err}")).await;
            }
            return;
        }
    };

    settings::set_value("lastUpdateCheck", Value::String(paths::iso_now()));
    let _ = settings::save();

    let current = crate::misc::build_info(app)
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("1.5.4")
        .to_string();
    let latest = info
        .get("latestVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let description = info
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if latest.is_empty() {
        if is_manual {
            crate::dlg::message(app, "检查更新", "更新服务未返回有效版本号。").await;
        }
        return;
    }

    let allow_beta = settings::get_bool("receiveBetaUpdates", false);
    if !has_update(&current, &latest, allow_beta) {
        if is_manual {
            crate::dlg::message(app, "检查更新", &format!("当前已是最新版本 (v{current})。")).await;
        }
        return;
    }

    if is_manual {
        let formatted = description.replace("\\n", "\n");
        crate::dlg::message(
            app,
            "发现新版本",
            &format!(
                "发现新版本 {latest}\n\n{}",
                if formatted.is_empty() {
                    "已开始后台下载更新包。".to_string()
                } else {
                    formatted
                }
            ),
        )
        .await;
    }

    download_and_install(app, &client, &latest, &description).await;
}

async fn download_and_install(
    app: &AppHandle,
    client: &reqwest::Client,
    latest: &str,
    description: &str,
) {
    // 1. 取文件列表
    let mut installer: Option<Value> = None;
    for system in system_candidates() {
        let url = format!(
            "{UPDATE_API_BASE}/getUpdateFilelist?version={}&sys={}",
            url_encode(latest),
            url_encode(system)
        );
        if let Ok(data) = fetch_json(client, &url).await {
            if let Some(files) = data.get("files").and_then(|v| v.as_array()) {
                if !files.is_empty() {
                    installer = pick_installer(files);
                    if installer.is_some() {
                        break;
                    }
                }
            }
        }
    }

    let Some(installer) = installer else {
        toast(app, "error", "更新失败: 未找到可用的安装包".to_string());
        return;
    };

    let name = installer
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let download_url = installer
        .get("downloadUrl")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if name.is_empty() || download_url.is_empty() {
        toast(app, "error", "更新失败: 安装包信息不完整".to_string());
        return;
    }

    cleanup_old_installers(pending_installer_path().as_deref());
    let installer_path: PathBuf = paths::oicpp_dir().join(&name);

    if !installer_path.exists() {
        toast(
            app,
            "info",
            format!("更新下载已开始: 正在后台下载 {latest}，可继续正常使用。"),
        );
        if let Err(err) = download_installer(app, &download_url, &installer_path, latest).await {
            mutate_state(|state| {
                state.downloading = false;
                state.version = String::new();
                state.progress = 0;
            });
            broadcast_state(app);
            toast(app, "error", format!("更新下载失败: {err}"));
            return;
        }
        if paths::is_windows() {
            toast(
                app,
                "success",
                format!("更新下载完成: 版本 {latest} 已下载完成，关闭 OICPP 后将自动安装。"),
            );
        } else {
            toast(
                app,
                "success",
                format!("更新下载完成: 版本 {latest} 已下载完成，请手动运行安装包完成更新。"),
            );
        }
    }

    settings::set_value(
        "pendingUpdate",
        json!({
            "version": latest,
            "installerPath": installer_path.to_string_lossy(),
            "installerName": name,
            "description": description,
            "autoInstallOnQuit": paths::is_windows(),
            "downloadTime": paths::iso_now()
        }),
    );
    settings::set_value(
        "postInstallNotice",
        json!({
            "targetVersion": latest,
            "description": description,
            "source": "download",
            "armedAt": paths::iso_now()
        }),
    );
    let _ = settings::save();
    broadcast_state(app);

    #[cfg(target_os = "linux")]
    {
        let app = app.clone();
        let path = installer_path.clone();
        tauri::async_runtime::spawn(async move {
            if crate::dlg::confirm(
                &app,
                "更新已下载",
                "安装包已下载完成。是否打开安装包所在目录？",
                "打开目录",
                "稍后",
            )
            .await
            {
                if let Some(parent) = path.parent() {
                    let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
                }
            }
        });
    }
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

/// 退出时静默拉起待安装的更新包（仅 Windows）
pub fn run_pending_installer_on_quit() {
    let Some(pending) = pending_update() else {
        return;
    };
    if pending.get("autoInstallOnQuit").and_then(|v| v.as_bool()) != Some(true) {
        return;
    }
    let Some(path) = pending.get("installerPath").and_then(|v| v.as_str()) else {
        return;
    };
    if !Path::new(path).exists() {
        return;
    }
    if let Some(version) = pending.get("version").and_then(|v| v.as_str()) {
        settings::set_value(
            "postInstallNotice",
            json!({
                "targetVersion": version,
                "description": pending.get("description").cloned().unwrap_or(Value::String(String::new())),
                "source": "quit-install",
                "armedAt": paths::iso_now()
            }),
        );
        let _ = settings::save();
    }
    let mut command = std::process::Command::new(path);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
        command.creation_flags(0x0000_0008 | 0x0000_0200);
    }
    let _ = command.spawn();
}

/// 启动维护：安装后提示、清理旧安装包、清理已失效的 pendingUpdate。
pub fn handle_startup(app: &AppHandle) {
    prune_stale_pending_update();
    show_post_install_notice(app);
    cleanup_old_installers(pending_installer_path().as_deref());
}

/// 安装包被删除/移动后，pendingUpdate 会让 check_updates 永远提前返回。
fn prune_stale_pending_update() -> bool {
    let Some(pending) = pending_update() else {
        return false;
    };
    let path = pending
        .get("installerPath")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if path.is_empty() || !Path::new(path).exists() {
        settings::set_value("pendingUpdate", Value::Null);
        let _ = settings::save();
        return true;
    }
    false
}

fn show_post_install_notice(app: &AppHandle) {
    let Some(notice) = settings::get_value("postInstallNotice").filter(|v| !v.is_null()) else {
        return;
    };
    let target = notice
        .get("targetVersion")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let current = env!("CARGO_PKG_VERSION");
    if target != current {
        return;
    }
    let description = notice
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .replace("\\n", "\n");
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let detail = if description.trim().is_empty() {
            "本次更新未提供额外更新说明。".to_string()
        } else {
            description
        };
        crate::dlg::message(
            &app,
            &format!("OICPP IDE 已更新到 {current}"),
            &detail,
        )
        .await;
        settings::set_value("postInstallNotice", Value::Null);
        let pending_matches = pending_update()
            .and_then(|value| value.get("version").and_then(|v| v.as_str()).map(String::from))
            .map(|version| version == current)
            .unwrap_or(false);
        if pending_matches {
            settings::set_value("pendingUpdate", Value::Null);
        }
        let _ = settings::save();
        broadcast_state(&app);
    });
}

// ------------------------------------------------------------------ IPC

/// `get-update-download-status` / `check-updates-manual` 由 `misc` 模块转发到本模块，
/// 因此这里不需要额外认领通道。
pub async fn dispatch(_ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    None
}

/// 启动后的自动检查（延迟执行，避免和窗口加载抢资源），之后每 3 小时检查一次。
pub fn schedule_auto_check(app: &AppHandle) {
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        loop {
            mutate_state(|state| state.auto_checking = true);
            broadcast_state(&handle);
            check_updates(&handle, false).await;
            mutate_state(|state| state.auto_checking = false);
            broadcast_state(&handle);
            tokio::time::sleep(Duration::from_secs(3 * 60 * 60)).await;
        }
    });
}
