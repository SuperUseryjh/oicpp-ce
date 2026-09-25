//! 编译 / 运行相关通道（对应 Electron 版 main.js 的 compileFile / runExecutable / runProgram）。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::codec;
use crate::ipc::Ipc;
use crate::paths;

const COMPILE_TIMEOUT_MS: u64 = 120_000;
const OUTPUT_LIMIT_BYTES: usize = 256 * 1024 * 1024;

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

    // 编译缓存：源文件不比目标文件新时直接复用
    let input_mtime = std::fs::metadata(&input).and_then(|m| m.modified()).ok();
    let output_mtime = std::fs::metadata(&output).and_then(|m| m.modified()).ok();
    if let (Some(input_mtime), Some(output_mtime)) = (input_mtime, output_mtime) {
        if input_mtime <= output_mtime {
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

    let mut args = parse_args_preserving_quotes(compiler_args);
    if !paths::is_windows() {
        args.retain(|arg| arg != "-static");
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

    Ok(json!({
        "success": exit_code == Some(0) && !timed_out,
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
            RunOptions {
                executable,
                args,
                working_directory,
                use_cmd_prefix: false,
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
                }
            } else {
                RunOptions {
                    executable: raw,
                    args: Vec::new(),
                    working_directory: None,
                    use_cmd_prefix: false,
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
) -> Value {
    json!({
        "output": output,
        "time": elapsed_ms,
        "timeout": timeout,
        "exitCode": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "outputLimitExceeded": output_limit_exceeded,
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
    let mut exit_code: Option<i32> = None;

    let effective_limit = time_limit.filter(|value| value.is_finite() && *value > 0.0);

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

        // 极端兜底：进程长时间不退出
        if started.elapsed() > Duration::from_secs(86_400) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
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
        output.push_str("输出超过限制 (256 MB)，程序已被终止。");
    }

    let elapsed = started.elapsed().as_millis() as i64;
    let elapsed = if let Some(limit) = effective_limit {
        elapsed.clamp(0, (limit as i64) + 100)
    } else {
        elapsed
    };

    Ok(run_result(
        output,
        elapsed,
        timeout,
        exit_code,
        stdout,
        stderr,
        output_limit_exceeded,
        captured,
        observed,
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
