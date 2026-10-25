//! 编译器 / testlib 的下载、解压与选择。
//!
//! 对应 Electron 版 main.js 的 `download-compiler` / `download-testlib` /
//! `select-compiler` / `select-testlib` / `get-downloaded-*` 通道；
//! 下载算法对齐 `utils/multi-thread-downloader.js`（HEAD 探测 + Range 分片 + 重试退避）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::fs::File as TokioFile;
use tokio::io::AsyncWriteExt;

use crate::ipc::Ipc;

/// 分片大小 2MB
const CHUNK_SIZE: u64 = 2 * 1024 * 1024;
/// 启用多线程下载的最小文件大小 2MB
const MIN_MULTI_SIZE: u64 = 2 * 1024 * 1024;
/// 最大并发分片数
const MAX_CONCURRENCY: usize = 16;
/// 分片 / 单线程下载最大尝试次数
const MAX_ATTEMPTS: u32 = 8;
/// 进度上报最小间隔（毫秒）
const PROGRESS_INTERVAL_MS: i64 = 300;

/// 下载进度统计（多线程时按分片累加）
struct Progress {
    app: tauri::AppHandle,
    /// 进度事件里的 type 字段：compiler / testlib
    event_type: &'static str,
    version: String,
    /// 文件总大小，未知时为 0
    total: u64,
    started_ms: i64,
    last_report_ms: AtomicI64,
    /// 每个分片（单线程时只有一项）已写入的字节数
    parts: Vec<Arc<AtomicU64>>,
}

impl Progress {
    fn new(
        app: tauri::AppHandle,
        event_type: &'static str,
        version: String,
        total: u64,
        part_count: usize,
    ) -> Arc<Self> {
        Arc::new(Progress {
            app,
            event_type,
            version,
            total,
            started_ms: crate::paths::now_millis(),
            last_report_ms: AtomicI64::new(0),
            parts: (0..part_count.max(1))
                .map(|_| Arc::new(AtomicU64::new(0)))
                .collect(),
        })
    }

    fn part_count(&self) -> usize {
        self.parts.len()
    }

    /// 直接覆盖某个分片的进度（重试时会被重置为从 0 开始）
    fn set_part(&self, index: usize, value: u64) {
        if let Some(slot) = self.parts.get(index) {
            slot.store(value, Ordering::Relaxed);
        }
    }

    fn downloaded(&self) -> u64 {
        self.parts
            .iter()
            .map(|slot| slot.load(Ordering::Relaxed))
            .sum()
    }

    /// 按 300ms 节流上报
    fn tick(&self) {
        let now = crate::paths::now_millis();
        let last = self.last_report_ms.load(Ordering::Relaxed);
        if now - last < PROGRESS_INTERVAL_MS {
            return;
        }
        if self
            .last_report_ms
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.emit();
    }

    fn emit(&self) {
        let downloaded = self.downloaded();
        let elapsed_ms = (crate::paths::now_millis() - self.started_ms).max(1) as f64;
        let speed = downloaded as f64 / (elapsed_ms / 1000.0);
        let progress = if self.total > 0 {
            (downloaded as f64 / self.total as f64) * 100.0
        } else {
            0.0
        };
        crate::events::broadcast_one(
            &self.app,
            "download-progress",
            json!({
                "type": self.event_type,
                "version": self.version.as_str(),
                "downloadedBytes": downloaded,
                "totalBytes": self.total,
                "progress": progress.min(100.0),
                "speed": speed
            }),
        );
    }
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "get-downloaded-compilers" => ipc.ok(list_dirs(&crate::paths::compilers_dir())),
        "get-downloaded-testlibs" => ipc.ok(list_dirs(&crate::paths::testlibs_dir())),
        "download-compiler" => Some(download(ipc, false).await),
        "download-testlib" => Some(download(ipc, true).await),
        "select-compiler" => Some(select_compiler(ipc)),
        "select-testlib" => Some(select_testlib(ipc)),
        "is-testlib-selected" => {
            let version = ipc.string_arg(0);
            let selected_path = crate::settings::testlib_path();
            let version_dir = crate::paths::testlibs_dir().join(&version);
            let selected = !version.trim().is_empty()
                && !selected_path.is_empty()
                && Path::new(&selected_path).starts_with(&version_dir);
            Some(Ok(Value::Bool(selected)))
        }
        // 渲染层不能直接请求外部接口：原版 Electron 设了 `webSecurity: false` 绕过同源校验，
        // 而 WebView2 强制 CORS，浏览器侧会直接报 `Failed to fetch`（连请求都发不出去）。
        // 这里由 Rust 侧代发请求，返回解析后的 JSON。
        "fetch-json" => Some(fetch_json(&ipc.string_arg(0)).await),
        _ => None,
    }
}

/// 由 Rust 侧代渲染层发起 GET 请求，成功时返回解析后的 JSON（非 JSON 则原样返回字符串）。
/// 不这么做的话，渲染层的 fetch 会被 WebView2 的 CORS 拦掉（见 dispatch 中的注释）。
async fn fetch_json(url: &str) -> Result<Value, String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err(format!("仅支持 http(s) 地址: {url}"));
    }
    let response = reqwest::get(url).await.map_err(|err| format!("请求失败: {err}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|err| format!("读取响应失败: {err}"))?;
    if !status.is_success() {
        let snippet: String = text.chars().take(300).collect();
        return Err(format!("HTTP {}: {snippet}", status.as_u16()));
    }
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

/// 列举目录下所有子目录名；不存在或出错返回空数组
fn list_dirs(root: &Path) -> Vec<String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect()
}

fn select_compiler(ipc: &Ipc<'_>) -> Result<Value, String> {
    let version = ipc.string_arg(0);
    let version_dir = crate::paths::compilers_dir().join(&version);
    if !version_dir.exists() {
        return Ok(json!({ "success": false, "error": "编译器版本不存在" }));
    }
    let Some(found) = find_compiler_executable(&version_dir) else {
        return Ok(json!({ "success": false, "error": "未找到编译器可执行文件" }));
    };
    let compiler_path = path_string(&found);
    crate::settings::set_value("compilerPath", Value::String(compiler_path.clone()));
    if let Err(err) = crate::settings::save() {
        return Ok(json!({ "success": false, "error": err }));
    }
    Ok(json!({ "success": true, "compilerPath": compiler_path }))
}

fn select_testlib(ipc: &Ipc<'_>) -> Result<Value, String> {
    let version = ipc.string_arg(0);
    let version_dir = crate::paths::testlibs_dir().join(&version);
    if !version_dir.exists() {
        return Ok(json!({ "success": false, "error": "testlib版本不存在" }));
    }
    let Some(found) = find_testlib_file(&version_dir) else {
        return Ok(json!({ "success": false, "error": "未找到testlib文件" }));
    };
    let testlib_path = path_string(&found);
    crate::settings::set_value("testlibPath", Value::String(testlib_path.clone()));
    if let Err(err) = crate::settings::save() {
        return Ok(json!({ "success": false, "error": err }));
    }
    Ok(json!({ "success": true, "testlibPath": testlib_path }))
}

/// `download-compiler` / `download-testlib` 共用实现
async fn download(ipc: &Ipc<'_>, is_testlib: bool) -> Result<Value, String> {
    // 编译器内置下载仅限 Windows；testlib 不受此限制
    if !is_testlib && !crate::paths::is_windows() {
        return Ok(json!({
            "success": false,
            "error": "非 Windows 平台已禁用内置编译器下载，请访问官网获取: https://oicpp.mywwzh.top/"
        }));
    }

    let args = ipc.object_arg(0);
    let url = args.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let version = args
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let name = args.get("name").and_then(Value::as_str).unwrap_or("").to_string();
    if url.is_empty() || version.is_empty() || name.is_empty() {
        return Ok(json!({ "success": false, "error": "缺少必要的下载参数" }));
    }

    let root = if is_testlib {
        crate::paths::testlibs_dir()
    } else {
        crate::paths::compilers_dir()
    };
    let version_dir = root.join(&version);
    if version_dir.exists() {
        return Ok(json!({ "success": false, "error": "该版本已存在" }));
    }

    let Some(ext) = url_extension(&url) else {
        return Ok(json!({ "success": false, "error": "无法识别下载文件类型" }));
    };
    if ext != "zip" && ext != "7z" {
        return Ok(json!({
            "success": false,
            "error": format!("不支持的文件格式: {ext}")
        }));
    }
    let temp_file = root.join(format!("{version}.{ext}"));
    let event_type: &'static str = if is_testlib { "testlib" } else { "compiler" };

    match install(ipc.app, &url, &version, &temp_file, &version_dir, &ext, event_type).await {
        Ok(()) => Ok(if is_testlib {
            let testlib_path = find_testlib_file(&version_dir)
                .map(|path| path_string(&path))
                .unwrap_or_else(|| path_string(&version_dir.join("testlib.h")));
            json!({ "success": true, "testlibPath": testlib_path })
        } else {
            let compiler_path = find_compiler_executable(&version_dir)
                .map(|path| path_string(&path))
                .unwrap_or_else(|| {
                    path_string(
                        &version_dir
                            .join("bin")
                            .join(format!("g++{}", crate::paths::exe_suffix())),
                    )
                });
            json!({ "success": true, "compilerPath": compiler_path })
        }),
        Err(err) => {
            // 失败时清理下载残留，避免下次被判为“该版本已存在”
            let _ = std::fs::remove_file(&temp_file);
            let _ = std::fs::remove_dir_all(&version_dir);
            Ok(json!({ "success": false, "error": format!("下载失败: {err}") }))
        }
    }
}

// ------------------------------------------------------------------ 下载

/// 下载压缩包并解压到版本目录
async fn install(
    app: &tauri::AppHandle,
    url: &str,
    version: &str,
    temp_file: &Path,
    version_dir: &Path,
    ext: &str,
    event_type: &'static str,
) -> Result<(), String> {
    download_file(app, url, temp_file, event_type, version).await?;

    tokio::fs::create_dir_all(version_dir)
        .await
        .map_err(|e| format!("创建版本目录失败: {e}"))?;

    extract_archive(temp_file, version_dir, ext)?;

    tokio::fs::remove_file(temp_file)
        .await
        .map_err(|e| format!("删除压缩包失败: {e}"))?;
    Ok(())
}

/// HEAD 探测后选择多线程分片下载或单线程下载
async fn download_file(
    app: &tauri::AppHandle,
    url: &str,
    output: &Path,
    event_type: &'static str,
    version: &str,
) -> Result<(), String> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| format!("创建下载客户端失败: {e}"))?;

    let mut total: u64 = 0;
    let mut supports_range = false;
    if let Ok(response) = client.head(url).send().await {
        total = response.content_length().unwrap_or(0);
        supports_range = response
            .headers()
            .get("accept-ranges")
            .and_then(|value| value.to_str().ok())
            .map(|value| value.to_ascii_lowercase().contains("bytes"))
            .unwrap_or(false);
    }

    if supports_range && total >= MIN_MULTI_SIZE {
        let part_count = ((total + CHUNK_SIZE - 1) / CHUNK_SIZE) as usize;
        let progress = Progress::new(app.clone(), event_type, version.to_string(), total, part_count);
        download_multi(&client, url, output, total, progress.clone()).await?;
        progress.emit();
    } else {
        let progress = Progress::new(app.clone(), event_type, version.to_string(), total, 1);
        download_single(&client, url, output, progress.clone()).await?;
        progress.emit();
    }
    Ok(())
}

/// 多线程分片下载（最多 16 并发），完成后按索引顺序合并
async fn download_multi(
    client: &reqwest::Client,
    url: &str,
    output: &Path,
    total: u64,
    progress: Arc<Progress>,
) -> Result<(), String> {
    let parent = output
        .parent()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let temp_dir = parent.join(format!("temp_{}", crate::paths::now_millis()));
    tokio::fs::create_dir_all(&temp_dir)
        .await
        .map_err(|e| format!("创建临时目录失败: {e}"))?;

    let parts = progress.part_count();
    let tasks: Vec<(usize, u64, u64)> = (0..parts)
        .map(|index| {
            let start = index as u64 * CHUNK_SIZE;
            let end = (start + CHUNK_SIZE - 1).min(total.saturating_sub(1));
            (index, start, end)
        })
        .collect();

    let client = client.clone();
    let url = url.to_string();
    let stream = futures_util::stream::iter(tasks.into_iter().map(|(index, start, end)| {
        let client = client.clone();
        let url = url.clone();
        let temp_dir = temp_dir.clone();
        let progress = progress.clone();
        async move {
            download_chunk(&client, &url, start, end, index, &temp_dir, &progress).await
        }
    }));
    let results: Vec<Result<(), String>> = stream.buffer_unordered(MAX_CONCURRENCY).collect().await;

    if let Some(err) = results.into_iter().find_map(|result| result.err()) {
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
        return Err(err);
    }

    // 按索引顺序追加合并
    let mut merged = TokioFile::create(output)
        .await
        .map_err(|e| format!("创建文件失败: {e}"))?;
    for index in 0..parts {
        let chunk_path = temp_dir.join(format!("chunk_{index}.tmp"));
        let mut chunk = TokioFile::open(&chunk_path)
            .await
            .map_err(|e| format!("打开分片失败: {e}"))?;
        tokio::io::copy(&mut chunk, &mut merged)
            .await
            .map_err(|e| format!("合并分片失败: {e}"))?;
    }
    merged
        .flush()
        .await
        .map_err(|e| format!("写入文件失败: {e}"))?;

    let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    Ok(())
}

/// 单个分片下载，失败最多重试 8 次（退避 2^(n-1)*1000ms，上限 8000ms）
async fn download_chunk(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    index: usize,
    temp_dir: &Path,
    progress: &Progress,
) -> Result<(), String> {
    let chunk_file = temp_dir.join(format!("chunk_{index}.tmp"));
    let mut attempt: u32 = 0;
    let mut last_error = String::new();

    while attempt < MAX_ATTEMPTS {
        attempt += 1;
        progress.set_part(index, 0);
        match fetch_chunk(client, url, start, end, &chunk_file, index, progress).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_error = err;
                let _ = tokio::fs::remove_file(&chunk_file).await;
                if attempt >= MAX_ATTEMPTS {
                    break;
                }
                let delay = (1000u64 << (attempt - 1).min(3)).min(8000);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
    }
    Err(format!("分片 {index} 下载失败: {last_error}"))
}

async fn fetch_chunk(
    client: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    chunk_file: &Path,
    index: usize,
    progress: &Progress,
) -> Result<(), String> {
    let response = client
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }

    let mut file = TokioFile::create(chunk_file)
        .await
        .map_err(|e| format!("创建分片文件失败: {e}"))?;
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| format!("读取响应失败: {e}"))?;
        file.write_all(&bytes)
            .await
            .map_err(|e| format!("写入分片失败: {e}"))?;
        written += bytes.len() as u64;
        progress.set_part(index, written);
        progress.tick();
    }
    file.flush()
        .await
        .map_err(|e| format!("写入分片失败: {e}"))?;
    Ok(())
}

/// 单线程下载，失败最多重试 8 次
async fn download_single(
    client: &reqwest::Client,
    url: &str,
    output: &Path,
    progress: Arc<Progress>,
) -> Result<(), String> {
    let mut attempt: u32 = 0;
    let mut last_error = String::new();

    while attempt < MAX_ATTEMPTS {
        attempt += 1;
        progress.set_part(0, 0);
        match fetch_single(client, url, output, &progress).await {
            Ok(()) => {
                if progress.total > 0 {
                    progress.set_part(0, progress.total);
                }
                return Ok(());
            }
            Err(err) => {
                last_error = err;
                let _ = tokio::fs::remove_file(output).await;
                if attempt >= MAX_ATTEMPTS {
                    break;
                }
                let delay = (1000u64 << (attempt - 1).min(3)).min(8000);
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
    }
    Err(last_error)
}

async fn fetch_single(
    client: &reqwest::Client,
    url: &str,
    output: &Path,
    progress: &Progress,
) -> Result<(), String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("请求失败: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }

    let mut file = TokioFile::create(output)
        .await
        .map_err(|e| format!("创建文件失败: {e}"))?;
    let mut stream = response.bytes_stream();
    let mut written: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| format!("读取响应失败: {e}"))?;
        file.write_all(&bytes)
            .await
            .map_err(|e| format!("写入文件失败: {e}"))?;
        written += bytes.len() as u64;
        progress.set_part(0, written);
        progress.tick();
    }
    file.flush()
        .await
        .map_err(|e| format!("写入文件失败: {e}"))?;
    Ok(())
}

// ------------------------------------------------------------------ 解压

/// 解压 zip / 7z 到目标目录（解压库为阻塞调用，直接在当前线程执行）
fn extract_archive(temp_file: &Path, version_dir: &Path, ext: &str) -> Result<(), String> {
    match ext {
        "zip" => {
            let file =
                std::fs::File::open(temp_file).map_err(|e| format!("打开压缩包失败: {e}"))?;
            let mut archive =
                zip::ZipArchive::new(file).map_err(|e| format!("解析 zip 失败: {e}"))?;
            archive
                .extract(version_dir)
                .map_err(|e| format!("解压 zip 失败: {e}"))
        }
        "7z" => sevenz_rust::decompress_file(temp_file, version_dir)
            .map_err(|e| format!("解压 7z 失败: {e:?}")),
        other => Err(format!("不支持的文件格式: {other}")),
    }
}

// ------------------------------------------------------------------ 探测

/// 查找编译器可执行文件（优先常见相对路径，否则递归搜索）
fn find_compiler_executable(base_dir: &Path) -> Option<PathBuf> {
    let suffix = crate::paths::exe_suffix();
    let common_paths = [
        format!("bin/g++{suffix}"),
        format!("bin/gcc{suffix}"),
        format!("mingw64/bin/g++{suffix}"),
        format!("mingw32/bin/g++{suffix}"),
        format!("x86_64-w64-mingw32/bin/g++{suffix}"),
        format!("i686-w64-mingw32/bin/g++{suffix}"),
    ];
    for relative in common_paths.iter() {
        let full = base_dir.join(relative);
        if full.exists() {
            return Some(full);
        }
    }

    let mut files = Vec::new();
    walk_files(base_dir, &mut files);
    files.into_iter().find(|file| {
        let name = file_name_lower(file);
        name == "g++.exe" || name == "g++" || name == "gcc.exe" || name == "gcc"
    })
}

/// 查找 testlib.h（优先根目录，否则递归搜索）
fn find_testlib_file(base_dir: &Path) -> Option<PathBuf> {
    let root_testlib = base_dir.join("testlib.h");
    if root_testlib.exists() {
        return Some(root_testlib);
    }

    let mut files = Vec::new();
    walk_files(base_dir, &mut files);
    files
        .into_iter()
        .find(|file| file_name_lower(file) == "testlib.h")
}

/// 递归收集目录下所有文件（跳过 .dsym）
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if file_name_lower(&path).ends_with(".dsym") {
            continue;
        }
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk_files(&path, out),
            Ok(_) => out.push(path),
            Err(_) => {}
        }
    }
}

fn file_name_lower(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// 取 URL 中 path 部分的小写扩展名（不含点），识别不出返回 None
fn url_extension(url: &str) -> Option<String> {
    let after_scheme = match url.find("://") {
        Some(index) => &url[index + 3..],
        None => url,
    };
    let path = after_scheme
        .split(|c| c == '?' || c == '#')
        .next()
        .unwrap_or("");
    let ext = crate::paths::extname(path);
    let trimmed = ext.trim_start_matches('.');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
