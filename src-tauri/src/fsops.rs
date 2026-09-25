//! 文件系统相关的 IPC（对应 main.js 中的对话框 / 读写 / 复制粘贴等逻辑）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::ipc::Ipc;
use crate::paths;

// ------------------------------------------------------------------ 基础工具

/// 读取文本文件（自动探测编码），文件不存在返回 `None`
pub fn read_text_file(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(crate::codec::decode_auto(&bytes))
}

/// 内容不同才写入（对应 main.js 的 writeUtf8FileIfChanged）
fn write_if_changed(path: &str, content: &str) -> Result<(), String> {
    let target = Path::new(path);
    if let Ok(existing) = std::fs::read_to_string(target) {
        if crate::codec::fingerprint(&existing) == crate::codec::fingerprint(content) {
            return Ok(());
        }
    }
    std::fs::write(target, content).map_err(|e| e.to_string())
}

/// 创建文件（校验名称 -> 建父目录 -> 去重 -> 写内容），返回最终路径
fn create_file_at(file_path: &str, content: &str) -> Result<String, String> {
    let file_name = paths::basename(file_path);
    paths::validate_file_name(&file_name)?;
    let dir = paths::dirname(file_path);
    if !dir.is_empty() {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    let mut final_path = PathBuf::from(file_path);
    if final_path.exists() {
        let parent = Path::new(file_path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        final_path = paths::unique_path(&parent, &file_name);
    }
    std::fs::write(&final_path, content).map_err(|e| e.to_string())?;
    Ok(final_path.to_string_lossy().to_string())
}

/// 删除文件或目录（目录递归删除）
fn remove_path(path: &str) -> Result<(), String> {
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(path).map_err(|e| e.to_string())
    }
}

/// 递归复制目录（对应 main.js 的 copyDirectorySync）
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let entries = std::fs::read_dir(src).map_err(|e| e.to_string())?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// `base (n).ext` 去重（目录由路径推导）
fn unique_in_parent(path: &str, name: &str) -> PathBuf {
    let parent = Path::new(path)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    paths::unique_path(&parent, name)
}

// ------------------------------------------------------------------ 分发

pub async fn dispatch(ipc: &Ipc<'_>) -> Option<Result<Value, String>> {
    match ipc.channel {
        // 1. 打开文件
        "open-file-dialog" => {
            let picked = crate::dlg::pick_open(
                ipc.app,
                crate::dlg::OpenOptions {
                    filters: vec![
                        (
                            "C++ Files".into(),
                            vec!["cpp".into(), "cxx".into(), "cc".into(), "c".into()],
                        ),
                        (
                            "Text Files".into(),
                            vec!["txt".into(), "in".into(), "out".into()],
                        ),
                        ("All Files".into(), vec!["*".into()]),
                    ],
                    ..Default::default()
                },
            )
            .await;
            let Some(path) = picked.into_iter().next() else {
                return Some(Ok(Value::Null));
            };
            let path_str = path.to_string_lossy().to_string();
            match std::fs::read(&path) {
                Ok(bytes) => {
                    if crate::codec::is_binary(&bytes) {
                        ipc.emit_one(
                            "file-open-binary",
                            Value::String(paths::basename(&path_str)),
                        );
                    } else {
                        let content = String::from_utf8_lossy(&bytes).to_string();
                        ipc.emit_one(
                            "file-opened",
                            json!({
                                "fileName": paths::basename(&path_str),
                                "filePath": path_str,
                                "content": content
                            }),
                        );
                    }
                }
                Err(err) => return ipc.fail(err.to_string()),
            }
            Some(Ok(Value::Null))
        }

        // 2. 打开文件夹 / 工作区
        "open-folder-dialog" | "open-folder" => {
            let picked = crate::dlg::pick_open(
                ipc.app,
                crate::dlg::OpenOptions {
                    directory: true,
                    ..Default::default()
                },
            )
            .await;
            let Some(path) = picked.into_iter().next() else {
                return Some(Ok(Value::Null));
            };
            let mut resolved = path.to_string_lossy().to_string();
            if std::fs::metadata(&resolved)
                .map(|m| m.is_file())
                .unwrap_or(false)
            {
                resolved = paths::dirname(&resolved);
            }

            crate::settings::set_value("lastOpen", Value::String(resolved.clone()));
            let name = paths::basename(&resolved);
            let dir_for_list = resolved.clone();
            crate::settings::update(|settings| {
                if let Some(list) = settings
                    .get_mut("recentFiles")
                    .and_then(|v| v.as_array_mut())
                {
                    list.retain(|item| {
                        item.get("path").and_then(|p| p.as_str()) != Some(dir_for_list.as_str())
                    });
                    list.insert(
                        0,
                        json!({
                            "path": dir_for_list,
                            "name": name,
                            "lastAccessed": paths::iso_now()
                        }),
                    );
                    list.truncate(10);
                }
            });
            let _ = crate::settings::save();

            ipc.emit_one("folder-opened", Value::String(resolved));
            Some(Ok(Value::Null))
        }

        // 3. 保存文件（invoke / send 共用同一段写盘逻辑）
        "save-file" => {
            let file_path = ipc.string_arg(0);
            let content = ipc.string_arg(1);
            if file_path.to_ascii_lowercase().starts_with("cloud:") {
                if ipc.is_invoke() {
                    return ipc.fail("云端文件不支持本地保存");
                }
                ipc.emit(
                    "file-saved",
                    vec![
                        Value::String(file_path),
                        Value::String("云端文件不支持本地保存".into()),
                    ],
                );
                return Some(Ok(Value::Null));
            }
            match write_if_changed(&file_path, &content) {
                Ok(_) => {
                    ipc.emit(
                        "file-saved",
                        vec![Value::String(file_path), Value::Null],
                    );
                    if ipc.is_invoke() {
                        Some(Ok(Value::Bool(true)))
                    } else {
                        Some(Ok(Value::Null))
                    }
                }
                Err(err) => {
                    if ipc.is_invoke() {
                        ipc.fail(err)
                    } else {
                        ipc.emit(
                            "file-saved",
                            vec![Value::String(file_path), Value::String(err)],
                        );
                        Some(Ok(Value::Null))
                    }
                }
            }
        }

        // 4. 另存为
        "save-as-file" => {
            let content = ipc.string_arg(0);
            let picked = crate::dlg::pick_save(
                ipc.app,
                Some("另存为"),
                Some("untitled.cpp"),
                vec![
                    ("C++ Files".into(), vec!["cpp".into(), "cc".into(), "cxx".into(), "c++".into()]),
                    ("C Files".into(), vec!["c".into()]),
                    ("Header Files".into(), vec!["h".into(), "hpp".into(), "hxx".into()]),
                    ("All Files".into(), vec!["*".into()]),
                ],
            )
            .await;
            let Some(path) = picked else {
                return Some(Ok(Value::Null));
            };
            let path_str = path.to_string_lossy().to_string();
            match write_if_changed(&path_str, &content) {
                Ok(_) => {
                    ipc.emit_one("file-saved", Value::String(path_str.clone()));
                    Some(Ok(Value::String(path_str)))
                }
                Err(err) => ipc.fail(err),
            }
        }

        // 5. 写临时文件
        "save-temp-file" => {
            let relative = ipc.string_arg(0);
            let content = ipc.string_arg(1);
            let target = paths::code_temp_dir().join(&relative);
            let parent = target
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(paths::code_temp_dir);
            if let Err(err) = std::fs::create_dir_all(&parent) {
                return ipc.fail(err.to_string());
            }
            match std::fs::write(&target, &content) {
                Ok(_) => ipc.ok(target.to_string_lossy().to_string()),
                Err(err) => ipc.fail(err.to_string()),
            }
        }

        // 6. 写二进制临时文件
        "save-binary-temp-file" => {
            let file_name = ipc.string_arg(0);
            let data = match ipc.arg(1) {
                Some(Value::String(s)) if !s.is_empty() => s.clone(),
                _ => return ipc.fail("缺少文件数据"),
            };
            let bytes = match crate::codec::b64_decode(&data) {
                Ok(bytes) => bytes,
                Err(err) => return ipc.fail(err),
            };
            let dir = paths::code_temp_dir();
            let mut safe_name = paths::basename(&file_name);
            if safe_name.is_empty() {
                safe_name = format!("temp-{}.bin", paths::now_millis());
            }
            let mut target = dir.join(&safe_name);
            if target.exists() {
                target = paths::unique_path(&dir, &safe_name);
            }
            match std::fs::write(&target, &bytes) {
                Ok(_) => ipc.ok(target.to_string_lossy().to_string()),
                Err(err) => ipc.fail(err.to_string()),
            }
        }

        // 7. 读取临时文件（相对 home 目录）
        "load-temp-file" => {
            let target = paths::home_dir().join(ipc.string_arg(0));
            if !target.exists() {
                return Some(Ok(Value::Null));
            }
            match std::fs::read_to_string(&target) {
                Ok(content) => ipc.ok(content),
                Err(err) => ipc.fail(err.to_string()),
            }
        }

        // 8. 删除临时文件
        "delete-temp-file" => {
            let input = ipc.string_arg(0);
            let target = if Path::new(&input).is_absolute() {
                PathBuf::from(&input)
            } else {
                paths::code_temp_dir().join(&input)
            };
            if target.exists() {
                if let Err(err) = std::fs::remove_file(&target) {
                    return ipc.fail(err.to_string());
                }
            }
            ipc.ok(true)
        }

        // 9. 文件是否存在
        "check-file-exists" => {
            let path = ipc.string_arg(0);
            let exists = std::fs::metadata(&path).map(|_| true).unwrap_or(false);
            ipc.ok(exists)
        }

        // 10. 路径信息
        "get-path-info" => {
            let path = ipc.string_arg(0);
            ipc.ok(json!({
                "dirname": paths::dirname(&path),
                "basename": paths::basename(&path),
                "extname": paths::extname(&path),
                "basenameWithoutExt": paths::basename_without_ext(&path)
            }))
        }

        // 11. 确保目录存在
        "ensure-dir" | "ensure-directory" => {
            let dir = ipc.string_arg(0);
            match std::fs::create_dir_all(&dir) {
                Ok(_) => ipc.ok(true),
                Err(err) => ipc.fail(err.to_string()),
            }
        }

        // 12. 写文件（不广播事件）
        "write-file" => {
            let file_path = ipc.string_arg(0);
            let content = ipc.string_arg(1);
            match write_if_changed(&file_path, &content) {
                Ok(_) => Some(Ok(json!({ "success": true }))),
                Err(err) => Some(Ok(json!({ "success": false, "error": err }))),
            }
        }

        // 13. 创建文件
        "create-file" => {
            let file_path = ipc.string_arg(0);
            let content = ipc.string_arg(1);
            if ipc.is_invoke() {
                match create_file_at(&file_path, &content) {
                    Ok(final_path) => {
                        ipc.emit(
                            "file-created",
                            vec![Value::String(final_path.clone()), Value::Null],
                        );
                        Some(Ok(json!({ "success": true, "filePath": final_path })))
                    }
                    Err(err) => Some(Ok(json!({ "success": false, "error": err }))),
                }
            } else {
                match create_file_at(&file_path, &content) {
                    Ok(final_path) => ipc.emit(
                        "file-created",
                        vec![Value::String(final_path), Value::Null],
                    ),
                    Err(err) => ipc.emit(
                        "file-created",
                        vec![Value::String(file_path), Value::String(err)],
                    ),
                }
                Some(Ok(Value::Null))
            }
        }

        // 14. 创建文件夹
        "create-folder" => {
            let folder_path = ipc.string_arg(0);
            let result = (|| -> Result<String, String> {
                let name = paths::basename(&folder_path);
                paths::validate_file_name(&name)?;
                let mut final_path = PathBuf::from(&folder_path);
                if final_path.exists() {
                    final_path = unique_in_parent(&folder_path, &name);
                }
                std::fs::create_dir_all(&final_path).map_err(|e| e.to_string())?;
                Ok(final_path.to_string_lossy().to_string())
            })();
            match result {
                Ok(final_path) => ipc.emit(
                    "folder-created",
                    vec![Value::String(final_path), Value::Null],
                ),
                Err(err) => ipc.emit(
                    "folder-created",
                    vec![Value::String(folder_path), Value::String(err)],
                ),
            }
            Some(Ok(Value::Null))
        }

        // 15. 重命名文件
        "rename-file" => {
            let old_path = ipc.string_arg(0);
            let new_name = ipc.string_arg(1);
            let result = (|| -> Result<String, String> {
                paths::validate_file_name(&new_name)?;
                let mut new_path = Path::new(&old_path)
                    .parent()
                    .map(|p| p.join(&new_name))
                    .unwrap_or_else(|| PathBuf::from(&new_name));
                if new_path.exists() {
                    new_path = unique_in_parent(&old_path, &new_name);
                }
                std::fs::rename(&old_path, &new_path).map_err(|e| e.to_string())?;
                Ok(new_path.to_string_lossy().to_string())
            })();
            match result {
                Ok(new_path) => ipc.emit(
                    "file-renamed",
                    vec![
                        Value::String(old_path),
                        Value::String(new_path),
                        Value::Null,
                    ],
                ),
                Err(err) => ipc.emit(
                    "file-renamed",
                    vec![Value::String(old_path), Value::Null, Value::String(err)],
                ),
            }
            Some(Ok(Value::Null))
        }

        // 16. 删除文件 / 目录
        "delete-file" => {
            let file_path = ipc.string_arg(0);
            let result = remove_path(&file_path);
            if ipc.is_invoke() {
                match result {
                    Ok(_) => Some(Ok(json!({ "success": true }))),
                    Err(err) => Some(Ok(json!({ "success": false, "error": err }))),
                }
            } else {
                match result {
                    Ok(_) => ipc.emit(
                        "file-deleted",
                        vec![Value::String(file_path), Value::Null],
                    ),
                    Err(err) => ipc.emit(
                        "file-deleted",
                        vec![Value::String(file_path), Value::String(err)],
                    ),
                }
                Some(Ok(Value::Null))
            }
        }

        // 17. 粘贴（复制 / 剪切）
        "paste-file" => {
            let source_path = ipc.string_arg(0);
            let target_dir = ipc.string_arg(1);
            let operation = ipc.string_arg(2);
            let result = (|| -> Result<String, String> {
                let name = paths::basename(&source_path);
                let dir = PathBuf::from(&target_dir);
                let mut target_path = dir.join(&name);
                if target_path.exists() {
                    target_path = paths::unique_path(&dir, &name);
                }
                if operation == "copy" {
                    let src = Path::new(&source_path);
                    let meta = std::fs::metadata(src).map_err(|e| e.to_string())?;
                    if meta.is_dir() {
                        copy_dir_recursive(src, &target_path)?;
                    } else {
                        std::fs::copy(src, &target_path).map_err(|e| e.to_string())?;
                    }
                } else {
                    std::fs::rename(&source_path, &target_path).map_err(|e| e.to_string())?;
                }
                Ok(target_path.to_string_lossy().to_string())
            })();
            match result {
                Ok(target_path) => ipc.emit(
                    "file-pasted",
                    vec![
                        Value::String(source_path),
                        Value::String(target_path),
                        Value::String(operation),
                        Value::Null,
                    ],
                ),
                Err(err) => ipc.emit(
                    "file-pasted",
                    vec![
                        Value::String(source_path),
                        Value::Null,
                        Value::String(operation),
                        Value::String(err),
                    ],
                ),
            }
            Some(Ok(Value::Null))
        }

        // 18. 移动文件
        "move-file" => {
            let source_path = ipc.string_arg(0);
            let target_path = ipc.string_arg(1);
            if !Path::new(&source_path).exists() {
                ipc.emit(
                    "file-move-error",
                    vec![
                        Value::String(source_path),
                        Value::String("源文件不存在".into()),
                    ],
                );
                return Some(Ok(Value::Null));
            }
            let result = (|| -> Result<(), String> {
                let dir = paths::dirname(&target_path);
                if !dir.is_empty() {
                    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
                }
                std::fs::rename(&source_path, &target_path).map_err(|e| e.to_string())
            })();
            match result {
                Ok(_) => ipc.emit(
                    "file-moved",
                    vec![Value::String(source_path), Value::String(target_path)],
                ),
                Err(err) => ipc.emit(
                    "file-move-error",
                    vec![Value::String(source_path), Value::String(err)],
                ),
            }
            Some(Ok(Value::Null))
        }

        // 19. 路径拼接
        "path-join" => {
            let mut out = PathBuf::new();
            for value in ipc.args {
                let part = paths::value_to_string(Some(value));
                if part.is_empty() {
                    continue;
                }
                out.push(part);
            }
            ipc.ok(out.to_string_lossy().to_string())
        }

        // 20. 取目录名
        "path-dirname" => ipc.ok(paths::dirname(&ipc.string_arg(0))),

        // 21. 另存为（send 版：只选路径并通知渲染进程，由渲染进程自行保存）
        "save-file-as" => {
            let picked = crate::dlg::pick_save(
                ipc.app,
                Some("另存为"),
                Some("untitled.cpp"),
                vec![
                    (
                        "C++ Files".into(),
                        vec!["cpp".into(), "cc".into(), "cxx".into(), "c++".into()],
                    ),
                    ("C Files".into(), vec!["c".into()]),
                    (
                        "Header Files".into(),
                        vec!["h".into(), "hpp".into(), "hxx".into()],
                    ),
                    ("All Files".into(), vec!["*".into()]),
                ],
            )
            .await;
            if let Some(path) = picked {
                ipc.emit_one("file-save-as", Value::String(path.to_string_lossy().to_string()));
            }
            Some(Ok(Value::Null))
        }

        // 22. 清空目录内容（保留目录本身）
        "clear-directory-contents" => {
            let raw = ipc.string_arg(0);
            if raw.trim().is_empty() {
                return Some(Ok(json!({ "success": false, "error": "路径无效" })));
            }
            let resolved = std::fs::canonicalize(raw.trim())
                .unwrap_or_else(|_| PathBuf::from(raw.trim()));
            if !resolved.is_dir() {
                return Some(Ok(json!({ "success": false, "error": "指定路径不是目录" })));
            }
            // 安全保护：禁止清空驱动器 / 文件系统根目录
            if resolved.parent().is_none() {
                return Some(Ok(json!({ "success": false, "error": "不允许清空根目录" })));
            }
            let entries = match std::fs::read_dir(&resolved) {
                Ok(entries) => entries,
                Err(err) => return Some(Ok(json!({ "success": false, "error": err.to_string() }))),
            };
            let mut removed = 0usize;
            for entry in entries.flatten() {
                let path = entry.path();
                let result = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match result {
                    Ok(_) => removed += 1,
                    Err(err) => {
                        return Some(Ok(json!({ "success": false, "error": err.to_string() })))
                    }
                }
            }
            Some(Ok(json!({ "success": true, "removed": removed })))
        }

        _ => None,
    }
}
