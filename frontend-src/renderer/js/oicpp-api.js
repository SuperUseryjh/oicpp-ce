/**
 * OICPP IDE 的应用 API 层（Tauri 原生）。
 *
 * 背景：移植初期为了复用原 Electron 渲染层，运行期注入了一层「模拟 Electron」的 shim
 * （`window.electronAPI` / `ipcRenderer` / `process` 等）。那是权宜之计，副作用是调试时
 * 要先判断「这个行为是原代码的问题，还是 shim 没模拟对」。
 *
 * 从现在起逐模块迁移：渲染层直接调用本模块，不再经过 Electron 模拟层。
 * 迁移期的约定：
 *   1. 被迁移的模块与新增代码只使用 `window.oicppApi`；
 *   2. 尚未迁移的模块继续走 shim，两者可以共存；
 *   3. 本模块内部只做一件事 —— 把调用落到 Rust 侧。
 *
 * 说明：Rust 侧目前仍是「按 channel 分发」的结构（`ipc_invoke` + 通道名），所以下面
 * 用常量集中列出通道名。将来 Rust 侧改成具名命令时，只需改本文件，调用方不受影响。
 */
(function () {
    'use strict';

    var CHANNELS = {
        pickFolder: 'open-folder-dialog'
    };

    function core() {
        return window.__TAURI__ && window.__TAURI__.core;
    }

    /// 需要等结果的调用
    function invoke(channel, args) {
        var api = core();
        if (!api || typeof api.invoke !== 'function') {
            return Promise.reject(new Error('Tauri API 不可用（withGlobalTauri 未开启？）'));
        }
        return api.invoke('ipc_invoke', { channel: channel, args: args || [] });
    }

    /// 只发不等结果的调用（结果通过事件回推）
    function send(channel, args) {
        var api = core();
        if (!api || typeof api.invoke !== 'function') {
            return;
        }
        try {
            var pending = api.invoke('ipc_send', { channel: channel, args: args || [] });
            if (pending && typeof pending.catch === 'function') {
                pending.catch(function () { });
            }
        } catch (_) { }
    }

    window.oicppApi = {
        dialog: {
            /**
             * 弹出文件夹选择框。Rust 侧选中后会写入工作区设置，并通过 `folder-opened`
             * 事件把路径回推给渲染层（渲染层用 onFolderOpened 接收）。
             */
            pickFolder: function () {
                return send(CHANNELS.pickFolder, []);
            }
        }
    };
})();
