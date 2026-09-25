/**
 * OICPP Tauri 兼容层
 * ---------------------------------------------------------------------------
 * 原项目是 Electron 应用，渲染进程通过 preload.js 暴露的 `window.electronAPI` /
 * `window.electron` / `window.electronIPC` / `window.markdownAPI` 等全局对象与主进程通信。
 *
 * 该脚本由 Rust 侧通过 `initialization_script` 注入到每个 WebView（在页面任何脚本之前执行），
 * 用 Tauri 的 IPC 复刻同一套 API，从而让 oicpp-origin 的渲染层代码无需改动即可运行。
 *
 * 后端契约（与 src-tauri/src/ipc.rs 对应）：
 *   ipc_invoke(channel: string, args: any[]) -> any      请求/响应
 *   ipc_send(channel: string, args: any[]) -> void       即发即忘
 *   事件：Tauri 事件名 'oicpp'，payload = { channel: string, args: any[] }
 */
(function () {
    'use strict';

    if (window.__OICPP_TAURI_SHIM__) return;
    window.__OICPP_TAURI_SHIM__ = true;

    var ENV = window.__OICPP_ENV__ || {};
    var EVENT_NAME = 'oicpp';

    // 设置窗口的 query 参数（Tauri 自定义协议下 location.search 可能为空，由 prepare-frontend 注入的回退读取）
    if (ENV.queryString) {
        window.__OICPP_QUERY_STRING__ = ENV.queryString;
    }

    // ---------------------------------------------------------------- 本地文件资源
    // WebView 不允许从 http 源加载 file:// 子资源（Electron 下没有这个限制）。
    // 用户自定义背景图恰好走 file:// 路径（渲染层把绝对路径拼成 file:///…），
    // 因此在这里把写进 CSS 的 file:// 地址改写成 Tauri 的 asset 协议地址
    // （tauri.conf.json 已开启 assetProtocol，scope 为 **）。
    var ASSET_ORIGIN = ENV.assetOrigin || 'asset://localhost';

    function rewriteLocalFileUrls(value) {
        if (typeof value !== 'string' || value.indexOf('file://') === -1) {
            return value;
        }
        return value.replace(/file:\/\/+/g, ASSET_ORIGIN + '/');
    }

    (function patchStyleUrls() {
        if (typeof CSSStyleDeclaration === 'undefined') {
            return;
        }
        var proto = CSSStyleDeclaration.prototype;

        ['backgroundImage', 'background'].forEach(function (prop) {
            var descriptor = Object.getOwnPropertyDescriptor(proto, prop);
            if (!descriptor || typeof descriptor.set !== 'function') {
                return;
            }
            Object.defineProperty(proto, prop, {
                configurable: true,
                enumerable: descriptor.enumerable,
                get: descriptor.get,
                set: function (value) {
                    descriptor.set.call(this, rewriteLocalFileUrls(value));
                }
            });
        });

        var originalSetProperty = proto.setProperty;
        proto.setProperty = function (name, value, priority) {
            return originalSetProperty.call(this, name, rewriteLocalFileUrls(value), priority);
        };
    })();

    // ---------------------------------------------------------------- 诊断
    // 资源加载失败（404）不会冒泡到 window.onerror，而在 Tauri 下 vendor/ 资源
    // 路径出问题正是典型症状。这里在捕获阶段监听，把失败项写进应用日志
    // （~/.oicpp/logs），方便在没有 devtools 的正式包里定位。
    var DIAGNOSTIC_LIMIT = 50;
    var diagnosticCount = 0;

    function reportDiagnostic(message) {
        if (diagnosticCount >= DIAGNOSTIC_LIMIT) {
            return;
        }
        diagnosticCount += 1;
        try {
            var t = window.__TAURI__;
            if (!t || !t.core || typeof t.core.invoke !== 'function') {
                return;
            }
            var pending = t.core.invoke('ipc_send', {
                channel: 'logger-log',
                args: [{ level: 'error', args: [message] }]
            });
            // 必须吞掉失败，否则 invoke 被拒 → unhandledrejection → 递归上报
            if (pending && typeof pending.catch === 'function') {
                pending.catch(function () { });
            }
        } catch (_) { }
    }

    window.addEventListener('error', function (event) {
        var target = event && event.target;
        if (target && target !== window && target.tagName) {
            reportDiagnostic('[资源加载失败] ' + target.tagName.toLowerCase() + ' ' + (target.src || target.href || ''));
        }
    }, true);

    window.addEventListener('unhandledrejection', function (event) {
        var reason = event && event.reason;
        reportDiagnostic('[未处理的 Promise 拒绝] ' + ((reason && reason.message) ? reason.message : String(reason)));
    });

    // ---------------------------------------------------------------- 基础桥接
    function tauriCore() {
        var t = window.__TAURI__;
        return t && t.core ? t.core : null;
    }

    function rawInvoke(command, payload) {
        var core = tauriCore();
        if (!core || typeof core.invoke !== 'function') {
            return Promise.reject(new Error('Tauri IPC 不可用'));
        }
        return core.invoke(command, payload);
    }

    function ipcInvoke(channel, args) {
        try {
            return rawInvoke('ipc_invoke', { channel: channel, args: args || [] });
        } catch (err) {
            return Promise.reject(err);
        }
    }

    function ipcSend(channel, args) {
        try {
            var p = rawInvoke('ipc_send', { channel: channel, args: args || [] });
            if (p && typeof p.catch === 'function') p.catch(function () { });
        } catch (_) { }
    }

    // ---------------------------------------------------------------- 事件分发
    var listeners = Object.create(null);

    function addListener(channel, listener) {
        if (typeof listener !== 'function') return function () { };
        if (!listeners[channel]) listeners[channel] = [];
        listeners[channel].push(listener);
        return function () {
            removeListener(channel, listener);
        };
    }

    function removeListener(channel, listener) {
        var list = listeners[channel];
        if (!list) return;
        var idx = list.indexOf(listener);
        if (idx >= 0) list.splice(idx, 1);
    }

    function removeAllListeners(channel) {
        if (channel === undefined) listeners = Object.create(null);
        else delete listeners[channel];
    }

    function makeEventObject(channel) {
        return {
            channel: channel,
            sender: {
                id: 0,
                send: function (ch) { ipcSend(ch, Array.prototype.slice.call(arguments, 1)); },
                isDestroyed: function () { return false; }
            },
            preventDefault: function () { },
            reply: function (ch) { }
        };
    }

    function dispatchEvent(payload) {
        if (!payload || typeof payload !== 'object') return;
        var channel = payload.channel;
        var args = Array.isArray(payload.args) ? payload.args : [];
        var list = listeners[channel];
        if (!list || !list.length) return;
        var event = makeEventObject(channel);
        // 复制一份，避免回调内部增删监听造成遍历异常
        for (var fn of list.slice()) {
            try {
                fn.apply(null, [event].concat(args));
            } catch (err) {
                try { console.error('[oicpp-shim] listener error on', channel, err); } catch (_) { }
            }
        }
    }

    (function setupEventBridge() {
        var t = window.__TAURI__;
        if (!t || !t.event || typeof t.event.listen !== 'function') {
            setTimeout(setupEventBridge, 20);
            return;
        }
        t.event.listen(EVENT_NAME, function (e) { dispatchEvent(e && e.payload); }).catch(function () { });
    })();

    // ---------------------------------------------------------------- 日志
    function sanitize(value, depth) {
        depth = depth || 0;
        try {
            if (value === null || value === undefined) return value;
            var type = typeof value;
            if (type === 'string' || type === 'number' || type === 'boolean') return value;
            if (type === 'function') return '[Function]';
            if (type === 'bigint') return value.toString();
            if (value instanceof Error) return { name: value.name, message: value.message, stack: value.stack };
            if (depth > 3) return '[Object]';
            if (Array.isArray(value)) return value.slice(0, 50).map(function (v) { return sanitize(v, depth + 1); });
            if (typeof value === 'object') {
                var out = {};
                var keys = Object.keys(value).slice(0, 50);
                for (var k of keys) {
                    try { out[k] = sanitize(value[k], depth + 1); } catch (_) { out[k] = '[Unserializable]'; }
                }
                return out;
            }
            return String(value);
        } catch (_) {
            return '[Unserializable]';
        }
    }

    function sendLog(level, args) {
        try {
            var meta;
            if (level === 'warn' || level === 'error') {
                meta = {
                    source: 'renderer',
                    ts: Date.now(),
                    userAgent: typeof navigator !== 'undefined' ? navigator.userAgent : undefined,
                    stack: (function () { try { throw new Error('__trace__'); } catch (e) { return e.stack; } })()
                };
            }
            var payload = { level: level, args: Array.prototype.map.call(args, function (a) { return sanitize(a); }) };
            if (meta) payload.meta = sanitize(meta);
            ipcSend('logger-log', [JSON.parse(JSON.stringify(payload))]);
        } catch (_) { }
    }

    window.logInfo = function () { sendLog('info', arguments); };
    window.logWarn = function () { sendLog('warn', arguments); };
    window.logError = function () { sendLog('error', arguments); };
    window.logwarn = window.logWarn;
    window.logerror = window.logError;

    // ---------------------------------------------------------------- Buffer 垫片
    function base64FromBytes(bytes) {
        var bin = '';
        for (var i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
        return btoa(bin);
    }

    function MiniBuffer(input) {
        if (typeof input === 'number') {
            this.bytes = new Uint8Array(input);
        } else if (ArrayBuffer.isView(input)) {
            this.bytes = new Uint8Array(input.buffer, input.byteOffset, input.byteLength);
        } else if (input instanceof ArrayBuffer) {
            this.bytes = new Uint8Array(input);
        } else if (Array.isArray(input)) {
            this.bytes = new Uint8Array(input);
        } else {
            this.bytes = new Uint8Array(0);
        }
    }
    MiniBuffer.from = function (input, encoding) {
        if (typeof input === 'string') {
            if (encoding === 'base64') {
                var bin = atob(input);
                var arr = new Uint8Array(bin.length);
                for (var i = 0; i < bin.length; i++) arr[i] = bin.charCodeAt(i);
                return new MiniBuffer(arr);
            }
            return new MiniBuffer(new TextEncoder().encode(input));
        }
        return new MiniBuffer(input);
    };
    MiniBuffer.prototype.toString = function (encoding) {
        if (encoding === 'base64') return base64FromBytes(this.bytes);
        if (encoding === 'hex') {
            var hex = '';
            for (var i = 0; i < this.bytes.length; i++) hex += this.bytes[i].toString(16).padStart(2, '0');
            return hex;
        }
        return new TextDecoder().decode(this.bytes);
    };
    MiniBuffer.prototype.toJSON = function () {
        return { type: 'Buffer', data: Array.prototype.slice.call(this.bytes) };
    };
    window.Buffer = MiniBuffer;

    // ---------------------------------------------------------------- process
    var platform = ENV.platform || 'linux';
    window.process = {
        platform: platform,
        versions: {
            tauri: (ENV.tauriVersion || '2'),
            node: '',
            chrome: (function () {
                var m = navigator.userAgent.match(/Chrome\/([\d.]+)/);
                return m ? m[1] : '';
            })(),
            electron: undefined
        },
        env: {
            NODE_ENV: ENV.debug ? 'development' : 'production',
            CI: false,
            HOME: ENV.home || '',
            USERPROFILE: ENV.home || ''
        }
    };

    // ---------------------------------------------------------------- electronAPI
    function sendAndForget(channel) {
        return function () { ipcSend(channel, Array.prototype.slice.call(arguments)); };
    }

    function makeInvoker(channel, argCount) {
        return function () {
            var args = Array.prototype.slice.call(arguments);
            if (typeof argCount === 'number') args = args.slice(0, argCount);
            return ipcInvoke(channel, args);
        };
    }

    function onTransform(channel, transform) {
        return function (callback) {
            return addListener(channel, function (event) {
                var args = Array.prototype.slice.call(arguments, 1);
                return callback.apply(null, transform ? transform.apply(null, [event].concat(args)) : args);
            });
        };
    }

    function onRaw(channel) {
        return function (callback) {
            return addListener(channel, callback);
        };
    }

    window.electronAPI = {
        // ---- 文件 / 对话框
        openFile: sendAndForget('open-file-dialog'),
        openFolder: sendAndForget('open-folder-dialog'),
        saveFile: makeInvoker('save-file'),
        saveAsFile: makeInvoker('save-as-file'),
        readFileContent: makeInvoker('read-file-content'),
        readFileBuffer: makeInvoker('read-file-buffer'),
        readZipTextFiles: makeInvoker('read-zip-text-files'),
        showItemInFolder: function (filePath) { return ipcInvoke('open-path', [filePath, { reveal: true }]); },
        openPath: function (targetPath, options) { return ipcInvoke('open-path', [targetPath, options || {}]); },
        showOpenDialog: makeInvoker('show-open-dialog'),
        showSaveDialog: makeInvoker('show-save-dialog'),

        // ---- 临时文件
        saveTempFile: makeInvoker('save-temp-file'),
        saveBinaryTempFile: makeInvoker('save-binary-temp-file'),
        loadTempFile: makeInvoker('load-temp-file'),
        deleteTempFile: makeInvoker('delete-temp-file'),

        // ---- 设置
        getAllSettings: makeInvoker('get-all-settings'),
        getSettings: makeInvoker('get-settings'),
        sendSettingsPreview: function (settings) { ipcSend('settings-preview', [settings]); },
        updateSettings: makeInvoker('update-settings'),
        updateEditorSettings: makeInvoker('update-editor-settings'),
        resetSettings: makeInvoker('reset-settings'),
        exportSettings: makeInvoker('export-settings'),
        importSettings: makeInvoker('import-settings'),
        saveSetting: makeInvoker('save-setting'),

        openCompilerSettings: makeInvoker('open-compiler-settings'),
        openEditorSettings: makeInvoker('open-editor-settings'),
        openTemplateSettings: sendAndForget('open-template-settings'),
        openBackupSettings: makeInvoker('open-backup-settings'),

        // ---- 平台 / 构建信息
        getPlatform: makeInvoker('get-platform'),
        getUserHome: makeInvoker('get-user-home'),
        getUserIconPath: makeInvoker('get-user-icon-path'),
        getBuildInfo: makeInvoker('get-build-info'),

        // ---- 编译器 / testlib
        getDownloadedCompilers: makeInvoker('get-downloaded-compilers'),
        downloadCompiler: makeInvoker('download-compiler'),
        selectCompiler: makeInvoker('select-compiler'),
        getDownloadedTestlibs: makeInvoker('get-downloaded-testlibs'),
        downloadTestlib: makeInvoker('download-testlib'),
        selectTestlib: makeInvoker('select-testlib'),
        testTestlib: makeInvoker('test-testlib'),
        isTestlibSelected: makeInvoker('is-testlib-selected'),

        // ---- 编译 / 运行
        compileFile: makeInvoker('compile-file'),
        runExecutable: makeInvoker('run-executable'),
        runProgram: makeInvoker('run-program'),
        runInteractive: makeInvoker('run-interactive'),

        // ---- 文件系统
        readDirectory: makeInvoker('read-directory'),
        renameFile: makeInvoker('rename-file'),
        deleteFile: makeInvoker('delete-file'),
        clearDirectoryContents: makeInvoker('clear-directory-contents'),
        writeFile: makeInvoker('write-file'),
        createFile: makeInvoker('create-file'),
        createFolder: makeInvoker('create-folder'),
        checkFileExists: makeInvoker('check-file-exists'),
        getPathInfo: makeInvoker('get-path-info'),
        ensureDirectory: makeInvoker('ensure-directory'),
        watchFile: makeInvoker('watch-file'),
        unwatchFile: makeInvoker('unwatch-file'),

        pathJoin: makeInvoker('path-join'),
        pathDirname: makeInvoker('path-dirname'),
        getHomeDir: makeInvoker('get-home-dir'),
        ensureDir: makeInvoker('ensure-dir'),

        // ---- 终端
        getTerminalFeatureStatus: makeInvoker('terminal-feature-status'),
        createTerminal: makeInvoker('terminal-create'),
        writeTerminal: makeInvoker('terminal-write'),
        resizeTerminal: makeInvoker('terminal-resize'),
        killTerminal: makeInvoker('terminal-kill'),
        listTerminals: makeInvoker('terminal-list'),
        getTerminalTTY: makeInvoker('terminal-get-tty'),

        // ---- 菜单事件
        onMenuSaveFile: onRaw('menu-save-file'),
        onApplySettingsPreview: onTransform('apply-settings-preview', function () {
            return Array.prototype.slice.call(arguments, 1);
        }),
        onSettingsApplied: onTransform('settings-applied', function () { return Array.prototype.slice.call(arguments, 1); }),
        onMenuFormatCode: onRaw('menu-format-code'),
        onMenuFindReplace: onRaw('menu-find-replace'),
        onMenuCompile: onRaw('menu-compile'),
        onMenuCompileRun: onRaw('menu-compile-run'),
        onMenuDebug: onRaw('menu-debug'),
        onMenuNewTempFile: onRaw('menu-new-temp-file'),
        onMenuOpenFile: onRaw('menu-open-file'),
        onMenuOpenFolder: onRaw('menu-open-folder'),
        onMenuSaveAs: onRaw('menu-save-as'),
        onMenuOpenTerminal: onRaw('menu-open-terminal'),
        onMenuOpenBrowser: onRaw('menu-open-browser'),
        onMenuNewBrowserTab: onRaw('menu-new-browser-tab'),
        onMenuAbout: onRaw('menu-about'),
        onMenuSettings: onRaw('menu-settings'),
        onMenuCheckUpdates: onRaw('menu-check-updates'),
        getUpdateDownloadStatus: makeInvoker('get-update-download-status'),
        onUpdateDownloadStatus: onTransform('update-download-status', function () { return [arguments[1]]; }),
        onAppToast: onTransform('app-toast', function () { return [arguments[1]]; }),

        onShowDebugDevelopingMessage: onRaw('show-debug-developing-message'),
        onSettingsChanged: onRaw('settings-changed'),
        onSettingsReset: onRaw('settings-reset'),
        onSettingsImported: onRaw('settings-imported'),
        onThemeChanged: onRaw('theme-changed'),
        onFileOpened: onRaw('file-opened'),
        onFileSaved: onTransform('file-saved', function () { return [arguments[1], arguments[2]]; }),
        onFolderOpened: onTransform('folder-opened', function () { return [arguments[1]]; }),
        reportWorkspacePath: function (folderPath) { ipcSend('workspace-path-report', [folderPath]); },
        onFileOpenedFromArgs: onTransform('file-opened-from-args', function () { return [arguments[1]]; }),
        consumeStartupWorkspaceToOpen: makeInvoker('consume-startup-workspace-to-open'),
        onExternalFileChange: function (callback) {
            if (typeof callback !== 'function') return function () { };
            return addListener('external-file-changed', function (event) { callback(arguments[1]); });
        },
        onSampleTesterCreateProblem: onTransform('sample-tester-create-problem', function () { return [arguments[1]]; }),
        onTerminalData: onTransform('terminal-data', function () { return [arguments[1]]; }),
        onTerminalExit: onTransform('terminal-exit', function () { return [arguments[1]]; }),

        getCpuThreads: makeInvoker('get-cpu-threads'),

        sendFeedback: makeInvoker('send-feedback'),
        listClientLogs: makeInvoker('list-client-logs'),
        uploadClientLog: makeInvoker('upload-client-log'),
        getDeviceInfo: makeInvoker('get-device-info'),
        getEncodedToken: makeInvoker('get-encoded-token'),

        openExternal: makeInvoker('open-external'),

        getLanguage: makeInvoker('get-language'),
        // 语言包随前端一起分发到 /lang/，直接由 WebView 读取，无需经过 Rust
        getLanguageFile: function (langCode) {
            var code = langCode || 'zh-cn';
            return fetch('/lang/' + encodeURIComponent(code) + '.json', { cache: 'no-store' })
                .then(function (res) { return res.ok ? res.json() : null; })
                .catch(function () { return null; });
        },
        getAvailableLanguages: function () {
            return fetch('/lang/languages.json', { cache: 'no-store' })
                .then(function (res) { return res.ok ? res.json() : []; })
                .catch(function () { return []; });
        },
        onLanguageChanged: onTransform('language-changed', function () { return [arguments[1]]; }),

        startIdeLogin: makeInvoker('ide-login-start'),
        getIdeLoginStatus: makeInvoker('ide-login-status'),
        logoutIdeAccount: makeInvoker('ide-logout'),
        cloudSyncRequest: makeInvoker('cloud-sync-request'),
        backupSettingsToCloud: makeInvoker('backup-settings-to-cloud'),
        getSettingsBackupInfo: makeInvoker('get-settings-backup-info'),
        syncSettingsFromCloud: makeInvoker('sync-settings-from-cloud'),
        onIdeLoginUpdated: onTransform('ide-login-updated', function () { return [arguments[1]]; }),
        onIdeLoginError: onTransform('ide-login-error', function () { return [arguments[1]]; }),

        getRecentFiles: makeInvoker('get-recent-files'),
        openRecentFile: makeInvoker('open-recent-file'),

        getFileHistory: makeInvoker('get-file-history'),
        addToFileHistory: makeInvoker('add-to-file-history'),
        openFileFromHistory: makeInvoker('open-file-from-history'),
        clearFileHistory: makeInvoker('clear-file-history'),
        saveLastOpenTabs: makeInvoker('save-last-open-tabs'),
        getLastOpenTabs: makeInvoker('get-last-open-tabs'),
        onMenuOpenFileHistory: onRaw('menu-open-file-history'),

        versions: window.process.versions,

        relaunchApp: makeInvoker('relaunch-app'),

        clipboardWriteText: makeInvoker('clipboard-write-text'),
        clipboardReadText: makeInvoker('clipboard-read-text'),

        walkDirectory: makeInvoker('walk-directory'),

        lspStart: makeInvoker('lsp-start'),
        lspStop: makeInvoker('lsp-stop'),
        lspRestart: makeInvoker('lsp-restart'),
        lspRequest: makeInvoker('lsp-request'),
        lspNotify: makeInvoker('lsp-notify'),
        onLspNotification: onTransform('lsp-notification', function () { return [arguments[1]]; }),

        onRequestSaveAll: function (callback) {
            return addListener('request-save-all', function () { if (typeof callback === 'function') callback(); });
        },
        notifySaveAllComplete: function () { ipcSend('save-all-complete', []); },

        // ---- 内置浏览器
        browserResolveUrl: makeInvoker('browser-resolve-url'),
        browserGetPageTitle: makeInvoker('browser-get-page-title'),
        // Tauri 版内置浏览器：主窗口内嵌多 webview（替代 Electron 的 <webview> 标签）
        browserViewCreate: makeInvoker('browser-webview-create'),
        browserViewSetBounds: makeInvoker('browser-webview-set-bounds'),
        browserViewNavigate: makeInvoker('browser-webview-navigate'),
        browserViewAction: makeInvoker('browser-webview-action'),
        browserViewSetVisible: makeInvoker('browser-webview-set-visible'),
        browserViewState: makeInvoker('browser-webview-state'),
        browserViewClose: makeInvoker('browser-webview-close'),
        browserViewList: makeInvoker('browser-webview-list'),
        onBrowserViewEvent: onTransform('browser-webview-event', function () { return [arguments[1]]; }),
        onBrowserOpenNewTab: function (callback) {
            if (typeof callback !== 'function') return function () { };
            return addListener('browser-open-new-tab', function (event) { callback(arguments[1]); });
        },

        // ---- 调试（渲染层有一处直接使用 electronAPI.* 的兼容入口）
        onDebugOutput: onRaw('debug-output'),
        onDebugError: onRaw('debug-error'),
        onDebugStopped: onRaw('debug-stopped'),
        onDebugRunning: onRaw('debug-running'),
        onDebugTerminalOutput: onRaw('debug-terminal-output'),
        onDebugVariablesUpdated: onRaw('debug-variables-updated'),
        onDebugCallstackUpdated: onRaw('debug-callstack-updated'),
        onDebugBreakpointSet: onRaw('debug-breakpoint-set'),
        onDebugBreakpointRemoved: onRaw('debug-breakpoint-removed'),
        onDebugBreakpointHit: onRaw('debug-breakpoint-hit'),
        onDebugVariableExpanded: onRaw('debug-variable-expanded'),
        onDebugProgramExited: onRaw('debug-program-exited'),
        onDebugReadyWaiting: onRaw('debug-ready-waiting'),
        onGotoSourceLocation: onRaw('goto-source-location')
    };

    // ---------------------------------------------------------------- electronIPC
    function makeIpcRenderer() {
        return {
            send: function (channel) { ipcSend(channel, Array.prototype.slice.call(arguments, 1)); },
            invoke: function (channel) { return ipcInvoke(channel, Array.prototype.slice.call(arguments, 1)); },
            on: function (channel, listener) { return addListener(channel, listener); },
            once: function (channel, listener) {
                if (typeof listener !== 'function') return function () { };
                var off = addListener(channel, function () {
                    var args = arguments;
                    off();
                    return listener.apply(null, args);
                });
                return off;
            },
            removeListener: function (channel, listener) { removeListener(channel, listener); },
            removeAllListeners: function (channel) { removeAllListeners(channel); }
        };
    }

    var ipcRenderer = makeIpcRenderer();
    window.electronIPC = Object.assign({}, ipcRenderer, { ipcRenderer: ipcRenderer });

    var shellShim = {
        openExternal: function (url) { return ipcInvoke('open-external', [url]); },
        showItemInFolder: function (p) { return ipcInvoke('open-path', [p, { reveal: true }]); },
        openPath: function (p) { return ipcInvoke('open-path', [p, {}]); }
    };

    window.electron = { ipcRenderer: ipcRenderer, shell: shellShim };

    window.__electronRequireAvailable = true;
    window.getElectronModule = function () {
        return { ipcRenderer: ipcRenderer, shell: shellShim };
    };

    // ---------------------------------------------------------------- 窗口拖动
    // 原实现依赖 Electron 的 `-webkit-app-region: drag`，WebView2 / WKWebView 不生效，
    // 这里按同样的语义（标题栏空白区域可拖动、交互元素除外）调用 Tauri 的 startDragging。
    try {
        document.addEventListener('mousedown', function (ev) {
            if (ev.button !== 0) return;
            var target = ev.target;
            if (!(target instanceof HTMLElement)) return;
            if (target.closest('.menu-bar, .titlebar-controls, button, input, select, textarea, a[href], .no-drag')) {
                return;
            }
            if (!target.closest('.titlebar')) return;
            try {
                var t = window.__TAURI__;
                if (t && t.window && typeof t.window.getCurrentWindow === 'function') {
                    t.window.getCurrentWindow().startDragging();
                }
            } catch (_) { }
        }, true);
    } catch (_) { }

    // ---------------------------------------------------------------- 剪贴板 / 外链全局处理（对应 preload.js）
    function showToast(message, type, durationMs) {
        try {
            type = ['info', 'success', 'error', 'warning'].indexOf(type) >= 0 ? type : 'info';
            var dur = typeof durationMs === 'number' ? durationMs : 1200;
            var ensure = function () {
                var existing = document.querySelector('.message-toast');
                if (existing) return existing;
                var div = document.createElement('div');
                div.className = 'message-toast info';
                div.style.pointerEvents = 'none';
                document.body.appendChild(div);
                return div;
            };
            var show = function () {
                var toast = ensure();
                toast.className = 'message-toast ' + type;
                toast.textContent = String(message == null ? '' : message);
                toast.style.display = 'block';
                clearTimeout(toast.__hideTimer);
                toast.__hideTimer = setTimeout(function () { toast.style.display = 'none'; }, dur);
            };
            if (document.readyState === 'loading') document.addEventListener('DOMContentLoaded', show, { once: true });
            else show();
        } catch (_) { }
    }

    try {
        window.addEventListener('click', function (ev) {
            var target = ev.target;
            if (!(target instanceof HTMLElement)) return;

            var anchor = target.closest('a[href]');
            if (anchor instanceof HTMLAnchorElement) {
                var href = anchor.getAttribute('href') || '';
                if (href && href.charAt(0) !== '#' && !/^javascript:/i.test(href) && (ev.ctrlKey || ev.metaKey)) {
                    try {
                        var resolved = new URL(href, window.location.href);
                        if (['http:', 'https:', 'mailto:'].indexOf(resolved.protocol) < 0) return;
                        ev.preventDefault();
                        ev.stopPropagation();
                        ipcInvoke('open-external', [resolved.href]).catch(function () { });
                    } catch (err) {
                        console.error('Failed to open external link:', err);
                    }
                    return;
                }
            }

            var btn = target.closest('.copy-code-btn');
            if (!(btn instanceof HTMLElement)) return;
            var encoded = btn.getAttribute('data-code');
            if (!encoded) return;

            ev.preventDefault();
            ev.stopPropagation();

            var text;
            try { text = decodeURIComponent(encoded); } catch (_) { text = encoded; }

            ipcInvoke('clipboard-write-text', [text])
                .then(function () { showToast('已复制到剪贴板', 'success', 1200); })
                .catch(function () { showToast('复制失败', 'error', 1600); });
        }, true);
    } catch (_) { }
})();
