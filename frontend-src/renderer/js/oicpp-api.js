/**
 * OICPP IDE 的应用 API 层（Tauri 原生）。
 *
 * 背景：移植初期为了复用原 Electron 渲染层，运行期注入了一层「模拟 Electron」的 shim
 * （`window.oicppApi` / `ipcRenderer` / `process` 等）。那是权宜之计，副作用是调试时
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
        pickFolder: 'open-folder-dialog',
        openCompilerSettings: 'open-compiler-settings',
        fetchJson: 'fetch-json',
        readDirectory: 'read-directory',
        ensureDirectory: 'ensure-directory',
        walkDirectory: 'walk-directory',
        readFileContent: 'read-file-content'
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
        },
        settings: {
            /// 打开「编译器设置」窗口（由 Rust 侧创建独立窗口）
            openCompilerSettings: function () {
                return send(CHANNELS.openCompilerSettings, []);
            }
        },
        http: {
            /**
             * 取外部接口的 JSON。由 Rust 侧代发请求 —— 渲染层直接 fetch 会被 WebView2 的
             * CORS 拦掉（原版 Electron 靠 `webSecurity: false` 绕过，Tauri 没有等价开关），
             * 表现为 `Failed to fetch`，且请求根本发不出去（后端无任何痕迹）。
             */
            getJson: function (url) {
                return invoke(CHANNELS.fetchJson, [url]);
            }
        },
        fs: {
            /// 读目录（工作区选择器用，返回 {name, path, type} 列表）
            readDirectory: function (dirPath) {
                return invoke(CHANNELS.readDirectory, [dirPath]);
            },
            /// 确保目录存在（新建文件夹）
            ensureDirectory: function (dirPath) {
                return invoke(CHANNELS.ensureDirectory, [dirPath]);
            },
            /// 递归遍历工作区（快速打开文件用）
            walkDirectory: function (root, options) {
                return invoke(CHANNELS.walkDirectory, [root, options]);
            },
            /// 读文本文件内容
            readFile: function (filePath) {
                return invoke(CHANNELS.readFileContent, [filePath]);
            }
        },
        /// 原始通道发送（不等结果）。给需要直达 Rust 通道的场景用，例如调试面板上报状态。
        send: function (channel, args) {
            return send(channel, args);
        },

        // 内置浏览器：Rust 子 WebView 控制。
        browserViewCreate: function (options) {
            return invoke('browser-webview-create', [options]);
        },
        browserViewSetBounds: function (options) {
            return invoke('browser-webview-set-bounds', [options]);
        },
        browserViewSetVisible: function (options) {
            return invoke('browser-webview-set-visible', [options]);
        },
        browserViewState: function (options) {
            return invoke('browser-webview-state', [options]);
        },
        browserViewNavigate: function (options) {
            return invoke('browser-webview-navigate', [options]);
        },
        browserViewAction: function (options) {
            return invoke('browser-webview-action', [options]);
        },
        browserViewClose: function (options) {
            return invoke('browser-webview-close', [options]);
        },

        // clangd JSON-RPC。
        lspStart: function (options) {
            return invoke('lsp-start', [options]);
        },
        lspStop: function () {
            return invoke('lsp-stop', []);
        },
        lspRestart: function (options) {
            return invoke('lsp-restart', [options]);
        },
        lspRequest: function (method, params) {
            return invoke('lsp-request', [method, params]);
        },
        lspNotify: function (method, params) {
            return invoke('lsp-notify', [method, params]);
        }
    };

    /**
     * 静态 HTML 里的按钮用 `data-oicpp-action="<动作名>"` 声明行为，由这里统一分发。
     * 不用内联 onclick：Tauri 注入的 CSP 会让内联处理器静默失效（点击完全无响应、
     * 连异常都没有），排查成本很高。
     */
    var ACTIONS = {
        'compiler.openSettings': function () {
            return window.oicppApi.settings.openCompilerSettings();
        },
        'folder.open': function () {
            return window.oicppApi.dialog.pickFolder();
        },
        // 样例测试面板（原内联 onchange/onclick 迁移过来）
        'sampleTester.updateTestlib': function (element) {
            return callSampleTester('updateGlobalSetting', ['useTestlib', element.checked]);
        },
        'sampleTester.updateInteractive': function (element) {
            return callSampleTester('updateGlobalSetting', ['useInteractive', element.checked]);
        },
        'sampleTester.updateSpjPath': function (element) {
            return callSampleTester('updateGlobalSetting', ['spjPath', element.value]);
        },
        'sampleTester.clearGraderFile': function () {
            return callSampleTester('clearGlobalGraderFile', []);
        },
        'sampleTester.selectSpjFile': function () {
            return callSampleTester('selectGlobalSpjFile', []);
        },
        'sampleTester.clearSpjFile': function () {
            return callSampleTester('clearGlobalSpjFile', []);
        }
    };

    /// 调用渲染层既有的 sampleTester 管理器；不可用时给出明确错误而不是 TypeError
    function callSampleTester(method, args) {
        var manager = window.sampleTester;
        if (!manager || typeof manager[method] !== 'function') {
            throw new Error('sampleTester.' + method + ' 不可用');
        }
        return manager[method].apply(manager, args);
    }

    function dispatchAction(event) {
        var target = event.target;
        if (!target || typeof target.closest !== 'function') {
            return;
        }
        var trigger = target.closest('[data-oicpp-action]');
        if (!trigger) {
            return;
        }
        var action = ACTIONS[trigger.getAttribute('data-oicpp-action')];
        if (!action) {
            return;
        }

        var tag = trigger.tagName;
        var isFormControl = tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA';
        if (isFormControl) {
            // 表单控件只在 change 阶段处理：勾选/输入是默认行为、发生在事件派发之后，
            // 所以在 click 阶段读到的是旧值；而且绝不能 preventDefault，否则勾选状态不变
            if (event.type !== 'change') {
                return;
            }
        } else if (tag === 'A') {
            // 链接需要阻止默认跳转（对应原内联处理器的 `return false`）
            event.preventDefault();
        }

        try {
            action(trigger);
        } catch (error) {
            // 静默失败是这轮排查最耗时的地方，这里显式记录
            if (window.logError) {
                window.logError('[oicppApi] 动作执行失败: ' + trigger.getAttribute('data-oicpp-action') + ' -> ' + error);
            }
        }
    }

    // 静态元素既可能用 click（按钮/链接）也可能用 change（输入框）
    ['click', 'change'].forEach(function (type) {
        document.addEventListener(type, dispatchAction);
    });
})();
