//! 工具链自检通道：test-compiler / test-testlib / check-gdb-availability。

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::compile;
use crate::ipc::Ipc;
use crate::paths;

const TEST_SOURCE: &str =
    "#include <iostream>\nint main() { std::cout << \"oicpp-test-success\"; return 0; }\n";

async fn test_compiler(compiler_path: &str) -> Value {
    if compiler_path.trim().is_empty() || !PathBuf::from(compiler_path).exists() {
        return json!({ "success": false, "message": "编译器路径无效或不存在。" });
    }

    let test_dir = std::env::temp_dir().join("oicpp-test");
    if let Err(err) = std::fs::create_dir_all(&test_dir) {
        return json!({ "success": false, "message": err.to_string() });
    }

    let source = test_dir.join("test.cpp");
    let output = test_dir.join(format!("test{}", paths::exe_suffix()));
    if let Err(err) = std::fs::write(&source, TEST_SOURCE) {
        return json!({ "success": false, "message": err.to_string() });
    }

    let compile_result = compile::compile_for_test(
        &source.to_string_lossy(),
        &output.to_string_lossy(),
        compiler_path,
        "-std=c++17",
        &test_dir.to_string_lossy(),
    )
    .await;

    let result = match compile_result {
        Err(err) => json!({ "success": false, "message": err }),
        Ok(value) => {
            let success = value
                .get("success")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !success {
                json!({
                    "success": false,
                    "message": "编译测试代码失败。",
                    "details": value.get("stderr").and_then(|v| v.as_str()).unwrap_or("")
                })
            } else {
                match compile::run_child_capture(&output.to_string_lossy(), &[], 10_000).await {
                    Ok((code, stdout, stderr)) => {
                        if code == Some(0) && stdout.contains("oicpp-test-success") {
                            json!({ "success": true })
                        } else {
                            let details = if stderr.is_empty() { stdout } else { stderr };
                            json!({
                                "success": false,
                                "message": format!("测试程序运行失败 (退出码: {})", code.unwrap_or(-1)),
                                "details": if details.is_empty() { "程序无输出".to_string() } else { details }
                            })
                        }
                    }
                    Err(err) => json!({
                        "success": false,
                        "message": format!("运行测试程序失败: {err}")
                    }),
                }
            }
        }
    };

    let _ = std::fs::remove_file(&source);
    if output.exists() {
        let _ = std::fs::remove_file(&output);
    }
    result
}

fn test_testlib(testlib_path: &str) -> Value {
    if testlib_path.trim().is_empty() || !PathBuf::from(testlib_path).exists() {
        return json!({ "success": false, "message": "testlib路径无效或不存在。" });
    }
    let name = paths::basename(testlib_path).to_lowercase();
    if name != "testlib.h" {
        return json!({ "success": false, "message": "所选文件不是testlib.h文件。" });
    }
    match std::fs::read_to_string(testlib_path) {
        Ok(content) => {
            if content.contains("This file contains testlib library") {
                json!({ "success": true, "message": "testlib测试成功！" })
            } else {
                json!({ "success": false, "message": "所选文件不是有效的testlib库文件。" })
            }
        }
        Err(err) => json!({ "success": false, "message": err.to_string() }),
    }
}

async fn check_gdb_availability() -> Value {
    if paths::is_macos() {
        return json!({
            "available": false,
            "debugger": "unsupported",
            "message": "macOS 暂不支持调试功能。"
        });
    }

    match compile::run_child_capture("gdb", &["--version".to_string()], 5_000).await {
        Ok((code, stdout, stderr)) => {
            if code == Some(0) && stdout.contains("GNU gdb") && stderr.trim().is_empty() {
                let version = stdout.lines().next().unwrap_or("").trim().to_string();
                json!({
                    "available": true,
                    "version": version,
                    "message": format!("GDB可用: {version}")
                })
            } else {
                json!({
                    "available": false,
                    "message": "GDB调试器未安装或不可用。请安装GDB调试器以使用调试功能。"
                })
            }
        }
        Err(err) if err == "__timeout__" => json!({
            "available": false,
            "message": "GDB检查超时。请确保GDB调试器已正确安装。"
        }),
        Err(err) => json!({
            "available": false,
            "message": format!("GDB调试器不可用: {err}。请安装GDB调试器以使用调试功能。")
        }),
    }
}

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        "test-compiler" => {
            let compiler_path = ipc.string_arg(0);
            Some(Ok(test_compiler(&compiler_path).await))
        }
        "test-testlib" => {
            let testlib_path = ipc.string_arg(0);
            Some(Ok(test_testlib(&testlib_path)))
        }
        "check-gdb-availability" => Some(Ok(check_gdb_availability().await)),
        _ => None,
    }
}
