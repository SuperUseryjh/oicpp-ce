//! 原生对话框封装（异步 + oneshot，避免阻塞主线程）

use std::path::PathBuf;

use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

#[derive(Default, Clone)]
pub struct OpenOptions {
    pub title: Option<String>,
    pub directory: bool,
    pub multiple: bool,
    pub default_path: Option<String>,
    pub filters: Vec<(String, Vec<String>)>,
}

pub async fn pick_open(app: &AppHandle, opts: OpenOptions) -> Vec<PathBuf> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<Vec<PathBuf>>>();
    let mut builder = app.dialog().file();
    if let Some(title) = opts.title.clone() {
        builder = builder.set_title(title);
    }
    if let Some(default_path) = opts.default_path.clone() {
        builder = builder.set_directory(PathBuf::from(default_path));
    }
    for (name, exts) in opts.filters.clone() {
        let refs: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();
        builder = builder.add_filter(name, &refs);
    }
    let multiple = opts.multiple;
    let directory = opts.directory;
    let sender = std::sync::Arc::new(std::sync::Mutex::new(Some(tx)));
    let send = move |value: Option<Vec<PathBuf>>| {
        if let Ok(mut guard) = sender.lock() {
            if let Some(tx) = guard.take() {
                let _ = tx.send(value);
            }
        }
    };
    if directory {
        if multiple {
            let send_ok = send.clone();
            builder.pick_folders(move |paths| {
                send_ok(paths.map(|list| list.into_iter().filter_map(path_of).collect()));
            });
        } else {
            let send_ok = send.clone();
            builder.pick_folder(move |path| {
                send_ok(path.map(|p| path_of(p).into_iter().collect()));
            });
        }
    } else if multiple {
        builder.pick_files(move |paths| {
            send(paths.map(|list| list.into_iter().filter_map(path_of).collect()));
        });
    } else {
        builder.pick_file(move |path| {
            send(path.map(|p| path_of(p).into_iter().collect()));
        });
    }
    rx.await.ok().flatten().unwrap_or_default()
}

pub async fn pick_save(
    app: &AppHandle,
    title: Option<&str>,
    default_name: Option<&str>,
    filters: Vec<(String, Vec<String>)>,
) -> Option<PathBuf> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<PathBuf>>();
    let mut builder = app.dialog().file();
    if let Some(title) = title {
        builder = builder.set_title(title);
    }
    if let Some(name) = default_name {
        builder = builder.set_file_name(name);
    }
    for (name, exts) in filters {
        let refs: Vec<&str> = exts.iter().map(|s| s.as_str()).collect();
        builder = builder.add_filter(name, &refs);
    }
    builder.save_file(move |path| {
        let _ = tx.send(path.and_then(path_of));
    });
    rx.await.ok().flatten()
}

pub async fn confirm(
    app: &AppHandle,
    title: &str,
    message: &str,
    yes: &str,
    no: &str,
) -> bool {
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    app.dialog()
        .message(message.to_string())
        .title(title.to_string())
        .buttons(vec![yes.to_string(), no.to_string()])
        .show(move |answer| {
            let _ = tx.send(answer.to_string() == yes);
        });
    rx.await.unwrap_or(false)
}

pub async fn message(app: &AppHandle, title: &str, body: &str) {
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    app.dialog()
        .message(body.to_string())
        .title(title.to_string())
        .show(move |_| {
            let _ = tx.send(());
        });
    let _ = rx.await;
}

fn path_of(path: tauri_plugin_dialog::FilePath) -> Option<PathBuf> {
    path.into_path().ok()
}
