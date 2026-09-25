//! 原生菜单栏（快捷键入口）。
//!
//! Electron 版由主进程构建原生菜单，菜单项通过 `webContents.send('menu-*')`
//! 通知渲染进程；这里用同样的事件名广播，渲染层的既有监听即可复用。

use tauri::menu::{
    IsMenuItem, MenuBuilder, MenuItem, MenuItemBuilder, PredefinedMenuItem, SubmenuBuilder,
};
use tauri::{AppHandle, Manager, Wry};

use crate::settings;

fn kb(key: &str, fallback: &str) -> Option<String> {
    let value = settings::get_value("keybindings")?;
    let raw = value.get(key).and_then(|v| v.as_str()).unwrap_or(fallback);
    Some(raw.to_string())
}

fn item(app: &AppHandle<Wry>, id: &str, label: &str, accel: Option<&str>) -> MenuItem<Wry> {
    if let Some(accel) = accel {
        if !accel.is_empty() {
            let mut builder = MenuItemBuilder::with_id(id, label);
            builder = builder.accelerator(accel);
            if let Ok(item) = builder.build(app) {
                return item;
            }
        }
    }
    MenuItemBuilder::with_id(id, label)
        .build(app)
        .expect("构建菜单项失败")
}

pub fn rebuild(app: &AppHandle) {
    let app = app.clone();
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    let _ = window.set_menu(build(&app));
}

pub fn build(app: &AppHandle<Wry>) -> tauri::menu::Menu<Wry> {
    let sep = || PredefinedMenuItem::separator(app);

    let file_menu = SubmenuBuilder::new(app, "文件")
        .item(&item(app, "menu-new-file", "新建文件", Some("CmdOrCtrl+N")))
        .item(&item(
            app,
            "menu-new-temp-file",
            "新建临时文件",
            Some("CmdOrCtrl+Shift+N"),
        ))
        .item(&sep())
        .item(&item(
            app,
            "menu-open-folder",
            "打开文件夹",
            Some("CmdOrCtrl+K"),
        ))
        .item(&item(
            app,
            "menu-open-file-history",
            "历史记录",
            None,
        ))
        .item(&sep())
        .item(&item(app, "menu-save-file", "保存", Some("CmdOrCtrl+S")))
        .item(&item(
            app,
            "menu-save-as",
            "另存为",
            Some("CmdOrCtrl+Shift+S"),
        ))
        .item(&sep())
        .item(&PredefinedMenuItem::quit(app, Some("退出")))
        .build()
        .expect("构建文件菜单失败");

    let edit_menu = SubmenuBuilder::new(app, "编辑")
        .item(&PredefinedMenuItem::undo(app, Some("撤销")))
        .item(&PredefinedMenuItem::redo(app, Some("重做")))
        .item(&sep())
        .item(&PredefinedMenuItem::cut(app, Some("剪切")))
        .item(&PredefinedMenuItem::copy(app, Some("复制")))
        .item(&PredefinedMenuItem::paste(app, Some("粘贴")))
        .item(&PredefinedMenuItem::select_all(app, Some("全选")))
        .item(&sep())
        .item(&item(
            app,
            "menu-format-code",
            "格式化代码",
            kb("formatCode", "Alt+Shift+S").as_deref(),
        ))
        .item(&item(
            app,
            "menu-find-replace",
            "查找 / 替换",
            Some("CmdOrCtrl+F"),
        ))
        .build()
        .expect("构建编辑菜单失败");

    let run_menu = SubmenuBuilder::new(app, "运行")
        .item(&item(
            app,
            "menu-compile",
            "编译",
            kb("compileCode", "F9").as_deref(),
        ))
        .item(&item(
            app,
            "menu-compile-run",
            "编译并运行",
            kb("compileAndRun", "F11").as_deref(),
        ))
        .item(&item(
            app,
            "menu-debug",
            "调试",
            kb("toggleDebug", "F5").as_deref(),
        ))
        .build()
        .expect("构建运行菜单失败");

    let view_menu = SubmenuBuilder::new(app, "视图")
        .item(&item(
            app,
            "menu-open-terminal",
            "终端",
            kb("openTerminal", "").as_deref(),
        ))
        .item(&sep())
        .item(&item(app, "menu-open-browser", "内置浏览器", None))
        .item(&item(app, "menu-new-browser-tab", "新建浏览器标签页", None))
        .build()
        .expect("构建视图菜单失败");

    let help_menu = SubmenuBuilder::new(app, "帮助")
        .item(&item(app, "menu-settings", "设置", Some("CmdOrCtrl+,")))
        .item(&item(app, "menu-check-updates", "检查更新", None))
        .item(&sep())
        .item(&item(app, "menu-about", "关于 OICPP IDE", None))
        .build()
        .expect("构建帮助菜单失败");

    MenuBuilder::new(app)
        .items(&[
            &file_menu as &dyn IsMenuItem<Wry>,
            &edit_menu as &dyn IsMenuItem<Wry>,
            &run_menu as &dyn IsMenuItem<Wry>,
            &view_menu as &dyn IsMenuItem<Wry>,
            &help_menu as &dyn IsMenuItem<Wry>,
        ])
        .build()
        .expect("构建菜单栏失败")
}

/// 菜单项 id -> 渲染进程事件通道
pub fn channel_of(menu_id: &str) -> Option<&'static str> {
    Some(match menu_id {
        "menu-new-file" => "menu-new-file",
        "menu-new-temp-file" => "menu-new-temp-file",
        "menu-open-folder" => "menu-open-folder",
        "menu-open-file-history" => "menu-open-file-history",
        "menu-save-file" => "menu-save-file",
        "menu-save-as" => "menu-save-as",
        "menu-format-code" => "menu-format-code",
        "menu-find-replace" => "menu-find-replace",
        "menu-compile" => "menu-compile",
        "menu-compile-run" => "menu-compile-run",
        "menu-debug" => "menu-debug",
        "menu-open-terminal" => "menu-open-terminal",
        "menu-open-browser" => "menu-open-browser",
        "menu-new-browser-tab" => "menu-new-browser-tab",
        "menu-settings" => "menu-settings",
        "menu-check-updates" => "menu-check-updates",
        "menu-about" => "menu-about",
        "menu-open-file" => "menu-open-file",
        "menu-open-file-history-request" => "menu-open-file-history",
        _ => return None,
    })
}
