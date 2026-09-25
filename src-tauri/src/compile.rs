//! 编译 / 运行相关通道（对应 Electron 版 main.js 的 compileFile / runExecutable / runProgram）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::codec;
use crate::ipc::Ipc;
use crate::paths;

const COMPILE_TIMEOUT_MS: u64 = 120_000;
const OUTPUT_LIMIT_BYTES: usize = 256 * 1024 * 1024;
/// 编译缓存签名版本（与 Electron 版保持一致）
const COMPILE_CACHE_VERSION: i64 = 1;
/// 内存采样间隔（毫秒）
const MEMORY_SAMPLE_INTERVAL_MS: u64 = 200;
/// 轮询等待子进程的间隔
const WAIT_POLL_MS: u64 = 10;

// ------------------------------------------------------------------ 工具

/// 按空白拆分参数，保留单/双引号包裹的内容
pub fn parse_args_preserving_quotes(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_token = false;

    for ch in input.chars() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    has_token = true;
                } else if ch.is_whitespace() {
                    if has_token {
                        out.push(std::mem::take(&mut current));
                        has_token = false;
                    }
                } else {
                    current.push(ch);
                    has_token = true;
                }
            }
        }
    }
    if has_token {
        out.push(current);
    }
    out.retain(|item| !item.is_empty());
    out
}

/// 编译器根目录下的候选 bin 目录（存在的才返回）
fn compiler_bin_dirs(compiler_path: &str) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if compiler_path.trim().is_empty() {
        return dirs;
    }
    let compiler = PathBuf::from(compiler_path);
    let Some(dir) = compiler.parent() else {
        return dirs;
    };
    dirs.push(dir.to_path_buf());
    if let Some(root) = dir.parent() {
        for sub in ["bin", "mingw64/bin", "mingw32/bin"] {
            let candidate = root.join(sub);
            if candidate.exists() {
                dirs.push(candidate);
            }
        }
    }
    dirs
}

/// 在 PATH 前面追加编译器目录
fn with_compiler_path(cmd: &mut tokio::process::Command, compiler_path: &str) {
    let dirs = compiler_bin_dirs(compiler_path);
    if dirs.is_empty() {
        return;
    }
    let mut entries: Vec<PathBuf> = dirs;
    if let Some(existing) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&existing));
    }
    if let Ok(joined) = std::env::join_paths(entries) {
        cmd.env("PATH", joined);
    }
}

fn decode(bytes: &[u8]) -> String {
    codec::decode_auto(bytes)
}

/// 文件修改时间（毫秒时间戳）
fn mtime_ms(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or(0.0)
}

fn absolute(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// 编译缓存签名文件：`<outputFile>.oicpp-cache`
fn compile_cache_path(output_file: &str) -> String {
    format!("{}.oicpp-cache", absolute(output_file))
}

/// 构造编译缓存记录（含 sha256 签名）
fn build_cache_record(
    input_file: &str,
    output_file: &str,
    compiler_path: &str,
    compiler_args: &[String],
    working_directory: &str,
) -> Option<Value> {
    let input_meta = std::fs::metadata(input_file).ok()?;
    let compiler_meta = std::fs::metadata(compiler_path).ok()?;
    let working_dir = if working_directory.trim().is_empty() {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        absolute(working_directory)
    };

    let payload = json!({
        "cacheVersion": COMPILE_CACHE_VERSION,
        "inputFile": absolute(input_file),
        "inputSize": input_meta.len(),
        "inputMtimeMs": mtime_ms(&input_meta),
        "outputFile": absolute(output_file),
        "compilerPath": absolute(compiler_path),
        "compilerSize": compiler_meta.len(),
        "compilerMtimeMs": mtime_ms(&compiler_meta),
        "compilerArgs": compiler_args,
        "workingDirectory": working_dir
    });
    let signature = codec::sha256_hex(&payload.to_string());
    let mut record = payload;
    if let Some(map) = record.as_object_mut() {
        map.insert("signature".into(), Value::String(signature));
    }
    Some(record)
}

fn cache_signature(record: &Value) -> String {
    record
        .get("signature")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// 读取当前进程（或指定 pid）常驻内存字节数
async fn read_process_memory_bytes(pid: u32) -> u64 {
    if cfg!(target_os = "linux") {
        if let Ok(content) = std::fs::read_to_string(format!("/proc/{pid}/status")) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    let kb = rest
                        .trim()
                        .trim_end_matches("kB")
                        .trim()
                        .replace([' ', ','], "")
                        .parse::<u64>()
                        .unwrap_or(0);
                    return kb.saturating_mul(1024);
                }
            }
        }
        return 0;
    }

    if cfg!(windows) {
        let output = tokio::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .await;
        if let Ok(output) = output {
            let text = String::from_utf8_lossy(&output.stdout).to_string();
            if let Some(re) = regex::Regex::new(r#""([\d.,]+)\s*K""#).ok() {
                if let Some(caps) = re.captures(&text) {
                    let digits: String = caps
                        .get(1)
                        .map(|m| m.as_str())
                        .unwrap_or("")
                        .chars()
                        .filter(|c| c.is_ascii_digit())
                        .collect();
                    let kb = digits.parse::<u64>().unwrap_or(0);
                    return kb.saturating_mul(1024);
                }
            }
        }
        return 0;
    }

    let output = tokio::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .await;
    match output {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            text.parse::<u64>().unwrap_or(0).saturating_mul(1024)
        }
        Err(_) => 0,
    }
}

/// 解析 g++/clang 风格诊断输出
fn parse_diagnostics(text: &str) -> (Vec<String>, Vec<String>, Vec<Value>) {
    let mut warnings: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut diagnostics: Vec<Value> = Vec::new();

    let pattern = regex::Regex::new(
        r"^(?P<file>[^:\r\n]+):(?P<line>\d+):(?:(?P<col>\d+):)?\s*(?P<sev>error|warning|note|fatal error):\s*(?P<msg>.*)$",
    )
    .ok();

    for raw_line in text.lines() {
        let line = raw_line.trim_end();
        if line.is_empty() {
            continue;
        }
        if line.contains("warning:") {
            warnings.push(line.to_string());
        }
        if line.contains("error:") {
            errors.push(line.to_string());
        }
        let Some(re) = pattern.as_ref() else {
            continue;
        };
        if let Some(caps) = re.captures(line) {
            let severity_raw = caps.name("sev").map(|m| m.as_str()).unwrap_or("");
            let severity = match severity_raw {
                "warning" => "warning",
                "note" => "note",
                _ => "error",
            };
            let line_no = caps
                .name("line")
                .and_then(|m| m.as_str().parse::<i64>().ok())
                .unwrap_or(1)
                .max(1);
            let column_no = caps
                .name("col")
                .and_then(|m| m.as_str().parse::<i64>().ok())
                .unwrap_or(1)
                .max(1);
            diagnostics.push(json!({
                "file": caps.name("file").map(|m| m.as_str()).unwrap_or(""),
                "line": line_no,
                "column": column_no,
                "severity": severity,
                "message": caps.name("msg").map(|m| m.as_str().trim()).unwrap_or(""),
                "raw": line
            }));
        }
    }

    (warnings, errors, diagnostics)
}

// ------------------------------------------------------------------ 编译

async fn compile_source(
    input_file: &str,
    output_file: &str,
    compiler_path: &str,
    compiler_args: &str,
    working_directory: &str,
) -> Result<Value, String> {
    let compiler = PathBuf::from(compiler_path);
    if !compiler.exists() {
        return Err(format!("编译器不存在: {compiler_path}"));
    }
    let input = PathBuf::from(input_file);
    if !input.exists() {
        return Err(format!("源文件不存在: {input_file}"));
    }
    let output = PathBuf::from(output_file);
    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            if std::fs::create_dir_all(parent).is_err() {
                return Err(format!("无法创建输出目录: {}", parent.to_string_lossy()));
            }
        }
    }

    let mut args = parse_args_preserving_quotes(compiler_args);
    if !paths::is_windows() {
        args.retain(|arg| arg != "-static");
    }

    // 编译缓存：源文件 / 编译器 / 参数 / 工作目录签名全部一致时跳过编译
    let cache_path = compile_cache_path(output_file);
    let mut cache_record: Option<Value> = None;
    if input.exists() && compiler.exists() {
        cache_record = build_cache_record(
            input_file,
            output_file,
            compiler_path,
            &args,
            working_directory,
        );
        if let Some(record) = cache_record.as_ref() {
            let output_mtime = std::fs::metadata(&output)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as f64)
                .unwrap_or(0.0);
            let input_mtime = record
                .get("inputMtimeMs")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if output_mtime >= input_mtime && Path::new(&cache_path).exists() {
                if let Ok(text) = std::fs::read_to_string(&cache_path) {
                    if let Ok(cached) = serde_json::from_str::<Value>(&text) {
                        let version_ok = cached
                            .get("cacheVersion")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(-1)
                            == COMPILE_CACHE_VERSION;
                        if version_ok && cache_signature(&cached) == cache_signature(record) {
                            return Ok(json!({
                                "success": true,
                                "cached": true,
                                "exitCode": 0,
                                "stdout": "",
                                "stderr": "",
                                "warnings": [],
                                "errors": [],
                                "diagnostics": []
                            }));
                        }
                    }
                }
            }
        }
    }

    let cwd = if working_directory.trim().is_empty() {
        input
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    } else {
        PathBuf::from(working_directory)
    };

    let mut cmd = tokio::process::Command::new(&compiler);
    cmd.arg(&input)
        .args(&args)
        .arg("-o")
        .arg(&output)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    with_compiler_path(&mut cmd, compiler_path);

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("编译器启动失败: {err}（编译器文件不存在或路径错误、权限不足等问题）"))?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buffer).await;
        }
        buffer
    });
    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buffer).await;
        }
        buffer
    });

    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    match tokio::time::timeout(Duration::from_millis(COMPILE_TIMEOUT_MS), child.wait()).await {
        Ok(Ok(status)) => exit_code = status.code(),
        Ok(Err(_)) => {}
        Err(_) => {
            timed_out = true;
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    let stdout_bytes = stdout_task.await.unwrap_or_default();
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    let stdout = decode(&stdout_bytes);
    let stderr = decode(&stderr_bytes);

    #[cfg(unix)]
    {
        if exit_code == Some(0) && !timed_out {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&output) {
                let mut perms = meta.permissions();
                if perms.mode() & 0o111 == 0 {
                    perms.set_mode(0o755);
                    let _ = std::fs::set_permissions(&output, perms);
                }
            }
        }
    }

    let combined = format!("{stdout}\n{stderr}");
    let (mut warnings, mut errors, diagnostics) = parse_diagnostics(&combined);
    if timed_out {
        errors.push("编译超时（120 秒），已终止编译器进程。".to_string());
    }

    let succeeded = exit_code == Some(0) && !timed_out;

    // 编译成功且期间源文件 / 编译器未变化时写入缓存签名
    if succeeded && output.exists() {
        if let Some(previous) = cache_record.as_ref() {
            if let Some(latest) = build_cache_record(
                input_file,
                output_file,
                compiler_path,
                &args,
                working_directory,
            ) {
                if cache_signature(&latest) == cache_signature(previous) {
                    let _ = std::fs::write(&cache_path, latest.to_string());
                }
            }
        }
    }

    Ok(json!({
        "success": succeeded,
        "exitCode": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "warnings": warnings,
        "errors": errors,
        "diagnostics": diagnostics
    }))
}

// ------------------------------------------------------------------ 运行

async fn read_capped<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> (Vec<u8>, bool, usize) {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    let mut observed = 0usize;
    let mut exceeded = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                observed += n;
                if buffer.len() + n > limit {
                    let remain = limit.saturating_sub(buffer.len());
                    buffer.extend_from_slice(&chunk[..remain]);
                    exceeded = true;
                    break;
                }
                buffer.extend_from_slice(&chunk[..n]);
            }
            Err(_) => break,
        }
    }
    (buffer, exceeded, observed)
}

struct RunOptions {
    executable: String,
    args: Vec<String>,
    working_directory: Option<String>,
    use_cmd_prefix: bool,
    memory_limit: Option<f64>,
}

fn run_options_from(first: &Value) -> RunOptions {
    match first {
        Value::Object(map) => {
            let executable = map
                .get("executablePath")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args: Vec<String> = map
                .get("args")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|item| item.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            let working_directory = map
                .get("workingDirectory")
                .and_then(|v| v.as_str())
                .map(String::from);
            let memory_limit = map.get("memoryLimit").and_then(|v| match v {
                Value::Number(n) => n.as_f64(),
                Value::String(s) => s.parse::<f64>().ok(),
                _ => None,
            });
            RunOptions {
                executable,
                args,
                working_directory,
                use_cmd_prefix: false,
                memory_limit,
            }
        }
        other => {
            let raw = paths::value_to_string(Some(other));
            if let Some(rest) = raw.strip_prefix("cmd /c ") {
                RunOptions {
                    executable: rest.to_string(),
                    args: Vec::new(),
                    working_directory: None,
                    use_cmd_prefix: true,
                    memory_limit: None,
                }
            } else {
                RunOptions {
                    executable: raw,
                    args: Vec::new(),
                    working_directory: None,
                    use_cmd_prefix: false,
                    memory_limit: None,
                }
            }
        }
    }
}

fn run_result(
    output: String,
    elapsed_ms: i64,
    timeout: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    output_limit_exceeded: bool,
    captured: usize,
    observed: usize,
    memory_limit_exceeded: bool,
    memory_limit_bytes: u64,
    memory_bytes: u64,
) -> Value {
    json!({
        "output": output,
        "time": elapsed_ms,
        "timeout": timeout,
        "exitCode": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "outputLimitExceeded": output_limit_exceeded,
        "memoryLimitExceeded": memory_limit_exceeded,
        "memoryLimitBytes": memory_limit_bytes,
        "memoryBytes": memory_bytes,
        "outputLimitBytes": OUTPUT_LIMIT_BYTES,
        "capturedOutputBytes": captured,
        "observedOutputBytes": observed
    })
}

async fn run_program(ipc: &Ipc<'_>) -> Result<Value, String> {
    let first = ipc.arg(0).cloned().unwrap_or(Value::Null);
    let input = ipc
        .arg(1)
        .map(|v| paths::value_to_string(Some(v)))
        .unwrap_or_default();
    let time_limit = ipc.number_arg(2);

    let options = run_options_from(&first);

    // 第 4 个位置参数为内存限制（MB）；对象参数里的 memoryLimit 优先
    let memory_limit = options.memory_limit.or(ipc.number_arg(3));
    let effective_memory_limit_mb = match memory_limit {
        Some(value) if value.is_finite() && value > 0.0 => value,
        _ => 0.0,
    };
    let memory_limit_bytes = if effective_memory_limit_mb > 0.0 {
        (effective_memory_limit_mb * 1024.0 * 1024.0) as u64
    } else {
        0
    };
    let memory_limit_label = format!("{effective_memory_limit_mb}");

    let mut cmd = if options.use_cmd_prefix {
        let mut cmd = tokio::process::Command::new("cmd");
        #[cfg(windows)]
        cmd.arg("/c");
        cmd.arg(&options.executable);
        cmd
    } else if !options.args.is_empty() {
        let mut cmd = tokio::process::Command::new(&options.executable);
        cmd.args(&options.args);
        cmd
    } else {
        let resolved = std::fs::canonicalize(&options.executable)
            .unwrap_or_else(|_| PathBuf::from(&options.executable));
        tokio::process::Command::new(resolved)
    };

    if let Some(cwd) = options.working_directory.as_ref().filter(|c| !c.is_empty()) {
        cmd.current_dir(cwd);
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::piped());
    with_compiler_path(&mut cmd, &crate::settings::compiler_path());

    let started = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let message = err.to_string();
            return Ok(run_result(
                message.clone(),
                0,
                false,
                Some(-1),
                String::new(),
                message,
                false,
                0,
                0,
                false,
                memory_limit_bytes,
                0,
            ));
        }
    };

    // stdin：仅在 args 为空时喂入输入
    let feed_input = options.args.is_empty();
    if let Some(mut stdin) = child.stdin.take() {
        let payload = input.clone();
        tokio::spawn(async move {
            if feed_input && !payload.is_empty() {
                let _ = stdin.write_all(payload.as_bytes()).await;
            }
            let _ = stdin.shutdown().await;
        });
    }

    let (exceeded_tx, mut exceeded_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_tx = exceeded_tx.clone();
    let stderr_tx = exceeded_tx.clone();

    let stdout_task = tokio::spawn(async move {
        match stdout_pipe {
            Some(pipe) => {
                let (buffer, exceeded, observed) = read_capped(pipe, OUTPUT_LIMIT_BYTES).await;
                if exceeded {
                    let _ = stdout_tx.send(());
                }
                (buffer, observed)
            }
            None => (Vec::new(), 0),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match stderr_pipe {
            Some(pipe) => {
                let (buffer, exceeded, observed) = read_capped(pipe, OUTPUT_LIMIT_BYTES).await;
                if exceeded {
                    let _ = stderr_tx.send(());
                }
                (buffer, observed)
            }
            None => (Vec::new(), 0),
        }
    });

    let mut timeout = false;
    let mut output_limit_exceeded = false;
    let mut memory_limit_exceeded = false;
    let mut peak_memory_bytes: u64 = 0;
    let mut exit_code: Option<i32> = None;

    let effective_limit = time_limit.filter(|value| value.is_finite() && *value > 0.0);
    let child_pid = child.id();
    let mut last_memory_sample = Instant::now() - Duration::from_millis(MEMORY_SAMPLE_INTERVAL_MS);

    // 轮询等待：避免在 select 中同时持有 `child.wait()` 的可变借用与 `child.kill()`
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }

        if !output_limit_exceeded && exceeded_rx.try_recv().is_ok() {
            output_limit_exceeded = true;
            let _ = child.kill().await;
            let _ = child.try_wait();
            break;
        }

        if !timeout {
            if let Some(limit) = effective_limit {
                if started.elapsed() >= Duration::from_millis(limit as u64) {
                    timeout = true;
                    let _ = child.kill().await;
                    let _ = child.try_wait();
                    break;
                }
            }
        }

        // 内存采样（对应 main.js 的 sampleMemory）
        if memory_limit_bytes > 0
            && !memory_limit_exceeded
            && last_memory_sample.elapsed() >= Duration::from_millis(MEMORY_SAMPLE_INTERVAL_MS)
        {
            last_memory_sample = Instant::now();
            if let Some(pid) = child_pid {
                let bytes = read_process_memory_bytes(pid).await;
                peak_memory_bytes = peak_memory_bytes.max(bytes);
                if bytes > memory_limit_bytes {
                    memory_limit_exceeded = true;
                    let _ = child.kill().await;
                    let _ = child.try_wait();
                    break;
                }
            }
        }

        // 极端兜底：进程长时间不退出
        if started.elapsed() > Duration::from_secs(86_400) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(WAIT_POLL_MS)).await;
    }

    let (stdout_bytes, stdout_observed) = stdout_task.await.unwrap_or_default();
    let (stderr_bytes, stderr_observed) = stderr_task.await.unwrap_or_default();
    let stdout = decode(&stdout_bytes);
    let stderr = decode(&stderr_bytes);
    let captured = stdout_bytes.len() + stderr_bytes.len();
    let observed = stdout_observed + stderr_observed;

    let mut output = if let Some(code) = exit_code {
        if code != 0 {
            if !stderr.is_empty() {
                stderr.clone()
            } else if !stdout.is_empty() {
                stdout.clone()
            } else {
                format!("程序异常退出，退出码: {code}")
            }
        } else {
            stdout.clone()
        }
    } else if !stderr.is_empty() {
        stderr.clone()
    } else if !stdout.is_empty() {
        stdout.clone()
    } else {
        "程序被强制终止或异常退出".to_string()
    };

    if output_limit_exceeded {
        let notice = "输出超过限制 (256 MB)，程序已被终止。";
        if output.is_empty() {
            output = notice.to_string();
        } else if output.ends_with('\n') {
            output.push_str(notice);
        } else {
            output.push('\n');
            output.push_str(notice);
        }
    }
    if memory_limit_exceeded {
        let notice = format!("内存超过限制 ({memory_limit_label} MB)，程序已被终止。");
        if output.is_empty() {
            output = notice;
        } else if output.ends_with('\n') {
            output.push_str(&notice);
        } else {
            output.push('\n');
            output.push_str(&notice);
        }
    }

    let effective_exit_code = if output_limit_exceeded {
        exit_code.or(Some(-3))
    } else {
        exit_code
    };
    let measured_time = if effective_limit.is_some() {
        (started.elapsed().as_millis() as i64).clamp(0, (effective_limit.unwrap() as i64) + 100)
    } else {
        (started.elapsed().as_millis() as i64).max(0)
    };
    let timed_out = if output_limit_exceeded { false } else { timeout };

    Ok(run_result(
        output,
        measured_time,
        timed_out,
        effective_exit_code,
        stdout,
        stderr,
        output_limit_exceeded,
        captured,
        observed,
        memory_limit_exceeded,
        memory_limit_bytes,
        peak_memory_bytes,
    ))
}

async fn ensure_console_pauser(app: &tauri::AppHandle) -> Result<String, String> {
    let target = paths::console_pauser_path();
    if target.exists() {
        return Ok(target.to_string_lossy().to_string());
    }
    let source = paths::resource_dir(app).join("ConsolePauser.exe");
    if !source.exists() {
        return Err(
            "未找到consolepauser.exe，无法启动程序。请确保%userprofile%/.oicpp/consolepauser.exe已正确生成。"
                .to_string(),
        );
    }
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::copy(&source, &target).map_err(|e| e.to_string())?;
    Ok(target.to_string_lossy().to_string())
}

async fn run_executable(ipc: &Ipc<'_>) -> Result<Value, String> {
    let options = ipc.object_arg(0);
    let executable_path = options
        .get("executablePath")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let working_directory = options
        .get("workingDirectory")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if executable_path.trim().is_empty() || !Path::new(&executable_path).exists() {
        return Err(format!("可执行文件不存在: {executable_path}"));
    }
    let absolute = std::fs::canonicalize(&executable_path)
        .unwrap_or_else(|_| PathBuf::from(&executable_path))
        .to_string_lossy()
        .to_string();
    let cwd = if working_directory.trim().is_empty() {
        paths::dirname(&absolute)
    } else {
        working_directory.clone()
    };

    if crate::settings::run_mode() == "integrated-terminal" {
        return Ok(json!({
            "success": true,
            "mode": "integrated-terminal",
            "executablePath": absolute,
            "workingDirectory": cwd,
            "message": "程序已转交内置终端运行"
        }));
    }

    #[cfg(windows)]
    {
        let pauser = ensure_console_pauser(ipc.app).await?;
        let mut cmd = tokio::process::Command::new("cmd");
        cmd.arg("/c")
            .arg("start")
            .arg("Program Running")
            .arg(&pauser)
            .arg(&absolute)
            .current_dir(if cwd.is_empty() { "." } else { cwd.as_str() })
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null());
        let _ = cmd.spawn().map_err(|err| format!("启动程序失败: {err}"))?;
        return Ok(json!({ "success": true, "message": "程序已在新窗口启动" }));
    }

    #[cfg(not(windows))]
    {
        let candidates: [(&str, &[&str]); 5] = [
            ("gnome-terminal", &["--"]),
            ("konsole", &["-e"]),
            ("xfce4-terminal", &["-x"]),
            ("x-terminal-emulator", &["-e"]),
            ("xterm", &["-e"]),
        ];
        let script = format!(
            "cd '{}'; '{}'; printf '\\n程序执行完成，按回车键继续...\\n'; read -r",
            cwd.replace('\'', "'\\''"),
            absolute.replace('\'', "'\\''")
        );
        for (program, prefix) in candidates.iter() {
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(*prefix)
                .arg("bash")
                .arg("-lc")
                .arg(&script)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .stdin(Stdio::null());
            if cmd.spawn().is_ok() {
                return Ok(json!({ "success": true, "message": "程序已在新窗口启动" }));
            }
        }
        Err("未找到可用的终端模拟器（gnome-terminal/konsole/xfce4-terminal/x-terminal-emulator/xterm）"
            .to_string())
    }
}

// ------------------------------------------------------------------ 交互题运行

fn interactive_error_result(message: &str) -> Value {
    json!({
        "output": message,
        "time": 0,
        "timeout": false,
        "exitCode": -1,
        "contestantExitCode": -1,
        "graderExitCode": -1,
        "stdout": "",
        "stderr": message,
        "outputLimitExceeded": false,
        "memoryLimitExceeded": false,
        "memoryBytes": 0
    })
}

/// 交互题的管道泵：读取一端输出，必要时转发到对端 stdin，并做全局输出上限控制
fn spawn_interactive_pump<R>(
    reader: Option<R>,
    forward: Option<tokio::process::ChildStdin>,
    captured: Arc<AtomicUsize>,
    ole: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<Vec<u8>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer: Vec<u8> = Vec::new();
        let Some(mut reader) = reader else {
            return buffer;
        };
        let mut sink = forward;
        let mut chunk = [0u8; 65536];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) => break,
                Ok(n) => {
                    let previous = captured.fetch_add(n, Ordering::SeqCst);
                    if previous + n > OUTPUT_LIMIT_BYTES {
                        let remain = OUTPUT_LIMIT_BYTES.saturating_sub(previous);
                        buffer.extend_from_slice(&chunk[..remain]);
                        ole.store(true, Ordering::SeqCst);
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..n]);
                    if sink.is_some() {
                        let failed = match sink.as_mut() {
                            Some(writer) => writer.write_all(&chunk[..n]).await.is_err(),
                            None => false,
                        };
                        if failed {
                            sink = None;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        if let Some(mut writer) = sink {
            let _ = writer.shutdown().await;
        }
        buffer
    })
}

async fn join_pump(handle: tokio::task::JoinHandle<Vec<u8>>) -> Vec<u8> {
    match tokio::time::timeout(Duration::from_secs(3), handle).await {
        Ok(Ok(buffer)) => buffer,
        _ => Vec::new(),
    }
}

async fn run_interactive(ipc: &Ipc<'_>) -> Result<Value, String> {
    let options = ipc.object_arg(0);
    let text_of = |key: &str| {
        options
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let number_of = |key: &str| {
        options.get(key).and_then(|v| match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.parse::<f64>().ok(),
            _ => None,
        })
    };

    let contestant_path = text_of("contestantExecutablePath");
    let grader_path = text_of("graderExecutablePath");
    let input_file = text_of("inputFilePath");
    let time_limit = number_of("timeLimit");
    let memory_limit = number_of("memoryLimit");

    if contestant_path.trim().is_empty()
        || grader_path.trim().is_empty()
        || input_file.trim().is_empty()
        || !Path::new(&input_file).exists()
    {
        return Err("交互题运行参数不完整".to_string());
    }

    let contestant_abs = absolute(&contestant_path);
    let grader_abs = absolute(&grader_path);
    let contestant_cwd = options
        .get("contestantWorkingDirectory")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| paths::dirname(&contestant_abs));
    let grader_cwd = options
        .get("graderWorkingDirectory")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| paths::dirname(&grader_abs));

    let compiler_path = crate::settings::compiler_path();

    let mut contestant_cmd = tokio::process::Command::new(&contestant_abs);
    contestant_cmd
        .current_dir(&contestant_cwd)
        .env("OICPP_INTERACTIVE_INPUT", &input_file)
        .env("OICPP_CONTESTANT_EXECUTABLE", &contestant_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    with_compiler_path(&mut contestant_cmd, &compiler_path);

    let mut grader_cmd = tokio::process::Command::new(&grader_abs);
    grader_cmd
        .arg(&input_file)
        .current_dir(&grader_cwd)
        .env("OICPP_INTERACTIVE_INPUT", &input_file)
        .env("OICPP_CONTESTANT_EXECUTABLE", &contestant_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    with_compiler_path(&mut grader_cmd, &compiler_path);

    let mut contestant = match contestant_cmd.spawn() {
        Ok(child) => child,
        Err(err) => return Ok(interactive_error_result(&err.to_string())),
    };
    let mut grader = match grader_cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = contestant.kill().await;
            return Ok(interactive_error_result(&err.to_string()));
        }
    };

    let contestant_stdout = contestant.stdout.take();
    let contestant_stderr = contestant.stderr.take();
    let grader_stdout = grader.stdout.take();
    let grader_stderr = grader.stderr.take();
    let contestant_stdin = contestant.stdin.take();
    let grader_stdin = grader.stdin.take();

    let captured = Arc::new(AtomicUsize::new(0));
    let ole = Arc::new(AtomicBool::new(false));

    // 选手 stdout -> 评测器 stdin；评测器 stdout -> 选手 stdin
    let t_contestant_out = spawn_interactive_pump(
        contestant_stdout,
        grader_stdin,
        captured.clone(),
        ole.clone(),
    );
    let t_grader_out = spawn_interactive_pump(
        grader_stdout,
        contestant_stdin,
        captured.clone(),
        ole.clone(),
    );
    let t_contestant_err =
        spawn_interactive_pump(contestant_stderr, None, captured.clone(), ole.clone());
    let t_grader_err = spawn_interactive_pump(grader_stderr, None, captured.clone(), ole.clone());

    let started = Instant::now();
    let effective_time_limit = time_limit.filter(|value| value.is_finite() && *value > 0.0);
    let memory_limit_bytes = match memory_limit {
        Some(value) if value.is_finite() && value > 0.0 => (value * 1024.0 * 1024.0) as u64,
        _ => 0,
    };
    let memory_limit_label = memory_limit
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|value| format!("{value}"))
        .unwrap_or_else(|| "0".to_string());

    let mut timeout = false;
    let mut memory_limit_exceeded = false;
    let mut peak_memory_bytes: u64 = 0;
    let mut contestant_exit: Option<i32> = None;
    let mut grader_exit: Option<i32> = None;
    let mut killed = false;
    let contestant_pid = contestant.id();
    let mut last_memory_sample =
        Instant::now() - Duration::from_millis(MEMORY_SAMPLE_INTERVAL_MS);

    loop {
        if contestant_exit.is_none() {
            if let Ok(Some(status)) = contestant.try_wait() {
                contestant_exit = status.code();
            }
        }
        if grader_exit.is_none() {
            if let Ok(Some(status)) = grader.try_wait() {
                grader_exit = status.code();
            }
        }

        // 评测器异常退出时终止选手进程
        if let Some(code) = grader_exit {
            if code != 0
                && !timeout
                && !memory_limit_exceeded
                && !ole.load(Ordering::SeqCst)
                && contestant_exit.is_none()
            {
                let _ = contestant.kill().await;
            }
        }

        if contestant_exit.is_some() && grader_exit.is_some() {
            break;
        }

        if !killed {
            let mut should_kill = ole.load(Ordering::SeqCst);
            if !should_kill {
                if let Some(limit) = effective_time_limit {
                    if started.elapsed() >= Duration::from_millis(limit as u64) {
                        timeout = true;
                        should_kill = true;
                    }
                }
            }
            if !should_kill {
                if let Some(limit) = effective_time_limit {
                    if started.elapsed() >= Duration::from_millis((limit * 1.1) as u64) {
                        should_kill = true;
                    }
                }
            }
            if should_kill {
                killed = true;
                let _ = contestant.kill().await;
                let _ = grader.kill().await;
            }
        }

        if memory_limit_bytes > 0
            && !memory_limit_exceeded
            && last_memory_sample.elapsed() >= Duration::from_millis(MEMORY_SAMPLE_INTERVAL_MS)
        {
            last_memory_sample = Instant::now();
            if let Some(pid) = contestant_pid {
                let bytes = read_process_memory_bytes(pid).await;
                peak_memory_bytes = peak_memory_bytes.max(bytes);
                if bytes > memory_limit_bytes {
                    memory_limit_exceeded = true;
                    let _ = contestant.kill().await;
                    let _ = grader.kill().await;
                }
            }
        }

        if started.elapsed() > Duration::from_secs(86_400) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(WAIT_POLL_MS)).await;
    }

    let contestant_out = join_pump(t_contestant_out).await;
    let grader_out = join_pump(t_grader_out).await;
    let contestant_err = join_pump(t_contestant_err).await;
    let grader_err = join_pump(t_grader_err).await;

    let contestant_stdout = decode(&contestant_out);
    let contestant_stderr = decode(&contestant_err);
    let grader_stdout = decode(&grader_out);
    let grader_stderr = decode(&grader_err);

    let mut parts: Vec<String> = Vec::new();
    if !grader_stdout.is_empty() {
        parts.push(format!("[grader stdout]\n{grader_stdout}"));
    }
    if !contestant_stdout.is_empty() {
        parts.push(format!("[contestant stdout]\n{contestant_stdout}"));
    }
    if !grader_stderr.is_empty() {
        parts.push(format!("[grader stderr]\n{grader_stderr}"));
    }
    if !contestant_stderr.is_empty() {
        parts.push(format!("[contestant stderr]\n{contestant_stderr}"));
    }
    if memory_limit_exceeded {
        parts.push(format!("内存超过限制 ({memory_limit_label} MB)，程序已被终止。"));
    }

    let output_limit_exceeded = ole.load(Ordering::SeqCst);
    let observed = captured.load(Ordering::SeqCst);
    let stderr_joined = [grader_stderr.clone(), contestant_stderr.clone()]
        .into_iter()
        .filter(|value| !value.is_empty())
        .collect::<Vec<String>>()
        .join("\n");

    Ok(json!({
        "output": parts.join("\n"),
        "time": started.elapsed().as_millis() as i64,
        "timeout": if output_limit_exceeded || memory_limit_exceeded { false } else { timeout },
        "exitCode": contestant_exit,
        "contestantExitCode": contestant_exit,
        "graderExitCode": grader_exit,
        "stdout": contestant_stdout,
        "stderr": stderr_joined,
        "graderStdout": grader_stdout,
        "graderStderr": grader_stderr,
        "outputLimitExceeded": output_limit_exceeded,
        "outputLimitBytes": OUTPUT_LIMIT_BYTES,
        "capturedOutputBytes": observed.min(OUTPUT_LIMIT_BYTES),
        "observedOutputBytes": observed,
        "memoryLimitExceeded": memory_limit_exceeded,
        "memoryLimitBytes": memory_limit_bytes,
        "memoryBytes": peak_memory_bytes
    }))
}

// ------------------------------------------------------------------ IPC

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "compile-file" => {
            let options = ipc.object_arg(0);
            let input_file = options.get("inputFile").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let output_file = options.get("outputFile").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let compiler_path = options
                .get("compilerPath")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let compiler_args = options
                .get("compilerArgs")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let working_directory = options
                .get("workingDirectory")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(
                compile_source(
                    &input_file,
                    &output_file,
                    &compiler_path,
                    &compiler_args,
                    &working_directory,
                )
                .await,
            )
        }
        "run-program" => Some(run_program(ipc).await),
        "run-interactive" => Some(run_interactive(ipc).await),
        "run-executable" => Some(run_executable(ipc).await),
        // 原实现中该通道调用了未定义的函数（死代码），这里保持空实现
        "compile-code" => Some(Ok(Value::Null)),
        _ => None,
    }
}

/// 供 toolchain.rs 复用：固定参数编译一段测试源码
pub async fn compile_for_test(
    input_file: &str,
    output_file: &str,
    compiler_path: &str,
    compiler_args: &str,
    working_directory: &str,
) -> Result<Value, String> {
    compile_source(
        input_file,
        output_file,
        compiler_path,
        compiler_args,
        working_directory,
    )
    .await
}

/// 供 toolchain.rs 复用：带环境 PATH 的简单进程执行
pub async fn run_child_capture(
    program: &str,
    args: &[String],
    timeout_ms: u64,
) -> Result<(Option<i32>, String, String), String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    with_compiler_path(&mut cmd, &crate::settings::compiler_path());

    let mut child = cmd.spawn().map_err(|e| e.to_string())?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buffer).await;
        }
        buffer
    });
    let stderr_task = tokio::spawn(async move {
        let mut buffer = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buffer).await;
        }
        buffer
    });

    let status = match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait()).await {
        Ok(Ok(status)) => Some(status.code()),
        Ok(Err(_)) => None,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err("__timeout__".to_string());
        }
    };

    let stdout = stdout_task.await.unwrap_or_default();
    let stderr = stderr_task.await.unwrap_or_default();
    Ok((status, decode(&stdout), decode(&stderr)))
}
